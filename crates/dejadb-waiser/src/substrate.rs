//! `DejaDbSubstrate` — implements `waiser::OmsSubstrate` over `DejaDbFacade`.
//!
//! Two wrapper types share one set of operations (free functions over
//! `&DejaDbFacade`): [`DejaDbSubstrate`] **owns** its facade (the CLI path,
//! which moves a store in), while [`BorrowedSubstrate`] holds `&DejaDbFacade`
//! (the bindings path, where the pyclass/napi object already owns the facade
//! and a second store handle would violate single-writer-per-file). Every
//! operation goes through the facade's `&self` methods (`with_store`,
//! `cal_add`/`cal_supersede`/`cal_delete`), so a shared borrow suffices even
//! for the trait's `&mut self` writers.
//!
//! Two mapping decisions, both interim and documented:
//!
//! 1. **Waiser-internal grains ride as Facts.** The recommendation grain
//!    (OMS 0x0C) is not yet realized in dejadb-core, so recommendation and
//!    audit grains are stored as Facts in the `waiser` namespace with the full
//!    waiser field-map serialized into the Fact's `object`, tagged by
//!    `relation` (`waiser_recommendation` / `waiser_audit`). They are real,
//!    content-addressed, syncable grains; only the type byte differs.
//! 2. **Liveness comes from `derived_from`.** `supersede` stamps the new
//!    grain's `derived_from` with the old hash, so a grain is superseded iff
//!    its hash appears in some sibling's `derived_from`. All-namespace scans
//!    (no ns filter) enumerate via the op-log (`changes_since`), since `recent`
//!    requires a namespace.

use dejadb_cal::{parse, CalExecutor, CalExecutorConfig, CalStoreFacade, DejaDbFacade};
use dejadb_core::error::{DejaDbError, Hash};
use dejadb_core::format::deserialize::DeserializedGrain;
use dejadb_core::types::{Fact, Grain, GrainType};
use dejadb_store::{DejaDB, OP_FORGET};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use waiser::error::{Error as WErr, Result as WResult};
use waiser::{
    Capabilities, GrainRecord, GrainSpec, HeadGroup, OmsSubstrate, ReadOpts, SubstrateRead,
};

/// The namespace waiser's own grains live in (mirrors `waiser::WAISER_NS`).
const WAISER_NS: &str = "waiser";
/// Sentinel subject/relation for the persisted waiser state blob.
const STATE_SUBJECT: &str = "__waiser_state__";
const STATE_RELATION: &str = "state";
/// Upper bound on grains scanned per query (interim; real substrate paginates).
const MAX_SCAN: usize = 1_000_000;

fn we(e: DejaDbError) -> WErr {
    WErr::Substrate(e.to_string())
}

/// A DejaDB-backed substrate that owns its facade (CLI path).
pub struct DejaDbSubstrate {
    facade: DejaDbFacade,
}

impl DejaDbSubstrate {
    /// Wrap an open store, scoping waiser's own writes to `session_ns`
    /// (defaults to the `waiser` namespace when `None`).
    pub fn new(store: DejaDB, session_ns: Option<String>) -> Self {
        let ns = session_ns.unwrap_or_else(|| WAISER_NS.to_string());
        DejaDbSubstrate {
            facade: DejaDbFacade::with_session(store, Some(ns), None),
        }
    }

    /// Recover the underlying store (e.g. to reuse it for another verb).
    pub fn into_store(self) -> DejaDB {
        self.facade.into_inner()
    }
}

/// A DejaDB-backed substrate that borrows a facade (bindings path). Construct
/// one per waiser call over the host object's existing `&DejaDbFacade`.
pub struct BorrowedSubstrate<'a> {
    facade: &'a DejaDbFacade,
}

impl<'a> BorrowedSubstrate<'a> {
    pub fn new(facade: &'a DejaDbFacade) -> Self {
        BorrowedSubstrate { facade }
    }
}

impl DejaDbSubstrate {
    fn facade_ref(&self) -> &DejaDbFacade {
        &self.facade
    }
}

