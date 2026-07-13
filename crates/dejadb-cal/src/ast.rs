//! CAL Abstract Syntax Tree types.
//!
//! All 12 statement variants are defined here, even though Phase 1 only
//! executes RECALL and EXISTS.  The parser needs to recognise every variant
//! so it can produce meaningful error messages for unsupported statements.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::errors::Span;

// ---------------------------------------------------------------------------
// Version prefix
// ---------------------------------------------------------------------------

/// The `CAL/<n>` version prefix on a query (e.g. `CAL/1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalVersion(pub u32);

impl Default for CalVersion {
    fn default() -> Self {
        Self(1)
    }
}

// ---------------------------------------------------------------------------
// Top-level query
// ---------------------------------------------------------------------------

/// A fully parsed CAL query — version prefix + statement + optional pipeline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalQuery {
    /// Explicit version prefix, if present. Defaults to `CAL/1`.
    #[serde(default)]
    pub version: CalVersion,

    /// The core statement.
    pub statement: CalStatement,

    /// Pipeline stages applied after the statement (`| SELECT ...`, etc.).
    pub pipeline: Vec<PipelineStage>,

    /// `WITH` options (e.g. `WITH superseded`, `WITH score_breakdown`).
    pub with_options: Vec<WithOption>,

    /// `FORMAT` spec (e.g. `FORMAT json`, `FORMAT [markdown, json]`).
    pub format: Option<FormatClause>,

    /// `LET` bindings extracted before the statement (e.g.
    /// `LET $x = SUBJECTS OF (...)`).
    pub let_bindings: Vec<LetBinding>,

    /// `WITH VARS { "key": "value", ... }` — user-injected display variables.
    ///
    /// These are string-only values accessible in FORMAT TEMPLATE via `{{$key}}`
    /// syntax. They do NOT affect query execution — display only.
    #[serde(default)]
    pub user_vars: HashMap<String, String>,

    /// Warnings emitted during parsing (non-fatal).
    #[serde(skip)]
    pub warnings: Vec<super::errors::CalWarning>,
}

// ---------------------------------------------------------------------------
// Statements (22 variants)
// ---------------------------------------------------------------------------

/// The 22 CAL statement types.
///
/// Phase 1 (Core conformance) executes `Recall` and `Exists`.  All others
/// parse correctly so the engine can report "unsupported in this tier" rather
/// than a cryptic parse error.
///
/// Each variant carries serde aliases for its uppercase / PascalCase forms so
/// JSON-CAL callers can use any casing for the `"kind"` tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum CalStatement {
    // ── Phase 1: Core (Conformance Level 1) ─────────────────────────────
    /// `RECALL facts WHERE ...`
    #[serde(alias = "RECALL", alias = "Recall")]
    Recall(RecallStmt),

    /// `RECALL ... INTERSECT RECALL ...`
    #[serde(alias = "SET_OP", alias = "SetOp")]
    SetOp(SetOpStmt),

    /// `EXISTS facts WHERE ...`
    #[serde(alias = "EXISTS", alias = "Exists")]
    Exists(ExistsStmt),

    /// `ASSEMBLE "topic" FROM ... WHERE ...`
    #[serde(alias = "ASSEMBLE", alias = "Assemble")]
    Assemble(AssembleStmt),

    /// `HISTORY OF <hash>`
    #[serde(alias = "HISTORY", alias = "History")]
    History(HistoryStmt),

    /// `EXPLAIN <query>`
    #[serde(alias = "EXPLAIN", alias = "Explain")]
    Explain(ExplainStmt),

    /// `DESCRIBE facts` / `DESCRIBE SCHEMA`
    #[serde(alias = "DESCRIBE", alias = "Describe")]
    Describe(DescribeStmt),

    /// `BATCH { ... ; ... }`
    #[serde(alias = "BATCH", alias = "Batch")]
    Batch(BatchStmt),

    /// `COALESCE facts WHERE ...`
    #[serde(alias = "COALESCE", alias = "Coalesce")]
    Coalesce(CoalesceStmt),

    // ── Tier 1: Write statements ────────────────────────────────────────
    /// `ADD fact subject=... relation=... object=...`
    #[serde(alias = "ADD", alias = "Add")]
    Add(AddStmt),

    /// `ADD workflow "name" [ON "trigger"] graph... [BIND ...] REASON "..."`
    #[serde(alias = "ADD_WORKFLOW", alias = "AddWorkflow")]
    AddWorkflow(AddWorkflowStmt),

    /// `SUPERSEDE <hash> SET ... BECAUSE "..."`
    #[serde(alias = "SUPERSEDE", alias = "Supersede")]
    Supersede(SupersedeStmt),

    /// `SUPERSEDE <hash> [ON "trigger"] graph... [BIND ...] REASON "..."`
    #[serde(alias = "SUPERSEDE_WORKFLOW", alias = "SupersedeWorkflow")]
    SupersedeWorkflow(SupersedeWorkflowStmt),

    /// `ACCUMULATE <grain_type> [<hash>] [WHERE ...] ADD ... [SET ...] REASON "..."`
    #[serde(alias = "ACCUMULATE", alias = "Accumulate")]
    Accumulate(AccumulateStmt),

    /// `REVERT <hash> BECAUSE "..."`
    #[serde(alias = "REVERT", alias = "Revert")]
    Revert(RevertStmt),

    // ── Tier 2: Destructive statements (gated by allow_destructive_ops) ─
    /// `FORGET <hash>` / `FORGET USER "<user_id>"` / `FORGET SCOPE "<scope>"`
    Forget(ForgetStmt),

    /// `PURGE STALE [OLDER THAN <n> DAYS] [IN "<namespace>"] [LIMIT <n>]`
    Purge(PurgeStmt),

    // ── Template management ──────────────────────────────────────────────
    /// `DEFINE TEMPLATE "name" [DESCRIPTION "..."] [EXTENDS "parent"] [FOR facts, events] AS "source"`
    DefineTemplate(DefineTemplateStmt),

    /// `DROP TEMPLATE "name"`
    DropTemplate(DropTemplateStmt),

    // ── Saved query management ───────────────────────────────────────────
    /// `DEFINE QUERY "name"($params) [DESCRIPTION "..."] AS { body }`
    DefineQuery(DefineQueryStmt),

    /// `DROP QUERY "name"`
    DropQuery(DropQueryStmt),

    /// `RUN "name"($param = value, ...) [WITH ...] [FORMAT ...]`
    RunQuery(RunQueryStmt),
}

