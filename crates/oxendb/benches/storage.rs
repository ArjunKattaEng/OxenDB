//! Storage-layer benchmarks.
//!
//! Run with `cargo bench -p oxendb --bench storage`. Results depend heavily
//! on the disk and on how the OS implements fsync, so always report them
//! with the machine they came from (see `docs/benchmarks.md`).
//!
//! This is a plain timing harness with no dependencies. Each workload
//! reports throughput and, where each operation is timed individually,
//! p50/p95/p99 latency.

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use oxendb::storage::page::{Page, PageId, PageType};
use oxendb::{Database, Options};

fn main() {
    // `cargo bench` passes `--bench`; ignore arguments.
    let dir = BenchDir::new();
    println!("oxenDB storage benchmarks");
    println!("temp dir: {}", dir.0.display());
    println!();
    commit_pages(&dir, 1, 1_000);
    commit_pages(&dir, 16, 300);
    read_cached(&dir);
    read_uncached(&dir);
    checkpoint(&dir);
    recovery(&dir);
}

struct BenchDir(PathBuf);

impl BenchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("oxendb-bench-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        BenchDir(path)
    }

    /// A fresh database path for one workload.
    fn db(&self, name: &str) -> PathBuf {
        self.0.join(format!("{name}.oxen"))
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn options(buffer_pool_pages: usize) -> Options {
    Options {
        buffer_pool_pages,
        auto_checkpoint_bytes: None,
        ..Options::default()
    }
}

fn write_u64(page: &mut Page, value: u64) {
    page.payload_mut()[..8].copy_from_slice(&value.to_le_bytes());
}

fn read_u64(page: &Page) -> u64 {
    u64::from_le_bytes(page.payload()[..8].try_into().unwrap())
}

/// Creates a database with `pages` heap pages, checkpointed.
fn populate(path: &Path, pages: u64, buffer_pool_pages: usize) -> Database {
    let db = Database::open(path, options(buffer_pool_pages)).unwrap();
    let mut txn = db.begin_write().unwrap();
    for i in 0..pages {
        let id = txn.allocate_page(PageType::Heap);
        write_u64(txn.page_mut(id).unwrap(), i);
    }
    txn.commit().unwrap();
    db.checkpoint().unwrap();
    db
}

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

fn report(name: &str, ops: u64, total: Duration, mut latencies: Vec<Duration>) {
    let per_sec = ops as f64 / total.as_secs_f64();
    print!("{name:<32} {ops:>9} ops  {per_sec:>12.0} ops/s");
    if !latencies.is_empty() {
        latencies.sort_unstable();
        let pct = |p: f64| latencies[((latencies.len() - 1) as f64 * p) as usize];
        print!(
            "  p50 {:>9.1?}  p95 {:>9.1?}  p99 {:>9.1?}",
            pct(0.50),
            pct(0.95),
            pct(0.99)
        );
    }
    println!();
}

/// Reports a workload that runs once, so percentiles would be meaningless.
fn report_once(name: &str, units: u64, unit_name: &str, elapsed: Duration) {
    let per_sec = units as f64 / elapsed.as_secs_f64();
    println!("{name:<32} {elapsed:>9.1?} total  {units} {unit_name} ({per_sec:.0}/s)");
}

/// Durable commits, each rewriting `pages_per_txn` existing pages.
fn commit_pages(dir: &BenchDir, pages_per_txn: u64, txns: u64) {
    let db = populate(&dir.db(&format!("commit{pages_per_txn}")), 1_000, 4_096);
    let mut rng = Rng(0x1234_5678);
    let mut latencies = Vec::with_capacity(txns as usize);
    let start = Instant::now();
    for n in 0..txns {
        let op = Instant::now();
        let mut txn = db.begin_write().unwrap();
        for _ in 0..pages_per_txn {
            let id = PageId(1 + rng.below(1_000));
            write_u64(txn.page_mut(id).unwrap(), n);
        }
        txn.commit().unwrap();
        latencies.push(op.elapsed());
    }
    report(
        &format!("commit ({pages_per_txn} page/txn, fsync)"),
        txns,
        start.elapsed(),
        latencies,
    );
}

/// Random page reads with every page in the buffer pool.
fn read_cached(dir: &BenchDir) {
    const PAGES: u64 = 1_000;
    const READS: u64 = 2_000_000;
    let db = populate(&dir.db("read_cached"), PAGES, 4_096);
    let read = db.begin_read().unwrap();
    let mut rng = Rng(0x9ABC_DEF0);
    // Warm the cache.
    for id in 1..=PAGES {
        black_box(read.read_page(PageId(id), read_u64).unwrap());
    }
    // Operations are too short to time one by one; report throughput only.
    let start = Instant::now();
    for _ in 0..READS {
        let id = PageId(1 + rng.below(PAGES));
        black_box(read.read_page(id, read_u64).unwrap());
    }
    report("read (cached)", READS, start.elapsed(), Vec::new());
}

/// Random page reads from a database 16x larger than the buffer pool. Reads
/// mostly hit the OS page cache, not the device.
fn read_uncached(dir: &BenchDir) {
    const PAGES: u64 = 16_384;
    const READS: u64 = 100_000;
    let db = populate(&dir.db("read_uncached"), PAGES, 1_024);
    let read = db.begin_read().unwrap();
    let mut rng = Rng(0x0FED_CBA9);
    let mut latencies = Vec::with_capacity(READS as usize);
    let start = Instant::now();
    for _ in 0..READS {
        let id = PageId(1 + rng.below(PAGES));
        let op = Instant::now();
        black_box(read.read_page(id, read_u64).unwrap());
        latencies.push(op.elapsed());
    }
    report("read (pool 1/16 of db)", READS, start.elapsed(), latencies);
}

/// Time to checkpoint a large number of dirty pages.
fn checkpoint(dir: &BenchDir) {
    const PAGES: u64 = 10_000;
    let db = Database::open(dir.db("checkpoint"), options(16_384)).unwrap();
    let mut txn = db.begin_write().unwrap();
    for i in 0..PAGES {
        let id = txn.allocate_page(PageType::Heap);
        write_u64(txn.page_mut(id).unwrap(), i);
    }
    txn.commit().unwrap();
    let start = Instant::now();
    db.checkpoint().unwrap();
    let elapsed = start.elapsed();
    report_once("checkpoint", PAGES, "dirty pages", elapsed);
}

/// Time to open a database whose WAL holds many committed transactions.
fn recovery(dir: &BenchDir) {
    const TXNS: u64 = 1_000;
    let path = dir.db("recovery");
    {
        let db = populate(&path, 1_000, 4_096);
        let mut rng = Rng(0x5555_AAAA);
        for n in 0..TXNS {
            let mut txn = db.begin_write().unwrap();
            write_u64(txn.page_mut(PageId(1 + rng.below(1_000))).unwrap(), n);
            txn.commit().unwrap();
        }
        // Dropped without a checkpoint, so the next open replays the WAL.
    }
    let wal_bytes = std::fs::metadata(path.with_file_name("recovery.oxen-wal"))
        .unwrap()
        .len();
    let start = Instant::now();
    let db = Database::open(&path, options(4_096)).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(db.open_report().recovery.committed_txns as u64, TXNS);
    report_once("recovery", TXNS, "txns replayed", elapsed);
    println!(
        "  replayed WAL size: {:.1} MiB",
        wal_bytes as f64 / (1024.0 * 1024.0)
    );
}
