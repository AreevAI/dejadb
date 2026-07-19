//! Out-of-box LLM provider backends for Waiser (design: `waiser-reflection.md`
//! §9). Three adapters implement `waiser::LlmBackend` over a small **blocking**
//! HTTP client (`ureq`) — no tokio/reqwest, matching the tree's dependency-light
//! posture; the HTTP surface lives in this opt-in crate so `waiser`/core stay
//! serde-only:
//!
//! - [`OpenAiCompat`] — the workhorse. One `POST {base_url}/chat/completions`
//!   shape reaches ~90% of providers: OpenAI, Groq, DeepSeek, xAI, Together,
//!   Mistral, **Gemini's OpenAI-compat endpoint**, OpenRouter, LiteLLM, vLLM,
//!   LM Studio, and `llama.cpp` server.
//! - [`Anthropic`] — native `/v1/messages`.
//! - [`Ollama`] — native `/api/chat`, local, no key.
//!
//! [`resolve`] turns `--model provider:name` + environment keys into a boxed
//! backend, so the feature lights up with zero config when a standard key is
//! present. `--llm-cmd` (`waiser::CommandLlm`) remains the zero-dependency
//! escape hatch for anything these three don't cover.
//!
//! Each adapter translates the Waiser wire protocol (a JSON request whose
//! `instructions` field is the fixed engine prompt, kept separate from the
//! evidence data) into a chat request: `instructions` → the **system** message,
//! the remaining request JSON → the **user** message. Output is requested as
//! JSON; Waiser's parsers tolerate anything malformed (dropping that stage's
//! contribution), so a stray wrapper is safe.

use serde_json::{json, Value};
use std::time::Duration;
use waiser::{Error, LlmBackend, Result};

const CONNECT_SECS: u64 = 30;
const READ_SECS: u64 = 120; // the reflection loop is async/batchy — a slow call is fine

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(CONNECT_SECS))
        .timeout_read(Duration::from_secs(READ_SECS))
        .build()
}

/// POST a JSON body and return the parsed JSON response. `headers` is a slice of
/// (name, value). Maps transport / non-2xx / decode faults to `Error::LlmBackend`.
fn post_json(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Value> {
    let mut req = agent().post(url).set("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let body = serde_json::to_string(body)
        .map_err(|e| Error::LlmBackend(format!("encode request: {e}")))?;
    let text = req
        .send_string(&body)
        .map_err(|e| Error::LlmBackend(format!("{url}: {e}")))?
        .into_string()
        .map_err(|e| Error::LlmBackend(format!("read response: {e}")))?;
    serde_json::from_str(&text).map_err(|e| Error::LlmBackend(format!("decode response: {e}")))
}

/// Split a Waiser protocol request into (system, user): `instructions` become
/// the system prompt; the remaining fields (op/findings/evidence/claims) become
/// the user content, so the fixed instruction never interleaves with the
/// (possibly attacker-influenced) evidence text.
fn split_request(request: &str) -> (String, String) {
    let mut v: Value = serde_json::from_str(request).unwrap_or(Value::Null);
    let system = v
        .get("instructions")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("instructions");
    }
    let user = serde_json::to_string(&v).unwrap_or_else(|_| request.to_string());
    (system, user)
}

/// Answer the construction-time probe (`{"op":"probe"}`) locally — HTTP adapters
/// know their model, so no network call is needed.
fn probe_reply(request: &str, model: &str) -> Option<String> {
    let v: Value = serde_json::from_str(request).ok()?;
    (v.get("op").and_then(|o| o.as_str()) == Some("probe"))
        .then(|| json!({ "model": model }).to_string())
}

// ---- OpenAI-compatible (the workhorse / universal fallback) -----------------

pub struct OpenAiCompat {
    base_url: String,
    api_key: String,
    model: String,
}

impl OpenAiCompat {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        OpenAiCompat { base_url: base_url.into(), api_key: api_key.into(), model: model.into() }
    }

    fn build_body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            // Guaranteed syntactic JSON; the instruction carries the shape and
            // Waiser tolerates deviation. (A per-op json_schema is a refinement.)
            "response_format": {"type": "json_object"},
            "temperature": 0
        })
    }

    fn extract(resp: &Value) -> String {
        resp.pointer("/choices/0/message/content")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for OpenAiCompat {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let (system, user) = split_request(request);
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let resp = post_json(&url, &[("Authorization", &auth)], &self.build_body(&system, &user))?;
        Ok(Self::extract(&resp))
    }
}

