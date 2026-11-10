//! The Kermit file-transfer protocol.
//!
//! Supports normal and extended-length packets, block checks 1/2/3, control,
//! 8th-bit and repeat-count prefixing, attribute packets and selective-repeat
//! sliding windows (up to 31 slots). Interoperates with C-Kermit.
//!
//! All transfers are binary: file contents are sent and stored verbatim.

use std::path::PathBuf;
use std::time::Duration;

use xfer_common::{Control, Port, RecvOptions, Result, Summary};

mod detect;
mod encode;
mod link;
mod packet;
mod params;
mod recv;
mod send;

pub use detect::Detector;

/// 8th-bit prefixing policy (the QBIN Send-Init field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EightBitPrefix {
    /// Agree to 8th-bit prefixing if the other side asks for it (`Y`).
    /// Right for 8-bit-clean links.
    IfRequested,
    /// Request it (`&`), e.g. on a 7-bit link.
    Always,
    /// Refuse it (`N`).
    Never,
}

#[derive(Debug, Clone)]
pub struct KermitOptions {
    /// Longest packet we accept and will send (capped at 9024). Values above
    /// 94 need `long_packets`.
    pub max_packet_len: usize,
    /// Offer extended-length ("long") packets.
    pub long_packets: bool,
    /// Sliding-window slots to offer (1 = stop-and-wait, max 31).
    pub window: u8,
    /// Preferred block check type: 1, 2 or 3 (CRC-16).
    pub block_check: u8,
    pub eight_bit: EightBitPrefix,
    /// Offer repeat-count compression (`~`).
    pub repeat_counts: bool,
    /// Send attribute packets (file size / type) when the receiver accepts them.
    pub attributes: bool,
    /// How long to wait for a packet before retransmitting / NAKing.
    pub timeout: Duration,
    /// Retries per packet before giving up.
    pub retry_limit: u32,
    /// When receiving, NAK packet 0 straight away to make the sender resend
    /// its Send-Init (needed when the terminal's [`Detector`] swallowed it).
    pub initial_nak: bool,
}

impl Default for KermitOptions {
    fn default() -> Self {
        Self {
            max_packet_len: 4000,
            long_packets: true,
            window: 30,
            block_check: 3,
            eight_bit: EightBitPrefix::IfRequested,
            repeat_counts: true,
            attributes: true,
            timeout: Duration::from_secs(5),
            retry_limit: 10,
            initial_nak: true,
        }
    }
}

/// Send `files` to a Kermit receiver.
pub fn send(port: &mut dyn Port, ctl: &mut dyn Control, files: &[PathBuf], opts: &KermitOptions) -> Result<Summary> {
    send::send(port, ctl, files, opts)
}

/// Receive files from a Kermit sender into `recv.dest_dir`.
pub fn recv(port: &mut dyn Port, ctl: &mut dyn Control, recv: &RecvOptions, opts: &KermitOptions) -> Result<Summary> {
    recv::recv(port, ctl, recv, opts, None)
}

/// Like [`recv()`], but starts from a Send-Init packet already captured by a
/// [`Detector`] (`Detector::packet()`), so no resend is needed.
pub fn recv_with_init(
    port: &mut dyn Port,
    ctl: &mut dyn Control,
    recv: &RecvOptions,
    opts: &KermitOptions,
    init_packet: &[u8],
) -> Result<Summary> {
    recv::recv(port, ctl, recv, opts, Some(init_packet))
}
