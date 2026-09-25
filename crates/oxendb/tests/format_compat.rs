//! Checks that this build still reads database files written earlier.
//!
//! `tests/fixtures/format-v1.oxen` (and its `-wal`) were written by
//! `write_fixture`. If this test fails, the on-disk format changed. That is
//! allowed before 1.0 only together with a `FORMAT_VERSION` bump, a CHANGELOG
//! entry, and a regenerated fixture:
//!
//! ```sh
//! OXENDB_WRITE_FIXTURE=1 cargo test -p oxendb --test format_compat write_fixture
//! ```
//!
//! After 1.0, old fixtures must keep passing forever; add new ones instead.

mod common;

use std::path::PathBuf;

use common::*;
use oxendb::storage::page::{PageId, PageType};
use oxendb::{Database, Options};

const PAGES: u64 = 3;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-v1.oxen")
}

fn expected_payload(id: u64) -> Vec<u8> {
    (0..64).map(|i| (id * 31 + i) as u8).collect()
}

#[test]
fn write_fixture() {
    if std::env::var_os("OXENDB_WRITE_FIXTURE").is_none() {
        return;
    }
    let path = fixture_path();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(wal_path(&path));
    let db = Database::open(&path, Options::default()).unwrap();
    let mut txn = db.begin_write().unwrap();
    for id in 1..=PAGES {
        assert_eq!(txn.allocate_page(PageType::Heap), PageId(id));
        txn.page_mut(PageId(id)).unwrap().payload_mut()[..64]
            .copy_from_slice(&expected_payload(id));
    }
    txn.commit().unwrap();
    db.close().unwrap();
}

#[test]
fn reads_format_v1_fixture() {
    // Work on a copy: opening a database may write to it.
    let dir = TempDir::new("format-compat");
    let path = dir.path().join("copy.oxen");
    std::fs::copy(fixture_path(), &path).unwrap();
    std::fs::copy(wal_path(&fixture_path()), wal_path(&path)).unwrap();

    let options = Options {
        create_if_missing: false,
        ..Options::default()
    };
    let db = Database::open(&path, options).unwrap();
    assert_eq!(db.page_count(), 1 + PAGES);
    let read = db.begin_read().unwrap();
    for id in 1..=PAGES {
        let payload = read
            .read_page(PageId(id), |page| page.payload()[..64].to_vec())
            .unwrap();
        assert_eq!(payload, expected_payload(id), "page {id}");
    }
}
