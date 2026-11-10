//! Receiving side: the data phase of one file (blocks 1.. and EOT).

use std::io::Write;
use std::time::Duration;

use xfer_common::{Event, ProgressThrottle, Result, XferError};

use crate::block::{Check, InBlock, Inbound, ACK, LONG_BLOCK, NAK, SUB};
use crate::link::Link;
use crate::{BLOCK_TIMEOUT, INTER_BYTE_TIMEOUT, MAX_ERRORS};

/// How the data phase is received.
#[derive(Debug, Clone, Copy)]
pub struct RecvMode {
    pub check: Check,
    /// YMODEM-G: never ACK/NAK data; any error is fatal.
    pub streaming: bool,
    /// NAK the first EOT and only ACK when it is repeated (YMODEM), which
    /// guards against a noise byte being mistaken for end of file.
    pub double_eot: bool,
    /// Exact file length, if the protocol told us (YMODEM block 0). When
    /// unknown, trailing SUB padding is stripped from the final block.
    pub size: Option<u64>,
}

/// How to solicit the first block.
#[derive(Debug, Clone, Copy)]
pub struct Handshake {
    /// Byte sent to ask for data: NAK, 'C' or 'G'.
    pub prompt: u8,
    /// Wait between prompts.
    pub interval: Duration,
    /// Total prompts before giving up.
    pub tries: u32,
    /// Switch from 'C' (CRC) to NAK (checksum) after this many prompts.
    pub fallback_after: Option<u32>,
}

/// Receive one file's data into `out`. Returns the bytes written.
pub fn recv_data(
    link: &mut Link<'_>,
    inb: &mut InBlock,
    out: &mut dyn Write,
    mode: RecvMode,
    hs: Handshake,
    throttle: &mut ProgressThrottle,
) -> Result<u64> {
    let mut check = mode.check;
    let mut prompt = hs.prompt;
    let mut prompts = 1u32;
    let mut started = false;
    let mut eot_naked = false;
    let mut expected: u8 = 1;
    let mut errors = 0u32;
    let mut written = 0u64;
    let mut received = 0u64;
    // Without a known size, the most recent block is held back so its SUB
    // padding can be stripped if it turns out to be the last one.
    let mut held: Vec<u8> = Vec::with_capacity(LONG_BLOCK);

    link.write(&[prompt])?;
    loop {
        let waiting_for_first = !started && !eot_naked;
        let timeout = if waiting_for_first { hs.interval } else { BLOCK_TIMEOUT };
        match inb.read(link, check, timeout, INTER_BYTE_TIMEOUT)? {
            Inbound::Block { num } if num == expected => {
                started = true;
                eot_naked = false;
                errors = 0;
                expected = expected.wrapping_add(1);
                let data = inb.data();
                received += data.len() as u64;
                match mode.size {
                    Some(size) => {
                        let room = size.saturating_sub(written);
                        let take = data.len().min(usize::try_from(room).unwrap_or(usize::MAX));
                        out.write_all(&data[..take])?;
                        written += take as u64;
                    }
                    None => {
                        out.write_all(&held)?;
                        written += held.len() as u64;
                        held.clear();
                        held.extend_from_slice(data);
                    }
                }
                if !mode.streaming {
                    link.write(&[ACK])?;
                }
                let shown = mode.size.map_or(received, |s| received.min(s));
                throttle.report(&mut *link.ctl, shown, false);
            }
            Inbound::Block { num } if num == expected.wrapping_sub(1) => {
                // A retransmission because our ACK was lost: ACK and discard.
                if !mode.streaming {
                    link.write(&[ACK])?;
                    if !started {
                        // A repeated YMODEM block 0: the sender will now be
                        // waiting for our data request again.
                        link.write(&[prompt])?;
                    }
                }
            }
            Inbound::Block { num } => {
                return Err(XferError::Protocol(format!("block sequence error: got {num}, expected {expected}")));
            }
            Inbound::Eot => {
                if mode.double_eot && !eot_naked {
                    eot_naked = true;
                    link.write(&[NAK])?;
                    continue;
                }
                link.write(&[ACK])?;
                break;
            }
            Inbound::Bad(reason) => {
                if mode.streaming {
                    return Err(XferError::Protocol(format!("YMODEM-G: {reason}")));
                }
                errors += 1;
                link.retry(reason);
                link.purge_line()?;
                link.write(&[NAK])?;
            }
            Inbound::Timeout if waiting_for_first => {
                prompts += 1;
                if prompts > hs.tries {
                    return Err(XferError::Timeout);
                }
                if hs.fallback_after.is_some_and(|n| prompts > n) && check == Check::Crc {
                    check = Check::Sum;
                    prompt = NAK;
                    link.event(Event::Message("no response to CRC request, trying checksum".into()));
                }
                link.retry("waiting for sender");
                link.write(&[prompt])?;
            }
            Inbound::Timeout => {
                if mode.streaming {
                    return Err(XferError::Timeout);
                }
                errors += 1;
                link.retry(format!("timeout waiting for block {expected}"));
                link.write(&[NAK])?;
            }
            Inbound::RemoteCancel => return Err(XferError::RemoteCancelled),
        }
        if errors >= MAX_ERRORS {
            return Err(XferError::TooManyErrors(format!("block {expected} kept failing")));
        }
    }

    if mode.size.is_none() {
        // Heuristic: XMODEM can't express the true length, so trailing SUB
        // (0x1A) bytes of the final block are taken to be padding. A file
        // that genuinely ends in 0x1A loses those bytes.
        while held.last() == Some(&SUB) {
            held.pop();
        }
        out.write_all(&held)?;
        written += held.len() as u64;
    }
    out.flush()?;
    throttle.report(&mut *link.ctl, written, true);
    Ok(written)
}
