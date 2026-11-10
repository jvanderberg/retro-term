//! A [`Port`] + [`Control`] pair with cancellation-aware timed reads.

use std::time::{Duration, Instant};

use xfer_common::{Control, Event, Port, Result, XferError};

use crate::block::CAN;

/// Longest single wait on the port, so `Control::cancelled` is polled often.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// How long to wait for the second CAN of a cancel pair.
const CAN_PAIR_TIMEOUT: Duration = Duration::from_secs(1);
/// Line silence that ends a purge.
const PURGE_QUIET: Duration = Duration::from_millis(150);
/// Upper bound on a purge when the line never goes quiet.
const PURGE_MAX: Duration = Duration::from_secs(3);
/// What we send to abort: a run of CANs (two are required; lrzsz sends more).
const CANCEL_SEQ: [u8; 8] = [CAN; 8];

pub struct Link<'a> {
    pub port: &'a mut dyn Port,
    pub ctl: &'a mut dyn Control,
    cancel_sent: bool,
}

impl<'a> Link<'a> {
    pub fn new(port: &'a mut dyn Port, ctl: &'a mut dyn Control) -> Self {
        Self { port, ctl, cancel_sent: false }
    }

    /// Fail with [`XferError::Cancelled`] (telling the remote) if the user
    /// asked to stop.
    pub fn check_cancel(&mut self) -> Result<()> {
        if self.ctl.cancelled() {
            self.send_cancel();
            return Err(XferError::Cancelled);
        }
        Ok(())
    }

    /// Tell the remote we are aborting (once). Errors are ignored: the line
    /// may already be gone.
    pub fn send_cancel(&mut self) {
        if !self.cancel_sent {
            self.cancel_sent = true;
            let _ = self.port.write_all(&CANCEL_SEQ);
            let _ = self.port.flush();
        }
    }

    /// Abort with `err`, sending CANs to the remote unless it was the remote
    /// that cancelled.
    pub fn abort(&mut self, err: XferError) -> XferError {
        if !matches!(err, XferError::RemoteCancelled) {
            self.send_cancel();
        }
        err
    }

    /// Read whatever is available, waiting at most `timeout` for the first
    /// byte. `Ok(0)` on timeout.
    pub fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            self.check_cancel()?;
            let slice = deadline.saturating_duration_since(Instant::now()).min(POLL_SLICE);
            let n = self.port.read_timeout(buf, slice)?;
            if n > 0 {
                return Ok(n);
            }
            if Instant::now() >= deadline {
                return Ok(0);
            }
        }
    }

    pub fn read_byte(&mut self, timeout: Duration) -> Result<Option<u8>> {
        let mut b = [0u8; 1];
        Ok(match self.read(&mut b, timeout)? {
            0 => None,
            _ => Some(b[0]),
        })
    }

    /// Fill `buf`, allowing `inter_byte` between chunks. `Ok(false)` on timeout.
    pub fn read_exact(&mut self, buf: &mut [u8], inter_byte: Duration) -> Result<bool> {
        let mut got = 0;
        while got < buf.len() {
            let n = self.read(&mut buf[got..], inter_byte)?;
            if n == 0 {
                return Ok(false);
            }
            got += n;
        }
        Ok(true)
    }

    /// Having just read one CAN, is it followed by another?
    pub fn second_can(&mut self) -> Result<bool> {
        Ok(self.read_byte(CAN_PAIR_TIMEOUT)? == Some(CAN))
    }

    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        let res = self.port.write_all(data).and_then(|()| self.port.flush());
        match res {
            Ok(()) => Ok(()),
            // The remote may have cancelled and hung up before we read its
            // CANs; report that rather than a bare broken pipe.
            Err(_) if self.unread_cancel() => Err(XferError::RemoteCancelled),
            Err(e) => Err(e.into()),
        }
    }

    /// Whether input already received contains CAN CAN.
    fn unread_cancel(&mut self) -> bool {
        let mut buf = [0u8; 256];
        let mut prev_can = false;
        while let Ok(n) = self.port.read_timeout(&mut buf, Duration::ZERO) {
            if n == 0 {
                break;
            }
            for &b in &buf[..n] {
                if b == CAN && prev_can {
                    return true;
                }
                prev_can = b == CAN;
            }
        }
        false
    }

    /// Wait for the line to go quiet, then drop everything received.
    pub fn purge_line(&mut self) -> Result<()> {
        let deadline = Instant::now() + PURGE_MAX;
        let mut scratch = [0u8; 1024];
        while Instant::now() < deadline {
            if self.read(&mut scratch, PURGE_QUIET)? == 0 {
                break;
            }
        }
        self.port.purge_input();
        Ok(())
    }

    pub fn event(&mut self, ev: Event) {
        self.ctl.event(ev);
    }

    pub fn retry(&mut self, reason: impl Into<String>) {
        self.ctl.event(Event::Retry { reason: reason.into() });
    }
}
