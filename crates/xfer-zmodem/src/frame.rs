//! Low-level ZMODEM framing: ZDLE escaping, hex/binary headers and data
//! subpackets, on top of a buffered [`Port`].

use std::time::{Duration, Instant};

use xfer_common::crc::{crc16, crc16_update, crc32_update};
use xfer_common::{Control, Port, Result, XferError};

use crate::consts::*;

/// How often blocking reads wake up to check for cancellation.
const POLL_SLICE: Duration = Duration::from_millis(100);

/// Non-header bytes tolerated while hunting for a header before giving up
/// (so the caller can re-send its request). Large, because after an error the
/// sender may legitimately have a lot of stale data in flight.
const GARBAGE_LIMIT: usize = 4 << 20;

/// How a header was (or will be) encoded on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enc {
    Hex,
    Bin16,
    Bin32,
}

/// A decoded ZMODEM header: frame type plus four data bytes in wire order
/// (`d[0]` = ZP0 = ZF3 ... `d[3]` = ZP3 = ZF0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub d: [u8; 4],
    pub enc: Enc,
}

impl Header {
    /// The 32-bit position/count carried by position-type headers.
    pub fn pos(&self) -> u32 {
        u32::from_le_bytes(self.d)
    }
    /// ZF0 flag byte.
    pub fn zf0(&self) -> u8 {
        self.d[3]
    }
    /// Whether data subpackets following this header use CRC-32.
    pub fn crc32(&self) -> bool {
        self.enc == Enc::Bin32
    }
}

/// Header data bytes for a position.
pub fn pos_bytes(p: u32) -> [u8; 4] {
    p.to_le_bytes()
}

/// Header data bytes from the flag view (ZF0 is the last byte on the wire).
pub fn flag_bytes(zf0: u8, zf1: u8, zf2: u8, zf3: u8) -> [u8; 4] {
    [zf3, zf2, zf1, zf0]
}

/// Result of waiting for a header.
#[derive(Debug)]
pub enum Got {
    Hdr(Header),
    /// Something arrived but it was not a valid header.
    Bad(&'static str),
    Timeout,
}

/// Result of reading a data subpacket.
#[derive(Debug, PartialEq, Eq)]
pub enum Sub {
    /// Good subpacket; carries its terminator (ZCRCE/G/Q/W).
    Ok(u8),
    Bad(&'static str),
}

enum Zdl {
    Byte(u8),
    End(u8),
    Bad,
}

// Transmit escape tables: 0 = send as is, 1 = escape, 2 = escape only if the
// previous byte on the wire was '@' (the "CR after @" telenet rule).
//
// ^O (0x0F) and ^V (0x16) are always escaped too: lrzsz leaves IEXTEN set on
// its tty, and the macOS line discipline then swallows VDISCARD/VLNEXT even in
// raw mode. Every receiver decodes ZDLE-escaped control characters, so this
// costs two bytes in 256 and makes uploads to `rz` in a PTY reliable.
const fn tx_table(ctl: bool) -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if i & 0x60 != 0 {
            0
        } else {
            match i {
                0x18 | 0x11 | 0x13 | 0x91 | 0x93 | 0x10 | 0x90 | 0x0f | 0x16 => 1,
                0x0d | 0x8d => {
                    if ctl {
                        1
                    } else {
                        2
                    }
                }
                _ => {
                    if ctl {
                        1
                    } else {
                        0
                    }
                }
            }
        };
        i += 1;
    }
    t
}

// Receive tables: non-zero bytes need the slow path (ZDLE, flow control
// characters that must be ignored, and raw control characters when the
// sender promised to escape them).
const fn rx_table(ctl: bool) -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        if (ctl && i & 0x60 == 0) || matches!(i, 0x18 | 0x11 | 0x13 | 0x91 | 0x93) {
            t[i] = 1;
        }
        i += 1;
    }
    t
}