impl BorrowedSubstrate<'_> {
    fn facade_ref(&self) -> &DejaDbFacade {
        self.facade
    }
}

// The two wrappers implement the traits identically by delegating to the free
// functions below (each operation borrows the facade only for its own call).
macro_rules! impl_substrate {
    ($ty:ty) => {
        impl SubstrateRead for $ty {
            fn capabilities(&self) -> Capabilities {
                caps(self.facade_ref())
            }
            fn grains_of_type(
                &self,
                grain_type: &str,
                namespace: Option<&str>,
                opts: ReadOpts,
            ) -> WResult<Vec<GrainRecord>> {
                grains_of_type(self.facade_ref(), grain_type, namespace, opts)
            }
            fn grain(&self, hash: &str) -> WResult<Option<GrainRecord>> {
                grain(self.facade_ref(), hash)
            }
            fn heads(&self, namespace: Option<&str>) -> WResult<Vec<HeadGroup>> {
                heads(self.facade_ref(), namespace)
            }
        }

        impl OmsSubstrate for $ty {
            fn put_grain(&mut self, spec: &GrainSpec) -> WResult<String> {
                put_grain(self.facade_ref(), spec)
            }
            fn supersede(
                &mut self,
                target_hash: &str,
                spec: &GrainSpec,
                _j: &str,
            ) -> WResult<String> {
                supersede_op(self.facade_ref(), target_hash, spec)
            }
            fn retract(&mut self, hash: &str, _reason: &str) -> WResult<()> {
                retract_op(self.facade_ref(), hash)
            }
            fn execute_cal(&mut self, cal: &str) -> WResult<Vec<Value>> {
                execute_cal(self.facade_ref(), cal)
            }
            fn validate_cal(&self, cal: &str) -> WResult<()> {
                validate_cal(cal)
            }
            fn load_state(&self) -> WResult<Value> {
                load_state(self.facade_ref())
            }
            fn store_state(&mut self, state: &Value) -> WResult<()> {
                store_state(self.facade_ref(), state)
            }
        }
    };
}

