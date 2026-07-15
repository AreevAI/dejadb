# DejaDB Cookbook

Task-oriented recipes with copy-pasteable commands. Every command below is
verified against the `deja` CLI. Run `deja help` for the full usage summary.

Conventions used throughout:

- Every command needs `--db <file>` (the memory file). It is created on first
  write.
- `--ns <namespace>` partitions grains within a file; it defaults to `shared`.
- `-k <N>` caps result counts.

If you are running from source instead of an installed binary, replace `deja`
with `cargo run -p dejadb --`.

---

## 1. Add and recall a memory (CLI)

Store a fact (subject–relation–object), then read it back:

```bash
# Add a fact (confidence defaults to 0.9)
deja add --db john.db --ns caller \
  --subject john --relation prefers --object "window seat" --confidence 0.95

# Recall everything about a subject, newest-first (JSON lines)
deja recall --db john.db --ns caller --subject john

# Narrow to one relation, cap results
deja recall --db john.db --ns caller --subject john --relation prefers -k 5
```

`add` prints the new grain's content address (64-hex). Fetch any grain by hash:

```bash
deja get <hash> --db john.db
```

### Render model-ready context

Instead of raw JSON, render recall results into context for a model, under an
optional token budget:

```bash
deja recall --db john.db --ns caller --subject john --render sml
deja recall --db john.db --ns caller --subject john --render markdown --budget 300
```

`--render` accepts `sml`, `toon`, `markdown`, `plain`, or `json`. A one-line
summary (grain count, estimated tokens, whether it was truncated) is printed to
stderr.

### Hybrid text search

`search` runs hybrid recall (structural + BM25, fused with RRF):

```bash
deja search --db john.db --ns caller --query "seat preference" -k 10
```

---

## 2. Run a CAL query

CAL (Context Assembly Language) is DejaDB's query language. It has no bulk
destruction — `DELETE`/`DROP` are not tokens in the grammar; the only
destructive statement is `FORGET <hash>` (a single-grain tombstone, gated —
disable with `--no-destructive-ops`).

```bash
# Count matching facts
deja cal 'RECALL facts WHERE subject = "john" | COUNT' --db john.db --ns caller

# Recall with a filter
deja cal 'RECALL facts WHERE subject = "john" AND relation = "prefers"' \
  --db john.db --ns caller

# Add through CAL (ADD requires a REASON/BECAUSE clause)
deja cal 'ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" REASON "session note"' \
  --db john.db --ns caller

# Assemble one prompt from several sources in a single statement
deja cal 'ASSEMBLE "prompt" FROM
  policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"),
  profile:  (RECALL facts WHERE subject = "john")' --db john.db

# Ask whether a specific grain exists, or view a subject's history
deja cal 'EXISTS sha256:<64-hex>' --db john.db
deja cal 'HISTORY WHERE subject = "john" AND relation = "prefers"' --db john.db --ns caller
```

For an interactive shell (with `.stats`, `.log`, `.verify`, `.help`, `.quit`
dot-commands):

```bash
deja repl --db john.db --ns caller
```

See [`cal-reference.md`](cal-reference.md) for the full language.

---

## 3. Run the MCP server for Claude Code

DejaDB ships a built-in MCP server on stdio. Register it with Claude Code in one
line:

```bash
claude mcp add deja -- deja serve --mcp --db ~/.dejadb/code.db --ns claude-code
```

This exposes 6 tools to the model: `dejadb_recall`, `dejadb_add`,
`dejadb_supersede`, `dejadb_forget`, `dejadb_remember`, and `dejadb_cal`.

Any MCP client can launch the same server directly:

```bash
deja serve --mcp --db ~/.dejadb/code.db --ns claude-code
```

### Auto-capture each Claude Code turn (optional)

Print a ready-made hook snippet for `~/.claude/settings.json` (it only *prints* —
it never edits your config):

```bash
deja hook claude-code --db ~/.dejadb/code.db --ns claude-code
```

The snippet wires the `Stop` hook to `deja capture-stop`, which reads Claude
Code's hook JSON on stdin and stores the last exchange as thread-indexed Event
grains.

