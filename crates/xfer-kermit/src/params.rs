//! Send-Init parameter exchange and negotiation.

use std::time::Duration;

use crate::encode::Codec;
use crate::packet::{ctl, tochar, unchar, FrameParams, MAX_LONG, MAX_NORMAL};
use crate::{EightBitPrefix, KermitOptions};

/// CAPAS bit: extended-length packets.
pub const CAP_LONG: u8 = 2;
/// CAPAS bit: sliding windows.
pub const CAP_WINDOWS: u8 = 4;
/// CAPAS bit: attribute packets.
pub const CAP_ATTRS: u8 = 8;

/// Our control prefix.
pub const MY_QCTL: u8 = b'#';
/// The 8th-bit prefix we request when we want one.
pub const MY_QBIN: u8 = b'&';
/// The repeat prefix we offer.
pub const MY_REPT: u8 = b'~';

/// One side's Send-Init parameters (the data field of an S packet or of its ACK).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitParams {
    /// Longest normal packet this side can receive.
    pub maxl: u8,
    /// Timeout (seconds) the other side should use when waiting for this side.
    pub time: u8,
    pub npad: u8,
    pub padc: u8,
    /// End-of-line character this side wants packets terminated with.
    pub eol: u8,
    /// Control prefix this side will use when sending.
    pub qctl: u8,
    /// 8th-bit prefixing: `Y`, `N`, or the prefix character requested.
    pub qbin: u8,
    /// Block check type requested (`1`, `2`, `3`).
    pub chkt: u8,
    /// Repeat prefix offered (space = none).
    pub rept: u8,
    /// First CAPAS byte (6-bit value).
    pub capas: u8,
    /// Window slots.
    pub window: u8,
    /// Longest extended packet this side can receive (0 = not given).
    pub maxlx: usize,
}

/// A character usable as an 8th-bit or repeat prefix.
pub fn valid_prefix(c: u8) -> bool {
    (33..=62).contains(&c) || (96..=126).contains(&c)
}

fn secs(d: Duration) -> u8 {
    d.as_secs().clamp(1, 94) as u8
}

impl InitParams {
    fn base(opts: &KermitOptions) -> Self {
        let maxlx = opts.max_packet_len.clamp(20, MAX_LONG);
        let long = opts.long_packets && maxlx > MAX_NORMAL;
        let window = opts.window.clamp(1, 31);
        let mut capas = CAP_ATTRS;
        if long {
            capas |= CAP_LONG;
        }
        if window > 1 {
            capas |= CAP_WINDOWS;
        }
        Self {
            maxl: if long { MAX_NORMAL as u8 } else { maxlx.min(MAX_NORMAL) as u8 },
            time: secs(opts.timeout),
            npad: 0,
            padc: 0,
            eol: b'\r',
            qctl: MY_QCTL,
            qbin: match opts.eight_bit {
                EightBitPrefix::IfRequested => b'Y',
                EightBitPrefix::Always => MY_QBIN,
                EightBitPrefix::Never => b'N',
            },
            chkt: b'0' + opts.block_check.clamp(1, 3),
            rept: if opts.repeat_counts { MY_REPT } else { b' ' },
            capas,
            window,
            maxlx: if long { maxlx } else { 0 },
        }
    }

    /// Parameters for the S packet we send as file sender.
    pub fn for_sender(opts: &KermitOptions) -> Self {
        Self::base(opts)
    }