// ---------------------------------------------------------------------------
// RECALL
// ---------------------------------------------------------------------------

/// `RECALL <grain_type_plural> [ABOUT "..."] [WHERE ...] [RECENT n] ...`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecallStmt {
    /// The grain type being queried (plural form in CAL syntax, e.g.
    /// `facts`, `events`).
    pub grain_type: GrainTypePlural,

    /// Free-text `ABOUT "..."` clause for semantic search.
    pub about: Option<AboutClause>,

    /// Structured `WHERE ...` filter.
    pub where_clause: Option<WhereClause>,

    /// `RECENT <n>` shorthand for `ORDER BY created_at DESC LIMIT n`.
    pub recent: Option<RecentClause>,

    /// `SINCE "..."` temporal filter.
    pub since: Option<SinceClause>,

    /// `UNTIL "..."` temporal upper-bound. Can combine with SINCE for a range.
    pub until: Option<UntilClause>,

    /// `LIKE "..."` text-similarity filter.
    pub like: Option<LikeClause>,

    /// `BETWEEN "..." AND "..."` temporal range.
    pub between: Option<BetweenClause>,

    /// `CONTRADICTIONS OF (...)` sub-query.
    pub contradictions: Option<ContradictionsClause>,

    /// Inline `LIMIT` (separate from pipeline).
    pub limit: Option<u64>,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// Per-query output format override (`AS json`, `AS [markdown, json]`, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_format: Option<FormatClause>,

    /// Source span of the entire statement.
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// SET operations
// ---------------------------------------------------------------------------

/// A set operation combining two or more queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetOpStmt {
    pub op: SetOp,
    pub operands: Vec<CalStatement>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// Set operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

// ---------------------------------------------------------------------------
// EXISTS
// ---------------------------------------------------------------------------

/// `EXISTS <grain_type_plural> WHERE ...`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExistsStmt {
    pub grain_type: GrainTypePlural,
    pub where_clause: Option<WhereClause>,
    pub about: Option<AboutClause>,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// ASSEMBLE
// ---------------------------------------------------------------------------

/// `ASSEMBLE "topic" FROM <source> [WHERE ...]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssembleStmt {
    /// The topic or title of the assembly (Phase 1 field).
    pub topic: String,
    /// Source sub-query or grain set (Phase 1 single-source path).
    pub from: Source,
    pub where_clause: Option<WhereClause>,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// Explicit context name label (`ASSEMBLE "name"`). When present,
    /// overrides `topic`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_name: Option<String>,

    /// Multi-source FROM clause. When present, overrides the single `from`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<NamedSource>>,

    /// `BUDGET <n>` clause — token budget for assembled output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSpec>,

    /// `PRIORITY label1: 0.7, label2: 0.3` — per-source priority weights.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Vec<PrioritySpec>>,

    /// `FORMAT markdown` etc. — output format for assembled context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<FormatClause>,

    /// `FOR "someone"` — target audience / user for the assembly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub for_whom: Option<String>,

    /// `WITH dedup(field), summarize` — assembly-specific WITH options.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assemble_with: Vec<AssembleWithOption>,

    /// `STREAM ASSEMBLE ...` — enable SSE streaming (FR-004).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub streaming: bool,

    #[serde(skip)]
    pub span: Option<Span>,
}

/// A labeled source in a multi-source `ASSEMBLE ... FROM label1: (RECALL ...),
/// label2: (RECALL ...)` clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedSource {
    /// The label for this source (e.g. `"recent"`, `"context"`).
    pub label: String,
    /// The sub-query producing this source's grains.
    pub query: Box<CalStatement>,
    /// Per-source WITH options (e.g. `WITH exhaustive`). When non-empty,
    /// these override the parent query's with_options for this source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with_options: Vec<WithOption>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// Unit for the BUDGET clause — `tokens` (default) or `grains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum BudgetUnit {
    /// Token-based budget (default).
    #[default]
    Tokens,
    /// Grain-count budget.
    Grains,
}

/// `BUDGET <n> [tokens|grains]` — token/grain budget for assembled output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetSpec {
    /// Number of tokens (or grains).
    pub tokens: u32,
    /// The unit for the budget limit.
    #[serde(default)]
    pub unit: BudgetUnit,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `PRIORITY label: weight` — priority weight for a named source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrioritySpec {
    /// The source label this priority applies to.
    pub label: String,
    /// Weight (0.0..=1.0).
    pub weight: f64,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// ASSEMBLE-specific WITH options (distinct from the top-level WithOption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssembleWithOption {
    /// Deduplicate near-identical entries, optionally by a specific field.
    Dedup { field: Option<String> },
}

/// Source for ASSEMBLE FROM clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// `FROM facts WHERE ...` — an inline query.
    Query(Box<RecallStmt>),
    /// `FROM $parameter` — a bound parameter holding a result set.
    Parameter { name: String },
    /// `FROM <hash>, <hash>, ...` — explicit hash list.
    Hashes(Vec<String>),
}

// ---------------------------------------------------------------------------
// HISTORY
// ---------------------------------------------------------------------------

/// `HISTORY OF <hash>` or `HISTORY WHERE subject = ... AND relation = ...`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryStmt {
    /// Content-address hash (Phase 1 path). Empty string when using WHERE.
    pub hash: String,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// Structured WHERE clause for triple-based history lookup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<WhereClause>,

    /// `DIFF sha256:bbb` — compare two versions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_target: Option<String>,

    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// EXPLAIN
// ---------------------------------------------------------------------------

