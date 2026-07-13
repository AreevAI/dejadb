//! CalStoreFacade — constrained store interface for CAL query execution.
//!
//! The facade is a **trait** that limits what the executor can access. The
//! executor receives a `&dyn CalStoreFacade`, never a `&DejaDB` directly. This
//! provides a structural safety guarantee: destructive operations are gated by
//! executor config flags, not exposed unconditionally.
//!
//! # Security (S-2)
//!
//! ADD, SUPERSEDE, and ACCUMULATE are available on this trait and enabled
//! by default (`CalExecutorConfig::tier1_enabled = true`). They can be
//! disabled by setting `tier1_enabled = false`. REVERT remains **absent** —
//! its semantics are not yet defined.
//!
//! FORGET is available on this trait, gated by
//! `CalExecutorConfig::allow_destructive_ops` in the executor (**enabled by
//! default**; set it to `false` for a read-only session). `FORGET <hash>`
//! removes a single grain (a tombstone via `DejaDB::forget`). The
//! `cal_forget_user`/`cal_forget_scope` methods are unimplemented stubs —
//! there is no user/scope crypto-erasure primitive in the store yet.
//!
//! # Object safety
//!
//! The trait is designed to be object-safe — `&dyn CalStoreFacade` must
//! compile. All methods take `&self` and use concrete types only (no generics
//! or associated types).

use crate::store_types::{RecallParams, SearchHit};
use crate::store_types::VersionEntry;
use dejadb_core::error::{Hash, Result};
use dejadb_core::format::deserialize::DeserializedGrain;

// ---------------------------------------------------------------------------
// RerankType — which reranker to use for post-merge ASSEMBLE reranking
// ---------------------------------------------------------------------------

/// Reranker type selector for `CalStoreFacade::rerank_passages()`.
#[derive(Debug, Clone)]
pub enum RerankType {
    /// BERT cross-encoder reranking (feature: `rerank`).
    CrossEncoder,
    /// LLM listwise reranking via external backend (feature: `llm-rerank`).
    Llm,
}

// ---------------------------------------------------------------------------
// CalStoreFacade trait
// ---------------------------------------------------------------------------

/// Constrained store interface for CAL query execution.
///
/// Exposes read operations, Tier 1 write operations (ADD, SUPERSEDE), and
/// Tier 2 destructive operations (FORGET). Destructive operations are gated
/// by `CalExecutorConfig::allow_destructive_ops` — permitted by default, but
/// the operator can set it to `false` to make a session read-only.
pub trait CalStoreFacade: Send + Sync {
    /// Execute a recall query. Maps to `DejaDB::recall()`.
    fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>>;

    /// Check if a grain exists by hash. Maps to `DejaDB::has()`.
    fn exists(&self, hash: &Hash) -> Result<bool>;

    /// Retrieve a single grain by hash. Maps to `DejaDB::get()`.
    fn get(&self, hash: &Hash) -> Result<DeserializedGrain>;

    /// Count total grains. Maps to `DejaDB::count()`.
    fn count(&self) -> Result<usize>;

    /// Get version history for a `(namespace, subject, relation)` triple.
    fn get_history(
        &self,
        namespace: &str,
        subject: &str,
        relation: &str,
    ) -> Result<Vec<VersionEntry>>;

    /// Return the default namespace for this session, if any.
    ///
    /// Set by the auth/capability token layer (capability-scoped namespace
    /// restricts which namespace a client may query). The executor consults
    /// this value when no explicit namespace is provided in the CAL query.
    fn default_namespace(&self) -> Option<&str>;

    /// Return the active user ID for this session, if any.
    ///
    /// Injected from the auth/capability token. When present, the executor
    /// uses it to populate `RecallParams::user_id` for all queries, ensuring
    /// a tenant cannot cross-query another tenant's data.
    fn active_user(&self) -> Option<&str>;

    // ── Tier 1 write operations (ADD, SUPERSEDE) ────────────────────────

