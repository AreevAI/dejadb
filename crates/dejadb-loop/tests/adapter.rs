//! End-to-end tests for the DejaDB substrate adapter against a real temp
//! `.db`: grain round-trip, liveness, state persistence, and the full
//! run → review → apply loop through `deja_loop::Engine`.

use dejadb_core::types::{Fact, Grain, Tool};
use dejadb_store::DejaDB;
use dejadb_loop::DejaDbSubstrate;
use deja_loop::{
    Decision, Engine, ObserverType, OmsSubstrate, ReadOpts, RecStatus, RunOptions, ScopeSet,
    SubstrateRead,
};

const NOW: i64 = 1_700_000_000_000;

fn open_temp() -> (tempfile::TempDir, DejaDB) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let store = DejaDB::open(path.to_str().unwrap()).unwrap();
    (dir, store)
}

fn seed(store: &mut DejaDB) {
    // Exact-duplicate facts (also a fork on the same value).
    store
        .add(&Fact::new("acme", "tier", "Enterprise").namespace("caller"))
        .unwrap();
    store
        .add(&Fact::new("acme", "tier", "Enterprise").namespace("caller"))
        .unwrap();
    // Two live values under a functional relation → contradiction (+ fork).
    store
        .add(&Fact::new("acme", "deploy_target", "us-east-1").namespace("caller"))
        .unwrap();
    store
        .add(&Fact::new("acme", "deploy_target", "eu-west-1").namespace("caller"))
        .unwrap();
}

#[test]
fn state_round_trips_through_the_file() {
    let (_d, store) = open_temp();
    let mut sub = DejaDbSubstrate::new(store, None);
    assert!(
        sub.load_state().unwrap().is_null(),
        "fresh file has no state"
    );
    sub.store_state(&serde_json::json!({"watermark_ms": 42}))
        .unwrap();
    assert_eq!(
        sub.load_state().unwrap(),
        serde_json::json!({"watermark_ms": 42})
    );
    // Overwrite (supersede the state chain).
    sub.store_state(&serde_json::json!({"watermark_ms": 99}))
        .unwrap();
    assert_eq!(sub.load_state().unwrap()["watermark_ms"], 99);
}

#[test]
fn user_facts_are_readable_and_live_filtered() {
    let (_d, mut store) = open_temp();
    let h1 = store
        .add(&Fact::new("u", "name", "Ann").namespace("caller"))
        .unwrap();
    // Supersede it; the old grain must drop out of a live read.
    let mut newer = Fact::new("u", "name", "Anne").namespace("caller");
    store.supersede(&h1, &mut newer).unwrap();
    let sub = DejaDbSubstrate::new(store, None);

    let live = sub
        .grains_of_type("fact", None, ReadOpts::default())
        .unwrap();
    let objs: Vec<_> = live
        .iter()
        .filter_map(|g| g.fact_object().map(str::to_string))
        .collect();
    assert!(objs.contains(&"Anne".to_string()), "live head present");
    assert!(
        !objs.contains(&"Ann".to_string()),
        "superseded grain filtered out via derived_from"
    );
}

#[test]
fn end_to_end_run_review_apply() {
    let (_d, mut store) = open_temp();
    seed(&mut store);
    let mut sub = DejaDbSubstrate::new(store, None);
    let engine = Engine::with_builtins();

    let r = engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();
    assert!(r.ran());
    assert!(
        r.stored >= 2,
        "expected duplicate + contradiction (+fork), got {}",
        r.stored
    );

    let pending = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap();
    assert_eq!(pending.len() as u64, r.stored, "listed == stored");

    // Second run is idempotent (dedup suppresses re-proposals).
    let r2 = engine
        .run(&mut sub, &RunOptions::default(), NOW + 1000)
        .unwrap();
    assert_eq!(r2.stored, 0, "no re-proposals on the second run");

    // Approve + apply a non-destructive recommendation through the real store.
    let target = pending
        .iter()
        .find(|x| !x.destructive)
        .expect("a non-destructive rec");
    let hash = target.hash.clone();
    let scopes = ScopeSet::all();
    engine
        .review(
            &mut sub,
            &hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "confirmed",
            NOW + 2000,
        )
        .unwrap();
    engine
        .apply(
            &mut sub,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "applying",
            false,
            NOW + 3000,
        )
        .unwrap();

    let after = engine.recommendations(&sub, None).unwrap();
    let applied = after.iter().find(|x| x.hash == hash).unwrap();
    assert_eq!(
        applied.status,
        RecStatus::Applied,
        "status persisted as applied"
    );
}