/// `EXPLAIN <query>` — returns a query plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplainStmt {
    pub inner: Box<CalStatement>,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// DESCRIBE
// ---------------------------------------------------------------------------

/// `DESCRIBE facts` / `DESCRIBE SCHEMA` — introspection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DescribeStmt {
    pub target: DescribeTarget,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// What to describe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescribeTarget {
    /// Describe the schema of a specific grain type.
    GrainType(GrainTypePlural),
    /// Describe the entire database schema.
    Schema,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// `DESCRIBE CAPABILITIES` — CAL conformance level and supported features.
    Capabilities,
    /// `DESCRIBE SERVER` — server information (version, uptime, etc.).
    Server,
    /// `DESCRIBE FIELDS [grain_type]` — list filterable/sortable fields.
    Fields(Option<GrainTypePlural>),
    /// `DESCRIBE TEMPLATES` — list registered output templates.
    Templates,
    /// `DESCRIBE GRAMMAR` — dump the CAL grammar (BNF or similar).
    Grammar,
    /// `DESCRIBE QUERIES` — list registered saved queries.
    Queries,
    /// `DESCRIBE QUERY "name"` — details of a specific saved query.
    Query(String),
}

// ---------------------------------------------------------------------------
// BATCH
// ---------------------------------------------------------------------------

/// A single entry inside a BATCH block, carrying the statement together with
/// any per-entry pipeline stages, FORMAT clause, WITH options, and user vars.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchEntry {
    pub statement: CalStatement,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub pipeline: Vec<PipelineStage>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub with_options: Vec<super::ast::WithOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<FormatClause>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub user_vars: HashMap<String, String>,
}

/// `BATCH { stmt1 ; stmt2 ; ... }`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchStmt {
    /// Positional (unlabeled) entries.
    pub statements: Vec<BatchEntry>,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// Labeled entries: `BATCH { label1: RECALL ...; label2: RECALL ...; }`.
    /// When present, results are keyed by label instead of index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labeled: Option<Vec<(String, BatchEntry)>>,

    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// COALESCE
// ---------------------------------------------------------------------------

/// `COALESCE <grain_type_plural> WHERE ...` (Phase 1) or
/// `COALESCE { query1 } OR { query2 } ELSE { fallback }` (Phase 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoalesceStmt {
    /// Grain type for Phase 1 single-branch path.
    pub grain_type: GrainTypePlural,
    /// WHERE clause for Phase 1 single-branch path.
    pub where_clause: Option<WhereClause>,

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// Multi-branch fallback chain: `{ query1 } OR { query2 } OR ...`.
    /// Each branch is tried in order until one returns non-empty results.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<CoalesceBranch>,

    /// Optional `ELSE { fallback }` — executed if all branches return empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub else_branch: Option<Box<CalStatement>>,

    #[serde(skip)]
    pub span: Option<Span>,
}

/// A single branch in a `COALESCE { ... } OR { ... }` chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoalesceBranch {
    /// The query to try for this branch.
    pub query: CalStatement,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// ADD (Tier 1)
// ---------------------------------------------------------------------------

/// `ADD <grain_type_singular> <field>=<value> ... [WITH ...]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddStmt {
    pub grain_type: GrainTypeSingular,
    /// Key-value pairs (`subject="john"`, `relation="likes"`, etc.).
    pub fields: Vec<FieldAssignment>,
    /// Mandatory REASON / BECAUSE clause (required for all Tier 1 writes).
    pub reason: String,
    /// Per-call intelligence options (`WITH extract_memories, auto_relate`, etc.).
    #[serde(default)]
    pub with_options: Vec<AddWithOption>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// A `field = value` assignment in an ADD or SET clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldAssignment {
    pub field: String,
    pub value: Value,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// ADD WORKFLOW (Tier 1 — graph syntax)
// ---------------------------------------------------------------------------

/// A graph edge in a workflow: `src -> dst [WHEN "cond"] [* N]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphEdge {
    pub src: String,
    pub dst: String,
    pub cond: Option<String>,
    pub repeat: Option<u32>,
}

/// A BIND clause: `BIND node = sha256:hash`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindClause {
    pub node: String,
    pub hash: String,
}

/// `ADD workflow "name" [ON "trigger"] graph... [BIND ...] REASON "..."`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddWorkflowStmt {
    /// Workflow name (positional string after `ADD workflow`).
    pub name: String,
    /// Optional trigger (`ON "..."`).
    pub trigger: Option<String>,
    /// All nodes discovered during parsing (unique, in declaration order).
    pub nodes: Vec<String>,
    /// Graph edges parsed from arrow chains.
    pub edges: Vec<GraphEdge>,
    /// BIND clauses mapping nodes to Tool definition hashes.
    pub bindings: Vec<BindClause>,
    /// REASON / BECAUSE string.
    pub reason: String,
    /// Per-call intelligence options (`WITH ...`).
    #[serde(default)]
    pub with_options: Vec<AddWithOption>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `SUPERSEDE <hash> [ON "trigger"] graph... [BIND ...] REASON "..."`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupersedeWorkflowStmt {
    /// Hash of the grain to supersede.
    pub hash: String,
    /// Optional trigger (`ON "..."`).
    pub trigger: Option<String>,
    /// All nodes discovered during parsing.
    pub nodes: Vec<String>,
    /// Graph edges parsed from arrow chains.
    pub edges: Vec<GraphEdge>,
    /// BIND clauses.
    pub bindings: Vec<BindClause>,
    /// REASON / BECAUSE string.
    pub reason: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// SUPERSEDE (Tier 1)
// ---------------------------------------------------------------------------

/// `SUPERSEDE <hash> SET <field>=<value>, ... BECAUSE "reason"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupersedeStmt {
    pub hash: String,
    pub set_clauses: Vec<FieldAssignment>,
    pub reason: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// ACCUMULATE (Tier 1)
// ---------------------------------------------------------------------------

/// `ACCUMULATE <grain_type> [<hash>] [WHERE ...] ADD ... [SET ...] REASON "..."`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccumulateStmt {
    /// Target grain type (singular: fact, event, state, etc.).
    pub grain_type: GrainTypeSingular,
    /// Resolution mode: either a content hash or a WHERE-based tip lookup.
    pub target: AccumulateTarget,
    /// Numeric delta operations (ADD field = value).
    pub add_ops: Vec<DeltaOp>,
    /// Last-writer-wins field replacements (SET field = value).
    pub set_ops: Vec<FieldAssignment>,
    /// Reason for the accumulation (required).
    pub reason: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// How to identify the grain to accumulate into.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccumulateTarget {
    /// Resolve the current tip via entity_latest lookup.
    TipResolved {
        subject: String,
        relation: String,
        namespace: Option<String>,
    },
    /// Target a specific grain by hash (optimistic concurrency).
    Hash { hash: String },
}

