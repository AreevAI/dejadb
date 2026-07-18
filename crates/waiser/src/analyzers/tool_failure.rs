//! Tool-failure clustering (T0) — the flagship analyzer. Groups error Tool
//! grains (captured tool calls) by (tool_name, normalized error signature) and
//! fires when a cluster is both
//! frequent (≥ min_count) and a meaningful share of that tool's calls
//! (≥ min_rate). Emits a memory lesson. Because the signature is derived from
//! attacker-influenceable tool output, this analyzer never auto-applies
//! (§6.3) — its manifest is `Never`.

use crate::analyzer::{AnalyzeCtx, Analyzer};
use crate::analyzers::bound_evidence;
use crate::cal;
use crate::error::Result;
use crate::manifest::*;
use crate::model::{ActionKind, Severity};
use crate::recommendation::{MetricSnapshot, Proposal, RecDraft, Summary};
use std::collections::BTreeMap;

use serde_json::{json, Map};

pub struct ToolFailureClustering {
    manifest: AnalyzerManifest,
}

impl ToolFailureClustering {
    pub fn new() -> Self {
        ToolFailureClustering {
            manifest: AnalyzerManifest {
                id: "waiser.tool_failure/1".into(),
                title: "Tool-failure clustering".into(),
                description: "Clusters recurring tool failures into a memory lesson.".into(),
                tier: Tier::T0,
                cadence: CadenceClass::Batch,
                requires: vec![],
                target_classes: vec![TargetClass::Memory],
                auto_apply: AutoApplyClass::Never, // evidence-derived free text
                trust_class: TrustClass::Builtin,
                params: vec![
                    ParamSpec::Int {
                        name: "min_count".into(),
                        default: 3,
                        min: 1,
                        max: 1000,
                        description: "Minimum failures in a cluster to fire.".into(),
                    },
                    ParamSpec::Float {
                        name: "min_rate".into(),
                        default: 0.4,
                        min: 0.0,
                        max: 1.0,
                        description: "Minimum share of the tool's calls.".into(),
                    },
                    ParamSpec::Int {
                        name: "window_days".into(),
                        default: 30,
                        min: 1,
                        max: 365,
                        description: "Lookback window.".into(),
                    },
                ],
                default_on: true,
            },
        }
    }
}

impl Default for ToolFailureClustering {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for ToolFailureClustering {
    fn manifest(&self) -> &AnalyzerManifest {
        &self.manifest
    }

