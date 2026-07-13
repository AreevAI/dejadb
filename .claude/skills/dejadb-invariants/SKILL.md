---
name: dejadb-invariants
description: Load-bearing invariants and the pre-change quality gate for the DejaDB codebase. Use BEFORE editing any Rust source in this repo, and ALWAYS before committing — especially when touching the .mg format, canonical serialization, content addressing, CAL, error codes, or the network/crypto surfaces.
---

# DejaDB invariants & quality gate

DejaDB is an embedded memory engine. A handful of invariants are load-bearing —
violating them silently corrupts data or breaks spec conformance. Check them
before you change code and before you commit.

## The invariants (do not break without a design discussion)

1. **Grains are immutable and content-addressed** — the hash is SHA-256 over the
   whole `.mg` blob. Never mutate a stored blob. Every edit is a *supersession*,
   every removal a *tombstone* (`forget`) or crypto-erasure. Only the index layer
   is mutated.
2. **Canonical serialization is frozen** — NFC normalization, sorted keys, compact
   keys, omit-defaults. Any byte change alters every content address and breaks
   OMS conformance. If you touch `dejadb-core/src/format/{serialize,deserialize,
   header,field_map}.rs`, assume you are changing the wire format and STOP to
   confirm it is intended. The serialize path enforces the same size/depth limits
   as the deserializer, so a grain that can be written can always be read.
3. **CAL destruction is gated, not structural** — the only destructive CAL
   statement is `FORGET <hash>` (single-grain tombstone), gated at execution by
   `CalExecutorConfig::allow_destructive_ops` (default on; `--no-destructive-ops`
   turns it off per-process). `DELETE`/`ERASE`/`TRUNCATE`/… stay lexer-blocked
   non-tokens; `PURGE` and user/scope erasure stay out of the text grammar;
   `DROP` accepts only TEMPLATE/QUERY; saved-query bodies stay read-only; the
   server path still requires `admin` scope. Widening the destructive surface,
   or any new CAL syntax, is a spec-level (OMS-conformance) decision.
4. **Error codes are append-only** — every user-facing error carries a stable
   `DOMAIN-Ennn` code (see `ERROR_CODES.md`). Never renumber or reuse one; add new
   codes at the end. Format/uniqueness are test-enforced.
5. **One memory = one file** — single writer per file; cross-file queries go
   through `ASSEMBLE` with facade mounts, not shared connections. Host config
   (embedder, executor limits, encryption key) is per-process and never persisted
   in the file.
6. **Dependency-light by policy** — no clap, no HTTP framework, no MCP SDK, no
   workspace-wide async runtime. Justify any new dependency in the PR.

## Before you commit — run the gate

```bash
cargo test --workspace                              # full suite must be green
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
```

- **Do NOT run blanket `cargo fmt`** — the tree is not uniformly rustfmt-clean by
  design. Format only the lines you touch; match the surrounding style.
- If you changed anything under `dejadb-core/src/format/`, also confirm the OMS
  conformance suite passes: `cargo test -p dejadb-core --test oms_conformance`.
- Fuzz the untrusted-input surfaces if you touched them:
  `cargo +nightly fuzz run deserialize_blob` (also `cal_parse`, `tool_schema_parse`).
- Security-sensitive change (server, crypto, deserialize, CLI bind)? Re-read
  `docs/security-model.md` and keep it accurate.

## Adding an error code

Every user-facing error carries a stable `DOMAIN-Ennn` code as the **leading
token of its `Display` string** — a permanent debugging handle. Codes are
**append-only**: never renumber, reuse, or repurpose one.

- Domains: `FMT` `MEM` `STO` `CRY` `VAL` `CAL` `SYS`. New subsystem with no
  fitting domain → add a 3-letter mnemonic domain to `ERROR_CODES.md` first.
- Pick the next free number in the right domain range. Put `DOMAIN-Ennn: ` at
  the front of the variant's `#[error]`/Display string; add the arm to the
  type's `code()` (except `dejadb-cal`, where the code lives *inside* the
  `#[error]` string, no separate fn). Add a row to `ERROR_CODES.md`.
- Source of truth: `DejaDbError`/`SchemaSubsetError` (`dejadb-core/src/error.rs`)
  and `CalError` (`dejadb-cal/src/errors.rs`).
- Keep the tests green and extend their representative-variant lists:
  `dejadb-core`'s `error_code_tests`; `dejadb-cal`'s `test_error_codes_match_display`
  and `test_all_error_codes_have_unique_codes`.

## Touching store hot paths — run the perf gates

If you changed recall, the dictionary/index layer, or anything on the add/recall
path in `dejadb-store`, confirm you did not regress the latency budgets:

```bash
cargo run --release -p dejadb-store --example bench       # recall p50 < 200µs, latest < 100µs
cargo run --release -p dejadb-store --example voice_loop  # holds a 50ms frame cadence
```

`voice_loop` spin-waits rather than sleeps and runs `index_text: false` (FTS
costs ~150ms/write). If a gate regresses, treat it as a real failure, not noise.

## Where things live

`dejadb-core` (format/grains) ← `dejadb-store` (Turso store) ← `dejadb-cal`
(query language) ← `dejadb-context` (rendering); `dejadb-mcp`, `dejadb-server`,
`dejadb`, `dejadb-py` sit on top. See `AGENTS.md` and `ARCHITECTURE.md`.
