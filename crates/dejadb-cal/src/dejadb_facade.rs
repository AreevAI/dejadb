//! DejaDbFacade — CalStoreFacade over the embedded dejadb-store.
//!
//! Session-scoped: carries the capability defaults (namespace, user) that
//! CAL queries inherit when they don't specify one (§7.11 direction).
//!
//! M2 scope: structural recall only. Semantic (`query`) recall returns a
//! clear error until the FTS/vector legs land (M4).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use dejadb_core::authz::Verb;
use dejadb_core::error::{Hash, DejaDbError, Result};
use dejadb_core::format::deserialize::DeserializedGrain;
use dejadb_core::types::Grain;
use dejadb_store::DejaDB;

use crate::ast::QueryParam;
use crate::errors::CalError;
use crate::facade::{CalStoreFacade, TemplateInfo};
use crate::json_build::{build_grain_from_json, GrainSink};
use crate::queries::{PersistedQuery, QueryEntry, QueryListEntry, QueryRegistry};
use crate::store_types::{
    ConflictStatus, DiversityMethod, ForkGroupInfo, RecallParams, SearchHit, SupersessionStatus,
    VersionEntry,
};
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A synthetic tool-call id for a host that did not supply one, so that two
/// executions of the same tool stay two records.
///
/// The `auto:` prefix is deliberate: it marks the id as minted here rather than
/// carried from a provider, so nothing downstream mistakes it for a real
/// `tool_call_id` it could correlate against an LLM transcript.
///
/// Uniqueness comes from the counter, which is monotonic within a process and
/// therefore holds even where the clock is coarse or steps backwards. The
/// timestamp and pid only separate *different* processes writing the same
/// memory in sequence — a clock tick alone would not, since a short-lived
/// process restarts the counter at zero.
///
/// This does make the record non-reproducible: recording the same call twice
/// writes two grains. That is the intended reading of an occurrence log — it
/// happened twice — and it is why the id is minted per call rather than derived
/// from the call's content, which would put us straight back into collapsing
/// retries.
fn occurrence_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "auto:{}-{}-{}",
        std::process::id(),
        chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_else(|| now_ms().saturating_mul(1_000_000)),
        n
    )
}

/// Membership test for the `WHERE <field> IN (...)` filters. `None` is "no
/// filter"; an empty set never reaches here (the caller short-circuits, because
/// an empty `IN` selects nothing rather than everything).
fn set_contains(set: &Option<Vec<String>>, value: Option<&str>) -> bool {
    match set {
        None => true,
        Some(values) => value.is_some_and(|v| values.iter().any(|c| c == v)),
    }
}

