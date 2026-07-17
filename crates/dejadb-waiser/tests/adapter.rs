//! End-to-end tests for the DejaDB substrate adapter against a real temp
//! `.db`: grain round-trip, liveness, state persistence, and the full
//! run → review → apply loop through `waiser::Engine`.

use dejadb_core::types::{Fact, Grain};
use dejadb_store::DejaDB;
use dejadb_waiser::DejaDbSubstrate;
use waiser::{
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
    assert!(matches!(blocked, Err(waiser::Error::SelfApproval(_))));
}
