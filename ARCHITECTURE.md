# DejaDB Architecture

DejaDB is an embedded memory engine for AI agents and the reference
implementation of the **Open Memory Spec (OMS)** — the open standard for
portable, provenance-verified agent memory. It stores memories as immutable,
content-addressed *grains* in per-file [Turso](https://github.com/tursodatabase/turso)
databases, queries them with **CAL** (the Context Assembly Language), and
renders the results into model-ready context in-process. There is no server in
the recall path.

This document describes the system for developers who want to understand,
embed, or contribute to DejaDB. It covers the data model, the storage layer,
the query language, recall, versioning, context rendering, the crate layout,
and the design decisions that shape all of it.

Related references:

- [CAL query language reference](docs/cal-reference.md)
- [MCP server reference](docs/mcp-reference.md)
- [Security model & threat model](docs/security-model.md)
- [Vulnerability reporting](SECURITY.md)

---

## 1. Design goals

DejaDB is shaped by three constraints, in priority order:

1. **In-process, microsecond recall.** The flagship consumer is a real-time
   voice loop that cannot pay a network round trip. The primary interface is a
   Rust handle (`DejaDB::open(path)`); MCP, HTTP, and language bindings are
   thin layers over the same engine.
2. **Portable, verifiable memory.** Every memory is a file the user owns.
   Grains are content-addressed and immutable, so memory can be exported,
   backed up, synced, and audited without trusting any single service.
3. **Safe-by-default for agents.** The query surface's only destructive verb is
   a single-grain `FORGET`, gated by a per-process switch (default on) and
   backed by no bulk-erasure primitive — enforced by the grammar and type
   system, and fully disable-able for untrusted input.

Everything below follows from these constraints.

---

## 2. The core model: immutable content-addressed grains

A **grain** is the atomic unit of memory: one fact, one event, one state
snapshot, one tool call. Grains are:

- **Immutable.** A stored grain is never edited in place. Every "update" is a
  new grain that *supersedes* the old one; every "removal" is a tombstone or a
  cryptographic erasure. Store code mutates only the *index layer* that points
  at grains — never the grain blobs themselves.
- **Content-addressed.** A grain's identity is the SHA-256 hash of its entire
  serialized blob (header included). The address *is* the content: two
  byte-identical grains collapse to one address, and any change to a grain
  produces a different address. This is what makes memory tamper-evident and
  deduplicated by construction.

### 2.1 The `.mg` blob format

Each grain serializes to a `.mg` blob:

```
blob = 9-byte header  ++  canonical MessagePack payload
address = SHA-256(entire blob, header included)
```

The 9-byte header is fixed-width and self-describing:

| Bytes | Field | Meaning |
|---|---|---|
| 0 | `version` | Format version (currently `0x01`) |
| 1 | `flags` | Bit flags (see below) |
| 2 | `grain_type` | The grain type byte (`0x01`–`0x0B`) |
| 3–4 | `ns_hash` | First 2 bytes of SHA-256(namespace), big-endian |
| 5–8 | `created_at_sec` | Creation time, epoch **seconds**, big-endian u32 |

Flag bits: `signed` `0x01`, `encrypted` `0x02`, `compressed` `0x04`,
`has_content_refs` `0x08`, `has_embedding_refs` `0x10`, `ai_generated` `0x20`,
and bits 6–7 encode a sensitivity level. The payload carries full timestamps in
epoch **milliseconds**; the header's second-resolution timestamp is a coarse
sort/filter key.

### 2.2 Canonical serialization

Because the content address is computed over the serialized bytes, the
serialization must be **canonical** — the same logical grain must always
produce the same bytes on every machine. DejaDB freezes these rules:

- **NFC normalization.** Every string is Unicode-NFC-normalized before hashing,
  so composition variants of the same text collapse to one address.
- **Sorted map keys.** Maps are emitted in sorted key order (built as
  `BTreeMap`).
- **Compact keys.** Field names serialize to short canonical forms (a fixed
  long↔short table). A handful of fields stay uncompacted by design.
- **Omit-when-default.** `None`/empty fields and default enum values are
  omitted from the payload entirely.

These rules are a conformance contract: changing any of them would silently
change the content address of every grain ever written and break OMS test-vector
conformance. They are treated as frozen unless the spec itself moves.

### 2.3 The 11 grain types

OMS defines 11 grain types, each with a stable header byte. The type byte, the
canonical name, and the fields are part of the format contract.

| Byte | Type | Purpose | Key fields |
|---|---|---|---|
| `0x01` | **Fact** | A subject–relation–object triple: durable structured knowledge | `subject`, `relation`, `object`, `confidence` |
| `0x02` | **Event** | A conversational or system event; the transcript unit | `role`, `session_id`, `content`, `created_at` |
| `0x03` | **State** | An agent state snapshot / checkpoint | `context`, `plan`, `checkpoint_data` |
| `0x04` | **Workflow** | A DAG of steps bound to tool definitions | `nodes`, `edges`, `bindings`, `trigger` |
| `0x05` | **Tool** | A tool definition, call, or result across its lifecycle | `tool_name`, `tool_phase`, `input`, `is_error` |
| `0x06` | **Observation** | A raw observation from a sensor or observer | `observer_id`, `observer_type`, `value`, `unit` |
| `0x07` | **Goal** | A goal or task with state and dependencies | `description`, `goal_state`, `deadline`, `depends_on` |
| `0x08` | **Reasoning** | A recorded inference (premises → conclusion) | `reasoning_type`, `premises`, `conclusion` |
| `0x09` | **Consensus** | An agreement across multiple observers | `threshold`, `agreement_count`, `participating_observers` |
| `0x0A` | **Consent** | A consent / authorization record (DID-scoped) | `consent_action`, `purpose`, `grantor_did`, `grantee_did` |
| `0x0B` | **Skill** | A packaged, reusable agent capability with learned proficiency | `name`, `domain`, `proficiency`, `transferable` |

All 11 types share a common envelope (`namespace`, timestamps, provenance,
supersession links, optional content/embedding references). The type-specific
fields above are what each type adds on top.

> **Tool grains are data, never executables.** DejaDB stores, correlates, and
> renders tool definitions/calls/results — it never runs them. A Tool grain's
> `tool_phase` distinguishes a `definition` (name + input/output schema) from a
> `call` (input + correlation id) from a `result` (output + `is_error`). The
> engine can render stored definitions to nine provider tool-schema formats
> (OpenAI, Anthropic, Gemini, MCP, and text variants) for tool-RAG, but
> execution is always the host's job.

---

## 3. Storage: one memory = one file

Each memory is a single Turso (SQLite-lineage, embedded, MIT-licensed) database
file. This is the load-bearing decision that makes the rest coherent:

> **One file is simultaneously the unit of erasure, sync, portability, write
> parallelism, and retention.**

- **Erasure** is file-granular: crypto-erase a memory by destroying its key.
- **Sync/backup** operates on a file's grain stream.
- **Write parallelism** is one writer queue per file; there is no cross-file
  transaction to coordinate.
- **Portability**: a memory is one file you can copy, hand to a user, or import
  into any OMS implementation.

Applications partition memory into files along whatever boundary their domain
needs — per user, per organization, per category, per conversation. Within a
file, hot queries partition further by namespace, session, and thread. When a
session needs to span several files, it does so through
[ASSEMBLE with facade mounts](#54-assemble-and-facade-mounts), not through
shared connections.

### 3.1 The index layer

Grains are opaque immutable blobs; everything queryable is a *derived index*
maintained on write. The store keeps, among others:

- **Dictionary-encoded triple indexes.** Fact subject/relation/object strings
  are mapped through a terms dictionary to fixed-width integer ids, and stored
  as narrow permutation indexes (SPO + POS, with a selective OSP permutation
  for entity-valued objects). This is the "hexastore-equivalent" the spec
  permits — the permutations CAL's bounded traversal actually needs, rather
  than the full six.
- **`entity_latest`** — the current head(s) per `(subject, relation)`, so
  "current value of X" is a point read.
- **A full-text index** (BM25) and a **vector index** for hybrid recall.
- **A thread index** `(namespace, session_id, seq)` for transcript-tail and
  session-directory queries.
- **An op-log** with a hybrid logical clock (HLC) and tombstones — the ordered,
  replayable record that powers sync and point-in-time restore.

Because user strings are dictionary-encoded to integer term-ids before they
reach the triple queries, and all store access uses parameterized SQL, there is
no SQL-injection surface.

### 3.2 Content-addressed blob sidecar (CAS)

OMS keeps grains small (~100-byte class) and references media by URI. DejaDB
implements the reference target: a per-memory content-addressed `blobs/`
sidecar. Media is stored once, addressed by `cas://sha256:...`, deduplicated by
construction, garbage-collected by ref-count from live grains, and read back
hash-verified. Recall never scans bytes — searchability comes from *derived
text* (transcripts, extractions) stored in grain content and from embedding
references. See the [security model](docs/security-model.md) for the current
plaintext-sidecar limitation.

---

## 4. Versioning: heads, forks, supersession, tombstones

Because grains are immutable, "change" and "delete" are modeled as new state in
the index layer, never as edits.

- **Supersession.** To evolve a memory, write a new grain whose `derived_from`
  points at the old one. The store sets the old grain's index-layer
  `superseded_by` pointer and system-valid-to timestamp. The old blob is
  untouched and fully recoverable — supersession builds an append-only version
  history, and `HISTORY OF <hash>` walks it.
- **Heads.** `entity_latest` is a *heads set* per `(subject, relation)`, not a
  single row. In the common single-writer case there is exactly one head.
- **Forks.** When two writers concurrently supersede the same head (v1 → v2a
  and v1 → v2b), immutability means **both tips survive** — the conflict
  structurally cannot destroy either version. Reads never block: recall serves
  a **provisional head** that every node computes identically (HLC, then hash
  tiebreak — zero coordination). Resolution is an explicit **merge
  supersession** that records both parents and closes the fork — auditable
  forever. For an agent, cross-channel disagreement is context, not an error.
  *Surfacing:* `deja forks` enumerates every open fork and `deja merge
  --subject S --relation R --object O` closes one. Recall itself does **not**
  stamp a contested marker — that would add a per-hit head probe to the
  microsecond hot path — so surfacing is an explicit operator query, not a
  recall-time cost. The `CONTRADICTIONS` CAL clause parses but is not yet wired
  to the executor.
- **Tombstones and erasure.** Removal is never an in-place delete (which would
  leave recoverable data in free pages and the WAL). `forget` writes a
  tombstone to the op-log and drops the grain from the hot index. The strong
  erasure path is cryptographic: encrypt the memory with a per-file key and
  destroy the key.

The grain set is a grow-only structure: **adds are pure set union and have no
conflict class at all.** The only semantic conflict — concurrent supersession
of one head — resolves deterministically and surfaces as a first-class fork.

---

## 5. CAL: the Context Assembly Language

CAL is the query language and the primary API surface — it is what makes DejaDB
a database rather than a library. A CAL statement runs a pipeline:

```
text → length check → bidi rejection → NFC normalize → lex → parse
     → CalQuery (AST) → execute → pipeline stages → format → result
```

Full syntax, statement types, and safety limits are in the
[CAL reference](docs/cal-reference.md). The architectural essentials:

### 5.1 Read and write tiers

- **Read tier**: `RECALL`, `ASSEMBLE`, `EXISTS`, `HISTORY`, `DESCRIBE`,
  `COALESCE`, set operations, and a post-statement pipeline (`| SELECT`,
  `| ORDER BY`, `| LIMIT`, `| COUNT`, …).
- **Write tier**: `ADD` and `SUPERSEDE` (append-only). Every write requires a
  `REASON`/`BECAUSE` clause, so the provenance of a change is captured in the
  change itself.

### 5.2 The narrow, gated destructive surface

CAL's destructive surface is deliberately tiny and defense-in-depth gated. The
**only** destructive statement is `FORGET <hash>` — a single-grain tombstone
(`DejaDB::forget`). Everything larger is kept out, and even FORGET is gated:

1. **Lexer.** A destructive-keyword blocklist (`DELETE`, `ERASE`, `TRUNCATE`,
   `INSERT`, `CREATE`, `GRANT`, …) is rejected before tokenization. `DELETE`
   has no token in the grammar at all — the deletion verb is `FORGET`.
2. **Parser.** Those identifiers are fast-rejected with a dedicated error.
   `FORGET <hash>` parses; the bulk/scope forms (`FORGET USER/SCOPE`, `PURGE`)
   exist in the AST but the text parser still refuses them, and `DROP` accepts
   only `TEMPLATE`/`QUERY`. Saved-query bodies are re-checked read-only.
3. **Execution gate.** FORGET/DROP/PURGE execute only when
   `CalExecutorConfig::allow_destructive_ops` is set. It defaults to **on**, but
   any host can flip it off per-process (`deja serve --mcp --no-destructive-ops`,
   likewise `deja ui` / `deja cal`), yielding a read-only session in which every
   destructive statement returns `Unsupported`. On the server path, FORGET
   additionally requires the `admin` capability scope.

The same capability backs both surfaces: the Rust API, the MCP `dejadb_forget`
tool, and CAL `FORGET` all reduce to `DejaDB::forget(hash)`. Bulk erasure by
user or scope is intentionally **not** implemented — there is no store primitive
for it — so a single query cannot wipe a namespace. A CAL session can be
pinned to a namespace via `CalExecutorConfig::namespace_override` (enforced on
the server path; not yet wired to the MCP/CLI surfaces, where the caller picks
its namespace). Sensitivity is recorded per grain in the header; recall-time
enforcement of a sensitivity ceiling is host-side today. Against untrusted
input the operator can disable deletion entirely with one flag.

### 5.3 Safety limits

The parser and executor enforce hard bounds so a hostile or runaway query
cannot exhaust resources: max query length (64 KiB), max nesting depth (8), max
result limit (1000), max pipeline stages (5), max `LET` bindings (5) with a
1000-grain cap per binding, and more. The full table is in the
[CAL reference](docs/cal-reference.md#safety-limits). Two Unicode invariants run
before tokenization: bidirectional-override rejection (defeats visual spoofing)
and NFC normalization.

### 5.4 ASSEMBLE and facade mounts

`ASSEMBLE` is CAL's context-composition statement: it draws from multiple
labeled sources, applies per-source token budgets and priorities, deduplicates,
and renders a single budgeted block ready for a model prompt.

Cross-file recall goes through **facade mounts**, not shared connections. A
`DejaDbFacade` wraps one writable session store and any number of *read-only*
mounted stores:

```rust
facade.mount("org", org_replica);   // read-only
// CAL reaches the mount via the `alias.inner` namespace inside a source:
//   ASSEMBLE "prompt" FROM
//     policies: (RECALL facts  WHERE namespace = "org.policies" RECENT 10),
//     profile:  (RECALL facts  WHERE subject = "john"),
//     session:  (RECALL events WHERE session_id = "call-42" RECENT 10)
//   BUDGET 1500 tokens
//   PRIORITY profile: 0.5, session: 0.3, policies: 0.2
//   FORMAT sml
```

A namespace of the form `alias.inner` routes to the mounted store; writes only
ever hit the session store, so mounts are read-only *by construction*. This is
how a voice edge attaches local organization/category replicas and assembles a
whole prompt in one in-process statement.

---

## 6. Recall: hybrid retrieval with RRF fusion

Recall has three independent legs, fused in the engine:

1. **Structural** — indexed triple lookups (`subject`/`relation`/`object`,
   `entity_latest`, thread tail). This is the microsecond hot path and needs no
   model.
2. **Lexical (BM25)** — full-text search over grain content.
3. **Vector** — semantic similarity over embeddings.

The lexical and vector legs are combined with **Reciprocal Rank Fusion (RRF)**
in Rust, then optionally reranked. The design is deliberately degradable: with
no embedding backend installed, recall runs on structural + BM25 alone — enough
for profile and booking-style workloads, and the default for constrained
"edge" deployments where every millisecond of prefill is compute-bound.

**Embedders and rerankers are traits** (`EmbedBackend`, `RerankBackend`). DejaDB
ships no mandatory external service: bring a remote HTTP embedder, a local
model, or nothing at all. Because a memory file records its embedding provenance
(model + dimension) in its `meta` table, a mismatched embedder warns rather than
silently mixing vector spaces.

Bounded graph reads sit on the same indexes: 1-hop neighborhoods, relation-filtered
k-hop traversal, bounded shortest paths (for "why does the agent believe X"
provenance walks), and as-of temporal reads — all indexed reads at recall
latency with depth/frontier/deadline caps. This is *temporal graph reads without
a graph database*; unbounded traversal and graph analytics are deliberately out
of scope.

---

## 7. Context rendering: budget-aware, provider-optimal

The last step in the recall path is turning grains into model-ready text under
a token budget. The context layer renders to **SML, TOON, Markdown, and JSON**,
with provider presets (e.g. SML for Claude-class, Markdown for GPT-class) and
grain-type diversity floors so a budget doesn't collapse to a single type.

Rendering uses **progressive disclosure**: as the budget fills, individual
grains degrade from full form to summary to omitted (at tuned thresholds)
rather than the whole block being truncated at a byte boundary. `ASSEMBLE`'s
`BUDGET` clause drives this directly, and prompt-assembly logic can live in
named, versioned saved CAL queries — hot-swappable without redeploying the
agent.

---

## 8. Crate layout

DejaDB is a Rust workspace of 9 crates. The dependency order (foundation
first):

```
dejadb-core ──┬── dejadb-store ──┬── dejadb-cal ──┬── dejadb-context
              │                  │                │
              └──────────────────┴────────────────┴──> dejadb-mcp
                                                        dejadb-server
                                                        dejadb-py
                                                        dejadb (binary)
                                                        dejadb-bench (harness)
```

| Crate | Depends on | What it does |
|---|---|---|
| **dejadb-core** | — | The `.mg` format, canonical serialization, content addressing, the 11 grain types, and tool-schema rendering. Storage-agnostic; everything depends on it. |
| **dejadb-store** | core | The Turso store: dictionary-encoded triple indexes, `entity_latest` heads/forks, hybrid recall + RRF, bounded graph ops, the op-log + HLC + tombstones, the CAS blob sidecar, bundles/streaming, and the memory-tool adapter. |
| **dejadb-cal** | core, store | CAL lexer, parser, AST, executor, multi-source ASSEMBLE, templates, saved queries, and the `DejaDbFacade` (with read-only mounts) that binds CAL to the store. |
| **dejadb-context** | cal, core | Budget-aware rendering (SML/TOON/Markdown/JSON), progressive disclosure, provider presets, and tool-schema formats. |
| **dejadb-mcp** | cal, core, store | The stdio MCP server — six memory-semantic tools over newline-delimited JSON-RPC 2.0. See the [MCP reference](docs/mcp-reference.md). |
| **dejadb-server** | cal, context, core, store | A dependency-light HTTP/1.1 web console (loopback, no auth by default) plus an optional sync-hub mode with bearer-token auth. |
| **dejadb** | all of the above | The `deja` binary: ~27 verbs (`add`, `recall`, `cal`, `history`, `log`, `bundle`, `import`, `migrate`, `reindex`, `verify`, `serve --mcp`, `ui`, `repl`, `remember`, …). |
| **dejadb-py** | cal, context, core, store | Python bindings (`import dejadb`); scalars in, JSON strings out. |
| **dejadb-bench** | most of the stack | Reproducible accuracy and latency benchmark harnesses. |

---

## 9. Key design decisions and trade-offs

These are the decisions that most shape the system, and what they buy.

### Dependency-light by policy

DejaDB avoids heavy dependencies on principle: no CLI-args framework (arguments
are hand-parsed), no HTTP framework (the server is std `TcpListener`), no MCP
SDK (JSON-RPC is hand-rolled), and no workspace-wide async runtime (the store
wraps a private current-thread runtime behind a synchronous API). Point reads in
the microsecond class cannot afford executor hops, and a small dependency
surface is a smaller attack surface and a smaller thing to keep building for
years. Think twice before adding a dependency.

### Single writer per file

Each memory file has exactly one writer queue. There are no cross-file
transactions, so scaling out is *adding files/shards*, and the audio thread on a
voice edge never blocks on a lock. Multi-writer conflict is handled honestly by
the [heads/forks model](#4-versioning-heads-forks-supersession-tombstones)
rather than by hidden last-writer-wins.

### Host config is never persisted in the file

A memory file declares *what it physically is* — its text-index and
entity-relation settings and its embedding provenance live in a `meta` table, so
the same file behaves identically on any machine and needs no external registry
to travel. Everything else — which embedder the host can run, executor limits,
mounts, write quotas — is *host capability and policy*, supplied per process
(CLI flags, env, MCP args) and never written into the file or read from global
config by the library. Embedded behavior must be machine-independent.
Reconciliation between a file's declarations and a host's config is *loud, not
fatal*: a bare `open()` honors the file; an explicit `open_with()` re-stamps and
reports every change through open warnings.

### CAL's destructive surface is narrow and gated

The [gated destructive surface](#52-the-narrow-gated-destructive-surface) is a
first-class feature, not a footnote. In a landscape where agents have wiped
production databases, an agent-facing query language whose *only* destructive
verb is a single-grain `FORGET` — with no bulk-erasure primitive to reach for,
and a one-flag switch to make a session fully read-only for untrusted input — is
a safety property you can rely on.

### Portability and provenance over lock-in

Grains are content-addressed, immutable, and hash-linked; the format reserves
a signing flag (COSE envelope — designed, not yet implemented).
Memory exports to `.mg` and imports into any OMS implementation. `deja bundle
--since <hash>` produces incremental, resumable, tamper-evident backups to any
dumb remote (directory, rsync, S3) — end-to-end encrypted when grains and blobs
are encrypted, so the remote never reads the memory. This is *git for agent
memory*: log, diff, time-travel, forks with explicit merges, and encrypted
sync, built into the data model because grains already are content-addressed
immutable objects.

---

## 10. Deployment topology

DejaDB has no platform dependency. Three tiers cover a multi-channel fleet:

1. **Embedded** — voice and interactive edges run DejaDB in-process for
   microsecond recall, with per-caller working files and the op-log streaming
   out.
2. **Hub (`dejad`)** — an optional self-hosted daemon that owns a directory of
   memory files (one writer queue each), serves HTTP/MCP recall/add for
   latency-tolerant channels, serves subscriptions, and handles bundle
   push/pull. It shards by hashing the memory key; with no cross-file
   transactions, scaling is adding shards.
3. **Object storage** — the segment archive and restore source.

Organization/category knowledge fans out read-only to every edge via pull
subscriptions, which is what keeps a session's `ASSEMBLE` local: a session opens
the user file and attaches local org replicas as read-only mounts. See the
[security model](docs/security-model.md) for the trust boundaries of the console
and hub, and [SECURITY.md](SECURITY.md) to report a vulnerability.
</content>
</invoke>
