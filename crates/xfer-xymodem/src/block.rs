//! Wire constants and block framing shared by XMODEM and YMODEM.
//!
//! A block on the wire is:
//!
//! ```text
//! SOH|STX  num  !num  data[128|1024]  check (1-byte sum | 2-byte CRC-16, big endian)
//! ```

use std::time::Duration;

use xfer_common::crc::{checksum8, crc16};
use xfer_common::Result;

use crate::link::Link;

pub const SOH: u8 = 0x01;
pub const STX: u8 = 0x02;
pub const EOT: u8 = 0x04;
pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;
pub const CAN: u8 = 0x18;
/// CP/M end-of-file, used to pad the last block.
pub const SUB: u8 = 0x1A;
/// Receiver's request for CRC-16 mode.
pub const WANT_CRC: u8 = b'C';
/// Receiver's request for YMODEM-G streaming.
pub const WANT_G: u8 = b'G';

pub const SHORT_BLOCK: usize = 128;
pub const LONG_BLOCK: usize = 1024;

/// Most bytes between two blocks we skip as line noise before giving up on
/// the block and NAKing.
const MAX_NOISE: usize = 2 * LONG_BLOCK;

/// Integrity check carried by each block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// 8-bit arithmetic sum (original XMODEM).
    Sum,
    /// CRC-16/XMODEM, big endian.
    Crc,
}

impl Check {
    pub fn len(self) -> usize {
        match self {
            Check::Sum => 1,
            Check::Crc => 2,
        }
    }
}

/// An outgoing block, built in place so that file data can be read straight
/// into it and the whole block sent with one `write_all`.
pub struct OutBlock {
    buf: Vec<u8>,
    wire_len: usize,
}

impl OutBlock {
    pub fn new() -> Self {
        Self { buf: vec![0; 3 + LONG_BLOCK + 2], wire_len: 0 }
    }

    /// The data area (up to 1024 bytes) to fill before [`OutBlock::seal`].
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.buf[3..3 + LONG_BLOCK]
    }

    /// Finish a block whose first `filled` data bytes are valid: pad the rest
    /// of the `block_len` data area with `pad`, write the header and check.
    pub fn seal(&mut self, num: u8, filled: usize, block_len: usize, check: Check, pad: u8) {
        debug_assert!(block_len == SHORT_BLOCK || block_len == LONG_BLOCK);
        debug_assert!(filled <= block_len);
        self.buf[0] = if block_len == LONG_BLOCK { STX } else { SOH };
        self.buf[1] = num;
        self.buf[2] = !num;
        let data_end = 3 + block_len;
        self.buf[3 + filled..data_end].fill(pad);
        let data = &self.buf[3..data_end];
        match check {
            Check::Sum => {
                self.buf[data_end] = checksum8(data);
            }
            Check::Crc => {
                let crc = crc16(data);
                self.buf[data_end..data_end + 2].copy_from_slice(&crc.to_be_bytes());
            }
        }
        self.wire_len = data_end + check.len();
    }

    /// The sealed block as it goes on the wire.
    pub fn wire(&self) -> &[u8] {
        &self.buf[..self.wire_len]
    }
}

/// What arrived when a block was expected.
#[derive(Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A block that passed its checks; its data is [`InBlock::data`].
    Block { num: u8 },
    /// End of file.
    Eot,
    /// Something block-shaped that failed validation.
    Bad(String),
    /// Nothing arrived in time.
    Timeout,
    /// The remote sent CAN CAN.
    RemoteCancel,
}

/// Reusable receive buffer for one block.
pub struct InBlock {
    buf: Vec<u8>,
    len: usize,
}

impl InBlock {
    pub fn new() -> Self {
        Self { buf: vec![0; 2 + LONG_BLOCK + 2], len: 0 }
    }

    /// Data of the last good block (128 or 1024 bytes).
    pub fn data(&self) -> &[u8] {
        &self.buf[2..2 + self.len]
    }

    /// Wait up to `timeout` for the start of a block, then read and validate
    /// it, allowing `inter_byte` between bytes of the block body.
    pub fn read(
        &mut self,
        link: &mut Link<'_>,
        check: Check,
        timeout: Duration,
        inter_byte: Duration,
    ) -> Result<Inbound> {
        let mut noise = 0usize;
        let len = loop {
            let Some(b) = link.read_byte(timeout)? else {
                return Ok(Inbound::Timeout);
            };
            match b {
                SOH => break SHORT_BLOCK,
                STX => break LONG_BLOCK,
                EOT => return Ok(Inbound::Eot),
                CAN if link.second_can()? => return Ok(Inbound::RemoteCancel),
                _ => {}
            }
            noise += 1;
            if noise > MAX_NOISE {
                return Ok(Inbound::Bad("line noise".into()));
            }
        };
        let total = 2 + len + check.len();
        if !link.read_exact(&mut self.buf[..total], inter_byte)? {
            return Ok(Inbound::Bad("short block (timeout)".into()));
        }
        let num = self.buf[0];
        if self.buf[1] != !num {
            return Ok(Inbound::Bad(format!("bad block number complement ({num:#04x}/{:#04x})", self.buf[1])));
        }
        let data = &self.buf[2..2 + len];
        let ok = match check {
            Check::Sum => checksum8(data) == self.buf[2 + len],
            Check::Crc => crc16(data).to_be_bytes() == self.buf[2 + len..2 + len + 2],
        };
        if !ok {
            let what = match check {
                Check::Sum => "checksum",
                Check::Crc => "CRC",
            };
            return Ok(Inbound::Bad(format!("bad {what} on block {num}")));
        }
        self.len = len;
        Ok(Inbound::Block { num })
    }
}
