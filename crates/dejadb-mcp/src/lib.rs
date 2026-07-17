//! dejadb-mcp — the built-in MCP server.
//!
//! Memory-semantic tools over newline-delimited JSON-RPC 2.0 on stdio —
//! not SQL-over-MCP. Tool surface is a tag-grouped set:
//! `dejadb_recall`, `dejadb_remember`, `dejadb_add`, `dejadb_supersede`,
//! `dejadb_forget`, `dejadb_cal`.
//!
//! Protocol errors are JSON-RPC errors; tool-execution failures are
//! `isError: true` tool results, per the MCP spec.

use std::io::{BufRead, Write};

use dejadb_cal::store_types::RecallParams;
use dejadb_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, DejaDbFacade};
use dejadb_core::error::Hash;
use dejadb_core::types::{Event, Role};
use dejadb_waiser::{now_ms, BorrowedSubstrate};
use serde_json::{json, Map, Value};
use waiser::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

pub const SERVER_NAME: &str = "dejadb";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Latest MCP protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

pub struct McpServer {
    facade: DejaDbFacade,
    executor: CalExecutor,
    default_ns: String,
    allow_destructive_ops: bool,
    /// When set, the session is pinned to this namespace: per-call `namespace`
    /// arguments are ignored and `dejadb_cal` queries are namespace-overridden,
    /// so an agent cannot read or write outside its partition.
    locked_ns: Option<String>,
}

impl McpServer {
    pub fn new(facade: DejaDbFacade, default_ns: Option<String>) -> Self {
        // Single source of truth for the session namespace: explicit arg,
        // else the facade's capability default, else "shared".
        let default_ns = default_ns
            .or_else(|| facade.default_namespace().map(String::from))
            .unwrap_or_else(|| "shared".to_string());
        McpServer {
            facade,
            executor: CalExecutor::new(CalExecutorConfig::default()),
            default_ns,
            allow_destructive_ops: true,
            locked_ns: None,
        }
    }

