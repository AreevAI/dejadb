//! In-file waiser config + state — file-truths persisted through the
//! substrate's `load_state`/`store_state` as one JSON blob. Carries a schema
//! version; unknown keys are ignored (serde default), so an older binary opens
//! a newer file unchanged (proposal §7.3).

use crate::model::Severity;
use crate::recommendation::{MetricSnapshot, RecStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Current persisted-state schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// The whole waiser persisted blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaiserPersisted {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Per-analyzer config, keyed by full analyzer id.
    #[serde(default)]
    pub config: BTreeMap<String, AnalyzerConfig>,
    #[serde(default)]
    pub state: WaiserState,
    /// Rebuildable lifecycle cache: recommendation hash → status.
    #[serde(default)]
    pub status_index: BTreeMap<String, RecStatus>,
    /// Per-recommendation latest audit hash, for hash-chaining.
    #[serde(default)]
    pub audit_heads: BTreeMap<String, String>,
    /// The creating actor per recommendation (for the self-approval block).
    #[serde(default)]
    pub creators: BTreeMap<String, String>,
    /// Rejection cooldowns keyed by dedup_key → cooldown-until epoch-ms.
    #[serde(default)]
    pub cooldowns: BTreeMap<String, i64>,
    /// Applied-recommendation records (inverse plan, metric, timing).
    #[serde(default)]
    pub applied: BTreeMap<String, AppliedRecord>,
    /// Per-recommendation set of horizons (ms after apply) already measured, so
    /// each checkpoint is measured exactly once.
    #[serde(default)]
    pub measured: BTreeMap<String, Vec<i64>>,
    /// Measured outcome time series (the Verify gate's output), keyed by
    /// recommendation — one entry per horizon checkpoint.
    #[serde(default)]
    pub outcomes: BTreeMap<String, Vec<crate::recommendation::OutcomeResult>>,
}

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for WaiserPersisted {
    fn default() -> Self {
        WaiserPersisted {
            schema_version: SCHEMA_VERSION,
            config: BTreeMap::new(),
            state: WaiserState::default(),
            status_index: BTreeMap::new(),
            audit_heads: BTreeMap::new(),
            creators: BTreeMap::new(),
            cooldowns: BTreeMap::new(),
            applied: BTreeMap::new(),
            measured: BTreeMap::new(),
            outcomes: BTreeMap::new(),
        }
    }
}

impl WaiserPersisted {
    /// Decode from the substrate state blob; `Value::Null` (nothing stored) →
    /// defaults.
    pub fn from_value(v: Value) -> crate::error::Result<Self> {
        if v.is_null() {
            return Ok(Self::default());
        }
        serde_json::from_value(v)
            .map_err(|e| crate::error::Error::Internal(format!("decode waiser state: {e}")))
    }

    pub fn to_value(&self) -> crate::error::Result<Value> {
        serde_json::to_value(self)
            .map_err(|e| crate::error::Error::Internal(format!("encode waiser state: {e}")))
    }
}

/// Per-analyzer configuration. The file may enable/disable, raise severity
/// floors, override params, and scope namespaces — never raise engine caps.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnalyzerConfig {
    /// `None` = follow the manifest default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub params: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity_floor: Option<Severity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
}

/// Run state: the watermark that makes repeat runs cheap no-ops.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaiserState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<i64>,
    /// Highest grain `created_at` processed so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark_ms: Option<i64>,
}

/// Record of an applied recommendation: how to undo it and what to re-measure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedRecord {
    pub applied_at_ms: i64,
    pub target_ref: String,
    pub rollbackable: bool,
    /// Grain hashes created by the apply, retracted on rollback (ADD inverse).
    #[serde(default)]
    pub created_hashes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<MetricSnapshot>,
}
