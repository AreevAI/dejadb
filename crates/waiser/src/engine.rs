//! The engine: the analyze → validate/dedup → store pipeline, the run-outcome
//! contract, and the review/apply/rollback lifecycle with the governance
//! gates. Deterministic given (store state, params, now) — the LLM path (§9)
//! is not in this increment; auto-apply execution is gated behind a
//! conservative shape check and stays off by default (build order: manage +
//! debug are the trust core; auto-apply lands with per-draft verification).

use crate::analyzer::{AnalyzeCtx, Analyzer, OutcomeInput};
use crate::cal;
use crate::config::{AppliedRecord, WaiserPersisted};
use crate::error::{Error, Result};
use crate::manifest::{AnalyzerManifest, Capability};
use crate::model::{Origin, Severity, TargetRef};
use crate::recommendation::{
    dedup_key, AuditRecord, ObserverType, Proposal, RecStatus, Recommendation, MAX_BECAUSE,
    MAX_EVIDENCE,
};
use crate::substrate::{Capabilities, OmsSubstrate, ReadOpts, SubstrateRead};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// The namespace waiser's own grains (recommendations, audit) live in.
pub const WAISER_NS: &str = "waiser";

/// Host-granted authority, per connection. `admin` implies all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Read,
    Write,
    Review,
    Apply,
    Admin,
}

/// A set of granted scopes.
#[derive(Debug, Clone, Default)]
pub struct ScopeSet(Vec<Scope>);

impl ScopeSet {
    pub fn of(scopes: &[Scope]) -> Self {
        ScopeSet(scopes.to_vec())
    }
    /// The local root of trust: whoever can run against the file holds all
    /// scopes (the CLI/embedded posture).
    pub fn all() -> Self {
        ScopeSet(vec![Scope::Admin])
    }
    pub fn has(&self, s: Scope) -> bool {
        self.0.contains(&Scope::Admin) || self.0.contains(&s)
    }
}

/// A review decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Reject,
}

/// Gating and scoping options for a run.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub min_new: Option<u64>,
    pub min_new_errors: Option<u64>,
    pub if_stale_ms: Option<i64>,
    /// Optional global namespace filter (empty = all).
    pub namespaces: Vec<String>,
}

/// Whether a run executed or was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunOutcome {
    Ran,
    Skipped,
}

/// Why a run was a no-op. `LockHeld` is produced by the host adapter (a
/// concurrent writer), surfaced here for a single contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    MinNewNotMet,
    NotStale,
    LockHeld,
}

/// One analyzer that did not contribute drafts, with why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnalyzerSkip {
    pub id: String,
    pub reason: String,
}

/// The run-outcome contract (proposal §13): one shape across CLI/API/MCP/bindings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    pub outcome: RunOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<SkipReason>,
    pub new_grains: u64,
    pub new_error_events: u64,
    pub proposed: u64,
    pub deduped: u64,
    pub stored: u64,
    #[serde(default)]
    pub analyzers_run: Vec<String>,
    #[serde(default)]
    pub analyzers_skipped: Vec<AnalyzerSkip>,
}

impl RunResult {
    fn skipped(reason: SkipReason, new_grains: u64, new_error_events: u64) -> Self {
        RunResult {
            outcome: RunOutcome::Skipped,
            skip_reason: Some(reason),
            new_grains,
            new_error_events,
            proposed: 0,
            deduped: 0,
            stored: 0,
            analyzers_run: vec![],
            analyzers_skipped: vec![],
        }
    }

    pub fn ran(&self) -> bool {
        self.outcome == RunOutcome::Ran
    }
}

/// The engine holds the registered analyzers.
pub struct Engine {
    analyzers: Vec<Box<dyn Analyzer>>,
}

impl Engine {
    /// An engine with the six default built-ins.
    pub fn with_builtins() -> Self {
        Engine {
            analyzers: crate::analyzer::builtin_analyzers(),
        }
    }

    /// An engine with no analyzers (register your own).
    pub fn empty() -> Self {
        Engine { analyzers: vec![] }
    }

    /// Register an additional analyzer (the linked-Rust seam).
    pub fn register(&mut self, analyzer: Box<dyn Analyzer>) {
        self.analyzers.push(analyzer);
    }

    pub fn analyzers(&self) -> &[Box<dyn Analyzer>] {
        &self.analyzers
    }

