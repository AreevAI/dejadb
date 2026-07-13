---
name: dejadb-add-operation
description: End-to-end playbook for adding or changing a user-facing DejaDB operation across every public surface — store method, CAL, MCP tool, CLI verb, Python, and Node bindings — with the test that gates each. Use whenever a new capability (or a changed signature) needs to be reachable by users, not just internal to one crate.
---

# Adding a DejaDB operation across all surfaces

A user-facing capability fans out across up to **six surfaces**. The failure
mode this skill prevents: adding it in one place (usually the store) and
forgetting the bindings, so Python/JS/MCP silently lag behind. Decide the reach
first, then touch each surface in dependency order.

## Decide the reach (do this before editing)

- **Core primitive?** New store behavior lives in `dejadb-store` `DejaDB`
  (`crates/dejadb-store/src/lib.rs`). Read the store CLAUDE.md first — the
  single-writer, immutable-blob, and fail-open invariants constrain what a new
  method may do.
- **Reachable from CAL?** Only if a query should trigger it. That is a
  **separate, heavier task** — use the `dejadb-cal-feature` skill, and remember
  CAL *syntax* is an OMS conformance contract.
- **Public binding?** If agents/users call it directly, it needs MCP + CLI +
  Python + JS. If it is internal plumbing, stop after the store (+ CAL).

## Surface map — touch in dependency order

1. **`dejadb-store/src/lib.rs`** — the `DejaDB` method. This is the source of
   truth; every surface below is a thin adapter over it. Gate:
   `crates/dejadb-store/tests/store_tests.rs` (copy the nearest existing test;
   fork/merge tests use *fixed* `created_at` for deterministic tiebreaks).
2. **`dejadb-mcp/src/lib.rs`** — TWO edits: add the arm to the `call_tool`
   match (~`lib.rs:114`) **and** the schema entry in `tool_defs()` (~`lib.rs:236`).
   Convention: tool *failures* are `isError: true` **results**, only protocol
   faults are JSON-RPC errors; notifications (no id) get no response. Gate:
   `crates/dejadb-cli/tests/mcp_smoke.rs` (drives the real binary over real
   stdio — there are no in-crate MCP tests).
3. **`dejadb-cli/src/main.rs`** — add an arm to `match cmd.as_str()`
   (~`main.rs:232`); flags come from `parse_args` (~`main.rs:116`) as a
   `HashMap`, no clap. Gate: `crates/dejadb-cli/tests/cli_smoke.rs`.
4. **`dejadb-py/src/lib.rs`** — a `#[pymethods]` fn with a `#[pyo3(signature=…)]`.
   FFI convention: **scalars in, JSON strings out**; errors → `PyValueError`.
   Gate: `crates/dejadb-py/tests/test_dejadb.py` (CI runs `maturin develop`
   then pytest).
5. **`dejadb-js/src/lib.rs`** — a `#[napi]` method (native Node addon, **not**
   wasm). Same scalars-in / JSON-out shape; `err()`/`parse_hash()` helpers
   already exist. Gate: `crates/dejadb-js/__test__/smoke.mjs` (`node --test`).
   NOTE: `dejadb-js` is a *standalone* napi package, **not** a workspace member —
   `cargo test --workspace` does not build it; CI's `node` job does.

## Verify

```bash
cargo test --workspace          # store + mcp_smoke + cli_smoke + py-less Rust
cargo clippy --workspace --all-targets -- -D warnings
```

- Python/JS are **not** in `cargo test --workspace`. If you touched them, run
  their gates the way CI does (maturin develop + pytest; napi build + node --test).
- Keep the six surfaces in parity: a reviewer should see the same operation
  named and shaped consistently across MCP tool, CLI verb, `py`, and `js`.
- Before committing, run the **dejadb-invariants** gate.
