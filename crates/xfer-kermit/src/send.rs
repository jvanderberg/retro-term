//! File sender: S, then F / A / D... / Z per file, then B. Data packets are
//! pipelined through a selective-repeat sliding window.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use xfer_common::{announce_name, Control, Event, Port, ProgressThrottle, Result, Summary, XferError};

use crate::link::Link;
use crate::packet::{check_len, tochar, Parsed, MAX_LONG, MAX_NORMAL};
use crate::params::{negotiate, InitParams};
use crate::KermitOptions;

struct Slot {
    seq: u8,
    typ: u8,
    frame: Vec<u8>,
    acked: bool,
    retries: u32,
    ack_data: Vec<u8>,
    /// ACKs seen for later packets since this one was last transmitted.
    later_acks: u32,
}

struct Sender<'a, 'b> {
    link: Link<'a>,
    ctl: &'b mut dyn Control,
    opts: &'b KermitOptions,
    win: VecDeque<Slot>,
    win_size: usize,
    next_seq: u8,
    /// Last time the window made progress (or we retransmitted).
    last_activity: Instant,
    /// `X` or `Z` from the receiver's ACK to a data packet.
    interrupt: Option<u8>,
    /// ACK data of the most recently retired slot.
    last_ack: Vec<u8>,
    remote_error: bool,
    data_cap: usize,
    attrs: bool,
}

pub fn send(port: &mut dyn Port, ctl: &mut dyn Control, files: &[PathBuf], opts: &KermitOptions) -> Result<Summary> {
    let mut s = Sender {
        link: Link::new(port),
        ctl,
        opts,
        win: VecDeque::new(),
        win_size: 1,
        next_seq: 0,
        last_activity: Instant::now(),
        interrupt: None,
        last_ack: Vec::new(),
        remote_error: false,
        data_cap: 80,
        attrs: false,
    };
    let res = s.run(files);
    if let Err(e) = &res {
        if !s.remote_error {
            let msg = match e {
                XferError::Cancelled => "Cancelled by user".to_string(),
                other => other.to_string(),
            };
            let seq = s.next_seq;
            s.link.send_error(seq, &msg);
        }
    }
    res
}

impl Sender<'_, '_> {
    fn run(&mut self, files: &[PathBuf]) -> Result<Summary> {
        self.send_init()?;
        let mut summary = Summary::default();
        for path in files {
            if self.interrupt == Some(b'Z') {
                let name = announce_name(path);
                self.ctl.event(Event::FileSkipped { name, reason: "receiver cancelled the batch".into() });
                continue;
            }
            if let Some(done) = self.send_file(path)? {
                summary.files.push(done);
            }
        }
        self.send_break()?;
        Ok(summary)
    }

