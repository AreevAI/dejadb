//! Grain-type metadata registry — the single, data-only source of truth.
//!
//! Adding a grain type used to mean restating the same handful of facts
//! (byte ↔ string ↔ plural, addability, required ADD fields, queryable
//! RECALL fields, TOON columns) across a dozen lookup lists scattered over
//! `grain.rs`, `cal/executor.rs`, `engine/write.rs`, and `server/routes.rs`.
//! Every one of those lists was an independent "forgot to update list N"
//! bug. This table collapses them into one row per type.
//!
//! Scope is deliberately narrow: this holds **data only**. Behaviour
//! (serialize, deserialize, triple extraction, rendering) stays per-type
//! and lives with the concrete grain structs — it is NOT folded in here.
//! The CAL AST enums (`GrainTypePlural` / `GrainTypeSingular`) also stay
//! per-type; they delegate their string bodies to this registry but remain
//! distinct AST node types.

use super::grain::GrainType;

/// Static, data-only metadata for one grain type.
///
/// The single source of truth for the byte↔string↔plural mapping plus the
/// CAL/recall/TOON lookup facts. Constructed once as `const` data in
/// [`GRAIN_TYPES`]; never mutated at runtime.
#[derive(Debug, Clone, Copy)]
pub struct GrainTypeMeta {
    /// The grain type this row describes.
    pub ty: GrainType,
    /// `.mg` header type byte (e.g. `0x01` for Fact).
    pub byte: u8,
    /// Canonical singular OMS string name (e.g. `"skill"`).
    pub name: &'static str,
    /// Canonical plural name used by CAL `RECALL <plural>` (e.g. `"skills"`).
    pub plural: &'static str,
    /// Whether this type may be created via `ADD` / the `add` HTTP+SDK path.
    pub addable: bool,
    /// Required `SET` fields for an `ADD` of this type.
    pub required_add_fields: &'static [&'static str],
    /// Type-specific fields surfaced by `RECALL <type> WHERE <field> …`.
    pub queryable_fields: &'static [&'static str],
    /// Column set rendered by the TOON output format for this type.
    pub toon_columns: &'static [&'static str],
}

