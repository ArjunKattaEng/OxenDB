//! CRC32C (Castagnoli) checksums for on-disk data.
//!
//! CRC32C is used instead of CRC32 (IEEE) because it has better error
//! detection for the block sizes databases use and has dedicated CPU
//! instructions on x86-64 and ARMv8.
//!
//! [`Crc32c::update`] uses those instructions when the CPU has them (see
//! `hw.rs`) and otherwise falls back to "slicing-by-8": eight lookup tables
//! let each step consume 8 bytes instead of 1. The byte-at-a-time loop
//! remains for the tail and as the reference implementation in tests.

mod hw;

/// Reflected CRC32C polynomial.
const POLY: u32 = 0x82F6_3B78;

/// `TABLES[0]` is the classic byte-at-a-time table. `TABLES[k][b]` is the
/// CRC contribution of byte `b` followed by `k` zero bytes, which is what
/// lets slicing-by-8 process 8 bytes with 8 independent lookups.
const TABLES: [[u32; 256]; 8] = build_tables();

const fn build_tables() -> [[u32; 256]; 8] {
    let mut tables = [[0u32; 256]; 8];
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
        tables[0][i] = crc;
        i += 1;
    }
    let mut k = 1;
    while k < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = tables[k - 1][i];
            tables[k][i] = (prev >> 8) ^ tables[0][(prev & 0xFF) as usize];
            i += 1;
        }
        k += 1;
    }
    tables
}

/// Byte-at-a-time update. Slow, but obviously correct.
fn update_bytewise(mut crc: u32, bytes: &[u8]) -> u32 {
    for &byte in bytes {
        crc = TABLES[0][((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc
}

/// Slicing-by-8 update.
fn update_slice8(mut crc: u32, bytes: &[u8]) -> u32 {
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let lo = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
        let hi = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        crc = TABLES[7][(lo & 0xFF) as usize]
            ^ TABLES[6][((lo >> 8) & 0xFF) as usize]
            ^ TABLES[5][((lo >> 16) & 0xFF) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][(hi & 0xFF) as usize]
            ^ TABLES[2][((hi >> 8) & 0xFF) as usize]
            ^ TABLES[1][((hi >> 16) & 0xFF) as usize]
            ^ TABLES[0][(hi >> 24) as usize];
    }
    update_bytewise(crc, chunks.remainder())
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
        self.state =
            hw::update(self.state, bytes).unwrap_or_else(|| update_slice8(self.state, bytes));
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
    fn slice8_matches_bytewise_reference() {
        let data: Vec<u8> = (0..5000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        // Every length up to 64 at every alignment, plus page-sized inputs.
        for start in 0..8 {
            for len in (0..=64).chain([511, 4095, 4096, 4097, 4992]) {
                let input = &data[start..start + len];
                for seed in [0u32, !0, 0x1234_5678] {
                    assert_eq!(
                        update_slice8(seed, input),
                        update_bytewise(seed, input),
                        "start {start} len {len} seed {seed:#x}"
                    );
                }
            }
        }
    }

    #[test]
    fn hardware_matches_bytewise_reference() {
        let data: Vec<u8> = (0..5000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let Some(_) = hw::update(0, &[]) else {
            eprintln!("no hardware CRC32C on this CPU; skipping");
            return;
        };
        for start in 0..8 {
            for len in (0..=64).chain([511, 4095, 4096, 4097, 4992]) {
                let input = &data[start..start + len];
                for seed in [0u32, !0, 0x1234_5678] {
                    assert_eq!(
                        hw::update(seed, input),
                        Some(update_bytewise(seed, input)),
                        "start {start} len {len} seed {seed:#x}"
                    );
                }
            }
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
