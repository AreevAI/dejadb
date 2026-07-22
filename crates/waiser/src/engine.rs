//! The engine: the analyze → DISCOVER → ENRICH → validate/dedup → store
//! pipeline, the run-outcome contract, and the review/apply/rollback lifecycle
//! with the governance gates. The DETERMINISTIC output is a pure function of
//! (store state, params, now); the optional LLM stages (§9) only *add* cited
//! drafts (origin=llm, never auto-apply) and whitelisted guidance — with no
//! backend they are the identity, so the deterministic path is unchanged.
//! Auto-apply execution is gated behind a conservative shape check and stays
//! off by default.

use crate::analyzer::{AnalyzeCtx, Analyzer, OutcomeInput};
use crate::cal;
use crate::config::{AppliedRecord, WaiserPersisted};
use crate::error::{Error, Result};
use crate::manifest::{AnalyzerManifest, Capability};
use crate::model::{normalize_ident, ActionKind, GrainRecord, Origin, Severity, TargetRef};
use crate::recommendation::{
    dedup_key, AuditRecord, ObserverType, Proposal, RecStatus, Recommendation, Summary,
    MAX_BECAUSE, MAX_EVIDENCE,
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
    /// Re-analyze the WHOLE memory this pass, not just grains since the last-run
    /// watermark (`deja waiser reflect`). Dedup/cooldowns still suppress anything
    /// already queued, and the watermark still advances at the end — so a sweep
    /// is safe to run any time and later runs stay incremental. Mainly widens the
    /// watermark-sensitive inputs (tool-failure window, the non-parasitic LLM
    /// evidence bundle) to the full history.
    pub full_sweep: bool,
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
    /// Of the stored recommendations, how many were auto-applied by policy.
    #[serde(default)]
    pub auto_applied: u64,
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
            auto_applied: 0,
            analyzers_run: vec![],
            analyzers_skipped: vec![],
        }
    }

    pub fn ran(&self) -> bool {
        self.outcome == RunOutcome::Ran
    }
}

/// The engine holds the registered analyzers, the host policy, and an optional
/// LLM enrichment backend (§9).
pub struct Engine {
    analyzers: Vec<Box<dyn Analyzer>>,
    policy: crate::policy::Policy,
    /// Optional LLM backend. `None` → the DISCOVER/ENRICH stages are the
    /// identity, so the pipeline is byte-for-byte the deterministic path.
    llm: Option<Box<dyn crate::llm::LlmBackend>>,
    /// Optional separate backend for the GROUND stage (§5.2, §11). `None` →
    /// grounding rides `llm`. Lets a team point entailment at a cheaper or
    /// specialized model (or take the generative model out of grounding
    /// entirely) without changing the proposer/verifier.
    ground_llm: Option<Box<dyn crate::llm::LlmBackend>>,
}

impl Engine {
    /// An engine with the default built-ins and a default (fully closed)
    /// policy — nothing auto-applies, no LLM.
    pub fn with_builtins() -> Self {
        Engine {
            analyzers: crate::analyzer::builtin_analyzers(),
            policy: crate::policy::Policy::default(),
            llm: None,
            ground_llm: None,
        }
    }

    /// An engine with no analyzers (register your own).
    pub fn empty() -> Self {
        Engine {
            analyzers: vec![],
            policy: crate::policy::Policy::default(),
            llm: None,
            ground_llm: None,
        }
    }

