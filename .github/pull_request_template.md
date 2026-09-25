**What this changes and why**

**How it was tested**

**Durability and concurrency**
If this touches the file format, WAL, recovery, or locking, explain why it is
crash-safe and race-free. Otherwise write "N/A".

- [ ] `cargo fmt`, `cargo clippy -D warnings`, and `cargo test` (debug and release) pass
- [ ] Docs, ADRs, and CHANGELOG updated where relevant
