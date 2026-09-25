//! Fixed-size pages, the unit of disk I/O and caching.
//!
//! # Page header layout
//!
//! Every page except the file header page starts with a 24-byte header.
//! All integers are little-endian.
//!
//! | Offset | Size | Field                                        |
//! |--------|------|----------------------------------------------|
//! | 0      | 4    | CRC32C of bytes `4..PAGE_SIZE`               |
//! | 4      | 1    | Page type                                    |
//! | 5      | 3    | Reserved, must be zero                       |
//! | 8      | 8    | Page id (detects misdirected reads/writes)   |
//! | 16     | 8    | LSN of the last WAL record applied to page   |

use std::fmt;

use crate::error::{Error, Result};
use crate::storage::checksum::crc32c;

/// Size of every page in bytes.
///
/// 4 KiB matches the block size of common filesystems and SSDs, so a page
/// write maps to a single device block. Making this configurable per
/// database is possible later; it is a constant for now so the compiler can
/// optimize fixed-size copies.
pub const PAGE_SIZE: usize = 4096;

/// Identifies a page by its position in the database file.
///
/// Page `n` lives at byte offset `n * PAGE_SIZE`. Page 0 holds the file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u64);

impl PageId {
    /// The page holding the database file header.
    pub const HEADER: PageId = PageId(0);

    /// Byte offset of this page within the database file.
    pub fn file_offset(self) -> u64 {
        self.0 * PAGE_SIZE as u64
    }
}

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "page {}", self.0)
    }
}

/// Size of the header at the start of every data page.
pub const PAGE_HEADER_SIZE: usize = 24;

const CHECKSUM_RANGE: std::ops::Range<usize> = 0..4;
const TYPE_OFFSET: usize = 4;
const RESERVED_RANGE: std::ops::Range<usize> = 5..8;
const PAGE_ID_RANGE: std::ops::Range<usize> = 8..16;
const LSN_RANGE: std::ops::Range<usize> = 16..24;

/// What a page is used for.
///
/// Zero is deliberately not a valid type so that an all-zero page (for
/// example, a hole in a sparse file) is never mistaken for real data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    /// Allocated but not in use.
    Free = 1,
    /// Holds table rows.
    Heap = 2,
}

impl TryFrom<u8> for PageType {
    type Error = u8;

    fn try_from(value: u8) -> std::result::Result<Self, u8> {
        match value {
            1 => Ok(PageType::Free),
            2 => Ok(PageType::Heap),
            other => Err(other),
        }
    }
}

/// Decoded page header fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageHeader {
    /// What the page holds.
    pub page_type: PageType,
    /// The page's own id, as written when the page was sealed.
    pub page_id: PageId,
    /// Log sequence number of the last change applied to the page.
    pub lsn: u64,
}

/// An owned, heap-allocated page buffer.
///
/// Pages are boxed so moving a `Page` moves a pointer rather than 4 KiB.
#[derive(Clone, PartialEq, Eq)]
pub struct Page {
    data: Box<[u8; PAGE_SIZE]>,
}

impl Page {
    /// Returns a page with every byte set to zero.
    pub fn zeroed() -> Self {
        Page {
            data: Box::new([0u8; PAGE_SIZE]),
        }
    }

    /// Returns a zeroed page with an initialized header and LSN 0.
    ///
    /// The checksum is not valid until [`Page::seal`] is called.
    pub fn new(page_id: PageId, page_type: PageType) -> Self {
        let mut page = Page::zeroed();
        page.data[TYPE_OFFSET] = page_type as u8;
        page.data[PAGE_ID_RANGE].copy_from_slice(&page_id.0.to_le_bytes());
        page
    }

    /// Updates the LSN stored in the header.
    pub fn set_lsn(&mut self, lsn: u64) {
        self.data[LSN_RANGE].copy_from_slice(&lsn.to_le_bytes());
    }

    /// Bytes after the header, available to the page's owner.
    pub fn payload(&self) -> &[u8] {
        &self.data[PAGE_HEADER_SIZE..]
    }

    /// Mutable bytes after the header.
    pub fn payload_mut(&mut self) -> &mut [u8] {
        &mut self.data[PAGE_HEADER_SIZE..]
    }

    /// Computes and stores the checksum. Call before writing to disk.
    pub fn seal(&mut self) {
        let checksum = crc32c(&self.data[CHECKSUM_RANGE.end..]);
        self.data[CHECKSUM_RANGE].copy_from_slice(&checksum.to_le_bytes());
    }

