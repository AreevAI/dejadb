//! dejadb-server — the opt-in thin HTTP surface and
//! the local test console (`deja ui`).
//!
//! Deliberately minimal: std-only HTTP/1.1 on 127.0.0.1, one request per
//! connection, JSON API + one embedded HTML page. This is a *local
//! inspection console*, not a service — no auth, binds loopback only.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use dejadb_cal::{CalExecutor, CalExecutorConfig, CalStoreFacade, DejaDbFacade};
use dejadb_waiser::{now_ms, BorrowedSubstrate};
use serde_json::{json, Value};
use waiser::{Decision, Engine, ObserverType, RecStatus, RunOptions, ScopeSet};

const CONSOLE_HTML: &str = include_str!("console.html");

/// Per-connection read/write timeout — bounds slow-client (slowloris) attacks.
const READ_TIMEOUT_SECS: u64 = 15;
/// Hard ceiling on bytes read from one connection (1 MiB body cap + headroom
/// for the request line and headers). Bounds memory against oversized requests.
const MAX_CONN_BYTES: u64 = (1 << 20) + (128 << 10);
/// Cap on total header bytes and header count per request.
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADERS: usize = 100;
/// Total wall-clock deadline for reading + handling one request. A per-read
/// timeout cannot bound a slow-drip client (each byte resets it), so a watchdog
/// shuts the socket down at this deadline.
const REQUEST_DEADLINE_SECS: u64 = 30;

pub struct UiServer {
    facade: DejaDbFacade,
    executor: CalExecutor,
    db_label: String,
    /// Shared secret required for access when set. In hub mode it guards only
    /// mutating/sync endpoints (`Bearer`); in console-auth mode (`auth_all`) it
    /// guards every request via `Bearer` **or** HTTP `Basic` (password = token).
    token: Option<String>,
    /// When true, *every* request requires the token — the console page, all
    /// reads, and all writes — and a `401` carries `WWW-Authenticate: Basic` so
    /// browsers prompt. Set by [`UiServer::with_auth`]; hub mode leaves it off.
    auth_all: bool,
    /// Directory for received segments (dejad hub mode).
    segment_dir: Option<String>,
    /// When false (the default), reject any request whose `Host` header is not
    /// loopback — the standard DNS-rebinding defense for a localhost service.
    /// Set true only when the operator intentionally serves to other hosts
    /// (CLI `--allow-remote`), where a non-loopback `Host` is expected.
    allow_remote: bool,
}

impl UiServer {
    pub fn new(facade: DejaDbFacade, db_label: String) -> Self {
        UiServer {
            facade,
            executor: CalExecutor::new(CalExecutorConfig::default()),
            db_label,
            token: None,
            auth_all: false,
            segment_dir: None,
            allow_remote: false,
        }
    }

    /// Accept requests whose `Host` header is non-loopback. Off by default so
    /// the loopback console is protected against DNS-rebinding reads even when
    /// unauthenticated; the CLI enables it under `--allow-remote`.
    pub fn allow_remote(mut self, yes: bool) -> Self {
        self.allow_remote = yes;
        self
    }

    /// Require a shared secret on **every** request (the console page, reads,
    /// and writes). Browsers authenticate through the native `Basic` prompt
    /// (any username; password = `token`); scripts may send `Authorization:
    /// Bearer <token>`. Use this to serve the console to more than a single
    /// trusted local operator. Pair with a TLS-terminating proxy for non-local
    /// exposure — the token crosses the wire in the clear otherwise.
    pub fn with_auth(mut self, token: String) -> Self {
        self.token = Some(token);
        self.auth_all = true;
        self
    }

    /// Permit or forbid destructive CAL (`FORGET <hash>`) from the query
    /// console. Enabled by default; pass `false` to serve a read-only console.
    /// Since the plain console is unauthenticated, disabling this is the safe
    /// choice when the console is exposed beyond a trusted local operator.
    pub fn allow_destructive_ops(mut self, allow: bool) -> Self {
        self.executor = CalExecutor::new(CalExecutorConfig {
            allow_destructive_ops: allow,
            ..CalExecutorConfig::default()
        });
        self
    }

    /// dejad mode: require `Authorization: Bearer <token>` on POST
    /// endpoints and accept segment push/pull under `dir`.
    pub fn into_hub(mut self, token: Option<String>, dir: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        self.token = token;
        self.segment_dir = Some(dir.to_string());
        // The hub is a network service (segment push/pull from other nodes), so
        // non-loopback Hosts are expected — writes stay bearer-gated.
        self.allow_remote = true;
        Ok(self)
    }

    /// Bind and return the listener (lets callers learn the ephemeral port).
    pub fn bind(addr: &str) -> std::io::Result<TcpListener> {
        TcpListener::bind(addr)
    }