    /// Add a grain from a type name and JSON fields. Maps to `DejaDB::add_from_json()`.
    fn cal_add(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash>;

    /// Add a grain with per-call options. Maps to `DejaDB::add_from_json_with_options()`.
    fn cal_add_with_options(
        &self,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
        options: crate::store_types::AddOptions,
    ) -> Result<crate::store_types::AddResult> {
        // Default: fall back to basic cal_add (ignoring options).
        let _ = options;
        self.cal_add(grain_type, fields)
            .map(crate::store_types::AddResult::plain)
    }

    /// Supersede a grain from a type name and JSON fields. Maps to `DejaDB::supersede_from_json()`.
    fn cal_supersede(
        &self,
        old_hash: &Hash,
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Hash>;

    /// Accumulate delta updates into the current tip of a grain's supersession chain.
    ///
    /// Resolves the target grain (by hash or entity_latest), applies ADD deltas
    /// (numeric addition) and SET replacements, creates a new grain, and supersedes
    /// the old one — all under the per-shard supersede lock.
    fn cal_accumulate(
        &self,
        grain_type: &str,
        target: &super::ast::AccumulateTarget,
        add_ops: &[(String, f64)],
        set_ops: &serde_json::Map<String, serde_json::Value>,
        reason: &str,
    ) -> Result<AccumulateResult> {
        let _ = (grain_type, target, add_ops, set_ops, reason);
        Err(dejadb_core::error::DejaDbError::Internal(
            "accumulate not available".into(),
        ))
    }

    // ── Phase 2 additions (default implementations) ──────────────────────

    /// Return CAL capabilities and conformance level information.
    fn describe_capabilities(&self) -> CalCapabilities {
        CalCapabilities::default()
    }

    /// Return information about all supported grain types.
    fn describe_grain_types(&self) -> Vec<GrainTypeInfo> {
        Vec::new()
    }

    /// Return field information, optionally scoped to a specific grain type.
    fn describe_fields(&self, _grain_type: Option<dejadb_core::types::GrainType>) -> Vec<FieldInfo> {
        Vec::new()
    }

    // ── Template management (FR-003) ─────────────────────────────────────

    /// Register a custom template (persists to store).
    fn define_template(
        &self,
        _name: &str,
        _source: &str,
        _description: Option<&str>,
        _parent: Option<&str>,
        _grain_types: &[String],
    ) -> Result<()> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "template management not available".into(),
        ))
    }

    /// Drop a custom template (removes from store).
    fn drop_template(&self, _name: &str) -> Result<()> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "template management not available".into(),
        ))
    }

    // ── Reranking (post-merge ASSEMBLE) ──────────────────────────────────

    /// Rerank text passages against a query using the specified reranker.
    ///
    /// Returns a permutation vector: `result[i]` is the original index of the
    /// passage that should appear at position `i` in the reranked output.
    ///
    /// Default implementation returns identity permutation (no reranking).
    /// The `DejaDB` implementation delegates to the cross-encoder or LLM
    /// reranker registry, enforcing HIPAA/erasure/audit guards internally.
    fn rerank_passages(
        &self,
        _query: &str,
        _passages: &[&str],
        _rerank_type: RerankType,
        _model: Option<&str>,
        _user_id: Option<&str>,
    ) -> Result<Vec<usize>> {
        Ok((0.._passages.len()).collect())
    }

    // ── Tier 2 destructive operations (DELETE, FORGET) ────────────────────

    /// Delete a single grain by hash. Maps to `DejaDB::forget()`.
    fn cal_delete(&self, _hash: &Hash) -> Result<()> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// Crypto-erase all data for a user. Maps to `DejaDB::forget_user()`.
    fn cal_forget_user(&self, _user_id: &str) -> Result<crate::store_types::ErasureProof> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// Crypto-erase all data in a scope. Maps to `DejaDB::forget_scope()`.
    fn cal_forget_scope(&self, _scope: &str) -> Result<crate::store_types::ErasureProof> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// Purge stale grains using decay curve. Maps to `DejaDB::forget_stale()`.
    fn cal_purge_stale(
        &self,
        _min_age_days: f64,
        _namespace: Option<&str>,
        _batch_limit: usize,
    ) -> Result<usize> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "destructive operations not available".into(),
        ))
    }

    /// List all templates (built-in + custom).
    fn list_templates(&self) -> Vec<TemplateInfo> {
        Vec::new()
    }

    /// Get a single template by name.
    fn get_template(&self, _name: &str) -> Option<TemplateInfo> {
        None
    }

    /// Record that a template was used in a FORMAT preset render.
    fn record_template_run(&self, _name: &str) {}

    // ── Saved query management ────────────────────────────────────────────

    /// Register a saved query (persists to store).
    fn define_query(
        &self,
        _name: &str,
        _body: &str,
        _description: Option<&str>,
        _params: &[crate::ast::QueryParam],
    ) -> Result<()> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "saved query management not available".into(),
        ))
    }

    /// Drop a saved query (removes from store).
    fn drop_query(&self, _name: &str) -> Result<()> {
        Err(dejadb_core::error::DejaDbError::Internal(
            "saved query management not available".into(),
        ))
    }

    /// List all saved queries.
    fn list_queries(&self) -> Vec<crate::queries::QueryListEntry> {
        Vec::new()
    }

    /// Get a single saved query by name.
    fn get_query(&self, _name: &str) -> Option<crate::queries::QueryEntry> {
        None
    }

    /// Record `last_run_at` timestamp for a saved query after successful RUN execution.
    /// Default: no-op (for mock/test stores that don't persist).
    fn update_query_last_run(&self, _name: &str) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AccumulateResult
