# Changelog

All notable changes to DejaDB are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`deja recall-hook --with-waiser`** — the UserPromptSubmit hook now closes
  the loop *into* the agent's context: after the memory block it appends a
  compact pending-recommendation queue (severity + summary, capped at 3,
  `origin=llm`/external entries labeled). `deja init` and `deja hook
  claude-code` print the flag in their snippets. Flagless behavior unchanged.
- **Contradiction-recurrence metric** — an applied contradiction resolution is
  now re-measured at the 1d/7d/30d checkpoints (does the subject again hold
  two live values under the functional relation?); a returned conflict
  regresses and proposes a revert. `MetricSnapshot` gains optional
  `namespace`/`relation` fields (additive; older snapshots unaffected).
  Duplicate consolidation deliberately carries no metric yet: a supersession
  creates a replacement grain, so a live-grain count can't honestly measure it
  (needs a supersede-by-existing primitive).
- **Waiser bindings parity** — Python/Node gain `rollback_recommendation`,
  `waiser_outcomes`, and `waiser_run(full_sweep=…, policy=…)`: the full-memory
  `reflect` semantics and the host policy file (the only auto-apply path) are
  now reachable from the bindings.
- **Host policy on every run surface** — `deja ui --policy` and
  `deja serve --mcp --policy` (or `$WAISER_POLICY`) attach the same
  `waiser-policy.json` the CLI takes, so console- and MCP-triggered runs honor
  one set of grants; never controllable by a client. The console's Waiser tab
  states it; the `dejadb_waiser` tool description no longer implies the CLI
  and MCP engines are identical (LLM reflection remains CLI-only).
- **`examples/analyzers/`** — a ready-to-run external command analyzer (a PII
  scan in dependency-free Python) with the probe/analyze protocol documented
  inline; validated live against the demo corpus.
- **`waiser_reflection` results table in RESULTS.md** — the Effective-
  Reliability machinery numbers (verifier lifts ER +0.00 → +1.00 on the
  reference corpus) are now recorded alongside the analyzer-precision table.

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
  and `budget_pressure` (assembly overflow). All default-on (`budget_pressure`
  once its ASSEMBLE overflow datasource was wired — see below);
  `cold_grains`/`coverage_gap` at 1.00 fixture precision.
- **Optional LLM enrichment (§9).** `deja waiser run --llm-cmd 'CMD'` attaches a
  subprocess backend (`CommandLlm`, mirroring `--embed-cmd`) that only *adds* —
  DISCOVER proposes cited `origin=llm` drafts (never auto-applied), ENRICH adds
  a whitelisted guidance note; with no backend the stages are the identity, so
  the deterministic output is unchanged. Backends in `examples/llm/`. New error
  `WSR-E050`.
- **Console Sessions + Setup views** and `GET /api/waiser/telemetry`: visualize
  recall activity, coverage gaps, and the effective configuration.
- **Waiser reflection verifier + measurement** (design:
  `docs/waiser-reflection.md`). The LLM path is no longer "cite a real hash and
  hope": DISCOVER runs under an abstention-legitimate objective, then every
  draft passes an independent **GROUND** (evidence-entailment) and **VERIFY**
  (adversarial keep/kill) gate — each a separate call (proposer ≠ scorer) —
  before it can reach the review queue, stamped with the verifier's calibrated
  confidence. Measured, not asserted: a `waiser_reflection` Effective-Reliability
  bench (the verifier lifts ER from +0.00 to +1.00 on the reference corpus by
  filtering decoys) and a live approval-rate metric on `deja waiser`.
