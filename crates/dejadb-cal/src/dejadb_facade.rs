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

use crate::facade::CalStoreFacade;
use crate::json_build::{build_grain_from_json, GrainSink};
use crate::store_types::{DiversityMethod, RecallParams, SearchHit, VersionEntry};

/// CalStoreFacade implementation over an embedded `DejaDB` store.
pub struct DejaDbFacade {
    store: Mutex<DejaDB>,
    namespace: Option<String>,
    user: Option<String>,
    /// Read-only mounted memories (org/category replicas): alias → store.
    /// A recall with namespace "alias.inner" routes to the mount (§8).
    mounts: std::collections::HashMap<String, Mutex<DejaDB>>,
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

impl CalStoreFacade for DejaDbFacade {
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
        let raw = if params.subject.is_none() && params.query.is_none() {
            match params.grain_type {
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
