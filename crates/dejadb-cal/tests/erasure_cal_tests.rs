//! CAL 1.3 §8.14 Tier-2 destruction, end to end: `FORGET SUBJECT` and
//! `PURGE OLDER THAN` through text → executor → facade → store, with the
//! mandatory-BECAUSE rule, the erase-verb gate, and the audit Observation.
//!
//! Per the testing rules: every filter asserts what it must NOT touch — an
//! erasure that over-reaches is worse than one that fails.

use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_core::authz::{AUTHZ_NS, REL_PERMITS};
use dejadb_core::types::{Event, Fact, Grain};
use dejadb_store::DejaDB;
use tempfile::TempDir;

fn setup() -> (CalExecutor, DejaDbFacade, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    let facade = DejaDbFacade::with_session(m, Some("caller".to_string()), None);
    (CalExecutor::new(CalExecutorConfig::default()), facade, dir)
}

fn payload(ex: &CalExecutor, f: &DejaDbFacade, q: &str) -> serde_json::Value {
    serde_json::to_value(ex.execute(q, f).unwrap().payload_json().unwrap()).unwrap()
}

fn grain_count(ex: &CalExecutor, f: &DejaDbFacade, q: &str) -> usize {
    payload(ex, f, q)["grains"].as_array().map(|a| a.len()).unwrap_or(0)
}

#[test]
fn forget_subject_erases_the_identity_and_only_the_identity() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
        m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(2_000)).unwrap();
        m.add(&Fact::new("alice", "prefers", "coffee").namespace("caller").created_at(3_000)).unwrap();
    });

    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "gdpr erasure request""#);
    assert_eq!(v["type"], "forgotten", "{v}");
    assert_eq!(v["target"], "subject:pat");
    assert!(v["count"].as_u64().unwrap() >= 2, "{v}");

    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 0);
    // The neighbor must survive — erasure takes an identity, not a namespace.
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "alice""#), 1);

    // The audit Observation is a grain: principal, verb, target, reason.
    let audit = payload(
        &ex,
        &f,
        r#"RECALL observations WHERE namespace = "agent:authz""#,
    );
    let grains = audit["grains"].as_array().unwrap();
    assert_eq!(grains.len(), 1, "{audit}");
    let fields = &grains[0]["fields"];
    assert_eq!(fields["subject"], "subject:pat ns:caller");
    assert_eq!(fields["object"], "erase");
    let ctx = &fields["context"];
    assert_eq!(ctx["because"], "gdpr erasure request");
    assert_eq!(ctx["audit"], "tier2");
}