// ---------------------------------------------------------------------------

/// Result of an ACCUMULATE operation.
#[derive(Debug, Clone)]
pub struct AccumulateResult {
    /// Hash of the old (now-superseded) grain.
    pub old_hash: Hash,
    /// Hash of the new grain with deltas applied.
    pub new_hash: Hash,
    /// The delta operations that were applied (for audit): (field, old_value, new_value).
    pub applied_deltas: Vec<(String, f64, f64)>,
}

// ---------------------------------------------------------------------------
// Phase 2 facade types
// ---------------------------------------------------------------------------

/// Re-export for convenience.
pub use super::templates::TemplateListEntry as TemplateInfo;

/// CAL engine capabilities and conformance level.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CalCapabilities {
    /// CAL specification version (e.g. 1).
    pub cal_version: u8,
    /// Conformance level (1 = Core, 2 = Extended, 3 = Full).
    pub conformance_level: u8,
    /// List of supported statement types.
    pub supported_statements: Vec<String>,
    /// Maximum number of named sources in a multi-source ASSEMBLE.
    pub max_sources: u8,
    /// Maximum number of LET bindings in a single query.
    pub max_let_bindings: u8,
    /// Maximum token budget for ASSEMBLE BUDGET clause.
    pub max_budget_tokens: u32,
}

impl Default for CalCapabilities {
    fn default() -> Self {
        Self {
            cal_version: 1,
            conformance_level: 2,
            supported_statements: vec![
                "RECALL".into(),
                "EXISTS".into(),
                "ASSEMBLE".into(),
                "HISTORY".into(),
                "EXPLAIN".into(),
                "DESCRIBE".into(),
                "BATCH".into(),
                "COALESCE".into(),
                "ADD".into(),
                "SUPERSEDE".into(),
                "ACCUMULATE".into(),
                "REVERT".into(),
                "FORGET".into(),
                "PURGE".into(),
                "DROP".into(),
            ],
            max_sources: 8,
            max_let_bindings: 5,
            max_budget_tokens: 100_000,
        }
    }
}

