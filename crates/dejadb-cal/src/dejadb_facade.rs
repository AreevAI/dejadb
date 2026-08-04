//! DejaDbFacade — CalStoreFacade over the embedded dejadb-store.
//!
//! Session-scoped: carries the capability defaults (namespace, user) that
//! CAL queries inherit when they don't specify one (§7.11 direction).
//!
//! M2 scope: structural recall only. Semantic (`query`) recall returns a
//! clear error until the FTS/vector legs land (M4).

use std::sync::Mutex;

use dejadb_core::error::{Hash, DejaDbError, Result};
use dejadb_core::format::deserialize::DeserializedGrain;
use dejadb_core::types::Grain;
use dejadb_store::DejaDB;

use crate::ast::QueryParam;
use crate::errors::CalError;
use crate::facade::{CalStoreFacade, TemplateInfo};
use crate::json_build::{build_grain_from_json, GrainSink};
use crate::queries::{PersistedQuery, QueryEntry, QueryListEntry, QueryRegistry};
use crate::store_types::{DiversityMethod, ForkGroupInfo, RecallParams, SearchHit, VersionEntry};
use crate::templates::{PersistedTemplate, TemplateRegistry};

/// `meta` key prefixes for CAL host metadata. One row per entry, so recording
/// a last-run timestamp does not rewrite the whole set.
const QRY_PREFIX: &str = "qry:";
const TPL_PREFIX: &str = "tpl:";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// CalStoreFacade implementation over an embedded `DejaDB` store.
pub struct DejaDbFacade {
    store: Mutex<DejaDB>,
    namespace: Option<String>,
    user: Option<String>,
    /// Read-only mounted memories (org/category replicas): alias → store.
    /// A recall with namespace "alias.inner" routes to the mount (§8).
    mounts: std::collections::HashMap<String, Mutex<DejaDB>>,
    /// Saved queries and custom templates are *host metadata* carried by the
    /// file (`meta` rows), not memories — they travel with the .db so the
    /// CLI, MCP and console all see the same set. Rehydrated on first use.
    queries: Mutex<Option<QueryRegistry>>,
    templates: Mutex<Option<TemplateRegistry>>,
    /// Entries the file carries that this process could not load — a template
    /// that outgrew the §10.8 body limit, a set past the per-file cap, a row
    /// written by a newer version. Reported alongside `DejaDB::open_warnings`
    /// so a silently smaller set of saved queries is something the operator
    /// sees rather than discovers.
    meta_warnings: Mutex<Vec<String>>,
}

impl DejaDbFacade {
    pub fn new(store: DejaDB) -> Self {
        Self::with_session(store, None, None)
    }

    /// Session-scoped facade: `namespace`/`user` become the capability
    /// defaults consulted by the executor.
    pub fn with_session(store: DejaDB, namespace: Option<String>, user: Option<String>) -> Self {
        DejaDbFacade {
            store: Mutex::new(store),
            namespace,
            user,
            mounts: std::collections::HashMap::new(),
            queries: Mutex::new(None),
            templates: Mutex::new(None),
            meta_warnings: Mutex::new(Vec::new()),
        }
    }

    /// Mount a read-only memory (an org/category replica) under an alias.
    /// CAL reaches it with `WHERE namespace = "<alias>.<inner-ns>"` — which
    /// is what makes single-statement ASSEMBLE span user + org files.
    pub fn mount(&mut self, alias: &str, store: DejaDB) {
        self.mounts.insert(alias.to_string(), Mutex::new(store));
    }

    pub fn into_inner(self) -> DejaDB {
        self.store.into_inner().unwrap_or_else(|p| p.into_inner())
    }

    /// Run a closure against the underlying store — the escape hatch for
    /// implementation-level operations CAL structurally excludes (forget,
    /// bundle, stats). Host-surface only; never reachable from CAL text.
    /// The session's capability namespace, if scoped.
    pub fn session_namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    /// Recall over-fetch multiplier: `recall_hybrid` is asked for
    /// `limit × RECALL_OVERFETCH` candidates before post-filtering.
    pub const RECALL_OVERFETCH: usize = 4;

