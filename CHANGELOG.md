# Changelog

All notable changes to DejaDB are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
  remain Waiser's job. New `CalStoreFacade::open_forks()` (default: no forks) and
  `store_types::ForkGroupInfo`. Covered by ten end-to-end tests in
  `crates/dejadb-cal/tests/cal_integration.rs`.
- **`deja hub`** — the sync hub (`dejad`) as a CLI verb: many apps, one shared
  memory, segment push/pull, default `127.0.0.1:7438`. Unlike the console this
  is a network service by construction, so `--token-env` is **mandatory** and a
  non-loopback bind still needs `--allow-remote`. Segment reads are gated too,
  not just pushes; a pushed segment can only ever *add* grains.
- **A redesigned web console** for non-technical reviewers: a plain-language
  memory browser with an interactive graph, the Waiser review queue, a CAL
  workbench (cards, table, graph, formats, history, saved queries), and a
  Developer-mode toggle that reveals hashes, the op log and CAL. Still one
  embedded `console.html` with no build step.
- **`dejadb.helpers`**, shipped inside the Python wheel: `fresh`, `facts` /
  `show_facts`, `recs` / `show_recs`, `audit`, `outcomes`, `days_later`
  (a context manager over the `WAISER_NOW_MS` clock-pin seam), `auto_model`
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
  anchored leg's `WITH superseded` remains the known bug its ignored golden
  test documents). New `DejaDB::recent_live`, so scans that legitimately want
  every version — Waiser's analyzers — keep `recent` unchanged.
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
- `dejadb.helpers.days_later` restores a pre-existing `WAISER_NOW_MS` instead
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
- **Waiser.** Tool-failure lessons re-measure against their exact failure
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

[Unreleased]: https://github.com/AreevAI/dejadb/compare/v1.0.3...HEAD
[1.0.3]: https://github.com/AreevAI/dejadb/compare/v1.0.2...v1.0.3
[1.0.2]: https://github.com/AreevAI/dejadb/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/AreevAI/dejadb/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/AreevAI/dejadb/releases/tag/v1.0.0
