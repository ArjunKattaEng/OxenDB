//! Read and write transactions over pages.
//!
//! This is the lowest transactional layer: it deals in whole pages. Tables,
//! rows, and indexes will be built on top of it.
//!
//! # Isolation
//!
//! - Only one [`WriteTxn`] exists at a time; [`Database::begin_write`]
//!   blocks until the current one finishes.
//! - A write transaction works on private copies of pages. Nobody else sees
//!   its changes until it commits.
//! - A [`ReadTxn`] sees a consistent snapshot: commits cannot publish
//!   changes while any read transaction is open, so a commit waits for
//!   open readers to finish.
//!
//! A thread must not wait on its own transactions: calling `begin_write`
//! while holding a `WriteTxn`, or committing while the same thread holds a
//! `ReadTxn`, deadlocks.

use std::collections::BTreeMap;
use std::sync::{MutexGuard, RwLockReadGuard};

use super::Database;
use crate::error::{Error, Result};
use crate::storage::file_header::FileHeader;
use crate::storage::page::{Page, PageId, PageType};
use crate::storage::wal::{TxnId, Wal, WalRecord};

/// A read-only view of the committed database.
#[derive(Debug)]
pub struct ReadTxn<'db> {
    db: &'db Database,
    _snapshot: RwLockReadGuard<'db, ()>,
}

impl<'db> ReadTxn<'db> {
    pub(super) fn new(db: &'db Database) -> Result<Self> {
        db.check_poisoned()?;
        let snapshot = db
            .publish
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Ok(ReadTxn {
            db,
            _snapshot: snapshot,
        })
    }

    /// Number of pages in the database, including the header page.
    pub fn page_count(&self) -> u64 {
        self.db.disk.page_count()
    }

    /// Calls `f` with the contents of page `id`.
    pub fn read_page<R>(&self, id: PageId, f: impl FnOnce(&Page) -> R) -> Result<R> {
        check_data_page(id, self.page_count())?;
        let handle = self.db.pool.fetch_page(id)?;
        let page = handle.read();
        Ok(f(&page))
    }
}

/// A transaction that can modify pages. Dropping it without calling
/// [`WriteTxn::commit`] rolls it back.
#[derive(Debug)]
pub struct WriteTxn<'db> {
    db: &'db Database,
    wal: MutexGuard<'db, Wal>,
    /// Private copies of every page this transaction touched.
    dirty: BTreeMap<PageId, Page>,
    /// Page count including pages allocated by this transaction.
    page_count: u64,
}

impl<'db> WriteTxn<'db> {
    pub(super) fn new(db: &'db Database) -> Result<Self> {
        db.check_poisoned()?;
        let wal = db.lock_wal();
        // Check again: the previous writer may have poisoned the database
        // while we waited for the lock.
        db.check_poisoned()?;
        let page_count = db.disk.page_count();
        Ok(WriteTxn {
            db,
            wal,
            dirty: BTreeMap::new(),
            page_count,
        })
    }

    /// Number of pages, including any allocated by this transaction.
    pub fn page_count(&self) -> u64 {
        self.page_count
    }

    /// Calls `f` with the contents of page `id` as this transaction sees
    /// it, including its own uncommitted changes.
    pub fn read_page<R>(&self, id: PageId, f: impl FnOnce(&Page) -> R) -> Result<R> {
        check_data_page(id, self.page_count)?;
        if let Some(page) = self.dirty.get(&id) {
            return Ok(f(page));
        }
        let handle = self.db.pool.fetch_page(id)?;
        let page = handle.read();
        Ok(f(&page))
    }

    /// Returns this transaction's private, writable copy of page `id`.
    pub fn page_mut(&mut self, id: PageId) -> Result<&mut Page> {
        check_data_page(id, self.page_count)?;
        if !self.dirty.contains_key(&id) {
            let copy = self.db.pool.fetch_page(id)?.read().clone();
            self.dirty.insert(id, copy);
        }
        Ok(self.dirty.get_mut(&id).expect("inserted above"))
    }

    /// Adds a new, empty page of the given type and returns its id.
    pub fn allocate_page(&mut self, page_type: PageType) -> PageId {
        let id = PageId(self.page_count);
        self.page_count += 1;
        self.dirty.insert(id, Page::new(id, page_type));
        id
    }

