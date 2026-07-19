//! Engine end-to-end tests over the reference substrate: the full
//! propose → review → apply → rollback loop, gating, dedup idempotency, the
//! destructive gate, scopes, and the self-approval block. Compiled only under
//! `cfg(test)`.

use crate::engine::{Decision, Engine, RunOptions, RunOutcome, Scope, ScopeSet, SkipReason};
use crate::error::Error;
use crate::policy::Policy;
use crate::recommendation::{ObserverType, RecStatus};
use crate::testkit::TestSubstrate;

fn seed_all(sub: &mut TestSubstrate) {
    // exact-duplicate facts
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    // contradiction under a functional relation
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    // an expired grain
    sub.add_fact_valid_to("promo", "active", "true", 500);
    // a dominant tool-failure cluster
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    sub.add_tool_call("stripe_refund", false, "ok");
}

#[test]
fn run_proposes_across_analyzers_and_is_idempotent() {
    let mut sub = TestSubstrate::new();
    seed_all(&mut sub);
    let e = Engine::with_builtins();

    let r1 = e
        .run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    assert!(r1.ran());
    assert!(
        r1.stored >= 4,
        "expected duplicate + contradiction + staleness + tool-failure, got {}",
        r1.stored
    );

    // Same findings on a second run collapse to nothing new (dedup).
    let r2 = e
        .run(&mut sub.inner, &RunOptions::default(), 20_000)
        .unwrap();
    assert!(r2.ran());
    assert_eq!(r2.stored, 0, "no re-proposals");
}

/// A canned LLM backend keyed by op (discover / ground / verify / enrich) so a
/// test can drive the whole PROPOSE → GROUND → VERIFY → ENRICH pipeline.
struct MockLlm {
    discover: String,
    ground: String,
    verify: String,
    enrich: String,
}
impl crate::llm::LlmBackend for MockLlm {
    fn model(&self) -> &str {
        "mock-llm"
    }
    fn complete(&self, request: &str) -> crate::error::Result<String> {
        Ok(if request.contains("\"op\":\"discover\"") {
            self.discover.clone()
        } else if request.contains("\"op\":\"ground\"") {
            self.ground.clone()
        } else if request.contains("\"op\":\"verify\"") {
            self.verify.clone()
        } else {
            self.enrich.clone()
        })
    }
}

#[test]
fn llm_discover_verified_rec_is_stamped_with_confidence_and_enrich_adds_guidance() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    // A contradiction gives a deterministic finding whose cited evidence seeds
    // the DISCOVER bundle.
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");

    // One draft cites a real evidence hash (kept); one cites a bogus hash
    // (dropped as uncited before the verifier even runs).
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"prod region is ambiguous","target":"entity:test/acme","guidance":"pick one","evidence":["{h1}"],"confidence":0.9}},
          {{"summary":"uncited nonsense","target":"entity:test/acme","evidence":["deadbeef"],"confidence":0.9}}
        ]}}"#
    );
    // After validation only the cited draft remains → verifier id 0.
    let ground = r#"{"results":[{"id":0,"supported":true,"reason":"entailed"}]}"#.to_string();
    let verify =
        r#"{"results":[{"id":0,"keep":true,"confidence":0.88,"reason":"novel and real"}]}"#.to_string();
    let enrich =
        r#"{"notes":[{"target":"entity:test/acme","guidance":"resolve to latest"}]}"#.to_string();

    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    // Exactly one llm-origin rec — cited, grounded, verified.
    let llm: Vec<_> = recs
        .iter()
        .filter(|r| matches!(r.origin, Origin::Llm { .. }))
        .collect();
    assert_eq!(llm.len(), 1, "only the cited+grounded+verified draft survives");
    assert!(llm[0].summary.render().contains("ambiguous"));
    assert_eq!(llm[0].evidence, vec![h1.clone()]);
    // The verifier's calibrated confidence is stamped (not a hardcoded default).
    assert!((llm[0].confidence - 0.88).abs() < 1e-9, "conf {}", llm[0].confidence);
    assert!(!llm[0].destructive);
    assert_eq!(llm[0].status, RecStatus::Pending);

    // ENRICH added a whitelisted guidance note to the deterministic finding
    // without touching its templated summary.
    let det = recs
        .iter()
        .find(|r| r.analyzer.starts_with("waiser.contradiction"))
        .expect("a contradiction recommendation");
    assert_eq!(det.guidance.as_deref(), Some("resolve to latest"));
    assert!(det.summary.render().contains("deploy_target"));

    // §6b approval-rate metric: one surfaced llm proposal, still undecided.
    let m = e.llm_metrics(&sub.inner).unwrap();
    assert_eq!(m.proposed, 1);
    assert_eq!(m.pending, 1);
    assert_eq!(m.approval_rate, None);
}

