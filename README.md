# DejaDB

> English · [中文](README.zh-CN.md)

**The embedded memory engine for AI agents** — memory that doesn't rot, stays
current, and proves where every fact came from — plus **Deja Loop**, built-in
governed self-improvement: evidence-cited, undoable, measured.

[![CI](https://github.com/AreevAI/dejadb/actions/workflows/ci.yml/badge.svg)](https://github.com/AreevAI/dejadb/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-blue.svg)](#install)

*Named for **déjà vu** — French for "already seen." That's what your agent's
memory is for: recognizing what it has encountered before.*

Embed it in-process, store memories as immutable content-addressed grains, query
them with CAL (the Context Assembly Language), and hand the results straight to a
model — on the default embedded backend: no server, no sidecars, no network hop
in the recall path. **Recall in microseconds** — fast enough to run inside a
real-time **voice agent's** turn, where a network memory call can't. **Your
agent's memory is a file you own.** And when the deployment has nowhere to put
a file — stateless containers, multi-instance services — the same engine runs
over a [PostgreSQL schema](#postgresql-backend-server-tier) instead, same
semantics, millisecond-class recall.

> git for your agent's memory: log, diff, time-travel, forks with explicit
> merges, and encrypted incremental sync — built into the data model, because
> grains *are* content-addressed immutable objects.

*Status: `1.1.1` — the `.mg` format and CAL are stable and documented (conformant
with the Open Memory Spec, OMS).*

## Watch the 2½-minute overview

[![DejaDB in 2½ minutes — grains, CAL, and the Deja Loop learning engine](demo/screens/video-cover.png)](https://www.youtube.com/watch?v=HqNcgkTIryQ)

Grains → context assembly → CAL → the agent loop → Deja Loop, in one animated pass.

## Screenshots

The web console — browse memories, inspect the graph, and run CAL with a live
grain inspector (click to enlarge):

<p align="center">
  <a href="demo/screens/memories.png"><img src="demo/screens/memories.png" width="320" alt="Console — memories"></a>
  <a href="demo/screens/graph.png"><img src="demo/screens/graph.png" width="320" alt="Console — graph"></a>
  <a href="demo/screens/query.png"><img src="demo/screens/query.png" width="320" alt="Console — query + grain inspector"></a>
</p>

## Why

Agent memory today is a vector store plus an extraction pipeline — and audited
deployments keep finding the same failure: the store fills with duplicates and
stale values nobody can trace. DejaDB is a different shape: an **engine you
embed**, built so memory *can't* rot silently.

- **Doesn't rot — measured, not promised**: memories are immutable,
  content-addressed grains, so byte-identical re-writes collapse to **one**
  grain; updates are supersessions, so recall returns **1 current value, 0
  stale** with the full history kept; **100%** of grains trace to when and how
  they entered. All deterministic, no LLM in the loop:
  `cargo run -p dejadb-bench --bin honesty_metrics`.
- **Safe for agents that learn**: in a self-improvement loop, rot *compounds*
  — an agent that keeps stale lessons and duplicates gets worse, not better.
  Supersession (revisions replace, never co-rank), lessons structurally linked
  to the experience that taught them, replay-idempotent sync, and
  point-in-time rollback of the memory file make the loop auditable and
  reversible:
  [build an agent that learns](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn--by-hand).
- **Self-improvement with governance — [Deja Loop](#deja-loop--governed-self-improvement-built-in),
  built in**: twelve deterministic analyzers turn the agent's own history into
  recommendations — *"this tool failed 71% of its calls"*, *"these two facts
  contradict"* — each citing the grains it was computed from, gated
  propose → review → apply → verify, undoable, and re-measured after apply.
  Zero model calls required; attach an LLM and its findings are grounded
  against the evidence and independently verified before a human ever sees
  them.
- **CAL-native**: `RECALL` / `ASSEMBLE` / `EXISTS` / `HISTORY` / `ADD` /
  `SUPERSEDE` — a query language where destruction is **shaped**: it takes a
  hash, an identity, or an age, **never a predicate**. `DELETE` isn't a token
  in the grammar; the three destructive statements each require an
  authorization grant and a recorded reason, write an audit record, and can be
  capped off per process. The right-to-erasure pair (`REPORT SUBJECT` /
  `FORGET SUBJECT`) shares one selector, so what a data-subject request
  discloses is exactly what an erasure removes — see [`docs/gdpr.md`](docs/gdpr.md).
- **Fast where it matters** (measured, Apple M4 Max): structural recall **~30µs**,
  `entity_latest` **~9µs**, 50ms-cadence voice loop with live write-back
  **79µs p50 / 152µs p99** per frame recall.
- **Hybrid recall**: structural + BM25 + vector legs fused with RRF; multilingual
  by construction (Arabic and English ride every leg; unspaced CJK rides the
  vector leg). Bring any embedder: the `EmbedBackend` trait in Rust, a callback
  in Python (`set_embedder`), or a command on every surface
  (`--embed-cmd 'my-embedder'` — text on stdin, JSON vector on stdout).
- **Distributed the git way**: op-log streaming with generations and
  point-in-time restore; pull subscriptions for fleet-wide knowledge
  distribution; concurrent edits become **branches with a deterministic
  provisional head** — surfaced, merged explicitly, never silently lost.
- **Private by design**: local-first, no telemetry; optional **AES-256-GCM
  encryption at rest** with an Argon2id-derived key; deletion is a tombstone or
  **crypto-erasure** (destroy the key, destroy the memory). See [Security](#security--privacy).
- **Model-native**: built-in MCP server, [Anthropic memory-tool backend
  adapter](docs/memory-tool.md), budget-aware context rendering (SML / Markdown /
  TOON / JSON), tool-schema rendering for 9 provider formats, Python and Node
  bindings.
- **A format you keep, with a paved road in**: the `.mg` format is fully
  documented and [OMS](https://github.com/openmemoryspec/oms)-conformant
  (byte-exact test vectors), so your memory outlives this engine — and
  [`deja migrate`](docs/migrate.md) imports what you have today from **mem0**
  (keeping its full edit history as supersession chains), **Zep/Graphiti**,
  **Letta**, **LangMem/LangGraph**, **Basic Memory**, or any store via generic
  JSONL.

## Install

DejaDB ships on all three registries — install the surface you need:

```bash
cargo install dejadb          # the `deja` CLI
pip install dejadb            # Python bindings
npm install dejadb            # Node bindings
```

No Rust toolchain? Every release also carries prebuilt `deja` binaries for
Linux (x86_64 / aarch64), macOS (Intel / Apple Silicon) and Windows x86_64:

```bash
curl -fsSL https://raw.githubusercontent.com/AreevAI/dejadb/main/scripts/install.sh | sh
```

It installs to `~/.local/bin` (`/usr/local/bin` as root; override with
`DEJA_INSTALL`), pins with `DEJA_VERSION=v1.1.1`, and verifies the download
against the release's `SHA256SUMS`. Or grab an archive straight from the
[Releases page](https://github.com/AreevAI/dejadb/releases) — handy in a
notebook, where the wheel covers the memory and the loop but `deja ui` (the
web console, including the review queue) lives in the binary.

Embedding the store in a Rust project? Add the library crates instead of the CLI:

```bash
cargo add dejadb-store dejadb-core
```

Or build from source (Rust 1.90+):

```bash
git clone https://github.com/AreevAI/dejadb
cd dejadb
cargo build --release                       # builds the `deja` binary
./target/release/deja --help
# Python bindings (maturin):  maturin develop -m crates/dejadb-py/Cargo.toml
# Node bindings (napi-rs):    cd crates/dejadb-js && npm ci && npm run build
```

## Quickstart (CLI)

Store a fact, recall it, hand it to a model — three commands, no ceremony
(`--db` is optional; it falls back to `$DEJADB_DB`, then `~/.dejadb/default.db`):

```bash
deja add    john prefers "window seat"     # subject relation object
deja recall john                           # → the stored fact, one JSON grain per line
deja recall john --render sml              # → "john prefers window seat" as a model-ready block
```

Point it at a specific file with `-d mem.db` (or `export DEJADB_DB=mem.db`).
Then explore: `deja cal '<QUERY>'` runs the query language, `deja ui` opens the
web console (http://127.0.0.1:7437), and `deja repl` is an interactive CAL shell.

### Give Claude Code (or any MCP client) persistent memory

```bash
claude mcp add deja -- deja serve --mcp --db ~/.dejadb/code.db --ns claude-code
```

`deja serve --mcp` speaks newline-delimited JSON-RPC 2.0 on stdio and works
with any MCP client — see [`docs/mcp-reference.md`](docs/mcp-reference.md).

### Already using mem0, Zep, Letta, or LangMem?

Bring your memories with you — including their edit history:

```bash
deja migrate --from mem0 --file export.json --history history.json --db mine.db
deja migrate --from basic-memory --file ~/basic-memory --db mine.db
```

mem0 history events replay as real supersession chains (ADD → add, UPDATE →
supersede, DELETE → forget) with their **original timestamps**, so `HISTORY`
shows your memory's pre-import evolution; note-shaped sources land as live
memory-tool files under `/memories`. Re-running an import skips what's already
there. Per-source export one-liners: [`docs/migrate.md`](docs/migrate.md).

### Build an agent that learns — and can unlearn

Memory rot *compounds* in a self-improvement loop: an agent that re-learns
duplicates and keeps stale lessons doesn't plateau, it gets worse. DejaDB's
write path is the safety mechanism for that loop — log raw experience,
distill lessons into facts, track proficiency as a supersession chain:

```bash
deja remember --observer executor --content "Attempt 2: isolated the tempdir per test - PASSED."
deja cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "lesson"
  SET object = "Shared tempdirs need per-test isolation." REASON "distilled from session 41"'
deja cal 'HISTORY WHERE subject = "fix_flaky_tests" AND relation = "proficiency"'  # the learning curve
deja restore --db rewound.db --from ./checkpoints --until-hlc <T>  # roll back a bad learning episode
```

Distilling the lessons is a model call, and it is yours to own: no model runs
unless you point DejaDB at one (`--model provider:name` or `--llm-cmd`, key
from the environment). Point `remember` at one and it extracts the facts for
you — stamped `verification_status="unverified"` with the model named on the
grain, after the raw text is already stored, so a hallucinated extraction is
reviewable and never costs you the source
([cookbook §9](docs/cookbook.md#9-ingest-raw-conversation-then-distill-facts)).
What the write path guarantees either way: revised lessons replace instead of
co-ranking, every lesson links back to the experience that taught it
(`derived_from`),
synced/replayed writes can't double-store, and a bad episode rewinds with
point-in-time restore (checkpoint first — the recipe shows the flow). Even a
*paraphrased* re-learning is caught: `deja novelty` reports the nearest existing
lesson so the harness supersedes it instead of adding a near-duplicate
(advise-only — it never drops a write itself). Full loop:
[cookbook §10](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn--by-hand).

### Deja Loop — governed self-improvement, built in

The section above is the loop *by hand*. **Deja Loop** governs it: it turns your
agent's history into recommendations — evidence-cited, reviewable, undoable,
measured — starting with **zero model calls**. The fastest way to see it needs
no agent and no waiting:

```python
import dejadb, json
db = dejadb.DejaDB("proof.db", actor="user:me")
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
db.loop_run()                                             # deterministic; never gated when bare
for r in json.loads(db.recommendations('{"status":"pending"}')): print(r["severity"], r["summary"])
# → high  Tool "stripe_refund" failed 5 times (71% of calls): rate_limited
db.apply_recommendation(<hash>, because="retries belong in the client")   # audited, undoable
```

What that buys you:

- **Your agent stops repeating what fails.** Twelve deterministic analyzers
  (ten default-on) cluster recurring tool failures into lessons, catch
  duplicate and contradictory facts, flag stale grains, and surface forks —
  computed over typed grains, never raw prose. With the recall-telemetry
  sidecar on, three of them see memory *utility*, not just hygiene: facts
  never recalled (`cold_grains`), questions that keep coming back empty
  (`coverage_gap`), context budgets overflowing (`budget_pressure`).
  Precision is measured, not asserted: 1.00 on the labeled fixture,
  with a 0.90 failure floor when the fixture runner is invoked
  (`cargo run -p dejadb-bench --bin loop_precision`). The reusable Effective
  Reliability arithmetic and loop correctness tests run in ordinary CI; the
  fixture binary itself is an explicit evaluation command.
- **Nothing changes behind your back.** Four gates — propose → review →
  apply → verify — with separation of duties, a **mandatory reason** on every
  decision, a hash-chained audit grain per transition, and a stored inverse
  for every apply. Auto-apply is off unless a host policy file explicitly
  grants it, and never for destructive or LLM-originated changes.
- **It proves whether its own advice worked.** A recommendation that carries
  a metric is re-measured after you apply it — at 1d / 7d / 30d checkpoints,
  against what actually happened (did that tool failure recur?); a late
  regression proposes a revert. `deja loop outcomes` is the receipt.
- **Add an LLM for what determinism can't see — verified, never trusted.**
  `deja loop run --model claude-sonnet` (or `openai:gpt-5`,
  `ollama:llama3.1`, any OpenAI-compatible endpoint, or `--llm-cmd 'CMD'`)
  lets a model discover cross-fact issues like a semantic contradiction — but
  every draft must ground against the cited grains and survive an
  **independent verifier** (the proposer never grades itself) before it
  reaches the queue, and `origin = llm` can never auto-apply. "Nothing to
  report" is a first-class answer, so it doesn't invent findings to look busy.

### Reproducible trajectories and governed corpora

The trajectory path keeps the typed evidence needed to replay or train from a
run: `record-tool-call` stores JSON arguments separately from results,
`capture-stop` preserves every ordered chat/content block, `run-manifest`
binds a run to a content-addressed configuration, and sampled ASSEMBLE
manifests record the exact included/dropped hashes plus the rendered digest.
Set `--run-id` to join full-mode recall telemetry to the same trajectory.

`deja corpus --select '<READ CAL>' [--out train.jsonl] [--recipient ID]` reuses CAL as the
authorized selector and streams OpenAI chat JSONL with tool definitions,
step-level loss weights/quality labels, elision records, and trace/model/policy/
subject-fingerprint bindings. Each export writes a replicating manifest grain
whose `related_to` edges name every source hash; `--recipient` records the
downstream trainer/model owner that must act on a stale-export notice. Later identity or retention
erasure reports which exported corpora are stale and must be retired or
re-derived; this is auditable suppression/re-derivation, not a claim that a
subject has been removed from model weights.
- **It runs where you already run things — no daemon.** A cheap, idempotent
  command with watermark gates (`--min-new`, `--if-stale`): a Claude Code
  `SessionEnd` hook, cron, CI (`deja loop list --fail-on high` exits 2 —
  a build gate), or the `dejadb_loop` MCP tool. And the loop closes *into*
  the agent: `deja recall-hook --with-loop` rides the pending queue into
  the context Claude Code injects, so the agent sees its own recommendations
  without polling. The console (`deja ui`) shows the queue, recall sessions,
  and measured outcomes.

From a fresh install: `deja init --db demo.db --template demo` seeds a demo
corpus, `deja loop run` proposes across analyzers (`deja loop reflect`
sweeps the whole memory), and the Deja Loop tab in `deja ui` is the governed
review queue. Full guide: [docs/loop.md](docs/loop.md) · why the LLM layer
is verified, never trusted: [docs/loop-reflection.md](docs/loop-reflection.md).

### Rust

Embed the store in-process. Add it to your `Cargo.toml`:

```toml
[dependencies]
dejadb-store = "1"
dejadb-core  = "1"
```

Most agent hosts are async (Tokio, axum). Use `AsyncDejaDB` there — it runs each
operation on the blocking pool and tears the store down off the async worker, so
neither a call nor a drop can panic inside a runtime:

```rust
use dejadb_store::AsyncDejaDB;
use dejadb_core::types::Fact;

let db = AsyncDejaDB::open("agent.db").await?;
db.add(Fact::new("john", "prefers", "dark mode")).await?;
let latest = db.latest("caller", "john", "prefers").await?;
```

In synchronous code (a CLI, a script, a test) use `DejaDB` directly:

```rust
use dejadb_store::DejaDB;
use dejadb_core::types::Fact;

let mut db = DejaDB::open("agent.db")?;
db.add(&Fact::new("john", "prefers", "dark mode"))?;
```

> `DejaDB` is blocking and drives its own runtime, so it must not be called — or
> dropped — from inside an async runtime. Reach for `AsyncDejaDB` in async code.

### Python

```python
import dejadb, json
m = dejadb.DejaDB("john.db", ns="caller")
m.add_fact("john", "prefers", "tea", confidence=0.95)
m.recall("john")                     # JSON string, newest-first — needs a subject
m.search("tea", k=5)                 # free text, when you don't have a subject.
                                     # BM25-only out of the box, so it matches
                                     # words that are present; install an
                                     # embedder (below) for semantic hits like
                                     # "hot drinks".
m.cal('RECALL facts WHERE subject = "john"')
m.memory_tool(json.dumps({"command": "view", "path": "/memories"}))  # Anthropic memory-tool backend
```

`DejaDB(..., index_text=False)` turns the BM25 index off for this file (a
deliberate re-stamp, reported by `open_warnings()`). That trades `search()`'s
text leg — keep it working by installing an embedder — for write latency that
stays flat as the file grows. `add_batch(...)` writes many grains in one
transaction; to load another system's export, prefer `migrate()`.

### Node

```js
const { DejaDb } = require('dejadb')

const mem = new DejaDb('john.db', 'caller')                  // 3rd arg: passphrase for AES-256 at rest
await mem.addFact('john', 'prefers', 'tea', 0.95)
await mem.recall('john')                                     // JSON string, newest-first
await mem.cal('RECALL facts WHERE subject = "john"')
await mem.memoryTool('{"command": "view", "path": "/memories"}')  // Anthropic memory-tool backend
```

Every method returns a promise — store calls run on libuv's thread pool rather
than blocking the event loop. The constructor is the exception, so opening a
file still fails at the line that opened it. **Await your writes**: promises
settle in completion order, not call order.

### PostgreSQL backend (server tier)

One memory = one file is the edge story. In stateless deployments (Cloud Run,
autoscaled containers) there is no durable disk — so the same store runs over
**one PostgreSQL schema per memory** instead, behind the non-default
`postgres` cargo feature:

```bash
cargo install dejadb --features postgres
deja add luis prefers window_seat --db 'postgres://user:pass@host/db?schema=memory_luis'
deja recall --db 'postgres://user:pass@host/db?schema=memory_luis' --subject luis
```

The bindings ship with the backend built in — the same class takes a DSN
where it takes a path:

```python
m = dejadb.DejaDB("postgres://user:pass@host/db?schema=memory_luis")
dejadb.drop_postgres_schema(url, "memory_luis")   # memory-level erasure
```

```js
const m = new DejaDb('postgres://user:pass@host/db?schema=memory_luis')
dropPostgresSchema(url, 'memory_luis')            // memory-level erasure
```

```rust
let mut m = DejaDB::open_postgres("postgres://user:pass@host/db", "memory_luis")?;
```

Identical semantics by construction — the same store logic (fork election,
supersession, op-log, BM25, hybrid recall) runs over either backend, pinned by
a conformance suite that executes the same case list against both. The
differences are deliberate and explicit:

- **Latency class**: point reads are microseconds embedded, milliseconds over
  a network. The voice frame path stays on the embedded backend by design.
- **Multiple concurrent writers per memory**: any number of app instances can
  hold handles on the same schema. Write transactions claim their id blocks
  from an in-schema counters row, which serializes them briefly — so the
  op-log stays gapless and ordered for followers, racing supersedes of one
  head produce one winner and one clean `SupersessionConflict`, and readers
  never block (MVCC). One instance can likewise hold handles to many
  memories (the schema-per-tenant shape).
- **Vectors** use [pgvector](https://github.com/pgvector/pgvector); the
  `vector(dim)` column is created when the first embedder is installed, and a
  dimension mismatch is a hard refusal rather than a degraded leg.
- **Erasure and portability** map to schema operations: `pg_dump -n <schema>`
  exports a memory, `DROP SCHEMA … CASCADE` erases one (exposed as
  `drop_postgres_schema`). Recall telemetry rides the memory's schema too.
  Page-level crypto-erasure remains a file-backend capability; encrypt at
  the deployment layer (TDE/pgcrypto) instead.
- **Right to erasure and retention** (both backends): `forget_subject`
  erases every structured reference to one identity — full history, object
  references, thread events, the dictionary entry itself — with replicating
  tombstones; `forget_older_than` is the age-based retention sweep. Both
  are host-level operations, deliberately not reachable from CAL; see
  [docs/erasure.md](docs/erasure.md) for the scope contract and the
  documented OMS deviation.
- **HA is inherited**: run it on a regionally-replicated Postgres and the
  memory inherits the failover, PITR, and backup story your ops team already
  drilled.

### Encryption at rest

```bash
export DEJADB_KEY="correct horse battery staple"
deja add --db secret.db --ns caller --subject john --relation prefers \
  --object "window seat" --passphrase-env DEJADB_KEY   # AES-256-GCM, Argon2id key
```

### Durability & fleets

```bash
deja stream  --db john.db --to  s3-mounted/john/     # continuous op-log shipping (~Litestream, grain-level)
deja restore --db new.db  --from s3-mounted/john/ [--until-hlc T]   # incl. point-in-time
deja follow  --db org-replica.db --from org-pub/     # subscribe: org knowledge → every edge
deja verify  --db john.db                            # integrity + full content-address recheck
```

One memory = one file: the unit of erasure (crypto-erase = key destruction),
sync, portability, and write parallelism. Partition by user, org, category, or
conversation — your call.

## Benchmarks

Reproducible harnesses in `crates/dejadb-bench` (accuracy, honesty, transport)
and `crates/dejadb-store/examples` (`bench`, `voice_loop` — the in-process
latency gates) — full methodology and raw data in
[`RESULTS.md`](crates/dejadb-bench/RESULTS.md); committed transcripts in
[`results/`](crates/dejadb-bench/results).

**Memory quality — [LoCoMo](https://github.com/snap-research/locomo)** (10
conversations, 5,882 turns, 1,982 QAs), a plain retrieve-then-read pipeline with
no task-specific tuning:

| retrieval leg | DejaDB |
|---|---|
| hit@10 / hit@20 — OpenAI `text-embedding-3-small` | **74.5% / 81.6%** |

End-to-end answer accuracy is **54.2%** across all 1,982 QAs (gpt-4o-mini reader,
gpt-4o judge, k=20) — a cheap, untuned reader over that retrieval, where the
reader (not recall) is the ceiling; a stronger reader lifts it. Bring your own
models (`$DEJADB_LLM_CMD` / `$DEJADB_JUDGE_CMD`) and embedder (the `EmbedBackend`
trait; the no-API TF-IDF floor still scores 40.7% hit@10). Every answer and judge
verdict is committed for audit — the category has a history of unreproducible
claims, so we publish the receipts:
[transcripts](crates/dejadb-bench/results/locomo-gpt-4o-mini-k20-2026-07-07.transcripts.jsonl)
([summary](crates/dejadb-bench/results/locomo-gpt-4o-mini-k20-2026-07-07.summary.json)).

**Memory integrity — honesty metrics** (structural, deterministic, no LLM):
byte-identical writes settle to **one grain** (idempotent import, sync replay,
and retries — paraphrase dedup is host-side); after 20 updates recall returns
**1 current value, 0 stale** with full history kept; writes cost **~136µs and
0 LLM calls** (text index off or deferred; a live FTS index adds ~140ms/write
— RESULTS.md finding #1); **100%** of grains trace to when/how they entered.
`cargo run -p dejadb-bench --bin honesty_metrics`.

**Latency** (Apple M4 Max) — the microseconds that make an embedded engine a
different shape from a memory *service*:

| recall operation | p50 | p99 |
|---|---|---|
| `entity_latest` (in-process) | **~9 µs** | — |
| structural recall (in-process) | **~30 µs** | — |
| inside a 50 ms voice frame, live write-back | **79 µs** | 152 µs |
| same recall via localhost HTTP sidecar | 158 µs | 264 µs |
| same recall via MCP stdio (agent host) | 129 µs | 205 µs |

Every surface above fits inside 0.6% of a 50 ms audio frame; the two transport
rows show the cost is the network hop, not the store — the whole argument for
embedding it.

**On edge hardware** — benchmarked on the devices themselves, not extrapolated.
A **$35 Raspberry Pi 3 B from 2016** (1 GB RAM, 1.2 GHz Cortex-A53, consumer
microSD) serves recall at **~361 µs, flat from 500 to 8,000 grains**; an
**Intel NUC8i3BEH from 2018** (i3-8109U, NVMe) does the same in **~30 µs** —
matching the M4 Max figure above, through the Python binding's FFI. Both install
with `pip install dejadb` in 16 seconds, no compiler. 16× the corpus, same
latency: a device can accumulate memory for months and answer as fast on day 200
as on day 1. The write path is the one thing to design for (bulk-load at
0.4–4 ms/grain vs 24–201 ms with a live FTS index). Clock-certified per phase,
with a projection for current Pi hardware:
[RESULTS.md §6](crates/dejadb-bench/RESULTS.md).

## Documentation

| Doc | For |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How DejaDB works: grains, `.mg` format, CAL, recall, sync |
| [`docs/loop.md`](docs/loop.md) | Deja Loop — governed self-improvement (analyzers, four gates, policy, CLI/bindings/MCP/API) |
| [`docs/loop-reflection.md`](docs/loop-reflection.md) | The reflection engine — how LLM proposals are grounded, verified, and measured |
| [`docs/cal-reference.md`](docs/cal-reference.md) | The CAL query language reference |
| [`docs/mcp-reference.md`](docs/mcp-reference.md) | The MCP server + its 16 tools |
| [`docs/migrate.md`](docs/migrate.md) | Importing from mem0, Zep, Letta, LangMem, Basic Memory, JSONL |
| [`docs/memory-tool.md`](docs/memory-tool.md) | The Anthropic memory-tool backend (Python / Node / CLI) |
| [`docs/cookbook.md`](docs/cookbook.md) | Task-oriented recipes |
| [`FAQ.md`](FAQ.md) | Questions & answers (also LLM-friendly) |
| [`SECURITY.md`](SECURITY.md) · [`docs/security-model.md`](docs/security-model.md) | Security policy & threat model |
| [`docs/gdpr.md`](docs/gdpr.md) · [`docs/erasure.md`](docs/erasure.md) | GDPR obligations → capabilities (for a DPIA), and the erasure requirement record |
| [`AGENTS.md`](AGENTS.md) · [`llms.txt`](llms.txt) | For AI agents working in / with this repo |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to contribute (DCO sign-off) |

## Security & privacy

DejaDB is local-first and collects no telemetry. Optional **AES-256-GCM
encryption at rest** protects the database and its CAS attachment sidecar (key
derived from a passphrase via Argon2id); deleting a memory is a tombstone or
**crypto-erasure**. The web console binds loopback with no auth by design and
refuses to expose itself to the network without an explicit opt-in.

Read the honest [threat model](docs/security-model.md) before deploying beyond a
local machine, and report vulnerabilities per our [security policy](SECURITY.md)
— **please don't open public issues for them**.

### Handling a data-subject request

Software can't *be* GDPR-compliant — a deployment is. What DejaDB gives you is
the mechanism, and the evidence:

```bash
deja subject-report "pat" --db memory.db --ns caller --out pat.jsonl --bundle pat.mgb
deja forget-subject  "pat" --db memory.db --ns caller --yes --because "Art. 17 request #42"
deja audit export --db memory.db --out evidence.jsonl
```

The report and the erasure run **one selector**, so what an access request
discloses is exactly what an erasure removes — including partition keys
(`pat#visit1`) and the full supersession history, and optionally prose
mentions. The `.mgb` bundle is the Art. 20 portability artifact. The audit
record names a *fingerprint* of the identity, not the identity: verifiable by
recomputation, unusable for enumeration — because an immutable, replicating
audit grain that named the subject would undo the erasure it records.

[`docs/gdpr.md`](docs/gdpr.md) is the article→capability map to lift into a
DPIA, including the deployment requirements (one hub per trust domain, TLS
proxy off-loopback, a documented archive-retention window) and the limits
stated honestly.

## Workspace

| Crate | What |
|---|---|
| `dejadb-core` | `.mg` format, canonical serialization, content addressing, 12 grain types, tool-schema rendering |
| `dejadb-store` | Turso-backed store: dictionary-encoded triples, hybrid recall, heads/forks, blobs (CAS), bundles/streaming, memory-tool adapter |
| `dejadb-cal` | CAL lexer/parser/executor, multi-source ASSEMBLE, saved queries, `DejaDbFacade` (+ read-only mounts) |
| `dejadb-context` | Budget-aware provider-optimal rendering (SML/TOON/Markdown/JSON) |
| `deja-loop` | The self-improvement engine — substrate-agnostic: analyzers, four gates, recommendation lifecycle, LLM verifier (no DejaDB deps) |
| `dejadb-loop` | DejaDB substrate adapter for Deja Loop + the recall-telemetry sidecar |
| `dejadb-llm` | Out-of-box LLM backends for Deja Loop reflection (OpenAI-compatible / Anthropic / Ollama) |
| `dejadb-mcp` | Stdio MCP server (`dejadb_recall/add/supersede/forget/remember/cal` + `dejadb_loop/recommendations`) |
| `dejadb-server` | Local web console (memories / graph / query / Deja Loop queue / sessions, light + dark) + dejad hub mode (segment push/pull, bearer auth) |
| `dejadb` | The `deja` binary |
| `dejadb-py` | Python bindings (`import dejadb`) |
| `dejadb-js` | Node bindings (napi-rs native addon, `require('dejadb')`) |

Built on [Turso Database](https://github.com/tursodatabase/turso) (MIT) — see
`THIRD-PARTY-NOTICES.md`.

## Contributing

Contributions are welcome under the [DCO](https://developercertificate.org/) — see
[CONTRIBUTING.md](CONTRIBUTING.md) and our [Code of Conduct](CODE_OF_CONDUCT.md).
Questions and ideas: [GitHub Discussions](https://github.com/AreevAI/dejadb/discussions).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. Unless you explicitly state otherwise,
any contribution you intentionally submit for inclusion is dual-licensed as
above, with no additional terms. The OMS specification itself is CC0.
