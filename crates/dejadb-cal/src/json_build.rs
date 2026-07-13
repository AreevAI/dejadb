//! JSON → typed grain construction (the engine write path).
//! Used by the CAL facade for ADD / SUPERSEDE tier-1 statements.

use std::collections::BTreeMap;
use dejadb_core::error::{DejaDbError, Result};
use dejadb_core::types::*;

const COMMON_KNOWN_FIELDS: &[&str] = &[
    "namespace",
    "user_id",
    "confidence",
    "source_type",
    "created_at",
    "importance",
    "embedding_text",
    "tags",
    "structural_tags",
    "temporal_type",
    "valid_from",
    "valid_to",
    "system_valid_from",
    "system_valid_to",
    "author_did",
    "origin_did",
    "origin_namespace",
    "derived_from",
    "consolidation_level",
    "success_count",
    "failure_count",
    "superseded_by",
    "verification_status",
    "context",
    "invalidation_policy",
    "content_refs",
    "embedding_refs",
    "provenance_chain",
    "related_to",
    "supersession_justification",
    "supersession_auth",
    "type",
    "grain_type",
];

/// Per-grain-type known fields (type-specific, not in common).
fn type_known_fields(grain_type: &str) -> &'static [&'static str] {
    match grain_type {
        "fact" => &["subject", "relation", "object"],
        "event" => &["content", "subject", "object"],
        "state" => &["data", "context_data"],
        "workflow" => &[
            "name", "nodes", "edges", "bindings", "retries", "trigger", "status",
        ],
        "tool" => &[
            "tool_name",
            "input",
            "content",
            "is_error",
            "error",
            "duration_ms",
            "parent_task_id",
            "output_schema",
        ],
        "observation" => &[
            "content",
            "observer_id",
            "observer_type",
            "subject",
            "object",
            "observer_model",
            "frame_id",
            "sync_group",
            "observation_mode",
            "observation_scope",
            "compression_ratio",
        ],
        "goal" => &[
            "description",
            "goal_state",
            "subject",
            "object",
            "priority",
            "criteria",
            "criteria_structured",
            "parent_goals",
            "state_reason",
            "satisfaction_evidence",
            "progress",
            "delegate_to",
            "delegate_from",
            "expiry_policy",
            "recurrence",
            "evidence_required",
            "rollback_on_failure",
            "allowed_transitions",
        ],
        "reasoning" => &[
            "premises",
            "conclusion",
            "inference_method",
            "alternatives_considered",
            "thinking_content",
            "thinking_redacted",
        ],
        "consensus" => &[
            "participating_observers",
            "threshold",
            "agreement_count",
            "dissent_count",
            "dissent_grains",
            "agreed_content",
        ],
        "consent" => &[
            "subject_did",
            "grantee_did",
            "scope",
            "is_withdrawal",
            "basis",
            "jurisdiction",
            "prior_consent",
            "witness_dids",
        ],
        "skill" => &[
            "name",
            "description",
            "instructions",
            "when_to_use",
            "version",
            "allowed_tools",
            "resources",
            "dependencies",
            "input_modalities",
            "output_modalities",
            "domain",
            "holder_did",
            "proficiency",
            "practice_count",
            "last_practiced_at",
            "strategies",
            "transferable",
        ],
        _ => &[],
    }
}


