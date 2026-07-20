# Waiser — governed self-improvement for AI agents

Waiser turns an agent's own history into **recommendations** — evidence-cited,
reviewable, undoable, measured — and governs every change to the agent's
memory through four gates. The core is **deterministic**: it produces useful
recommendations with **zero model calls** by computing over DejaDB's typed
grains, never over raw prose.

Waiser ships inside DejaDB: the `deja waiser` verb family, the `dejadb.*`
binding methods, two MCP tools, the `/api/waiser/*` HTTP routes, and a Waiser
tab in `deja ui`. It is not a separate install.

- Design & rationale: [`waiser-proposal.md`](waiser-proposal.md)
- Analyzer precision numbers: `crates/dejadb-bench/RESULTS.md`
- Trust model: [`security-model.md`](security-model.md)

## The 60-second proof (no agent, no LLM, no waiting)

The fastest way to see the loop is a REPL and ~15 lines — five failing tool
calls and a couple of contradictory facts light up the analyzers
deterministically:

```python
import dejadb, json

db = dejadb.DejaDB("proof.db", actor="user:me")   # actor labels the audit chain

# tool-failure clustering: 5 failures + 2 successes for one tool
for _ in range(5): db.record_tool_call("stripe_refund", '{"error":"rate_limited"}', is_error=True)
for _ in range(2): db.record_tool_call("stripe_refund", '{"ok":true}', is_error=False)

# contradiction sweep: two live values under a functional relation
db.add_fact("acme", "deploy_target", "us-east-1", 0.9)
db.add_fact("acme", "deploy_target", "eu-west-1", 0.9)

health = db.waiser_run()                             # explicit call: never gated
for rec in json.loads(db.recommendations('{"status":"pending"}')):
    print(rec["severity"], rec["summary"])

# review with judgment — never rubber-stamp
pending = json.loads(db.recommendations('{"status":"pending"}'))
db.apply_recommendation(pending[0]["hash"], because="rate-limit retries belong in the client")
db.dismiss_recommendation(pending[1]["hash"], "those were one expired key")
```

Or from a fresh install with the CLI, using the seeded demo corpus:

```bash
deja init --db demo.db --template demo    # plants dupes, a contradiction, a stale grain
deja waiser run --db demo.db              # ~3 recommendations across analyzers
deja waiser list --db demo.db
deja ui --db demo.db --token-env DEJA_TOKEN   # the Waiser tab shows the queue
```

## The loop

```
capture  (tool calls, facts, events)        — record_tool_call / add / import
  → analyze   (deterministic, typed)         — eleven analyzers over grain semantics
  → recommend (recommendation + evidence)    — dedup'd, template-rendered, cited
  → govern    (review / policy auto-apply)   — four gates, hash-chained audit
  → apply     (undoable supersession)        — scope-checked at execution
  → measure   (outcome review)               — re-run the metric, revert on regression
```

The loop closes with **no LLM**. Every recommendation cites the grains it was
computed from; every apply stores its inverse (or is marked non-rollbackable
up front); every decision carries a written reason.

## The four gates

1. **Propose** — only recommendation objects enter the queue, each carrying a
   versioned analyzer id + params, a deterministic template-rendered summary,
   bounded evidence hashes, a severity, and (where applicable) a reproducible
   metric snapshot. Analyzers cannot emit free prose.
2. **Review** — separation of duties (`write` grants neither `review` nor
   `apply`); a **mandatory reason** (BECAUSE) on every decision; self-approval
   is blocked against the recommendation's creating actor.
3. **Apply** — requires the `apply` scope; destructive applies additionally
   require `admin` + `allow_destructive`; every apply records its inverse.
4. **Verify** — outcome review re-runs the stored metric after `review_after`
   and proposes a revert on regression.

The **audit trail is grains**: one immutable Observation per transition,
hash-chained per recommendation, carrying the actor label and the reason. It
syncs with the file and is queryable.

## The analyzers

Eleven built-in analyzers, all deterministic (T0/T1), computing over typed
grains — never raw prose. Ten are default-on; goal stagnation is opt-in (see
the table). The last three are **telemetry-fed** —
they read the recall-telemetry sidecar (below) and move Waiser from *hygiene*
(is memory internally correct?) to *utility* (is memory used, and does it
help?):