impl_substrate!(DejaDbSubstrate);
impl_substrate!(BorrowedSubstrate<'_>);

// --- operations (free functions over &DejaDbFacade) ---

fn recent_ns(f: &DejaDbFacade, ns: &str, gt: GrainType) -> WResult<Vec<DeserializedGrain>> {
    f.with_store(|m| m.recent(ns, Some(gt), MAX_SCAN))
        .map_err(we)
}

/// Enumerate every grain of `gt` across all namespaces via the op-log.
fn all_grains(f: &DejaDbFacade, gt: GrainType) -> WResult<Vec<DeserializedGrain>> {
    let ops = f.with_store(|m| m.changes_since(0, MAX_SCAN)).map_err(we)?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for op in ops {
        if op.op == OP_FORGET || !seen.insert(op.hash.to_hex()) {
            continue;
        }
        if let Ok(g) = f.with_store(|m| m.get(&op.hash)) {
            if g.grain_type == gt {
                out.push(g);
            }
        }
    }
    Ok(out)
}

fn read_user_type(
    f: &DejaDbFacade,
    gt: GrainType,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    let raw = match namespace {
        Some(ns) => recent_ns(f, ns, gt)?,
        None => all_grains(f, gt)?,
    };
    let superseded = superseded_set(&raw);
    let mut out = Vec::new();
    for g in raw {
        let ns = grain_namespace(&g);
        // Never surface waiser-internal grains as user data.
        if ns == WAISER_NS {
            continue;
        }
        if opts.live_only && superseded.contains(&g.hash.to_hex()) {
            continue;
        }
        let created = grain_created_ms(&g);
        if opts.since_ms.is_some_and(|s| created < s) {
            continue;
        }
        out.push(map_user_grain(&g, ns, created));
    }
    Ok(out)
}

/// Read waiser-internal grains (recommendations / audit) stored as Facts.
fn read_waiser(
    f: &DejaDbFacade,
    relation: &str,
    out_type: &str,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    let facts = recent_ns(f, WAISER_NS, GrainType::Fact)?;
    let mut out = Vec::new();
    for fact in facts {
        if fact.get_str("relation") != Some(relation) {
            continue;
        }
        let Some(payload) = fact.get_str("object") else {
            continue;
        };
        let fields: Map<String, Value> = match serde_json::from_str(payload) {
            Ok(Value::Object(m)) => m,
            _ => continue,
        };
        let created = grain_created_ms(&fact);
        if opts.since_ms.is_some_and(|s| created < s) {
            continue;
        }
        out.push(waiser_record(
            &fact.hash.to_hex(),
            out_type,
            created,
            fields,
        ));
    }
    Ok(out)
}

fn caps(f: &DejaDbFacade) -> Capabilities {
    Capabilities {
        forks: true,
        telemetry: false,
        embeddings: f.with_store(|m| m.declared_embedding().is_some()),
    }
}

fn grains_of_type(
    f: &DejaDbFacade,
    grain_type: &str,
    namespace: Option<&str>,
    opts: ReadOpts,
) -> WResult<Vec<GrainRecord>> {
    match grain_type {
        "recommendation" => read_waiser(f, "waiser_recommendation", "recommendation", opts),
        "fact" => read_user_type(f, GrainType::Fact, namespace, opts),
        "event" => read_user_type(f, GrainType::Event, namespace, opts),
        "tool" => read_user_type(f, GrainType::Tool, namespace, opts),
        "observation" => read_user_type(f, GrainType::Observation, namespace, opts),
        other => Err(WErr::Substrate(format!("unsupported grain type {other:?}"))),
    }
}

fn grain(f: &DejaDbFacade, hash: &str) -> WResult<Option<GrainRecord>> {
    let h = Hash::from_hex(hash).map_err(we)?;
    let g = match f.with_store(|m| m.get(&h)) {
        Ok(g) => g,
        Err(DejaDbError::NotFound(_)) => return Ok(None),
        Err(e) => return Err(we(e)),
    };
    let created = grain_created_ms(&g);
    // Reconstruct waiser-internal grains from their JSON payload.
    if let Some(rel) = g.get_str("relation") {
        if let Some(out_type) = waiser_relation_type(rel) {
            if let Some(Value::Object(fields)) = g
                .get_str("object")
                .and_then(|p| serde_json::from_str(p).ok())
            {
                return Ok(Some(waiser_record(hash, out_type, created, fields)));
            }
        }
    }
    Ok(Some(map_user_grain(&g, grain_namespace(&g), created)))
}

fn heads(f: &DejaDbFacade, namespace: Option<&str>) -> WResult<Vec<HeadGroup>> {
    let forks = f.with_store(|m| m.open_forks()).map_err(we)?;
    Ok(forks
        .into_iter()
        .filter(|fg| fg.namespace != WAISER_NS)
        .filter(|fg| namespace.is_none_or(|ns| fg.namespace == ns))
        .map(|fg| HeadGroup {
            entity: format!("{}/{}", fg.subject, fg.relation),
            heads: fg.heads.iter().map(|h| h.to_hex()).collect(),
        })
        .collect())
}

fn put_grain(f: &DejaDbFacade, spec: &GrainSpec) -> WResult<String> {
    let payload = serde_json::to_string(&spec.fields)
        .map_err(|e| WErr::Substrate(format!("encode grain: {e}")))?;
    let fact = Fact::new(
        &unique_subject(&payload),
        waiser_relation(&spec.grain_type),
        &payload,
    )
    .namespace(WAISER_NS)
    .confidence(1.0);
    f.with_store(|m| m.add(&fact))
        .map(|h| h.to_hex())
        .map_err(we)
}

fn supersede_op(f: &DejaDbFacade, target_hash: &str, spec: &GrainSpec) -> WResult<String> {
    let payload = serde_json::to_string(&spec.fields)
        .map_err(|e| WErr::Substrate(format!("encode grain: {e}")))?;
    let old = Hash::from_hex(target_hash).map_err(we)?;
    let mut fact = Fact::new(
        &unique_subject(&payload),
        waiser_relation(&spec.grain_type),
        &payload,
    )
    .namespace(WAISER_NS);
    f.with_store(|m| m.supersede(&old, &mut fact))
        .map(|h| h.to_hex())
        .map_err(we)
}

fn retract_op(f: &DejaDbFacade, hash: &str) -> WResult<()> {
    // No index-only retraction primitive exists; the honest mapping for undoing
    // an engine-created ADD is a tombstone of that grain.
    let h = Hash::from_hex(hash).map_err(we)?;
    f.with_store(|m| m.forget(&h)).map_err(we)
}

fn execute_cal(f: &DejaDbFacade, cal: &str) -> WResult<Vec<Value>> {
    // Waiser proposals are a compact store-op form (ADD/SUPERSEDE/FORGET
    // `<type> {json}`), applied via the facade's typed JSON methods — not the
    // CAL text grammar (which uses SET/BECAUSE). Genuine CAL reads
    // (metric/evidence queries) fall through to the real executor.
    let mut rows = Vec::new();
    for line in cal.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        match keyword.to_ascii_uppercase().as_str() {
            "FORGET" => {
                let h = Hash::from_hex(rest.trim()).map_err(we)?;
                f.cal_delete(&h).map_err(we)?;
            }
            "ADD" => {
                let (gtype, fields) = parse_type_and_json(rest)?;
                let h = f.cal_add(&gtype, &fields).map_err(we)?;
                rows.push(serde_json::json!({ "hash": h.to_hex() }));
            }
            "SUPERSEDE" => {
                let (target, after) = rest
                    .split_once(" WITH ")
                    .ok_or_else(|| WErr::CalUnsupported(format!("malformed SUPERSEDE: {line}")))?;
                let (gtype, fields) = parse_type_and_json(after)?;
                let old = Hash::from_hex(target.trim()).map_err(we)?;
                let h = f.cal_supersede(&old, &gtype, &fields).map_err(we)?;
                rows.push(serde_json::json!({ "hash": h.to_hex() }));
            }
            _ => {
                let ex = CalExecutor::new(CalExecutorConfig::default());
                let res = ex
                    .execute(line, f)
                    .map_err(|e| WErr::CalUnsupported(e.to_string()))?;
                let payload = serde_json::to_value(&res.result)
                    .map_err(|e| WErr::Substrate(format!("encode CAL result: {e}")))?;
                let mut hs = Vec::new();
                collect_hashes(&payload, &mut hs);
                rows.extend(hs.into_iter().map(|h| serde_json::json!({ "hash": h })));
            }
        }
    }
    Ok(rows)
}

