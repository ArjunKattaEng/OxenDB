//! Page-granular access to the database file.
//!
//! [`DiskManager`] is the only code that reads or writes the database file.
//! It seals every page before writing and verifies every page after reading,
//! so corrupt pages never reach the layers above.

#[cfg(not(unix))]
compile_error!("oxenDB currently supports only Unix-like platforms");

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Mutex;

use crate::error::{Error, Result};
use crate::storage::file_header::FileHeader;
use crate::storage::page::{PAGE_SIZE, Page, PageId, PageType};

/// Reads, writes, and allocates pages in a single database file.
///
/// All methods take `&self` and are safe to call from multiple threads.
/// Reads and writes use positional I/O, so they do not contend on a shared
/// file cursor.
#[derive(Debug)]
pub struct DiskManager {
    file: File,
    /// Serializes allocation and guards the in-memory copy of the header.
    header: Mutex<FileHeader>,
}

impl DiskManager {
    /// Creates a new database file. Fails if `path` already exists.
    pub fn create(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let header = FileHeader::new();
        file.write_all_at(header.encode().as_bytes(), PageId::HEADER.file_offset())?;
        file.sync_all()?;
        sync_parent_dir(path)?;
        Ok(DiskManager {
            file,
            header: Mutex::new(header),
        })
    }

