# Changelog

All notable changes to DejaDB are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Waiser recall-telemetry sidecar (§8).** A disposable, never-syncing
  `<file>.telemetry.db` records what recall actually surfaced — grain access,
  query outcomes, assembly-budget pressure — so Waiser can see memory *utility*,
  not just internal consistency. Encrypted under the main file's key,
  `FORGET`-scrubbed, rebuildable. Capture on the recall path is buffered and
  non-blocking (voice-loop recall p50 stays ~82µs with telemetry on). Host-only
  mode `off | aggregate | full`: `deja --telemetry`, `telemetry=` on the
  Python/Node constructors (default `aggregate`); a bare library `open()`
  records nothing.
- **Three telemetry-fed analyzers** (11 built-ins total): `cold_grains` (facts
  never recalled), `coverage_gap` (recurring questions the memory can't answer),
  and `budget_pressure` (assembly overflow, opt-in until its ASSEMBLE datasource
  is wired). `cold_grains`/`coverage_gap` are default-on at 1.00 fixture
  precision.
- **Optional LLM enrichment (§9).** `deja waiser run --llm-cmd 'CMD'` attaches a
  subprocess backend (`CommandLlm`, mirroring `--embed-cmd`) that only *adds* —
  DISCOVER proposes cited `origin=llm` drafts (never auto-applied), ENRICH adds
  a whitelisted guidance note; with no backend the stages are the identity, so
  the deterministic output is unchanged. Backends in `examples/llm/`. New error
  `WSR-E050`.
- **Console Sessions + Setup views** and `GET /api/waiser/telemetry`: visualize
  recall activity, coverage gaps, and the effective configuration.

## [1.0.1] - 2026-07-15

### Added

- **`AsyncDejaDB` — a runtime-safe handle for async callers.** DejaDB owns a
  Tokio runtime and drives every operation with `block_on`; calling the
  blocking store from inside an async runtime panics (Tokio forbids a runtime
  within a runtime). `AsyncDejaDB` owns that workaround: operations run on the
  blocking pool where `block_on` is legal, `Drop` hands teardown to a plain OS
  thread (Drop cannot await), a one-permit semaphore queues callers so N
  concurrent operations can't starve the blocking pool, `close()` awaits
  teardown, and `with()` is an escape hatch for any op not mirrored on the
  async surface. Purely additive — the blocking API is untouched, no `unsafe`,
  and `tokio` is pulled in with only `rt` + `sync`.

### Fixed

- **MSRV badge** corrected (1.82 → 1.90) to match `rust-version`; README now
  documents Rust installation.

### Packaging

- PyPI and npm release workflows (`release-pypi.yml`, `release-npm.yml`):
  abi3 wheels across the platform matrix, and per-platform napi prebuilds
  (`dejadb-<platform>`) plus the thin main package wired via
  `optionalDependencies`. The npm Windows platform package
  (`dejadb-win32-x64-msvc`) is temporarily deferred pending an npm
  name-registration review; non-Windows platforms and PyPI ship in this release.

## [1.0.0] - 2026-07-13

_The first public release. The on-disk `.mg` format and CAL syntax are stable
and OMS-conformant; content addresses and error codes are contracts from here._

### Added

- **Self-improving-agent surfaces** — a batch that makes the "memory safe to
  learn on" story reachable, not just designed:
  - *Value-level idempotent add* — `DejaDB::add_if_novel` / `deja add
    --idempotent` / `dejadb_add idempotent:true` / bindings `idempotent` flag:
    a re-add of the value already at the `(subject, relation)` head writes
    nothing and returns the existing hash (dedup by value, not just
    byte-identical replay).
  - *Advise-mode novelty gate* — `DejaDB::nearest_semantic` / `deja novelty` /
    Python·Node `nearest`: nearest existing grains to a candidate text (needs
    an embedder), so a reflection harness can supersede a paraphrase instead of
    adding a near-duplicate. Never writes; the host decides.
  - *Reverse provenance* — `DejaDB::grains_derived_from` / `deja provenance
    <source-hash>` / bindings `provenance`: every grain distilled from a given
    observation, for credit assignment and episode-scoped unlearn.
  - *Recallable experience log* — `RECALL events RECENT N` /
    `RECALL observations WHERE observer_id = X` now work (bounded recent-scan
    when there is no subject/free-text anchor), so a loop can read its own
    experience back.
  - *Auto loop wiring* — `deja hook claude-code` now prints a
    `UserPromptSubmit → deja recall-hook` (injects matching memory as context)
    alongside the `Stop → deja capture-stop` hook, and `capture-stop` records
    tool calls/results (flagging `is_error`), not just prose.
- **Namespace locking** — `deja serve --mcp --lock-ns NS` pins a session:
  per-call namespaces are ignored and CAL queries are namespace-overridden, so
  an agent can't read or write outside its partition.
- **Fork surfacing** — `deja forks` enumerates open forks (>1 live head) and
  `deja merge --subject S --relation R --object O` closes one, exposing the
  previously Rust-only heads/merge model.
- **Migration importers** — `deja migrate --from mem0 | mem0-history |
  langgraph | letta | letta-archival | zep | basic-memory | jsonl` (also
  `migrate()` in the Python/Node bindings): file-based imports that preserve
  original timestamps and provenance, replay mem0 edit history as real
  supersession chains, map Zep's bi-temporal validity onto world-time
  validity, land note-shaped sources as live memory-tool files, and skip
  already-imported records on re-runs. See `docs/migrate.md`.
- **Bulk-load fast path** — `defer_text_index()` / `rebuild_text_index()`
  drop and re-create the FTS index around bulk writes (Turso indexes existing
  rows at CREATE INDEX time), removing the ~150ms/write FTS tax from imports;
  `deja reindex` backfills and rebuilds the text index for files that turned
  `--index-text true` on after writing.
- **Host-command embedder** — `CommandEmbed` (CLI `--embed-cmd 'CMD'`
  [`--embed-model NAME`], Python `set_embedder_command`, Node
  `setEmbedderCommand`): CMD gets the text on stdin and prints a JSON vector,
  enabling vector recall on every surface with no in-engine model. Python
  additionally takes a native callback via `set_embedder(fn, model=...)`.
- **Bindings parity** — Python and Node constructors accept a `passphrase`
  (AES-256-GCM at rest, Argon2id-derived key, same rules as
  `--passphrase-env`); Node gains the Anthropic memory-tool backend
  (`memoryTool`), and both gain `openWarnings`/`open_warnings` and
  `reindexText`/`reindex_text`.
- **`embedding_text` honored on the write path** — the documented per-grain
  override now feeds both the BM25 and vector indexes (import pipelines and
  the memory-tool adapter set it), so memory-file bodies and imported prose
  are searchable; `rebuild_text_index()` and the reranker share the same
  projection.
- **Core engine (`dejadb-core`)** — the OMS `.mg` binary format with frozen
  canonical serialization, SHA-256 content addressing, all 11 grain types, and
  tool-schema rendering for 9 provider formats.
- **Store (`dejadb-store`)** — embedded Turso-backed store with dictionary-encoded
  triples, hybrid recall (structural + BM25 + vector, fused with RRF),
  heads/forks/supersession, content-addressed blob storage, git-style bundles &
  op-log streaming with point-in-time restore, and an Anthropic memory-tool
  backend adapter.
- **CAL (`dejadb-cal`)** — the Context Assembly Language: a lexer/parser/executor
  and multi-source `ASSEMBLE` with facade mounts. Narrow, gated destructive
  surface — the only destructive statement is `FORGET <hash>` (a single-grain
  tombstone), gated by `allow_destructive_ops` (on by default; disable
  per-process with `--no-destructive-ops`) and requiring the `admin` scope on
  the server path; `DELETE`/`DROP` remain non-tokens and there is no bulk erasure
  from a query. Enforced alongside query-length, nesting-depth, and result-size
  limits.
- **Context rendering (`dejadb-context`)** — budget-aware rendering to
  SML / TOON / Markdown / JSON.
- **MCP server (`dejadb-mcp`)** — a stdio JSON-RPC 2.0 server exposing
  `dejadb_recall` / `add` / `supersede` / `forget` / `remember` / `cal`.
- **Web console & sync hub (`dejadb-server`)** — a local inspection console
  (memories / graph / query) and an optional bearer-token-authenticated hub for
  segment push/pull.
- **CLI (`dejadb`)** — verbs over the engine, including `add`, `recall`,
  `search`, `cal`, `history`, `log`, `bundle`, `import`, `stream`, `restore`,
  `follow`, `verify`, `serve --mcp`, `repl`, `remember`, and `ui`.
- **Python bindings (`dejadb-py`)** — `import dejadb` via PyO3 (abi3).
- **Encryption at rest** — optional AES-256-GCM with an Argon2id passphrase-derived
  key (`--passphrase-env`); tombstone and crypto-erasure deletion paths.
- **Documentation** — architecture, CAL and MCP references, a cookbook
  (including a verified self-improving-agent recipe: experience log →
  distilled lessons → proficiency supersession chain → point-in-time
  rollback), an FAQ, agent-facing docs (`AGENTS.md`, `llms.txt`), a security
  policy, and a threat model.

### Security

- Loopback-only web console by default; non-loopback binds require an explicit
  opt-in.
- HTTP request timeouts, header/body caps, and a wall-clock request deadline.
- Iterative framing validation of untrusted `.mg` blobs (depth + allocation
  bounds) before decoding, enforced symmetrically at serialize time.
- Constant-time bearer-token comparison and traversal-safe segment filenames.
- Argon2id key derivation with zeroization of key material.
- `cargo-deny` supply-chain gate and a pinned encryption dependency.

[Unreleased]: https://github.com/AreevAI/dejadb/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/AreevAI/dejadb/releases/tag/v1.0.0
