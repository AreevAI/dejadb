//! dejadb — Node.js (napi-rs) bindings for DejaDB.
//!
//! Mirrors the Python binding (crates/dejadb-py): thin and version-stable by
//! design — scalar args in, JSON strings out for anything structured; every
//! error surfaces as a JS `Error`. turso/tokio are native, so this is a
//! *native* Node addon (napi-rs), not WASM. Build with
//! `napi build --platform --release`; `require('dejadb')`.

use dejadb_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, DejaDbFacade};
use dejadb_core::error::{DejaDbError, Hash};
use dejadb_store::memory_tool::MemoryTool;
use dejadb_store::{
    parse_relations, Axis, CommandEmbed, DejaDB as RustDejaDB, Direction, FactDraft, TelemetryMode,
};
use dejadb_waiser::{now_ms, BorrowedSubstrate};
use napi_derive::napi;
use serde_json::json;
use waiser::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

/// Resolve an LLM backend the same two ways the CLI does: a subprocess
/// (`llmCmd`, the zero-dependency escape hatch) or a built-in HTTP provider
/// (`model`, key read from the environment). The subprocess wins when both are
/// given. Both fail at construction, before anything is written.
fn resolve_llm(
    cmd: Option<String>,
    spec: Option<String>,
) -> napi::Result<Option<Box<dyn waiser::LlmBackend>>> {
    if let Some(cmd) = cmd {
        return Ok(Some(Box::new(waiser::CommandLlm::new(&cmd, None).map_err(err)?)));
    }
    if let Some(spec) = spec {
        return Ok(Some(dejadb_llm::resolve(&spec, None, None).map_err(err)?));
    }
    Ok(None)
}

/// Run the shared extract → confidence floor → ground pipeline and convert the
/// survivors to store drafts. An extraction that fails names the Event
/// the raw text was already stored under, so the caller can retry against it
/// instead of losing the content.
fn extract_and_ground(
    llm: &dyn waiser::LlmBackend,
    grounder: Option<&dyn waiser::LlmBackend>,
    source: &Hash,
    content: &str,
    hint: Option<&str>,
    min_confidence: f64,
) -> napi::Result<(usize, Vec<FactDraft>, Option<&'static str>)> {
    let hex = source.to_hex();
    let ex = dejadb_llm::extract_pipeline(llm, grounder, &hex, content, hint, min_confidence)
        .map_err(|e| err(format!("{e} (event {hex} was stored)")))?;
    let drafts = ex
        .facts
        .into_iter()
        .map(|f| FactDraft {
            subject: f.subject,
            relation: f.relation,
            object: f.object,
            confidence: f.confidence,
        })
        .collect();
    let status = if ex.grounded { "verified" } else { "unverified" };
    Ok((ex.proposed, drafts, Some(status)))
}