/// "3 hours ago" for `WITH annotate_relative_time`. Coarse on purpose: the
/// point is freshness at a glance, and a model reading "2 weeks ago" needs no
/// more precision than that. Negative ages (a grain stamped in the future,
/// which import and clock skew both produce) read as "just now" rather than
/// wrapping into a nonsense past.
fn relative_time_label(age_ms: i64) -> String {
    const MIN: i64 = 60_000;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    match age_ms {
        a if a < MIN => "just now".into(),
        a if a < HOUR => plural(a / MIN, "minute"),
        a if a < DAY => plural(a / HOUR, "hour"),
        a if a < 7 * DAY => plural(a / DAY, "day"),
        a if a < 30 * DAY => plural(a / (7 * DAY), "week"),
        a if a < 365 * DAY => plural(a / (30 * DAY), "month"),
        a => plural(a / (365 * DAY), "year"),
    }
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
    /// The session's resolved rights. Defaults to the owner session — a
    /// local open with no principal asserted is the implicit superuser
    /// (`root@localhost`), which is why the single-user path never meets
    /// authorization. `with_principal` replaces it with a fail-closed
    /// restricted set built from the file's own grant grains.
    authz: dejadb_core::authz::AuthzSet,
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
            authz: dejadb_core::authz::AuthzSet::owner("user:local"),
        }
    }

    /// Bind this facade to a principal: rights become exactly what the
    /// file's live grant grains cover (fail closed — a file with no grants
    /// for this principal allows nothing). Grants are read once here; a
    /// grant written later needs a rebind (the CAL `GRANT` path will
    /// refresh this itself when it lands).
    pub fn with_principal(mut self, principal: &str) -> dejadb_core::error::Result<Self> {
        let grants = {
            let mut guard = self.store.lock().unwrap();
            guard.authz_grants(principal)?
        };
        self.authz = dejadb_core::authz::AuthzSet::restricted(principal, grants);
        Ok(self)
    }

    /// The session's resolved rights (owner unless a principal was bound).
    pub fn authz(&self) -> &dejadb_core::authz::AuthzSet {
        &self.authz
    }

    /// The namespace a JSON-built write lands in: the explicit `namespace`
    /// field, else the session default, else `"shared"` (the store default).
    /// This is the resource every write-verb check runs against.
    fn write_ns<'a>(&'a self, fields: &'a serde_json::Map<String, serde_json::Value>) -> &'a str {
        fields
            .get("namespace")
            .and_then(|v| v.as_str())
            .or(self.namespace.as_deref())
            .unwrap_or("shared")
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

    /// How many first-pass results seed `WITH multi_hop`. The top few name the
    /// entities worth following; taking every result would fan out over the
    /// long tail of a weak match.
    pub const MULTI_HOP_SEED: usize = 8;

    /// Candidates pulled per entity per hop. Small on purpose — a hop is a
    /// widening heuristic, and the candidate pool is capped anyway.
    pub const MULTI_HOP_FANOUT: usize = 8;

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
        self.authz.check(Verb::Write, self.write_ns(fields))?;
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddIfNovelSink { m: &mut m })
    }

    /// Record one tool execution. The single implementation behind
    /// `record_tool_call` on every binding, so Python and Node cannot drift.
    ///
    /// Each call is stamped with an identity, which is what makes this record an
    /// *occurrence* rather than a value. Without one they collapsed: grains are
    /// content-addressed over the whole blob and `created_at` has millisecond
    /// resolution, so byte-identical calls recorded inside one millisecond hash
    /// to a single address, and since 1.1.1 a duplicate add is a silent no-op.
    /// An agent retrying a failing tool in a tight loop is precisely that
    /// workload — and "how many times did this fail" is the entire input to
    /// `loop.tool_failure`, so the flagship analyzer lost its signal exactly
    /// where it is meant to fire. The documented five-failure example stored one
    /// grain and produced no recommendation at all (#66).
    ///
    /// `call_id` is the host's invocation id (an LLM `tool_call_id`), stored as
    /// the grain's `tool_call_id` and queryable as such — the correlation key
    /// back to the provider transcript. Absent one, [`occurrence_id`] mints a
    /// synthetic id.
    ///
    /// It is not a de-duplication key: replaying the same `call_id` later writes
    /// a second grain, since `created_at` is part of the content address too.
    /// Recording is append-only, and a host replaying a tool log owns not
    /// replaying it twice.
    pub fn record_tool_call(
        &self,
        ns: &str,
        tool_name: &str,
        result: &str,
        is_error: bool,
        thread: Option<&str>,
        call_id: Option<&str>,
    ) -> Result<Hash> {
        let mut fields = serde_json::Map::new();
        fields.insert("tool_name".into(), serde_json::json!(tool_name));
        fields.insert("content".into(), serde_json::json!(result));
        fields.insert("is_error".into(), serde_json::json!(is_error));
        fields.insert("namespace".into(), serde_json::json!(ns));
        if let Some(t) = thread {
            fields.insert("session_id".into(), serde_json::json!(t));
        }
        fields.insert(
            "tool_call_id".into(),
            serde_json::json!(match call_id {
                Some(id) => id.to_string(),
                None => occurrence_id(),
            }),
        );
        self.cal_add("tool", &fields)
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
        // Every entry's namespace is checked before anything is written —
        // a batch is all-or-nothing for authorization like it is for
        // validation.
        for (_, fields) in entries {
            self.authz.check(Verb::Write, self.write_ns(fields))?;
        }
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
        // Saved queries and templates are the host-config surface: admin.
        self.authz.check(Verb::Admin, "*")?;
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
        self.authz.check(Verb::Admin, "*")?;
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
        self.authz.check(Verb::Admin, "*")?;
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
        self.authz.check(Verb::Admin, "*")?;
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
        // `WHERE <field> IN (...)` with nothing in it selects nothing. This is
        // the sharp edge of the whole `IN` family: `LET $friends = …` binding to
        // the empty set is the *natural* "this user has no friends yet"
        // outcome, and treating an empty filter as no filter answered with the
        // entire table. Fail closed, before any leg runs.
        for empty in [&params.subject_in, &params.relation_in, &params.object_in] {
            if empty.as_ref().is_some_and(|v| v.is_empty()) {
                return Ok(Vec::new());
            }
        }

        // mount routing: "alias.inner" namespaces hit mounted replicas
        let requested = params.namespace.as_deref().or(self.namespace.as_deref());
        // The read gate. A session with no namespace scope reads everything,
        // so it needs a grant covering `*`; owner sessions pass untouched.
        self.authz.check(Verb::Read, requested.unwrap_or("*"))?;
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
        //
        // `WHERE subject IN (a, b, c)` anchors just as well as `subject = a` —
        // it is a union of anchored legs, one per value. Treating it as
        // unanchored dropped the query into the recent-by-type scan, where the
        // filter was never applied at all (it lives in `RecallParams` and
        // nothing downstream read it), so the documented
        // `LET $friends = SUBJECTS OF (…)` pattern answered with *everybody's*
        // preferences instead of the friends'. Anchoring per value also means
        // the scan is bounded by the subjects asked for rather than by
        // `default_limit` — a post-filter over the newest 50 would miss a
        // named subject whose grains are older than that.
        let anchors: Vec<String> = match (&params.subject, &params.subject_in) {
            (Some(s), _) => vec![s.clone()],
            (None, Some(values)) => values.clone(),
            (None, None) => Vec::new(),
        };
        let unanchored = anchors.is_empty() && params.query.is_none();
        // `WITH superseded` — the caller is asking about the past, so every leg
        // widens from the heads to the whole supersession chain.
        let include_superseded = params.exclude_superseded == Some(false);
        let raw = if unanchored {
            match params.grain_type {
                // Heads only, unless `WITH superseded` asked otherwise. The
                // anchored leg already serves heads; this one read the grains
                // table straight through, so a superseded value came back
                // alongside the head that replaced it and recall reported both
                // as current. Supersession is index-layer state, so the
                // distinction has to be made in the query, not after it.
                Some(_) if !include_superseded => m.recent_live(
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
                include_superseded,
            };
            let budget = k.saturating_mul(Self::RECALL_OVERFETCH);
            match anchors.len() {
                // The common case, and the voice hot path: one anchored leg.
                0 | 1 => m.recall_hybrid_tuned(
                    ns,
                    anchors.first().map(String::as_str),
                    params.relation.as_deref(),
                    params.query.as_deref(),
                    budget,
                    None,
                    tuning,
                )?,
                // `subject IN (a, b, …)`: one anchored leg per value, unioned.
                // Each leg gets the full budget so a subject with many grains
                // cannot starve the others out of the result set; the union is
                // deduped by content address and the caller's LIMIT is applied
                // below, as it is on every other path.
                _ => {
                    let mut seen: HashSet<Hash> = HashSet::new();
                    let mut union = Vec::new();
                    for anchor in &anchors {
                        for g in m.recall_hybrid_tuned(
                            ns,
                            Some(anchor.as_str()),
                            params.relation.as_deref(),
                            params.query.as_deref(),
                            budget,
                            None,
                            tuning,
                        )? {
                            if seen.insert(g.hash) {
                                union.push(g);
                            }
                        }
                    }
                    // Newest first, matching what a single anchored leg returns.
                    union.sort_by(|a, b| {
                        b.get_i64("created_at")
                            .unwrap_or(0)
                            .cmp(&a.get_i64("created_at").unwrap_or(0))
                    });
                    union
                }
            }
        };

        // `WITH multi_hop(n)` — entity-graph expansion.
        //
        // Take the entities the first pass surfaced (its results' subject and
        // object terms), anchor a fresh recall on each, and add what comes back
        // to the candidate pool. Repeat `n` times, following the graph outward.
        //
        // Both directions: an entity is followed as a subject (what does X point
        // at) *and* as an object (what points at X). Forward-only expansion made
        // "who else works here" — the archetypal one-hop question — return
        // nothing, since entities are harvested from the `object` field but were
        // only ever re-anchored as subjects. The reverse leg reads the OSP index,
        // so it sees the relations the file declares as entity relations; that is
        // the same rule every other reverse traversal in the engine follows.
        //
        // Expansion happens here, before the post-filters and the `LIMIT` below,
        // so hops *compete* for the k slots the caller asked for rather than
        // extending past them. Appended after the first pass, so a direct match
        // always outranks something reached by association.
        //
        // Fail-open, like every other recall refinement: a hop that errors is
        // skipped rather than failing the query.
        let mut raw = raw;
        if let Some(hops) = params.multi_hop.filter(|h| *h > 0) {
            let mut seen: HashSet<Hash> = raw.iter().map(|g| g.hash).collect();
            let mut visited: HashSet<String> = HashSet::new();
            let mut frontier: Vec<String> = Vec::new();
            let push_entities = |g: &DeserializedGrain,
                                     visited: &mut HashSet<String>,
                                     out: &mut Vec<String>| {
                for field in ["subject", "object"] {
                    if let Some(v) = g.get_str(field) {
                        if !v.is_empty() && visited.insert(v.to_string()) {
                            out.push(v.to_string());
                        }
                    }
                }
            };
            for g in raw.iter().take(Self::MULTI_HOP_SEED) {
                push_entities(g, &mut visited, &mut frontier);
            }

            let budget = k.saturating_mul(Self::RECALL_OVERFETCH);
            'hops: for _ in 0..hops {
                let mut next: Vec<String> = Vec::new();
                for entity in std::mem::take(&mut frontier) {
                    if raw.len() >= budget {
                        break 'hops;
                    }
                    let forward = m
                        .recall_hybrid(
                            ns,
                            Some(&entity),
                            None,
                            params.query.as_deref(),
                            Self::MULTI_HOP_FANOUT,
                            None,
                        )
                        .unwrap_or_default();
                    let reverse = m
                        .grains_by_object(ns, &entity, Self::MULTI_HOP_FANOUT)
                        .unwrap_or_default();
                    for g in forward.into_iter().chain(reverse) {
                        if seen.insert(g.hash) {
                            push_entities(&g, &mut visited, &mut next);
                            raw.push(g);
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
        }
        drop(m);

        let mut hits: Vec<SearchHit> = raw
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
            // The `IN` family. `subject_in` already anchored the legs above, but
            // an anchored leg on "jane" can still surface grains where jane is
            // the *object*, so all three are re-applied here as membership
            // tests. Before this, nothing in the engine read these three fields
            // at all: the executor set them and the query returned every row the
            // rest of the WHERE matched, with no error and no warning.
            .filter(|g| set_contains(&params.subject_in, g.get_str("subject")))
            .filter(|g| set_contains(&params.relation_in, g.get_str("relation")))
            .filter(|g| set_contains(&params.object_in, g.get_str("object")))
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

        // Label the history. A widened recall returns stale versions next to the
        // heads that replaced them, and nothing in an immutable blob says which
        // is which — unlabeled, `WITH superseded` would quietly feed a model
        // outdated values that read as current. Renderers and the RF-2
        // context rule key off these two fields, so stamping them is what makes
        // the widened result honest rather than merely bigger.
        if include_superseded && !hits.is_empty() {
            let hashes: Vec<Hash> = hits.iter().map(|h| h.hash).collect();
            let mut m = match &mount_alias {
                Some(a) => self.mounts.get(a).unwrap().lock().unwrap(),
                None => self.store.lock().unwrap(),
            };
            // Fail-open, like every other recall refinement: an unlabeled result
            // is worse than a labeled one but far better than a failed query.
            if let Ok(map) = m.supersession_map(&hashes) {
                drop(m);
                for hit in &mut hits {
                    hit.supersession_status = Some(match map.get(&hit.hash) {
                        Some(_) => SupersessionStatus::Superseded,
                        None => SupersessionStatus::Current,
                    });
                    hit.superseded_by_hash = map.get(&hit.hash).copied();
                }
            }
        }

        // `WITH conflict_resolution` — keep only the newest grain per
        // (subject, relation). Documented in §5 and advertised by DESCRIBE, but
        // the implementation lived on the ASSEMBLE post-merge path only, which
        // a RECALL payload never reaches — so a caller specifically trying to
        // avoid feeding two contradictory values into a model's context got
        // both, with no signal. Ties break on the later hash so the choice is
        // deterministic across nodes rather than dependent on scan order.
        if params.conflict_resolution == Some(true) {
            let mut newest: HashMap<(String, String), (i64, Hash)> = HashMap::new();
            for h in &hits {
                let (Some(s), Some(r)) = (h.grain.get_str("subject"), h.grain.get_str("relation"))
                else {
                    continue;
                };
                let created = h.grain.get_i64("created_at").unwrap_or(0);
                let key = (s.to_string(), r.to_string());
                let better = match newest.get(&key) {
                    None => true,
                    Some((best, best_hash)) => {
                        (created, h.hash.as_bytes()) > (*best, best_hash.as_bytes())
                    }
                };
                if better {
                    newest.insert(key, (created, h.hash));
                }
            }
            for h in &mut hits {
                let keep = match (h.grain.get_str("subject"), h.grain.get_str("relation")) {
                    // A grain with no (subject, relation) key has nothing to
                    // conflict with, so it is never the loser of a comparison.
                    (Some(s), Some(r)) => newest
                        .get(&(s.to_string(), r.to_string()))
                        .is_none_or(|(_, hash)| *hash == h.hash),
                    _ => true,
                };
                h.conflict_status = Some(if keep {
                    ConflictStatus::Current
                } else {
                    ConflictStatus::Outdated
                });
            }
            hits.retain(|h| h.conflict_status != Some(ConflictStatus::Outdated));
        }

        // `WITH annotate_relative_time` — "3 hours ago" beside the epoch
        // milliseconds, so a model reading the context does not have to do
        // date arithmetic to know whether a memory is fresh.
        if params.annotate_relative_time == Some(true) {
            let now = now_ms();
            for h in &mut hits {
                h.relative_time = h
                    .grain
                    .get_i64("created_at")
                    .map(|created| relative_time_label(now - created));
            }
        }

        // `WITH explanation` — why this grain came back. Template-based and
        // derived entirely from the predicates that ran (the field's contract:
        // no LLM, suitable for EU AI Act Art. 86 disclosure).
        if params.explanation == Some(true) {
            for h in &mut hits {
                let mut why: Vec<String> = Vec::new();
                if !anchors.is_empty() {
                    if let Some(s) = h.grain.get_str("subject") {
                        if anchors.iter().any(|a| a == s) {
                            why.push(format!("anchored on subject \"{s}\""));
                        }
                    }
                }
                if let Some(q) = &params.query {
                    why.push(format!("matched the free-text query \"{q}\""));
                }
                if let Some(r) = params.relation.as_deref().or_else(|| {
                    params.relation_in.as_ref().and_then(|_| h.grain.get_str("relation"))
                }) {
                    why.push(format!("relation is \"{r}\""));
                }
                if why.is_empty() {
                    why.push(match params.grain_type {
                        Some(gt) => format!("recent {} in this namespace", gt.as_str()),
                        None => "recent grain in this namespace".into(),
                    });
                }
                if h.supersession_status == Some(SupersessionStatus::Superseded) {
                    why.push("included as history by WITH superseded".into());
                }
                h.explanation = Some(why.join("; "));
            }
        }

        Ok(hits)
    }

    fn exists(&self, hash: &Hash) -> Result<bool> {
        if self.authz.is_owner() {
            return self.store.lock().unwrap().has(hash);
        }
        // An existence bit is a read, and a hash names no namespace — so a
        // restricted session resolves the grain first and answers only for
        // namespaces its grants can read.
        let fetched = self.store.lock().unwrap().get(hash);
        match fetched {
            Ok(g) => {
                self.authz
                    .check(Verb::Read, g.get_str("namespace").unwrap_or("shared"))?;
                Ok(true)
            }
            Err(DejaDbError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn get(&self, hash: &Hash) -> Result<DeserializedGrain> {
        let g = self.store.lock().unwrap().get(hash)?;
        self.authz
            .check(Verb::Read, g.get_str("namespace").unwrap_or("shared"))?;
        Ok(g)
    }

    fn count(&self) -> Result<usize> {
        // A store-wide count spans every namespace.
        self.authz.check(Verb::Read, "*")?;
        self.store.lock().unwrap().count()
    }

    fn get_history(&self, namespace: &str, subject: &str, relation: &str) -> Result<Vec<VersionEntry>> {
        self.authz.check(Verb::Read, namespace)?;
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
        // The fork listing spans every namespace.
        self.authz.check(Verb::Read, "*")?;
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
        self.authz.check(Verb::Write, self.write_ns(fields))?;
        let mut m = self.store.lock().unwrap();
        build_grain_from_json(grain_type, fields, AddSink { m: &mut m })
    }

    fn cal_supersede(
        &self,
        old_hash: &Hash,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash> {
        self.authz.check(Verb::Supersede, self.write_ns(fields))?;
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
    /// Capped upstream by `CalExecutorConfig::allow_destructive_ops`; the
    /// session's `delete` grant on the grain's own namespace is the
    /// authorization.
    fn cal_delete(&self, hash: &Hash) -> Result<()> {
        if !self.authz.is_owner() {
            let ns = {
                let g = self.store.lock().unwrap().get(hash)?;
                g.get_str("namespace").unwrap_or("shared").to_string()
            };
            self.authz.check(Verb::Delete, &ns)?;
        }
        self.store.lock().unwrap().forget(hash)
    }
}
