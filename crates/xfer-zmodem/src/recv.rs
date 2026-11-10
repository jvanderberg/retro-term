//! ZMODEM receiver (the `rz` side).

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use xfer_common::{
    create_dest_file, mtime_from_unix, Event, ProgressThrottle, RecvOptions, Result, Summary, XferError,
};

use crate::consts::*;
use crate::frame::{pos_bytes, Got, Header, Link, Sub};
use crate::Options;

/// Largest data subpacket we accept (lrzsz sends up to 8 KiB; be lenient).
const MAX_SUBPACKET: usize = 16 * 1024;
/// Largest re-sent overlap we silently discard rather than re-requesting.
const MAX_OVERLAP: u64 = 2 << 20;
/// After asking for a resend, a bad header seen within this many bytes is
/// assumed to be stale in-flight data that merely looked like a header, and
/// does not trigger another ZRPOS.
const RESEND_AFTER: u64 = 256 * 1024;

/// Parsed ZFILE information block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub name: String,
    pub size: Option<u64>,
    pub mtime: Option<u64>,
}

/// Parse "name\0size mtime mode serial files_left bytes_left\0".
pub(crate) fn parse_file_info(d: &[u8]) -> FileInfo {
    let nul = d.iter().position(|&b| b == 0).unwrap_or(d.len());
    let name = String::from_utf8_lossy(&d[..nul]).into_owned();
    let rest = d.get(nul + 1..).unwrap_or(&[]);
    let rest = &rest[..rest.iter().position(|&b| b == 0).unwrap_or(rest.len())];
    let text = String::from_utf8_lossy(rest);
    let mut fields = text.split_ascii_whitespace();
    let size = fields.next().and_then(|s| s.parse::<u64>().ok());
    let mtime = fields.next().and_then(|s| u64::from_str_radix(s, 8).ok()).filter(|&t| t > 0);
    FileInfo { name, size, mtime }
}

struct Receiver<'l, 'a> {
    link: &'l mut Link<'a>,
    ropts: &'l RecvOptions,
    opts: &'l Options,
    data: Vec<u8>,
    attn: Vec<u8>,
    summary: Summary,
}

pub(crate) fn run(link: &mut Link, ropts: &RecvOptions, opts: &Options) -> Result<Summary> {
    if opts.escape_ctl {
        link.set_rx_ctl(true);
        link.set_tx_ctl(true);
    }
    let mut r = Receiver {
        link,
        ropts,
        opts,
        data: Vec::with_capacity(MAX_SUBPACKET + 64),
        attn: Vec::new(),
        summary: Summary::default(),
    };
    r.session()
}

impl Receiver<'_, '_> {
    fn retry(&mut self, errors: &mut u32, reason: &str) -> Result<()> {
        *errors += 1;
        if *errors > self.opts.max_retries {
            return Err(XferError::TooManyErrors(reason.to_string()));
        }
        self.link.ctl.event(Event::Retry { reason: reason.to_string() });
        Ok(())
    }

    fn send_zrinit(&mut self) -> Result<()> {
        let mut f0 = CANFDX | CANOVIO;
        if self.opts.use_crc32 {
            f0 |= CANFC32;
        }
        if self.opts.escape_ctl {
            f0 |= ESCCTL;
        }
        let b = self.opts.rx_buffer_size.to_le_bytes();
        self.link.send_hex(ZRINIT, [b[0], b[1], 0, f0])
    }

    fn send_rpos(&mut self, offset: u64, with_attn: bool) -> Result<()> {
        if with_attn && !self.attn.is_empty() {
            // 0xDD (break) and 0xDE (pause) are not representable here.
            let attn: Vec<u8> = self.attn.iter().copied().filter(|&b| b != 0xdd && b != 0xde).collect();
            self.link.put_raw(&attn);
        }
        self.link.send_hex(ZRPOS, pos_bytes(offset as u32))
    }

