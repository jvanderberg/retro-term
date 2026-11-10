//! ZMODEM sender (the `sz` side).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use xfer_common::crc::crc32_update;
use xfer_common::{announce_name, Event, ProgressThrottle, Result, Summary, XferError};

use crate::consts::*;
use crate::frame::{flag_bytes, pos_bytes, Got, Header, Link};
use crate::Options;

/// How long to wait for a further header after a (probably duplicate) ZRINIT
/// before re-sending ZFILE.
const DUP_WAIT: Duration = Duration::from_secs(1);
/// Smallest subpacket we shrink to after errors.
const MIN_BLOCK: usize = 512;

enum Offer {
    Pos(u32),
    Skip,
}

enum Outcome {
    Sent(u64),
    Skipped(String),
}

enum EofReply {
    Done,
    Rpos(u32),
    Skip,
}

struct Sender<'l, 'a> {
    link: &'l mut Link<'a>,
    opts: &'l Options,
    crc32: bool,
    /// Receiver buffer size (0 = full streaming).
    rx_bufsize: u64,
    max_blk: usize,
    blk: usize,
    window: u64,
    buf: Vec<u8>,
    progress: ProgressThrottle,
}

pub(crate) fn run(link: &mut Link, files: &[PathBuf], opts: &Options) -> Result<Summary> {
    let mut s = Sender {
        link,
        opts,
        crc32: false,
        rx_bufsize: 0,
        max_blk: opts.max_block.clamp(32, 8192),
        blk: 0,
        window: opts.window,
        buf: vec![0u8; 8192],
        progress: ProgressThrottle::default(),
    };
    if opts.send_rz_command {
        s.link.put_raw(b"rz\r");
    }
    s.handshake()?;

    let sizes: Vec<u64> = files.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).collect();
    let mut summary = Summary::default();
    for (i, path) in files.iter().enumerate() {
        let name = announce_name(path);
        let files_left = files.len() - i;
        let bytes_left: u64 = sizes[i..].iter().sum();
        match s.send_file(path, &name, files_left, bytes_left)? {
            Outcome::Sent(n) => {
                s.link.ctl.event(Event::FileDone { name: name.clone(), bytes: n, path: path.clone() });
                summary.files.push((name, n, path.clone()));
            }
            Outcome::Skipped(reason) => s.link.ctl.event(Event::FileSkipped { name, reason }),
        }
    }
    s.finish()?;
    Ok(summary)
}

fn read_full(f: &mut File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

fn file_info(name: &str, meta: &std::fs::Metadata, files_left: usize, bytes_left: u64) -> Vec<u8> {
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::MetadataExt;
        meta.mode()
    };
    #[cfg(not(unix))]
    let mode = 0o100644u32;
    let mut info = name.as_bytes().to_vec();
    info.push(0);
    info.extend_from_slice(format!("{} {:o} {:o} 0 {} {}", meta.len(), mtime, mode, files_left, bytes_left).as_bytes());
    info.push(0);
    info
}

impl Sender<'_, '_> {
    fn retry(&mut self, errors: &mut u32, reason: String) -> Result<()> {
        *errors += 1;
        if *errors > self.opts.max_retries {
            return Err(XferError::TooManyErrors(reason));
        }
        self.link.ctl.event(Event::Retry { reason });
        Ok(())
    }

    fn handshake(&mut self) -> Result<()> {
        let mut errors = 0;
        loop {
            self.link.send_hex(ZRQINIT, [0; 4])?;
            loop {
                match self.link.read_header(self.opts.timeout)? {
                    Got::Hdr(h) => match h.kind {
                        ZRINIT => {
                            self.apply_zrinit(&h);
                            return self.maybe_zsinit();
                        }
                        ZCHALLENGE => self.link.send_hex(ZACK, h.d)?,
                        ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                        ZNAK => break,
                        _ => {}
                    },
                    Got::Bad(r) => {
                        self.retry(&mut errors, r.to_string())?;
                        break;
                    }
                    Got::Timeout => {
                        if errors >= self.opts.max_retries {
                            return Err(XferError::Timeout);
                        }
                        self.retry(&mut errors, "no ZRINIT from receiver".into())?;
                        break;
                    }
                }
            }
        }
    }

