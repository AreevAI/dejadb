# DejaDB — demo video script & storyboard

A shot-by-shot script for a ~2.5-minute launch demo. Optimized for a terminal
screen-cast (asciinema/`vhs`) with optional voice-over. Keep it real: every
command below actually runs. Record at a calm pace; viewers should be able to
read each result.

**Goal of the video:** in 150 seconds, show that DejaDB is (1) an *embedded*
memory engine — a file you own, microsecond recall, (2) *honest* — immutable,
content-addressed, provenance-traceable, (3) *model-native* — one line to give
an agent memory.

---

## Cold open (0:00–0:12)

- **Screen:** black terminal, DejaDB wordmark fades in.
- **VO / caption:** "Your AI agent's memory shouldn't be a database server on the
  other side of a network call. It should be a file you own."
- **Cut to:** an empty prompt.

## Beat 1 — Add & recall (0:12–0:40)

- **Type:**
  ```bash
  deja add --db john.db --ns caller --subject john --relation prefers --object "window seat"
  deja recall --db john.db --ns caller --subject john
  ```
- **Show:** the recall returns the grain instantly. Highlight there was **no
  server, no embedding API call** — it's in-process.
- **Caption:** "Store a memory. Recall it. No sidecar, no network hop."

## Beat 2 — It's honest (0:40–1:05)

- **Type** the same fact again, then update it:
  ```bash
  deja add --db john.db --ns caller --subject john --relation prefers --object "window seat"
  deja add --db john.db --ns caller --subject john --relation prefers --object "aisle seat"
  deja cal 'RECALL facts WHERE subject = "john" AND relation = "prefers"' --db john.db --ns caller
  deja history --db john.db --ns caller --subject john --relation prefers
  ```
- **Show:** the duplicate collapses to one grain; recall returns the **current**
  value (aisle), and `history` shows the superseded one is *kept, not lost*.
- **Caption:** "Immutable & content-addressed. Edits supersede, they don't
  overwrite. Nothing is silently lost — and nothing is silently duplicated."

## Beat 3 — Query language that can't delete your data (1:05–1:25)

- **Type:**
  ```bash
  deja repl --db john.db --ns caller
  # in the shell:
  RECALL facts WHERE subject = "john" | COUNT
  DESCRIBE
  ```
- **VO / caption:** "Query with CAL. And notice what you *can't* do — `DELETE`
  and `DROP` aren't even tokens in the grammar. The query language is
  structurally incapable of destroying memory."

## Beat 4 — One line of memory for an agent (1:25–1:55)

- **Type:**
  ```bash
  claude mcp add deja -- deja serve --mcp --db ~/.dejadb/code.db --ns claude-code
  ```
- **Cut to:** a Claude Code (or any MCP client) session recalling something it was
  told earlier in a *previous* session.
- **Caption:** "One line gives any MCP client persistent memory. Works with Claude
  Code today; it's just MCP."

## Beat 5 — Yours, private, portable (1:55–2:20)

- **Type:**
  ```bash
  export DEJADB_KEY="correct horse battery staple"
  deja add --db secret.db --ns caller --subject john --relation ssn --object "***" --passphrase-env DEJADB_KEY
  deja stream --db john.db --to ./backup/     # git-style op-log shipping
  deja verify --db john.db                    # full content-address recheck
  ```
- **Show:** encryption-at-rest turning on (AES-256-GCM warning), a backup stream,
  and an integrity verify pass.
- **Caption:** "Encrypted at rest. Backed up like git. Verifiable to the byte.
  One memory = one file you can move, encrypt, or crypto-erase."

## Close (2:20–2:30)

- **Screen:** the benchmark headline — "structural recall ~28µs · LoCoMo
  retrieval 81.6% hit@20 (54.2% end-to-end, untuned reader)" — then the repo URL.
- **VO / caption:** "DejaDB. The embedded memory engine for AI agents.
  Open source. github.com/AreevAI/dejadb"

---

## Production notes

- **Tooling:** [`vhs`](https://github.com/charmbracelet/vhs) for a scripted,
  reproducible cast, or [`asciinema`](https://asciinema.org) for a live capture +
  [`agg`](https://github.com/asciinema/agg) to export a GIF for the README.
- **Pre-seed** a `john.db` with a little history off-camera so `history` looks
  lived-in, or run the beats in order so it builds naturally.
- **Font:** a legible mono at large size; dark theme; hide the shell prompt noise.
- **Length:** keep under 3 minutes; the first 15 seconds must land the "a file you
  own" hook.
- When recorded, drop the cast/GIF into the README's **Demo** section (replace the
  placeholder comment) and link the full video.
