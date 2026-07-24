//! Regression tests for bugs found in the 2026-07-22 combination-hunt.
//! Each test pins the CORRECT behavior of a fix; the file name groups them so
//! the provenance is obvious. See also the dejadb-testing skill's
//! combination-coverage checklist.

use dejadb_core::error::Hash;
use dejadb_core::types::{Fact, Grain};
use dejadb_store::{DejaDB, EmbedBackend, OP_SUPERSEDE};
use tempfile::TempDir;

fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

fn open_mem() -> (DejaDB, TempDir) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    (DejaDB::open(path.to_str().unwrap()).unwrap(), dir)
}

/// A 2-tip fork on (ns, john, plan): tip A object="pro", tip B="enterprise".
fn build_two_tip_fork() -> (DejaDB, TempDir, Hash, Hash) {
    let d = TempDir::new().unwrap();
    let (pa, pb) = (d.path().join("a.db"), d.path().join("b.db"));
    let (bv1, bb) = (d.path().join("v1.mgb"), d.path().join("b.mgb"));
    let mut a = DejaDB::open(pa.to_str().unwrap()).unwrap();
    let v1 = a.add(&fact("ns", "john", "plan", "basic")).unwrap();
    let st = a.bundle_since(0, bv1.to_str().unwrap()).unwrap();
    let mut b = DejaDB::open(pb.to_str().unwrap()).unwrap();
    b.import_bundle(bv1.to_str().unwrap()).unwrap();
    let mut v2a = fact("ns", "john", "plan", "pro");
    v2a.common.created_at = Some(1_800_000_000_000);
    let h2a = a.supersede(&v1, &mut v2a).unwrap();
    let mut v2b = fact("ns", "john", "plan", "enterprise");
    v2b.common.created_at = Some(1_800_000_000_500);
    let h2b = b.supersede(&v1, &mut v2b).unwrap();
    b.bundle_since(st.last_op_seq, bb.to_str().unwrap()).unwrap();
    a.import_bundle(bb.to_str().unwrap()).unwrap();
    (a, d, h2a, h2b)
}

// #4 — forget() reconciles the heads table (no dangling head).
#[test]
fn forget_clears_head_row() {
    let (mut m, _d) = open_mem();
    let h = m.add(&fact("caller", "alice", "prefers", "tea")).unwrap();
    assert_eq!(m.heads("caller", "alice", "prefers").unwrap().len(), 1);
    m.forget(&h).unwrap();
    assert!(m.heads("caller", "alice", "prefers").unwrap().is_empty(), "forgotten head must not linger");
    assert!(m.open_forks().unwrap().is_empty());
}

#[test]
fn supersede_then_forget_new_head_clears_head() {
    let (mut m, _d) = open_mem();
    let h1 = m.add(&fact("ns", "x", "state", "v1")).unwrap();
    let mut v2 = fact("ns", "x", "state", "v2");
    let h2 = m.supersede(&h1, &mut v2).unwrap();
    m.forget(&h2).unwrap();
    assert!(m.heads("ns", "x", "state").unwrap().is_empty(), "heads must not list the forgotten new head");
    // every head heads() returns must be a live, gettable grain
    for (hh, _) in m.heads("ns", "x", "state").unwrap() {
        assert!(m.get(&hh).is_ok());
    }
}

#[test]
fn fork_then_forget_one_tip_resolves_fork() {
    let (mut a, _d, _h2a, h2b) = build_two_tip_fork();
    assert_eq!(a.heads("ns", "john", "plan").unwrap().len(), 2);
    a.forget(&h2b).unwrap();
    assert_eq!(a.recall("ns", "john", Some("plan"), 8).unwrap().len(), 1);
    assert_eq!(a.heads("ns", "john", "plan").unwrap().len(), 1, "heads drop to 1 after forgetting a tip");
    assert!(a.open_forks().unwrap().is_empty(), "fork resolved");
    // the surviving head is live
    let (hh, _) = a.heads("ns", "john", "plan").unwrap()[0];
    assert!(a.get(&hh).is_ok());
}

