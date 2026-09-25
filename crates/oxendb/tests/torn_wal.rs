//! Simulated crashes during a WAL write.
//!
//! A crash while a commit's records are being written can leave any prefix
//! of them on disk, and because the OS may persist blocks out of order,
//! even a later part without an earlier one. None of these partial states
//! was acknowledged as committed, so recovery must discard all of them.

mod common;

use std::fs;

use common::*;
use oxendb::Database;

const COUNTERS: u64 = 4;
const COMMITTED: u64 = 5;

/// Files captured after `COMMITTED` commits, plus the bytes the next commit
/// appends to the WAL.
struct Scenario {
    _dir: TempDir,
    db_path: std::path::PathBuf,
    data_before: Vec<u8>,
    wal_before: Vec<u8>,
    next_commit: Vec<u8>,
    ids: Vec<oxendb::storage::page::PageId>,
}

fn scenario() -> Scenario {
    let dir = TempDir::new("torn-wal");
    let db_path = dir.path().join("t.oxen");
    let db = Database::open(&db_path, quiet_options()).unwrap();
    let ids = create_counters(&db, COUNTERS);
    for value in 1..=COMMITTED {
        set_all(&db, &ids, value);
    }
    let data_before = fs::read(&db_path).unwrap();
    let wal_before = fs::read(wal_path(&db_path)).unwrap();
    set_all(&db, &ids, COMMITTED + 1);
    let wal_after = fs::read(wal_path(&db_path)).unwrap();
    drop(db);

    assert_eq!(
        fs::read(&db_path).unwrap(),
        data_before,
        "data file changed without checkpoint"
    );
    assert_eq!(&wal_after[..wal_before.len()], &wal_before[..]);
    let next_commit = wal_after[wal_before.len()..].to_vec();
    Scenario {
        _dir: dir,
        db_path,
        data_before,
        wal_before,
        next_commit,
        ids,
    }
}

/// Restores the pre-commit data file, installs `wal`, recovers, and returns
/// the counters' common value.
fn recover_with_wal(s: &Scenario, wal: &[u8]) -> u64 {
    fs::write(&s.db_path, &s.data_before).unwrap();
    fs::write(wal_path(&s.db_path), wal).unwrap();
    let db = Database::open(&s.db_path, quiet_options()).unwrap();
    read_uniform(&db, &s.ids)
}

#[test]
fn every_prefix_of_an_unacknowledged_commit_is_discarded() {
    let s = scenario();
    let len = s.next_commit.len();
    // Every cut near record boundaries and headers is covered by the sampled
    // step plus the first and last 64 bytes.
    let cuts = (0..64)
        .chain((64..len).step_by(61))
        .chain(len.saturating_sub(64)..len);
    for cut in cuts {
        let mut wal = s.wal_before.clone();
        wal.extend_from_slice(&s.next_commit[..cut]);
        assert_eq!(
            recover_with_wal(&s, &wal),
            COMMITTED,
            "cut at {cut} of {len}"
        );
    }
}

#[test]
fn complete_commit_is_recovered() {
    let s = scenario();
    let mut wal = s.wal_before.clone();
    wal.extend_from_slice(&s.next_commit);
    assert_eq!(recover_with_wal(&s, &wal), COMMITTED + 1);
}

#[test]
fn commit_record_without_earlier_images_is_discarded() {
    // Out-of-order persistence: the tail of the batch (including the commit
    // record) reached disk, but a 4 KiB block in the middle did not.
    let s = scenario();
    let len = s.next_commit.len();
    for hole_start in (0..len - 4096).step_by(1024) {
        let mut batch = s.next_commit.clone();
        batch[hole_start..hole_start + 4096].fill(0);
        let mut wal = s.wal_before.clone();
        wal.extend_from_slice(&batch);
        assert_eq!(
            recover_with_wal(&s, &wal),
            COMMITTED,
            "hole at {hole_start}"
        );
    }
}

#[test]
fn recovery_after_recovery_is_stable() {
    // A crash during recovery itself must be survivable: recovering twice
    // from the same files gives the same answer.
    let s = scenario();
    let mut wal = s.wal_before.clone();
    wal.extend_from_slice(&s.next_commit);
    fs::write(&s.db_path, &s.data_before).unwrap();
    fs::write(wal_path(&s.db_path), &wal).unwrap();
    let data = s.db_path.clone();
    let recovered_data = {
        let db = Database::open(&data, quiet_options()).unwrap();
        assert_eq!(read_uniform(&db, &s.ids), COMMITTED + 1);
        drop(db);
        fs::read(&data).unwrap()
    };
    // Put the full WAL back, as if the crash happened before it was reset.
    fs::write(wal_path(&data), &wal).unwrap();
    let db = Database::open(&data, quiet_options()).unwrap();
    assert_eq!(read_uniform(&db, &s.ids), COMMITTED + 1);
    drop(db);
    assert_eq!(fs::read(&data).unwrap(), recovered_data);
}
