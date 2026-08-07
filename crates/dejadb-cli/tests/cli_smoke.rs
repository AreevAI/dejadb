//! CLI smoke test — the M2 exit flow end-to-end through the binary:
//! add → recall → cal → bundle → import into a second memory → verify.

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn deja(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(args)
        .output()
        .expect("spawn deja");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn cli_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db_a = dir.path().join("a.db");
    let db_a = db_a.to_str().unwrap();
    let db_b = dir.path().join("b.db");
    let db_b = db_b.to_str().unwrap();
    let bundle = dir.path().join("delta.mgb");
    let bundle = bundle.to_str().unwrap();

    // add
    let (ok, hash, err) = deja(&[
        "add", "--db", db_a, "--ns", "caller", "--subject", "alice", "--relation", "prefers",
        "--object", "tea",
    ]);
    assert!(ok, "add failed: {err}");
    let hash = hash.trim().to_string();
    assert_eq!(hash.len(), 64);

    // recall
    let (ok, out, err) = deja(&[
        "recall", "--db", db_a, "--ns", "caller", "--subject", "alice",
    ]);
    assert!(ok, "recall failed: {err}");
    assert!(out.contains("\"object\":\"tea\"") || out.contains("\"object\": \"tea\""), "{out}");

    // cal — the language from the shell (ADD tier + read tier)
    let (ok, _out, err) = deja(&[
        "cal",
        r#"ADD fact SET subject = "alice" SET relation = "speaks" SET object = "German" SET namespace = "caller" REASON "cli""#,
        "--db", db_a, "--ns", "caller",
    ]);
    assert!(ok, "cal add failed: {err}");
    let (ok, out, err) = deja(&[
        "cal", r#"RECALL facts WHERE subject = "alice" | COUNT"#, "--db", db_a, "--ns", "caller",
    ]);
    assert!(ok, "cal count failed: {err}");
    assert!(out.contains("\"count\": 2") || out.contains("\"count\":2"), "{out}");

    // get by hash
    let (ok, out, _) = deja(&["get", &hash, "--db", db_a]);
    assert!(ok);
    assert!(out.contains("prefers"));

    // bundle → import into fresh memory → recall parity
    let (ok, out, err) = deja(&["bundle", "--db", db_a, "--out", bundle]);
    assert!(ok, "bundle failed: {err}");
    assert!(out.contains("bundled"));
    let (ok, out, err) = deja(&["import", "--db", db_b, "--bundle", bundle]);
    assert!(ok, "import failed: {err}");
    assert!(out.contains("applied"));
    let (ok, out, _) = deja(&[
        "recall", "--db", db_b, "--ns", "caller", "--subject", "alice",
    ]);
    assert!(ok);
    assert!(out.lines().count() == 2, "replica should hold both facts: {out}");

    // verify + stats + log on the replica
    let (ok, out, err) = deja(&["verify", "--db", db_b]);
    assert!(ok, "verify failed: {err}\n{out}");
    assert!(out.contains("integrity: ok"));
    let (ok, out, _) = deja(&["stats", "--db", db_b]);
    assert!(ok);
    assert!(out.contains("grains: 2"));
    let (ok, out, _) = deja(&["log", "--db", db_b]);
    assert!(ok);
    assert_eq!(out.lines().count(), 2);

    // destructive CAL statement fails through the CLI too
    let (ok, _, err) = deja(&["cal", "DELETE sha256:abc", "--db", db_a]);
    assert!(!ok, "DELETE must fail, got success");
    assert!(!err.is_empty());
}

/// Ergonomics: positional `add <s> <r> <o>` + `-d`, and recall resolving the
/// memory file from `$DEJADB_DB` when no --db/-d is given.
#[test]
fn version_flag_prints_crate_version() {
    for arg in ["--version", "-V", "version"] {
        let (ok, out, _) = deja(&[arg]);
        assert!(ok, "`deja {arg}` should exit 0");
        assert_eq!(
            out.trim(),
            format!("deja {}", env!("CARGO_PKG_VERSION")),
            "`deja {arg}` prints the crate version"
        );
    }
}

