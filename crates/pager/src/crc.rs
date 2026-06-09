//! CRC32C (Castagnoli) checksum — software, table-driven.
//!
//! Implemented in-house rather than pulled as a dependency: it is a small,
//! well-understood, non-security-sensitive integrity check (used to detect page
//! corruption, not to resist tampering). See `DECISIONS.md` D3.

/// Reflected Castagnoli polynomial (0x1EDC6F41 reflected).
const POLY: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ POLY;
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// Compute the CRC32C of `data`.
///
/// # Examples
///
/// ```
/// // Standard CRC32C check value for the ASCII string "123456789".
/// assert_eq!(pager::crc32c(b"123456789"), 0xE306_9283);
/// ```
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        let idx = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn detects_single_bit_flips() {
        let a = crc32c(b"the quick brown fox");
        let b = crc32c(b"the quick brown fox.");
        assert_ne!(a, b);
    }
}