// ---- Anthropic (native /v1/messages) ---------------------------------------

pub struct Anthropic {
    api_key: String,
    model: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Anthropic { api_key: api_key.into(), model: model.into() }
    }

    fn build_body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "max_tokens": 4096,
            "system": system,
            "messages": [{"role": "user", "content": user}]
        })
    }

    fn extract(resp: &Value) -> String {
        // {"content":[{"type":"text","text":"..."}], ...}
        resp.get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .find_map(|b| b.get("text").and_then(|t| t.as_str()))
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for Anthropic {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let (system, user) = split_request(request);
        let resp = post_json(
            "https://api.anthropic.com/v1/messages",
            &[("x-api-key", &self.api_key), ("anthropic-version", "2023-06-01")],
            &self.build_body(&system, &user),
        )?;
        Ok(Self::extract(&resp))
    }
}

// ---- Ollama (native /api/chat, local) --------------------------------------

pub struct Ollama {
    host: String,
    model: String,
}

impl Ollama {
    pub fn new(host: impl Into<String>, model: impl Into<String>) -> Self {
        Ollama { host: host.into(), model: model.into() }
    }

    fn build_body(&self, system: &str, user: &str) -> Value {
        json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user}
            ],
            "stream": false,
            "format": "json"
        })
    }

    fn extract(resp: &Value) -> String {
        resp.pointer("/message/content")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl LlmBackend for Ollama {
    fn model(&self) -> &str {
        &self.model
    }
    fn complete(&self, request: &str) -> Result<String> {
        if let Some(p) = probe_reply(request, &self.model) {
            return Ok(p);
        }
        let (system, user) = split_request(request);
        let url = format!("{}/api/chat", self.host.trim_end_matches('/'));
        let resp = post_json(&url, &[], &self.build_body(&system, &user))?;
        Ok(Self::extract(&resp))
    }
}

// ---- the factory -----------------------------------------------------------

/// The environment variable a provider reads its key from by default.
fn default_key_env(provider: &str) -> &'static str {
    match provider {
        "anthropic" | "claude" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    }
}

fn read_key(key_env: Option<&str>, provider: &str) -> Result<String> {
    let var = key_env.unwrap_or_else(|| default_key_env(provider));
    let k = std::env::var(var).map_err(|_| {
        Error::LlmBackend(format!(
            "${var} is not set — export your API key (or pass --llm-api-key-env)"
        ))
    })?;
    if k.trim().is_empty() {
        return Err(Error::LlmBackend(format!("${var} is empty")));
    }
    Ok(k)
}

/// Split `spec` into (provider, model). `provider:model` is explicit; a bare
/// name is routed by a prefix heuristic, then by which key is present, else to
/// local Ollama.
fn split_spec(spec: &str) -> (String, String) {
    if let Some((p, m)) = spec.split_once(':') {
        return (p.trim().to_lowercase(), m.trim().to_string());
    }
    let low = spec.to_lowercase();
    let provider = if low.starts_with("claude") {
        "anthropic"
    } else if low.starts_with("gpt") || low.starts_with('o') && low.chars().nth(1).is_some_and(|c| c.is_ascii_digit()) {
        "openai"
    } else if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        "anthropic"
    } else if std::env::var("OPENAI_API_KEY").is_ok() {
        "openai"
    } else {
        "ollama"
    };
    (provider.to_string(), spec.to_string())
}

