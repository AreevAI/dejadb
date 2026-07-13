//! M2 exit test: CAL text → executor → DejaDbFacade → embedded Turso store.
//!
//! Covers the read tier (RECALL, EXISTS, HISTORY, pipeline COUNT) and the
//! ADD tier (ADD, SUPERSEDE) end-to-end against a real memory file.

use dejadb_cal::executor::CalResultPayload;
use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_store::DejaDB;
use tempfile::TempDir;

fn setup() -> (CalExecutor, DejaDbFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = DejaDbFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn added_hash(payload: &CalResultPayload) -> String {
    match payload {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        CalResultPayload::Superseded { new_hash, .. } => new_hash.clone(),
        other => panic!("expected Added/Superseded payload, got: {other:?}"),
    }
}

#[test]
fn cal_add_then_recall() {
    let (ex, facade, _d) = setup();

    let add = ex
        .execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "integration""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);
    assert_eq!(hash.len(), 64);

    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "john""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "rust");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_recall_recent_experience_without_subject() {
    // The "reflect over recent experience" path: RECALL by grain type with no
    // subject/free-text anchor now does a bounded recent-scan (it used to
    // error). Observations carry observer_id, filtered as a post-condition.
    let (ex, facade, _d) = setup();
    for (obs_id, body) in [
        ("executor", "attempt 1 failed"),
        ("executor", "attempt 2 passed"),
        ("planner", "unrelated note"),
    ] {
        ex.execute(
            &format!(
                r#"ADD observation SET observer_id = "{obs_id}" SET observer_type = "llm" SET content = "{body}" SET namespace = "caller" REASON "log""#
            ),
            &facade,
        )
        .unwrap();
    }

    // Bare recent-by-type scan returns all three, newest first.
    let all = ex.execute("RECALL observations RECENT 10", &facade).unwrap();
    match all.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 3),
        other => panic!("expected Grains, got {other:?}"),
    }

    // observer_id post-filter narrows to the two executor observations.
    let filtered = ex
        .execute(
            r#"RECALL observations WHERE observer_id = "executor" RECENT 10"#,
            &facade,
        )
        .unwrap();
    match filtered.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 2),
        other => panic!("expected Grains, got {other:?}"),
    }

    // A wildcard recall with no anchor at all is still rejected as too broad.
    assert!(ex.execute("RECALL * RECENT 10", &facade).is_err());
}

#[test]
fn cal_exists_after_add() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "a" SET relation = "r" SET object = "o" SET namespace = "caller" REASON "t""#,
            &facade,
        )
        .unwrap();
    let hash = added_hash(&add.result);

    let q = format!("EXISTS sha256:{hash}");
    let res = ex.execute(&q, &facade).unwrap();
    match res.result {
        CalResultPayload::Exists { exists, .. } => assert!(exists),
        other => panic!("expected Exists, got {other:?}"),
    }
}

#[test]
fn cal_supersede_and_history() {
    let (ex, facade, _d) = setup();
    let add = ex
        .execute(
            r#"ADD fact SET subject = "acct" SET relation = "balance" SET object = "100" SET namespace = "caller" REASON "init""#,
            &facade,
        )
        .unwrap();
    let h1 = added_hash(&add.result);

    let sup = ex
        .execute(
            &format!(r#"SUPERSEDE sha256:{h1} SET object = "80" REASON "withdrawal""#),
            &facade,
        )
        .unwrap();
    let h2 = added_hash(&sup.result);
    assert_ne!(h1, h2);

    // Current recall sees only the new version.
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "acct""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "80");
        }
        other => panic!("expected Grains, got {other:?}"),
    }

    // HISTORY walks the chain, newest first.
    let hist = ex
        .execute(
            r#"HISTORY WHERE subject = "acct" AND relation = "balance""#,
            &facade,
        )
        .unwrap();
    match hist.result {
        CalResultPayload::History { versions } => {
            assert_eq!(versions.len(), 2);
            let v = serde_json::to_value(&versions).unwrap();
            assert_eq!(v[0]["object"], "80");
            assert_eq!(v[1]["object"], "100");
        }
        other => panic!("expected History, got {other:?}"),
    }
}

#[test]
fn cal_pipeline_count() {
    let (ex, facade, _d) = setup();
    for i in 0..3 {
        ex.execute(
            &format!(
                r#"ADD fact SET subject = "kid" SET relation = "likes" SET object = "toy{i}" SET namespace = "caller" REASON "t""#
            ),
            &facade,
        )
        .unwrap();
    }
    let res = ex
        .execute(r#"RECALL facts WHERE subject = "kid" | COUNT"#, &facade)
        .unwrap();
    match res.result {
        CalResultPayload::Count { count } => assert_eq!(count, 3),
        other => panic!("expected Count, got {other:?}"),
    }
}

#[test]
fn destructive_tokens_are_parse_errors() {
    let (ex, facade, _d) = setup();
    for q in ["DELETE sha256:abc", r#"DROP TABLE grains"#] {
        assert!(ex.execute(q, &facade).is_err(), "{q} must not execute");
    }
}

#[test]
fn cal_add_inherits_session_namespace() {
    // ADD without `SET namespace` must land in the session namespace so the
    // same session's RECALL can see it (RECALL already scoped to the session).
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" REASON "session ns""#,
        &facade,
    )
    .unwrap();
    let recall = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "session RECALL must see the session ADD");
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["namespace"], "caller");
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_add_explicit_namespace_still_wins() {
    let (ex, facade, _d) = setup();
    ex.execute(
        r#"ADD fact SET subject = "zoe" SET relation = "team" SET object = "core" SET namespace = "other" REASON "explicit ns""#,
        &facade,
    )
    .unwrap();
    // Not visible in the session namespace…
    let in_session = ex
        .execute(r#"RECALL facts WHERE subject = "zoe""#, &facade)
        .unwrap();
    match in_session.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
    // …but present where the user explicitly put it.
    let in_other = ex
        .execute(
            r#"RECALL facts WHERE namespace = "other" AND subject = "zoe""#,
            &facade,
        )
        .unwrap();
    match in_other.result {
        CalResultPayload::Grains { grains, .. } => assert_eq!(grains.len(), 1),
        other => panic!("expected Grains, got {other:?}"),
    }
}
