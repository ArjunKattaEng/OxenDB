//! The database handle: opening, recovery, and closing.
//!
//! A database is a data file plus a write-ahead log next to it named
//! `<data file>-wal`. Opening a database always runs recovery first, so a
//! successfully opened [`Database`] reflects every committed transaction.

mod txn;

pub use txn::{ReadTxn, WriteTxn};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use crate::error::{Error, Result};
use crate::storage::buffer_pool::BufferPool;
use crate::storage::disk::DiskManager;
use crate::storage::recovery::{self, RecoveryStats};
use crate::storage::wal::log::DiscardedTail;
use crate::storage::wal::{TxnId, Wal};

/// Settings for opening a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Number of pages the buffer pool caches in memory.
    pub buffer_pool_pages: usize,
    /// Create the database if it does not exist.
    pub create_if_missing: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            // 16 MiB of 4 KiB pages.
            buffer_pool_pages: 4096,
            create_if_missing: true,
        }
    }
}

/// What happened while opening a database.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenReport {
    /// True if the database was newly created.
    pub created: bool,
    /// Results of WAL replay.
    pub recovery: RecoveryStats,
    /// Bytes discarded from the end of the WAL, typically the remains of a
    /// write interrupted by a crash.
    pub discarded_wal_tail: Option<DiscardedTail>,
}

/// An open oxenDB database.
#[derive(Debug)]
pub struct Database {
    disk: Arc<DiskManager>,
    pool: BufferPool,
    /// Held for the whole of a write transaction or checkpoint, so at most
    /// one runs at a time. Also guards the log.
    wal: Mutex<Wal>,
    /// Read transactions hold this shared; publishing a commit holds it
    /// exclusively, so readers never see a half-published commit.
    publish: RwLock<()>,
    next_txn: AtomicU64,
    /// Set when a failure left in-memory state out of step with the files.
    /// Every later transaction fails until the database is reopened.
    poisoned: AtomicBool,
    report: OpenReport,
}

impl Database {
    /// Opens the database at `path`, creating it if allowed by `options`.
    pub fn open(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = path.as_ref();
        let wal_path = wal_path_for(path)?;
        if path.exists() {
            Self::open_existing(path, &wal_path, &options)
        } else if !options.create_if_missing {
            Err(Error::InvalidArgument(format!(
                "database {} does not exist",
                path.display()
            )))
        } else if wal_path.exists() {
            // Refuse rather than silently starting over: the WAL may hold
            // committed data whose data file was deleted or moved.
            Err(Error::InvalidArgument(format!(
                "{} exists but its database file {} does not",
                wal_path.display(),
                path.display()
            )))
        } else {
            Self::create(path, &wal_path, &options)
        }
    }

    fn create(path: &Path, wal_path: &Path, options: &Options) -> Result<Self> {
        let disk = Arc::new(DiskManager::create(path)?);
        let wal = Wal::create(wal_path)?;
        let report = OpenReport {
            created: true,
            ..OpenReport::default()
        };
        Self::assemble(disk, wal, options, report)
    }

    fn open_existing(path: &Path, wal_path: &Path, options: &Options) -> Result<Self> {
        // A missing WAL means there is nothing to recover, as in SQLite.
        if !wal_path.exists() {
            let disk = Arc::new(DiskManager::open(path)?);
            let wal = Wal::create(wal_path)?;
            return Self::assemble(disk, wal, options, OpenReport::default());
        }
        let opened = Wal::open(wal_path)?;
        let recovery = recovery::replay(path, &opened.records)?;
        let disk = Arc::new(DiskManager::open(path)?);
        let mut wal = opened.wal;
        // Replay fsynced the data file, so the log is no longer needed.
        wal.reset()?;
        let report = OpenReport {
            created: false,
            recovery,
            discarded_wal_tail: opened.discarded,
        };
        Self::assemble(disk, wal, options, report)
    }

    fn assemble(
        disk: Arc<DiskManager>,
        wal: Wal,
        options: &Options,
        report: OpenReport,
    ) -> Result<Self> {
        let pool = BufferPool::new(Arc::clone(&disk), options.buffer_pool_pages)?;
        Ok(Database {
            disk,
            pool,
            wal: Mutex::new(wal),
            publish: RwLock::new(()),
            next_txn: AtomicU64::new(1),
            poisoned: AtomicBool::new(false),
            report,
        })
    }

    /// What happened while this database was opened.
    pub fn open_report(&self) -> &OpenReport {
        &self.report
    }

    /// Number of pages in the database, including the header page.
    pub fn page_count(&self) -> u64 {
        self.disk.page_count()
    }

