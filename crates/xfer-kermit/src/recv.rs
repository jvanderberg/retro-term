//! File receiver with a selective-repeat receive window.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use xfer_common::{create_dest_file, Control, Event, Port, ProgressThrottle, RecvOptions, Result, Summary, XferError};

use crate::link::Link;
use crate::packet::{unchar, Packet, PacketReader, Parsed};
use crate::params::{negotiate, InitParams};
use crate::KermitOptions;

struct OpenFile {
    name: String,
    path: PathBuf,
    out: BufWriter<File>,
    bytes: u64,
    size: Option<u64>,
    announced: bool,
}

struct Receiver<'a, 'b> {
    link: Link<'a>,
    ctl: &'b mut dyn Control,
    opts: &'b KermitOptions,
    ropts: &'b RecvOptions,
    win_size: usize,
    /// Oldest sequence number not yet received.
    rlow: u8,
    /// Packets received ahead of `rlow`.
    held: Vec<Option<Packet>>,
    /// Last ACK frame sent for each sequence number (for re-ACKing duplicates).
    acks: Vec<Option<Vec<u8>>>,
    /// Gaps we have already NAKed since they opened.
    nakked: [bool; 64],
    retries: u32,
    file: Option<OpenFile>,
    summary: Summary,
    throttle: ProgressThrottle,
    done: bool,
    remote_error: bool,
    scratch: Vec<u8>,
}

pub fn recv(
    port: &mut dyn Port,
    ctl: &mut dyn Control,
    ropts: &RecvOptions,
    opts: &KermitOptions,
    init_packet: Option<&[u8]>,
) -> Result<Summary> {
    let mut r = Receiver {
        link: Link::new(port),
        ctl,
        opts,
        ropts,
        win_size: 1,
        rlow: 0,
        held: vec![None; 64],
        acks: vec![None; 64],
        nakked: [false; 64],
        retries: 0,
        file: None,
        summary: Summary::default(),
        throttle: ProgressThrottle::default(),
        done: false,
        remote_error: false,
        scratch: Vec::new(),
    };
    let res = r.run(init_packet);
    if let Err(e) = &res {
        r.discard_partial();
        if !r.remote_error {
            let msg = match e {
                XferError::Cancelled => "Cancelled by user".to_string(),
                other => other.to_string(),
            };
            let seq = r.rlow;
            r.link.send_error(seq, &msg);
        }
    }
    res.map(|()| r.summary)
}