    /// Install a host policy (the only place auto-apply is granted).
    pub fn with_policy(mut self, policy: crate::policy::Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Attach an optional LLM enrichment backend (§9). Only ever *adds* cited
    /// draft recommendations (stamped `origin = llm`, never auto-applied) and
    /// whitelisted guidance notes — it can never gate or rewrite deterministic
    /// output.
    pub fn with_llm(mut self, backend: Box<dyn crate::llm::LlmBackend>) -> Self {
        self.llm = Some(backend);
        self
    }

    /// Attach a separate backend for the GROUND stage (§5.2). Without this,
    /// grounding uses the `with_llm` backend. Independent of the proposer so an
    /// operator can run entailment on a cheaper/specialized model.
    pub fn with_ground_llm(mut self, backend: Box<dyn crate::llm::LlmBackend>) -> Self {
        self.ground_llm = Some(backend);
        self
    }

    pub fn policy(&self) -> &crate::policy::Policy {
        &self.policy
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
        // A full sweep analyzes the whole memory (watermark ignored for the
        // analysis inputs), while gating, `new` counts, and the end-of-run
        // watermark advance still use the real watermark. Dedup/cooldowns keep
        // it from re-proposing what is already queued.
        let analysis_watermark = if opts.full_sweep { None } else { watermark };

        let (new_grains, new_error_events) = count_new(sub, watermark)?;
        if let Some(reason) = gate(opts, &persisted, new_grains, new_error_events, now_ms) {
            return Ok(RunResult::skipped(reason, new_grains, new_error_events));
        }

        // Phase 0: re-measure applied recommendations due for review (the
        // Verify gate). Records a measured outcome per due recommendation.
        let outcome_inputs = measure_outcomes(sub, &mut persisted, now_ms)?;

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
            if self.policy.denies(m.family()) {
                analyzers_skipped.push(AnalyzerSkip {
                    id: m.id.clone(),
                    reason: "denied by host policy".into(),
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
                analysis_watermark,
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

        // DISCOVER (§9, optional): the LLM proposes additional cited drafts,
        // stamped origin=llm (never auto-apply). Identity when no backend; a
        // failed/garbled call drops the contribution, never the run.
        if self.llm.is_some() {
            let discovered =
                self.discover(&*sub, &candidates, analysis_watermark, &opts.namespaces, now_ms);
            candidates.extend(discovered);
        }

        // Phase 2: validate/dedup. The dedup_key (family+target+action)
        // collapses TRUE duplicates (e.g. the same finding across two runs) but
        // deliberately keeps distinct-family findings on one entity — an entity
        // can have a contradiction AND a staleness AND an llm-found semantic
        // issue at once. Novelty of llm findings vs determinism is a semantic
        // judgement, steered at DISCOVER ("find what determinism misses") and
        // settled by human review — not an entity-level drop here (too coarse).
        let proposed = candidates.len() as u64;
        let mut seen = BTreeSet::new();
        let mut survivors = Vec::new();
        for c in candidates {
            // Effective floor = the stricter of the file config and host policy.
            let family = crate::manifest::analyzer_family(&c.analyzer);
            let floor = [severity_floor_for(&persisted, &c.analyzer), self.policy.severity_floor(family)]
                .into_iter()
                .flatten()
                .max();
            if let Some(floor) = floor {
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

        // ENRICH (§9, optional): the LLM adds a whitelisted guidance note to
        // deterministic survivors. The engine-templated summary is untouched.
        if self.llm.is_some() {
            self.enrich(&mut survivors);
        }

        // Phase 3 (needs &mut): store survivors + propose audit, then
        // auto-apply the ones the host policy grants (all gates in §6.3).
        let mut stored = 0u64;
        let mut auto_applied = 0u64;
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
            persisted.audit_heads.insert(hash.clone(), audit_hash);
            stored += 1;

            if self.can_auto_apply(&*sub, &rec) {
                self.auto_apply(sub, &mut persisted, &rec, now_ms)?;
                auto_applied += 1;
            }
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
            auto_applied,
            analyzers_run,
            analyzers_skipped,
        })
    }

    /// DISCOVER (§9): ask the LLM for additional draft recommendations, given
    /// the deterministic findings as *context* and a bounded, provenance-tagged
    /// evidence bundle. Every returned draft must cite evidence present in the
    /// bundle and target a memory/query surface; it is stamped `origin = llm`
    /// (so it can never auto-apply) and enters the ordinary dedup/store path. A
    /// failed or garbled response yields no drafts — never a failed run.
    fn discover<S: OmsSubstrate>(
        &self,
        sub: &S,
        candidates: &[Recommendation],
        watermark: Option<i64>,
        namespaces: &[String],
        now_ms: i64,
    ) -> Vec<Recommendation> {
        let Some(llm) = &self.llm else {
            return Vec::new();
        };
        let findings: Vec<crate::llm::FindingBrief> = candidates
            .iter()
            .take(32)
            .map(|c| crate::llm::FindingBrief {
                analyzer: c.analyzer.clone(),
                summary: c.summary.render(),
                target: c.target_ref.clone(),
                severity: c.severity.as_str().to_string(),
            })
            .collect();
        // Evidence bundle. Seeded first from the grains the deterministic
        // findings cite, THEN — the non-parasitic step (§11) — topped up with
        // RECENT grains (created since the last run) so the LLM gets its own
        // lens and can find issues in grains no analyzer flagged. Without this
        // the LLM could only elaborate near what determinism already caught.
        let mut evidence: Vec<crate::llm::EvidenceItem> = Vec::new();
        let mut bundle: BTreeSet<String> = BTreeSet::new();
        for c in candidates {
            for h in &c.evidence {
                if !bundle.contains(h) {
                    if let Ok(Some(g)) = sub.grain(h) {
                        push_evidence(&mut evidence, &mut bundle, &g);
                    }
                }
            }
        }
        let scan_ns: Vec<Option<&str>> = if namespaces.is_empty() {
            vec![None]
        } else {
            namespaces.iter().map(|n| Some(n.as_str())).collect()
        };
        let opts = ReadOpts { live_only: true, since_ms: watermark };
        'seed: for gt in [
            crate::model::grain_type::FACT,
            crate::model::grain_type::OBSERVATION,
        ] {
            for ns in &scan_ns {
                if let Ok(recent) = sub.grains_of_type(gt, *ns, opts) {
                    for g in recent {
                        if evidence.len() >= 64 {
                            break 'seed;
                        }
                        push_evidence(&mut evidence, &mut bundle, &g);
                    }
                }
            }
        }
        if evidence.is_empty() {
            return Vec::new(); // nothing to reflect on
        }
        // PROPOSE (§5.1): the abstention-legitimate objective — "nothing to
        // report" is a first-class, zero-penalty answer. The operator-taste
        // history (recent approve/reject decisions on llm findings) is passed so
        // the model learns what this reviewer accepts.
        let (approved, rejected) = self.llm_history(sub);
        let request = crate::llm::LlmRequest {
            waiser: 1,
            op: "discover",
            instructions: DISCOVER_INSTRUCTIONS,
            findings: findings.clone(),
            evidence: evidence.clone(),
            rejected,
            approved,
        };
        let Ok(body) = serde_json::to_string(&request) else {
            return Vec::new();
        };
        let raw = match llm.complete(&body) {
            Ok(r) => r,
            Err(_) => return Vec::new(), // fail-soft
        };
        // Cheap structural validation (cite-check + target class); collect the
        // survivors for the verifier. Storing the normalized target string
        // avoids a TargetRef clone through the pipeline.
        let mut validated: Vec<(crate::llm::LlmDraft, String, Vec<String>)> = Vec::new();
        for d in crate::llm::parse_discover(&raw)
            .recommendations
            .into_iter()
            .take(crate::llm::MAX_LLM_DRAFTS)
        {
            let cited: Vec<String> =
                d.evidence.iter().filter(|h| bundle.contains(*h)).cloned().collect();
            if cited.is_empty() {
                continue; // uncited → drop (no fabrication)
            }
            let Ok(target) = TargetRef::parse(&d.target) else {
                continue;
            };
            let tc = target.target_class();
            if tc != "memory" && tc != "query" {
                continue; // never prompt/host
            }
            validated.push((d, target.as_string(), cited));
        }
        if validated.is_empty() {
            return Vec::new();
        }
        // GROUND → VERIFY → ROUTE (§5.2–5.4): only drafts that survive an
        // independent grounding entailment check *and* an adversarial
        // verification pass (each a separate call — proposer ≠ scorer) reach the
        // queue, stamped with the verifier's calibrated confidence.
        // GROUND may run on a separate backend (§11); VERIFY always uses the
        // main llm (the proposer≠scorer independence is on VERIFY, not GROUND).
        let ground = self.ground_llm.as_deref().unwrap_or(&**llm);
        self.verify_drafts(&**llm, ground, &validated, &evidence, now_ms)
    }

    /// GROUND → VERIFY → ROUTE (§5.2–5.4). Two independent model calls, batched
    /// over the drafts: a grounding-entailment gate ("does the cited evidence
    /// support the claim?"), then an adversarial keep/kill with a calibrated
    /// confidence. A draft reaches the queue only if it is grounded **and** kept
    /// **and** clears the confidence floor. Any failed call drops the whole LLM
    /// contribution for the run (safe default), never the run.
    fn verify_drafts(
        &self,
        llm: &dyn crate::llm::LlmBackend,
        ground: &dyn crate::llm::LlmBackend,
        validated: &[(crate::llm::LlmDraft, String, Vec<String>)],
        evidence: &[crate::llm::EvidenceItem],
        now_ms: i64,
    ) -> Vec<Recommendation> {
        use crate::llm::*;
        let ev_by_hash: std::collections::BTreeMap<&str, &EvidenceItem> =
            evidence.iter().map(|e| (e.hash.as_str(), e)).collect();
        let ev_for = |cited: &[String]| -> Vec<EvidenceItem> {
            cited
                .iter()
                .filter_map(|h| ev_by_hash.get(h.as_str()).map(|e| (*e).clone()))
                .collect()
        };

        // GROUND (§5.2): decompose-then-entail per draft, batched into one call.
        let claims: Vec<GroundItem> = validated
            .iter()
            .enumerate()
            .map(|(i, (d, _t, cited))| GroundItem {
                id: i,
                claim: cap(&d.summary, MAX_SUMMARY_LEN),
                evidence: ev_for(cited),
            })
            .collect();
        let ground_req = GroundRequest {
            waiser: 1,
            op: "ground",
            instructions: GROUND_INSTRUCTIONS,
            claims,
        };
        let grounded: std::collections::BTreeSet<usize> = match serde_json::to_string(&ground_req)
            .ok()
            .and_then(|b| ground.complete(&b).ok())
        {
            Some(raw) => parse_ground(&raw)
                .results
                .into_iter()
                .filter(|r| r.supported)
                .map(|r| r.id)
                .collect(),
            None => return Vec::new(),
        };
        if grounded.is_empty() {
            return Vec::new();
        }

        // VERIFY (§5.3): adversarial keep/kill over the grounded drafts, a
        // separate call from the proposer. Soundness + abstention only — NOT
        // novelty. Novelty is steered at DISCOVER and settled by human review;
        // asking a weak verifier to judge it just makes it hallucinate "already
        // known" and kill genuine findings (§11).
        let items: Vec<VerifyItem> = validated
            .iter()
            .enumerate()
            .filter(|(i, _)| grounded.contains(i))
            .map(|(i, (d, t, cited))| VerifyItem {
                id: i,
                summary: cap(&d.summary, MAX_SUMMARY_LEN),
                target: t.clone(),
                evidence: ev_for(cited),
            })
            .collect();
        let verify_req = VerifyRequest {
            waiser: 1,
            op: "verify",
            instructions: VERIFY_INSTRUCTIONS,
            findings: items,
        };
        let verdicts: std::collections::BTreeMap<usize, f64> =
            match serde_json::to_string(&verify_req).ok().and_then(|b| llm.complete(&b).ok()) {
                Some(raw) => parse_verify(&raw)
                    .results
                    .into_iter()
                    .filter(|r| r.keep)
                    .map(|r| (r.id, r.confidence.clamp(0.0, 1.0)))
                    .collect(),
                None => return Vec::new(),
            };

        // ROUTE (§5.4): grounded ∧ kept ∧ verifier-confidence ≥ floor. The
        // verifier's confidence (the independent signal) is what we trust and
        // stamp — not the proposer's self-report.
        let mut out = Vec::new();
        for (i, (d, target_str, cited)) in validated.iter().enumerate() {
            if let Some(&conf) = verdicts.get(&i) {
                if conf >= MIN_LLM_CONFIDENCE {
                    out.push(stamp_llm(
                        llm.model(),
                        d,
                        target_str.clone(),
                        cited.clone(),
                        conf,
                        now_ms,
                    ));
                }
            }
        }
        out
    }

    /// Recent operator decisions on `origin = llm` findings — approved (incl.
    /// applied) and rejected summaries, most-recent first and bounded — so
    /// DISCOVER can learn what this reviewer accepts (§9). Best-effort: a read
    /// failure yields empty history, never an error.
    fn llm_history<S: OmsSubstrate>(&self, sub: &S) -> (Vec<String>, Vec<String>) {
        const MAX: usize = 20;
        let Ok(mut recs) = self.recommendations(sub, None) else {
            return (Vec::new(), Vec::new());
        };
        recs.retain(|r| matches!(r.origin, Origin::Llm { .. }));
        recs.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        let mut approved = Vec::new();
        let mut rejected = Vec::new();
        for r in &recs {
            match r.status {
                RecStatus::Approved | RecStatus::Applied | RecStatus::RolledBack
                    if approved.len() < MAX =>
                {
                    approved.push(r.summary.render());
                }
                RecStatus::Rejected if rejected.len() < MAX => rejected.push(r.summary.render()),
                _ => {}
            }
        }
        (approved, rejected)
    }

    /// ENRICH (§9): ask the LLM to add a short guidance note to the surviving
    /// deterministic recommendations. Whitelist-only — only `guidance` is
    /// merged (capped), and only onto recs that don't already have one; the
    /// engine-templated summary is never touched. Fail-soft.
    fn enrich(&self, survivors: &mut [Recommendation]) {
        let Some(llm) = &self.llm else {
            return;
        };
        if survivors.is_empty() {
            return;
        }
        let findings: Vec<crate::llm::FindingBrief> = survivors
            .iter()
            .map(|r| crate::llm::FindingBrief {
                analyzer: r.analyzer.clone(),
                summary: r.summary.render(),
                target: r.target_ref.clone(),
                severity: r.severity.as_str().to_string(),
            })
            .collect();
        let request = crate::llm::LlmRequest {
            waiser: 1,
            op: "enrich",
            instructions: ENRICH_INSTRUCTIONS,
            findings,
            evidence: Vec::new(),
            rejected: Vec::new(),
            approved: Vec::new(),
        };
        let Ok(body) = serde_json::to_string(&request) else {
            return;
        };
        let raw = match llm.complete(&body) {
            Ok(r) => r,
            Err(_) => return,
        };
        for note in crate::llm::parse_enrich(&raw).notes {
            if note.guidance.trim().is_empty() {
                continue;
            }
            if let Some(r) = survivors
                .iter_mut()
                .find(|r| r.target_ref == note.target && r.guidance.is_none())
            {
                r.guidance = Some(crate::llm::cap(&note.guidance, crate::llm::MAX_GUIDANCE_LEN));
            }
        }
    }

    /// Evaluate the auto-apply gate (§6.3) — ALL preconditions must hold:
    /// host opt-in + policy grant, builtin origin, memory/query target,
    /// non-destructive, and engine-side shape verification: SUPERSEDE-only
    /// structural curation (never an ADD that introduces evidence-derived
    /// text) whose every replacement is **value-identical** to the grain it
    /// supersedes (the exact-equality check — a near-duplicate consolidation
    /// stays pending). A default (closed) policy never grants, so nothing
    /// auto-applies.
    fn can_auto_apply<S: OmsSubstrate>(&self, sub: &S, rec: &Recommendation) -> bool {
        if !rec.origin.auto_apply_eligible() || rec.destructive {
            return false;
        }
        // The analyzer must declare its curation auto-appliable. An analyzer
        // whose manifest is `Never` (e.g. fork surfacing — a lossy merge) is
        // never auto-applied even if the payload passes the shape check.
        let manifest_ok = self
            .analyzers
            .iter()
            .map(|a| a.manifest())
            .find(|m| m.id == rec.analyzer)
            .is_some_and(|m| m.auto_apply == crate::manifest::AutoApplyClass::StructuralCuration);
        if !manifest_ok {
            return false;
        }
        let Ok(target) = TargetRef::parse(&rec.target_ref) else {
            return false;
        };
        let family = crate::manifest::analyzer_family(&rec.analyzer);
        if !self.policy.grants_auto_apply(family, target.target_class(), rec.severity) {
            return false;
        }
        // Shape verification: only a CAL batch of pure SUPERSEDE statements
        // whose replacements change no value is structural curation. An ADD
        // (introducing content), a FORGET (destructive), or a supersession
        // that alters any field disqualifies.
        match &rec.proposal {
            Proposal::Cal { cal } => cal
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .all(|l| supersede_is_value_identical(sub, l)),
            _ => false,
        }
    }

    /// Apply a recommendation as `policy:auto` (the only `pending → applied`
    /// path). Records the applied inverse + a hash-chained audit grain.
    fn auto_apply<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        p: &mut WaiserPersisted,
        rec: &Recommendation,
        now_ms: i64,
    ) -> Result<()> {
        let mut created = Vec::new();
        if let Proposal::Cal { cal } = &rec.proposal {
            for r in sub.execute_cal(cal)? {
                if let Some(h) = r.get("hash").and_then(Value::as_str) {
                    created.push(h.to_string());
                }
            }
        }
        let applied = AppliedRecord {
            applied_at_ms: now_ms,
            target_ref: rec.target_ref.clone(),
            rollbackable: rec.rollbackable,
            created_hashes: created,
            metric: rec.metric.clone(),
        };
        let prev = p.audit_heads.get(&rec.hash).cloned();
        let audit = AuditRecord {
            rec_hash: rec.hash.clone(),
            from: Some(RecStatus::Pending),
            to: RecStatus::Applied,
            actor: "policy:auto".into(),
            observer_type: ObserverType::Policy,
            because: "auto-applied per host policy".into(),
            previous_audit_hash: prev,
            at_ms: now_ms,
        };
        let audit_hash = sub.put_grain(&audit.to_grain_spec(WAISER_NS))?;
        p.audit_heads.insert(rec.hash.clone(), audit_hash);
        p.status_index.insert(rec.hash.clone(), RecStatus::Applied);
        p.applied.insert(rec.hash.clone(), applied);
        Ok(())
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

    /// Per-analyzer effective settings for the Setup view: the manifest facts
    /// merged with the file-config (override or manifest default). Read-only.
    pub fn analyzer_settings<S: OmsSubstrate>(
        &self,
        sub: &S,
    ) -> Result<Vec<crate::config::AnalyzerSetting>> {
        let p = WaiserPersisted::from_value(sub.load_state()?)?;
        Ok(self
            .analyzers
            .iter()
            .map(|a| {
                let m = a.manifest();
                let cfg = p.config.get(&m.id);
                crate::config::AnalyzerSetting {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    tier: format!("{:?}", m.tier),
                    trust_class: format!("{:?}", m.trust_class).to_lowercase(),
                    default_on: m.default_on,
                    enabled: cfg.and_then(|c| c.enabled).unwrap_or(m.default_on),
                    severity_floor: cfg
                        .and_then(|c| c.severity_floor)
                        .map(|s| s.as_str().to_string()),
                }
            })
            .collect())
    }

    /// Update one analyzer's file-config (enable/disable, severity floor, param
    /// overrides, namespace scoping). Requires `Admin`. Params are validated
    /// against the analyzer's manifest first (unknown keys rejected, fail-closed),
    /// and the analyzer must exist. Returns the merged config as stored. This is
    /// the only write into `persisted.config` — the config layer, never a grain.
    pub fn set_analyzer_config<S: OmsSubstrate>(
        &self,
        sub: &mut S,
        analyzer_id: &str,
        update: crate::config::AnalyzerConfigUpdate,
        scopes: &ScopeSet,
    ) -> Result<crate::config::AnalyzerConfig> {
        if !scopes.has(Scope::Admin) {
            return Err(Error::ScopeDenied("admin".into()));
        }
        let manifest = self
            .analyzers
            .iter()
            .map(|a| a.manifest())
            .find(|m| m.id == analyzer_id)
            .ok_or_else(|| Error::NotFound(format!("unknown analyzer {analyzer_id:?}")))?;
        // Validate params against the manifest BEFORE touching state.
        if let Some(params) = &update.params {
            manifest.resolve_params(params)?;
        }
        let mut p = WaiserPersisted::from_value(sub.load_state()?)?;
        let cfg = p.config.entry(analyzer_id.to_string()).or_default();
        if let Some(enabled) = update.enabled {
            cfg.enabled = Some(enabled);
        }
        if update.clear_floor {
            cfg.severity_floor = None;
        } else if let Some(floor) = update.severity_floor {
            cfg.severity_floor = Some(floor);
        }
        if let Some(params) = update.params {
            cfg.params = params;
        }
        if let Some(ns) = update.namespaces {
            cfg.namespaces = ns;
        }
        let stored = cfg.clone();
        sub.store_state(&p.to_value()?)?;
        Ok(stored)
    }

    /// The measured outcome time series (the Verify gate's history) across all
    /// recommendations, ordered by when each checkpoint was measured.
    pub fn outcomes<S: OmsSubstrate>(&self, sub: &S) -> Result<Vec<crate::recommendation::OutcomeResult>> {
        let p = WaiserPersisted::from_value(sub.load_state()?)?;
        let mut out: Vec<_> = p.outcomes.into_values().flatten().collect();
        out.sort_by_key(|o| (o.measured_at_ms, o.horizon_ms));
        Ok(out)
    }

    /// A health snapshot — when the loop last ran, how much is un-analyzed
    /// since, and the queue counts. Lets a host surface "the loop may be stale"
    /// so a forgotten SessionEnd hook / cron doesn't silently kill it.
    pub fn health<S: OmsSubstrate>(&self, sub: &S, now_ms: i64) -> Result<Health> {
        let p = WaiserPersisted::from_value(sub.load_state()?)?;
        let (grains_since_run, error_events_since_run) = count_new(sub, p.state.watermark_ms)?;
        let recs = self.recommendations(sub, None)?;
        let mut pending = 0;
        let mut applied = 0;
        for r in &recs {
            match r.status {
                RecStatus::Pending => pending += 1,
                RecStatus::Applied => applied += 1,
                _ => {}
            }
        }
        // Stale if it has never run, or it's been a while / a lot has piled up.
        let stale = match p.state.last_run_ms {
            None => true,
            Some(last) => now_ms - last >= 7 * 86_400_000 || grains_since_run >= 100,
        };
        Ok(Health {
            last_run_ms: p.state.last_run_ms,
            grains_since_run,
            error_events_since_run,
            pending,
            applied,
            total: recs.len() as u64,
            stale,
        })
    }

    /// Approval-rate metric for `origin = llm` recommendations (reflection
    /// design §6b) — the live field-quality signal that accrues off the audit
    /// chain: what fraction of the model's *surfaced* proposals a reviewer
    /// accepts. Complements the offline Effective-Reliability eval.
    pub fn llm_metrics<S: OmsSubstrate>(&self, sub: &S) -> Result<LlmMetrics> {
        let recs = self.recommendations(sub, None)?;
        let mut m = LlmMetrics::default();
        for r in &recs {
            if !matches!(r.origin, Origin::Llm { .. }) {
                continue;
            }
            m.proposed += 1;
            match r.status {
                RecStatus::Pending => m.pending += 1,
                RecStatus::Approved | RecStatus::Applied | RecStatus::RolledBack => m.approved += 1,
                RecStatus::Rejected => m.rejected += 1,
                RecStatus::Expired => {}
            }
        }
        let decided = m.approved + m.rejected;
        m.approval_rate = (decided > 0).then(|| m.approved as f64 / decided as f64);
        Ok(m)
    }
}

/// A health snapshot for the backend's self-improvement loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_ms: Option<i64>,
    pub grains_since_run: u64,
    pub error_events_since_run: u64,
    pub pending: u64,
    pub applied: u64,
    pub total: u64,
    /// True when the loop looks stalled (never run, or ≥7d / ≥100 new grains
    /// since the last run) — a nudge that a trigger may be unwired.
    pub stale: bool,
}

/// Approval-rate metric for `origin = llm` recommendations (reflection §6b).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LlmMetrics {
    /// Total llm-origin recommendations ever stored (those that survived the
    /// verifier and reached the queue).
    pub proposed: u64,
    pub pending: u64,
    /// Approved + Applied + RolledBack (a reviewer said yes at least once).
    pub approved: u64,
    pub rejected: u64,
    /// approved / (approved + rejected); `None` until at least one is decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_rate: Option<f64>,
}

/// Re-measure applied recommendations at each **checkpoint** past due, via the
/// engine's typed reads — no CAL-scalar round-trip. A recommendation
/// accumulates one `OutcomeResult` per horizon (measured once each), forming a
/// time series, so a late regression (held at 1d, regressed at 30d) is caught.
/// Only *regressed* checkpoints feed the outcome analyzer (→ a revert).
/// Unknown metric kinds are skipped, never faked.
fn measure_outcomes<S: OmsSubstrate>(
    sub: &S,
    p: &mut WaiserPersisted,
    now_ms: i64,
) -> Result<Vec<OutcomeInput>> {
    // Collect all due (recommendation, horizon) checkpoints first.
    let mut due: Vec<(String, crate::config::AppliedRecord, i64)> = Vec::new();
    for (h, a) in &p.applied {
        if p.status_index.get(h) != Some(&RecStatus::Applied) {
            continue;
        }
        let Some(metric) = &a.metric else { continue };
        let done = p.measured.get(h).cloned().unwrap_or_default();
        for horizon in metric.horizons() {
            if now_ms - a.applied_at_ms >= horizon && !done.contains(&horizon) {
                due.push((h.clone(), a.clone(), horizon));
            }
        }
    }

    let mut out = Vec::new();
    for (rec_hash, applied, horizon) in due {
        let metric = applied.metric.as_ref().unwrap();
        let Some(current) = measure_metric(sub, metric, applied.applied_at_ms)? else {
            continue; // metric kind not yet re-measurable
        };
        let regressed = current > metric.baseline + f64::EPSILON;
        p.outcomes.entry(rec_hash.clone()).or_default().push(
            crate::recommendation::OutcomeResult {
                rec_hash: rec_hash.clone(),
                metric: metric.metric.clone(),
                baseline: metric.baseline,
                current,
                verdict: if regressed { "regressed" } else { "held" }.into(),
                horizon_ms: horizon,
                measured_at_ms: now_ms,
            },
        );
        p.measured.entry(rec_hash.clone()).or_default().push(horizon);
        if regressed {
            out.push(OutcomeInput {
                rec_hash,
                target_ref: applied.target_ref.clone(),
                metric: metric.metric.clone(),
                baseline: metric.baseline,
                current,
                unit: metric.unit.clone(),
            });
        }
    }
    Ok(out)
}

/// Typed re-measurement for the fixed set of metric kinds the engine knows.
fn measure_metric<S: SubstrateRead>(
    sub: &S,
    metric: &crate::recommendation::MetricSnapshot,
    since_ms: i64,
) -> Result<Option<f64>> {
    match metric.metric.as_str() {
        // How many times did this tool fail again after the lesson was applied?
        "tool_error_recurrence" => {
            let Some(tool) = &metric.subject else { return Ok(None) };
            let tools = sub.grains_of_type(
                crate::model::grain_type::TOOL,
                None,
                ReadOpts { live_only: true, since_ms: Some(since_ms) },
            )?;
            let n = tools
                .iter()
                .filter(|t| t.tool_name() == Some(tool.as_str()) && t.is_error())
                .count();
            Ok(Some(n as f64))
        }
        // After a resolve-to-latest, does the subject again hold more than one
        // live value under the functional relation? Live-state read (no since
        // filter): the excess beyond one distinct object is the regression.
        "contradiction_recurrence" => {
            let (Some(subject), Some(relation)) = (&metric.subject, &metric.relation) else {
                return Ok(None);
            };
            let facts = scoped_live_facts(sub, metric.namespace.as_deref(), subject)?;
            let distinct: BTreeSet<String> = facts
                .iter()
                .filter(|f| {
                    f.fact_relation()
                        .is_some_and(|r| normalize_ident(r) == *relation)
                })
                .filter_map(|f| f.fact_object().map(normalize_ident))
                .collect();
            Ok(Some(distinct.len().saturating_sub(1) as f64))
        }
        _ => Ok(None),
    }
}

/// Live facts for one normalized (namespace?, subject) — the shared scope of
/// the fact-shaped recurrence metrics. `namespace: None` spans all namespaces.
fn scoped_live_facts<S: SubstrateRead>(
    sub: &S,
    namespace: Option<&str>,
    subject: &str,
) -> Result<Vec<GrainRecord>> {
    let facts = sub.grains_of_type(
        crate::model::grain_type::FACT,
        None,
        ReadOpts { live_only: true, since_ms: None },
    )?;
    Ok(facts
        .into_iter()
        .filter(|f| namespace.is_none_or(|ns| normalize_ident(&f.namespace) == ns))
        .filter(|f| {
            f.fact_subject()
                .is_some_and(|s| normalize_ident(s) == subject)
        })
        .collect())
}

// --- free helpers ---

/// The §6.3 exact-equality check: a SUPERSEDE is *value-identical* when every
/// replacement field equals the superseded grain's value — strings after
/// case-fold/trim (upstream NFC is an OMS invariant), `namespace` against the
/// grain's own namespace, everything else exactly. This is what makes an
/// auto-applied consolidation provably information-preserving; a
/// near-duplicate (an observation body off by one token) fails it and stays
/// pending for human review. Fails closed: an unrecognized line shape, an
/// empty replacement, a missing grain, or a field the original never had all
/// disqualify.
fn supersede_is_value_identical<S: SubstrateRead>(sub: &S, line: &str) -> bool {
    let Some((target, _gtype, fields)) = crate::cal::parse_own_supersede(line) else {
        return false;
    };
    if fields.is_empty() {
        return false;
    }
    let Ok(Some(grain)) = sub.grain(&target) else {
        return false;
    };
    fields.iter().all(|(k, v)| {
        if k == "namespace" {
            return v
                .as_str()
                .is_some_and(|s| normalize_ident(s) == normalize_ident(&grain.namespace));
        }
        match (v, grain.fields.get(k)) {
            (Value::String(a), Some(Value::String(b))) => {
                normalize_ident(a) == normalize_ident(b)
            }
            (a, Some(b)) => a == b,
            (_, None) => false,
        }
    })
}

/// The confidence floor (§5.4): a verified draft below this is dropped. The
/// verifier's calibrated confidence is the gate, not the proposer's self-report.
const MIN_LLM_CONFIDENCE: f64 = 0.75;

/// The fixed DISCOVER instruction (§5.1). The scoring rule makes "nothing to
/// report" a first-class, zero-penalty answer — the structural antidote to
/// over-generation. Kept in its own request field so it never interleaves with
/// (attacker-influenced) evidence text.
const DISCOVER_INSTRUCTIONS: &str = "You review an agent's memory for quality. \
Given deterministic findings and the evidence they cite, propose ADDITIONAL \
findings the deterministic checks would miss (e.g. a semantic contradiction, a \
stale assumption, a duplicated meaning). SCORING: propose a finding ONLY if you \
are more than 0.75 confident it is BOTH correct AND materially useful. A correct, \
useful finding earns 1; a wrong or trivial one is penalized 2; returning nothing \
earns 0. When in doubt, propose nothing — an empty list is the correct answer \
when there is nothing worth flagging. The 'approved' and 'rejected' lists, when \
present, show findings this reviewer recently accepted or rejected — prefer the \
kind they accept and avoid the kind they reject. Every proposal MUST cite one \
or more evidence hashes from the bundle, target a memory entity, and include \
your confidence 0.0-1.0. Return JSON: {\"recommendations\":[{\"summary\":\"...\",\
\"target\":\"entity:<ns>/<subject>\",\"guidance\":\"...\",\"evidence\":[\"<hash>\"],\
\"confidence\":0.0}]}. Propose nothing you cannot ground in the evidence.";

/// The fixed GROUND instruction (§5.2): verify the finding's factual PREMISES
/// are real (anti-fabrication), while allowing an inference. A self-improvement
/// finding may reason BEYOND the evidence (e.g. 'HQ=SF and country=Germany are
/// inconsistent'); grounding checks the premises (HQ=SF, country=Germany) are in
/// the evidence — the soundness of the inference is VERIFY's job, not this one.
const GROUND_INSTRUCTIONS: &str = "You are a grounding checker guarding against \
fabrication. A finding may draw an INFERENCE from facts — your job is to confirm \
the facts it relies on are actually present in the cited evidence, NOT that its \
conclusion is stated verbatim. Decompose the finding into the factual claims it \
depends on. Mark supported=true when those facts are present in the evidence \
(even if the finding reasons beyond them). Mark supported=false ONLY if it relies \
on a fact that is NOT in the evidence (a fabrication) or cites evidence about a \
different subject. Return JSON: {\"results\":[{\"id\":0,\"supported\":true,\"reason\":\"...\"}]}.";

/// The fixed VERIFY instruction (§5.3): adversarial, abstention-biased.
const VERIFY_INSTRUCTIONS: &str = "You are an adversarial reviewer stress-testing \
each finding for SOUNDNESS — reject unsound findings, but only for real reasons, \
never invented ones. For each finding, ask: (1) Reality — is there a genuine, \
SPECIFIC problem, or is it vague/speculative? Reject hedged 'potential' or \
'possible' findings with no concrete defect, and reject any claimed \
inconsistency or contradiction that is not backed by at least two actually \
conflicting facts in the cited evidence. (2) Context — does the finding \
correctly read its cited evidence, or misinterpret what the grains say? Keep a \
finding when it names a genuine, specific problem grounded in its evidence and \
materially useful to a human reviewer; otherwise reject it, and default to \
keep=false when uncertain. Do NOT reject a finding for being 'already known', \
redundant, or a 'common type of error' — duplication is handled elsewhere, and a \
grounded cross-fact inconsistency with two conflicting facts is exactly what to \
KEEP. Give a calibrated confidence 0.0-1.0. Return JSON: \
{\"results\":[{\"id\":0,\"keep\":true,\"confidence\":0.0,\"reason\":\"...\"}]}.";

/// The fixed ENRICH instruction.
const ENRICH_INSTRUCTIONS: &str = "For each finding, optionally add a one-sentence \
guidance note to help a human reviewer decide. Do not restate the finding. Return \
JSON: {\"notes\":[{\"target\":\"<target_ref>\",\"guidance\":\"...\"}]}.";

/// Add a grain to the evidence bundle — deduplicated, bounded at 64, text
/// capped. Shared by the deterministic-citation and recent-grain seeding.
fn push_evidence(
    evidence: &mut Vec<crate::llm::EvidenceItem>,
    bundle: &mut BTreeSet<String>,
    g: &GrainRecord,
) {
    if evidence.len() < 64 && bundle.insert(g.hash.clone()) {
        evidence.push(crate::llm::EvidenceItem {
            hash: g.hash.clone(),
            grain_type: g.grain_type.clone(),
            text: crate::llm::cap(&grain_brief(g), 400),
        });
    }
}

/// A short human-readable projection of a grain for the evidence bundle.
fn grain_brief(g: &GrainRecord) -> String {
    if let (Some(s), Some(r), Some(o)) = (g.fact_subject(), g.fact_relation(), g.fact_object()) {
        return format!("{s} {r} {o}");
    }
    for key in ["content", "body", "text", "summary"] {
        if let Some(v) = g.fields.get(key).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

/// Stamp a validated DISCOVER draft as an `origin = llm` recommendation. LLM
/// drafts are always advisory `Flag`s carrying `Proposal::Data` (never an
/// executable CAL mutation), lower-confidence, and — via `Origin::Llm` and a
/// no-manifest analyzer id — structurally ineligible for auto-apply.
fn stamp_llm(
    model: &str,
    d: &crate::llm::LlmDraft,
    target_ref: String,
    cited: Vec<String>,
    confidence: f64,
    now_ms: i64,
) -> Recommendation {
    let summary_text = crate::llm::cap(&d.summary, crate::llm::MAX_SUMMARY_LEN);
    let mut args = serde_json::Map::new();
    args.insert("text".into(), Value::from(summary_text));
    let guidance = if d.guidance.trim().is_empty() {
        None
    } else {
        Some(crate::llm::cap(&d.guidance, crate::llm::MAX_GUIDANCE_LEN))
    };
    let action = ActionKind::Flag;
    let mut data = serde_json::Map::new();
    data.insert("source".into(), Value::from("llm"));
    Recommendation {
        hash: String::new(),
        analyzer: "waiser.llm/1".to_string(),
        params_snapshot: serde_json::Map::new(),
        origin: Origin::Llm { model: model.to_string() },
        target_ref: target_ref.clone(),
        action_kind: action,
        dedup_key: dedup_key("llm", &target_ref, action),
        summary: Summary::new("llm.discover", args),
        severity: Severity::Low,
        proposal: Proposal::Data { data },
        destructive: false,
        rollbackable: false,
        evidence: cited,
        evidence_query: None,
        metric: None,
        // The verifier's calibrated confidence — not a hardcoded default.
        confidence: confidence.clamp(0.0, 1.0),
        importance: 0.3,
        created_at_ms: now_ms,
        guidance,
        status: RecStatus::Pending,
    }
}

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
    // Provenance follows the analyzer's trust class: a subprocess
    // (`--analyzer-cmd`) finding is stamped `Command` — structurally
    // auto-apply-ineligible and badged [external] on the recall surface —
    // not `Builtin`.
    let origin = match m.trust_class {
        crate::manifest::TrustClass::Command => Origin::Command { id: m.id.clone() },
        _ => Origin::Builtin,
    };
    Ok(Recommendation {
        hash: String::new(),
        analyzer: m.id.clone(),
        params_snapshot: params.snapshot(),
        origin,
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
        guidance: None,
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
        // Pending/approved (still open) and applied (already handled)
        // recommendations suppress re-proposal of the same finding. Rejected
        // is handled by cooldowns; rolled_back/expired may legitimately
        // re-propose (the situation returned).
        if matches!(
            status,
            RecStatus::Pending | RecStatus::Approved | RecStatus::Applied
        ) {
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