#[test]
fn verifier_drops_ungrounded_and_low_confidence_drafts() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    // Two cited drafts pass validation → verifier ids 0 and 1.
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"grounded but the verifier is unsure","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}},
          {{"summary":"an ungrounded claim","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    // Grounding: id 0 supported, id 1 NOT — id 1 never reaches verify.
    let ground =
        r#"{"results":[{"id":0,"supported":true},{"id":1,"supported":false}]}"#.to_string();
    // Verify (only id 0): kept, but confidence 0.5 is below the 0.75 floor → dropped.
    let verify = r#"{"results":[{"id":0,"keep":true,"confidence":0.5}]}"#.to_string();
    let enrich = r#"{"notes":[]}"#.to_string();

    let e = Engine::with_builtins().with_llm(Box::new(MockLlm { discover, ground, verify, enrich }));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "ungrounded (id 1) and below-floor (id 0) drafts never reach the queue"
    );
}

#[test]
fn separate_ground_backend_is_consulted_for_grounding() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    let h1 = sub.add_fact("acme", "deploy_target", "us-east-1");
    let _h2 = sub.add_fact("acme", "deploy_target", "eu-west-1");
    let discover = format!(
        r#"{{"recommendations":[
          {{"summary":"grounded per the main model","target":"entity:test/acme","evidence":["{h1}"],"confidence":0.9}}
        ]}}"#
    );
    // The MAIN backend would ground (supported:true) and keep the draft.
    let main = MockLlm {
        discover,
        ground: r#"{"results":[{"id":0,"supported":true}]}"#.to_string(),
        verify: r#"{"results":[{"id":0,"keep":true,"confidence":0.9}]}"#.to_string(),
        enrich: r#"{"notes":[]}"#.to_string(),
    };
    // The SEPARATE ground backend REJECTS (supported:false). If it — not the main
    // backend — is the one consulted for GROUND, the draft dies before verify.
    let ground = MockLlm {
        discover: String::new(),
        ground: r#"{"results":[{"id":0,"supported":false}]}"#.to_string(),
        verify: String::new(),
        enrich: String::new(),
    };
    let e = Engine::with_builtins()
        .with_llm(Box::new(main))
        .with_ground_llm(Box::new(ground));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "the separate ground backend's rejection gates the draft"
    );
}

#[cfg(unix)]
#[test]
fn external_command_analyzer_surfaces_advisory_findings() {
    use crate::analyzer::Analyzer; // for .manifest()
    use std::os::unix::fs::PermissionsExt;
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "country", "germany");

    // A tiny analyzer: consume stdin, always emit one finding. One fixed body
    // serves both the probe (reads `id`) and analyze (reads `findings`), since
    // each reply type ignores the other's fields. Written to a space-free temp
    // path (argv is whitespace-split, like --llm-cmd).
    let script = std::env::temp_dir().join(format!("waiser_ext_{}.sh", std::process::id()));
    std::fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"id\":\"acme.ext/1\",\"title\":\"ext\",\
         \"findings\":[{\"target\":\"entity:test/acme\",\"summary\":\"external flags acme\",\
         \"severity\":\"medium\",\"evidence\":[\"deadbeef\"]}]}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let analyzer = crate::external::CommandAnalyzer::new(script.to_str().unwrap()).unwrap();
    assert_eq!(analyzer.manifest().id, "acme.ext/1");
    assert_eq!(analyzer.manifest().trust_class, crate::manifest::TrustClass::Command);
    assert_eq!(analyzer.manifest().auto_apply, crate::manifest::AutoApplyClass::Never);

    let mut e = Engine::with_builtins();
    e.register(Box::new(analyzer));
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();

    let ext: Vec<_> = recs.iter().filter(|r| r.analyzer == "acme.ext/1").collect();
    assert_eq!(ext.len(), 1, "the external finding is surfaced");
    assert_eq!(ext[0].summary.render(), "external flags acme");
    assert_eq!(ext[0].severity, crate::model::Severity::Medium);
    assert!(!ext[0].destructive, "advisory flag, not a mutation");

    std::fs::remove_file(&script).ok();
}