    /// Run one analysis pass. Idempotent under `dedup_key`; the watermark is
    /// advanced at the end, so a crashed run simply re-runs.
    pub fn run<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        opts: &RunOptions,
        now_ms: i64,
    ) -> Result<RunResult> {
        let mut persisted = WaiserPersisted::from_value(sub.load_state()?)?;
        let watermark = persisted.state.watermark_ms;

        let (new_grains, new_error_events) = count_new(sub, watermark)?;
        if let Some(reason) = gate(opts, &persisted, new_grains, new_error_events, now_ms) {
            return Ok(RunResult::skipped(reason, new_grains, new_error_events));
        }

        // Phase 0 (needs &mut): re-measure applied recommendations due for review.
        let outcome_inputs = self.compute_outcome_inputs(sub, &persisted, now_ms)?;

        // Existing live dedup keys (pending/approved) to suppress re-proposals.
        let existing = existing_dedup_keys(sub, &persisted)?;

        // Phase 1 (shared borrow): run each enabled analyzer.
        let mut analyzers_run = Vec::new();
        let mut analyzers_skipped = Vec::new();
        let mut candidates: Vec<Recommendation> = Vec::new();
        let caps = sub.capabilities();

        for analyzer in &self.analyzers {
            let m = analyzer.manifest();
            let cfg = persisted.config.get(&m.id);
            let enabled = cfg.and_then(|c| c.enabled).unwrap_or(m.default_on);
            if !enabled {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: "disabled".into(),
                });
                continue;
            }
            if let Some(missing) = missing_capability(m, caps) {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: format!("missing capability: {missing}"),
                });
                continue;
            }
            let overrides = cfg.map(|c| c.params.clone()).unwrap_or_default();
            let params = match m.resolve_params(&overrides) {
                Ok(p) => p,
                Err(e) => {
                    analyzers_skipped.push(AnalyzerSkip {
                        id: m.id.clone(),
                        reason: e.to_string(),
                    });
                    continue;
                }
            };
            let ns_owned = cfg.map(|c| c.namespaces.clone()).unwrap_or_default();
            let ns_slice: &[String] = if ns_owned.is_empty() {
                &opts.namespaces
            } else {
                &ns_owned
            };

            let reader: &dyn SubstrateRead = &*sub;
            let ctx = AnalyzeCtx::new(
                reader,
                &params,
                ns_slice,
                watermark,
                now_ms,
                &outcome_inputs,
            );
            match analyzer.analyze(&ctx) {
                Ok(drafts) => {
                    analyzers_run.push(m.id.clone());
                    for d in drafts {
                        match stamp(m, &params, d, now_ms) {
                            Ok(rec) => candidates.push(rec),
                            Err(e) => analyzers_skipped.push(AnalyzerSkip {
                                id: m.id.clone(),
                                reason: e.to_string(),
                            }),
                        }
                    }
                }
                Err(e) => analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: e.to_string(),
                }),
            }
        }

        // Phase 2: validate/dedup.
        let proposed = candidates.len() as u64;
        let mut seen = BTreeSet::new();
        let mut survivors = Vec::new();
        for c in candidates {
            if let Some(floor) = severity_floor_for(&persisted, &c.analyzer) {
                if c.severity < floor {
                    continue;
                }
            }
            if !seen.insert(c.dedup_key.clone()) {
                continue; // within-run duplicate
            }
            if existing.contains(&c.dedup_key) {
                continue; // already queued live
            }
            if let Some(&until) = persisted.cooldowns.get(&c.dedup_key) {
                if now_ms < until {
                    continue; // rejected recently
                }
            }
            survivors.push(c);
        }
        let deduped = proposed - survivors.len() as u64;

        // Phase 3 (needs &mut): store survivors + propose audit.
        let mut stored = 0u64;
        for mut rec in survivors {
            let spec = rec.to_grain_spec(WAISER_NS)?;
            let hash = sub.put_grain(&spec)?;
            rec.hash = hash.clone();
            let actor = format!("engine:{}", rec.analyzer);
            let audit = AuditRecord {
                rec_hash: hash.clone(),
                from: None,
                to: RecStatus::Pending,
                actor: actor.clone(),
                observer_type: ObserverType::System,
                because: "analyzer proposed".into(),
                previous_audit_hash: None,
                at_ms: now_ms,
            };
            let audit_hash = sub.put_grain(&audit.to_grain_spec(WAISER_NS))?;
            persisted
                .status_index
                .insert(hash.clone(), RecStatus::Pending);
            persisted.creators.insert(hash.clone(), actor);
            persisted.audit_heads.insert(hash, audit_hash);
            stored += 1;
        }

        persisted.state.last_run_ms = Some(now_ms);
        persisted.state.watermark_ms = Some(now_ms);
        sub.store_state(&persisted.to_value()?)?;

        Ok(RunResult {
            outcome: RunOutcome::Ran,
            skip_reason: None,
            new_grains,
            new_error_events,
            proposed,
            deduped,
            stored,
            analyzers_run,
            analyzers_skipped,
        })
    }

    /// Approve or reject a pending recommendation. Requires the `review` scope,
    /// a mandatory BECAUSE, and blocks self-approval against the creating actor.
    #[allow(clippy::too_many_arguments)]
    pub fn review<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        decision: Decision,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        now_ms: i64,
    ) -> Result<()> {
        if !scopes.has(Scope::Review) {
            return Err(Error::ScopeDenied("review".into()));
        }
        let because = validate_because(because)?;
        let mut p = WaiserPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        let to = match decision {
            Decision::Approve => RecStatus::Approved,
            Decision::Reject => RecStatus::Rejected,
        };
        if !status.can_transition_to(to, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> {}",
                status.as_str(),
                to.as_str()
            )));
        }
        if to == RecStatus::Approved {
            if let Some(creator) = p.creators.get(rec_hash) {
                if creator == actor {
                    return Err(Error::SelfApproval(format!(
                        "{actor} created this recommendation"
                    )));
                }
            }
        }
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(WAISER_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index.insert(rec_hash.into(), to);
        if to == RecStatus::Rejected {
            if let Ok(rec) = load_rec(sub, rec_hash) {
                // Cooldown (doubling keyed on dedup_key; base 7d).
                let base = 7 * 86_400_000;
                let until = now_ms + base;
                p.cooldowns.insert(rec.dedup_key, until);
            }
        }
        sub.store_state(&p.to_value()?)?;
        Ok(())
    }

    /// Apply an approved recommendation. Requires `apply`; destructive payloads
    /// additionally require `admin` + `allow_destructive`. Records the applied
    /// info (inverse plan) for rollback.
    #[allow(clippy::too_many_arguments)]
    pub fn apply<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        allow_destructive: bool,
        now_ms: i64,
    ) -> Result<AppliedRecord> {
        if !scopes.has(Scope::Apply) {
            return Err(Error::ScopeDenied("apply".into()));
        }
        let because = validate_because(because)?;
        let mut p = WaiserPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        if !status.can_transition_to(RecStatus::Applied, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> applied (approve first)",
                status.as_str()
            )));
        }
        let rec = load_rec(sub, rec_hash)?;
        if rec.destructive && (!scopes.has(Scope::Admin) || !allow_destructive) {
            return Err(Error::DestructiveGated(
                "destructive apply requires admin scope + allow_destructive".into(),
            ));
        }

        // Execute the proposal.
        let mut created = Vec::new();
        match &rec.proposal {
            Proposal::Cal { cal } => {
                let rows = sub.execute_cal(cal)?;
                for r in rows {
                    if let Some(h) = r.get("hash").and_then(Value::as_str) {
                        created.push(h.to_string());
                    }
                }
            }
            // Doc/host targets are applied in the host's world (§12.3); the
            // engine records mark-applied without a store write.
            Proposal::Edit { .. } | Proposal::Data { .. } => {}
        }

        let applied = AppliedRecord {
            applied_at_ms: now_ms,
            target_ref: rec.target_ref.clone(),
            rollbackable: rec.rollbackable,
            created_hashes: created,
            metric: rec.metric.clone(),
        };
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to: RecStatus::Applied,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(WAISER_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index.insert(rec_hash.into(), RecStatus::Applied);
        p.applied.insert(rec_hash.into(), applied.clone());
        sub.store_state(&p.to_value()?)?;
        Ok(applied)
    }

    /// Roll back an applied recommendation by retracting the grains it created.
    /// Fails for non-rollbackable applies (e.g. FORGET).
    #[allow(clippy::too_many_arguments)]
    pub fn rollback<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        rec_hash: &str,
        actor: &str,
        observer: ObserverType,
        scopes: &ScopeSet,
        because: &str,
        now_ms: i64,
    ) -> Result<()> {
        if !scopes.has(Scope::Apply) {
            return Err(Error::ScopeDenied("apply".into()));
        }
        let because = validate_because(because)?;
        let mut p = WaiserPersisted::from_value(sub.load_state()?)?;
        let status = *p
            .status_index
            .get(rec_hash)
            .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
        if !status.can_transition_to(RecStatus::RolledBack, false) {
            return Err(Error::LifecycleViolation(format!(
                "{} -> rolled_back",
                status.as_str()
            )));
        }
        let applied = p
            .applied
            .get(rec_hash)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no applied record for {rec_hash}")))?;
        if !applied.rollbackable {
            return Err(Error::LifecycleViolation(
                "recommendation is non-rollbackable (FORGET has no inverse)".into(),
            ));
        }
        for h in &applied.created_hashes {
            sub.retract(h, &format!("rollback of {rec_hash}"))?;
        }
        let prev = p.audit_heads.get(rec_hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec_hash.into(),
            from: Some(status),
            to: RecStatus::RolledBack,
            actor: actor.into(),
            observer_type: observer,
            because,
            previous_audit_hash: prev,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(WAISER_NS))?;
        p.audit_heads.insert(rec_hash.into(), audit_hash);
        p.status_index
            .insert(rec_hash.into(), RecStatus::RolledBack);
        sub.store_state(&p.to_value()?)?;
        Ok(())
    }

    /// List stored recommendations, optionally filtered by status. Status comes
    /// from the rebuildable index, not the immutable grain body.
    pub fn recommendations<S: OmsSubstrate>(
        &self,
        sub: &S,
        status_filter: Option<RecStatus>,
    ) -> Result<Vec<Recommendation>> {
        let p = WaiserPersisted::from_value(sub.load_state()?)?;
        let grains = sub.grains_of_type(
            crate::model::grain_type::RECOMMENDATION,
            Some(WAISER_NS),
            ReadOpts {
                live_only: false,
                since_ms: None,
            },
        )?;
        let mut out = Vec::new();
        for g in grains {
            let mut rec = Recommendation::from_fields(&g.hash, &g.fields)?;
            rec.status = p
                .status_index
                .get(&g.hash)
                .copied()
                .unwrap_or(RecStatus::Pending);
            if let Some(f) = status_filter {
                if rec.status != f {
                    continue;
                }
            }
            out.push(rec);
        }
        out.sort_by(|a, b| a.hash.cmp(&b.hash));
        Ok(out)
    }

    fn compute_outcome_inputs<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        p: &WaiserPersisted,
        now_ms: i64,
    ) -> Result<Vec<OutcomeInput>> {
        let mut out = Vec::new();
        for (rec_hash, applied) in &p.applied {
            if p.status_index.get(rec_hash) != Some(&RecStatus::Applied) {
                continue;
            }
            let Some(metric) = &applied.metric else {
                continue;
            };
            if now_ms - applied.applied_at_ms < metric.review_after_ms {
                continue;
            }
            let rows = sub.execute_cal(&metric.query)?;
            let Some(current) = extract_metric_value(&rows) else {
                continue;
            };
            out.push(OutcomeInput {
                rec_hash: rec_hash.clone(),
                target_ref: applied.target_ref.clone(),
                metric: metric.metric.clone(),
                baseline: metric.baseline,
                current,
                unit: metric.unit.clone(),
            });
        }
        Ok(out)
    }
}

