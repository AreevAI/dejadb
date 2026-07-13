//! CAL Phase 4 — Template engine for formatting grain result sets.
//!
//! Implements a Mustache-subset template engine with:
//! - Single-pass O(n) character scanner parser
//! - Closed variable set with parse-time validation
//! - 10 built-in filters (no external crate dependencies)
//! - Template registry with 6 built-in templates
//! - Per-grain-type rendering blocks
//! - Progressive disclosure tiers (Summary, Headlines, Full)
//! - 1-level template inheritance
//!
//! # Security invariants
//!
//! - **F1**: Render output capped at `MAX_RENDER_OUTPUT_SIZE` (1MB), checked per-grain.
//! - **F2**: `truncate` filter argument validated at parse time to [1, 100_000].
//! - **F3**: `format_epoch` rejects negative epochs (returns `"<invalid date>"`).
//! - **F4**: `date` filter format string capped at 256 chars (parse time + runtime).
//! - **F5**: Runtime `ALLOWED_FIELDS` defense-in-depth on variable resolution.
//! - **F6**: Template names validated with `is_valid_template_name()`.
//! - **F7**: `select_tier` clamps `grain_count` to `u32::MAX` before cast.

use std::collections::HashMap;

use super::errors::{CalError, CalResult, Span};
use super::executor::CalGrainResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum template source size in bytes (64KB).
pub const MAX_TEMPLATE_SIZE: usize = 65_536;

/// Maximum rendered output size in bytes (1MB).
pub const MAX_RENDER_OUTPUT_SIZE: usize = 1_048_576;

/// Closed set of allowed grain field names for runtime resolution.
/// Defense-in-depth (F5): even if `grain.fields` contains extra keys,
/// only these are accessible through templates.
const ALLOWED_FIELDS: &[&str] = &[
    // Common fields
    "subject",
    "relation",
    "object",
    "namespace",
    "user_id",
    "confidence",
    "importance",
    "created_at",
    "tags",
    "session_id",
    "content",
    "summary",
    "source_hash",
    "source_hashes",
    // Event
    "actor",
    "action",
    "participants",
    // State
    "entity",
    "state_value",
    "validity",
    // Workflow
    "name",
    "nodes",
    "edges",
    "bindings",
    "retries",
    "trigger",
    "node_count",
    "edge_count",
    "status",
    // Tool
    "tool_name",
    "input",
    "is_error",
    "duration_ms",
    "output_schema",
    // Phase 1 (2026-04-19) — Tool definition fields promoted from
    // extra_fields. Available to templates so the Phase 2 renderer can
    // emit tool catalogs in markdown / hermes / openai-tools / etc.
    "kind",
    "tool_description",
    "input_schema",
    "executor_uri",
    "locked_params",
    "examples",
    "annotations",
    "spec_hash",
    "tool_call_id",
    "call_batch_id",
    // Observation
    "observer",
    "observed",
    "sensory_data",
    "sensor",
    "value",
    "unit",
    // Goal
    "title",
    "description",
    "goal_state",
    "assigned_to",
    "priority",
    "parent_hash",
    // Reasoning
    "premises",
    "conclusion",
    // Consensus
    "agreement_level",
    "agreement",
    // Consent
    "consenter",
    "scope",
    "granted",
    "expires_at",
    "grantee_did",
    "subject_did",
    "is_withdrawal",
    "basis",
];

/// The full closed set of valid template variable names (fields + metadata + direct).
const ALL_VALID_VARIABLES: &[&str] = &[
    // Direct CalGrainResult fields
    "hash",
    "grain_type",
    "score",
    // Render context metadata
    "_index",
    "_count",
    "_first",
    "_last",
    "_now",
    // Common grain fields
    "subject",
    "relation",
    "object",
    "namespace",
    "user_id",
    "confidence",
    "importance",
    "created_at",
    "tags",
    "session_id",
    "content",
    "summary",
    "source_hash",
    "source_hashes",
    // Event
    "actor",
    "action",
    "participants",
    // State
    "entity",
    "state_value",
    "validity",
    // Workflow
    "name",
    "nodes",
    "edges",
    "bindings",
    "retries",
    "trigger",
    "node_count",
    "edge_count",
    "status",
    // Tool
    "tool_name",
    "input",
    "is_error",
    "duration_ms",
    "output_schema",
    // Phase 1 (2026-04-19) — Tool definition fields promoted from
    // extra_fields. Available to templates so the Phase 2 renderer can
    // emit tool catalogs in markdown / hermes / openai-tools / etc.
    "kind",
    "tool_description",
    "input_schema",
    "executor_uri",
    "locked_params",
    "examples",
    "annotations",
    "spec_hash",
    "tool_call_id",
    "call_batch_id",
    // Observation
    "observer",
    "observed",
    "sensory_data",
    "sensor",
    "value",
    "unit",
    // Goal
    "title",
    "description",
    "goal_state",
    "assigned_to",
    "priority",
    "parent_hash",
    // Reasoning
    "premises",
    "conclusion",
    // Consensus
    "agreement_level",
    "agreement",
    // Consent
    "consenter",
    "scope",
    "granted",
    "expires_at",
    "grantee_did",
    "subject_did",
    "is_withdrawal",
    "basis",
];

/// Known filter names.
const KNOWN_FILTERS: &[&str] = &[
    "truncate",
    "date",
    "relative",
    "humanize",
    "percent",
    "uppercase",
    "lowercase",
    "json",
    "default",
    "join",
];

// ---------------------------------------------------------------------------
// Template AST types
// ---------------------------------------------------------------------------

/// A single node in a parsed template tree.
#[derive(Debug, Clone, PartialEq)]
pub enum TemplateNode {
    /// Literal text -- emitted verbatim.
    Text(String),

    /// `{{variable}}` or `{{variable | filter1 | filter2}}` -- resolved
    /// against the current grain's fields + render context metadata.
    Variable {
        /// Field path (e.g. "subject", "confidence", "hash").
        name: String,
        /// Filter pipeline applied left-to-right after resolution.
        filters: Vec<Filter>,
    },

    /// `{{#each grains}}...{{/each}}` -- iterates over the grain result set.
    Each {
        /// Must be the literal string "grains".
        collection: String,
        body: Vec<TemplateNode>,
    },

    /// `{{#if field}}...{{/if}}` or `{{#if field}}...{{else}}...{{/if}}`
    Condition {
        /// Field name to test for truthiness.
        field: String,
        /// Nodes to render when the field is truthy.
        if_body: Vec<TemplateNode>,
        /// Nodes to render when the field is falsy (optional).
        else_body: Vec<TemplateNode>,
    },

    /// `{{^field}}...{{/field}}` -- inverted section (renders when field
    /// is falsy or absent).
    InvertedSection {
        field: String,
        body: Vec<TemplateNode>,
    },

    /// `{{#grain_type "fact"}}...{{/grain_type}}` -- renders body only
    /// when the current grain's type matches.
    GrainTypeBlock {
        /// The grain type name (singular, lowercase).
        grain_type: String,
        body: Vec<TemplateNode>,
    },

    /// `{{! comment text }}` -- stripped during rendering, preserved in AST.
    Comment(String),
}

/// A single filter in a variable's pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// Filter name (e.g. "truncate", "date", "relative").
    pub name: String,
    /// Optional argument (e.g. "80" for truncate, "%Y-%m-%d" for date).
    pub arg: Option<String>,
}

// ---------------------------------------------------------------------------
// Template struct
// ---------------------------------------------------------------------------

/// A parsed, validated template ready for rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    /// Original source text (preserved for `get_template` responses).
    source: String,
    /// Parsed AST nodes.
    nodes: Vec<TemplateNode>,
}

impl Template {
    /// Return the original template source.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Return the parsed node tree (for introspection/debugging).
    pub fn nodes(&self) -> &[TemplateNode] {
        &self.nodes
    }
}

// ---------------------------------------------------------------------------
// ResolvedValue
// ---------------------------------------------------------------------------

/// A resolved template variable value.
#[derive(Debug, Clone)]
pub enum ResolvedValue {
    Str(String),
    Number(f64),
    Integer(i64),
    Bool(bool),
    Array(Vec<String>),
    Null,
}

impl ResolvedValue {
    /// Truthiness test for conditional sections.
    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Str(s) => !s.is_empty(),
            Self::Number(n) => *n != 0.0,
            Self::Integer(n) => *n != 0,
            Self::Bool(b) => *b,
            Self::Array(a) => !a.is_empty(),
            Self::Null => false,
        }
    }

    /// Render to string for output.
    pub fn to_display(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::Number(n) => {
                // Avoid trailing .0 for whole numbers
                if *n == (*n as i64) as f64 {
                    format!("{}", *n as i64)
                } else {
                    format!("{:.2}", n)
                }
            }
            Self::Integer(n) => format!("{}", n),
            Self::Bool(b) => if *b { "true" } else { "false" }.to_string(),
            Self::Array(a) => a.join(", "),
            Self::Null => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Disclosure tiers
// ---------------------------------------------------------------------------

/// Disclosure tier for progressive rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisclosureTier {
    /// ~50 tokens/grain -- minimal detail.
    Summary,
    /// ~150 tokens/grain -- moderate detail.
    Headlines,
    /// No limit -- full detail.
    Full,
}

// ---------------------------------------------------------------------------
// Render context
// ---------------------------------------------------------------------------

/// Render context passed to the template renderer.
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// Current Unix timestamp for relative time calculations.
    pub now_secs: i64,
    /// Progressive disclosure tier.
    pub tier: DisclosureTier,
    /// Total grains being rendered (for `_count`).
    pub total_count: usize,
    /// User-injected display variables from `WITH VARS { ... }`.
    ///
    /// Accessible in templates via `{{$key}}` syntax. String-only.
    pub user_vars: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Template name validation
// ---------------------------------------------------------------------------