#[test]
fn no_llm_backend_is_the_identity() {
    use crate::model::Origin;
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "deploy_target", "us-east-1");
    sub.add_fact("acme", "deploy_target", "eu-west-1");
    let e = Engine::with_builtins(); // no LLM attached
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    let recs = e.recommendations(&sub.inner, None).unwrap();
    assert!(
        recs.iter().all(|r| !matches!(r.origin, Origin::Llm { .. })),
        "no llm-origin recs without a backend"
    );
}

#[test]
fn review_apply_rollback_on_nondestructive() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("stripe_refund", true, "rate_limited 429");
    }
    sub.add_tool_call("stripe_refund", false, "ok");
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();

    let recs = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap();
    let tf = recs
        .iter()
        .find(|r| r.analyzer.starts_with("waiser.tool_failure"))
        .expect("a tool-failure recommendation");
    let hash = tf.hash.clone();
    assert!(!tf.destructive);

    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "retries belong in the client",
        11_000,
    )
    .unwrap();
    let applied = e
        .apply(
            &mut sub.inner,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "applying the lesson",
            false,
            12_000,
        )
        .unwrap();
    assert!(applied.rollbackable);
    assert_eq!(
        applied.created_hashes.len(),
        1,
        "the ADD created one lesson grain"
    );
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::Applied);

    e.rollback(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "undo",
        13_000,
    )
    .unwrap();
    assert_eq!(status_of(&e, &sub, &hash), RecStatus::RolledBack);
}

#[test]
fn destructive_apply_requires_admin_and_flag() {
    let mut sub = TestSubstrate::new();
    sub.add_fact_valid_to("promo", "active", "true", 500);
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();

    let recs = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap();
    let st = recs
        .iter()
        .find(|r| r.analyzer.starts_with("waiser.staleness"))
        .expect("a staleness recommendation");
    let hash = st.hash.clone();
    assert!(st.destructive);

    let scopes = ScopeSet::all();
    e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "expired",
        11_000,
    )
    .unwrap();

    // Without allow_destructive → gated even with admin scope.
    let denied = e.apply(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &scopes,
        "apply",
        false,
        12_000,
    );
    assert!(matches!(denied, Err(Error::DestructiveGated(_))));

    // With allow_destructive → applies, and is non-rollbackable.
    let ok = e
        .apply(
            &mut sub.inner,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "apply",
            true,
            12_000,
        )
        .unwrap();
    assert!(!ok.rollbackable, "FORGET has no inverse");
}

#[test]
fn apply_on_pending_is_rejected() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    // pending → applied is policy-only; a human must approve first.
    let res = e.apply(
        &mut sub.inner,
        &hash,
        "user:alice",
        ObserverType::Human,
        &ScopeSet::all(),
        "x",
        false,
        11_000,
    );
    assert!(matches!(res, Err(Error::LifecycleViolation(_))));
}

#[test]
fn self_approval_blocked_against_creator() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let rec = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .clone();
    let creator = format!("engine:{}", rec.analyzer);

    let blocked = e.review(
        &mut sub.inner,
        &rec.hash,
        Decision::Approve,
        &creator,
        ObserverType::System,
        &ScopeSet::all(),
        "self",
        11_000,
    );
    assert!(matches!(blocked, Err(Error::SelfApproval(_))));

    // A different actor approves fine.
    assert!(e
        .review(
            &mut sub.inner,
            &rec.hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &ScopeSet::all(),
            "ok",
            11_000
        )
        .is_ok());
}

#[test]
fn review_requires_review_scope() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    let write_only = ScopeSet::of(&[Scope::Read, Scope::Write]);
    let res = e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:bob",
        ObserverType::Human,
        &write_only,
        "x",
        11_000,
    );
    assert!(matches!(res, Err(Error::ScopeDenied(_))), "write ⊉ review");
}

