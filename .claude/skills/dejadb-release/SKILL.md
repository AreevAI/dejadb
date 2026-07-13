---
name: dejadb-release
description: Runbook for cutting a DejaDB release — version bump, changelog, and the correct publish order across crates.io, PyPI, and npm. Use when the user asks to release, publish, tag, or ship a new version of DejaDB.
---

# DejaDB release runbook

DejaDB is a Rust workspace (9 crates) plus Python bindings and a JS binding.
Follow this order; the workspace has internal `path` dependencies, so crates
must publish bottom-up.

## 1. Pre-flight

- Working tree clean; on an up-to-date `main`.
- `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo deny check` passes (advisories, licenses, sources, bans).
- Fuzz smoke: `cargo +nightly fuzz build`.
- Confirm `THIRD-PARTY-NOTICES.md` is current (regenerate with `cargo about` if deps changed).

## 2. Version + changelog

- Bump `version` in `[workspace.package]` in the root `Cargo.toml` (all crates
  inherit it via `version.workspace = true`).
- Move the `[Unreleased]` section of `CHANGELOG.md` under a new dated version
  heading; add a fresh empty `[Unreleased]`.
- Commit: `Release vX.Y.Z`. Tag: `git tag vX.Y.Z`.

## 3. Publish crates.io (bottom-up dependency order)

Each crate currently has `publish = false` — flip it to publish the intended
crates, then publish in this order (a crate can only publish after its path
dependencies are on crates.io):

```
dejadb-core → dejadb-store → dejadb-cal → dejadb-context
            → dejadb-mcp, dejadb-server, dejadb-cli
```

```bash
cargo publish -p dejadb-core
# wait for it to index, then the next tier, etc.
```

`dejadb-bench` stays unpublished (internal harness). `dejadb-py` is not published
to crates.io — it ships to PyPI.

## 4. Publish PyPI (dejadb-py)

Build abi3 wheels with maturin (cibuildwheel or maturin-action in CI for the
full platform matrix), then upload:

```bash
maturin build --release -m crates/dejadb-py/Cargo.toml
# CI builds linux/macos/windows abi3 wheels; then:
maturin upload target/wheels/*   # or twine upload
```

The package name is `dejadb` (reserved). Requires-Python `>=3.9` (abi3-py39).

## 5. Publish npm (dejadb-js, napi)

`dejadb-js` is a **native Node addon built with napi-rs (not wasm)** and is a
standalone package — it is not a `cargo` workspace member, so it publishes
independently of the crates.io tier. Build the per-platform prebuilds and
publish (name `dejadb`, reserved):

```bash
# from crates/dejadb-js — CI builds the platform matrix via `napi build --release`
cd crates/dejadb-js
npm publish --access public
```

## 6. Post-release

- Push the tag: `git push origin main --tags`.
- Create a GitHub Release from the tag with the changelog section.
- Verify install paths work: `cargo install dejadb-cli`, `pip install dejadb`,
  `npx dejadb` / `npm i dejadb`.

## Notes

- All three registry names (`dejadb` on crates.io/PyPI/npm) are reserved.
- Keep `rust-version` (MSRV) in `[workspace.package]` accurate — CI has an MSRV job.
- Never reuse or renumber error codes across releases (append-only).
