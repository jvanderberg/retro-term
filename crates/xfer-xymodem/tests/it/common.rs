//! Shared helpers for the integration tests.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use xfer_common::{Event, Port};

pub const SUB: u8 = 0x1A;
pub const ACK: u8 = 0x06;
pub const NAK: u8 = 0x15;

/// Deterministic pseudo-random bytes (xorshift). The last byte is never SUB
/// so XMODEM's padding strip can't eat real data.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    let mut v: Vec<u8> = (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    if let Some(last) = v.last_mut() {
        if *last == SUB {
            *last = 0x55;
        }
    }
    v
}

/// Every byte value, a few times over, plus a tail.
pub fn all_bytes() -> Vec<u8> {
    let mut v: Vec<u8> = (0..5).flat_map(|_| 0..=255u8).collect();
    v.extend_from_slice(b"tail");
    v
}

/// The standard test set: (file name, contents).
pub fn test_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("empty.bin".into(), Vec::new()),
        ("one.bin".into(), vec![0x42]),
        ("exact128.bin".into(), random_bytes(128 * 3, 1)),
        ("exact1024.bin".into(), random_bytes(1024 * 4, 2)),
        ("odd1100.bin".into(), random_bytes(1100, 3)),
        ("allbytes.bin".into(), all_bytes()),
        ("big.bin".into(), random_bytes(1_500_000, 4)),
    ]
}

pub fn write_files(dir: &Path, files: &[(String, Vec<u8>)]) -> Vec<PathBuf> {
    files
        .iter()
        .map(|(name, data)| {
            let p = dir.join(name);
            std::fs::write(&p, data).unwrap();
            p
        })
        .collect()
}

/// XMODEM output equals `orig` followed only by SUB padding up to `block`.
pub fn assert_padded_eq(got: &[u8], orig: &[u8], block: usize, what: &str) {
    assert!(got.len() >= orig.len(), "{what}: received {} < sent {}", got.len(), orig.len());
    assert_eq!(&got[..orig.len()], orig, "{what}: content differs");
    assert!(got[orig.len()..].iter().all(|&b| b == SUB), "{what}: non-SUB padding");
    assert!(got.len() - orig.len() < block, "{what}: too much padding");
}

pub fn count_retries(events: &[Event]) -> usize {
    events.iter().filter(|e| matches!(e, Event::Retry { .. })).count()
}

/// A port wrapper that damages traffic in controlled ways.
pub struct Faulty<P: Port> {
    pub inner: P,
    /// Corrupt one byte of every Nth large write (a block).
    pub corrupt_every: Option<usize>,
    /// Turn these (1-based) single-byte ACK writes into NAKs.
    pub ack_to_nak: Vec<usize>,
    big_writes: usize,
    acks: usize,
}

impl<P: Port> Faulty<P> {
    pub fn new(inner: P) -> Self {
        Self { inner, corrupt_every: None, ack_to_nak: Vec::new(), big_writes: 0, acks: 0 }
    }
}

impl<P: Port> Port for Faulty<P> {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        self.inner.read_timeout(buf, timeout)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if data.len() > 100 {
            self.big_writes += 1;
            if self.corrupt_every.is_some_and(|n| self.big_writes.is_multiple_of(n)) {
                let mut v = data.to_vec();
                // Alternate between damaging the data and the block number.
                let idx = if self.big_writes.is_multiple_of(2) { 50 } else { 1 };
                v[idx] ^= 0x5A;
                return self.inner.write_all(&v);
            }
        }
        if data == [ACK] {
            self.acks += 1;
            if self.ack_to_nak.contains(&self.acks) {
                return self.inner.write_all(&[NAK]);
            }
        }
        self.inner.write_all(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn purge_input(&mut self) {
        self.inner.purge_input()
    }
}
