//! M3 exit test: an MCP client round-trips memory with zero adapter code.
//!
//! Drives `deja serve --mcp` over real stdio with a scripted JSON-RPC
//! session (all requests written up front, stdin closed, responses parsed).

use std::io::Write;
use std::process::{Command, Stdio};
use tempfile::TempDir;

fn rpc(id: u64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

#[test]
fn mcp_round_trip() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("m.db");
    let db = db.to_str().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["serve", "--mcp", "--db", db, "--ns", "caller"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let script = [
        rpc(1, "initialize", serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}})),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        rpc(2, "tools/list", serde_json::json!({})),
        rpc(3, "tools/call", serde_json::json!({"name": "dejadb_add", "arguments": {
            "fields": {"subject": "alice", "relation": "prefers", "object": "tea", "confidence": 0.95}}})),
        rpc(4, "tools/call", serde_json::json!({"name": "dejadb_recall", "arguments": {"subject": "alice"}})),
        rpc(5, "tools/call", serde_json::json!({"name": "dejadb_remember", "arguments": {
            "content": "caller asked about refunds", "session_id": "call-1", "role": "user"}})),
        rpc(6, "tools/call", serde_json::json!({"name": "dejadb_cal", "arguments": {
            "query": "RECALL facts WHERE subject = \"alice\" | COUNT"}})),
        rpc(7, "tools/call", serde_json::json!({"name": "dejadb_cal", "arguments": {
            "query": "DELETE sha256:abc"}})),
        rpc(8, "ping", serde_json::json!({})),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in &script {
            writeln!(stdin, "{line}").unwrap();
        }
    } // drop stdin → EOF → server exits

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // 8 requests (the notification gets no response)
    assert_eq!(lines.len(), 8, "one response per request");

    let by_id = |id: u64| lines.iter().find(|v| v["id"] == id).unwrap();

    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "dejadb");
    let tools = by_id(2)["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 6);

    // add returned a hash
    let add_text = by_id(3)["result"]["content"][0]["text"].as_str().unwrap();
    let add: serde_json::Value = serde_json::from_str(add_text).unwrap();
    assert_eq!(add["hash"].as_str().unwrap().len(), 64);
    assert_eq!(by_id(3)["result"]["isError"], false);

    // recall sees it (namespace defaulted from the session)
    let rec_text = by_id(4)["result"]["content"][0]["text"].as_str().unwrap();
    let rec: serde_json::Value = serde_json::from_str(rec_text).unwrap();
    assert_eq!(rec.as_array().unwrap().len(), 1);
    assert_eq!(rec[0]["fields"]["object"], "tea");

    // remember stored an event
    let rem_text = by_id(5)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(rem_text.contains("\"stored_as\":\"event\""));

    // CAL through MCP
    let cal_text = by_id(6)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(cal_text.contains("\"count\":1") || cal_text.contains("\"count\": 1"));

    // destructive CAL is a tool error, not a crash
    assert_eq!(by_id(7)["result"]["isError"], true);

    assert!(by_id(8)["result"].is_object());
}

/// `--lock-ns` pins the session: a caller-supplied `namespace` in tool
/// arguments/fields is ignored, so writes land in the locked namespace and an
/// agent cannot escape its partition. Here the add carries
/// `namespace: "attacker"`, but the fact must be recallable in the locked
/// namespace and absent from "attacker".
#[test]
fn mcp_lock_ns_pins_writes() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("v.db");
    let db = db.to_str().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["serve", "--mcp", "--db", db, "--lock-ns", "vault"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let script = [
        rpc(1, "initialize", serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}})),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        // The caller tries to write into "attacker" — the lock must ignore it.
        rpc(2, "tools/call", serde_json::json!({"name": "dejadb_add", "arguments": {
            "namespace": "attacker",
            "fields": {"subject": "x", "relation": "r", "object": "o", "namespace": "attacker"}}})),
        // A CAL recall naming "attacker" must be overridden to the locked ns.
        rpc(3, "tools/call", serde_json::json!({"name": "dejadb_cal", "arguments": {
            "query": "RECALL facts WHERE namespace = \"attacker\" AND subject = \"x\" | COUNT"}})),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in &script {
            writeln!(stdin, "{line}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let by_id = |id: u64| lines.iter().find(|v| v["id"] == id).unwrap();

    // The write succeeded; the CAL count sees it because the WHERE namespace
    // was overridden to "vault" (where the write actually landed).
    let cal_text = by_id(3)["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        cal_text.contains("\"count\":1") || cal_text.contains("\"count\": 1"),
        "locked CAL should find the fact in the overridden namespace: {cal_text}"
    );

    // Cross-check with a fresh, unlocked reader: the fact is in "vault", not
    // "attacker" — proving the caller's namespace was ignored on the write.
    let recall = |ns: &str| -> usize {
        let out = Command::new(env!("CARGO_BIN_EXE_deja"))
            .args(["cal", "RECALL facts WHERE subject = \"x\" | COUNT", "--db", db, "--ns", ns])
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        v["count"].as_u64().unwrap_or(0) as usize
    };
    assert_eq!(recall("vault"), 1, "write landed in the locked namespace");
    assert_eq!(recall("attacker"), 0, "caller-supplied namespace was ignored");
}