#[test]
fn capture_stop_keeps_tool_outcomes() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("code.db");
    let db = db.to_str().unwrap();

    // A Claude Code transcript where the load-bearing signal is a failing
    // tool result, not the final prose. The old capture kept only text blocks
    // and would have dropped it.
    let transcript = dir.path().join("t.jsonl");
    std::fs::write(
        &transcript,
        [
            r#"{"message":{"role":"user","content":[{"type":"text","text":"fix the flaky test"}]}}"#,
            r#"{"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test flaky"}}]}}"#,
            r#"{"message":{"role":"user","content":[{"type":"tool_result","is_error":true,"content":"assertion failed: shared tempdir race"}]}}"#,
            r#"{"message":{"role":"assistant","content":[{"type":"text","text":"Root cause: tests share a tempdir."}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    let hook = serde_json::json!({
        "session_id": "sess-1",
        "transcript_path": transcript.to_str().unwrap(),
    })
    .to_string();

    let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["capture-stop", "--db", db, "--ns", "code"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn capture-stop");
    child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "capture-stop failed");

    // The captured events must carry the tool outcome, flagged as an error.
    let recall = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["cal", "RECALL events RECENT 10", "--db", db, "--ns", "code"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&recall.stdout);
    assert!(text.contains("tool_result ERROR"), "tool error signal missing: {text}");
    assert!(text.contains("shared tempdir race"), "tool output body missing: {text}");
}

/// `recall-hook --with-waiser` closes the loop: pending recommendations ride
/// into the injected context (compact, capped) instead of waiting to be
/// polled; without the flag the hook stays memory-only.
#[test]
fn recall_hook_with_waiser_injects_pending_queue() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("w.db");
    let db = db.to_str().unwrap();

    let (ok, _, err) = deja(&["init", "--db", db, "--ns", "caller", "--template", "demo"]);
    assert!(ok, "init demo failed: {err}");
    let (ok, _, err) = deja(&["waiser", "run", "--db", db, "--ns", "caller"]);
    assert!(ok, "waiser run failed: {err}");

    let hook = serde_json::json!({ "prompt": "what do we know about acme" }).to_string();
    let run_hook = |extra: &[&str]| {
        let mut args = vec!["recall-hook", "--db", db, "--ns", "caller"];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn recall-hook");
        child.stdin.as_mut().unwrap().write_all(hook.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "recall-hook failed");
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let with = run_hook(&["--with-waiser"]);
    assert!(with.contains("pending recommendation"), "waiser block missing: {with}");
    assert!(with.contains("deja waiser list"), "review pointer missing: {with}");

    let without = run_hook(&[]);
    assert!(
        !without.contains("pending recommendation"),
        "flagless hook must stay memory-only: {without}"
    );
}

#[test]
fn cli_positional_and_env_db() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("p.db");
    let db = db.to_str().unwrap();

    // positional add with the -d short flag
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["add", "alice", "prefers", "tea", "-d", db])
        .output()
        .expect("spawn deja");
    assert!(out.status.success(), "positional add: {}", String::from_utf8_lossy(&out.stderr));

    // positional recall, memory file resolved from $DEJADB_DB (no --db/-d)
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .env("DEJADB_DB", db)
        .args(["recall", "alice"])
        .output()
        .expect("spawn deja");
    assert!(out.status.success(), "env-db recall failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"object\":\"tea\"") || stdout.contains("\"object\": \"tea\""),
        "{stdout}"
    );
}

/// Regression: a boolean flag (`--once`, `--mcp`, `--allow-remote`) must not
/// swallow a following `-d`. `deja serve --mcp -d mem.db` used to lose the
/// path to `--mcp` and silently fall back to the default memory file.
#[test]
fn boolean_flag_does_not_swallow_short_db_flag() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let seg = dir.path().join("seg");
    let seg = seg.to_str().unwrap();

    let (ok, _, err) = deja(&["add", "bob", "likes", "chess", "-d", db]);
    assert!(ok, "add failed: {err}");

    // --once is boolean; the -d after it must still name the memory file.
    let (ok, _out, err) = deja(&["stream", "--once", "-d", db, "--to", seg]);
    assert!(ok, "stream failed: {err}");
    assert!(
        !err.contains("using default memory"),
        "-d was swallowed by --once; deja fell back to the default file: {err}"
    );
    assert!(err.contains("shipped"), "expected ops shipped from {db}: {err}");
}