fn validate_cal(cal: &str) -> WResult<()> {
    for line in cal.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (keyword, _) = split_keyword(line);
        match keyword.to_ascii_uppercase().as_str() {
            "ADD" | "SUPERSEDE" | "FORGET" => {}
            _ => {
                parse(line)
                    .map(|_| ())
                    .map_err(|e| WErr::CalUnsupported(e.to_string()))?;
            }
        }
    }
    Ok(())
}

fn load_state(f: &DejaDbFacade) -> WResult<Value> {
    let head = f
        .with_store(|m| m.latest(WAISER_NS, STATE_SUBJECT, STATE_RELATION))
        .map_err(we)?;
    match head.as_ref().and_then(|g| g.get_str("object")) {
        Some(json) => serde_json::from_str(json)
            .map_err(|e| WErr::Substrate(format!("decode waiser state: {e}"))),
        None => Ok(Value::Null),
    }
}

fn store_state(f: &DejaDbFacade, state: &Value) -> WResult<()> {
    let json = serde_json::to_string(state)
        .map_err(|e| WErr::Substrate(format!("encode waiser state: {e}")))?;
    let existing = f
        .with_store(|m| m.latest(WAISER_NS, STATE_SUBJECT, STATE_RELATION))
        .map_err(we)?;
    f.with_store(|m| {
        let mut fact = Fact::new(STATE_SUBJECT, STATE_RELATION, &json).namespace(WAISER_NS);
        match &existing {
            Some(g) => m.supersede(&g.hash, &mut fact).map(|_| ()),
            None => m.add(&fact).map(|_| ()),
        }
    })
    .map_err(we)
}