/// The `dejadb_forget` tool is gated by `--no-destructive-ops`. By default the
/// request reaches the store (a bogus hash → "grain not found"); with the flag
/// it is refused before touching the store ("disabled"). Both are `isError`
/// tool results, never JSON-RPC protocol errors.
#[test]
fn mcp_forget_gate() {
    let bogus = "0".repeat(64);

    let run = |extra: &[&str]| -> String {
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("g.db");
        let db = db.to_str().unwrap();
        let mut args = vec!["serve", "--mcp", "--db", db, "--ns", "caller"];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let script = [
            rpc(
                1,
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
            ),
            rpc(
                2,
                "tools/call",
                serde_json::json!({
                    "name": "dejadb_forget", "arguments": {"hash": bogus.as_str()}}),
            ),
        ];
        {
            let stdin = child.stdin.as_mut().unwrap();
            for line in &script {
                writeln!(stdin, "{line}").unwrap();
            }
        }
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let r = lines.iter().find(|v| v["id"] == 2).unwrap();
        assert_eq!(r["result"]["isError"], true, "forget is always a tool error here");
        r["result"]["content"][0]["text"].as_str().unwrap().to_string()
    };

    // Default: destructive ops permitted → the request reaches the store.
    let allowed = run(&[]);
    assert!(
        allowed.contains("not found"),
        "gate should be open by default, got: {allowed}"
    );
    assert!(!allowed.contains("disabled"), "gate must be open by default: {allowed}");

    // Opt-out: destructive ops refused before touching the store.
    let blocked = run(&["--no-destructive-ops"]);
    assert!(
        blocked.contains("disabled"),
        "gate should be closed with --no-destructive-ops, got: {blocked}"
    );
}

/// `--mount alias=path` adds a read-only file; a single `dejadb_cal` ASSEMBLE
/// then spans the primary (writable) memory and the mounted org memory.
#[test]
fn mcp_mount_cross_file() {
    let dir = TempDir::new().unwrap();
    let user = dir.path().join("user.db");
    let user = user.to_str().unwrap();
    let org = dir.path().join("org.db");
    let org = org.to_str().unwrap();

    // Populate both files via the binary. The org fact lives in namespace
    // "policies"; through the mount alias "org" it is reached as "org.policies".
    let add = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_deja")).args(args).output().unwrap();
        assert!(out.status.success(), "add failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    add(&["add", "refunds", "window_days", "45", "-d", org, "--ns", "policies"]);
    add(&["add", "john", "plan", "enterprise", "-d", user, "--ns", "caller"]);

    let mut child = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["serve", "--mcp", "--db", user, "--ns", "caller", "--mount", &format!("org={org}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let assemble = r#"ASSEMBLE "prompt" FROM policies: (RECALL facts WHERE namespace = "org.policies" AND subject = "refunds"), profile: (RECALL facts WHERE subject = "john")"#;
    let script = [
        rpc(1, "initialize", serde_json::json!({
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}})),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        rpc(2, "tools/call", serde_json::json!({"name": "dejadb_cal", "arguments": {"query": assemble}})),
    ];
    {
        let stdin = child.stdin.as_mut().unwrap();
        for line in &script {
            writeln!(stdin, "{line}").unwrap();
        }
    }

    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let resp: serde_json::Value = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .find(|v| v["id"] == 2)
        .unwrap();
    assert_eq!(resp["result"]["isError"], false, "cal errored: {resp}");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    // Cross-file: the mounted org fact and the primary user fact are both here.
    assert!(text.contains("45"), "org (mounted) fact missing: {text}");
    assert!(text.contains("enterprise"), "user (primary) fact missing: {text}");
}

/// `serve` refuses to run without an explicit memory file: it must not silently
/// fall back to the personal default db (`~/.dejadb/default.db`) and serve the
/// wrong file. `--db`/`-d` or `$DEJADB_DB` is required.
#[test]
fn serve_requires_explicit_db() {
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(["serve", "--mcp"])
        .env_remove("DEJADB_DB")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!out.status.success(), "serve without --db must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("explicit memory file"),
        "expected an explicit-db error, got: {err}"
    );
}
