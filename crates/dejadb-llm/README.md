# dejadb-llm

Out-of-box LLM provider backends for [Waiser](../waiser)'s reflection engine
(design: [`docs/waiser-reflection.md`](../../docs/waiser-reflection.md) §9).

Three adapters implement `waiser::LlmBackend` over a small **blocking** HTTP
client (`ureq`) — no tokio/reqwest, matching the tree's dependency-light posture.
The HTTP surface lives in this opt-in crate so `waiser` and the core stay
serde-only.

| Adapter | Endpoint | Reaches |
|---|---|---|
| `OpenAiCompat` | `POST {base_url}/chat/completions` | OpenAI, Groq, DeepSeek, xAI, Together, Mistral, **Gemini (OpenAI-compat)**, OpenRouter, LiteLLM, vLLM, LM Studio, `llama.cpp` server |
| `Anthropic` | `POST /v1/messages` | Claude models |
| `Ollama` | `POST /api/chat` | local models, no key |

## Use it from `deja`

```bash
export ANTHROPIC_API_KEY=sk-...
deja waiser run --db agent.db --model claude-sonnet     # key from the env
deja waiser run --db agent.db --model openai:gpt-5
deja waiser run --db agent.db --model ollama:llama3.1   # local, no key
```

Keys are read from the environment (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` /
`OLLAMA_HOST`, or `--llm-api-key-env VAR`), never taken on the command line.
`--llm-base-url` points the OpenAI-compatible adapter at any gateway/local
server. `--llm-cmd` (a subprocess) remains the zero-dependency escape hatch for
anything these three don't cover.

## Library

```rust
let backend = dejadb_llm::resolve("claude-sonnet", None, None)?; // Box<dyn waiser::LlmBackend>
let engine = waiser::Engine::with_builtins().with_llm(backend);
```

Each adapter translates the Waiser wire protocol (a JSON request whose
`instructions` field is the fixed engine prompt, kept separate from the evidence
data) into a chat request — `instructions` → the system message, the rest → the
user message — and requests JSON output. Waiser's parsers tolerate malformed
output (dropping that stage's contribution), so the whole thing is fail-soft.

Not published during the engine's churn phase (`publish = false`).
