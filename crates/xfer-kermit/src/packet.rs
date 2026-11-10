//! Kermit packet framing: building packets and parsing them out of a byte
//! stream.
//!
//! ```text
//! normal:   MARK LEN SEQ TYPE DATA... CHECK EOL
//! extended: MARK ' ' SEQ TYPE LENX1 LENX2 HCHECK DATA... CHECK EOL
//! ```
//!
//! `LEN` counts the bytes from `SEQ` through the end of `CHECK`. In an
//! extended ("long") packet `LEN` is `tochar(0)` and `LENX1*95+LENX2` counts
//! the bytes of `DATA` plus `CHECK`; `HCHECK` is a type-1 checksum of the
//! five header bytes `LEN..LENX2`.

use std::io;
use std::time::{Duration, Instant};

use xfer_common::Port;

/// Start-of-packet mark (Ctrl-A).
pub const SOH: u8 = 0x01;
/// The longest extended packet the length field can describe.
pub const MAX_LONG: usize = 95 * 95 - 1;
/// The longest normal packet (`LEN` field value).
pub const MAX_NORMAL: usize = 94;

/// Map a value 0..=94 to a printable character.
pub const fn tochar(x: u8) -> u8 {
    x.wrapping_add(32)
}

/// Inverse of [`tochar`].
pub const fn unchar(c: u8) -> u8 {
    c.wrapping_sub(32)
}

/// Toggle bit 6 ("controlify"): maps control characters to printables and back.
pub const fn ctl(c: u8) -> u8 {
    c ^ 64
}

/// Number of bytes a block check of type `chk` occupies.
pub const fn check_len(chk: u8) -> usize {
    match chk {
        2 => 2,
        3 => 3,
        _ => 1,
    }
}

/// Type-1 (6-bit folded) checksum of `bytes`, as a printable character.
pub fn check1(bytes: &[u8]) -> u8 {
    let s: u32 = bytes.iter().map(|&b| u32::from(b)).sum();
    tochar(((s + ((s & 0xC0) >> 6)) & 0x3F) as u8)
}

/// Append the block check of type `chk` computed over `bytes` to `out`.
pub fn push_check(chk: u8, bytes: &[u8], out: &mut Vec<u8>) {
    match chk {
        2 => {
            let s: u32 = bytes.iter().map(|&b| u32::from(b)).sum::<u32>() & 0x0FFF;
            out.push(tochar(((s >> 6) & 0x3F) as u8));
            out.push(tochar((s & 0x3F) as u8));
        }
        3 => {
            let crc = xfer_common::crc::crc16_kermit(bytes);
            out.push(tochar(((crc >> 12) & 0x0F) as u8));
            out.push(tochar(((crc >> 6) & 0x3F) as u8));
            out.push(tochar((crc & 0x3F) as u8));
        }
        _ => out.push(check1(bytes)),
    }
}

fn check_matches(chk: u8, bytes: &[u8], expect: &[u8]) -> bool {
    let mut got = Vec::with_capacity(3);
    push_check(chk, bytes, &mut got);
    got == expect
}

/// A decoded (but still prefix-encoded) packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub seq: u8,
    pub typ: u8,
    pub data: Vec<u8>,
}

/// Framing parameters for outgoing packets.
#[derive(Debug, Clone, Copy)]
pub struct FrameParams {
    /// Block check type (1, 2 or 3).
    pub chk: u8,
    /// Character to end each packet with (the remote's requested EOL).
    pub eol: u8,
    /// Padding characters to send before each packet.
    pub npad: u8,
    pub padc: u8,
    /// Largest normal-packet `LEN` the remote accepts; longer packets are
    /// sent in extended format.
    pub normal_max: usize,
}

impl Default for FrameParams {
    fn default() -> Self {
        Self { chk: 1, eol: b'\r', npad: 0, padc: 0, normal_max: MAX_NORMAL }
    }
}

/// Build a complete packet ready to write to the line.
pub fn frame(fp: &FrameParams, seq: u8, typ: u8, data: &[u8]) -> Vec<u8> {
    let chklen = check_len(fp.chk);
    let mut p = Vec::with_capacity(fp.npad as usize + data.len() + 12);
    p.extend(std::iter::repeat_n(fp.padc, fp.npad as usize));
    p.push(SOH);
    let start = p.len();
    let len = 2 + data.len() + chklen;
    if len <= fp.normal_max.min(MAX_NORMAL) {
        p.push(tochar(len as u8));
        p.push(tochar(seq & 63));
        p.push(typ);
    } else {
        let lx = (data.len() + chklen).min(MAX_LONG);
        p.push(tochar(0));
        p.push(tochar(seq & 63));
        p.push(typ);
        p.push(tochar((lx / 95) as u8));
        p.push(tochar((lx % 95) as u8));
        let h = check1(&p[start..start + 5]);
        p.push(h);
    }
    p.extend_from_slice(data);
    let body = p[start..].to_vec();
    push_check(fp.chk, &body, &mut p);
    p.push(fp.eol);
    p
}