See [`mcp-reference.md`](mcp-reference.md) for the tool schemas.

---

## 4. Use encryption at rest

Add `--passphrase-env <VAR>` to **any** command to encrypt the database at rest.
DejaDB derives an AES-256 key (Argon2id) from the passphrase held in the named
environment variable; the non-secret salt is kept in a `<db>.kdf` sidecar.

```bash
# Keep the passphrase in the environment, never on the command line
export DEJADB_PASS='correct horse battery staple'

deja add    --db secret.db --passphrase-env DEJADB_PASS \
  --ns caller --subject john --relation prefers --object tea
deja recall --db secret.db --passphrase-env DEJADB_PASS --ns caller --subject john
```

Back up the `secret.db.kdf` sidecar alongside `secret.db` — without it the key
cannot be re-derived.

> Caveats: the `.blobs` sidecar (large binary payloads) is **not** encrypted, and
> encryption-at-rest uses the storage engine's AES-256-GCM, an experimental
> Turso feature. Treat it as defense-in-depth, not a substitute for
> full-disk encryption. Read [`../SECURITY.md`](../SECURITY.md) first.

---

## 5. Back up with a bundle, then restore

A **bundle** is a portable, incremental, git-shaped backup of the op-log.

```bash
# Write a full backup
deja bundle --db john.db --out john-backup.mgb

# Apply it to another file (fast-forward, idempotent)
deja import --db restored.db --bundle john-backup.mgb
```

For incremental backups, `bundle` prints the cursor for the next run — pass it
back as `--since`:

```bash
deja bundle --db john.db --out inc-01.mgb                 # prints: next --since <N>
deja bundle --db john.db --out inc-02.mgb --since <N>     # only new ops
```

Inspect the change feed at any time:

```bash
deja log   --db john.db --limit 20
deja verify --db john.db      # integrity + full content-address recheck
deja stats  --db john.db
```

---

## 6. Stream / sync between two files

`stream` continuously ships op-log segments (with generations, Litestream-shaped)
to a directory — a local path, an NFS mount, or an object-store mount. `follow`
subscribes and applies new segments; `restore` rebuilds from scratch, including
point-in-time restore.

```bash
# Producer: keep shipping changes to a shared directory
deja stream --db john.db --to ./sync-dir --interval-ms 500

# Consumer: subscribe and apply new segments as they appear
deja follow --db replica.db --from ./sync-dir --interval-ms 1000

# One-shot variants for scripts/cron
deja stream --db john.db     --to ./sync-dir --once
deja follow --db replica.db  --from ./sync-dir --once

# Rebuild a fresh file from a stream dir, optionally to a point in time
deja restore --db new.db --from ./sync-dir
deja restore --db new.db --from ./sync-dir --until-hlc <HLC>
```

Because grains are content-addressed and imports are idempotent, concurrent edits
that arrive out of order become **branches (heads)** with a deterministic
provisional head rather than lost writes.

---

## 7. Use the Python bindings

Install the published package:

```bash
pip install dejadb
```

