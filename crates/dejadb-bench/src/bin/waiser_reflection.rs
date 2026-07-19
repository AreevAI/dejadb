//! waiser_reflection — the offline **Effective Reliability** eval for the LLM
//! reflection path (design `docs/waiser-reflection.md` §6). It is the LLM-path
//! analog of `waiser_precision`: a labeled corpus of planted issues + decoys,
//! run through the *real* GROUND→VERIFY→ROUTE pipeline, scored by a metric that
//! **subtracts for confident-wrong** — the shape that punishes over-generation:
//!
//!   Effective Reliability = (useful-correct − wrong) / total_scenarios
//!
//! Two things to be honest about:
//!
//! 1. The built-in `RefReviewer` is a **deterministic stand-in** for a real
//!    model — it over-generates at PROPOSE (a finding for every seeded subject,
//!    decoys included) and its VERIFY reads the cited evidence and keeps a
//!    finding only if the grains *genuinely* conflict. That makes this run a
//!    *machinery reference* (does the pipeline route planted issues through and
//!    filter decoys, and does the scorer compute ER correctly?) — **not** a
//!    model-quality claim. The field number comes from running the same corpus
//!    through a real model via `--llm-cmd` (`CommandLlm`).
//! 2. It also prints the ER the pipeline would get **without** the verifier
//!    (accept every grounded draft) — the delta is the verifier's value.
//!
//! Run: `cargo run --release -p dejadb-bench --bin waiser_reflection`

use serde_json::{json, Map, Value};
use waiser::{Engine, GrainRecord, LlmBackend, ReferenceSubstrate, RunOptions};

// Kept small so the over-generated drafts (one per subject, pos+neg) fit under
// the engine's per-run MAX_LLM_DRAFTS cap without truncation.
const N: usize = 3; // planted positives (and decoys)
const NOW: i64 = 2_000_000_000_000;

/// A human-style label for a surfaced (or abstained) finding.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    UsefulCorrect,
    Wrong,
}

/// The Effective-Reliability report.
struct Reliability {
    surfaced: usize,
    useful_correct: usize,
    wrong: usize,
    er: f64,
    precision: f64,
    recall: f64,
    spurious_rate: f64, // wrong / surfaced
}

/// Score a set of surfaced-finding verdicts. `positives` is how many planted
/// issues exist; ER divides the net (useful-correct − wrong) by that, so both
/// a miss (recall) and a false positive (wrong) pull it down.
fn score(verdicts: &[Verdict], positives: usize) -> Reliability {
    let surfaced = verdicts.len();
    let useful_correct = verdicts.iter().filter(|v| **v == Verdict::UsefulCorrect).count();
    let wrong = verdicts.iter().filter(|v| **v == Verdict::Wrong).count();
    let er = (useful_correct as f64 - wrong as f64) / positives.max(1) as f64;
    let precision = if surfaced > 0 { useful_correct as f64 / surfaced as f64 } else { 1.0 };
    let recall = useful_correct as f64 / positives.max(1) as f64;
    let spurious_rate = if surfaced > 0 { wrong as f64 / surfaced as f64 } else { 0.0 };
    Reliability { surfaced, useful_correct, wrong, er, precision, recall, spurious_rate }
}

/// A deterministic reference reviewer (a stand-in for a real model; see the
/// module docs). Over-generates at DISCOVER; grounds everything that cites real
/// grains; and VERIFIES by actually reading the cited evidence — keeping a
/// finding only when its two grains genuinely conflict (same subject+relation,
/// different object). It never reads the pos_/neg_ labels — only the evidence.
struct RefReviewer;

impl LlmBackend for RefReviewer {
    fn model(&self) -> &str {
        "reference-reviewer"
    }
    fn complete(&self, request: &str) -> waiser::Result<String> {
        let req: Value = serde_json::from_str(request).unwrap_or(Value::Null);
        let op = req.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match op {
            "discover" => Ok(discover_over_generate(&req)),
            "ground" => Ok(ground_all_supported(&req)),
            "verify" => Ok(verify_by_reading_evidence(&req)),
            _ => Ok("{}".into()), // enrich etc.
        }
    }
}