/// Regression (#16): opening an encrypted memory without the passphrase used
/// to fail with a bare STO-E001 "file is not a database". When a `<db>.kdf`
/// sidecar exists and no key was given, the error must say the file is
/// encrypted and point at `--passphrase-env`.
#[test]
fn encrypted_open_without_passphrase_hints_at_kdf_sidecar() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("enc.db");
    let db = db.to_str().unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["add", "alice", "likes", "tea", "-d", db, "--passphrase-env", "DEJA_TEST_PASS"])
        .env("DEJA_TEST_PASS", "correct horse")
        .output()
        .expect("spawn deja");
    assert!(
        out.status.success(),
        "encrypted add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Same file, no passphrase: must fail, and must say why.
    let (ok, _out, err) = deja(&["recall", "alice", "-d", db]);
    assert!(!ok, "recall without the passphrase unexpectedly succeeded");
    assert!(err.contains(".kdf exists"), "no encryption hint in: {err}");
    assert!(
        err.contains("--passphrase-env"),
        "no --passphrase-env pointer in: {err}"
    );
}

/// `deja hub` is the sync hub (`dejad`). Unlike the console it is a network
/// service by construction, so the shared key is mandatory and a non-loopback
/// bind must be opted into — both refusals happen before anything is served.
#[test]
fn hub_requires_a_key_and_guards_the_bind_address() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("hub.db");
    let db = db.to_str().unwrap();
    let seg = dir.path().join("segments");
    let seg = seg.to_str().unwrap();

    // no --token-env at all
    let (ok, _o, err) = deja(&["hub", "-d", db, "--dir", seg]);
    assert!(!ok, "hub without a token must refuse to start");
    assert!(err.contains("--token-env"), "no pointer to --token-env in: {err}");

    // named variable is not set
    let (ok, _o, err) = deja(&["hub", "-d", db, "--dir", seg, "--token-env", "DEJA_UNSET_VAR"]);
    assert!(!ok, "hub with an unset token variable must refuse to start");
    assert!(err.contains("is not set"), "unhelpful message: {err}");

    // set but empty
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["hub", "-d", db, "--dir", seg, "--token-env", "DEJA_EMPTY_VAR"])
        .env("DEJA_EMPTY_VAR", "   ")
        .output()
        .expect("spawn deja");
    assert!(!out.status.success(), "hub with an empty token must refuse to start");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("token is empty"),
        "unhelpful message: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // a real key, but bound off-loopback without --allow-remote
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args([
            "hub", "-d", db, "--dir", seg, "--token-env", "DEJA_HUB_TEST_KEY",
            "--addr", "0.0.0.0:0",
        ])
        .env("DEJA_HUB_TEST_KEY", "a-real-key")
        .output()
        .expect("spawn deja");
    assert!(!out.status.success(), "off-loopback hub must require --allow-remote");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("non-loopback") && err.contains("--allow-remote"),
        "bind guard message is unclear: {err}"
    );
}

/// The graph and as-of reads used to be reachable only by linking the crate.
#[test]
fn cli_graph_and_temporal_verbs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("g.db");
    let db = db.to_str().unwrap();

    for (s, o) in [("alice", "bob"), ("bob", "carol")] {
        let (ok, _, err) = deja(&[
            "add", "--db", db, "--ns", "org", "--subject", s, "--relation", "reports_to",
            "--object", o,
        ]);
        assert!(ok, "add failed: {err}");
    }

    // Forward walk, two hops.
    let (ok, out, err) = deja(&[
        "related", "--db", db, "--ns", "org", "--start", "alice", "--relations", "reports_to",
    ]);
    assert!(ok, "related failed: {err}");
    assert!(out.contains("bob") && out.contains("carol"), "{out}");

    // Reverse walk uses the OSP index; `reports_to` is entity-valued by default.
    let (ok, out, err) = deja(&[
        "related", "--db", db, "--ns", "org", "--start", "carol", "--relations", "reports_to",
        "--direction", "in",
    ]);
    assert!(ok, "reverse related failed: {err}");
    assert!(out.contains("alice"), "{out}");

    // A bad direction is refused rather than silently defaulting.
    let (ok, _, err) = deja(&[
        "related", "--db", db, "--ns", "org", "--start", "alice", "--relations", "reports_to",
        "--direction", "sideways",
    ]);
    assert!(!ok, "bad direction must fail");
    assert!(err.contains("out, in, both"), "{err}");

    // As-of read on the knowledge axis.
    let (ok, out, err) = deja(&[
        "entity-at", "--db", db, "--ns", "org", "--subject", "alice", "--relation", "reports_to",
        "--at", "4102444800000", "--axis", "knowledge",
    ]);
    assert!(ok, "entity-at failed: {err}");
    assert!(out.contains("bob"), "{out}");

    // An unknown entity answers, it does not error.
    let (ok, out, err) = deja(&[
        "entity-at", "--db", db, "--ns", "org", "--subject", "nobody", "--relation", "reports_to",
        "--at", "4102444800000",
    ]);
    assert!(ok, "entity-at on unknown subject failed: {err}");
    assert!(out.contains("nothing known"), "{out}");
}

