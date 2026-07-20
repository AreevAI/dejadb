# dejadb-bench — adoption benchmark results (first pass)

*Run: 2026-07-06 · Apple M4 Max, macOS 26.5 (laptop — commodity-CI rerun
pending, same caveat as the earlier m0 substrate spike) · all harnesses in this crate,
`--release`, workspace LTO profile · dataset everywhere: 10k facts / 800
subjects (the `bench.rs` shape), identical query workloads per surface via
seeded xorshift.*

Four benchmarks (per the design doc): frame chart, trust suite, **honesty metrics**
(§3), and the **LoCoMo self-run** (§4) — retrieval hit-rate plus LLM-judged
end-to-end answer accuracy (bring-your-own reader/judge).

## 1. Frame chart — "recall inside an audio frame"

`cargo run --release -p dejadb-bench --bin frame_chart`
(needs `cargo build --release -p dejadb` first for the MCP leg)

One retrieval op — up to 16 most-recent facts about a caller — measured over
every surface a voice developer could actually deploy. Nothing simulated:
the HTTP and MCP legs drive the real `UiServer` and the real `deja serve
--mcp` binary over real sockets/pipes.

| surface | p50 µs | p95 µs | p99 µs | p99 as % of one 50ms frame |
|---|---|---|---|---|
| A in-process `recall` (voice hot path) | 33.1 | 46.6 | 60.2 | 0.12% |
| B localhost HTTP `/api/cal` (sidecar) | 158.1 | 216.3 | 263.5 | 0.53% |
| C MCP stdio `dejadb_recall` (agent host) | 128.6 | 181.5 | 205.0 | 0.41% |
| — network memory service (reference) | — | — | — | Zep's own enterprise headline: "retrieval under 200 ms" = **400%** of one frame (vendor-stated, not measured here) |

Readings: (a) every DejaDB surface fits inside 0.6% of a frame; the
category's stated floor is 4 frames. (b) Even *our own* localhost sidecar
costs ~5x the in-process path — transport, not storage, is the latency
budget, which is the whole architectural argument. (c) MCP stdio at ~129µs
p50 is the Claude Code / agent-host number.

## 2. Trust suite — durability + integrity artifacts

`cargo run --release -p dejadb-bench --bin trust_suite` (exit 0 = all pass;
CI-gate shaped)

| artifact | result |
|---|---|
| T1 kill −9 mid-write → reopen | **PASS** — 4,858 grains survived a SIGKILL during continuous writes; `integrity=ok`, 0 hash mismatches, 0 undecodable |
| T2 tamper detection | **PASS** — attacker with file access flips 1 byte in 1 of 100 stored blobs (verified persisted via independent connection); `verify` content-address recheck reports exactly 1 mismatch |
| T3 deletion-remnant scan | **evidence, both ways** (below) |
| T4 point-in-time restore | **PASS** — 5,000-op bundle (0.7 MB); full restore 2.9s (~1,750 ops/s, integrity ok); restore-until-HLC applied exactly 2,501 ops |

### T3, the honest-erasure evidence

The same adversarial byte-scan (find a deleted secret in the raw files) run
against both engines:

- **SQLite, upstream defaults** (`secure_delete=OFF`): secret **still present**
  in the main db file after a WHERE-scoped `DELETE`. Gone only after a manual
  `wal_checkpoint(TRUNCATE); VACUUM;` — operations no application runs on a
  schedule. (Note: Apple's *system* sqlite3 ships `secure_delete=2`/FAST, which
  does scrub — that is an Apple patch, not stock SQLite behavior; the bench
  measures both.)
- **DejaDB `forget`**: recall returns nothing, the op-log records the
  tombstone — but secret bytes **still present** in the WAL at file level.
  `forget` is an auditable index-level removal, not byte erasure, exactly as
  designed.

Conclusion the suite exists to keep precise: logical deletion is not byte
erasure in *any* SQLite-lineage engine. The only honest erasure is per-file
crypto-erasure (key destruction) — now wired in the store
(`DejaDB::open_encrypted` / `open_with_passphrase`, AES-256-GCM + Argon2id)
and proven by `dejadb-store/tests/encryption_tests.rs`: reopen without the
key (or with the wrong key) is denied, and a plaintext-marker scan of the db
and WAL bytes finds no leak. The `.blobs` CAS sidecar remains plaintext
(loud open warning) — see `docs/security-model.md`.