#[test]
fn empty_because_is_rejected() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 10_000)
        .unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()[0]
        .hash
        .clone();

    let res = e.review(
        &mut sub.inner,
        &hash,
        Decision::Approve,
        "user:bob",
        ObserverType::Human,
        &ScopeSet::all(),
        "   ",
        11_000,
    );
    assert!(
        matches!(res, Err(Error::InvalidProposal(_))),
        "BECAUSE is mandatory"
    );
}

#[test]
fn gating_min_new_skips_but_stale_runs_first() {
    // min_new gate on a thin file skips cleanly.
    let mut sub = TestSubstrate::new();
    sub.add_fact("a", "b", "c");
    let e = Engine::with_builtins();
    let opts = RunOptions {
        min_new: Some(100),
        ..Default::default()
    };
    let r = e.run(&mut sub.inner, &opts, 10_000).unwrap();
    assert_eq!(r.outcome, RunOutcome::Skipped);
    assert_eq!(r.skip_reason, Some(SkipReason::MinNewNotMet));

    // if_stale on a never-run file runs (last_run is None).
    let mut sub2 = TestSubstrate::new();
    sub2.add_fact("a", "b", "c");
    let stale = RunOptions {
        if_stale_ms: Some(3_600_000),
        ..Default::default()
    };
    assert!(e.run(&mut sub2.inner, &stale, 10_000).unwrap().ran());
}

#[test]
fn min_new_errors_wakes_a_run() {
    let mut sub = TestSubstrate::new();
    // Two prior runs establish a watermark; then add only error events.
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000)
        .unwrap();
    for _ in 0..4 {
        sub.add_tool_call("s", true, "boom 1");
    }
    // min_new very high (won't trip) but min_new_errors low (will).
    let opts = RunOptions {
        min_new: Some(1000),
        min_new_errors: Some(3),
        ..Default::default()
    };
    assert!(
        e.run(&mut sub.inner, &opts, 2_000).unwrap().ran(),
        "error gate wakes the run"
    );
}

#[test]
fn default_policy_auto_applies_nothing() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let e = Engine::with_builtins();
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "a closed policy applies nothing");
    assert!(e
        .recommendations(&sub.inner, None)
        .unwrap()
        .iter()
        .all(|x| x.status == RecStatus::Pending));
}

#[test]
fn policy_grant_auto_applies_structural_consolidation() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise"); // exact dup → SUPERSEDE-only proposal
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "waiser.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 1, "the consolidation is auto-applied");
    let applied = e.recommendations(&sub.inner, Some(RecStatus::Applied)).unwrap();
    assert_eq!(applied.len(), 1);
    assert!(applied[0].analyzer.starts_with("waiser.duplicate_sweep"));
}

#[test]
fn fork_merge_never_auto_applies_even_when_granted() {
    let mut sub = TestSubstrate::new();
    sub.add_fork("caller/john", &["ref-a", "ref-b"]);
    // A merge is SUPERSEDE-only (passes the shape check), but it is lossy, so
    // its manifest is Never — even an explicit policy grant cannot auto-apply it.
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "waiser.fork_surfacing", "targets": ["memory"], "max_severity": "high"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "a lossy fork merge is never auto-applied");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|x| x.analyzer.starts_with("waiser.fork_surfacing")),
        "it is proposed for human review instead"
    );
}

#[test]
fn auto_apply_never_touches_free_text_add() {
    // tool-failure proposes an ADD carrying an evidence-derived signature —
    // shape verification rejects it even when the policy names it.
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call("s", true, "boom 1");
    }
    let policy = Policy::from_json(
        r#"{"auto_apply_enabled": true,
            "auto_apply": [{"analyzer": "waiser.tool_failure", "targets": ["memory"], "max_severity": "high"}]}"#,
    )
    .unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    let r = e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert_eq!(r.auto_applied, 0, "an ADD-with-text proposal never auto-applies");
}