    /// Starts a read-only transaction.
    pub fn begin_read(&self) -> Result<ReadTxn<'_>> {
        ReadTxn::new(self)
    }

    /// Starts a write transaction, waiting for any active one to finish.
    pub fn begin_write(&self) -> Result<WriteTxn<'_>> {
        WriteTxn::new(self)
    }

    /// Writes every committed change to the data file and empties the WAL.
    ///
    /// Durability never depends on checkpoints; they only keep the WAL
    /// small and make the next open faster. Blocks until any active write
    /// transaction finishes.
    pub fn checkpoint(&self) -> Result<()> {
        self.check_poisoned()?;
        let mut wal = self.lock_wal();
        self.checkpoint_locked(&mut wal)
    }

    /// Checkpoints and closes the database, reporting any error. Dropping a
    /// `Database` instead is also safe; the next open recovers from the WAL.
    pub fn close(self) -> Result<()> {
        self.checkpoint()
    }

    fn checkpoint_locked(&self, wal: &mut Wal) -> Result<()> {
        // Order matters: pages and header must be durable in the data file
        // before the WAL copies of them are discarded. A torn header write
        // here is repaired from the WAL, which is still intact until reset.
        self.pool.flush_all()?;
        self.disk.write_file_header()?;
        self.disk.sync()?;
        wal.reset()
    }

    fn check_poisoned(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(Error::Poisoned(
                "an earlier failure left the database in an unknown state; reopen it to recover"
                    .into(),
            ));
        }
        Ok(())
    }

    fn poison(&self) {
        self.poisoned.store(true, Ordering::Release);
    }

    fn next_txn_id(&self) -> TxnId {
        TxnId(self.next_txn.fetch_add(1, Ordering::Relaxed))
    }

    fn lock_wal(&self) -> MutexGuard<'_, Wal> {
        // The Wal poisons itself on I/O failure, so a panic while holding
        // the lock leaves it in a state it can still report on.
        self.wal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Returns `<path>-wal`.
fn wal_path_for(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| Error::InvalidArgument(format!("{} is not a file path", path.display())))?;
    let mut wal_name = file_name.to_os_string();
    wal_name.push("-wal");
    Ok(path.with_file_name(wal_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn wal_path_is_next_to_database() {
        assert_eq!(
            wal_path_for(Path::new("/a/b/app.oxen")).unwrap(),
            Path::new("/a/b/app.oxen-wal")
        );
        assert!(wal_path_for(Path::new("/")).is_err());
    }

    #[test]
    fn creates_then_reopens() {
        let dir = TempDir::new();
        let path = dir.path().join("app.oxen");
        {
            let db = Database::open(&path, Options::default()).unwrap();
            assert!(db.open_report().created);
            assert_eq!(db.page_count(), 1);
        }
        assert!(path.exists());
        assert!(wal_path_for(&path).unwrap().exists());
        let db = Database::open(&path, Options::default()).unwrap();
        assert!(!db.open_report().created);
    }

    #[test]
    fn refuses_to_create_when_asked_not_to() {
        let dir = TempDir::new();
        let options = Options {
            create_if_missing: false,
            ..Options::default()
        };
        let err = Database::open(dir.path().join("app.oxen"), options).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn missing_wal_is_recreated() {
        let dir = TempDir::new();
        let path = dir.path().join("app.oxen");
        drop(Database::open(&path, Options::default()).unwrap());
        std::fs::remove_file(wal_path_for(&path).unwrap()).unwrap();
        Database::open(&path, Options::default()).unwrap();
        assert!(wal_path_for(&path).unwrap().exists());
    }

    #[test]
    fn orphaned_wal_is_refused() {
        let dir = TempDir::new();
        let path = dir.path().join("app.oxen");
        drop(Database::open(&path, Options::default()).unwrap());
        std::fs::remove_file(&path).unwrap();
        let err = Database::open(&path, Options::default()).unwrap_err();
        assert!(err.to_string().contains("does not"), "{err}");
    }

    #[test]
    fn checkpoint_and_close_leave_an_empty_wal() {
        let dir = TempDir::new();
        let path = dir.path().join("app.oxen");
        let db = Database::open(&path, Options::default()).unwrap();
        db.checkpoint().unwrap();
        db.close().unwrap();
        let wal_len = std::fs::metadata(wal_path_for(&path).unwrap())
            .unwrap()
            .len();
        assert_eq!(wal_len, crate::storage::wal::log::WAL_HEADER_SIZE as u64);
    }

    #[test]
    fn rejects_zero_sized_buffer_pool() {
        let dir = TempDir::new();
        let options = Options {
            buffer_pool_pages: 0,
            ..Options::default()
        };
        assert!(Database::open(dir.path().join("app.oxen"), options).is_err());
    }
}