    /// Validates a page read from disk as `expected` and returns its header.
    ///
    /// Fails with [`Error::Corruption`] if the checksum does not match, the
    /// header is malformed, or the page claims to be a different page.
    pub fn verify(&self, expected: PageId) -> Result<PageHeader> {
        let stored = u32::from_le_bytes(self.data[CHECKSUM_RANGE].try_into().unwrap());
        let actual = crc32c(&self.data[CHECKSUM_RANGE.end..]);
        if stored != actual {
            return Err(Error::corruption(format!(
                "{expected}: checksum mismatch (stored {stored:#010x}, computed {actual:#010x})"
            )));
        }
        let header = self.decode_header(expected)?;
        if header.page_id != expected {
            return Err(Error::corruption(format!(
                "{expected}: header says it is {}",
                header.page_id
            )));
        }
        Ok(header)
    }

    fn decode_header(&self, expected: PageId) -> Result<PageHeader> {
        let page_type = PageType::try_from(self.data[TYPE_OFFSET])
            .map_err(|raw| Error::corruption(format!("{expected}: unknown page type {raw}")))?;
        if self.data[RESERVED_RANGE].iter().any(|&b| b != 0) {
            return Err(Error::corruption(format!(
                "{expected}: reserved header bytes are set"
            )));
        }
        let page_id = PageId(u64::from_le_bytes(
            self.data[PAGE_ID_RANGE].try_into().unwrap(),
        ));
        let lsn = u64::from_le_bytes(self.data[LSN_RANGE].try_into().unwrap());
        Ok(PageHeader {
            page_type,
            page_id,
            lsn,
        })
    }

    /// Read-only view of the raw page bytes.
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.data
    }

    /// Mutable view of the raw page bytes.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.data
    }
}

impl fmt::Debug for Page {
    // Printing 4 KiB of bytes is never useful; show only the length.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page")
            .field("len", &PAGE_SIZE)
            .finish_non_exhaustive()
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::zeroed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_offsets() {
        assert_eq!(PageId::HEADER.file_offset(), 0);
        assert_eq!(PageId(3).file_offset(), 3 * 4096);
    }

    #[test]
    fn zeroed_page_is_all_zero() {
        let page = Page::zeroed();
        assert!(page.as_bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn writes_are_visible() {
        let mut page = Page::zeroed();
        page.as_bytes_mut()[100] = 42;
        assert_eq!(page.as_bytes()[100], 42);
    }

    fn sealed_page(id: u64) -> Page {
        let mut page = Page::new(PageId(id), PageType::Heap);
        page.set_lsn(77);
        page.payload_mut()[..5].copy_from_slice(b"hello");
        page.seal();
        page
    }

    #[test]
    fn sealed_page_round_trips() {
        let page = sealed_page(9);
        let header = page.verify(PageId(9)).unwrap();
        assert_eq!(
            header,
            PageHeader {
                page_type: PageType::Heap,
                page_id: PageId(9),
                lsn: 77
            }
        );
        assert_eq!(&page.payload()[..5], b"hello");
        assert_eq!(page.payload().len(), PAGE_SIZE - PAGE_HEADER_SIZE);
    }

    #[test]
    fn every_single_bit_flip_is_detected() {
        let original = sealed_page(9);
        for byte in 0..PAGE_SIZE {
            for bit in 0..8 {
                let mut page = original.clone();
                page.as_bytes_mut()[byte] ^= 1 << bit;
                assert!(
                    page.verify(PageId(9)).is_err(),
                    "flip at byte {byte} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn misdirected_page_is_rejected() {
        let page = sealed_page(9);
        let err = page.verify(PageId(10)).unwrap_err();
        assert!(matches!(err, Error::Corruption(_)), "{err}");
    }

    #[test]
    fn zeroed_page_is_rejected() {
        assert!(Page::zeroed().verify(PageId(1)).is_err());
    }

    #[test]
    fn unknown_page_type_is_rejected_even_with_valid_checksum() {
        let mut page = sealed_page(9);
        page.as_bytes_mut()[TYPE_OFFSET] = 200;
        page.seal();
        let err = page.verify(PageId(9)).unwrap_err();
        assert!(err.to_string().contains("unknown page type 200"), "{err}");
    }

    #[test]
    fn unsealed_modification_is_rejected() {
        let mut page = sealed_page(9);
        page.payload_mut()[0] = b'j';
        assert!(page.verify(PageId(9)).is_err());
    }
}
