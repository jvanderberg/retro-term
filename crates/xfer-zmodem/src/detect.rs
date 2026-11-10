//! Auto-start detection: spot a remote `sz`'s ZRQINIT or a remote `rz`'s
//! ZRINIT in the normal terminal byte stream.

use xfer_common::crc::crc16;

use crate::consts::{ZDLE, ZHEX, ZPAD, ZRINIT, ZRQINIT};

/// What the remote side started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detected {
    /// Saw ZRQINIT: the remote `sz` started; run [`crate::recv()`].
    RemoteSending,
    /// Saw ZRINIT: the remote `rz` is waiting; pick files and run
    /// [`crate::send()`].
    RemoteReceiving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Pad1,
    Pads,
    Zdle,
    Hex,
}

/// Streaming detector for ZMODEM hex headers (`** ZDLE B tt dddddddd cccc`).
/// The full header including its CRC is validated, so ordinary terminal
/// output never triggers it.
#[derive(Debug, Clone)]
pub struct Detector {
    state: State,
    digits: [u8; 14],
    n: usize,
}

impl Default for Detector {
    fn default() -> Self {
        Self::new()
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl Detector {
    pub fn new() -> Self {
        Self { state: State::Idle, digits: [0; 14], n: 0 }
    }

    /// Feed one incoming byte; returns `Some` right after the last byte of a
    /// valid ZRQINIT/ZRINIT header.
    pub fn feed(&mut self, byte: u8) -> Option<Detected> {
        let c = byte & 0x7f;
        self.state = match self.state {
            State::Idle | State::Pad1 if c == ZPAD => {
                if self.state == State::Idle {
                    State::Pad1
                } else {
                    State::Pads
                }
            }
            State::Pads if c == ZPAD => State::Pads,
            State::Pads if c == ZDLE => State::Zdle,
            State::Zdle if c == ZHEX => {
                self.n = 0;
                State::Hex
            }
            State::Hex => match hex_val(c) {
                Some(v) => {
                    self.digits[self.n] = v;
                    self.n += 1;
                    if self.n == self.digits.len() {
                        self.state = State::Idle;
                        return self.decode();
                    }
                    State::Hex
                }
                None => Self::restart(c),
            },
            _ => Self::restart(c),
        };
        None
    }

    fn restart(c: u8) -> State {
        if c == ZPAD {
            State::Pad1
        } else {
            State::Idle
        }
    }

    fn decode(&self) -> Option<Detected> {
        let mut raw = [0u8; 7];
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = (self.digits[2 * i] << 4) | self.digits[2 * i + 1];
        }
        if u16::from_be_bytes([raw[5], raw[6]]) != crc16(&raw[..5]) {
            return None;
        }
        match raw[0] {
            ZRQINIT => Some(Detected::RemoteSending),
            ZRINIT => Some(Detected::RemoteReceiving),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(d: &mut Detector, bytes: &[u8]) -> Vec<(usize, Detected)> {
        bytes.iter().enumerate().filter_map(|(i, &b)| d.feed(b).map(|r| (i, r))).collect()
    }

    #[test]
    fn detects_lrzsz_zrqinit() {
        let mut d = Detector::new();
        let stream = b"rz\r**\x18B00000000000000\r\x8a\x11";
        let hits = feed_all(&mut d, stream);
        assert_eq!(hits, vec![(20, Detected::RemoteSending)]);
    }

    #[test]
    fn detects_zrinit_with_flags() {
        // lrzsz rz's ZRINIT: CANFC32|CANFDX|CANOVIO|CANBRK?, CANVHDR.
        let mut d = Detector::new();
        let mut hdr = Vec::new();
        let raw = [ZRINIT, 0, 0, 0, 0x23];
        let crc = crc16(&raw).to_be_bytes();
        hdr.extend_from_slice(b"hello world ***\x18B");
        for b in raw.iter().chain(crc.iter()) {
            hdr.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        hdr.extend_from_slice(b"\r\n");
        let hits = feed_all(&mut d, &hdr);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, Detected::RemoteReceiving);
        // Uppercase hex is accepted too.
        let upper = String::from_utf8(hdr.clone()).unwrap().to_uppercase();
        let mut d = Detector::new();
        assert_eq!(feed_all(&mut d, upper.as_bytes()).len(), 1);
    }

    #[test]
    fn ignores_bad_crc_and_other_frames() {
        let mut d = Detector::new();
        assert!(feed_all(&mut d, b"**\x18B00000000000001\r\n").is_empty());
        // ZFIN header (type 08) is valid but not a trigger.
        let raw = [8u8, 0, 0, 0, 0];
        let crc = crc16(&raw).to_be_bytes();
        let mut s = b"**\x18B".to_vec();
        for b in raw.iter().chain(crc.iter()) {
            s.extend_from_slice(format!("{b:02x}").as_bytes());
        }
        assert!(feed_all(&mut d, &s).is_empty());
        // Plain text with asterisks.
        assert!(feed_all(&mut d, b"*** bold ** text *\x18 B00").is_empty());
    }

    #[test]
    fn recovers_after_interruption() {
        let mut d = Detector::new();
        let hits = feed_all(&mut d, b"**\x18B000**\x18B00000000000000");
        assert_eq!(hits.len(), 1);
        // Works repeatedly.
        let hits = feed_all(&mut d, b"**\x18B00000000000000**\x18B00000000000000");
        assert_eq!(hits.len(), 2);
    }
}
