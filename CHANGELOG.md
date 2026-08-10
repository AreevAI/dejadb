# Changelog

All notable changes to DejaDB are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Principals reach every surface.** `deja ui --auth deja-auth.json`
  serves the console in multi-principal mode: the credential map resolves
  bearer/Basic tokens to principal names (env refs or sha256 — no raw
  secrets, no policy in the file), rights come from the memory's own grant
  grains, requests bind per-request (the guard restores the default on
  drop, panic included), unknown or missing tokens run as `anonymous`
  (read-only unless the file grants more), `/api/loop/run` is gated on
  `loop.run`, and in this mode the audit actor IS the bound principal — a
  request body cannot claim an identity. `deja cal|repl|serve
  --as <principal> [--auth FILE]` runs a session under the file's grants
  (fail closed), `forget-subject`/`purge-older-than --as` check `erase`
  before touching anything, and both bindings gain `principal=` /
  `principal` on open — the loop actor follows the bound principal.
- **`REMEMBER` is a CAL statement.** The onboarding verb, in the language:
  `REMEMBER "<content>" [WITH session("<id>"), role("<r>"), run("<id>")]`
  captures an Event through the same store path as `deja remember` and the
  bindings' `capture` — thread-indexed, run-joined, observer = the bound
  session's principal. Requires the `write` grant. LLM fact extraction
  stays host configuration; the statement carries no model names.
- **The cross-surface parity gate is a test.** "Every governed operation
  has a CAL spelling" — the truth condition behind "CAL is all you need" —
  is now an executable table (`cal_parity_tests.rs`): 28 operations, each
  parsed and pinned to its statement class in CI.
