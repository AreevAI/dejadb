# AGENTS.md

Orientation for an AI coding agent working **in this repository**. (This is the
tool-neutral counterpart to a `CLAUDE.md` / Cursor rules file.) It is about
hacking on DejaDB's source — for using DejaDB *as* a memory engine, see the
[Cookbook](docs/cookbook.md) and [FAQ](FAQ.md).

## What DejaDB is

An **embedded memory engine for AI agents** and the reference implementation of
the [Open Memory Spec (OMS)](https://github.com/openmemoryspec/oms). Memories
are immutable, content-addressed **grains** stored one-memory-per-file in a
Turso (SQLite-compatible) database, queried with **CAL** (Context Assembly
Language), and rendered into model-ready context in-process — no server in the
recall path. It is a Rust workspace of 9 crates plus Python (`dejadb-py`) and
Node/napi (`dejadb-js`) bindings.

Think: *git for an agent's memory* — append-only log, history, forks with
explicit merges, content addressing — built into the data model.

## Build & test

```bash
cargo build --workspace
cargo test  --workspace          # full suite, fast
cargo test  -p dejadb-cal        # one crate
cargo run   -p dejadb -- --help
cargo run --release -p dejadb-store --example bench       # latency gates
cargo run --release -p dejadb-store --example voice_loop  # 50ms-cadence gate
```

- Minimum Rust: see `rust-version` in the workspace `Cargo.toml`.
- **Do not run blanket `cargo fmt`.** The tree is intentionally not
  rustfmt-clean; formatting the whole tree produces a huge, unreviewable diff.
  Match the surrounding style and format only the lines you touch.
- Keep it warning-clean: `cargo clippy --workspace` should add no new warnings.
- CI (`.github/workflows/ci.yml`) runs the workspace tests on
  ubuntu/macos/windows, clippy with `-D warnings`, an MSRV build, `cargo doc`,
  coverage, and the Python + Node binding suites; `security.yml` runs
  `cargo deny`. Still run tests locally before opening a PR.

## Workspace (dependency order)

```
dejadb-core ← dejadb-store ← dejadb-cal ← dejadb-context
                                  ↑              ↑
                    dejadb-mcp, dejadb-server, dejadb-py, dejadb (binary)
```

| Crate | What it does |
|---|---|
| `dejadb-core` | The `.mg` binary format, canonical serialization, SHA-256 content addressing, the 11 grain types, and tool-schema rendering. Depends on no other workspace crate. |
| `dejadb-store` | Turso-backed store: dictionary-encoded triple indexes, hybrid recall, heads/forks/merge, CAS blob sidecar, bundles & streaming, the Anthropic memory-tool adapter. |
| `dejadb-cal` | CAL lexer / parser / executor, multi-source `ASSEMBLE`, saved queries, and `DejaDbFacade` (with read-only mounts) that binds CAL to the store. |
| `dejadb-context` | Budget-aware rendering of recall results into model-ready context (SML / TOON / Markdown / JSON / plain text). |
| `dejadb-mcp` | Stdio MCP server exposing 6 memory tools over newline-delimited JSON-RPC. |
| `dejadb-server` | Std-only HTTP: local web console (memories / graph / query) plus a sync-hub mode (bearer-token segment push/pull). |
| `dejadb` | The `deja` binary — a thin shell over store + CAL. |
| `dejadb-py` | PyO3 bindings: `import dejadb`. |
| `dejadb-bench` | Reproducible benchmark harnesses (latency, honesty metrics, LoCoMo accuracy). |
| `dejadb-js` | Node.js (napi-rs) bindings: `require('dejadb')`. Standalone package, **not** a `cargo` workspace member. |

## Load-bearing invariants

These are the constraints that keep DejaDB correct and OMS-conformant. Changes
that break them will not be merged without a design discussion first.

1. **Grains are immutable and content-addressed.** The content address is
   SHA-256 over the entire `.mg` blob. Nothing ever edits a stored blob: every
   edit is a *supersession* (a new grain that points back), every removal a
   *tombstone* (`forget`) or crypto-erasure. Store code mutates the **index
   layer** only — never the blob. See `crates/dejadb-store`.
2. **Canonical serialization is frozen.** NFC-normalized strings, sorted map
   keys, compact field keys, omit-when-default. Changing any of this silently
   changes the content address of every grain ever written and breaks OMS
   conformance. Treat as frozen unless the spec moves. See `crates/dejadb-core`.
3. **CAL's destructive surface is narrow and gated.** The only destructive
   statement is `FORGET <hash>` (single-grain tombstone), gated by
   `allow_destructive_ops` (default on; `--no-destructive-ops` makes a session
   read-only) and requiring the `admin` scope on the server path. `DELETE`/`DROP`
   are not grammar tokens, `PURGE`/user/scope erasure stay out of the text
   grammar, and saved-query bodies stay read-only. Don't widen this without a
   spec-level (OMS) decision. See `crates/dejadb-cal`.
4. **One memory = one file.** The unit of erasure, sync, portability, and write
   parallelism — single writer per file. Cross-file queries go through
   `ASSEMBLE` with facade mounts, not shared connections. Files are
   self-describing: the `meta` table carries file-truths (text index, entity
   relations, embedding provenance); host capabilities are per-process and never
   persisted in the file.
5. **Error codes are append-only.** Every user-facing error carries a stable
   `DOMAIN-Ennn` code as the leading token of its `Display` string, plus a
   `code()` method. Domains: `FMT`, `MEM`, `STO`, `CRY`, `VAL`, `CAL`, `SYS`.
   Never renumber or reuse a code. Format and uniqueness are test-enforced. See
   [`ERROR_CODES.md`](ERROR_CODES.md).
6. **Dependency-light by policy.** No `clap` (args are hand-rolled), no HTTP
   framework (std `TcpListener`), no MCP SDK (hand-rolled JSON-RPC), no
   workspace-wide async runtime (the store wraps a private tokio current-thread
   runtime behind a sync API). Think twice before adding a dependency and
   justify the trade-off in your PR.

## Where key things live

- **Grain types & the `.mg` format** — `crates/dejadb-core/src/format/` and
  `crates/dejadb-core/src/types/`. The registry
  (`types/registry.rs`) is the source of truth for the 11 grain types.
- **Store schema, recall, forks/merge, bundles** — `crates/dejadb-store/src/lib.rs`.
- **CAL grammar & executor** — `crates/dejadb-cal/src/{lexer,parser,executor}.rs`.
  These are large; navigate with grep and offset reads, not full reads.
- **CLI verbs & flags** — `crates/dejadb-cli/src/main.rs` (`USAGE` const + the
  big `match` in `run()`).
- **MCP tools** — `crates/dejadb-mcp/src/lib.rs` (`tool_defs()`).
- **Error registry** — [`ERROR_CODES.md`](ERROR_CODES.md); text is inline on the
  error enums in `dejadb-core/src/error.rs` and `dejadb-cal/src/errors.rs`.
- **Per-crate deep dives** — each core crate has its own `CLAUDE.md`
  (`crates/dejadb-{core,store,cal}/CLAUDE.md`) with module maps and gotchas.
- **Design overview** — [`ARCHITECTURE.md`](ARCHITECTURE.md).
- **Contributing rules (DCO sign-off, PR flow)** — [`CONTRIBUTING.md`](CONTRIBUTING.md).

## Gotchas worth knowing early

- The `dejadb-store` `DejaDB` type is a **sync** facade over async Turso; it owns
  a private current-thread runtime and assumes a single writer per file.
- Turso's experimental FTS costs ~150ms per write transaction once the text
  index exists; the voice/edge profile runs with `index_text = false`.
- CAL runtime failures (bad grain type, unresolved param) come back as an **Ok**
  result with an `Unsupported` payload — check the payload, not just `Ok`/`Err`.
- If CLI/MCP smoke tests fail with `spawn dejadb: No such file or directory`
  after the repo folder moved, the cached test binary has a stale path baked in:
  `touch crates/dejadb-cli/tests/*.rs` and re-run.