    /// Serve forever on an already-bound listener.
    pub fn serve(&self, listener: TcpListener) -> std::io::Result<()> {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    if let Err(e) = self.handle(s) {
                        eprintln!("deja ui: request error: {e}");
                    }
                }
                Err(e) => eprintln!("deja ui: accept error: {e}"),
            }
        }
        Ok(())
    }

    fn handle(&self, stream: TcpStream) -> std::io::Result<()> {
        // The server handles one connection at a time, so a single slow-drip
        // client could otherwise hold it open indefinitely (a per-read timeout
        // resets on every byte). A watchdog thread enforces a hard wall-clock
        // deadline by shutting the socket down; it is unparked and joined the
        // instant the request completes, so it adds no latency in the fast path.
        let watchdog_stream = stream.try_clone()?;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_done = std::sync::Arc::clone(&done);
        let watchdog = std::thread::spawn(move || {
            std::thread::park_timeout(Duration::from_secs(REQUEST_DEADLINE_SECS));
            if !watchdog_done.load(std::sync::atomic::Ordering::Acquire) {
                let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
            }
        });
        let result = self.handle_request(stream);
        done.store(true, std::sync::atomic::Ordering::Release);
        watchdog.thread().unpark();
        let _ = watchdog.join();
        result
    }

    fn handle_request(&self, stream: TcpStream) -> std::io::Result<()> {
        // Bound slow clients (slowloris) and total bytes read from the
        // connection so a malicious client cannot stall the server or exhaust
        // memory with an oversized request line / headers / body.
        stream.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        stream.set_write_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)))?;
        let mut reader = BufReader::new(stream.try_clone()?.take(MAX_CONN_BYTES));
        let mut request_line = String::new();
        reader.read_line(&mut request_line)?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();

        // headers → content-length + authorization + origin + host
        let mut content_length = 0usize;
        let mut bearer: Option<String> = None;
        let mut origin: Option<String> = None;
        let mut host: Option<String> = None;
        let mut header_bytes = 0usize;
        let mut header_count = 0usize;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // EOF / connection closed by peer
            }
            header_bytes += n;
            header_count += 1;
            if header_bytes > MAX_HEADER_BYTES || header_count > MAX_HEADERS {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "request headers too large",
                ));
            }
            let l = line.trim();
            if l.is_empty() {
                break;
            }
            let low = l.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if let Some(v) = low.strip_prefix("origin:") {
                origin = Some(v.trim().to_string());
            }
            if let Some(v) = low.strip_prefix("host:") {
                host = Some(v.trim().to_string());
            }
            if low.starts_with("authorization:") {
                if let Some((_, v)) = l.split_once(':') {
                    let v = v.trim();
                    // Accept `Bearer <token>` (scripts/CLI) or HTTP `Basic`
                    // (browsers, via the native login prompt). For Basic the
                    // credential is base64(user:pass) and the token is the
                    // password; the username is ignored.
                    bearer = if let Some(t) =
                        v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))
                    {
                        Some(t.trim().to_string())
                    } else if let Some(b64) =
                        v.strip_prefix("Basic ").or_else(|| v.strip_prefix("basic "))
                    {
                        basic_auth_password(b64.trim())
                    } else {
                        Some(v.to_string())
                    };
                }
            }
        }
        // DNS-rebinding defense: a browser tricked into pointing an attacker
        // domain at 127.0.0.1 sends that domain in the Host header. Unless the
        // operator opted into remote serving, reject any non-loopback Host on
        // *every* method (the Origin check below only covers POST, so this is
        // what stops a drive-by page from reading memory over GET). A missing
        // Host (bare HTTP/1.0, some CLI clients) passes, mirroring Origin.
        if !self.allow_remote && host.as_deref().is_some_and(|h| !host_is_local(h)) {
            let payload = br#"{"ok":false,"error":"non-loopback Host rejected (use --allow-remote to serve remotely)"}"#;
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            out.write_all(payload)?;
            return out.flush();
        }

        let mut body = vec![0u8; content_length.min(1 << 20)];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }

        // Cross-origin drive-by protection: browsers attach an Origin header
        // to cross-site requests. Only loopback pages (the console itself)
        // may mutate; curl/CLI clients send no Origin and pass through.
        if method == "POST" && origin.as_deref().is_some_and(|o| !origin_is_local(o)) {
            let payload = br#"{"ok":false,"error":"cross-origin request rejected"}"#;
            let mut out = stream;
            write!(
                out,
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            out.write_all(payload)?;
            return out.flush();
        }

        let (status, ctype, payload) = self.route(&method, &path, &body, bearer.as_deref());
        // On a console-auth 401, challenge with Basic so browsers show the
        // native login prompt (any username; password = token).
        let auth_challenge = if self.auth_all && status.starts_with("401") {
            "WWW-Authenticate: Basic realm=\"DejaDB console\", charset=\"UTF-8\"\r\n"
        } else {
            ""
        };
        let mut out = stream;
        write!(
            out,
            "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{auth_challenge}Content-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )?;
        out.write_all(&payload)?;
        out.flush()
    }

    fn route(&self, method: &str, path: &str, body: &[u8], bearer: Option<&str>) -> (&'static str, &'static str, Vec<u8>) {
        // Auth: console-auth mode (`auth_all`) guards every request; hub mode
        // guards only mutating + sync endpoints. The credential arrives as a
        // Bearer token or an HTTP Basic password (see `handle_request`).
        if let Some(tok) = &self.token {
            let guarded = self.auth_all || method == "POST" || path.starts_with("/api/segment");
            let authorized = bearer.is_some_and(|b| ct_eq(b.as_bytes(), tok.as_bytes()));
            if guarded && !authorized {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"authentication required"}"#.to_vec());
            }
        } else if method == "POST" {
            // §5.7: token-less `deja ui` is read-only. The ONLY POST allowed is
            // a read-only CAL statement; every write (any waiser mutation, an
            // ADD/SUPERSEDE/FORGET CAL batch, etc.) requires --token-env. This
            // closes the bypass where a local process could execute a
            // proposal's CAL directly and skip the review queue.
            let base = path.split_once('?').map(|(p, _)| p).unwrap_or(path);
            let allowed = base == "/api/cal" && cal_body_is_read_only(body);
            if !allowed {
                return ("401 Unauthorized", "application/json",
                        br#"{"ok":false,"error":"read-only console: restart deja ui with --token-env VAR to enable writes"}"#.to_vec());
            }
        }
        let (path, query) = match path.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path, ""),
        };
        let q = |key: &str| -> Option<String> {
            query.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=')?;
                (k == key).then(|| urldecode(v))
            })
        };
        match (method, path) {
            ("GET", "/") => (
                "200 OK",
                "text/html; charset=utf-8",
                CONSOLE_HTML
                    .replace("{{DB}}", &html_escape(&self.db_label))
                    .into_bytes(),
            ),
            ("POST", "/api/cal") => {
                let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
                let queryt = req.get("query").and_then(|v| v.as_str()).unwrap_or("");
                match self.executor.execute(queryt, &self.facade) {
                    Ok(res) => ok_json(json!({
                        "ok": true,
                        "result": res.result,
                        "warnings": res.warnings,
                        "statement": res.metadata.statement_type,
                        "elapsed_ms": res.metadata.execution_time_ms,
                    })),
                    Err(e) => {
                        // Structured error: code + span + hint let the console
                        // point at the offending token instead of just quoting.
                        let mut err = json!({
                            "ok": false,
                            "error": e.sanitize_for_client(),
                            "code": e.code(),
                        });
                        if let Some(sp) = e.span() {
                            err["span"] = json!({
                                "start": sp.start, "end": sp.end,
                                "line": sp.line, "col": sp.col,
                            });
                        }
                        if let Some(hint) = e.suggestion() {
                            err["suggestion"] = json!(hint);
                        }
                        ok_json(err)
                    }
                }
            }
            ("GET", "/api/stats") => {
                let stats = self.facade.with_store(|m| m.stats());
                match stats {
                    Ok(s) => ok_json(json!({
                        "db": self.db_label,
                        "grains": s.grains, "current": s.current,
                        "triples": s.triples, "terms": s.terms,
                        "ops": s.ops, "events_indexed": s.events_indexed,
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/log") => {
                let since: i64 = q("since").and_then(|v| v.parse().ok()).unwrap_or(0);
                let limit: usize = q("limit").and_then(|v| v.parse().ok()).unwrap_or(100);
                match self.facade.with_store(|m| m.changes_since(since, limit)) {
                    Ok(ops) => {
                        let rows: Vec<Value> = ops
                            .iter()
                            .map(|o| {
                                json!({
                                    "op_seq": o.op_seq, "hlc": o.hlc,
                                    "op": match o.op { 1 => "add", 2 => "supersede", 3 => "forget", _ => "?" },
                                    "hash": o.hash.to_hex(),
                                })
                            })
                            .collect();
                        ok_json(json!(rows))
                    }
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/config") => {
                // Read-only observability: the *effective* per-process
                // configuration. Nothing here is persisted in the .db —
                // the file holds data only; these values are supplied by
                // the host at open time.
                let cfg = self.executor.config();
                let (index_text, embedder_dim, declared_embed, mut warnings) =
                    self.facade.with_store(|m| {
                        (
                            m.index_text_enabled(),
                            m.embedder_dim(),
                            m.declared_embedding().map(|(mm, d)| (mm.to_string(), d)),
                            m.open_warnings().to_vec(),
                        )
                    });
                if let Some((m, d)) = &declared_embed {
                    if embedder_dim.is_none() {
                        warnings.push(format!(
                            "vector leg dormant: file expects {m}@{d}, no embedding backend installed"
                        ));
                    }
                }
                ok_json(json!({
                    "ok": true,
                    "db": self.db_label,
                    "warnings": warnings,
                    "file": {
                        "text_index": index_text,
                        "embedding": declared_embed.map(|(m, d)| json!({"model": m, "dim": d})),
                    },
                    "session": {
                        "namespace": self.facade.session_namespace(),
                        "mounts": self.facade.mount_aliases(),
                    },
                    "store": {
                        "index_text": index_text,
                        "embedder": embedder_dim.map(|d| json!({"dim": d})),
                    },
                    "recall": {
                        "fusion": "rrf",
                        "rrf_k0": dejadb_store::RRF_K0,
                        "overfetch_factor": DejaDbFacade::RECALL_OVERFETCH,
                        "legs": {
                            "structural": true,
                            "bm25": index_text,
                            "vector": embedder_dim.is_some(),
                        },
                    },
                    "executor": {
                        "max_limit": cfg.max_limit,
                        "default_limit": cfg.default_limit,
                        "tier1_writes": cfg.tier1_enabled,
                        "namespace_override": cfg.namespace_override,
                        "user_id_override": cfg.user_id_override,
                    },
                    "server": {
                        "hub_mode": self.segment_dir.is_some(),
                        "auth_required": self.token.is_some(),
                        // true = every request is authenticated (console-auth);
                        // false with auth_required = hub mode (writes/sync only).
                        "auth_all": self.auth_all,
                        "segment_dir": self.segment_dir,
                    },
                    "persistence": "per-process (host-supplied at open) — not stored in the .db",
                }))
            }
            ("GET", "/api/browse") => {
                // Browse-without-queries: the tail of the op-log joined with
                // grain summaries, newest first. Supersession and tombstone
                // status are resolved within the returned window so the
                // console can dim/strike them (grains are never mutated —
                // this reads the index + immutable blobs only).
                let limit: i64 = q("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(500)
                    .clamp(1, 2000);
                let built = self.facade.with_store(|m| -> Result<(i64, Vec<Value>), String> {
                    let total = m.stats().map_err(|e| e.to_string())?.ops as i64;
                    let after = (total - limit).max(0);
                    let ops = m.changes_since(after, limit as usize).map_err(|e| e.to_string())?;
                    let mut rows: Vec<Value> = Vec::with_capacity(ops.len());
                    let mut idx: std::collections::HashMap<String, usize> =
                        std::collections::HashMap::new();
                    for o in &ops {
                        let hex = o.hash.to_hex();
                        let op_name = match o.op { 1 => "add", 2 => "supersede", 3 => "forget", _ => "?" };
                        if o.op == 3 {
                            // tombstone: flag the earlier row if it is in the
                            // window, else emit a stub (blob is already gone).
                            match idx.get(&hex) {
                                Some(&i) => { rows[i]["forgotten"] = json!(true); }
                                None => rows.push(json!({
                                    "hash": hex, "op_seq": o.op_seq, "hlc": o.hlc,
                                    "op": op_name, "forgotten": true,
                                })),
                            }
                            continue;
                        }
                        // A SUPERSEDE logs two ops for the new grain (add +
                        // supersede) — merge them into one row, keeping the
                        // later op label.
                        if let Some(&i) = idx.get(&hex) {
                            rows[i]["op"] = json!(op_name);
                            rows[i]["op_seq"] = json!(o.op_seq);
                            rows[i]["hlc"] = json!(o.hlc);
                            continue;
                        }
                        let mut row = json!({
                            "hash": hex, "op_seq": o.op_seq, "hlc": o.hlc, "op": op_name,
                        });
                        match m.get(&o.hash) {
                            Ok(g) => {
                                row["type"] = json!(format!("{:?}", g.grain_type).to_lowercase());
                                row["fields"] = serde_json::to_value(&g.fields).unwrap_or(Value::Null);
                            }
                            // erased since (forget op outside the window)
                            Err(_) => { row["missing"] = json!(true); }
                        }
                        idx.insert(hex, rows.len());
                        rows.push(row);
                    }
                    // A supersede op's grain points at its predecessor via
                    // derived_from; mark the predecessor if it is in-window.
                    let marks: Vec<(String, String)> = rows.iter()
                        .filter(|r| r["op"] == "supersede")
                        .filter_map(|r| {
                            let old = r["fields"].get("derived_from")?.as_str()?;
                            Some((old.to_string(), r["hash"].as_str()?.to_string()))
                        })
                        .collect();
                    for (old, newer) in marks {
                        if let Some(&i) = idx.get(&old) {
                            rows[i]["superseded_by"] = json!(newer);
                        }
                    }
                    rows.reverse();
                    Ok((total, rows))
                });
                match built {
                    Ok((total, rows)) => ok_json(json!({"ok": true, "total_ops": total, "grains": rows})),
                    Err(e) => ok_json(json!({"ok": false, "error": e})),
                }
            }
            ("GET", "/api/grain") => {
                let hash = q("hash").unwrap_or_default();
                match dejadb_core::error::Hash::from_hex(&hash)
                    .and_then(|h| self.facade.get(&h))
                {
                    Ok(g) => ok_json(json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "fields": g.fields,
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/verify") => match self.facade.with_store(|m| m.verify()) {
                Ok(r) => ok_json(json!({
                    "integrity": r.integrity, "grains": r.grains,
                    "hash_mismatches": r.hash_mismatches, "undecodable": r.undecodable,
                })),
                Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
            },
            ("POST", "/api/segment") => {
                // push: body = raw MGB1 bundle; applied immediately + archived
                let Some(dir) = &self.segment_dir else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"not in hub mode"}"#.to_vec());
                };
                let name = q("name").unwrap_or_else(|| format!("push-{}.mgb", now_label()));
                let Some(safe) = safe_segment_name(&name) else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"invalid segment name"}"#.to_vec());
                };
                let path = format!("{dir}/{safe}");
                if std::fs::write(&path, body).is_err() {
                    return ("500 Internal Server Error", "application/json",
                            br#"{"ok":false,"error":"write failed"}"#.to_vec());
                }
                match self.facade.with_store(|m| m.import_bundle(&path)) {
                    Ok(st) => ok_json(json!({"ok": true, "applied": st.applied, "skipped": st.skipped, "stored": safe})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string()})),
                }
            }
            ("GET", "/api/segments") => {
                let Some(dir) = &self.segment_dir else {
                    return ok_json(json!([]));
                };
                let mut names: Vec<String> = std::fs::read_dir(dir)
                    .map(|d| d.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                        .filter(|n| n.ends_with(".mgb")).collect())
                    .unwrap_or_default();
                names.sort();
                ok_json(json!(names))
            }
            ("GET", "/api/segment") => {
                let Some(dir) = &self.segment_dir else {
                    return ("400 Bad Request", "application/json", b"{}".to_vec());
                };
                let name = q("name").unwrap_or_default();
                let Some(safe) = safe_segment_name(&name) else {
                    return ("400 Bad Request", "application/json",
                            br#"{"ok":false,"error":"invalid segment name"}"#.to_vec());
                };
                match std::fs::read(format!("{dir}/{safe}")) {
                    Ok(b) => ("200 OK", "application/octet-stream", b),
                    Err(_) => ("404 Not Found", "application/json",
                               br#"{"ok":false,"error":"no such segment"}"#.to_vec()),
                }
            }
            // ── Waiser API (§5.4) — GETs are reads (token-less OK); the POST
            //    mutations are guarded above (token-less → 401). ────────────
            ("GET", "/api/waiser/recommendations") => {
                let status = q("status").and_then(|s| status_from_str(&s));
                let sub = BorrowedSubstrate::new(&self.facade);
                match Engine::with_builtins().recommendations(&sub, status) {
                    Ok(recs) => ok_json(json!({
                        "ok": true,
                        "recommendations": recs.iter().map(rec_json).collect::<Vec<_>>(),
                    })),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/waiser/health") => {
                let sub = BorrowedSubstrate::new(&self.facade);
                match Engine::with_builtins().health(&sub, now_ms()) {
                    Ok(h) => ok_json(json!({"ok": true, "health": h})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/waiser/outcomes") => {
                let sub = BorrowedSubstrate::new(&self.facade);
                match Engine::with_builtins().outcomes(&sub) {
                    Ok(o) => ok_json(json!({"ok": true, "outcomes": o})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/waiser/analyzers") => {
                // Effective settings (manifest merged with the file-config), so
                // the Setup view renders accurate on/off state and floors.
                let sub = BorrowedSubstrate::new(&self.facade);
                match Engine::with_builtins().analyzer_settings(&sub) {
                    Ok(list) => ok_json(json!({"ok": true, "analyzers": list})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("GET", "/api/waiser/telemetry") => {
                // Recall-telemetry rollups for the Sessions view. A read — open
                // in read-only mode like the other waiser GETs.
                let mode = self.facade.with_store(|m| m.telemetry_mode());
                if mode == dejadb_store::TelemetryMode::Off {
                    ok_json(json!({"ok": true, "enabled": false}))
                } else {
                    let access = self.facade.with_store(|m| m.telemetry_access_stats(None));
                    let queries = self.facade.with_store(|m| m.telemetry_query_stats(None));
                    let budget = self.facade.with_store(|m| m.telemetry_budget_stats());
                    match (access, queries, budget) {
                        (Ok(mut a), Ok(mut q), Ok(b)) => {
                            // Most-recalled first; recurring-gap questions first.
                            a.sort_by_key(|x| std::cmp::Reverse(x.recall_count));
                            a.truncate(200);
                            q.sort_by_key(|x| std::cmp::Reverse(x.run_count));
                            q.truncate(200);
                            ok_json(json!({
                                "ok": true,
                                "enabled": true,
                                "mode": mode.as_str(),
                                "access": a.iter().map(|x| json!({
                                    "hash": x.hash, "recall_count": x.recall_count, "last_ms": x.last_ms,
                                })).collect::<Vec<_>>(),
                                "queries": q.iter().map(|x| json!({
                                    "sample": x.sample, "run_count": x.run_count,
                                    "empty_count": x.empty_count,
                                })).collect::<Vec<_>>(),
                                "budget": {
                                    "sample_count": b.sample_count,
                                    "overflow_count": b.overflow_count,
                                },
                            }))
                        }
                        _ => ok_json(json!({"ok": false, "error": "telemetry read failed"})),
                    }
                }
            }
            ("POST", "/api/waiser/run") => {
                let mut sub = BorrowedSubstrate::new(&self.facade);
                match Engine::with_builtins().run(&mut sub, &RunOptions::default(), now_ms()) {
                    Ok(res) => ok_json(json!({"ok": true, "run": res})),
                    Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
                }
            }
            ("POST", "/api/waiser/review") => self.waiser_review(body),
            ("POST", "/api/waiser/apply") => self.waiser_apply(body),
            ("POST", "/api/waiser/rollback") => self.waiser_rollback(body),
            ("POST", "/api/waiser/config") => self.waiser_config(body),
            _ => (
                "404 Not Found",
                "application/json",
                br#"{"ok":false,"error":"not found"}"#.to_vec(),
            ),
        }
    }

    /// The console is one principal (§5.7); authenticated requests hold all
    /// scopes (local root of trust), actor `user:console` unless overridden.
    fn waiser_review(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let actor = req.get("actor").and_then(Value::as_str).unwrap_or("user:console");
        let decision = if req.get("decision").and_then(Value::as_str) == Some("reject") {
            Decision::Reject
        } else {
            Decision::Approve
        };
        let mut sub = BorrowedSubstrate::new(&self.facade);
        match Engine::with_builtins().review(
            &mut sub, hash, decision, actor, ObserverType::Human, &ScopeSet::all(), because, now_ms(),
        ) {
            Ok(()) => ok_json(json!({"ok": true})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    fn waiser_apply(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let actor = req.get("actor").and_then(Value::as_str).unwrap_or("user:console");
        let allow_destructive = req.get("allow_destructive").and_then(Value::as_bool).unwrap_or(false);
        let mut sub = BorrowedSubstrate::new(&self.facade);
        match Engine::with_builtins().apply(
            &mut sub, hash, actor, ObserverType::Human, &ScopeSet::all(), because, allow_destructive, now_ms(),
        ) {
            Ok(applied) => ok_json(json!({"ok": true, "rollbackable": applied.rollbackable})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    fn waiser_rollback(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        let hash = req.get("hash").and_then(Value::as_str).unwrap_or("");
        let because = req.get("because").and_then(Value::as_str).unwrap_or("");
        let actor = req.get("actor").and_then(Value::as_str).unwrap_or("user:console");
        let mut sub = BorrowedSubstrate::new(&self.facade);
        match Engine::with_builtins().rollback(
            &mut sub, hash, actor, ObserverType::Human, &ScopeSet::all(), because, now_ms(),
        ) {
            Ok(()) => ok_json(json!({"ok": true})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }

    /// Edit one analyzer's file-config from the Setup view. The body is
    /// `{analyzer_id, enabled?, severity_floor?, clear_floor?, params?,
    /// namespaces?}`; absent fields are left unchanged. The console holds all
    /// scopes (local root of trust), so `Admin` is satisfied; an unknown
    /// analyzer or bad param is a structured `ok:false` (not a 500).
    fn waiser_config(&self, body: &[u8]) -> (&'static str, &'static str, Vec<u8>) {
        let id = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|v| v.get("analyzer_id").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        // The update reads the same body; the extra `analyzer_id` key is ignored.
        let update: waiser::AnalyzerConfigUpdate = serde_json::from_slice(body).unwrap_or_default();
        let mut sub = BorrowedSubstrate::new(&self.facade);
        match Engine::with_builtins().set_analyzer_config(&mut sub, &id, update, &ScopeSet::all()) {
            Ok(cfg) => ok_json(json!({"ok": true, "config": cfg})),
            Err(e) => ok_json(json!({"ok": false, "error": e.to_string(), "code": e.code()})),
        }
    }
}

/// True when a `host[:port]` (or `[ipv6]:port`) authority names a loopback
/// host. Shared by the Origin drive-by check and the Host-header
/// (DNS-rebinding) check.
fn host_is_local(host_port: &str) -> bool {
    let host = if let Some(h) = host_port.strip_prefix('[') {
        h.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// True when an Origin header names a loopback host (any port, http/https).
fn origin_is_local(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let host_port = rest.split('/').next().unwrap_or("");
    host_is_local(host_port)
}

fn now_label() -> String {
    format!("{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0))
}

fn ok_json(v: Value) -> (&'static str, &'static str, Vec<u8>) {
    ("200 OK", "application/json", v.to_string().into_bytes())
}

/// True when a `POST /api/cal` body is a read-only statement — checked by the
/// *leading* keyword only (CAL is one statement per query; BATCH and the write
/// verbs are treated as writes). Conservative and fail-closed: anything not
/// clearly a read requires a token.
fn cal_body_is_read_only(body: &[u8]) -> bool {
    let req: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let q = req.get("query").and_then(Value::as_str).unwrap_or("");
    let first = q
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "RECALL" | "ASSEMBLE" | "EXPLAIN" | "HISTORY" | "EXISTS" | "DESCRIBE" | "COUNT"
    )
}

fn status_from_str(s: &str) -> Option<RecStatus> {
    match s {
        "pending" => Some(RecStatus::Pending),
        "approved" => Some(RecStatus::Approved),
        "rejected" => Some(RecStatus::Rejected),
        "applied" => Some(RecStatus::Applied),
        "rolled_back" => Some(RecStatus::RolledBack),
        "expired" => Some(RecStatus::Expired),
        _ => None, // includes "all"
    }
}

fn rec_json(r: &waiser::Recommendation) -> Value {
    json!({
        "hash": r.hash,
        "status": r.status.as_str(),
        "severity": r.severity.as_str(),
        "analyzer": r.analyzer,
        "summary": r.summary.render(),
        "target_ref": r.target_ref,
        "destructive": r.destructive,
        "rollbackable": r.rollbackable,
        "evidence": r.evidence,
    })
}

fn urldecode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() + 1 && i + 2 < b.len() + 1 => {
                if i + 2 < b.len() + 1 && i + 2 < b.len() {
                    let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                    if let Ok(v) = u8::from_str_radix(hex, 16) {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                }
                out.push(b[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Constant-time byte comparison — avoids leaking the bearer token through
/// response timing. A length mismatch fails fast (token length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Decode standard base64 (RFC 4648), enough for HTTP Basic credentials.
/// Hand-rolled to keep the server dependency-free. Returns `None` on any
/// invalid character or truncated input.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        acc = (acc << 6) | val(c)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Extract the password from an HTTP Basic credential (`base64("user:pass")`).
/// The console ignores the username and treats the password as the token.
/// Returns `None` if the value is not valid base64/UTF-8 or has no `:`.
fn basic_auth_password(b64: &str) -> Option<String> {
    let text = String::from_utf8(base64_decode(b64)?).ok()?;
    text.split_once(':').map(|(_user, pass)| pass.to_string())
}

/// Sanitize a client-supplied segment name into a single safe filename.
/// Strips anything outside `[A-Za-z0-9-._]`, then requires the result to be
/// exactly one normal path component — rejecting empty names, `.`/`..`, and
/// path separators so a push/pull cannot escape the segment directory.
fn safe_segment_name(name: &str) -> Option<String> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        .collect();
    if safe.is_empty() || safe.len() > 128 {
        return None;
    }
    let mut comps = std::path::Path::new(&safe).components();
    match (comps.next(), comps.next()) {
        (Some(std::path::Component::Normal(c)), None) if c.to_str() == Some(safe.as_str()) => {
            Some(safe)
        }
        _ => None,
    }
}

#[cfg(test)]
mod waiser_route_tests {
    use super::UiServer;
    use dejadb_cal::DejaDbFacade;
    use dejadb_core::types::{Fact, Grain};
    use dejadb_store::DejaDB;

    fn server(auth: Option<&str>) -> UiServer {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let mut store = DejaDB::open(path.to_str().unwrap()).unwrap();
        // Two identical facts → a duplicate-consolidation recommendation.
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller")).unwrap();
        store.add(&Fact::new("acme", "tier", "Enterprise").namespace("caller")).unwrap();
        std::mem::forget(dir); // keep the file alive for the server's lifetime
        let facade = DejaDbFacade::with_session(store, Some("caller".into()), None);
        let s = UiServer::new(facade, "test".into());
        match auth {
            Some(t) => s.with_auth(t.to_string()),
            None => s,
        }
    }

    fn text(r: &(&str, &str, Vec<u8>)) -> String {
        String::from_utf8_lossy(&r.2).to_string()
    }

    #[test]
    fn token_less_console_is_read_only() {
        let s = server(None);
        // A waiser mutation (run) is a write → 401.
        assert!(s.route("POST", "/api/waiser/run", b"{}", None).0.starts_with("401"));
        // A write CAL → 401.
        let w = s.route("POST", "/api/cal", br#"{"query":"ADD fact SET subject=\"x\""}"#, None);
        assert!(w.0.starts_with("401"), "write CAL must be 401: {}", text(&w));
        // A read CAL → 200.
        let r = s.route("POST", "/api/cal", br#"{"query":"RECALL facts WHERE subject = \"acme\""}"#, None);
        assert!(r.0.starts_with("200"), "read CAL allowed: {}", text(&r));
        // Waiser reads stay open token-less.
        assert!(s.route("GET", "/api/waiser/recommendations", b"", None).0.starts_with("200"));
    }

    #[test]
    fn telemetry_endpoint_is_an_open_read() {
        let s = server(None);
        let r = s.route("GET", "/api/waiser/telemetry", b"", None);
        assert!(r.0.starts_with("200"), "telemetry read open: {}", text(&r));
        let body = text(&r);
        assert!(body.contains("\"ok\":true"), "{body}");
        // The test store opens bare (telemetry off) → enabled:false.
        assert!(body.contains("\"enabled\":false"), "{body}");
    }

    #[test]
    fn authenticated_run_review_apply_roundtrip() {
        let s = server(Some("tok"));
        let run = s.route("POST", "/api/waiser/run", b"{}", Some("tok"));
        assert!(run.0.starts_with("200"), "run: {}", text(&run));
        assert!(text(&run).contains("\"ran\""));

        let list = s.route("GET", "/api/waiser/recommendations?status=pending", b"", Some("tok"));
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let recs = v["recommendations"].as_array().unwrap();
        assert!(!recs.is_empty(), "at least one recommendation");
        let hash = recs[0]["hash"].as_str().unwrap().to_string();

        let rev = s.route(
            "POST",
            "/api/waiser/review",
            format!(r#"{{"hash":"{hash}","decision":"approve","because":"ok"}}"#).as_bytes(),
            Some("tok"),
        );
        assert!(text(&rev).contains("\"ok\":true"), "review: {}", text(&rev));

        let ap = s.route(
            "POST",
            "/api/waiser/apply",
            format!(r#"{{"hash":"{hash}","because":"go"}}"#).as_bytes(),
            Some("tok"),
        );
        assert!(text(&ap).contains("\"ok\":true"), "apply: {}", text(&ap));
    }

    #[test]
    fn config_edit_toggles_analyzer_via_console() {
        let s = server(Some("tok"));
        // Token-less write → 401 (guarded like every POST).
        assert!(s.route("POST", "/api/waiser/config", b"{}", None).0.starts_with("401"));

        // Read the analyzers; pick one that is on by default.
        let list = s.route("GET", "/api/waiser/analyzers", b"", Some("tok"));
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let id = v["analyzers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["enabled"] == true)
            .expect("some analyzer is on")["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Disable it via the console endpoint.
        let post = s.route(
            "POST",
            "/api/waiser/config",
            format!(r#"{{"analyzer_id":"{id}","enabled":false}}"#).as_bytes(),
            Some("tok"),
        );
        assert!(text(&post).contains("\"ok\":true"), "config: {}", text(&post));

        // It reads back disabled.
        let list2 = s.route("GET", "/api/waiser/analyzers", b"", Some("tok"));
        let v2: serde_json::Value = serde_json::from_slice(&list2.2).unwrap();
        let now = v2["analyzers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["id"] == id.as_str())
            .unwrap();
        assert_eq!(now["enabled"], false, "toggled off and persisted");
    }

    #[test]
    fn self_approval_surfaces_wsr_code() {
        let s = server(Some("tok"));
        s.route("POST", "/api/waiser/run", b"{}", Some("tok"));
        let list = s.route("GET", "/api/waiser/recommendations?status=pending", b"", Some("tok"));
        let v: serde_json::Value = serde_json::from_slice(&list.2).unwrap();
        let rec = &v["recommendations"].as_array().unwrap()[0];
        let hash = rec["hash"].as_str().unwrap();
        let analyzer = rec["analyzer"].as_str().unwrap();
        let creator = format!("engine:{analyzer}");
        // The engine actor approving its own proposal is blocked.
        let rev = s.route(
            "POST",
            "/api/waiser/review",
            format!(r#"{{"hash":"{hash}","decision":"approve","because":"self","actor":"{creator}"}}"#).as_bytes(),
            Some("tok"),
        );
        assert!(text(&rev).contains("WSR-E021"), "self-approval blocked: {}", text(&rev));
    }
}

#[cfg(test)]
mod security_tests {
    use super::{base64_decode, basic_auth_password, ct_eq, safe_segment_name};

    #[test]
    fn ct_eq_matches_only_equal() {
        assert!(ct_eq(b"secret-token", b"secret-token"));
        assert!(!ct_eq(b"secret-token", b"secret-toker"));
        assert!(!ct_eq(b"secret", b"secret-token")); // length mismatch
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn base64_decode_roundtrips_known_vectors() {
        // RFC 4648 test vectors (with and without padding).
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        // padding is optional for us
        assert_eq!(base64_decode("Zg").unwrap(), b"f");
        // invalid characters are rejected
        assert!(base64_decode("****").is_none());
    }

    #[test]
    fn basic_auth_extracts_password_ignoring_username() {
        // base64("deja:s3cret") and base64(":s3cret") both yield the token.
        assert_eq!(basic_auth_password("ZGVqYTpzM2NyZXQ=").as_deref(), Some("s3cret"));
        assert_eq!(basic_auth_password("OnMzY3JldA==").as_deref(), Some("s3cret"));
        // a password may itself contain ':' — only the first ':' splits.
        // base64("u:a:b") -> "a:b"
        assert_eq!(basic_auth_password("dTphOmI=").as_deref(), Some("a:b"));
        // no ':' at all → not a valid Basic credential
        assert_eq!(basic_auth_password("bm9jb2xvbg=="), None); // "nocolon"
        assert_eq!(basic_auth_password("****"), None); // not base64
    }

    #[test]
    fn safe_segment_name_blocks_traversal() {
        assert_eq!(safe_segment_name("push-123.mgb").as_deref(), Some("push-123.mgb"));
        assert_eq!(safe_segment_name(".."), None);
        assert_eq!(safe_segment_name("."), None);
        assert_eq!(safe_segment_name(""), None);
        // Separators are stripped, so any accepted name is a single component
        // that cannot escape the segment directory.
        for probe in ["../../etc/passwd", "/etc/passwd", "a/../b", "foo/bar"] {
            if let Some(s) = safe_segment_name(probe) {
                assert!(!s.contains('/') && s != ".." && s != ".", "unsafe: {s}");
            }
        }
    }
}
