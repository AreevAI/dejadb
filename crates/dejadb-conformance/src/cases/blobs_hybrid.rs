//! CAS blob store and hybrid-recall legs — backend-generic: the embedded
//! backend keeps blobs in the `.blobs` fan-out dir and scans vectors with
//! `vector_distance_cos`; the postgres backend uses an in-schema `blobs`
//! table and pgvector. Same contract either way.

use crate::{fact, Backend};
use dejadb_core::error::Result;
use dejadb_store::EmbedBackend;

pub fn cas_blob_roundtrip_and_gc(b: &dyn Backend) {
    let mut m = b.open();
    let uri = m.put_blob(b"hello media bytes").unwrap();
    assert!(uri.starts_with("cas://sha256:"));
    // idempotent put, verified get
    assert_eq!(m.put_blob(b"hello media bytes").unwrap(), uri);
    assert_eq!(m.get_blob(&uri).unwrap(), b"hello media bytes");
    // malformed / absent URIs are clean errors, never panics
    assert!(m.get_blob("cas://sha256:xyz").is_err());
    assert!(m.get_blob(&format!("cas://sha256:{}", "a".repeat(64))).is_err());
    // nothing references the blob -> gc reclaims exactly it
    let removed = m.gc_blobs().unwrap();
    assert_eq!(removed, 1, "unreferenced blob must be reclaimed");
    assert!(m.get_blob(&uri).is_err(), "reclaimed blob is gone");
}

pub fn bm25_leg_finds_text(b: &dyn Backend) {
    let mut m = b.open();
    m.add(&fact("caller", "john", "allergic_to", "peanuts")).unwrap();
    m.add(&fact("caller", "john", "prefers", "quiet rooms")).unwrap();
    m.add(&fact("caller", "mary", "prefers", "loud music")).unwrap();
    // single-token free-text query, no subject anchor
    let hits = m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].get_str("object"), Some("peanuts"));
    // multi-token (exercises the batched postings path on networked backends)
    let hits = m.recall_hybrid("caller", None, None, Some("quiet rooms"), 8, None).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("object"), Some("quiet rooms"));
    // a query hitting nothing returns empty, not an error
    assert!(m.recall_hybrid("caller", None, None, Some("zebra"), 8, None).unwrap().is_empty());
    // heads-only: superseded text drops out of the live results
    let h = m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap();
    let h0 = h[0].hash;
    let mut v2 = fact("caller", "john", "allergic_to", "shellfish");
    m.supersede(&h0, &mut v2).unwrap();
    assert!(
        m.recall_hybrid("caller", None, None, Some("peanuts"), 8, None).unwrap().is_empty(),
        "superseded grain must not surface in a heads-only text search"
    );
}

/// Deterministic toy embedder: character histogram, normalized. Same text →
/// same vector on every backend, so ordering assertions are stable.
struct HistEmbed;
impl EmbedBackend for HistEmbed {
    fn dim(&self) -> usize {
        32
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; 32];
        for (i, b) in text.bytes().enumerate() {
            v[(b as usize + i) % 32] += 1.0;
        }
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        Ok(v.into_iter().map(|x| x / n).collect())
    }
    fn model(&self) -> &str {
        "hist-test"
    }
}

pub fn vector_leg_roundtrip(b: &dyn Backend) {
    let mut m = b.open();
    m.set_embedder(Box::new(HistEmbed));
    assert!(
        !m.open_warnings().iter().any(|w| w.contains("vector recall disabled")),
        "embedder must install: {:?}",
        m.open_warnings()
    );
    m.add(&fact("caller", "john", "drinks", "espresso every morning")).unwrap();
    m.add(&fact("caller", "john", "eats", "toast with jam")).unwrap();
    // the vector leg answers a free-text query (identical text = identical
    // vector = exact nearest hit)
    let hits = m
        .recall_hybrid("caller", None, None, Some("espresso every morning"), 4, None)
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].get_str("object"), Some("espresso every morning"));
    // nearest_semantic returns scored hashes, best first. What gets embedded
    // is the PROJECTED text ("subject relation object"), so querying that
    // exact projection is the self-similarity ~1 case.
    let near = m
        .nearest_semantic("caller", None, None, "john drinks espresso every morning", 2)
        .unwrap();
    assert!(!near.is_empty());
    let top = m.get(&near[0].0).unwrap();
    assert_eq!(top.get_str("object"), Some("espresso every morning"));
    assert!(near[0].1 > 0.9, "self-similarity should be ~1, got {}", near[0].1);
    // vectors survive reopen (stored, not recomputed)
    drop(m);
    let mut m = b.open();
    m.set_embedder(Box::new(HistEmbed));
    let near = m
        .nearest_semantic("caller", None, None, "john eats toast with jam", 1)
        .unwrap();
    assert_eq!(m.get(&near[0].0).unwrap().get_str("object"), Some("toast with jam"));
}