impl Receiver<'_, '_> {
    fn run(&mut self, init_packet: Option<&[u8]>) -> Result<()> {
        self.wait_init(init_packet)?;
        while !self.done {
            self.check_cancel()?;
            match self.link.read(self.opts.timeout, &*self.ctl)? {
                None => {
                    self.bump_retry("timeout waiting for packet")?;
                    let seq = self.rlow;
                    self.nak(seq)?;
                }
                Some(Parsed::Corrupt(why)) => {
                    self.bump_retry(&why)?;
                    let seq = self.rlow;
                    self.nak(seq)?;
                }
                Some(Parsed::Packet(p)) => self.on_packet(p)?,
            }
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<()> {
        if self.ctl.cancelled() {
            Err(XferError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn bump_retry(&mut self, reason: &str) -> Result<()> {
        self.retries += 1;
        if self.retries > self.opts.retry_limit {
            return Err(XferError::TooManyErrors(reason.to_string()));
        }
        self.ctl.event(Event::Retry { reason: reason.to_string() });
        Ok(())
    }

    fn nak(&mut self, seq: u8) -> Result<()> {
        let f = self.link.frame(seq, b'N', b"");
        self.link.write(&f)
    }

    fn ack(&mut self, seq: u8, data: &[u8]) -> Result<()> {
        let f = self.link.frame(seq, b'Y', data);
        self.link.write(&f)?;
        self.acks[seq as usize] = Some(f);
        Ok(())
    }

    fn wait_init(&mut self, init_packet: Option<&[u8]>) -> Result<()> {
        if let Some(bytes) = init_packet {
            let mut rd = PacketReader::new();
            rd.push(bytes);
            if let Some(Parsed::Packet(p)) = rd.parse(1) {
                if p.typ == b'S' {
                    return self.accept_init(&p);
                }
            }
        }
        if self.opts.initial_nak || init_packet.is_some() {
            self.nak(0)?;
        }
        loop {
            self.check_cancel()?;
            match self.link.read(self.opts.timeout, &*self.ctl)? {
                Some(Parsed::Packet(p)) if p.typ == b'S' => return self.accept_init(&p),
                Some(Parsed::Packet(p)) if p.typ == b'E' => {
                    self.remote_error = true;
                    return Err(XferError::Protocol(self.link.error_text(&p.data)));
                }
                Some(Parsed::Packet(_)) => {}
                Some(Parsed::Corrupt(why)) => {
                    self.bump_retry(&why)?;
                    self.nak(0)?;
                }
                None => {
                    self.retries += 1;
                    if self.retries > self.opts.retry_limit {
                        return Err(XferError::Timeout);
                    }
                    self.ctl.event(Event::Retry { reason: "waiting for Send-Init".into() });
                    self.nak(0)?;
                }
            }
        }
    }

    fn accept_init(&mut self, p: &Packet) -> Result<()> {
        let theirs = InitParams::parse(&p.data);
        let mine = InitParams::for_receiver(self.opts, &theirs);
        // The ACK to a Send-Init always carries a type-1 check.
        let f = self.link.frame_chk(1, p.seq, b'Y', &mine.encode());
        self.link.write(&f)?;
        self.acks[p.seq as usize] = Some(f);
        let neg = negotiate(&mine, &theirs, false);
        self.link.apply(&neg);
        self.win_size = neg.window.clamp(1, 31);
        self.rlow = (p.seq + 1) % 64;
        self.retries = 0;
        self.ctl.event(Event::Message(format!(
            "Kermit: block check {}, window {}{}{}",
            neg.chk,
            self.win_size,
            if neg.dec.qbin.is_some() { ", 8th-bit prefixing" } else { "" },
            if neg.dec.rept.is_some() { ", repeat counts" } else { "" },
        )));
        Ok(())
    }

    fn on_packet(&mut self, p: Packet) -> Result<()> {
        if p.typ == b'E' {
            self.remote_error = true;
            return Err(XferError::Protocol(self.link.error_text(&p.data)));
        }
        if matches!(p.typ, b'Y' | b'N') {
            return Ok(());
        }
        let ahead = ((p.seq as usize) + 64 - self.rlow as usize) % 64;
        let behind = ((self.rlow as usize) + 64 - p.seq as usize) % 64;
        if ahead < self.win_size {
            self.nakked[p.seq as usize] = false;
            if ahead == 0 {
                self.retries = 0;
                let reply = self.process(&p)?;
                self.ack(p.seq, &reply)?;
                self.advance();
                while !self.done {
                    let Some(q) = self.held[self.rlow as usize].take() else { break };
                    self.process(&q)?;
                    self.advance();
                }
            } else {
                if self.held[p.seq as usize].is_none() {
                    // NAK any gaps below this packet we have not asked for yet.
                    for d in 0..ahead {
                        let s = ((self.rlow as usize + d) % 64) as u8;
                        if self.held[s as usize].is_none() && !self.nakked[s as usize] {
                            self.nakked[s as usize] = true;
                            self.nak(s)?;
                        }
                    }
                    self.held[p.seq as usize] = Some(p.clone());
                }
                self.ack(p.seq, b"")?;
            }
        } else if (1..=self.win_size.max(1)).contains(&behind) {
            // A retransmission of something already handled: our ACK was lost.
            match self.acks[p.seq as usize].clone() {
                Some(f) => self.link.write(&f)?,
                None => self.ack(p.seq, b"")?,
            }
        }
        Ok(())
    }

    fn advance(&mut self) {
        self.rlow = (self.rlow + 1) % 64;
    }

    /// Act on an in-sequence packet; returns the data for its ACK.
    fn process(&mut self, p: &Packet) -> Result<Vec<u8>> {
        match p.typ {
            b'F' => {
                self.finish_file(false)?;
                let raw = self.decode(&p.data)?;
                let name = String::from_utf8_lossy(&raw).into_owned();
                let (path, f) = create_dest_file(&self.ropts.dest_dir, &name, self.ropts.overwrite)?;
                self.file = Some(OpenFile {
                    name,
                    path,
                    out: BufWriter::with_capacity(256 * 1024, f),
                    bytes: 0,
                    size: None,
                    announced: false,
                });
                Ok(Vec::new())
            }
            b'A' => {
                let size = parse_size(&p.data);
                if let Some(f) = self.file.as_mut() {
                    f.size = size.or(f.size);
                }
                self.announce();
                Ok(b"Y".to_vec())
            }
            b'D' => {
                self.announce();
                let mut buf = std::mem::take(&mut self.scratch);
                buf.clear();
                if self.link.dec.decode(&p.data, &mut buf).is_err() {
                    return Err(XferError::Protocol("malformed data packet".into()));
                }
                let f = self
                    .file
                    .as_mut()
                    .ok_or_else(|| XferError::Protocol("data packet without a file header".into()))?;
                f.out.write_all(&buf)?;
                f.bytes += buf.len() as u64;
                let bytes = f.bytes;
                self.scratch = buf;
                self.throttle.report(self.ctl, bytes, false);
                Ok(Vec::new())
            }
            b'Z' => {
                self.announce();
                self.finish_file(p.data.first() == Some(&b'D'))?;
                Ok(Vec::new())
            }
            b'B' => {
                self.finish_file(false)?;
                self.done = true;
                Ok(Vec::new())
            }
            _ => Ok(Vec::new()),
        }
    }

    fn decode(&self, data: &[u8]) -> Result<Vec<u8>> {
        self.link.dec.decode_vec(data).map_err(|_| XferError::Protocol("malformed packet data".into()))
    }

    fn announce(&mut self) {
        if let Some(f) = self.file.as_mut() {
            if !f.announced {
                f.announced = true;
                let ev = Event::FileStart { name: f.name.clone(), size: f.size };
                self.ctl.event(ev);
            }
        }
    }

    fn finish_file(&mut self, discard: bool) -> Result<()> {
        let Some(mut f) = self.file.take() else { return Ok(()) };
        if discard {
            drop(f.out);
            let _ = std::fs::remove_file(&f.path);
            self.ctl.event(Event::FileSkipped { name: f.name, reason: "sender discarded the file".into() });
            return Ok(());
        }
        f.out.flush()?;
        self.throttle.report(self.ctl, f.bytes, true);
        self.ctl.event(Event::FileDone { name: f.name.clone(), bytes: f.bytes, path: f.path.clone() });
        self.summary.files.push((f.name, f.bytes, f.path));
        Ok(())
    }

    fn discard_partial(&mut self) {
        if let Some(f) = self.file.take() {
            drop(f.out);
            let _ = std::fs::remove_file(&f.path);
        }
    }
}

/// Exact file length from attribute `1` (falling back to `!`, in K).
fn parse_size(data: &[u8]) -> Option<u64> {
    let mut i = 0;
    let mut exact = None;
    let mut kilo = None;
    while i + 1 < data.len() {
        let tag = data[i];
        let len = unchar(data[i + 1]) as usize;
        let end = (i + 2 + len).min(data.len());
        let val = std::str::from_utf8(&data[i + 2..end]).ok().and_then(|s| s.trim().parse::<u64>().ok());
        match tag {
            b'1' => exact = val,
            b'!' => kilo = val.and_then(|k| k.checked_mul(1024)),
            _ => {}
        }
        i = end;
    }
    exact.or(kilo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_attribute() {
        assert_eq!(parse_size(b"\"\"B8!\"101%10000"), Some(10000));
        assert_eq!(parse_size(b"!#123"), Some(123 * 1024));
        assert_eq!(parse_size(b""), None);
    }
}
