//! Sending side: handshake, data blocks, EOT.

use std::io::{self, Read};
use std::time::{Duration, Instant};

use xfer_common::{ProgressThrottle, Result, XferError};

use crate::block::{Check, OutBlock, ACK, CAN, EOT, NAK, SHORT_BLOCK, SUB, WANT_CRC, WANT_G};
use crate::link::Link;
use crate::{ACK_TIMEOUT, HANDSHAKE_TIMEOUT, MAX_ERRORS, SENDER_HANDSHAKE_TRIES};

/// How the data phase of one file is sent.
#[derive(Debug, Clone, Copy)]
pub struct SendMode {
    pub check: Check,
    /// 128 or 1024. With 1024, a final chunk of 128 bytes or less still goes
    /// in a 128-byte block to save padding.
    pub block_len: usize,
    /// YMODEM-G: no per-block ACK.
    pub streaming: bool,
}

/// What the receiver answered to a block or EOT.
enum Reply {
    Ack,
    Nak,
    Timeout,
}

/// Wait for the receiver to ask for data: NAK (checksum), 'C' (CRC) or,
/// when `accept_g`, 'G' (YMODEM-G). Anything else is ignored. Queued
/// duplicate requests are purged so they can't be mistaken for replies.
pub fn wait_start(link: &mut Link<'_>, accept_g: bool) -> Result<u8> {
    for attempt in 1..=SENDER_HANDSHAKE_TRIES {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(b) = link.read_byte(left)? else {
                break;
            };
            match b {
                NAK | WANT_CRC => {
                    link.port.purge_input();
                    return Ok(b);
                }
                WANT_G if accept_g => {
                    link.port.purge_input();
                    return Ok(b);
                }
                CAN if link.second_can()? => return Err(XferError::RemoteCancelled),
                _ => {}
            }
        }
        link.retry(format!("waiting for receiver to start ({attempt}/{SENDER_HANDSHAKE_TRIES})"));
    }
    Err(XferError::Timeout)
}

/// Read ACK / NAK, skipping noise (including stray 'C'/'G' start requests,
/// which are never treated as a NAK so a request that crossed our first block
/// on the wire can't cause an extra retransmission and a stale ACK).
fn await_reply(link: &mut Link<'_>, timeout: Duration) -> Result<Reply> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match link.read_byte(left)? {
            None => return Ok(Reply::Timeout),
            Some(ACK) => return Ok(Reply::Ack),
            Some(NAK) => return Ok(Reply::Nak),
            Some(CAN) => {
                if link.second_can()? {
                    return Err(XferError::RemoteCancelled);
                }
            }
            Some(_) => {}
        }
    }
}

/// In streaming mode, look (without waiting) for a CAN CAN abort.
fn poll_remote_cancel(link: &mut Link<'_>) -> Result<()> {
    let mut buf = [0u8; 64];
    loop {
        let n = link.read(&mut buf, Duration::ZERO)?;
        if n == 0 {
            return Ok(());
        }
        let got = &buf[..n];
        if got.windows(2).any(|w| w == [CAN, CAN]) {
            return Err(XferError::RemoteCancelled);
        }
        if got.last() == Some(&CAN) && link.second_can()? {
            return Err(XferError::RemoteCancelled);
        }
    }
}

/// Send one sealed block, retransmitting on NAK/timeout unless streaming.
pub fn send_block(link: &mut Link<'_>, blk: &OutBlock, num: u8, streaming: bool) -> Result<()> {
    let mut errors = 0;
    loop {
        link.write(blk.wire())?;
        if streaming {
            return poll_remote_cancel(link);
        }
        match await_reply(link, ACK_TIMEOUT)? {
            Reply::Ack => return Ok(()),
            Reply::Nak => link.retry(format!("block {num} NAKed, resending")),
            Reply::Timeout => link.retry(format!("no ACK for block {num}, resending")),
        }
        errors += 1;
        if errors >= MAX_ERRORS {
            return Err(XferError::TooManyErrors(format!("block {num} not acknowledged")));
        }
    }
}

/// Read until `buf` is full or EOF.
fn fill(src: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match src.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Send all of `src` as blocks numbered from 1. Returns the bytes sent.
pub fn send_data(
    link: &mut Link<'_>,
    blk: &mut OutBlock,
    src: &mut dyn Read,
    mode: SendMode,
    throttle: &mut ProgressThrottle,
) -> Result<u64> {
    let mut num: u8 = 1;
    let mut sent = 0u64;
    loop {
        link.check_cancel()?;
        let filled = fill(src, &mut blk.data_mut()[..mode.block_len])?;
        if filled == 0 {
            return Ok(sent);
        }
        let block_len = if filled <= SHORT_BLOCK { SHORT_BLOCK } else { mode.block_len };
        blk.seal(num, filled, block_len, mode.check, SUB);
        send_block(link, blk, num, mode.streaming)?;
        sent += filled as u64;
        num = num.wrapping_add(1);
        throttle.report(&mut *link.ctl, sent, false);
    }
}

/// Send EOT until it is ACKed. A NAK (YMODEM receivers NAK the first EOT on
/// purpose) just gets another EOT.
pub fn send_eot(link: &mut Link<'_>) -> Result<()> {
    for attempt in 1..=MAX_ERRORS {
        link.write(&[EOT])?;
        match await_reply(link, ACK_TIMEOUT)? {
            Reply::Ack => return Ok(()),
            // The first NAK is the normal YMODEM double-EOT dance.
            Reply::Nak if attempt == 1 => {}
            Reply::Nak => link.retry("EOT NAKed, resending"),
            Reply::Timeout => link.retry("no ACK for EOT, resending"),
        }
    }
    Err(XferError::TooManyErrors("EOT not acknowledged".into()))
}

/// Send the final block of a batch (YMODEM's empty block 0). The receiver
/// may exit right after acknowledging it, or without doing so, so a timeout
/// or closed line here is not an error.
pub fn send_final_block(link: &mut Link<'_>, blk: &OutBlock, streaming: bool) -> Result<()> {
    const TRIES: u32 = 3;
    for _ in 0..TRIES {
        match link.write(blk.wire()) {
            Ok(()) => {}
            Err(XferError::Io(_)) => return Ok(()),
            Err(e) => return Err(e),
        }
        if streaming {
            return Ok(());
        }
        match await_reply(link, ACK_TIMEOUT) {
            Ok(Reply::Ack) | Ok(Reply::Timeout) | Err(XferError::Io(_)) => return Ok(()),
            Ok(Reply::Nak) => link.retry("end-of-batch block NAKed, resending"),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