    /// Parameters for our ACK to the remote sender's S packet.
    pub fn for_receiver(opts: &KermitOptions, theirs: &InitParams) -> Self {
        let mut p = Self::base(opts);
        // Agree to whatever block check the sender asks for if we support it.
        p.chkt = if (b'1'..=b'3').contains(&theirs.chkt) { theirs.chkt } else { b'1' };
        if opts.repeat_counts && valid_prefix(theirs.rept) && theirs.rept != MY_QCTL {
            p.rept = theirs.rept;
        } else {
            p.rept = b' ';
        }
        if opts.eight_bit == EightBitPrefix::Always
            && valid_prefix(theirs.qbin)
            && theirs.qbin != MY_QCTL
            && theirs.qbin != p.rept
        {
            p.qbin = theirs.qbin;
        }
        if theirs.capas & CAP_WINDOWS != 0 {
            p.window = p.window.min(theirs.window.max(1));
        } else {
            p.window = 1;
        }
        if p.window <= 1 {
            p.capas &= !CAP_WINDOWS;
            p.window = 1;
        }
        p
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = vec![
            tochar(self.maxl),
            tochar(self.time),
            tochar(self.npad),
            ctl(self.padc),
            tochar(self.eol),
            self.qctl,
            self.qbin,
            self.chkt,
            self.rept,
            tochar(self.capas & !1),
            tochar(self.window),
        ];
        let lx = if self.maxlx > 0 { self.maxlx.min(MAX_LONG) } else { 0 };
        v.push(tochar((lx / 95) as u8));
        v.push(tochar((lx % 95) as u8));
        v
    }

    pub fn parse(d: &[u8]) -> Self {
        let get = |i: usize| d.get(i).copied().filter(|&c| c != b' ' || i == 8);
        let maxl = get(0).map(unchar).filter(|v| (10..=94).contains(v)).unwrap_or(80);
        let time = get(1).map(unchar).filter(|&v| v > 0 && v <= 94).unwrap_or(5);
        let npad = get(2).map(unchar).filter(|&v| v <= 94).unwrap_or(0);
        let padc = d.get(3).map(|&c| ctl(c)).unwrap_or(0);
        let eol = get(4).map(unchar).filter(|&v| v > 0 && v < 32).unwrap_or(b'\r');
        let qctl = get(5).filter(|&c| (33..=126).contains(&c)).unwrap_or(b'#');
        let qbin = get(6).unwrap_or(b'N');
        let chkt = get(7).unwrap_or(b'1');
        let rept = d.get(8).copied().unwrap_or(b' ');
        let mut i = 9;
        let mut capas = 0;
        if let Some(&c) = d.get(i) {
            capas = unchar(c) & 0x3F;
            i += 1;
            let mut more = capas & 1 != 0;
            while more {
                match d.get(i) {
                    Some(&c) => {
                        more = unchar(c) & 1 != 0;
                        i += 1;
                    }
                    None => more = false,
                }
            }
        }
        let window = d.get(i).map(|&c| unchar(c)).filter(|&v| (1..=32).contains(&v)).unwrap_or(1);
        let maxlx = match (d.get(i + 1), d.get(i + 2)) {
            (Some(&a), Some(&b)) => unchar(a) as usize * 95 + unchar(b) as usize,
            _ => 0,
        };
        Self { maxl, time, npad, padc, eol, qctl, qbin, chkt, rept, capas, window, maxlx }
    }
}

/// Settings in force after the Send-Init exchange.
#[derive(Debug, Clone)]
pub struct Negotiated {
    pub chk: u8,
    pub window: usize,
    pub attrs: bool,
    pub long: bool,
    /// Framing for packets we send.
    pub frame: FrameParams,
    /// Longest packet (LEN or extended length) the remote accepts.
    pub their_max: usize,
    /// Encoder for our outgoing data.
    pub enc: Codec,
    /// Decoder for the remote's data.
    pub dec: Codec,
}

