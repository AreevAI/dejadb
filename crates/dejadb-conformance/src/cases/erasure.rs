//! Bulk erasure: right-to-erasure for one identity and age-based retention.
//! Deliberate host-level extensions beyond the OMS single-grain tombstone
//! (docs/erasure.md) — their semantics must hold identically on every
//! backend, INCLUDING replication through ordinary tombstones.

use crate::{fact, fact_at, Backend};
use dejadb_store::Capture;

pub fn subject_erasure_is_complete(b: &dyn Backend) {
    let mut m = b.open();
    // A history chain ABOUT the identity (head + superseded — both must go).
    let v1 = m.add(&fact("ns", "pat", "condition", "wellumbrix onset")).unwrap();
    let mut v2f = fact("ns", "pat", "condition", "wellumbrix resolved");
    let v2 = m.supersede(&v1, &mut v2f).unwrap();
    // A grain REFERENCING the identity in object position.
    let href = m.add(&fact("ns", "dr_lee", "treats", "pat")).unwrap();
    // A thread event in the identity's session.
    let hev = m
        .capture(
            "ns",
            "call notes mentioning zanthrofel",
            &Capture { session_id: Some("pat"), ..Capture::default() },
        )
        .unwrap();
    // An unrelated grain that must SURVIVE.
    m.add(&fact("ns", "mara", "prefers", "tea")).unwrap();
    // The erasable text is findable before…
    assert!(!m.recall_hybrid("ns", None, None, Some("zanthrofel"), 8, None).unwrap().is_empty());

    let rep = m.forget_subject("ns", "pat").unwrap();
    assert_eq!(rep.grains_erased, 4, "chain (2) + reference (1) + session event (1)");
    assert!(rep.vocab_removed >= 1, "erased-only vocabulary must go: {rep:?}");
    assert_eq!(rep.terms_removed, 1, "the identity's dictionary entry must go");

    // Every structured trace is gone — history included.
    for h in [v1, v2, href, hev] {
        assert!(m.get(&h).is_err(), "erased grain still readable");
    }
    assert!(m.recall("ns", "pat", None, 8).unwrap().is_empty());
    assert!(m.recall("ns", "dr_lee", None, 8).unwrap().is_empty(), "referencing grain erased");
    assert!(m.latest("ns", "dr_lee", "treats").unwrap().is_none());
    assert!(m.thread_tail("ns", "pat", 8).unwrap().is_empty());
    assert!(m.heads("ns", "pat", "condition").unwrap().is_empty());
    // …and unfindable by text afterwards (postings + vocabulary gone).
    assert!(m.recall_hybrid("ns", None, None, Some("zanthrofel"), 8, None).unwrap().is_empty());
    assert!(m.recall_hybrid("ns", None, None, Some("wellumbrix"), 8, None).unwrap().is_empty());
    // The unrelated grain survives untouched.
    assert_eq!(m.recall("ns", "mara", Some("prefers"), 8).unwrap().len(), 1);
    assert_eq!(m.count().unwrap(), 1);
    // Erasing an unknown identity is a clean empty report, never an error.
    let rep2 = m.forget_subject("ns", "nobody").unwrap();
    assert_eq!(rep2.grains_erased, 0);
    // The store keeps working for the same identity afterwards (fresh intern).
    m.add(&fact("ns", "pat", "condition", "new record")).unwrap();
    assert_eq!(m.recall("ns", "pat", None, 8).unwrap().len(), 1);
}

pub fn subject_erasure_replicates(b: &dyn Backend) {
    let b1 = b.scratch().join("erase_base.mgb");
    let b2 = b.scratch().join("erase_delta.mgb");
    let mut a = b.open_named("erase_a");
    let v1 = a.add(&fact("ns", "pat", "plan", "basic")).unwrap();
    let mut v2 = fact("ns", "pat", "plan", "pro");
    a.supersede(&v1, &mut v2).unwrap();
    let keep = a.add(&fact("ns", "mara", "plan", "max")).unwrap();
    let st = a.bundle_since(0, b1.to_str().unwrap()).unwrap();
    let mut peer = b.open_named("erase_b");
    peer.import_bundle(b1.to_str().unwrap()).unwrap();
    assert_eq!(peer.count().unwrap(), 3);

    let rep = a.forget_subject("ns", "pat").unwrap();
    assert_eq!(rep.grains_erased, 2);
    a.bundle_since(st.last_op_seq, b2.to_str().unwrap()).unwrap();
    peer.import_bundle(b2.to_str().unwrap()).unwrap();
    assert!(peer.recall("ns", "pat", None, 8).unwrap().is_empty(), "erasure must replicate");
    assert!(peer.get(&keep).is_ok(), "unrelated grain survives on the peer");
    assert_eq!(peer.count().unwrap(), 1);
    // Tombstone replay stays idempotent.
    let again = peer.import_bundle(b2.to_str().unwrap()).unwrap();
    assert_eq!(again.applied, 0);
}

pub fn retention_erases_only_older(b: &dyn Backend) {
    let mut m = b.open();
    m.add(&fact_at("ns", "old1", "note", "ancient one", 1_700_000_000_000)).unwrap();
    m.add(&fact_at("ns", "old2", "note", "ancient two", 1_700_000_100_000)).unwrap();
    let keep = m.add(&fact_at("ns", "new1", "note", "recent", 1_800_000_000_000)).unwrap();
    let rep = m.forget_older_than(Some("ns"), 1_750_000_000_000, None).unwrap();
    assert_eq!(rep.grains_erased, 2, "only grains older than the cutoff");
    assert!(m.recall("ns", "old1", None, 8).unwrap().is_empty());
    assert!(m.recall("ns", "old2", None, 8).unwrap().is_empty());
    assert!(m.get(&keep).is_ok());
    assert_eq!(m.count().unwrap(), 1);
    // The cutoff is exclusive on the grain's own created_at.
    let rep2 = m.forget_older_than(Some("ns"), 1_800_000_000_000, None).unwrap();
    assert_eq!(rep2.grains_erased, 0, "created_at == cutoff is NOT older");
    let rep3 = m.forget_older_than(Some("ns"), 1_800_000_000_001, None).unwrap();
    assert_eq!(rep3.grains_erased, 1);
    assert_eq!(m.count().unwrap(), 0);
}
