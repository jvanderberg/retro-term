//! Helpers shared by the integration tests.

use std::path::{Path, PathBuf};

/// Deterministic xorshift64* byte generator.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Every byte value, several times, plus ZMODEM-hostile sequences.
pub fn all_byte_values() -> Vec<u8> {
    let mut v: Vec<u8> = (0..=255u8).cycle().take(256 * 8).collect();
    v.extend_from_slice(b"@\r@\r\x18\x18\x18\x18\x18**\x18B00\x18h\x18k\x11\x13\x91\x93");
    v.extend(random_bytes(10_000, 99));
    v
}

pub fn write_file(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, data).unwrap();
    p
}

/// Assert every source file has an identical copy (same name) in `dir`.
pub fn assert_same(files: &[PathBuf], dir: &Path) {
    for f in files {
        let name = f.file_name().unwrap();
        let got = std::fs::read(dir.join(name)).unwrap_or_else(|e| panic!("{name:?} missing: {e}"));
        let want = std::fs::read(f).unwrap();
        assert_eq!(got.len(), want.len(), "{name:?} length");
        assert!(got == want, "{name:?} contents differ");
    }
}

/// A standard set of test files: empty, all byte values, a multi-MB file.
pub fn standard_files(dir: &Path) -> Vec<PathBuf> {
    vec![
        write_file(dir, "empty.bin", &[]),
        write_file(dir, "allbytes.bin", &all_byte_values()),
        write_file(dir, "small.txt", b"hello\r\nworld\n"),
        write_file(dir, "big.bin", &random_bytes(3 * 1024 * 1024 + 123, 7)),
    ]
}