## 3. Honesty metrics — the numbers incumbents won't publish

`cargo run --release -p dejadb-bench --bin honesty_metrics` (exit 0 = all gates hold)

Four structural properties, measured deterministically — no LLM, no network,
no competitor hosting, so anyone can reproduce them and nobody can fudge them.
Contrast column cites primary GitHub issues by number.

| metric | measured | the failure it answers |
|---|---|---|
| **M1 idempotency** | 808 byte-identical writes → **1 grain** (807 rejected on content address) | mem0 #4573: a hallucinated "User prefers Vim" stored **808×** (97.8% of a 10k store was junk) |
| **M2 staleness-rate** | 20 supersessions → recall surfaces **1** current value (0 stale), **21-deep** history retained; the same as naive appends → recall surfaces **all 21** | mem0 #5330 (stale co-ranks), #4536 (update deletes both → empty memory) |
| **M3 write-cost** | **136µs/write** amortized (7,343/s); single-add p50 **117µs** / p99 5.2ms — **0 LLM calls, 0 tokens, $0** | mem0: 2 LLM calls/write, ~$0.30–0.80 per 100-turn chat (openwalrus), 20s add (#2813) |
| **M4 provenance** | **100%** of 504 grains carry an op-log record (op + HLC + content address); derived facts trace to their source Observation; supersession lineage reconstructs | mem0 #4573: developers hand-build a `memory_sources` table to see why a memory surfaced |

Scope, kept honest:
- M1 is EXACT-duplicate collapse (identical content incl. `created_at`) — the
  property that makes bundle import / op-log replay / retried sync idempotent.
  It is NOT a paraphrase deduper; near-duplicate phrasings need a write-time
  novelty gate (roadmap).
- M2's clean recall depends on using `supersede` (the intended update path); a
  blind re-`add` of a new value co-ranks like an append-only store. The point
  is that DejaDB *has* the primitive and it costs an index-layer flip, not two
  LLM calls — the update model mem0 lacks.

## 4. Accuracy — LoCoMo self-run

`cargo run --release -p dejadb-bench --bin accuracy -- <locomo10.json> [conv_limit]`
(dataset: snap-research/locomo `data/locomo10.json`)

The DejaDB half of the LR-1 accuracy story. Every conversation turn is ingested
as an Event; each question asks `recall_hybrid` for the top-k turns and we check
whether a gold-evidence turn (LoCoMo `evidence` dia_ids) is in the set. Full
LoCoMo: **10 conversations, 5,882 turns, 1,982 answerable QAs.**

The embedder is pluggable (`EmbedBackend`), so we report both the no-API floor
and a real semantic model. hit@k = at least one gold-evidence turn (LoCoMo
`evidence` dia_ids) in the top-k.

| embedder | hit@1 | hit@10 | hit@20 | MRR@10 |
|---|---|---|---|---|
| **OpenAI text-embedding-3-small (512-d)** | **33.1%** | **74.5%** | **81.6%** | **0.465** |
| TF-IDF+bigram (no API, lexical floor) | 18.6% | 40.7% | 49.3% | 0.250 |

Real embeddings roughly double the floor; **k=20 is the chosen operating point**
(retrieval keeps climbing with k, but the reader — not recall — is the bottleneck).
This is the *retrieval* leg only (vector path), scored against a lenient "≥1
evidence turn" proxy.

Reproduce the real-embedder row (precompute once, then look up in-process):
```
python3 crates/dejadb-bench/scripts/embed_locomo.py locomo10.json cache.json 512
DEJADB_EMBED_CACHE=cache.json \
  cargo run --release -p dejadb-bench --bin accuracy -- locomo10.json 10
```

### End-to-end answer accuracy (LLM-judged) — bring your own models

The reader answers each question from the recalled turns (session dates are
included so relative time — "yesterday", "last week" — resolves to absolute
dates, which LoCoMo's temporal category requires); an LLM judge grades the answer
against gold. Reader and judge are independently swappable — `$DEJADB_LLM_CMD`
and `$DEJADB_JUDGE_CMD`, any stdin→stdout command. `scripts/openai_chat.py` is a
ready OpenAI adapter; `DEJADB_LLM_DEBUG=1` logs every (question, gold, answer,
verdict) tuple for the raw transcripts you must publish alongside any number.

```
DEJADB_EMBED_CACHE=cache.json DEJADB_TOPK=20 \
DEJADB_LLM_CMD='python3 crates/dejadb-bench/scripts/openai_chat.py gpt-4o-mini' \
DEJADB_JUDGE_CMD='python3 crates/dejadb-bench/scripts/openai_chat.py gpt-4o' \
  cargo run --release -p dejadb-bench --bin accuracy -- locomo10.json 10
```

Full run (gpt-4o-mini reader, gpt-4o judge, real embeddings, k=20, all 1,982 QAs,
2026-07-07): **54.2%**. Every question / gold / answer / judge verdict committed in
[`results/…k20….transcripts.jsonl`](results/locomo-gpt-4o-mini-k20-2026-07-07.transcripts.jsonl)
for audit.

| category | answer accuracy |
|---|---|
| single-hop | 71.2% |
| temporal | 67.9% |
| open-domain | 45.7% |
| multi-hop | 39.4% |
| adversarial | 23.5% |

A plain retrieve-then-read pipeline, cheap reader, no LoCoMo-specific tuning;
temporal resolves because session dates are fed to the reader. Caveat, kept
precise: the number depends on the reader/judge models + the retrieval above, not
the store alone — publish model ids + raw transcripts (`DEJADB_LLM_DEBUG=1`); the
LoCoMo answer key is itself ~6% wrong (dev.to/penfieldlabs). ~$0.85 / ~50 min.

**CAL + context validation** (`cargo run -p dejadb-bench --bin cal_validate` —
correctness, not score; CI-gate shaped, exit 1 on any faithfulness miss). On 16
real LoCoMo questions, every DejaDB assembly path faithfully renders the recalled
grains: `facade.recall` 16/16, CAL `RECALL…FORMAT markdown` 16/16, CAL
`ASSEMBLE…FORMAT markdown` 16/16, `ContextAssembler` 16/16. This validates the
dejadb-cal parser→executor→facade→FORMAT and dejadb-context render paths on real
data (input→expected-output). Finding: `ContextAssembler` renders each turn's date
from `Event.created_at`, so driving the reader prompt through CAL/ContextAssembler
(rather than hand-formatting) requires turns to carry their real LoCoMo session
timestamp — the wiring for the next iteration.

## 5. In-process latency gates — `dejadb-store` examples

*Rerun 2026-07-14 · Apple M4 Max, macOS 26.5.2 · `--release`, workspace LTO
profile. These are the source of the README/FAQ in-process latency figures.
They live in `crates/dejadb-store/examples`, not `dejadb-bench`.*

`cargo run --release -p dejadb-store --example bench` — 13k grains (10k facts /
800 subjects + 3k events / 150 sessions), bare `open()` (FTS index **on**):

| operation | p50 µs | p95 µs | p99 µs | target µs | verdict |
|---|---|---|---|---|---|
| recall about subject (k≤16, deserialize) | 30.2 | 42.6 | 60.7 | 200 | PASS |
| `entity_latest` head (full grain) | 9.2 | 12.0 | 19.0 | 100 | PASS |
| thread_tail 20 events (deserialize) | 125.2 | 160.1 | 241.2 | 2000 | PASS |
| add single grain (full txn, **FTS on**) | 303,826 | 333,683 | 359,985 | 1000 | FAIL |

The `add` row FAILs its 1ms gate **by design of the load**: with the FTS index
live, every single-grain txn pays the ~140ms/write text-index tax (finding #1),
and single-row txns don't amortize it. This is the write path production
voice/edge deployments avoid — `DejaDbOptions { index_text: false }` (or
`defer_text_index()` for bulk loads) drops it to the tens-of-µs class; see the
honesty §3 write-cost metric (~136µs amortized) and the voice-loop write-back
below. Recall latency (the other three rows) is independent of how the data was
loaded.

`cargo run --release -p dejadb-store --example voice_loop` — 50ms-cadence loop,
FTS off (the voice/edge profile):

```
voice loop: 400 frames @50ms, 50 write-backs, wall 20.0s
frame recall  p50 79.0µs  p95 98.4µs  p99 151.9µs  (target <200µs)
write-back    p50 494.2µs p95 1085.7µs             (off audio thread in prod)
verdict: PASS
```

## Findings for the tree (from building this)

1. **FTS write tax is per-row, not per-txn as documented.** Loading 10k
   facts through `add_batch` (500/batch) with default options
   (`index_text: true`) took **1,383s** (~138ms/grain); the identical load
   with `index_text: false` takes **1.0s**. The store CLAUDE.md says "~150ms
   per write txn" — batching does not amortize it.
   **FIXED for bulk loads**: `defer_text_index()` drops the FTS index for
   the duration and `rebuild_text_index()` re-creates it afterwards — Turso
   indexes all existing rows at CREATE INDEX time (measured: 500 pre-existing
   rows indexed in ~4.5ms vs ~160ms for a single 100-row live-index txn).
   `deja migrate` does this automatically; `deja reindex` exposes the rebuild
   (including text backfill for files that flipped `--index-text true` after
   writing). The per-row tax still applies to normal live writes with the
   index present.
2. **Raw-turso autocommit writes can silently fail to persist.** The T2
   tamper write initially "succeeded" via bare `execute(UPDATE)` on a raw
   turso connection but was gone after reopen; an explicit `BEGIN`/`COMMIT`
   persists. Upstream-report candidate; also relevant to anything else that
   opens store files with raw turso.
3. **`forget` leaves the object string in the terms dictionary and WAL**
   (T3b). Fine under the crypto-erasure story, but a `terms` GC (or at least
   a docs note) would tighten it.

## Waiser analyzer precision (fixture floor)

`cargo run --release -p dejadb-bench --bin waiser_precision`

Fixture-measured precision/recall for the deterministic Waiser analyzers
(proposal §8: no invented precision — measured numbers decide default-on).
The fixture plants, per analyzer, N=6 positives (situations the analyzer
should flag) and N=6 decoys (look-alikes it must not), then runs the real
engine over the in-memory reference substrate and classifies every proposed
recommendation by its deterministic summary. On this clean fixture a correct
analyzer scores precision 1.00 (never fires on a decoy); the bin exits
non-zero if a default-on analyzer drops below 0.90, so it also guards against
regressions in CI.

| analyzer | proposed | TP | FP | precision | recall |
|---|---|---|---|---|---|
| waiser.cold_grains | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.contradiction_sweep | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.coverage_gap | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.duplicate_sweep | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.skill_stall | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.staleness | 6 | 6 | 0 | 1.00 | 1.00 |
| waiser.tool_failure | 6 | 6 | 0 | 1.00 | 1.00 |

(`waiser.goal_stagnation` is default-**off** — "stalled" is ambiguous — and
`waiser.budget_pressure`, default-on since its ASSEMBLE overflow datasource was
wired, is a single global signal; neither appears in this per-finding fixture,
and both are unit-tested separately. The two telemetry-fed fixtures,
`cold_grains` and `coverage_gap`, run over an injected telemetry snapshot in
the same harness.)

This is a **synthetic floor**, not a field number: it proves the analyzers
don't fire on obvious look-alikes and catch obvious positives. Real-world
precision needs a real telemetry + labels corpus (fork_surfacing and
outcome_review need concurrent heads / applied history and are exercised by
the crate tests, not this fixture). All seven fixture analyzers clear the
0.90 default-on bar.

## Waiser reflection — Effective Reliability (verifier machinery)

`cargo run --release -p dejadb-bench --bin waiser_reflection`

Scores the LLM reflection pipeline on a reference corpus of planted positives
(real hidden issues DISCOVER should surface) and decoys (superficially similar
but legitimate), with a deterministic mock backend so the run is reproducible
in CI. **Effective Reliability = (useful-correct − wrong) / positives** —
it subtracts for confident-wrong, so over-generation lowers it, unlike raw
precision.

| pipeline | surfaced | useful | wrong | ER | precision | recall | spurious |
|---|---|---|---|---|---|---|---|
| no verifier (accept grounded) | 6 | 3 | 3 | +0.00 | 0.50 | 1.00 | 0.50 |
| with verifier (GROUND → VERIFY → ROUTE) | 3 | 3 | 0 | **+1.00** | 1.00 | 1.00 | 0.00 |

The verifier lifts ER from +0.00 to +1.00 on this corpus by filtering the
decoys; CI guards spurious = 0 and recall ≥ 0.9. This is the **machinery
number** (mock backend, reference corpus) — it proves the pre-queue filter
discriminates, not what a given model scores in the field. A live model can be
scored with `WAISER_EVAL_MODEL` (see `waiser_reflection.rs`); the live
approval-rate of `origin=llm` findings accrues per file and prints on
`deja waiser`. A corpus-scale ER number on a labeled non-parasitic corpus is
tracked as an open follow-up in `docs/waiser-reflection.md` §6.
