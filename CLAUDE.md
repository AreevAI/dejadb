# DejaDB

Embedded memory engine for AI agents — reference implementation of OMS (Open
Memory Spec). Rust workspace of 9 crates (plus `dejadb-js`, a standalone napi
package built outside the workspace). Memories are immutable
content-addressed grains in per-file Turso databases, queried with CAL, and
rendered into model-ready context in-process (no server in the recall path).

**Status**: 1.0.0; nothing published yet (all crates inherit the workspace
version `1.0.0`, `publish = false`). `ARCHITECTURE.md` is the design source of truth —
the architecture and the numbered design decisions. `CHANGELOG.md` summarizes
what exists; `crates/dejadb-bench/RESULTS.md` has the benchmark numbers.

## Commands

```bash
cargo test --workspace            # full suite (~950 tests, fast)
cargo test -p dejadb-cal          # per-crate
cargo run --release -p dejadb-store --example bench       # latency gates
cargo run --release -p dejadb-store --example voice_loop  # 50ms-cadence gate
cargo run -p dejadb-cli -- recall --db demo.db --ns caller --subject john
```

- **Do not run blanket `cargo fmt`** — the tree is not uniformly rustfmt-clean
  (~177 files differ). Match surrounding style; format only
  the lines you touch.
- If CLI/MCP smoke tests fail with "spawn dejadb: No such file or directory":
  the cached test binary has a stale absolute path baked in via
  `CARGO_BIN_EXE_dejadb` (happens after the repo folder moves/renames).
  Fix: `touch crates/dejadb-cli/tests/*.rs` and re-run.
- CI (`.github/workflows/ci.yml`): test on ubuntu/macos/windows, clippy
  (`-D warnings`), MSRV build, `cargo doc`, coverage, Python (maturin + pytest),
  and Node (napi build + `node --test`). `security.yml` runs `cargo deny`.
  Still run tests locally before pushing.

## Workspace (dependency order)

```
dejadb-core ← dejadb-store ← dejadb-cal ← dejadb-context
                                  ↑              ↑
                    dejadb-mcp, dejadb-server, dejadb-py, dejadb-cli (binary)
```

| Crate | What | CLAUDE.md |
|---|---|---|
| `dejadb-core` | `.mg` format, canonical serialization, content addressing, 11 grain types, tool-schema rendering | yes |
| `dejadb-store` | Turso store: dictionary-encoded triples, hybrid recall, heads/forks, bundles, CAS blobs, memory-tool adapter, migration importers | yes |
| `dejadb-cal` | CAL lexer/parser/executor, ASSEMBLE, `DejaDbFacade` + mounts | yes |
| `dejadb-context` | Budget-aware SML/TOON/Markdown/JSON rendering | yes |
| `dejadb-mcp` | Stdio MCP server (see below) | — |
| `dejadb-server` | Web console + dejad hub (see below) | — |
| `dejadb-cli` | The `deja` binary (see below) | — |
| `dejadb-py` | PyO3 bindings (see below) | — |
| `dejadb-bench` | Reproducible benchmark harnesses (latency, honesty, LoCoMo accuracy) | — |
| `dejadb-js` | Node (napi) bindings — **standalone package, not a workspace member** (see below) | — |

## Cross-cutting invariants

1. **Grains are immutable and content-addressed** (SHA-256 over the whole
   `.mg` blob). Nothing ever edits a stored blob; every edit is a
   supersession, every removal a tombstone (`forget`) or crypto-erasure.
   Store code mutates the *index layer* only.
2. **Canonical serialization is frozen** (NFC, sorted keys, compact keys,
   omit-defaults). Changing it silently changes every content address and
   breaks OMS conformance — see `crates/dejadb-core/CLAUDE.md`.
3. **CAL destruction is gated, not structural** — the only destructive CAL
   statement is `FORGET <hash>` (a single-grain tombstone), gated at execution
   by `CalExecutorConfig::allow_destructive_ops` (**default on**; disable
   per-process with `--no-destructive-ops` on `deja serve`/`ui`/`cal`, or the
   MCP `dejadb_forget` tool). `DELETE`/`ERASE`/`TRUNCATE`/… remain lexer-blocked
   non-tokens, `PURGE` stays out of the text grammar, `DROP` accepts only
   TEMPLATE/QUERY, saved-query bodies stay read-only, and the server path still
   requires the `admin` scope. Don't widen the destructive surface (e.g. bulk
   PURGE, user/scope erasure) without a design + OMS-conformance decision.
4. **CAL syntax is an OMS conformance contract** — no new CAL syntax
   without a spec-level decision.
