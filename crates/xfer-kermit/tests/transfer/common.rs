//! Shared helpers for the integration tests.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use xfer_common::Port;

/// Small deterministic PRNG (xorshift64*).
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// True with probability `per_mille / 1000`.
    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.next_u64() % 1000 < per_mille
    }
}

pub fn random_bytes(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Rng::new(seed);
    (0..n).map(|_| (r.next_u64() >> 32) as u8).collect()
}

/// A set of interesting test files written into `dir`.
pub fn make_files(dir: &Path, big: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut put = |name: &str, data: &[u8]| {
        let p = dir.join(name);
        fs::write(&p, data).unwrap();
        files.push(p);
    };
    put("empty.dat", b"");
    let all: Vec<u8> = (0..=255u8).cycle().take(256 * 4).collect();
    put("all-bytes.bin", &all);
    put("hello.txt", b"Hello, Kermit!\r\nSecond line with # & ~ prefixes.\n");
    let mut big_data = random_bytes(42, big);
    // A compressible stretch to exercise repeat counts.
    big_data.extend(std::iter::repeat_n(0u8, 70_000));
    big_data.extend(std::iter::repeat_n(b'~', 3_000));
    big_data.extend(random_bytes(7, 5_000));
    put("big.bin", &big_data);
    files
}

pub fn assert_same_files(sent: &[PathBuf], dest: &Path) {
    for src in sent {
        let name = src.file_name().unwrap();
        let got = fs::read(dest.join(name)).unwrap_or_else(|e| panic!("{name:?} missing: {e}"));
        let want = fs::read(src).unwrap();
        assert!(got == want, "{name:?}: contents differ ({} vs {} bytes)", got.len(), want.len());
    }
}

/// Port wrapper that drops and corrupts whole writes (i.e. packets) and can
/// strip the 8th bit to simulate a 7-bit line.
pub struct FaultyPort<P: Port> {
    pub inner: P,
    pub rng: Rng,
    pub drop_per_mille: u64,
    pub corrupt_per_mille: u64,
    pub strip_8th_bit: bool,
    pub dropped: u64,
    pub corrupted: u64,
}

impl<P: Port> FaultyPort<P> {
    pub fn new(inner: P, seed: u64) -> Self {
        Self {
            inner,
            rng: Rng::new(seed),
            drop_per_mille: 0,
            corrupt_per_mille: 0,
            strip_8th_bit: false,
            dropped: 0,
            corrupted: 0,
        }
    }
}

impl<P: Port> Port for FaultyPort<P> {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        let n = self.inner.read_timeout(buf, timeout)?;
        if self.strip_8th_bit {
            for b in &mut buf[..n] {
                *b &= 0x7F;
            }
        }
        Ok(n)
    }

    fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        if self.rng.chance(self.drop_per_mille) {
            self.dropped += 1;
            return Ok(());
        }
        let mut d = data.to_vec();
        if self.strip_8th_bit {
            for b in &mut d {
                *b &= 0x7F;
            }
        }
        if !d.is_empty() && self.rng.chance(self.corrupt_per_mille) {
            let i = (self.rng.next_u64() % d.len() as u64) as usize;
            d[i] ^= 1 << (self.rng.next_u64() % 7);
            self.corrupted += 1;
        }
        self.inner.write_all(&d)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn purge_input(&mut self) {
        self.inner.purge_input()
    }
}
