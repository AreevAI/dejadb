---
name: dejadb-bindings
description: Playbook for the Python (dejadb-py, PyO3) and Node (dejadb-js, napi-rs) bindings — the "scalars in, JSON strings out" FFI convention, keeping the two in lockstep with the core API, and the per-language build/test (maturin/pytest, napi/node --test). Use whenever you add or change a binding method, or need to build/test either binding. Critical gotcha: dejadb-js is a STANDALONE package, not a cargo workspace member, so `cargo test --workspace` never touches it and a Rust change can silently break it.
---

# Python + Node bindings

Both wrap the same core: `#[pyclass] DejaDB` (`dejadb-py/src/lib.rs`) and
`#[napi]` methods (`dejadb-js/src/lib.rs`), each over a `DejaDbFacade`. They are
**thin** — the logic lives in the memory stack; a binding only marshals types.

## The FFI convention (do not deviate)

- **Scalars in, JSON strings out.** Arguments are primitives (str, int, float,
  bool, `Option`); anything structured is returned as a **JSON string** the
  caller parses. Don't invent per-language struct returns — it breaks the
  cross-language symmetry and the docs.
- **Errors** → the language's exception: Python maps every core error via
  `err()` → `PyValueError` (`lib.rs:44`); Node returns a napi `Error`. A core
  `DejaDbError` must never escape as a panic.
- **Parity is the contract.** A method exists in one binding iff it exists in
  the other, with the same name shape and the same JSON payload. Changing one
  without the other is the #1 binding bug.

## Adding or changing a method — the fan-out

1. **`dejadb-py/src/lib.rs`** — add the `fn` inside the `#[pymethods] impl
   DejaDB` block; scalars in, `PyResult<String>` (JSON) out; map errors with
   `err(...)`. Reuse the `parse_hash`/`parse_duration_ms`/`status_from_str`
   helpers.
2. **`dejadb-js/src/lib.rs`** — add the matching `#[napi]` method, same
   signature shape, same JSON return.
3. **Both smoke tests** — extend `dejadb-py/tests/test_dejadb.py` (pytest) and
   `dejadb-js/__test__/smoke.mjs` (`node --test`). These are the only gate that
   proves parity, since neither is in `cargo test --workspace`.
4. If this is a brand-new user-facing operation (not just a binding of an
   existing one), it also fans out across store/CAL/MCP/CLI — see
   [[dejadb-add-operation]].

## Build & test

**Python** (`crate-type = ["cdylib"]`, `pyo3` `abi3-py39` → one wheel for
py39+):
```bash
cd crates/dejadb-py
maturin develop            # build + install into the current venv
python -m pytest tests/    # or: pytest test_dejadb.py
```
`build.rs` adds macOS `-undefined dynamic_lookup` so a bare `cargo build` links
without a Python lib present (extension-module resolves symbols at load time).

**Node** (napi-rs, native addon — **not** wasm):
```bash
cd crates/dejadb-js
napi build --release       # produces dejadb.<platform>.node + index.js/.d.ts
node --test __test__/smoke.mjs
```

## The standalone-JS gotcha (read this)

`dejadb-js` is deliberately **outside the cargo workspace** (its `Cargo.toml`
declares its own `[workspace]`; napi cdylibs resolve Node-API symbols at load
time and need their own build). Consequences:

- `cargo test --workspace` and `cargo build --workspace` **skip it entirely** —
  a change to `dejadb-core`/`dejadb-store`/`dejadb-cal` can break the JS binding
  and the workspace suite stays green. CI's separate **`node` job** is what
  catches it (`napi build --release` + `node --test`); run that locally after
  any core API change that the JS binding surfaces.
- The Python binding **is** a workspace member (`dejadb-py`, `publish = false`),
  so `cargo build -p dejadb-py` compiles with the workspace, but its Python
  behavior is only proven by pytest, not `cargo test`.

## Naming & publish

Python module is `dejadb` (PyPI `dejadb`); npm package `dejadb` with a
`dejadb-<platform>` native subpackage per target. `dejadb-py`/`dejadb-bench`
stay `publish = false` in cargo. Version is inherited from
`[workspace.package]`; releasing is [[dejadb-release]].