    fn apply_zrinit(&mut self, h: &Header) {
        let flags = h.zf0();
        self.crc32 = self.opts.use_crc32 && flags & CANFC32 != 0;
        self.link.set_tx_ctl(self.opts.escape_ctl || flags & ESCCTL != 0);
        let mut bufsize = u64::from(u16::from_le_bytes([h.d[0], h.d[1]]));
        if bufsize == 0 && flags & CANOVIO == 0 {
            bufsize = self.max_blk as u64;
        }
        self.rx_bufsize = bufsize;
        if bufsize > 0 {
            self.max_blk = self.max_blk.min(bufsize as usize).max(32);
        }
        self.blk = self.max_blk;
    }

    fn maybe_zsinit(&mut self) -> Result<()> {
        if !self.opts.escape_ctl {
            return Ok(());
        }
        let mut errors = 0;
        loop {
            self.link.put_bin_header(ZSINIT, flag_bytes(TESCCTL, 0, 0, 0), self.crc32);
            self.link.put_subpacket(&[0], ZCRCW, self.crc32);
            self.link.flush_out()?;
            loop {
                match self.link.read_header(self.opts.timeout)? {
                    Got::Hdr(h) if h.kind == ZACK => return Ok(()),
                    Got::Hdr(h) if matches!(h.kind, ZCAN | ZABORT) => return Err(XferError::RemoteCancelled),
                    Got::Hdr(h) if h.kind == ZRINIT => continue,
                    Got::Hdr(_) | Got::Bad(_) | Got::Timeout => {
                        self.retry(&mut errors, "no ZACK for ZSINIT".into())?;
                        break;
                    }
                }
            }
        }
    }

    fn send_file(&mut self, path: &Path, name: &str, files_left: usize, bytes_left: u64) -> Result<Outcome> {
        let mut f = match File::open(path) {
            Ok(f) => f,
            Err(e) => return Ok(Outcome::Skipped(format!("cannot open: {e}"))),
        };
        let meta = f.metadata()?;
        if !meta.is_file() {
            return Ok(Outcome::Skipped("not a regular file".into()));
        }
        let size = meta.len();
        if size > u64::from(u32::MAX) {
            return Ok(Outcome::Skipped("file too large for ZMODEM (4 GiB limit)".into()));
        }
        self.link.ctl.event(Event::FileStart { name: name.to_string(), size: Some(size) });
        let info = file_info(name, &meta, files_left, bytes_left);
        let start = match self.offer(&info, &mut f)? {
            Offer::Pos(p) => p,
            Offer::Skip => return Ok(Outcome::Skipped("receiver skipped the file".into())),
        };
        self.progress = ProgressThrottle::default();
        self.stream(&mut f, start, size)
    }

    /// Reply to a ZCRC request with the CRC-32 of the first `len` bytes
    /// (whole file when 0).
    fn reply_crc(&mut self, f: &mut File, len: u32) -> Result<()> {
        f.seek(SeekFrom::Start(0))?;
        let mut remaining = if len == 0 { u64::MAX } else { u64::from(len) };
        let mut crc = 0xFFFF_FFFFu32;
        while remaining > 0 {
            let want = self.buf.len().min(usize::try_from(remaining).unwrap_or(usize::MAX));
            let n = read_full(f, &mut self.buf[..want])?;
            if n == 0 {
                break;
            }
            crc = crc32_update(crc, &self.buf[..n]);
            remaining -= n as u64;
        }
        self.link.put_bin_header(ZCRC, pos_bytes(!crc), self.crc32);
        self.link.flush_out()
    }

