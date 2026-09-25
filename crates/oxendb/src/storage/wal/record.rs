//! Encoding and decoding of individual WAL records.
//!
//! # Record layout
//!
//! All integers are little-endian.
//!
//! | Offset | Size | Field                                          |
//! |--------|------|------------------------------------------------|
//! | 0      | 4    | Total record length in bytes, including this   |
//! | 4      | 4    | CRC32C of bytes `8..length`                    |
//! | 8      | 8    | Sequence number (previous record's + 1)        |
//! | 16     | 1    | Record type                                    |
//! | 17     | 8    | Transaction id                                 |
//! | 25     | ...  | Type-specific body                             |
//!
//! Bodies:
//!
//! - `PageImage` (type 1): page id (8 bytes), then the full page (4096 bytes).
//! - `Commit` (type 2): empty.
//!
//! The length is covered by the checksum indirectly: a wrong length makes the
//! checksum cover the wrong bytes, and each type has exactly one valid length.

use crate::storage::checksum::crc32c;
use crate::storage::page::{PAGE_SIZE, Page, PageId};

/// Size of the fields common to every record.
pub const RECORD_HEADER_SIZE: usize = 25;

const PAGE_IMAGE_LEN: usize = RECORD_HEADER_SIZE + 8 + PAGE_SIZE;
const COMMIT_LEN: usize = RECORD_HEADER_SIZE;

const TYPE_PAGE_IMAGE: u8 = 1;
const TYPE_COMMIT: u8 = 2;

/// Identifies a write transaction within the WAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId(pub u64);

/// A logical WAL record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalRecord {
    /// The full contents of a page as written by a transaction.
    PageImage {
        /// Writing transaction.
        txn: TxnId,
        /// Page the image belongs to.
        page_id: PageId,
        /// The page contents. Sealed before logging.
        page: Page,
    },
    /// Marks a transaction as committed. Its page images before this record
    /// take effect; images from transactions without a commit are ignored.
    Commit {
        /// Committing transaction.
        txn: TxnId,
    },
}

/// A record together with the sequence number it was stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRecord {
    /// The record's sequence number.
    pub seq: u64,
    /// The record itself.
    pub record: WalRecord,
    /// Number of bytes the record occupied.
    pub len: usize,
}

/// Why a record could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ends before the record does (a torn tail or end of log).
    Incomplete,
    /// The bytes are not a valid record.
    Invalid(String),
}

impl WalRecord {
    /// The transaction the record belongs to.
    pub fn txn(&self) -> TxnId {
        match self {
            WalRecord::PageImage { txn, .. } | WalRecord::Commit { txn } => *txn,
        }
    }

