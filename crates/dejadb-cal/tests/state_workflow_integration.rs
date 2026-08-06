//! End-to-end coverage for the State (0x03) and Workflow (0x04) grains.
//!
//! Both types were writable from CAL/MCP/bindings but had no test that crossed
//! the parser boundary — every existing workflow test asserted on the AST and
//! stopped. That gap hid a family of bugs: the State snapshot being overwritten
//! by the common `context` field, `context_data` accepted on the write path but
//! never read, workflow `name`/`status` landing somewhere the templates could
//! not see, and `* N` bounds silently overwriting each other on a shared
//! destination node. These tests drive real CAL text through a real store and
//! read the grains back.

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

/// Recall every grain of `plural` and return the first as JSON.
fn recall_one(ex: &CalExecutor, facade: &DejaDbFacade, plural: &str) -> serde_json::Value {
    let out = ex
        .execute(&format!("RECALL {plural} RECENT 10"), facade)
        .unwrap();
    match out.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 1, "expected exactly one {plural} grain");
            serde_json::to_value(&grains[0]).unwrap()
        }
        other => panic!("expected Grains for {plural}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Workflow (0x04)
// ---------------------------------------------------------------------------

#[test]
fn workflow_graph_survives_the_round_trip() {
    let (ex, facade, _d) = setup();

    let add = ex
        .execute(
            r#"ADD workflow "CI pipeline" ON "merge to main"
                 build -> test -> deploy
               BIND test = sha256:1111111111111111111111111111111111111111111111111111111111111111
               REASON "integration""#,
            &facade,
        )
        .unwrap();
    assert_eq!(added_hash(&add.result).len(), 64);

    let g = recall_one(&ex, &facade, "workflows");
    let f = &g["fields"];

    assert_eq!(f["trigger"], "merge to main");
    assert_eq!(
        f["nodes"],
        serde_json::json!(["build", "test", "deploy"]),
        "node order is load-bearing — it defines the entry point (OMS §8.4)"
    );

    // The topology itself must survive, not just its cardinality.
    let edges = f["edges"].as_array().expect("edges must round-trip");
    assert_eq!(edges.len(), 2, "got {edges:?}");
    assert_eq!(edges[0]["src"], "build");
    assert_eq!(edges[0]["dst"], "test");
    assert_eq!(edges[1]["src"], "test");
    assert_eq!(edges[1]["dst"], "deploy");

    assert_eq!(
        f["bindings"]["test"],
        "sha256:1111111111111111111111111111111111111111111111111111111111111111"
    );
}

#[test]
fn workflow_name_is_readable_at_top_level() {
    // `name` has no OMS §8.4 field, so it rides in `extra_fields` and serializes
    // verbatim at top level. It used to be routed into `common.context`, where
    // `templates::field_str` — which only ever reads top-level fields — could not
    // see it, leaving every shipped `{{name}}` template rendering blank.
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "nightly backup" a -> b REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(
        g["fields"]["name"], "nightly backup",
        "workflow name must be a top-level field, not buried in context"
    );
}

#[test]
fn workflow_conditional_edges_keep_their_conditions() {
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "triage" review -> escalate WHEN "severity > 3" REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    let edges = g["fields"]["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["cond"], "severity > 3");
}

#[test]
fn workflow_diamond_converges_with_a_single_retry_bound() {
    // `retries` is keyed by node, so when several edges share a destination the
    // executor must decide what a repeat means. Today the group syntax gives every
    // converging edge the *same* `* N`, so the fold is a no-op — the executor takes
    // the largest bound defensively, which is only observable if the grammar ever
    // grows a way to give converging edges different repeats (there is no
    // multi-chain syntax at present).
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "fanin" fetch -> (left, right) -> merge * 2 REASON "integration""#,
        &facade,
    )
    .unwrap();

    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(
        g["fields"]["edges"].as_array().unwrap().len(),
        4,
        "diamond expands to fetch->left, fetch->right, left->merge, right->merge"
    );
    assert_eq!(
        g["fields"]["retries"]["merge"], 2,
        "both converging edges carry * 2; the node keeps one bound"
    );
}

#[test]
fn workflow_repeat_attaches_to_the_target_node() {
    let (ex, facade, _d) = setup();

    ex.execute(
        r#"ADD workflow "retry" a -> c * 2 -> d REASON "integration""#,
        &facade,
    )
    .unwrap();
    let g = recall_one(&ex, &facade, "workflows");
    assert_eq!(g["fields"]["retries"]["c"], 2);
    assert!(
        g["fields"]["retries"].get("d").is_none(),
        "only the node the * N was attached to gets a bound"
    );
}

#[test]
fn workflow_rejects_edges_referencing_unknown_nodes() {
    // OMS §8.4 validation constraint: every `src`/`dst` MUST reference an element
    // of `nodes`. CAL's graph syntax makes a dangling edge unspellable, but the
    // JSON path (MCP / Python / Node all reach it) can express one, so the
    // validation has to live there.
    let (_ex, facade, _d) = setup();
    use dejadb_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "nodes": ["build", "test"],
        "edges": [{ "src": "build", "dst": "nowhere" }],
        "namespace": "caller",
    });
    let err = facade
        .cal_add("workflow", fields.as_object().unwrap())
        .expect_err("a dangling edge must be rejected, not silently stored");
    let msg = err.to_string();
    assert!(
        msg.contains("nowhere"),
        "error should name the offending node, got: {msg}"
    );
}

