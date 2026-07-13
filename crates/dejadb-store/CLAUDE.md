# dejadb-store

The Turso-backed store: one memory = one Turso database file. `DejaDB`
(src/lib.rs) is a **sync** facade over the async `turso` crate — it owns a
tokio current-thread `Runtime` and wraps every call in `rt.block_on`. Single
`Connection`, in-memory counters (`next_seq/next_op/next_term/hlc_last`)
loaded on open → **single-writer-per-file assumption**; hot statements are
lazily prepared and cached (`ensure_stmt`).

## Schema (SCHEMA const, lib.rs ~160)

- `terms(id, term)` — the dictionary; S/R/O strings become fixed-width ids
  (`term_id` cached forward map; `term_str` is an O(n) reverse scan).
- `grains` — `seq` PK, `hash` (content address), ns/gtype/created_at,
  s/p/o dict ids, `vf/vt` (world-time validity), `svf/svt` (knowledge-time /
  supersession), `superseded_by/supersedes`, `text` (FTS source), and the
  **immutable serialized blob**.
- "2½ permutations": `triples` with `idx_spo` + `idx_pos` (mandatory) plus a
  separate `osp` table — the "½" — written **only** when the relation is in
  `DejaDbOptions::entity_relations`. Reverse traversal (`Direction::In/Both`)
  silently finds nothing for relations outside that set.
- `entity_latest` PK(ns,s,p) — the µs point read. `heads` PK(ns,s,p,seq) —
  fork tips. `oplog(op_seq, hlc, op, hash)` — OP_ADD/OP_SUPERSEDE/OP_FORGET.
  `thread_idx` — session transcripts. `embeddings(seq, vec)`.
- `meta(k, v)` — **file-carried declarations**:
  `text_index` ("1"/"0"), `entity_relations` (sorted JSON array),
  `embedding_model`/`embedding_dim` (provenance, stamped by the first
  `set_embedder`). Bare `open()` honors these; `open_with()` re-stamps and
  records changes in `open_warnings()`; a different-dim embedder warns
  instead of mixing vector spaces. Host config is never persisted here —
  the file describes itself, the host supplies capabilities.
  `tests/meta_tests.rs` covers persistence/reconciliation.
