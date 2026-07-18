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
  → analyze   (deterministic, typed)         — six analyzers over grain semantics
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

## The six analyzers

All deterministic (T0/T1), default-on, computing over typed grains:

| Analyzer | Fires on | Proposes |
|---|---|---|
| `tool_failure` | ≥N Tool-grain errors clustered by (tool, normalized signature) at ≥40% of a tool's calls | a memory lesson (never auto-applies — evidence-derived text) |
| `duplicate_sweep` | exact-duplicate facts (NFC + case-fold) and near-duplicate observations (Jaccard) | consolidation (SUPERSEDE the extras) |
| `contradiction_sweep` | ≥2 live values under a functional relation (seeded list: `deploy_target`, `lives_in`, `tier`, …) | resolve to the latest value |
| `fork_surfacing` | an entity with >1 live head | a merge |
| `staleness` | a grain past its declared `valid_to` | a single-grain `FORGET` (destructive, never auto-applies) |
| `outcome_review` | an applied recommendation past `review_after` that regressed | a revert |

Precision is measured, never asserted: `cargo run -p dejadb-bench --bin
waiser_precision` scores each analyzer against a labeled fixture and gates
CI at 0.90.

## Surfaces

### CLI — `deja waiser`

```
deja init   [--template blank|demo|coding-agent] [--ns NS]   seed a backend + print hooks
deja waiser run     [--min-new N --min-new-errors N --if-stale 6h --format json --quiet]
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
db.recommendations('{"status":"pending"}')
db.apply_recommendation(hash, because="…")     # audited approve+apply
db.dismiss_recommendation(hash, "…")           # audited reject
```

Node mirrors these as `recordToolCall`, `waiserRun`, `recommendations`,
`applyRecommendation`, `dismissRecommendation`, plus the `actor` constructor
argument.

### MCP — two tools

`dejadb_waiser` runs a pass and returns the pending queue (call it at session
start). `dejadb_recommendations` lists, or acts (`apply`/`approve`/`reject`
with a mandatory `because`). Launch a reviewer process and worker processes
with different `--scopes`/`--actor` so no agent can approve its own proposals.

### HTTP — `/api/waiser/*`

`GET recommendations|health|analyzers` (reads) and `POST run|review|apply|
rollback` (writes). The console's Waiser tab renders the queue with severity
dots, evidence, and approve/apply/reject actions gated behind a mandatory
reason.

## Does it actually work? — the Verify gate

The honest test of self-improvement is not "did it make a change" but "did the
change help." Waiser answers that for itself. When you apply a recommendation
that carries a metric, the engine re-measures it after the review window and
records a **measured outcome** — `held` or `regressed`:

- A tool-failure lesson's metric is **recurrence**: after you apply the lesson,
  does that exact tool failure happen again? Baseline is zero — the fix is
  supposed to stop it. If the failure recurs, the outcome is `regressed` and
  outcome review proposes a **revert**; if it doesn't, the outcome is `held`.

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
engine-side shape verification (SUPERSEDE-only structural curation — an ADD
that introduces evidence-derived text, or a FORGET, disqualifies). Anything
failing stays pending. The policy file rejects unknown keys, so it can never
arrive pre-armed; it is host config and is never persisted in a memory file.
`deja waiser policy` prints the effective policy.

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
- **Determinism.** A waiser run's recommendations are a pure function of
  (store state, params) — the same finding yields the same `dedup_key` on any
  host, so a synced file behaves identically on its next host.

## Status

Built and tested: the engine (six analyzers, lifecycle, dedup, gating,
auto-apply), the DejaDB adapter, the `deja waiser` CLI + `deja init`, the
Python/Node bindings, the MCP tools, the tool-log importer, the policy file,
the `/api/waiser/*` API, the read-only-token-less auth change, the Waiser
console tab, and the precision bench. Not yet built: the optional LLM
enrichment layer, the telemetry sidecar and its analyzers, and the console's
Sessions/Setup editors. See `waiser-proposal.md` for the full plan.