#[test]
fn workflow_rejects_duplicate_node_ids() {
    // OMS §8.4: node IDs MUST be unique within the grain — they are also the
    // keys for `bindings` and `retries`, so a duplicate makes those ambiguous.
    let (_ex, facade, _d) = setup();
    use dejadb_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "nodes": ["build", "build"],
        "namespace": "caller",
    });
    assert!(
        facade
            .cal_add("workflow", fields.as_object().unwrap())
            .is_err(),
        "duplicate node IDs must be rejected"
    );
}

// ---------------------------------------------------------------------------
// State (0x03)
// ---------------------------------------------------------------------------

#[test]
fn state_snapshot_round_trips_through_the_store() {
    // `context` is the OMS §8.3 field name and takes precedence over the `data` /
    // `context_data` aliases. (The common-context clobber this used to guard is
    // only reachable through the Rust API — see dejadb-core's
    // `state_context_is_not_clobbered_by_common_context`.)
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "planning_phase", "active_node": "build", "step": 3 },
        "namespace": "caller",
    });
    let hash = facade_add(&facade, "state", &fields);
    assert_eq!(hash.len(), 64);

    let g = recall_one(&ex, &facade, "states");
    let ctx = &g["fields"]["context"];
    assert_eq!(ctx["label"], "planning_phase");
    assert_eq!(ctx["active_node"], "build");
    assert_eq!(ctx["step"], 3);
}

#[test]
fn state_context_takes_precedence_over_data_alias() {
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "wins" },
        "data": { "label": "loses" },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(g["fields"]["context"]["label"], "wins");
}

#[test]
fn state_accepts_context_data_alias() {
    // `context_data` was listed as a known State field but the builder only ever
    // read `data`, so this stored an empty snapshot and dropped the payload.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context_data": { "label": "resumed", "cursor": 42 },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(g["fields"]["context"]["label"], "resumed");
    assert_eq!(g["fields"]["context"]["cursor"], 42);
}

#[test]
fn state_plan_and_history_round_trip() {
    // OMS §8.3 optional fields that had no struct field at all until now.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "label": "mid-run" },
        "plan": ["fetch", "transform", "load"],
        "history": [{ "node": "fetch", "ok": true }],
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    assert_eq!(
        g["fields"]["plan"],
        serde_json::json!(["fetch", "transform", "load"])
    );
    assert_eq!(g["fields"]["history"][0]["node"], "fetch");
}

#[test]
fn state_snapshot_keys_are_never_rewritten() {
    // A State's inner keys are user data. Applying the OMS FIELD_MAP to them
    // would rewrite any key colliding with a short code (`o` -> `object`),
    // corrupting the payload and destabilizing the content address on rewrite.
    let (ex, facade, _d) = setup();

    let fields = serde_json::json!({
        "context": { "o": "not-an-object-field", "s": 1, "rel": "keep-me" },
        "namespace": "caller",
    });
    facade_add(&facade, "state", &fields);

    let g = recall_one(&ex, &facade, "states");
    let ctx = &g["fields"]["context"];
    assert_eq!(ctx["o"], "not-an-object-field");
    assert_eq!(ctx["s"], 1);
    assert_eq!(ctx["rel"], "keep-me");
    assert!(ctx.get("object").is_none(), "inner keys must stay verbatim");
}

// ---------------------------------------------------------------------------
// Event.run_id — the only run-scoped correlation key in the grain model
// ---------------------------------------------------------------------------

