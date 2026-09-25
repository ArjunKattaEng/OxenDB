//! Randomized robustness tests for on-disk decoders.
//!
//! Decoders read bytes that may be corrupt or hostile. They must return an
//! error for bad input, never panic. These tests feed them random and
//! mutated inputs from a fixed-seed generator, so failures reproduce.
//!
//! Most random mutations fail the checksum immediately, which would leave
//! the validation behind it untested, so each test also re-seals the
//! checksum after mutating.
//!
//! This is not coverage-guided fuzzing; that is still to do (see
//! `docs/release-criteria.md`). Set `OXENDB_ROBUSTNESS_ITERS` to run more
//! cases than the default.

mod common;

use oxendb::storage::checksum::crc32c;
use oxendb::storage::file_header::FileHeader;
use oxendb::storage::page::{Page, PageId, PageType};
use oxendb::storage::wal::record::{DecodeError, WalRecord};
use oxendb::storage::wal::{TxnId, Wal};

/// xorshift64*: tiny, deterministic, good enough for test inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

fn iterations(default: usize) -> usize {
    std::env::var("OXENDB_ROBUSTNESS_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Applies 1 to 8 random edits: bit flips, byte overwrites, or u64 writes of
/// interesting values (zero, max, small numbers) at random offsets.
fn mutate(rng: &mut Rng, bytes: &mut [u8]) {
    for _ in 0..1 + rng.below(8) {
        let at = rng.below(bytes.len());
        match rng.below(3) {
            0 => bytes[at] ^= 1 << rng.below(8),
            1 => bytes[at] = rng.byte(),
            _ => {
                let value: u64 = match rng.below(4) {
                    0 => 0,
                    1 => u64::MAX,
                    2 => rng.below(8) as u64,
                    _ => rng.next(),
                };
                let end = (at + 8).min(bytes.len());
                bytes[at..end].copy_from_slice(&value.to_le_bytes()[..end - at]);
            }
        }
    }
}

fn valid_page(id: u64) -> Page {
    let mut page = Page::new(PageId(id), PageType::Heap);
    page.payload_mut()[..16].copy_from_slice(b"robustness-check");
    page.seal();
    page
}

fn valid_records() -> Vec<Vec<u8>> {
    let mut image = Vec::new();
    WalRecord::PageImage {
        txn: TxnId(7),
        page_id: PageId(3),
        page: valid_page(3),
    }
    .encode(11, &mut image);
    let mut commit = Vec::new();
    WalRecord::Commit { txn: TxnId(7) }.encode(12, &mut commit);
    vec![image, commit]
}

/// Recomputes a WAL record's checksum if its length field fits the buffer.
fn reseal_record(bytes: &mut [u8]) {
    if bytes.len() < 8 {
        return;
    }
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    if (8..=bytes.len()).contains(&len) {
        let checksum = crc32c(&bytes[8..len]);
        bytes[4..8].copy_from_slice(&checksum.to_le_bytes());
    }
}

#[test]
fn wal_record_decoder_never_panics() {
    let mut rng = Rng(0x5EED_0001);
    let seeds = valid_records();
    for i in 0..iterations(20_000) {
        let mut bytes = seeds[i % seeds.len()].clone();
        mutate(&mut rng, &mut bytes);
        if rng.below(2) == 0 {
            reseal_record(&mut bytes);
        }
        let cut = if rng.below(4) == 0 {
            rng.below(bytes.len() + 1)
        } else {
            bytes.len()
        };
        match WalRecord::decode(&bytes[..cut]) {
            // Anything accepted must re-encode to exactly the bytes read.
            Ok(decoded) => {
                let mut again = Vec::new();
                decoded.record.encode(decoded.seq, &mut again);
                assert_eq!(again, bytes[..decoded.len], "case {i}");
            }
            Err(DecodeError::Incomplete | DecodeError::Invalid(_)) => {}
        }
    }
}

#[test]
fn wal_record_decoder_handles_random_bytes() {
    let mut rng = Rng(0x5EED_0002);
    for _ in 0..iterations(20_000) {
        let len = rng.below(64);
        let mut bytes: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
        reseal_record(&mut bytes);
        let _ = WalRecord::decode(&bytes);
    }
}

#[test]
fn page_verify_never_panics() {
    let mut rng = Rng(0x5EED_0003);
    let original = valid_page(5);
    for i in 0..iterations(5_000) {
        let mut page = original.clone();
        mutate(&mut rng, page.as_bytes_mut());
        let resealed = rng.below(2) == 0;
        if resealed {
            page.seal();
        }
        if let Ok(header) = page.verify(PageId(5)) {
            assert_eq!(header.page_id, PageId(5), "case {i}");
        }
    }
}

#[test]
fn file_header_decoder_never_panics() {
    let mut rng = Rng(0x5EED_0004);
    let original = FileHeader { page_count: 42 }.encode();
    for _ in 0..iterations(5_000) {
        let mut page = original.clone();
        // Keep mutations in the first 64 bytes, where the fields are.
        mutate(&mut rng, &mut page.as_bytes_mut()[..64]);
        if rng.below(2) == 0 {
            let checksum = crc32c(&page.as_bytes()[4..]);
            page.as_bytes_mut()[..4].copy_from_slice(&checksum.to_le_bytes());
        }
        if let Ok(header) = FileHeader::decode(&page) {
            assert!(header.page_count > 0);
        }
    }
}

#[test]
fn wal_open_never_panics_on_corrupt_files() {
    let dir = common::TempDir::new("robust-wal");
    let path = dir.path().join("w.oxen-wal");
    {
        let mut wal = Wal::create(&path).unwrap();
        for record in [
            WalRecord::PageImage {
                txn: TxnId(1),
                page_id: PageId(1),
                page: valid_page(1),
            },
            WalRecord::Commit { txn: TxnId(1) },
            WalRecord::Commit { txn: TxnId(2) },
        ] {
            wal.append(&record).unwrap();
        }
        wal.flush().unwrap();
    }
    let original = std::fs::read(&path).unwrap();
    let mut rng = Rng(0x5EED_0005);
    for i in 0..iterations(300) {
        let mut bytes = original.clone();
        mutate(&mut rng, &mut bytes);
        let cut = if rng.below(2) == 0 {
            rng.below(bytes.len() + 1)
        } else {
            bytes.len()
        };
        std::fs::write(&path, &bytes[..cut]).unwrap();
        if let Ok(opened) = Wal::open(&path) {
            // Whatever survives must be a prefix of what was written, in order.
            for (n, record) in opened.records.iter().enumerate() {
                assert!(n < 3, "case {i}: more records than were written");
                assert_eq!(record.seq, opened.records[0].seq + n as u64, "case {i}");
            }
        }
    }
}