static TX_STD: [u8; 256] = tx_table(false);
static TX_CTL: [u8; 256] = tx_table(true);
static RX_STD: [u8; 256] = rx_table(false);
static RX_CTL: [u8; 256] = rx_table(true);

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// CRC-16 over `data` followed by `extra` (header body or subpacket + end).
fn crc16_of(data: &[u8], extra: &[u8]) -> u16 {
    crc16_update(crc16(data), extra)
}

/// Transmitted CRC-32 (complemented) over `data` followed by `extra`.
fn crc32_of(data: &[u8], extra: &[u8]) -> u32 {
    !crc32_update(crc32_update(0xFFFF_FFFF, data), extra)
}

/// A buffered ZMODEM link: the port, the control (for cancellation and
/// events), a read buffer and a reusable output frame buffer.
pub struct Link<'a> {
    port: &'a mut dyn Port,
    pub ctl: &'a mut dyn Control,
    rbuf: Box<[u8]>,
    rpos: usize,
    rlen: usize,
    out: Vec<u8>,
    tx_last: u8,
    tx_ctl: bool,
    rx_ctl: bool,
    can_run: u8,
    rx_total: u64,
    /// Default per-byte timeout.
    pub timeout: Duration,
}

impl<'a> Link<'a> {
    pub fn new(port: &'a mut dyn Port, ctl: &'a mut dyn Control, timeout: Duration) -> Self {
        Self {
            port,
            ctl,
            rbuf: vec![0u8; 64 * 1024].into_boxed_slice(),
            rpos: 0,
            rlen: 0,
            out: Vec::with_capacity(20 * 1024),
            tx_last: 0,
            tx_ctl: false,
            rx_ctl: false,
            can_run: 0,
            rx_total: 0,
            timeout,
        }
    }

    /// Escape all control characters on transmit (ESCCTL).
    pub fn set_tx_ctl(&mut self, on: bool) {
        self.tx_ctl = on;
    }

    /// Drop raw (unescaped) control characters on receive.
    pub fn set_rx_ctl(&mut self, on: bool) {
        self.rx_ctl = on;
    }

    // ---------------------------------------------------------------- input

    fn fill(&mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ctl.cancelled() {
                return Err(XferError::Cancelled);
            }
            let slice = deadline.saturating_duration_since(Instant::now()).min(POLL_SLICE);
            let n = self.port.read_timeout(&mut self.rbuf, slice)?;
            if n > 0 {
                self.rx_total += n as u64;
                self.rpos = 0;
                self.rlen = n;
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
        }
    }

    /// Total bytes consumed from the port so far.
    pub fn consumed(&self) -> u64 {
        self.rx_total - (self.rlen - self.rpos) as u64
    }

    /// Read one byte, waiting at most `timeout` (`XferError::Timeout`).
    #[inline]
    pub fn byte_t(&mut self, timeout: Duration) -> Result<u8> {
        if self.rpos >= self.rlen && !self.fill(timeout)? {
            return Err(XferError::Timeout);
        }
        let b = self.rbuf[self.rpos];
        self.rpos += 1;
        Ok(b)
    }

    /// Whether input is available right now (never blocks).
    pub fn poll_ready(&mut self) -> Result<bool> {
        if self.rpos < self.rlen {
            return Ok(true);
        }
        self.fill(Duration::ZERO)
    }

    /// Push back the byte just read.
    fn unread(&mut self) {
        debug_assert!(self.rpos > 0);
        self.rpos -= 1;
    }

    /// Count a CAN seen outside a frame; five in a row means the remote
    /// aborted.
    pub fn note_can(&mut self, is_can: bool) -> Result<()> {
        if is_can {
            self.can_run += 1;
            if self.can_run >= 5 {
                self.can_run = 0;
                return Err(XferError::RemoteCancelled);
            }
        } else {
            self.can_run = 0;
        }
        Ok(())
    }

    /// Read a 7-bit character, ignoring XON/XOFF (used for hex headers).
    fn noxrd7(&mut self, t: Duration) -> Result<u8> {
        loop {
            let c = self.byte_t(t)? & 0x7f;
            if c != XON && c != XOFF {
                return Ok(c);
            }
        }
    }