    fn check_cancel(&self) -> Result<()> {
        if self.ctl.cancelled() {
            Err(XferError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn send_init(&mut self) -> Result<()> {
        let mine = InitParams::for_sender(self.opts);
        let frame = self.link.frame_chk(1, 0, b'S', &mine.encode());
        self.link.write(&frame)?;
        let mut retries = 0u32;
        let mut nudged = false;
        let theirs = loop {
            self.check_cancel()?;
            let resend = match self.link.read(self.opts.timeout, &*self.ctl)? {
                Some(Parsed::Packet(p)) if p.typ == b'Y' && p.seq == 0 => break InitParams::parse(&p.data),
                Some(Parsed::Packet(p)) if p.typ == b'E' => {
                    self.remote_error = true;
                    return Err(XferError::Protocol(self.link.error_text(&p.data)));
                }
                Some(Parsed::Packet(p)) if p.typ == b'N' && !nudged => {
                    // A receiver that is already waiting NAKs to prompt us;
                    // just resend.
                    nudged = true;
                    self.link.write(&frame)?;
                    None
                }
                Some(Parsed::Packet(p)) if p.typ == b'N' => Some("NAK of Send-Init".to_string()),
                Some(Parsed::Packet(_)) => None,
                Some(Parsed::Corrupt(why)) => Some(why),
                None => Some("timeout waiting for Send-Init ACK".to_string()),
            };
            if let Some(reason) = resend {
                retries += 1;
                if retries > self.opts.retry_limit {
                    return Err(XferError::TooManyErrors(reason));
                }
                self.ctl.event(Event::Retry { reason });
                self.link.write(&frame)?;
            }
        };
        let neg = negotiate(&mine, &theirs, true);
        self.link.apply(&neg);
        self.win_size = neg.window.max(1);
        self.attrs = neg.attrs && self.opts.attributes;
        let chklen = check_len(neg.chk);
        let ours = self.opts.max_packet_len.clamp(20, MAX_LONG);
        let maxlen = if neg.long { neg.their_max.min(ours) } else { neg.their_max.min(ours).min(MAX_NORMAL) };
        let cap = if maxlen > MAX_NORMAL {
            maxlen.saturating_sub(chklen + 7)
        } else {
            maxlen.min(neg.frame.normal_max).saturating_sub(chklen + 3)
        };
        // One encoded byte can take up to 5 characters (repeat + 8th-bit +
        // control prefix), so never go below that even for absurdly small
        // remote limits.
        self.data_cap = cap.max(5);
        self.next_seq = 1;
        self.ctl.event(Event::Message(format!(
            "Kermit: block check {}, window {}, packet length {}{}{}",
            neg.chk,
            self.win_size,
            maxlen,
            if neg.enc.qbin.is_some() { ", 8th-bit prefixing" } else { "" },
            if neg.enc.rept.is_some() { ", repeat counts" } else { "" },
        )));
        Ok(())
    }

    fn send_file(&mut self, path: &Path) -> Result<Option<(String, u64, PathBuf)>> {
        let name = announce_name(path);
        let mut file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                self.ctl.event(Event::FileSkipped { name, reason: e.to_string() });
                return Ok(None);
            }
        };
        let size = file.metadata().map(|m| m.len()).ok();

        let mut fname = Vec::new();
        let enc = self.link.enc;
        enc.encode(name.as_bytes(), &mut fname, self.data_cap.min(200));
        self.send_single(b'F', &fname)?;
        self.ctl.event(Event::FileStart { name: name.clone(), size });

        if self.attrs {
            let ack = self.send_single(b'A', &attributes(size))?;
            if ack.first() == Some(&b'N') {
                self.send_single(b'Z', b"D")?;
                self.ctl.event(Event::FileSkipped { name, reason: "receiver refused the file".into() });
                return Ok(None);
            }
        }

        self.interrupt = None;
        let mut throttle = ProgressThrottle::default();
        let mut buf: Vec<u8> = Vec::with_capacity(512 * 1024);
        let mut pos = 0usize;
        let mut eof = false;
        let mut sent = 0u64;
        loop {
            if self.interrupt.is_some() {
                break;
            }
            if !eof && buf.len() - pos < 256 * 1024 {
                buf.drain(..pos);
                pos = 0;
                eof = fill(&mut file, &mut buf, 512 * 1024)?;
            }
            if pos >= buf.len() {
                break;
            }
            let mut pkt = Vec::with_capacity(self.data_cap);
            let used = enc.encode(&buf[pos..], &mut pkt, self.data_cap);
            if used == 0 {
                return Err(XferError::Protocol("packet length too small to carry data".into()));
            }
            pos += used;
            sent += used as u64;
            self.push(b'D', &pkt)?;
            throttle.report(self.ctl, sent, false);
        }
        self.drain()?;

        if let Some(code) = self.interrupt {
            self.send_single(b'Z', b"D")?;
            let reason = if code == b'Z' { "receiver cancelled the batch" } else { "receiver skipped the file" };
            self.ctl.event(Event::FileSkipped { name, reason: reason.into() });
            return Ok(None);
        }
        self.send_single(b'Z', b"")?;
        throttle.report(self.ctl, sent, true);
        self.ctl.event(Event::FileDone { name: name.clone(), bytes: sent, path: path.to_path_buf() });
        Ok(Some((name, sent, path.to_path_buf())))
    }

    fn send_break(&mut self) -> Result<()> {
        match self.send_single(b'B', b"") {
            Ok(_) => Ok(()),
            // Every file has already been acknowledged; a lost ACK to the
            // final B (or the receiver hanging up right after sending it) is
            // not worth failing the transfer over.
            Err(XferError::TooManyErrors(_)) => {
                self.ctl.event(Event::Message("no ACK for end-of-transfer packet; assuming done".into()));
                Ok(())
            }
            Err(XferError::Io(e)) if matches!(e.kind(), ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Send one packet stop-and-wait style and return the ACK's data.
    fn send_single(&mut self, typ: u8, data: &[u8]) -> Result<Vec<u8>> {
        self.drain()?;
        self.push(typ, data)?;
        self.drain()?;
        Ok(std::mem::take(&mut self.last_ack))
    }

    /// Queue a packet in the window, waiting for room first.
    fn push(&mut self, typ: u8, data: &[u8]) -> Result<()> {
        while self.win.len() >= self.win_size {
            self.service(true)?;
        }
        let seq = self.next_seq;
        let frame = self.link.send(seq, typ, data)?;
        if self.win.is_empty() {
            self.last_activity = Instant::now();
        }
        self.win.push_back(Slot { seq, typ, frame, acked: false, retries: 0, ack_data: Vec::new(), later_acks: 0 });
        self.next_seq = (seq + 1) % 64;
        // Pick up any ACKs that have already arrived.
        while self.service(false)? {}
        Ok(())
    }

    fn drain(&mut self) -> Result<()> {
        while !self.win.is_empty() {
            self.service(true)?;
        }
        Ok(())
    }

    fn retire(&mut self) {
        while self.win.front().is_some_and(|s| s.acked) {
            if let Some(s) = self.win.pop_front() {
                self.last_ack = s.ack_data;
            }
        }
    }

    fn retransmit(&mut self, idx: usize, reason: String) -> Result<()> {
        let limit = self.opts.retry_limit;
        let slot = &mut self.win[idx];
        slot.retries += 1;
        slot.later_acks = 0;
        if slot.retries > limit {
            return Err(XferError::TooManyErrors(reason));
        }
        let frame = slot.frame.clone();
        self.ctl.event(Event::Retry { reason });
        self.link.write(&frame)?;
        self.last_activity = Instant::now();
        Ok(())
    }

    /// Packets are delivered in order, so ACKs for several packets after the
    /// oldest unacknowledged one mean it (or its ACK) was lost: resend it
    /// without waiting for the timer.
    fn fast_retransmit(&mut self, acked_idx: usize) -> Result<()> {
        if let Some(oldest) = self.win.iter().position(|s| !s.acked) {
            if oldest < acked_idx {
                self.win[oldest].later_acks += 1;
                if self.win[oldest].later_acks >= 3 {
                    let seq = self.win[oldest].seq;
                    self.retransmit(oldest, format!("packet {seq} not acknowledged, resending"))?;
                }
            }
        }
        Ok(())
    }

    /// Handle at most one incoming packet. With `wait`, block until a packet
    /// arrives or the retransmission timer fires. Returns whether a packet
    /// was processed.
    fn service(&mut self, wait: bool) -> Result<bool> {
        self.check_cancel()?;
        let timeout = if wait {
            (self.last_activity + self.opts.timeout).saturating_duration_since(Instant::now())
        } else {
            Duration::ZERO
        };
        match self.link.read(timeout, &*self.ctl)? {
            None => {
                if wait && Instant::now() >= self.last_activity + self.opts.timeout {
                    if let Some(idx) = self.win.iter().position(|s| !s.acked) {
                        let seq = self.win[idx].seq;
                        self.retransmit(idx, format!("timeout, resending packet {seq}"))?;
                    } else {
                        self.last_activity = Instant::now();
                    }
                }
                Ok(false)
            }
            Some(Parsed::Corrupt(why)) => {
                self.ctl.event(Event::Retry { reason: format!("bad packet from receiver: {why}") });
                Ok(true)
            }
            Some(Parsed::Packet(p)) => {
                match p.typ {
                    b'Y' => {
                        if let Some(idx) = self.win.iter().position(|s| s.seq == p.seq) {
                            let slot = &mut self.win[idx];
                            if !slot.acked {
                                slot.acked = true;
                                if slot.typ == b'D' && matches!(p.data.first(), Some(b'X' | b'Z')) {
                                    self.interrupt = Some(p.data[0]);
                                }
                                slot.ack_data = p.data;
                                self.last_activity = Instant::now();
                                self.fast_retransmit(idx)?;
                                self.retire();
                            }
                        }
                    }
                    b'N' => {
                        if let Some(idx) = self.win.iter().position(|s| s.seq == p.seq) {
                            if !self.win[idx].acked {
                                self.retransmit(idx, format!("NAK for packet {}", p.seq))?;
                            }
                        } else if p.seq == self.next_seq && !self.win.is_empty() {
                            // NAK for the packet after the window: the receiver
                            // has everything we sent.
                            for s in self.win.iter_mut() {
                                s.acked = true;
                            }
                            self.last_activity = Instant::now();
                            self.retire();
                        }
                    }
                    b'E' => {
                        self.remote_error = true;
                        return Err(XferError::Protocol(self.link.error_text(&p.data)));
                    }
                    _ => {}
                }
                Ok(true)
            }
        }
    }
}

/// Read from `file` until `buf` holds `target` bytes or EOF. Returns true at EOF.
fn fill(file: &mut File, buf: &mut Vec<u8>, target: usize) -> Result<bool> {
    let mut chunk = vec![0u8; 64 * 1024];
    while buf.len() < target {
        match file.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

/// Attribute packet data: file type binary, size in K and exact size.
/// (Attribute packets are not prefix-encoded.)
fn attributes(size: Option<u64>) -> Vec<u8> {
    let mut a = Vec::new();
    let mut put = |tag: u8, val: &str| {
        a.push(tag);
        a.push(tochar(val.len() as u8));
        a.extend_from_slice(val.as_bytes());
    };
    put(b'"', "B8");
    if let Some(n) = size {
        put(b'!', &n.div_ceil(1024).to_string());
        put(b'1', &n.to_string());
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{frame, FrameParams, PacketReader};
    use xfer_common::{ChannelPort, RecordingControl};

    /// A scripted receiver that interrupts the first file with `X` and the
    /// second with `Z`; returns the (type, data) of every packet it saw.
    fn scripted_receiver(mut port: ChannelPort) -> Vec<(u8, Vec<u8>)> {
        let opts = KermitOptions::default();
        let mut rd = PacketReader::new();
        let mut seen = Vec::new();
        let mut fp = FrameParams::default();
        let mut files = 0;
        let mut interrupted = false;
        loop {
            let Some(Parsed::Packet(p)) = rd.read(&mut port, Duration::from_secs(5), fp.chk, &|| false).unwrap() else {
                panic!("receiver timed out");
            };
            seen.push((p.typ, p.data.clone()));
            let reply: Vec<u8> = match p.typ {
                b'S' => {
                    let theirs = InitParams::parse(&p.data);
                    let mine = InitParams::for_receiver(&opts, &theirs);
                    port.write_all(&frame(&fp, p.seq, b'Y', &mine.encode())).unwrap();
                    fp = negotiate(&mine, &theirs, false).frame;
                    continue;
                }
                b'F' => {
                    files += 1;
                    interrupted = false;
                    Vec::new()
                }
                b'A' => b"Y".to_vec(),
                b'D' if !interrupted => {
                    interrupted = true;
                    if files == 1 {
                        b"X".to_vec()
                    } else {
                        b"Z".to_vec()
                    }
                }
                _ => Vec::new(),
            };
            port.write_all(&frame(&fp, p.seq, b'Y', &reply)).unwrap();
            if p.typ == b'B' {
                return seen;
            }
        }
    }

    #[test]
    fn receiver_interrupts_file_and_batch() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("f{i}"));
                std::fs::write(&p, vec![i as u8 + 1; 100_000]).unwrap();
                p
            })
            .collect();
        let (mut a, b) = ChannelPort::pair();
        let rx = std::thread::spawn(move || scripted_receiver(b));
        let mut ctl = RecordingControl::default();
        let opts = KermitOptions { window: 4, max_packet_len: 300, repeat_counts: false, ..KermitOptions::default() };
        let summary = send(&mut a, &mut ctl, &files, &opts).unwrap();
        let seen = rx.join().unwrap();
        assert!(summary.files.is_empty());
        let skipped: Vec<&str> = ctl
            .events
            .iter()
            .filter_map(|e| match e {
                Event::FileSkipped { reason, .. } => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(skipped.len(), 3, "{skipped:?}");
        assert!(skipped[0].contains("skipped") && skipped[1].contains("batch") && skipped[2].contains("batch"));
        // Two discarding EOFs, only two file headers, then end of batch.
        let zs: Vec<&[u8]> = seen.iter().filter(|(t, _)| *t == b'Z').map(|(_, d)| d.as_slice()).collect();
        assert_eq!(zs, [b"D".as_slice(), b"D".as_slice()]);
        assert_eq!(seen.iter().filter(|(t, _)| *t == b'F').count(), 2);
        assert_eq!(seen.last().unwrap().0, b'B');
        // The window was stopped early: far fewer data packets than a full file.
        assert!(seen.iter().filter(|(t, _)| *t == b'D').count() < 20);
    }
}