    /// Rebuild the executor from the current gate settings. Called by every
    /// builder so `--lock-ns` and `--no-destructive-ops` compose instead of
    /// clobbering each other's config.
    fn rebuild_executor(&mut self) {
        self.executor = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: self.allow_destructive_ops,
            namespace_override: self.locked_ns.clone(),
            ..CalExecutorConfig::default()
        });
    }

    /// Permit or forbid destructive operations — the `dejadb_forget` tool and
    /// `FORGET <hash>` via `dejadb_cal`. Enabled by default; pass `false` to
    /// serve a read-only session (`deja serve --mcp --no-destructive-ops`).
    pub fn allow_destructive_ops(mut self, allow: bool) -> Self {
        self.allow_destructive_ops = allow;
        self.rebuild_executor();
        self
    }

    /// Pin the session to a single namespace (`deja serve --mcp --lock-ns NS`).
    /// Per-call `namespace` arguments are ignored and CAL queries are
    /// namespace-overridden, so a multi-tenant host can hand an agent a session
    /// it cannot escape. Without this, namespaces are filters, not boundaries.
    pub fn lock_namespace(mut self, ns: impl Into<String>) -> Self {
        let ns = ns.into();
        self.default_ns = ns.clone();
        self.locked_ns = Some(ns);
        self.rebuild_executor();
        self
    }

    /// Serve until EOF on the reader (stdio transport: one JSON-RPC
    /// message per line).
    pub fn serve<R: BufRead, W: Write>(&self, reader: R, mut writer: W) -> std::io::Result<()> {
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    let resp = json!({"jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": format!("parse error: {e}")}});
                    writeln!(writer, "{resp}")?;
                    writer.flush()?;
                    continue;
                }
            };
            let id = msg.get("id").cloned();
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            // Notifications get no response.
            if id.is_none() || id == Some(Value::Null) {
                continue;
            }
            let id = id.unwrap();
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let resp = match method {
                "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL_VERSION),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                }}),
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tool_defs()}}),
                "tools/call" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = params
                        .get("arguments")
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    match self.call_tool(name, &args) {
                        Ok(text) => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": text}], "isError": false}}),
                        Err(e) => json!({"jsonrpc": "2.0", "id": id, "result": {
                            "content": [{"type": "text", "text": e}], "isError": true}}),
                    }
                }
                other => json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {other}")}}),
            };
            writeln!(writer, "{resp}")?;
            writer.flush()?;
        }
        Ok(())
    }

    pub fn serve_stdio(&self) -> std::io::Result<()> {
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        self.serve(stdin.lock(), stdout.lock())
    }

    fn ns<'a>(&'a self, args: &'a Map<String, Value>) -> &'a str {
        // A locked session ignores caller-supplied namespaces entirely.
        if self.locked_ns.is_some() {
            return &self.default_ns;
        }
        args.get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.default_ns)
    }

    /// Set the `namespace` field for a write. A locked session **forces** it
    /// (overwriting any caller-supplied value in `fields`), so an agent cannot
    /// write outside its partition; otherwise it fills in the session default
    /// only when the caller left it unset.
    fn set_ns(&self, fields: &mut Map<String, Value>, args: &Map<String, Value>) {
        let ns = Value::String(self.ns(args).to_string());
        if self.locked_ns.is_some() {
            fields.insert("namespace".to_string(), ns);
        } else {
            fields.entry("namespace".to_string()).or_insert(ns);
        }
    }

    fn call_tool(&self, name: &str, args: &Map<String, Value>) -> Result<String, String> {
        match name {
            "dejadb_recall" => {
                let subject = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .ok_or("dejadb_recall requires 'subject'")?;
                let mut p = RecallParams::default();
                p.subject = Some(subject.to_string());
                p.relation = args.get("relation").and_then(|v| v.as_str()).map(String::from);
                p.namespace = Some(self.ns(args).to_string());
                p.limit = Some(
                    args.get("k").and_then(|v| v.as_u64()).unwrap_or(16) as usize
                );
                let hits = self.facade.recall(&p).map_err(|e| e.to_string())?;
                let out: Vec<Value> = hits
                    .iter()
                    .map(|h| {
                        json!({
                            "hash": h.hash.to_hex(),
                            "type": format!("{:?}", h.grain.grain_type).to_lowercase(),
                            "fields": h.grain.fields,
                        })
                    })
                    .collect();
                Ok(serde_json::to_string(&out).unwrap_or_default())
            }
            "dejadb_add" => {
                let gtype = args
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("fact");
                let mut fields = args
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .ok_or("dejadb_add requires 'fields' object")?;
                self.set_ns(&mut fields, args);
                // `idempotent: true` collapses a re-add of the value already at
                // the (subject, relation) head — for learning loops that would
                // otherwise re-store the same distilled lesson.
                if args.get("idempotent").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let (h, inserted) =
                        self.facade.cal_add_if_novel(gtype, &fields).map_err(|e| e.to_string())?;
                    Ok(json!({"hash": h.to_hex(), "inserted": inserted}).to_string())
                } else {
                    let h = self.facade.cal_add(gtype, &fields).map_err(|e| e.to_string())?;
                    Ok(json!({"hash": h.to_hex()}).to_string())
                }
            }
            "dejadb_supersede" => {
                let old = args
                    .get("old_hash")
                    .and_then(|v| v.as_str())
                    .ok_or("dejadb_supersede requires 'old_hash'")?;
                let old = Hash::from_hex(old).map_err(|e| e.to_string())?;
                let gtype = args.get("type").and_then(|v| v.as_str()).unwrap_or("fact");
                let mut fields = args
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .ok_or("dejadb_supersede requires 'fields' object")?;
                self.set_ns(&mut fields, args);
                let h = self
                    .facade
                    .cal_supersede(&old, gtype, &fields)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": h.to_hex(), "supersedes": old.to_hex()}).to_string())
            }
            "dejadb_forget" => {
                if !self.allow_destructive_ops {
                    return Err("destructive operations are disabled for this server \
                                (started with --no-destructive-ops)"
                        .to_string());
                }
                let h = args
                    .get("hash")
                    .and_then(|v| v.as_str())
                    .ok_or("dejadb_forget requires 'hash'")?;
                let h = Hash::from_hex(h).map_err(|e| e.to_string())?;
                self.facade
                    .with_store(|m| m.forget(&h))
                    .map_err(|e| e.to_string())?;
                Ok(json!({"forgotten": h.to_hex()}).to_string())
            }
            "dejadb_remember" => {
                // v1: store the raw content as an Event grain. Extraction into
                // Facts is host-side (the `remember()` callback seam) — the
                // MCP client (the model) can follow up with dejadb_add for the
                // distilled facts it wants to keep.
                let content = args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .ok_or("dejadb_remember requires 'content'")?;
                let ns = self.ns(args).to_string();
                let session = args.get("session_id").and_then(|v| v.as_str()).map(String::from);
                let role = args
                    .get("role")
                    .and_then(|v| v.as_str())
                    .and_then(Role::from_str);
                let h = self
                    .facade
                    .with_store(|m| {
                        let mut e = Event::new(content);
                        e.common.namespace = Some(ns.clone());
                        e.session_id = session.clone();
                        e.role = role;
                        m.add(&e)
                    })
                    .map_err(|e| e.to_string())?;
                Ok(json!({"hash": h.to_hex(), "stored_as": "event",
                    "note": "distill durable facts with dejadb_add"}).to_string())
            }
            "dejadb_cal" => {
                let query = args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .ok_or("dejadb_cal requires 'query'")?;
                let res = self
                    .executor
                    .execute(query, &self.facade)
                    .map_err(|e| e.to_string())?;
                serde_json::to_string(&res.result).map_err(|e| e.to_string())
            }
            "dejadb_waiser" => {
                let opts = RunOptions {
                    min_new: args.get("min_new").and_then(Value::as_u64),
                    min_new_errors: args.get("min_new_errors").and_then(Value::as_u64),
                    if_stale_ms: None,
                    namespaces: Vec::new(),
                };
                let mut sub = BorrowedSubstrate::new(&self.facade);
                let engine = Engine::with_builtins();
                let res = engine.run(&mut sub, &opts, now_ms()).map_err(|e| e.to_string())?;
                let pending = engine
                    .recommendations(&sub, Some(RecStatus::Pending))
                    .map_err(|e| e.to_string())?;
                let list: Vec<Value> = pending.iter().map(rec_json).collect();
                Ok(json!({ "run": res, "pending": list }).to_string())
            }
            "dejadb_recommendations" => {
                let engine = Engine::with_builtins();
                let action = args.get("action").and_then(Value::as_str);
                if let Some(action) = action {
                    let hash = args
                        .get("hash")
                        .and_then(Value::as_str)
                        .ok_or("dejadb_recommendations action requires 'hash'")?;
                    let because = args
                        .get("because")
                        .and_then(Value::as_str)
                        .ok_or("dejadb_recommendations action requires 'because'")?;
                    let actor = "agent:mcp";
                    let scopes = ScopeSet::all();
                    let mut sub = BorrowedSubstrate::new(&self.facade);
                    let now = now_ms();
                    match action {
                        "apply" => {
                            engine
                                .review(&mut sub, hash, Decision::Approve, actor, ObserverType::Agent, &scopes, because, now)
                                .map_err(|e| e.to_string())?;
                            engine
                                .apply(&mut sub, hash, actor, ObserverType::Agent, &scopes, because, false, now)
                                .map_err(|e| e.to_string())?;
                        }
                        "approve" => engine
                            .review(&mut sub, hash, Decision::Approve, actor, ObserverType::Agent, &scopes, because, now)
                            .map_err(|e| e.to_string())?,
                        "reject" | "dismiss" => engine
                            .review(&mut sub, hash, Decision::Reject, actor, ObserverType::Agent, &scopes, because, now)
                            .map_err(|e| e.to_string())?,
                        other => return Err(format!("unknown action '{other}' (apply|approve|reject)")),
                    }
                    Ok(json!({ "hash": hash, "action": action }).to_string())
                } else {
                    let status = args
                        .get("status")
                        .and_then(Value::as_str)
                        .filter(|s| *s != "all")
                        .map(status_or_pending)
                        .or(Some(RecStatus::Pending));
                    let sub = BorrowedSubstrate::new(&self.facade);
                    let recs = engine.recommendations(&sub, status).map_err(|e| e.to_string())?;
                    let list: Vec<Value> = recs.iter().map(rec_json).collect();
                    Ok(serde_json::to_string(&list).unwrap_or_default())
                }
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

fn status_or_pending(s: &str) -> RecStatus {
    match s {
        "approved" => RecStatus::Approved,
        "rejected" => RecStatus::Rejected,
        "applied" => RecStatus::Applied,
        "rolled_back" => RecStatus::RolledBack,
        "expired" => RecStatus::Expired,
        _ => RecStatus::Pending,
    }
}

fn rec_json(r: &waiser::Recommendation) -> Value {
    json!({
        "hash": r.hash,
        "status": r.status.as_str(),
        "severity": r.severity.as_str(),
        "analyzer": r.analyzer,
        "summary": r.summary.render(),
        "target_ref": r.target_ref,
        "destructive": r.destructive,
    })
}

fn tool_defs() -> Vec<Value> {
    let s = |desc: &str| json!({"type": "string", "description": desc});
    vec![
        json!({
            "name": "dejadb_recall",
            "description": "Recall current memories about a subject (structural, µs-class). Returns grains newest-first.",
            "inputSchema": {"type": "object", "properties": {
                "subject": s("entity to recall about, e.g. 'caller:john'"),
                "relation": s("optional relation filter, e.g. 'prefers'"),
                "namespace": s("optional namespace (defaults to session namespace)"),
                "k": {"type": "integer", "description": "max results (default 16)"}
            }, "required": ["subject"]}
        }),
        json!({
            "name": "dejadb_add",
            "description": "Add a durable memory grain (append-only; content-addressed). Use type 'fact' with subject/relation/object fields for structured knowledge.",
            "inputSchema": {"type": "object", "properties": {
                "type": s("grain type: fact|event|state|goal|observation|... (default fact)"),
                "fields": {"type": "object", "description": "grain fields, e.g. {subject, relation, object, confidence}"},
                "namespace": s("optional namespace"),
                "idempotent": {"type": "boolean", "description": "if true, skip the write when this exact value is already the current head for (subject, relation) — returns the existing hash and inserted:false. Use when re-learning a fact you may already hold."}
            }, "required": ["fields"]}
        }),
        json!({
            "name": "dejadb_supersede",
            "description": "Evolve a memory: write a new version superseding old_hash. The old version is preserved (append-only history), never deleted.",
            "inputSchema": {"type": "object", "properties": {
                "old_hash": s("content address (64-hex) of the version to supersede"),
                "type": s("grain type of the new version (default fact)"),
                "fields": {"type": "object"},
                "namespace": s("optional namespace")
            }, "required": ["old_hash", "fields"]}
        }),
        json!({
            "name": "dejadb_forget",
            "description": "Erase a grain from the hot store (tombstoned in the op-log). Host-level operation — not reachable from CAL.",
            "inputSchema": {"type": "object", "properties": {
                "hash": s("content address (64-hex) to forget")
            }, "required": ["hash"]}
        }),
        json!({
            "name": "dejadb_remember",
            "description": "Store raw conversational content as an Event grain (session transcript). Distill durable knowledge with dejadb_add afterwards.",
            "inputSchema": {"type": "object", "properties": {
                "content": s("the utterance/observation text"),
                "session_id": s("optional session/thread id"),
                "role": s("optional: user|assistant|system|tool"),
                "namespace": s("optional namespace")
            }, "required": ["content"]}
        }),
        json!({
            "name": "dejadb_cal",
            "description": "Execute a CAL statement (RECALL/ASSEMBLE/EXISTS/HISTORY/ADD/SUPERSEDE/...). CAL is structurally incapable of deleting data.",
            "inputSchema": {"type": "object", "properties": {
                "query": s("CAL text, e.g. RECALL facts WHERE subject = \"alice\" | COUNT")
            }, "required": ["query"]}
        }),
        json!({
            "name": "dejadb_waiser",
            "description": "Run one governed self-improvement pass (deterministic, no LLM) and return the run outcome plus the pending recommendation queue. Call at session start; review pending recommendations before acting.",
            "inputSchema": {"type": "object", "properties": {
                "min_new": {"type": "integer", "description": "only run if at least this many new grains since the last run (optional)"},
                "min_new_errors": {"type": "integer", "description": "or this many new tool failures (optional)"}
            }}
        }),
        json!({
            "name": "dejadb_recommendations",
            "description": "List recommendations, or act on one. Without 'action', lists by status (default pending). With action=apply|approve|reject and a 'hash' + mandatory 'because' reason, performs the audited transition (self-approval of an agent's own proposals is blocked).",
            "inputSchema": {"type": "object", "properties": {
                "status": s("filter: pending|approved|applied|all (default pending)"),
                "action": s("apply|approve|reject (omit to list)"),
                "hash": s("recommendation hash (required for an action)"),
                "because": s("mandatory written reason for the decision")
            }}
        }),
    ]
}
