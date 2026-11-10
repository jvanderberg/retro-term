//! CRC routines used by the transfer protocols.

const fn make_crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn make_reflected_table32(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

const fn make_reflected_table16(poly: u16) -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u16;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static CRC16_TABLE: [u16; 256] = make_crc16_table();
static CRC32_TABLE: [u32; 256] = make_reflected_table32(0xEDB8_8320);
static KERMIT_TABLE: [u16; 256] = make_reflected_table16(0x8408);

/// Update a CRC-16/XMODEM (CCITT poly 0x1021, MSB first) with `data`.
/// Start from 0 for XMODEM/YMODEM/ZMODEM.
pub fn crc16_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc << 8) ^ CRC16_TABLE[(((crc >> 8) as u8) ^ b) as usize];
    }
    crc
}

/// CRC-16/XMODEM of `data` (initial value 0).
pub fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// Update a raw (un-inverted) reflected CRC-32/IEEE register. ZMODEM starts
/// with 0xFFFFFFFF and transmits the complement of the final register.
pub fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc as u8) ^ b) as usize];
    }
    crc
}

/// Standard CRC-32/IEEE (as used by zip, ZMODEM, ...).
pub fn crc32(data: &[u8]) -> u32 {
    !crc32_update(0xFFFF_FFFF, data)
}

/// Kermit's 16-bit CRC (CRC-CCITT reflected, poly 0x8408, init 0).
pub fn crc16_kermit_update(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ KERMIT_TABLE[((crc as u8) ^ b) as usize];
    }
    crc
}

pub fn crc16_kermit(data: &[u8]) -> u16 {
    crc16_kermit_update(0, data)
}

/// The 8-bit arithmetic checksum used by original XMODEM.
pub fn checksum8(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_values() {
        // Standard "123456789" check values.
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc16_kermit(b"123456789"), 0x2189);
    }
}