    /// Read one ZDLE-decoded byte.
    fn zdl(&mut self, t: Duration) -> Result<Zdl> {
        let tbl = if self.rx_ctl { &RX_CTL } else { &RX_STD };
        loop {
            let c = self.byte_t(t)?;
            if tbl[c as usize] == 0 {
                return Ok(Zdl::Byte(c));
            }
            if c == ZDLE {
                break;
            }
            // XON/XOFF, or a raw control character in ESCCTL mode: ignore.
        }
        loop {
            let mut c = self.byte_t(t)?;
            // ZDLE followed by four CANs is a remote abort.
            for _ in 0..3 {
                if c != CAN {
                    break;
                }
                c = self.byte_t(t)?;
            }
            return Ok(match c {
                CAN => return Err(XferError::RemoteCancelled),
                ZCRCE | ZCRCG | ZCRCQ | ZCRCW => Zdl::End(c),
                ZRUB0 => Zdl::Byte(0x7f),
                ZRUB1 => Zdl::Byte(0xff),
                0x11 | 0x13 | 0x91 | 0x93 => continue,
                _ if self.rx_ctl && c & 0x60 == 0 => continue,
                _ if c & 0x60 == 0x40 => Zdl::Byte(c ^ 0x40),
                _ => Zdl::Bad,
            });
        }
    }

    fn zdl_bytes<const N: usize>(&mut self, t: Duration) -> Result<Option<[u8; N]>> {
        let mut out = [0u8; N];
        for slot in out.iter_mut() {
            match self.zdl(t)? {
                Zdl::Byte(b) => *slot = b,
                Zdl::End(_) | Zdl::Bad => return Ok(None),
            }
        }
        Ok(Some(out))
    }

    /// Wait up to `timeout` for a header.
    pub fn read_header(&mut self, timeout: Duration) -> Result<Got> {
        match self.read_header_inner(timeout) {
            Err(XferError::Timeout) => Ok(Got::Timeout),
            other => other,
        }
    }

    fn read_header_inner(&mut self, t: Duration) -> Result<Got> {
        let mut garbage = 0usize;
        loop {
            let c = self.byte_t(t)?;
            self.note_can(c == CAN)?;
            if c & 0x7f != ZPAD {
                garbage += 1;
                if garbage > GARBAGE_LIMIT {
                    return Ok(Got::Bad("garbage instead of header"));
                }
                continue;
            }
            // One or more ZPADs, then ZDLE.
            let mut c = self.byte_t(t)?;
            while c & 0x7f == ZPAD {
                c = self.byte_t(t)?;
            }
            if c & 0x7f != ZDLE {
                self.note_can(false)?;
                garbage += 1;
                continue;
            }
            let f = self.byte_t(t)?;
            if f == CAN {
                // ZDLE + CAN: part of an abort sequence.
                self.can_run = 2;
                continue;
            }
            self.can_run = 0;
            return match f & 0x7f {
                ZBIN => self.read_bin_header(Enc::Bin16, t),
                ZBIN32 => self.read_bin_header(Enc::Bin32, t),
                ZHEX => self.read_hex_header(t),
                _ => {
                    garbage += 1;
                    continue;
                }
            };
        }
    }

    fn read_bin_header(&mut self, enc: Enc, t: Duration) -> Result<Got> {
        let Some(body) = self.zdl_bytes::<5>(t)? else {
            return Ok(Got::Bad("bad binary header"));
        };
        let ok = if enc == Enc::Bin32 {
            match self.zdl_bytes::<4>(t)? {
                Some(c) => u32::from_le_bytes(c) == crc32_of(&body, &[]),
                None => false,
            }
        } else {
            match self.zdl_bytes::<2>(t)? {
                Some(c) => u16::from_be_bytes(c) == crc16(&body),
                None => false,
            }
        };
        if !ok {
            return Ok(Got::Bad("header CRC error"));
        }
        Ok(Got::Hdr(Header { kind: body[0], d: [body[1], body[2], body[3], body[4]], enc }))
    }