    fn session(&mut self) -> Result<Summary> {
        let mut errors = 0;
        let mut heard = false;
        self.send_zrinit()?;
        loop {
            let h = match self.link.read_header(self.opts.timeout)? {
                Got::Hdr(h) => h,
                Got::Bad(r) => {
                    self.retry(&mut errors, r)?;
                    self.send_zrinit()?;
                    continue;
                }
                Got::Timeout => {
                    if errors >= self.opts.max_retries {
                        return Err(if heard {
                            XferError::TooManyErrors("timed out waiting for sender".into())
                        } else {
                            XferError::Timeout
                        });
                    }
                    self.retry(&mut errors, "waiting for sender")?;
                    self.send_zrinit()?;
                    continue;
                }
            };
            heard = true;
            match h.kind {
                ZRQINIT => self.send_zrinit()?,
                ZSINIT => match self.link.read_subpacket(h.crc32(), MAX_SUBPACKET, &mut self.data)? {
                    Sub::Ok(_) => {
                        let end = self.data.iter().position(|&b| b == 0).unwrap_or(self.data.len());
                        self.attn = self.data[..end].to_vec();
                        if h.zf0() & TESCCTL != 0 {
                            self.link.set_rx_ctl(true);
                            self.link.set_tx_ctl(true);
                        }
                        self.link.send_hex(ZACK, pos_bytes(1))?;
                    }
                    Sub::Bad(r) => {
                        self.retry(&mut errors, r)?;
                        self.link.send_hex(ZNAK, [0; 4])?;
                    }
                },
                ZFILE => match self.link.read_subpacket(h.crc32(), MAX_SUBPACKET, &mut self.data)? {
                    Sub::Ok(_) => {
                        self.receive_file()?;
                        errors = 0;
                    }
                    Sub::Bad(r) => {
                        self.retry(&mut errors, r)?;
                        self.link.send_hex(ZNAK, [0; 4])?;
                    }
                },
                ZFIN => {
                    self.finish()?;
                    return Ok(std::mem::take(&mut self.summary));
                }
                ZCOMMAND => {
                    let sub = self.link.read_subpacket(h.crc32(), MAX_SUBPACKET, &mut self.data)?;
                    if matches!(sub, Sub::Ok(_)) {
                        let end = self.data.iter().position(|&b| b == 0).unwrap_or(self.data.len());
                        let cmd = String::from_utf8_lossy(&self.data[..end]).into_owned();
                        self.link.ctl.event(Event::Message(format!("refused remote command request: {cmd:?}")));
                        // Report failure; never execute anything.
                        self.link.send_hex(ZCOMPL, pos_bytes(1))?;
                    } else {
                        self.link.send_hex(ZNAK, [0; 4])?;
                    }
                }
                ZFREECNT => self.link.send_hex(ZACK, pos_bytes(u32::MAX))?,
                ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                // The sender is out of step (e.g. it missed our ZRINIT).
                ZEOF | ZDATA | ZNAK => self.send_zrinit()?,
                _ => {}
            }
        }
    }