    /// Aliases of read-only mounted stores (ASSEMBLE cross-file sources).
    pub fn mount_aliases(&self) -> Vec<String> {
        let mut a: Vec<String> = self.mounts.keys().cloned().collect();
        a.sort();
        a
    }

    pub fn with_store<R>(&self, f: impl FnOnce(&mut DejaDB) -> R) -> R {
        let mut guard = self.store.lock().unwrap();
        f(&mut guard)
    }

    /// Value-level idempotent add (see [`DejaDB::add_if_novel`]). Returns the
    /// grain hash and whether a new grain was written (`false` = the value was
    /// already the current head). Bindings expose this as an `idempotent` flag.
    pub fn cal_add_if_novel(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(Hash, bool)> {
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddIfNovelSink { m: &mut m })
    }

    /// Add many grains in one store transaction. Each entry is the same
    /// `(grain_type, fields)` pair [`cal_add`](CalStoreFacade::cal_add) takes,
    /// validated identically — the batching is in the write, not the parsing,
    /// so a malformed entry fails the whole call and writes nothing.
    ///
    /// Worth roughly 1.6x over the same grains added one at a time (244 ->
    /// 148 us/grain, measured at 2k grains), saturating around a batch of 10.
    /// Note this only helps with the BM25 text index **off**: with it on, the
    /// per-row index cost dominates so completely that batch size makes no
    /// difference at all (~17ms/grain at every size) — see
    /// `defer_text_index` and tursodatabase/turso#8170.
    pub fn cal_add_batch(
        &self,
        entries: &[(String, serde_json::Map<String, serde_json::Value>)],
    ) -> Result<Vec<Hash>> {
        // Build every grain first so a bad entry is rejected before anything
        // is written, then hand the whole set to the store as one batch.
        let mut built: Vec<Box<dyn dejadb_store::AddableDyn>> = Vec::with_capacity(entries.len());
        for (grain_type, fields) in entries {
            build_grain_from_json(grain_type, fields, CollectSink { out: &mut built })?;
        }
        let refs: Vec<&dyn dejadb_store::AddableDyn> = built.iter().map(|b| b.as_ref()).collect();
        let mut m = self.store.lock().unwrap();
        m.add_batch(&refs)
    }

    fn hit(grain: DeserializedGrain) -> SearchHit {
        let hash = grain.hash;
        SearchHit {
            grain,
            score: 1.0,
            hash,
            score_breakdown: None,
            explanation: None,
            scope_depth: None,
            source_namespace: None,
            relative_time: None,
            conflict_status: None,
            supersession_status: None,
            superseded_by_hash: None,
            recall_source: None,
        }
    }
}

/// Sink that keeps the built grain instead of writing it, so a whole batch
/// can be validated up front and then written in one transaction.
struct CollectSink<'a> {
    out: &'a mut Vec<Box<dyn dejadb_store::AddableDyn>>,
}
impl GrainSink for CollectSink<'_> {
    type Out = ();
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<()> {
        self.out.push(Box::new(grain.clone()));
        Ok(())
    }
}

struct AddSink<'a> {
    m: &'a mut DejaDB,
}
impl GrainSink for AddSink<'_> {
    type Out = Hash;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Hash> {
        self.m.add(grain)
    }
}

struct AddIfNovelSink<'a> {
    m: &'a mut DejaDB,
}
impl GrainSink for AddIfNovelSink<'_> {
    type Out = (Hash, bool);
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<(Hash, bool)> {
        self.m.add_if_novel(grain)
    }
}