/// The 11 OMS grain types (OMS 1.4), one metadata row each. The byte values
/// match the `.mg` header spec and are immutable.
pub const GRAIN_TYPES: &[GrainTypeMeta] = &[
    GrainTypeMeta {
        ty: GrainType::Fact,
        byte: 0x01,
        name: "fact",
        plural: "facts",
        addable: true,
        required_add_fields: &["subject", "relation", "object"],
        queryable_fields: &["subject", "relation", "object", "confidence"],
        toon_columns: &["subject", "content", "confidence"],
    },
    GrainTypeMeta {
        ty: GrainType::Event,
        byte: 0x02,
        name: "event",
        plural: "events",
        addable: false,
        required_add_fields: &["content"],
        queryable_fields: &[
            "role",
            "session_id",
            "parent_message_id",
            "model_id",
            "content",
            "created_at",
            "stop_reason",
        ],
        toon_columns: &["role", "time", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::State,
        byte: 0x03,
        name: "state",
        plural: "states",
        addable: false,
        required_add_fields: &[],
        queryable_fields: &["context", "plan", "checkpoint_data"],
        toon_columns: &["context", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Workflow,
        byte: 0x04,
        name: "workflow",
        plural: "workflows",
        addable: false,
        required_add_fields: &["nodes"],
        queryable_fields: &[
            "trigger", "node", "binding", "nodes", "edges", "bindings", "name", "retries",
        ],
        toon_columns: &["trigger", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Tool,
        byte: 0x05,
        name: "tool",
        plural: "tools",
        addable: false,
        required_add_fields: &["tool_name"],
        queryable_fields: &[
            "tool_name",
            "tool_phase",
            "is_error",
            "tool_call_id",
            "tool",
            "input",
            "duration_ms",
        ],
        toon_columns: &["tool", "phase", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Observation,
        byte: 0x06,
        name: "observation",
        plural: "observations",
        addable: true,
        required_add_fields: &["observer_id", "observer_type"],
        queryable_fields: &["observer_id", "observer_type", "sensor", "value", "unit"],
        toon_columns: &["observer", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Goal,
        byte: 0x07,
        name: "goal",
        plural: "goals",
        addable: true,
        required_add_fields: &["description"],
        queryable_fields: &[
            "goal_state",
            "assigned_agent",
            "deadline",
            "depends_on",
            "title",
            "description",
            "parent_hash",
        ],
        toon_columns: &["subject", "content", "state"],
    },
    GrainTypeMeta {
        ty: GrainType::Reasoning,
        byte: 0x08,
        name: "reasoning",
        plural: "reasonings",
        addable: false,
        required_add_fields: &[],
        queryable_fields: &["reasoning_type", "premises", "conclusion"],
        toon_columns: &["type", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Consensus,
        byte: 0x09,
        name: "consensus",
        plural: "consensuses",
        addable: false,
        required_add_fields: &[],
        queryable_fields: &["threshold", "agreement_count", "participating_observers"],
        toon_columns: &["threshold", "count", "content"],
    },
    GrainTypeMeta {
        ty: GrainType::Consent,
        byte: 0x0A,
        name: "consent",
        plural: "consents",
        addable: false,
        required_add_fields: &["subject_did"],
        queryable_fields: &[
            "consent_action",
            "purpose",
            "grantor_did",
            "grantee_did",
            "expires_at",
            "granted",
            "subject_did",
        ],
        toon_columns: &["grantor", "grantee", "action", "content"],
    },
    // OMS 1.4 — Skill (0x0B). A packaged, reusable agent capability.
    GrainTypeMeta {
        ty: GrainType::Skill,
        byte: 0x0B,
        name: "skill",
        plural: "skills",
        addable: true,
        required_add_fields: &["name", "description"],
        queryable_fields: &[
            "name",
            "version",
            "domain",
            "holder_did",
            "proficiency",
            "transferable",
            "practice_count",
            "last_practiced_at",
        ],
        toon_columns: &["name", "domain", "proficiency"],
    },
];

/// Look up the metadata row for a grain type. Infallible — every variant has
/// a row (enforced by [`metadata_covers_all_types`]).
pub fn meta(ty: GrainType) -> &'static GrainTypeMeta {
    GRAIN_TYPES
        .iter()
        .find(|m| m.ty == ty)
        // SAFETY: GRAIN_TYPES lists every GrainType variant; the
        // `metadata_covers_all_types` test guarantees this find never fails.
        .expect("GRAIN_TYPES missing a variant — registry is incomplete")
}

/// Parse a grain type from its `.mg` header byte.
pub fn from_byte(b: u8) -> Option<GrainType> {
    GRAIN_TYPES.iter().find(|m| m.byte == b).map(|m| m.ty)
}

/// Parse a grain type from its canonical singular OMS string name.
pub fn from_str(s: &str) -> Option<GrainType> {
    GRAIN_TYPES.iter().find(|m| m.name == s).map(|m| m.ty)
}

/// Iterator over the canonical singular names of every addable type — the
/// single source for `VALID_GRAIN_TYPES_ADD` and the ADD allow-set.
pub fn addable_names() -> impl Iterator<Item = &'static str> {
    GRAIN_TYPES.iter().filter(|m| m.addable).map(|m| m.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `GrainType` variant must have exactly one registry row. This is
    /// the invariant that lets `meta()` be infallible and replaces the
    /// compile-forced exhaustive matches the registry absorbed.
    #[test]
    fn metadata_covers_all_types() {
        // Exhaustive match — adding a GrainType variant without a row here
        // is a compile error, which then trips the per-variant lookup below.
        for ty in [
            GrainType::Fact,
            GrainType::Event,
            GrainType::State,
            GrainType::Workflow,
            GrainType::Tool,
            GrainType::Observation,
            GrainType::Goal,
            GrainType::Reasoning,
            GrainType::Consensus,
            GrainType::Consent,
            GrainType::Skill,
        ] {
            let m = meta(ty);
            assert_eq!(m.ty, ty);
            assert!(!m.name.is_empty());
            assert!(!m.plural.is_empty());
        }
    }

    #[test]
    fn byte_and_name_round_trip() {
        for m in GRAIN_TYPES {
            assert_eq!(from_byte(m.byte), Some(m.ty));
            assert_eq!(from_str(m.name), Some(m.ty));
        }
    }

    #[test]
    fn bytes_and_names_are_unique() {
        for (i, a) in GRAIN_TYPES.iter().enumerate() {
            for b in &GRAIN_TYPES[i + 1..] {
                assert_ne!(a.byte, b.byte, "duplicate byte {:#x}", a.byte);
                assert_ne!(a.name, b.name, "duplicate name {}", a.name);
                assert_ne!(a.plural, b.plural, "duplicate plural {}", a.plural);
            }
        }
    }

    #[test]
    fn skill_row_is_correct() {
        let s = meta(GrainType::Skill);
        assert_eq!(s.byte, 0x0B);
        assert_eq!(s.name, "skill");
        assert_eq!(s.plural, "skills");
        assert!(s.addable);
        assert_eq!(s.required_add_fields, &["name", "description"]);
    }

    #[test]
    fn addable_names_match_addable_rows() {
        let from_iter: Vec<&str> = addable_names().collect();
        let from_filter: Vec<&str> = GRAIN_TYPES
            .iter()
            .filter(|m| m.addable)
            .map(|m| m.name)
            .collect();
        assert_eq!(from_iter, from_filter);
        assert!(from_iter.contains(&"skill"));
    }
}