/// One draft per distinct subject seen in the evidence bundle (decoys included).
fn discover_over_generate(req: &Value) -> String {
    let mut by_subject: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    if let Some(ev) = req.get("evidence").and_then(|v| v.as_array()) {
        for e in ev {
            let hash = e.get("hash").and_then(|v| v.as_str()).unwrap_or("");
            let text = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(subject) = text.split_whitespace().next() {
                by_subject.entry(subject.to_string()).or_default().push(hash.to_string());
            }
        }
    }
    let recs: Vec<Value> = by_subject
        .into_iter()
        .map(|(subject, hashes)| {
            json!({
                "summary": format!("possible semantic issue with {subject}"),
                "target": format!("entity:bench/{subject}"),
                "evidence": hashes,
                "confidence": 0.9,
            })
        })
        .collect();
    json!({ "recommendations": recs }).to_string()
}

fn ground_all_supported(req: &Value) -> String {
    let ids = req.get("claims").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let results: Vec<Value> = ids
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_u64()))
        .map(|id| json!({ "id": id, "supported": true }))
        .collect();
    json!({ "results": results }).to_string()
}

/// Keep a finding iff its cited grains genuinely conflict: ≥2 grains that share
/// subject+relation but differ on object. Reads the evidence, not the labels.
fn verify_by_reading_evidence(req: &Value) -> String {
    let items = req.get("findings").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let results: Vec<Value> = items
        .iter()
        .filter_map(|it| {
            let id = it.get("id").and_then(|v| v.as_u64())?;
            let ev = it.get("evidence").and_then(|v| v.as_array())?;
            // Parse "subject relation object" out of each cited grain.
            let mut objs_by_sr: std::collections::BTreeMap<(String, String), std::collections::BTreeSet<String>> =
                Default::default();
            for e in ev {
                let text = e.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let parts: Vec<&str> = text.splitn(3, ' ').collect();
                if parts.len() == 3 {
                    objs_by_sr
                        .entry((parts[0].to_string(), parts[1].to_string()))
                        .or_default()
                        .insert(parts[2].to_string());
                }
            }
            let conflicts = objs_by_sr.values().any(|objs| objs.len() >= 2);
            Some(json!({ "id": id, "keep": conflicts, "confidence": if conflicts { 0.9 } else { 0.2 } }))
        })
        .collect();
    json!({ "results": results }).to_string()
}

