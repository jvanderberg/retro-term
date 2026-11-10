//! ZMODEM file transfer (compatible with lrzsz `sz`/`rz`).
//!
//! * [`send()`] / [`recv()`] run a complete batch transfer over a [`Port`].
//! * [`Detector`] spots a remote `sz`/`rz` starting up in the terminal byte
//!   stream so the terminal can launch the matching side automatically.
//!
//! Supported: hex, binary CRC-16 and binary CRC-32 headers; CRC-16/CRC-32
//! data subpackets with ZCRCE/ZCRCG/ZCRCQ/ZCRCW; full streaming with a
//! sliding window (ZCRCQ acknowledgements), receiver-buffer windows (ZCRCW),
//! ZRPOS error recovery, ZSKIP, ZCRC, ZSINIT/ESCCTL, ZFREECNT; ZCOMMAND is
//! always refused.

use std::path::PathBuf;
use std::time::Duration;

use xfer_common::{Control, Port, RecvOptions, Result, Summary, XferError};

mod consts;
mod detect;
mod frame;
mod recv;
mod send;

pub use detect::{Detected, Detector};

/// Tunables shared by sender and receiver. `Options::default()` is what
/// [`send()`] and [`recv()`] use.
#[derive(Debug, Clone)]
pub struct Options {
    /// How long to wait for a header or the next byte before retrying.
    pub timeout: Duration,
    /// Consecutive recoverable errors tolerated before giving up.
    pub max_retries: u32,
    /// Ask for (and perform) escaping of all control characters.
    pub escape_ctl: bool,
    /// Use / advertise CRC-32.
    pub use_crc32: bool,
    /// Sender: largest data subpacket (32..=8192).
    pub max_block: usize,
    /// Sender: most unacknowledged bytes in flight while streaming
    /// (0 = unlimited). ZCRCQ acknowledgements advance the window.
    pub window: u64,
    /// Receiver: buffer size advertised in ZRINIT (0 = full streaming).
    pub rx_buffer_size: u16,
    /// Sender: type `rz\r` first so a remote shell starts its receiver.
    pub send_rz_command: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_retries: 10,
            escape_ctl: false,
            use_crc32: true,
            max_block: 8192,
            window: 256 * 1024,
            rx_buffer_size: 0,
            send_rz_command: false,
        }
    }
}

/// Send `files` to a ZMODEM receiver. Starts with ZRQINIT, so it works both
/// when started by hand and after [`Detected::RemoteReceiving`].
pub fn send(port: &mut dyn Port, ctl: &mut dyn Control, files: &[PathBuf]) -> Result<Summary> {
    send_with_options(port, ctl, files, &Options::default())
}

/// [`send()`] with explicit [`Options`].
pub fn send_with_options(
    port: &mut dyn Port,
    ctl: &mut dyn Control,
    files: &[PathBuf],
    opts: &Options,
) -> Result<Summary> {
    let mut link = frame::Link::new(port, ctl, opts.timeout);
    let r = send::run(&mut link, files, opts);
    conclude(&mut link, r)
}

/// Receive files from a ZMODEM sender into `opts.dest_dir`. Sends ZRINIT
/// immediately, so it works after [`Detected::RemoteSending`] (whose
/// ZRQINIT has already been consumed).
pub fn recv(port: &mut dyn Port, ctl: &mut dyn Control, opts: &RecvOptions) -> Result<Summary> {
    recv_with_options(port, ctl, opts, &Options::default())
}

/// [`recv()`] with explicit [`Options`].
pub fn recv_with_options(
    port: &mut dyn Port,
    ctl: &mut dyn Control,
    ropts: &RecvOptions,
    opts: &Options,
) -> Result<Summary> {
    let mut link = frame::Link::new(port, ctl, opts.timeout);
    let r = recv::run(&mut link, ropts, opts);
    conclude(&mut link, r)
}

/// On fatal local errors tell the remote to stop.
fn conclude(link: &mut frame::Link, r: Result<Summary>) -> Result<Summary> {
    match &r {
        Ok(_) | Err(XferError::RemoteCancelled) => {}
        Err(XferError::Io(e))
            if matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe) => {}
        Err(_) => link.abort(),
    }
    r
}
