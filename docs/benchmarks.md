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

## Latest results: 2026-09-26

Commit `5f39cef` (hardware CRC32C). Three consecutive runs; the table shows
the range.

**Machine:** Apple M1 Max (10 cores), 32 GiB RAM, internal SSD, APFS,
macOS 26.6.2, Rust 1.94.0, release profile.

| Workload                               | Throughput         | p50       | p95        | p99        |
|----------------------------------------|--------------------|-----------|------------|------------|
| Commit, 1 page per txn (fsync)         | 196–210 txn/s      | 4.5–5.0ms | 6.2–7.3ms  | 8.0–10.6ms |
| Commit, 16 pages per txn (fsync)       | 177–189 txn/s      | 5.1–5.3ms | 7.1–8.3ms  | 9.0–10.4ms |
| Page read, all cached                  | 18.6–23.7 M/s      | n/a       | n/a        | n/a        |
| Page read, pool 1/16 of database       | 608–649 K/s        | 1.5µs     | 1.9–2.2µs  | 3.1–5.5µs  |
| Checkpoint 10,000 dirty pages          | 74–119ms total     |           |            |            |
| Recovery, 1,000 txns / 4 MiB WAL       | 18–28ms total      |           |            |            |

CRC32C alone (`cargo bench -p oxendb --bench checksum`), one run:

| Input   | Throughput  | Per call |
|---------|-------------|----------|
| 64 B    | 10.29 GiB/s | 5.0ns    |
| 4 KiB   | 8.28 GiB/s  | 460ns    |
| 1 MiB   | 7.94 GiB/s  | 123µs    |

Cached reads are measured in bulk because a single read is too short to
time individually, so they have no percentiles.

## History

### Concurrent reads and buffer pool sharding (2026-09-26)

A new benchmark (`read (cached, N threads)`) showed that concurrent reads
got *slower* as threads were added: every read took the buffer pool's one
global mutex twice. Two changes followed:

- `9686c7c`: a multiplicative hasher for the page table instead of the
  standard DoS-resistant one. Single-threaded cached reads went from
  18.6–23.7M/s to 30.4–32.5M/s.
- `e917415`: the pool is split into independently locked shards.

Old and new benchmark binaries were run alternately, three times each,
under the same conditions. The machine was heavily loaded by other
programs (load average about 17 on 10 cores), so absolute multi-threaded
numbers are pessimistic; the comparison between the two is fair.

| Threads | One lock      | Sharded       |
|---------|---------------|---------------|
| 1       | 37.7–38.4M/s  | 34.2–34.5M/s  |
| 2       | 16.6–17.8M/s  | 31.2–31.5M/s  |
| 4       | 6.1–7.2M/s    | 35.2–36.2M/s  |
| 8       | 3.1–5.1M/s    | 11.8–12.2M/s  |

Sharding costs about 9% single-threaded (an extra hash to pick the shard)
and removes the collapse under concurrency. Total throughput still does not
grow with thread count: each read still takes a shard mutex twice (pin and
unpin), and the 8-thread result needs re-measuring on an idle machine
before drawing conclusions from it.

### Checksum speedup (2026-09-26)

The first run showed uncached reads at 11µs. Measuring CRC32C on its own
found it took 10.3µs per 4 KiB page, nearly all of that time. Two changes
fixed it:

| Change                                   | 4 KiB CRC32C | Uncached read p50 |
|------------------------------------------|--------------|-------------------|
| Byte-at-a-time table (baseline)          | 10.3µs       | 11.2µs            |
| Slicing-by-8 (`e69c328`)                 | 2.1µs        | not measured      |
| Hardware instructions (`5f39cef`)        | 460ns        | 1.5µs             |

The same change made checkpoints about 1.7x and recovery about 2.4x
faster, since both checksum every page. Commits did not change: they are
bound by fsync, not checksums.

### First results (2026-09-25)

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

## What these numbers mean

- **Commits are bound by fsync.** On macOS, Rust's standard library
  implements `sync_data` with `F_FULLFSYNC`, which flushes the drive's
  write cache and takes several milliseconds on this machine. Plain `fsync`
  on macOS is faster but does not guarantee durability, so it is not used.
  Linux numbers will differ substantially. There is no group commit yet:
  concurrent writers would still pay one fsync each, serialized.
- **Uncached reads are served by the OS page cache**, not the SSD: the
  database fits in RAM. They measure `pread` plus checksum verification plus
  buffer pool bookkeeping.
- **Recovery** replays page images, so its cost scales with the WAL size, not
  the number of transactions.

## Not measured yet

Memory usage, CPU utilization, concurrent readers and writers, database
startup without recovery, and anything involving SQL, indexes, or rows. The
benchmark list in `docs/release-criteria.md` tracks what is still missing.