    fn receive_file(&mut self) -> Result<()> {
        let info = parse_file_info(&self.data);
        self.link.ctl.event(Event::FileStart { name: info.name.clone(), size: info.size });
        let (path, file) = match create_dest_file(&self.ropts.dest_dir, &info.name, self.ropts.overwrite) {
            Ok(pf) => pf,
            Err(e) => {
                let reason = format!("cannot create {} in {}: {e}", info.name, self.ropts.dest_dir.display());
                self.link.ctl.event(Event::FileSkipped { name: info.name, reason });
                return self.link.send_hex(ZSKIP, [0; 4]);
            }
        };
        let mut w = BufWriter::with_capacity(256 * 1024, file);
        let mut offset = 0u64;
        let mut errors = 0u32;
        let mut eof_mismatch = 0u32;
        let mut progress = ProgressThrottle::default();
        self.send_rpos(0, false)?;
        let mut rpos_at = self.link.consumed();
        loop {
            let h: Header = match self.link.read_header(self.opts.timeout)? {
                Got::Hdr(h) => h,
                Got::Bad(r) => {
                    if self.link.consumed() - rpos_at >= RESEND_AFTER {
                        self.retry(&mut errors, r)?;
                        self.send_rpos(offset, true)?;
                        rpos_at = self.link.consumed();
                    }
                    continue;
                }
                Got::Timeout => {
                    self.retry(&mut errors, "timeout waiting for data")?;
                    self.send_rpos(offset, true)?;
                    continue;
                }
            };
            match h.kind {
                ZDATA => {
                    let start = u64::from(h.pos());
                    // A restart a little behind us (the sender acting on a
                    // duplicate ZRPOS) is absorbed by discarding the overlap;
                    // answering it with another ZRPOS would start a
                    // ZRPOS/ZDATA ping-pong.
                    if start > offset || offset - start > MAX_OVERLAP {
                        self.retry(&mut errors, "data at wrong offset")?;
                        self.send_rpos(offset, true)?;
                        rpos_at = self.link.consumed();
                        continue;
                    }
                    let mut skip = offset - start;
                    loop {
                        match self.link.read_subpacket(h.crc32(), MAX_SUBPACKET, &mut self.data)? {
                            Sub::Ok(end) => {
                                let k = usize::try_from(skip).unwrap_or(usize::MAX).min(self.data.len());
                                skip -= k as u64;
                                w.write_all(&self.data[k..])?;
                                offset += (self.data.len() - k) as u64;
                                errors = 0;
                                progress.report(self.link.ctl, offset, false);
                                match end {
                                    ZCRCW => {
                                        self.link.send_hex(ZACK, pos_bytes(offset as u32))?;
                                        break;
                                    }
                                    ZCRCQ => self.link.send_hex(ZACK, pos_bytes(offset as u32))?,
                                    ZCRCG => {}
                                    _ => break,
                                }
                            }
                            Sub::Bad(r) => {
                                self.retry(&mut errors, r)?;
                                self.send_rpos(offset, true)?;
                                rpos_at = self.link.consumed();
                                break;
                            }
                        }
                    }
                }
                ZEOF => {
                    if u64::from(h.pos()) != offset {
                        // Probably sent before our ZRPOS reached the sender;
                        // only re-request if it keeps happening.
                        eof_mismatch += 1;
                        if eof_mismatch >= 2 {
                            eof_mismatch = 0;
                            self.retry(&mut errors, "end of file at wrong offset")?;
                            self.send_rpos(offset, true)?;
                        }
                        continue;
                    }
                    let file = w.into_inner().map_err(|e| XferError::Io(e.into_error()))?;
                    if let Some(t) = info.mtime.and_then(mtime_from_unix) {
                        let _ = file.set_modified(t);
                    }
                    drop(file);
                    progress.report(self.link.ctl, offset, true);
                    self.link.ctl.event(Event::FileDone { name: info.name.clone(), bytes: offset, path: path.clone() });
                    self.summary.files.push((info.name, offset, path));
                    return self.send_zrinit();
                }
                ZFILE => {
                    // Sender didn't see our ZRPOS.
                    let _ = self.link.read_subpacket(h.crc32(), MAX_SUBPACKET, &mut self.data)?;
                    self.send_rpos(offset, false)?;
                }
                ZNAK => self.send_rpos(offset, false)?,
                ZCAN | ZABORT => return Err(XferError::RemoteCancelled),
                ZFIN => return Err(XferError::Protocol(format!("sender ended the session during {}", info.name))),
                _ => {}
            }
        }
    }

    /// Answer ZFIN and collect the sender's "OO".
    fn finish(&mut self) -> Result<()> {
        let wait = self.opts.timeout.min(Duration::from_secs(3));
        for _ in 0..3 {
            self.link.send_hex(ZFIN, [0; 4])?;
            let deadline = Instant::now() + wait;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match self.link.byte_t(left) {
                    Ok(b'O') => {
                        let _ = self.link.byte_t(Duration::from_millis(100));
                        return Ok(());
                    }
                    // Sender re-sent ZFIN: it missed ours.
                    Ok(ZPAD) => break,
                    Ok(_) => {}
                    Err(XferError::Cancelled) => return Err(XferError::Cancelled),
                    Err(_) => return Ok(()),
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrzsz_file_info() {
        let fi = parse_file_info(b"hello.txt\x0012345 14567123456 100644 0 3 99999\0");
        assert_eq!(fi.name, "hello.txt");
        assert_eq!(fi.size, Some(12345));
        assert_eq!(fi.mtime, Some(0o14567123456));
        let fi = parse_file_info(b"bare\0");
        assert_eq!(fi, FileInfo { name: "bare".into(), size: None, mtime: None });
    }
}