    fn offer(&mut self, info: &[u8], f: &mut File) -> Result<Offer> {
        let mut errors = 0;
        'send: loop {
            self.link.put_bin_header(ZFILE, flag_bytes(ZCBIN, 0, 0, 0), self.crc32);
            self.link.put_subpacket(info, ZCRCW, self.crc32);
            self.link.flush_out()?;
            let mut wait = self.opts.timeout;
            loop {
                match self.link.read_header(wait)? {
                    Got::Hdr(h) => match h.kind {
                        ZRPOS => return Ok(Offer::Pos(h.pos())),
                        ZSKIP => return Ok(Offer::Skip),
                        // Probably a duplicate from the handshake; if nothing
                        // else follows shortly, the receiver missed ZFILE.
                        ZRINIT => wait = DUP_WAIT,
                        ZCRC => {
                            self.reply_crc(f, h.pos())?;
                            wait = self.opts.timeout;
                        }
                        ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                        ZFIN => return Err(XferError::Protocol("receiver ended the session".into())),
                        ZNAK => {
                            self.retry(&mut errors, "receiver NAKed ZFILE".into())?;
                            continue 'send;
                        }
                        _ => {}
                    },
                    Got::Bad(r) => {
                        self.retry(&mut errors, r.to_string())?;
                        continue 'send;
                    }
                    Got::Timeout => {
                        self.retry(&mut errors, "no response to ZFILE".into())?;
                        continue 'send;
                    }
                }
            }
        }
    }

    /// Terminate an open ZDATA frame (after an interruption).
    fn end_frame(&mut self) -> Result<()> {
        self.link.put_subpacket(&[], ZCRCE, self.crc32);
        self.link.flush_out()
    }

    fn stream(&mut self, f: &mut File, start: u32, size: u64) -> Result<Outcome> {
        let mut pos = u64::from(start);
        let mut lrxpos = pos;
        let mut last_rpos: Option<u64> = None;
        let mut same_rpos = 0u32;
        let mut good = 0u32;
        // After a ZRPOS, send one ZCRCW subpacket and wait for its ZACK
        // before streaming again (as lrzsz does). Otherwise each duplicate
        // ZRPOS a receiver emits while skipping stale data would trigger
        // another full window of data and a ZRPOS/ZDATA ping-pong.
        let mut resync = false;
        let ack_every = if self.window > 0 { (self.window / 4).max(self.max_blk as u64) } else { 256 * 1024 };

        // Handle a ZRPOS: decide whether we are making progress.
        macro_rules! rpos {
            ($p:expr) => {{
                let p = u64::from($p);
                if last_rpos == Some(p) {
                    // Duplicates are normal (a receiver may repeat ZRPOS
                    // while resynchronising), so be generous here; the
                    // receiver enforces its own error limit.
                    same_rpos += 1;
                    if same_rpos > 4 * self.opts.max_retries {
                        return Err(XferError::TooManyErrors(format!("receiver keeps asking for offset {p}")));
                    }
                } else {
                    same_rpos = 0;
                }
                last_rpos = Some(p);
                self.link.ctl.event(Event::Retry { reason: format!("receiver requested resend from offset {p}") });
                self.blk = (self.blk / 2).max(MIN_BLOCK.min(self.max_blk));
                good = 0;
                pos = p;
                lrxpos = p;
                resync = true;
            }};
        }

        'restart: loop {
            f.seek(SeekFrom::Start(pos))?;
            self.link.put_bin_header(ZDATA, pos_bytes(pos as u32), self.crc32);
            let mut since_q = 0u64;
            let mut since_w = 0u64;
            loop {
                if self.link.ctl.cancelled() {
                    return Err(XferError::Cancelled);
                }
                let blk = self.blk;
                let n = read_full(f, &mut self.buf[..blk])?;
                let eof = n < blk || pos + n as u64 >= size;
                let end = if eof {
                    ZCRCE
                } else if resync || (self.rx_bufsize > 0 && since_w + (n as u64) + (blk as u64) > self.rx_bufsize) {
                    ZCRCW
                } else if since_q + n as u64 >= ack_every {
                    ZCRCQ
                } else {
                    ZCRCG
                };
                self.link.put_subpacket(&self.buf[..n], end, self.crc32);
                self.link.flush_out()?;
                pos += n as u64;
                since_w += n as u64;
                since_q = if end == ZCRCQ { 0 } else { since_q + n as u64 };
                self.progress.report(self.link.ctl, pos, false);
                good += 1;
                if good >= 32 && self.blk < self.max_blk {
                    self.blk = (self.blk * 2).min(self.max_blk);
                    good = 0;
                }

                if eof {
                    match self.eof(pos)? {
                        EofReply::Done => {
                            self.progress.report(self.link.ctl, pos, true);
                            return Ok(Outcome::Sent(pos));
                        }
                        EofReply::Skip => return Ok(Outcome::Skipped("receiver skipped the file".into())),
                        EofReply::Rpos(p) => {
                            rpos!(p);
                            continue 'restart;
                        }
                    }
                }

                if end == ZCRCW {
                    let mut errors = 0;
                    loop {
                        match self.link.read_header(self.opts.timeout)? {
                            Got::Hdr(h) => match h.kind {
                                ZACK => {
                                    // Only the acknowledgement of exactly
                                    // what we sent counts; older ones are
                                    // stale (e.g. from a ZCRCQ).
                                    lrxpos = u64::from(h.pos()).min(pos);
                                    if lrxpos == pos {
                                        resync = false;
                                        continue 'restart;
                                    }
                                }
                                ZRPOS => {
                                    rpos!(h.pos());
                                    continue 'restart;
                                }
                                ZSKIP => return Ok(Outcome::Skipped("receiver skipped the file".into())),
                                ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                                _ => {}
                            },
                            Got::Bad(r) => self.retry(&mut errors, r.to_string())?,
                            Got::Timeout => {
                                self.retry(&mut errors, "no ZACK for ZCRCW".into())?;
                                pos = lrxpos;
                                continue 'restart;
                            }
                        }
                    }
                }

                // Full streaming: check the back channel without blocking.
                if let Some(h) = self.link.poll_header()? {
                    match h.kind {
                        ZRPOS => {
                            self.end_frame()?;
                            rpos!(h.pos());
                            continue 'restart;
                        }
                        ZACK => lrxpos = u64::from(h.pos()).min(pos),
                        ZSKIP => {
                            self.end_frame()?;
                            return Ok(Outcome::Skipped("receiver skipped the file".into()));
                        }
                        ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                        _ => {}
                    }
                }

                // Sliding window: don't run too far ahead of the receiver.
                while self.window > 0 && pos - lrxpos >= self.window {
                    match self.link.read_header(self.opts.timeout)? {
                        Got::Hdr(h) => match h.kind {
                            ZACK => lrxpos = u64::from(h.pos()).min(pos),
                            ZRPOS => {
                                self.end_frame()?;
                                rpos!(h.pos());
                                continue 'restart;
                            }
                            ZSKIP => {
                                self.end_frame()?;
                                return Ok(Outcome::Skipped("receiver skipped the file".into()));
                            }
                            ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                            _ => {}
                        },
                        Got::Bad(_) => {}
                        Got::Timeout => {
                            self.link.ctl.event(Event::Message(
                                "receiver does not acknowledge ZCRCQ; streaming without a window".into(),
                            ));
                            self.window = 0;
                        }
                    }
                }
            }
        }
    }

    fn eof(&mut self, pos: u64) -> Result<EofReply> {
        let mut errors = 0;
        loop {
            self.link.put_bin_header(ZEOF, pos_bytes(pos as u32), self.crc32);
            self.link.flush_out()?;
            loop {
                match self.link.read_header(self.opts.timeout)? {
                    Got::Hdr(h) => match h.kind {
                        ZRINIT => return Ok(EofReply::Done),
                        ZRPOS => return Ok(EofReply::Rpos(h.pos())),
                        ZSKIP => return Ok(EofReply::Skip),
                        ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                        ZFIN => return Err(XferError::Protocol("receiver ended the session".into())),
                        _ => {}
                    },
                    Got::Bad(r) => self.retry(&mut errors, r.to_string())?,
                    Got::Timeout => {
                        self.retry(&mut errors, "no response to ZEOF".into())?;
                        break;
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> Result<()> {
        for _ in 0..self.opts.max_retries.clamp(1, 5) {
            self.link.send_hex(ZFIN, [0; 4])?;
            loop {
                match self.link.read_header(self.opts.timeout)? {
                    Got::Hdr(h) if h.kind == ZFIN => {
                        self.link.put_raw(b"OO");
                        return self.link.flush_out();
                    }
                    Got::Hdr(h) if matches!(h.kind, ZCAN | ZABORT) => return Ok(()),
                    Got::Hdr(_) | Got::Bad(_) => {}
                    Got::Timeout => break,
                }
            }
        }
        self.link.ctl.event(Event::Message("receiver did not acknowledge ZFIN".into()));
        Ok(())
    }
}