    /// Opens an existing database file and validates its header.
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < PAGE_SIZE as u64 {
            return Err(Error::UnsupportedFormat(format!(
                "file is {file_len} bytes, too small to be an oxenDB database"
            )));
        }
        let mut page = Page::zeroed();
        file.read_exact_at(page.as_bytes_mut(), PageId::HEADER.file_offset())?;
        let header = FileHeader::decode(&page)?;
        // A file longer than the header claims is expected after a crash
        // during allocation. A shorter file means pages were lost.
        let expected_len = header.page_count * PAGE_SIZE as u64;
        if file_len < expected_len {
            return Err(Error::corruption(format!(
                "file is {file_len} bytes but header lists {} pages ({expected_len} bytes)",
                header.page_count
            )));
        }
        Ok(DiskManager {
            file,
            header: Mutex::new(header),
        })
    }

    /// Number of pages in the file, including the header page.
    pub fn page_count(&self) -> u64 {
        self.lock_header().page_count
    }

    /// Reads and verifies a data page.
    pub fn read_page(&self, id: PageId) -> Result<Page> {
        self.check_data_page(id)?;
        let mut page = Page::zeroed();
        self.file
            .read_exact_at(page.as_bytes_mut(), id.file_offset())?;
        page.verify(id)?;
        Ok(page)
    }

    /// Seals and writes a data page. Does not fsync; see [`DiskManager::sync`].
    pub fn write_page(&self, id: PageId, page: &mut Page) -> Result<()> {
        self.check_data_page(id)?;
        if page.page_id() != id {
            return Err(Error::InvalidArgument(format!(
                "refusing to write {} to the location of {id}",
                page.page_id()
            )));
        }
        page.seal();
        self.file.write_all_at(page.as_bytes(), id.file_offset())?;
        Ok(())
    }

    /// Appends a new [`PageType::Free`] page to the file and returns its id.
    pub fn allocate_page(&self) -> Result<PageId> {
        let mut header = self.lock_header();
        let id = PageId(header.page_count);
        // Write the page before the header. If we crash in between, the file
        // is merely longer than the header says, which `open` tolerates.
        let mut page = Page::new(id, PageType::Free);
        page.seal();
        self.file.write_all_at(page.as_bytes(), id.file_offset())?;
        let updated = FileHeader {
            page_count: header.page_count + 1,
        };
        self.file
            .write_all_at(updated.encode().as_bytes(), PageId::HEADER.file_offset())?;
        *header = updated;
        Ok(id)
    }

    /// Flushes all written data and metadata to stable storage.
    pub fn sync(&self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn check_data_page(&self, id: PageId) -> Result<()> {
        if id == PageId::HEADER {
            return Err(Error::InvalidArgument(
                "page 0 is the file header and cannot be accessed as a data page".into(),
            ));
        }
        let page_count = self.page_count();
        if id.0 >= page_count {
            return Err(Error::InvalidArgument(format!(
                "{id} is out of bounds (file has {page_count} pages)"
            )));
        }
        Ok(())
    }

    fn lock_header(&self) -> std::sync::MutexGuard<'_, FileHeader> {
        // The header is only replaced after a successful write, so a panic
        // while holding the lock cannot leave it half-updated.
        self.header
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Makes a newly created file's directory entry durable.
pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn heap_page(id: PageId, fill: u8) -> Page {
        let mut page = Page::new(id, PageType::Heap);
        page.payload_mut().fill(fill);
        page
    }

    #[test]
    fn create_then_open_empty_database() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        DiskManager::create(&path).unwrap();
        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 1);
    }

    #[test]
    fn create_refuses_to_overwrite() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        DiskManager::create(&path).unwrap();
        assert!(matches!(DiskManager::create(&path), Err(Error::Io(_))));
    }

    #[test]
    fn pages_persist_across_reopen() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        let ids: Vec<PageId> = {
            let disk = DiskManager::create(&path).unwrap();
            let ids: Vec<PageId> = (0..3).map(|_| disk.allocate_page().unwrap()).collect();
            for (i, &id) in ids.iter().enumerate() {
                disk.write_page(id, &mut heap_page(id, i as u8 + 1))
                    .unwrap();
            }
            disk.sync().unwrap();
            ids
        };
        assert_eq!(ids, vec![PageId(1), PageId(2), PageId(3)]);

        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 4);
        for (i, &id) in ids.iter().enumerate() {
            let page = disk.read_page(id).unwrap();
            assert!(page.payload().iter().all(|&b| b == i as u8 + 1));
        }
    }

    #[test]
    fn newly_allocated_page_is_free() {
        let dir = TempDir::new();
        let disk = DiskManager::create(&dir.path().join("db.oxen")).unwrap();
        let id = disk.allocate_page().unwrap();
        let header = disk.read_page(id).unwrap().verify(id).unwrap();
        assert_eq!(header.page_type, PageType::Free);
    }

    #[test]
    fn rejects_header_and_out_of_bounds_pages() {
        let dir = TempDir::new();
        let disk = DiskManager::create(&dir.path().join("db.oxen")).unwrap();
        assert!(matches!(
            disk.read_page(PageId::HEADER),
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            disk.read_page(PageId(1)),
            Err(Error::InvalidArgument(_))
        ));
        let mut page = heap_page(PageId(5), 0);
        assert!(matches!(
            disk.write_page(PageId(5), &mut page),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn rejects_writing_page_to_wrong_location() {
        let dir = TempDir::new();
        let disk = DiskManager::create(&dir.path().join("db.oxen")).unwrap();
        let id = disk.allocate_page().unwrap();
        let _other = disk.allocate_page().unwrap();
        let mut page = heap_page(PageId(2), 0);
        assert!(matches!(
            disk.write_page(id, &mut page),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn detects_on_disk_corruption() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        let id = {
            let disk = DiskManager::create(&path).unwrap();
            let id = disk.allocate_page().unwrap();
            disk.write_page(id, &mut heap_page(id, 7)).unwrap();
            id
        };
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0xFF], id.file_offset() + 1000).unwrap();
        drop(file);

        let disk = DiskManager::open(&path).unwrap();
        assert!(matches!(disk.read_page(id), Err(Error::Corruption(_))));
    }

    #[test]
    fn detects_truncated_file() {
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        {
            let disk = DiskManager::create(&path).unwrap();
            disk.allocate_page().unwrap();
            disk.allocate_page().unwrap();
        }
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(2 * PAGE_SIZE as u64).unwrap();
        drop(file);
        assert!(matches!(
            DiskManager::open(&path),
            Err(Error::Corruption(_))
        ));
    }

    #[test]
    fn tolerates_extra_trailing_page() {
        // Simulates a crash after the new page was written but before the
        // header was updated.
        let dir = TempDir::new();
        let path = dir.path().join("db.oxen");
        DiskManager::create(&path).unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(2 * PAGE_SIZE as u64).unwrap();
        drop(file);

        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 1);
        assert_eq!(disk.allocate_page().unwrap(), PageId(1));
    }

    #[test]
    fn rejects_non_database_file() {
        let dir = TempDir::new();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"just some text").unwrap();
        assert!(matches!(
            DiskManager::open(&path),
            Err(Error::UnsupportedFormat(_))
        ));
    }
}
