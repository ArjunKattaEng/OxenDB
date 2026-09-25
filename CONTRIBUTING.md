# Contributing to oxenDB

Thanks for looking. This file covers how to get set up, what we expect from a
change, and how review works.

## Setup

You need Rust 1.85 or newer on Linux or macOS.

```sh
cargo build
cargo test
```

## Before opening a pull request

Run all four; CI runs the same checks:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --workspace --release
```

## What a good change looks like

- **Small.** One commit should do one thing: "Add WAL record encoding", not
  "Implement WAL". A PR can contain several such commits; please don't squash
  them into one.
- **Tested.** Every behavior change comes with a test that fails without it.
  Bug fixes start with a test that reproduces the bug.
- **Honest about durability.** Anything touching the file format, the WAL,
  or locking should explain in the PR description why it is crash-safe and
  race-free, not just that tests pass.
- **Documented.** Public items need doc comments. Changes to the on-disk
  format must update `docs/storage-format.md` and bump the format version.
  Significant design choices get an ADR in `docs/adr/`.
- **No new dependencies without a reason.** Say in the PR what the
  dependency replaces and why writing it ourselves is worse.
- **No `unsafe` without a reason.** If you need it, isolate it, document the
  invariants it relies on, and test the safe wrapper.

## Commit messages

Short imperative subject, no prefix:

```
Add buffer pool eviction and write-back
```

Use the body to explain *why* when it isn't obvious.

## Performance changes

Include before/after numbers from a benchmark in the repository, with the
machine you ran on. A change that makes code harder to read needs a
measurable improvement to justify it.

## Reporting bugs

Open an issue with the oxenDB version or commit, your OS, and the smallest
steps that reproduce it. For anything that looks like data corruption, a copy
of the affected file (if it holds nothing sensitive) is very helpful.

Security issues should not go in public issues; see [SECURITY.md](SECURITY.md).

## License

By contributing, you agree that your contributions are licensed under the
same terms as the project: MIT OR Apache-2.0, at the user's option.