fn collect_extra_fields(
    fields: &serde_json::Map<String, serde_json::Value>,
    grain_type: &str,
) -> BTreeMap<String, serde_json::Value> {
    let type_known = type_known_fields(grain_type);
    fields
        .iter()
        .filter(|(k, v)| {
            !v.is_null()
                && !COMMON_KNOWN_FIELDS.contains(&k.as_str())
                && !type_known.contains(&k.as_str())
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Build a `Workflow` from JSON fields, parsing nodes, edges, bindings, retries,
/// and trigger. Validates referential integrity of all graph components.
fn build_workflow_from_json(
    fields: &serde_json::Map<String, serde_json::Value>,
    get_str: &dyn Fn(&str) -> Option<String>,
) -> Result<Workflow> {
    // nodes — required, array of strings
    let nodes: Vec<String> = fields
        .get("nodes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Validate: node IDs must be unique
    {
        let mut seen = std::collections::HashSet::with_capacity(nodes.len());
        for n in &nodes {
            if !seen.insert(n.as_str()) {
                return Err(DejaDbError::Validation(format!(
                    "workflow nodes contain duplicate ID: '{}'",
                    n
                )));
            }
        }
    }

    let mut wf = Workflow::new(nodes.clone());

    // edges — optional, array of objects {src, dst, cond?, max_cycles?}
    if let Some(edges_arr) = fields.get("edges").and_then(|v| v.as_array()) {
        for (i, edge_val) in edges_arr.iter().enumerate() {
            let obj = edge_val.as_object().ok_or_else(|| {
                DejaDbError::Validation(format!("workflow edges[{}] must be an object", i))
            })?;
            let src = obj.get("src").and_then(|v| v.as_str()).ok_or_else(|| {
                DejaDbError::Validation(format!("workflow edges[{}] requires string 'src'", i))
            })?;
            let dst = obj.get("dst").and_then(|v| v.as_str()).ok_or_else(|| {
                DejaDbError::Validation(format!("workflow edges[{}] requires string 'dst'", i))
            })?;
            // Validate src/dst reference existing nodes
            if !nodes.iter().any(|n| n == src) {
                return Err(DejaDbError::Validation(format!(
                    "workflow edges[{}].src '{}' does not reference a known node",
                    i, src
                )));
            }
            if !nodes.iter().any(|n| n == dst) {
                return Err(DejaDbError::Validation(format!(
                    "workflow edges[{}].dst '{}' does not reference a known node",
                    i, dst
                )));
            }
            let cond = obj.get("cond").and_then(|v| v.as_str());
            let max_cycles = obj
                .get("max_cycles")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            wf.edges.push(WorkflowEdge {
                src: src.to_string(),
                dst: dst.to_string(),
                cond: cond.map(|s| s.to_string()),
                max_cycles,
            });
        }
    }

    // bindings — optional, object mapping node names to hash strings
    if let Some(bindings_obj) = fields.get("bindings").and_then(|v| v.as_object()) {
        for (key, val) in bindings_obj {
            if !nodes.iter().any(|n| n == key) {
                return Err(DejaDbError::Validation(format!(
                    "workflow bindings key '{}' does not reference a known node",
                    key
                )));
            }
            if let Some(hash) = val.as_str() {
                wf.bindings.insert(key.clone(), hash.to_string());
            }
        }
    }

    // retries — optional, object mapping node names to integers
    if let Some(retries_obj) = fields.get("retries").and_then(|v| v.as_object()) {
        for (key, val) in retries_obj {
            if !nodes.iter().any(|n| n == key) {
                return Err(DejaDbError::Validation(format!(
                    "workflow retries key '{}' does not reference a known node",
                    key
                )));
            }
            if let Some(max) = val.as_u64() {
                wf.retries.insert(key.clone(), max as u32);
            }
        }
    }

    // trigger — optional string
    if let Some(trigger) = get_str("trigger") {
        wf = wf.trigger(&trigger);
    }

    Ok(wf)
}

/// Build a [`Skill`] (OMS 1.4) from a JSON field map. Shared by the `add`
/// (`build_grain_from_json`) and `supersede` (`supersede_from_json`) arms so
/// the per-field mapping exists in exactly one place. Required: `name` +
/// `description`. `proficiency` aliases confidence (D3); an explicitly
/// out-of-range value is rejected. The held-skill `user_id` requirement (BC1)
/// is enforced downstream by the policy gate.
fn build_skill_from_json(
    fields: &serde_json::Map<String, serde_json::Value>,
    get_str: &dyn Fn(&str) -> Option<String>,
    get_f64: &dyn Fn(&str) -> Option<f64>,
    get_i64: &dyn Fn(&str) -> Option<i64>,
) -> Result<Skill> {
    let name = get_str("name")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| DejaDbError::Validation("skill requires non-empty 'name'".into()))?;
    let description = get_str("description")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| DejaDbError::Validation("skill requires non-empty 'description'".into()))?;
    let mut s = Skill::new(&name, &description);
    if let Some(instr) = get_str("instructions") {
        s.instructions = Some(instr);
    }
    if let Some(wtu) = get_str("when_to_use") {
        s.when_to_use = Some(wtu);
    }
    if let Some(v) = get_str("version") {
        s.version = Some(v);
    }
    if let Some(d) = get_str("domain") {
        s.domain = Some(d);
    }
    let str_array = |key: &str| -> Vec<String> {
        fields
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    s.allowed_tools = str_array("allowed_tools");
    s.resources = str_array("resources");
    s.dependencies = str_array("dependencies");
    s.input_modalities = str_array("input_modalities");
    s.output_modalities = str_array("output_modalities");
    if let Some(hdid) = get_str("holder_did") {
        s.holder_did = Some(hdid);
    }
    if let Some(p) = get_f64("proficiency") {
        if !(0.0..=1.0).contains(&p) {
            return Err(DejaDbError::Validation(format!(
                "proficiency must be between 0.0 and 1.0, got {p}"
            )));
        }
        s = s.with_proficiency(p);
    }
    if let Some(pc) = get_i64("practice_count") {
        if pc >= 0 {
            s.practice_count = Some(pc as u32);
        }
    }
    if let Some(lpa) = get_i64("last_practiced_at") {
        s.last_practiced_at = Some(lpa);
    }
    if let Some(strat_val) = fields.get("strategies") {
        s.strategies = serde_json::from_value(strat_val.clone())
            .map_err(|e| DejaDbError::Validation(format!("skill 'strategies' invalid: {e}")))?;
    }
    if let Some(x) = fields.get("transferable").and_then(|v| v.as_bool()) {
        s.transferable = Some(x);
    }
    Ok(s)
}
/// Object-safe-ish sink: what to do with the constructed grain.
pub trait GrainSink {
    type Out;
    fn consume<G: Grain + Clone + 'static>(self, grain: &G) -> Result<Self::Out>;
}

const ADD_JSON_KNOWN_FIELDS: &[&str] = &[
        "grain_type",
        "type",
        "content",
        "subject",
        "relation",
        "object",
        "namespace",
        "user_id",
        "confidence",
        "source_type",
        "created_at",
        "importance",
        "embedding_text",
        "tags",
        "structural_tags",
        "trigger",
        "steps",
        "tool_name",
        "tool_call_id",
        "input",
        "output",
        "is_error",
        "data",
        "context_data",
        "objective",
        "description",
        "observer_id",
        "observer_type",
        "inference_method",
        "threshold",
        "subject_did",
        "grantee_did",
        "purpose",
        "scope",
        "expiry",
        "source_text",
        "reason",
        "supersession_justification",
        "context",
        "derived_from",
    ];

fn collect_context_extras(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    let extras: serde_json::Map<String, serde_json::Value> = fields
        .iter()
        .filter(|(k, v)| !ADD_JSON_KNOWN_FIELDS.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if extras.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(extras))
    }
}

pub fn build_grain_from_json<S: GrainSink>(
        grain_type: &str,
        fields: &serde_json::Map<String, serde_json::Value>,
        sink: S,
    ) -> Result<S::Out> {
        let get_str = |key: &str| -> Option<String> {
            fields
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let require_str = |key: &str, grain: &str| -> Result<String> {
            get_str(key)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    DejaDbError::Validation(format!("{grain} requires non-empty '{key}'"))
                })
        };
        let get_f64 = |key: &str| -> Option<f64> { fields.get(key).and_then(|v| v.as_f64()) };
        let get_i64 = |key: &str| -> Option<i64> { fields.get(key).and_then(|v| v.as_i64()) };

        macro_rules! apply_common {
            ($grain:expr) => {{
                let mut g = $grain;
                if let Some(ns) = get_str("namespace") {
                    g = g.namespace(&ns);
                }
                if let Some(uid) = get_str("user_id") {
                    g = g.user_id(&uid);
                }
                if let Some(c) = get_f64("confidence") {
                    if !(0.0..=1.0).contains(&c) {
                        return Err(DejaDbError::Validation(format!(
                            "confidence must be between 0.0 and 1.0, got {}",
                            c
                        )));
                    }
                    g = g.confidence(c);
                }
                if let Some(st) = get_str("source_type") {
                    g = g.source_type(&st);
                }
                if let Some(ts) = get_i64("created_at") {
                    g = g.created_at(ts);
                }
                if let Some(imp) = get_f64("importance") {
                    g = g.importance(imp);
                }
                if let Some(et) = get_str("embedding_text") {
                    g.common_mut().embedding_text = Some(et);
                }
                // Provenance: route `derived_from` to the first-class common
                // field (mirrors copy_common_fields_from_deserialized) so it
                // serializes top-level and populates provenance_idx — needed
                // by both the add (Add) and supersede (Supersede) applier paths.
                if let Some(df) = get_str("derived_from") {
                    g.common_mut().derived_from = Some(df);
                }
                // DX-P1-2: Also accept "structural_tags" key (used by REST add_grain handler).
                let tag_source = fields.get("tags").or_else(|| fields.get("structural_tags"));
                if let Some(tags) = tag_source.and_then(|v| v.as_array()) {
                    let tag_strs: Vec<String> = tags
                        .iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                    if !tag_strs.is_empty() {
                        g = g.tags(tag_strs);
                    }
                }
                if let Some(extra) = collect_context_extras(fields) {
                    let mut ctx = match g.common().context.clone() {
                        Some(serde_json::Value::Object(m)) => m,
                        _ => serde_json::Map::new(),
                    };
                    if let serde_json::Value::Object(m) = extra {
                        ctx.extend(m);
                    }
                    g.common_mut().context = Some(serde_json::Value::Object(ctx));
                }
                g
            }};
        }

        // Reject null bytes in string fields — they can cause issues with
        // C-based indexing (Tantivy, USearch) and downstream consumers.
        for (key, value) in fields.iter() {
            if let Some(s) = value.as_str() {
                if s.contains('\0') {
                    return Err(DejaDbError::Validation(format!(
                        "field '{}' contains null bytes, which are not permitted in text fields",
                        key
                    )));
                }
            }
        }

        match grain_type {
            "fact" => {
                let subject = require_str("subject", "fact")?;
                let relation = require_str("relation", "fact")?;
                let object = require_str("object", "fact")?;
                let mut grain = apply_common!(Fact::new(&subject, &relation, &object));
                grain.common_mut().extra_fields = collect_extra_fields(fields, "fact");
                sink.consume(&grain)
            }
            "event" => {
                let content = require_str("content", "event")?;
                let mut ev = Event::new(&content);
                if let Some(s) = get_str("subject") { ev = ev.subject(&s); }
                if let Some(o) = get_str("object") { ev = ev.object(&o); }
                let mut grain = apply_common!(ev);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "event");
                sink.consume(&grain)
            }
            "state" => {
                let data = fields.get("data").cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
                let mut grain = apply_common!(State::new(data));
                grain.common_mut().extra_fields = collect_extra_fields(fields, "state");
                sink.consume(&grain)
            }
            "workflow" => {
                let wf = build_workflow_from_json(fields, &get_str)?;
                let mut grain = apply_common!(wf);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "workflow");
                sink.consume(&grain)
            }
            "tool" => {
                let tool_name = require_str("tool_name", "tool")?;
                let mut tool = Tool::new(&tool_name);
                if let Some(inp_val) = fields.get("input") {
                    if let Some(s) = inp_val.as_str() {
                        tool = tool.input_str(s);
                    } else {
                        tool = tool.input(inp_val.clone());
                    }
                }
                if let Some(cnt) = get_str("content") {
                    tool = tool.content(&cnt);
                }
                if let Some(ie) = fields.get("is_error").and_then(|v| v.as_bool()) {
                    tool = tool.is_error(ie);
                }
                if let Some(d) = get_i64("duration_ms") {
                    if d >= 0 {
                        tool = tool.duration_ms(d as u64);
                    }
                }
                let mut grain = apply_common!(tool);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "tool");
                sink.consume(&grain)
            }
            "observation" => {
                // Relaxed validation: observation grains now require `content` as the
                // primary field. `observer_id` and `observer_type` default to "unknown"
                // and "agent" respectively when omitted, since callers like Atmatic
                // typically just need to store an observation with content.
                let content = require_str("content", "observation")?;
                let observer_id = get_str("observer_id").unwrap_or_else(|| "unknown".to_string());
                let observer_type = get_str("observer_type").unwrap_or_else(|| "agent".to_string());
                let mut obs = Observation::new(&observer_id, &observer_type);
                if let Some(s) = get_str("subject") { obs = obs.subject(&s); }
                // Store content in `object` so it persists and is searchable.
                let obj = get_str("object").unwrap_or(content);
                obs = obs.object(&obj);
                let mut grain = apply_common!(obs);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "observation");
                sink.consume(&grain)
            }
            "goal" => {
                let description = get_str("description")
                    .or_else(|| get_str("object"))
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| DejaDbError::Validation("goal requires non-empty 'description' (or 'object' as fallback)".into()))?;
                let mut g = Goal::new(&description);
                if let Some(s) = get_str("subject") { g = g.subject(&s); }
                if let Some(o) = get_str("object") { g = g.object(&o); }
                let mut grain = apply_common!(g);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "goal");
                sink.consume(&grain)
            }
            "reasoning" => {
                let mut r = Reasoning::new();
                if let Some(c) = get_str("conclusion") { r.conclusion = Some(c); }
                if let Some(m) = get_str("inference_method") { r.inference_method = Some(m); }
                let mut grain = apply_common!(r);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "reasoning");
                sink.consume(&grain)
            }
            "consensus" => {
                let mut c = Consensus::new();
                if let Some(po) = fields.get("participating_observers").and_then(|v| v.as_array()) {
                    c.participating_observers = po.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
                if let Some(t) = get_f64("threshold") { c.threshold = Some(t); }
                if let Some(ac) = get_i64("agreement_count") { c.agreement_count = Some(ac); }
                if let Some(dc) = get_i64("dissent_count") { c.dissent_count = Some(dc); }
                if let Some(dg) = fields.get("dissent_grains").and_then(|v| v.as_array()) {
                    c.dissent_grains = dg.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
                if let Some(ac) = get_str("agreed_content") { c.agreed_content = Some(ac); }
                let mut grain = apply_common!(c);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "consensus");
                sink.consume(&grain)
            }
            "consent" => {
                let subject_did = require_str("subject_did", "consent")?;
                // Consent grains must identify the data subject via user_id.
                // This prevents unauthenticated callers from forging consent records without
                // a verifiable subject identity.
                let user_id_val = get_str("user_id");
                match &user_id_val {
                    None => {
                        return Err(DejaDbError::Validation(
                            "consent grains require a non-empty 'user_id' field".into(),
                        ));
                    }
                    Some(uid) if uid.trim().is_empty() => {
                        return Err(DejaDbError::Validation(
                            "consent grains require a non-empty 'user_id' field".into(),
                        ));
                    }
                    _ => {}
                }
                let mut c = Consent::new(&subject_did);
                if let Some(g) = get_str("grantee_did") { c.grantee_did = Some(g); }
                if let Some(s) = get_str("scope") { c.scope = Some(s); }
                if let Some(iw) = fields.get("is_withdrawal").and_then(|v| v.as_bool()) { c.is_withdrawal = Some(iw); }
                if let Some(b) = get_str("basis") { c.basis = Some(b); }
                if let Some(j) = get_str("jurisdiction") { c.jurisdiction = Some(j); }
                let mut grain = apply_common!(c);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "consent");
                sink.consume(&grain)
            }
            "skill" => {
                let s = build_skill_from_json(fields, &get_str, &get_f64, &get_i64)?;
                let mut grain = apply_common!(s);
                grain.common_mut().extra_fields = collect_extra_fields(fields, "skill");
                sink.consume(&grain)
            }
            _ => Err(DejaDbError::Validation(format!(
                "unknown grain type: '{}'. Valid types: fact, event, state, workflow, tool, observation, goal, reasoning, consensus, consent, skill",
                grain_type
            ))),
        }
    }