- CAS blob sidecar at `"{path}.blobs"`, git-style `hex[..2]/hex[2..]` fan-out:
  `put_blob` (idempotent, tmp+rename), `get_blob` (re-verifies sha256),
  `gc_blobs` (ref-count from live grains' `content_refs`).

## Core invariants

- **Blobs are immutable.** `supersede` and `forget` mutate the index layer
  only (`svt`, `superseded_by`, head recompute); stored blobs never change.
- Double-supersede of the same head → `SupersessionConflict` error locally;
  the same event arriving via import becomes a **fork** instead.
- Unknown terms short-circuit to empty results, never errors.
- HLC = `now_ms() << 16`, monotone, restored from `MAX(hlc)` on open.

## Forks / heads / merge (the "grains as git" model)

- Local add collapses the head (DELETE+INSERT into `heads`); **import UNIONs**
  (`insert_blob`), which is what creates forks.
- `apply_supersede_flip`: old grain already superseded by a *different* grain
  → keep both tips as heads. Deterministic provisional head everywhere =
  max `(created_at, hash)` tuple — zero coordination, same answer on every
  node. `heads()` orders provisional-first.
- `merge_heads` requires ≥2 tips, records all `merge_parents` in `context`
  (inside the blob, so it replicates), supersedes every open tip.

## Hybrid recall

`recall_hybrid` = structural (`recall_seqs`) + BM25 (`search_text`, Turso FTS,
only when `index_text`) + vector (`search_vector`, brute-force
`vector_distance_cos`) fused with RRF (k0=60). **Deadline-bounded fail-open**:
legs past the budget are skipped and partial results returned — never errors.
Embeddings come from the host via the `EmbedBackend` trait (`dim`/`embed`,
installed with `set_embedder`); there is no built-in model. `CommandEmbed`
shells out to a host command per embed (text on stdin → JSON array on stdout;
CLI `--embed-cmd`, py `set_embedder_command`, js `setEmbedderCommand`) — fine
for turn-level recall, not the voice frame path.

The FTS/embed text projection is `projected_text` (lib.rs): the grain's
`embedding_text` override when present (import pipelines + memory_tool set
it), else "s r o" + top-level `content`. The write path, the reranker's
`candidate_text`, and the `rebuild_text_index` backfill all share it — keep
them in lockstep.

**Bulk loads**: `defer_text_index()` drops the FTS index (writes then skip the
~150ms/txn FTS tax; the `text` column keeps populating), and
`rebuild_text_index()` backfills NULL `text` from blobs and re-creates the
index — Turso indexes all existing rows at CREATE INDEX time (ms, not
per-row). Crash-safe: open's `CREATE INDEX IF NOT EXISTS` self-heals.
`tests/text_index_tests.rs` pins the flow.

`recall_hybrid` delegates to `recall_hybrid_tuned(.., RecallTuning)`, which
adds the opt-in post-fusion refinements (all default off, all fail-open,
pool-capped at `REFINE_POOL`=64):
- **query expansion** (Tier-1): rule-based query variants → extra BM25 legs,
  RRF-fused. `QueryExpander` trait; built-in `EnglishExpander` (synonyms +
  naive stemming, English-only) when none installed via `set_query_expander`.
- **rerank** (Tier-2): a host-installed `RerankBackend` (`set_reranker` —
  same seam shape as `EmbedBackend`, no in-engine ML dep) re-scores the
  candidate pool's text; takes precedence over MMR.
- **diversity** (Tier-1): greedy MMR (`lambda·rel − (1−lambda)·max_sim`) over
  embedded candidates, using `vector_distance_cos` for both query-relevance
  and pairwise similarity; needs an embedder, silently skipped otherwise.

CAL reaches these via the already-ported `WITH diversity|rerank|
query_expansion` options (executor → `RecallParams` → `DejaDbFacade` →
`RecallTuning`). Covered by `tests/recall_tuning_tests.rs` (store) and
dejadb-cal's `tests/recall_tuning_cal_tests.rs` (end-to-end).

## Bundles / sync

`BUNDLE_MAGIC = b"MGB1"`. `bundle_since(cursor)` exports op-log records
(`op·hlc·hash·len·blob`; forgotten grains have len 0). `import_bundle_until`
replays idempotently in op order; its `max_hlc` filter is point-in-time
restore. `changes_since` is the follow/pull cursor primitive. Streaming
("generations", `deja stream/restore/follow`) is CLI-level orchestration of
these same calls — there is no separate segment abstraction in this crate.

## memory_tool.rs

Anthropic memory-tool backend: `view/create/str_replace/insert/delete/rename`
over a `/memories/...` path space. Each file = a supersession chain of Fact
grains (`relation="memory_file"`, body in `context.content` so the term
dictionary never stores file bodies; body also mirrored into `embedding_text`
so files reach the BM25/vector legs). Every edit is a supersession; delete
forgets the whole chain; path traversal is rejected.

## migrate.rs

File-based importers from other memory systems (mem0 incl. history→
supersession replay, langgraph/langmem, letta + letta-archival, zep/graphiti
with bi-temporal validity, basic-memory notes → `memory_file` chains, generic
jsonl). Conventions: original timestamps in `created_at`, `source_type =
"import"`, provenance in `context.import`, prose in `context.content` +
capped `embedding_text`; re-runs skip what's already there (content-address
probe / chain-existence check). `migrate_payload` is the bindings' string
dispatcher and wraps the load in defer/rebuild_text_index; the CLI dispatcher
(`run_migrate` in dejadb-cli) adds the basic-memory vault walk.
`tests/migrate_tests.rs` + dejadb-cli `tests/migrate_smoke.rs` gate it.

## Turso gotchas (documented in-code)

- `experimental_index_method(true)` is required at open.
- FTS costs ~150ms per write txn once the index exists — even for NULL text.
  Voice/edge profile runs `DejaDbOptions { index_text: false }` (see
  `examples/voice_loop.rs`).
- `PRAGMA integrity_check` miscounts experimental FTS internals; `verify()`
  classifies `__turso_internal_fts` lines as benign `fts_notes`. The real
  tamper check is the per-blob content-address re-hash.

## Tests & benches

`cargo test -p dejadb-store`. All tests use `tempfile::TempDir`.
- `store_tests.rs` — add/recall/supersede/forget, graph ops, `entity_at`
  both axes, reopen persistence.
- `fork_merge_tests.rs` — fork → provisional head → merge (uses **fixed**
  `created_at` values to make the tiebreak deterministic — copy that pattern).
- `fts_hybrid_tests.rs` — RRF ranking, zero-deadline fail-open.
- `multilingual_vector_tests.rs` — `TrigramEmbed` test backend, EN/AR/ZH.
- `bundle_blob_tests.rs` — CAS + bundle replication.
- `memtool_remember_tests.rs` — memory-tool cookbook flows, `remember()`.

Benchmarks: `cargo run --release -p dejadb-store --example bench` (latency
gates: recall p50 < 200µs, latest < 100µs) and `--example voice_loop`
(50ms frame cadence; spin-waits rather than sleeps).
