# examples

Copy-paste-runnable material for DejaDB + Waiser. These are docs-with-files,
not a package — clone the repo (they are not shipped in `pip`/`npm`/`cargo`
installs). See [`docs/waiser.md`](../docs/waiser.md) for the full guide.

| Dir | What |
|---|---|
| [`policy/`](policy/) | Three `waiser-policy.json` variants (solo / team / locked-down prod) |
| [`import/`](import/) | A tool-call JSONL sample + walkthrough → Tool grains → tool-failure clustering |
| [`ci/`](ci/) | A GitHub Actions job that fails the build on pending high-severity recommendations |
| [`mcp/`](mcp/) | The multi-agent supervisor pattern (separation of duties over MCP) |
| [`llm/`](llm/) | Ready-to-run `--llm-cmd` backends (`claude -p`, OpenAI, Ollama, a dependency-free mock) + the stdin/stdout protocol |
| [`analyzers/`](analyzers/) | A bring-your-own command analyzer (`--analyzer-cmd`, advisory-only) with the probe/analyze protocol |

Every example models **judgment** — approve one recommendation, dismiss one
with a reason. Never a rubber-stamp loop.
