//! Crash tests that kill a real writer process with SIGKILL.
//!
//! The test binary re-executes itself: `child_writer` does nothing unless
//! `OXENDB_CRASH_DB` is set, in which case it commits in a loop and prints
//! `ack <value>` after each commit returns. The parent kills it at a random
//! point and checks that recovery kept every acknowledged commit and applied
//! no commit partially.
//!
//! SIGKILL tests process crashes, not power loss: data the child wrote is
//! still in the OS page cache. It also cannot tear a WAL batch, since each
//! batch is one `pwrite` call, which SIGKILL does not interrupt partway.
//! What it does catch: lost acknowledged commits, and bugs in the
//! interaction between commits, eviction write-back, and checkpoints (the
//! small buffer pool and checkpoint limit make both happen constantly).
//! Torn writes and power loss are covered by `torn_wal.rs`.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use common::*;
use oxendb::storage::page::PageId;
use oxendb::{Database, Options};

const ENV_DB: &str = "OXENDB_CRASH_DB";
const COUNTERS: u64 = 24;
const ROUNDS: u64 = 12;

/// Small enough that commits constantly evict pages and checkpoint.
fn stressful_options() -> Options {
    Options {
        buffer_pool_pages: 8,
        auto_checkpoint_bytes: Some(128 * 1024),
        ..Options::default()
    }
}

fn counter_ids() -> Vec<PageId> {
    (1..=COUNTERS).map(PageId).collect()
}

#[test]
fn child_writer() {
    let Some(path) = std::env::var_os(ENV_DB) else {
        return; // Not running as a child.
    };
    let db = Database::open(&path, stressful_options()).unwrap();
    let ids = counter_ids();
    let mut value = read_uniform(&db, &ids);
    loop {
        value += 1;
        set_all(&db, &ids, value);
        println!("ack {value}");
    }
}

#[test]
fn killed_writer_loses_nothing_acknowledged() {
    let dir = TempDir::new("process-kill");
    let path = dir.path().join("t.oxen");
    {
        let db = Database::open(&path, stressful_options()).unwrap();
        assert_eq!(create_counters(&db, COUNTERS), counter_ids());
        db.close().unwrap();
    }

    let exe = std::env::current_exe().unwrap();
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    let mut last_ack = 0;
    for round in 0..ROUNDS {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let kill_after = 1 + seed % 60;

        let mut child = Command::new(&exe)
            .args(["--exact", "child_writer", "--nocapture", "--test-threads=1"])
            .env(ENV_DB, &path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut acks_this_round = 0;
        for line in lines.by_ref() {
            if let Some(value) = line.unwrap().strip_prefix("ack ") {
                last_ack = value.parse().unwrap();
                acks_this_round += 1;
                if acks_this_round == kill_after {
                    break;
                }
            }
        }
        child.kill().unwrap();
        // Acks printed before the kill landed still count.
        for line in lines {
            if let Some(value) = line.unwrap().strip_prefix("ack ") {
                last_ack = value.parse().unwrap();
            }
        }
        child.wait().unwrap();

        let db = Database::open(&path, stressful_options()).unwrap();
        let recovered = read_uniform(&db, &counter_ids());
        assert!(
            recovered == last_ack || recovered == last_ack + 1,
            "round {round}: last acknowledged {last_ack}, recovered {recovered} (seed {seed})"
        );
        last_ack = recovered;
    }
}