    fn read_hex_header(&mut self, t: Duration) -> Result<Got> {
        let mut raw = [0u8; 7];
        for slot in raw.iter_mut() {
            let hi = hex_val(self.noxrd7(t)?);
            let lo = hex_val(self.noxrd7(t)?);
            match (hi, lo) {
                (Some(h), Some(l)) => *slot = (h << 4) | l,
                _ => return Ok(Got::Bad("bad hex header")),
            }
        }
        if u16::from_be_bytes([raw[5], raw[6]]) != crc16(&raw[..5]) {
            return Ok(Got::Bad("hex header CRC error"));
        }
        // Swallow the trailing CR LF so a following data subpacket starts
        // cleanly.
        let short = Duration::from_millis(100);
        if let Ok(c) = self.byte_t(short) {
            if c & 0x7f == b'\r' {
                if let Ok(c2) = self.byte_t(short) {
                    if c2 & 0x7f != b'\n' {
                        self.unread();
                    }
                }
            } else {
                self.unread();
            }
        }
        Ok(Got::Hdr(Header { kind: raw[0], d: [raw[1], raw[2], raw[3], raw[4]], enc: Enc::Hex }))
    }

    /// Read a data subpacket into `data` (cleared first). At most `max`
    /// payload bytes are accepted.
    pub fn read_subpacket(&mut self, crc32: bool, max: usize, data: &mut Vec<u8>) -> Result<Sub> {
        data.clear();
        let t = self.timeout;
        loop {
            // Fast path: copy a run of plain bytes straight from the buffer.
            let tbl = if self.rx_ctl { &RX_CTL } else { &RX_STD };
            let start = self.rpos;
            let avail = &self.rbuf[start..self.rlen];
            let run = avail.iter().position(|&b| tbl[b as usize] != 0).unwrap_or(avail.len());
            data.extend_from_slice(&avail[..run]);
            self.rpos = start + run;
            if data.len() > max {
                return Ok(Sub::Bad("data subpacket too long"));
            }
            match self.zdl(t) {
                Ok(Zdl::Byte(b)) => data.push(b),
                Ok(Zdl::End(end)) => return self.finish_subpacket(crc32, end, data, t),
                Ok(Zdl::Bad) => return Ok(Sub::Bad("bad ZDLE escape in data")),
                Err(XferError::Timeout) => return Ok(Sub::Bad("timeout in data subpacket")),
                Err(e) => return Err(e),
            }
        }
    }

    fn finish_subpacket(&mut self, crc32: bool, end: u8, data: &[u8], t: Duration) -> Result<Sub> {
        let ok = if crc32 {
            match self.zdl_bytes::<4>(t) {
                Ok(Some(c)) => u32::from_le_bytes(c) == crc32_of(data, &[end]),
                Ok(None) => false,
                Err(XferError::Timeout) => return Ok(Sub::Bad("timeout in data CRC")),
                Err(e) => return Err(e),
            }
        } else {
            match self.zdl_bytes::<2>(t) {
                Ok(Some(c)) => u16::from_be_bytes(c) == crc16_of(data, &[end]),
                Ok(None) => false,
                Err(XferError::Timeout) => return Ok(Sub::Bad("timeout in data CRC")),
                Err(e) => return Err(e),
            }
        };
        Ok(if ok { Sub::Ok(end) } else { Sub::Bad("data CRC error") })
    }

    /// Scan whatever has already arrived (without blocking) for a header.
    /// Used by the sender to watch the back channel while streaming. Raw
    /// XOFF pauses until XON (or timeout).
    pub fn poll_header(&mut self) -> Result<Option<Header>> {
        while self.poll_ready()? {
            let c = self.byte_t(Duration::ZERO)?;
            match c {
                ZPAD | 0xaa => {
                    self.unread();
                    return Ok(match self.read_header(self.timeout)? {
                        Got::Hdr(h) => Some(h),
                        Got::Bad(_) | Got::Timeout => None,
                    });
                }
                CAN => self.note_can(true)?,
                XOFF | 0x93 => {
                    self.note_can(false)?;
                    loop {
                        match self.byte_t(self.timeout) {
                            Ok(XON | 0x91) | Err(XferError::Timeout) => break,
                            Ok(b) => self.note_can(b == CAN)?,
                            Err(e) => return Err(e),
                        }
                    }
                }
                _ => self.note_can(false)?,
            }
        }
        Ok(None)
    }

