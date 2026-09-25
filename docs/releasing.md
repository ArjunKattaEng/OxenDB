# Releasing

Steps for cutting a release. Check `docs/release-criteria.md` first: a
version number is only used once every criterion for it is met.

1. **Check criteria.** Every box for the target version in
   `docs/release-criteria.md` is ticked on `main`.
2. **Check format compatibility.** If the on-disk format changed since the
   last release, confirm `FORMAT_VERSION` in
   `crates/oxendb/src/storage/file_header.rs` was bumped and
   `docs/storage-format.md` describes the new layout.
3. **Update the changelog.** Move the `Unreleased` entries in
   `CHANGELOG.md` under a new `## [X.Y.Z] - YYYY-MM-DD` heading.
4. **Bump the version** in the root `Cargo.toml` (`workspace.package.version`).
5. **Run the full check locally:**

   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cargo test --workspace --release
   cargo package -p oxendb
   ```

6. **Commit** as `Release X.Y.Z` and open a PR. Merge once CI passes.
7. **Tag** the merge commit: `git tag -a vX.Y.Z -m "oxenDB X.Y.Z"` and push
   the tag.
8. **Publish:** `cargo publish -p oxendb`.
9. **Write the GitHub release** from the changelog section. Call out file
   format changes and anything that needs user action at the top.

A published crate version cannot be replaced, only yanked. If a release is
broken, yank it, fix forward, and release a new patch version.
