//! Engine end-to-end tests over the reference substrate: the full
//! propose → review → apply → rollback loop, gating, dedup idempotency, the
//! destructive gate, scopes, and the self-approval block. Compiled only under
//! `cfg(test)`.

use crate::engine::{Decision, Engine, RunOptions, RunOutcome, Scope, ScopeSet, SkipReason};
use crate::error::Error;
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
        sub.add_event("stripe_refund", true, "rate_limited 429");
    }
    sub.add_event("stripe_refund", false, "ok");
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

#[test]
fn review_apply_rollback_on_nondestructive() {
    let mut sub = TestSubstrate::new();
    for _ in 0..5 {
        sub.add_event("stripe_refund", true, "rate_limited 429");
    }
    sub.add_event("stripe_refund", false, "ok");
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
        sub.add_event("s", true, "boom 1");
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
        sub.add_event("s", true, "boom 1");
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
        sub.add_event("s", true, "boom 1");
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
        sub.add_event("s", true, "boom 1");
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
        sub.add_event("s", true, "boom 1");
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

fn status_of(e: &Engine, sub: &TestSubstrate, hash: &str) -> RecStatus {
    e.recommendations(&sub.inner, None)
        .unwrap()
        .into_iter()
        .find(|r| r.hash == hash)
        .unwrap()
        .status
}
