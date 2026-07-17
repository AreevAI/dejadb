//! The `OmsSubstrate` trait — the engine's only contact with a store. It is
//! defined in terms of the OMS Level-2 protocol (CAL text ↔ JSON rows, grain
//! get/put/supersede) plus curated typed reads the built-in analyzers use.
//! DejaDB is the first substrate; the in-repo `ReferenceSubstrate` lets engine
//! CI run with zero DejaDB, and doubles as the third-party conformance kit.

use crate::error::Result;
use crate::model::GrainRecord;
use serde_json::{Map, Value};

/// Optional substrate capabilities, declared once and matched against each
/// analyzer manifest's `requires` list. A missing capability degrades an
/// analyzer to an activation-ladder entry, never a silent no-op (§8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Multiple concurrent heads per entity are tracked and queryable
    /// (fork surfacing needs this).
    pub forks: bool,
    /// A telemetry sidecar records recall/access history.
    pub telemetry: bool,
    /// An embedder is installed (upgrades T0 analyzers to T1).
    pub embeddings: bool,
}

/// Read filters for curated grain reads.
#[derive(Debug, Clone, Copy)]
pub struct ReadOpts {
    /// When true (default), only live (non-superseded) grains are returned.
    pub live_only: bool,
    /// When set, only grains created at or after this epoch-ms are returned
    /// (the incremental watermark scan, §8).
    pub since_ms: Option<i64>,
}

impl Default for ReadOpts {
    fn default() -> Self {
        ReadOpts {
            live_only: true,
            since_ms: None,
        }
    }
}

/// A grain to be written by an apply. `derived_from` and other provenance go
/// in `fields`; the substrate computes the content address.
#[derive(Debug, Clone, PartialEq)]
pub struct GrainSpec {
    pub grain_type: String,
    pub namespace: String,
    pub fields: Map<String, Value>,
}

impl GrainSpec {
    pub fn new(grain_type: impl Into<String>, namespace: impl Into<String>) -> Self {
        GrainSpec {
            grain_type: grain_type.into(),
            namespace: namespace.into(),
            fields: Map::new(),
        }
    }

    pub fn with_field(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }
}

/// One entity holding more than one live head (fork surfacing input).
#[derive(Debug, Clone, PartialEq)]
pub struct HeadGroup {
    /// Entity identity, e.g. `"caller/john"` (namespace-qualified subject).
    pub entity: String,
    /// The competing head hashes.
    pub heads: Vec<String>,
}

/// The read-only slice of the substrate. Analyzers receive this (via
/// `AnalyzeCtx`) and nothing else — the trust floor's "analyzers execute
/// read-only" is enforced by the type system: a `&dyn SubstrateRead` cannot
/// reach any mutating method. It is object-safe (no generics) so
/// `builtin_analyzers()` can hand out `Box<dyn Analyzer>`.
pub trait SubstrateRead {
    /// Declared optional capabilities.
    fn capabilities(&self) -> Capabilities;

    /// Curated read: all grains of one OMS type, optionally namespace-scoped
    /// and watermark/liveness filtered.
    fn grains_of_type(
        &self,
        grain_type: &str,
        namespace: Option<&str>,
        opts: ReadOpts,
    ) -> Result<Vec<GrainRecord>>;

    /// Fetch one grain by content address.
    fn grain(&self, hash: &str) -> Result<Option<GrainRecord>>;

    /// Entities with more than one live head. Requires the `forks` capability;
    /// the default impl reports it missing so non-fork substrates degrade
    /// cleanly rather than pretend.
    fn heads(&self, _namespace: Option<&str>) -> Result<Vec<HeadGroup>> {
        Err(crate::error::Error::CapabilityMissing("forks".into()))
    }
}

/// The full store protocol the engine binds to: reads (via the supertrait)
/// plus governed writes, CAL, and state persistence. All methods are fallible;
/// a substrate fault surfaces as [`crate::error::Error::Substrate`].
pub trait OmsSubstrate: SubstrateRead {
    /// Append a new grain; returns its content address.
    fn put_grain(&mut self, spec: &GrainSpec) -> Result<String>;

    /// Supersede `target_hash` with a new grain carrying `justification`;
    /// returns the new grain's address. Atomic and distinct from put
    /// (OMS §28.4).
    fn supersede(
        &mut self,
        target_hash: &str,
        spec: &GrainSpec,
        justification: &str,
    ) -> Result<String>;

    /// Index-layer retraction (`verification_status = retracted`) — the
    /// inverse of an applied ADD, used by rollback. Not destructive (the grain
    /// stays content-addressed; only the index marks it retracted). The
    /// default reports it unsupported so substrates opt in.
    fn retract(&mut self, hash: &str, reason: &str) -> Result<()> {
        Err(crate::error::Error::Substrate(format!(
            "retract not supported by this substrate ({hash}: {reason})"
        )))
    }

    /// Execute CAL text, returning result rows as JSON. Used to regenerate
    /// evidence sets (`evidence_query`) and to apply `proposal_cal`. A
    /// substrate MAY reject CAL it cannot run with [`Error::CalUnsupported`].
    ///
    /// [`Error::CalUnsupported`]: crate::error::Error::CalUnsupported
    fn execute_cal(&mut self, cal: &str) -> Result<Vec<Value>>;

    /// Validate a CAL batch without executing it (statement classification,
    /// destructive-op detection). Delegated to the substrate — the engine
    /// contains a CAL *writer*, never a parser.
    fn validate_cal(&self, cal: &str) -> Result<()>;

    /// Load the persisted waiser state blob (config + watermarks/cooldowns).
    /// Returns `Value::Null` when nothing has been stored yet.
    fn load_state(&self) -> Result<Value>;

    /// Persist the waiser state blob (a file-truth, so it travels with the
    /// file on sync).
    fn store_state(&mut self, state: &Value) -> Result<()>;
}
