//! dejadb — Python bindings for DejaDB (LR-3, alpha).
//!
//! Thin and version-stable by design: scalar args in, JSON strings out
//! for anything structured (the Python wrapper layer can pretty this up;
//! the FFI stays honest). Install `dejadb`, `import dejadb`.

use dejadb_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, DejaDbFacade};
use dejadb_core::error::{Hash, DejaDbError};
use dejadb_store::memory_tool::MemoryTool;
use dejadb_store::{CommandEmbed, EmbedBackend, FactDraft, TelemetryMode, DejaDB as RustDejaDB};
use dejadb_waiser::{now_ms, BorrowedSubstrate};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde_json::json;
use waiser::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

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

fn err<E: std::fmt::Display>(e: E) -> PyErr {
    PyValueError::new_err(e.to_string())
}

fn parse_hash(hex: &str) -> PyResult<Hash> {
    Hash::from_hex(hex).map_err(err)
}

/// [`EmbedBackend`] over a Python callable `embed(text: str) -> list[float]`.
/// The binding methods run on the interpreter thread already attached to
/// Python; re-attaching from inside a store call on the same thread is safe
/// (`Python::attach` is reentrant).
struct PyEmbed {
    f: Py<PyAny>,
    dim: usize,
    model: String,
}

impl EmbedBackend for PyEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> dejadb_core::error::Result<Vec<f32>> {
        Python::attach(|py| {
            let out = self
                .f
                .call1(py, (text,))
                .map_err(|e| DejaDbError::Storage(format!("python embedder raised: {e}")))?;
            out.extract::<Vec<f32>>(py).map_err(|e| {
                DejaDbError::Validation(format!(
                    "python embedder must return a sequence of floats: {e}"
                ))
            })
        })
    }
    fn model(&self) -> &str {
        &self.model
    }
}

/// One memory = one file. Open with `dejadb.DejaDB("caller.db", ns="caller")`.
#[pyclass]
struct DejaDB {
    facade: DejaDbFacade,
    ns: String,
    /// Host-asserted actor label stamped on every waiser audit grain (§6.6).
    actor: String,
}

#[pymethods]
impl DejaDB {
    #[new]
    #[pyo3(signature = (path, ns = "shared".to_string(), passphrase = None, actor = "user:local".to_string(), telemetry = "aggregate".to_string()))]
    fn new(
        path: String,
        ns: String,
        passphrase: Option<String>,
        actor: String,
        telemetry: String,
    ) -> PyResult<Self> {
        // Recall-telemetry sidecar (host capability, §8): agents are the main
        // telemetry producers, so the binding default is `aggregate`; pass
        // telemetry="off" to disable. It is never a file-truth.
        let tel = TelemetryMode::parse(&telemetry)
            .ok_or_else(|| err(format!("unknown telemetry mode '{telemetry}' (off|aggregate|full)")))?;
        // Encryption at rest: a passphrase derives an AES-256 key (Argon2id;
        // non-secret salt in a <path>.kdf sidecar). Same key rules as the
        // CLI's --passphrase-env: host-supplied, never stored in the file.
        let store = match passphrase {
            Some(p) => RustDejaDB::open_with_passphrase_telemetry(&path, &p, tel).map_err(err)?,
            None => RustDejaDB::open_with_telemetry(&path, tel).map_err(err)?,
        };
        let facade = DejaDbFacade::with_session(store, Some(ns.clone()), None);
        Ok(DejaDB { facade, ns, actor })
    }

    /// Reconciliation warnings from open (file-vs-host declaration changes,
    /// embedding-model mismatches). JSON list string.
    fn open_warnings(&self) -> PyResult<String> {
        let w = self.facade.with_store(|m| m.open_warnings().to_vec());
        serde_json::to_string(&w).map_err(err)
    }

    /// Install an embedding callback: `embed(text: str) -> list[float]`.
    /// Probed once here to learn the dimension (recorded as the file's
    /// embedding provenance). Enables the vector recall leg; grains added
    /// afterwards are embedded — run `reindex_text()`-style backfills via
    /// `migrate`/re-adds, embeddings are not retro-computed.
    #[pyo3(signature = (embed, model = None))]
    fn set_embedder(&self, embed: Py<PyAny>, model: Option<String>) -> PyResult<()> {
        let dim = Python::attach(|py| {
            embed
                .call1(py, ("dimension probe",))?
                .extract::<Vec<f32>>(py)
        })
        .map_err(err)?
        .len();
        if dim == 0 {
            return Err(err("embedder returned an empty vector"));
        }
        let backend = PyEmbed {
            f: embed,
            dim,
            model: model.unwrap_or_else(|| "python".to_string()),
        };
        self.facade.with_store(|m| m.set_embedder(Box::new(backend)));
        Ok(())
    }