/// Metadata about a grain type (for DESCRIBE SCHEMA / DESCRIBE grain_types).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GrainTypeInfo {
    /// Singular name (e.g. "fact").
    pub name: String,
    /// Plural name (e.g. "facts").
    pub plural: String,
    /// Type-specific fields beyond the common set.
    pub specific_fields: Vec<String>,
}

/// Metadata about a queryable field (for DESCRIBE FIELDS).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FieldInfo {
    /// Field name (e.g. "subject", "confidence").
    pub name: String,
    /// Field type description (e.g. "string", "number", "timestamp").
    pub field_type: String,
    /// Whether this field can appear in WHERE clauses.
    pub filterable: bool,
    /// Whether this field can appear in ORDER BY clauses.
    pub sortable: bool,
}

// ---------------------------------------------------------------------------
// Blanket implementation for DejaDB
// ---------------------------------------------------------------------------

// Blanket implementation of `CalStoreFacade` for the `DejaDB` engine.
//
// Wraps each DejaDB method directly. The `default_namespace` and
// `active_user` helpers return values from the engine's own session state.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dejadb_core::error::DejaDbError;

    // -----------------------------------------------------------------------
    // Minimal mock implementation of CalStoreFacade
    // -----------------------------------------------------------------------

    /// A mock store that holds an in-memory list of (hash, grain) pairs.
    struct MockStore {
        grains: Vec<(Hash, DeserializedGrain)>,
        default_ns: Option<String>,
        active_user: Option<String>,
    }

    impl MockStore {
        fn empty() -> Self {
            Self {
                grains: Vec::new(),
                default_ns: None,
                active_user: None,
            }
        }

        fn with_namespace(ns: &str) -> Self {
            Self {
                grains: Vec::new(),
                default_ns: Some(ns.to_string()),
                active_user: None,
            }
        }

        fn with_user(user: &str) -> Self {
            Self {
                grains: Vec::new(),
                default_ns: None,
                active_user: Some(user.to_string()),
            }
        }
    }

    fn make_grain(subject: &str) -> DeserializedGrain {
        use dejadb_core::format::header::MgHeader;
        use dejadb_core::types::GrainType;
        use std::collections::HashMap;

        let mut fields = HashMap::new();
        fields.insert(
            "subject".to_string(),
            serde_json::Value::String(subject.to_string()),
        );
        fields.insert(
            "grain_type".to_string(),
            serde_json::Value::String("fact".to_string()),
        );

        // Construct a minimal hash from the subject bytes.
        let mut hash_bytes = [0u8; 32];
        for (i, b) in subject.as_bytes().iter().enumerate().take(32) {
            hash_bytes[i] = *b;
        }
        let hash = Hash::from_bytes(&hash_bytes);

        DeserializedGrain {
            header: MgHeader {
                version: 1,
                flags: 0,
                grain_type: 0x01, // Fact
                ns_hash: 0,
                created_at_sec: 0,
            },
            grain_type: GrainType::Fact,
            fields,
            hash,
        }
    }

    impl CalStoreFacade for MockStore {
        fn recall(&self, params: &RecallParams) -> Result<Vec<SearchHit>> {
            let mut hits: Vec<SearchHit> = self
                .grains
                .iter()
                .filter(|(_, g)| {
                    if let Some(ref s) = params.subject {
                        if g.get_str("subject") != Some(s.as_str()) {
                            return false;
                        }
                    }
                    true
                })
                .map(|(hash, grain)| SearchHit {
                    grain: grain.clone(),
                    score: 1.0,
                    hash: *hash,
                    score_breakdown: None,
                    explanation: None,
                    scope_depth: None,
                    source_namespace: None,
                    #[cfg(feature = "rerank")]
                    rerank_score: None,
                    #[cfg(feature = "llm-rerank")]
                    llm_rerank_score: None,
                    relative_time: None,
                    conflict_status: None,
                    supersession_status: None,
                    superseded_by_hash: None,
                    recall_source: None,
                })
                .collect();
            if let Some(limit) = params.limit {
                hits.truncate(limit);
            }
            Ok(hits)
        }

        fn exists(&self, hash: &Hash) -> Result<bool> {
            Ok(self.grains.iter().any(|(h, _)| h == hash))
        }

        fn get(&self, hash: &Hash) -> Result<DeserializedGrain> {
            self.grains
                .iter()
                .find(|(h, _)| h == hash)
                .map(|(_, g)| g.clone())
                .ok_or(DejaDbError::NotFound(*hash))
        }

        fn count(&self) -> Result<usize> {
            Ok(self.grains.len())
        }

        fn get_history(
            &self,
            _namespace: &str,
            _subject: &str,
            _relation: &str,
        ) -> Result<Vec<VersionEntry>> {
            Ok(Vec::new())
        }

        fn cal_add(
            &self,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<Hash> {
            Err(DejaDbError::Validation(
                "mock: cal_add not implemented".into(),
            ))
        }

        fn cal_supersede(
            &self,
            _old_hash: &Hash,
            _grain_type: &str,
            _fields: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<Hash> {
            Err(DejaDbError::Validation(
                "mock: cal_supersede not implemented".into(),
            ))
        }

        fn default_namespace(&self) -> Option<&str> {
            self.default_ns.as_deref()
        }

        fn active_user(&self) -> Option<&str> {
            self.active_user.as_deref()
        }

        fn list_templates(&self) -> Vec<TemplateInfo> {
            // Return built-in templates from a default registry so DESCRIBE TEMPLATES works.
            let registry = crate::templates::TemplateRegistry::new();
            registry.list()
        }
    }

    // -----------------------------------------------------------------------
    // Object-safety test: &dyn CalStoreFacade must compile
    // -----------------------------------------------------------------------

    #[test]
    fn test_trait_is_object_safe() {
        let store = MockStore::empty();
        // This assignment verifies that CalStoreFacade is object-safe.
        let _facade: &dyn CalStoreFacade = &store;
    }

    #[test]
    fn test_mock_count_empty() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.count().unwrap(), 0);
    }

    #[test]
    fn test_mock_exists_not_found() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let hash = Hash::from_bytes(&[0u8; 32]);
        assert!(!facade.exists(&hash).unwrap());
    }

    #[test]
    fn test_mock_get_not_found() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let hash = Hash::from_bytes(&[0u8; 32]);
        let result = facade.get(&hash);
        assert!(result.is_err());
        match result.unwrap_err() {
            DejaDbError::NotFound(_) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn test_mock_recall_empty_store() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams::default();
        let hits = facade.recall(&params).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_mock_default_namespace_none() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.default_namespace(), None);
    }

    #[test]
    fn test_mock_default_namespace_some() {
        let store = MockStore::with_namespace("acme");
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.default_namespace(), Some("acme"));
    }

    #[test]
    fn test_mock_active_user_none() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.active_user(), None);
    }

    #[test]
    fn test_mock_active_user_some() {
        let store = MockStore::with_user("john");
        let facade: &dyn CalStoreFacade = &store;
        assert_eq!(facade.active_user(), Some("john"));
    }

    #[test]
    fn test_mock_get_history_empty() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let history = facade.get_history("ns", "john", "likes").unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_facade_can_be_boxed() {
        // Ensures the trait is usable as Box<dyn CalStoreFacade> (Send + Sync).
        let store = MockStore::empty();
        let _boxed: Box<dyn CalStoreFacade> = Box::new(store);
    }

    #[test]
    fn test_facade_recall_with_subject_filter() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams {
            subject: Some("john".to_string()),
            ..Default::default()
        };
        let hits = facade.recall(&params).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].hash, hash);
    }

    #[test]
    fn test_facade_recall_subject_filter_no_match() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let params = RecallParams {
            subject: Some("bob".to_string()),
            ..Default::default()
        };
        let hits = facade.recall(&params).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn test_facade_exists_known_hash() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        assert!(facade.exists(&hash).unwrap());
    }

    #[test]
    fn test_facade_get_known_hash() {
        let grain = make_grain("john");
        let hash = grain.hash;
        let mut store = MockStore::empty();
        store.grains.push((hash, grain));

        let facade: &dyn CalStoreFacade = &store;
        let retrieved = facade.get(&hash).unwrap();
        assert_eq!(retrieved.get_str("subject"), Some("john"));
    }

    // -----------------------------------------------------------------------
    // Phase 2 facade method tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_facade_describe_capabilities_default() {
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let caps = facade.describe_capabilities();
        assert_eq!(caps.cal_version, 1);
        assert_eq!(caps.conformance_level, 2);
        assert!(!caps.supported_statements.is_empty());
        assert!(caps.supported_statements.contains(&"RECALL".to_string()));
        assert!(caps.supported_statements.contains(&"ASSEMBLE".to_string()));
        assert!(caps.supported_statements.contains(&"COALESCE".to_string()));
        assert_eq!(caps.max_sources, 8);
        assert_eq!(caps.max_let_bindings, 5);
        assert_eq!(caps.max_budget_tokens, 100_000);
    }

    #[test]
    fn test_facade_describe_grain_types_default_is_empty() {
        // Default trait impl returns empty — real implementations override.
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let types = facade.describe_grain_types();
        assert!(types.is_empty(), "default impl should return empty");
    }

    #[test]
    fn test_facade_describe_fields_default_is_empty() {
        // Default trait impl returns empty — real implementations override.
        let store = MockStore::empty();
        let facade: &dyn CalStoreFacade = &store;
        let fields = facade.describe_fields(None);
        assert!(fields.is_empty(), "default impl should return empty");
    }

    #[test]
    fn test_cal_capabilities_default_values() {
        let caps = CalCapabilities::default();
        assert_eq!(caps.cal_version, 1);
        assert_eq!(caps.conformance_level, 2);
        assert_eq!(caps.supported_statements.len(), 15);
        assert!(caps.supported_statements.contains(&"RECALL".to_string()));
        assert!(caps.supported_statements.contains(&"EXISTS".to_string()));
        assert!(caps.supported_statements.contains(&"ASSEMBLE".to_string()));
        assert!(caps.supported_statements.contains(&"HISTORY".to_string()));
        assert!(caps.supported_statements.contains(&"EXPLAIN".to_string()));
        assert!(caps.supported_statements.contains(&"DESCRIBE".to_string()));
        assert!(caps.supported_statements.contains(&"BATCH".to_string()));
        assert!(caps.supported_statements.contains(&"COALESCE".to_string()));
        assert!(caps.supported_statements.contains(&"ADD".to_string()));
        assert!(caps.supported_statements.contains(&"SUPERSEDE".to_string()));
        assert!(caps.supported_statements.contains(&"REVERT".to_string()));
        assert!(caps.supported_statements.contains(&"FORGET".to_string()));
        assert!(caps.supported_statements.contains(&"PURGE".to_string()));
        assert!(caps.supported_statements.contains(&"DROP".to_string()));
        assert_eq!(caps.max_sources, 8);
        assert_eq!(caps.max_let_bindings, 5);
        assert_eq!(caps.max_budget_tokens, 100_000);
    }

    #[test]
    fn test_grain_type_info_struct() {
        let info = GrainTypeInfo {
            name: "fact".to_string(),
            plural: "facts".to_string(),
            specific_fields: vec!["subject".to_string(), "relation".to_string()],
        };
        assert_eq!(info.name, "fact");
        assert_eq!(info.plural, "facts");
        assert_eq!(info.specific_fields.len(), 2);
    }

    #[test]
    fn test_field_info_struct() {
        let info = FieldInfo {
            name: "subject".to_string(),
            field_type: "string".to_string(),
            filterable: true,
            sortable: true,
        };
        assert_eq!(info.name, "subject");
        assert!(info.filterable);
        assert!(info.sortable);
    }
}