fn main() {
    let mut sub = ReferenceSubstrate::new();
    let mut clock = 0i64;
    let mut mk = |sub: &mut ReferenceSubstrate, subject: &str, relation: &str, object: &str| {
        clock += 1;
        let mut fields = Map::new();
        fields.insert("subject".into(), json!(subject));
        fields.insert("relation".into(), json!(relation));
        fields.insert("object".into(), json!(object));
        fields.insert("namespace".into(), json!("bench"));
        sub.insert(GrainRecord {
            hash: String::new(),
            grain_type: "fact".into(),
            namespace: "bench".into(),
            created_at_ms: NOW - 3_600_000 + clock,
            valid_to_ms: None,
            superseded_by: None,
            fields,
        });
    };
    for i in 0..N {
        // positive: a real contradiction (two live values under a functional
        // relation). The deterministic sweep fires AND the finding is genuine,
        // so the reviewer's over-generated draft should survive VERIFY.
        mk(&mut sub, &format!("pos_con_{i}"), "deploy_target", "us-east-1");
        mk(&mut sub, &format!("pos_con_{i}"), "deploy_target", "eu-west-1");
        // decoy: an exact duplicate (two identical facts). The deterministic
        // sweep fires (seeding the bundle), the reviewer over-generates a
        // spurious "semantic issue", but VERIFY reads the evidence and sees no
        // conflict → kills it. A finding that survives here is a false positive.
        mk(&mut sub, &format!("neg_dup_{i}"), "tier", "gold");
        mk(&mut sub, &format!("neg_dup_{i}"), "tier", "gold");
    }

    // Default: the deterministic reference reviewer (a machinery reference).
    // `WAISER_EVAL_MODEL=openrouter:openai/gpt-4o-mini` runs the corpus through
    // a real model for the field number.
    let real = std::env::var("WAISER_EVAL_MODEL").ok().filter(|s| !s.is_empty());
    let engine = match &real {
        Some(spec) => {
            eprintln!("# reviewer: real model `{spec}` (via dejadb-llm)");
            Engine::with_builtins()
                .with_llm(dejadb_llm::resolve(spec, None, None).expect("resolve WAISER_EVAL_MODEL"))
        }
        None => Engine::with_builtins().with_llm(Box::new(RefReviewer)),
    };
    engine.run(&mut sub, &RunOptions::default(), NOW).expect("run");
    let recs = engine.recommendations(&sub, None).expect("list");

    // Classify only the llm-origin (surfaced-by-reflection) recs.
    let mut verdicts = Vec::new();
    for r in &recs {
        if !format!("{:?}", r.origin).contains("Llm") {
            continue;
        }
        let s = r.summary.render();
        if s.contains("pos_") {
            verdicts.push(Verdict::UsefulCorrect);
        } else if s.contains("neg_") {
            verdicts.push(Verdict::Wrong);
        }
    }
    let rep = score(&verdicts, N);

    // The counterfactual: without the verifier, every over-generated draft
    // (pos AND neg) would surface — N useful, N wrong.
    let raw = score(&[Verdict::UsefulCorrect, Verdict::Wrong].repeat(N), N);

    println!("# waiser_reflection — Effective-Reliability (machinery reference; real number via --llm-cmd)\n");
    println!("planted positives: {N}   decoys: {N}\n");
    println!("| pipeline | surfaced | useful | wrong | ER | precision | recall | spurious |");
    println!("|---|---|---|---|---|---|---|---|");
    let row = |name: &str, r: &Reliability| {
        println!(
            "| {name} | {} | {} | {} | {:+.2} | {:.2} | {:.2} | {:.2} |",
            r.surfaced, r.useful_correct, r.wrong, r.er, r.precision, r.recall, r.spurious_rate
        );
    };
    row("no verifier (accept grounded)", &raw);
    row("with verifier", &rep);
    println!(
        "\n_Effective Reliability = (useful-correct − wrong) / positives; it subtracts for \
         confident-wrong, so over-generation lowers it. The verifier lifts ER from {:+.2} to \
         {:+.2} on this corpus by filtering the decoys._",
        raw.er, rep.er
    );

    // CI guard applies only to the deterministic reference reviewer; a real
    // model produces an exploratory number, not a gate.
    if real.is_none() && (rep.spurious_rate > 0.0 || rep.recall < 0.9) {
        eprintln!(
            "\nREGRESSION: spurious_rate {:.2} (want 0), recall {:.2} (want ≥0.9)",
            rep.spurious_rate, rep.recall
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn er_subtracts_for_wrong() {
        // 4 useful, 1 wrong, 5 planted → ER = (4 − 1)/5 = 0.6.
        let v = [
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::UsefulCorrect,
            Verdict::Wrong,
        ];
        let r = score(&v, 5);
        assert!((r.er - 0.6).abs() < 1e-9, "er {}", r.er);
        assert!((r.precision - 0.8).abs() < 1e-9);
        assert!((r.recall - 0.8).abs() < 1e-9);
        assert!((r.spurious_rate - 0.2).abs() < 1e-9);
    }

    #[test]
    fn er_can_go_negative_when_mostly_wrong() {
        // 1 useful, 3 wrong, 4 planted → (1 − 3)/4 = −0.5. Over-generation is
        // punished below zero — the point of the metric.
        let v = [Verdict::UsefulCorrect, Verdict::Wrong, Verdict::Wrong, Verdict::Wrong];
        let r = score(&v, 4);
        assert!((r.er + 0.5).abs() < 1e-9, "er {}", r.er);
    }

    #[test]
    fn perfect_abstention_scores_zero_not_negative() {
        let r = score(&[], 3);
        assert_eq!(r.er, 0.0);
        assert_eq!(r.surfaced, 0);
    }
}