// #5 — merge_heads closure is in the op-log.
#[test]
fn merge_heads_closure_logged() {
    let (mut a, _d, _h2a, _h2b) = build_two_tip_fork();
    let before = a.changes_since(0, 10_000).unwrap().len();
    let mut v3 = fact("ns", "john", "plan", "enterprise (merged)");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    let after = a.changes_since(0, 10_000).unwrap();
    let new_ops: Vec<i64> = after[before..].iter().map(|o| o.op).collect();
    assert!(new_ops.contains(&OP_SUPERSEDE), "merge must log OP_SUPERSEDE: {new_ops:?}");
}

// #5 — a supersession survives 2-hop replication (A->B->C) without forking.
#[test]
fn supersede_two_hop_replication_converges() {
    let (da, db_, dc) = (TempDir::new().unwrap(), TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut a = DejaDB::open(da.path().join("a.db").to_str().unwrap()).unwrap();
    let h = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let mut munich = fact("ns", "alice", "lives_in", "Munich");
    a.supersede(&h, &mut munich).unwrap();
    let ab = da.path().join("ab.mgb");
    a.bundle_since(0, ab.to_str().unwrap()).unwrap();
    let mut b = DejaDB::open(db_.path().join("b.db").to_str().unwrap()).unwrap();
    b.import_bundle(ab.to_str().unwrap()).unwrap();
    assert_eq!(b.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(), 1);
    // B's op-log must carry the supersession for onward replication.
    assert!(b.changes_since(0, 100).unwrap().iter().any(|o| o.op == OP_SUPERSEDE),
        "replica op-log missing OP_SUPERSEDE");
    let bc = db_.path().join("bc.mgb");
    b.bundle_since(0, bc.to_str().unwrap()).unwrap();
    let mut c = DejaDB::open(dc.path().join("c.db").to_str().unwrap()).unwrap();
    c.import_bundle(bc.to_str().unwrap()).unwrap();
    assert_eq!(c.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(), 1,
        "two-hop supersede must not fork on C");
    assert_eq!(c.latest("ns", "alice", "lives_in").unwrap().unwrap().get_str("object"), Some("Munich"));
}

// #5 — a merge replicates as a fork-closure (import closes all merge_parents).
#[test]
fn merge_replicates_as_fork_closure() {
    let (mut a, d, _h2a, _h2b) = build_two_tip_fork();
    let mut v3 = fact("ns", "john", "plan", "enterprise (merged)");
    a.merge_heads("ns", "john", "plan", &mut v3).unwrap();
    // replicate the whole history (incl. the merge) into a fresh peer
    let full = d.path().join("full.mgb");
    a.bundle_since(0, full.to_str().unwrap()).unwrap();
    let dp = TempDir::new().unwrap();
    let mut peer = DejaDB::open(dp.path().join("peer.db").to_str().unwrap()).unwrap();
    peer.import_bundle(full.to_str().unwrap()).unwrap();
    assert_eq!(peer.heads("ns", "john", "plan").unwrap().len(), 1, "merge must close the fork on the peer");
    assert_eq!(peer.recall("ns", "john", Some("plan"), 8).unwrap().len(), 1);
}

// #5 — import remains idempotent after the added op-log writes.
#[test]
fn supersede_import_idempotent() {
    let d = TempDir::new().unwrap();
    let mut a = DejaDB::open(d.path().join("a.db").to_str().unwrap()).unwrap();
    let h = a.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let mut m = fact("ns", "alice", "lives_in", "Munich");
    a.supersede(&h, &mut m).unwrap();
    let bundle = d.path().join("a.mgb");
    a.bundle_since(0, bundle.to_str().unwrap()).unwrap();
    let dp = TempDir::new().unwrap();
    let mut b = DejaDB::open(dp.path().join("b.db").to_str().unwrap()).unwrap();
    b.import_bundle(bundle.to_str().unwrap()).unwrap();
    let ops1 = b.changes_since(0, 1000).unwrap().len();
    let s2 = b.import_bundle(bundle.to_str().unwrap()).unwrap(); // re-import
    assert_eq!(s2.applied, 0, "second import must apply nothing");
    assert_eq!(b.changes_since(0, 1000).unwrap().len(), ops1, "re-import must not grow the op-log");
    assert_eq!(b.recall("ns", "alice", Some("lives_in"), 16).unwrap().len(), 1);
}

// #9 — supersede with a changed (subject,relation) reconciles the old key.
#[test]
fn supersede_changed_relation_reconciles_old_key() {
    let (mut m, _d) = open_mem();
    let h1 = m.add(&fact("ns", "alice", "lives_in", "Berlin")).unwrap();
    let mut newg = fact("ns", "alice", "mood", "happy");
    m.supersede(&h1, &mut newg).unwrap();
    assert!(m.recall("ns", "alice", Some("lives_in"), 8).unwrap().is_empty());
    assert!(m.latest("ns", "alice", "lives_in").unwrap().is_none(), "old key must not surface the superseded grain");
    assert!(m.heads("ns", "alice", "lives_in").unwrap().is_empty(), "old key must have no open head");
    // the new relation is live
    assert_eq!(m.latest("ns", "alice", "mood").unwrap().unwrap().get_str("object"), Some("happy"));
}

// #8 — get_blob returns Err (never panics) on a malformed cas URI.
#[test]
fn get_blob_rejects_malformed_uri() {
    let (mut m, _d) = open_mem();
    for bad in ["cas://sha256:", "cas://sha256:a", "cas://sha256:\u{20ac}", "cas://sha256:xyz", "not-a-uri"] {
        assert!(m.get_blob(bad).is_err(), "must reject {bad:?}");
    }
    // a well-formed but absent blob is also a clean Err
    assert!(m.get_blob(&format!("cas://sha256:{}", "a".repeat(64))).is_err());
}

// #6 — FTS special characters fail open: recall_hybrid never errors even when
// the raw query trips the FTS query-grammar (`:` quotes parens AND/OR). The
// low-level search_text primitive may still surface the parse error; the
// documented "never errors" contract is on recall_hybrid, and its BM25 leg
// degrades to empty while the structural/vector legs still answer.
#[test]
fn fts_special_chars_fail_open() {
    let (mut m, _d) = open_mem();
    m.add(&fact("caller", "john", "allergic_to", "peanuts")).unwrap();
    for q in ["did:john", "he said \"hi", "(memory", "3:30", "a AND", "b OR", "x:y:z"] {
        let r = m.recall_hybrid("caller", None, None, Some(q), 10, None);
        assert!(r.is_ok(), "query {q:?} must fail open, got {:?}", r.err());
    }
    // and a normal query still works
    assert!(!m.recall_hybrid("caller", None, None, Some("peanuts"), 10, None).unwrap().is_empty());
}

// #10 — a vector leg with a mismatched embedder dim fails open.
struct Embed64;
impl EmbedBackend for Embed64 {
    fn dim(&self) -> usize { 64 }
    fn embed(&self, text: &str) -> dejadb_core::error::Result<Vec<f32>> {
        let mut v = vec![0f32; 64];
        for (i, b) in text.bytes().enumerate() { v[i % 64] += b as f32; }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
}
struct Embed128;
impl EmbedBackend for Embed128 {
    fn dim(&self) -> usize { 128 }
    fn embed(&self, text: &str) -> dejadb_core::error::Result<Vec<f32>> {
        let mut v = vec![0f32; 128];
        for (i, b) in text.bytes().enumerate() { v[i % 128] += b as f32; }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
}
#[test]
fn vector_dim_mismatch_fails_open() {
    let d = TempDir::new().unwrap();
    let p = d.path().join("m.db");
    let p = p.to_str().unwrap();
    {
        let mut m = DejaDB::open(p).unwrap();
        m.set_embedder(Box::new(Embed64));
        m.add(&fact("caller", "john", "allergic_to", "peanuts")).unwrap();
    }
    let mut m = DejaDB::open(p).unwrap();
    m.set_embedder(Box::new(Embed128));
    let r = m.recall_hybrid("caller", None, None, Some("peanuts"), 10, None);
    assert!(r.is_ok(), "dim mismatch must fail open: {:?}", r.err());
    // structural leg still answers
    assert!(!m.recall_hybrid("caller", Some("john"), None, Some("peanuts"), 10, None).unwrap().is_empty());
}
