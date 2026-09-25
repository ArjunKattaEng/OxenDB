# Changelog

All notable changes are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions
follow [Semantic Versioning](https://semver.org/). Before 1.0, minor versions
may contain breaking changes, including to the file format.

## [Unreleased]

### Added

- Single-file database format with a versioned, checksummed file header
- 4 KiB pages with CRC32C checksums and self-identifying page ids
- Disk manager with positional I/O and page allocation
- Buffer pool with pinning, CLOCK eviction, and dirty-page write-back
- Write-ahead log with checksummed, sequence-numbered records
- `Database` handle with crash recovery on open, manual and automatic
  checkpoints, and `close`
- Atomic, durable write transactions over pages; snapshot read transactions
- Crash tests (simulated torn WAL writes and SIGKILL of a writer process),
  randomized decoder robustness tests, and a format compatibility fixture
- Storage benchmarks (`cargo bench -p oxendb --bench storage`)
