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

**Pre-alpha. Not usable as a database yet.** There is no SQL interface, no
CLI, and no crash recovery. Do not store data you care about in it.

What exists today is the lowest storage layer, described under
[Features](#features). The on-disk format will change without migration
until the project reaches 1.0.

## Quick start

There is nothing to run yet beyond the test suite:

```sh
git clone <repository-url> oxendb
cd oxendb
cargo test
```

## Example

SQL examples will appear here once `CREATE TABLE`, `INSERT`, and `SELECT`
work end to end. Until then the unit tests in `crates/oxendb/src/storage/`
are the best reference for how the pieces fit together.

## Features

Implemented:

- Single-file database format with a versioned, checksummed header
- 4 KiB pages with a CRC32C checksum and self-identifying page id, verified
  on every read
- Disk manager with positional I/O and page allocation
- Thread-safe buffer pool with pinning, CLOCK eviction, and dirty-page
  write-back

## Architecture

The engine lives in one crate, `crates/oxendb`, organized by subsystem:

```
storage/
  checksum      CRC32C
  page          page buffer and header
  file_header   page 0 layout
  disk          reads, writes, and allocates pages in the file
  buffer_pool   fixed-size page cache
```

Lower layers never depend on higher ones. Catalog, SQL, execution, and
transactions will be added as sibling modules above `storage`.

Further reading:

- [Storage format](docs/storage-format.md)
- [ADR 0001: foundations](docs/adr/0001-foundations.md)

## Performance

There are no benchmarks yet, so there are no performance claims.
Benchmarks will be added alongside the first operations worth measuring, with
the hardware and methodology recorded next to the results.

## Roadmap

Roughly in order, nothing below exists yet:

1. **Storage foundation:** write-ahead log, crash recovery, free-space
   management, catalog, basic transactions
2. **Minimal SQL:** `CREATE TABLE`, `INSERT`, `SELECT` with `WHERE`, over
   sequential scans
3. **Execution engine:** composable operators (scan, filter, project, limit,
   sort, aggregate, join) independent of the SQL front end
4. **Indexes:** B-tree and hash indexes with planner-driven index selection
5. **Concurrency:** MVCC, concurrent readers and writers
6. **Performance:** vectorized execution, parallel scans, reproducible
   benchmarks

Later: CLI and interactive shell, server mode, PostgreSQL wire protocol,
vector and full-text search, JSON, backups, time travel, and replication.
Each of these waits until the core it depends on is solid.

## Building from source

Requires Rust 1.85 or newer. Linux and macOS are supported; Windows is not
yet.

```sh
cargo build --release
```

## Testing

```sh
cargo test                    # all tests
cargo test --release          # also worth running; timing-sensitive tests behave differently
cargo clippy --all-targets    # lints
cargo fmt --all --check       # formatting
```

## Contributing

The project is early, so the most useful contributions right now are
reviews of the storage code, especially the concurrency and durability
reasoning, and bug reports with a failing test.

Guidelines:

- Keep commits small and focused on one change.
- Add tests with every behavior change.
- Run the four commands under [Testing](#testing) before opening a PR.
- Record significant design decisions as an ADR in `docs/adr/`.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
