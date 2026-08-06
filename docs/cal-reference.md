# CAL Reference

**CAL** — the **Context Assembly Language** — is DejaDB's query language and its
primary API surface. It reads memory (`RECALL`, `ASSEMBLE`, `EXISTS`,
`HISTORY`), writes memory append-only (`ADD`, `SUPERSEDE`), introspects the
store (`DESCRIBE`, `EXPLAIN`), and renders results into model-ready context —
all through one text (or JSON-AST) surface.

A defining property of CAL is that its destructive surface is **narrow and
gated**: the only destructive statement is `FORGET <hash>` (a single-grain
tombstone), there are no `DELETE`/`DROP`-table tokens in the grammar, no bulk or
namespace erasure, and the whole surface can be switched off per-process
(`--no-destructive-ops`) for untrusted input. See [§8](#8-deletion-narrow-and-gated).
This reference documents the language as implemented in `dejadb-cal`.

For where CAL sits in the system, see [ARCHITECTURE.md](../ARCHITECTURE.md#5-cal-the-context-assembly-language).
For the security rationale, see the [security model](security-model.md).

---

## 1. Query structure

A CAL query is a version prefix (optional), a single statement, and a set of
optional modifiers:

```
[CAL/1]
[LET $name = <recall> ; ...]
<statement>
[| <pipeline stage> | <pipeline stage> ...]
[WITH <option>, <option> ...]
[FORMAT <spec>]
[WITH VARS { "key": "value", ... }]
```

- **Version prefix** — `CAL/1` (defaults to `CAL/1` if omitted).
- **`LET` bindings** — precompute sub-query results into `$parameters` (see
  [§7](#7-let-bindings)).
- **Statement** — exactly one (see [§3](#3-statement-types)).
- **Pipeline** — post-processing stages after `|` (see [§4](#4-the-pipeline)).
- **`WITH` options** — recall/behavior flags (see [§5](#5-with-options)).
- **`FORMAT`** — output rendering (see [§6](#6-output-formats)).
- **`WITH VARS`** — display-only string variables for template formats.

Comments start with `--` and run to end of line.

---

## 2. Grain types in CAL

Statements name grain types by their **plural** form for reads and **singular**
form for writes. The names are case-insensitive.

| Plural (read) | Singular (write) |
|---|---|
| `facts` | `fact` |
| `events` | `event` |
| `states` | `state` |
| `workflows` | `workflow` |
| `tools` | `tool` |
| `observations` | `observation` |
| `goals` | `goal` |
| `reasonings` | `reasoning` |
| `consensuses` | `consensus` |
| `consents` | `consent` |
| `skills` | `skill` |
| `recommendations` | — (query-only) |
| `*` / `grains` / `all` | — (wildcard: matches every type) |

`recommendations` (OMS 1.5 `0x0C`) is **read-only**: a recommendation is
engine-emitted and lifecycle-gated, so there is no `ADD recommendation`, and
`ADD`/`SUPERSEDE SET` never create or transition one. Queryable fields:
`target_ref`, `analyzer`, `severity`, `dedup_key`, `rec_status` — the last is
index-layer state rebuilt from the audit chain, never author-written.

---

## 3. Statement types

CAL's AST defines 22 statement variants. The ones below are **reachable from
text queries**. (A handful of variants exist in the AST for the JSON-CAL surface
and internal use but are intentionally not reachable from text — see
[§8](#8-deletion-narrow-and-gated).)

### 3.1 Read statements

#### `RECALL` — retrieve grains

```
RECALL <plural> [ABOUT "<free text>"] [WHERE <condition>]
       [RECENT <n>] [SINCE "..."] [UNTIL "..."] [LIKE "..."]
       [BETWEEN "..." AND "..."] [CONTRADICTIONS [OF (<sub-query>)]] [LIMIT <n>]
```

```sql
RECALL facts WHERE subject = "john" AND relation = "prefers"
RECALL facts WHERE subject = "john" RECENT 5
RECALL facts ABOUT "seating preferences" LIMIT 10
RECALL facts LIKE "window"
```

`RECALL *` is the explicit spelling of "any grain type" and means exactly the
same as omitting the type. It is still subject to the anchoring rule below: a
`*` recall must carry a subject filter or a free-text query, because `*` is not
a *specific* grain type and `LIMIT`/`RECENT` alone cannot bound an untyped scan.

Every `RECALL` needs a subject filter or a free-text query (`LIKE`/`ABOUT`) —
a bare type/namespace/RECENT scan is rejected with `VAL-E001`.

- `ABOUT "..."` runs semantic/free-text search (requires a full-text and/or
  vector leg; without them it returns a clear "unsupported" result rather than
  wrong data).
- `LIKE "..."` is the **same free-text leg as `ABOUT`**, spelled for the
  SQL-familiar — not a substring or wildcard filter, and not a separate scan.
  Both set the one free-text query the recall takes, so the two cannot be
  combined in a single `RECALL` (the parser rejects it). Use `WHERE ... =` for
  exact structured matching.
- `WHERE` is a structured filter (see [§3.4](#34-the-where-clause)).
- `RECENT n` is shorthand for "newest n" (`ORDER BY created_at DESC LIMIT n`).
- `SINCE` / `UNTIL` / `BETWEEN ... AND ...` are temporal filters accepting
  absolute dates or relative expressions (`"3 days ago"`).
- `CONTRADICTIONS` keeps only *contested* grains — see below.

##### `CONTRADICTIONS` — what is still disputed

When two writers change the same `(subject, relation)` and later sync, both
versions survive as live **heads** (a fork — see ARCHITECTURE.md §3). Recall
then answers with a deterministically-elected *provisional* head, which looks
like a settled value. `CONTRADICTIONS` asks the opposite question:

```sql
-- only the grains that are currently contested
RECALL facts CONTRADICTIONS

-- scoped: contested grains among the keys the sub-query selected
RECALL facts CONTRADICTIONS OF (RECALL facts WHERE subject = "john")
```

Each returned grain carries `contested_by` — the hashes of the other live tips
for its key — so a model can see *what* it disagrees with, not merely that
something is disputed:

```json
{ "hash": "ecb1af…", "fields": { "object": "enterprise" },
  "contested_by": ["ef8c16…"] }
```

The clause applies **after** every other filter, so it composes with
`ABOUT`/`WHERE`/`SINCE` rather than overriding them, and **before** `LIMIT`:
`LIMIT` bounds the contested grains, not the search that found them, so
`CONTRADICTIONS LIMIT 10` means "ten contested grains" and never "contested
among the first ten". The candidate scan widens to the executor's `max_limit`
(1000 by default) rather than stopping at the recall's usual limit.

An empty result is therefore a meaningful answer: nothing in scope is
contested. The one exception is announced — if the scan itself hit `max_limit`,
the query carries `CAL-W012` and the empty result covers only the grains it
managed to examine. Narrow with `WHERE`/`ABOUT`/`SINCE` when you see it.

To keep your normal result set and merely mark the disputed parts of it, use
[`WITH contradiction_detection`](#5-with-options) instead.

This detects **structural** contradiction — divergent writes to one key. Two
independently-added facts that merely disagree in meaning ("prefers tea" vs
"prefers coffee") are not a fork and are not reported here; that is a semantic
judgement, and belongs to the Waiser verifier (`docs/waiser.md`).

Resolve a fork with `deja merge` (or `DejaDB::merge_heads`), which supersedes
every tip with one merged grain and records all parents.

Set operations combine `RECALL`s:

```sql
RECALL facts WHERE subject = "john"
  INTERSECT
RECALL facts WHERE relation = "prefers"
```

`UNION`, `INTERSECT`, and `EXCEPT` are supported (up to 4 operands).

#### `EXISTS` — boolean existence check

```sql
EXISTS facts WHERE subject = "john" AND relation = "allergic_to"
```

Returns whether any matching grain exists, without materializing results.

#### `HISTORY` — version chain

```sql
HISTORY OF sha256:a1b2c3d4...
HISTORY WHERE subject = "john" AND relation = "prefers"
```

Walks the supersession chain for a grain (by content hash) or for a
`(subject, relation)` pair, newest to oldest. `HISTORY OF sha256:x DIFF
sha256:y` compares two versions.

#### `ASSEMBLE` — compose context

```
ASSEMBLE "<topic>" [FOR "<audience>"] FROM <sources> [WHERE ...]
         [BUDGET <n> [tokens|grains]]
         [PRIORITY label: weight, ... | PRIORITY a > b > c]
         [FORMAT <spec>] [WITH dedup(<field>)]
```

Clause order is fixed (it is the OMS §8.2 order): `FOR` directly after the topic,
`BUDGET` before `PRIORITY`, and `FORMAT` before `WITH dedup`. A clause written out
of order is not attached to the statement (e.g. `BUDGET` after `FORMAT` is
silently dropped — see the golden-test notes). `WHERE` applies to the
single-source form only. `PRIORITY` accepts weighted labels (`a: 0.5, b: 0.3`) or
an ordering chain (`a > b > c`, mapped to evenly spaced weights).

Single-source:

```sql
ASSEMBLE "caller profile" FROM facts WHERE subject = "john" FORMAT sml
```

Multi-source with per-source budgets and priorities:

```sql
ASSEMBLE "session prompt" FROM
  policies: (RECALL facts  WHERE namespace = "org.policies" RECENT 10),
  profile:  (RECALL facts  WHERE subject = "john"),
  recent:   (RECALL events WHERE session_id = "call-42" RECENT 10)
BUDGET 1500 tokens
PRIORITY profile: 0.5, recent: 0.3, policies: 0.2
FORMAT sml
WITH dedup(object)
```

Source labels are plain identifiers; `PRIORITY` labels must match them
(`CAL-E035`). `ASSEMBLE` is CAL's context-composition statement: it pulls from
labeled sources (including read-only [facade mounts](../ARCHITECTURE.md#55-assemble-and-facade-mounts),
addressed by the `alias.inner` *namespace string* inside a source's `RECALL` —
e.g. `WHERE namespace = "org.policies"`), applies per-source token budgets and
priorities, deduplicates, and renders one budgeted block. `STREAM ASSEMBLE ...` enables
streamed output. There is a 2000-grain post-dedup cap across all sources.

#### `COALESCE` — first non-empty fallback chain

```sql
COALESCE { RECALL facts WHERE subject = "john" AND relation = "seat" }
      OR { RECALL facts WHERE subject = "john" AND relation = "prefers" }
    ELSE { RECALL facts WHERE subject = "john" RECENT 1 }
```

Tries each branch in order, returning the first that yields results (with an
optional `ELSE` fallback).

### 3.2 Write statements (append-only)

Every write requires a `REASON` (or `BECAUSE`) clause — the provenance of a
change is captured in the change itself. Writes never mutate existing grains;
they add new content-addressed grains.

#### `ADD` — add a grain

```
ADD <singular> SET <field> = <value> [SET <field> = <value> ...]
    [WITH <add option> ...] REASON "<why>"
```

```sql
ADD fact SET subject = "john" SET relation = "prefers" SET object = "window seat"
    SET confidence = 0.9 REASON "caller stated during booking"

ADD goal SET description = "confirm flight" SET goal_state = "open"
    REASON "opened at call start"
```

Omitting `SET namespace = ...` stores the grain in the session namespace.

`ADD` intelligence options: `WITH extract_memories` (decompose content into
atomic facts), `WITH extract_event_date`, `WITH sync`. `WITH auto_relate`
parses but is **not implemented** — it infers no relations and emits CAL-W004.
Link grains explicitly via `related_to`, or widen at read time with
`WITH multi_hop(n)`.
There is also an `ADD workflow "name" ... graph ... BIND ... REASON "..."`
form for DAG-shaped Workflow grains.

#### `SUPERSEDE` — evolve a grain

```
SUPERSEDE sha256:<hash> SET <field> = <value> [SET <field> = <value> ...]
    BECAUSE "<why>"
```

```sql
SUPERSEDE sha256:a1b2c3d4... SET object = "aisle seat"
    BECAUSE "caller changed preference"
```

Writes a new version that supersedes the identified grain. The old version is
preserved as append-only history (visible via `HISTORY`), never deleted. A
matching `SUPERSEDE workflow` form exists for Workflow grains.

#### `ACCUMULATE` — numeric/last-writer-wins deltas

```sql
ACCUMULATE state WHERE subject = "john" AND relation = "call_count"
    ADD call_count = 1 REASON "another call handled"
```

Applies numeric `ADD field = delta` operations and `SET field = value`
replacements against the current tip (resolved by hash or by
`(subject, relation)` lookup), producing a new superseding grain.

#### `FORGET` — tombstone a single grain (gated)

```sql
FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d
```

The one destructive statement. Removes a single grain by content address (maps
to `DejaDB::forget`). Unlike `SUPERSEDE`, this is a genuine tombstone — the
grain is gone, not versioned. It is gated by the executor's
`allow_destructive_ops` (on by default; disable per-process with
`--no-destructive-ops`), refused inside saved-query bodies, and — when
capability scopes are enforced — requires the `admin` scope. Only the hash form
exists; there is no bulk/user/scope erasure from CAL (see [§8](#8-deletion-narrow-and-gated)).

### 3.3 Introspection & management

| Statement | Purpose |
|---|---|
| `DESCRIBE facts` / `DESCRIBE SCHEMA` | Describe a grain type or the whole schema |
| `DESCRIBE CAPABILITIES` | Report the CAL conformance level and supported features |
| `DESCRIBE FIELDS [type]` | List filterable/sortable fields |
| `DESCRIBE TEMPLATES` / `DESCRIBE QUERIES` | List registered templates / saved queries |
| `EXPLAIN <query>` | Return a query plan for a statement |
| `BATCH { stmt1 ; stmt2 ; ... }` | Run several statements as one batch (up to 10 entries; optional labels) |
| `DEFINE TEMPLATE <name> AS "<source>"` | Register a reusable output template (ELEMENT shorthand) |
| `DEFINE TEMPLATE <name> [EXTENDS <parent>] HEADER { } ELEMENT { } …` | Register a sectioned template (OMS CAL §10.6) |
| `DEFINE QUERY "name"($params) AS { body }` | Register a saved, parameterized query |
| `DROP TEMPLATE "name"` / `DROP QUERY "name"` | Remove a template or saved query |
| `RUN "name"($p = v, ...)` | Execute a saved query with bindings |

> **`DROP` is only ever `DROP TEMPLATE` / `DROP QUERY`.** The parser accepts no
> other `DROP` target. Templates and saved queries are host-managed metadata,
> not memory — dropping one removes a definition, never a grain.

Saved queries let prompt-assembly logic live as named, versioned CAL — hot-swappable
without redeploying the agent:

```sql
DEFINE QUERY "session_prompt"($user, $session)
  DESCRIPTION "standard session bootstrap"
AS {
  ASSEMBLE "session" FROM
    profile: (RECALL facts  WHERE subject = $user),
    recent:  (RECALL events WHERE session_id = $session RECENT 10)
  BUDGET 1200 FORMAT sml
}

RUN "session_prompt"($user = "john", $session = "call-42")
```

Saved-query limits: 100 per namespace, 8 KiB body, 10 parameters. Saved-query
bodies get an extra read-only verification pass, so a saved query can never
smuggle in a write or a blocked keyword.

Saved queries and custom templates are **host metadata carried by the memory
file**, not memories: they persist as `meta` rows (`qry:<name>`, `tpl:<name>`)
and so travel with the `.db` — the same set is visible from the CLI, MCP and
the web console. They are never grains, never content-addressed, and never
appear in recall results.

An entry the file carries that the running build cannot load — a template
written before a limit was tightened, a row from a newer version — is skipped
rather than failing the open, and reported: on stderr from `deja cal` / `repl`
/ `serve` / `ui` / `hub`, and in `warnings` on `GET /api/config`. It stays in
the file, so an older or newer build can still read it; overwriting the entry
is what loses it.

### 3.4 The `WHERE` clause

`WHERE` conditions combine with `AND`, `OR`, and `NOT` (up to nesting depth 8):

| Form | Example |
|---|---|
| Comparison | `confidence >= 0.8`, `subject = "john"`, `role != "system"` |
| Membership | `relation IN ("prefers", "likes")`, `subject NOT IN (...)` |
| Null checks | `deadline IS NULL`, `object IS NOT NULL` |
| Text | `object CONTAINS "seat"`, `subject STARTS WITH "caller:"` |
| Boolean logic | `subject = "john" AND (relation = "prefers" OR relation = "likes")` |

Comparators: `=`, `!=`, `>`, `>=`, `<`, `<=`. Values are strings (`"..."`),
numbers, booleans, arrays (`["a", "b"]`), content hashes (`sha256:abcdef...`), or
parameter references (`$name`). Membership sets are capped at 100 values.

---

## 4. The pipeline

Pipeline stages post-process a statement's result set, chained with `|` (up to
5 stages):

| Stage | Effect |
|---|---|
| `\| SELECT f1, f2` | Keep only these fields |
| `\| PROJECT f1 AS a, f2` | Select with renaming |
| `\| ORDER BY field [ASC\|DESC]` | Sort |
| `\| LIMIT n` / `\| OFFSET n` | Paginate (limit ≤ 1000) |
| `\| COUNT` | Return the count instead of the rows |
| `\| FIRST` | Return only the first result |
| `\| SUBJECTS` / `\| OBJECTS` | Extract the `subject`/`object` of each Fact |
| `\| HASHES` | Extract the content hash of each grain |
| `\| GROUP BY field` | Group results |
| `\| WHERE <condition>` | Post-pipeline filter |

```sql
RECALL facts WHERE subject = "john" | SELECT relation, object | LIMIT 5
RECALL facts WHERE namespace = "caller" | COUNT
RECALL facts WHERE relation = "knows" | OBJECTS
```

---

## 5. `WITH` options

`WITH` options tune recall behavior. There are roughly three dozen; a
representative selection:

| Option | Effect |
|---|---|
| `WITH superseded` | Include historical (superseded) grains, each stamped with the `superseded_by` hash of the version that replaced it. Applies to every recall leg — structural, free-text and vector — so text that survives only in an old version becomes findable. Forgotten grains stay gone. |
| `WITH provenance` | Include the provenance chain in results |
| `WITH include_sources` | Include `derived_from` source grains |
| `WITH score_breakdown` / `WITH explanation` | Return ranking detail |
| `WITH diversity(0.5)` | Apply MMR diversity (optional lambda) |
| `WITH dedup(object)` | Deduplicate, optionally by a field |
| `WITH rerank` / `WITH rerank("model")` | Cross-encoder reranking (feature-gated) |
| `WITH query_expansion` / `WITH query_decompose` / `WITH hyde` | Query rewriting strategies |
| `WITH multi_hop(2)` | Entity-graph expansion (1–3 hops): follows the entities named by the first-pass results and adds what they anchor to the candidate pool, competing within `LIMIT` |
| `WITH recency_weight(0.3)` / `WITH min_score(0.6)` | Scoring controls |
| `WITH conflict_resolution` | Keep only the newest grain per `(subject, relation)` |
| `WITH contradiction_detection` | Keep everything, but stamp `contested_by` on grains that are live tips of an open fork |
| `WITH annotate_relative_time` | Add "2 weeks ago"-style labels |
| `WITH progressive_disclosure(summary)` | OMS progressive-disclosure level |

```sql
RECALL facts ABOUT "dietary restrictions" WITH rerank, diversity(0.4), min_score(0.5)
RECALL facts WHERE subject = "john" WITH superseded, provenance
```

Options requiring an unavailable backend (e.g. a reranker feature that is not
compiled in) return an honest error rather than silently degrading.

---

## 6. Output formats

`FORMAT <spec>` renders the result set. A single format or a list (up to 5):

| Format | Output |
|---|---|
| `sml` | Structured Memory Language (compact, Claude-class) |
| `toon` | TOON compact tabular blocks |
| `markdown` | Markdown — one assertion per line (see below) |
| `json` | JSON |
| `yaml` | YAML |
| `text` / `table` / `csv` / `triples` | Plain text / Markdown table / CSV / `S R O` triples |
| `structured` / `readable` / `compact` / `data` | OMS §10.1 semantic presets — aliases for `sml` / `markdown` / `text` / `json` |
| `TEMPLATE <name>` | A registered template — **bare** name, quoting makes it a body |
| `TEMPLATE "<source>"` | An inline template body — **quoted** (ELEMENT shorthand) |
| `TEMPLATE { ELEMENT { … } }` | Inline sections |
| `preset "<name>"` | A registered template, older spelling of `TEMPLATE <name>` |

`FORMAT <fmt>` may also be written `AS <fmt>` (§7 `as_clause`), including the
bracketed multi-format list.

A `TEMPLATE` **name** follows the same rules as the name in
`DEFINE TEMPLATE <name>`, so words that happen to be CAL keywords
(`recent`, `scope`, …) work in both places. A *quoted* argument is never a
name — `FORMAT TEMPLATE "..."` is always an inline body.

### What `markdown` renders

`FORMAT markdown` produces one line per grain, stating what the memory asserts:

```
- **john** prefers window seat *(0.95, 2026-01-13)*
- **user**: what's the refund window?
- **stripe_refund** failed: rate_limited 429
```

The trailing italics carry only what changes how much weight to give the line —
confidence when it is below 1.0, the date, and `superseded`. Storage
bookkeeping (namespace, grain type, raw epochs, the content address) is left
out: this output exists to be pasted into a prompt, and none of it helps a
model reason. Use `FORMAT json` when you need the full envelope, or a
`FORMAT TEMPLATE` to choose the fields yourself. A `GROUP BY` render keeps its
group headings.

### Template sections and limits

A sectioned template's `ELEMENT` renders once per grain, `ELEMENT_SUMMARY`
replaces it when the budget squeezes the render below full disclosure, and
`ELEMENT_OMIT` accounts for grains the budget dropped. `SOURCE_BREAK` goes
between `ASSEMBLE` sources — not before the first. `HEADER`/`FOOTER` bracket
the whole render and are the only place `assembly.*` and `budget.*` make
sense; `source.*` is bound only inside an element run.

Sections you do not define are inherited from `EXTENDS <parent>`, defaulting
to `readable`. The three preset parents (`structured`, `readable`, `compact`)
define element-level sections only, so inheriting never adds a header you did
not ask for. `data` cannot be extended (`CAL-E119`).

| Limit (OMS CAL §10.8) | Value |
|---|---|
| Template body | 4096 bytes (`CAL-E040`) |
| Templates per namespace | 50 (`CAL-E118`) |
| Conditional nesting | 5 (`CAL-E117`) |
| `{{#each}}` iterations | 200 — emits `CAL-W011` when it truncates |
| Template name | 64 chars |
| Inheritance depth | 1 |

```sql
RECALL facts WHERE subject = "john" FORMAT sml
RECALL facts WHERE subject = "john" FORMAT [json AS data, markdown AS readable]
```

Rendering is budget-aware and uses progressive disclosure — as a `BUDGET` fills,
grains degrade from full form to summary to omitted rather than the block being
cut mid-token.

---

## 7. `LET` bindings

`LET` precomputes a sub-query into a `$parameter` that later clauses reference —
useful for two-step "find the set, then query within it" patterns:

```sql
LET $friends = SUBJECTS OF (RECALL facts WHERE relation = "knows" AND object = "john");
RECALL facts WHERE subject IN $friends AND relation = "prefers"
```

Extractors are `SUBJECTS`, `OBJECTS`, or `HASHES`. Limits: at most 5 `LET`
bindings per query, each capped at 1000 grains.

---

## 8. Deletion: narrow and gated

CAL has exactly one destructive statement — `FORGET <hash>`, a single-grain
tombstone — and it is gated. There is no way to delete in bulk, drop a table,
truncate, or erase a namespace from a query. Defense in depth:

1. **Lexer blocklist.** A set of destructive keywords has no statement token
   in the grammar — they only ever lex as inert identifiers, which the parser
   hard-rejects (`is_destructive_keyword`) before any dispatch. `DELETE` has
   no token at all — the deletion verb is `FORGET`. The blocked set includes:

   ```
   DELETE  ERASE   DESTROY  TRUNCATE  INSERT   CREATE   WRITE   STORE
   KEY     ENCRYPT DECRYPT  ROTATE    MASTER   DEK      SECRET  POLICY
   SEAL    UNSEAL  GRANT    REVOKE    CONSENT  RESTRICT SCHEMA  PARTITION
   INDEX   MIGRATION
   ```

2. **Parser.** Those identifiers are rejected with a dedicated error.
   `FORGET <hash>` parses; the bulk forms `FORGET USER`/`FORGET SCOPE` and
   `PURGE` have tokens but the text parser refuses them (they have no store
   backing). `DROP` accepts only `DROP TEMPLATE`/`DROP QUERY`.

3. **Execution gate.** `FORGET` runs only when the executor's
   `allow_destructive_ops` is enabled. It is **on by default**, but any host can
   turn it off per-process — `deja serve --mcp --no-destructive-ops` (likewise
   `deja ui` and `deja cal`) — giving a read-only session in which `FORGET`
   returns `Unsupported`. When capability scopes are enforced (server path),
   `FORGET` additionally requires the `admin` scope.

Additionally, saved-query bodies get a separate read-only verification pass, so
a stored query can never carry a `FORGET`.

**The same primitive backs every surface.** CAL `FORGET <hash>`, the Rust API
`forget`, and the MCP [`dejadb_forget`](mcp-reference.md#dejadb_forget) tool all
tombstone a single grain by content address. For untrusted input, disable the
whole surface with one flag; superseded versions remain as append-only history
(via `HISTORY`) regardless.

Two Unicode invariants also run before tokenization:

- **Bidi-override rejection** — bidirectional control characters
  (U+202A–202E, U+2066–2069) are rejected, defeating visual query spoofing.
- **NFC normalization** — the query is Unicode-NFC-normalized before lexing (and
  again when computing the audit hash).

---

## Safety limits

The parser and executor enforce these hard bounds:

| Limit | Value |
|---|---|
| Max query length | 64 KiB (65,536 bytes) |
| Max nesting depth | 8 |
| Max result `LIMIT` value | 1,000 |
| Max `IN (...)` set size | 100 |
| Max pipeline stages | 5 |
| Max set-operation operands | 4 |
| Max `BATCH` entries | 10 |
| Max `LET` bindings per query | 5 |
| Grain cap per `LET` binding | 1,000 |
| Post-dedup grain cap (`ASSEMBLE`) | 2,000 |
| Max `REASON` length | 500 |
| Max formats per `FORMAT` list | 5 |
| Max `WITH VARS` entries / size | 10 / 1 KiB each |
| Saved queries per namespace | 100 |
| Saved-query body size | 8 KiB |
| Saved-query parameters | 10 |

---

## Copy-pasteable examples

```sql
-- Current preferences for a caller, model-ready
RECALL facts WHERE subject = "john" AND relation = "prefers" FORMAT sml

-- Count everything in a namespace
RECALL facts WHERE namespace = "caller" | COUNT

-- Add a fact (REASON is mandatory)
ADD fact SET subject = "john" SET relation = "allergic_to" SET object = "peanuts"
    SET confidence = 1.0 REASON "stated at intake"

-- Evolve it; the old version is kept as history
SUPERSEDE sha256:<hash> SET confidence = 0.8 BECAUSE "unconfirmed on follow-up"

-- Version history for a (subject, relation)
HISTORY WHERE subject = "john" AND relation = "prefers"

-- Two-step: friends-of-john's preferences
LET $friends = SUBJECTS OF (RECALL facts WHERE relation = "knows" AND object = "john");
RECALL facts WHERE subject IN $friends AND relation = "prefers" | LIMIT 20

-- Assemble a budgeted session prompt from three sources
ASSEMBLE "session" FROM
  profile: (RECALL facts  WHERE subject = "john"),
  recent:  (RECALL events WHERE session_id = "call-42" RECENT 10)
PRIORITY profile: 0.6, recent: 0.4
BUDGET 1200 tokens FORMAT sml

-- Existence check (boolean)
EXISTS facts WHERE subject = "john" AND relation = "allergic_to"

-- Delete a single grain by content address (gated; on by default,
-- off under --no-destructive-ops, and needs the `admin` scope on the server):
FORGET sha256:684c6c9bda818630a870119d0726e4d242ed537af061658ef6f3acb158a2c67d

-- These FAIL by design — the destructive surface stays narrow:
--   DELETE facts WHERE subject = "john"     -- no token; rejected at the lexer
--   DROP TABLE grains                        -- DROP only accepts TEMPLATE/QUERY
--   FORGET USER "john"                       -- bulk erasure not reachable from text
--   PURGE STALE                              -- not reachable from text
```
</content>