/// Negotiate from `ours` and `theirs`, where `sender` / `receiver` say which
/// set belongs to the file sender.
pub fn negotiate(ours: &InitParams, theirs: &InitParams, we_send: bool) -> Negotiated {
    let (s, r) = if we_send { (ours, theirs) } else { (theirs, ours) };
    let chk = if s.chkt == r.chkt && (b'1'..=b'3').contains(&s.chkt) { s.chkt - b'0' } else { 1 };
    let rept = (s.rept == r.rept && valid_prefix(s.rept)).then_some(s.rept);
    let qbin = if valid_prefix(s.qbin) && (r.qbin == b'Y' || r.qbin == s.qbin) {
        Some(s.qbin)
    } else if valid_prefix(r.qbin) && s.qbin == b'Y' {
        Some(r.qbin)
    } else {
        None
    };
    let qbin = qbin.filter(|&q| q != ours.qctl && q != theirs.qctl && Some(q) != rept);
    let long = s.capas & CAP_LONG != 0 && r.capas & CAP_LONG != 0;
    let windows = s.capas & CAP_WINDOWS != 0 && r.capas & CAP_WINDOWS != 0;
    let window = if windows { s.window.min(r.window).clamp(1, 31) as usize } else { 1 };
    let attrs = s.capas & CAP_ATTRS != 0 && r.capas & CAP_ATTRS != 0;
    let their_max = if long {
        if theirs.maxlx > 0 {
            theirs.maxlx
        } else {
            500
        }
    } else {
        theirs.maxl as usize
    };
    Negotiated {
        chk,
        window,
        attrs,
        long,
        frame: FrameParams {
            chk,
            eol: theirs.eol,
            npad: theirs.npad,
            padc: theirs.padc,
            normal_max: (theirs.maxl as usize).min(MAX_NORMAL),
        },
        their_max,
        enc: Codec { qctl: ours.qctl, qbin, rept },
        dec: Codec { qctl: theirs.qctl, qbin, rept },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_parse_roundtrip() {
        let opts = KermitOptions::default();
        let p = InitParams::for_sender(&opts);
        assert_eq!(InitParams::parse(&p.encode()), p);
    }

    #[test]
    fn parse_ckermit_style() {
        // A typical C-Kermit 9 Send-Init: MAXL 94, TIME 13, no padding, EOL CR,
        // '#', 'Y', '3', '~', two CAPAS bytes, WINDO 30, MAXLX 1103,
        // then checkpoint/WHATAMI fields we ignore.
        let d = b"~- @-#Y3~_ >+Z0___\"U1";
        let p = InitParams::parse(d);
        assert_eq!(p.maxl, 94);
        assert_eq!(p.time, 13);
        assert_eq!(p.eol, 13);
        assert_eq!(p.qbin, b'Y');
        assert_eq!(p.chkt, b'3');
        assert_eq!(p.rept, b'~');
        assert_eq!(p.capas & (CAP_LONG | CAP_WINDOWS | CAP_ATTRS), CAP_LONG | CAP_WINDOWS | CAP_ATTRS);
        assert_eq!(p.window, 30);
        assert_eq!(p.maxlx, unchar(b'+') as usize * 95 + unchar(b'Z') as usize);
    }

    #[test]
    fn missing_fields_default() {
        let p = InitParams::parse(b"");
        assert_eq!((p.maxl, p.eol, p.qctl, p.chkt, p.window, p.maxlx), (80, 13, b'#', b'1', 1, 0));
        let n = negotiate(&InitParams::for_sender(&KermitOptions::default()), &p, true);
        assert_eq!((n.chk, n.window, n.long, n.enc.rept, n.enc.qbin), (1, 1, false, None, None));
        assert_eq!(n.their_max, 80);
    }

    #[test]
    fn eight_bit_negotiation() {
        let mut o = KermitOptions::default();
        let s = InitParams::for_sender(&o);
        let r = InitParams::for_receiver(&o, &s);
        assert_eq!(negotiate(&s, &r, true).enc.qbin, None);

        o.eight_bit = EightBitPrefix::Always;
        let s2 = InitParams::for_sender(&o);
        let r2 = InitParams::for_receiver(&KermitOptions::default(), &s2);
        assert_eq!(negotiate(&s2, &r2, true).enc.qbin, Some(b'&'));
        assert_eq!(negotiate(&r2, &s2, false).dec.qbin, Some(b'&'));

        // Receiver forces it with a 'Y' sender.
        let r3 = InitParams::for_receiver(&o, &s);
        assert_eq!(negotiate(&s, &r3, true).enc.qbin, Some(b'&'));

        // Refused.
        let never = KermitOptions { eight_bit: EightBitPrefix::Never, ..KermitOptions::default() };
        let r4 = InitParams::for_receiver(&never, &s2);
        assert_eq!(negotiate(&s2, &r4, true).enc.qbin, None);
    }
}