#[test]
fn policy_deny_disables_an_analyzer() {
    let mut sub = TestSubstrate::new();
    sub.add_fact("acme", "tier", "Enterprise");
    sub.add_fact("acme", "tier", "Enterprise");
    let policy = Policy::from_json(r#"{"deny": ["waiser.duplicate_sweep"]}"#).unwrap();
    let e = Engine::with_builtins().with_policy(policy);
    e.run(&mut sub.inner, &RunOptions::default(), 10_000).unwrap();
    assert!(
        e.recommendations(&sub.inner, None)
            .unwrap()
            .iter()
            .all(|x| !x.analyzer.starts_with("waiser.duplicate_sweep")),
        "a denied analyzer produces nothing"
    );
}

const DAY: i64 = 86_400_000;

/// Apply a tool-failure lesson; return (engine, sub, rec_hash) at apply time T.
fn apply_lesson(now: i64) -> (Engine, TestSubstrate, String) {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_tool_call_at("stripe_refund", true, "rate_limited 429", 1_000);
    }
    sub.add_tool_call_at("stripe_refund", false, "ok", 1_100);
    let e = Engine::with_builtins();
    e.run(&mut sub.inner, &RunOptions::default(), 1_000_000).unwrap();
    let hash = e
        .recommendations(&sub.inner, Some(RecStatus::Pending))
        .unwrap()
        .into_iter()
        .find(|r| r.analyzer.starts_with("waiser.tool_failure"))
        .expect("a tool-failure lesson")
        .hash;
    let scopes = ScopeSet::all();
    e.review(&mut sub.inner, &hash, Decision::Approve, "user:a", ObserverType::Human, &scopes, "codify", now).unwrap();
    e.apply(&mut sub.inner, &hash, "user:a", ObserverType::Human, &scopes, "apply the rule", false, now).unwrap();
    (e, sub, hash)
}

/// The multi-horizon Verify gate: a LATE recurrence that early checkpoints miss
/// is caught at a later one — held at 1d, held at 7d, regressed at 30d.
#[test]
fn outcome_time_series_catches_a_late_regression() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_lesson(t);

    // Measure the 1d and 7d checkpoints — no recurrence yet.
    e.run(&mut sub.inner, &RunOptions::default(), t + 2 * DAY).unwrap();
    e.run(&mut sub.inner, &RunOptions::default(), t + 8 * DAY).unwrap();

    // The failure recurs at day 20 — after the early checkpoints.
    for _ in 0..2 {
        sub.add_tool_call_at("stripe_refund", true, "rate_limited 429", t + 20 * DAY);
    }

    // Measure the 30d checkpoint.
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();

    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    let verdict_at = |h: i64| series.iter().find(|o| o.horizon_ms == h).map(|o| o.verdict.as_str());
    assert_eq!(verdict_at(DAY), Some("held"), "no recurrence at day 1");
    assert_eq!(verdict_at(7 * DAY), Some("held"), "still held at day 7");
    assert_eq!(verdict_at(30 * DAY), Some("regressed"), "the late recurrence is caught at day 30");
    assert!(
        e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("waiser.outcome_review")),
        "a revert is proposed once the regression appears"
    );
}

/// No recurrence at any checkpoint → the fix held across the whole series, no
/// revert ever proposed.
#[test]
fn outcome_time_series_holds_when_fix_works() {
    let t = 2_000_000;
    let (e, mut sub, hash) = apply_lesson(t);
    sub.add_tool_call_at("stripe_refund", false, "ok", t + 10 * DAY); // only a success
    e.run(&mut sub.inner, &RunOptions::default(), t + 31 * DAY).unwrap();

    let series: Vec<_> = e
        .outcomes(&sub.inner)
        .unwrap()
        .into_iter()
        .filter(|o| o.rec_hash == hash)
        .collect();
    assert_eq!(series.len(), 3, "all three checkpoints measured");
    assert!(series.iter().all(|o| o.verdict == "held"), "held throughout");
    assert!(
        !e.recommendations(&sub.inner, Some(RecStatus::Pending))
            .unwrap()
            .iter()
            .any(|r| r.analyzer.starts_with("waiser.outcome_review")),
        "no revert when the fix held"
    );
}

fn status_of(e: &Engine, sub: &TestSubstrate, hash: &str) -> RecStatus {
    e.recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .find(|r| r.hash == hash)
        .unwrap()
        .status
}
