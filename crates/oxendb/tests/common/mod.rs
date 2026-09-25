//! Helpers shared by integration tests.

#![allow(dead_code)] // Each test binary uses a different subset.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use oxendb::storage::page::{Page, PageId, PageType};
use oxendb::{Database, Options};

/// A uniquely named directory under the system temp dir, removed on drop.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = format!(
            "oxendb-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub fn wal_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.file_name().unwrap().to_os_string();
    name.push("-wal");
    db_path.with_file_name(name)
}

pub fn read_u64(page: &Page) -> u64 {
    u64::from_le_bytes(page.payload()[..8].try_into().unwrap())
}

pub fn write_u64(page: &mut Page, value: u64) {
    page.payload_mut()[..8].copy_from_slice(&value.to_le_bytes());
}

/// Options that keep every page in memory and never checkpoint on their own,
/// so the data file only changes when a test says so.
pub fn quiet_options() -> Options {
    Options {
        buffer_pool_pages: 1024,
        auto_checkpoint_bytes: None,
        ..Options::default()
    }
}

/// Creates `count` heap pages holding 0 and returns their ids.
pub fn create_counters(db: &Database, count: u64) -> Vec<PageId> {
    let mut txn = db.begin_write().unwrap();
    let ids: Vec<PageId> = (0..count)
        .map(|_| txn.allocate_page(PageType::Heap))
        .collect();
    for &id in &ids {
        write_u64(txn.page_mut(id).unwrap(), 0);
    }
    txn.commit().unwrap();
    ids
}

/// Sets every counter page to `value` in one transaction.
pub fn set_all(db: &Database, ids: &[PageId], value: u64) {
    let mut txn = db.begin_write().unwrap();
    for &id in ids {
        write_u64(txn.page_mut(id).unwrap(), value);
    }
    txn.commit().unwrap();
}

/// Reads every counter and asserts they all hold the same value, which
/// shows no transaction was partially applied. Returns that value.
pub fn read_uniform(db: &Database, ids: &[PageId]) -> u64 {
    let read = db.begin_read().unwrap();
    let values: Vec<u64> = ids
        .iter()
        .map(|&id| read.read_page(id, read_u64).unwrap())
        .collect();
    assert!(
        values.iter().all(|&v| v == values[0]),
        "partially applied transaction: {values:?}"
    );
    values[0]
}
