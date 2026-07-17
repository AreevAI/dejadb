//! waiser_precision — fixture-measured precision/recall for the Waiser
//! analyzers (proposal §8: "measured numbers decide the default-on set; no
//! invented precision percentages anywhere").
//!
//! The fixture is a labeled corpus: for each analyzer, N *positives* (planted
//! situations the analyzer SHOULD flag, on subjects prefixed `pos`) and N
//! *decoys* (look-alikes it must NOT flag, prefixed `neg`). We run the real
//! engine over an in-memory `ReferenceSubstrate` and classify every proposed
//! recommendation by whether its (deterministic) summary names a `pos` or a
//! `neg` subject:
//!
//!   precision = pos-hits / (pos-hits + neg-hits)     recall = pos-hits / N
//!
//! On this clean fixture a correct analyzer scores precision 1.0 (never fires
//! on a decoy); the bin exits non-zero if a default-on analyzer regresses
//! below 0.9, so it doubles as a CI guard. Real-world precision needs a real
//! telemetry+labels corpus — this harness is the reproducible floor.
//!
//! Run: `cargo run --release -p dejadb-bench --bin waiser_precision`

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use waiser::{Engine, GrainRecord, ReferenceSubstrate, RunOptions};

const N: usize = 6; // positives (and decoys) per analyzer
const NOW: i64 = 2_000_000_000_000; // ~2033, so a past valid_to is elapsed
const PAST: i64 = 1_000_000_000_000; // ~2001
const FUTURE: i64 = 9_000_000_000_000; // ~2255

