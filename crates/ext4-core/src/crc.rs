//! Checksums used by ext4 and jbd2.
//!
//! ext4 and jbd2 store CRC32C values *without* the final inversion: the
//! running value starts at `!0` (or at a seed derived the same way) and the
//! raw register is stored on disk. [`crc32c`] implements exactly that.
//! The `gdt_csum` feature uses CRC16 (polynomial 0x8005, reflected) instead.

/// Raw CRC32C update: continue the register `crc` over `data`.
/// No pre- or post-inversion is applied.
#[inline]
pub fn crc32c(crc: u32, data: &[u8]) -> u32 {
    // `crc32c_append` takes and returns *finalized* values (inverted register).
    !::crc32c::crc32c_append(!crc, data)
}

const fn crc32_be_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u32) << 24;
        let mut k = 0;
        while k < 8 {
            c = if c & 0x8000_0000 != 0 {
                (c << 1) ^ 0x04C1_1DB7
            } else {
                c << 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32_BE_TABLE: [u32; 256] = crc32_be_table();

/// jbd2 v1 checksums (`JBD2_FEATURE_COMPAT_CHECKSUM`) use standard CRC32
/// (big-endian polynomial 0x04C11DB7 as in `crc32_be`).
pub fn crc32_be(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc = (crc << 8) ^ CRC32_BE_TABLE[((crc >> 24) ^ byte as u32) as usize];
    }
    crc
}

const fn crc16_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u16;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xA001
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC16_TABLE: [u16; 256] = crc16_table();

/// CRC16 (ANSI, reflected 0xA001) as used by `gdt_csum` group descriptors.
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc = (crc >> 8) ^ CRC16_TABLE[((crc ^ b as u16) & 0xFF) as usize];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_matches_standard_vector() {
        // Standard CRC32C("123456789") = 0xE3069283 with pre/post inversion.
        assert_eq!(!crc32c(!0, b"123456789"), 0xE306_9283);
    }

    #[test]
    fn crc32c_is_incremental() {
        let whole = crc32c(!0, b"hello world");
        let split = crc32c(crc32c(!0, b"hello "), b"world");
        assert_eq!(whole, split);
    }

    #[test]
    fn crc16_matches_arc_vector() {
        // CRC-16/ARC("123456789") = 0xBB3D (init 0).
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    #[test]
    fn crc32_be_matches_vector() {
        // CRC-32/MPEG-2("123456789") = 0x0376E6E7 (init !0, no final xor).
        assert_eq!(crc32_be(!0, b"123456789"), 0x0376_E6E7);
    }
}
