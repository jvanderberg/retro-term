//! XMODEM / XMODEM-CRC / XMODEM-1K and YMODEM / YMODEM-G file transfer.
//!
//! All functions are synchronous: run them on a worker thread with a
//! [`Port`] connected to the remote side. They poll [`Control::cancelled`]
//! at least every 100 ms and, when it is set, send a run of CANs and return
//! [`XferError::Cancelled`](xfer_common::XferError::Cancelled). A remote
//! `CAN CAN` yields `RemoteCancelled`. Any other failure also sends CANs so
//! the remote stops promptly.
//!
//! Interoperability notes:
//! * Senders follow whatever the receiver asks for: NAK = 8-bit checksum,
//!   `C` = CRC-16, `G` = YMODEM-G streaming.
//! * A final chunk of 128 bytes or less is sent in a 128-byte block even in
//!   1K modes (as lrzsz does), to keep padding small.
//! * YMODEM receivers NAK the first EOT and ACK the repeat; senders accept
//!   an immediate ACK as well. XMODEM receivers ACK the first EOT.
//! * XMODEM has no length field, so receivers strip trailing SUB (0x1A)
//!   bytes from the final block. A file that really ends in 0x1A loses
//!   those bytes. YMODEM truncates to the size in block 0 instead.

use std::path::{Path, PathBuf};
use std::time::Duration;

use xfer_common::{Control, Port, RecvOptions, Result, Summary};

mod block;
mod link;
mod receiver;
mod sender;
mod xmodem;
mod ymodem;

pub use ymodem::{encode_header, parse_header, Header};

/// Consecutive errors tolerated on one block before giving up.
const MAX_ERRORS: u32 = 10;
/// How long a sender waits for each start request ('C', NAK, 'G').
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Start-request waits a sender makes before giving up (~100 s total).
const SENDER_HANDSHAKE_TRIES: u32 = 10;
/// How long a sender waits for ACK/NAK of a block or EOT.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a receiver waits for the next block once data is flowing.
const BLOCK_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest gap allowed between bytes within one block.
const INTER_BYTE_TIMEOUT: Duration = Duration::from_secs(2);
/// Receiver's interval between start requests while no sender has answered.
const PROMPT_INTERVAL: Duration = Duration::from_secs(3);
/// Start requests a receiver sends before giving up (~60 s).
const RECV_HANDSHAKE_TRIES: u32 = 20;
/// 'C' requests an XMODEM-CRC receiver sends before falling back to NAK.
const CRC_PROBES: u32 = 3;
/// File I/O buffer size.
const FILE_BUF: usize = 64 * 1024;

/// Which flavour of XMODEM to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XVariant {
    /// 128-byte blocks; receiver asks for the 8-bit checksum (NAK).
    Checksum,
    /// 128-byte blocks; receiver asks for CRC-16 ('C').
    Crc,
    /// 1024-byte blocks with CRC-16.
    OneK,
}

/// Send one file.
///
/// `Checksum`/`Crc`: 128-byte blocks; the receiver's NAK vs 'C' picks the
/// check (either is accepted regardless of `variant`). `OneK`: 1024-byte STX
/// blocks with CRC, falling back to 128-byte checksum blocks if the receiver
/// only NAKs.
pub fn xmodem_send(port: &mut dyn Port, ctl: &mut dyn Control, file: &Path, variant: XVariant) -> Result<Summary> {
    xmodem::send(port, ctl, file, variant)
}

/// Receive one file into `dest` (exact path, created or truncated).
///
/// `Checksum` starts with NAK; `Crc`/`OneK` start with 'C' and fall back to
/// NAK after three unanswered requests. Both 128-byte (SOH) and 1024-byte
/// (STX) blocks are accepted. Trailing SUB (0x1A) padding is stripped from
/// the last block, since XMODEM carries no file length.
pub fn xmodem_recv(port: &mut dyn Port, ctl: &mut dyn Control, dest: &Path, variant: XVariant) -> Result<Summary> {
    xmodem::recv(port, ctl, dest, variant)
}

/// YMODEM batch send: block 0 header `name NUL size mtime-octal mode-octal`,
/// 1K data blocks, then an empty block 0 to end the batch.
///
/// The mode is chosen by the receiver: 'G' gets YMODEM-G streaming (no
/// per-block ACK; any error aborts), 'C' plain YMODEM. `g` only records the
/// caller's intent: if it is set and the receiver asks for 'C', a
/// `Message` event notes the fallback to plain YMODEM.
pub fn ymodem_send(port: &mut dyn Port, ctl: &mut dyn Control, files: &[PathBuf], g: bool) -> Result<Summary> {
    ymodem::send(port, ctl, files, g)
}

/// YMODEM batch receive into `opts.dest_dir` (created if missing). Names come
/// from block 0 via [`xfer_common::safe_dest_path`]; each file is truncated
/// to the size from block 0 when given and gets its mtime when given.
/// `g` requests YMODEM-G ('G' instead of 'C').
pub fn ymodem_recv(port: &mut dyn Port, ctl: &mut dyn Control, opts: &RecvOptions, g: bool) -> Result<Summary> {
    ymodem::recv(port, ctl, opts, g)
}