    fn analyze(&self, ctx: &AnalyzeCtx) -> Result<Vec<RecDraft>> {
        let min_count = ctx.params().get_int("min_count").max(1) as usize;
        let min_rate = ctx.params().get_float("min_rate");
        let window_ms = ctx.params().get_int("window_days") * 86_400_000;
        let since = Some(ctx.now_ms() - window_ms);

        let tools = ctx.tools_since(since)?;

        // Per-tool total calls, and per (tool, signature) error clusters.
        let mut tool_totals: BTreeMap<String, usize> = BTreeMap::new();
        let mut clusters: BTreeMap<(String, String), Vec<(String, i64)>> = BTreeMap::new();
        for e in &tools {
            let Some(tool) = e.tool_name() else {
                continue;
            };
            *tool_totals.entry(tool.to_string()).or_default() += 1;
            if e.is_error() {
                let sig = normalize_signature(e.tool_content().unwrap_or(""));
                clusters
                    .entry((tool.to_string(), sig))
                    .or_default()
                    .push((e.hash.clone(), e.created_at_ms));
            }
        }

        let mut drafts = Vec::new();
        for ((tool, signature), mut members) in clusters {
            let count = members.len();
            let total = tool_totals.get(&tool).copied().unwrap_or(count).max(1);
            let rate = count as f64 / total as f64;
            if count < min_count || rate < min_rate {
                continue;
            }
            members.sort_by(|a, b| a.0.cmp(&b.0));
            let evidence = bound_evidence(members.iter().map(|(h, _)| h.clone()).collect());
            let rate_pct = (rate * 100.0).round() as i64;

            let mut args = Map::new();
            args.insert("tool".into(), json!(tool));
            args.insert("count".into(), json!(count));
            args.insert("rate".into(), json!(rate_pct));
            args.insert("signature".into(), json!(signature));

            // Proposed lesson: a fact recording the recurring failure.
            let mut lesson = Map::new();
            lesson.insert("subject".into(), json!(tool));
            lesson.insert("relation".into(), json!("fails_with"));
            lesson.insert("object".into(), json!(signature));
            lesson.insert("confidence".into(), json!(rate));

            let severity = if rate >= 0.7 && count >= 5 {
                Severity::High
            } else if rate >= 0.5 {
                Severity::Medium
            } else {
                Severity::Low
            };

            drafts.push(
                RecDraft::new(
                    format!("entity:lessons/{tool}"),
                    ActionKind::ClusterFailure,
                    Summary::new("tool_failure.cluster", args),
                    Proposal::Cal {
                        cal: cal::add("fact", &lesson),
                    },
                )
                .severity(severity)
                .evidence(evidence)
                .confidence(rate)
                .metric(MetricSnapshot {
                    // After the lesson is applied, does this exact tool failure
                    // recur? Baseline 0 = we expect zero recurrences if the
                    // lesson worked; any recurrence is a regression → revert.
                    metric: "tool_error_recurrence".into(),
                    baseline: 0.0,
                    unit: "count".into(),
                    n: total as u64,
                    window: format!("{}d", ctx.params().get_int("window_days")),
                    subject: Some(tool.clone()),
                    query: format!("RECALL tools WHERE tool_name = \"{tool}\" AND is_error SINCE <applied_at> | COUNT"),
                    review_after_ms: 86_400_000,
                    // Re-measure at 1 day, 1 week, 1 month — a late recurrence
                    // (held at 1d, regressed at 30d) is caught by the schedule.
                    horizons_ms: vec![86_400_000, 7 * 86_400_000, 30 * 86_400_000],
                }),
            );
        }
        drafts.sort_by(|a, b| a.target_ref.cmp(&b.target_ref));
        Ok(drafts)
    }
}

/// Normalize an error message into a stable signature: lowercase, first ~80
/// chars, digit runs → `#`, path-like tokens → `<path>`. Regex-free (std only).
fn normalize_signature(content: &str) -> String {
    let lowered: String = content.trim().to_lowercase().chars().take(80).collect();
    let mut out = String::with_capacity(lowered.len());
    for token in lowered.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        if token.contains('/') || token.contains('\\') {
            out.push_str("<path>");
        } else {
            let mut prev_digit = false;
            for c in token.chars() {
                if c.is_ascii_digit() {
                    if !prev_digit {
                        out.push('#');
                    }
                    prev_digit = true;
                } else {
                    out.push(c);
                    prev_digit = false;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::TestSubstrate;

    #[test]
    fn signature_strips_digits_and_paths() {
        assert_eq!(
            normalize_signature("Rate limited after 4295 ms"),
            "rate limited after # ms"
        );
        assert_eq!(
            normalize_signature("open /etc/passwd failed"),
            "open <path> failed"
        );
    }

    #[test]
    fn fires_on_frequent_and_dominant_cluster() {
        let mut sub = TestSubstrate::new();
        for _ in 0..5 {
            sub.add_tool_call("stripe_refund", true, "rate_limited 429");
        }
        sub.add_tool_call("stripe_refund", false, "ok");
        let drafts = sub.analyze(&ToolFailureClustering::new(), 10_000);
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].action_kind, ActionKind::ClusterFailure);
        assert_eq!(drafts[0].evidence.len(), 5);
    }

    #[test]
    fn silent_below_rate_threshold() {
        let mut sub = TestSubstrate::new();
        sub.add_tool_call("search", true, "boom 500");
        sub.add_tool_call("search", true, "boom 500");
        for _ in 0..20 {
            sub.add_tool_call("search", false, "ok");
        }
        // 2 errors of 22 calls = 9% < 40%.
        assert!(sub
            .analyze(&ToolFailureClustering::new(), 10_000)
            .is_empty());
    }
}