/// Parse a duration like `6h` / `30m` / `2d` / `3600s` into milliseconds.
fn parse_duration_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match &s[split..] {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

fn status_from_str(s: &str) -> Option<RecStatus> {
    match s {
        "pending" => Some(RecStatus::Pending),
        "approved" => Some(RecStatus::Approved),
        "rejected" => Some(RecStatus::Rejected),
        "applied" => Some(RecStatus::Applied),
        "rolled_back" => Some(RecStatus::RolledBack),
        "expired" => Some(RecStatus::Expired),
        _ => None,
    }
}

fn parse_hash(hex: &str) -> napi::Result<Hash> {
    Hash::from_hex(hex).map_err(err)
}

/// Runs one store call on libuv's thread pool and settles a JS promise with
/// the result.
///
/// Every method here used to do its work inline, on the thread calling into
/// the addon — which in Node is the thread running everything else. A single
/// `importBundle` or `migrate` stopped timers, sockets and the HTTP server for
/// as long as it took. Node has exactly one place to put blocking work, and
/// this is it.
///
/// `Task::compute` deliberately runs on a libuv worker rather than on a tokio
/// runtime: the store owns a current-thread runtime and drives it with
/// `block_on`, and doing that from inside another runtime's worker panics.
/// A libuv thread has no runtime attached, so `block_on` is free to take it.
/// One job type per return type, rather than one generic job.
///
/// A generic `StoreJob<T>` compiles and runs, but napi's TypeScript generator
/// cannot see through the parameter: a type alias comes out as the literal
/// `Job<string>` (which is not a type the `.d.ts` defines) and the un-aliased
/// form degrades to `Promise<unknown>`. Both hand callers a binding that works
/// at runtime and lies at compile time. Concrete types generate the real
/// signatures — `Promise<string>`, `Promise<string | null>`, `Promise<void>`.
macro_rules! job_types {
    ($($(#[$m:meta])* $name:ident => $ty:ty),* $(,)?) => {$(
        $(#[$m])*
        pub struct $name {
            work: Option<Box<dyn FnOnce() -> napi::Result<$ty> + Send>>,
        }

        impl $name {
            fn spawn(
                work: impl FnOnce() -> napi::Result<$ty> + Send + 'static,
            ) -> napi::bindgen_prelude::AsyncTask<Self> {
                napi::bindgen_prelude::AsyncTask::new($name { work: Some(Box::new(work)) })
            }
        }

        impl napi::Task for $name {
            type Output = $ty;
            type JsValue = $ty;

            fn compute(&mut self) -> napi::Result<$ty> {
                // Called once per task; the Option exists only because the
                // trait hands out `&mut self` rather than `self`.
                match self.work.take() {
                    Some(work) => work(),
                    None => Err(err("store job polled twice")),
                }
            }

            fn resolve(&mut self, _env: napi::Env, output: $ty) -> napi::Result<$ty> {
                Ok(output)
            }
        }
    )*};
}

job_types! {
    /// Store call whose result is a JSON string — most of this surface.
    StringJob => String,
    /// Store call that can legitimately answer "nothing" (`latest`).
    MaybeStringJob => Option<String>,
    /// Store call kept for its effect (`forget`, `setEmbedderCommand`).
    UnitJob => (),
    /// Store call returning a count.
    U32Job => u32,
    /// Store call returning an op-log cursor.
    I64Job => i64,
}

/// One memory = one file. Open with `new DejaDb("caller.db", "caller")`.
///
/// Every method returns a promise. Opening is the one exception — it is
/// synchronous, so a constructor can still fail loudly.
///
/// **Await your writes.** Promises settle in completion order, not call order,
/// and concurrent calls contend for one lock inside the store. Firing
/// `addFact` and `recall` without awaiting leaves which one lands first up to
/// the thread pool.
#[napi]
pub struct DejaDb {
    /// Shared so a queued job can hold the store open independently of the JS
    /// object that started it.
    facade: std::sync::Arc<DejaDbFacade>,
    ns: String,
    /// Host-asserted actor label stamped on every waiser audit grain (§6.6).
    actor: String,
}

#[napi]
impl DejaDb {
    #[napi(constructor)]
    pub fn new(
        path: String,
        ns: Option<String>,
        passphrase: Option<String>,
        actor: Option<String>,
        telemetry: Option<String>,
    ) -> napi::Result<Self> {
        let ns = ns.unwrap_or_else(|| "shared".to_string());
        let actor = actor.unwrap_or_else(|| "user:local".to_string());
        // Recall-telemetry sidecar (host capability, §8): agents are the main
        // telemetry producers, so the binding default is `aggregate`; pass
        // telemetry="off" to disable. Never a file-truth.
        let telemetry = telemetry.unwrap_or_else(|| "aggregate".to_string());
        let tel = TelemetryMode::parse(&telemetry)
            .ok_or_else(|| err(format!("unknown telemetry mode '{telemetry}' (off|aggregate|full)")))?;
        // Encryption at rest: a passphrase derives an AES-256 key (Argon2id;
        // non-secret salt in a <path>.kdf sidecar). Host-supplied, never
        // stored in the file — same rules as the CLI's --passphrase-env.
        let store = match passphrase {
            Some(p) => RustDejaDB::open_with_passphrase_telemetry(&path, &p, tel).map_err(err)?,
            None => RustDejaDB::open_with_telemetry(&path, tel).map_err(err)?,
        };
        let facade = std::sync::Arc::new(DejaDbFacade::with_session(store, Some(ns.clone()), None));
        Ok(DejaDb { facade, ns, actor })
    }

    /// Reconciliation warnings from open (file-vs-host declaration changes,
    /// embedding-model mismatches). JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn open_warnings(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let w = facade.with_store(|m| m.open_warnings().to_vec());
            serde_json::to_string(&w).map_err(err)
        })
    }

    /// Install a command embedder (same contract as the CLI's --embed-cmd):
    /// the command gets the text on stdin and must print a JSON array of
    /// numbers. Probed once here to learn the dimension. Enables the vector
    /// recall leg; grains added afterwards are embedded. (An in-process JS
    /// callback embedder needs an async surface — planned; the command
    /// embedder is the stable path today.)
    #[napi(ts_return_type = "Promise<void>")]
    pub fn set_embedder_command(
        &self,
        cmd: String,
        model: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let facade = self.facade.clone();
        UnitJob::spawn(move || {
            // Probing the command spawns a child process — worth keeping off
            // the event loop even though it only happens once.
            let ce = CommandEmbed::new(&cmd, model.as_deref()).map_err(err)?;
            facade.with_store(|m| m.set_embedder(Box::new(ce)));
            Ok(())
        })
    }

    /// Backfill + rebuild the BM25 text index (e.g. after bulk loads, or on
    /// a file that flipped text indexing on later). Returns rows backfilled.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn reindex_text(&self) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let facade = self.facade.clone();
        U32Job::spawn(move || {
            facade
                .with_store(|m| m.rebuild_text_index())
                .map(|n| n as u32)
                .map_err(err)
        })
    }

    /// Anthropic memory-tool command (view/create/str_replace/insert/delete/
    /// rename over /memories): pass the tool-call object as JSON; returns the
    /// tool result text. Wire this as your memory-tool backend.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn memory_tool(
        &self,
        command_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let cmd: serde_json::Value = serde_json::from_str(&command_json).map_err(err)?;
            facade
                .with_store(|m| {
                    let mut t = MemoryTool::new(m, &ns);
                    t.execute(&cmd)
                })
                .map_err(err)
        })
    }

    /// Import another memory system's export. `source`: mem0 | mem0-history |
    /// langgraph | letta | letta-archival | zep | jsonl. `payload` is the
    /// export file's contents; `history` the optional mem0 history payload.
    /// (basic-memory vault directories import via the CLI: `deja migrate`.)
    /// Returns {added, superseded, forgotten, skipped, notes} as JSON.
    /// Re-runs skip what is already imported.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn migrate(
        &self,
        source: String,
        payload: String,
        history: Option<String>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let rep = facade
                .with_store(|m| {
                    dejadb_store::migrate::migrate_payload(
                        m,
                        &ns,
                        &source,
                        &payload,
                        history.as_deref(),
                    )
                })
                .map_err(err)?;
            Ok(rep.to_json().to_string())
        })
    }

    /// Add a Fact. Returns the content address (64-hex).
    /// Add a Fact. With `idempotent = true`, a re-add of the value already at
    /// the `(subject, relation)` head writes nothing and returns the existing
    /// hash (value-level dedup, not just byte-identical replay).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add_fact(
        &self,
        subject: String,
        relation: String,
        object: String,
        confidence: Option<f64>,
        ns: Option<String>,
        idempotent: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("confidence".into(), json!(confidence.unwrap_or(0.9)));
        fields.insert(
            "namespace".into(),
            json!(ns.unwrap_or_else(|| self.ns.clone())),
        );
        let idempotent = idempotent.unwrap_or(false);
        StringJob::spawn(move || {
            if idempotent {
                Ok(facade.cal_add_if_novel("fact", &fields).map_err(err)?.0.to_hex())
            } else {
                Ok(facade.cal_add("fact", &fields).map_err(err)?.to_hex())
            }
        })
    }

    /// Add any grain type from a JSON fields object. Returns the hash.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn add(
        &self,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let mut fields: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&fields_json).map_err(err)?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(default_ns));
            Ok(facade.cal_add(&grain_type, &fields).map_err(err)?.to_hex())
        })
    }

    /// Structural recall, newest-first. Returns a JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recall(
        &self,
        subject: String,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(16) as usize;
        StringJob::spawn(move || {
            let grains = facade
                .with_store(|m| m.recall(&ns, &subject, relation.as_deref(), k))
                .map_err(err)?;
            let out: Vec<serde_json::Value> = grains
                .iter()
                .map(|g| {
                    json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "fields": g.fields,
                    })
                })
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Current head for (subject, relation) — JSON string or null.
    #[napi(ts_return_type = "Promise<string | null>")]
    pub fn latest(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<MaybeStringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        MaybeStringJob::spawn(move || {
            let head = facade
                .with_store(|m| m.latest(&ns, &subject, &relation))
                .map_err(err)?;
            Ok(head.map(|g| {
                json!({
                    "hash": g.hash.to_hex(),
                    "fields": g.fields,
                })
                .to_string()
            }))
        })
    }

    /// Supersede old_hash with a new version (append-only evolution).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn supersede(
        &self,
        old_hash: String,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let default_ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let old = parse_hash(&old_hash)?;
            let mut fields: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&fields_json).map_err(err)?;
            fields
                .entry("namespace".to_string())
                .or_insert_with(|| json!(default_ns));
            Ok(facade
                .cal_supersede(&old, &grain_type, &fields)
                .map_err(err)?
                .to_hex())
        })
    }

    /// Erase a grain from the hot store (tombstoned). Host-level op.
    #[napi(ts_return_type = "Promise<void>")]
    pub fn forget(&self, hash: String) -> napi::bindgen_prelude::AsyncTask<UnitJob> {
        let facade = self.facade.clone();
        UnitJob::spawn(move || {
            let h = parse_hash(&hash)?;
            facade.with_store(|m| m.forget(&h)).map_err(err)
        })
    }

    /// remember(): store content as an Observation, then attach the facts
    /// distilled from it. Three routes to those facts, in precedence order:
    /// `factsJson` (pre-extracted by the host — a JSON list of
    /// {subject, relation, object, confidence}), `llmCmd` (a subprocess
    /// backend), or `model` ("openai:gpt-4o-mini", key from the env).
    ///
    /// The raw text is written before the model is called, so a failed
    /// extraction never costs the raw text — the error names the hash it was
    /// stored under. Model-extracted facts are stamped
    /// `verification_status="unverified"` unless `groundModel`/`groundCmd`
    /// runs a separate entailment pass (proposer ≠ scorer); facts it does not
    /// support are dropped and survivors are stamped `"verified"`.
    ///
    /// The raw text is stored as an **Event** grain (a transcript turn) —
    /// pass `sessionId`/`role` to place it in a conversation thread.
    ///
    /// Returns {"event", "facts"} JSON, plus {"model", "proposed",
    /// "dropped", "verification_status"} when a model ran.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn remember(
        &self,
        content: String,
        facts_json: Option<String>,
        observer: Option<String>,
        ns: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
        extract_hint: Option<String>,
        min_confidence: Option<f64>,
        session_id: Option<String>,
        role: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let observer = observer.unwrap_or_else(|| "node".to_string());
        // Runs on the worker pool: a model call is a network round trip, and
        // blocking the event loop across it was the worst case of the old
        // synchronous surface.
        StringJob::spawn(move || {
            let explicit = match facts_json {
                Some(j) => Some(FactDraft::from_json_array(&j).map_err(err)?),
                None => None,
            };
            let llm = match explicit {
                Some(_) => None,
                None => resolve_llm(llm_cmd, model)?,
            };
            let grounder = match llm {
                Some(_) => resolve_llm(ground_cmd, ground_model)?,
                None => None,
            };
            let meta = dejadb_store::Capture {
                observer: Some(observer.as_str()),
                session_id: session_id.as_deref(),
                role: role.as_deref(),
            };
            let event = facade
                .with_store(|m| m.capture(&ns, &content, &meta))
                .map_err(err)?;

            let (proposed, drafts, status) = match &llm {
                None => {
                    let d = explicit.unwrap_or_default();
                    (d.len(), d, None)
                }
                Some(l) => extract_and_ground(
                    l.as_ref(),
                    grounder.as_deref(),
                    &event,
                    &content,
                    extract_hint.as_deref(),
                    min_confidence.unwrap_or(0.0),
                )?,
            };
            let attribution = dejadb_store::FactAttribution {
                verification_status: status,
                extractor_model: llm.as_ref().map(|l| l.model()),
            };
            let facts = facade
                .with_store(|m| m.attach_facts(&ns, &event, &drafts, &attribution))
                .map_err(err)?;

            let mut out = json!({
                "event": event.to_hex(),
                "facts": facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
            });
            if let (Some(obj), Some(l)) = (out.as_object_mut(), &llm) {
                obj.insert("model".into(), json!(l.model()));
                obj.insert("verification_status".into(), json!(status));
                obj.insert("proposed".into(), json!(proposed));
                obj.insert("dropped".into(), json!(proposed.saturating_sub(facts.len())));
            }
            Ok(out.to_string())
        })
    }

    /// Execute CAL. Returns the wire-format payload as a JSON string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn cal(&self, query: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let ex = CalExecutor::new(CalExecutorConfig::default());
            let res = ex.execute(&query, &*facade).map_err(err)?;
            serde_json::to_string(&res.result).map_err(err)
        })
    }

    /// Supersession-chain history for (subject, relation), newest first.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn history(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let versions = facade
                .with_store(|m| m.history(&ns, &subject, &relation))
                .map_err(err)?;
            let out: Vec<serde_json::Value> = versions
                .iter()
                .map(|v| {
                    json!({
                        "hash": v.hash.to_hex(), "object": v.object,
                        "created_at": v.created_at, "confidence": v.confidence,
                        "superseded_by": v.superseded_by.map(|h| h.to_hex()),
                    })
                })
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Reverse provenance: grains distilled from `sourceHash` (their
    /// `derived_from`), newest first, as a JSON list string.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn provenance(&self, source_hash: String) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let h = source_hash.strip_prefix("sha256:").unwrap_or(&source_hash);
            let parent = parse_hash(h)?;
            let kids = facade
                .with_store(|m| m.grains_derived_from(&parent))
                .map_err(err)?;
            let out: Vec<serde_json::Value> = kids
                .iter()
                .map(|g| {
                    json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "subject": g.get_str("subject"),
                        "relation": g.get_str("relation"),
                        "object": g.get_str("object"),
                    })
                })
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Advise-mode novelty check: nearest existing grains to `text`, optionally
    /// scoped to (subject, relation), as a JSON list of {hash, similarity},
    /// most similar first. Requires an installed embedder; never writes.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn nearest(
        &self,
        text: String,
        subject: Option<String>,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(5) as usize;
        StringJob::spawn(move || {
            let matches = facade
                .with_store(|m| {
                    m.nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k)
                })
                .map_err(err)?;
            let out: Vec<serde_json::Value> = matches
                .iter()
                .map(|(h, sim)| json!({"hash": h.to_hex(), "similarity": sim}))
                .collect();
            serde_json::to_string(&out).map_err(err)
        })
    }

    /// Store statistics as JSON.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn stats(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let s = facade.with_store(|m| m.stats()).map_err(err)?;
            Ok(json!({
                "grains": s.grains, "current": s.current, "triples": s.triples,
                "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
            })
            .to_string())
        })
    }

    /// Incremental backup to a bundle file. Returns last_op_seq cursor.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn bundle(
        &self,
        path: String,
        since: Option<i64>,
    ) -> napi::bindgen_prelude::AsyncTask<I64Job> {
        let facade = self.facade.clone();
        I64Job::spawn(move || {
            let st = facade
                .with_store(|m| m.bundle_since(since.unwrap_or(0), &path))
                .map_err(err)?;
            Ok(st.last_op_seq)
        })
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    #[napi(ts_return_type = "Promise<number>")]
    pub fn import_bundle(&self, path: String) -> napi::bindgen_prelude::AsyncTask<U32Job> {
        let facade = self.facade.clone();
        U32Job::spawn(move || {
            let st = facade.with_store(|m| m.import_bundle(&path)).map_err(err)?;
            Ok(st.applied as u32)
        })
    }

    /// Integrity + content-address verification. Throws on failure.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn verify(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let r = facade.with_store(|m| m.verify()).map_err(err)?;
            if r.integrity != "ok" || r.hash_mismatches > 0 || r.undecodable > 0 {
                return Err(err(DejaDbError::Storage(format!(
                    "verification failed: integrity={} mismatches={} undecodable={}",
                    r.integrity, r.hash_mismatches, r.undecodable
                ))));
            }
            Ok(json!({"integrity": r.integrity, "grains": r.grains}).to_string())
        })
    }

    /// Bounded k-hop walk over the entity graph.
    ///
    /// `relations` is comma-separated. `direction` is out|in|both — in/both use
    /// the reverse index, which only covers relations the file declares
    /// entity-valued, so they find nothing for relations outside that set.
    ///
    /// Argument validation happens inside the task so a bad `direction` or an
    /// empty relation list *rejects* the promise, matching every other method
    /// here — throwing synchronously would contradict the `Promise<string>`
    /// signature napi generates.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn related(
        &self,
        start: String,
        relations: String,
        direction: Option<String>,
        depth: Option<u32>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let depth = depth.unwrap_or(2) as usize;
        let limit = limit.unwrap_or(64) as usize;
        StringJob::spawn(move || {
            let rels = parse_relations(&relations);
            if rels.is_empty() {
                return Err(napi::Error::from_reason(
                    "relations must name at least one relation",
                ));
            }
            let dir = Direction::parse(direction.as_deref().unwrap_or("out")).ok_or_else(|| {
                napi::Error::from_reason("direction must be one of: out, in, both")
            })?;
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            let reached = facade
                .with_store(|m| m.related(&ns, &start, &refs, dir, depth, limit))
                .map_err(err)?;
            Ok(json!({"start": start, "reached": reached}).to_string())
        })
    }

    /// As-of read on two axes: `world` = what was true at `at`,
    /// `knowledge` = what the agent knew at `at`. `at` is epoch milliseconds.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn entity_at(
        &self,
        subject: String,
        relation: String,
        at: i64,
        axis: Option<String>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        StringJob::spawn(move || {
            let ax = Axis::parse(axis.as_deref().unwrap_or("world"))
                .ok_or_else(|| napi::Error::from_reason("axis must be one of: world, knowledge"))?;
            let found = facade
                .with_store(|m| m.entity_at(&ns, &subject, &relation, at, ax))
                .map_err(err)?;
            Ok(match found {
                Some(g) => json!({"found": true, "grain": g}).to_string(),
                None => json!({"found": false}).to_string(),
            })
        })
    }

    /// What a run recorded, and what it produced downstream.
    ///
    /// Returns `{run_id, trace, produced}` — `trace` is the run's own grains,
    /// `produced` is what was derived from them and is not itself part of the
    /// run. This is the query that crosses from execution history into
    /// semantic memory.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn run_trace(
        &self,
        run_id: String,
        limit: Option<u32>,
        include_yield: Option<bool>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let limit = limit.unwrap_or(64) as usize;
        let want_yield = include_yield.unwrap_or(true);
        StringJob::spawn(move || {
            let (trace, produced) = facade
                .with_store(|m| {
                    let t = m.run_trace(&ns, &run_id, limit)?;
                    let p = if want_yield {
                        m.run_yield(&ns, &run_id, limit)?
                    } else {
                        Vec::new()
                    };
                    Ok::<_, dejadb_core::error::DejaDbError>((t, p))
                })
                .map_err(err)?;
            Ok(json!({"run_id": run_id, "trace": trace, "produced": produced}).to_string())
        })
    }

    /// Which runs produced or refined this grain — the reverse join.
    ///
    /// Runs that merely *read* the grain are not recorded: a read leaves no
    /// grain behind, so nothing in an append-only store can attest to it.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn runs_touching(
        &self,
        hash: String,
        depth: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let depth = depth.unwrap_or(4) as usize;
        StringJob::spawn(move || {
            let h = parse_hash(&hash)?;
            let runs = facade
                .with_store(|m| m.runs_touching(&ns, &h, depth))
                .map_err(err)?;
            Ok(json!({"hash": h.to_hex(), "runs": runs}).to_string())
        })
    }

    /// Execution records for a workflow: which grains ran which of its nodes.
    ///
    /// A Workflow grain is immutable, so runs point at the plan rather than
    /// mutating it — retries show up as several records for one node.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn step_actions(
        &self,
        workflow: String,
        node: Option<String>,
        limit: Option<u32>,
        ns: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let limit = limit.unwrap_or(64) as usize;
        StringJob::spawn(move || {
            let wf = parse_hash(&workflow)?;
            let rows = facade
                .with_store(|m| m.step_actions(&ns, &wf, node.as_deref(), limit))
                .map_err(err)?;
            let steps: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(n, h)| json!({"node": n, "hash": h.to_hex()}))
                .collect();
            Ok(json!({"workflow": wf.to_hex(), "steps": steps}).to_string())
        })
    }

    // ── Waiser: the governed self-improvement loop (§6.6) ────────────────────

    /// Record a tool call as a Tool grain — the flagship analyzer's food.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn record_tool_call(
        &self,
        name: String,
        result: String,
        is_error: Option<bool>,
        thread: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let mut fields = serde_json::Map::new();
        fields.insert("tool_name".into(), json!(name));
        fields.insert("content".into(), json!(result));
        fields.insert("is_error".into(), json!(is_error.unwrap_or(false)));
        fields.insert("namespace".into(), json!(self.ns));
        if let Some(t) = thread {
            fields.insert("session_id".into(), json!(t));
        }
        StringJob::spawn(move || Ok(facade.cal_add("tool", &fields).map_err(err)?.to_hex()))
    }

    /// Run one analysis pass. Bare it never gates. `fullSweep` re-analyzes
    /// the whole memory (`deja waiser reflect` semantics); `policy` is a path
    /// to a host `waiser-policy.json` — the only way auto-apply is granted
    /// from the bindings. Returns run-outcome JSON.
    #[napi(ts_return_type = "Promise<string>")]
    #[allow(clippy::too_many_arguments)] // a flat FFI surface; each knob is a distinct scalar
    pub fn waiser_run(
        &self,
        min_new: Option<u32>,
        min_new_errors: Option<u32>,
        if_stale: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
        analyzer_cmd: Option<String>,
        full_sweep: Option<bool>,
        policy: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let opts = RunOptions {
            min_new: min_new.map(|n| n as u64),
            min_new_errors: min_new_errors.map(|n| n as u64),
            if_stale_ms: if_stale.as_deref().and_then(parse_duration_ms),
            namespaces: Vec::new(),
            full_sweep: full_sweep.unwrap_or(false),
        };
        // The longest call on this surface — a sweep plus, optionally, several
        // LLM round trips. Blocking the event loop across that was the worst
        // case of the old synchronous surface.
        StringJob::spawn(move || {
            // Optional verified LLM reflection: `model` ("claude-sonnet", key from
            // the env) attaches a built-in HTTP backend; `llmCmd` a subprocess.
            let mut engine = Engine::with_builtins();
            // Host policy file (§6.2) — mirrors the CLI's --policy. Host config,
            // read per call, never persisted in the memory file.
            if let Some(path) = policy {
                let s = std::fs::read_to_string(&path)
                    .map_err(|e| err(format!("policy {path}: {e}")))?;
                engine = engine.with_policy(waiser::Policy::from_json(&s).map_err(err)?);
            }
            if let Some(cmd) = llm_cmd {
                let llm = waiser::CommandLlm::new(&cmd, None).map_err(err)?;
                engine = engine.with_llm(Box::new(llm));
            } else if let Some(spec) = model {
                engine = engine.with_llm(dejadb_llm::resolve(&spec, None, None).map_err(err)?);
            }
            // Optional separate grounding backend (defaults to the reflection model).
            if let Some(cmd) = ground_cmd {
                let g = waiser::CommandLlm::new(&cmd, None).map_err(err)?;
                engine = engine.with_ground_llm(Box::new(g));
            } else if let Some(spec) = ground_model {
                engine =
                    engine.with_ground_llm(dejadb_llm::resolve(&spec, None, None).map_err(err)?);
            }
            // Optional external analyzer (advisory only — never auto-applies).
            if let Some(cmd) = analyzer_cmd {
                engine.register(Box::new(waiser::CommandAnalyzer::new(&cmd).map_err(err)?));
            }
            let mut sub = BorrowedSubstrate::new(&facade);
            let res = engine.run(&mut sub, &opts, now_ms()).map_err(err)?;
            serde_json::to_string(&res).map_err(err)
        })
    }

    /// List recommendations. `filter` is optional JSON, e.g. `{"status":
    /// "pending"}`; `{"status":"all"}` clears the filter. JSON list.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn recommendations(
        &self,
        filter: Option<String>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let status = filter
            .and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
            .filter(|s| s != "all")
            .and_then(|s| status_from_str(&s))
            .or(Some(RecStatus::Pending));
        StringJob::spawn(move || {
        let sub = BorrowedSubstrate::new(&facade);
        let recs = Engine::with_builtins().recommendations(&sub, status).map_err(err)?;
        let rows: Vec<_> = recs
            .iter()
            .map(|r| {
                json!({
                    "hash": r.hash,
                    "status": r.status.as_str(),
                    "severity": r.severity.as_str(),
                    "analyzer": r.analyzer,
                    "summary": r.summary.render(),
                    "target_ref": r.target_ref,
                    "destructive": r.destructive,
                })
            })
            .collect();
        serde_json::to_string(&rows).map_err(err)
        })
    }

    /// Approve and apply a recommendation in one audited step (§6.6). The
    /// `because` reason is mandatory.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn apply_recommendation(
        &self,
        hash: String,
        because: String,
        allow_destructive: Option<bool>,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let mut sub = BorrowedSubstrate::new(&facade);
            let engine = Engine::with_builtins();
            let now = now_ms();
            let scopes = ScopeSet::all();
            engine
                .review(&mut sub, &hash, Decision::Approve, &actor, ObserverType::Human, &scopes, &because, now)
                .map_err(err)?;
            let applied = engine
                .apply(&mut sub, &hash, &actor, ObserverType::Human, &scopes, &because, allow_destructive.unwrap_or(false), now)
                .map_err(err)?;
            Ok(json!({"hash": hash, "rollbackable": applied.rollbackable}).to_string())
        })
    }

    /// Reject a recommendation with a reason (library-friendly `reject`).
    #[napi(ts_return_type = "Promise<string>")]
    pub fn dismiss_recommendation(
        &self,
        hash: String,
        why: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let mut sub = BorrowedSubstrate::new(&facade);
            Engine::with_builtins()
                .review(&mut sub, &hash, Decision::Reject, &actor, ObserverType::Human, &ScopeSet::all(), &why, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash, "status": "rejected"}).to_string())
        })
    }

    /// Roll back an applied recommendation (retracts the grains it created).
    /// Mandatory reason; fails for non-rollbackable applies (FORGET has no
    /// inverse). Parity with `deja waiser rollback`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn rollback_recommendation(
        &self,
        hash: String,
        because: String,
    ) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        let actor = self.actor.clone();
        StringJob::spawn(move || {
            let mut sub = BorrowedSubstrate::new(&facade);
            Engine::with_builtins()
                .rollback(&mut sub, &hash, &actor, ObserverType::Human, &ScopeSet::all(), &because, now_ms())
                .map_err(err)?;
            Ok(json!({"hash": hash, "status": "rolled_back"}).to_string())
        })
    }

    /// Measured outcomes of applied recommendations — the Verify gate's
    /// record (`held` / `regressed` per checkpoint). JSON list, parity with
    /// `deja waiser outcomes`.
    #[napi(ts_return_type = "Promise<string>")]
    pub fn waiser_outcomes(&self) -> napi::bindgen_prelude::AsyncTask<StringJob> {
        let facade = self.facade.clone();
        StringJob::spawn(move || {
            let sub = BorrowedSubstrate::new(&facade);
            let outs = Engine::with_builtins().outcomes(&sub).map_err(err)?;
            serde_json::to_string(&outs).map_err(err)
        })
    }
}