fn main() {
    let mut sub = ReferenceSubstrate::new();
    let mut clock = 0i64;
    let mut expected: BTreeMap<&str, usize> = BTreeMap::new();
    let mk = |sub: &mut ReferenceSubstrate, clock: &mut i64, gtype: &str, fields: Map<String, Value>, valid_to: Option<i64>| {
        *clock += 1;
        sub.insert(GrainRecord {
            hash: String::new(),
            grain_type: gtype.into(),
            namespace: "bench".into(),
            // Recent (inside the tool-failure 30-day window), still ordered.
            created_at_ms: NOW - 3_600_000 + *clock,
            valid_to_ms: valid_to,
            superseded_by: None,
            fields,
        });
    };
    let fact = |subject: &str, relation: &str, object: &str| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("subject".into(), json!(subject));
        m.insert("relation".into(), json!(relation));
        m.insert("object".into(), json!(object));
        m.insert("namespace".into(), json!("bench"));
        m
    };
    let tool = |name: &str, is_error: bool, content: &str| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("tool_name".into(), json!(name));
        m.insert("is_error".into(), json!(is_error));
        m.insert("content".into(), json!(content));
        m.insert("namespace".into(), json!("bench"));
        m
    };

    for i in 0..N {
        // --- duplicate_sweep ---
        // positive: two identical triples.
        mk(&mut sub, &mut clock, "fact", fact(&format!("pos_dup_{i}"), "tier", "gold"), None);
        mk(&mut sub, &mut clock, "fact", fact(&format!("pos_dup_{i}"), "tier", "gold"), None);
        // decoy: same subject, DIFFERENT object under a non-functional relation.
        mk(&mut sub, &mut clock, "fact", fact(&format!("neg_dup_{i}"), "likes", "tea"), None);
        mk(&mut sub, &mut clock, "fact", fact(&format!("neg_dup_{i}"), "likes", "coffee"), None);
        *expected.entry("duplicate_sweep").or_default() += 1;

        // --- contradiction_sweep ---
        // positive: two live values under a functional relation.
        mk(&mut sub, &mut clock, "fact", fact(&format!("pos_con_{i}"), "deploy_target", "us-east-1"), None);
        mk(&mut sub, &mut clock, "fact", fact(&format!("pos_con_{i}"), "deploy_target", "eu-west-1"), None);
        // decoy: a single value under a functional relation (no conflict).
        mk(&mut sub, &mut clock, "fact", fact(&format!("neg_con_{i}"), "deploy_target", "us-east-1"), None);
        *expected.entry("contradiction_sweep").or_default() += 1;

        // --- staleness ---
        // positive: elapsed valid_to. decoy: future valid_to.
        mk(&mut sub, &mut clock, "fact", fact(&format!("pos_stale_{i}"), "promo", "on"), Some(PAST));
        mk(&mut sub, &mut clock, "fact", fact(&format!("neg_stale_{i}"), "promo", "on"), Some(FUTURE));
        *expected.entry("staleness").or_default() += 1;

        // --- tool_failure ---
        // positive: a dominant failure cluster (4/5). decoy: 1/5 (below rate).
        for _ in 0..4 {
            mk(&mut sub, &mut clock, "tool", tool(&format!("pos_tool_{i}"), true, "rate_limited 429"), None);
        }
        mk(&mut sub, &mut clock, "tool", tool(&format!("pos_tool_{i}"), false, "ok"), None);
        mk(&mut sub, &mut clock, "tool", tool(&format!("neg_tool_{i}"), true, "boom 500"), None);
        for _ in 0..4 {
            mk(&mut sub, &mut clock, "tool", tool(&format!("neg_tool_{i}"), false, "ok"), None);
        }
        *expected.entry("tool_failure").or_default() += 1;
    }

    let engine = Engine::with_builtins();
    engine
        .run(&mut sub, &RunOptions::default(), NOW)
        .expect("run");
    let recs = engine.recommendations(&sub, None).expect("list");

    // Classify each recommendation by its (deterministic) summary text.
    let mut tp: BTreeMap<String, usize> = BTreeMap::new();
    let mut fp: BTreeMap<String, usize> = BTreeMap::new();
    for r in &recs {
        let fam = family(&r.analyzer).to_string();
        let summary = r.summary.render();
        if summary.contains("pos_") {
            *tp.entry(fam).or_default() += 1;
        } else if summary.contains("neg_") {
            *fp.entry(fam).or_default() += 1;
        } else {
            // A rec whose summary names neither (shouldn't happen on this
            // fixture) counts as a false positive — better to over-penalize.
            *fp.entry(fam).or_default() += 1;
        }
    }

    println!("# waiser_precision — fixture-measured (N={N} positives + {N} decoys per analyzer)\n");
    println!("| analyzer | proposed | TP | FP | precision | recall |");
    println!("|---|---|---|---|---|---|");
    let mut regressions = Vec::new();
    let mut families: Vec<&str> = expected.keys().copied().collect();
    families.sort_unstable();
    for fam in families {
        let exp = expected[fam];
        let t = tp.get(fam).copied().unwrap_or(0);
        let f = fp.get(fam).copied().unwrap_or(0);
        let proposed = t + f;
        let precision = if proposed > 0 { t as f64 / proposed as f64 } else { 1.0 };
        let recall = if exp > 0 { t as f64 / exp as f64 } else { 1.0 };
        println!(
            "| waiser.{fam} | {proposed} | {t} | {f} | {:.2} | {:.2} |",
            precision, recall
        );
        if precision < 0.9 {
            regressions.push(format!("waiser.{fam} precision {:.2} < 0.90", precision));
        }
    }
    println!("\n_Default-on candidates: analyzers with precision ≥ 0.90 on the fixture._");

    if !regressions.is_empty() {
        eprintln!("\nREGRESSION:");
        for r in &regressions {
            eprintln!("  {r}");
        }
        std::process::exit(1);
    }
}

/// `waiser.duplicate_sweep/1` → `duplicate_sweep`.
fn family(analyzer_id: &str) -> &str {
    analyzer_id
        .strip_prefix("waiser.")
        .unwrap_or(analyzer_id)
        .split('/')
        .next()
        .unwrap_or(analyzer_id)
}
