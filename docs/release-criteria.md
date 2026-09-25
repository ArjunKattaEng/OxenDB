# Release criteria

This is the checklist a release must meet before it gets a given version
number. It exists so "v1" means something specific and checkable, not a
feeling.

Status markers: `[x]` done, `[ ]` not done. Keep this file up to date as work
lands; the release PR for each milestone should show every box ticked.

## 0.x releases (pre-stable)

Any 0.x release may change the file format, SQL behavior, and public API.
Each 0.x release must still:

- [ ] Pass CI on every supported platform
- [ ] Have a CHANGELOG entry describing user-visible changes
- [ ] Refuse to open files from an incompatible format version, with a clear
      error (never misread them)

## 1.0

1.0 is a promise: data written by 1.0 stays readable, and a crash does not
lose committed data. Nothing below is optional.

### Durability and correctness

- [x] Write-ahead log with checksummed records
- [x] Crash recovery that restores every committed transaction and no
      uncommitted one
- [ ] Crash tests that kill the process at every WAL and page write point
      and verify recovery. *Partial:* torn WAL writes are simulated at many
      offsets and a writer is SIGKILLed at random points; torn data-file
      writes during checkpoint are covered only by unit tests of replay.
- [ ] Atomic, isolated transactions (`BEGIN` / `COMMIT` / `ROLLBACK`).
      *Partial:* page-level transactions exist; SQL statements do not.
- [ ] Concurrency tests with many readers and writers, run repeatedly in CI.
      *Partial:* readers against a single writer are tested; concurrent
      writers are serialized by design until MVCC.
- [x] Checkpointing so the WAL does not grow without bound
- [ ] Free-space management so deleted data's pages are reused
- [ ] Fuzzing of the page decoder, WAL decoder, and SQL parser, with no open
      crashes. *Partial:* randomized, checksum-aware robustness tests for the
      page, header, and WAL decoders; no coverage-guided fuzzer yet.

### Stability promises

- [ ] File format frozen and fully documented in `docs/storage-format.md`
- [x] Format compatibility test: a checked-in database file written by 1.0
      that every later build must open
- [ ] Public Rust API reviewed; everything not meant to be stable is private
      or clearly marked unstable
- [ ] Supported SQL subset documented, with tests for every documented
      statement

### Usability

- [ ] SQL: `CREATE TABLE`, `DROP TABLE`, `INSERT`, `SELECT` (with `WHERE`,
      `ORDER BY`, `LIMIT`, basic aggregates), `UPDATE`, `DELETE`
- [ ] At least one index type used automatically by the planner
- [ ] Command-line shell for running SQL against a database file
- [ ] Error messages that say what went wrong and, where possible, what to do

### Performance

- [ ] Reproducible benchmark suite (insert, point lookup, scan, update,
      delete, recovery time) with documented hardware and method.
      *Partial:* page-level commit, read, checkpoint, and recovery
      benchmarks in `docs/benchmarks.md`.
- [ ] No known performance cliffs left undocumented

### Project

- [x] Security policy, contributing guide, changelog
- [ ] Release process documented and exercised on at least one 0.x release
- [ ] Windows support, or its absence stated in the README

## Current state

As of 2026-09-25: the storage layer is durable and crash-tested (WAL,
recovery, checkpoints, page-level transactions). Everything above the
storage layer is still missing: free-space management, the catalog, SQL,
indexes, and the CLI. oxenDB is not ready for a 0.1 release until CI has
run green on every supported platform.