struct SupersedeSink<'a> {
    m: &'a mut DejaDB,
    old: Hash,
}
impl GrainSink for SupersedeSink<'_> {
    type Out = Hash;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Hash> {
        let mut g = grain.clone();
        self.m.supersede(&self.old, &mut g)
    }
}

impl DejaDbFacade {
    /// Note an entry the file carries that this process could not load.
    ///
    /// Skipping is deliberate — one unloadable row must not make the whole
    /// memory unusable — but skipping *silently* is not: a saved query that
    /// vanishes without a word looks like it was never written.
    fn note_meta_warning(&self, kind: &str, name: &str, why: impl std::fmt::Display) {
        self.meta_warnings
            .lock()
            .expect("meta warnings poisoned")
            .push(format!(
                "{kind} \"{name}\" is in the file but could not be loaded ({why}); \
                 it is not available in this process and will be lost if you overwrite it"
            ));
    }

    /// Saved queries and templates the file carries that this process could not
    /// load. Empty when everything in the file is usable here.
    pub fn meta_warnings(&self) -> Vec<String> {
        self.meta_warnings
            .lock()
            .expect("meta warnings poisoned")
            .clone()
    }

    /// Run `f` against the saved-query registry, rehydrating it from the
    /// file's `meta` rows on first use. A row that fails to parse or register
    /// is skipped rather than failing the whole open — one bad entry must not
    /// make the memory unusable — but it is recorded in [`Self::meta_warnings`].
    fn with_queries<R>(&self, f: impl FnOnce(&mut QueryRegistry) -> R) -> R {
        let mut guard = self.queries.lock().expect("query registry poisoned");
        if guard.is_none() {
            let mut reg = QueryRegistry::new();
            match self.with_store(|m| m.meta_scan(QRY_PREFIX)) {
                Ok(rows) => {
                    for (name, json) in rows {
                        match serde_json::from_str::<PersistedQuery>(&json) {
                            Ok(p) => {
                                if let Err(e) = reg.register_full(
                                    &name,
                                    &p.body,
                                    &p.description,
                                    &p.params,
                                    p.last_run_at,
                                    p.updated_at,
                                ) {
                                    self.note_meta_warning("saved query", &name, e);
                                }
                            }
                            Err(e) => self.note_meta_warning("saved query", &name, e),
                        }
                    }
                }
                Err(e) => self.note_meta_warning("saved queries", "*", e),
            }
            *guard = Some(reg);
        }
        f(guard.as_mut().expect("initialised directly above"))
    }

    /// Same for custom templates.
    fn with_templates<R>(&self, f: impl FnOnce(&mut TemplateRegistry) -> R) -> R {
        let mut guard = self.templates.lock().expect("template registry poisoned");
        if guard.is_none() {
            let mut reg = TemplateRegistry::new();
            match self.with_store(|m| m.meta_scan(TPL_PREFIX)) {
                Ok(rows) => {
                    for (name, json) in rows {
                        match serde_json::from_str::<PersistedTemplate>(&json) {
                            Ok(p) => {
                                match reg.register(
                                    &name,
                                    &p.source,
                                    &p.description,
                                    p.parent.as_deref(),
                                ) {
                                    Ok(()) => {
                                        reg.restore_timestamps(&name, p.last_run_at, p.updated_at);
                                        // The FOR clause lives on the statement,
                                        // not in the body, so it only survives a
                                        // reload if it is put back explicitly.
                                        reg.set_grain_types(&name, &p.grain_types);
                                    }
                                    Err(e) => self.note_meta_warning("template", &name, e),
                                }
                            }
                            Err(e) => self.note_meta_warning("template", &name, e),
                        }
                    }
                }
                Err(e) => self.note_meta_warning("templates", "*", e),
            }
            *guard = Some(reg);
        }
        f(guard.as_mut().expect("initialised directly above"))
    }

    fn persist_query(&self, name: &str, p: &PersistedQuery) -> Result<()> {
        let json = serde_json::to_string(p)
            .map_err(|e| DejaDbError::Validation(format!("saved query \"{name}\": {e}")))?;
        self.with_store(|m| m.meta_put(&format!("{QRY_PREFIX}{name}"), &json))
    }