#[test]
fn cli_step_actions_reads_workflow_execution_records() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("w.db");
    let db = db.to_str().unwrap();

    let (ok, out, err) = deja(&[
        "cal", "--db", db, "--ns", "ci",
        r#"ADD workflow "pipeline" build -> test REASON "smoke""#,
    ]);
    assert!(ok, "add workflow failed: {err}");
    let wf = out
        .split('"')
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .expect("a workflow hash in the CAL output")
        .to_string();

    // The plan exists and nothing has run against it yet.
    let (ok, out, err) = deja(&["step-actions", "--db", db, "--ns", "ci", "--workflow", &wf]);
    assert!(ok, "step-actions failed: {err}");
    assert!(out.contains("no execution records"), "{out}");

    let (ok, _, err) = deja(&[
        "step-actions", "--db", db, "--ns", "ci", "--workflow", "not-a-hash",
    ]);
    assert!(!ok, "a malformed hash must fail");
    assert!(!err.is_empty());
}

/// The join: run history and semantic memory queried across the seam.
#[test]
fn cli_run_join_verbs() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("j.db");
    let db = db.to_str().unwrap();

    // An event in a run, then a fact distilled from it.
    let (ok, ev, err) = deja(&[
        "remember", "--db", db, "--ns", "ops", "--content", "caller asked about refunds",
        "--session-id", "s1",
    ]);
    assert!(ok, "remember failed: {err}");
    let ev_hash = ev
        .split(|c: char| !c.is_ascii_hexdigit())
        .find(|t| t.len() == 64)
        .expect("an event hash")
        .to_string();

    let (ok, _, err) = deja(&[
        "cal", "--db", db, "--ns", "ops",
        &format!(
            r#"ADD fact SET subject = "refunds" SET relation = "window_days" SET object = "30" SET derived_from = "{ev_hash}" REASON "distilled""#
        ),
    ]);
    assert!(ok, "derived add failed: {err}");

    // Nothing carries a run_id here, so the run query answers empty — cleanly.
    let (ok, out, err) = deja(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "nope"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(out.contains("no grains recorded"), "{out}");

    // runs-touching walks provenance and reports honestly when no run made it.
    let (ok, out, err) = deja(&[
        "runs-touching", "--db", db, "--ns", "ops", "--hash", &ev_hash,
    ]);
    assert!(ok, "runs-touching failed: {err}");
    assert!(out.contains("no run produced") || !out.trim().is_empty(), "{out}");

    let (ok, _, err) = deja(&[
        "runs-touching", "--db", db, "--ns", "ops", "--hash", "not-a-hash",
    ]);
    assert!(!ok, "a malformed hash must fail");
    assert!(!err.is_empty());
}

/// `deja remember --run-id` closes the loop on the run/memory join: before it,
/// no CLI verb could put a grain in a run, so `run-trace` could only ever print
/// "no grains recorded" and the index behind it was unreachable from the shell.
#[test]
fn remember_run_id_is_readable_by_run_trace() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("runid.db");
    let db = db.to_str().unwrap();

    for text in ["caller asked about refunds", "agent quoted the 30-day policy"] {
        let (ok, _, err) = deja(&[
            "remember", "--db", db, "--ns", "ops", "--content", text, "--run-id", "run-a",
        ]);
        assert!(ok, "remember --run-id failed: {err}");
    }
    let (ok, _, err) = deja(&[
        "remember", "--db", db, "--ns", "ops", "--content", "different run", "--run-id", "run-b",
    ]);
    assert!(ok, "remember failed: {err}");

    let (ok, out, err) = deja(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "run-a"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(
        out.contains("recorded during run-a: 2 grain(s)"),
        "both of run-a's turns must appear, and only those: {out}"
    );

    let (ok, out_b, err) = deja(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "run-b"]);
    assert!(ok, "run-trace failed: {err}");
    assert!(
        out_b.contains("recorded during run-b: 1 grain(s)"),
        "runs must not bleed into each other: {out_b}"
    );

    // An unknown run still answers cleanly rather than erroring.
    let (ok, out, _) = deja(&["run-trace", "--db", db, "--ns", "ops", "--run-id", "nope"]);
    assert!(ok);
    assert!(out.contains("no grains recorded"), "{out}");
}