- **Out-of-box LLM providers** (`dejadb-llm` crate): `deja waiser run --model
  claude-sonnet` (or `openai:gpt-5`, `ollama:llama3.1`) attaches a built-in
  backend — OpenAI-compatible (covers ~90% of providers incl. Gemini's compat
  endpoint, Groq, OpenRouter, vLLM, LM Studio, llama.cpp), Anthropic, or Ollama
  — over a small blocking HTTP client, key read from the environment. `--llm-cmd`
  remains the zero-dependency escape hatch. Core crates stay serde-only; the HTTP
  surface is isolated to this opt-in crate. Structured output is
  **schema-constrained** per stage (OpenAI/compat `json_schema` strict, Ollama
  native `format`) with a `json_object` fallback; prompt caching is transparent
  on OpenAI/OpenRouter and explicit (`cache_control`) on Anthropic; an
  `openrouter:` shortcut reaches many models with one key. `--model` / `--llm-cmd`
  are also exposed on the Python and Node `waiser_run`.
- **budget_pressure is now default-on**: the ASSEMBLE budget allocator records
  overflow (grains dropped to fit the token budget) via
  `CalStoreFacade::note_assembly_budget`, feeding the analyzer's telemetry.
- **Reflection quality**: the DISCOVER stage now receives the operator's recent
  approve/reject decisions (taste history) so the model learns what this reviewer
  accepts.
- **Non-parasitic evidence bundle** — DISCOVER seeds its bundle from deterministic
  citations *and* recent grains (since the last-run watermark), so the LLM gets
  its own lens and finds issues no analyzer flagged. Validated end-to-end with a
  real model: a hidden cross-fact inconsistency (each fact individually
  well-formed) is proposed, grounded, verified, and queued; a consistent corpus
  abstains. Three pipeline fixes made it discriminate: **GROUND** now checks a
  finding's factual *premises* (anti-fabrication) while allowing an inference (so
  semantic findings aren't rejected for not stating their conclusion verbatim);
  **VERIFY** judges soundness + abstention only, never novelty (a weak verifier
  hallucinated "already known" and killed genuine findings); novelty stays a
  DISCOVER concern settled by human review, not an over-coarse entity dedup.
- **Pluggable grounding backend** (`--ground-model` / `--ground-cmd`, and
  `ground_*` on the bindings): run the GROUND entailment check on a cheaper or
  specialized model — or take the generative model out of grounding entirely.
  Falls back to the reflection backend; VERIFY always stays on the main model.
- **External command analyzers** (`--analyzer-cmd`, `analyzer_cmd` on the
  bindings): a subprocess receives a live-grain snapshot and returns advisory
  findings — trust class `command`, auto-apply `never` (surfaces, never mutates).
  The only custom-analyzer path for Python/Node. A failure skips the analyzer,
  never the run.
- **Full-memory reflection sweep** (`deja waiser reflect`): re-analyze the whole
  memory in one pass, ignoring the incremental watermark, for a first look at an
  imported memory or a periodic deep pass. Dedup/cooldowns still suppress what is
  already queued and the watermark still advances, so later runs stay incremental.
- **Writable console Setup**: toggle analyzers on/off from the console, persisted
  to the file's waiser config (`POST /api/waiser/config`, Admin-gated like every
  write). `GET /api/waiser/analyzers` now returns effective settings + trust
  class. Auto-apply is still only grantable via a host policy file, never the UI.

### Fixed

- **Auto-apply now enforces the exact-equality shape check** duplicate_sweep's
  docs promised: a granted consolidation auto-applies only when every
  SUPERSEDE replacement is value-identical (case-fold; `namespace` against the
  grain's own) to the grain it supersedes. Previously a near-duplicate
  *observation* consolidation (Jaccard ≥ 0.9 — a body rewrite) could
  auto-apply under a `duplicate_sweep` grant; it now always stays pending for
  human review.
- **Analyzer writes carry their namespace.** The consolidation/resolution
  replacement grains and the tool-failure lesson previously omitted
  `namespace`, so applying them moved the surviving value to the store default
  namespace — invisible to the ns-scoped recall the agent actually runs. The
  duplicate/contradiction replacements now inherit the original grain's
  namespace, and the lesson lands in the dominant namespace of its evidence
  tool calls.
- `crates/dejadb-bench/RESULTS.md` no longer claims `budget_pressure` is
  default-off (it has been default-on since its ASSEMBLE datasource was
  wired), and `examples/README.md` no longer lists the shipped `llm/`
  directory as unimplemented.

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