| Analyzer | Fires on | Proposes |
|---|---|---|
| `tool_failure` | ≥N Tool-grain errors clustered by (tool, normalized signature), at ≥40% of a tool's calls **or** a large absolute count (so high-volume, moderate-rate failures aren't hidden) | a memory lesson (never auto-applies — evidence-derived text) |
| `duplicate_sweep` | exact-duplicate facts (NFC + case-fold) and near-duplicate observations (Jaccard) | consolidation (SUPERSEDE the extras) |
| `contradiction_sweep` | ≥2 live values under a functional relation (seeded list: `deploy_target`, `lives_in`, `tier`, … — extendable per domain via `extra_relations`) | resolve to the latest value |
| `fork_surfacing` | an entity with >1 live head | a merge (approval-required — a merge is lossy, never auto-applies) |
| `staleness` | a grain past its declared `valid_to` | a single-grain `FORGET` (destructive, never auto-applies) |
| `skill_stall` | a Skill practiced ≥N times whose proficiency stays low — doing it, not getting better at it | an advisory flag (never auto-applies) |
| `goal_stagnation` | an active Goal with little progress that's gone stale (**opt-in** — "stalled" is ambiguous; enable per file) | an advisory flag |
| `cold_grains` *(telemetry)* | a live fact never recalled past a grace window — memory not earning its place | a retire-candidate flag (advisory; cold ≠ wrong) |
| `coverage_gap` *(telemetry)* | a recurring recall question that keeps returning nothing — knowledge the memory should hold | a gap flag (advisory; the fix is to *add* memory) |
| `budget_pressure` *(telemetry)* | context assembly repeatedly overflowing its token budget (fed by the ASSEMBLE allocator) | a flag: raise the budget or curate |
| `outcome_review` | an applied recommendation past `review_after` that regressed | a revert |

Precision is measured, never asserted: `cargo run -p dejadb-bench --bin
waiser_precision` scores each analyzer against a labeled fixture and gates
CI at 0.90. On the current fixture the seven default-on analyzers it covers —
contradiction, duplicate, staleness, tool-failure, skill-stall, **cold-grains,
and coverage-gap** — each score **1.00** precision and recall; `fork_surfacing`
and `outcome_review` need concurrent heads / applied history, and
`budget_pressure` is a global signal, so those three are covered by the crate
tests instead. See `crates/dejadb-bench/RESULTS.md` for the table.

## Recall telemetry (the utility signal)

Telemetry is what lets the last three analyzers exist. A disposable
`<file>.telemetry.db` sidecar records what recall actually surfaced — which
grains were retrieved, which questions came back empty, how often — so Waiser
can see memory *utility*, not just internal consistency.

- **Host-only; off in the library, `aggregate` for agent hosts.** The `deja`
  CLI (`--telemetry off|aggregate|full`, default aggregate) and the Python/Node
  constructors (`telemetry="aggregate"`) turn it on; a bare library `open()`
  records nothing. It is never a file-truth.
- **Buffered and non-blocking.** The recall hot path only pushes an in-memory
  event — no SQLite I/O touches the ~136µs recall / 50ms voice budgets (proven:
  voice-loop recall p50 stays ~82µs with telemetry on). The buffer drains
  off-path.
- **Encrypted under the same key** as the main file (crypto-erasure covers it),
  **never syncs** (the hub carries the memory file only), **rebuildable** —
  losing it costs evidence detail, never state. `FORGET` synchronously scrubs
  it. Modes: `off` | `aggregate` (rollups) | `full` (+ a per-recall ring log).

The console **Sessions** view visualizes it; `GET /api/waiser/telemetry` serves it.

## LLM enrichment (optional)

The deterministic loop closes with no model. Attach one out of the box with
`deja waiser run --model claude-sonnet` (the key comes from
`$ANTHROPIC_API_KEY`/`$OPENAI_API_KEY`/`$OLLAMA_HOST`; `--model openai:gpt-5`,
`--model ollama:llama3.1`, `--llm-base-url` for any gateway) — or
`--llm-cmd 'CMD'` for a subprocess backend. The built-in adapters
(OpenAI-compatible, Anthropic, Ollama) live in `dejadb-llm` over a small
blocking HTTP client, so the core crates stay dependency-light. Either way the
pipeline gains **strictly additive** stages —
`ANALYZE → DISCOVER → GROUND → VERIFY → ENRICH → VALIDATE+DEDUP → STORE` — that
are the identity when no backend is set:

- **DISCOVER** — the model proposes *additional* findings determinism can't see
  (a semantic contradiction, a stale assumption), under an **abstention-legitimate
  objective**: "nothing to report" is a first-class, zero-penalty answer, so it
  isn't pushed to over-generate. Every draft must **cite evidence** (uncited →
  dropped) and target a memory entity; `origin = llm` so it can **never
  auto-apply**.