/// Resolve `--model <spec>` into a backend. `base_url` overrides the endpoint
/// (or `$OPENAI_BASE_URL` / `$OLLAMA_HOST`); `key_env` overrides which env var
/// the key is read from. Keys are read from the environment, never taken on the
/// command line.
pub fn resolve(spec: &str, base_url: Option<&str>, key_env: Option<&str>) -> Result<Box<dyn LlmBackend>> {
    let (provider, model) = split_spec(spec);
    if model.is_empty() {
        return Err(Error::LlmBackend("--model: empty model name".into()));
    }
    match provider.as_str() {
        "anthropic" | "claude" => Ok(Box::new(Anthropic::new(read_key(key_env, "anthropic")?, model))),
        "openai" | "gpt" | "openai-compat" | "compat" => {
            let base = base_url
                .map(str::to_string)
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            Ok(Box::new(OpenAiCompat::new(base, read_key(key_env, "openai")?, model)))
        }
        "ollama" | "local" => {
            let host = base_url
                .map(str::to_string)
                .or_else(|| std::env::var("OLLAMA_HOST").ok())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            Ok(Box::new(Ollama::new(host, model)))
        }
        other => Err(Error::LlmBackend(format!(
            "unknown provider {other:?} (use anthropic|openai|ollama, or provider:model, or --llm-cmd)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_request_isolates_instructions() {
        let (sys, user) = split_request(
            r#"{"waiser":1,"op":"discover","instructions":"be careful","evidence":[{"hash":"h1"}]}"#,
        );
        assert_eq!(sys, "be careful");
        assert!(user.contains("\"op\":\"discover\""));
        assert!(user.contains("h1"));
        assert!(!user.contains("be careful"), "instructions must not leak into user content");
    }

    #[test]
    fn probe_is_answered_locally() {
        assert_eq!(
            probe_reply(r#"{"op":"probe"}"#, "m-1").as_deref(),
            Some(r#"{"model":"m-1"}"#)
        );
        assert!(probe_reply(r#"{"op":"discover"}"#, "m-1").is_none());
    }

    #[test]
    fn openai_body_and_extract() {
        let a = OpenAiCompat::new("https://x/v1", "k", "gpt-x");
        let b = a.build_body("sys", "usr");
        assert_eq!(b["model"], "gpt-x");
        assert_eq!(b["response_format"]["type"], "json_object");
        assert_eq!(b["messages"][0]["content"], "sys");
        let r = json!({"choices":[{"message":{"content":"{\"ok\":1}"}}]});
        assert_eq!(OpenAiCompat::extract(&r), "{\"ok\":1}");
    }

    #[test]
    fn anthropic_extract_reads_text_block() {
        let r = json!({"content":[{"type":"text","text":"hi"}]});
        assert_eq!(Anthropic::extract(&r), "hi");
    }

    #[test]
    fn ollama_body_sets_json_format() {
        let a = Ollama::new("http://localhost:11434", "llama3.1");
        assert_eq!(a.build_body("s", "u")["format"], "json");
        let r = json!({"message":{"content":"{}"}});
        assert_eq!(Ollama::extract(&r), "{}");
    }

    #[test]
    fn resolve_routes_by_prefix_and_explicit_provider() {
        std::env::set_var("ANTHROPIC_API_KEY", "test-key");
        assert_eq!(resolve("claude-sonnet", None, None).unwrap().model(), "claude-sonnet");
        assert_eq!(resolve("anthropic:my-model", None, None).unwrap().model(), "my-model");
        // Ollama needs no key.
        assert_eq!(resolve("ollama:llama3.1", None, None).unwrap().model(), "llama3.1");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn resolve_reports_missing_key() {
        // `Box<dyn LlmBackend>` isn't Debug, so match rather than unwrap_err.
        let e = match resolve("openai:gpt-x", None, Some("DEFINITELY_UNSET_VAR_XYZ")) {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-key error"),
        };
        assert!(e.to_string().contains("DEFINITELY_UNSET_VAR_XYZ"));
    }
}