// --- free helpers ---

fn stamp(
    m: &AnalyzerManifest,
    params: &crate::manifest::Params,
    d: crate::recommendation::RecDraft,
    now_ms: i64,
) -> Result<Recommendation> {
    let target = TargetRef::parse(&d.target_ref)?;
    let dedup = dedup_key(m.family(), &d.target_ref, d.action_kind);
    let destructive = match &d.proposal {
        Proposal::Cal { cal } => cal::contains_forget(cal),
        _ => false,
    };
    let rollbackable = match &d.proposal {
        Proposal::Cal { .. } => !destructive,
        Proposal::Edit { .. } => true,
        Proposal::Data { .. } => false,
    };
    let mut evidence = d.evidence;
    evidence.truncate(MAX_EVIDENCE);
    Ok(Recommendation {
        hash: String::new(),
        analyzer: m.id.clone(),
        params_snapshot: params.snapshot(),
        origin: Origin::Builtin,
        target_ref: target.as_string(),
        action_kind: d.action_kind,
        dedup_key: dedup,
        summary: d.summary,
        severity: d.severity,
        proposal: d.proposal,
        destructive,
        rollbackable,
        evidence,
        evidence_query: d.evidence_query,
        metric: d.metric,
        confidence: d.confidence,
        importance: d.importance,
        created_at_ms: now_ms,
        status: RecStatus::Pending,
    })
}