    // --------------------------------------------------------------- output

    fn esc(&mut self, data: &[u8]) {
        let tbl = if self.tx_ctl { &TX_CTL } else { &TX_STD };
        let out = &mut self.out;
        out.reserve(data.len() + data.len() / 16 + 16);
        let mut start = 0;
        for (i, &b) in data.iter().enumerate() {
            let k = tbl[b as usize];
            if k == 0 {
                continue;
            }
            if k == 2 {
                let prev = if i > start { data[i - 1] } else { out.last().copied().unwrap_or(self.tx_last) };
                if prev & 0x7f != b'@' {
                    continue;
                }
            }
            out.extend_from_slice(&data[start..i]);
            out.push(ZDLE);
            out.push(b ^ 0x40);
            start = i + 1;
        }
        out.extend_from_slice(&data[start..]);
    }

    /// Queue a hex header.
    pub fn put_hex_header(&mut self, kind: u8, d: [u8; 4]) {
        let raw = [kind, d[0], d[1], d[2], d[3]];
        let crc = crc16(&raw).to_be_bytes();
        self.out.extend_from_slice(&[ZPAD, ZPAD, ZDLE, ZHEX]);
        for b in raw.iter().chain(crc.iter()) {
            self.out.push(HEX[(b >> 4) as usize]);
            self.out.push(HEX[(b & 15) as usize]);
        }
        self.out.extend_from_slice(&[b'\r', b'\n' | 0x80]);
        if kind != ZFIN && kind != ZACK {
            self.out.push(XON);
        }
    }

    /// Queue a binary header (CRC-16 or CRC-32).
    pub fn put_bin_header(&mut self, kind: u8, d: [u8; 4], crc32: bool) {
        let raw = [kind, d[0], d[1], d[2], d[3]];
        self.out.extend_from_slice(&[ZPAD, ZDLE, if crc32 { ZBIN32 } else { ZBIN }]);
        self.esc(&raw);
        if crc32 {
            self.esc(&crc32_of(&raw, &[]).to_le_bytes());
        } else {
            self.esc(&crc16(&raw).to_be_bytes());
        }
    }

    /// Queue a data subpacket ending with `end` (ZCRCE/G/Q/W).
    pub fn put_subpacket(&mut self, data: &[u8], end: u8, crc32: bool) {
        self.esc(data);
        self.out.push(ZDLE);
        self.out.push(end);
        if crc32 {
            self.esc(&crc32_of(data, &[end]).to_le_bytes());
        } else {
            self.esc(&crc16_of(data, &[end]).to_be_bytes());
        }
        if end == ZCRCW {
            self.out.push(XON);
        }
    }

    /// Queue raw bytes.
    pub fn put_raw(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
    }

    /// Write everything queued in one `write_all`.
    pub fn flush_out(&mut self) -> Result<()> {
        if let Some(&last) = self.out.last() {
            self.port.write_all(&self.out)?;
            self.port.flush()?;
            self.tx_last = last;
            self.out.clear();
        }
        Ok(())
    }

    /// Send a hex header immediately.
    pub fn send_hex(&mut self, kind: u8, d: [u8; 4]) -> Result<()> {
        self.put_hex_header(kind, d);
        self.flush_out()
    }