5. **One memory = one file** — the unit of erasure, sync, portability, and
   write parallelism. Single writer per file; cross-file queries go through
   ASSEMBLE with facade mounts, not shared connections. Files are
   self-describing: the `meta` table carries file-truths (`text_index`,
   `entity_relations`, embedding provenance). Bare `open()` honors them;
   `open_with()` deliberately re-stamps and reports changes via
   `open_warnings()`. Host config (embedder capability, executor limits) is
   per-process and never persisted in the file.
6. **Dependency-light by policy**: no clap (hand-rolled args), no HTTP
   framework (std `TcpListener`), no MCP SDK (hand-rolled JSON-RPC), no
   workspace-wide async runtime (store wraps a private tokio current-thread
   runtime behind a sync API). Think twice before adding a dependency.

## Error codes

Every user-facing error carries a stable `DOMAIN-Ennn` code (3-letter
uppercase domain, `-E`, digits) as the **leading token of its `Display`
string**, plus a `code()` method. Domains: `FMT` (.mg format), `MEM`
(grains + tool-schema binding), `STO` (Turso store), `CRY` (crypto), `VAL`
(input validation), `CAL` (query language), `SYS` (internal). A reported code
alone locates the variant and subsystem. **Codes are append-only** — never
renumber or reuse one. Source of truth for text is inline on `DejaDbError`
(`dejadb-core/src/error.rs`), `SchemaSubsetError`, and `CalError`
(`dejadb-cal/src/errors.rs`); the full registry + the rule for adding one is
[`ERROR_CODES.md`](ERROR_CODES.md). Format/uniqueness are test-enforced
(`error_code_tests`, `test_all_error_codes_have_unique_codes`).

## Smaller crates

- **dejadb-mcp**: 6 tools (`dejadb_recall/add/supersede/forget/remember/cal`)
  over newline-delimited JSON-RPC 2.0 on stdio, protocol rev `2025-06-18`.
  Convention: tool failures are `isError: true` *results*; only protocol
  errors are JSON-RPC errors. Notifications (no id) get no response. No
  in-crate tests — exercised by `dejadb-cli/tests/mcp_smoke.rs`, which drives
  the real binary over real stdio.
- **dejadb-server**: hand-rolled std-only HTTP/1.1, one request per
  connection. `ui` console binds loopback and is **unauthenticated by
  default**; `with_auth(token)` (CLI `deja ui --token-env VAR`) requires the
  token on **every** request — browsers via the native HTTP Basic prompt (any
  username, password = token), scripts via `Authorization: Bearer` — and a 401
  carries `WWW-Authenticate: Basic` so browsers prompt. `into_hub(token, dir)`
  is the separate hub mode: bearer auth on POSTs + `/api/segment*` only (reads
  open) for segment push/pull. Base64 for Basic is hand-rolled (no dep). Body
  cap 1 MiB. Cross-origin POSTs are rejected via Origin check (drive-by
  protection). The console is
  one embedded HTML file (`console.html`, vanilla JS): memories/graph/query
  tabs, light + dark themes, JSON tree viewer, grain inspector; design
  source of truth is the Paper file "DejaDB". Read-only `GET /api/config`
  reports effective config + file-vs-host reconciliation warnings.
  `tests/multichannel_tests.rs` is the §8 acceptance test (voice + WhatsApp +
  email sharing one memory via the hub).
- **dejadb-cli**: ~24 verbs (incl. `migrate` from other memory systems and
  `reindex`), hand-rolled `parse_args` → HashMap; global `--embed-cmd` installs
  a `CommandEmbed` for vector recall on any verb. Opens honor
  the file's meta declarations; `--index-text true|false` explicitly
  re-stamps; open warnings print to stderr.
  `hook claude-code` only *prints* the settings snippet (never writes user
  config); `capture-stop` reads Claude Code hook JSON from stdin and stores
  the last exchange as thread-indexed Events.
- **dejadb-py**: `#[pyclass] DejaDB` over `DejaDbFacade`. FFI convention:
  **scalars in, JSON strings out**; errors → `PyValueError`. abi3-py39
  cdylib; build with maturin (`build.rs` handles macOS
  `-undefined dynamic_lookup` for bare cargo builds).
- **dejadb-js**: `#[napi]` methods over `DejaDbFacade`. Same **scalars in, JSON
  strings out** convention as `dejadb-py`; native Node addon via napi-rs (not
  wasm). Standalone package — **not** a `cargo` workspace member, so
  `cargo test --workspace` skips it; CI's `node` job builds it with
  `napi build --release` and runs `node --test __test__/smoke.mjs`.

## Local artifacts (gitignored, don't commit or rely on)

`demo.db*` and `*.db/-wal/.blobs` (scratch memories), `m0-data/` (spike
outputs), `name-reservation/` (registry placeholder stubs), `target/`.

## Naming

Brand "DejaDB", CLI binary `deja` (package/crate `dejadb`), hub daemon
"dejad", Python module `dejadb`. The OMS spec itself is external (CC0); OMS
conformance is the
compatibility mechanism with other implementations.
