# Colab notebooks

Runnable in any Jupyter environment (JupyterLab, VS Code, Google Colab)
against the `dejadb` wheel — no server, no Rust toolchain. Outputs are
pre-baked from a validated end-to-end run, so every notebook reads well
without executing.

**Helpers ship inside the wheel.** Each notebook opens with one
`%pip install` and `from dejadb.helpers import *` (dejadb ≥ 1.0.4, current
release 1.0.5): `fresh` (clean re-runnable
opens), `facts`/`show_recs`/`audit`/`outcomes` (plain-dict views over the
JSON-string FFI), `days_later` (rehearse the 1d/7d/30d Verify checkpoints),
`auto_model` (LLM auto-detection), and `bar` (labeled chart + PNG export).

**LLM learning:** the learning notebooks call
`waiser_run(model=auto_model())`. Provide an `ANTHROPIC_API_KEY`,
`OPENAI_API_KEY`, or `OPENROUTER_API_KEY` (environment variable, or Colab's
Secrets panel) and the sweep adds an LLM discovery pass — drafts must survive
GROUND→VERIFY and human review before anything changes. Without a key, the
deterministic analyzers run everything; the floor is keyless.

**Run locally** (any Jupyter, or from a repo checkout to test unreleased changes):

```bash
python3 -m venv ~/.venvs/dejadb-demo
~/.venvs/dejadb-demo/bin/pip install maturin jupyterlab matplotlib
VIRTUAL_ENV=~/.venvs/dejadb-demo ~/.venvs/dejadb-demo/bin/maturin develop -m crates/dejadb-py/Cargo.toml
cd examples/colab && ~/.venvs/dejadb-demo/bin/jupyter lab
```

| Notebook | Story | Covers | ~Live time |
|---|---|---|---|
| [`01_memory_is_not_improvement.ipynb`](01_memory_is_not_improvement.ipynb) | A retailer's "where is my order?" assistant remembers everything and improves nothing — until the five-rung ladder: remember → recall → learn → govern → **measure** | What "self-improving" actually means; memory vs measurable improvement | 6–8 min |
| [`02_the_wrong_lesson.ipynb`](02_the_wrong_lesson.ipynb) | Month-end close: an approved lesson turns out wrong — measured **regression**, a system-filed revert, audited rollback — then an **LLM connects facts the agent had all along** (credential expiry → 401s → close deadline), adopted under the same review gate | Correcting bad knowledge; measuring lessons; rollback/supersede/erasure; LLM discovery, governed | 8–10 min |
| [`03_detect_review_govern.ipynb`](03_detect_review_govern.ipynb) | A customer-success desk: flaky CRM export flagged, ordinary flakiness correctly ignored, a contract-status contradiction resolved by supersession, autonomy granted only via policy file | Detecting tool failures & contradictions; reviewing/approving; four gates | 7–9 min |
| [`04_hermes_vs_governed.ipynb`](04_hermes_vs_governed.ipynb) | A refund agent during a flash sale: the write-approval-off reflection loop simulated in 12 lines (wrong skill, no evidence, no history) vs the same experience governed — where even the LLM's hunch queues for review | What Hermes is / skills from experience; the Hermes–DejaDB comparison | 6–8 min |
| [`05_enterprise_architecture.ipynb`](05_enterprise_architecture.ipynb) | File-per-tenant, encryption at rest with live wrong-key rejection, forget/crypto-erasure, bundle sync, mem0 migration with provenance, policy-as-code, reference deployment | Architectural considerations for enterprise agent systems | 7–9 min |
| [`06_agent_learns_from_conversation.ipynb`](06_agent_learns_from_conversation.ipynb) | A support desk runs a **real agent turn loop** — assemble → LLM extraction → capture back with provenance — then a handover call contradicts what it learned in March; deterministic detection resolves it, and an LLM pass finds a three-fact policy inconsistency no rule can reach | An agent learning from conversation; provenance; contradiction & correction; where the model earns its place | 8–10 min |
| [`self_improving_agents.ipynb`](self_improving_agents.ipynb) | **The full tour** — one arc end to end, plus the memory graph, vector recall, and the console launcher | All of it | 15–20 min |

Suggested pairings for a single session: **02 + 04** (the "wrong lesson"
headline plus the Hermes comparison), or **01 + 03** (concept ladder plus the
governance machinery), or the full tour alone.

Notes: notebooks create scratch `.db` files in the working directory and clean
them up on re-run. The full tour's vector cell downloads a ~90 MB embedding
model once (flag-gated); its console cell needs the `deja` CLI
(`cargo install dejadb`) — prebuilt binaries are tracked in
[#38](https://github.com/AreevAI/dejadb/issues/38).