fn validate_because(because: &str) -> Result<String> {
    let trimmed = because.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidProposal(
            "a BECAUSE reason is required".into(),
        ));
    }
    if trimmed.chars().count() > MAX_BECAUSE {
        return Err(Error::InvalidProposal(format!(
            "BECAUSE exceeds {MAX_BECAUSE} chars"
        )));
    }
    Ok(trimmed.to_string())
}

fn missing_capability(m: &AnalyzerManifest, caps: Capabilities) -> Option<&'static str> {
    for req in &m.requires {
        match req {
            Capability::Forks if !caps.forks => return Some("forks"),
            Capability::Telemetry if !caps.telemetry => return Some("telemetry"),
            Capability::Embeddings if !caps.embeddings => return Some("embeddings"),
            _ => {}
        }
    }
    None
}

fn severity_floor_for(p: &WaiserPersisted, analyzer_id: &str) -> Option<Severity> {
    p.config.get(analyzer_id).and_then(|c| c.severity_floor)
}

fn gate(
    opts: &RunOptions,
    p: &WaiserPersisted,
    new_grains: u64,
    new_errors: u64,
    now_ms: i64,
) -> Option<SkipReason> {
    let any = opts.min_new.is_some() || opts.min_new_errors.is_some() || opts.if_stale_ms.is_some();
    if !any {
        return None;
    }
    let min_new_ok = opts.min_new.is_some_and(|m| new_grains >= m);
    let min_err_ok = opts.min_new_errors.is_some_and(|m| new_errors >= m);
    let stale_ok = opts
        .if_stale_ms
        .is_some_and(|d| p.state.last_run_ms.is_none_or(|last| now_ms - last >= d));
    if min_new_ok || min_err_ok || stale_ok {
        return None;
    }
    // Choose the honest reason: staleness-only gate → not_stale, else min_new.
    if opts.if_stale_ms.is_some() && opts.min_new.is_none() && opts.min_new_errors.is_none() {
        Some(SkipReason::NotStale)
    } else {
        Some(SkipReason::MinNewNotMet)
    }
}

