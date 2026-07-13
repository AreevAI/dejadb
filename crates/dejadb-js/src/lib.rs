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
use dejadb_store::{CommandEmbed, DejaDB as RustDejaDB, FactDraft};
use napi_derive::napi;
use serde_json::json;

fn err<E: std::fmt::Display>(e: E) -> napi::Error {
    napi::Error::from_reason(e.to_string())
}

fn parse_hash(hex: &str) -> napi::Result<Hash> {
    Hash::from_hex(hex).map_err(err)
}

/// One memory = one file. Open with `new DejaDb("caller.db", "caller")`.
#[napi]
pub struct DejaDb {
    facade: DejaDbFacade,
    ns: String,
}

#[napi]
impl DejaDb {
    #[napi(constructor)]
    pub fn new(path: String, ns: Option<String>, passphrase: Option<String>) -> napi::Result<Self> {
        let ns = ns.unwrap_or_else(|| "shared".to_string());
        // Encryption at rest: a passphrase derives an AES-256 key (Argon2id;
        // non-secret salt in a <path>.kdf sidecar). Host-supplied, never
        // stored in the file — same rules as the CLI's --passphrase-env.
        let store = match passphrase {
            Some(p) => RustDejaDB::open_with_passphrase(&path, &p).map_err(err)?,
            None => RustDejaDB::open(&path).map_err(err)?,
        };
        let facade = DejaDbFacade::with_session(store, Some(ns.clone()), None);
        Ok(DejaDb { facade, ns })
    }

    /// Reconciliation warnings from open (file-vs-host declaration changes,
    /// embedding-model mismatches). JSON list string.
    #[napi]
    pub fn open_warnings(&self) -> napi::Result<String> {
        let w = self.facade.with_store(|m| m.open_warnings().to_vec());
        serde_json::to_string(&w).map_err(err)
    }

    /// Install a command embedder (same contract as the CLI's --embed-cmd):
    /// the command gets the text on stdin and must print a JSON array of
    /// numbers. Probed once here to learn the dimension. Enables the vector
    /// recall leg; grains added afterwards are embedded. (An in-process JS
    /// callback embedder needs an async surface — planned; the command
    /// embedder is the stable path today.)
    #[napi]
    pub fn set_embedder_command(&self, cmd: String, model: Option<String>) -> napi::Result<()> {
        let ce = CommandEmbed::new(&cmd, model.as_deref()).map_err(err)?;
        self.facade.with_store(|m| m.set_embedder(Box::new(ce)));
        Ok(())
    }

    /// Backfill + rebuild the BM25 text index (e.g. after bulk loads, or on
    /// a file that flipped text indexing on later). Returns rows backfilled.
    #[napi]
    pub fn reindex_text(&self) -> napi::Result<u32> {
        self.facade
            .with_store(|m| m.rebuild_text_index())
            .map(|n| n as u32)
            .map_err(err)
    }

    /// Anthropic memory-tool command (view/create/str_replace/insert/delete/
    /// rename over /memories): pass the tool-call object as JSON; returns the
    /// tool result text. Wire this as your memory-tool backend.
    #[napi]
    pub fn memory_tool(&self, command_json: String, ns: Option<String>) -> napi::Result<String> {
        let cmd: serde_json::Value = serde_json::from_str(&command_json).map_err(err)?;
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        self.facade
            .with_store(|m| {
                let mut t = MemoryTool::new(m, &ns);
                t.execute(&cmd)
            })
            .map_err(err)
    }

