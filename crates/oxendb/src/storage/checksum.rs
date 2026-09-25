//! CRC32C (Castagnoli) checksums for on-disk data.
//!
//! CRC32C is used instead of CRC32 (IEEE) because it has better error
//! detection for the block sizes databases use and has dedicated CPU
//! instructions on x86-64 and ARMv8, which a later change can take advantage
//! of. This portable implementation is the reference the accelerated one will
//! be tested against.

/// Reflected CRC32C polynomial.
const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Incremental CRC32C hasher for data that arrives in pieces.
#[derive(Debug, Clone, Copy)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    /// Starts a new checksum.
    pub const fn new() -> Self {
        Crc32c { state: !0 }
    }

    /// Feeds more bytes into the checksum.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut crc = self.state;
        for &byte in bytes {
            crc = TABLE[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        self.state = crc;
    }

    /// Returns the checksum of all bytes fed so far.
    pub const fn finalize(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes the CRC32C of `bytes` in one call.
pub fn crc32c(bytes: &[u8]) -> u32 {
    let mut hasher = Crc32c::new();
    hasher.update(bytes);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn empty_input() {
        assert_eq!(crc32c(&[]), 0);
    }

    // Test vectors from RFC 3720, appendix B.4.
    #[test]
    fn rfc3720_vectors() {
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
        let descending: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
        for split in [0, 1, 7, 500, 999, 1000] {
            let mut hasher = Crc32c::new();
            hasher.update(&data[..split]);
            hasher.update(&data[split..]);
            assert_eq!(hasher.finalize(), crc32c(&data), "split at {split}");
        }
    }

    #[test]
    fn detects_single_bit_flip() {
        let mut data = vec![0x5Au8; 4096];
        let original = crc32c(&data);
        data[2048] ^= 0x01;
        assert_ne!(crc32c(&data), original);
    }
}
