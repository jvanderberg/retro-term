//! Spotting a remote `kermit -s` in the terminal's incoming byte stream.

use crate::packet::{check1, unchar, SOH};

/// Streaming detector for a Kermit Send-Init packet.
///
/// Feed it every byte received from the remote. It returns `true` once a
/// complete, checksum-valid `S` packet has gone past; the packet bytes are
/// then available from [`Detector::packet`] (pass them to
/// [`crate::recv_with_init`] to skip the resend round trip).
#[derive(Debug, Default, Clone)]
pub struct Detector {
    buf: Vec<u8>,
    last: Vec<u8>,
}

impl Detector {
    pub fn new() -> Self {
        Self::default()
    }

    /// The most recently detected Send-Init packet (from SOH through check).
    pub fn packet(&self) -> &[u8] {
        &self.last
    }

    pub fn reset(&mut self) {
        self.buf.clear();
    }

    pub fn feed(&mut self, byte: u8) -> bool {
        if byte == SOH {
            self.buf.clear();
            self.buf.push(byte);
            return false;
        }
        if self.buf.is_empty() {
            return false;
        }
        if !(32..=126).contains(&byte) {
            self.buf.clear();
            return false;
        }
        self.buf.push(byte);
        match self.buf.len() {
            // LEN: a Send-Init is a short, normal packet.
            2 if !(5..=94).contains(&unchar(byte)) => self.buf.clear(),
            // SEQ: Send-Init is packet 0.
            3 if byte != b' ' => self.buf.clear(),
            4 if byte != b'S' => self.buf.clear(),
            n if n >= 4 => {
                let len = unchar(self.buf[1]) as usize;
                if n == len + 2 {
                    let ok = check1(&self.buf[1..n - 1]) == self.buf[n - 1];
                    if ok {
                        self.last = std::mem::take(&mut self.buf);
                    }
                    self.buf.clear();
                    return ok;
                }
            }
            _ => {}
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{frame, FrameParams};
    use crate::params::InitParams;
    use crate::KermitOptions;

    fn feed_all(d: &mut Detector, bytes: &[u8]) -> usize {
        bytes.iter().filter(|&&b| d.feed(b)).count()
    }

    #[test]
    fn detects_send_init_amid_noise() {
        let s = frame(&FrameParams::default(), 0, b'S', &InitParams::for_sender(&KermitOptions::default()).encode());
        let mut d = Detector::new();
        let mut stream = b"$ kermit -s foo.bin\r\n\x1b[0m".to_vec();
        stream.extend_from_slice(&s);
        assert_eq!(feed_all(&mut d, &stream), 1);
        assert_eq!(d.packet(), &s[..s.len() - 1]);
        // Byte by byte with junk in front.
        let mut d = Detector::new();
        assert_eq!(feed_all(&mut d, b"\x01\x01garbage\x01"), 0);
        assert_eq!(feed_all(&mut d, &s), 1);
    }

    #[test]
    fn ignores_other_packets_and_bad_checksums() {
        let fp = FrameParams::default();
        let mut d = Detector::new();
        assert_eq!(feed_all(&mut d, &frame(&fp, 0, b'Y', b"~* @-#Y3~")), 0);
        assert_eq!(feed_all(&mut d, &frame(&fp, 1, b'S', b"~* @-#Y3~")), 0);
        let mut s = frame(&fp, 0, b'S', b"~* @-#Y3~");
        s[6] ^= 1;
        assert_eq!(feed_all(&mut d, &s), 0);
        assert_eq!(feed_all(&mut d, b"plain text with ^A in it \x01 S nope"), 0);
    }
}