- **GROUND → VERIFY** — before a draft is ever queued it must pass an
  independent **grounding** check (are the finding's factual *premises* present
  in the cited evidence? — this guards against fabrication while still allowing a
  genuine *inference*, e.g. "HQ=San Francisco and country=Germany conflict") and
  an adversarial **verification** pass (is the finding sound and specific, not
  vague or spurious — abstention is legitimate). **Each is a separate call, so
  the proposer never grades itself**; grounding can even run on a different model
  (`--ground-model` / `--ground-cmd`) to take the generator out of the loop. Only
  findings that survive, above a confidence floor, reach review. This is what
  turns "generates something" into "generates something that survived a skeptic."
  Quality is measured, not asserted: the `waiser_reflection` bench scores
  **Effective Reliability**, and `deja waiser` reports the live approval-rate of
  LLM findings. Full design + evidence: [`waiser-reflection.md`](waiser-reflection.md).
- **ENRICH** — a whitelisted one-line `guidance` note on a deterministic
  finding; the engine-templated summary is always kept.
- **Fail-soft**: a failed/garbled/slow backend drops the contribution, never
  the run. Instructions never interleave with (untrusted) evidence text.

`CommandLlm` mirrors `--embed-cmd`: a JSON request on stdin → a JSON response on
stdout, one process per call, probed at construction. CLI-only, never persisted.
Ready-to-run backends live in `examples/llm/` (`claude -p`, OpenAI, ollama, and
a dependency-free mock) with the protocol documented.

## External analyzers (optional)

Determinism you can extend without recompiling: `deja waiser run --analyzer-cmd
'CMD'` registers a subprocess analyzer. It receives a live-grain snapshot on
stdin and returns advisory findings on stdout (`{op:analyze,grains:[…]}` →
`{findings:[{target,summary,severity,evidence}]}`, self-describing via a probe).
It runs at **trust class `command`, auto-apply `never`** — a domain-specific
check (PII, a house style rule, a compliance sweep) can *surface* an issue a
human then reviews, but can never mutate memory. A failure skips that analyzer
for the run, never the pass. This is also the only custom-analyzer path from
Python/Node (which can't implement the Rust `Analyzer` trait): `waiser_run(…,
analyzer_cmd="…")`. A ready-to-run sample (a PII scan, protocol documented
inline) lives in `examples/analyzers/`.

## Surfaces

### CLI — `deja waiser`

```
deja init   [--template blank|demo|coding-agent] [--ns NS]   seed a backend + print hooks
deja waiser run     [--min-new N --min-new-errors N --if-stale 6h --format json --quiet]
                    [--model P:N | --llm-cmd 'CMD'] [--ground-model P:N | --ground-cmd 'CMD']
                    [--analyzer-cmd 'CMD']
deja waiser reflect  like run, but re-analyzes the WHOLE memory (ignores the incremental
                    watermark) — a full sweep; same flags as run
