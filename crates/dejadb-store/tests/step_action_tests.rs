//! OMS §8.4 execution records: `mg:step_action:<node_id>`.
//!
//! A Workflow grain is immutable and content-addressed, so it can never
//! accumulate run state. The spec's answer is to point the execution records at
//! the plan instead — a Tool grain carrying a `related_to` link of type
//! `mg:step_action:<node_id>` whose target is the Workflow's hash.
//!
//! These links were serialized but never indexed (`related_to` appeared nowhere
//! in this crate), so they were unreachable. They are now indexed into
//! `triples`/`osp` — and deliberately **not** into `heads`/`entity_latest`,
//! because OMS §15.3 is normative that a `related_to` link MUST NOT change the
//! target grain's supersession state.

use dejadb_core::types::*;
use dejadb_store::{DejaDB, Direction};
use tempfile::TempDir;

fn open_mem() -> (DejaDB, TempDir) {
    let d = TempDir::new().unwrap();
    let m = DejaDB::open(d.path().join("m.db").to_str().unwrap()).unwrap();
    (m, d)
}

/// A three-step plan, stored.
fn plan(m: &mut DejaDB) -> dejadb_core::error::Hash {
    let wf = Workflow::new(vec!["build".into(), "test".into(), "deploy".into()])
        .trigger("merge to main")
        .edge("build", "test")
        .edge("test", "deploy")
        .created_at(1_700_000_000_000)
        .namespace("ci");
    m.add(&wf).unwrap()
}

/// One execution record for `node`, linked to the plan.
fn ran(m: &mut DejaDB, wf: &dejadb_core::error::Hash, node: &str, at: i64) -> dejadb_core::error::Hash {
    let tool = Tool::new(node)
        .created_at(at)
        .namespace("ci")
        .step_action(&wf.to_hex(), node);
    m.add(&tool).unwrap()
}

#[test]
fn step_actions_link_execution_records_to_the_plan() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let build = ran(&mut m, &wf, "build", 1_700_000_001_000);
    let test = ran(&mut m, &wf, "test", 1_700_000_002_000);

    let got = m.step_actions("ci", &wf, None, 100).unwrap();
    assert_eq!(got.len(), 2, "{got:?}");
    assert!(got.contains(&("build".to_string(), build)));
    assert!(got.contains(&("test".to_string(), test)));
}

#[test]
fn step_actions_narrow_to_one_node() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    ran(&mut m, &wf, "build", 1_700_000_001_000);
    let test = ran(&mut m, &wf, "test", 1_700_000_002_000);

    let got = m.step_actions("ci", &wf, Some("test"), 100).unwrap();
    assert_eq!(got, vec![("test".to_string(), test)]);
}

#[test]
fn repeated_attempts_at_one_node_all_survive() {
    // Retries are the normal case for a workflow node, and every attempt is its
    // own immutable grain — none of them supersedes another.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let a1 = ran(&mut m, &wf, "test", 1_700_000_001_000);
    let a2 = ran(&mut m, &wf, "test", 1_700_000_002_000);
    let a3 = ran(&mut m, &wf, "test", 1_700_000_003_000);

    let got = m.step_actions("ci", &wf, Some("test"), 100).unwrap();
    assert_eq!(got.len(), 3, "every attempt is a distinct record: {got:?}");
    for h in [a1, a2, a3] {
        assert!(got.iter().any(|(_, g)| *g == h), "missing attempt {h:?}");
    }
}

#[test]
fn step_action_does_not_touch_the_workflow_supersession_state() {
    // OMS §15.3, normative: a `related_to` link is an annotation. Indexing it
    // must not mark the target superseded, contradicted, or closed — otherwise
    // recording a run would silently retire the plan it ran.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    for node in ["build", "test", "deploy"] {
        ran(&mut m, &wf, node, 1_700_000_001_000);
    }

    let g = m.get(&wf).unwrap();
    assert_eq!(g.grain_type, GrainType::Workflow);
    assert!(
        !g.fields.contains_key("superseded_by"),
        "execution records must not supersede the plan"
    );

    // And the plan is still the live head of its own namespace.
    let live = m.recent_live("ci", Some(GrainType::Workflow), 10).unwrap();
    assert_eq!(live.len(), 1, "plan must remain a live workflow");
}

#[test]
fn links_are_traversable_in_both_directions() {
    // The link is indexed as a triple subject-ed on the executing grain's own
    // hash, so the generic graph API reaches it from either end.
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);
    let build = ran(&mut m, &wf, "build", 1_700_000_001_000);
    let rel = step_action_relation("build");

    let out = m
        .related("ci", &build.to_hex(), &[rel.as_str()], Direction::Out, 1, 10)
        .unwrap();
    assert_eq!(out, vec![wf.to_hex()], "record -> plan");

    let back = m
        .related("ci", &wf.to_hex(), &[rel.as_str()], Direction::In, 1, 10)
        .unwrap();
    assert_eq!(back, vec![build.to_hex()], "plan -> record");
}

#[test]
fn unrelated_workflows_do_not_bleed_into_each_other() {
    let (mut m, _d) = open_mem();
    let a = plan(&mut m);
    let b = m
        .add(
            &Workflow::new(vec!["build".into()])
                .trigger("nightly")
                .created_at(1_700_000_009_000)
                .namespace("ci"),
        )
        .unwrap();
    assert_ne!(a, b);

    ran(&mut m, &a, "build", 1_700_000_001_000);

    assert_eq!(m.step_actions("ci", &a, None, 100).unwrap().len(), 1);
    assert!(
        m.step_actions("ci", &b, None, 100).unwrap().is_empty(),
        "a record links to exactly the plan it names"
    );
}

#[test]
fn step_actions_survive_reopen() {
    let d = TempDir::new().unwrap();
    let path = d.path().join("m.db");
    let wf;
    let rec;
    {
        let mut m = DejaDB::open(path.to_str().unwrap()).unwrap();
        wf = plan(&mut m);
        rec = ran(&mut m, &wf, "build", 1_700_000_001_000);
    }
    let mut m = DejaDB::open(path.to_str().unwrap()).unwrap();
    assert_eq!(
        m.step_actions("ci", &wf, None, 100).unwrap(),
        vec![("build".to_string(), rec)]
    );
}

#[test]
fn empty_and_unknown_inputs_return_nothing_rather_than_erroring() {
    let (mut m, _d) = open_mem();
    let wf = plan(&mut m);

    assert!(m.step_actions("ci", &wf, None, 100).unwrap().is_empty());
    assert!(m.step_actions("nope", &wf, None, 100).unwrap().is_empty());
    assert!(m
        .step_actions("ci", &wf, Some("no-such-node"), 100)
        .unwrap()
        .is_empty());
}

#[test]
fn step_action_relation_parses_round_trip() {
    assert_eq!(step_action_relation("build"), "mg:step_action:build");
    assert_eq!(step_action_node("mg:step_action:build"), Some("build"));
    // A node id may itself contain a colon; only the prefix is structural.
    assert_eq!(step_action_node("mg:step_action:a:b"), Some("a:b"));
    // Not an execution record.
    assert_eq!(step_action_node("mg:knows"), None);
    // No node named — an execution record must say which step it ran.
    assert_eq!(step_action_node("mg:step_action:"), None);
}