#[test]
fn tool_failure_clusters_from_tool_grains_and_applies() {
    let (_d, mut store) = open_temp();
    // Record tool calls as real Tool grains (what record_tool_call / the
    // tool-log importer produce). Content compacts to `cnt` → `tool_content`.
    //
    // The four failures carry distinct payloads on purpose. Grains are
    // content-addressed over the whole blob including `created_at`, so
    // byte-identical calls recorded inside the same millisecond hash to the
    // same address and the second one fails the UNIQUE constraint. This loop
    // used to be four identical grains and passed only because writes were
    // slow enough to straddle a millisecond — once the BM25 leg stopped
    // costing ~1.6ms per write, they started landing together. Same reasoning
    // and same fix as `crates/dejadb-js/__test__/smoke.mjs`.
    for attempt in 0..4 {
        store
            .add(
                &Tool::new("stripe_refund")
                    .content(&format!("rate_limited 429 (attempt {attempt})"))
                    .is_error(true)
                    .namespace("caller"),
            )
            .unwrap();
    }
    store
        .add(
            &Tool::new("stripe_refund")
                .content("ok")
                .is_error(false)
                .namespace("caller"),
        )
        .unwrap();
    let mut sub = DejaDbSubstrate::new(store, None);
    let engine = Engine::with_builtins();
    engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();

    let recs = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap();
    let tf = recs
        .iter()
        .find(|r| r.analyzer.starts_with("loop.tool_failure"))
        .expect("a tool-failure recommendation");
    // The error signature (hence the lesson's object) must be non-empty.
    assert!(
        tf.summary.render().contains("rate_limited"),
        "signature must be present, got: {}",
        tf.summary.render()
    );

    // Apply must succeed — an empty object would trip VAL-E001.
    let hash = tf.hash.clone();
    let scopes = ScopeSet::all();
    engine
        .review(
            &mut sub,
            &hash,
            Decision::Approve,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "ok",
            NOW + 1,
        )
        .unwrap();
    engine
        .apply(
            &mut sub,
            &hash,
            "user:alice",
            ObserverType::Human,
            &scopes,
            "apply",
            false,
            NOW + 2,
        )
        .unwrap();
    let after = engine.recommendations(&sub, None).unwrap();
    assert_eq!(
        after.iter().find(|r| r.hash == hash).unwrap().status,
        RecStatus::Applied
    );
}

#[test]
fn self_approval_block_holds_on_the_real_store() {
    let (_d, mut store) = open_temp();
    seed(&mut store);
    let mut sub = DejaDbSubstrate::new(store, None);
    let engine = Engine::with_builtins();
    engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();
    let rec = engine
        .recommendations(&sub, Some(RecStatus::Pending))
        .unwrap()[0]
        .clone();
    let creator = format!("engine:{}", rec.analyzer);

    let blocked = engine.review(
        &mut sub,
        &rec.hash,
        Decision::Approve,
        &creator,
        ObserverType::System,
        &ScopeSet::all(),
        "self",
        NOW + 100,
    );
    assert!(matches!(blocked, Err(deja_loop::Error::SelfApproval(_))));
}

/// The review queue must come back worst-first and in the *same* order every
/// run. It used to sort on the content hash alone, which is deterministic
/// within a run but reshuffles across runs, because a grain's hash covers its
/// creation timestamp — so an identical queue was presented to the reviewer in
/// a different order each time, and severity had no bearing on it at all.
#[test]
fn review_queue_is_severity_ordered_and_stable_across_runs() {
    let mut orders = Vec::new();
    for _ in 0..3 {
        let (_d, mut store) = open_temp();
        seed(&mut store);
        // A tool failure lands a higher severity alongside the seeded findings.
        for i in 0..4 {
            store
                .add(
                    &Tool::new("crm_export")
                        .content(&format!("timeout after 30s (attempt {i})"))
                        .is_error(true)
                        .namespace("caller"),
                )
                .unwrap();
        }
        let mut sub = DejaDbSubstrate::new(store, None);
        let engine = Engine::with_builtins();
        engine.run(&mut sub, &RunOptions::default(), NOW).unwrap();

        let pending = engine
            .recommendations(&sub, Some(RecStatus::Pending))
            .unwrap();
        assert!(pending.len() >= 2, "need several to have an order at all");

        let severities: Vec<_> = pending.iter().map(|r| r.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(
            severities, sorted,
            "queue must be highest-severity-first, got {severities:?}"
        );

        orders.push(
            pending
                .iter()
                .map(|r| (r.severity, r.analyzer.clone(), r.summary.render()))
                .collect::<Vec<_>>(),
        );
    }

    assert_eq!(orders[0], orders[1], "order drifted between run 1 and 2");
    assert_eq!(orders[1], orders[2], "order drifted between run 2 and 3");
}