    /// Import another memory system's export. `source`: mem0 | mem0-history |
    /// langgraph | letta | letta-archival | zep | jsonl. `payload` is the
    /// export file's contents; `history` the optional mem0 history payload.
    /// (basic-memory vault directories import via the CLI: `deja migrate`.)
    /// Returns {added, superseded, forgotten, skipped, notes} as JSON.
    /// Re-runs skip what is already imported.
    #[napi]
    pub fn migrate(
        &self,
        source: String,
        payload: String,
        history: Option<String>,
        ns: Option<String>,
    ) -> napi::Result<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let rep = self
            .facade
            .with_store(|m| {
                dejadb_store::migrate::migrate_payload(m, &ns, &source, &payload, history.as_deref())
            })
            .map_err(err)?;
        Ok(rep.to_json().to_string())
    }

    /// Add a Fact. Returns the content address (64-hex).
    /// Add a Fact. With `idempotent = true`, a re-add of the value already at
    /// the `(subject, relation)` head writes nothing and returns the existing
    /// hash (value-level dedup, not just byte-identical replay).
    #[napi]
    pub fn add_fact(
        &self,
        subject: String,
        relation: String,
        object: String,
        confidence: Option<f64>,
        ns: Option<String>,
        idempotent: Option<bool>,
    ) -> napi::Result<String> {
        let mut fields = serde_json::Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("confidence".into(), json!(confidence.unwrap_or(0.9)));
        fields.insert(
            "namespace".into(),
            json!(ns.unwrap_or_else(|| self.ns.clone())),
        );
        if idempotent.unwrap_or(false) {
            Ok(self
                .facade
                .cal_add_if_novel("fact", &fields)
                .map_err(err)?
                .0
                .to_hex())
        } else {
            Ok(self.facade.cal_add("fact", &fields).map_err(err)?.to_hex())
        }
    }

    /// Add any grain type from a JSON fields object. Returns the hash.
    #[napi]
    pub fn add(
        &self,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::Result<String> {
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
    #[napi]
    pub fn recall(
        &self,
        subject: String,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::Result<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(16) as usize;
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

    /// Current head for (subject, relation) — JSON string or null.
    #[napi]
    pub fn latest(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::Result<Option<String>> {
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
    #[napi]
    pub fn supersede(
        &self,
        old_hash: String,
        grain_type: String,
        fields_json: String,
        ns: Option<String>,
    ) -> napi::Result<String> {
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
    #[napi]
    pub fn forget(&self, hash: String) -> napi::Result<()> {
        let h = parse_hash(&hash)?;
        self.facade.with_store(|m| m.forget(&h)).map_err(err)
    }

    /// remember(): store content as an Observation; optional pre-extracted
    /// facts (JSON list of {subject, relation, object, confidence}) become
    /// provenance-linked Facts. Returns {"observation", "facts"} JSON.
    #[napi]
    pub fn remember(
        &self,
        content: String,
        facts_json: Option<String>,
        observer: Option<String>,
        ns: Option<String>,
    ) -> napi::Result<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let observer = observer.unwrap_or_else(|| "node".to_string());
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
    #[napi]
    pub fn cal(&self, query: String) -> napi::Result<String> {
        let ex = CalExecutor::new(CalExecutorConfig::default());
        let res = ex.execute(&query, &self.facade).map_err(err)?;
        serde_json::to_string(&res.result).map_err(err)
    }

    /// Supersession-chain history for (subject, relation), newest first.
    #[napi]
    pub fn history(
        &self,
        subject: String,
        relation: String,
        ns: Option<String>,
    ) -> napi::Result<String> {
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

    /// Reverse provenance: grains distilled from `sourceHash` (their
    /// `derived_from`), newest first, as a JSON list string.
    #[napi]
    pub fn provenance(&self, source_hash: String) -> napi::Result<String> {
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
    /// scoped to (subject, relation), as a JSON list of {hash, similarity},
    /// most similar first. Requires an installed embedder; never writes.
    #[napi]
    pub fn nearest(
        &self,
        text: String,
        subject: Option<String>,
        relation: Option<String>,
        k: Option<u32>,
        ns: Option<String>,
    ) -> napi::Result<String> {
        let ns = ns.unwrap_or_else(|| self.ns.clone());
        let k = k.unwrap_or(5) as usize;
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

    /// Store statistics as JSON.
    #[napi]
    pub fn stats(&self) -> napi::Result<String> {
        let s = self.facade.with_store(|m| m.stats()).map_err(err)?;
        Ok(json!({
            "grains": s.grains, "current": s.current, "triples": s.triples,
            "terms": s.terms, "ops": s.ops, "events_indexed": s.events_indexed,
        })
        .to_string())
    }

    /// Incremental backup to a bundle file. Returns last_op_seq cursor.
    #[napi]
    pub fn bundle(&self, path: String, since: Option<i64>) -> napi::Result<i64> {
        let st = self
            .facade
            .with_store(|m| m.bundle_since(since.unwrap_or(0), &path))
            .map_err(err)?;
        Ok(st.last_op_seq)
    }

    /// Apply a bundle (fast-forward, idempotent). Returns ops applied.
    #[napi]
    pub fn import_bundle(&self, path: String) -> napi::Result<u32> {
        let st = self
            .facade
            .with_store(|m| m.import_bundle(&path))
            .map_err(err)?;
        Ok(st.applied as u32)
    }

    /// Integrity + content-address verification. Throws on failure.
    #[napi]
    pub fn verify(&self) -> napi::Result<String> {
        let r = self.facade.with_store(|m| m.verify()).map_err(err)?;
        if r.integrity != "ok" || r.hash_mismatches > 0 || r.undecodable > 0 {
            return Err(err(DejaDbError::Storage(format!(
                "verification failed: integrity={} mismatches={} undecodable={}",
                r.integrity, r.hash_mismatches, r.undecodable
            ))));
        }
        Ok(json!({"integrity": r.integrity, "grains": r.grains}).to_string())
    }
}