    /// Makes every change durable and visible, or none of them.
    ///
    /// When this returns `Ok`, the changes survive a crash. If it returns an
    /// error, the changes may or may not have been committed; reopening the
    /// database gives a definite answer.
    pub fn commit(mut self) -> Result<()> {
        if self.dirty.is_empty() {
            return Ok(());
        }
        // Validate everything before logging anything, so a bad page is
        // rejected without touching the WAL.
        for (&id, page) in self.dirty.iter_mut() {
            page.seal();
            page.verify(id).map_err(|err| {
                Error::InvalidArgument(format!("transaction left {id} malformed: {err}"))
            })?;
        }

        let txn = self.db.next_txn_id();
        let grew = self.page_count > self.db.disk.page_count();
        let mut images: Vec<WalRecord> = Vec::with_capacity(self.dirty.len() + 1);
        if grew {
            let header = FileHeader {
                page_count: self.page_count,
            };
            images.push(WalRecord::PageImage {
                txn,
                page_id: PageId::HEADER,
                page: header.encode(),
            });
        }
        let dirty = std::mem::take(&mut self.dirty);
        images.extend(
            dirty
                .into_iter()
                .map(|(page_id, page)| WalRecord::PageImage { txn, page_id, page }),
        );

        if let Err(err) = self.log(txn, &images) {
            self.db.poison();
            return Err(err);
        }
        // Durable from here on. Failing to publish would leave memory out of
        // step with the log, so poison instead; reopening replays the WAL.
        if let Err(err) = self.publish(images) {
            self.db.poison();
            return Err(err);
        }
        Ok(())
    }

    /// Discards every change. Equivalent to dropping the transaction.
    pub fn rollback(self) {}

    fn log(&mut self, txn: TxnId, images: &[WalRecord]) -> Result<()> {
        for record in images {
            self.wal.append(record)?;
        }
        self.wal.append(&WalRecord::Commit { txn })?;
        self.wal.flush()
    }

    fn publish(&self, images: Vec<WalRecord>) -> Result<()> {
        let _exclusive = self
            .db
            .publish
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.db.disk.set_page_count(self.page_count)?;
        for record in images {
            if let WalRecord::PageImage { page_id, page, .. } = record {
                if page_id != PageId::HEADER {
                    self.db.pool.install_page(page_id, page)?;
                }
            }
        }
        Ok(())
    }
}