Or build from a local checkout with [maturin](https://github.com/PyO3/maturin):

```bash
pip install maturin
maturin develop -m crates/dejadb-py/Cargo.toml    # into the active virtualenv
```

Then:

```python
import dejadb, json

m = dejadb.DejaDB("john.db", ns="caller")

# Add facts (returns the 64-hex content address)
h = m.add_fact("john", "prefers", "window seat", confidence=0.95)

# Structural recall and CAL both return JSON strings
print(m.recall("john"))
print(m.cal('RECALL facts WHERE subject = "john" | COUNT'))

# Current head for a (subject, relation); evolve it with supersede
head = m.latest("john", "prefers")
m.supersede(h, "fact", json.dumps({
    "subject": "john", "relation": "prefers", "object": "aisle seat"
}))

# Full history, portable backup, integrity check
print(m.history("john", "prefers"))
m.bundle("john-backup.mgb", 0)
print(m.verify())

# Anthropic memory-tool backend, scalars in / JSON string out
print(m.memory_tool(json.dumps({"command": "view", "path": "/memories"})))
```

The bindings follow **scalars in, JSON strings out**; errors raise
`ValueError`. Encryption at rest is currently CLI-only.

---

## 8. Open the web console

`ui` serves a local, browser-based console (memories, graph, and query tabs;
light + dark themes; grain inspector):

```bash
deja ui --db john.db
# → dejadb console → http://127.0.0.1:7437
```

The console binds loopback (`127.0.0.1:7437`) with **no authentication** by
design. It refuses to bind a non-loopback address unless you pass
`--allow-remote` — and even then serves an unauthenticated, writable console over
plaintext HTTP, so only do that behind a TLS-terminating reverse proxy with its
own auth:

```bash
# Choose a different loopback port
deja ui --db john.db --addr 127.0.0.1:8080

# Override the loopback guard (NOT recommended — front it with a TLS proxy + auth)
deja ui --db john.db --addr 0.0.0.0:8080 --allow-remote
```

See [`../SECURITY.md`](../SECURITY.md) for the trust model and the operator
hardening checklist.

---

## 9. Ingest raw conversation, then distill facts

`remember` stores raw content as an Observation grain (it prints the hash).
DejaDB runs no LLM itself, so fact extraction is host-supplied — pass your
extractor's output as JSON:

```bash
deja remember --db john.db --ns caller \
  --content "I always want a window seat, and I'm vegetarian." \
  --observer voice-agent \
  --facts '[{"subject":"john","relation":"prefers","object":"window seat","confidence":0.9},
            {"subject":"john","relation":"diet","object":"vegetarian","confidence":0.95}]'
```

The Anthropic memory-tool backend maps a `/memories/...` file space onto
supersession chains of Fact grains (full wiring guide:
[`memory-tool.md`](memory-tool.md)):

```bash
deja memtool '{"command":"view","path":"/memories"}' --db john.db --ns caller
deja memtool '{"command":"create","path":"/memories/notes.md","file_text":"prefers window seat"}' \
  --db john.db --ns caller
```

## 10. Build an agent that learns (and can unlearn)

A self-improvement loop is: **act → log experience → reflect → distill lessons
→ recall them next time**. DejaDB is the substrate for that loop, not the loop
itself: reflection (deriving lessons from experience) is a model call your host
makes, like all LLM work. What the store guarantees is that learning cannot rot
the memory it feeds on *silently* — revised lessons replace instead of
co-ranking (supersession), every lesson links to the experience that taught it
(`derived_from` + `REASON`), replayed or re-synced writes are idempotent
(content addressing), and a bad learning episode can be rolled back
(point-in-time restore). One honest limit: a **paraphrased re-learning is new
bytes and therefore a new grain** — content addressing alone cannot know it's a
duplicate. For that, `deja novelty` gives an *advise-mode* check (below): the
harness looks up the nearest existing lesson before writing and supersedes it
instead of adding a paraphrase. In a learning loop these properties are not
hygiene: rot compounds, because the agent keeps learning from its own mistakes.

Log experience as it happens — `remember` stores each entry as an Observation
grain and prints its hash (keep it: the lesson below links back to it). Writes
never call an LLM, so log everything:

```bash
deja remember --db agent.db --ns agent --observer executor \
  --content "Task: fix flaky test. Attempt 1: reran without isolation - FAILED."
deja remember --db agent.db --ns agent --observer executor \
  --content "Task: fix flaky test. Attempt 2: isolated the shared tempdir per test - PASSED."
```

Read experience back for reflection with `deja log --db agent.db` (op-log,
newest ops last) and `deja get <hash>` per grain. Write-cost note: the
microsecond write path assumes the text index is off or deferred — a live FTS
index costs ~140ms/write (`RESULTS.md` finding #1). For high-volume experience
logging, open with `--index-text false` and `deja reindex` before
recall-heavy phases, or keep raw experience and lessons in separate files.

After an episode, reflect (your model call) over the recent experience and
store each distilled lesson as a fact keyed to the skill it belongs to.
`derived_from` links the lesson to the observation that taught it —
structural provenance, not just a comment — and `REASON` records why:

```bash
deja cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "lesson"
  SET object = "Flaky tests sharing a tempdir need per-test isolation; rerunning alone never fixes them."
  SET confidence = 0.7 SET derived_from = "<observation-hash>"
  REASON "distilled from session flaky-01"' --db agent.db --ns agent
```

Track proficiency as its own supersession chain — `ADD` once, then `SUPERSEDE`
the tip after each success (both print the hash you supersede next time):

```bash
deja cal 'ADD fact SET subject = "fix_flaky_tests" SET relation = "proficiency"
  SET object = "0.30" REASON "first successful fix"' --db agent.db --ns agent

deja cal 'SUPERSEDE sha256:<tip-hash> SET object = "0.55"
  BECAUSE "second successful fix, different repo"' --db agent.db --ns agent
```

Recall surfaces only the current value — no stale value co-ranks with a
revised one. (That guarantee holds *within* a supersession chain: two
independently `ADD`ed facts on the same subject both surface as current, so
revise, don't re-add.) The full learning curve — every level and the reason it
changed — is one query; per-version wall-clock rides the op-log (`deja log`),
since supersession carries the original `created_at` forward:

```bash
deja cal 'HISTORY WHERE subject = "fix_flaky_tests" AND relation = "proficiency"' \
  --db agent.db --ns agent
```

At act time, pull the lessons back into the model's context:

```bash
deja search --db agent.db --ns agent --query "flaky test" -k 5
deja recall --db agent.db --ns agent --subject fix_flaky_tests --render sml --budget 300
```

**Unlearning** is what makes the loop safe to run unattended. A single bad
lesson is superseded (revised) or forgotten (tombstoned) by hash.

For an **episode-scoped** unlearn — undo everything the agent distilled from one
bad session without losing the good writes around it — link each lesson to its
source experience with `SET derived_from = "<observation-hash>"` (shown above),
then walk it back with `deja provenance`:

```bash
# Which lessons came from this observation/session? (reverse provenance)
deja provenance <observation-hash> --db agent.db --ns agent
# Revise or tombstone each returned hash — e.g. forget them all:
deja provenance <observation-hash> --db agent.db --ns agent \
  | python3 -c 'import json,sys; [print(json.loads(l)["hash"]) for l in sys.stdin]' \
  | while read h; do deja cal "FORGET sha256:$h" --db agent.db --ns agent; done
```

`deja provenance` is precise (only grains derived from that source) and keeps
the surrounding good writes intact — the credit-assignment tool for a learning
loop. When you instead need to roll the *whole file* back to a point in time,
checkpoint before risky learning and rewind (this also discards good writes in
the window, and produces a new file you swap in):

```bash
deja stream --db agent.db --to ./checkpoints --once   # checkpoint: ship the op-log
# ... a bad reflection episode writes junk lessons ...
deja log    --db agent.db                             # note the HLC of the last good op
deja stream --db agent.db --to ./checkpoints --once   # ship the rest, then rewind:
deja restore --db rewound.db --from ./checkpoints --until-hlc <HLC>
```

For a typed capability record there is also the OMS **Skill** grain:

```bash
deja cal 'ADD skill SET name = "fix_flaky_tests" SET description = "Diagnose and fix flaky tests"
  SET when_to_use = "test passes alone but fails in suite" SET confidence = 0.3
  REASON "first successful fix"' --db agent.db --ns agent
```

A Skill's `confidence` **is** its proficiency (OMS aliases them), and it
carries definition fields like `instructions` and `when_to_use`. Evolve it with
`SUPERSEDE sha256:<hash> SET confidence = 0.55 BECAUSE "..."` — unchanged
fields carry forward — and fetch any version with `deja get <hash>`. Skill
grains are hash-addressed records today; keep the *queryable* index in facts,
as above.

### Closing the loop automatically (Claude Code)

The steps above are the mechanics; two hooks make the loop run without you
thinking about it. `deja hook claude-code` prints a `settings.json` snippet
that wires both directions:

```bash
deja hook claude-code --db ~/.dejadb/code.db --ns claude-code   # prints, never writes
```

- **`UserPromptSubmit` → `deja recall-hook`** reads each prompt, hybrid-searches
  memory, and prints matching lessons to stdout — which Claude Code injects as
  context. Retrieval stops depending on the model *choosing* to call a tool.
- **`Stop` → `deja capture-stop`** stores the turn's last exchange as Events,
  including tool calls and their outcomes (a failing `tool_result` is captured
  and flagged), which is the raw signal reflection distills from.

`recall-hook` reads the hook JSON on stdin (`{"prompt": "..."}`), so it also
works from any tool that can run a command per prompt; it stays silent when
there is no prompt or no match, so it never adds noise.

### The reflection harness (your model call)

DejaDB runs no LLM, so the *reflect* step — turning captured experience into
lessons — is a host job. The shape of a nightly (or on-`SessionEnd`) harness:

```bash
# 1. Pull recent experience (now that the experience log is recallable):
deja cal 'RECALL events RECENT 100' --db agent.db --ns agent --render plain > episode.txt

# 2. Distill lessons with YOUR model (any stdin→stdout command), e.g.:
#    claude -p "Read this session. Emit each durable lesson as one line:
#               subject | relation | object | derived_from=<observation-hash>"
cat episode.txt | claude -p "$(cat reflect-prompt.txt)" > lessons.tsv

# 3. Write each lesson back. --idempotent collapses an exact repeat; for
#    paraphrases, ask `deja novelty` for the nearest existing lesson first and
#    supersede it past a similarity threshold instead of adding a near-dup:
while IFS='|' read -r s r o df; do
  near=$(deja novelty --text "$o" --subject "$s" --relation "$r" \
           --db agent.db --ns agent --embed-cmd 'my-embedder' -k 1)
  sim=$(printf '%s' "$near" | python3 -c 'import json,sys; l=sys.stdin.read().strip(); print(json.loads(l)["similarity"] if l else 0)')
  if awk "BEGIN{exit !($sim > 0.9)}"; then
    hash=$(printf '%s' "$near" | python3 -c 'import json,sys; print(json.loads(sys.stdin.read())["hash"])')
    deja cal "SUPERSEDE sha256:$hash SET object = \"$o\" BECAUSE \"refined lesson\"" --db agent.db --ns agent
  else
    deja add --db agent.db --ns agent --subject "$s" --relation "$r" --object "$o" --idempotent
  fi
done < lessons.tsv
```

`deja novelty` is *advise-only* — it never drops or writes; the harness decides
supersede-vs-add (the exact failure mode of stores that silently deduped and
lost updates). `--idempotent` handles exact repeats, `SET derived_from` on
lessons lets `deja provenance` walk them back. The engine gives you the safe
substrate; the harness is the one piece you own.

---

## 11. Migrate from mem0, Zep, Letta, LangMem, or Basic Memory

Dump your existing memories to a file (per-source one-liners in
[`migrate.md`](migrate.md)), then:

```bash
deja migrate --from mem0 --file export.json --history history.json --db mine.db
deja migrate --from basic-memory --file ~/basic-memory --db mine.db   # notes → /memories/*
deja migrate --from jsonl --file memories.jsonl --db mine.db          # pgvector/Chroma/homegrown
```

mem0 history becomes real supersession chains with original timestamps;
re-running an import skips what's already there. Check the result:

```bash
deja stats  --db mine.db
deja search --query "anything you remember" --db mine.db
deja memtool '{"command":"view","path":"/memories"}' --db mine.db
```

To embed while importing (vector recall), add
`--embed-cmd 'python3 my_embedder.py'` — the command reads text on stdin and
prints a JSON array of floats.

---

## See also

- [`../ARCHITECTURE.md`](../ARCHITECTURE.md) — how DejaDB is built
- [`cal-reference.md`](cal-reference.md) — the CAL query language
- [`mcp-reference.md`](mcp-reference.md) — the MCP tools
- [`../FAQ.md`](../FAQ.md) — concepts and comparisons
- [`../SECURITY.md`](../SECURITY.md) — trust model and hardening