deja waiser list    [--status pending|applied|all] [--fail-on high]   (exit 2 on match → CI gate)
deja waiser show <hash>
deja waiser approve|reject|apply|rollback <hash> --because "…" [--actor A] [--allow-destructive]
deja waiser outcomes     the Verify gate — did applied advice hold or regress?
deja waiser analyzers | policy
deja waiser              (bare: a health summary)
```

`run` returns the **run-outcome contract** — `{outcome, skip_reason,
new_grains, new_error_events, proposed, deduped, stored, auto_applied,
analyzers_run, analyzers_skipped}`. Exit 0 on ran *or* clean skip (cron never
pages on a healthy no-op), 1 on error. Hashes accept git-style unique
prefixes.

### Bindings — Python & Node

Same methods in both (scalars in, JSON strings out):

```python
db = dejadb.DejaDB("agent.db", actor="user:alice")
db.record_tool_call("stripe_refund", result_json, is_error=True, thread="sess-42")
db.waiser_run(min_new=20, min_new_errors=3, if_stale="6h")   # gated; bare call never gates
db.waiser_run(full_sweep=True)                 # the `reflect` semantics: whole memory
db.waiser_run(policy="waiser-policy.json")     # host policy file — the only auto-apply path
db.recommendations('{"status":"pending"}')
db.apply_recommendation(hash, because="…")     # audited approve+apply
db.dismiss_recommendation(hash, "…")           # audited reject
db.rollback_recommendation(hash, because="…")  # retract what an apply created
db.waiser_outcomes()                           # the Verify gate's held/regressed record
```

Node mirrors these as `recordToolCall`, `waiserRun` (incl. `fullSweep` /
`policy`), `recommendations`, `applyRecommendation`, `dismissRecommendation`,
`rollbackRecommendation`, and `waiserOutcomes`, plus the `actor` constructor
argument.

### MCP — two tools

`dejadb_waiser` runs a pass and returns the pending queue (call it at session
start). `dejadb_recommendations` lists, or acts (`apply`/`approve`/`reject`
with a mandatory `because`). Launch a reviewer process and worker processes
with different `--scopes`/`--actor` so no agent can approve its own proposals.

### HTTP — `/api/waiser/*`

`GET recommendations|health|analyzers` (reads) and `POST run|review|apply|
rollback|config` (writes). The console's Waiser tab renders the queue with
severity dots, evidence, and approve/apply/reject actions gated behind a
mandatory reason; the **Setup** tab is writable — click an analyzer on/off to
persist an enable/disable to the file's config (`POST /api/waiser/config`).
Auto-apply is never grantable from the console — only via a host policy file.

## Does it actually work? — the Verify gate

The honest test of self-improvement is not "did it make a change" but "did the
change help." Waiser answers that for itself. When you apply a recommendation
that carries a metric, the engine re-measures it after the review window and
records a **measured outcome** — `held` or `regressed`:

- A tool-failure lesson's metric is **recurrence**: after you apply the lesson,
  does that exact tool failure happen again? Baseline is zero — the fix is
  supposed to stop it. If the failure recurs, the outcome is `regressed` and
  outcome review proposes a **revert**; if it doesn't, the outcome is `held`.
- A contradiction resolution's metric is **recurrence** too: after resolving
  to the latest value, does the subject again hold two live values under that
  functional relation? A returned conflict regresses the checkpoint and
  proposes a revert for human judgment. (Duplicate consolidation carries no
  metric yet: a supersession creates a replacement grain, so a live-grain
  count can't honestly measure it — that needs a supersede-by-existing
  substrate primitive first.)

Crucially, it re-measures on a **schedule of checkpoints** (1d / 7d / 30d), not
once — so an outcome that looked fine early can be caught regressing later. A
single fixed window would freeze a false "held"; the time series doesn't:

```bash
deja waiser outcomes --db agent.db
#   a6f8133  tool_error_recurrence  @1d    baseline 0 → current 0  [held]
#   a6f8133  tool_error_recurrence  @7d    baseline 0 → current 0  [held]
#   a6f8133  tool_error_recurrence  @30d   baseline 0 → current 2  [regressed]  ← late recurrence caught; revert proposed
```

The re-measurement is a typed read over subsequent history (no LLM, no
guessing), recorded as a file-truth so it syncs and accumulates. That is the
difference between "governed memory hygiene" and self-improvement that proves
its own advice — the record is the evidence.

**The honest boundary.** This works for **internal, bounded, attributable**
outcomes — facts about data Waiser owns (did this tool fail again, does this
duplicate still exist). It does **not** measure open-ended, confounded,
world-facing outcomes (was a generated post good, is a patient happier). Those
depend on signals outside DejaDB and on a hundred factors that aren't the
change, so the honest output is a **monitored trend a human judges**, never a
machine verdict — the design suppresses causal claims at low sample sizes on
purpose. Waiser improves the agent's *memory*, not its *outputs* (§2.4).
Outcomes accrue over real calendar time as checkpoints elapse; the loop is
exercised end-to-end by the engine test suite, which controls the clock.

## Triggers — no daemon, anywhere

A waiser run is a cheap, idempotent command that hosts trigger however they
already trigger things (hooks, cron, CI, MCP calls). Gates make repeat runs
free:

- `--min-new N` / `--min-new-errors N` — run only after enough new grains /
  tool failures since the last run (a file-truth watermark).
- `--if-stale 6h` — run only if the last run is older than the interval.

The SessionEnd Claude Code hook runs `deja waiser run --min-new 20
--min-new-errors 3 --quiet`, so most session ends are a watermark check that
exits immediately. There is no scheduler in the product.

The loop also closes **into** the agent's context: the UserPromptSubmit hook
`deja recall-hook --with-waiser` appends a compact block of pending
recommendations (severity + summary, capped at 3, `origin=llm` entries
labeled) to the memory it injects — so the agent sees its own pending queue
instead of waiting to be asked. `deja init` and `deja hook claude-code` print
the flag in their snippets.

## Auto-apply & the policy file

Auto-apply is **off by default** and is granted **only** by an optional
host policy file — `deja waiser --policy waiser-policy.json` (or
`$WAISER_POLICY`):

```json
{
  "auto_apply_enabled": true,
  "auto_apply": [
    { "analyzer": "waiser.duplicate_sweep", "targets": ["memory"], "max_severity": "low" }
  ],
  "deny": [],
  "severity_floors": { "waiser.staleness": "medium" },
  "telemetry": "aggregate"
}
```

A recommendation auto-applies **only if all** hold (proposal §6.3): host
opt-in + a matching grant, a built-in analyzer (never command/LLM), a
`memory`/`query` target (never prompt/host), non-destructive, and
engine-side shape verification — the batch must be SUPERSEDE-only **and
value-identical**: every replacement field is checked against the grain it
supersedes (case-fold/trim; `namespace` against the grain's own), so only
consolidation that provably changes no value qualifies. An ADD that
introduces evidence-derived text, a FORGET, or a near-duplicate consolidation
that rewrites an observation body all stay pending. Anything failing stays
pending. The policy file rejects unknown keys, so it can never arrive
pre-armed; it is host config and is never persisted in a memory file.
`deja waiser policy` prints the effective policy.

The same policy file attaches to the other run surfaces — `deja ui --policy`
(console-triggered runs) and `deja serve --mcp --policy` (the `dejadb_waiser`
tool) — so every surface honors one set of grants, set at process start and
never controllable by a client.

## Read-only console (breaking change)

Token-less `deja ui` is **read-only**: it browses the queue but cannot act.
Every write — any waiser mutation, an `ADD`/`SUPERSEDE`/`FORGET` CAL batch —
requires `deja ui --token-env VAR`. This closes the path where a local
process could execute a proposal's CAL directly and skip the review queue.
Existing write callers add `--token-env`; a token unlocks review + apply.

## Compatibility notes

- **Interim grain mapping.** The OMS 0x0C recommendation type is not yet
  realized in dejadb-core, so recommendation and audit grains currently ride
  as Facts in the `waiser` namespace with the field-map carried as JSON.
  They are real, content-addressed, syncable grains; when 0x0C lands this
  becomes a native mapping and existing content addresses stay valid
  (additive, per OMS §4.5).
- **Tool grains.** The flagship analyzer reads Tool grains (0x05), which
  carry `tool_name`/`is_error`/`content` natively. `record_tool_call` and
  `deja migrate --from tool-log` both produce them.
- **Determinism.** A waiser run's *deterministic* recommendations are a pure
  function of (store state, params, now) — the same finding yields the same
  `dedup_key` on any host, so a synced file behaves identically on its next
  host. The optional LLM layer only *adds* `origin = llm` drafts; it never
  changes the deterministic set.

## Status

Built and tested: the engine (eleven analyzers, lifecycle, dedup, gating,
auto-apply, the multi-horizon Verify gate, the optional LLM DISCOVER/ENRICH
stages), the recall-telemetry sidecar and its three telemetry-fed analyzers,
the DejaDB adapter, the `deja waiser` CLI + `deja init` (incl. `--telemetry`
and `--llm-cmd`), the Python/Node bindings (telemetry-enabled), the MCP tools,
the tool-log importer, the policy file, the `/api/waiser/*` API (incl.
`/telemetry`), the read-only-token-less auth, the Waiser console tab (queue /
analyzers / **sessions** / outcomes / **setup**), the `examples/llm/` backends,
and the precision bench.

Also shipped since: `budget_pressure` reads the live ASSEMBLE overflow signal
(default-on); the LLM operator-taste history (recent approvals/rejections) is
passed to DISCOVER so the model learns this reviewer's taste; the bindings carry
`model`/`llm_cmd`/`ground_*`/`analyzer_cmd`; a **pluggable grounding backend**
(`--ground-cmd`), **external command analyzers** (`--analyzer-cmd`), a
**full-memory sweep** (`deja waiser reflect`), and a **writable console Setup**.

And in the post-merge follow-up pass: the auto-apply **value-identity check**
(near-duplicate consolidations stay pending, as §6.3 always intended);
analyzer writes now **carry their namespace** (a consolidation or lesson can
no longer drift to the store default namespace — the tool-failure lesson lands
in the dominant namespace of its evidence); a **contradiction-recurrence
metric** (the Verify gate now measures resolutions, not just tool lessons);
**`recall-hook --with-waiser`** (pending recommendations ride into the
injected context); bindings parity (`rollback_recommendation`,
`waiser_outcomes`, `full_sweep`, `policy`); the **host policy attaches to
`deja ui` and `deja serve --mcp`**; and an `examples/analyzers/` sample.

Remaining follow-ups (documented, not blockers): the **native OMS `0x0C`
Recommendation grain** in `dejadb-core` — deliberately deferred, because it
changes the frozen canonical serialization / grain-type registry and is an
OMS-spec-level decision; until then recommendations ride as Facts with a
distinguishing relation (`waiser_recommendation`). And a labeled non-parasitic
corpus for a published Effective-Reliability number. See `waiser-proposal.md`
for the full plan.
