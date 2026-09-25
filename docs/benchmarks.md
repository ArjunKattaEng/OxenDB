# Benchmarks

These numbers describe the storage layer only: page-level transactions, no
SQL. They are here to track oxenDB against itself over time, not to compare
it with other databases. No comparison with another database has been run.

## Running

```sh
cargo bench -p oxendb --bench storage
```

The harness is `crates/oxendb/benches/storage.rs`. It creates databases in
the system temp directory and deletes them afterwards.

## Results: 2026-09-25

Commit `aec0c72`. Three consecutive runs; the
table shows the range.

**Machine:** Apple M1 Max (10 cores), 32 GiB RAM, internal SSD, APFS,
macOS 26.6.2, Rust 1.94.0, release profile.

| Workload                               | Throughput         | p50       | p95        | p99        |
|----------------------------------------|--------------------|-----------|------------|------------|
| Commit, 1 page per txn (fsync)         | 200–215 txn/s      | 4.5–5.0ms | 5.8–6.8ms  | 8.5–9.2ms  |
| Commit, 16 pages per txn (fsync)       | 146–172 txn/s      | 5.5–6.3ms | 7.3–10.4ms | 9.2–14.1ms |
| Page read, all cached                  | 22.3–22.9 M/s      | n/a       | n/a        | n/a        |
| Page read, pool 1/16 of database       | 91–93 K/s          | 11.2µs    | 12.4–14.0µs| 14.2–14.5µs|
| Checkpoint 10,000 dirty pages          | 160–171ms total    |           |            |            |
| Recovery, 1,000 txns / 4 MiB WAL       | 43–69ms total      |           |            |            |

Cached reads are measured in bulk because a single read is too short to
time individually, so they have no percentiles.

## What these numbers mean

- **Commits are bound by fsync.** On macOS, Rust's standard library
  implements `sync_data` with `F_FULLFSYNC`, which flushes the drive's
  write cache and takes several milliseconds on this machine. Plain `fsync`
  on macOS is faster but does not guarantee durability, so it is not used.
  Linux numbers will differ substantially. There is no group commit yet:
  concurrent writers would still pay one fsync each, serialized.
- **Uncached reads are served by the OS page cache**, not the SSD: the
  database fits in RAM. They measure `pread` plus checksum verification plus
  buffer pool bookkeeping. At 11µs this is slower than expected and has not
  been profiled yet.
- **Recovery** replays page images, so its cost scales with the WAL size, not
  the number of transactions.

## Not measured yet

Memory usage, CPU utilization, concurrent readers and writers, database
startup without recovery, and anything involving SQL, indexes, or rows. The
benchmark list in `docs/release-criteria.md` tracks what is still missing.
