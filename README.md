# oxenDB

An embeddable SQL database engine written in Rust. It is in early
development, with the storage layer being built first.

## Why oxenDB?

SQLite showed how much a database gains from being a single file you can
embed anywhere. oxenDB aims to keep that simplicity while growing into things
SQLite leaves out: a server mode, the PostgreSQL wire protocol, vectorized
execution for analytics, and vector and full-text search.

That is a long road, and the order matters. The plan is to make a single-node
engine correct and fast first: pages, caching, write-ahead logging, crash
recovery, transactions. Only after that is solid do we build SQL,
indexes, and the rest on top. Every layer gets tests before the next one
goes on.

## Status

**Pre-alpha. Not usable as a SQL database yet.** There is no SQL interface
and no CLI.

What exists is a transactional page store: you can open a database, change
pages in atomic, durable transactions, and read consistent snapshots. It
recovers from crashes using a write-ahead log, and that recovery is tested by
killing a writer process and by simulating torn writes. The on-disk format
may still change before 1.0; see [release criteria](docs/release-criteria.md)
for what 1.0 requires and what is done.

## Quick start

oxenDB is not published to crates.io yet. Clone the repository and run
the tests:

```sh
git clone <repository-url> oxendb
cd oxendb
cargo test
```

## Example

The API today is page-level. Tables and SQL will be built on top of it.

```rust
use oxendb::storage::page::PageType;
use oxendb::{Database, Options};

let db = Database::open("example.oxen", Options::default())?;

// Write transactions are atomic and durable once `commit` returns.
let mut txn = db.begin_write()?;
let id = txn.allocate_page(PageType::Heap);
txn.page_mut(id)?.payload_mut()[..5].copy_from_slice(b"hello");
txn.commit()?;

// Read transactions see a consistent snapshot of committed data.
let read = db.begin_read()?;
let greeting = read.read_page(id, |page| page.payload()[..5].to_vec())?;
assert_eq!(greeting, b"hello");
```

This is the crate-level doctest in `crates/oxendb/src/lib.rs`, so it is
compiled and run by `cargo test`.

## Features

Implemented:

- Single-file database plus a write-ahead log (`<name>-wal`)
- 4 KiB pages with CRC32C checksums and self-identifying page ids, verified
  on every read
- Atomic, durable write transactions; rollback by dropping the transaction
- Read transactions with consistent snapshots (commits wait for readers)
- Crash recovery by replaying committed page images; repairs torn page writes
- Automatic and manual checkpoints to keep the WAL bounded
- Buffer pool with pinning, CLOCK eviction, and dirty-page write-back

Known limitations:

- One writer at a time, and a commit waits for all open readers
- A write transaction's changes are held in memory until commit
- No free-space reuse: the file only grows
- Linux and macOS only

## Architecture

The engine lives in one crate, `crates/oxendb`, organized by subsystem:

```
db/             Database handle, open/recover/checkpoint, transactions
storage/
  page          page buffer and header
  file_header   page 0 layout
  disk          reads and writes pages in the data file
  buffer_pool   fixed-size page cache
  wal/          write-ahead log records and file
  recovery      WAL replay
  checksum      CRC32C
```

Lower layers never depend on higher ones. Catalog, SQL, and execution will
sit above `db`.

Further reading:

- [Storage format](docs/storage-format.md)
- [ADR 0001: foundations](docs/adr/0001-foundations.md)
- [ADR 0002: write-ahead log and recovery](docs/adr/0002-wal-and-recovery.md)
- [Release criteria](docs/release-criteria.md)

## Performance

First measurements of the storage layer, with the machine they ran on and
what they do and do not show, are in [docs/benchmarks.md](docs/benchmarks.md).
No comparisons with other databases have been made.

```sh
cargo bench -p oxendb --bench storage
```

## Roadmap

Roughly in order; nothing below exists yet:

1. **Finish the storage foundation:** free-space management, catalog,
   fuzzing with a coverage-guided fuzzer
2. **Minimal SQL:** `CREATE TABLE`, `INSERT`, `SELECT` with `WHERE`, over
   sequential scans, plus a command-line shell
3. **Execution engine:** composable operators (scan, filter, project, limit,
   sort, aggregate, join) independent of the SQL front end
4. **Indexes:** B-tree and hash indexes with planner-driven index selection
5. **Concurrency:** MVCC, concurrent readers and writers, group commit
6. **Performance:** vectorized execution, parallel scans, broader benchmarks

Later: server mode, PostgreSQL wire protocol, vector and full-text search,
JSON, backups, time travel, and replication. Each of these waits until the
core it depends on is solid.

## Building from source

Requires Rust 1.85 or newer. Linux and macOS are supported; Windows is not
yet.

```sh
cargo build --release
```

## Testing

```sh
cargo test                    # unit, integration, crash, and doc tests
cargo test --release          # also worth running; timing-sensitive tests behave differently
cargo clippy --all-targets    # lints
cargo fmt --all --check       # formatting
```

The crash tests live in `crates/oxendb/tests/`: `torn_wal.rs` simulates
power loss during a commit, and `process_kill.rs` kills a writer process with
SIGKILL at random points. For a longer run of the randomized decoder tests:

```sh
OXENDB_ROBUSTNESS_ITERS=1000000 cargo test --release --test decoder_robustness
```

## Contributing

The most useful contributions right now are reviews of the storage code,
especially the concurrency and durability reasoning, and bug reports with a
failing test. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup and
expectations, and [SECURITY.md](SECURITY.md) for reporting vulnerabilities.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
