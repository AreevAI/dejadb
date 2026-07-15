# DejaDB FAQ

Common questions about DejaDB, in question-and-answer form. For hands-on
recipes see the [Cookbook](docs/cookbook.md); for the design see
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Basics

### What is DejaDB?

DejaDB is an **embedded memory engine for AI agents**. You embed it in your
process (like SQLite, not like a database server), store memories as immutable
content-addressed **grains** in a per-memory file, query them with **CAL**, and
hand the results straight to your model — no network hop in the recall path. It
is the reference implementation of the [Open Memory Spec (OMS)](https://github.com/openmemoryspec/oms).

### Who is DejaDB for?

Anyone building an agent, assistant, or LLM app that needs durable, inspectable
memory: chat assistants that should remember users across sessions, voice agents
that can't afford a network round-trip per turn, coding agents that accumulate
project knowledge, and fleets that need to distribute shared knowledge to many
edges.

### What languages / platforms does it run on?

DejaDB is written in Rust and ships as a Rust library, the `deja` command-line
binary, an MCP server, and Python (`import dejadb`) and Node
(`require('dejadb')`) bindings. It runs anywhere Rust and Turso run — Linux,
macOS, and edge devices.

### Is it free? What license?

Yes. DejaDB is open source under **MIT OR Apache-2.0** (your choice). The OMS
specification it implements is CC0 (public domain). See
[`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE).

## Concepts

### What is a "grain"?

A grain is the atomic unit of memory — one immutable, content-addressed record.
Its address is the SHA-256 hash of its entire serialized `.mg` blob, so
identical content always yields the same address, and any change produces a new
grain. There are 11 grain types: **Fact, Event, State, Workflow, Tool,
Observation, Goal, Reasoning, Consensus, Consent, Skill**. A `Fact` is a
subject–relation–object triple (e.g. `john · prefers · "window seat"`); an
`Event` is raw conversational content; and so on.

### What does "content-addressed / immutable" mean for edits and deletes?

Nothing ever mutates a stored grain. To **edit** a memory you *supersede* it:
write a new grain that points back to the old one. The old version stays in the
history — recall returns the current value, but the full lineage is queryable.
To **delete**, you use the host-level `forget`, which tombstones the grain in
the change log (and crypto-erasure destroys the key for encrypted data). This is
why DejaDB behaves like *git for memory*: you get an append-only log, diffable
history, and time-travel for free.

### What is OMS?

The **Open Memory Spec** — an open standard (CC0) for portable,
provenance-verified agent memory: the `.mg` binary format, canonical
serialization, content addressing, and the grain model. DejaDB is a conformant
reference implementation, verified byte-exact against the spec's test vectors.
Because the format is open, your memories are portable and not locked to DejaDB.

### What is CAL?

**CAL** (Context Assembly Language) is DejaDB's query language. Statements
include `RECALL`, `ASSEMBLE`, `EXISTS`, `HISTORY`, `ADD`, `SUPERSEDE`, and
`DESCRIBE`, plus pipeline stages like `| COUNT`. A key property: **CAL has no
bulk destruction** — `DELETE` and `DROP` are not tokens in the grammar, and
the only destructive statement is `FORGET <hash>`, a single-grain tombstone
gated at execution (disable with `--no-destructive-ops`). `ASSEMBLE` can gather memories from several sources into a single
budgeted prompt in one statement. See [`docs/cal-reference.md`](docs/cal-reference.md).

```
RECALL facts WHERE subject = "john"
RECALL facts WHERE subject = "john" | COUNT
ASSEMBLE "prompt" FROM
  policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"),
  profile:  (RECALL facts WHERE subject = "john")
```

### What is a namespace, and why "one memory = one file"?

Each DejaDB file is one self-contained memory: the unit of erasure, sync,
portability, and write parallelism (single writer per file). Within a file,
**namespaces** partition grains (e.g. `caller`, `org.policies`). Partition your
files however fits — by user, org, category, or conversation. Cross-file queries
go through `ASSEMBLE` with read-only mounts, not shared connections.

## Comparisons

### How is DejaDB different from a vector database?

A vector database stores embeddings and does similarity search; that's *one* of
DejaDB's recall legs, not the whole thing. DejaDB is a memory *engine* with a
typed grain model, immutability and full history, structural (subject/relation)
lookup in microseconds, BM25 text search, optional vector search, and provenance
— all in one embedded file you own. Vector search is optional: you bring your own
embedder through the `EmbedBackend` trait, and DejaDB works fine without one.

### How is DejaDB different from RAG?

RAG is usually "chunk documents, embed them, retrieve top-k passages." DejaDB
stores *structured, evolving memory* — facts, events, goals, consent — not
document chunks, and it tracks how each memory entered and changed over time.
You can absolutely use DejaDB as the retrieval layer of a RAG pipeline, but it
adds structure, history, provenance, and immutability that a plain vector index
does not.

### Why not just use SQLite / Postgres with a memory table?

You can build memory on a raw database, and DejaDB is in fact built on Turso (a
SQLite-compatible engine). What DejaDB adds is the parts you'd otherwise
hand-roll: an OMS-conformant immutable content-addressed format, hybrid recall
fused with RRF, supersession/history/forks/merge, a non-destructive query
language, budget-aware context rendering, an MCP server, and content addressing
for integrity. It's the memory-specific layer, not a general SQL store.

### Does it need a server or network service?

No. Recall happens in-process against a local file — that's the point. There
*are* optional networked surfaces (a local web console and a sync hub), but the
core read/write/recall path never leaves the process.

## Using it

### I'm on mem0 / Zep / Letta / LangMem — how do I switch?

`deja migrate` imports each system's export from a file (DejaDB never calls
your old provider's API), preserving original timestamps and provenance:

```bash
deja migrate --from mem0 --file export.json --history history.json --db mine.db
```

mem0's edit history replays as real supersession chains — ADD → add, UPDATE →
supersede, DELETE → forget — so your memory's pre-import evolution stays
queryable with `HISTORY`. Zep/Graphiti edges keep their bi-temporal validity;
Letta core-memory blocks and Basic Memory notes become live memory-tool files
under `/memories`; anything else imports via generic JSONL. Re-running an
import skips what's already there. Per-source export one-liners:
[`docs/migrate.md`](docs/migrate.md).

### How do I add and recall a memory from the CLI?

```bash
deja add    --db john.db --ns caller --subject john --relation prefers --object "window seat"
deja recall --db john.db --ns caller --subject john
deja recall --db john.db --ns caller --subject john --render sml   # model-ready context
```

`--ns` defaults to `shared`. See the [Cookbook](docs/cookbook.md) for more.

### How do I run a CAL query from the CLI?

```bash
deja cal 'RECALL facts WHERE subject = "john" | COUNT' --db john.db --ns caller
deja repl --db john.db          # interactive CAL shell
```

### How do I use DejaDB from Python?

`pip install dejadb` (Node: `npm install dejadb`), then:

```python
import dejadb, json
m = dejadb.DejaDB("john.db", ns="caller")
m.add_fact("john", "prefers", "tea", confidence=0.95)
print(m.recall("john"))                              # JSON string
print(m.cal('RECALL facts WHERE subject = "john"'))  # JSON string
```

The bindings follow a **scalars-in, JSON-strings-out** convention and are built
with [maturin](https://github.com/PyO3/maturin). Encryption at rest is available
through the constructor in both bindings — pass a `passphrase`
(`dejadb.DejaDB("john.db", ns="caller", passphrase="…")` in Python,
`new DejaDb("john.db", "caller", "…")` in Node); it derives an AES-256 key with
Argon2id, host-supplied and never stored in the file, exactly like the CLI's
`--passphrase-env`.

### How do I use DejaDB with Claude Code or another MCP client?

DejaDB ships a built-in MCP server on stdio. For Claude Code:

```bash
claude mcp add deja -- deja serve --mcp --db ~/.dejadb/code.db --ns claude-code
```

Any MCP client can launch `deja serve --mcp --db <file>` and get 6 tools:
`dejadb_recall`, `dejadb_add`, `dejadb_supersede`, `dejadb_forget`,
`dejadb_remember`, and `dejadb_cal`. See [`docs/mcp-reference.md`](docs/mcp-reference.md).

### Can I build a self-improving (adaptive) agent on DejaDB?

Yes — DejaDB is the memory *substrate* for the loop, not the loop itself. A
self-improvement loop is act → log experience → reflect → distill lessons →
recall them next time; the reflection step is a model call your host makes
(DejaDB runs no LLM). The store's job is making that loop safe to run
unattended, because in a learning loop memory rot **compounds** — an agent that
re-learns duplicates or keeps stale lessons gets systematically worse:

- **Replay-idempotent writes** — a synced, imported, or retried write
  collapses to one grain by content address, so distribution can't
  double-store a lesson. For a *paraphrased* re-learning (new bytes), `deja
  novelty` reports the nearest existing lesson so the harness supersedes it
  instead of adding a near-duplicate — advise-only, so a write is never
  silently dropped.
- **Supersession** — a revised lesson *replaces* the old one at recall time
  (no stale co-ranking); proficiency tracked as a supersession chain makes
  `HISTORY` the agent's measured learning curve.
- **Provenance** — every lesson records why it was written (`REASON`) and links
  to the experience that taught it (`derived_from`). `deja provenance
  <source-hash>` lists every lesson distilled from a given observation — credit
  assignment when behavior goes wrong.
- **Rollback** — a single bad lesson is superseded or forgotten by hash; a whole
  bad *episode* is unlearned precisely via `deja provenance` (forget everything
  derived from that session) or, more bluntly, rolled back with point-in-time
  restore (`--until-hlc`).

The full verified loop — experience Events, lesson facts, a proficiency chain,
act-time recall, and the rewind — is
[cookbook §10](docs/cookbook.md#10-build-an-agent-that-learns-and-can-unlearn).

### How does recall work (hybrid RRF)?

`recall_hybrid` runs up to three legs — **structural** (exact subject/relation
lookup), **BM25** full-text (when a text index exists), and **vector** similarity
(when you've installed an embedder) — and fuses their rankings with **Reciprocal
Rank Fusion (RRF)**. It is *deadline-bounded and fail-open*: a leg that misses
its time budget is skipped and partial results are returned rather than erroring.
Plain structural `recall` (no fusion) is the microsecond-class point read.
Optional post-fusion refinements (query expansion, MMR diversity, cross-encoder
rerank) are all off by default and also fail-open.

### Is it multilingual?

Yes. Structural and BM25 legs handle spaced scripts (e.g. English and Arabic);
unspaced scripts like Chinese ride the vector leg. Bring any multilingual
embedder through the `EmbedBackend` trait.

## Operations & security

### Is my data encrypted?

Optionally, at rest. Add `--passphrase-env <VAR>` to any CLI command to derive
an AES-256 key (Argon2id) from the passphrase held in environment variable
`<VAR>`; the non-secret salt is kept in a `<db>.kdf` sidecar (back it up with the
database). Important caveats: the `.blobs` sidecar for large payloads is **not**
encrypted, and encryption-at-rest uses the storage engine's AES-256-GCM, which is
an experimental Turso feature — treat it as defense-in-depth, not a substitute
for full-disk encryption. Read [`SECURITY.md`](SECURITY.md) before relying on it.

### How do I back up or sync memories?

Several ways, all built on the append-only op-log:

- **Bundle** — `deja bundle --out backup.mgb` writes a portable incremental
  backup; `deja import --bundle backup.mgb` fast-forwards another file.
- **Stream** — `deja stream --to <dir>` continuously ships op-log segments
  (Litestream-shaped, with generations); `deja restore --from <dir>` rebuilds,
  including point-in-time restore via `--until-hlc`.
- **Follow** — `deja follow --from <dir>` subscribes and applies new segments
  (fleet-wide knowledge distribution).

See the [Cookbook](docs/cookbook.md) for full recipes.

### What happens on concurrent edits — do I lose data?

No. Concurrent supersedes of the same memory become **branches (heads)** with a
deterministic provisional head, so every node agrees on the current value with
zero coordination, and both tips survive — nothing is silently lost. Find open
forks with `deja forks` and close one with `deja merge --subject S --relation R
--object O` (an explicit supersession that records all parents). Recall does not
stamp a contested marker — that would put a per-hit head probe on the
microsecond hot path — so surfacing is the explicit `deja forks` query. Note
this covers concurrent *supersedes* of one chain: two independently *added*
facts about the same subject are separate grains and both surface as current
(the engine can't structurally tell an intended multi-value from a
contradiction — that's a semantic, host-side, judgment).

### Can I inspect or edit memories in a UI?

Yes. `deja ui --db <file>` serves a local web console (memories, graph, and
query tabs, light + dark themes) at `http://127.0.0.1:7437`. It binds loopback
with **no authentication** by design — do not expose it to a network without a
TLS-terminating reverse proxy and auth in front. See [`SECURITY.md`](SECURITY.md).

### What are the latency characteristics?

Recall is in-process and fast. Measured on an Apple M4 Max: structural recall
around **30µs**, the `entity_latest` point read around **9µs**, and a
50ms-cadence voice loop with live write-back recalls at about **79µs p50 /
152µs p99** per frame. Benchmarks and methodology live in `crates/dejadb-bench`
and the `crates/dejadb-store/examples` latency gates (`bench`, `voice_loop`).

### How accurate is recall quality?

On the public [LoCoMo](https://github.com/snap-research/locomo) long-conversation
benchmark, a plain retrieve-then-read pipeline scored around 74.5% / 81.6%
retrieval hit@10 / hit@20 (with `text-embedding-3-small`) and ~54.2% end-to-end
answer accuracy at k=20. Bring your own models and embedder. Every answer and
judge verdict is committed for audit under `crates/dejadb-bench/results/`.

## Project

### Is DejaDB production-ready?

**It is `1.0.0`** — the `.mg` format, canonical serialization, CAL syntax, and
error codes are stable contracts, and the engine is built and tested (a large
test suite runs locally). Two honest caveats remain, so read
[`SECURITY.md`](SECURITY.md) before deploying beyond a trusted environment:
encryption-at-rest rides an experimental storage-engine (Turso) AES-GCM feature
and the `.blobs` sidecar is plaintext, and the network surfaces are not yet
hardened for hostile multi-tenant deployment. Safe for local and
trusted-environment use today.

### Is DejaDB stable — can the format change under me?

No. The **on-disk `.mg` format, canonical serialization, CAL syntax, and error
codes are stable as of `1.0.0`** and follow semantic versioning — a breaking
change to any of them would be a major version bump. Changing the format would
break every content address and OMS conformance, so it won't happen silently.
Because the format is an open spec, your data stays portable regardless.

### What does DejaDB deliberately *not* do?

It takes no LLM dependency: it does not run models for you. Fact extraction is
supplied by the host (you pass your extractor's output), and embeddings come from
a host-installed embedder. LLM-dependent recall options fail loudly with a clear
error rather than silently doing nothing. This keeps the engine small,
predictable, and free of hidden model costs.

### How do I contribute?

Contributions are welcome — bugs, docs, tests, code. Sign off your commits
(`git commit -s`) under the Developer Certificate of Origin, keep the test suite
green, and **don't run blanket `cargo fmt`** (the tree is intentionally not
rustfmt-clean; format only the lines you touch). Full guidelines are in
[`CONTRIBUTING.md`](CONTRIBUTING.md); working *in* the codebase is described in
[`AGENTS.md`](AGENTS.md).

### Where do I report a security issue?

Privately — please do not open a public issue. See [`SECURITY.md`](SECURITY.md)
for the disclosure process.

### Where can I learn more?

- [`README.md`](README.md) — overview and quickstart
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — design and internals
- [`docs/cal-reference.md`](docs/cal-reference.md) — the CAL query language
- [`docs/mcp-reference.md`](docs/mcp-reference.md) — the MCP tools
- [`docs/cookbook.md`](docs/cookbook.md) — task-oriented recipes
- [`ERROR_CODES.md`](ERROR_CODES.md) — the `DOMAIN-Ennn` error registry