    /// Install a command embedder (same contract as the CLI's --embed-cmd):
    /// the command gets the text on stdin and prints a JSON array of numbers.
    #[pyo3(signature = (cmd, model = None))]
    fn set_embedder_command(&self, cmd: String, model: Option<String>) -> PyResult<()> {
        let ce = CommandEmbed::new(&cmd, model.as_deref()).map_err(err)?;
        self.facade.with_store(|m| m.set_embedder(Box::new(ce)));
        Ok(())
    }

    /// Backfill + rebuild the BM25 text index (e.g. after bulk loads, or on
    /// a file that flipped text indexing on later). Returns rows backfilled.
    fn reindex_text(&self) -> PyResult<usize> {
        self.facade.with_store(|m| m.rebuild_text_index()).map_err(err)
    }

    /// Import another memory system's export. `source`: mem0 | mem0-history |
    /// langgraph | letta | letta-archival | zep | jsonl. `payload` is the
    /// export file's contents; `history` the optional mem0 history payload.
    /// (basic-memory vault directories import via the CLI: `deja migrate`.)
    /// Returns the report as JSON: {added, superseded, forgotten, skipped,
    /// notes}. Re-runs skip what is already imported.
    #[pyo3(signature = (source, payload, history = None, ns = None))]
    fn migrate(
        &self,
        source: String,
        payload: String,
        history: Option<String>,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let rep = self
            .facade
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
    }

    /// Add a Fact. Returns the content address (64-hex).
    ///
    /// With `idempotent=True`, if the current head for `(subject, relation)`
    /// already holds this exact object, no grain is written and the existing
    /// head's hash is returned (value-level dedup, not just byte-identical).
    #[pyo3(signature = (subject, relation, object, confidence = 0.9, ns = None, idempotent = false))]
    fn add_fact(
        &self,
        subject: String,
        relation: String,
        object: String,
        confidence: f64,
        ns: Option<String>,
        idempotent: bool,
    ) -> PyResult<String> {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("confidence".into(), json!(confidence));
        fields.insert("namespace".into(), json!(ns.unwrap_or_else(|| self.ns.clone())));
        if idempotent {
            Ok(self.facade.cal_add_if_novel("fact", &fields).map_err(err)?.0.to_hex())
        } else {
            Ok(self.facade.cal_add("fact", &fields).map_err(err)?.to_hex())
        }
    }