    /// Send the abort sequence (8 CAN + 8 backspace), ignoring errors.
    pub fn abort(&mut self) {
        self.out.clear();
        self.out.extend_from_slice(&[CAN; 8]);
        self.out.extend_from_slice(&[BS; 8]);
        let _ = self.flush_out();
        self.port.purge_input();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xfer_common::{ChannelPort, RecordingControl};

    const T: Duration = Duration::from_millis(500);

    fn with_links(f: impl FnOnce(&mut Link, &mut Link)) {
        let (mut a, mut b) = ChannelPort::pair();
        let mut ca = RecordingControl::default();
        let mut cb = RecordingControl::default();
        let mut la = Link::new(&mut a, &mut ca, T);
        let mut lb = Link::new(&mut b, &mut cb, T);
        f(&mut la, &mut lb);
    }

    #[test]
    fn headers_roundtrip_all_encodings() {
        with_links(|tx, rx| {
            for (i, enc) in [Enc::Hex, Enc::Bin16, Enc::Bin32].into_iter().enumerate() {
                for &d in &[[0u8; 4], [0x18, 0x11, 0x13, 0x0d], [0xff, 0x91, 0x40, 0x8d]] {
                    let kind = ZDATA + i as u8;
                    match enc {
                        Enc::Hex => tx.put_hex_header(kind, d),
                        Enc::Bin16 => tx.put_bin_header(kind, d, false),
                        Enc::Bin32 => tx.put_bin_header(kind, d, true),
                    }
                    tx.flush_out().unwrap();
                    match rx.read_header(T).unwrap() {
                        Got::Hdr(h) => assert_eq!(h, Header { kind, d, enc }),
                        other => panic!("{other:?}"),
                    }
                }
            }
        });
    }

    #[test]
    fn subpacket_roundtrip_all_bytes() {
        let data: Vec<u8> = (0..=255u8).chain((0..=255u8).rev()).chain(b"@\r@\x8d\r".iter().copied()).collect();
        for ctl in [false, true] {
            for crc32 in [false, true] {
                with_links(|tx, rx| {
                    tx.set_tx_ctl(ctl);
                    rx.set_rx_ctl(ctl);
                    for end in [ZCRCE, ZCRCG, ZCRCQ, ZCRCW] {
                        tx.put_subpacket(&data, end, crc32);
                        tx.flush_out().unwrap();
                        let mut got = Vec::new();
                        assert_eq!(rx.read_subpacket(crc32, 8192, &mut got).unwrap(), Sub::Ok(end));
                        assert_eq!(got, data);
                        if end == ZCRCW {
                            // Trailing XON is ignored by the next read.
                            tx.put_subpacket(b"x", ZCRCE, crc32);
                            tx.flush_out().unwrap();
                            assert_eq!(rx.read_subpacket(crc32, 8192, &mut got).unwrap(), Sub::Ok(ZCRCE));
                            assert_eq!(got, b"x");
                        }
                    }
                });
            }
        }
    }

    #[test]
    fn escaping_rules() {
        with_links(|tx, _rx| {
            tx.esc(b"a\r@\rb\x18\x11\x13\x91\x93\x10\x90\x7f\xff\x01");
            assert_eq!(tx.out, b"a\r@\x18Mb\x18X\x18Q\x18S\x18\xd1\x18\xd3\x18P\x18\xd0\x7f\xff\x01".to_vec());
            tx.out.clear();
            tx.set_tx_ctl(true);
            tx.esc(b"\x01a\x9f");
            assert_eq!(tx.out, b"\x18Aa\x18\xdf".to_vec());
        });
    }

    #[test]
    fn corrupt_subpacket_detected() {
        with_links(|tx, rx| {
            tx.put_subpacket(b"hello world", ZCRCW, true);
            let n = tx.out.len();
            tx.out[3] ^= 1;
            assert!(n > 3);
            tx.flush_out().unwrap();
            let mut got = Vec::new();
            assert_eq!(rx.read_subpacket(true, 8192, &mut got).unwrap(), Sub::Bad("data CRC error"));
        });
    }

    #[test]
    fn can_sequence_detected() {
        with_links(|tx, rx| {
            tx.put_raw(b"junk");
            tx.put_raw(&[CAN; 8]);
            tx.flush_out().unwrap();
            assert!(matches!(rx.read_header(T), Err(XferError::RemoteCancelled)));
        });
        with_links(|tx, rx| {
            tx.put_raw(b"abc");
            tx.put_raw(&[CAN; 5]);
            tx.flush_out().unwrap();
            let mut got = Vec::new();
            assert!(matches!(rx.read_subpacket(true, 100, &mut got), Err(XferError::RemoteCancelled)));
        });
    }
}