#[test]
fn because_is_mandatory_on_subject_and_purge_but_not_on_hash() {
    let (ex, f, _dir) = setup();
    let err = ex
        .execute(r#"FORGET SUBJECT "pat""#, &f)
        .expect_err("subject erasure without BECAUSE must not parse");
    assert!(err.to_string().contains("CAL-E018"), "{err}");

    let err = ex
        .execute(r#"PURGE OLDER THAN 30d"#, &f)
        .expect_err("purge without BECAUSE must not parse");
    assert!(err.to_string().contains("CAL-E018"), "{err}");

    // The hash form predates the rule: BECAUSE stays optional there.
    let h = f.with_store(|m| {
        m.add(&Fact::new("x", "r", "o").namespace("caller").created_at(1_000)).unwrap()
    });
    let v = payload(&ex, &f, &format!("FORGET sha256:{}", h.to_hex()));
    assert_eq!(v["type"], "forgotten", "{v}");
}

#[test]
fn forget_hash_records_its_reason_in_the_audit() {
    let (ex, f, _dir) = setup();
    let h = f.with_store(|m| {
        m.add(&Fact::new("x", "r", "o").namespace("caller").created_at(1_000)).unwrap()
    });
    let v = payload(
        &ex,
        &f,
        &format!(r#"FORGET sha256:{} BECAUSE "test cleanup""#, h.to_hex()),
    );
    assert_eq!(v["type"], "forgotten", "{v}");
    let audit = payload(
        &ex,
        &f,
        r#"RECALL observations WHERE namespace = "agent:authz""#,
    );
    let fields = &audit["grains"][0]["fields"];
    assert_eq!(fields["object"], "delete");
    assert_eq!(fields["context"]["because"], "test cleanup");
}

#[test]
fn user_and_scope_spellings_are_refused_with_a_pointer_to_subject() {
    let (ex, f, _dir) = setup();
    let err = ex
        .execute(r#"FORGET USER "pat" BECAUSE "x""#, &f)
        .expect_err("FORGET USER must not parse");
    assert!(err.to_string().contains("SUBJECT"), "{err}");
    assert!(ex.execute(r#"FORGET SCOPE "s" BECAUSE "x""#, &f).is_err());
}

#[test]
fn purge_sweeps_by_age_and_type_and_spares_everything_else() {
    let (ex, f, _dir) = setup();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let old = now - 40 * 86_400_000;
    f.with_store(|m| {
        let mut e_old = Event::new("session ended abruptly");
        e_old.common.namespace = Some("caller".into());
        e_old.common.created_at = Some(old);
        m.add(&e_old).unwrap();
        let mut e_new = Event::new("session completed");
        e_new.common.namespace = Some("caller".into());
        e_new.common.created_at = Some(now - 1_000);
        m.add(&e_new).unwrap();
        // Same age as the old event, wrong type — the TYPE filter must
        // spare it.
        m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(old)).unwrap();
        // Same age and type, different namespace — the IN scope must spare it.
        let mut e_other = Event::new("other-ns event");
        e_other.common.namespace = Some("shared".into());
        e_other.common.created_at = Some(old);
        m.add(&e_other).unwrap();
    });

    let v = payload(
        &ex,
        &f,
        r#"PURGE OLDER THAN 30d TYPE event IN "caller" BECAUSE "retention policy""#,
    );
    assert_eq!(v["type"], "purged", "{v}");
    assert_eq!(v["count"], 1, "{v}");

    assert_eq!(grain_count(&ex, &f, r#"RECALL events WHERE namespace = "caller" RECENT 10"#), 1);
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 1);
    assert_eq!(grain_count(&ex, &f, r#"RECALL events WHERE namespace = "shared" RECENT 10"#), 1);
}

#[test]
fn erasure_requires_the_erase_verb() {
    let dir = TempDir::new().unwrap();
    let mut m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    // A principal with everything except erase, and one with erase.
    m.add(
        &Fact::new("agent:worker", REL_PERMITS, "read,write,delete ON caller")
            .namespace(AUTHZ_NS)
            .created_at(1_000),
    )
    .unwrap();
    m.add(
        &Fact::new("job:retention", REL_PERMITS, "read,erase ON caller")
            .namespace(AUTHZ_NS)
            .created_at(2_000),
    )
    .unwrap();
    m.add(&Fact::new("pat", "tier", "gold").namespace("caller").created_at(3_000)).unwrap();

    let ex = CalExecutor::new(CalExecutorConfig::default());
    let f = DejaDbFacade::with_session(m, Some("caller".to_string()), None)
        .with_principal("agent:worker")
        .unwrap();
    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "not allowed""#);
    assert_eq!(v["type"], "unsupported", "{v}");
    assert!(v["message"].as_str().unwrap().contains("AUT-E001"), "{v}");
    // Nothing was erased.
    assert_eq!(grain_count(&ex, &f, r#"RECALL facts WHERE subject = "pat""#), 1);

    let f = DejaDbFacade::with_session(f.into_inner(), Some("caller".to_string()), None)
        .with_principal("job:retention")
        .unwrap();
    let v = payload(&ex, &f, r#"FORGET SUBJECT "pat" BECAUSE "authorized erasure""#);
    assert_eq!(v["type"], "forgotten", "{v}");
}

#[test]
fn text_mentions_extends_the_erasure() {
    let (ex, f, _dir) = setup();
    f.with_store(|m| {
        m.add(&Fact::new("pat", "prefers", "tea").namespace("caller").created_at(1_000)).unwrap();
        // A grain about someone else whose text mentions the identity.
        let mut e = Event::new("call with pat about the refund");
        e.common.namespace = Some("caller".into());
        e.common.created_at = Some(2_000);
        m.add(&e).unwrap();
    });

    let v = payload(
        &ex,
        &f,
        r#"FORGET SUBJECT "pat" WITH text_mentions BECAUSE "gdpr, thorough""#,
    );
    assert_eq!(v["type"], "forgotten", "{v}");
    assert!(v["count"].as_u64().unwrap() >= 2, "text mention must be swept: {v}");
    assert_eq!(grain_count(&ex, &f, r#"RECALL events RECENT 10"#), 0);
}