/// A numeric delta operation: `ADD field = value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeltaOp {
    pub field: String,
    pub delta: f64,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// REVERT (Tier 1)
// ---------------------------------------------------------------------------

/// `REVERT <hash> BECAUSE "reason"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevertStmt {
    pub hash: String,
    pub reason: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// FORGET (Tier 2)
// ---------------------------------------------------------------------------

/// `FORGET <hash>`, `FORGET USER "<user_id>"`, or `FORGET SCOPE "<scope>"`.
///
/// Gated by `CalExecutorConfig::allow_destructive_ops`. Only the `Hash`
/// target is backed by the store (`DejaDB::forget`, a single-grain tombstone);
/// `User`/`Scope` crypto-erasure is not implemented yet and returns
/// `Unsupported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForgetStmt {
    /// What to forget (hash, user, or scope).
    pub target: ForgetTarget,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// Target of a FORGET statement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForgetTarget {
    /// `FORGET <hash>` — remove a single grain by content-address hash.
    Hash { hash: String },
    /// `FORGET USER "<user_id>"` — crypto-erase all data for a user.
    User { user_id: String },
    /// `FORGET SCOPE "<scope>"` — crypto-erase all data in a scope.
    Scope { scope: String },
}

// ---------------------------------------------------------------------------
// PURGE (Tier 2)
// ---------------------------------------------------------------------------

/// `PURGE STALE [OLDER THAN <n> DAYS] [IN "<namespace>"] [LIMIT <n>]`
///
/// Cleanup expired/stale grains using the decay curve engine.
/// Gated by `CalExecutorConfig::allow_destructive_ops` (and not backed by the
/// store yet — returns `Unsupported`; also not reachable from CAL text).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PurgeStmt {
    /// Minimum age in days (from `OLDER THAN <n> DAYS`). Default: 30.
    pub min_age_days: Option<f64>,
    /// Namespace scope (from `IN "<namespace>"`). Default: "default".
    pub namespace: Option<String>,
    /// Maximum grains to purge (from `LIMIT <n>`). Default: 1000.
    pub limit: Option<usize>,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// DEFINE TEMPLATE / DROP TEMPLATE
// ---------------------------------------------------------------------------

