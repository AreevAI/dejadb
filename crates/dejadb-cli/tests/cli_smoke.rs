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
