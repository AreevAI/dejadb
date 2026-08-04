# DejaDB as a Hermes Agent memory provider

[Hermes Agent](https://github.com/NousResearch/hermes-agent) has a pluggable
memory layer: one external `MemoryProvider` runs alongside its built-in files.
This directory is that provider, backed by a single DejaDB file.

Verified against **Hermes 0.16.0** — loaded through its real plugin loader,
subclassing its real ABC, driven through its real `MemoryManager`.

## What it adds

Hermes's built-in memory is two character-capped markdown files (`MEMORY.md`,
2,200 chars; `USER.md`, 1,375) plus an FTS5 index over session history. It is
deliberately small and it works. What it cannot do is keep what it evicts:
when a file approaches its cap the agent consolidates by *replacing* entries,
and the previous wording is gone from disk.

| | Hermes built-in | with this provider |
|---|---|---|
| an entry is rewritten | previous text gone | previous text is still a grain; the sequence stays queryable |
| per-turn recall | both files pasted in whole, every turn | one budgeted `ASSEMBLE` — profile + prior sessions, token-capped |
| current session's turns | in context already | deliberately excluded from injection, so budget goes to what the model *cannot* see |
| "what did I believe last month" | not answerable | `dejadb_memory` with `action: history` |
| hygiene | — | Waiser's deterministic analyzers at session end, advisory only |

Measured here, at 2,080 grains (40 profile notes + 2,000 prior turns), Apple M4
Max: **`prefetch()` p50 0.83 ms** (p95 3.9 ms), `sync_turn()` 0.47 ms. `prefetch()`
sits on Hermes's *synchronous* turn path (`agent/turn_context.py`), which is
why in-process matters — a network-backed provider pays its round trip there,
on every turn.

## Install

```bash
pip install dejadb                      # into the venv Hermes runs from
ln -s "$(pwd)/examples/hermes/dejadb" "$HERMES_HOME/plugins/dejadb"
hermes config set memory.provider dejadb
```

`$HERMES_HOME` defaults to `~/.hermes`. Confirm with `hermes memory setup`,
which will list `dejadb` and walk through the config below.

> **Requires a DejaDB newer than 1.0.4.** The provider uses `index_text=` on the
> constructor and `search()`, both added after that release. Until the next
> release, build the binding from this repo:
> `maturin build --release -m crates/dejadb-py/Cargo.toml` and `pip install` the
> resulting wheel.

## Configure

Optional; every key has a default. `hermes memory setup` writes
`$HERMES_HOME/dejadb.json`:

```json
{
  "db_path": "",
  "namespace": "hermes",
  "index_text": false,
  "embed_cmd": "",
  "budget_tokens": 800,
  "recent_turns": 8,
  "waiser_on_session_end": true
}
```

**`index_text` is off by default, which is not DejaDB's own default.** With the
BM25 index on, every write costs time proportional to the file's size — ~1.6 ms
at 500 grains, ~64 ms at 4,000, still climbing
([tursodatabase/turso#8170](https://github.com/tursodatabase/turso/issues/8170)).
A memory provider writes on every turn forever, which is exactly the shape that
cannot absorb it. Off, writes stay flat at ~0.16 ms at any size.

The cost is that free-text search has no lexical leg. Set `embed_cmd` to get
the vector leg instead — that is the recommended pairing:

```json
{ "embed_cmd": "python3 /path/to/embed.py" }
```

Same contract as the CLI's `--embed-cmd`: text on stdin, JSON array of floats on
stdout (see [`../llm/`](../llm/) for backend patterns). Without either leg,
recall still works through the structural legs (profile by subject, prior turns
by time); only `dejadb_memory action=search` is unavailable, and it says so
rather than returning an empty list.

## How it maps

| Hermes hook | what this does |
|---|---|
| `prefetch(query)` | one CAL `ASSEMBLE`: profile facts + prior-session turns, budgeted, rendered markdown |
| `sync_turn(user, assistant)` | one Event grain per side, tagged with the Hermes `session_id` |
| `on_memory_write(action, target, content)` | mirrors each `MEMORY.md`/`USER.md` edit as an audit Event **and** a profile Fact |
| `on_pre_compress(messages)` | persists what compression is about to discard |
| `on_session_end(messages)` | `waiser_run()` — deterministic analyzers, no LLM, advisory only |
| `get_tool_schemas()` | one tool, `dejadb_memory`, with `search` / `recall` / `history` / `cal` |

Assembly logic is data, not code: define a saved query named `hermes_prefetch`
in the file and the provider runs that instead, so you can re-tune what gets
injected without touching this plugin or restarting the agent.

```bash
deja cal 'DEFINE QUERY "hermes_prefetch"($user, $query) DESCRIPTION "my prompt"
  AS { ASSEMBLE "session" FROM
         profile: (RECALL facts WHERE subject = $user)
       BUDGET 400 tokens FORMAT markdown }' --db ~/.hermes/dejadb/default.db
```

## Limits, stated plainly

- **`remove` is never mirrored.** The bridge that notifies providers fires only
  for `add` and `replace` (`action in {"add", "replace"}` in
  `agent/agent_runtime_helpers.py`). So an entry the agent deletes outright
  leaves no record *of the deletion* — though the content itself is still here
  from when it was added. The provider handles `remove` already, so this starts
  working if that guard widens.
- **No LLM distillation.** Turns are stored verbatim as Events; the profile
  comes from mirroring Hermes's own curated `USER.md`/`MEMORY.md` rather than
  from extracting facts out of conversation. That is a deliberate floor, not a
  placeholder: extraction without a verifier is how memory layers accumulate
  confident nonsense. Run `deja waiser` for the governed version.
- **Skills are out of scope.** Hermes's procedural memory has its own
  `skills.write_approval` path and no provider hook; a memory provider cannot
  see or govern it.
- **One writer per file.** Concurrent gateway sessions share one DejaDB file
  behind a mutex, so their writes serialize. Fine for CLI and typical chat
  volume; if you run many concurrent users, give each a `db_path`.

## Verify it end to end

```bash
python3 examples/hermes/test_provider.py     # no Hermes needed
```

With Hermes installed, `hermes memory setup` → pick `dejadb` → start a session,
then:

```bash
deja recall --db ~/.hermes/dejadb/default.db --ns hermes/<user> --subject user:<user>
deja cal 'RECALL events WHERE role = "memory_write" RECENT 20' --db ~/.hermes/dejadb/default.db
```

The second is the audit trail of the agent editing its own memory.
