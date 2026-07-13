//! `deja migrate` / `deja reindex` through the real binary: mem0 export +
//! history land as searchable supersession chains, a Basic Memory vault
//! becomes live memory-tool files, re-runs are no-ops, and reindex makes an
//! `--index-text false` file searchable after the flip.

use std::process::Command;
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
fn mem0_migrate_end_to_end() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();
    let export = dir.path().join("export.json");
    let history = dir.path().join("history.json");
    std::fs::write(
        &export,
        r#"{"results": [
            {"id": "m-1", "memory": "Works at Initech", "user_id": "u7",
             "created_at": "2024-06-01T10:00:00Z"},
            {"id": "m-2", "memory": "Allergic to peanuts", "user_id": "u7",
             "created_at": "2024-04-01T09:00:00Z"}
        ]}"#,
    )
    .unwrap();
    std::fs::write(
        &history,
        r#"[
            {"memory_id": "m-1", "event": "ADD", "new_memory": "Works at Acme",
             "created_at": "2024-03-01T10:00:00Z"},
            {"memory_id": "m-1", "event": "UPDATE", "new_memory": "Works at Initech",
             "created_at": "2024-06-01T10:00:00Z"}
        ]"#,
    )
    .unwrap();

    let (ok, out, err) = deja(&[
        "migrate", "--from", "mem0", "--file", export.to_str().unwrap(),
        "--history", history.to_str().unwrap(), "--db", db, "--ns", "main",
    ]);
    assert!(ok, "migrate failed: {err}");
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 2, "chain root + m-2: {out}");
    assert_eq!(rep["superseded"], 1, "{out}");

    // history survived the migration as a supersession chain
    let (ok, out, err) = deja(&[
        "history", "--subject", "mem0/m-1", "--relation", "mem0_memory",
        "--db", db, "--ns", "main",
    ]);
    assert!(ok, "history failed: {err}");
    assert_eq!(out.trim().lines().count(), 2, "{out}");

    // prose is BM25-searchable through the hybrid search verb
    let (ok, out, err) = deja(&[
        "search", "--query", "peanuts", "--db", db, "--ns", "main",
    ]);
    assert!(ok, "search failed: {err}");
    assert!(out.contains("Allergic to peanuts"), "{out}");

    // re-run: everything skips, nothing duplicates
    let (ok, out, _) = deja(&[
        "migrate", "--from", "mem0", "--file", export.to_str().unwrap(),
        "--history", history.to_str().unwrap(), "--db", db, "--ns", "main",
    ]);
    assert!(ok);
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 0, "{out}");
}

#[test]
fn basic_memory_vault_becomes_memory_tool_files() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("bm.db");
    let db = db.to_str().unwrap();
    let vault = dir.path().join("vault");
    std::fs::create_dir_all(vault.join("recipes")).unwrap();
    std::fs::write(
        vault.join("recipes/espresso.md"),
        "---\ntitle: Espresso\npermalink: recipes/espresso\n---\n18g in, 36g out, 28 seconds.\n",
    )
    .unwrap();
    std::fs::write(vault.join("inbox.md"), "Call the venue about the offsite.\n").unwrap();

    let (ok, out, err) = deja(&[
        "migrate", "--from", "basic-memory", "--file", vault.to_str().unwrap(),
        "--db", db, "--ns", "main",
    ]);
    assert!(ok, "migrate failed: {err}");
    let rep: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(rep["added"], 2, "{out}");

    // imported notes are live for the Anthropic memory tool
    let (ok, out, err) = deja(&[
        "memtool", r#"{"command": "view", "path": "/memories"}"#, "--db", db, "--ns", "main",
    ]);
    assert!(ok, "memtool failed: {err}");
    assert!(out.contains("/memories/recipes/espresso"), "{out}");
    assert!(out.contains("/memories/inbox"), "{out}");

    let (ok, out, _) = deja(&[
        "memtool", r#"{"command": "view", "path": "/memories/recipes/espresso"}"#,
        "--db", db, "--ns", "main",
    ]);
    assert!(ok);
    assert!(out.contains("36g out"), "{out}");
}

#[test]
fn reindex_after_index_text_flip() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("edge.db");
    let db = db.to_str().unwrap();

    // voice/edge profile: write with text indexing off
    let (ok, _, err) = deja(&[
        "add", "alice", "prefers", "matcha", "--db", db, "--ns", "main",
        "--index-text", "false",
    ]);
    assert!(ok, "add failed: {err}");
    let (ok, out, _) = deja(&["search", "--query", "matcha", "--db", db, "--ns", "main"]);
    assert!(ok);
    assert!(!out.contains("matcha"), "BM25 leg should be off: {out}");

    // flip the declaration and rebuild in one command
    let (ok, out, err) = deja(&["reindex", "--db", db, "--ns", "main", "--index-text", "true"]);
    assert!(ok, "reindex failed: {err}");
    assert!(out.contains("1 rows backfilled"), "{out}");

    let (ok, out, _) = deja(&["search", "--query", "matcha", "--db", db, "--ns", "main"]);
    assert!(ok);
    assert!(out.contains("matcha"), "{out}");
}
