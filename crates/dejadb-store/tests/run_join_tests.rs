//! The join: querying across execution history and semantic memory.
//!
//! Every agent stack keeps these apart — a checkpointer holds in-thread
//! execution state, a memory store holds cross-thread facts — and nothing can
//! query across the seam. Here they are the same substrate, so "what did this
//! run produce" and "which runs touched this fact" are one lookup each.
//!
//! Two indexes make that cheap: `run_idx` (run_id -> grains) and `prov_idx`
//! (parent address -> grains derived from it). Before `prov_idx`,
//! `grains_derived_from` read and deserialized *every grain in the store* on
//! each call.

use dejadb_core::types::*;
use dejadb_store::DejaDB;
use tempfile::TempDir;

fn open_mem() -> (DejaDB, TempDir) {
    let d = TempDir::new().unwrap();
    let m = DejaDB::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

/// One run: two transcript Events, and a Fact distilled from the first.
/// Returns `(event1, event2, fact)`.
fn a_run(
    m: &mut DejaDB,
    run: &str,
    at: i64,
) -> (
    dejadb_core::error::Hash,
    dejadb_core::error::Hash,
    dejadb_core::error::Hash,
) {
    let e1 = m
        .add(
            &Event::new("caller asked about refunds")
                .run_id(run.to_string())
                .created_at(at)
                .namespace("ops"),
        )
        .unwrap();
    let e2 = m
        .add(
            &Event::new("agent quoted the 30-day policy")
                .run_id(run.to_string())
                .created_at(at + 1)
                .namespace("ops"),
        )
        .unwrap();
    let mut fact = Fact::new("refunds", "window_days", "30");
    fact.common.derived_from = Some(e1.to_hex());
    fact.common.created_at = Some(at + 2);
    fact.common.namespace = Some("ops".into());
    let f = m.add(&fact).unwrap();
    (e1, e2, f)
}

#[test]
fn run_trace_returns_only_that_runs_grains() {
    let (mut m, _d) = open_mem();
    a_run(&mut m, "run-a", 1_700_000_000_000);
    a_run(&mut m, "run-b", 1_700_000_010_000);

    let trace = m.run_trace("ops", "run-a", 100).unwrap();
    assert_eq!(trace.len(), 2, "two events, and the fact is not in the run");
    for g in &trace {
        assert_eq!(g.get_str("run_id"), Some("run-a"));
    }
}

#[test]
fn run_yield_crosses_from_execution_history_into_memory() {
    // The join: the run's transcript is Events; what it *produced* is a Fact
    // that is not itself part of the run.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let produced = m.run_yield("ops", "run-a", 100).unwrap();
    assert_eq!(produced.len(), 1, "{produced:?}");
    assert_eq!(produced[0].hash, fact);
    assert_eq!(produced[0].get_str("object"), Some("30"));
}

#[test]
fn runs_touching_walks_back_from_a_fact_into_the_runs_that_made_it() {
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);
    a_run(&mut m, "run-b", 1_700_000_010_000);

    let runs = m.runs_touching("ops", &fact, 4).unwrap();
    assert_eq!(runs, vec!["run-a".to_string()], "only the run that made it");
}

#[test]
fn runs_touching_follows_a_refinement_chain_across_runs() {
    // A later run supersedes/refines the fact. Both runs touched it, and the
    // chain is what connects them — neither grain names the other run.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let e3 = m
        .add(
            &Event::new("policy changed to 45 days")
                .run_id("run-b".to_string())
                .created_at(1_700_000_020_000)
                .namespace("ops"),
        )
        .unwrap();
    let mut refined = Fact::new("refunds", "window_days", "45");
    refined.common.derived_from = Some(fact.to_hex());
    refined.common.created_at = Some(1_700_000_021_000);
    refined.common.namespace = Some("ops".into());
    let refined_hash = m.add(&refined).unwrap();
    assert_ne!(refined_hash, e3);

    // From the original fact, walking down reaches the refinement; walking up
    // from the refinement reaches the original and its run.
    let mut runs = m.runs_touching("ops", &fact, 4).unwrap();
    runs.sort();
    assert_eq!(runs, vec!["run-a".to_string()]);

    // The refinement's own lineage reaches run-a through the fact it refined.
    let runs2 = m.runs_touching("ops", &refined_hash, 4).unwrap();
    assert!(runs2.contains(&"run-a".to_string()), "{runs2:?}");
}

#[test]
fn runs_touching_is_bounded_by_depth() {
    // A long-lived memory's lineage is unbounded, so the walk must be capped.
    let (mut m, _d) = open_mem();
    let (e1, _e2, _f) = a_run(&mut m, "run-a", 1_700_000_000_000);

    // A chain of derivations, each in its own run, hanging off the first event.
    let mut parent = e1;
    for i in 0..5 {
        let mut f = Fact::new("chain", "step", &i.to_string());
        f.common.derived_from = Some(parent.to_hex());
        f.common.created_at = Some(1_700_000_100_000 + i);
        f.common.namespace = Some("ops".into());
        parent = m.add(&f).unwrap();
    }

    let shallow = m.runs_touching("ops", &parent, 1).unwrap();
    let deep = m.runs_touching("ops", &parent, 8).unwrap();
    assert!(
        deep.len() >= shallow.len(),
        "a deeper walk cannot see less: {shallow:?} vs {deep:?}"
    );
    assert!(deep.contains(&"run-a".to_string()), "{deep:?}");
}

#[test]
fn grains_derived_from_is_served_by_the_index() {
    let (mut m, _d) = open_mem();
    let (e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let kids = m.grains_derived_from(&e1).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].hash, fact);

    // A grain nothing derives from answers empty, not everything.
    assert!(m.grains_derived_from(&fact).unwrap().is_empty());
}

#[test]
fn the_join_survives_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let fact;
    {
        let mut m = DejaDB::open(path.to_str().unwrap()).unwrap();
        let (_e1, _e2, f) = a_run(&mut m, "run-a", 1_700_000_000_000);
        fact = f;
    }
    let mut m = DejaDB::open(path.to_str().unwrap()).unwrap();
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.run_yield("ops", "run-a", 100).unwrap().len(), 1);
    assert_eq!(m.runs_touching("ops", &fact, 4).unwrap(), vec!["run-a"]);
}

#[test]
fn unknown_runs_and_namespaces_answer_empty_rather_than_erroring() {
    let (mut m, _d) = open_mem();
    a_run(&mut m, "run-a", 1_700_000_000_000);

    assert!(m.run_trace("ops", "no-such-run", 100).unwrap().is_empty());
    assert!(m.run_trace("no-such-ns", "run-a", 100).unwrap().is_empty());
    assert!(m.run_yield("ops", "no-such-run", 100).unwrap().is_empty());
}

#[test]
fn rebuild_link_indexes_backfills_and_is_idempotent() {
    // A file written before these indexes existed answers provenance and run
    // questions with nothing until it is reindexed.
    let (mut m, _d) = open_mem();
    let (_e1, _e2, fact) = a_run(&mut m, "run-a", 1_700_000_000_000);

    let first = m.rebuild_link_indexes().unwrap();
    assert!(first >= 3, "two run rows + one provenance row: {first}");
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.runs_touching("ops", &fact, 4).unwrap(), vec!["run-a"]);

    // Running it twice must not double-count or duplicate rows.
    let second = m.rebuild_link_indexes().unwrap();
    assert_eq!(first, second);
    assert_eq!(m.run_trace("ops", "run-a", 100).unwrap().len(), 2);
    assert_eq!(m.grains_derived_from(&_e1).unwrap().len(), 1);
}