    /// Appends the encoded record, stored under `seq`, to `out`.
    pub fn encode(&self, seq: u64, out: &mut Vec<u8>) {
        let start = out.len();
        let (len, record_type) = match self {
            WalRecord::PageImage { .. } => (PAGE_IMAGE_LEN, TYPE_PAGE_IMAGE),
            WalRecord::Commit { .. } => (COMMIT_LEN, TYPE_COMMIT),
        };
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]); // checksum, filled in below
        out.extend_from_slice(&seq.to_le_bytes());
        out.push(record_type);
        out.extend_from_slice(&self.txn().0.to_le_bytes());
        if let WalRecord::PageImage { page_id, page, .. } = self {
            out.extend_from_slice(&page_id.0.to_le_bytes());
            out.extend_from_slice(page.as_bytes());
        }
        debug_assert_eq!(out.len() - start, len);
        let checksum = crc32c(&out[start + 8..]);
        out[start + 4..start + 8].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Decodes the record at the start of `buf`.
    pub fn decode(buf: &[u8]) -> Result<DecodedRecord, DecodeError> {
        if buf.len() < 4 {
            return Err(DecodeError::Incomplete);
        }
        let len = read_u32(buf, 0) as usize;
        if len != PAGE_IMAGE_LEN && len != COMMIT_LEN {
            return Err(DecodeError::Invalid(format!(
                "impossible record length {len}"
            )));
        }
        if buf.len() < len {
            return Err(DecodeError::Incomplete);
        }
        let bytes = &buf[..len];
        let stored = read_u32(bytes, 4);
        let actual = crc32c(&bytes[8..]);
        if stored != actual {
            return Err(DecodeError::Invalid(format!(
                "checksum mismatch (stored {stored:#010x}, computed {actual:#010x})"
            )));
        }
        let seq = read_u64(bytes, 8);
        let txn = TxnId(read_u64(bytes, 17));
        let record = match (bytes[16], len) {
            (TYPE_PAGE_IMAGE, PAGE_IMAGE_LEN) => {
                let page_id = PageId(read_u64(bytes, RECORD_HEADER_SIZE));
                let mut page = Page::zeroed();
                page.as_bytes_mut()
                    .copy_from_slice(&bytes[RECORD_HEADER_SIZE + 8..]);
                WalRecord::PageImage { txn, page_id, page }
            }
            (TYPE_COMMIT, COMMIT_LEN) => WalRecord::Commit { txn },
            (record_type, len) => {
                return Err(DecodeError::Invalid(format!(
                    "record type {record_type} with length {len}"
                )));
            }
        };
        Ok(DecodedRecord { seq, record, len })
    }
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::page::PageType;

    fn image(txn: u64, page_id: u64, fill: u8) -> WalRecord {
        let mut page = Page::new(PageId(page_id), PageType::Heap);
        page.payload_mut().fill(fill);
        page.seal();
        WalRecord::PageImage {
            txn: TxnId(txn),
            page_id: PageId(page_id),
            page,
        }
    }

    fn encode(record: &WalRecord, seq: u64) -> Vec<u8> {
        let mut out = Vec::new();
        record.encode(seq, &mut out);
        out
    }

    #[test]
    fn page_image_round_trips() {
        let record = image(3, 9, 0xAB);
        let bytes = encode(&record, 41);
        assert_eq!(bytes.len(), PAGE_IMAGE_LEN);
        let decoded = WalRecord::decode(&bytes).unwrap();
        assert_eq!(
            decoded,
            DecodedRecord {
                seq: 41,
                record,
                len: PAGE_IMAGE_LEN
            }
        );
    }

    #[test]
    fn commit_round_trips() {
        let record = WalRecord::Commit {
            txn: TxnId(u64::MAX),
        };
        let bytes = encode(&record, 0);
        assert_eq!(WalRecord::decode(&bytes).unwrap().record, record);
    }

    #[test]
    fn decodes_first_of_several_records() {
        let mut bytes = encode(&WalRecord::Commit { txn: TxnId(1) }, 5);
        image(2, 3, 1).encode(6, &mut bytes);
        let first = WalRecord::decode(&bytes).unwrap();
        assert_eq!(first.seq, 5);
        let second = WalRecord::decode(&bytes[first.len..]).unwrap();
        assert_eq!(second.seq, 6);
    }

    #[test]
    fn every_truncation_is_incomplete() {
        for record in [image(1, 1, 7), WalRecord::Commit { txn: TxnId(1) }] {
            let bytes = encode(&record, 1);
            for cut in 0..bytes.len() {
                assert_eq!(
                    WalRecord::decode(&bytes[..cut]),
                    Err(DecodeError::Incomplete),
                    "cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn every_single_bit_flip_is_rejected() {
        for record in [image(1, 1, 7), WalRecord::Commit { txn: TxnId(1) }] {
            let original = encode(&record, 1);
            for byte in 0..original.len() {
                for bit in 0..8 {
                    let mut bytes = original.clone();
                    bytes[byte] ^= 1 << bit;
                    match WalRecord::decode(&bytes) {
                        Ok(_) => panic!("flip at byte {byte} bit {bit} was accepted"),
                        Err(DecodeError::Invalid(_) | DecodeError::Incomplete) => {}
                    }
                }
            }
        }
    }

    #[test]
    fn zeroed_bytes_are_invalid() {
        assert!(matches!(
            WalRecord::decode(&[0u8; 64]),
            Err(DecodeError::Invalid(_))
        ));
    }

    #[test]
    fn mismatched_type_and_length_is_invalid() {
        let mut bytes = encode(&WalRecord::Commit { txn: TxnId(1) }, 1);
        bytes[16] = TYPE_PAGE_IMAGE;
        let checksum = crc32c(&bytes[8..]);
        bytes[4..8].copy_from_slice(&checksum.to_le_bytes());
        assert!(matches!(
            WalRecord::decode(&bytes),
            Err(DecodeError::Invalid(_))
        ));
    }
}
