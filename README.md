# DejaDB

> English · [中文](README.zh-CN.md)

**The embedded memory engine for AI agents** — memory that doesn't rot, stays
current, and proves where every fact came from — plus **Waiser**, the built-in
loop that improves it: governed, evidence-cited, undoable, measured.

[![CI](https://github.com/AreevAI/dejadb/actions/workflows/ci.yml/badge.svg)](https://github.com/AreevAI/dejadb/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/rustc-1.90%2B-blue.svg)](#install)

*Named for **déjà vu** — French for "already seen." That's what your agent's
memory is for: recognizing what it has encountered before.*

Embed it in-process, store memories as immutable content-addressed grains, query
them with CAL (the Context Assembly Language), and hand the results straight to a
model — no server, no sidecars, no network hop in the recall path. **Recall in
microseconds** — fast enough to run inside a real-time **voice agent's** turn,
where a network memory call can't. **Your agent's memory is a file you own.**

> git for your agent's memory: log, diff, time-travel, forks with explicit
> merges, and encrypted incremental sync — built into the data model, because
> grains *are* content-addressed immutable objects.

*Status: `1.0.1` — the `.mg` format and CAL are stable and documented (conformant
with the Open Memory Spec, OMS).*

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
- **Self-improvement with governance — [Waiser](#waiser--governed-self-improvement-built-in),
  built in**: eleven deterministic analyzers turn the agent's own history into
  recommendations — *"this tool failed 71% of its calls"*, *"these two facts
  contradict"* — each citing the grains it was computed from, gated
  propose → review → apply → verify, undoable, and re-measured after apply.
  Zero model calls required; attach an LLM and its findings are grounded
  against the evidence and independently verified before a human ever sees
  them.
- **CAL-native**: `RECALL` / `ASSEMBLE` / `EXISTS` / `HISTORY` / `ADD` /
  `SUPERSEDE` — a query language with no bulk destruction: `DELETE` and `DROP`
  are not tokens in the grammar, and the one destructive statement —
  `FORGET <hash>`, a single-grain tombstone — is gated and can be disabled
  per process.
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

Embedding the store in a Rust project? Add the library crates instead of the CLI:

```bash
cargo add dejadb-store dejadb-core
```

> **npm on Windows:** the `dejadb-win32-x64-msvc` prebuilt binary isn't on npm yet
> (package name under review); macOS and Linux install cleanly today, and Windows
> resolves automatically once it publishes. `pip install dejadb` and
> `cargo install dejadb` already work on Windows.

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

Reflection (deriving the lessons) is your model call — DejaDB never runs an
LLM. What it guarantees: revised lessons replace instead of co-ranking, every
lesson links back to the experience that taught it (`derived_from`),
synced/replayed writes can't double-store, and a bad episode rewinds with
point-in-time restore (checkpoint first — the recipe shows the flow). Even a
*paraphrased* re-learning is caught: `deja novelty` reports the nearest existing
lesson so the harness supersedes it instead of adding a near-duplicate
(advise-only — it never drops a write itself). Full loop:
[cookbook §10](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn--by-hand).

### Waiser — governed self-improvement, built in

The section above is the loop *by hand*. **Waiser** governs it: it turns your
agent's history into recommendations — evidence-cited, reviewable, undoable,
measured — starting with **zero model calls**. The fastest way to see it needs
no agent and no waiting:

```python
import dejadb, json
db = dejadb.DejaDB("proof.db", actor="user:me")
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)
db.waiser_run()                                             # deterministic; never gated when bare
for r in json.loads(db.recommendations('{"status":"pending"}')): print(r["severity"], r["summary"])
# → high  Tool "stripe_refund" failed 5 times (71% of calls): rate_limited
db.apply_recommendation(<hash>, because="retries belong in the client")   # audited, undoable
```

What that buys you:

- **Your agent stops repeating what fails.** Eleven deterministic analyzers
  (ten default-on) cluster recurring tool failures into lessons, catch
  duplicate and contradictory facts, flag stale grains, and surface forks —
  computed over typed grains, never raw prose. With the recall-telemetry
  sidecar on, three of them see memory *utility*, not just hygiene: facts
  never recalled (`cold_grains`), questions that keep coming back empty
  (`coverage_gap`), context budgets overflowing (`budget_pressure`).
  Precision is measured, not asserted: 1.00 on the labeled fixture,
  CI-gated at 0.90 (`cargo run -p dejadb-bench --bin waiser_precision`).
- **Nothing changes behind your back.** Four gates — propose → review →
  apply → verify — with separation of duties, a **mandatory reason** on every
  decision, a hash-chained audit grain per transition, and a stored inverse
  for every apply. Auto-apply is off unless a host policy file explicitly
  grants it, and never for destructive or LLM-originated changes.
- **It proves whether its own advice worked.** A recommendation that carries
  a metric is re-measured after you apply it — at 1d / 7d / 30d checkpoints,
  against what actually happened (did that tool failure recur?); a late
  regression proposes a revert. `deja waiser outcomes` is the receipt.
- **Add an LLM for what determinism can't see — verified, never trusted.**
  `deja waiser run --model claude-sonnet` (or `openai:gpt-5`,
  `ollama:llama3.1`, any OpenAI-compatible endpoint, or `--llm-cmd 'CMD'`)
  lets a model discover cross-fact issues like a semantic contradiction — but
  every draft must ground against the cited grains and survive an
  **independent verifier** (the proposer never grades itself) before it
  reaches the queue, and `origin = llm` can never auto-apply. "Nothing to
  report" is a first-class answer, so it doesn't invent findings to look busy.
- **It runs where you already run things — no daemon.** A cheap, idempotent
  command with watermark gates (`--min-new`, `--if-stale`): a Claude Code
  `SessionEnd` hook, cron, CI (`deja waiser list --fail-on high` exits 2 —
  a build gate), or the `dejadb_waiser` MCP tool. And the loop closes *into*
  the agent: `deja recall-hook --with-waiser` rides the pending queue into
  the context Claude Code injects, so the agent sees its own recommendations
  without polling. The console (`deja ui`) shows the queue, recall sessions,
  and measured outcomes.

From a fresh install: `deja init --db demo.db --template demo` seeds a demo
corpus, `deja waiser run` proposes across analyzers (`deja waiser reflect`
sweeps the whole memory), and the Waiser tab in `deja ui` is the governed
review queue. Full guide: [docs/waiser.md](docs/waiser.md) · why the LLM layer
is verified, never trusted: [docs/waiser-reflection.md](docs/waiser-reflection.md).

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
m.cal('RECALL facts WHERE subject = "john"')
m.memory_tool(json.dumps({"command": "view", "path": "/memories"}))  # Anthropic memory-tool backend
```

### Node

```js
const { DejaDb } = require('dejadb')

const mem = new DejaDb('john.db', 'caller')                  // 3rd arg: passphrase for AES-256 at rest
mem.addFact('john', 'prefers', 'tea', 0.95)
mem.recall('john')                                           // JSON string, newest-first
mem.cal('RECALL facts WHERE subject = "john"')
mem.memoryTool('{"command": "view", "path": "/memories"}')  // Anthropic memory-tool backend
```

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

## Documentation

| Doc | For |
|---|---|
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | How DejaDB works: grains, `.mg` format, CAL, recall, sync |
| [`docs/waiser.md`](docs/waiser.md) | Waiser — governed self-improvement (analyzers, four gates, policy, CLI/bindings/MCP/API) |
| [`docs/waiser-reflection.md`](docs/waiser-reflection.md) | The reflection engine — how LLM proposals are grounded, verified, and measured |
| [`docs/cal-reference.md`](docs/cal-reference.md) | The CAL query language reference |
| [`docs/mcp-reference.md`](docs/mcp-reference.md) | The MCP server + its 8 tools |
| [`docs/migrate.md`](docs/migrate.md) | Importing from mem0, Zep, Letta, LangMem, Basic Memory, JSONL |
| [`docs/memory-tool.md`](docs/memory-tool.md) | The Anthropic memory-tool backend (Python / Node / CLI) |
| [`docs/cookbook.md`](docs/cookbook.md) | Task-oriented recipes |
| [`FAQ.md`](FAQ.md) | Questions & answers (also LLM-friendly) |
| [`SECURITY.md`](SECURITY.md) · [`docs/security-model.md`](docs/security-model.md) | Security policy & threat model |
| [`AGENTS.md`](AGENTS.md) · [`llms.txt`](llms.txt) | For AI agents working in / with this repo |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | How to contribute (DCO sign-off) |

## Security & privacy

DejaDB is local-first and collects no telemetry. Optional **AES-256-GCM
encryption at rest** protects the database (key derived from a passphrase via
Argon2id); deleting a memory is a tombstone or **crypto-erasure**. The web
console binds loopback with no auth by design and refuses to expose itself to the
network without an explicit opt-in.

Read the honest [threat model](docs/security-model.md) before deploying beyond a
local machine, and report vulnerabilities per our [security policy](SECURITY.md)
— **please don't open public issues for them**.

## Workspace

| Crate | What |
|---|---|
| `dejadb-core` | `.mg` format, canonical serialization, content addressing, 11 grain types, tool-schema rendering |
| `dejadb-store` | Turso-backed store: dictionary-encoded triples, hybrid recall, heads/forks, blobs (CAS), bundles/streaming, memory-tool adapter |
| `dejadb-cal` | CAL lexer/parser/executor, multi-source ASSEMBLE, saved queries, `DejaDbFacade` (+ read-only mounts) |
| `dejadb-context` | Budget-aware provider-optimal rendering (SML/TOON/Markdown/JSON) |
| `waiser` | The self-improvement engine — substrate-agnostic: analyzers, four gates, recommendation lifecycle, LLM verifier (no DejaDB deps) |
| `dejadb-waiser` | DejaDB substrate adapter for Waiser + the recall-telemetry sidecar |
| `dejadb-llm` | Out-of-box LLM backends for Waiser reflection (OpenAI-compatible / Anthropic / Ollama) |
| `dejadb-mcp` | Stdio MCP server (`dejadb_recall/add/supersede/forget/remember/cal` + `dejadb_waiser/recommendations`) |
| `dejadb-server` | Local web console (memories / graph / query / Waiser queue / sessions, light + dark) + dejad hub mode (segment push/pull, bearer auth) |
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