/// `DEFINE TEMPLATE "name" [DESCRIPTION "..."] [EXTENDS "parent"] [FOR facts, events] AS "source"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefineTemplateStmt {
    /// Template name (validated: `^[a-zA-Z][a-zA-Z0-9 _-]{0,63}$`).
    pub name: String,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional parent template name (1-level inheritance).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Optional grain type restriction (e.g. `FOR facts, events`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub grain_types: Vec<String>,
    /// Template source (Mustache-subset).
    pub source: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `DROP TEMPLATE "name"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DropTemplateStmt {
    /// Template name to drop.
    pub name: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// DEFINE QUERY / DROP QUERY / RUN
// ---------------------------------------------------------------------------

/// A parameter declaration in a saved query definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryParam {
    /// Parameter name (without the `$` prefix).
    pub name: String,
    /// Optional default value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// `DEFINE QUERY "name"($params) [DESCRIPTION "..."] AS { body }`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefineQueryStmt {
    /// Query name (validated: `^[a-zA-Z][a-zA-Z0-9 _-]{0,63}$`).
    pub name: String,
    /// Optional human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Parameter declarations with optional defaults.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<QueryParam>,
    /// Raw CAL body text (stored as-is, parsed at RUN time).
    pub body: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `DROP QUERY "name"`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DropQueryStmt {
    /// Query name to drop.
    pub name: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `RUN "name"($param = value, ...) [WITH ...] [FORMAT ...]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunQueryStmt {
    /// Saved query name to execute.
    pub name: String,
    /// Parameter bindings ($name = value).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<(String, Value)>,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// WHERE clause
// ---------------------------------------------------------------------------

/// Structured filter clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhereClause {
    pub condition: Condition,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// A filter condition (possibly nested).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Condition {
    /// `field <op> value`
    Comparison {
        field: String,
        comparator: Comparator,
        value: Value,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field IN (v1, v2, ...)`
    In {
        field: String,
        values: Vec<Value>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field NOT IN (v1, v2, ...)`
    NotIn {
        field: String,
        values: Vec<Value>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field IS NULL`
    IsNull {
        field: String,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field IS NOT NULL`
    IsNotNull {
        field: String,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field CONTAINS "text"`
    Contains {
        field: String,
        value: String,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `field STARTS WITH "text"`
    StartsWith {
        field: String,
        value: String,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `cond AND cond`
    And {
        left: Box<Condition>,
        right: Box<Condition>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `cond OR cond`
    Or {
        left: Box<Condition>,
        right: Box<Condition>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `NOT cond`
    Not {
        inner: Box<Condition>,
        #[serde(skip)]
        span: Option<Span>,
    },

    // ── Phase 2 additions ────────────────────────────────────────────────
    /// `field IS PREFERENCE` / `field IS KNOWLEDGE` etc. — category filter.
    IsCategory {
        field: String,
        category: String,
        #[serde(skip)]
        span: Option<Span>,
    },
}

// ---------------------------------------------------------------------------
// Comparators
// ---------------------------------------------------------------------------

/// Comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparator {
    /// `=`
    Eq,
    /// `!=`
    NotEq,
    /// `>=`
    Gte,
    /// `<=`
    Lte,
    /// `>`
    Gt,
    /// `<`
    Lt,
}

impl std::fmt::Display for Comparator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eq => write!(f, "="),
            Self::NotEq => write!(f, "!="),
            Self::Gte => write!(f, ">="),
            Self::Lte => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::Lt => write!(f, "<"),
        }
    }
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// A literal value or parameter reference in a CAL expression.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Value {
    /// A quoted string literal: `"hello"`.
    String { value: String },
    /// A numeric literal: `42`, `3.14`.
    Number { value: f64 },
    /// A boolean literal: `true` / `false`.
    Boolean { value: bool },
    /// An array literal: `["a", "b"]`.
    Array { values: Vec<Value> },
    /// A content-address hash literal: `#abcdef01...`.
    Hash { value: String },
    /// A bound parameter reference: `$name`.
    Parameter { name: String },
}

impl Value {
    /// Human-readable type label for error messages. Stable, does not
    /// expose the underlying struct shape.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::String { .. } => "string",
            Self::Number { .. } => "number",
            Self::Boolean { .. } => "boolean",
            Self::Array { .. } => "array",
            Self::Hash { .. } => "hash",
            Self::Parameter { .. } => "parameter",
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::String { value } => write!(f, "\"{}\"", value),
            Self::Number { value } => write!(f, "{}", value),
            Self::Boolean { value } => write!(f, "{}", value),
            Self::Array { values } => {
                write!(f, "[")?;
                for (i, v) in values.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Self::Hash { value } => write!(f, "#{}", value),
            Self::Parameter { name } => write!(f, "${}", name),
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline stages
// ---------------------------------------------------------------------------

/// A pipeline stage following `|`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case")]
pub enum PipelineStage {
    /// `| SELECT field1, field2, ...`
    Select {
        fields: Vec<String>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| ORDER BY field [ASC|DESC]`
    OrderBy {
        field: String,
        descending: bool,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| LIMIT n`
    Limit {
        value: u64,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| OFFSET n`
    Offset {
        value: u64,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| COUNT`
    Count {
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| FIRST`
    First {
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| SUBJECTS` — extract the `subject` field from each Fact.
    Subjects {
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| OBJECTS` — extract the `object` field from each Fact.
    Objects {
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| HASHES` — extract the content-address hash of each grain.
    Hashes {
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| GROUP BY field`
    GroupBy {
        field: String,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `| PROJECT field1, field2, ...` (alias for SELECT with remapping).
    Project {
        fields: Vec<ProjectField>,
        #[serde(skip)]
        span: Option<Span>,
    },

    /// `WHERE condition` appearing after pipeline stages (post-pipeline filter).
    ///
    /// Allows queries like `RECALL facts SELECT subject WHERE subject = "john"`
    /// where WHERE follows a pipeline stage rather than appearing in the RECALL
    /// statement body.
    Filter {
        condition: Condition,
        #[serde(skip)]
        span: Option<Span>,
    },
}

/// A field in a `PROJECT` clause, optionally with an alias.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectField {
    pub field: String,
    pub alias: Option<String>,
}

// ---------------------------------------------------------------------------
// WITH options
// ---------------------------------------------------------------------------

/// A `WITH` option modifying query behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "option", rename_all = "snake_case")]
pub enum WithOption {
    /// Include superseded (historical) grains in results.
    Superseded,
    /// Include relevance score breakdown per result.
    ScoreBreakdown,
    /// Include a human-readable explanation of ranking.
    Explanation,
    /// Include provenance chain in results.
    Provenance,
    /// Enable contradiction detection on the result set.
    ContradictionDetection,
    /// Apply MMR diversity to the result set.
    Diversity { lambda: Option<f64> },
    /// Deduplicate results, optionally keyed by a specific field name.
    /// EBNF: `"dedup" , "(" , field_name , ")"`.
    Dedup { field: Option<String> },

    /// Progressive disclosure level (OMS §4 `progressive_disclosure(level)`).
    /// `level` is `summary | headlines | full`. Bare form (no parens) maps
    /// to `None` and lets the assembler pick a default.
    ProgressiveDisclosure { level: Option<String> },

    /// Consistency level (OMS §4 `consistency(level)` where
    /// `level = "eventual" | "bounded" | "linearizable"`).
    Consistency { level: Option<String> },

    /// Locale hint for humanization / collation (OMS §4 `locale("en-US")`).
    Locale { tag: String },

    /// Cache directive (OMS §4 `cache(ttl=300)`).
    Cache { ttl_seconds: u64 },

    // -- Recall feature flags (parity with HTTP/gRPC/MCP/A2A) ---------------
    /// Enable cross-encoder reranking (requires `rerank` feature).
    /// Optional model name selects a specific reranker from the registry.
    Rerank { model: Option<String> },
    /// Enable LLM listwise reranking (requires `llm-rerank` feature).
    /// Optional model name selects a specific LLM reranker from the registry.
    LlmRerank { model: Option<String> },
    /// Enable rule-based query expansion (stemming + synonyms).
    QueryExpansion,
    /// ADR-023: Enable rule-based query decomposition (2-4 sub-queries per strategy).
    QueryDecompose,
    /// Enable hypothetical document embeddings.
    Hyde,
    /// Keep only newest grain per (subject, relation).
    ConflictResolution,
    /// Include `derived_from` source grains in results.
    IncludeSources,
    /// Annotate results with relative time labels (e.g. "2 weeks ago").
    AnnotateRelativeTime,
    /// Set recency weight for temporal freshness scoring.
    RecencyWeight { weight: f64 },
    /// Set minimum relevance score threshold.
    MinScore { score: f64 },
    /// Enable entity-graph multi-hop retrieval (1-3 hops).
    MultiHop { hops: u64 },
    /// Set session affinity boost factor [0.0–1.0].
    SessionAffinity { boost: f64 },
    /// Set subject affinity boost factor [0.0–1.0].
    SubjectAffinity { boost: f64 },
    /// FR-005: Minimum grains per namespace for cross-session coverage.
    SessionCoverage { min_per_ns: u64 },
    /// FR-005: Maximum unique namespaces in results.
    MaxNamespaces { max: u64 },
    /// WI-EXHAUST: Enable exhaustive entity-class recall.
    /// Optional rounds parameter.
    Exhaustive { max_rounds: Option<u64> },
    /// RF-3: Session-census retrieval.
    /// Optional parameters: min_per_session, min_score.
    SessionCensus {
        min_per_session: Option<u64>,
        min_score: Option<f64>,
    },
    /// RQ-3: Keep superseded grains at natural scores for aggregation queries.
    AggregationIntent,
    /// Enrich preference queries with co-occurring session grains.
    PreferenceEnrichment,
}

/// A `WITH` option on an `ADD` statement controlling intelligence behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "option", rename_all = "snake_case")]
pub enum AddWithOption {
    /// Extract temporal references from content → auto-populate `valid_from`.
    ExtractEventDate,
    /// Auto-detect updates/extends relationships with existing grains.
    AutoRelate,
    /// Decompose content into atomic facts linked via `derived_from`.
    ExtractMemories,
    /// Force immediate commit (bypass write batch buffer).
    Sync,
}

// ---------------------------------------------------------------------------
// FORMAT spec
// ---------------------------------------------------------------------------

/// Output format specification (`FORMAT json`, `FORMAT markdown`, etc.).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
pub enum FormatSpec {
    Sml,
    Toon,
    Markdown,
    Json,
    Yaml,
    Text,
    /// Triple output (subject, relation, object per line).
    Triples,
    /// CSV output (header row + data rows).
    Csv,
    /// Markdown table output.
    Table,
    /// A named preset format (e.g. `FORMAT preset "compact"`).
    Preset {
        name: String,
    },
    /// A custom template string.
    Template {
        template: String,
    },
}

impl FormatSpec {
    /// Return the canonical key name used in multi-format response payloads.
    pub fn canonical_key(&self) -> &str {
        match self {
            Self::Json => "json",
            Self::Markdown => "markdown",
            Self::Yaml => "yaml",
            Self::Text => "text",
            Self::Sml => "sml",
            Self::Toon => "toon",
            Self::Triples => "triples",
            Self::Csv => "csv",
            Self::Table => "table",
            Self::Preset { name } => name.as_str(),
            Self::Template { .. } => "template",
        }
    }
}

/// A format spec with an optional alias for multi-format lists.
///
/// `FORMAT [json AS customers, TEMPLATE "..." AS users]`
///
/// When an alias is present, it becomes the key in the `MultiFormatted`
/// response payload instead of the canonical format name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AliasedFormat {
    pub spec: FormatSpec,
    /// Optional alias (`AS <identifier>`). When `None`, the canonical format
    /// name is used as the response key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// A FORMAT/AS clause that is either a single format or a list of formats
/// (CAL spec v1.0.1, Section 10.1.1).
///
/// Single format: `FORMAT json` or `AS markdown`
/// Multi-format:  `FORMAT [markdown, json]` or `AS [markdown, json]`
/// Aliased multi: `FORMAT [json AS data, markdown AS readable]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormatClause {
    /// A single output format (existing behavior).
    Single(FormatSpec),
    /// Multiple output formats rendered from a single query execution.
    /// Maximum 5 formats per list (CAL-E110).
    Multi(Vec<AliasedFormat>),
}

// ---------------------------------------------------------------------------
// Grain type names (plural / singular)
// ---------------------------------------------------------------------------

/// Plural grain type name as used in `RECALL facts`, `RECALL events`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrainTypePlural {
    Facts,
    Events,
    States,
    Workflows,
    Tools,
    Observations,
    Goals,
    Reasonings,
    Consensuses,
    Consents,
    Skills,
    /// Wildcard — `RECALL *` or `RECALL grains` — matches all types.
    All,
}

impl GrainTypePlural {
    /// Parse a plural grain type name (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "facts" | "fact" => Some(Self::Facts),
            "events" | "event" => Some(Self::Events),
            "states" | "state" => Some(Self::States),
            "workflows" | "workflow" => Some(Self::Workflows),
            "tools" | "tool" => Some(Self::Tools),
            "observations" | "observation" => Some(Self::Observations),
            "goals" | "goal" => Some(Self::Goals),
            "reasonings" | "reasoning" => Some(Self::Reasonings),
            "consensuses" | "consensus" => Some(Self::Consensuses),
            "consents" | "consent" => Some(Self::Consents),
            "skills" | "skill" => Some(Self::Skills),
            "*" | "grains" | "all" => Some(Self::All),
            _ => None,
        }
    }

    /// Return the canonical plural string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Events => "events",
            Self::States => "states",
            Self::Workflows => "workflows",
            Self::Tools => "tools",
            Self::Observations => "observations",
            Self::Goals => "goals",
            Self::Reasonings => "reasonings",
            Self::Consensuses => "consensuses",
            Self::Consents => "consents",
            Self::Skills => "skills",
            Self::All => "*",
        }
    }

    /// Convert to the engine's `GrainType` enum, if not the wildcard.
    pub fn to_grain_type(&self) -> Option<dejadb_core::types::GrainType> {
        match self {
            Self::Facts => Some(dejadb_core::types::GrainType::Fact),
            Self::Events => Some(dejadb_core::types::GrainType::Event),
            Self::States => Some(dejadb_core::types::GrainType::State),
            Self::Workflows => Some(dejadb_core::types::GrainType::Workflow),
            Self::Tools => Some(dejadb_core::types::GrainType::Tool),
            Self::Observations => Some(dejadb_core::types::GrainType::Observation),
            Self::Goals => Some(dejadb_core::types::GrainType::Goal),
            Self::Reasonings => Some(dejadb_core::types::GrainType::Reasoning),
            Self::Consensuses => Some(dejadb_core::types::GrainType::Consensus),
            Self::Consents => Some(dejadb_core::types::GrainType::Consent),
            Self::Skills => Some(dejadb_core::types::GrainType::Skill),
            Self::All => None,
        }
    }
}

impl std::fmt::Display for GrainTypePlural {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Singular grain type name as used in `ADD fact`, `ADD event`, etc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrainTypeSingular {
    Fact,
    Event,
    State,
    Workflow,
    Tool,
    Observation,
    Goal,
    Reasoning,
    Consensus,
    Consent,
    Skill,
}

impl GrainTypeSingular {
    /// Parse a singular grain type name (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "fact" => Some(Self::Fact),
            "event" => Some(Self::Event),
            "state" => Some(Self::State),
            "workflow" => Some(Self::Workflow),
            "tool" => Some(Self::Tool),
            "observation" => Some(Self::Observation),
            "goal" => Some(Self::Goal),
            "reasoning" => Some(Self::Reasoning),
            "consensus" => Some(Self::Consensus),
            "consent" => Some(Self::Consent),
            "skill" => Some(Self::Skill),
            _ => None,
        }
    }

    /// Return the canonical singular string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Event => "event",
            Self::State => "state",
            Self::Workflow => "workflow",
            Self::Tool => "tool",
            Self::Observation => "observation",
            Self::Goal => "goal",
            Self::Reasoning => "reasoning",
            Self::Consensus => "consensus",
            Self::Consent => "consent",
            Self::Skill => "skill",
        }
    }

    /// Convert to the engine's `GrainType` enum.
    pub fn to_grain_type(&self) -> dejadb_core::types::GrainType {
        match self {
            Self::Fact => dejadb_core::types::GrainType::Fact,
            Self::Event => dejadb_core::types::GrainType::Event,
            Self::State => dejadb_core::types::GrainType::State,
            Self::Workflow => dejadb_core::types::GrainType::Workflow,
            Self::Tool => dejadb_core::types::GrainType::Tool,
            Self::Observation => dejadb_core::types::GrainType::Observation,
            Self::Goal => dejadb_core::types::GrainType::Goal,
            Self::Reasoning => dejadb_core::types::GrainType::Reasoning,
            Self::Consensus => dejadb_core::types::GrainType::Consensus,
            Self::Consent => dejadb_core::types::GrainType::Consent,
            Self::Skill => dejadb_core::types::GrainType::Skill,
        }
    }

    /// Return the plural form.
    pub fn to_plural(&self) -> GrainTypePlural {
        match self {
            Self::Fact => GrainTypePlural::Facts,
            Self::Event => GrainTypePlural::Events,
            Self::State => GrainTypePlural::States,
            Self::Workflow => GrainTypePlural::Workflows,
            Self::Tool => GrainTypePlural::Tools,
            Self::Observation => GrainTypePlural::Observations,
            Self::Goal => GrainTypePlural::Goals,
            Self::Reasoning => GrainTypePlural::Reasonings,
            Self::Consensus => GrainTypePlural::Consensuses,
            Self::Consent => GrainTypePlural::Consents,
            Self::Skill => GrainTypePlural::Skills,
        }
    }
}

impl std::fmt::Display for GrainTypeSingular {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Clause types
// ---------------------------------------------------------------------------

/// `ABOUT "free text query"` — semantic search clause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AboutClause {
    pub text: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `RECENT <n>` — shorthand for ORDER BY created_at DESC LIMIT n.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecentClause {
    pub count: u64,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `SINCE "2024-01-01"` or `SINCE "3 days ago"` — temporal lower-bound.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SinceClause {
    pub expression: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `UNTIL "2024-12-31"` or `UNTIL "1 week ago"` — temporal upper-bound.
/// Standalone: filters grains with date <= expression.
/// Combined with SINCE: forms a date range (SINCE "start" UNTIL "end").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UntilClause {
    pub expression: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `LIKE "example text"` — text-similarity filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LikeClause {
    pub text: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `BETWEEN "start" AND "end"` — temporal range filter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BetweenClause {
    pub start: String,
    pub end: String,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// `CONTRADICTIONS [OF (sub-query)]` — find contradicting grains.
/// `CONTRADICTIONS` is a bare terminal per spec; the `OF (sub-query)` tail
/// is a DejaDB extension and is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContradictionsClause {
    pub inner: Option<Box<CalStatement>>,
    #[serde(skip)]
    pub span: Option<Span>,
}

// ---------------------------------------------------------------------------
// HISTORY DIFF types
// ---------------------------------------------------------------------------

/// A single field-level difference between two grain versions.
///
/// Used in `CalResultPayload::Diff` to represent the result of a
/// `HISTORY <hash> DIFF <hash>` comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldDiff {
    /// A field present in the target grain but absent in the source grain.
    Added {
        field: String,
        value: serde_json::Value,
    },
    /// A field present in the source grain but absent in the target grain.
    Removed {
        field: String,
        value: serde_json::Value,
    },
    /// A field present in both grains with different values.
    Changed {
        field: String,
        old: serde_json::Value,
        new: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// LET bindings
// ---------------------------------------------------------------------------

/// `LET $name = <extractor> OF (sub-query)`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LetBinding {
    /// Parameter name (without the `$` prefix).
    pub name: String,
    /// The extractor to apply.
    pub extractor: Extractor,
    /// The sub-query to extract from.
    pub source: Box<CalStatement>,
    #[serde(skip)]
    pub span: Option<Span>,
}

/// Extractor used in LET bindings and pipeline stages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Extractor {
    /// Extract the `subject` field from each Fact grain.
    Subjects,
    /// Extract the `object` field from each Fact grain.
    Objects,
    /// Extract the content-address hash from each grain.
    Hashes,
}

impl std::fmt::Display for Extractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Subjects => write!(f, "SUBJECTS"),
            Self::Objects => write!(f, "OBJECTS"),
            Self::Hashes => write!(f, "HASHES"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grain_type_plural_parse() {
        assert_eq!(
            GrainTypePlural::parse("facts"),
            Some(GrainTypePlural::Facts)
        );
        assert_eq!(
            GrainTypePlural::parse("FACTS"),
            Some(GrainTypePlural::Facts)
        );
        assert_eq!(
            GrainTypePlural::parse("Events"),
            Some(GrainTypePlural::Events)
        );
        assert_eq!(GrainTypePlural::parse("*"), Some(GrainTypePlural::All));
        assert_eq!(GrainTypePlural::parse("grains"), Some(GrainTypePlural::All));
        assert_eq!(GrainTypePlural::parse("unknown"), None);
    }

    #[test]
    fn test_grain_type_singular_parse() {
        assert_eq!(
            GrainTypeSingular::parse("fact"),
            Some(GrainTypeSingular::Fact)
        );
        assert_eq!(
            GrainTypeSingular::parse("TOOL"),
            Some(GrainTypeSingular::Tool)
        );
        assert_eq!(GrainTypeSingular::parse("facts"), None); // plural != singular
    }

    #[test]
    fn test_grain_type_plural_to_engine_type() {
        let plural = GrainTypePlural::Facts;
        assert_eq!(plural.to_grain_type(), Some(dejadb_core::types::GrainType::Fact));
        assert_eq!(GrainTypePlural::All.to_grain_type(), None);
    }

    #[test]
    fn test_grain_type_singular_to_plural() {
        assert_eq!(GrainTypeSingular::Fact.to_plural(), GrainTypePlural::Facts);
        assert_eq!(
            GrainTypeSingular::Consent.to_plural(),
            GrainTypePlural::Consents
        );
    }

    #[test]
    fn test_comparator_display() {
        assert_eq!(format!("{}", Comparator::Eq), "=");
        assert_eq!(format!("{}", Comparator::NotEq), "!=");
        assert_eq!(format!("{}", Comparator::Gte), ">=");
        assert_eq!(format!("{}", Comparator::Lt), "<");
    }

    #[test]
    fn test_value_display() {
        assert_eq!(
            format!(
                "{}",
                Value::String {
                    value: "hello".into()
                }
            ),
            "\"hello\""
        );
        assert_eq!(format!("{}", Value::Number { value: 42.0 }), "42");
        assert_eq!(format!("{}", Value::Boolean { value: true }), "true");
        assert_eq!(format!("{}", Value::Parameter { name: "x".into() }), "$x");
        assert_eq!(
            format!(
                "{}",
                Value::Hash {
                    value: "abc123".into()
                }
            ),
            "#abc123"
        );
        let arr = Value::Array {
            values: vec![
                Value::String { value: "a".into() },
                Value::Number { value: 1.0 },
            ],
        };
        assert_eq!(format!("{}", arr), "[\"a\", 1]");
    }

    #[test]
    fn test_cal_version_default() {
        assert_eq!(CalVersion::default(), CalVersion(1));
    }

    #[test]
    fn test_extractor_display() {
        assert_eq!(format!("{}", Extractor::Subjects), "SUBJECTS");
        assert_eq!(format!("{}", Extractor::Objects), "OBJECTS");
        assert_eq!(format!("{}", Extractor::Hashes), "HASHES");
    }

    #[test]
    fn test_set_op_serializes() {
        // Verify the serde rename works
        let op = SetOp::Intersect;
        let json = serde_json::to_string(&op).unwrap();
        assert_eq!(json, "\"intersect\"");
    }

    #[test]
    fn test_recall_stmt_construction() {
        let stmt = RecallStmt {
            grain_type: GrainTypePlural::Facts,
            about: Some(AboutClause {
                text: "john preferences".into(),
                span: None,
            }),
            where_clause: Some(WhereClause {
                condition: Condition::Comparison {
                    field: "subject".into(),
                    comparator: Comparator::Eq,
                    value: Value::String {
                        value: "john".into(),
                    },
                    span: None,
                },
                span: None,
            }),
            recent: None,
            since: None,
            until: None,
            like: None,
            between: None,
            contradictions: None,
            limit: Some(10),
            as_format: None,
            span: None,
        };
        assert_eq!(stmt.grain_type, GrainTypePlural::Facts);
        assert!(stmt.about.is_some());
        assert!(stmt.where_clause.is_some());
        assert_eq!(stmt.limit, Some(10));
    }

    #[test]
    fn test_cal_query_construction() {
        let query = CalQuery {
            version: CalVersion(1),
            statement: CalStatement::Recall(RecallStmt {
                grain_type: GrainTypePlural::Events,
                about: None,
                where_clause: None,
                recent: Some(RecentClause {
                    count: 5,
                    span: None,
                }),
                since: None,
                until: None,
                like: None,
                between: None,
                contradictions: None,
                limit: None,
                as_format: None,
                span: None,
            }),
            pipeline: vec![
                PipelineStage::OrderBy {
                    field: "created_at".into(),
                    descending: true,
                    span: None,
                },
                PipelineStage::Limit {
                    value: 5,
                    span: None,
                },
            ],
            with_options: vec![WithOption::ScoreBreakdown],
            format: Some(FormatClause::Single(FormatSpec::Json)),
            let_bindings: vec![],
            user_vars: HashMap::new(),
            warnings: vec![],
        };
        assert_eq!(query.version, CalVersion(1));
        assert_eq!(query.pipeline.len(), 2);
        assert_eq!(query.with_options.len(), 1);
    }

    #[test]
    fn test_nested_condition() {
        let cond = Condition::And {
            left: Box::new(Condition::Comparison {
                field: "subject".into(),
                comparator: Comparator::Eq,
                value: Value::String {
                    value: "john".into(),
                },
                span: None,
            }),
            right: Box::new(Condition::Or {
                left: Box::new(Condition::Comparison {
                    field: "confidence".into(),
                    comparator: Comparator::Gte,
                    value: Value::Number { value: 0.8 },
                    span: None,
                }),
                right: Box::new(Condition::IsNotNull {
                    field: "tags".into(),
                    span: None,
                }),
                span: None,
            }),
            span: None,
        };
        // Just verify it constructs without panic
        match &cond {
            Condition::And { .. } => {}
            _ => panic!("expected And"),
        }
    }
}