/// Validate a template name: `^[a-zA-Z][a-zA-Z0-9 _-]{0,63}$`
/// Allows mixed case, spaces, hyphens, underscores, and digits.
/// Rejects leading/trailing whitespace and consecutive spaces.
/// Hand-validated without regex crate.
pub fn is_valid_template_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    // Reject leading/trailing whitespace and consecutive spaces.
    if name != name.trim() || name.contains("  ") {
        return false;
    }
    let bytes = name.as_bytes();
    // First character must be a letter (upper or lower).
    if !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    // Remaining characters: letter, digit, space, hyphen, or underscore.
    for &b in &bytes[1..] {
        if !(b.is_ascii_alphanumeric() || b == b' ' || b == b'-' || b == b'_') {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Template parsing
// ---------------------------------------------------------------------------

/// Parse and validate a template string.
///
/// Returns `CalError` on invalid syntax, unknown variables, or unknown filters.
/// Single-pass O(n) character scanner.
pub fn parse_template(source: &str) -> CalResult<Template> {
    // 1. Check size limit (64KB).
    if source.len() > MAX_TEMPLATE_SIZE {
        return Err(CalError::TemplateTooLarge {
            size: source.len(),
            max: MAX_TEMPLATE_SIZE,
            span: Some(Span::zero()),
        });
    }

    // 2. Scan characters, building a Vec<TemplateNode>.
    let chars: Vec<char> = source.chars().collect();
    let nodes = parse_nodes(&chars, 0, &mut 0, false, None)?;

    // 3. Validate: known variables in proper context.
    validate_variables(&nodes, None)?;

    Ok(Template {
        source: source.to_string(),
        nodes,
    })
}

/// Parse template nodes from a character slice. `in_each` tracks whether we
/// are inside an `{{#each}}` block (to prohibit nesting). `grain_type_ctx`
/// tracks the enclosing grain_type block for variable validation.
///
/// Returns the parsed nodes and advances `pos` past the consumed characters.
fn parse_nodes(
    chars: &[char],
    start: usize,
    pos: &mut usize,
    in_each: bool,
    stop_tag: Option<&str>,
) -> CalResult<Vec<TemplateNode>> {
    let mut nodes = Vec::new();
    *pos = start;

    while *pos < chars.len() {
        if *pos + 1 < chars.len() && chars[*pos] == '{' && chars[*pos + 1] == '{' {
            // Check for closing tags first
            let tag_start = *pos;
            let tag_content = read_tag(chars, pos)?;

            let trimmed = tag_content.trim();

            // Comment: {{! ... }}
            if let Some(stripped) = trimmed.strip_prefix('!') {
                let comment_text = stripped.trim();
                nodes.push(TemplateNode::Comment(comment_text.to_string()));
                continue;
            }

            // Closing tag: {{/...}}
            if let Some(stripped) = trimmed.strip_prefix('/') {
                let close_name = stripped.trim();
                if let Some(expected) = stop_tag {
                    if close_name == expected {
                        return Ok(nodes);
                    }
                }
                return Err(CalError::TemplateSyntaxError {
                    detail: format!("unexpected closing tag \"{}\"", close_name),
                    span: Some(make_span(tag_start, *pos)),
                });
            }

            // Else tag: {{else}}
            if trimmed == "else" {
                if let Some(expected) = stop_tag {
                    if expected == "if" || expected.starts_with("if:") {
                        // Push a sentinel and return; caller handles else
                        nodes.push(TemplateNode::Comment("__else__".to_string()));
                        // Continue parsing for else body (handled by caller)
                        return Ok(nodes);
                    }
                }
                return Err(CalError::TemplateSyntaxError {
                    detail: "{{else}} without matching {{#if}}".to_string(),
                    span: Some(make_span(tag_start, *pos)),
                });
            }

            // Section open: {{#...}}
            if let Some(stripped) = trimmed.strip_prefix('#') {
                let section = stripped.trim();
                if section.starts_with("each ") || section == "each" {
                    // {{#each grains}}
                    if in_each {
                        return Err(CalError::TemplateNestedEach {
                            span: Some(make_span(tag_start, *pos)),
                        });
                    }
                    let collection = section.strip_prefix("each").unwrap_or("").trim();
                    let collection = if collection.is_empty() {
                        "grains"
                    } else {
                        collection
                    };
                    if collection != "grains" {
                        return Err(CalError::TemplateSyntaxError {
                            detail: format!(
                                "{{{{#each}}}} only supports \"grains\", got \"{}\"",
                                collection
                            ),
                            span: Some(make_span(tag_start, *pos)),
                        });
                    }
                    let body = parse_nodes(chars, *pos, pos, true, Some("each"))?;
                    nodes.push(TemplateNode::Each {
                        collection: collection.to_string(),
                        body,
                    });
                } else if section.starts_with("if ") || section == "if" {
                    // {{#if field}}
                    let field = section.strip_prefix("if").unwrap_or("").trim().to_string();
                    if field.is_empty() {
                        return Err(CalError::TemplateSyntaxError {
                            detail: "{{#if}} requires a field name".to_string(),
                            span: Some(make_span(tag_start, *pos)),
                        });
                    }
                    let if_body = parse_nodes(chars, *pos, pos, in_each, Some("if"))?;

                    // Check if we got an {{else}} sentinel
                    let has_else = if_body
                        .last()
                        .is_some_and(|n| matches!(n, TemplateNode::Comment(c) if c == "__else__"));

                    let (if_body, else_body) = if has_else {
                        let if_nodes: Vec<TemplateNode> = if_body
                            .into_iter()
                            .filter(|n| !matches!(n, TemplateNode::Comment(c) if c == "__else__"))
                            .collect();
                        let else_body = parse_nodes(chars, *pos, pos, in_each, Some("if"))?;
                        (if_nodes, else_body)
                    } else {
                        (if_body, Vec::new())
                    };

                    nodes.push(TemplateNode::Condition {
                        field,
                        if_body,
                        else_body,
                    });
                } else if section.starts_with("grain_type ") {
                    // {{#grain_type "fact"}}
                    let rest = section.strip_prefix("grain_type").unwrap_or("").trim();
                    let grain_type =
                        unquote(rest).ok_or_else(|| CalError::TemplateSyntaxError {
                            detail: format!(
                                "{{{{#grain_type}}}} requires a quoted type name, got \"{}\"",
                                rest
                            ),
                            span: Some(make_span(tag_start, *pos)),
                        })?;
                    let body = parse_nodes(chars, *pos, pos, in_each, Some("grain_type"))?;
                    nodes.push(TemplateNode::GrainTypeBlock { grain_type, body });
                } else {
                    return Err(CalError::TemplateSyntaxError {
                        detail: format!("unknown section type \"{}\"", section),
                        span: Some(make_span(tag_start, *pos)),
                    });
                }
                continue;
            }

            // Inverted section: {{^field}}
            if let Some(stripped) = trimmed.strip_prefix('^') {
                let field = stripped.trim().to_string();
                if field.is_empty() {
                    return Err(CalError::TemplateSyntaxError {
                        detail: "{{^}} requires a field name".to_string(),
                        span: Some(make_span(tag_start, *pos)),
                    });
                }
                let body = parse_nodes(chars, *pos, pos, in_each, Some(&field))?;
                nodes.push(TemplateNode::InvertedSection { field, body });
                continue;
            }

            // Variable (possibly with filters): {{name | filter1 | filter2}}
            let (var_name, filters) = parse_variable_and_filters(trimmed, tag_start, *pos)?;
            nodes.push(TemplateNode::Variable {
                name: var_name,
                filters,
            });
        } else {
            // Literal text -- accumulate until the next `{{`.
            let text_start = *pos;
            while *pos < chars.len() {
                if *pos + 1 < chars.len() && chars[*pos] == '{' && chars[*pos + 1] == '{' {
                    break;
                }
                *pos += 1;
            }
            let text: String = chars[text_start..*pos].iter().collect();
            if !text.is_empty() {
                nodes.push(TemplateNode::Text(text));
            }
        }
    }

    // If we expected a closing tag but hit EOF, that's an error.
    if let Some(expected) = stop_tag {
        return Err(CalError::TemplateSyntaxError {
            detail: format!("unclosed {{{{#{}}}}} block", expected),
            span: Some(make_span(start, chars.len())),
        });
    }

    Ok(nodes)
}

/// Read a `{{ ... }}` tag, advancing `pos` past the closing `}}`.
/// Returns the content between the delimiters (trimmed of delimiters).
fn read_tag(chars: &[char], pos: &mut usize) -> CalResult<String> {
    let tag_start = *pos;
    // Skip opening `{{`
    *pos += 2;

    let content_start = *pos;
    let mut depth = 1;

    while *pos < chars.len() {
        if *pos + 1 < chars.len() && chars[*pos] == '}' && chars[*pos + 1] == '}' {
            depth -= 1;
            if depth == 0 {
                let content: String = chars[content_start..*pos].iter().collect();
                *pos += 2; // Skip closing `}}`
                return Ok(content);
            }
        }
        if *pos + 1 < chars.len() && chars[*pos] == '{' && chars[*pos + 1] == '{' {
            depth += 1;
            *pos += 2;
            continue;
        }
        *pos += 1;
    }

    Err(CalError::TemplateSyntaxError {
        detail: "unclosed template tag (missing `}}`)".to_string(),
        span: Some(make_span(tag_start, *pos)),
    })
}

/// Parse a variable expression with optional filter pipeline.
/// Input: `"name | filter1 arg | filter2"` (already trimmed of `{{ }}`).
fn parse_variable_and_filters(
    expr: &str,
    tag_start: usize,
    tag_end: usize,
) -> CalResult<(String, Vec<Filter>)> {
    let parts = split_pipe(expr);
    if parts.is_empty() {
        return Err(CalError::TemplateSyntaxError {
            detail: "empty variable expression".to_string(),
            span: Some(make_span(tag_start, tag_end)),
        });
    }

    let var_name = parts[0].trim().to_string();
    if var_name.is_empty() {
        return Err(CalError::TemplateSyntaxError {
            detail: "empty variable name".to_string(),
            span: Some(make_span(tag_start, tag_end)),
        });
    }

    let mut filters = Vec::new();
    for part in &parts[1..] {
        let filter = parse_single_filter(part.trim(), tag_start, tag_end)?;
        filters.push(filter);
    }

    Ok((var_name, filters))
}

/// Parse a single filter segment like `"truncate 80"` or `"relative"` or
/// `"date \"%Y-%m-%d\""`.
fn parse_single_filter(segment: &str, tag_start: usize, tag_end: usize) -> CalResult<Filter> {
    let segment = segment.trim();
    if segment.is_empty() {
        return Err(CalError::TemplateSyntaxError {
            detail: "empty filter name".to_string(),
            span: Some(make_span(tag_start, tag_end)),
        });
    }

    // Split on first whitespace or opening paren to get name and optional argument.
    // Supports both `truncate 5` (space-separated) and `truncate(5)` (parenthesized).
    let (name, arg) = if let Some(paren_idx) = segment.find('(') {
        // Parenthesized syntax: name(arg)
        let name = segment[..paren_idx].trim();
        let rest = &segment[paren_idx + 1..];
        let close = rest.rfind(')').unwrap_or(rest.len());
        let raw_arg = rest[..close].trim();
        let arg = unquote(raw_arg).unwrap_or_else(|| raw_arg.to_string());
        (name, Some(arg))
    } else {
        match segment.find(|c: char| c.is_whitespace()) {
            Some(idx) => {
                let name = &segment[..idx];
                let raw_arg = segment[idx..].trim();
                let arg = unquote(raw_arg).unwrap_or_else(|| raw_arg.to_string());
                (name, Some(arg))
            }
            None => (segment, None),
        }
    };

    // Validate filter name is known.
    if !KNOWN_FILTERS.contains(&name) {
        return Err(CalError::TemplateUnknownFilter {
            name: name.to_string(),
            span: Some(make_span(tag_start, tag_end)),
        });
    }

    // Parse-time validation of filter arguments.
    match name {
        "truncate" => {
            if let Some(ref a) = arg {
                match a.parse::<usize>() {
                    Ok(n) if (1..=100_000).contains(&n) => {}
                    Ok(n) => {
                        return Err(CalError::TemplateSyntaxError {
                            detail: format!("truncate argument {} out of range [1, 100000]", n),
                            span: Some(make_span(tag_start, tag_end)),
                        });
                    }
                    Err(_) => {
                        return Err(CalError::TemplateSyntaxError {
                            detail: format!("truncate argument \"{}\" is not a valid number", a),
                            span: Some(make_span(tag_start, tag_end)),
                        });
                    }
                }
            }
        }
        "date" => {
            if let Some(ref a) = arg {
                if a.len() > 256 {
                    return Err(CalError::TemplateSyntaxError {
                        detail: format!("date format string too long ({} chars, max 256)", a.len()),
                        span: Some(make_span(tag_start, tag_end)),
                    });
                }
            }
        }
        _ => {}
    }

    Ok(Filter {
        name: name.to_string(),
        arg,
    })
}

/// Split an expression on `|` characters, respecting quoted strings.
fn split_pipe(expr: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quote = false;
    let mut quote_char = '"';

    for ch in expr.chars() {
        if in_quote {
            current.push(ch);
            if ch == quote_char {
                in_quote = false;
            }
        } else if ch == '"' || ch == '\'' {
            in_quote = true;
            quote_char = ch;
            current.push(ch);
        } else if ch == '|' {
            parts.push(current.clone());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() || parts.is_empty() {
        parts.push(current);
    }
    parts
}

/// Remove surrounding quotes from a string if present.
fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        return Some(s[1..s.len() - 1].to_string());
    }
    // Return the string as-is if not quoted (for unquoted arguments).
    if !s.is_empty() {
        Some(s.to_string())
    } else {
        None
    }
}

/// Create a Span from byte offsets (approximated from char positions).
fn make_span(start: usize, end: usize) -> Span {
    Span::new(start, end, 1, start + 1)
}

// ---------------------------------------------------------------------------
// Variable validation (parse-time)
// ---------------------------------------------------------------------------

/// Validate all variables in the node tree against the closed variable set.
fn validate_variables(nodes: &[TemplateNode], grain_type_ctx: Option<&str>) -> CalResult<()> {
    for node in nodes {
        match node {
            TemplateNode::Variable { name, .. } => {
                validate_variable_name(name, grain_type_ctx)?;
            }
            TemplateNode::Each { body, .. } => {
                validate_variables(body, grain_type_ctx)?;
            }
            TemplateNode::Condition {
                field,
                if_body,
                else_body,
            } => {
                validate_variable_name(field, grain_type_ctx)?;
                validate_variables(if_body, grain_type_ctx)?;
                validate_variables(else_body, grain_type_ctx)?;
            }
            TemplateNode::InvertedSection { field, body } => {
                validate_variable_name(field, grain_type_ctx)?;
                validate_variables(body, grain_type_ctx)?;
            }
            TemplateNode::GrainTypeBlock { grain_type, body } => {
                validate_variables(body, Some(grain_type))?;
            }
            TemplateNode::Text(_) | TemplateNode::Comment(_) => {}
        }
    }
    Ok(())
}

/// Validate a single variable name against the closed set.
///
/// `$`-prefixed names are user-injected display variables (from `WITH VARS`)
/// and bypass the closed set check — they are validated at query time, not
/// template definition time.
fn validate_variable_name(name: &str, _grain_type_ctx: Option<&str>) -> CalResult<()> {
    // User-injected display variables: $-prefixed, bypass closed set.
    if let Some(stripped) = name.strip_prefix('$') {
        // Basic validation: must be a valid identifier after stripping $.
        if !stripped.is_empty()
            && (stripped.as_bytes()[0].is_ascii_alphabetic() || stripped.as_bytes()[0] == b'_')
            && stripped
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Ok(());
        }
        return Err(CalError::TemplateUnknownVariable {
            name: name.to_string(),
            span: None,
            suggestion: Some("user variable names must match $[a-zA-Z_][a-zA-Z0-9_]*".into()),
        });
    }

    if ALL_VALID_VARIABLES.contains(&name) {
        return Ok(());
    }

    // Try to suggest a close match.
    let suggestion = find_closest_variable(name);
    Err(CalError::TemplateUnknownVariable {
        name: name.to_string(),
        span: None,
        suggestion,
    })
}

/// Simple Levenshtein-like closest match for unknown variable suggestions.
fn find_closest_variable(name: &str) -> Option<String> {
    let mut best: Option<(&str, usize)> = None;
    for &var in ALL_VALID_VARIABLES {
        let dist = simple_edit_distance(name, var);
        if dist <= 3 && best.is_none_or(|(_, d)| dist < d) {
            best = Some((var, dist));
        }
    }
    best.map(|(v, _)| format!("did you mean \"{}\"?", v))
}

/// Simple edit distance (substitution-only for performance).
fn simple_edit_distance(a: &str, b: &str) -> usize {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let len = a_bytes.len().max(b_bytes.len());
    let min_len = a_bytes.len().min(b_bytes.len());
    let mut dist = len - min_len;
    for i in 0..min_len {
        if a_bytes[i] != b_bytes[i] {
            dist += 1;
        }
    }
    dist
}

// ---------------------------------------------------------------------------
// Variable resolution (runtime)
// ---------------------------------------------------------------------------

/// Resolve a template variable against grain fields and render context.
fn resolve_variable(
    name: &str,
    grain: Option<&CalGrainResult>,
    index: Option<usize>,
    ctx: &RenderContext,
) -> ResolvedValue {
    // 0. User-injected display variables ($prefix).
    if let Some(var_name) = name.strip_prefix('$') {
        return match ctx.user_vars.get(var_name) {
            Some(val) => ResolvedValue::Str(val.clone()),
            None => ResolvedValue::Null,
        };
    }

    // 1. Metadata variables (_prefix).
    match name {
        "_index" => return ResolvedValue::Integer(index.unwrap_or(0) as i64),
        "_count" => return ResolvedValue::Integer(ctx.total_count as i64),
        "_first" => return ResolvedValue::Bool(index == Some(0)),
        "_last" => return ResolvedValue::Bool(index.is_some_and(|i| i + 1 == ctx.total_count)),
        "_now" => return ResolvedValue::Integer(ctx.now_secs),
        _ => {}
    }

    let Some(grain) = grain else {
        return ResolvedValue::Null;
    };

    // 2. Direct CalGrainResult fields.
    match name {
        "hash" => return ResolvedValue::Str(grain.hash.clone()),
        "grain_type" => return ResolvedValue::Str(grain.grain_type.clone()),
        "score" => return ResolvedValue::Number(grain.score),
        _ => {}
    }

    // 3. Defense-in-depth: check ALLOWED_FIELDS (F5).
    if !ALLOWED_FIELDS.contains(&name) {
        return ResolvedValue::Null;
    }

    // 4. Lookup in grain.fields.
    resolve_from_fields(&grain.fields, name)
}

/// Extract a value from a serde_json::Value object by field name.
fn resolve_from_fields(fields: &serde_json::Value, name: &str) -> ResolvedValue {
    let Some(val) = fields.get(name) else {
        return ResolvedValue::Null;
    };

    match val {
        serde_json::Value::String(s) => ResolvedValue::Str(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ResolvedValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ResolvedValue::Number(f)
            } else {
                ResolvedValue::Str(n.to_string())
            }
        }
        serde_json::Value::Bool(b) => ResolvedValue::Bool(*b),
        serde_json::Value::Array(arr) => {
            let strings: Vec<String> = arr
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            ResolvedValue::Array(strings)
        }
        serde_json::Value::Null => ResolvedValue::Null,
        serde_json::Value::Object(_) => {
            // Render objects as JSON.
            ResolvedValue::Str(val.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Filter pipeline
// ---------------------------------------------------------------------------

/// Apply a filter to a resolved value.
pub fn apply_filter(
    value: &ResolvedValue,
    filter: &Filter,
    ctx: &RenderContext,
) -> CalResult<ResolvedValue> {
    match filter.name.as_str() {
        "truncate" => {
            // F2 safety: argument validated at parse time to be in [1, 100_000].
            let n: usize = filter
                .arg
                .as_ref()
                .and_then(|a| a.parse().ok())
                .unwrap_or(80);
            // Apply tier-based effective truncation.
            let effective = effective_truncate(Some(n), ctx.tier);
            let s = value.to_display();
            if char_len(&s) > effective {
                let truncated = truncate_at_char_boundary(&s, effective);
                Ok(ResolvedValue::Str(format!("{}...", truncated)))
            } else {
                Ok(ResolvedValue::Str(s))
            }
        }
        "relative" => match value {
            ResolvedValue::Integer(epoch) => Ok(ResolvedValue::Str(
                super::humanize::humanize_time(*epoch, ctx.now_secs),
            )),
            ResolvedValue::Number(n) => Ok(ResolvedValue::Str(super::humanize::humanize_time(
                *n as i64,
                ctx.now_secs,
            ))),
            _ => Ok(ResolvedValue::Str(value.to_display())),
        },
        "humanize" => {
            let s = value.to_display();
            Ok(ResolvedValue::Str(super::humanize::humanize_relation(&s)))
        }
        "percent" => match value {
            ResolvedValue::Number(n) => Ok(ResolvedValue::Str(format!("{}%", (*n * 100.0) as u32))),
            ResolvedValue::Integer(n) => Ok(ResolvedValue::Str(format!("{}%", *n * 100))),
            _ => Ok(ResolvedValue::Str(value.to_display())),
        },
        "uppercase" => Ok(ResolvedValue::Str(value.to_display().to_uppercase())),
        "lowercase" => Ok(ResolvedValue::Str(value.to_display().to_lowercase())),
        "json" => {
            // Render the value as a JSON-compatible string.
            let s = match value {
                ResolvedValue::Str(s) => {
                    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
                }
                ResolvedValue::Number(n) => format!("{}", n),
                ResolvedValue::Integer(n) => format!("{}", n),
                ResolvedValue::Bool(b) => format!("{}", b),
                ResolvedValue::Array(a) => {
                    let items: Vec<String> = a
                        .iter()
                        .map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")))
                        .collect();
                    format!("[{}]", items.join(","))
                }
                ResolvedValue::Null => "null".to_string(),
            };
            Ok(ResolvedValue::Str(s))
        }
        "default" => {
            if value.is_truthy() {
                Ok(value.clone())
            } else {
                Ok(ResolvedValue::Str(filter.arg.clone().unwrap_or_default()))
            }
        }
        "join" => {
            let sep = filter.arg.as_deref().unwrap_or(", ");
            match value {
                ResolvedValue::Array(items) => Ok(ResolvedValue::Str(items.join(sep))),
                _ => Ok(ResolvedValue::Str(value.to_display())),
            }
        }
        "date" => match value {
            ResolvedValue::Integer(epoch) => {
                let fmt = filter.arg.as_deref().unwrap_or("%Y-%m-%d");
                Ok(ResolvedValue::Str(format_epoch(*epoch, fmt)))
            }
            ResolvedValue::Number(n) => {
                let fmt = filter.arg.as_deref().unwrap_or("%Y-%m-%d");
                Ok(ResolvedValue::Str(format_epoch(*n as i64, fmt)))
            }
            _ => Ok(ResolvedValue::Str(value.to_display())),
        },
        unknown => Err(CalError::TemplateUnknownFilter {
            name: unknown.to_string(),
            span: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Date formatting (zero-dependency)
// ---------------------------------------------------------------------------

/// Format a Unix epoch timestamp using a simple format string.
///
/// Supported placeholders: `%Y`, `%m`, `%d`, `%H`, `%M`, `%S`.
/// UTC only, post-epoch only. Negative epochs return `"<invalid date>"` (F3).
pub fn format_epoch(epoch_secs: i64, fmt: &str) -> String {
    // F3 safety: reject negative epochs.
    if epoch_secs < 0 {
        return "<invalid date>".to_string();
    }
    // F4 safety: format string length re-checked at runtime as defense-in-depth.
    if fmt.len() > 256 {
        return "<format too long>".to_string();
    }

    // Extract H/M/S from time-of-day seconds (safe: epoch_secs >= 0).
    let day_secs = (epoch_secs % 86400) as u32;
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;

    // Date from civil_from_days (Howard Hinnant algorithm).
    let total_days = (epoch_secs / 86400) as i32;
    let z = total_days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if mo <= 2 { y + 1 } else { y };

    // Replace format placeholders.
    let mut result = String::with_capacity(fmt.len() + 16);
    let fchars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < fchars.len() {
        if fchars[i] == '%' && i + 1 < fchars.len() {
            match fchars[i + 1] {
                'Y' => {
                    result.push_str(&format!("{:04}", year));
                    i += 2;
                }
                'm' => {
                    result.push_str(&format!("{:02}", mo));
                    i += 2;
                }
                'd' => {
                    result.push_str(&format!("{:02}", d));
                    i += 2;
                }
                'H' => {
                    result.push_str(&format!("{:02}", h));
                    i += 2;
                }
                'M' => {
                    result.push_str(&format!("{:02}", m));
                    i += 2;
                }
                'S' => {
                    result.push_str(&format!("{:02}", s));
                    i += 2;
                }
                _ => {
                    result.push('%');
                    result.push(fchars[i + 1]);
                    i += 2;
                }
            }
        } else {
            result.push(fchars[i]);
            i += 1;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// String helpers
// ---------------------------------------------------------------------------

/// Count the number of characters in a string.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Truncate a string at a character boundary (not byte boundary).
fn truncate_at_char_boundary(s: &str, max_chars: usize) -> &str {
    if max_chars == 0 {
        return "";
    }
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// Truncate a string, appending "..." if truncated. Used by default_grain_render.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if char_len(s) > max_chars {
        format!("{}...", truncate_at_char_boundary(s, max_chars))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Progressive disclosure
// ---------------------------------------------------------------------------

/// Select disclosure tier based on available tokens per grain.
pub fn select_tier(budget_tokens: u32, grain_count: usize) -> DisclosureTier {
    if grain_count == 0 {
        return DisclosureTier::Full;
    }
    // F7 safety: clamp grain_count to u32::MAX before cast to prevent
    // silent truncation to zero on 64-bit platforms.
    let clamped_count = grain_count.min(u32::MAX as usize) as u32;
    if clamped_count == 0 {
        return DisclosureTier::Summary;
    }
    let tokens_per_grain = budget_tokens / clamped_count;
    if tokens_per_grain >= 200 {
        DisclosureTier::Full
    } else if tokens_per_grain >= 80 {
        DisclosureTier::Headlines
    } else {
        DisclosureTier::Summary
    }
}

/// Compute effective truncation limit based on explicit limit and tier.
fn effective_truncate(explicit_limit: Option<usize>, tier: DisclosureTier) -> usize {
    match explicit_limit {
        Some(n) => match tier {
            DisclosureTier::Summary => n.min(40),
            DisclosureTier::Headlines => n.min(80),
            DisclosureTier::Full => n,
        },
        None => match tier {
            DisclosureTier::Summary => 40,
            DisclosureTier::Headlines => 80,
            DisclosureTier::Full => usize::MAX,
        },
    }
}

// ---------------------------------------------------------------------------
// Template renderer
// ---------------------------------------------------------------------------

/// Render a grain result set using a parsed template.
pub fn render(
    template: &Template,
    grains: &[CalGrainResult],
    ctx: &RenderContext,
) -> CalResult<String> {
    render_with_limit(template, grains, ctx, MAX_RENDER_OUTPUT_SIZE)
}

/// Render with a configurable output size limit.
pub fn render_with_limit(
    template: &Template,
    grains: &[CalGrainResult],
    ctx: &RenderContext,
    max_output_size: usize,
) -> CalResult<String> {
    let mut output = String::with_capacity(grains.len().min(1024) * 128);
    render_nodes(
        &template.nodes,
        grains,
        None,
        ctx,
        &mut output,
        max_output_size,
    )?;
    Ok(output)
}

/// Recursively render template nodes into the output string.
fn render_nodes(
    nodes: &[TemplateNode],
    grains: &[CalGrainResult],
    current_grain: Option<(&CalGrainResult, usize)>,
    ctx: &RenderContext,
    output: &mut String,
    max_output_size: usize,
) -> CalResult<()> {
    for node in nodes {
        match node {
            TemplateNode::Text(s) => output.push_str(s),

            TemplateNode::Variable { name, filters } => {
                let grain = current_grain.map(|(g, _)| g);
                let index = current_grain.map(|(_, i)| i);
                let mut value = resolve_variable(name, grain, index, ctx);
                for filter in filters {
                    value = apply_filter(&value, filter, ctx)?;
                }
                output.push_str(&value.to_display());
            }

            TemplateNode::Each { body, .. } => {
                for (i, grain) in grains.iter().enumerate() {
                    render_nodes(body, grains, Some((grain, i)), ctx, output, max_output_size)?;
                    // F1 safety: check output size after each grain iteration.
                    if output.len() > max_output_size {
                        return Err(CalError::RenderOutputTooLarge {
                            size: output.len(),
                            max: max_output_size,
                            span: None,
                        });
                    }
                }
            }

            TemplateNode::Condition {
                field,
                if_body,
                else_body,
            } => {
                let grain = current_grain.map(|(g, _)| g);
                let index = current_grain.map(|(_, i)| i);
                let value = resolve_variable(field, grain, index, ctx);
                if value.is_truthy() {
                    render_nodes(if_body, grains, current_grain, ctx, output, max_output_size)?;
                } else if !else_body.is_empty() {
                    render_nodes(
                        else_body,
                        grains,
                        current_grain,
                        ctx,
                        output,
                        max_output_size,
                    )?;
                }
            }

            TemplateNode::InvertedSection { field, body } => {
                let grain = current_grain.map(|(g, _)| g);
                let index = current_grain.map(|(_, i)| i);
                let value = resolve_variable(field, grain, index, ctx);
                if !value.is_truthy() {
                    render_nodes(body, grains, current_grain, ctx, output, max_output_size)?;
                }
            }

            TemplateNode::GrainTypeBlock { grain_type, body } => {
                if let Some((grain, _)) = current_grain {
                    if grain.grain_type == *grain_type {
                        render_nodes(body, grains, current_grain, ctx, output, max_output_size)?;
                    }
                }
            }

            TemplateNode::Comment(_) => { /* skip */ }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Default grain rendering (fallback)
// ---------------------------------------------------------------------------

/// Helpers for extracting typed fields from grain.fields JSON.
fn field_str(grain: &CalGrainResult, name: &str) -> String {
    match grain.fields.get(name) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

fn field_str_or(grain: &CalGrainResult, name: &str, default: &str) -> String {
    let s = field_str(grain, name);
    if s.is_empty() {
        default.to_string()
    } else {
        s
    }
}

fn field_f64(grain: &CalGrainResult, name: &str) -> f64 {
    match grain.fields.get(name) {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn field_i64(grain: &CalGrainResult, name: &str) -> i64 {
    match grain.fields.get(name) {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn field_bool(grain: &CalGrainResult, name: &str) -> bool {
    match grain.fields.get(name) {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => false,
    }
}

fn format_confidence(conf: f64) -> String {
    format!("{}%", (conf * 100.0) as u32)
}

/// Default one-line rendering per grain type (used when no grain_type block
/// matches or when the template has no grain_type blocks).
pub fn default_grain_render(grain: &CalGrainResult, ctx: &RenderContext) -> String {
    match grain.grain_type.as_str() {
        "fact" => format!(
            "{} {} {} ({})",
            field_str(grain, "subject"),
            super::humanize::humanize_relation(&field_str(grain, "relation")),
            field_str(grain, "object"),
            format_confidence(field_f64(grain, "confidence")),
        ),
        "event" => format!(
            "{} {} [{}]",
            field_str(grain, "actor"),
            field_str(grain, "action"),
            super::humanize::humanize_time(field_i64(grain, "created_at"), ctx.now_secs),
        ),
        "state" => format!(
            "{}: {} ({})",
            field_str(grain, "entity"),
            field_str(grain, "state_value"),
            field_str_or(grain, "validity", "current"),
        ),
        "workflow" => {
            let node_count = grain
                .fields
                .get("nodes")
                .and_then(|v: &serde_json::Value| v.as_array())
                .map(|a: &Vec<serde_json::Value>| a.len())
                .unwrap_or(0);
            let edge_count = grain
                .fields
                .get("edges")
                .and_then(|v: &serde_json::Value| v.as_array())
                .map(|a: &Vec<serde_json::Value>| a.len())
                .unwrap_or(0);
            format!(
                "{} ({} nodes, {} edges, {})",
                field_str(grain, "name"),
                node_count,
                edge_count,
                field_str(grain, "status"),
            )
        }
        "tool" => format!(
            "{}({}) -> {}{}",
            field_str(grain, "tool_name"),
            truncate_str(&field_str(grain, "input"), 40),
            truncate_str(&field_str(grain, "content"), 60),
            if field_bool(grain, "is_error") {
                " [ERROR]"
            } else {
                ""
            },
        ),
        "observation" => format!(
            "{} observed {}: {}",
            field_str(grain, "observer"),
            field_str(grain, "observed"),
            truncate_str(&field_str(grain, "sensory_data"), 60),
        ),
        "goal" => format!(
            "{} - {} ({})",
            field_str(grain, "goal_state"),
            field_str_or(grain, "assigned_to", "unassigned"),
            field_str_or(grain, "priority", "normal"),
        ),
        "reasoning" => format!(
            "{} -> {} ({})",
            truncate_str(&field_str(grain, "premises"), 60),
            field_str(grain, "conclusion"),
            format_confidence(field_f64(grain, "confidence")),
        ),
        "consensus" => format!(
            "{}: {}",
            field_str(grain, "participants"),
            field_str(grain, "agreement_level"),
        ),
        "consent" => format!(
            "{}: {} [{}]",
            field_str(grain, "consenter"),
            field_str(grain, "scope"),
            field_str_or(grain, "expires_at", "no expiry"),
        ),
        // Skill — data projection only (name + description + domain +
        // proficiency). `instructions`/`when_to_use` are deliberately NOT
        // interpolated raw into the template (design §13 non-blocking note).
        "skill" => format!(
            "{}: {} [{}] ({})",
            field_str(grain, "name"),
            truncate_str(&field_str(grain, "description"), 60),
            field_str_or(grain, "domain", "general"),
            format_confidence(field_f64(grain, "proficiency")),
        ),
        _ => format!("{}: {}", grain.grain_type, grain.hash),
    }
}

// ---------------------------------------------------------------------------
// Template registry
// ---------------------------------------------------------------------------

/// A registered template entry.
#[derive(Debug, Clone)]
pub struct TemplateEntry {
    /// The parsed template AST.
    pub template: Template,
    /// Whether this is a built-in (non-deletable) template.
    pub builtin: bool,
    /// Optional parent template name (for inheritance, 1-level only).
    pub parent: Option<String>,
    /// Human-readable description.
    pub description: String,
    /// Supported grain types (empty = all).
    pub grain_types: Vec<String>,
    /// Last time this template was used in a FORMAT preset render (epoch secs).
    pub last_run_at: Option<u64>,
    /// Last time this template was created or updated (epoch secs).
    pub updated_at: Option<u64>,
}

/// Summary of a registered template (for listing).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateListEntry {
    pub name: String,
    pub description: String,
    pub builtin: bool,
    pub parent: Option<String>,
    pub grain_types: Vec<String>,
    /// Original template source text (for preset rendering).
    pub source: String,
    pub last_run_at: Option<u64>,
    pub updated_at: Option<u64>,
}

/// Detailed template info (for `get_template` responses).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateInfo {
    pub name: String,
    pub description: String,
    pub builtin: bool,
    pub parent: Option<String>,
    pub source: String,
}

/// Serialization format for persisting custom templates to Fjall meta partition (FR-003).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PersistedTemplate {
    pub source: String,
    pub description: String,
    pub parent: Option<String>,
    pub grain_types: Vec<String>,
    #[serde(default)]
    pub last_run_at: Option<u64>,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

/// In-memory template registry.
///
/// Thread-safe when wrapped in `Arc<RwLock<...>>` externally. The
/// `CalExecutor` holds this and passes it by reference to the renderer.
#[derive(Debug, Clone)]
pub struct TemplateRegistry {
    templates: HashMap<String, TemplateEntry>,
}

impl Default for TemplateRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TemplateRegistry {
    /// Create a new registry pre-loaded with the 6 built-in templates.
    pub fn new() -> Self {
        let mut reg = Self {
            templates: HashMap::new(),
        };
        reg.load_builtins();
        reg
    }

    /// Register a custom template. Returns error if name is invalid or
    /// a built-in template would be overwritten.
    pub fn register(
        &mut self,
        name: &str,
        source: &str,
        description: &str,
        parent: Option<&str>,
    ) -> CalResult<()> {
        // 1. Validate name: ^[a-zA-Z][a-zA-Z0-9 _-]{0,63}$
        if !is_valid_template_name(name) {
            return Err(CalError::TemplateInvalidName {
                name: name.to_string(),
                span: None,
            });
        }
        // 2. Cannot overwrite built-in.
        if self.templates.get(name).is_some_and(|e| e.builtin) {
            return Err(CalError::TemplateBuiltinImmutable {
                name: name.to_string(),
                span: None,
            });
        }
        // 3. Validate parent exists (if specified).
        if let Some(p) = parent {
            if !self.templates.contains_key(p) {
                return Err(CalError::TemplateParentNotFound {
                    name: name.to_string(),
                    parent: p.to_string(),
                    span: None,
                });
            }
            // Prevent 2-level inheritance.
            if self.templates[p].parent.is_some() {
                return Err(CalError::TemplateInheritanceDepth {
                    name: name.to_string(),
                    span: None,
                });
            }
        }
        // 4. Parse and validate.
        let template = parse_template(source)?;
        // 5. Store.
        self.templates.insert(
            name.to_string(),
            TemplateEntry {
                template,
                builtin: false,
                parent: parent.map(|s| s.to_string()),
                description: description.to_string(),
                grain_types: Vec::new(),
                last_run_at: None,
                updated_at: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                ),
            },
        );
        Ok(())
    }

    /// Get a template by name.
    pub fn get(&self, name: &str) -> Option<&TemplateEntry> {
        self.templates.get(name)
    }

    /// List all templates (name + description + builtin flag).
    pub fn list(&self) -> Vec<TemplateListEntry> {
        let mut entries: Vec<TemplateListEntry> = self
            .templates
            .iter()
            .map(|(name, entry)| TemplateListEntry {
                name: name.clone(),
                description: entry.description.clone(),
                builtin: entry.builtin,
                parent: entry.parent.clone(),
                grain_types: entry.grain_types.clone(),
                source: entry.template.source().to_string(),
                last_run_at: entry.last_run_at,
                updated_at: entry.updated_at,
            })
            .collect();
        entries.sort_by_key(|b| std::cmp::Reverse(b.updated_at.unwrap_or(0)));
        entries
    }

    /// Restore timestamps from persisted data (used during rehydration).
    pub fn restore_timestamps(
        &mut self,
        name: &str,
        last_run_at: Option<u64>,
        updated_at: Option<u64>,
    ) {
        if let Some(entry) = self.templates.get_mut(name) {
            if last_run_at.is_some() {
                entry.last_run_at = last_run_at;
            }
            if updated_at.is_some() {
                entry.updated_at = updated_at;
            }
        }
    }

    /// Record that a template was used (updates last_run_at).
    pub fn record_run(&mut self, name: &str) {
        if let Some(entry) = self.templates.get_mut(name) {
            entry.last_run_at = Some(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            );
        }
    }

    /// Delete a custom template. Returns error for built-in templates.
    pub fn delete(&mut self, name: &str) -> CalResult<()> {
        match self.templates.get(name) {
            None => Err(CalError::TemplateNotFound {
                name: name.to_string(),
                span: None,
            }),
            Some(entry) if entry.builtin => Err(CalError::TemplateBuiltinImmutable {
                name: name.to_string(),
                span: None,
            }),
            _ => {
                self.templates.remove(name);
                Ok(())
            }
        }
    }

    /// Load the 6 built-in templates.
    fn load_builtins(&mut self) {
        // -- triples --
        let triples_src = "{{#each grains}}{{subject}} {{relation | humanize}} {{object}}{{#if confidence}} ({{confidence | percent}}){{/if}}\n{{/each}}";
        self.insert_builtin(
            "triples",
            triples_src,
            "One-line triple per grain: subject relation object (confidence%)",
        );

        // -- progressive --
        let progressive_src = concat!(
            "{{! Progressive disclosure template -- tier selection happens at render time }}\n",
            "{{#each grains}}",
            "{{#grain_type \"fact\"}}{{subject}} {{relation | humanize}} {{object}}{{/grain_type}}",
            "{{#grain_type \"event\"}}{{actor}} {{action}}{{#if created_at}} [{{created_at | relative}}]{{/if}}{{/grain_type}}",
            "{{#grain_type \"state\"}}{{entity}}: {{state_value}}{{#if validity}} ({{validity}}){{/if}}{{/grain_type}}",
            "{{#grain_type \"workflow\"}}{{name}} ({{node_count}} nodes, {{edge_count}} edges) — {{status}}{{/grain_type}}",
            "{{#grain_type \"tool\"}}{{tool_name}}({{input | truncate 40}}) -> {{content | truncate 60}}{{#if is_error}} [ERROR]{{/if}}{{/grain_type}}",
            "{{#grain_type \"observation\"}}{{observer}} observed {{observed}}: {{sensory_data | truncate 60}}{{/grain_type}}",
            "{{#grain_type \"goal\"}}{{goal_state}} — {{assigned_to | default \"unassigned\"}} ({{priority | default \"normal\"}}){{/grain_type}}",
            "{{#grain_type \"reasoning\"}}{{premises | truncate 60}} -> {{conclusion}}{{#if confidence}} ({{confidence | percent}}){{/if}}{{/grain_type}}",
            "{{#grain_type \"consensus\"}}{{participants | join \", \"}}: {{agreement_level}}{{/grain_type}}",
            "{{#grain_type \"consent\"}}{{consenter}}: {{scope}}{{#if expires_at}} [expires {{expires_at | relative}}]{{/if}}{{/grain_type}}\n",
            "{{/each}}"
        );
        self.insert_builtin(
            "progressive",
            progressive_src,
            "Per-grain-type rendering with progressive disclosure tiers",
        );

        // -- llm_system_prompt --
        let llm_system_src = concat!(
            "<context>\n<memories count=\"{{_count}}\">\n",
            "{{#each grains}}<memory type=\"{{grain_type}}\" hash=\"{{hash}}\" confidence=\"{{confidence}}\">\n",
            "{{#grain_type \"fact\"}}{{subject}} {{relation | humanize}} {{object}}{{/grain_type}}",
            "{{#grain_type \"event\"}}{{actor}} {{action}} [{{created_at | relative}}]{{/grain_type}}",
            "{{#grain_type \"state\"}}{{entity}}: {{state_value}}{{/grain_type}}",
            "{{#grain_type \"workflow\"}}{{name}}: {{status}} ({{node_count}} nodes, {{edge_count}} edges){{/grain_type}}",
            "{{#grain_type \"tool\"}}{{tool_name}}({{input | truncate 60}}) -> {{content | truncate 100}}{{/grain_type}}",
            "{{#grain_type \"observation\"}}{{observer}}: {{sensory_data | truncate 80}}{{/grain_type}}",
            "{{#grain_type \"goal\"}}[{{goal_state}}] {{title | default \"untitled\"}} ({{priority | default \"normal\"}}){{/grain_type}}",
            "{{#grain_type \"reasoning\"}}{{premises | truncate 80}} => {{conclusion}}{{/grain_type}}",
            "{{#grain_type \"consensus\"}}{{participants | join \", \"}} agreed: {{agreement_level}}{{/grain_type}}",
            "{{#grain_type \"consent\"}}{{consenter}} granted: {{scope}}{{#if expires_at}} [until {{expires_at | date \"%Y-%m-%d\"}}]{{/if}}{{/grain_type}}\n",
            "</memory>\n{{/each}}</memories>\n</context>"
        );
        self.insert_builtin(
            "llm_system_prompt",
            llm_system_src,
            "SML-tagged context for LLM system prompts",
        );

        // -- llm_chat --
        let llm_chat_src = concat!(
            "**Relevant memories** ({{_count}} results):\n\n",
            "{{#each grains}}- ",
            "{{#grain_type \"fact\"}}**{{subject}}** {{relation | humanize}} {{object}}{{#if confidence}} _({{confidence | percent}})_{{/if}}{{/grain_type}}",
            "{{#grain_type \"event\"}}**{{actor}}** {{action}} — _{{created_at | relative}}_{{/grain_type}}",
            "{{#grain_type \"state\"}}**{{entity}}**: {{state_value}}{{/grain_type}}",
            "{{#grain_type \"workflow\"}}**{{name}}** — {{status}} ({{node_count}} nodes, {{edge_count}} edges){{/grain_type}}",
            "{{#grain_type \"tool\"}}**{{tool_name}}**({{input | truncate 40}}) -> `{{content | truncate 60}}`{{#if is_error}} [ERROR]{{/if}}{{/grain_type}}",
            "{{#grain_type \"observation\"}}**{{observer}}** observed: {{sensory_data | truncate 60}}{{/grain_type}}",
            "{{#grain_type \"goal\"}}**[{{goal_state}}]** {{title | default \"untitled\"}} _({{priority | default \"normal\"}})_{{/grain_type}}",
            "{{#grain_type \"reasoning\"}}{{premises | truncate 60}} => **{{conclusion}}**{{/grain_type}}",
            "{{#grain_type \"consensus\"}}**{{participants | join \", \"}}**: {{agreement_level}}{{/grain_type}}",
            "{{#grain_type \"consent\"}}**{{consenter}}**: {{scope}}{{#if expires_at}} _expires {{expires_at | relative}}_{{/if}}{{/grain_type}}\n",
            "{{/each}}"
        );
        self.insert_builtin(
            "llm_chat",
            llm_chat_src,
            "Markdown-formatted context for LLM chat injection",
        );

        // -- weekly_standup --
        let weekly_standup_src = concat!(
            "# Weekly Activity Summary\n\n",
            "{{#each grains}}",
            "{{#grain_type \"tool\"}}- **{{tool_name}}**: {{content | truncate 80}}{{#if duration_ms}} ({{duration_ms}}ms){{/if}}{{#if is_error}} [FAILED]{{/if}} — _{{created_at | relative}}_\n{{/grain_type}}",
            "{{#grain_type \"goal\"}}- **Goal [{{goal_state}}]**: {{title | default \"untitled\"}}{{#if assigned_to}} ({{assigned_to}}){{/if}}\n{{/grain_type}}",
            "{{#grain_type \"workflow\"}}- **Workflow {{name}}**: {{status}} ({{node_count}} nodes, {{edge_count}} edges)\n{{/grain_type}}",
            "{{#grain_type \"fact\"}}- {{subject}} {{relation | humanize}} {{object}}\n{{/grain_type}}",
            "{{/each}}"
        );
        self.insert_builtin(
            "weekly_standup",
            weekly_standup_src,
            "Tool/goal/workflow-focused summary for weekly standups",
        );

        // -- toon --
        let toon_src = concat!(
            "{{#each grains}}",
            "{{grain_type}}: {{hash}}\n",
            "{{#grain_type \"fact\"}}  subject: {{subject}}\n  relation: {{relation}}\n  object: {{object}}{{#if confidence}}\n  confidence: {{confidence | percent}}{{/if}}{{/grain_type}}",
            "{{#grain_type \"event\"}}  actor: {{actor}}\n  action: {{action}}{{#if created_at}}\n  created_at: {{created_at | relative}}{{/if}}{{/grain_type}}",
            "{{#grain_type \"state\"}}  entity: {{entity}}\n  state_value: {{state_value}}{{#if validity}}\n  validity: {{validity}}{{/if}}{{/grain_type}}",
            "{{#grain_type \"workflow\"}}  name: {{name}}\n  status: {{status}}\n  nodes: {{node_count}}\n  edges: {{edge_count}}{{/grain_type}}",
            "{{#grain_type \"tool\"}}  tool_name: {{tool_name}}\n  input: {{input | truncate 60}}\n  content: {{content | truncate 80}}{{#if is_error}}\n  error: true{{/if}}{{/grain_type}}",
            "{{#grain_type \"observation\"}}  observer: {{observer}}\n  observed: {{observed}}\n  data: {{sensory_data | truncate 60}}{{/grain_type}}",
            "{{#grain_type \"goal\"}}  goal_state: {{goal_state}}\n  title: {{title | default \"untitled\"}}\n  priority: {{priority | default \"normal\"}}{{#if assigned_to}}\n  assigned_to: {{assigned_to}}{{/if}}{{/grain_type}}",
            "{{#grain_type \"reasoning\"}}  premises: {{premises | truncate 60}}\n  conclusion: {{conclusion}}{{#if confidence}}\n  confidence: {{confidence | percent}}{{/if}}{{/grain_type}}",
            "{{#grain_type \"consensus\"}}  participants: {{participants | join \", \"}}\n  agreement: {{agreement_level}}{{/grain_type}}",
            "{{#grain_type \"consent\"}}  consenter: {{consenter}}\n  scope: {{scope}}{{#if expires_at}}\n  expires_at: {{expires_at | relative}}{{/if}}{{/grain_type}}\n",
            "{{/each}}"
        );
        self.insert_builtin(
            "toon",
            toon_src,
            "Compact TOON format (Token-Oriented Object Notation) for LLM context",
        );
    }

    /// Insert a built-in template. Panics only in debug if the template is
    /// malformed (they are compile-time constants and always valid).
    fn insert_builtin(&mut self, name: &str, source: &str, description: &str) {
        let template = match parse_template(source) {
            Ok(t) => t,
            Err(e) => {
                // Built-in templates are compile-time constants; a parse failure
                // here indicates a bug in the template string, not user error.
                // In release builds, skip the broken builtin rather than panic.
                #[cfg(debug_assertions)]
                panic!("built-in template \"{}\" failed to parse: {}", name, e);
                #[cfg(not(debug_assertions))]
                {
                    let _ = e;
                    return;
                }
            }
        };
        self.templates.insert(
            name.to_string(),
            TemplateEntry {
                template,
                builtin: true,
                parent: None,
                description: description.to_string(),
                grain_types: Vec::new(),
                last_run_at: None,
                updated_at: None,
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Template inheritance
// ---------------------------------------------------------------------------

/// Resolve template inheritance (1-level max). Returns the effective template
/// after merging parent and child GrainTypeBlocks.
pub fn resolve_inheritance(entry: &TemplateEntry, registry: &TemplateRegistry) -> Template {
    let Some(ref parent_name) = entry.parent else {
        return entry.template.clone();
    };
    let Some(parent_entry) = registry.get(parent_name) else {
        // Should not happen (validated at registration time).
        return entry.template.clone();
    };

    merge_templates(&parent_entry.template, &entry.template)
}

/// Merge a parent template with a child template.
///
/// Strategy:
/// 1. Start with parent's nodes.
/// 2. For each GrainTypeBlock in child, replace matching block in parent.
/// 3. New GrainTypeBlocks from child are appended.
fn merge_templates(parent: &Template, child: &Template) -> Template {
    let mut merged_nodes = parent.nodes.clone();

    for child_node in &child.nodes {
        if let TemplateNode::GrainTypeBlock { grain_type, body } = child_node {
            let mut replaced = false;
            for node in merged_nodes.iter_mut() {
                match node {
                    TemplateNode::GrainTypeBlock {
                        grain_type: gt,
                        body: parent_body,
                    } if gt == grain_type => {
                        *parent_body = body.clone();
                        replaced = true;
                        break;
                    }
                    // Also check inside Each blocks.
                    TemplateNode::Each {
                        body: each_body, ..
                    } => {
                        for inner in each_body.iter_mut() {
                            if let TemplateNode::GrainTypeBlock {
                                grain_type: gt,
                                body: parent_body,
                            } = inner
                            {
                                if gt == grain_type {
                                    *parent_body = body.clone();
                                    replaced = true;
                                    break;
                                }
                            }
                        }
                        if replaced {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if !replaced {
                merged_nodes.push(child_node.clone());
            }
        }
    }

    Template {
        source: child.source.clone(),
        nodes: merged_nodes,
    }
}

// ---------------------------------------------------------------------------
// apply_format (for executor integration)
// ---------------------------------------------------------------------------

/// Apply FORMAT spec to a grain result payload. This function is designed
/// to be called by the executor after `execute_statement()` and
/// `apply_pipeline()`. It transforms grain payloads into formatted strings.
///
/// Note: This returns the rendered content as a String. The executor is
/// responsible for wrapping it into the appropriate CalResultPayload variant.
pub fn apply_format(
    grains: &[CalGrainResult],
    template: &Template,
    ctx: &RenderContext,
) -> CalResult<String> {
    render(template, grains, ctx)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper: create a test CalGrainResult.
    fn make_fact(subject: &str, relation: &str, object: &str, confidence: f64) -> CalGrainResult {
        CalGrainResult {
            hash: "abc123".to_string(),
            grain_type: "fact".to_string(),
            score: 0.95,
            fields: json!({
                "subject": subject,
                "relation": relation,
                "object": object,
                "confidence": confidence,
                "created_at": 1700000000_i64,
                "namespace": "default",
                "user_id": "user1",
            }),
            score_breakdown: None,
            explanation: None,
            is_deterministic: false,
        }
    }

    fn make_tool(tool: &str, input: &str, content: &str, is_error: bool) -> CalGrainResult {
        CalGrainResult {
            hash: "def456".to_string(),
            grain_type: "tool".to_string(),
            score: 0.90,
            fields: json!({
                "tool_name": tool,
                "input": input,
                "content": content,
                "is_error": is_error,
                "created_at": 1700000000_i64,
                "duration_ms": 150,
            }),
            score_breakdown: None,
            explanation: None,
            is_deterministic: false,
        }
    }

    fn make_event(actor: &str, action: &str) -> CalGrainResult {
        CalGrainResult {
            hash: "ghi789".to_string(),
            grain_type: "event".to_string(),
            score: 0.85,
            fields: json!({
                "actor": actor,
                "action": action,
                "created_at": 1700000000_i64,
            }),
            score_breakdown: None,
            explanation: None,
            is_deterministic: false,
        }
    }

    fn make_goal(title: &str, goal_state: &str) -> CalGrainResult {
        CalGrainResult {
            hash: "jkl012".to_string(),
            grain_type: "goal".to_string(),
            score: 0.80,
            fields: json!({
                "title": title,
                "goal_state": goal_state,
                "priority": "high",
                "assigned_to": "john",
            }),
            score_breakdown: None,
            explanation: None,
            is_deterministic: false,
        }
    }

    fn test_ctx() -> RenderContext {
        RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Template name validation
    // -----------------------------------------------------------------------

    #[test]
    fn test_valid_template_names() {
        assert!(is_valid_template_name("triples"));
        assert!(is_valid_template_name("a"));
        assert!(is_valid_template_name("my_template"));
        assert!(is_valid_template_name("template123"));
        assert!(is_valid_template_name("a_b_c_d"));
        assert!(is_valid_template_name("HasUpperCase"));
        assert!(is_valid_template_name("has space"));
        assert!(is_valid_template_name("has-hyphen"));
        assert!(is_valid_template_name("Customer Support Context"));
        assert!(is_valid_template_name("My Template-1"));
        // 64 characters (ok)
        assert!(is_valid_template_name(&"a".repeat(64)));
    }

    #[test]
    fn test_invalid_template_names() {
        assert!(!is_valid_template_name(""));
        assert!(!is_valid_template_name("1starts_with_digit"));
        assert!(!is_valid_template_name("_starts_with_underscore"));
        assert!(!is_valid_template_name("-starts-with-hyphen"));
        assert!(!is_valid_template_name("has.dot"));
        assert!(!is_valid_template_name("trailing space "));
        assert!(!is_valid_template_name(" leading space"));
        assert!(!is_valid_template_name("double  space"));
        // 65 characters (too long)
        assert!(!is_valid_template_name(&"a".repeat(65)));
    }

    // -----------------------------------------------------------------------
    // Template parsing
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_simple_text() {
        let t = parse_template("Hello, world!").unwrap();
        assert_eq!(t.nodes().len(), 1);
        assert_eq!(t.nodes()[0], TemplateNode::Text("Hello, world!".into()));
    }

    #[test]
    fn test_parse_variable() {
        let t = parse_template("{{subject}}").unwrap();
        assert_eq!(t.nodes().len(), 1);
        match &t.nodes()[0] {
            TemplateNode::Variable { name, filters } => {
                assert_eq!(name, "subject");
                assert!(filters.is_empty());
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_parse_variable_with_filter() {
        let t = parse_template("{{subject | uppercase}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Variable { name, filters } => {
                assert_eq!(name, "subject");
                assert_eq!(filters.len(), 1);
                assert_eq!(filters[0].name, "uppercase");
                assert!(filters[0].arg.is_none());
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_parse_variable_with_filter_arg() {
        let t = parse_template("{{content | truncate 80}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Variable { name, filters } => {
                assert_eq!(name, "content");
                assert_eq!(filters.len(), 1);
                assert_eq!(filters[0].name, "truncate");
                assert_eq!(filters[0].arg, Some("80".to_string()));
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_parse_multiple_filters() {
        let t = parse_template("{{relation | humanize | uppercase}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Variable { name, filters } => {
                assert_eq!(name, "relation");
                assert_eq!(filters.len(), 2);
                assert_eq!(filters[0].name, "humanize");
                assert_eq!(filters[1].name, "uppercase");
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_parse_each_block() {
        let t = parse_template("{{#each grains}}{{subject}}{{/each}}").unwrap();
        assert_eq!(t.nodes().len(), 1);
        match &t.nodes()[0] {
            TemplateNode::Each { collection, body } => {
                assert_eq!(collection, "grains");
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected Each"),
        }
    }

    #[test]
    fn test_parse_condition() {
        let t = parse_template("{{#if confidence}}yes{{/if}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Condition {
                field,
                if_body,
                else_body,
            } => {
                assert_eq!(field, "confidence");
                assert_eq!(if_body.len(), 1);
                assert!(else_body.is_empty());
            }
            _ => panic!("expected Condition"),
        }
    }

    #[test]
    fn test_parse_condition_with_else() {
        let t = parse_template("{{#if confidence}}yes{{else}}no{{/if}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Condition {
                field,
                if_body,
                else_body,
            } => {
                assert_eq!(field, "confidence");
                assert_eq!(if_body.len(), 1);
                assert_eq!(else_body.len(), 1);
            }
            _ => panic!("expected Condition"),
        }
    }

    #[test]
    fn test_parse_inverted_section() {
        let t = parse_template("{{^summary}}no summary{{/summary}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::InvertedSection { field, body } => {
                assert_eq!(field, "summary");
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected InvertedSection"),
        }
    }

    #[test]
    fn test_parse_grain_type_block() {
        let t = parse_template("{{#grain_type \"fact\"}}{{subject}}{{/grain_type}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::GrainTypeBlock { grain_type, body } => {
                assert_eq!(grain_type, "fact");
                assert_eq!(body.len(), 1);
            }
            _ => panic!("expected GrainTypeBlock"),
        }
    }

    #[test]
    fn test_parse_comment() {
        let t = parse_template("{{! this is a comment }}hello").unwrap();
        assert_eq!(t.nodes().len(), 2);
        match &t.nodes()[0] {
            TemplateNode::Comment(text) => assert_eq!(text, "this is a comment"),
            _ => panic!("expected Comment"),
        }
    }

    #[test]
    fn test_parse_nested_each_rejected() {
        let result = parse_template("{{#each grains}}{{#each grains}}{{/each}}{{/each}}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E041");
    }

    #[test]
    fn test_parse_unknown_variable_rejected() {
        let result = parse_template("{{unknown_variable}}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E042");
    }

    #[test]
    fn test_parse_unknown_filter_rejected() {
        let result = parse_template("{{subject | bogus_filter}}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E043");
    }

    #[test]
    fn test_parse_template_too_large() {
        let huge = "x".repeat(MAX_TEMPLATE_SIZE + 1);
        let result = parse_template(&huge);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E040");
    }

    #[test]
    fn test_parse_truncate_out_of_range() {
        let result = parse_template("{{subject | truncate 0}}");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E049");

        let result2 = parse_template("{{subject | truncate 200000}}");
        assert!(result2.is_err());
    }

    #[test]
    fn test_parse_unclosed_tag() {
        let result = parse_template("{{subject");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E049");
    }

    #[test]
    fn test_parse_unclosed_section() {
        let result = parse_template("{{#if confidence}}yes");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E049");
    }

    // -----------------------------------------------------------------------
    // ResolvedValue
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolved_value_truthiness() {
        assert!(ResolvedValue::Str("hello".into()).is_truthy());
        assert!(!ResolvedValue::Str("".into()).is_truthy());
        assert!(ResolvedValue::Number(1.0).is_truthy());
        assert!(!ResolvedValue::Number(0.0).is_truthy());
        assert!(ResolvedValue::Integer(1).is_truthy());
        assert!(!ResolvedValue::Integer(0).is_truthy());
        assert!(ResolvedValue::Bool(true).is_truthy());
        assert!(!ResolvedValue::Bool(false).is_truthy());
        assert!(ResolvedValue::Array(vec!["a".into()]).is_truthy());
        assert!(!ResolvedValue::Array(vec![]).is_truthy());
        assert!(!ResolvedValue::Null.is_truthy());
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn test_resolved_value_display() {
        assert_eq!(ResolvedValue::Str("hello".into()).to_display(), "hello");
        assert_eq!(ResolvedValue::Number(3.14).to_display(), "3.14");
        assert_eq!(ResolvedValue::Number(42.0).to_display(), "42");
        assert_eq!(ResolvedValue::Integer(42).to_display(), "42");
        assert_eq!(ResolvedValue::Bool(true).to_display(), "true");
        assert_eq!(
            ResolvedValue::Array(vec!["a".into(), "b".into()]).to_display(),
            "a, b"
        );
        assert_eq!(ResolvedValue::Null.to_display(), "");
    }

    // -----------------------------------------------------------------------
    // Variable resolution
    // -----------------------------------------------------------------------

    #[test]
    fn test_resolve_metadata_variables() {
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 5,
            user_vars: HashMap::new(),
        };
        let grain = make_fact("john", "mg:likes", "coffee", 0.94);

        assert_eq!(
            resolve_variable("_index", Some(&grain), Some(2), &ctx).to_display(),
            "2"
        );
        assert_eq!(
            resolve_variable("_count", Some(&grain), Some(0), &ctx).to_display(),
            "5"
        );
        assert!(resolve_variable("_first", Some(&grain), Some(0), &ctx).is_truthy());
        assert!(!resolve_variable("_first", Some(&grain), Some(1), &ctx).is_truthy());
        assert!(resolve_variable("_last", Some(&grain), Some(4), &ctx).is_truthy());
        assert!(!resolve_variable("_last", Some(&grain), Some(3), &ctx).is_truthy());
    }

    #[test]
    fn test_resolve_direct_fields() {
        let grain = make_fact("john", "mg:likes", "coffee", 0.94);
        let ctx = test_ctx();

        assert_eq!(
            resolve_variable("hash", Some(&grain), None, &ctx).to_display(),
            "abc123"
        );
        assert_eq!(
            resolve_variable("grain_type", Some(&grain), None, &ctx).to_display(),
            "fact"
        );
        assert_eq!(
            resolve_variable("score", Some(&grain), None, &ctx).to_display(),
            "0.95"
        );
    }

    #[test]
    fn test_resolve_grain_fields() {
        let grain = make_fact("john", "mg:likes", "coffee", 0.94);
        let ctx = test_ctx();

        assert_eq!(
            resolve_variable("subject", Some(&grain), None, &ctx).to_display(),
            "john"
        );
        assert_eq!(
            resolve_variable("relation", Some(&grain), None, &ctx).to_display(),
            "mg:likes"
        );
        assert_eq!(
            resolve_variable("object", Some(&grain), None, &ctx).to_display(),
            "coffee"
        );
    }

    #[test]
    fn test_resolve_disallowed_field_returns_null() {
        let grain = make_fact("john", "mg:likes", "coffee", 0.94);
        let ctx = test_ctx();

        // A field not in ALLOWED_FIELDS should return Null.
        assert!(!resolve_variable("secret_internal_field", Some(&grain), None, &ctx).is_truthy());
    }

    // -----------------------------------------------------------------------
    // Filter pipeline
    // -----------------------------------------------------------------------

    #[test]
    fn test_filter_truncate() {
        let ctx = test_ctx();
        let val = ResolvedValue::Str("Hello, world! This is a long string.".into());
        let filter = Filter {
            name: "truncate".into(),
            arg: Some("13".into()),
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "Hello, world!...");
    }

    #[test]
    fn test_filter_truncate_no_truncation_needed() {
        let ctx = test_ctx();
        let val = ResolvedValue::Str("short".into());
        let filter = Filter {
            name: "truncate".into(),
            arg: Some("80".into()),
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "short");
    }

    #[test]
    fn test_filter_relative() {
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let val = ResolvedValue::Integer(1700000000);
        let filter = Filter {
            name: "relative".into(),
            arg: None,
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "1m ago");
    }

    #[test]
    fn test_filter_humanize() {
        let ctx = test_ctx();
        let val = ResolvedValue::Str("mg:likes".into());
        let filter = Filter {
            name: "humanize".into(),
            arg: None,
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "likes");
    }

    #[test]
    fn test_filter_percent() {
        let ctx = test_ctx();
        let val = ResolvedValue::Number(0.94);
        let filter = Filter {
            name: "percent".into(),
            arg: None,
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "94%");
    }

    #[test]
    fn test_filter_uppercase_lowercase() {
        let ctx = test_ctx();
        let val = ResolvedValue::Str("Hello".into());

        let upper = apply_filter(
            &val,
            &Filter {
                name: "uppercase".into(),
                arg: None,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(upper.to_display(), "HELLO");

        let lower = apply_filter(
            &val,
            &Filter {
                name: "lowercase".into(),
                arg: None,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(lower.to_display(), "hello");
    }

    #[test]
    fn test_filter_default() {
        let ctx = test_ctx();

        let truthy = ResolvedValue::Str("value".into());
        let result = apply_filter(
            &truthy,
            &Filter {
                name: "default".into(),
                arg: Some("fallback".into()),
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(result.to_display(), "value");

        let null = ResolvedValue::Null;
        let result = apply_filter(
            &null,
            &Filter {
                name: "default".into(),
                arg: Some("fallback".into()),
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(result.to_display(), "fallback");
    }

    #[test]
    fn test_filter_join() {
        let ctx = test_ctx();
        let val = ResolvedValue::Array(vec!["a".into(), "b".into(), "c".into()]);
        let filter = Filter {
            name: "join".into(),
            arg: Some(", ".into()),
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "a, b, c");
    }

    #[test]
    fn test_filter_date() {
        let ctx = test_ctx();
        let val = ResolvedValue::Integer(1700000000);
        let filter = Filter {
            name: "date".into(),
            arg: Some("%Y-%m-%d".into()),
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "2023-11-14");
    }

    #[test]
    fn test_filter_date_negative_epoch() {
        let ctx = test_ctx();
        let val = ResolvedValue::Integer(-100);
        let filter = Filter {
            name: "date".into(),
            arg: None,
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "<invalid date>");
    }

    #[test]
    fn test_filter_json() {
        let ctx = test_ctx();
        let val = ResolvedValue::Str("hello".into());
        let filter = Filter {
            name: "json".into(),
            arg: None,
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "\"hello\"");

        let bool_val = ResolvedValue::Bool(true);
        let result = apply_filter(&bool_val, &filter, &ctx).unwrap();
        assert_eq!(result.to_display(), "true");
    }

    // -----------------------------------------------------------------------
    // format_epoch
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_epoch_basic() {
        assert_eq!(format_epoch(0, "%Y-%m-%d"), "1970-01-01");
        assert_eq!(format_epoch(1700000000, "%Y-%m-%d"), "2023-11-14");
        assert_eq!(
            format_epoch(1700000000, "%Y-%m-%d %H:%M:%S"),
            "2023-11-14 22:13:20"
        );
    }

    #[test]
    fn test_format_epoch_negative() {
        assert_eq!(format_epoch(-1, "%Y-%m-%d"), "<invalid date>");
    }

    #[test]
    fn test_format_epoch_long_format() {
        let long_fmt = "%Y".repeat(200);
        assert_eq!(format_epoch(0, &long_fmt), "<format too long>");
    }

    // -----------------------------------------------------------------------
    // Template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_render_simple_text() {
        let t = parse_template("Hello!").unwrap();
        let ctx = test_ctx();
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "Hello!");
    }

    #[test]
    fn test_render_variable() {
        let t = parse_template("{{#each grains}}{{subject}}{{/each}}").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "john");
    }

    #[test]
    fn test_render_each_with_filter() {
        let t = parse_template(
            "{{#each grains}}{{subject}} {{relation | humanize}} {{object}}\n{{/each}}",
        )
        .unwrap();
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_fact("bob", "mg:knows", "john", 0.87),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "john likes coffee\nbob knows john\n");
    }

    #[test]
    fn test_render_condition_true() {
        let t = parse_template("{{#each grains}}{{#if confidence}}yes{{/if}}{{/each}}").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "yes");
    }

    #[test]
    fn test_render_condition_false_with_else() {
        let t = parse_template(
            "{{#each grains}}{{#if summary}}has summary{{else}}no summary{{/if}}{{/each}}",
        )
        .unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "no summary");
    }

    #[test]
    fn test_render_inverted_section() {
        let t =
            parse_template("{{#each grains}}{{^summary}}no summary{{/summary}}{{/each}}").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "no summary");
    }

    #[test]
    fn test_render_grain_type_block() {
        let t = parse_template(concat!(
            "{{#each grains}}",
            "{{#grain_type \"fact\"}}FACT: {{subject}}{{/grain_type}}",
            "{{#grain_type \"tool\"}}TOOL: {{tool_name}}{{/grain_type}}",
            "\n{{/each}}"
        ))
        .unwrap();
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_tool("search", "weather", "sunny", false),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "FACT: john\nTOOL: search\n");
    }

    #[test]
    fn test_render_metadata_variables() {
        let t = parse_template(
            "Count: {{_count}}, {{#each grains}}[{{_index}}]{{subject}}{{#if _last}} END{{/if}} {{/each}}"
        ).unwrap();
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_fact("bob", "mg:knows", "john", 0.87),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&t, &grains, &ctx).unwrap();
        assert_eq!(result, "Count: 2, [0]john [1]bob END ");
    }

    #[test]
    fn test_render_output_size_limit() {
        let t = parse_template("{{#each grains}}{{subject}}{{/each}}").unwrap();
        let grains: Vec<CalGrainResult> = (0..1000)
            .map(|i| make_fact(&format!("subject_{}", i), "mg:knows", "object", 0.5))
            .collect();
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        // Set a tiny limit to trigger the safety check.
        let result = render_with_limit(&t, &grains, &ctx, 100);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E050");
    }

    // -----------------------------------------------------------------------
    // Default grain rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_default_grain_render_fact() {
        let grain = make_fact("john", "mg:likes", "coffee", 0.94);
        let ctx = test_ctx();
        let result = default_grain_render(&grain, &ctx);
        assert_eq!(result, "john likes coffee (94%)");
    }

    #[test]
    fn test_default_grain_render_tool() {
        let grain = make_tool("search", "weather", "sunny", false);
        let ctx = test_ctx();
        let result = default_grain_render(&grain, &ctx);
        assert_eq!(result, "search(weather) -> sunny");
    }

    #[test]
    fn test_default_grain_render_tool_error() {
        let grain = make_tool("search", "weather", "timeout", true);
        let ctx = test_ctx();
        let result = default_grain_render(&grain, &ctx);
        assert_eq!(result, "search(weather) -> timeout [ERROR]");
    }

    #[test]
    fn test_default_grain_render_event() {
        let grain = make_event("john", "logged in");
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result = default_grain_render(&grain, &ctx);
        assert_eq!(result, "john logged in [1m ago]");
    }

    #[test]
    fn test_default_grain_render_goal() {
        let grain = make_goal("Complete onboarding", "active");
        let ctx = test_ctx();
        let result = default_grain_render(&grain, &ctx);
        assert_eq!(result, "active - john (high)");
    }

    // -----------------------------------------------------------------------
    // Progressive disclosure tiers
    // -----------------------------------------------------------------------

    #[test]
    fn test_select_tier() {
        assert_eq!(select_tier(10000, 10), DisclosureTier::Full); // 1000 tokens/grain
        assert_eq!(select_tier(1000, 10), DisclosureTier::Headlines); // 100 tokens/grain
        assert_eq!(select_tier(100, 10), DisclosureTier::Summary); // 10 tokens/grain
        assert_eq!(select_tier(1000, 0), DisclosureTier::Full); // no grains
    }

    #[test]
    fn test_effective_truncate() {
        assert_eq!(effective_truncate(Some(100), DisclosureTier::Summary), 40);
        assert_eq!(effective_truncate(Some(100), DisclosureTier::Headlines), 80);
        assert_eq!(effective_truncate(Some(100), DisclosureTier::Full), 100);
        assert_eq!(effective_truncate(None, DisclosureTier::Summary), 40);
        assert_eq!(effective_truncate(None, DisclosureTier::Full), usize::MAX);
    }

    #[test]
    fn test_tier_affects_truncation() {
        let t = parse_template("{{#each grains}}{{content | truncate 100}}{{/each}}").unwrap();
        let grains = vec![{
            let mut g = make_fact("john", "mg:likes", "coffee", 0.94);
            g.fields
                .as_object_mut()
                .unwrap()
                .insert("content".into(), json!("x".repeat(200)));
            g
        }];

        // Full tier: truncate at 100
        let ctx_full = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result_full = render(&t, &grains, &ctx_full).unwrap();
        assert!(result_full.len() <= 103 + 3); // 100 chars + "..."

        // Summary tier: truncate at 40
        let ctx_summary = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Summary,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result_summary = render(&t, &grains, &ctx_summary).unwrap();
        assert!(result_summary.len() <= 43 + 3); // 40 chars + "..."
        assert!(result_summary.len() < result_full.len());
    }

    // -----------------------------------------------------------------------
    // Template registry
    // -----------------------------------------------------------------------

    #[test]
    fn test_registry_has_builtins() {
        let reg = TemplateRegistry::new();
        assert!(reg.get("triples").is_some());
        assert!(reg.get("progressive").is_some());
        assert!(reg.get("llm_system_prompt").is_some());
        assert!(reg.get("llm_chat").is_some());
        assert!(reg.get("weekly_standup").is_some());
        assert!(reg.get("toon").is_some());
        assert!(reg.get("triples").unwrap().builtin);
    }

    #[test]
    fn test_registry_builtin_count() {
        let reg = TemplateRegistry::new();
        let list = reg.list();
        assert_eq!(list.len(), 6);
        assert!(list.iter().all(|e| e.builtin));
    }

    #[test]
    fn test_registry_register_custom() {
        let mut reg = TemplateRegistry::new();
        reg.register(
            "my_template",
            "{{#each grains}}{{subject}}{{/each}}",
            "A custom template",
            None,
        )
        .unwrap();
        assert!(reg.get("my_template").is_some());
        assert!(!reg.get("my_template").unwrap().builtin);
        assert_eq!(reg.list().len(), 7);
    }

    #[test]
    fn test_registry_cannot_overwrite_builtin() {
        let mut reg = TemplateRegistry::new();
        let result = reg.register("triples", "{{subject}}", "override", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E046");
    }

    #[test]
    fn test_registry_invalid_name_rejected() {
        let mut reg = TemplateRegistry::new();
        let result = reg.register("123-starts-with-digit", "{{subject}}", "bad name", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        // CAL-E115 = TemplateInvalidName. (CAL-E044 = Tier1NotEnabled.)
        assert_eq!(err.code(), "CAL-E115");
    }

    #[test]
    fn test_registry_delete_custom() {
        let mut reg = TemplateRegistry::new();
        reg.register(
            "my_template",
            "{{#each grains}}{{subject}}{{/each}}",
            "test",
            None,
        )
        .unwrap();
        assert!(reg.get("my_template").is_some());
        reg.delete("my_template").unwrap();
        assert!(reg.get("my_template").is_none());
    }

    #[test]
    fn test_registry_cannot_delete_builtin() {
        let mut reg = TemplateRegistry::new();
        let result = reg.delete("triples");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E046");
    }

    #[test]
    fn test_registry_delete_nonexistent() {
        let mut reg = TemplateRegistry::new();
        let result = reg.delete("nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E045");
    }

    #[test]
    fn test_registry_inheritance() {
        let mut reg = TemplateRegistry::new();

        // Register a parent template.
        reg.register(
            "parent_tpl",
            concat!(
                "{{#each grains}}",
                "{{#grain_type \"fact\"}}B: {{subject}}{{/grain_type}}",
                "{{#grain_type \"tool\"}}A: {{tool_name}}{{/grain_type}}",
                "{{/each}}"
            ),
            "Parent",
            None,
        )
        .unwrap();

        // Register a child that overrides fact block.
        reg.register(
            "child_tpl",
            "{{#grain_type \"fact\"}}CUSTOM: {{subject}} -> {{object}}{{/grain_type}}",
            "Child",
            Some("parent_tpl"),
        )
        .unwrap();

        let child = reg.get("child_tpl").unwrap();
        let resolved = resolve_inheritance(child, &reg);

        // The resolved template should have the child's fact block
        // but keep the parent's tool block.
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_tool("search", "weather", "sunny", false),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&resolved, &grains, &ctx).unwrap();
        assert!(result.contains("CUSTOM: john -> coffee"));
        assert!(result.contains("A: search"));
    }

    #[test]
    fn test_registry_2_level_inheritance_rejected() {
        let mut reg = TemplateRegistry::new();

        reg.register(
            "grandparent",
            "{{#each grains}}{{subject}}{{/each}}",
            "GP",
            None,
        )
        .unwrap();

        reg.register(
            "parent_tpl",
            "{{#each grains}}{{subject}}{{/each}}",
            "P",
            Some("grandparent"),
        )
        .unwrap();

        let result = reg.register(
            "child_tpl",
            "{{#each grains}}{{subject}}{{/each}}",
            "C",
            Some("parent_tpl"),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E048");
    }

    #[test]
    fn test_registry_parent_not_found() {
        let mut reg = TemplateRegistry::new();
        let result = reg.register(
            "child_tpl",
            "{{#each grains}}{{subject}}{{/each}}",
            "C",
            Some("nonexistent"),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E047");
    }

    // -----------------------------------------------------------------------
    // Built-in template rendering
    // -----------------------------------------------------------------------

    #[test]
    fn test_builtin_triples_render() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("triples").unwrap();
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_fact("bob", "mg:knows", "john", 0.87),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&entry.template, &grains, &ctx).unwrap();
        assert!(result.contains("john likes coffee (94%)"));
        assert!(result.contains("bob knows john (87%)"));
    }

    #[test]
    fn test_builtin_llm_system_prompt_render() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("llm_system_prompt").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&entry.template, &grains, &ctx).unwrap();
        assert!(result.contains("<context>"));
        assert!(result.contains("<memories count=\"1\">"));
        assert!(result.contains("</memories>"));
        assert!(result.contains("</context>"));
        assert!(result.contains("john likes coffee"));
    }

    #[test]
    fn test_builtin_llm_chat_render() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("llm_chat").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&entry.template, &grains, &ctx).unwrap();
        assert!(result.contains("**Relevant memories**"));
        assert!(result.contains("**john**"));
    }

    #[test]
    fn test_builtin_weekly_standup_render() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("weekly_standup").unwrap();
        let grains = vec![
            make_tool("search", "weather", "sunny", false),
            make_goal("Complete onboarding", "active"),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&entry.template, &grains, &ctx).unwrap();
        assert!(result.contains("# Weekly Activity Summary"));
        assert!(result.contains("**search**"));
        assert!(result.contains("**Goal [active]**"));
    }

    #[test]
    fn test_builtin_progressive_render_mixed_types() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("progressive").unwrap();
        let grains = vec![
            make_fact("john", "mg:likes", "coffee", 0.94),
            make_tool("search", "weather", "sunny", false),
            make_event("john", "logged in"),
        ];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = render(&entry.template, &grains, &ctx).unwrap();
        assert!(result.contains("john likes coffee"));
        assert!(result.contains("search(weather) -> sunny"));
        assert!(result.contains("john logged in"));
    }

    // -----------------------------------------------------------------------
    // apply_format integration
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_format() {
        let reg = TemplateRegistry::new();
        let entry = reg.get("triples").unwrap();
        let grains = vec![make_fact("john", "mg:likes", "coffee", 0.94)];
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: grains.len(),
            user_vars: HashMap::new(),
        };
        let result = apply_format(&grains, &entry.template, &ctx).unwrap();
        assert!(result.contains("john likes coffee"));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_empty_template() {
        let t = parse_template("").unwrap();
        let ctx = test_ctx();
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_empty_grains() {
        let t = parse_template("Before{{#each grains}}{{subject}}{{/each}}After").unwrap();
        let ctx = test_ctx();
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "BeforeAfter");
    }

    #[test]
    fn test_unicode_truncation() {
        let ctx = test_ctx();
        // Japanese characters (3 bytes each in UTF-8)
        let val =
            ResolvedValue::Str("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\u{4e16}\u{754c}".into()); // "konnichiwa sekai"
        let filter = Filter {
            name: "truncate".into(),
            arg: Some("3".into()),
        };
        let result = apply_filter(&val, &filter, &ctx).unwrap();
        let display = result.to_display();
        // Should truncate at 3 characters, not 3 bytes.
        assert!(display.ends_with("..."));
        assert_eq!(display, "\u{3053}\u{3093}\u{306b}...");
    }

    #[test]
    fn test_render_with_null_fields() {
        // A grain with no subject/relation/object.
        let grain = CalGrainResult {
            hash: "abc123".to_string(),
            grain_type: "fact".to_string(),
            score: 0.5,
            fields: json!({}),
            score_breakdown: None,
            explanation: None,
            is_deterministic: false,
        };
        let t =
            parse_template("{{#each grains}}{{subject}} {{relation}} {{object}}{{/each}}").unwrap();
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &[grain], &ctx).unwrap();
        // Null fields render as empty strings.
        assert_eq!(result, "  ");
    }

    #[test]
    fn test_parse_date_filter_with_quoted_arg() {
        let t = parse_template("{{created_at | date \"%Y-%m-%d %H:%M\"}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Variable { filters, .. } => {
                assert_eq!(filters[0].name, "date");
                assert_eq!(filters[0].arg, Some("%Y-%m-%d %H:%M".to_string()));
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_parse_default_filter_with_quoted_arg() {
        let t = parse_template("{{summary | default \"No summary\"}}").unwrap();
        match &t.nodes()[0] {
            TemplateNode::Variable { filters, .. } => {
                assert_eq!(filters[0].name, "default");
                assert_eq!(filters[0].arg, Some("No summary".to_string()));
            }
            _ => panic!("expected Variable"),
        }
    }

    #[test]
    fn test_template_source_preserved() {
        let source = "{{#each grains}}{{subject}}{{/each}}";
        let t = parse_template(source).unwrap();
        assert_eq!(t.source(), source);
    }

    #[test]
    fn test_registry_list_sorted() {
        // `TemplateRegistry::list` sorts entries by `updated_at` descending
        // (most-recent first). A fresh registry has all builtins with
        // identical timestamps, so the relative order between them is not
        // guaranteed — but the list must still be a non-increasing sequence
        // of timestamps.
        let reg = TemplateRegistry::new();
        let list = reg.list();
        let ts: Vec<u64> = list.iter().map(|e| e.updated_at.unwrap_or(0)).collect();
        assert!(
            ts.windows(2).all(|w| w[0] >= w[1]),
            "list is not sorted by updated_at DESC: {:?}",
            ts
        );
    }

    #[test]
    fn test_select_tier_boundary_values() {
        // 200 tokens/grain = Full
        assert_eq!(select_tier(2000, 10), DisclosureTier::Full);
        // 199 tokens/grain = Headlines
        assert_eq!(select_tier(1990, 10), DisclosureTier::Headlines);
        // 80 tokens/grain = Headlines
        assert_eq!(select_tier(800, 10), DisclosureTier::Headlines);
        // 79 tokens/grain = Summary
        assert_eq!(select_tier(790, 10), DisclosureTier::Summary);
    }

    #[test]
    fn test_suggestion_for_unknown_variable() {
        let result = parse_template("{{subjet}}"); // typo of "subject"
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code(), "CAL-E042");
        // Should suggest "subject".
        let suggestion = err.suggestion();
        assert!(suggestion.is_some());
        assert!(suggestion.unwrap().contains("subject"));
    }

    // -----------------------------------------------------------------------
    // User-injected display variables ($-prefixed)
    // -----------------------------------------------------------------------

    #[test]
    fn test_user_var_parse_valid() {
        // $-prefixed variables should pass parse-time validation.
        let t = parse_template("{{$user_query}}");
        assert!(t.is_ok(), "should accept $-prefixed variable");
    }

    #[test]
    fn test_user_var_parse_with_filter() {
        let t = parse_template("{{$name | uppercase}}");
        assert!(t.is_ok(), "should accept $-prefixed variable with filter");
    }

    #[test]
    fn test_user_var_parse_in_condition() {
        let t = parse_template("{{#if $show_header}}Header{{/if}}");
        assert!(t.is_ok(), "should accept $-prefixed variable in condition");
    }

    #[test]
    fn test_user_var_parse_invalid_name() {
        let result = parse_template("{{$123bad}}");
        assert!(
            result.is_err(),
            "should reject $-prefixed var starting with digit"
        );
    }

    #[test]
    fn test_user_var_resolve_found() {
        let t = parse_template("Query: {{$user_query}}").unwrap();
        let mut vars = HashMap::new();
        vars.insert("user_query".into(), "what does john like?".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: vars,
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "Query: what does john like?");
    }

    #[test]
    fn test_user_var_resolve_missing() {
        let t = parse_template("{{$missing_var}}").unwrap();
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(
            result, "",
            "missing user var should resolve to empty string"
        );
    }

    #[test]
    fn test_user_var_with_filter() {
        let t = parse_template("{{$name | uppercase}}").unwrap();
        let mut vars = HashMap::new();
        vars.insert("name".into(), "john".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: vars,
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "JOHN");
    }

    #[test]
    fn test_user_var_with_grain_fields() {
        // User vars + grain fields coexist.
        let t = parse_template("{{$label}}: {{#each}}{{subject}} {{relation}} {{object}}{{/each}}")
            .unwrap();
        let grain = make_fact("john", "likes", "coffee", 0.9);
        let mut vars = HashMap::new();
        vars.insert("label".into(), "Results".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: vars,
        };
        let result = render(&t, &[grain], &ctx).unwrap();
        assert_eq!(result, "Results: john likes coffee");
    }

    #[test]
    fn test_user_var_condition_truthy() {
        let t = parse_template("{{#if $show}}visible{{/if}}").unwrap();
        let mut vars = HashMap::new();
        vars.insert("show".into(), "yes".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: vars,
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "visible");
    }

    #[test]
    fn test_user_var_condition_falsy() {
        // Missing user var → Null → falsy.
        let t = parse_template("{{#if $show}}visible{{/if}}").unwrap();
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: HashMap::new(),
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_user_var_condition_empty_string_is_falsy() {
        // Empty string user var → should be falsy (empty = no content).
        let t = parse_template("{{#if $show}}visible{{/if}}").unwrap();
        let mut vars = HashMap::new();
        vars.insert("show".into(), "".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 0,
            user_vars: vars,
        };
        let result = render(&t, &[], &ctx).unwrap();
        assert_eq!(result, "", "empty-string user var should be falsy");
    }

    #[test]
    fn test_user_var_no_collision_with_grain_fields() {
        // A user var named $subject should NOT override the grain field "subject".
        let t = parse_template("{{#each}}grain:{{subject}} user:{{$subject}}{{/each}}").unwrap();
        let grain = make_fact("john", "likes", "coffee", 0.9);
        let mut vars = HashMap::new();
        vars.insert("subject".into(), "override_attempt".into());
        let ctx = RenderContext {
            now_secs: 1700000100,
            tier: DisclosureTier::Full,
            total_count: 1,
            user_vars: vars,
        };
        let result = render(&t, &[grain], &ctx).unwrap();
        assert_eq!(result, "grain:john user:override_attempt");
    }
}