fn count_new<S: SubstrateRead>(sub: &S, watermark: Option<i64>) -> Result<(u64, u64)> {
    let opts = ReadOpts {
        live_only: false,
        since_ms: watermark.map(|w| w + 1),
    };
    let mut new_grains = 0u64;
    let mut new_errors = 0u64;
    for t in [
        crate::model::grain_type::FACT,
        crate::model::grain_type::EVENT,
        crate::model::grain_type::TOOL,
        crate::model::grain_type::OBSERVATION,
    ] {
        let g = sub.grains_of_type(t, None, opts)?;
        new_grains += g.len() as u64;
        // The error gate (--min-new-errors) watches captured tool failures.
        if t == crate::model::grain_type::TOOL {
            new_errors += g.iter().filter(|e| e.is_error()).count() as u64;
        }
    }
    Ok((new_grains, new_errors))
}

fn existing_dedup_keys<S: SubstrateRead>(sub: &S, p: &WaiserPersisted) -> Result<BTreeSet<String>> {
    let grains = sub.grains_of_type(
        crate::model::grain_type::RECOMMENDATION,
        Some(WAISER_NS),
        ReadOpts {
            live_only: false,
            since_ms: None,
        },
    )?;
    let mut set = BTreeSet::new();
    for g in grains {
        let status = p
            .status_index
            .get(&g.hash)
            .copied()
            .unwrap_or(RecStatus::Pending);
        // Only live (pending/approved) recommendations suppress re-proposal.
        if matches!(status, RecStatus::Pending | RecStatus::Approved) {
            if let Some(key) = g.str_field("dedup_key") {
                set.insert(key.to_string());
            }
        }
    }
    Ok(set)
}

fn load_rec<S: SubstrateRead>(sub: &S, rec_hash: &str) -> Result<Recommendation> {
    let g = sub
        .grain(rec_hash)?
        .ok_or_else(|| Error::NotFound(rec_hash.into()))?;
    Recommendation::from_fields(rec_hash, &g.fields)
}

fn extract_metric_value(rows: &[Value]) -> Option<f64> {
    rows.first()?.get("value").and_then(Value::as_f64)
}
