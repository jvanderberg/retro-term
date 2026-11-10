//! Kermit data-field encoding: control prefixing, 8th-bit prefixing and
//! repeat-count compression.

use crate::packet::{ctl, tochar, unchar};

/// Prefixing rules for one direction of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Codec {
    /// Control-character prefix (normally `#`).
    pub qctl: u8,
    /// 8th-bit prefix, when negotiated (normally `&`).
    pub qbin: Option<u8>,
    /// Repeat-count prefix, when negotiated (normally `~`).
    pub rept: Option<u8>,
}

impl Default for Codec {
    fn default() -> Self {
        Self { qctl: b'#', qbin: None, rept: None }
    }
}

/// Malformed prefix sequence in a received data field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError;

impl Codec {
    /// Encode a single byte (without repeat prefix) into `out`, returning its length.
    fn unit(&self, b: u8, out: &mut [u8; 3]) -> usize {
        let mut n = 0;
        let mut c = b;
        if let Some(q) = self.qbin {
            if c & 0x80 != 0 {
                out[n] = q;
                n += 1;
                c &= 0x7F;
            }
        }
        let c7 = c & 0x7F;
        if c7 < 32 || c7 == 127 {
            out[n] = self.qctl;
            n += 1;
            c = ctl(c);
        } else if c7 == self.qctl || Some(c7) == self.qbin || Some(c7) == self.rept {
            out[n] = self.qctl;
            n += 1;
        }
        out[n] = c;
        n + 1
    }

    /// Encode as much of `src` as fits into `out` without letting `out` grow
    /// beyond `cap` bytes. Returns the number of source bytes consumed.
    /// Encoded sequences are never split.
    pub fn encode(&self, src: &[u8], out: &mut Vec<u8>, cap: usize) -> usize {
        let mut i = 0;
        let mut u = [0u8; 3];
        while i < src.len() {
            let b = src[i];
            let ul = self.unit(b, &mut u);
            let mut run = 1;
            if self.rept.is_some() {
                while run < 94 && i + run < src.len() && src[i + run] == b {
                    run += 1;
                }
            }
            match self.rept {
                Some(r) if run * ul > ul + 2 => {
                    if out.len() + 2 + ul > cap {
                        break;
                    }
                    out.push(r);
                    out.push(tochar(run as u8));
                    out.extend_from_slice(&u[..ul]);
                    i += run;
                }
                _ => {
                    if out.len() + ul > cap {
                        break;
                    }
                    out.extend_from_slice(&u[..ul]);
                    i += 1;
                }
            }
        }
        i
    }

    /// Encode all of `src` (no size limit).
    #[cfg(test)]
    pub fn encode_all(&self, src: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(src.len() + src.len() / 4);
        self.encode(src, &mut out, usize::MAX);
        out
    }

    /// Decode a received data field, appending the result to `out`.
    pub fn decode(&self, data: &[u8], out: &mut Vec<u8>) -> Result<(), DecodeError> {
        let mut it = data.iter().copied();
        while let Some(mut a) = it.next() {
            let mut count = 1usize;
            if Some(a) == self.rept {
                count = unchar(it.next().ok_or(DecodeError)?) as usize;
                if count > 94 {
                    return Err(DecodeError);
                }
                a = it.next().ok_or(DecodeError)?;
            }
            let mut b8 = 0u8;
            if Some(a) == self.qbin {
                b8 = 0x80;
                a = it.next().ok_or(DecodeError)?;
            }
            if a == self.qctl {
                a = it.next().ok_or(DecodeError)?;
                let a7 = a & 0x7F;
                if (64..=95).contains(&a7) || a7 == 63 {
                    a = ctl(a);
                }
            }
            a |= b8;
            out.extend(std::iter::repeat_n(a, count));
        }
        Ok(())
    }

    pub fn decode_vec(&self, data: &[u8]) -> Result<Vec<u8>, DecodeError> {
        let mut out = Vec::with_capacity(data.len());
        self.decode(data, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codecs() -> Vec<Codec> {
        let mut v = Vec::new();
        for qbin in [None, Some(b'&')] {
            for rept in [None, Some(b'~')] {
                v.push(Codec { qctl: b'#', qbin, rept });
            }
        }
        v
    }

    #[test]
    fn known_encodings() {
        let c = Codec { qctl: b'#', qbin: Some(b'&'), rept: Some(b'~') };
        assert_eq!(c.encode_all(b"A\r\n"), b"A#M#J");
        assert_eq!(c.encode_all(b"#&~"), b"###&#~");
        assert_eq!(c.encode_all(&[0xC1]), b"&A");
        assert_eq!(c.encode_all(&[0x81]), b"&#A");
        assert_eq!(c.encode_all(&[0x7F, 0xFF]), b"#?&#?");
        assert_eq!(c.encode_all(b"aaaaa"), b"~%a");
        assert_eq!(c.encode_all(b"aa"), b"aa");
        assert_eq!(c.encode_all(&[0u8; 2]), b"#@#@");
        assert_eq!(c.encode_all(&[0u8; 3]), b"~##@");
        // Without 8th-bit prefixing, high controls are still control-prefixed
        // and the prefix characters with bit 7 set are quoted too.
        let c = Codec { qctl: b'#', qbin: None, rept: None };
        assert_eq!(c.encode_all(&[0x81, 0xA3, 0xA6]), [b'#', 0xC1, b'#', 0xA3, 0xA6]);
        assert_eq!(c.encode_all(b"&~"), b"&~");
    }

    #[test]
    fn roundtrip_every_byte_value() {
        let mut data: Vec<u8> = (0..=255u8).collect();
        data.extend((0..=255u8).flat_map(|b| std::iter::repeat_n(b, (b % 7) as usize)));
        data.extend(std::iter::repeat_n(0u8, 1000));
        data.extend(std::iter::repeat_n(b'~', 200));
        for c in codecs() {
            let enc = c.encode_all(&data);
            if c.qbin.is_some() {
                assert!(enc.iter().all(|&b| b < 0x80), "8th-bit prefixing must yield 7-bit output");
            }
            assert!(enc.iter().all(|&b| (b & 0x7F) >= 32 && (b & 0x7F) != 127), "no raw controls");
            assert_eq!(c.decode_vec(&enc).unwrap(), data, "{c:?}");
        }
    }

    #[test]
    fn capped_encoding_never_splits_sequences() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 37 % 256) as u8).collect();
        for c in codecs() {
            for cap in [5usize, 6, 7, 50, 91, 1000] {
                let mut pos = 0;
                let mut back = Vec::new();
                while pos < data.len() {
                    let mut out = Vec::new();
                    let used = c.encode(&data[pos..], &mut out, cap);
                    assert!(used > 0);
                    assert!(out.len() <= cap);
                    c.decode(&out, &mut back).unwrap();
                    pos += used;
                }
                assert_eq!(back, data);
            }
        }
    }

    #[test]
    fn decode_rejects_truncated() {
        let c = Codec { qctl: b'#', qbin: Some(b'&'), rept: Some(b'~') };
        assert!(c.decode_vec(b"#").is_err());
        assert!(c.decode_vec(b"&").is_err());
        assert!(c.decode_vec(b"~%").is_err());
    }
}
