//! LR-13 memory-tool adapter (cookbook flows) + remember() seam tests.

use dejadb_store::memory_tool::MemoryTool;
use dejadb_store::{FactDraft, DejaDB};
use serde_json::json;
use tempfile::TempDir;

fn open_mem() -> (DejaDB, TempDir) {
    let dir = TempDir::new().unwrap();
    let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();
    (m, dir)
}

#[test]
fn memory_tool_cookbook_flow() {
    let (mut m, _d) = open_mem();
    let mut t = MemoryTool::new(&mut m, "agent");

    // create → view (numbered lines, cookbook format)
    t.execute(&json!({"command": "create", "path": "/memories/preferences.md",
        "file_text": "# Preferences\n- indent: 2 spaces\n- tests first"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("   1: # Preferences"), "{v}");
    assert!(v.contains("   3: - tests first"), "{v}");

    // directory listing
    t.execute(&json!({"command": "create", "path": "/memories/projects.md", "file_text": "dejadb"})).unwrap();
    let dir = t.execute(&json!({"command": "view", "path": "/memories"})).unwrap();
    assert!(dir.contains("/memories/preferences.md") && dir.contains("/memories/projects.md"), "{dir}");

    // str_replace requires uniqueness
    t.execute(&json!({"command": "str_replace", "path": "/memories/preferences.md",
        "old_str": "2 spaces", "new_str": "4 spaces"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("4 spaces"), "{v}");
    let err = t.execute(&json!({"command": "str_replace", "path": "/memories/preferences.md",
        "old_str": "nonexistent", "new_str": "x"}));
    assert!(err.is_err());

    // insert
    t.execute(&json!({"command": "insert", "path": "/memories/preferences.md",
        "insert_line": 1, "insert_text": "- never push to main"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/preferences.md"})).unwrap();
    assert!(v.contains("   2: - never push to main"), "{v}");

    // every edit was a supersession — history is real
    let hist = m.history("agent", "/memories/preferences.md", "memory_file").unwrap();
    assert_eq!(hist.len(), 3, "create + str_replace + insert = 3 versions");

    // rename keeps content, erases old chain, links provenance
    let mut t = MemoryTool::new(&mut m, "agent");
    t.execute(&json!({"command": "rename", "old_path": "/memories/projects.md",
        "new_path": "/memories/work.md"})).unwrap();
    let v = t.execute(&json!({"command": "view", "path": "/memories/work.md"})).unwrap();
    assert!(v.contains("dejadb"), "{v}");
    assert!(t.execute(&json!({"command": "view", "path": "/memories/projects.md"})).is_err());

    // delete erases the chain
    t.execute(&json!({"command": "delete", "path": "/memories/work.md"})).unwrap();
    assert!(t.execute(&json!({"command": "view", "path": "/memories/work.md"})).is_err());

    // path traversal is rejected
    assert!(t.execute(&json!({"command": "view", "path": "/etc/passwd"})).is_err());
    assert!(t.execute(&json!({"command": "view", "path": "/memories/../etc"})).is_err());
}

#[test]
fn memory_tool_persists_across_reopen() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("m.db");
    let path = path.to_str().unwrap();
    {
        let mut m = DejaDB::open(path).unwrap();
        let mut t = MemoryTool::new(&mut m, "agent");
        t.execute(&json!({"command": "create", "path": "/memories/notes.md", "file_text": "hello"})).unwrap();
    }
    let mut m = DejaDB::open(path).unwrap();
    let mut t = MemoryTool::new(&mut m, "agent");
    let v = t.execute(&json!({"command": "view", "path": "/memories/notes.md"})).unwrap();
    assert!(v.contains("hello"));
}

#[test]
fn remember_extracts_facts_with_provenance() {
    let (mut m, _d) = open_mem();
    let extractor = |content: &str| -> Vec<FactDraft> {
        assert!(content.contains("Berlin"));
        vec![
            FactDraft { subject: "alice".into(), relation: "lives_in".into(), object: "Berlin".into(), confidence: 0.9 },
            FactDraft { subject: "alice".into(), relation: "prefers".into(), object: "morning calls".into(), confidence: 0.7 },
        ]
    };
    let res = m
        .remember("caller", "Alice said she moved to Berlin and prefers morning calls", "extractor:test", Some(&extractor))
        .unwrap();
    assert_eq!(res.facts.len(), 2);

    // derived facts are recallable and provenance-linked to the observation
    let facts = m.recall("caller", "alice", None, 16).unwrap();
    assert_eq!(facts.len(), 2);
    for f in &facts {
        assert_eq!(f.get_str("derived_from"), Some(res.observation.to_hex().as_str()));
        assert_eq!(f.get_str("source_type"), Some("derived"));
    }
    // observation grain holds the raw content
    let obs = m.get(&res.observation).unwrap();
    assert!(obs.fields["context"]["content"].as_str().unwrap().contains("morning calls"));
}

#[test]
fn remember_without_extractor_stores_observation_only() {
    let (mut m, _d) = open_mem();
    let res = m.remember("caller", "raw note", "cli", None).unwrap();
    assert!(res.facts.is_empty());
    assert!(m.get(&res.observation).is_ok());
}
