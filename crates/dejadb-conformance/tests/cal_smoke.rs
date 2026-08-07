//! CAL end-to-end over each backend: text → executor → DejaDbFacade →
//! store. The facade and executor are backend-blind by design (the Db seam
//! sits below `DejaDB`), so one smoke per backend pins the whole stack —
//! including the destructive-op gate, which must behave identically
//! regardless of where the bytes live.

use dejadb_cal::executor::CalResultPayload;
use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_conformance::{Backend, TursoBackend};

fn drive(facade: &DejaDbFacade) {
    let ex = CalExecutor::new(CalExecutorConfig::default());
    let add = ex
        .execute(
            r#"ADD fact SET subject = "john" SET relation = "likes" SET object = "rust" SET namespace = "caller" REASON "smoke""#,
            facade,
        )
        .unwrap();
    let hash = match &add.result {
        CalResultPayload::Added { hash, .. } => hash.clone(),
        other => panic!("expected Added, got {other:?}"),
    };
    assert_eq!(hash.len(), 64);

    let recall = ex.execute(r#"RECALL facts WHERE subject = "john""#, facade).unwrap();
    match recall.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1);
            let g = serde_json::to_value(&grains[0]).unwrap();
            assert_eq!(g["fields"]["object"], "rust");
        }
        other => panic!("expected Grains, got {other:?}"),
    }

    // The destructive gate is identical on every backend: FORGET works when
    // allowed, and a no-destructive executor refuses it.
    let gated = CalExecutor::new(CalExecutorConfig {
        allow_destructive_ops: false,
        ..CalExecutorConfig::default()
    });
    let refused = gated.execute(&format!("FORGET sha256:{hash}"), facade).unwrap();
    assert!(
        matches!(refused.result, CalResultPayload::Unsupported { .. }),
        "FORGET must come back Unsupported with allow_destructive_ops=false, got {:?}",
        refused.result
    );
    let forgotten = ex.execute(&format!("FORGET sha256:{hash}"), facade).unwrap();
    assert!(
        matches!(forgotten.result, CalResultPayload::Forgotten { .. }),
        "expected Forgotten, got {:?}",
        forgotten.result
    );
    let after = ex.execute(r#"RECALL facts WHERE subject = "john""#, facade).unwrap();
    match after.result {
        CalResultPayload::Grains { grains, .. } => assert!(grains.is_empty()),
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn cal_over_turso() {
    let b = TursoBackend::new();
    let facade = DejaDbFacade::with_session(b.open(), Some("caller".to_string()), None);
    drive(&facade);
}

#[cfg(feature = "postgres")]
#[test]
fn cal_over_postgres() {
    let url = match std::env::var("DEJADB_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
    {
        Some(u) => u,
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!("CI=true but no DATABASE_URL — the postgres job must not silently skip");
            }
            eprintln!("skipping: no DATABASE_URL/DEJADB_PG_URL");
            return;
        }
    };
    let b = dejadb_conformance::PgBackend::new(&url);
    let facade = DejaDbFacade::with_session(b.open(), Some("caller".to_string()), None);
    drive(&facade);
}
