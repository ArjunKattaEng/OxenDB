//! Fixed-size pages, the unit of disk I/O and caching.

use std::fmt;

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
}