    /// Add any grain type from a JSON fields object. Returns the hash.
    #[pyo3(signature = (grain_type, fields_json, ns = None))]
    fn add(&self, grain_type: String, fields_json: String, ns: Option<String>) -> PyResult<String> {
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&fields_json).map_err(err)?;
        fields
            .entry("namespace".to_string())
            .or_insert_with(|| json!(ns.unwrap_or_else(|| self.ns.clone())));
        Ok(self
            .facade
            .cal_add(&grain_type, &fields)
            .map_err(err)?
            .to_hex())
    }

    /// Structural recall, newest-first. Returns a JSON list string.
    #[pyo3(signature = (subject, relation = None, k = 16, ns = None))]
    fn recall(
        &self,
        subject: String,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let grains = self
            .facade
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
    }

    /// Current head for (subject, relation) — JSON string or None.
    #[pyo3(signature = (subject, relation, ns = None))]
    fn latest(&self, subject: String, relation: String, ns: Option<String>) -> PyResult<Option<String>> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let head = self
            .facade
            .with_store(|m| m.latest(&ns, &subject, &relation))
            .map_err(err)?;
        Ok(head.map(|g| {
            json!({
                "hash": g.hash.to_hex(),
                "fields": g.fields,
            })
            .to_string()
        }))
    }

    /// Supersede old_hash with a new version (append-only evolution).
    #[pyo3(signature = (old_hash, grain_type, fields_json, ns = None))]
    fn supersede(
        &self,
        old_hash: String,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let old = parse_hash(&old_hash)?;
        let mut fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&fields_json).map_err(err)?;
        fields
            .entry("namespace".to_string())
            .or_insert_with(|| json!(ns.unwrap_or_else(|| self.ns.clone())));
        Ok(self
            .facade
            .cal_supersede(&old, &grain_type, &fields)
            .map_err(err)?
            .to_hex())
    }

    /// Erase a grain from the hot store (tombstoned). Host-level op.
    fn forget(&self, hash: String) -> PyResult<()> {
        let h = parse_hash(&hash)?;
        self.facade.with_store(|m| m.forget(&h)).map_err(err)
    }

    /// remember(): store content as an Observation; optional pre-extracted
    /// facts (JSON list of {subject, relation, object, confidence}) become
    /// provenance-linked Facts. Returns {"observation", "facts"} JSON.
    #[pyo3(signature = (content, facts_json = None, observer = "python".to_string(), ns = None))]
    fn remember(
        &self,
        content: String,
        facts_json: Option<String>,
        observer: String,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let drafts: Vec<FactDraft> = match facts_json {
            Some(j) => {
                let arr: Vec<serde_json::Value> = serde_json::from_str(&j).map_err(err)?;
                arr.iter()
                    .map(|v| FactDraft {
                        subject: v["subject"].as_str().unwrap_or("").to_string(),
                        relation: v["relation"].as_str().unwrap_or("").to_string(),
                        object: v["object"].as_str().unwrap_or("").to_string(),
                        confidence: v["confidence"].as_f64().unwrap_or(0.8),
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        let extractor = move |_c: &str| drafts.clone();
        let res = self
            .facade
            .with_store(|m| m.remember(&ns, &content, &observer, Some(&extractor)))
            .map_err(err)?;
        Ok(json!({
            "observation": res.observation.to_hex(),
            "facts": res.facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        })
        .to_string())
    }

    /// Execute CAL. Returns the wire-format payload as a JSON string.
    fn cal(&self, query: String) -> PyResult<String> {
        let ex = CalExecutor::new(CalExecutorConfig::default());
        let res = ex.execute(&query, &self.facade).map_err(err)?;
        serde_json::to_string(&res.result).map_err(err)
    }

    /// Anthropic memory-tool command (LR-13): pass the tool-call dict as
    /// JSON; returns the tool result text. Wire this as your MemoryToolHandler.
    #[pyo3(signature = (command_json, ns = None))]
    fn memory_tool(&self, command_json: String, ns: Option<String>) -> PyResult<String> {
        let cmd: serde_json::Value = serde_json::from_str(&command_json).map_err(err)?;
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        self.facade
            .with_store(|m| {
                let mut t = MemoryTool::new(m, &ns);
                t.execute(&cmd)
            })
            .map_err(err)
    }

    /// Reverse provenance: grains distilled from `source_hash` (its
    /// `derived_from`), newest first, as a JSON list string. The
    /// credit-assignment / episode-unlearn query.
    fn provenance(&self, source_hash: String) -> PyResult<String> {
        let h = source_hash.strip_prefix("sha256:").unwrap_or(&source_hash);
        let parent = parse_hash(h)?;
        let kids = self
            .facade
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
    }

    /// Advise-mode novelty check: nearest existing grains to `text`, optionally
    /// scoped to (subject, relation), as a JSON list of {hash, similarity,
    /// object}, most similar first. Requires an installed embedder. Never
    /// writes — the caller decides supersede-vs-add.
    #[pyo3(signature = (text, subject = None, relation = None, k = 5, ns = None))]
    fn nearest(
        &self,
        text: String,
        subject: Option<String>,
        relation: Option<String>,
        k: usize,
        ns: Option<String>,
    ) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let matches = self
            .facade
            .with_store(|m| m.nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k))
            .map_err(err)?;
        let out: Vec<serde_json::Value> = matches
            .iter()
            .map(|(h, sim)| json!({"hash": h.to_hex(), "similarity": sim}))
            .collect();
        serde_json::to_string(&out).map_err(err)
    }

    /// Supersession-chain history for (subject, relation), newest first.
    #[pyo3(signature = (subject, relation, ns = None))]
    fn history(&self, subject: String, relation: String, ns: Option<String>) -> PyResult<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let versions = self
            .facade
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
    }

    /// Store statistics as JSON.
    fn stats(&self) -> PyResult<String> {
        let s = self.facade.with_store(|m| m.stats()).map_err(err)?;
        Ok(json!({
            "grains": s.grains, "current": s.current, "triples": s.triples,
            "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
        })
        .to_string())
    }

    /// Incremental backup to a bundle file. Returns last_op_seq cursor.
    #[pyo3(signature = (path, since = 0))]
    fn bundle(&self, path: String, since: i64) -> PyResult<i64> {
        let st = self
            .facade
            .with_store(|m| m.bundle_since(since, &path))
            .map_err(err)?;
        Ok(st.last_op_seq)
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    fn import_bundle(&self, path: String) -> PyResult<usize> {
        let st = self
            .facade
            .with_store(|m| m.import_bundle(&path))
            .map_err(err)?;
        Ok(st.applied)
    }

    /// Integrity + content-address verification. Raises on failure.
    fn verify(&self) -> PyResult<String> {
        let r = self.facade.with_store(|m| m.verify()).map_err(err)?;
        if r.integrity != "ok" || r.hash_mismatches > 0 || r.undecodable > 0 {
            return Err(err(DejaDbError::Storage(format!(
                "verification failed: integrity={} mismatches={} undecodable={}",
                r.integrity, r.hash_mismatches, r.undecodable
            ))));
        }
        Ok(json!({"integrity": r.integrity, "grains": r.grains}).to_string())
    }

    // ── Waiser: the governed self-improvement loop (§6.6) ────────────────────

    /// Record a tool call as a Tool grain — the flagship analyzer's food. One
    /// line per call in the agent's tool loop. `thread` groups a session.
    #[pyo3(signature = (name, result, is_error = false, thread = None))]
    fn record_tool_call(
        &self,
        name: String,
        result: String,
        is_error: bool,
        thread: Option<String>,
    ) -> PyResult<String> {
        let mut fields = serde_json::Map::new();
        fields.insert("tool_name".into(), json!(name));
        fields.insert("content".into(), json!(result));
        fields.insert("is_error".into(), json!(is_error));
        fields.insert("namespace".into(), json!(self.ns));
        if let Some(t) = thread {
            fields.insert("session_id".into(), json!(t));
        }
        let h = self.facade.cal_add("tool", &fields).map_err(err)?;
        Ok(h.to_hex())
    }

    /// Run one analysis pass. Bare (all args `None`) it never gates — an
    /// evaluator's first call always runs. Returns the run-outcome JSON.
    #[pyo3(signature = (min_new = None, min_new_errors = None, if_stale = None, model = None, llm_cmd = None, ground_model = None, ground_cmd = None))]
    fn waiser_run(
        &self,
        min_new: Option<u64>,
        min_new_errors: Option<u64>,
        if_stale: Option<String>,
        model: Option<String>,
        llm_cmd: Option<String>,
        ground_model: Option<String>,
        ground_cmd: Option<String>,
    ) -> PyResult<String> {
        let opts = RunOptions {
            min_new,
            min_new_errors,
            if_stale_ms: if_stale.as_deref().and_then(parse_duration_ms),
            namespaces: Vec::new(),
        };
        // Optional verified LLM reflection: `model="claude-sonnet"` (key from the
        // env) attaches a built-in HTTP backend; `llm_cmd="..."` a subprocess.
        let mut engine = Engine::with_builtins();
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
            engine = engine.with_ground_llm(dejadb_llm::resolve(&spec, None, None).map_err(err)?);
        }
        let mut sub = BorrowedSubstrate::new(&self.facade);
        let res = engine.run(&mut sub, &opts, now_ms()).map_err(err)?;
        serde_json::to_string(&res).map_err(err)
    }

    /// List recommendations. `filter` is optional JSON, e.g. `{"status":
    /// "pending"}`; omit or `{"status":"all"}` for every status. JSON list.
    #[pyo3(signature = (filter = None))]
    fn recommendations(&self, filter: Option<String>) -> PyResult<String> {
        let status = filter
            .and_then(|f| serde_json::from_str::<serde_json::Value>(&f).ok())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string))
            .filter(|s| s != "all")
            .and_then(|s| status_from_str(&s))
            .or(Some(RecStatus::Pending)); // default: pending
        // `{"status":"all"}` clears the filter; anything else defaults to pending.
        let sub = BorrowedSubstrate::new(&self.facade);
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
    }

    /// Approve and apply a recommendation in one audited step (§6.6). The
    /// `because` reason is mandatory. Non-rollbackable (destructive) payloads
    /// are rejected unless `allow_destructive=True`.
    #[pyo3(signature = (hash, because, allow_destructive = false))]
    fn apply_recommendation(&self, hash: String, because: String, allow_destructive: bool) -> PyResult<String> {
        let mut sub = BorrowedSubstrate::new(&self.facade);
        let engine = Engine::with_builtins();
        let now = now_ms();
        let scopes = ScopeSet::all();
        engine
            .review(&mut sub, &hash, Decision::Approve, &self.actor, ObserverType::Human, &scopes, &because, now)
            .map_err(err)?;
        let applied = engine
            .apply(&mut sub, &hash, &self.actor, ObserverType::Human, &scopes, &because, allow_destructive, now)
            .map_err(err)?;
        Ok(json!({"hash": hash, "rollbackable": applied.rollbackable}).to_string())
    }

    /// Reject a recommendation with a reason (the library-friendly name for
    /// `deja waiser reject`).
    fn dismiss_recommendation(&self, hash: String, why: String) -> PyResult<String> {
        let mut sub = BorrowedSubstrate::new(&self.facade);
        Engine::with_builtins()
            .review(&mut sub, &hash, Decision::Reject, &self.actor, ObserverType::Human, &ScopeSet::all(), &why, now_ms())
            .map_err(err)?;
        Ok(json!({"hash": hash, "status": "rejected"}).to_string())
    }
}

#[pymodule]
fn dejadb(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<DejaDB>()?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    Ok(())
}
