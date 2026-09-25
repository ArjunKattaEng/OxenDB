//! Crash recovery: replaying committed page images from the WAL.
//!
//! Recovery runs before the data file is opened normally, because the WAL
//! may hold the only intact copy of the file header. See
//! `docs/adr/0002-wal-and-recovery.md`.

use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::os::unix::fs::FileExt;
use std::path::Path;

use crate::error::{Error, Result};
use crate::storage::file_header::FileHeader;
use crate::storage::page::{Page, PageId};
use crate::storage::wal::record::DecodedRecord;
use crate::storage::wal::{TxnId, WalRecord};

/// What recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryStats {
    /// Transactions whose changes were replayed.
    pub committed_txns: usize,
    /// Transactions in the log without a commit record; their changes were
    /// ignored.
    pub incomplete_txns: usize,
    /// Distinct pages written to the data file.
    pub pages_restored: usize,
}

/// Writes the latest committed image of every page in `records` to the data
/// file at `data_path`, then fsyncs it.
///
/// Replaying the same records twice has the same effect as once.
pub fn replay(data_path: &Path, records: &[DecodedRecord]) -> Result<RecoveryStats> {
    let committed: HashSet<TxnId> = records
        .iter()
        .filter_map(|r| match r.record {
            WalRecord::Commit { txn } => Some(txn),
            WalRecord::PageImage { .. } => None,
        })
        .collect();
    let all_txns: HashSet<TxnId> = records.iter().map(|r| r.record.txn()).collect();

    // Later images of a page supersede earlier ones. A BTreeMap writes pages
    // in file order.
    let mut latest: BTreeMap<PageId, &Page> = BTreeMap::new();
    for decoded in records {
        if let WalRecord::PageImage { txn, page_id, page } = &decoded.record {
            if committed.contains(txn) {
                validate_image(*page_id, page, decoded.seq)?;
                latest.insert(*page_id, page);
            }
        }
    }

    if !latest.is_empty() {
        let file = OpenOptions::new().write(true).open(data_path)?;
        for (page_id, page) in &latest {
            file.write_all_at(page.as_bytes(), page_id.file_offset())?;
        }
        file.sync_all()?;
    }

    Ok(RecoveryStats {
        committed_txns: committed.len(),
        incomplete_txns: all_txns.len() - committed.len(),
        pages_restored: latest.len(),
    })
}

/// Checks that a logged image is a valid page. The WAL record's own
/// checksum already passed, so a failure here means a bug wrote a bad
/// image, not disk corruption. Refusing to replay it keeps the bug from
/// spreading into the data file.
fn validate_image(page_id: PageId, page: &Page, seq: u64) -> Result<()> {
    let result = if page_id == PageId::HEADER {
        FileHeader::decode(page).map(|_| ())
    } else {
        page.verify(page_id).map(|_| ())
    };
    result.map_err(|err| {
        Error::corruption(format!(
            "WAL record {seq} holds an invalid image of {page_id}: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::disk::DiskManager;
    use crate::storage::page::{PAGE_SIZE, PageType};
    use crate::test_util::TempDir;

    fn sealed(id: u64, fill: u8) -> Page {
        let mut page = Page::new(PageId(id), PageType::Heap);
        page.payload_mut().fill(fill);
        page.seal();
        page
    }

    fn image(txn: u64, page: Page) -> WalRecord {
        WalRecord::PageImage {
            txn: TxnId(txn),
            page_id: page.page_id(),
            page,
        }
    }

    fn header_image(txn: u64, page_count: u64) -> WalRecord {
        WalRecord::PageImage {
            txn: TxnId(txn),
            page_id: PageId::HEADER,
            page: FileHeader { page_count }.encode(),
        }
    }

    fn commit(txn: u64) -> WalRecord {
        WalRecord::Commit { txn: TxnId(txn) }
    }

    fn decoded(records: Vec<WalRecord>) -> Vec<DecodedRecord> {
        records
            .into_iter()
            .enumerate()
            .map(|(seq, record)| DecodedRecord {
                seq: seq as u64,
                record,
                len: 0,
            })
            .collect()
    }

    fn new_db(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("db.oxen");
        DiskManager::create(&path).unwrap();
        path
    }

    #[test]
    fn replays_committed_and_ignores_incomplete() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let records = decoded(vec![
            header_image(1, 3),
            image(1, sealed(1, 10)),
            image(1, sealed(2, 20)),
            commit(1),
            image(2, sealed(1, 99)),
        ]);
        let stats = replay(&path, &records).unwrap();
        assert_eq!(
            stats,
            RecoveryStats {
                committed_txns: 1,
                incomplete_txns: 1,
                pages_restored: 3
            }
        );

        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.page_count(), 3);
        assert_eq!(disk.read_page(PageId(1)).unwrap().payload()[0], 10);
        assert_eq!(disk.read_page(PageId(2)).unwrap().payload()[0], 20);
    }

    #[test]
    fn later_commits_win() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let records = decoded(vec![
            header_image(1, 2),
            image(1, sealed(1, 1)),
            commit(1),
            image(2, sealed(1, 2)),
            commit(2),
        ]);
        replay(&path, &records).unwrap();
        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.read_page(PageId(1)).unwrap().payload()[0], 2);
    }

    #[test]
    fn repairs_torn_data_page_and_header() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let records = decoded(vec![header_image(1, 2), image(1, sealed(1, 5)), commit(1)]);
        replay(&path, &records).unwrap();

        // Tear both the header and the data page.
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[0xAA; 512], 0).unwrap();
        file.write_all_at(&[0xBB; 512], PAGE_SIZE as u64 + 2048)
            .unwrap();
        drop(file);
        assert!(DiskManager::open(&path).is_err());

        replay(&path, &records).unwrap();
        let disk = DiskManager::open(&path).unwrap();
        assert_eq!(disk.read_page(PageId(1)).unwrap().payload()[0], 5);
    }

    #[test]
    fn replay_is_idempotent() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let records = decoded(vec![header_image(1, 2), image(1, sealed(1, 5)), commit(1)]);
        replay(&path, &records).unwrap();
        let first = std::fs::read(&path).unwrap();
        replay(&path, &records).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
    }

    #[test]
    fn empty_log_touches_nothing() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let before = std::fs::read(&path).unwrap();
        assert_eq!(replay(&path, &[]).unwrap(), RecoveryStats::default());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn refuses_invalid_committed_image() {
        let dir = TempDir::new();
        let path = new_db(&dir);
        let mut bad = sealed(1, 5);
        bad.payload_mut()[0] = 6; // modified after sealing
        let records = decoded(vec![image(1, bad), commit(1)]);
        assert!(matches!(replay(&path, &records), Err(Error::Corruption(_))));
    }
}