    fn snapshot_query(&self, name: &str) -> Option<PersistedQuery> {
        self.with_queries(|reg| {
            reg.get(name).map(|e| PersistedQuery {
                body: e.body.clone(),
                description: e.description.clone(),
                params: e.params.clone(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }

    fn snapshot_template(&self, name: &str) -> Option<PersistedTemplate> {
        self.with_templates(|reg| {
            reg.get(name).filter(|e| !e.builtin).map(|e| PersistedTemplate {
                source: e.template.source().to_string(),
                description: e.description.clone(),
                parent: e.parent.clone(),
                grain_types: e.grain_types.clone(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }
}

impl CalStoreFacade for DejaDbFacade {
    /// Record one assembly-budget sample into the telemetry sidecar (feeds the
    /// `budget_pressure` analyzer). Best-effort: telemetry never fails a query.
    fn note_assembly_budget(&self, overflow: bool) {
        let _ = self.with_store(|m| m.telemetry_note_budget(overflow));
    }

    // ── CAL host metadata: saved queries and custom templates ───────────
    //
    // The registry owns the rules (name shape, per-namespace cap, body size,
    // parameter count), so every write validates in memory first and only
    // then touches the file. If the file write fails the in-memory entry is
    // rolled back, so the registry never runs ahead of what is persisted.

    fn define_query(
        &self,
        name: &str,
        body: &str,
        description: Option<&str>,
        params: &[QueryParam],
    ) -> Result<()> {
        let existing = self.snapshot_query(name);
        self.with_queries(|reg| reg.register(name, body, description.unwrap_or(""), params))
            .map_err(DejaDbError::Validation)?;
        let Some(p) = self.snapshot_query(name) else {
            return Err(DejaDbError::Internal(format!(
                "saved query \"{name}\" vanished after registration"
            )));
        };
        if let Err(e) = self.persist_query(name, &p) {
            // Roll the registry back to whatever was there before.
            self.with_queries(|reg| {
                let _ = reg.delete(name);
                if let Some(prev) = &existing {
                    let _ = reg.register_full(
                        name,
                        &prev.body,
                        &prev.description,
                        &prev.params,
                        prev.last_run_at,
                        prev.updated_at,
                    );
                }
            });
            return Err(e);
        }
        Ok(())
    }

    fn drop_query(&self, name: &str) -> Result<()> {
        // Snapshot before deleting: if the file write fails the registry has to
        // go back, or the entry is gone here and still on disk — it would
        // reappear on the next open, which reads as the drop never happening.
        let existing = self.snapshot_query(name);
        self.with_queries(|reg| reg.delete(name))
            .map_err(DejaDbError::Validation)?;
        if let Err(e) = self.with_store(|m| m.meta_delete(&format!("{QRY_PREFIX}{name}"))) {
            if let Some(prev) = &existing {
                self.with_queries(|reg| {
                    let _ = reg.register_full(
                        name,
                        &prev.body,
                        &prev.description,
                        &prev.params,
                        prev.last_run_at,
                        prev.updated_at,
                    );
                });
            }
            return Err(e);
        }
        Ok(())
    }

    fn list_queries(&self) -> Vec<QueryListEntry> {
        self.with_queries(|reg| reg.list())
    }

    fn get_query(&self, name: &str) -> Option<QueryEntry> {
        self.with_queries(|reg| reg.get(name).cloned())
    }

    fn update_query_last_run(&self, name: &str) -> Result<()> {
        let now = now_secs();
        let updated = self.with_queries(|reg| {
            let e = reg.get(name)?;
            // Built-ins are not persisted, and a re-run within the same second
            // cannot change the one-second-resolution timestamp — so it would
            // be a write transaction that rewrites the row with what it
            // already holds.
            if e.builtin || e.last_run_at == Some(now) {
                return None;
            }
            let (body, description, params, updated_at) = (
                e.body.clone(),
                e.description.clone(),
                e.params.clone(),
                e.updated_at,
            );
            reg.register_full(
                name,
                &body,
                &description,
                &params,
                Some(now),
                updated_at,
            )
            .ok()?;
            Some(PersistedQuery {
                body,
                description,
                params,
                last_run_at: Some(now),
                updated_at,
            })
        });
        match updated {
            Some(p) => self.persist_query(name, &p),
            None => Ok(()),
        }
    }

    fn define_template(
        &self,
        name: &str,
        source: &str,
        description: Option<&str>,
        parent: Option<&str>,
        grain_types: &[String],
    ) -> Result<()> {
        let existing = self.snapshot_template(name);
        self.with_templates(|reg| {
            reg.register(name, source, description.unwrap_or(""), parent)?;
            // The FOR clause rides the statement, not the body — set it on the
            // entry too, so the in-memory and persisted views agree.
            reg.set_grain_types(name, grain_types);
            Ok(())
        })
        .map_err(|e: CalError| DejaDbError::Validation(e.to_string()))?;
        let p = PersistedTemplate {
            source: source.to_string(),
            description: description.unwrap_or("").to_string(),
            parent: parent.map(str::to_string),
            grain_types: grain_types.to_vec(),
            last_run_at: None,
            updated_at: Some(now_secs()),
        };
        let json = serde_json::to_string(&p)
            .map_err(|e| DejaDbError::Validation(format!("template \"{name}\": {e}")))?;
        if let Err(e) = self.with_store(|m| m.meta_put(&format!("{TPL_PREFIX}{name}"), &json)) {
            // Roll the registry back to whatever was there before, so it never
            // runs ahead of what is persisted.
            self.with_templates(|reg| {
                let _ = reg.delete(name);
                if let Some(prev) = &existing {
                    if reg
                        .register(name, &prev.source, &prev.description, prev.parent.as_deref())
                        .is_ok()
                    {
                        reg.restore_timestamps(name, prev.last_run_at, prev.updated_at);
                        reg.set_grain_types(name, &prev.grain_types);
                    }
                }
            });
            return Err(e);
        }
        Ok(())
    }

    fn drop_template(&self, name: &str) -> Result<()> {
        let existing = self.snapshot_template(name);
        self.with_templates(|reg| reg.delete(name))
            .map_err(|e| DejaDbError::Validation(e.to_string()))?;
        if let Err(e) = self.with_store(|m| m.meta_delete(&format!("{TPL_PREFIX}{name}"))) {
            if let Some(prev) = &existing {
                self.with_templates(|reg| {
                    if reg
                        .register(name, &prev.source, &prev.description, prev.parent.as_deref())
                        .is_ok()
                    {
                        reg.restore_timestamps(name, prev.last_run_at, prev.updated_at);
                        reg.set_grain_types(name, &prev.grain_types);
                    }
                });
            }
            return Err(e);
        }
        Ok(())
    }

    fn list_templates(&self) -> Vec<TemplateInfo> {
        self.with_templates(|reg| reg.list())
    }

    fn get_template(&self, name: &str) -> Option<TemplateInfo> {
        // Direct lookup, not `list().find()`: this runs on the FORMAT render
        // path, and building the whole list (every source string cloned) to
        // throw all but one away is work proportional to the registry on every
        // rendered query.
        self.with_templates(|reg| {
            reg.get(name).map(|e| TemplateInfo {
                name: name.to_string(),
                description: e.description.clone(),
                builtin: e.builtin,
                parent: e.parent.clone(),
                grain_types: e.grain_types.clone(),
                source: e.template.source().to_string(),
                last_run_at: e.last_run_at,
                updated_at: e.updated_at,
            })
        })
    }

    fn record_template_run(&self, name: &str) {
        let persisted = self.with_templates(|reg| {
            let before = reg.get(name).and_then(|e| e.last_run_at);
            reg.record_run(name);
            let entry = reg.get(name)?;
            // `last_run_at` has one-second resolution, so a second render in
            // the same second cannot change what is on disk. Skipping that
            // write matters: this runs on the FORMAT path, and a rendered
            // recall should not cost a write transaction per call.
            if entry.builtin || entry.last_run_at == before {
                return None;
            }
            Some(PersistedTemplate {
                source: entry.template.source().to_string(),
                description: entry.description.clone(),
                parent: entry.parent.clone(),
                grain_types: entry.grain_types.clone(),
                last_run_at: entry.last_run_at,
                updated_at: entry.updated_at,
            })
        });
        if let Some(p) = persisted {
            if let Ok(json) = serde_json::to_string(&p) {
                let _ = self.with_store(|m| m.meta_put(&format!("{TPL_PREFIX}{name}"), &json));
            }
        }
    }

    fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>> {
        // mount routing: "alias.inner" namespaces hit mounted replicas
        let requested = params.namespace.as_deref().or(self.namespace.as_deref());
        let (mount_alias, ns_owned) = match requested {
            Some(full) => match full.split_once('.') {
                Some((alias, inner)) if self.mounts.contains_key(alias) => {
                    (Some(alias.to_string()), inner.to_string())
                }
                _ => (None, full.to_string()),
            },
            None => (None, "shared".to_string()),
        };
        let ns = ns_owned.as_str();
        let mut m = match &mount_alias {
            Some(a) => self.mounts.get(a).unwrap().lock().unwrap(),
            None => self.store.lock().unwrap(),
        };
        let k = params.limit.unwrap_or(16).min(1000);

        // M4: hybrid recall — structural leg + BM25 leg fused with RRF.
        // A query alone, a subject alone, or both are all valid.
        //
        // With neither a subject nor a free-text query there is no leg to hang
        // ranking on, so fall back to a bounded recent-by-type scan (newest
        // first). This is the "reflect over recent experience" path — e.g.
        // `RECALL events RECENT 20`, `RECALL observations WHERE session_id = X`
        // — whose WHERE conditions (session_id, observer_id, object, …) are
        // applied as post-filters below and by the executor. Bare `RECALL *`
        // (no grain type) with no anchor is still rejected as too broad.
        // Which leg ran matters below: `recent` takes no structural predicates
        // and does not know about supersession, so this path has to reapply
        // both itself.
        let unanchored = params.subject.is_none() && params.query.is_none();
        let raw = if unanchored {
            match params.grain_type {
                // Heads only, unless `WITH superseded` asked otherwise. The
                // anchored leg already serves heads; this one read the grains
                // table straight through, so a superseded value came back
                // alongside the head that replaced it and recall reported both
                // as current. Supersession is index-layer state, so the
                // distinction has to be made in the query, not after it.
                Some(_) if params.exclude_superseded != Some(false) => m.recent_live(
                    ns,
                    params.grain_type,
                    k.saturating_mul(Self::RECALL_OVERFETCH),
                )?,
                Some(_) => m.recent(
                    ns,
                    params.grain_type,
                    k.saturating_mul(Self::RECALL_OVERFETCH),
                )?,
                None => {
                    return Err(DejaDbError::Validation(
                        "RECALL needs a subject filter, a free-text (LIKE) query, \
                         or a specific grain type with RECENT/LIMIT"
                            .into(),
                    ))
                }
            }
        } else {
            // Translate the recall flags the executor set from `WITH` options
            // (diversity / rerank / query_expansion) into engine tuning. MMR is
            // the only diversity method wired in-engine; the threshold variant
            // is not reachable from CAL's `WITH diversity`.
            let tuning = dejadb_store::RecallTuning {
                query_expansion: params.query_expansion == Some(true),
                rerank: params.rerank.is_some(),
                diversity_lambda: params.diversity.as_ref().and_then(|d| match d.method {
                    DiversityMethod::Mmr { lambda } => Some(lambda),
                    DiversityMethod::Threshold(_) => None,
                }),
            };
            m.recall_hybrid_tuned(
                ns,
                params.subject.as_deref(),
                params.relation.as_deref(),
                params.query.as_deref(),
                k.saturating_mul(Self::RECALL_OVERFETCH),
                None,
                tuning,
            )?
        };
        drop(m);

        let hits = raw
            .into_iter()
            // `relation` reaches the anchored leg as a store-side predicate,
            // but `recent` takes no predicates at all — so on that path the
            // filter was simply dropped and `RECALL facts WHERE relation = "x"`
            // answered with every grain of that type. Silently returning more
            // than was asked for is worse than returning nothing.
            .filter(|g| {
                !unanchored
                    || match &params.relation {
                        Some(r) => g.get_str("relation") == Some(r.as_str()),
                        None => true,
                    }
            })
            .filter(|g| match &params.object {
                Some(o) => g.get_str("object") == Some(o.as_str()),
                None => true,
            })
            .filter(|g| match params.grain_type {
                Some(gt) => g.grain_type == gt,
                None => true,
            })
            .filter(|g| {
                let ca = g.get_i64("created_at").unwrap_or(0);
                params.time_start.is_none_or(|t| ca >= t)
                    && params.time_end.is_none_or(|t| ca <= t)
            })
            .filter(|g| match params.confidence_threshold {
                Some(c) => g.get_f64("confidence").unwrap_or(0.0) >= c,
                None => true,
            })
            .take(k)
            .map(Self::hit)
            .collect();
        Ok(hits)
    }

    fn exists(&self, hash: &Hash) -> Result<bool> {
        self.store.lock().unwrap().has(hash)
    }

    fn get(&self, hash: &Hash) -> Result<DeserializedGrain> {
        self.store.lock().unwrap().get(hash)
    }

    fn count(&self) -> Result<usize> {
        self.store.lock().unwrap().count()
    }

    fn get_history(&self, namespace: &str, subject: &str, relation: &str) -> Result<Vec<VersionEntry>> {
        let entries = self.store.lock().unwrap().history(namespace, subject, relation)?;
        Ok(entries
            .into_iter()
            .map(|e| VersionEntry {
                hash: e.hash,
                object: e.object,
                created_at: e.created_at,
                confidence: e.confidence,
                superseded_by: e.superseded_by,
            })
            .collect())
    }

    fn open_forks(&self) -> Result<Vec<ForkGroupInfo>> {
        let groups = self.with_store(|m| m.open_forks())?;
        Ok(groups
            .into_iter()
            .map(|f| ForkGroupInfo {
                namespace: f.namespace,
                subject: f.subject,
                relation: f.relation,
                // `DejaDB::heads` orders `created_at DESC, hash DESC` — the same
                // tuple the provisional-head election uses — so tip 0 is the
                // value recall serves. Preserve that order; CONTRADICTIONS
                // reports every other tip as a peer of it.
                heads: f.heads.iter().map(|h| h.to_hex()).collect(),
            })
            .collect())
    }

    fn default_namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    fn active_user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    fn cal_add(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddSink { m: &mut m })
    }

    fn cal_supersede(
        &self,
        old_hash: &Hash,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(
            grain_type,
            fields,
            SupersedeSink {
                m: &mut m,
                old: *old_hash,
            },
        )
    }

    /// `FORGET <hash>` — tombstone a single grain by content address. Only
    /// ever hits the session store; mounts are read-only by construction.
    /// Gated upstream by `CalExecutorConfig::allow_destructive_ops`.
    fn cal_delete(&self, hash: &Hash) -> Result<()> {
        self.store.lock().unwrap().forget(hash)
    }
}
