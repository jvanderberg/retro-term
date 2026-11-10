//! The packet link shared by sender and receiver: framing, reading and the
//! negotiated encoders.

use std::time::Duration;

use xfer_common::{Control, Port, Result, XferError};

use crate::encode::Codec;
use crate::packet::{frame, FrameParams, PacketReader, Parsed};
use crate::params::Negotiated;

pub struct Link<'a> {
    pub port: &'a mut dyn Port,
    reader: PacketReader,
    pub fp: FrameParams,
    pub enc: Codec,
    pub dec: Codec,
}

impl<'a> Link<'a> {
    pub fn new(port: &'a mut dyn Port) -> Self {
        Self {
            port,
            reader: PacketReader::new(),
            fp: FrameParams::default(),
            enc: Codec::default(),
            dec: Codec::default(),
        }
    }

    pub fn apply(&mut self, n: &Negotiated) {
        self.fp = n.frame;
        self.enc = n.enc;
        self.dec = n.dec;
    }

    /// Frame a packet with the current parameters.
    pub fn frame(&self, seq: u8, typ: u8, data: &[u8]) -> Vec<u8> {
        frame(&self.fp, seq, typ, data)
    }

    /// Frame a packet with a specific block check type.
    pub fn frame_chk(&self, chk: u8, seq: u8, typ: u8, data: &[u8]) -> Vec<u8> {
        frame(&FrameParams { chk, ..self.fp }, seq, typ, data)
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.port.write_all(bytes)?;
        self.port.flush()?;
        Ok(())
    }

    /// Frame and send a packet, returning the framed bytes.
    pub fn send(&mut self, seq: u8, typ: u8, data: &[u8]) -> Result<Vec<u8>> {
        let f = self.frame(seq, typ, data);
        self.write(&f)?;
        Ok(f)
    }

    /// Wait up to `timeout` for a packet (`Ok(None)` on timeout); fails
    /// with [`XferError::Cancelled`] promptly once `ctl` is cancelled.
    pub fn read(&mut self, timeout: Duration, ctl: &dyn Control) -> Result<Option<Parsed>> {
        let chk = self.fp.chk;
        let got = self.reader.read(&mut *self.port, timeout, chk, &|| ctl.cancelled())?;
        if got.is_none() && ctl.cancelled() {
            return Err(XferError::Cancelled);
        }
        Ok(got)
    }

    /// Best-effort error packet; failures are ignored.
    pub fn send_error(&mut self, seq: u8, msg: &str) {
        let mut data = Vec::new();
        self.enc.encode(msg.as_bytes(), &mut data, 80);
        let f = self.frame(seq, b'E', &data);
        let _ = self.write(&f);
    }

    /// Decode the message carried by a remote E packet.
    pub fn error_text(&self, data: &[u8]) -> String {
        let raw = self.dec.decode_vec(data).unwrap_or_else(|_| data.to_vec());
        let s = String::from_utf8_lossy(&raw).trim().to_string();
        if s.is_empty() {
            "remote Kermit reported an error".to_string()
        } else {
            s
        }
    }
}