#[test]
fn events_are_filterable_by_run_id() {
    // `run_id` has been serialized since 1.0 but was absent from the registry's
    // queryable_fields, so it was write-only: you could store it and never ask
    // for it back.
    let (ex, facade, _d) = setup();
    use dejadb_cal::facade::CalStoreFacade;

    for (run, body) in [
        ("run-a", "started build"),
        ("run-a", "build ok"),
        ("run-b", "started build"),
    ] {
        let fields = serde_json::json!({
            "content": body,
            "run_id": run,
            "namespace": "caller",
        });
        facade
            .cal_add("event", fields.as_object().unwrap())
            .unwrap();
    }

    let out = ex
        .execute(r#"RECALL events WHERE run_id = "run-a" RECENT 20"#, &facade)
        .unwrap();
    match out.result {
        CalResultPayload::Grains { grains, .. } => {
            assert_eq!(grains.len(), 2, "expected only run-a's events");
            for g in &grains {
                let v = serde_json::to_value(g).unwrap();
                assert_eq!(v["fields"]["run_id"], "run-a");
            }
        }
        other => panic!("expected Grains, got {other:?}"),
    }
}

#[test]
fn run_id_round_trips_under_its_oms_wire_key() {
    // The typed builder must set `Event.run_id` (wire key `rid`), not leave it to
    // `extra_fields` — which wrote it twice and never under its spec key.
    let (ex, facade, _d) = setup();
    use dejadb_cal::facade::CalStoreFacade;

    let fields = serde_json::json!({
        "content": "step complete",
        "run_id": "wf_1234",
        "namespace": "caller",
    });
    facade
        .cal_add("event", fields.as_object().unwrap())
        .unwrap();

    let g = recall_one(&ex, &facade, "events");
    assert_eq!(g["fields"]["run_id"], "wf_1234");
    assert!(
        g["fields"]["context"].get("run_id").is_none(),
        "run_id must not be duplicated into the common context map"
    );
}

// ---------------------------------------------------------------------------
// `add_via_set` is a shape fact, not a permission
// ---------------------------------------------------------------------------

#[test]
fn generic_add_set_rejects_types_it_cannot_shape() {
    // A Workflow is a graph and a State is an arbitrary JSON snapshot; neither
    // can be expressed as a flat list of `SET k = v` pairs, so the generic form
    // refuses them — with a message naming what it *can* build.
    let (ex, facade, _d) = setup();

    for (stmt, ty) in [
        (
            r#"ADD workflow SET nodes = "a" REASON "x""#,
            "workflow",
        ),
        (
            r#"ADD state SET data = "a" REASON "x""#,
            "state",
        ),
    ] {
        match ex.execute(stmt, &facade) {
            // Either the parser refuses the shape or the executor reports it.
            Err(_) => {}
            Ok(out) => match out.result {
                CalResultPayload::Unsupported { message, .. } => {
                    assert!(
                        message.contains(ty),
                        "message should name the rejected type, got: {message}"
                    );
                }
                other => panic!("expected rejection for `{stmt}`, got {other:?}"),
            },
        }
    }
}

#[test]
fn structured_paths_are_not_gated_by_add_via_set() {
    // The same two types are perfectly writable through their purpose-built
    // paths. This is the invariant the flag's old name and doc obscured: it
    // gates one CAL form, not the right to create the grain. Gating `cal_add`
    // on it would break remember(), the memory-tool adapter, the migrate
    // importers, and every host that checkpoints State.
    let (ex, facade, _d) = setup();
    use dejadb_cal::facade::CalStoreFacade;

    // Dedicated CAL statement.
    ex.execute(
        r#"ADD workflow "shaped" a -> b REASON "integration""#,
        &facade,
    )
    .unwrap();
    assert_eq!(recall_one(&ex, &facade, "workflows")["fields"]["name"], "shaped");

    // Per-type JSON builder — the surface MCP, Python and Node reach.
    let fields = serde_json::json!({ "data": { "label": "ok" }, "namespace": "caller" });
    facade
        .cal_add("state", fields.as_object().unwrap())
        .expect("cal_add must not be gated by add_via_set");
    assert_eq!(recall_one(&ex, &facade, "states")["fields"]["context"]["label"], "ok");
}

/// Write a grain through the facade's JSON add path (the surface MCP, Python and
/// Node all reach), returning the hex hash.
fn facade_add(facade: &DejaDbFacade, grain_type: &str, fields: &serde_json::Value) -> String {
    use dejadb_cal::facade::CalStoreFacade;
    let map = fields.as_object().unwrap().clone();
    let hash = facade.cal_add(grain_type, &map).unwrap();
    hex::encode(hash.as_bytes())
}