/// Result of pulling one packet out of the input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Packet(Packet),
    /// Something that looked like a packet but failed validation.
    Corrupt(String),
}

/// Incremental packet parser over a [`Port`].
pub struct PacketReader {
    buf: Vec<u8>,
    start: usize,
    tmp: Vec<u8>,
}

impl Default for PacketReader {
    fn default() -> Self {
        Self::new()
    }
}

enum Step {
    NeedMore,
    Skip(usize, Option<String>),
    Done(usize, Parsed),
}

impl PacketReader {
    pub fn new() -> Self {
        Self { buf: Vec::new(), start: 0, tmp: vec![0u8; 32 * 1024] }
    }

    /// Feed raw bytes into the parser.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 && (self.start >= self.buf.len() || self.start > 64 * 1024) {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(bytes);
    }

    /// Try to extract the next packet from buffered bytes. `chk` is the block
    /// check currently in force; Send-Init/Init/Error packets and ACK/NAK of
    /// packet 0 are also accepted with a type-1 check (they may predate the
    /// negotiation).
    pub fn parse(&mut self, chk: u8) -> Option<Parsed> {
        loop {
            let s = &self.buf[self.start..];
            let Some(i) = s.iter().position(|&b| b == SOH) else {
                self.buf.clear();
                self.start = 0;
                return None;
            };
            self.start += i;
            match Self::step(&self.buf[self.start..], chk) {
                Step::NeedMore => return None,
                Step::Skip(n, why) => {
                    self.start += n;
                    if let Some(w) = why {
                        return Some(Parsed::Corrupt(w));
                    }
                }
                Step::Done(n, p) => {
                    self.start += n;
                    return Some(p);
                }
            }
        }
    }

    /// `s` starts with SOH.
    fn step(s: &[u8], chk: u8) -> Step {
        // A fresh SOH inside the packet span means the packet was truncated:
        // resynchronise on it.
        let resync = |upto: usize| -> Option<Step> {
            let end = upto.min(s.len());
            s[1..end].iter().position(|&b| b == SOH).map(|p| Step::Skip(p + 1, Some("truncated packet".into())))
        };
        if s.len() < 2 {
            return Step::NeedMore;
        }
        let lenc = s[1];
        if !(32..=126).contains(&lenc) {
            return Step::Skip(1, if lenc == SOH { None } else { Some("bad LEN field".into()) });
        }
        let len = unchar(lenc) as usize;
        if len == 0 {
            // Extended packet.
            if s.len() < 7 {
                return resync(7).unwrap_or(Step::NeedMore);
            }
            if let Some(r) = resync(7) {
                return r;
            }
            if check1(&s[1..6]) != s[6] {
                return Step::Skip(1, Some("bad extended header checksum".into()));
            }
            let lx = unchar(s[4]) as usize * 95 + unchar(s[5]) as usize;
            let total = 7 + lx;
            if let Some(r) = resync(total) {
                return r;
            }
            if s.len() < total {
                return Step::NeedMore;
            }
            match Self::validate(s, 7, total, chk) {
                Some(p) => Step::Done(total, Parsed::Packet(p)),
                None => Step::Skip(total, Some("block check error".into())),
            }
        } else if len < 3 {
            Step::Skip(1, Some("bad LEN field".into()))
        } else {
            let total = 2 + len;
            if let Some(r) = resync(total) {
                return r;
            }
            if s.len() < total {
                return Step::NeedMore;
            }
            match Self::validate(s, 4, total, chk) {
                Some(p) => Step::Done(total, Parsed::Packet(p)),
                None => Step::Skip(total, Some("block check error".into())),
            }
        }
    }

    /// Validate a complete packet `s[..total]` whose data starts at `data_at`.
    fn validate(s: &[u8], data_at: usize, total: usize, chk: u8) -> Option<Packet> {
        let seqc = s[2];
        let typ = s[3];
        if !(32..=95).contains(&seqc) || !(33..=126).contains(&typ) {
            return None;
        }
        let try_chk = |c: u8| -> Option<Packet> {
            let cl = check_len(c);
            if total < data_at + cl {
                return None;
            }
            let body_end = total - cl;
            if check_matches(c, &s[1..body_end], &s[body_end..total]) {
                Some(Packet { seq: unchar(seqc), typ, data: s[data_at..body_end].to_vec() })
            } else {
                None
            }
        };
        // Send-Init exchanges always use a type-1 check, and duplicates of them
        // (or of their ACK / NAK) can arrive after the switch; so can an error
        // packet from a side that had not finished negotiating.
        let init_era = matches!(typ, b'S' | b'I' | b'E') || (matches!(typ, b'Y' | b'N') && seqc == b' ');
        try_chk(chk).or_else(|| if chk != 1 && init_era { try_chk(1) } else { None })
    }

    /// Wait up to `timeout` for the next packet. `Ok(None)` on timeout or
    /// when `cancelled` turns true. The deadline holds even while a stream of
    /// bytes that never forms a packet keeps arriving.
    pub fn read(
        &mut self,
        port: &mut dyn Port,
        timeout: Duration,
        chk: u8,
        cancelled: &dyn Fn() -> bool,
    ) -> io::Result<Option<Parsed>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(p) = self.parse(chk) {
                return Ok(Some(p));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || cancelled() {
                return Ok(None);
            }
            let n = port.read_timeout(&mut self.tmp, remaining.min(Duration::from_millis(100)))?;
            if n > 0 {
                let chunk = self.tmp[..n].to_vec();
                self.push(&chunk);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(bytes: &[u8], chk: u8) -> Option<Parsed> {
        let mut r = PacketReader::new();
        r.push(bytes);
        r.parse(chk)
    }

    #[test]
    fn checks() {
        // "#" (LEN 3) " " (seq 0) "N" -> NAK 0 with type-1 check.
        let fp = FrameParams::default();
        let p = frame(&fp, 0, b'N', b"");
        assert_eq!(p, b"\x01# N3\r");
        assert_eq!(check1(b"# N"), b'3');
    }

    #[test]
    fn roundtrip_all_check_types_and_lengths() {
        for chk in 1..=3u8 {
            for len in [0usize, 1, 50, 88, 89, 90, 91, 92, 200, 4000, 9000] {
                let data: Vec<u8> = (0..len).map(|i| b' ' + (i % 90) as u8).collect();
                let fp = FrameParams { chk, ..FrameParams::default() };
                let seq = (len % 64) as u8;
                let bytes = frame(&fp, seq, b'D', &data);
                let got = parse_one(&bytes, chk).expect("packet");
                assert_eq!(got, Parsed::Packet(Packet { seq, typ: b'D', data: data.clone() }), "chk {chk} len {len}");
            }
        }
    }

    #[test]
    fn detects_corruption_and_resyncs() {
        let fp = FrameParams { chk: 3, ..FrameParams::default() };
        let mut a = frame(&fp, 5, b'D', b"hello world");
        a[8] ^= 1;
        let b = frame(&fp, 6, b'D', b"second");
        let mut r = PacketReader::new();
        r.push(b"noise\r\n");
        r.push(&a);
        r.push(&b);
        assert!(matches!(r.parse(3), Some(Parsed::Corrupt(_))));
        assert_eq!(r.parse(3), Some(Parsed::Packet(Packet { seq: 6, typ: b'D', data: b"second".to_vec() })));
        assert_eq!(r.parse(3), None);

        // A truncated packet followed by a good one.
        let mut r = PacketReader::new();
        let c = frame(&fp, 7, b'D', b"truncated packet here");
        r.push(&c[..10]);
        assert_eq!(r.parse(3), None);
        r.push(&b);
        assert!(matches!(r.parse(3), Some(Parsed::Corrupt(_))));
        assert!(matches!(r.parse(3), Some(Parsed::Packet(_))));
    }

    #[test]
    fn send_init_accepted_with_type1_after_negotiation() {
        let fp = FrameParams::default();
        let s = frame(&fp, 0, b'S', b"~* @-#Y3~");
        assert!(matches!(parse_one(&s, 3), Some(Parsed::Packet(Packet { typ: b'S', .. }))));
        let d = frame(&fp, 1, b'D', b"abc");
        assert!(matches!(parse_one(&d, 3), Some(Parsed::Corrupt(_))));
    }

    #[test]
    fn respects_normal_max() {
        let fp = FrameParams { chk: 1, normal_max: 40, ..FrameParams::default() };
        let p = frame(&fp, 1, b'D', &[b'a'; 60]);
        assert_eq!(p[1], tochar(0), "should use extended format");
        assert!(matches!(parse_one(&p, 1), Some(Parsed::Packet(_))));
    }
}
