# ADR 0002: Write-ahead log and crash recovery

- Status: Accepted
- Date: 2026-09-25

## Context

Before this change, a crash could leave a multi-page change half written, and
a torn page write could destroy a page outright. Checksums detect both, but
detection is not recovery. 1.0 requires that committed transactions survive
a crash and uncommitted ones leave no trace
(see `docs/release-criteria.md`).

## Decision

### Redo-only logging of full page images

A write transaction modifies *private copies* of pages. Nothing it changes
is visible in the buffer pool until commit. To commit:

1. Append one `PageImage` record per modified page, then a `Commit` record,
   to the WAL.
2. `fsync` the WAL. The transaction is now durable.
3. Install the new page images into the buffer pool as dirty pages.

Dirty pages are written back to the data file lazily (eviction or
checkpoint).

Because uncommitted changes never reach the buffer pool, they can never
reach the data file, so recovery needs no undo. Recovery reads the WAL from
the start, and for each transaction with a valid `Commit` record, writes its
page images to the data file. Replay is idempotent: an image is the full
page, so writing it twice is the same as writing it once.

Full images also repair torn data-file writes: if a crash tears a page
during write-back, that page's image is still in the WAL (the WAL is only
truncated after the data file is fsynced), and replay overwrites the torn
page.

### Page allocation and the file header

The file header (page 0) stores the page count, so growing the database
changes it. Writing it directly on every allocation would leave a window
where a torn header write makes the database unopenable, with no WAL copy
to repair it from. Instead:

- Allocation happens in memory inside the write transaction. Since only one
  write transaction runs at a time, a rollback simply discards the new page
  ids; no pages leak.
- A commit that grows the database logs a `PageImage` of the new file
  header, exactly like any other page.
- The data file's header is only written at checkpoint (after the pages it
  describes) and during recovery. Between checkpoints it lags behind, and the
  WAL is authoritative. A torn header write can only happen while the WAL
  still holds the header image that repairs it.

Recovery therefore runs *before* the data file's header is validated: it
writes committed images, including any header image, straight into the
file, and only then is the file opened normally.

### WAL file

The WAL lives next to the database as `<database>-wal`. It starts with a
header (magic, format version, checksum) followed by records. Each record
carries its own CRC32C and a sequence number that must increase by exactly
one. Reading stops at the first record that is incomplete, fails its
checksum, or breaks the sequence; everything from there on is treated as
the torn tail of an interrupted write and discarded.

### Checkpoint

A checkpoint writes all dirty pages, fsyncs the data file, then truncates the
WAL back to its header and fsyncs it. A crash between the two steps just
replays images that are already on disk.

### Isolation (for now)

A single database-wide reader-writer lock makes commits atomic with respect
to readers: a read transaction holds the shared lock, and the install step
of a commit holds the exclusive lock. Write transactions build their changes
without holding the lock, and only one write transaction may be active at a
time. This gives readers a consistent snapshot, at the cost of commits
waiting for active readers. MVCC (roadmap stage 5) will replace it.

## Alternatives considered

- **ARIES-style undo/redo with a steal buffer pool.** Supports transactions
  larger than memory and small delta records, but needs undo logging,
  compensation records, and a three-pass recovery. Much more code to get
  right, for benefits that do not matter yet.
- **Delta records instead of page images.** Far less write amplification for
  small changes, but no torn-page repair on its own (it needs full-page
  writes after each checkpoint anyway, as PostgreSQL does). Can be added
  later as an optimization with a benchmark to justify it.
- **Shadow paging / copy-on-write B-tree (LMDB style).** Elegant, no WAL,
  but ties the page allocator to the index structure and makes the file grow
  until old pages are reclaimed.

## Consequences

- Every committed page change writes 4 KiB to the WAL, even for one changed
  byte. This is the main cost of the design and will be measured.
- A write transaction's modified pages live in memory until commit, so
  transaction size is bounded by memory.
- A WAL file that is missing while the data file exists is treated as
  empty, as SQLite does. Deleting the WAL by hand after a crash loses the
  transactions in it.
- Commits wait for in-flight readers to finish.