fn check_data_page(id: PageId, page_count: u64) -> Result<()> {
    if id == PageId::HEADER || id.0 >= page_count {
        return Err(Error::InvalidArgument(format!(
            "{id} is not a data page (database has {page_count} pages)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Options;
    use crate::test_util::TempDir;
    use std::path::Path;

    fn open(path: &Path) -> Database {
        Database::open(
            path,
            Options {
                buffer_pool_pages: 16,
                ..Options::default()
            },
        )
        .unwrap()
    }

    fn read_u64(page: &Page) -> u64 {
        u64::from_le_bytes(page.payload()[..8].try_into().unwrap())
    }

    fn write_u64(page: &mut Page, value: u64) {
        page.payload_mut()[..8].copy_from_slice(&value.to_le_bytes());
    }

    /// Commits one new heap page holding `value` and returns its id.
    fn commit_new_page(db: &Database, value: u64) -> PageId {
        let mut txn = db.begin_write().unwrap();
        let id = txn.allocate_page(PageType::Heap);
        write_u64(txn.page_mut(id).unwrap(), value);
        txn.commit().unwrap();
        id
    }

    #[test]
    fn committed_changes_are_visible() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let id = commit_new_page(&db, 42);
        assert_eq!(db.page_count(), 2);
        let read = db.begin_read().unwrap();
        assert_eq!(read.read_page(id, read_u64).unwrap(), 42);
    }

    #[test]
    fn writer_sees_its_own_changes() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let id = commit_new_page(&db, 1);
        let mut txn = db.begin_write().unwrap();
        write_u64(txn.page_mut(id).unwrap(), 2);
        assert_eq!(txn.read_page(id, read_u64).unwrap(), 2);
    }

    #[test]
    fn uncommitted_changes_are_invisible_to_readers() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let id = commit_new_page(&db, 1);
        let mut txn = db.begin_write().unwrap();
        write_u64(txn.page_mut(id).unwrap(), 2);
        let new_id = txn.allocate_page(PageType::Heap);

        let read = db.begin_read().unwrap();
        assert_eq!(read.read_page(id, read_u64).unwrap(), 1);
        assert!(read.read_page(new_id, read_u64).is_err());
        assert_eq!(read.page_count(), 2);
    }

    #[test]
    fn rollback_discards_changes_and_allocations() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let id = commit_new_page(&db, 1);
        {
            let mut txn = db.begin_write().unwrap();
            write_u64(txn.page_mut(id).unwrap(), 2);
            txn.allocate_page(PageType::Heap);
            txn.rollback();
        }
        {
            let mut txn = db.begin_write().unwrap();
            write_u64(txn.page_mut(id).unwrap(), 3);
            // Dropped without commit.
        }
        let read = db.begin_read().unwrap();
        assert_eq!(read.read_page(id, read_u64).unwrap(), 1);
        assert_eq!(read.page_count(), 2);
        drop(read);
        // The rolled-back page id is reused.
        assert_eq!(commit_new_page(&db, 5), PageId(2));
    }

    #[test]
    fn commits_survive_reopen_without_checkpoint() {
        let dir = TempDir::new();
        let path = dir.path().join("t.oxen");
        let ids: Vec<PageId> = {
            let db = open(&path);
            (0..5).map(|i| commit_new_page(&db, 100 + i)).collect()
            // Dropped without close: the data file header was never updated.
        };
        let db = open(&path);
        assert_eq!(db.open_report().recovery.committed_txns, 5);
        assert_eq!(db.page_count(), 6);
        let read = db.begin_read().unwrap();
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(read.read_page(id, read_u64).unwrap(), 100 + i as u64);
        }
    }

    #[test]
    fn commits_survive_close_and_reopen() {
        let dir = TempDir::new();
        let path = dir.path().join("t.oxen");
        let id = {
            let db = open(&path);
            let id = commit_new_page(&db, 7);
            db.close().unwrap();
            id
        };
        let db = open(&path);
        assert_eq!(db.open_report().recovery.committed_txns, 0);
        assert_eq!(db.begin_read().unwrap().read_page(id, read_u64).unwrap(), 7);
    }

    #[test]
    fn many_pages_through_a_small_buffer_pool() {
        let dir = TempDir::new();
        let path = dir.path().join("t.oxen");
        {
            let db = open(&path); // 16 frames
            let mut txn = db.begin_write().unwrap();
            for i in 0..100 {
                let id = txn.allocate_page(PageType::Heap);
                write_u64(txn.page_mut(id).unwrap(), i);
            }
            txn.commit().unwrap();
            db.checkpoint().unwrap();
            let read = db.begin_read().unwrap();
            for i in 0..100 {
                assert_eq!(read.read_page(PageId(1 + i), read_u64).unwrap(), i);
            }
        }
        let db = open(&path);
        let read = db.begin_read().unwrap();
        for i in 0..100 {
            assert_eq!(read.read_page(PageId(1 + i), read_u64).unwrap(), i);
        }
    }

    #[test]
    fn malformed_page_is_rejected_before_logging() {
        let dir = TempDir::new();
        let path = dir.path().join("t.oxen");
        let db = open(&path);
        let id = commit_new_page(&db, 1);
        let wal_len_before = db.lock_wal().durable_len();

        let mut txn = db.begin_write().unwrap();
        // Corrupt the page type byte in the header.
        txn.page_mut(id).unwrap().as_bytes_mut()[4] = 0;
        assert!(matches!(txn.commit(), Err(Error::InvalidArgument(_))));
        assert_eq!(db.lock_wal().durable_len(), wal_len_before);
        // The database is still usable.
        assert_eq!(db.begin_read().unwrap().read_page(id, read_u64).unwrap(), 1);
    }

    #[test]
    fn header_page_is_not_accessible() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let mut txn = db.begin_write().unwrap();
        assert!(txn.page_mut(PageId::HEADER).is_err());
        assert!(txn.page_mut(PageId(1)).is_err());
    }

    #[test]
    fn poisoned_database_refuses_transactions() {
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        db.poison();
        assert!(matches!(db.begin_read(), Err(Error::Poisoned(_))));
        assert!(matches!(db.begin_write(), Err(Error::Poisoned(_))));
        assert!(matches!(db.checkpoint(), Err(Error::Poisoned(_))));
    }

    #[test]
    fn readers_never_see_a_partial_commit() {
        const TRANSFERS: u64 = 300;
        const TOTAL: u64 = 1_000;
        let dir = TempDir::new();
        let db = open(&dir.path().join("t.oxen"));
        let (a, b) = {
            let mut txn = db.begin_write().unwrap();
            let a = txn.allocate_page(PageType::Heap);
            let b = txn.allocate_page(PageType::Heap);
            write_u64(txn.page_mut(a).unwrap(), TOTAL);
            write_u64(txn.page_mut(b).unwrap(), 0);
            txn.commit().unwrap();
            (a, b)
        };

        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let mut checks = 0u64;
                    while !done.load(std::sync::atomic::Ordering::Acquire) || checks == 0 {
                        let read = db.begin_read().unwrap();
                        let sum = read.read_page(a, read_u64).unwrap()
                            + read.read_page(b, read_u64).unwrap();
                        assert_eq!(sum, TOTAL);
                        checks += 1;
                    }
                });
            }
            for i in 0..TRANSFERS {
                let mut txn = db.begin_write().unwrap();
                let amount = i % 7 + 1;
                let from = if i % 2 == 0 { a } else { b };
                let to = if from == a { b } else { a };
                let from_value = txn.read_page(from, read_u64).unwrap();
                if from_value >= amount {
                    write_u64(txn.page_mut(from).unwrap(), from_value - amount);
                    let to_value = txn.read_page(to, read_u64).unwrap();
                    write_u64(txn.page_mut(to).unwrap(), to_value + amount);
                }
                txn.commit().unwrap();
            }
            done.store(true, std::sync::atomic::Ordering::Release);
        });
    }
}
