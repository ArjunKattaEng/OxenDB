//! The database file header stored in page 0.
//!
//! # Layout
//!
//! All integers are little-endian. Bytes after the listed fields are
//! reserved and written as zero.
//!
//! | Offset | Size | Field                                   |
//! |--------|------|-----------------------------------------|
//! | 0      | 4    | CRC32C of bytes `4..PAGE_SIZE`          |
//! | 4      | 8    | Magic bytes `b"oxenDB\0\0"`             |
//! | 12     | 4    | Format version                          |
//! | 16     | 4    | Page size in bytes                      |
//! | 20     | 8    | Page count, including this header page  |

use crate::error::{Error, Result};
use crate::storage::checksum::crc32c;
use crate::storage::page::{PAGE_SIZE, Page};

/// Identifies a file as an oxenDB database.
pub const MAGIC: [u8; 8] = *b"oxenDB\0\0";

/// On-disk format version written by this build.
///
/// Bump this whenever the file layout changes incompatibly.
pub const FORMAT_VERSION: u32 = 1;

const CHECKSUM_RANGE: std::ops::Range<usize> = 0..4;
const MAGIC_RANGE: std::ops::Range<usize> = 4..12;
const VERSION_RANGE: std::ops::Range<usize> = 12..16;
const PAGE_SIZE_RANGE: std::ops::Range<usize> = 16..20;
const PAGE_COUNT_RANGE: std::ops::Range<usize> = 20..28;

/// Decoded contents of the file header page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// Number of pages in the file, including the header page itself.
    pub page_count: u64,
}

impl FileHeader {
    /// Header for a newly created database containing only the header page.
    pub fn new() -> Self {
        FileHeader { page_count: 1 }
    }

    /// Serializes the header into a sealed page.
    pub fn encode(&self) -> Page {
        let mut page = Page::zeroed();
        let bytes = page.as_bytes_mut();
        bytes[MAGIC_RANGE].copy_from_slice(&MAGIC);
        bytes[VERSION_RANGE].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[PAGE_SIZE_RANGE].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        bytes[PAGE_COUNT_RANGE].copy_from_slice(&self.page_count.to_le_bytes());
        let checksum = crc32c(&bytes[CHECKSUM_RANGE.end..]);
        bytes[CHECKSUM_RANGE].copy_from_slice(&checksum.to_le_bytes());
        page
    }

    /// Parses and validates a header page read from disk.
    pub fn decode(page: &Page) -> Result<Self> {
        let bytes = page.as_bytes();
        // Check the magic first so a non-database file gets a clear error
        // instead of a confusing checksum mismatch.
        if bytes[MAGIC_RANGE] != MAGIC {
            return Err(Error::UnsupportedFormat(
                "not an oxenDB database file".into(),
            ));
        }
        let stored = read_u32(bytes, CHECKSUM_RANGE);
        let actual = crc32c(&bytes[CHECKSUM_RANGE.end..]);
        if stored != actual {
            return Err(Error::corruption(format!(
                "file header checksum mismatch (stored {stored:#010x}, computed {actual:#010x})"
            )));
        }
        let version = read_u32(bytes, VERSION_RANGE);
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedFormat(format!(
                "file format version {version}, this build supports version {FORMAT_VERSION}"
            )));
        }
        let page_size = read_u32(bytes, PAGE_SIZE_RANGE);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::UnsupportedFormat(format!(
                "page size {page_size}, this build supports {PAGE_SIZE}"
            )));
        }
        let page_count = u64::from_le_bytes(bytes[PAGE_COUNT_RANGE].try_into().unwrap());
        if page_count == 0 {
            return Err(Error::corruption("file header reports zero pages"));
        }
        Ok(FileHeader { page_count })
    }
}

impl Default for FileHeader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_u32(bytes: &[u8; PAGE_SIZE], range: std::ops::Range<usize>) -> u32 {
    u32::from_le_bytes(bytes[range].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rewrites a field and recomputes the checksum, simulating a file that
    /// is internally consistent but has unexpected contents.
    fn tamper(page: &mut Page, range: std::ops::Range<usize>, value: &[u8]) {
        let bytes = page.as_bytes_mut();
        bytes[range].copy_from_slice(value);
        let checksum = crc32c(&bytes[CHECKSUM_RANGE.end..]);
        bytes[CHECKSUM_RANGE].copy_from_slice(&checksum.to_le_bytes());
    }

    #[test]
    fn round_trip() {
        let header = FileHeader { page_count: 12345 };
        assert_eq!(FileHeader::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn rejects_foreign_file() {
        let err = FileHeader::decode(&Page::zeroed()).unwrap_err();
        assert!(matches!(err, Error::UnsupportedFormat(_)), "{err}");
    }

    #[test]
    fn rejects_corrupted_header() {
        let mut page = FileHeader::new().encode();
        page.as_bytes_mut()[PAGE_COUNT_RANGE.start] ^= 0x10;
        let err = FileHeader::decode(&page).unwrap_err();
        assert!(matches!(err, Error::Corruption(_)), "{err}");
    }

    #[test]
    fn rejects_future_format_version() {
        let mut page = FileHeader::new().encode();
        tamper(&mut page, VERSION_RANGE, &2u32.to_le_bytes());
        let err = FileHeader::decode(&page).unwrap_err();
        assert!(err.to_string().contains("version 2"), "{err}");
    }

    #[test]
    fn rejects_other_page_size() {
        let mut page = FileHeader::new().encode();
        tamper(&mut page, PAGE_SIZE_RANGE, &8192u32.to_le_bytes());
        assert!(matches!(
            FileHeader::decode(&page),
            Err(Error::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn rejects_zero_page_count() {
        let mut page = FileHeader::new().encode();
        tamper(&mut page, PAGE_COUNT_RANGE, &0u64.to_le_bytes());
        assert!(matches!(
            FileHeader::decode(&page),
            Err(Error::Corruption(_))
        ));
    }
}
