# ADR 0001: Language, crate layout, and page format foundations

- Status: Accepted
- Date: 2026-09-25

## Context

oxenDB is starting from an empty repository. The first decisions constrain
everything after them: the implementation language, how the code is split
into packages, and the basic unit of storage.

## Decisions

### Rust

The engine is written in Rust. A database needs control over memory layout
and allocation, predictable latency (no garbage collector pauses), and a
strong defense against memory-safety and data-race bugs, which in a database
turn directly into corruption. Rust provides all three. It also has good
built-in tooling for tests, benchmarks, fuzzing, and a WASM target, which is
on the long-term roadmap.

`unsafe` is denied workspace-wide. A module that needs it must opt in
explicitly with a documented reason and tests covering its safe boundary.

### One core crate for now

All engine code lives in `crates/oxendb`, split into modules by subsystem
(`storage`, and later `catalog`, `sql`, `execution`, `txn`). Module
boundaries are cheap to move while the internal APIs are still settling;
crate boundaries are not. Subsystems will be split into their own crates once
their interfaces are stable, or when compile times justify it. Binaries (CLI,
server) will be separate crates that depend on the core.

### Fixed 4 KiB pages

The database file is a sequence of 4 KiB pages. Page 0 is the file header.
4 KiB matches common filesystem and SSD block sizes, so one page write maps
to one device block. The size is a compile-time constant for now and is
recorded in the file header so a future build with configurable page sizes can
detect mismatches.

### CRC32C on every page

Every page carries a CRC32C checksum and its own page id, verified on every
read. This catches torn writes, bit rot, and misdirected I/O before bad data
reaches higher layers. CRC32C was chosen over CRC32 (IEEE) because it has
hardware support on x86-64 and ARMv8, which can be used later without
changing the file format.

### No third-party runtime dependencies yet

Everything so far is small enough to write directly. Dependencies will be
added when they remove real complexity, and each one should be justified.

## Consequences

- Windows is not supported yet: the disk manager uses Unix positional I/O.
- A single crate means a change anywhere recompiles the whole engine. This is
  acceptable at the current size.
- Crash atomicity of multi-page changes is not provided by the page format
  alone; it depends on the WAL, which is the next major component.