// --- pure helpers ---

fn waiser_relation(grain_type: &str) -> &'static str {
    match grain_type {
        "recommendation" => "waiser_recommendation",
        _ => "waiser_audit", // the engine only puts recommendation + audit(observation) grains
    }
}

fn waiser_relation_type(relation: &str) -> Option<&'static str> {
    match relation {
        "waiser_recommendation" => Some("recommendation"),
        "waiser_audit" => Some("observation"),
        _ => None,
    }
}

/// Deterministic, collision-safe subject so distinct waiser grains never share
/// a `(subject, relation)` head (which would look like a fork).
fn unique_subject(payload: &str) -> String {
    use std::hash::{Hash as _, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    payload.hash(&mut h);
    format!("w{:016x}", h.finish())
}

fn grain_namespace(g: &DeserializedGrain) -> String {
    g.get_str("namespace").unwrap_or("").to_string()
}

fn grain_created_ms(g: &DeserializedGrain) -> i64 {
    g.get_i64("created_at")
        .unwrap_or_else(|| g.header.created_at_sec as i64 * 1000)
}

fn map_user_grain(g: &DeserializedGrain, namespace: String, created_ms: i64) -> GrainRecord {
    GrainRecord {
        hash: g.hash.to_hex(),
        grain_type: g.grain_type.as_str().to_string(),
        namespace,
        created_at_ms: created_ms,
        valid_to_ms: g.get_i64("valid_to"),
        superseded_by: None,
        fields: g.fields.clone().into_iter().collect(),
    }
}

fn waiser_record(
    hash: &str,
    grain_type: &str,
    created_ms: i64,
    fields: Map<String, Value>,
) -> GrainRecord {
    let valid_to_ms = fields.get("valid_to_ms").and_then(Value::as_i64);
    GrainRecord {
        hash: hash.to_string(),
        grain_type: grain_type.to_string(),
        namespace: WAISER_NS.to_string(),
        created_at_ms: created_ms,
        valid_to_ms,
        superseded_by: None,
        fields,
    }
}

/// Hashes that some sibling grain supersedes (via `derived_from`).
fn superseded_set(grains: &[DeserializedGrain]) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for g in grains {
        for parent in derived_parents(g) {
            set.insert(parent);
        }
    }
    set
}

fn derived_parents(g: &DeserializedGrain) -> Vec<String> {
    match g.fields.get("derived_from") {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn split_keyword(line: &str) -> (&str, &str) {
    match line.split_once(char::is_whitespace) {
        Some((k, rest)) => (k, rest.trim_start()),
        None => (line, ""),
    }
}

/// Parse `<type> {json}` → (type, fields).
fn parse_type_and_json(s: &str) -> WResult<(String, Map<String, Value>)> {
    let brace = s
        .find('{')
        .ok_or_else(|| WErr::CalUnsupported(format!("missing JSON object in {s:?}")))?;
    let gtype = s[..brace].trim().to_string();
    if gtype.is_empty() {
        return Err(WErr::CalUnsupported(format!("missing grain type in {s:?}")));
    }
    let value: Value = serde_json::from_str(s[brace..].trim())
        .map_err(|e| WErr::CalUnsupported(format!("bad JSON in {s:?}: {e}")))?;
    match value {
        Value::Object(m) => Ok((gtype, m)),
        _ => Err(WErr::CalUnsupported(format!("JSON not an object in {s:?}"))),
    }
}

fn collect_hashes(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if (k == "hash" || k == "new_hash") && val.is_string() {
                    out.push(val.as_str().unwrap().to_string());
                } else {
                    collect_hashes(val, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|x| collect_hashes(x, out)),
        _ => {}
    }
}