- **`docs/cal-for-llms.md`** — the one-page CAL grammar card designed to
  paste into a system prompt, because the adopter's end user is an LLM:
  every statement family, the identity rules (AUT-E001 means ask an admin,
  don't retry), and the boundary in the card itself.
- **CAL 1.3 governance: the loop lifecycle is CAL.** `RUN LOOP [FULL]
  [WITH min_new(N), if_stale("6h")]` triggers the analysis pass;
  `APPROVE`/`REJECT`/`APPLY`/`ROLLBACK <hash> BECAUSE "…"` are the review
  actions, with the reason as *syntax* — a parse error without it, backed
  by the engine's own non-empty check. `DESCRIBE LOOP` is the in-language
  `deja loop list` (health plus the pending queue with hashes);
  `DESCRIBE ANALYZERS|OUTCOMES|POLICY` are the other reads. Identity never
  rides the statement: the executor's `GovernanceHost` seam
  (`dejadb_loop::LoopGovernance`, attached on the CLI, MCP, and console
  surfaces) derives actor, scopes, and observer from the bound session, so
  the four gates — separation of duties, the self-approval block
  (including the run-trigger co-creator), the two-key destructive apply
  (`loop.apply` + the session's own delete/erase), the hash-chained
  audit — run exactly as on every other surface. Loop *policy* writes stay
  host-only (the policy gates CAL; CAL editing it would be
  self-licensing), governance statements refuse saved-query bodies, and an
  executor without a host says "governance is not wired" instead of
  pretending. Everyday node names (`approve`, `reject`, `apply`, …) still
  work in workflow definitions.
- **CAL 1.3 Tier-3 DCL: `GRANT`, `REVOKE`, `SHOW GRANTS`, and
  `DESCRIBE PRINCIPAL` are CAL statements.** Access control now lives in
  the language, SQL-style: `GRANT read, write ON caller TO
  "agent:support-bot" WITH because("support rotation")` writes a grant
  grain — an ordinary `mg:permits` Fact in the reserved `agent:authz`
  namespace, carrying grantor and reason, recallable like anything else.
  `REVOKE` is retraction-by-supersession: partial revokes supersede with
  the reduced grant, full revokes leave a retraction record — grant
  history is append-only, nothing is deleted; a revoke wider than a
  grant's scope is refused by name rather than silently splitting it.
  GRANT/REVOKE require the session's `admin` grant (owner sessions
  included), are capped by the writes cap, never appear inside
  saved-query bodies, and `GRANT`/`REVOKE` left the lexer blocklist —
  while `DELETE` and all credential/key vocabulary (now incl. `TOKEN`)
  stay blocked non-tokens forever. `SHOW GRANTS [FOR "<p>"]` and
  `DESCRIBE PRINCIPAL "<p>"` are the read side: the live grant rows and a
  principal's effective rights (an empty answer is stated, not implied).
- **CAL 1.3 Tier-2 destruction: `FORGET SUBJECT` and `PURGE OLDER THAN` are
  CAL statements.** Bulk erasure was a documented OMS deviation living only
  on host surfaces (`deja forget-subject` / `purge-older-than`); the CAL 1.3
  draft brings it into the language, authorization-gated. `FORGET SUBJECT
  "<id>" [WITH text_mentions] BECAUSE "…"` erases an identity in the session
  namespace; `PURGE OLDER THAN <n><d|h|m> [TYPE t] [IN "<ns>"] BECAUSE "…"`
  is the retention sweep (never an implicit all-namespace sweep). BECAUSE is
  mandatory on both (parse error without — `CAL-E018`), and
  optional-but-recorded on `FORGET <hash>`. Destruction still takes a hash,
  an identity, or an age — never a predicate — and `--no-destructive-ops`
  still switches the whole surface off.
- **Every destructive CAL execution now writes an audit Observation** in the
  reserved `agent:authz` namespace: the session principal, the verb
  (`delete`/`erase`), the target, the reason, and the erased count —
  hash-addressed, replicated, and recallable
  (`RECALL observations WHERE namespace = "agent:authz"`). Audit records
  are occurrences, so each carries a unique frame id (the #66 lesson —
  two identical erasures stay two records).
- **Grants-based authorization under CAL** (the "grants, not gates" model —
  `docs/cal-all-you-need-proposal.md`). Grants live in the memory file as
  `mg:permits` Facts in `agent:authz`; a facade bound to a principal
  (`with_principal`) enforces per-namespace verbs — read, write, supersede,
  delete, erase, loop.\*, admin — at the one chokepoint every CAL surface
  flows through, fail-closed, with `AUT-Ennn`/`CAL-E121` refusals that name
  the missing verb. Unbound local sessions stay the implicit owner: the
  single-user path is byte-identical to before.

### Fixed

- **A retried tool call is counted again instead of silently merging away**
  ([#66]). Making a duplicate add a no-op in 1.1.1 fixed a crash but turned it
  into a wrong answer in the same headline example: five identical
  `record_tool_call` failures collapsed to one grain, so `loop.tool_failure`
  had nothing to cluster and the README's "proof" block ran clean and printed
  nothing. Content-addressed dedup is right for a fact and wrong for an
  occurrence — an agent retrying a failing call with identical arguments is
  exactly the workload that analyzer exists to catch. `record_tool_call` now
  gives each call an identity, so occurrences stay distinct while the store
  stays deduplicated. The raw `add("tool", …)` path is unchanged.
- **`record_tool_call` gained `call_id`** (Python `call_id=`, Node `callId`) —
  the provider's own `tool_call_id`. It is stored on the grain and queryable
  (`RECALL tools WHERE tool_call_id = …`), linking a recommendation's evidence
  back to the transcript that produced it. Omitted, a synthetic id is stamped.
  `tool_call_id` had been serialized and listed as queryable since 1.0 but no
  builder ever set it, so it was unreachable from every host surface.
- **CAL warnings reach the bindings** ([#68]). Python, Node and the MCP
  `dejadb_cal` tool serialized the result payload alone, dropping every
  `CAL-Wnnn` the executor raised — so `WITH score_breakdown` on `RECALL`
  stayed a silent no-op despite `CAL-W014` existing to announce it, and none
  of the fourteen documented warnings could reach a binding user. `cal()` now
  returns them under a `warnings` key, present only when non-empty.
- **`add()` explains an engine-authored type instead of calling it unknown**
  ([#67]). `DESCRIBE` lists 12 grain types and `add("recommendation", …)`
  answered `unknown grain type`, sending callers to hunt for a typo in a name
  the engine had just returned. Recommendations stay host-unwritable — their
  `dedup_key` is computed from the analyzer family, so a host-authored one
  would enter the review queue as though an analyzer had produced it — but the
  refusal now says that, and points at `RECALL recommendations`. The
  addable set is sourced from the grain-type registry (`host_addable`) and
  test-pinned to the builder, so the surfaces cannot drift again.

- **A release-mode test flake, found while fixing the above.**
  `review_queue_is_severity_ordered_and_stable_across_runs` failed about half
  the time under `cargo test --release` (never in debug, which is what CI runs).
  Its fixture seeded two byte-identical Facts to stand for a duplicate, and
  those collapse to one grain whenever both writes land in the same
  millisecond — so `loop.duplicate_sweep` had nothing to find and the queue
  came back two long instead of three. The fixture now stamps explicit
  `created_at` values. Same underlying property as #66, but here dedup is
  behaving correctly and the test was the thing making a claim it had not
  arranged for.

- **The trigger of an LLM loop run can no longer approve its own model's
  output.** Every recommendation recorded its creator as `engine:<analyzer>` —
  correct for the deterministic analyzers, whose findings are computed rather
  than authored, but an LLM or external-command finding exists because a
  specific principal invoked it, and that principal appeared nowhere the
  review gate could see. So `deja loop run --model …` followed by
  `deja loop approve` from the same actor sailed past the self-approval
  block. Runs now record the triggering principal as co-creator on every
  non-builtin recommendation (the CLI's `--actor`, the server request's
  `actor`, MCP's `agent:mcp`, the bindings' handle actor), and review refuses
  approval from the creator *or* the trigger. Deterministic findings are
  unchanged — the engine stays their only creator, so a solo operator's
  normal run-then-approve flow still works.

[#66]: https://github.com/AreevAI/dejadb/issues/66
[#67]: https://github.com/AreevAI/dejadb/issues/67
[#68]: https://github.com/AreevAI/dejadb/issues/68

## [1.1.1] - 2026-08-09

**The self-improvement engine, previously named Waiser, is renamed Deja
Loop** — every surface, no aliases. This is technically a breaking release
shipped as a patch: nothing had adopted the old names, so the rename went
out clean rather than dragging a deprecation tail. If you somehow depended
on the old names, this is the one entry that maps them. Historical entries
below have been rewritten to the new names.

### Changed — breaking (rename only; no behavior changes)

- **Crates**: `waiser` → **`deja-loop`** (engine, still zero DejaDB deps),
  `dejadb-waiser` → **`dejadb-loop`** (substrate adapter). The old crates
  stay published on crates.io but are abandoned at 1.1.0.
- **CLI**: `deja waiser <sub>` → **`deja loop <sub>`** (same subcommands);
  `deja recall-hook --with-waiser` → `--with-loop`.
- **HTTP API**: `/api/waiser/*` → **`/api/loop/*`** (all ten routes).
- **MCP**: tool `dejadb_waiser` → **`dejadb_loop`** (`dejadb_recommendations`
  unchanged).
- **Bindings**: Python `waiser_run/waiser_health/waiser_analyzers/
  waiser_outcomes` → `loop_run/loop_health/loop_analyzers/loop_outcomes`;
  Node `waiserRun/…` → `loopRun/…`; Rust builders `with_waiser_policy` →
  `with_loop_policy`.
- **Host config**: policy file `waiser-policy.json` → **`loop-policy.json`**;
  env vars `WAISER_POLICY`/`WAISER_NOW_MS` → `DEJA_LOOP_POLICY`/
  `DEJA_LOOP_NOW_MS`.
- **Error codes**: domain `WSR` → **`LOP`** (same numbers, e.g. `WSR-E021` →
  `LOP-E021`). A one-time exception to the append-only rule, taken while
  nothing external consumed the codes.
- **Persisted values** (old files keep working for reads, but engine
  bookkeeping written by ≤1.1.0 is not found by 1.1.1 — regenerate with one
  `deja loop run`): internal namespace `waiser` → **`deja-loop`**
  (hyphenated so it cannot collide with a user namespace named `loop`),
  state subject `__waiser_state__` → `__loop_state__`, relations
  `waiser_recommendation`/`waiser_audit` → `loop_recommendation`/
  `loop_audit`, analyzer ids `waiser.<name>/1` → **`loop.<name>/1`**.
- **Wire protocols** (external `--llm-cmd` / `--analyzer-cmd` processes):
  request marker `"waiser": 1` → `"loop": 1`, `"waiser_analyzer": 1` →
  `"loop_analyzer": 1`.

### Fixed

- The engine's error-code uniqueness test now covers all 15 variants
  (`LlmBackend` was missing from the representative list).
- `loop.goal_stagnation/1`'s doc no longer references a `deja loop enable`
  subcommand that never existed.
- `crates/dejadb-js/Cargo.toml` version drift (was stuck at 1.0.5).

## [1.1.0] - 2026-08-08

A minor rather than a patch: this release removes and renames public
items in `dejadb-core`/`dejadb-cal`, adds a `GrainType` variant (so an
exhaustive match downstream needs a new arm), and changes what
`remember()` writes. See **Changed — breaking** below before upgrading a
Rust dependency; CLI and bindings users are affected by the `remember()`
and State-JSON changes only.

### Added

- **Bulk erasure for compliance** (both backends): `forget_subject(ns,
  subject)` — right-to-erasure for one identity: every grain holding a
  structured reference (full supersession history, object-position
  references, thread events), the identity's dictionary entry, and
  erased-only vocabulary tokens, all in one transaction with one
  replicating op-log tombstone per grain; and `forget_older_than(ns,
  cutoff, grain_type)` — the age-based retention sweep. Exposed in the
  bindings (`forget_subject`/`forgetSubject`, `forget_older_than`/
  `forgetOlderThan`) and the CLI (`deja forget-subject … --yes`,
  `deja purge-older-than <days> … --yes`); returns an `ErasureReport` of
  counts only. Deliberately NOT reachable from CAL text — the grammar and
  the `cal_forget_user`/`cal_forget_scope` stubs are unchanged. This is a
  documented OMS deviation; requirements, scope contract ("about a
  subject" = dictionary-indexed references), and backend caveats live in
  `docs/erasure.md`.

- **PostgreSQL backend** (non-default cargo feature `postgres`): the same
  store logic runs over one Postgres schema per memory — for stateless
  deployments where a file has nowhere durable to live. `DejaDB::open_postgres
  / open_postgres_with`, `deja --db postgres://…?schema=<name>` (CLI built
  with the feature), pgvector-backed vector recall (the `vector(dim)` column
  is created at the first `set_embedder`; a dim mismatch is a hard refusal),
  CAS blobs in an in-schema table, and **multiple concurrent writers per
  memory**: write transactions claim id blocks from an in-schema counters
  row (briefly serializing them, so the op-log stays gapless and ordered
  for followers), the term dictionary and BM25 stats are DB-authoritative
  on cache miss so instances immediately see each other's writes, and
  racing supersedes/forgets of one grain produce one winner and one clean
  `SupersessionConflict`/`NotFound` via in-transaction `FOR UPDATE`
  rechecks. (The **`STO-E002` StoreBusy** code is registered but currently
  unraised — reserved for exclusive-access arbitration.) One process can
  hold handles to many memories at once. The **Python and Node bindings
  ship the backend built in** — the same `DejaDB`/`DejaDb` class takes a
  `postgres://…?schema=<name>` DSN wherever it takes a path, and
  `drop_postgres_schema` exposes memory-level erasure. The **recall
  telemetry sidecar works on this backend** (tables ride the memory's
  schema; on the file backend the sidecar tables gained a `telem_` prefix —
  existing sidecars start fresh, telemetry is disposable evidence).
  `pg_bench` publishes the server-tier latency table (RESULTS.md §7).
  Erasure/portability map to
  `DROP SCHEMA … CASCADE` / `pg_dump -n`; the page cipher and the telemetry
  sidecar remain file-backend capabilities and are rejected with clear
  errors. Parity is pinned by the new **`dejadb-conformance`** crate: one
  case list (forks, two-hop replication, tombstones, PITR, BM25, vectors,
  CAS, CAL end-to-end) executed against both backends, plus a Postgres CI
  job on `pgvector/pgvector:pg16`.
- **Prebuilt `deja` binaries on every GitHub Release** (#38). Releases
  v1.0.0–v1.0.5 carried no binary assets, so the only way to get the CLI was
  `cargo install dejadb` — a full Rust build. `release-cli.yml` now builds
  Linux x86_64/aarch64, macOS x86_64/arm64 and Windows x86_64, smoke-tests each
  one (`--version`, then a real add/recall round trip) before packaging, and
  attaches the archives plus a `SHA256SUMS` file. Linux aarch64 builds on a
  native arm64 runner for the same reason `release-pypi.yml` does — cross-gcc
  cannot compile turso's mimalloc/zstd deps. `scripts/install.sh` is the
  matching `curl | sh` installer: it resolves the latest tag, verifies the
  download against `SHA256SUMS`, and installs to `~/.local/bin`. This is what
  makes `deja ui` — the console, including the Deja Loop review queue — reachable
  from a notebook or a scratch container, where the wheel covers the memory
  loop but the console lives in the binary.
- **Deja Loop bindings parity** (#39). Four capabilities the CLI and HTTP
  surfaces had and the bindings did not: `loop_health()` / `loopHealth()`
  (the loop's staleness snapshot — how a host notices a SessionEnd hook or cron
  came unwired), `approve_recommendation()` / `approveRecommendation()` (approve
  **without** applying, so a supervising agent can approve for a human to apply
  later), `loop_analyzers()` + `set_analyzer_config()` (the roster and the
  per-analyzer enable/disable behind the console's Setup tab), and an optional
  `scopes` argument on the review/apply calls. The bindings previously
  hardcoded `ScopeSet::all()`, so the separation-of-duties gate
  (write ≠ review ≠ apply) could not be enforced or even demonstrated from
  Python or Node although the engine implements it.
- Internal `Db` backend seam in `dejadb-store`: the store logic is
  backend-agnostic; the embedded Turso engine and the Postgres transport are
  interchangeable implementations behind it. By construction this also fixed
  three latent issues: `apply_supersede_flip` now runs in a transaction,
  `rebuild_link_indexes` rolls back on error, and the write path's per-add
  statement preparation is now cached.
- `DESCRIBE <grain type>` reports `required_fields` — the fields the write
  path refuses to build the grain without. Previously the only way to learn a
  type's shape was to hit `VAL-E001` one field at a time (`skill` asks for
  `name`, then asks for `description`). A test pins the list to the validator,
  and `crates/dejadb-cal/tests/docs_examples.rs` parses every `sql` example in
  `docs/cal-reference.md` on each run, so a documented query that does not
  parse fails CI rather than a user's first session.

### Fixed

- Head re-election after `forget`/changed-key `supersede` keys the
  heads↔triples join on `(ns,s,p)` — an unkeyed join could elect a
  `related_to` link target as `entity_latest.o` for a link-bearing tip,
  breaking `add_if_novel`'s value probe.
- The RRF fusion sort is NaN-safe, and `recall`/`thread_tail` fetch their
  blobs in the probe statement itself (join) instead of one query per hit.
- **Re-adding a grain that is already stored is a no-op instead of an error**
  (#40). A content address *is* the content, so two byte-identical grains are
  one grain — but the second insert raised
  `STO-E001: UNIQUE constraint failed: grains.hash`. `created_at` has
  millisecond resolution, so two identical events in the same millisecond
  genuinely are the same grain; an agent retrying a failing tool in a tight
  loop hit this intermittently through `record_tool_call`, the flagship
  analyzer's ingest path, and the only workaround was to corrupt the payload
  (jitter the result string) to satisfy the store. `add` now returns the
  existing address. A skipped duplicate consumes no sequence number and writes
  no op-log row — nothing changed, so nothing replicates — and duplicates
  *within* one batch collapse too.

- **A second handle on one file, in one process, is refused at open** (#50).
  The embedded backend is single-writer per file and enforces that across
  processes with an OS file lock; inside one process that lock is already held,
  so a second `open()` succeeded silently. The two handles then allocated from
  their own cached `next_seq`/`next_term` until a write collided — surfacing as
  `UNIQUE constraint failed: terms.id` on **the first handle**, long after the
  mistake, in a message naming neither handles nor writers. It is now `STO-E002`
  at open, naming the cause and the fix. Opening a handle per request or per
  agent turn is the natural move if you think of a memory file as a database
  connection; sharing one handle across threads is fully supported, and is
  documented on the Python class, in `ARCHITECTURE.md`, and in the error
  registry. The Postgres backend is unaffected — it admits multiple concurrent
  writers per memory by design. Node gains **`close()`**: Rust and Python
  release the claim on drop, but Node's drop waits for GC, so without it a
  handle that had gone out of JS scope would hold the file until then. Calling
  a method on a closed handle is an error, not a silent reopen.
- **Documentation that described a different engine than the one that
  shipped** (#37, #48, #49, #51, #54). `docs/cal-reference.md` §3.1 claimed
  *every* `RECALL` needs a subject filter or free-text query — the rule binds
  untyped (`*`/`grains`/`all`) recalls only, and the section's own
  `| COUNT` example depended on that; §4's pipeline table listed a
  `| WHERE` stage the grammar never had; the "copy-pasteable examples"
  `ASSEMBLE` printed `PRIORITY` before `BUDGET` and did not parse.
  `ARCHITECTURE.md` §2.3 omitted a required field from three of eleven grain
  shapes, so `observation`/`consent`/`skill` built from exactly their
  documented columns were rejected. `docs/cookbook.md` still called
  encryption at rest CLI-only, which stopped being true when both bindings
  gained a `passphrase` constructor argument.
- **`valid_to` set at the top level is stored as the typed field** (#36). The
  four bi-temporal validity bounds (`valid_from`, `valid_to`,
  `system_valid_from`, `system_valid_to`) are first-class common fields, but no
  builder arm claimed them, so they were swept into `common.context` — which
  compacts its keys on write, so `valid_to` came back as
  `{"context": {"vt": …}}`. Nothing reads that: `loop.staleness` looks for a
  top-level `valid_to`, and so does the store's world-time (`vf`/`vt`) column
  projection. Any bindings user setting expiry the documented way got a fact
  that silently never expires and never participates in an as-of query. The
  value now lands in exactly one place.
- **Integer literals set from CAL reach the grain** (found while fixing #36).
  Every number in the CAL AST is an `f64` and `serde_json::Number::from_f64`
  always produces a JSON *float*, so `as_i64()` returned `None` for all of them
  and every `i64`-typed field set from CAL text — `SET created_at = …`,
  `SET valid_to = …`, `SET duration_ms = …` — was discarded on the way to the
  grain builder. Fractional values are unaffected.
- **Errors name the API the caller is holding** (#52, #55). `nearest()` failed
  with "novelty check requires an embedder (e.g. `--embed-cmd`)" — a `deja` CLI
  flag that does not exist in Python or Node, and `pip install dejadb` /
  `npm i dejadb` ship no binary, so the advice was unactionable as written; the
  noun was also the CLI's `deja novelty` verb rather than the method called. It
  now reads "nearest() requires an embedder; install one with `set_embedder(fn)`
  or `set_embedder_command(cmd)`" (`setEmbedderCommand` on Node). `search()`'s
  error told you to reopen with `index_text=True`, which alone leaves a **silent
  empty result** — the re-stamp turns indexing on for future writes, so grains
  written while it was off stay invisible; it now names `reindex_text()` as the
  second step. `rebuild_text_index` no longer names a CLI flag either.
- **CAL clauses that parsed, ran, and did nothing** (#47, #53). Each of these
  failed *open* — no error, no warning, and the result feeds a model's
  context:
  - `WHERE <field> IN (...)` was never applied. The executor set
    `RecallParams::subject_in`/`relation_in`/`object_in` and nothing
    downstream read them, so the filter was dropped and every row the rest of
    the `WHERE` matched came back. `subject IN` now anchors the structural leg
    once per value (a union, not a filtered recent-scan, so a named subject
    older than `default_limit` is still found); all three are re-applied as
    membership tests.
  - **An empty `IN` set now selects nothing rather than everything.** This was
    the sharp edge: `LET $friends = …` binding to the empty set is the natural
    "no friends yet" outcome, and anyone using `LET` to scope a recall to a
    tenant, session, or user was over-fetching with no signal. An **unbound**
    `$var` is now `CAL-E008` rather than a silently dropped condition.
  - **`LET` bindings reach the `WHERE` clause at all.** The scope was
    evaluated and then dropped, so the documented two-step pattern —
    `LET $friends = SUBJECTS OF (…); RECALL facts WHERE subject IN $friends` —
    answered with everybody's preferences. Bindings resolve in declaration
    order and are inherited by `ASSEMBLE` sources.
  - `WHERE hash = "<address>"` matched nothing structurally and filtered
    nothing afterwards, so it returned the whole result set with a spurious
    `CAL-W010`; `hash IN (...)` took the other branch and returned none. Both
    resolve the envelope now, with or without the `sha256:` prefix.
  - `WITH conflict_resolution` and `WITH dedup(<field>)` were implemented on
    the `ASSEMBLE` post-merge path only, which a `RECALL` payload never
    reaches. Both work on `RECALL` now, and `dedup(<field>)` honours the field
    on `ASSEMBLE` too — so clause order no longer decides whether the option
    applies.
  - `WITH annotate_relative_time` and `WITH explanation` returned output
    byte-identical to the same query without them; both populate now.
  - `WITH score_breakdown` is inert on `RECALL` (that path returns fused
    grains, not per-leg scores) and says so with the new **`CAL-W014`** rather
    than degrading in silence. `DESCRIBE`'s `with_options` no longer
    advertises it, and lists the options that do change a recall.
- **`recommendations({"status":"all"})` returns every status** (#34). Both
  bindings filtered `"all"` out of the chain and then re-applied the pending
  default, so `"all"` behaved as `"pending"` and an applied or rejected
  recommendation was simply missing from a list both docstrings promised would
  span every state. An unrecognized status is now an error rather than a
  silent fall-back to pending.
- **A refused destructive apply no longer strands the recommendation** (#35).
  The bindings' fused `apply_recommendation` recorded the **approval first**
  and hit the destructive gate on the apply step — leaving the rec in
  `approved`, which has no exit but `applied` or `expired` (`approved →
  rejected` is not a legal transition). The reviewer could then neither apply
  nor dismiss it; the only ways out were performing the destructive change or
  waiting for expiry. `Engine::preflight_apply` now runs the scope and
  destructive checks read-only *before* the approval is recorded, so a refused
  apply leaves it `pending`. The CLI's separate verbs never had this trap.

### Changed

- **BREAKING: some grains now hash differently, so re-ingestion writes
  duplicates instead of deduplicating.** Content addressing is unchanged and
  every stored blob is untouched — but several shapes were losing or
  duplicating data on write, and fixing that changes the bytes a *newly built*
  grain serializes to. A pipeline that re-adds logically identical grains and
  relied on the content address to dedupe will now write a second copy of:

  - a **State** (the §8.3 snapshot now owns the `ctx` wire key, and a common
    context rides in `cctx`),
  - a **Workflow** carrying a `name` (now written once at top level instead of
    once there and once, unreadably, inside `ctx`),
  - a **Goal** setting any of the six fields that were silently dropped,
  - any grain with a `provenance_chain`, which was never serialized,
  - any grain built through the JSON path with a field no builder claims —
    those were written **twice**, verbatim at top level *and* compacted into
    `ctx` under a short code nothing reverses (`priority` as `pri`, `name` as
    `skname`, `status` as `ast`). The `ctx` copy is gone; the readable one
    stays.

  Nothing needs migrating: old grains remain valid and readable at their
  existing addresses. If you re-import a corpus, expect one supersession-free
  duplicate per affected grain, and `forget` the old copy if that matters.

- **BREAKING: removed public API.** `GoalTree`, `GoalNode`, `StateDiff` and
  `EngineEvent::AutoRelated` are gone from `dejadb-cal` — declarations with no
  constructor, caller, test or doc reference anywhere in the tree.
  `GrainTypeMeta::addable` is now `add_via_set` and
  `registry::addable_names()` is now `add_via_set_names()` (see Fixed, below,
  for why). `GrainType` gains a `Recommendation` variant, so an exhaustive
  match over it needs a new arm.

- **BREAKING: the rendered JSON for a State names the snapshot `context`.** It
  was `context_data`, the *Rust field name*, which never appears in a
  deserialized grain — so the key was always present and always empty. A
  consumer reading `context_data` out of `render(OutputFormat::Json)` must read
  `context`. Workflow's Markdown/plain/TOON renderings also changed shape, from
  `trigger (N nodes, M edges)` to the actual topology.

- **BREAKING: `remember()` stores an Event, not an Observation — every surface
  now agrees.** `deja remember` and the bindings wrote an **Observation** while
  the MCP `dejadb_remember` tool wrote an **Event**, for the same input. One
  operation, two grain types, so a memory written over MCP and one written from
  the CLI needed different queries to find. They now share a single write path
  (`DejaDB::capture`), and it writes an Event — the grain that models a
  transcript turn, which is what remembered content almost always is.

  This also fixes a quiet data bug: an Observation carried the text in
  `context.content`, which `projected_text` does **not** index, so remembered
  text was invisible to `deja search` and to the BM25 leg of hybrid recall. An
  Event's native `content` field is indexed, so remembered text is findable.

  What this breaks:

  - `RECALL observations` no longer finds newly remembered content — use
    `RECALL events`. Observations already in a file are untouched (grains are
    immutable), so an existing memory will hold both until you migrate it.
  - The returned JSON key `observation` is now `event` on the CLI and both
    bindings. MCP keeps `hash` and gains `event` alongside it.
  - `RememberResult.observation` is now `RememberResult.event`.
  - `DejaDB::observe()` (added earlier in this same unreleased cycle, never
    shipped) is now `DejaDB::capture()`, taking a `Capture` struct.

  `remember()`'s own signature is unchanged. `capture-stop`, which already
  wrote Events by hand, now goes through the same path.

- **`deja remember` gains `--session-id` and `--role`**, and the bindings gain
  the matching `session_id` / `role` params — the fields MCP always had. A
  remembered turn can now be recorded as part of its conversation from any
  surface (`RECALL events WHERE session_id = "..."`). The MCP tool gains
  `observer`, which the CLI always had. The two surfaces now take the same
  inputs and produce the same grain.

### Added

- **CAL 1.2 (OMS 1.5) conformance verified, and `recommendations` documented.**
  Both template features CAL 1.2 adds — the `ELEMENT` shorthand
  (`DEFINE TEMPLATE x AS "<text>"`) and the inline `FORMAT TEMPLATE "<text>"`
  — were already implemented here, and already desugar to an `ELEMENT` section
  as §10.6.1 requires. DejaDB had built what the spec's *examples* showed
  before the spec's grammar admitted them; CAL 1.2 fixed the grammar to match.
  Tests now pin the behaviour rather than the comments: the shorthand renders
  once per grain (not once per result), a named shorthand is byte-identical to
  its section form, combining the two forms is refused, and an inline shorthand
  can be aliased alongside another format. `docs/cal-reference.md` gains the
  `recommendations` row, marked read-only with its queryable fields.

- **The join: run history and semantic memory, queried together.** Every agent
  stack keeps these apart — a checkpointer holds in-thread execution state, a
  memory store holds cross-thread facts — and nothing can query across the
  seam. Here they are one substrate, so two questions become one lookup each:

  - `run_trace(ns, run_id, limit)` — what a run recorded, and (via
    `run_yield`) what it **produced downstream**: the facts and lessons derived
    from it that are not themselves part of the run. A transcript answers "what
    happened"; this answers "what did we keep".
  - `runs_touching(ns, hash, depth)` — the reverse: which runs produced or
    refined a given grain, by walking its provenance chain both ways. Runs that
    merely *read* it are not reported, and cannot be: a read leaves no grain
    behind, so nothing in an append-only store can attest to it.

  Shipped on the CLI (`deja run-trace` / `runs-touching`), MCP
  (`dejadb_run_trace` / `dejadb_runs_touching`), Python and Node.

  The write half ships with it: `remember()` takes a `run_id` on every surface
  (`deja remember --run-id`, the MCP tool's `run_id`, `run_id=` / `runId`), and
  `Capture` carries it. Without that, `run_id` was settable only by building an
  `Event` in Rust — so every non-Rust caller could ask which grains belonged to
  a run but had no way to put one there, and the reads answered empty by
  construction.

  Two narrow index tables back it: `prov_idx` (parent address → derived grains)
  and `run_idx` (run_id → grains). Deliberately not triple rows —
  `derived_from` sits on *every* grain, so indexing it as triples would inflate
  the index that recall scans.

  **A file written before these indexes existed heals itself on open**, the way
  a pre-BM25-rewrite file already rebuilds its postings, and says so through
  `open_warnings()`. Leaving it to a manual `deja reindex` would have made the
  worst failure available the default: an unindexed file answers every
  provenance and run question with an empty result, which is indistinguishable
  from an honest "nothing was derived from this". A `link_index` file-truth
  records the state — emptiness alone cannot tell "never indexed" from "nothing
  to index" — and a version bump re-heals if what the indexes hold ever widens.
  `deja reindex` still rebuilds on demand, and so do `reindex_links()` /
  `reindexLinks()` on Python and Node.

- **`grains_derived_from` no longer reads the whole store.** It scanned and
  deserialized **every grain** on each call, so one provenance question cost
  the entire corpus. Now served by `prov_idx`.

- **OMS 1.5: the Recommendation grain (`0x0C`).** A governed, auditable
  proposal to change memory or agent configuration — `target_ref`, the
  producing `analyzer`, a deterministic `summary` (`{template_id, args}`, never
  analyzer prose), a computed `dedup_key`, and exactly one proposal
  (`proposal_cal` / `proposal_edit` / `proposal_data`, modeled as an enum so
  §8.12 rule 1 is unrepresentable-if-broken). Byte values `0x01`–`0x0B` are
  unchanged, so every existing content address remains valid.

  Two details carry the design:

  - **`rec_status` is index-layer and never enters the blob.** That is what
    makes a recommendation's content address stable across its whole review
    lifecycle — propose, approve, apply and roll back never re-address the
    grain — while a change in *content* is a supersession. The identity durable
    across a supersession chain is `dedup_key`, not the content address.
  - **`dedup_key` is computed, never author-chosen**, by the normative §8.12
    rule 5 recipe (SHA-256 over NUL-separated analyzer-family, `target_ref`,
    action-kind; NFC after case-folding, and `target_ref` deliberately *not*
    folded). Two implementations must derive the same key or dedup fails on any
    imported, federated or forked store, so there is exactly one place that
    computes it.

  Query-only per CAL 1.2: `RECALL recommendations` works with its type-specific
  field set, but there is no `ADD recommendation` and lifecycle transitions
  never occur through `ADD`/`SUPERSEDE SET` — the type is engine-emitted and
  lifecycle-gated. SML 1.1's `<recommendation>` element renders it.

  Deja Loop has **not** been migrated onto the type — its recommendations still
  ride as Facts. Landing the format and rewriting a live queue are different
  risks, and they are sequenced separately.

- **Files carrying a post-1.4 grain declare `min_reader_version`.** OMS §4.5
  guarantees an additive type byte leaves existing content addresses valid; it
  says nothing about older *readers*, and `deserialize_blob` errors on an
  unknown type byte rather than skipping it — so such a file is not partially
  readable to a pre-1.5 build, it fails. The stamp turns that into a statement
  the file makes about itself, surfaced by `open_warnings()`. It cannot help
  builds that shipped before the check existed: for those the only safe posture
  is not to sync a file containing new grain types. New: `DejaDB::meta_get`.

- **The graph and as-of reads are reachable outside Rust.** `related`
  (bounded k-hop entity walk), `entity_at` (two-axis as-of read) and
  `step_actions` (workflow execution records) existed only as store methods —
  no CLI verb, no MCP tool, no binding — so the bounded traversal and the
  bitemporal read, arguably the two strongest capabilities in the engine, were
  invisible to every user who wasn't linking the crate. All three now ship on
  MCP (`dejadb_related` / `dejadb_entity_at` / `dejadb_step_actions`), the CLI
  (`deja related` / `entity-at` / `step-actions`), Python and Node, with the
  same scalars-in/JSON-out shape and the same `out|in|both` and
  `world|knowledge` vocabulary everywhere. No new CAL syntax: that is an OMS
  conformance decision, not a wiring task.

- **Workflow execution records — OMS §8.4 `mg:step_action:<node_id>`.** A
  Workflow grain is immutable and content-addressed, so it can never accumulate
  run state; the spec's answer is to point the execution records at the plan
  instead. `grain.step_action(workflow_hash, node_id)` attaches that link to a
  Tool grain, and `DejaDB::step_actions(ns, workflow_hash, node_id, limit)`
  reads it back as `(node_id, executing grain)`. Combined with the now-queryable
  `Event.run_id` and a working State checkpoint, a run can be persisted and
  reconstructed against its plan without a new grain type.

- **`related_to` cross-links are indexed.** The field has always serialized, but
  `related_to` appeared nowhere in `dejadb-store` — links were written and
  unreachable. They now index into `triples`/`osp`, subject-ed on the linking
  grain's own hash, so the existing `related()` traversal reaches them from
  either end. They are deliberately **not** written to `heads`/`entity_latest`:
  OMS §15.3 is normative that a `related_to` link is an annotation and MUST NOT
  change the target's supersession state. `osp` is unconditional for links
  (their object is always a grain hash, hence always an entity) regardless of
  the file's `entity_relations` declaration. **Existing files need `deja
  reindex` before old links become queryable.**

- **`to_state()` and `to_workflow()` typed reconstructors.** There was no way to
  get a `State` or a `Workflow` struct back out of a blob — the only typed
  reconstruction was `to_fact`/`to_event`/`to_tool`/`to_skill`, so every reader
  of a workflow hand-parsed raw JSON. `to_workflow()` restores the full topology
  (nodes in order, edges with conditions and cycle bounds, tool bindings,
  retries); `to_state()` restores the snapshot plus `plan`/`history`.

- **State grain: `plan` and `history` (OMS §8.3).** The struct previously had
  only `context`, so a caller with an ordered plan or a prior-state list had
  nowhere to put it.

- **`remember()` can extract facts with an LLM.** `remember` always took free
  text, but distilling it into Facts was a Rust-only closure seam — the CLI and
  both bindings could pass *pre-extracted* facts and nothing else (the CLI said
  so in a comment: "the CLI can't run an LLM"). `deja remember --content "..."
  --model openai:gpt-4o-mini` now does the extraction, as do `model=` /
  `llm_cmd=` on the Python and Node bindings. Extraction rides the existing
  Deja Loop wire protocol as a new `extract` op, so all three shipped providers
  (OpenAI-compatible / Anthropic / Ollama) plus the `--llm-cmd` subprocess
  escape hatch work with no new provider code, and no new dependency.

  A model writing its own output into memory is exactly the failure mode this
  engine exists to prevent, so the write is shaped around that:

  - The raw text is stored **before** the model is called. A failed or
    unreadable extraction costs the facts, never the source text — the hash is
    still reported so the extraction can be retried against it.
  - Extracted facts are stamped `verification_status="unverified"` and carry
    `extractor_model` naming the model that wrote them, alongside the existing
    `derived_from` / `source_type=derived`. `verification_status` is
    CAL-filterable, so `RECALL facts WHERE verification_status = "unverified"`
    is a review queue rather than a pile of new writes.
  - `--ground-model` / `--ground-cmd` adds an opt-in entailment pass in a
    *separate* call (proposer ≠ scorer, as in the Deja Loop verifier): unsupported
    facts are dropped, survivors become `"verified"`.
  - Drops are never silent — the output accounts for `proposed` vs `dropped`
    across the `--min-confidence` floor and the grounder, and a response that
    hits the per-call fact cap says so on stderr.

  Also `--dry-run` (extract and print, store nothing) and `--extract-hint`
  (steer extraction toward your domain). MCP's `dejadb_remember` is unchanged
  and deliberately has no extraction — there the client is already a model.

  New surface: `dejadb_llm::{extract_facts, ground_facts, extract_pipeline}`,
  `DejaDB::{capture, attach_facts}` + `Capture` / `FactAttribution`, and
  `FactDraft::from_json_array`. `remember()`'s signature is unchanged.

### Fixed

- **`forget` left the join's indexes behind, and a reused `seq` inherited
  them.** `forget` reconciles every index — triples, osp, embeddings, threads,
  postings, heads, entity_latest — but the two new tables were not added to
  that list. `seq` is re-derived as `MAX(seq)+1` on open, so forgetting the
  newest grain hands its seq to the next write, and the orphaned row then
  re-attached a completely unrelated grain to the forgotten one's parent and
  run: `grains_derived_from` returned a grain with no `derived_from` at all,
  and `run_trace` returned one that was never in the run.

- **`step_actions` returned an arbitrary subset in an arbitrary order.** With
  no `node_id`, the predicate set comes from a dictionary prefix scan over a
  `HashMap` and each predicate was queried and capped separately, so results
  arrived grouped by node in an order that varied per process — and the cap
  kept whichever group came first rather than the newest records. Documented as
  "newest first", which it now is.

- **`WITH multi_hop(n)` only walked forward.** Entities were harvested from a
  result's `subject` *and* `object` but each was re-anchored as a subject only,
  so an entity reached through the object position was a dead end: "who else
  reports to this manager" — one hop, the archetypal graph question — returned
  nothing, while the forward direction worked and made the feature look
  correct. Each entity is now followed both ways, the reverse leg via the OSP
  index (so it covers the relations the file declares entity-valued, the same
  rule every other reverse traversal follows). New: `grains_by_object`.

- **A State's common context was dropped instead of merely yielding.** The
  §8.3 snapshot rightly owns the `ctx` wire key, but the §6.1 common context
  was then discarded with no error — and that is where `merge_heads` records
  merge parents and the importers record provenance, so a merged or imported
  State lost that record at the blob boundary. It now rides in `cctx` and
  `to_state()` restores it. Only a colliding type pays; a Fact still keeps its
  common context in `ctx`.

- **`WITH auto_relate` warned under a code that means something else.** It
  reused `CAL-W004`, which is `UnknownExtensionOption` — a code has to locate
  one variant to be worth reporting. Now `CAL-W013`. The message also carried
  three runs of ~22 spaces from a string literal missing its line
  continuations.

- **Dead code removed.** Deleted `GoalTree`, `GoalNode`, `StateDiff` and the `AutoRelated` engine
  event — declarations with no constructor, caller, test or doc reference
  anywhere in the tree.

- **`WITH multi_hop(n)` now does something.** It was lexed, parsed, clamped to
  1–3 and written to `RecallParams` — and never read by any recall path, so the
  option documented as "entity-graph multi-hop retrieval" was a no-op. It now
  takes the entities named by the first-pass results, anchors a fresh recall on
  each, and adds what comes back to the candidate pool, repeating per hop.
  Expansion runs *before* post-filtering and `LIMIT`, so hops compete for the
  slots the caller asked for instead of extending past them, and a direct match
  always outranks something reached by association. Fail-open, like every other
  recall refinement.

- **`provenance_chain` was silently dropped on every write.** OMS §6.1 defines
  it (`pc`, the derivation trail) and `GrainCommon` declares it with a compact
  key, but nothing serialized it — anything a caller recorded there vanished at
  the blob boundary. Now written, uncompacted per §6.2. Optional, so existing
  blobs stay byte-identical.

- **`WITH auto_relate` says it is not implemented.** Parsed and accepted since
  1.0, never consumed by any store path. It stays in the grammar — it is
  documented, and removing it would turn working queries into parse errors —
  but now emits CAL-W013 instead of silently doing nothing.

- **`GrainTypeMeta::addable` renamed to `add_via_set`, and its doc corrected.**
  The field claimed to gate "creation via `ADD` / the `add` HTTP+SDK path", but
  the HTTP/SDK path never consulted it — so it read like a permission boundary
  that leaked. It is not a permission at all: it records whether a type can be
  built from a flat list of `SET k = v` pairs, and it has exactly one consumer,
  the `CalStatement::Add` arm. Types marked `false` are created through
  purpose-built paths that validate structure themselves — a dedicated CAL
  statement (`ADD workflow … build -> test`), the per-type JSON builders behind
  `cal_add`, or a host API such as `capture()`. Those are deliberately not
  gated; gating them would break `remember()`, the memory-tool adapter, the
  `migrate` importers, and State checkpointing. Values are unchanged; a test now
  pins the invariant in both directions. Access control remains scopes and
  `allow_destructive_ops`.

- **The State (`0x03`) and Workflow (`0x04`) grains now actually work.** Both
  types were writable from CAL, MCP and both bindings, but no test crossed the
  parser boundary — every workflow test asserted on the AST and stopped. That
  gap hid a family of read/write asymmetries:

  - A State's snapshot was **destroyed on write**. OMS §8.3 gives State a
    required `context` map; §6.1 gives every grain a common `context`. Both
    compact to the wire key `ctx`, and common fields were serialized *after*
    type-specific ones with an unconditional insert — so the common metadata map
    replaced the entire snapshot. Type-specific fields now win on collision.
  - `add("state", {"context_data": …})` was accepted and silently stored an
    **empty** snapshot: `context_data` was listed as a known field but the
    builder only ever read `data`. `context` (the spec name) is now primary,
    with `data`/`context_data` as aliases.
  - State never rendered its payload. Five sites across `render.rs` and
    `assembly.rs` read `fields["context_data"]` — the *Rust field name*, which
    never appears in a deserialized grain — so `state_label()` always fell
    through to the literal `"state"`.
  - `plan` and `history` (OMS §8.3) had **no struct field at all**, and the
    registry advertised a `checkpoint_data` field that existed nowhere: no
    struct field, no serializer, no deserializer. It was never storable.
  - Workflow `name` was routed into `common.context`, where `field_str` — which
    only reads top-level fields — could not see it, so every shipped `{{name}}`
    template rendered blank. The graph (`nodes`/`edges`/`bindings`/`retries`)
    was also written **twice**: typed at top level and verbatim inside `ctx`.
  - An edge's `max_cycles` came back off the wire as the raw compacted key
    `mxc`, so every reader looked for a key that was never there and **cycle
    bounds silently vanished on read**.
  - Renderers emitted only `trigger (N nodes, M edges)` — an agent that
    recalled a workflow got its size and never its shape. They now render the
    topology (`build -> test [when …]`), eliding past 12 edges.

- **Six Goal fields were silently dropped on every write.**
  `criteria_structured`, `expiry_policy`, `recurrence`, `evidence_required`,
  `rollback_on_failure` and `allowed_transitions` had struct fields *and*
  compact keys in `field_map.rs`, but no arm in `add_type_specific_fields`, so
  the data never reached the blob. All are optional, so unset stays unset and
  existing Goal blobs remain byte-identical.

- **`Event.run_id` is queryable.** It has been serialized since 1.0 but was
  absent from the registry's `queryable_fields`, making it write-only —
  storable, and impossible to ask for back. `RECALL events WHERE run_id = …`
  now works, and the JSON write path sets the typed field (wire key `rid`)
  instead of leaking it into both `extra_fields` and `context`. Note this is a
  post-filter over a bounded scan, not an index.

- **`remember` no longer stores empty-field Facts from malformed input.** The
  `--facts` / `facts_json` parse, duplicated across the CLI and both bindings,
  read each field with `unwrap_or("")` — so a row missing `object` became a
  Fact with an empty object rather than an error. The three copies are now one
  `FactDraft::from_json_array`, and an incomplete triple is rejected with
  `VAL-E001` naming the offending row.

- **`WITH superseded` now works on anchored recalls, and labels what it
  returns.** The option only ever had an effect on the *unanchored* path
  (`RECALL facts RECENT 20`); the moment a query carried a subject or an
  `ABOUT` clause it became a silent no-op, because all three legs underneath
  were hard-wired to the heads — the structural probe filtered `cur=1`, BM25
  dropped non-live postings after scoring, and the vector leg joined on
  `svt IS NULL`. So one option meant two different things depending on an
  unrelated clause, and the documented behavior ("include historical grains")
  was true only by accident. All three legs now widen.

  The half that matters more: superseded grains come back **marked**.
  Supersession is index-layer state and the blob carries no trace of it, so
  history returned unlabeled would hand a model outdated values that read as
  current — a worse answer than not returning them. Recall now stamps
  `superseded_by` on each stale version, which surfaces in the JSON payload and
  as `(superseded)` / `<superseded_by>` in the rendered formats.

  This also closes a real gap rather than just a promise: text that survives
  *only* in a superseded version was previously unfindable by search at any
  limit — `RECALL facts ABOUT "tea" WITH superseded` now finds it.

  Forgotten grains stay forgotten on the widened path: `forget` deletes index
  rows outright rather than flagging them, and there is now a regression test
  pinning that a tombstone survives the wider scan.

  New store surface: `RecallTuning::include_superseded`, `search_text_all`,
  `search_vector_all`, and `DejaDB::supersession_map`. Adding a public field to
  `RecallTuning` breaks struct-literal construction — use `..Default::default()`.

## [1.0.5] - 2026-08-04

> **Node users: this release breaks every call site.** `dejadb` on npm went
> from synchronous methods to promises, so `mem.recall(...)` now returns a
> `Promise<string>` rather than a string and existing code fails at runtime
> (typically `JSON.parse` on a promise). Add `await` — see the first entry
> below. Shipped as a patch version to keep all three registries aligned, so a
> `^1.0.4` range will pick it up; pin `1.0.4` if you are not ready. The Rust,
> Python and CLI surfaces are unaffected.

### Changed — breaking

- **Every `dejadb` Node method now returns a promise.** They used to run inline
  on the thread calling into the addon — which in Node is the thread running
  everything else — so a single `migrate` or `importBundle` held the event loop
  for its whole duration and timers, sockets and any HTTP server in the process
  stopped until it returned. Store calls now run on libuv's thread pool.
  Measured on a 400-record `migrate`: a 5 ms timer fires **263 times** during
  the call; before, it could not fire at all.

  **Every call site needs `await`** (or `.then`), and errors arrive as
  rejections rather than throws. The constructor stays synchronous, so opening a
  file still fails at the line that opened it. One trap worth stating outright:
  promises settle in completion order, not call order, and concurrent calls
  contend for one lock inside the store — **await your writes** before the read
  that expects to see them.

  This is a major-version change for the npm package; the Rust, Python and CLI
  surfaces are untouched.

### Changed

- **The BM25 leg is now DejaDB's own inverted index, not Turso's `USING fts`.**
  With that index in place a single-row `INSERT` cost time proportional to the
  rows already stored (1.6 ms at 500 grains, 57.9 ms at 3,000, still climbing;
  batching did not amortize it), and a `MATCH` lookup cost the same whether one
  row matched or every row did. Reproduced against bare `turso` with no DejaDB
  code involved, and identical on `0.8.0-pre.2`, so it was not a version bump
  away: [tursodatabase/turso#8170](https://github.com/tursodatabase/turso/issues/8170).
  After: writes are flat (~0.4 ms at 4k grains) and a rare-term lookup is flat
  (~0.4 ms); a term matching *every* document costs more as the corpus grows,
  which is correct — cost now tracks matches rather than file size.

  Scoring is textbook BM25 (`k1=1.2`, `b=0.75`). Tokenizing is deliberately
  plain — lowercase, split on non-alphanumeric — with no stemming and no
  stopwords, so results never depend on a language guess. **Ranking will differ
  from the old index**, which stemmed. Existing files self-heal: the first open
  rebuilds the index and reports it in `open_warnings()`.

  **This layer is meant to be deleted when the upstream issue is fixed.**
  `docs/facts/bm25-index.md` is the removal instructions, and the acceptance
  test for deciding the fix is real.
- The Hermes memory provider now defaults `index_text` **on**, reversing the
  default it shipped with hours earlier — that default only existed to dodge
  the write cost above, and there is no longer anything to dodge.

### Added

- **DejaDB as a Hermes Agent memory provider** (`examples/hermes/`), verified
  against Hermes 0.16.0 through its real plugin loader, ABC and `MemoryManager`.
  `prefetch()` is one budgeted CAL `ASSEMBLE` — **p50 0.83 ms at 2,080 grains**,
  which matters because that hook sits on Hermes's synchronous turn path.
  `MEMORY.md`/`USER.md` edits are mirrored as immutable grains, so wording an
  agent consolidates away stays recallable. Ships with its limits written down:
  Hermes never notifies providers of a `remove` (only `add`/`replace`), skills
  are out of reach of a memory provider, and the plugin defaults `index_text`
  **off** — the opposite of DejaDB's own default — because a per-turn writer
  cannot absorb an index whose cost grows with the file.

- **`DejaDB(..., index_text=True|False)` in the Python binding**, matching the
  CLI's `--index-text`. Left unset, the file's own declaration still wins;
  passed explicitly it is a deliberate re-stamp and the change is reported via
  `open_warnings()`. Turning it off is how a host trades the BM25 leg for write
  latency that does not grow with the file — measured 300 writes in 98 ms with
  it off, against a per-write cost that climbs past 60 ms at 4k grains with it
  on (tursodatabase/turso#8170). Previously Python could only take the file
  default.
- **`search(query, subject=None, relation=None, k=10)` in the Python binding** —
  free-text recall over the BM25 and vector legs, fused with the structural leg
  when anchored. The same path as `deja search` and CAL's `RECALL ... ABOUT`.
  `recall()` needs a subject you already have; reaching this otherwise meant
  hand-writing CAL. With **neither** leg available it raises rather than
  returning `[]`, which would read as "no matching memories" when the truth is
  that the file cannot answer free-text queries at all.
- **`add_batch(grains_json)` in the Python binding** and `cal_add_batch` on
  `DejaDbFacade` — many grains in one store transaction, validated up front so
  a malformed entry writes nothing. Worth ~1.6x over one-at-a-time adds (244 ->
  148 µs/grain at 2k grains), saturating around a batch of 10, and **only with
  the text index off**: with it on, per-row index cost swamps batching entirely
  (~17 ms/grain at every batch size). For another system's export, `migrate()`
  remains the better path — it already defers and rebuilds the index.

### Fixed

- **`EXPLAIN` no longer misreports a `LIKE` recall.** `LIKE` and `ABOUT` are the
  same free-text leg at execution — both set the recall's single query — but the
  plan was derived from `ABOUT` alone, so `EXPLAIN RECALL facts LIKE "x"`
  described a structural `O(n) full scan` over no index while actually running
  BM25. Both spellings now report the same plan (`bm25`, or `hybrid_rrf` when
  anchored). A planner that misreports is worse than one that says nothing.
  `cal-reference.md` now states outright that `LIKE` is `ABOUT` spelled for the
  SQL-familiar, not a substring filter.

- **`dejadb-py` no longer holds the GIL across store calls.** Every method
  reaching the store now runs it inside `py.detach(...)`, so the interpreter
  lock is released for the duration. Previously a single call held it end to
  end: measured on a 600-op `import_bundle`, another Python thread was starved
  for **3,266 ms of a 3,266 ms call** — it never ran. The practical effect was
  that a host moving writes onto a background thread (what agent frameworks do
  so a slow write cannot stall the next turn) got no isolation at all, and a
  concurrent reader blocked on the store mutex *while holding the GIL*, which
  froze every other thread too. Cheap methods detach as well, because it is the
  waiting on the mutex — not just the work — that has to happen outside the
  GIL. Same 600-op call after the fix: worst starvation 7 ms. Guarded by
  `test_store_calls_release_the_gil`, which asserts thread interleaving rather
  than a wall-clock threshold.

## [1.0.4] - 2026-08-04

### Changed — breaking

- **`FORMAT markdown` renders assertions, not field dumps.** A grain used to
  render as a `### fact (4451640f)` heading plus one bullet per field
  (`- **namespace**: personal`, `- **created_at**: 1768262400000`). It now
  renders as the sentence the memory asserts —
  `- **john** prefers window seat *(0.95, 2026-01-13)*` — keeping only the
  metadata that changes how much weight to give it. The point of this format is
  that its output can be pasted into a prompt, and storage bookkeeping
  (namespace, grain type, raw epochs, the content address) is noise there.
  **Anything parsing the old shape must be updated**; use `FORMAT json` for a
  machine-readable envelope, or a custom `FORMAT TEMPLATE` to pick fields
  yourself. Grouped renders keep their group heading.
- **The template body limit is 4 KiB, down from 64 KiB** (OMS CAL §10.8,
  `CAL-E040`), alongside new caps on nesting (5, `CAL-E117`), templates per
  file (50, `CAL-E118`) and `{{#each}}` iterations (200, `CAL-W011`). A
  template already in a memory file that exceeds a limit is **skipped on load
  rather than failing the open**, and reported — see *Fixed* below.
- **`FORMAT TEMPLATE "<text>"` goes through the template engine.** It used to
  be naive string substitution that replaced `{{any_field}}` with whatever the
  grain's JSON held, silently ignoring filters, conditionals and the closed
  variable set. It is now parsed and validated like any other template, so a
  variable outside that set is an error instead of a silent blank, and filters
  and conditionals work. Inline bodies referring to fields that were never
  valid template variables will now fail loudly.

### Added

- **Sectioned templates and the §10.5 variable namespaces (OMS CAL §10.5–§10.8).**
  `DEFINE TEMPLATE <name> [EXTENDS <parent>] HEADER { … } ELEMENT { … }
  ELEMENT_SUMMARY { … } ELEMENT_OMIT { … } SOURCE_BREAK { … } FOOTER { … }`
  inverts who drives iteration: the engine walks the grains, which is what lets
  it pick a different body per grain and interleave `SOURCE_BREAK` between
  `ASSEMBLE` sources. Section bodies are captured raw by the lexer, so a
  template may contain prose that is not CAL (`don't`, a lone quote, a word
  that happens to be a keyword). Sections you do not define are inherited from
  `EXTENDS`, defaulting to `readable`; `data` cannot be extended (`CAL-E119`).
  Adds `{{grain.*}}`, `{{assembly.*}}`, `{{source.*}}`, `{{budget.*}}`,
  `{{disclosure.level}}`, the §10.3.2 content-projection model,
  `humanize()` on relations, and relative-time rendering.
- **New `FORMAT` spellings.** `FORMAT TEMPLATE <name>` (registered),
  `FORMAT TEMPLATE "<text>"` (inline `ELEMENT` shorthand), `FORMAT TEMPLATE
  { … }` (inline sections); the §10.1 semantic presets `structured` /
  `readable` / `compact` / `data` as aliases for `sml` / `markdown` / `text` /
  `json`; `AS <format>` as a synonym for `FORMAT <format>` (§7 `as_clause`);
  and `RECALL *` as the explicit spelling of "any grain type".
- **Saved queries and custom templates persist in the memory file.** They ride
  the `meta` table as `qry:<name>` / `tpl:<name>` rows — host metadata, not
  memories: never grains, never content-addressed, never returned by recall.
  They travel with the `.db`, so the CLI, MCP and console see one set.
  New `DejaDB::meta_scan/meta_put/meta_delete`; `DEFINE`/`RUN` join
  `CalCapabilities::supported_statements`.
- **`CONTRADICTIONS` is wired to the executor — an agent can now ask what it
  holds that is still disputed.** When two writers change the same
  `(subject, relation)` and later sync, both versions survive as live heads and
  recall answers with a deterministically-elected *provisional* head — a value
  that looks settled but isn't. Finding those forks previously required an
  operator to run `deja forks`. Two CAL surfaces now expose it in-query:
  `RECALL … CONTRADICTIONS` returns **only** contested grains, optionally scoped
  by an `OF (sub-query)` tail; `WITH contradiction_detection` returns the normal
  result set with the disputed parts marked. Both stamp `contested_by` on each
  grain — the hashes of the other live tips — so a model sees *what* disagrees,
  not merely that something does. The clause applies after every other filter,
  so it composes with `ABOUT`/`WHERE`/`SINCE`. Costs one
  `GROUP BY … HAVING COUNT(*) > 1` over `heads` per query and only when asked
  for, leaving the microsecond recall path untouched; fail-open like the rest of
  recall, except that a *filtering* query yields nothing rather than a false
  all-clear. Reaches every surface that speaks CAL (`deja cal`, the MCP
  `dejadb_cal` tool, the console, both bindings). Detects **structural**
  contradiction only — semantically incompatible facts that were never forked
  remain Deja Loop's job. New `CalStoreFacade::open_forks()` (default: no forks) and
  `store_types::ForkGroupInfo`. Covered by ten end-to-end tests in
  `crates/dejadb-cal/tests/cal_integration.rs`.
- **`deja hub`** — the sync hub (`dejad`) as a CLI verb: many apps, one shared
  memory, segment push/pull, default `127.0.0.1:7438`. Unlike the console this
  is a network service by construction, so `--token-env` is **mandatory** and a
  non-loopback bind still needs `--allow-remote`. Segment reads are gated too,
  not just pushes; a pushed segment can only ever *add* grains.
- **A redesigned web console** for non-technical reviewers: a plain-language
  memory browser with an interactive graph, the Deja Loop review queue, a CAL
  workbench (cards, table, graph, formats, history, saved queries), and a
  Developer-mode toggle that reveals hashes, the op log and CAL. Still one
  embedded `console.html` with no build step.
- **`dejadb.helpers`**, shipped inside the Python wheel: `fresh`, `facts` /
  `show_facts`, `recs` / `show_recs`, `audit`, `outcomes`, `days_later`
  (a context manager over the `DEJA_LOOP_NOW_MS` clock-pin seam), `auto_model`
  and `bar`. Imported explicitly — the core surface is still exactly the native
  class. `dejadb-py` moves to maturin's mixed layout, so the native module is
  now `dejadb.dejadb`; `import dejadb` is unchanged.
- **Six Colab notebooks** under `examples/colab/` — the full tour plus five
  business-scenario walkthroughs — each executed end to end with outputs baked
  in. They require `dejadb >= 1.0.4` for the helpers.
- New error codes: `CAL-E117` (template nesting), `CAL-E118` (templates per
  file), `CAL-E119` (cannot extend `data`), `CAL-W011` (`{{#each}}` cap),
  `CAL-W012` (bounded `CONTRADICTIONS` scan).

### Fixed

- **An unanchored `WHERE` applies its filters, and serves heads.** A `RECALL`
  with no subject and no free-text query falls to a recent-by-type scan, which
  takes no structural predicates — so `RECALL facts WHERE relation = "x"`
  silently dropped the filter and answered with **every** grain of that type.
  Returning more than was asked for is worse than returning nothing: nothing in
  the result tells the caller the filter never ran. The same scan read the
  grains table straight through, and supersession is index-layer state, so a
  stale version came back next to the head that replaced it, both presented as
  current. `relation` is now applied on that path and the scan serves heads
  only; `WITH superseded` opts the full chain back in (on this leg — the
  anchored leg followed in the next release, see Unreleased).
  New `DejaDB::recent_live`, so scans that legitimately want
  every version — Deja Loop's analyzers — keep `recent` unchanged.
- **`CONTRADICTIONS` no longer answers "nothing is contested" about a memory
  that is.** The clause filters after recall, so the recall's `LIMIT` — 50 by
  default — also bounded which forks could be seen: a fork sitting below the
  newest 50 grains was invisible, and the query returned a clean-looking empty
  result. The candidate scan now widens to the executor's `max_limit`, `LIMIT`
  applies to the *contested* grains rather than to the search for them, and a
  scan that still hits the ceiling is announced as `CAL-W012` — this is the one
  clause whose empty answer an agent is meant to trust.
- **`CONTRADICTIONS OF (...)` no longer leaks across namespaces.** A fork is
  keyed `(namespace, subject, relation)`; the scope set was keyed on
  `(subject, relation)`, so an unrelated grain in another namespace sharing the
  pair pulled a fork into scope.
- **Single-source `ASSEMBLE … FORMAT` binds the assembly variables.** It
  rendered with an empty plan, so `{{assembly.*}}`, `{{source.*}}`,
  `{{budget.*}}` and `ELEMENT_OMIT` came back blank — populated on the
  multi-source path and silently empty here. (`budget.unit` reports `grains` on
  this path, which is what it budgets by.)
- **A saved query or template the file carries but this build cannot load is
  reported, not silently dropped.** Skipping one bad row is right — it must not
  make the memory unusable — but it is now surfaced through `GET /api/config`
  and on stderr from `deja cal` / `repl` / `serve` / `ui` / `hub`, so a
  shrinking set of saved queries is something you are told about.
- **A template's `FOR` clause survives a reopen.** It was written to the file
  and then dropped on the way back in.
- `DROP QUERY` / `DROP TEMPLATE` roll the in-memory registry back when the file
  write fails, so it never runs ahead of what is persisted — matching what
  `DEFINE` already did.
- A template rendered twice in the same second no longer costs a write
  transaction per render on the read path (`last_run_at` has one-second
  resolution, so the second write stored what was already there).
- `humanize()` time buckets only ever coarsen with age: 29 days read as
  "4w ago" and 31 days as "31d ago"; the 30-day-to-a-year bucket is now months.
- `DejaDB::meta_scan` escapes `%` and `_` in its prefix, so a prefix containing
  a `LIKE` wildcard cannot match rows it does not own.
- `dejadb.helpers.fresh` removes exactly the memory file and its known
  sidecars. It globbed on the name as a prefix, so `fresh("h.db")` also deleted
  a neighbouring `h.db.backup` — and `rmtree`'d a neighbouring directory.
- `dejadb.helpers.days_later` restores a pre-existing `DEJA_LOOP_NOW_MS` instead
  of unsetting it, so the context manager nests.
- An `ASSEMBLE` no longer holds a second copy of its whole result set: the
  budget trim splits the grains it already owns instead of cloning both the
  kept prefix and the dropped tail.

## [1.0.3] - 2026-08-01

### Added

- **Edge benchmark numbers, measured on two real devices** —
  `crates/dejadb-bench/scripts/edge_bench.py` runs the read/write/vector paths
  on the device itself and emits the markdown for
  [RESULTS.md §6](crates/dejadb-bench/RESULTS.md). Measured on a **Raspberry
  Pi 3 B** (Feb 2016, $35, 1 GB, 1.2 GHz Cortex-A53, microSD) and an **Intel
  NUC8i3BEH** (2018, i3-8109U, NVMe): recall is **flat in corpus size** on both
  — 348→361 µs on the Pi and 29→30 µs on the NUC across 500→8,000 grains — with
  the NUC matching the M4 Max figure in §5 *through* the Python binding's FFI.
  Single-grain writes against a live FTS index degrade to 201 ms (Pi) / 24.5 ms
  (NUC) while deferred-index bulk import stays flat at 3.9 / 0.39 ms per grain;
  storage explains why better hardware does not close that gap (durable 4 KiB
  writes improve only ~4× from SD to consumer NVMe). Vector recall works on
  both, but the embedding model is 96–98% of it, so prefer BM25 or off-device
  embedding on slow silicon. README and FAQ now state arm64/x86 edge support
  concretely, including that `cargo install` cannot link in 1 GB of RAM (2.0 GiB
  peak) — use the wheel or cross-compile. The harness samples the CPU clock
  throughout every phase and asserts that recall benchmarks actually hit, so
  every published figure is at the device's rated clock.
- **Regression tests** — opening an encrypted memory without the passphrase
  must point at the `.kdf` sidecar and `--passphrase-env` (the 1.0.2 fix for
  issue #16 shipped untested); and the LLM-provider HTTP path now has a
  loopback mock-server test (request line, headers, body, JSON round-trip,
  status-error mapping) instead of being exercised only at compile time.

### Changed

- **LLM-provider HTTP client migrated to ureq 3** (`dejadb-llm`). Same
  blocking, dependency-light posture. The single read timeout now applies to
  each receive phase separately (awaiting the response, then the body — a slow
  LLM still gets the full 120 s in each), and a response over the 10 MiB cap
  is now an error instead of ureq 2's silent truncation. Non-2xx statuses
  still surface as `LlmBackend` errors naming the URL.
- **Dependency refresh**: turso 0.7.1 (storage engine — recall and write
  latency re-measured at parity with the RESULTS.md baselines before
  merging), tokio 1.53.1, serde_json 1.0.151, `@napi-rs/cli` 3.7.4,
  `actions/setup-python` v7.
- `THIRD-PARTY-NOTICES.md` now attributes all runtime direct dependencies
  (added argon2, zeroize, getrandom, ureq).

## [1.0.2] - 2026-07-27

### Added

- **`deja recall-hook --with-loop`** — the UserPromptSubmit hook now closes
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
- **Deja Loop bindings parity** — Python/Node gain `rollback_recommendation`,
  `loop_outcomes`, and `loop_run(full_sweep=…, policy=…)`: the full-memory
  `reflect` semantics and the host policy file (the only auto-apply path) are
  now reachable from the bindings.
- **Host policy on every run surface** — `deja ui --policy` and
  `deja serve --mcp --policy` (or `$DEJA_LOOP_POLICY`) attach the same
  `loop-policy.json` the CLI takes, so console- and MCP-triggered runs honor
  one set of grants; never controllable by a client. The console's Deja Loop tab
  states it; the `dejadb_loop` tool description no longer implies the CLI
  and MCP engines are identical (LLM reflection remains CLI-only).
- **`examples/analyzers/`** — a ready-to-run external command analyzer (a PII
  scan in dependency-free Python) with the probe/analyze protocol documented
  inline; validated live against the demo corpus.
- **`loop_reflection` results table in RESULTS.md** — the Effective-
  Reliability machinery numbers (verifier lifts ER +0.00 → +1.00 on the
  reference corpus) are now recorded alongside the analyzer-precision table.

- **Deja Loop recall-telemetry sidecar (§8).** A disposable, never-syncing
  `<file>.telemetry.db` records what recall actually surfaced — grain access,
  query outcomes, assembly-budget pressure — so Deja Loop can see memory *utility*,
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
- **Optional LLM enrichment (§9).** `deja loop run --llm-cmd 'CMD'` attaches a
  subprocess backend (`CommandLlm`, mirroring `--embed-cmd`) that only *adds* —
  DISCOVER proposes cited `origin=llm` drafts (never auto-applied), ENRICH adds
  a whitelisted guidance note; with no backend the stages are the identity, so
  the deterministic output is unchanged. Backends in `examples/llm/`. New error
  `LOP-E050`.
- **Console Sessions + Setup views** and `GET /api/loop/telemetry`: visualize
  recall activity, coverage gaps, and the effective configuration.
- **Deja Loop reflection verifier + measurement** (design:
  `docs/loop-reflection.md`). The LLM path is no longer "cite a real hash and
  hope": DISCOVER runs under an abstention-legitimate objective, then every
  draft passes an independent **GROUND** (evidence-entailment) and **VERIFY**
  (adversarial keep/kill) gate — each a separate call (proposer ≠ scorer) —
  before it can reach the review queue, stamped with the verifier's calibrated
  confidence. Measured, not asserted: a `loop_reflection` Effective-Reliability
  bench (the verifier lifts ER from +0.00 to +1.00 on the reference corpus by
  filtering decoys) and a live approval-rate metric on `deja loop`.
- **Out-of-box LLM providers** (`dejadb-llm` crate): `deja loop run --model
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
  are also exposed on the Python and Node `loop_run`.
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
- **Full-memory reflection sweep** (`deja loop reflect`): re-analyze the whole
  memory in one pass, ignoring the incremental watermark, for a first look at an
  imported memory or a periodic deep pass. Dedup/cooldowns still suppress what is
  already queued and the watermark still advances, so later runs stay incremental.
- **Writable console Setup**: toggle analyzers on/off from the console, persisted
  to the file's loop config (`POST /api/loop/config`, Admin-gated like every
  write). `GET /api/loop/analyzers` now returns effective settings + trust
  class. Auto-apply is still only grantable via a host policy file, never the UI.

### Fixed

- **Correctness hardening (feature-combination bug-hunt).** Serialization now
  rejects non-finite floats and non-canonical trailing-byte blobs at the
  write⇒read boundary, preserves nested user-JSON keys that collide with OMS
  short codes, and NFC-normalizes map keys. `forget()` reconciles the fork-head
  index; `supersede` and `merge_heads` are single-transaction atomic; a
  supersession replicates correctly across two hops. Hybrid recall's FTS and
  vector legs fail open (never error) on hostile query text or an embedder
  dimension mismatch. `get_blob` validates the `cas://` URI instead of
  panicking. Census + JSON renders one valid array, and TOON rows no longer
  carry a `[CURRENT]`/`[OUTDATED]` marker that corrupted the column schema.
- **CAL scoping.** Saved-query bodies stay read-only even when parameterized,
  and a nested `ASSEMBLE`'s `WITH` recall-tuning options scope to that assemble
  instead of leaking to the enclosing `EXPLAIN`/`COALESCE` query.
- **Deja Loop.** Tool-failure lessons re-measure against their exact failure
  signature, so an unrelated later failure of the same tool no longer reverts a
  valid lesson; rejection cooldowns now back off exponentially; empty-signature
  clusters and auto-apply consolidations that would drop an expiry are refused;
  mem0 history re-import is idempotent for delete-terminated chains.
- **The npm Linux addons no longer require a bleeding-edge glibc.** 1.0.1's
  `dejadb-linux-x64-gnu` / `dejadb-linux-arm64-gnu` were built straight on
  `ubuntu-latest`, so they linked `GLIBC_2.39` and failed to load on every
  Debian 12–based host — including 64-bit Raspberry Pi OS Bookworm — with
  napi's misleading "Cannot find native binding … npm optional dependencies
  bug" (the real error is `GLIBC_2.39 not found`). Both legs now build
  *natively inside* a Debian 11 container (aarch64 on the free arm64 runner,
  so nothing cross-compiles), which puts the floor at `GLIBC_2.31` —
  Debian 11+, Ubuntu 20.04+, Raspberry Pi OS Bullseye+ — and `release-npm`
  fails the build if an addon ever needs more than that again. PyPI wheels
  were never affected (maturin's manylinux images already floor at 2.17).
  **Needs a patch release to reach users**: npm versions are immutable, so the
  broken 1.0.1 platform packages stay as they are.
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

[Unreleased]: https://github.com/AreevAI/dejadb/compare/v1.1.0...HEAD
[1.1.0]: https://github.com/AreevAI/dejadb/compare/v1.0.5...v1.1.0
[1.0.5]: https://github.com/AreevAI/dejadb/compare/v1.0.4...v1.0.5
[1.0.4]: https://github.com/AreevAI/dejadb/compare/v1.0.3...v1.0.4
[1.0.3]: https://github.com/AreevAI/dejadb/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/AreevAI/dejadb/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/dejadb/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/dejadb/releases/tag/v1.0.0
