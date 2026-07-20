//! deja — the DejaDB CLI (brand DejaDB, package dejadb, run `deja`).
//!
//! Thin shell over dejadb-store + dejadb-cal. One memory = one file.

use std::collections::HashMap;
use std::process::ExitCode;

use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_core::error::Hash;
use dejadb_core::types::{Fact, Grain, Tool};
use dejadb_store::DejaDB;
use dejadb_waiser::{now_ms, DejaDbSubstrate};
use waiser::{Decision, Engine, ObserverType, Policy, RecStatus, RunOptions, ScopeSet, Severity};

const USAGE: &str = "\
deja — embedded memory engine for AI agents (OMS + CAL on Turso)

USAGE:
  deja <command> [--db <memory.db>] [options]   (-d = --db)
  deja --version | -V                 print the version and exit
  deja help | --help | -h             show this help

COMMANDS:
  init     [--template blank|demo|coding-agent] [--ns NS]   seed a backend +
           print the Claude Code hook snippet (never writes your settings)
  waiser   <run|reflect|list|show|approve|reject|apply|rollback|analyzers|policy>
           the governed self-improvement loop (deterministic core; optional verified LLM):
           run    [--min-new N --min-new-errors N --if-stale 6h --format json --quiet]
                  [--model provider:name | --llm-cmd 'CMD']   optional LLM reflection
                  (--model reads the key from $ANTHROPIC_API_KEY/$OPENAI_API_KEY/etc.)
                  [--ground-model provider:name | --ground-cmd 'CMD']  separate
                  grounding backend (defaults to the reflection model)
                  [--analyzer-cmd 'CMD']   register an external analyzer
                  (advisory only — never auto-applies)
           reflect  like run, but re-analyzes the whole memory (ignores the
                  incremental watermark) — a full sweep; same flags as run
           list   [--status pending|all|applied|...] [--fail-on high]  (exit 2 on match)
           show <hash> | approve/reject/apply/rollback <hash> --because \"...\" [--actor A]
           outcomes  the Verify gate: did applied advice hold, or regress?
           [--policy FILE] grants auto-apply (else $WAISER_POLICY); `policy` prints it
  add      <subject> <relation> <object>       store a fact (positional)
           [--subject S --relation R --object O] [--ns NS] [--confidence C]
           [--idempotent]   no-op if this exact value is already the head
  recall   <subject> | --subject S   [--relation R] [--ns NS] [-k N]
           [--render sml|toon|markdown|plain|json] [--budget TOKENS]
  cal      <QUERY> [--ns NS]          execute a CAL statement
  search   --query TEXT [--subject S] [-k N]   hybrid recall (BM25 + structural, RRF)
  history  --subject S --relation R [--ns NS]
  provenance <source-hash>            grains distilled from a source (reverse)
  forks                               open forks (>1 head for a subject+relation)
  merge    --subject S --relation R --object O   close a fork with a resolved value
  novelty  --text T [--subject S] [--relation R] [-k N]   nearest existing grains
                                      (paraphrase check; needs --embed-cmd)
  log      [--since OP] [--limit N]   op-log (change feed)
  bundle   --out FILE [--since OP]    incremental backup (git-shaped)
  import   --bundle FILE              apply a bundle (fast-forward)
  migrate  --from SRC --file PATH [--history PATH]   import another system's
           export: mem0 | mem0-history | langgraph | letta | letta-archival |
           zep | basic-memory (PATH = vault dir) | jsonl (generic
           {subject,relation,object}|{content} lines) | tool-log (OpenAI-style
           tool-call JSONL → Tool grains, feeds tool-failure clustering).
           Re-runs skip what is already imported; see docs/migrate.md.
  reindex                             backfill + rebuild the BM25 text index
                                      (e.g. after --index-text true on a file
                                      written with indexing off)
  stream   --to DIR [--interval-ms N] [--once]   continuous op-log shipping
  restore  --from DIR [--until-hlc T]            rebuild from stream dir (PITR)
  follow   --from DIR [--interval-ms N] [--once]  subscribe: apply new segments
                                                  (org/category distribution)
  verify                              integrity + content-address recheck
  stats                               store counters
  serve    --mcp [--ns NS] [--mount alias=path,...] [--no-destructive-ops] [--lock-ns NS]  MCP server on stdio
                                      (--mount adds read-only files for
                                       cross-file ASSEMBLE; ns \"alias.inner\")
  repl     [--ns NS]                  interactive CAL console in the terminal
  remember --content TEXT [--facts JSON] [--observer ID]
  hook     claude-code               print settings.json hook snippet
                                      (auto recall-before-prompt + capture-on-stop)
  memtool  '<COMMAND-JSON>'           Anthropic memory-tool ops on grains
  ui       [--addr HOST:PORT] [--allow-remote] [--token-env VAR] [--no-destructive-ops]  web console (default 127.0.0.1:7437)

Namespace defaults to \"shared\". Exit code 0 on success.
--db is optional for one-shot commands: it falls back to $DEJADB_DB, then
~/.dejadb/default.db. `serve` and `ui` require an explicit --db (or $DEJADB_DB).
Files carry their own settings (text index, entity relations, embedding
provenance) in an internal meta table; a bare open honors them.
--index-text true|false explicitly re-stamps the file's declaration.

Encryption at rest: add --passphrase-env <VAR> to any command to derive an
AES-256 key (Argon2id) from the passphrase in environment variable VAR. The
non-secret salt is kept in a <memory.db>.kdf sidecar — back it up with the db.

Vector recall: add --embed-cmd 'CMD' [--embed-model NAME] to any command to
install a command embedder — CMD gets the text on stdin and must print a JSON
array of numbers. Turns on the vector leg for search/serve, and embeds grains
written by add/remember/migrate.";

fn flag(args: &HashMap<String, String>, k: &str) -> Option<String> {
    args.get(k).cloned()
}

/// Map a short flag to its long name. Terse aliases for what you type on every
/// invocation: `-d` for `--db`, `-k` for the recall/search limit.
fn short_flag(a: &str) -> Option<&'static str> {
    match a {
        "-d" => Some("db"),
        "-k" => Some("k"),
        _ => None,
    }
}

fn need(args: &HashMap<String, String>, k: &str) -> Result<String, String> {
    flag(args, k).ok_or_else(|| format!("missing required --{k}"))
}

/// Resolve the memory file: `--db`/`-d`, else `$DEJADB_DB`, else the default
/// personal memory `~/.dejadb/default.db` (its parent dir is created, and a
/// one-line note prints to stderr so the file stays discoverable).
///
/// When `require_explicit` is set, the default fallback is refused with an
/// error instead. Long-lived, potentially network-exposed commands (`serve`,
/// `ui`) pass this: silently serving the personal default memory — over MCP or
/// an unauthenticated HTTP console — risks exposing or mutating the wrong file,
/// so those commands must name their memory explicitly.
fn resolve_db(args: &HashMap<String, String>, require_explicit: bool) -> Result<String, String> {
    if let Some(p) = flag(args, "db") {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("DEJADB_DB") {
        if !p.trim().is_empty() {
            return Ok(p);
        }
    }
    if require_explicit {
        return Err(
            "this command needs an explicit memory file — pass --db <file> (or -d), \
             or set $DEJADB_DB. It will not fall back to the personal default memory \
             (~/.dejadb/default.db), to avoid serving the wrong file."
                .to_string(),
        );
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map_err(|_| "no --db given, and neither $DEJADB_DB nor $HOME is set".to_string())?;
    let path = format!("{home}/.dejadb/default.db");
    if let Some(parent) = std::path::Path::new(&path).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    eprintln!("deja: using default memory {path} (override with -d/--db or $DEJADB_DB)");
    Ok(path)
}

/// True when a `host:port` (or bare host) names a loopback interface. Used to
/// refuse binding the unauthenticated `ui` console to a routable address.
fn addr_is_loopback(addr: &str) -> bool {
    let a = addr.trim();
    // IPv6 bracketed form: [::1]:port
    if let Some(rest) = a.strip_prefix('[') {
        let host = rest.split(']').next().unwrap_or("");
        return host == "::1" || host.eq_ignore_ascii_case("localhost");
    }
    let host = a.rsplit_once(':').map(|(h, _)| h).unwrap_or(a);
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // An unqualified hostname or 0.0.0.0 is not provably loopback → treat as remote.
        Err(_) => false,
    }
}

fn parse_args(rest: &[String]) -> (HashMap<String, String>, Vec<String>) {
    let mut flags = HashMap::new();
    let mut positional = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if let Some(name) = a.strip_prefix("--") {
            // A value is the next token only if it is not itself a flag. Reject
            // any leading '-' (not just "--"), so a valueless long flag doesn't
            // swallow a following short flag as its value — e.g. `serve --mcp -d
            // db` must parse `-d` as --db, not as the value of `--mcp`. This
            // matches the short-flag branch below. Flag values in this CLI never
            // start with '-'.
            if i + 1 < rest.len() && !rest[i + 1].starts_with('-') {
                flags.insert(name.to_string(), rest[i + 1].clone());
                i += 2;
            } else {
                flags.insert(name.to_string(), "true".to_string());
                i += 1;
            }
        } else if let Some(long) = short_flag(a) {
            if i + 1 < rest.len() && !rest[i + 1].starts_with('-') {
                flags.insert(long.to_string(), rest[i + 1].clone());
                i += 2;
            } else {
                flags.insert(long.to_string(), "true".to_string());
                i += 1;
            }
        } else {
            positional.push(a.clone());
            i += 1;
        }
    }
    (flags, positional)
}

/// Render a Claude Code transcript message's `content` into a single string
/// for capture, preserving the tool signal a coding agent learns from: not
/// just assistant/user prose but which tools ran (`tool_use`) and how they
/// turned out (`tool_result`, flagged when `is_error`). Bodies are truncated
/// so a single huge tool output can't balloon the stored Event.
fn render_transcript_content(content: &serde_json::Value) -> String {
    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let head: String = s.chars().take(max).collect();
            format!("{head}… [{}+ chars truncated]", s.chars().count() - max)
        }
    }
    fn tool_result_body(content: &serde_json::Value) -> String {
        match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str().or_else(|| b.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            other if other.is_object() => other["text"].as_str().unwrap_or("").to_string(),
            _ => String::new(),
        }
    }
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                Some("tool_use") => {
                    let name = b["name"].as_str().unwrap_or("tool");
                    Some(format!("[tool_use {name}] {}", truncate(&b["input"].to_string(), 500)))
                }
                Some("tool_result") => {
                    let flag = if b["is_error"].as_bool().unwrap_or(false) {
                        " ERROR"
                    } else {
                        ""
                    };
                    Some(format!("[tool_result{flag}] {}", truncate(&tool_result_body(&b["content"]), 800)))
                }
                // Bare `{text: ...}` blocks with no `type` (older transcripts).
                _ => b["text"].as_str().map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Dispatch one `deja migrate --from <src>` run. Every source is file-based
/// (docs/migrate.md has the dump one-liners); `basic-memory` takes the vault
/// directory and walks its markdown files.
fn run_migrate(
    m: &mut DejaDB,
    ns: &str,
    from: &str,
    file: &str,
    history: Option<String>,
) -> Result<dejadb_store::migrate::MigrateReport, String> {
    use dejadb_store::migrate as mig;
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let parse = |p: &str, s: &str| {
        serde_json::from_str::<serde_json::Value>(s).map_err(|e| format!("{p}: bad JSON: {e}"))
    };
    let rep = match from {
        "mem0" => {
            let export = parse(file, &read(file)?)?;
            let history = match &history {
                Some(h) => Some(parse(h, &read(h)?)?),
                None => None,
            };
            mig::migrate_mem0(m, ns, Some(&export), history.as_ref())
        }
        "mem0-history" => {
            let h = parse(file, &read(file)?)?;
            mig::migrate_mem0(m, ns, None, Some(&h))
        }
        "langgraph" | "langmem" => mig::migrate_langgraph(m, ns, &read(file)?),
        "letta" => {
            let af = parse(file, &read(file)?)?;
            mig::migrate_letta(m, ns, &af)
        }
        "letta-archival" => mig::migrate_letta_archival(m, ns, &read(file)?),
        "zep" | "graphiti" => {
            let v = parse(file, &read(file)?)?;
            mig::migrate_zep(m, ns, &v)
        }
        "jsonl" => mig::migrate_jsonl(m, ns, &read(file)?),
        // Generic tool-call log (OpenAI-style JSONL) → Tool grains, so the
        // flagship analyzer can cluster failures from history that predates
        // DejaDB. One line per record; assistant `tool_calls` arrays expand.
        "tool-log" | "openai-tools" => {
            let mut rep = mig::MigrateReport::default();
            for (i, line) in read(file)?.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(line)
                    .map_err(|e| format!("{file}:{}: bad JSON: {e}", i + 1))?;
                for (name, content, is_err) in extract_tool_records(&v) {
                    let mut t = Tool::new(&name).is_error(is_err);
                    if !content.is_empty() {
                        t = t.content(&content);
                    }
                    let t = t.namespace(ns);
                    m.add(&t).map_err(|e| e.to_string())?;
                    rep.added += 1;
                }
            }
            Ok(rep)
        }
        "basic-memory" => {
            let root = std::path::PathBuf::from(file);
            if !root.is_dir() {
                return Err(format!(
                    "--from basic-memory expects --file to be the vault directory (got {file})"
                ));
            }
            // Deterministic walk: collect *.md paths, sorted.
            let mut notes: Vec<std::path::PathBuf> = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(d) = stack.pop() {
                let entries = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
                for entry in entries {
                    let path = entry.map_err(|e| e.to_string())?.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|x| x == "md") {
                        notes.push(path);
                    }
                }
            }
            notes.sort();
            let mut rep = mig::MigrateReport::default();
            for path in notes {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let md = read(&path.to_string_lossy())?;
                let mtime_ms = std::fs::metadata(&path)
                    .and_then(|md| md.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                mig::migrate_basic_memory_note(m, ns, &rel, &md, mtime_ms, &mut rep)
                    .map_err(|e| e.to_string())?;
            }
            Ok(rep)
        }
        other => {
            return Err(format!(
                "unknown --from '{other}' — sources: mem0, mem0-history, langgraph, letta, \
                 letta-archival, zep, basic-memory, jsonl, tool-log"
            ))
        }
    };
    rep.map_err(|e| e.to_string())
}

/// Extract `(tool_name, content, is_error)` records from one tool-log JSONL
/// line: a direct `{tool_name, content, is_error}` record, an OpenAI
/// `role:"tool"` result, or an assistant message carrying a `tool_calls` array.
fn extract_tool_records(v: &serde_json::Value) -> Vec<(String, String, bool)> {
    let stringify = |x: &serde_json::Value| match x {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    // Assistant message with a tool_calls array: one record per call.
    if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
        return calls
            .iter()
            .filter_map(|c| {
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .or_else(|| c.get("name").and_then(|n| n.as_str()))?;
                let args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .map(&stringify)
                    .unwrap_or_default();
                Some((name.to_string(), args, false))
            })
            .collect();
    }
    // A single tool record / tool-result message.
    let name = v
        .get("tool_name")
        .and_then(|n| n.as_str())
        .or_else(|| v.get("name").and_then(|n| n.as_str()))
        .or_else(|| v.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()));
    match name {
        Some(name) => {
            let content = v
                .get("content")
                .or_else(|| v.get("output"))
                .or_else(|| v.get("result"))
                .map(&stringify)
                .unwrap_or_default();
            let is_error = v
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or_else(|| v.get("error").is_some());
            vec![(name.to_string(), content, is_error)]
        }
        None => Vec::new(),
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match argv.first() {
        Some(c) => c.clone(),
        None => {
            println!("{USAGE}");
            return Ok(());
        }
    };
    if cmd == "help" || cmd == "--help" || cmd == "-h" {
        println!("{USAGE}");
        return Ok(());
    }
    if cmd == "version" || cmd == "--version" || cmd == "-V" {
        println!("deja {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (flags, positional) = parse_args(&argv[1..]);
    // Long-lived / exposed surfaces must name their memory explicitly rather
    // than silently defaulting to the personal file.
    let db = resolve_db(&flags, matches!(cmd.as_str(), "serve" | "ui"))?;
    let ns = flag(&flags, "ns").unwrap_or_else(|| "shared".to_string());

    // print-only verbs never open the store (paths may be untilde-expanded)
    if cmd == "hook" {
        let target = positional.first().map(String::as_str).unwrap_or("claude-code");
        if target != "claude-code" {
            return Err(format!("unknown hook target '{target}'"));
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "deja".into());
        println!(
            r#"Add to ~/.claude/settings.json (hooks section) to close the learning
loop automatically — inject relevant memory before each prompt, and capture
each exchange (with tool outcomes) when a turn ends:

{{
  "hooks": {{
    "UserPromptSubmit": [{{ "hooks": [{{
      "type": "command",
      "command": "{exe} recall-hook --db {db} --ns {ns} --with-waiser"
    }}] }}],
    "Stop": [{{ "hooks": [{{
      "type": "command",
      "command": "{exe} capture-stop --db {db} --ns {ns}"
    }}] }}]
  }}
}}

recall-hook reads the prompt and prints matching memories to stdout, which
Claude Code injects as context — so retrieval no longer depends on the model
choosing to call a tool. For on-demand reads/writes by the model itself, also
register the MCP server:
  claude mcp add deja -- {exe} serve --mcp --db {db} --ns {ns}

Nothing was written — apply the snippet yourself (or rerun with your own paths)."#
        );
        return Ok(());
    }

    // Optional encryption: when --passphrase-env <VAR> is given, derive an
    // AES-256 key from the passphrase held in that environment variable
    // (Argon2id; salt in a <db>.kdf sidecar). The passphrase and the derived
    // key are held in zeroizing buffers; note the storage engine keeps the key
    // resident in memory while the database is open.
    let enc_key = match flag(&flags, "passphrase-env") {
        Some(var) => {
            let pass = zeroize::Zeroizing::new(std::env::var(&var).map_err(|_| {
                format!("--passphrase-env {var}: environment variable is not set")
            })?);
            if pass.trim().is_empty() {
                return Err(format!("--passphrase-env {var}: passphrase is empty"));
            }
            Some(DejaDB::derive_key_for(&db, pass.as_str()).map_err(|e| e.to_string())?)
        }
        None => None,
    };

    // Recall-telemetry sidecar (host capability, §8): the agent-host default is
    // `aggregate`; `--telemetry off|aggregate|full` overrides. It is NOT a
    // file-truth, so it never re-stamps the file's declarations.
    let tel_mode = match flag(&flags, "telemetry") {
        Some(v) => dejadb_store::TelemetryMode::parse(&v)
            .ok_or_else(|| format!("--telemetry: unknown mode '{v}' (off|aggregate|full)"))?,
        None => dejadb_store::TelemetryMode::Aggregate,
    };

    // Files carry their own declarations (meta table); a bare open honors
    // them. --index-text is an explicit, deliberate re-stamp; encryption is a
    // host-supplied capability that also requires open_with.
    let explicit_index = flag(&flags, "index-text");
    let mut m = if explicit_index.is_some() || enc_key.is_some() {
        let mut o = dejadb_store::DejaDbOptions::default();
        if let Some(v) = &explicit_index {
            o.index_text = !matches!(v.as_str(), "false" | "0" | "off" | "no");
        }
        if let Some(key) = &enc_key {
            o.encryption_key = Some(**key);
        }
        o.telemetry = tel_mode;
        DejaDB::open_with(&db, o)
    } else if tel_mode != dejadb_store::TelemetryMode::Off {
        // Honor the file's declarations AND attach the telemetry sidecar.
        DejaDB::open_with_telemetry(&db, tel_mode)
    } else {
        DejaDB::open(&db)
    }
    .map_err(|e| e.to_string())?;
    for w in m.open_warnings() {
        eprintln!("deja: warning: {w}");
    }

    // Optional host-supplied embedder: --embed-cmd 'CMD' spawns CMD per embed
    // (text on stdin, JSON array of numbers on stdout). Enables the vector
    // recall leg and embeds newly written grains; the file records the model
    // as embedding provenance.
    if let Some(cmd_line) = flag(&flags, "embed-cmd") {
        let model = flag(&flags, "embed-model");
        let seen = m.open_warnings().len();
        let ce = dejadb_store::CommandEmbed::new(&cmd_line, model.as_deref())
            .map_err(|e| e.to_string())?;
        m.set_embedder(Box::new(ce));
        for w in &m.open_warnings()[seen..] {
            eprintln!("deja: warning: {w}");
        }
    }

    match cmd.as_str() {
        "add" => {
            // Positional `deja add <subject> <relation> <object>`, or the
            // explicit --subject/--relation/--object flags.
            let (s, r, o) = match (
                flag(&flags, "subject"),
                flag(&flags, "relation"),
                flag(&flags, "object"),
            ) {
                (Some(s), Some(r), Some(o)) => (s, r, o),
                _ if positional.len() >= 3 => (
                    positional[0].clone(),
                    positional[1].clone(),
                    positional[2].clone(),
                ),
                _ => {
                    return Err("usage: deja add <subject> <relation> <object>  \
                                (or --subject S --relation R --object O)"
                        .to_string())
                }
            };
            // An empty subject/relation/object stores an unrecallable fact —
            // reject at the door rather than pollute the memory file.
            for (name, v) in [("subject", &s), ("relation", &r), ("object", &o)] {
                if v.trim().is_empty() {
                    return Err(format!("{name} must not be empty"));
                }
            }
            let conf: f64 = flag(&flags, "confidence")
                .map(|c| c.parse().map_err(|_| "bad --confidence".to_string()))
                .transpose()?
                .unwrap_or(0.9);
            let mut f = Fact::new(&s, &r, &o).confidence(conf);
            f.common.namespace = Some(ns);
            // `--idempotent` collapses a re-add of the current value: if the
            // head for (subject, relation) already holds this object, the
            // existing grain's hash is returned instead of minting a new one.
            if flags.contains_key("idempotent") {
                let (h, inserted) = m.add_if_novel(&f).map_err(|e| e.to_string())?;
                println!("{h}");
                if !inserted {
                    eprintln!("(unchanged — value already current, no new grain)");
                }
            } else {
                let h = m.add(&f).map_err(|e| e.to_string())?;
                println!("{h}");
            }
        }
        "recall" => {
            // Positional `deja recall <subject>`, or --subject S.
            let s = flag(&flags, "subject")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "usage: deja recall <subject>  (or --subject S)".to_string())?;
            let rel = flag(&flags, "relation");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(16);
            let grains = m
                .recall(&ns, &s, rel.as_deref(), k)
                .map_err(|e| e.to_string())?;
            match flag(&flags, "render") {
                None => {
                    for g in grains {
                        let line = serde_json::json!({
                            "hash": g.hash.to_hex(),
                            "type": format!("{:?}", g.grain_type).to_lowercase(),
                            "fields": g.fields,
                        });
                        println!("{line}");
                    }
                }
                Some(render) => {
                    // model-ready context via dejadb-context (§7.5)
                    use dejadb_context::{ContextAssembler, FormatPolicy, OutputFormat};
                    let mut policy = match render.as_str() {
                        "sml" => FormatPolicy::claude(),
                        "markdown" => FormatPolicy::gpt4(),
                        "toon" => FormatPolicy::new(OutputFormat::Toon),
                        "plain" => FormatPolicy::new(OutputFormat::PlainText),
                        "json" => FormatPolicy::json_api(),
                        other => return Err(format!("unknown --render '{other}'")),
                    };
                    if let Some(b) = flag(&flags, "budget").and_then(|v| v.parse().ok()) {
                        policy.token_budget = Some(b);
                    }
                    let hits: Vec<dejadb_cal::store_types::SearchHit> = grains
                        .into_iter()
                        .map(|grain| {
                            let hash = grain.hash;
                            dejadb_cal::store_types::SearchHit {
                                grain,
                                score: 1.0,
                                hash,
                                score_breakdown: None,
                                explanation: None,
                                scope_depth: None,
                                source_namespace: None,
                                relative_time: None,
                                conflict_status: None,
                                supersession_status: None,
                                superseded_by_hash: None,
                                recall_source: None,
                            }
                        })
                        .collect();
                    let ctx = ContextAssembler::new().format(&hits, &policy);
                    println!("{}", ctx.text);
                    eprintln!(
                        "-- {} grains, ~{} tokens{}",
                        ctx.included_count,
                        ctx.estimated_tokens,
                        if ctx.truncated { " (truncated to budget)" } else { "" }
                    );
                }
            }
        }
        "search" => {
            let q = need(&flags, "query")?;
            let subject = flag(&flags, "subject");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(10);
            let grains = m
                .recall_hybrid(&ns, subject.as_deref(), None, Some(&q), k, None)
                .map_err(|e| e.to_string())?;
            for g in grains {
                println!("{}", serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                }));
            }
        }
        "cal" => {
            let query = positional
                .first()
                .ok_or_else(|| "usage: deja cal '<QUERY>' --db <file>".to_string())?
                .clone();
            let facade = DejaDbFacade::with_session(m, Some(ns), None);
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                ..CalExecutorConfig::default()
            });
            let res = ex.execute(&query, &facade).map_err(|e| e.to_string())?;
            let payload = serde_json::to_string_pretty(&res.result).map_err(|e| e.to_string())?;
            println!("{payload}");
            for w in res.warnings {
                eprintln!("warning: {w}");
            }
        }
        "history" => {
            let s = need(&flags, "subject")?;
            let r = need(&flags, "relation")?;
            let versions = m.history(&ns, &s, &r).map_err(|e| e.to_string())?;
            for v in versions {
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": v.hash.to_hex(),
                        "object": v.object,
                        "created_at": v.created_at,
                        "confidence": v.confidence,
                        "superseded_by": v.superseded_by.map(|h| h.to_hex()),
                    })
                );
            }
        }
        "log" => {
            let since: i64 = flag(&flags, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let limit: usize = flag(&flags, "limit").and_then(|v| v.parse().ok()).unwrap_or(50);
            for op in m.changes_since(since, limit).map_err(|e| e.to_string())? {
                let kind = match op.op {
                    dejadb_store::OP_ADD => "add",
                    dejadb_store::OP_SUPERSEDE => "supersede",
                    dejadb_store::OP_FORGET => "forget",
                    _ => "?",
                };
                println!("{:>6}  {:>20}  {:<9}  {}", op.op_seq, op.hlc, kind, op.hash.to_hex());
            }
        }
        "stream" => {
            // Litestream-shaped: snapshot + incrementing segments per
            // generation. Cursor + generation live in the target dir, so
            // any machine holding the dir can restore.
            let to = need(&flags, "to")?;
            let interval: u64 = flag(&flags, "interval-ms").and_then(|v| v.parse().ok()).unwrap_or(500);
            let once = flags.contains_key("once");
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            let cursor_path = format!("{to}/CURSOR");
            let (gen_id, cursor) = match std::fs::read_to_string(&cursor_path) {
                Ok(s) => {
                    let mut parts = s.trim().splitn(2, ' ');
                    let g = parts.next().unwrap_or("").to_string();
                    let c: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    (g, c)
                }
                Err(_) => (String::new(), 0),
            };
            let (gen_id, mut cursor) = if gen_id.is_empty() {
                // new generation: random-ish id + full snapshot as segment 0
                let g = format!("{:016x}", std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64);
                std::fs::create_dir_all(format!("{to}/gen-{g}")).map_err(|e| e.to_string())?;
                (g, 0i64)
            } else {
                (gen_id, cursor)
            };
            let mut seg: u32 = std::fs::read_dir(format!("{to}/gen-{gen_id}"))
                .map(|d| d.count() as u32)
                .unwrap_or(0);
            loop {
                let ops_now = m.changes_since(cursor, 1).map_err(|e| e.to_string())?;
                if !ops_now.is_empty() {
                    let seg_path = format!("{to}/gen-{gen_id}/segment-{seg:08}.mgb");
                    let st = m.bundle_since(cursor, &seg_path).map_err(|e| e.to_string())?;
                    cursor = st.last_op_seq;
                    seg += 1;
                    std::fs::write(&cursor_path, format!("{gen_id} {cursor}")).map_err(|e| e.to_string())?;
                    eprintln!("shipped {} ops → {seg_path}", st.ops);
                }
                if once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }
        }
        "follow" => {
            // Subscription pull (§5.10d): apply new segments from a stream
            // dir (local, NFS, or object-store mount). Follower keeps its
            // own cursor beside its db — offline edges catch up by replay.
            let from = need(&flags, "from")?;
            let interval: u64 = flag(&flags, "interval-ms").and_then(|v| v.parse().ok()).unwrap_or(1000);
            let once = flags.contains_key("once");
            let fcur_path = format!("{db}.follow");
            loop {
                let cursor = std::fs::read_to_string(format!("{from}/CURSOR")).unwrap_or_default();
                let gen_id = cursor.trim().split(' ').next().unwrap_or("").to_string();
                if !gen_id.is_empty() {
                    let (fgen, fseg) = match std::fs::read_to_string(&fcur_path) {
                        Ok(s) => {
                            let mut it = s.trim().splitn(2, ' ');
                            (it.next().unwrap_or("").to_string(),
                             it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0))
                        }
                        Err(_) => (String::new(), 0),
                    };
                    let mut fseg = if fgen != gen_id { 0 } else { fseg };
                    loop {
                        let seg_path = format!("{from}/gen-{gen_id}/segment-{fseg:08}.mgb");
                        if !std::path::Path::new(&seg_path).exists() {
                            break;
                        }
                        let st = m.import_bundle(&seg_path).map_err(|e| e.to_string())?;
                        eprintln!("applied segment {fseg} ({} ops, {} skipped)", st.applied, st.skipped);
                        fseg += 1;
                        std::fs::write(&fcur_path, format!("{gen_id} {fseg}")).map_err(|e| e.to_string())?;
                    }
                }
                if once {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(interval));
            }
        }
        "restore" => {
            let from = need(&flags, "from")?;
            let until: Option<i64> = flag(&flags, "until-hlc").and_then(|v| v.parse().ok());
            let cursor = std::fs::read_to_string(format!("{from}/CURSOR"))
                .map_err(|_| "no CURSOR in stream dir".to_string())?;
            let gen_id = cursor.trim().split(' ').next().unwrap_or("").to_string();
            let dir = format!("{from}/gen-{gen_id}");
            let mut segs: Vec<String> = std::fs::read_dir(&dir)
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok().map(|e| e.path().display().to_string()))
                .filter(|p| p.ends_with(".mgb"))
                .collect();
            segs.sort();
            let mut applied = 0usize;
            for s in &segs {
                let st = m.import_bundle_until(s, until).map_err(|e| e.to_string())?;
                applied += st.applied;
            }
            println!("restored {} ops from {} segments (gen {gen_id})", applied, segs.len());
        }
        "bundle" => {
            let out = need(&flags, "out")?;
            let since: i64 = flag(&flags, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let st = m.bundle_since(since, &out).map_err(|e| e.to_string())?;
            println!(
                "bundled {} ops ({} bytes) → {} (next --since {})",
                st.ops, st.bytes, out, st.last_op_seq
            );
        }
        "import" => {
            let bundle = need(&flags, "bundle")?;
            let st = m.import_bundle(&bundle).map_err(|e| e.to_string())?;
            println!("applied {} ops, skipped {}", st.applied, st.skipped);
        }
        "migrate" => {
            let from = need(&flags, "from")?;
            let file = need(&flags, "file")?;
            // Bulk-load fast path: drop the FTS index for the duration and
            // rebuild once at the end — Turso indexes all existing rows at
            // CREATE INDEX time, instead of ~150ms of FTS bookkeeping per
            // write transaction.
            let deferred = m.defer_text_index().map_err(|e| e.to_string())?;
            let rep = run_migrate(&mut m, &ns, &from, &file, flag(&flags, "history"));
            if deferred {
                m.rebuild_text_index().map_err(|e| e.to_string())?;
            }
            let rep = rep?;
            for n in &rep.notes {
                eprintln!("deja: migrate: {n}");
            }
            println!("{}", rep.to_json());
        }
        "reindex" => {
            let n = m.rebuild_text_index().map_err(|e| e.to_string())?;
            println!("text index rebuilt ({n} rows backfilled)");
        }
        "verify" => {
            let rep = m.verify().map_err(|e| e.to_string())?;
            println!(
                "integrity: {} | grains: {} | hash mismatches: {} | undecodable: {}",
                rep.integrity, rep.grains, rep.hash_mismatches, rep.undecodable
            );
            if rep.integrity != "ok" || rep.hash_mismatches > 0 || rep.undecodable > 0 {
                return Err("verification FAILED".to_string());
            }
        }
        "stats" => {
            let s = m.stats().map_err(|e| e.to_string())?;
            println!(
                "grains: {} ({} current) | triples: {} | terms: {} | ops: {} | thread-indexed events: {}",
                s.grains, s.current, s.triples, s.terms, s.ops, s.events_indexed
            );
        }
        "serve" => {
            if !flags.contains_key("mcp") {
                return Err("only --mcp transport is available (deja serve --mcp)".to_string());
            }
            let mut facade = dejadb_cal::DejaDbFacade::with_session(m, Some(ns), None);
            // Optional read-only mounts for cross-file ASSEMBLE:
            //   --mount alias=path[,alias=path...]
            // A recall/query in namespace "alias.inner" routes to the mount;
            // writes always stay on the primary file (mounts are read-only by
            // construction), so this only widens what recall/ASSEMBLE can read.
            if let Some(spec) = flag(&flags, "mount") {
                for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                    let (alias, path) = entry
                        .split_once('=')
                        .map(|(a, p)| (a.trim(), p.trim()))
                        .filter(|(a, p)| !a.is_empty() && !p.is_empty())
                        .ok_or_else(|| format!("--mount expects alias=path, got '{entry}'"))?;
                    let store =
                        DejaDB::open(path).map_err(|e| format!("mount '{alias}' ({path}): {e}"))?;
                    eprintln!("deja: mounted '{alias}' (read-only) → {path}");
                    facade.mount(alias, store);
                }
            }
            let mut server = dejadb_mcp::McpServer::new(facade, None);
            if flags.contains_key("no-destructive-ops") {
                server = server.allow_destructive_ops(false);
                eprintln!("deja: destructive operations disabled (read-only session)");
            }
            if let Some(lock) = flag(&flags, "lock-ns") {
                server = server.lock_namespace(lock.clone());
                eprintln!("deja: namespace locked to '{lock}' (per-call namespace ignored)");
            }
            // Host waiser policy (--policy FILE or $WAISER_POLICY): the
            // dejadb_waiser tool honors the same grants as the CLI run. Host
            // config set at process start — never controllable by the client.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_waiser_policy(p);
                eprintln!("deja: waiser host policy attached to dejadb_waiser");
            }
            server.serve_stdio().map_err(|e| e.to_string())?;
        }
        "capture-stop" => {
            // Claude Code Stop-hook: JSON on stdin with session_id +
            // transcript_path. Store the last user/assistant exchange as
            // Event grains (thread-indexed by session).
            use std::io::Read as IoRead;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
            let hook: serde_json::Value =
                serde_json::from_str(&input).map_err(|e| format!("bad hook json: {e}"))?;
            let session = hook["session_id"].as_str().unwrap_or("unknown-session").to_string();
            let tpath = hook["transcript_path"]
                .as_str()
                .ok_or("hook json missing transcript_path")?;
            let transcript = std::fs::read_to_string(tpath).map_err(|e| e.to_string())?;
            let mut last: std::collections::HashMap<String, String> = Default::default();
            for line in transcript.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let role = v["message"]["role"].as_str().unwrap_or("");
                if role != "user" && role != "assistant" {
                    continue;
                }
                // Capture the tool signal too (tool_use / tool_result +
                // is_error), not just prose — that is where a coding agent's
                // "which fix worked / failed review" actually lives.
                let text = render_transcript_content(&v["message"]["content"]);
                if !text.trim().is_empty() {
                    last.insert(role.to_string(), text);
                }
            }
            let mut stored = 0;
            for role in ["user", "assistant"] {
                if let Some(text) = last.get(role) {
                    let mut e = dejadb_core::types::Event::new(text);
                    e.common.namespace = Some(ns.clone());
                    e.session_id = Some(session.clone());
                    e.role = dejadb_core::types::Role::from_str(role);
                    m.add(&e).map_err(|e| e.to_string())?;
                    stored += 1;
                }
            }
            println!("captured {stored} events for session {session}");
        }
        "recall-hook" => {
            // Claude Code UserPromptSubmit hook: read the hook JSON on stdin,
            // hybrid-search memory for the prompt, and print matching grains to
            // stdout — which Claude Code injects into the model's context. This
            // is the retrieval half of the learning loop, made automatic
            // (no reliance on the model deciding to call a recall tool).
            use std::io::Read as IoRead;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).map_err(|e| e.to_string())?;
            let hook: serde_json::Value =
                serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
            // UserPromptSubmit carries `prompt`; SessionStart has none, so we
            // stay silent rather than inject noise.
            let query = hook["prompt"].as_str().unwrap_or("").trim().to_string();
            if query.is_empty() {
                return Ok(());
            }
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(5);
            // --with-waiser additionally injects the pending recommendation
            // queue (a compact, capped block) so the loop closes into the
            // agent's context instead of waiting to be polled.
            let with_waiser = flag(&flags, "with-waiser").is_some();
            let grains = m
                .recall_hybrid(&ns, None, None, Some(&query), k, None)
                .map_err(|e| e.to_string())?;
            if grains.is_empty() && !with_waiser {
                return Ok(());
            }
            if !grains.is_empty() {
                use dejadb_context::{ContextAssembler, FormatPolicy};
                let mut policy = FormatPolicy::claude();
                policy.token_budget =
                    Some(flag(&flags, "budget").and_then(|v| v.parse().ok()).unwrap_or(400));
                let hits: Vec<dejadb_cal::store_types::SearchHit> = grains
                    .into_iter()
                    .map(|grain| {
                        let hash = grain.hash;
                        dejadb_cal::store_types::SearchHit {
                            grain,
                            score: 1.0,
                            hash,
                            score_breakdown: None,
                            explanation: None,
                            scope_depth: None,
                            source_namespace: None,
                            relative_time: None,
                            conflict_status: None,
                            supersession_status: None,
                            superseded_by_hash: None,
                            recall_source: None,
                        }
                    })
                    .collect();
                let ctx = ContextAssembler::new().format(&hits, &policy);
                if !ctx.text.trim().is_empty() {
                    println!("Relevant memory from DejaDB:\n{}", ctx.text);
                }
            }
            if with_waiser {
                // Read-only listing over the same store handle (single writer
                // per file — never a second open). Engine-templated summaries;
                // origin=llm entries are labeled. Capped so a long queue can't
                // flood the prompt; the console/CLI remain the review surface.
                let sub = DejaDbSubstrate::new(m, Some(ns.to_string()));
                let engine = Engine::with_builtins();
                let mut pending = engine
                    .recommendations(&sub, Some(RecStatus::Pending))
                    .map_err(|e| e.to_string())?;
                if !pending.is_empty() {
                    pending.sort_by(|a, b| b.severity.cmp(&a.severity).then(a.hash.cmp(&b.hash)));
                    const CAP: usize = 3;
                    println!(
                        "\nWaiser: {} pending recommendation(s) for this memory (review with `deja waiser list`, act with approve/apply/reject --because):",
                        pending.len()
                    );
                    for r in pending.iter().take(CAP) {
                        let origin = match r.origin {
                            waiser::Origin::Llm { .. } => " [llm]",
                            waiser::Origin::Command { .. } => " [external]",
                            _ => "",
                        };
                        println!("  [{}] {}{}  {}", r.severity.as_str(), short(&r.hash), origin, r.summary.render());
                    }
                    if pending.len() > CAP {
                        println!("  … and {} more", pending.len() - CAP);
                    }
                }
            }
        }
        "remember" => {
            let content = need(&flags, "content")?;
            let observer = flag(&flags, "observer").unwrap_or_else(|| "cli".to_string());
            // Pre-extracted facts as JSON (the CLI can't run an LLM; hosts
            // pass their extractor's output here).
            let drafts: Vec<dejadb_store::FactDraft> = match flag(&flags, "facts") {
                Some(j) => {
                    let arr: Vec<serde_json::Value> =
                        serde_json::from_str(&j).map_err(|e| format!("bad --facts: {e}"))?;
                    arr.iter()
                        .map(|v| dejadb_store::FactDraft {
                            subject: v["subject"].as_str().unwrap_or("").to_string(),
                            relation: v["relation"].as_str().unwrap_or("").to_string(),
                            object: v["object"].as_str().unwrap_or("").to_string(),
                            confidence: v["confidence"].as_f64().unwrap_or(0.8),
                        })
                        .collect()
                }
                None => Vec::new(),
            };
            let extractor = move |_c: &str| drafts.clone();
            let res = m
                .remember(&ns, &content, &observer, Some(&extractor))
                .map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "observation": res.observation.to_hex(),
                    "facts": res.facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
                })
            );
        }
        "memtool" => {
            let cmd = positional
                .first()
                .ok_or_else(|| "usage: deja memtool '<json>' --db <file>".to_string())?;
            let cmd: serde_json::Value =
                serde_json::from_str(cmd).map_err(|e| format!("bad command json: {e}"))?;
            let mut t = dejadb_store::memory_tool::MemoryTool::new(&mut m, &ns);
            println!("{}", t.execute(&cmd).map_err(|e| e.to_string())?);
        }
        "repl" => {
            use std::io::{BufRead, Write as IoWrite};
            let facade = dejadb_cal::DejaDbFacade::with_session(m, Some(ns.clone()), None);
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                ..CalExecutorConfig::default()
            });
            eprintln!("deja repl — namespace '{ns}' · CAL statements, or .stats .log .help .quit");
            let stdin = std::io::stdin();
            let mut lines = stdin.lock().lines();
            loop {
                eprint!("cal> ");
                std::io::stderr().flush().ok();
                let line = match lines.next() {
                    Some(Ok(l)) => l,
                    _ => break,
                };
                let line = line.trim();
                match line {
                    "" => continue,
                    ".quit" | ".exit" => break,
                    ".help" => eprintln!(
                        "CAL: RECALL / ASSEMBLE / EXISTS / HISTORY / ADD / SUPERSEDE / DESCRIBE / | COUNT\n\
                         dot: .stats  .log  .verify  .quit"
                    ),
                    ".stats" => match facade.with_store(|st| st.stats()) {
                        Ok(s) => eprintln!(
                            "grains {} ({} current) · triples {} · ops {}",
                            s.grains, s.current, s.triples, s.ops
                        ),
                        Err(e) => eprintln!("error: {e}"),
                    },
                    ".log" => match facade.with_store(|st| st.changes_since(0, 20)) {
                        Ok(ops) => {
                            for o in ops {
                                eprintln!("{:>5}  {:<9}  {}", o.op_seq, o.op, o.hash.to_hex());
                            }
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                    ".verify" => match facade.with_store(|st| st.verify()) {
                        Ok(r) => eprintln!(
                            "integrity {} · {} grains · {} mismatches",
                            r.integrity, r.grains, r.hash_mismatches
                        ),
                        Err(e) => eprintln!("error: {e}"),
                    },
                    q => match ex.execute(q, &facade) {
                        Ok(res) => {
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&res.result).unwrap_or_default()
                            );
                            for w in res.warnings {
                                eprintln!("warning: {w}");
                            }
                        }
                        Err(e) => eprintln!("error: {e}"),
                    },
                }
            }
        }
        "ui" => {
            let addr = flag(&flags, "addr").unwrap_or_else(|| "127.0.0.1:7437".to_string());
            let allow_remote = flag(&flags, "allow-remote").is_some();

            // Optional console authentication. `--token-env <VAR>` names an
            // environment variable holding the shared secret, keeping it out of
            // argv and shell history (same convention as --passphrase-env).
            // When set, every request needs it: browsers via the native HTTP
            // Basic prompt (any username, password = token), scripts via
            // `Authorization: Bearer <token>`.
            let auth_token = match flag(&flags, "token-env") {
                Some(var) => {
                    let t = std::env::var(&var).map_err(|_| {
                        format!("--token-env {var}: environment variable is not set")
                    })?;
                    if t.trim().is_empty() {
                        return Err(format!("--token-env {var}: token is empty"));
                    }
                    Some(t)
                }
                None => None,
            };
            let has_auth = auth_token.is_some();

            if !addr_is_loopback(&addr) && !allow_remote {
                let why = if has_auth {
                    "It is authenticated, but still plaintext HTTP — the token and all \
                     memory cross the wire in the clear. Terminate TLS in front of it, \
                     or pass --allow-remote to accept that risk."
                } else {
                    "With no --token-env it is an UNAUTHENTICATED, writable console over \
                     plaintext HTTP — anyone who can reach it could read or modify your \
                     memory. Add --token-env <VAR>, keep it on loopback, put it behind a \
                     TLS-terminating proxy, or pass --allow-remote to override."
                };
                return Err(format!(
                    "refusing to bind the console to a non-loopback address ({addr}). {why}"
                ));
            }

            let facade = dejadb_cal::DejaDbFacade::with_session(m, Some(ns), None);
            let mut server = dejadb_server::UiServer::new(facade, db.clone());
            if allow_remote {
                server = server.allow_remote(true);
            }
            if flags.contains_key("no-destructive-ops") {
                server = server.allow_destructive_ops(false);
                eprintln!("deja: destructive operations disabled (read-only console)");
            }
            if let Some(tok) = auth_token {
                server = server.with_auth(tok);
                eprintln!(
                    "deja: console authentication ENABLED — HTTP Basic \
                     (any username, password = the token)"
                );
            } else {
                eprintln!("deja: console is UNAUTHENTICATED (enable with --token-env <VAR>)");
            }
            // Host waiser policy (--policy FILE or $WAISER_POLICY): a
            // console-triggered waiser run honors the same grants as the CLI
            // run. Never grantable from the console itself.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_waiser_policy(p);
                eprintln!("deja: waiser host policy attached to the console's waiser routes");
            }
            let listener = dejadb_server::UiServer::bind(&addr).map_err(|e| e.to_string())?;
            if !addr_is_loopback(&addr) {
                eprintln!(
                    "deja: WARNING — bound to non-loopback {addr} over plaintext HTTP; {}",
                    if has_auth {
                        "the token crosses the wire in the clear — use a TLS-terminating proxy."
                    } else {
                        "UNAUTHENTICATED — anyone who can reach it can read/modify memory."
                    }
                );
            }
            eprintln!(
                "deja console → http://{}  (Ctrl-C to stop)",
                listener.local_addr().map_err(|e| e.to_string())?
            );
            server.serve(listener).map_err(|e| e.to_string())?;
        }
        "get" => {
            let h = positional
                .first()
                .ok_or_else(|| "usage: deja get <hash> --db <file>".to_string())?;
            let hash = Hash::from_hex(h).map_err(|e| e.to_string())?;
            let g = m.get(&hash).map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": format!("{:?}", g.grain_type).to_lowercase(),
                    "fields": g.fields,
                })
            );
        }
        "provenance" => {
            // Reverse provenance: grains distilled from a source (credit
            // assignment / episode-scoped unlearn). Forward provenance — a
            // grain's own `derived_from` — is already visible via `deja get`.
            let h = flag(&flags, "of")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| {
                    "usage: deja provenance <source-hash> --db <file>  \
                     (lists grains whose derived_from is that hash)"
                        .to_string()
                })?;
            let h = h.strip_prefix("sha256:").unwrap_or(&h);
            let parent = Hash::from_hex(h).map_err(|e| e.to_string())?;
            let kids = m.grains_derived_from(&parent).map_err(|e| e.to_string())?;
            for g in kids {
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": g.hash.to_hex(),
                        "type": format!("{:?}", g.grain_type).to_lowercase(),
                        "subject": g.get_str("subject"),
                        "relation": g.get_str("relation"),
                        "object": g.get_str("object"),
                    })
                );
            }
        }
        "forks" => {
            // Surface open forks: (ns, subject, relation) with >1 live head,
            // from concurrent supersession of the same value (e.g. edits synced
            // from two writers). Both tips are kept — nothing is lost — until an
            // explicit `deja merge` closes the fork.
            let forks = m.open_forks().map_err(|e| e.to_string())?;
            if forks.is_empty() {
                eprintln!("no open forks");
            }
            for f in forks {
                println!(
                    "{}",
                    serde_json::json!({
                        "namespace": f.namespace,
                        "subject": f.subject,
                        "relation": f.relation,
                        "heads": f.heads.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
                    })
                );
            }
        }
        "merge" => {
            // Close an open fork by writing a resolved value that supersedes
            // every tip; the merge grain records all parents in its blob.
            let s = need(&flags, "subject")?;
            let r = need(&flags, "relation")?;
            let o = need(&flags, "object")?;
            let conf: f64 = flag(&flags, "confidence")
                .map(|c| c.parse().map_err(|_| "bad --confidence".to_string()))
                .transpose()?
                .unwrap_or(0.9);
            let mut merged = Fact::new(&s, &r, &o).confidence(conf);
            merged.common.namespace = Some(ns.clone());
            let h = m
                .merge_heads(&ns, &s, &r, &mut merged)
                .map_err(|e| e.to_string())?;
            println!("{h}");
        }
        "novelty" => {
            // Advise-mode paraphrase novelty check: nearest existing grains to
            // a candidate text (needs --embed-cmd). A reflection harness reads
            // the top similarity and decides to supersede vs add — the engine
            // never drops anything on its own.
            let text = need(&flags, "text")?;
            let subject = flag(&flags, "subject");
            let relation = flag(&flags, "relation");
            let k: usize = flag(&flags, "k").and_then(|v| v.parse().ok()).unwrap_or(5);
            let matches = m
                .nearest_semantic(&ns, subject.as_deref(), relation.as_deref(), &text, k)
                .map_err(|e| e.to_string())?;
            for (h, sim) in matches {
                let object = m.get(&h).ok().and_then(|g| g.get_str("object").map(String::from));
                println!(
                    "{}",
                    serde_json::json!({
                        "hash": h.to_hex(),
                        "similarity": (sim * 1000.0).round() / 1000.0,
                        "object": object,
                    })
                );
            }
        }
        "init" => {
            run_init(m, &ns, &flags, &positional)?;
        }
        "waiser" => {
            run_waiser(m, &ns, &flags, &positional)?;
        }
        other => return Err(format!("unknown command '{other}' — try `deja help`")),
    }
    Ok(())
}

/// Parse a duration like `6h` / `30m` / `2d` / `3600s` into milliseconds.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    let mult = match &s[split..] {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(n * mult)
}

fn parse_severity(s: &str) -> Severity {
    match s.to_ascii_lowercase().as_str() {
        "high" => Severity::High,
        "medium" => Severity::Medium,
        "low" => Severity::Low,
        _ => Severity::Info,
    }
}

/// `--status` filter; default is `pending`, `all` clears the filter.
fn status_filter(flags: &HashMap<String, String>) -> Option<RecStatus> {
    match flag(flags, "status").as_deref() {
        Some("approved") => Some(RecStatus::Approved),
        Some("rejected") => Some(RecStatus::Rejected),
        Some("applied") => Some(RecStatus::Applied),
        Some("rolled_back") => Some(RecStatus::RolledBack),
        Some("expired") => Some(RecStatus::Expired),
        Some("all") => None,
        _ => Some(RecStatus::Pending),
    }
}

fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Resolve a git-style unique hash prefix to a full recommendation hash.
fn resolve_hash(engine: &Engine, sub: &DejaDbSubstrate, prefix: &str) -> Result<String, String> {
    let recs = engine.recommendations(sub, None).map_err(|e| e.to_string())?;
    let matches: Vec<&str> = recs
        .iter()
        .map(|r| r.hash.as_str())
        .filter(|h| h.starts_with(prefix))
        .collect();
    match matches.len() {
        0 => Err(format!("no recommendation matches '{prefix}'")),
        1 => Ok(matches[0].to_string()),
        n => Err(format!("'{prefix}' is ambiguous ({n} matches) — use more characters")),
    }
}

/// Load the host policy from `--policy FILE` or `$WAISER_POLICY` (§6.2).
fn load_policy(flags: &HashMap<String, String>) -> Result<Option<Policy>, String> {
    let path = flag(flags, "policy").or_else(|| std::env::var("WAISER_POLICY").ok());
    match path {
        Some(p) => {
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
            Ok(Some(Policy::from_json(&s).map_err(|e| e.to_string())?))
        }
        None => Ok(None),
    }
}

/// `deja waiser <run|list|show|approve|reject|apply|rollback|analyzers|policy|status>`.
fn run_waiser(
    m: DejaDB,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("status");
    let mut sub = DejaDbSubstrate::new(m, Some(ns.to_string()));
    // Host policy (--policy FILE or $WAISER_POLICY) — the only place
    // auto-apply is granted. Absent → a closed default (nothing auto-applies).
    let policy = load_policy(flags)?;
    let mut engine = match policy {
        Some(p) => Engine::with_builtins().with_policy(p),
        None => Engine::with_builtins(),
    };
    // Optional LLM reflection: the model proposes findings that are grounded +
    // adversarially verified before they can reach the queue (origin=llm, never
    // auto-applied). Two ways to attach one, both CLI-only, never persisted:
    //   --model provider:name   → a built-in HTTP backend (key from the env)
    //   --llm-cmd 'CMD'         → a subprocess backend (the zero-dep escape hatch)
    // `--llm-cmd` wins if both are given.
    if let Some(cmd) = flag(flags, "llm-cmd") {
        let model = flag(flags, "llm-model");
        let llm = waiser::CommandLlm::new(&cmd, model.as_deref()).map_err(|e| e.to_string())?;
        engine = engine.with_llm(Box::new(llm));
    } else if let Some(spec) = flag(flags, "model") {
        // Key is read from the environment (ANTHROPIC_API_KEY / OPENAI_API_KEY /
        // OLLAMA_HOST, or --llm-api-key-env), never taken on the command line.
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let llm = dejadb_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        engine = engine.with_llm(llm);
    }
    // Optional SEPARATE grounding backend (§11): point the entailment check at a
    // cheaper or specialized model, or take the generative model out of grounding
    // entirely. Falls back to the main backend when absent. `--ground-cmd` wins.
    if let Some(cmd) = flag(flags, "ground-cmd") {
        let g = waiser::CommandLlm::new(&cmd, None).map_err(|e| e.to_string())?;
        engine = engine.with_ground_llm(Box::new(g));
    } else if let Some(spec) = flag(flags, "ground-model") {
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let g = dejadb_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        engine = engine.with_ground_llm(g);
    }
    // Optional external analyzer: a subprocess that flags domain-specific issues
    // (trust class Command → advisory only, never auto-applies). Registered up
    // front so it participates in the pass like a built-in.
    if let Some(cmd) = flag(flags, "analyzer-cmd") {
        let a = waiser::CommandAnalyzer::new(&cmd).map_err(|e| e.to_string())?;
        engine.register(Box::new(a));
    }
    let now = now_ms();
    let actor = flag(flags, "actor").unwrap_or_else(|| "user:local".to_string());
    let observer = ObserverType::Human;
    let scopes = ScopeSet::all(); // the CLI is the local root of trust
    let json = flag(flags, "format").as_deref() == Some("json");

    match sub_cmd {
        // `reflect` = a run that re-analyzes the whole memory (full sweep),
        // ignoring the incremental watermark; otherwise identical to `run`.
        "run" | "reflect" => {
            let opts = RunOptions {
                min_new: flag(flags, "min-new").and_then(|v| v.parse().ok()),
                min_new_errors: flag(flags, "min-new-errors").and_then(|v| v.parse().ok()),
                if_stale_ms: flag(flags, "if-stale").and_then(|v| parse_duration(&v)),
                namespaces: Vec::new(),
                full_sweep: sub_cmd == "reflect",
            };
            let res = engine.run(&mut sub, &opts, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&res).map_err(|e| e.to_string())?);
            } else if res.ran() {
                if !flags.contains_key("quiet") {
                    eprintln!(
                        "waiser: ran — proposed {} ({} deduped, {} auto-applied) across {} analyzer(s)",
                        res.stored,
                        res.deduped,
                        res.auto_applied,
                        res.analyzers_run.len()
                    );
                }
                if res.stored > 0 {
                    eprintln!("waiser: {} new — deja waiser list", res.stored);
                }
            } else if !flags.contains_key("quiet") {
                eprintln!("waiser: skipped ({:?})", res.skip_reason);
            }
        }
        "list" => {
            let filter = status_filter(flags);
            let recs = engine.recommendations(&sub, filter).map_err(|e| e.to_string())?;
            if json {
                let rows: Vec<_> = recs
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "hash": r.hash,
                            "status": r.status.as_str(),
                            "severity": r.severity.as_str(),
                            "analyzer": r.analyzer,
                            "destructive": r.destructive,
                            "summary": r.summary.render(),
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string(&rows).map_err(|e| e.to_string())?);
            } else if recs.is_empty() {
                eprintln!("no recommendations — run `deja waiser run` first");
            } else {
                for r in &recs {
                    println!(
                        "{}  {:<6}  {:<28}  {}",
                        short(&r.hash),
                        r.severity.as_str(),
                        r.analyzer,
                        r.summary.render()
                    );
                }
            }
            // CI gate: exit 2 if any pending recommendation meets --fail-on.
            if let Some(sev) = flag(flags, "fail-on") {
                let threshold = parse_severity(&sev);
                let hit = recs
                    .iter()
                    .any(|r| r.status == RecStatus::Pending && r.severity >= threshold);
                if hit {
                    eprintln!("waiser: pending recommendation(s) at or above severity '{sev}'");
                    std::process::exit(2);
                }
            }
        }
        "show" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: deja waiser show <hash>".to_string())?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let recs = engine.recommendations(&sub, None).map_err(|e| e.to_string())?;
            let r = recs.iter().find(|r| r.hash == hash).unwrap();
            let out = serde_json::json!({
                "hash": r.hash,
                "status": r.status.as_str(),
                "severity": r.severity.as_str(),
                "analyzer": r.analyzer,
                "target_ref": r.target_ref,
                "summary": r.summary.render(),
                "destructive": r.destructive,
                "rollbackable": r.rollbackable,
                "evidence": r.evidence,
                "dedup_key": r.dedup_key,
            });
            println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
        }
        "approve" | "reject" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| format!("usage: deja waiser {sub_cmd} <hash> --because \"...\""))?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let decision = if sub_cmd == "approve" { Decision::Approve } else { Decision::Reject };
            engine
                .review(&mut sub, &hash, decision, &actor, observer, &scopes, &because, now)
                .map_err(|e| e.to_string())?;
            eprintln!("{sub_cmd}d {}", short(&hash));
        }
        "apply" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: deja waiser apply <hash> --because \"...\"".to_string())?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let allow_destructive = flags.contains_key("allow-destructive");
            let applied = engine
                .apply(&mut sub, &hash, &actor, observer, &scopes, &because, allow_destructive, now)
                .map_err(|e| e.to_string())?;
            eprintln!(
                "applied {} ({})",
                short(&hash),
                if applied.rollbackable { "rollbackable" } else { "non-rollbackable" }
            );
        }
        "rollback" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: deja waiser rollback <hash> --because \"...\"".to_string())?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            engine
                .rollback(&mut sub, &hash, &actor, observer, &scopes, &because, now)
                .map_err(|e| e.to_string())?;
            eprintln!("rolled back {}", short(&hash));
        }
        "analyzers" => {
            for a in engine.analyzers() {
                let m = a.manifest();
                println!(
                    "{:<28}  {:?}  on={}  {}",
                    m.id, m.tier, m.default_on, m.title
                );
            }
        }
        // The Verify gate's measured history: did applied advice hold?
        "outcomes" => {
            let outcomes = engine.outcomes(&sub).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&outcomes).map_err(|e| e.to_string())?);
            } else if outcomes.is_empty() {
                eprintln!(
                    "no measured outcomes yet — outcome review runs after an applied \
                     recommendation's review window elapses"
                );
            } else {
                for o in &outcomes {
                    let horizon = if o.horizon_ms % 86_400_000 == 0 {
                        format!("{}d", o.horizon_ms / 86_400_000)
                    } else {
                        format!("{}h", o.horizon_ms / 3_600_000)
                    };
                    println!(
                        "{}  {:<22}  @{:<4}  baseline {} → current {}  [{}]",
                        short(&o.rec_hash),
                        o.metric,
                        horizon,
                        o.baseline,
                        o.current,
                        o.verdict
                    );
                }
            }
        }
        // Config reporting: the effective host policy (read-only).
        "policy" => {
            println!(
                "{}",
                serde_json::to_string_pretty(engine.policy()).map_err(|e| e.to_string())?
            );
        }
        // Bare `deja waiser` (or an unknown subcommand) prints a health summary.
        _ => {
            let h = engine.health(&sub, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&h).map_err(|e| e.to_string())?);
            } else {
                println!(
                    "waiser: {} recommendation(s) — {} pending, {} applied",
                    h.total, h.pending, h.applied
                );
                match h.last_run_ms {
                    None => println!("  never run — deja waiser run"),
                    Some(last) => {
                        let days = (now - last) / 86_400_000;
                        println!(
                            "  last run {days}d ago; {} new grain(s) ({} tool error(s)) since",
                            h.grains_since_run, h.error_events_since_run
                        );
                    }
                }
                // Reflection §6b: the live approval-rate for LLM-surfaced
                // findings (only shown once the LLM path has produced any).
                let lm = engine.llm_metrics(&sub).map_err(|e| e.to_string())?;
                if lm.proposed > 0 {
                    match lm.approval_rate {
                        Some(rate) => println!(
                            "  LLM findings: {} surfaced, {:.0}% approved ({} approved / {} rejected, {} pending)",
                            lm.proposed, rate * 100.0, lm.approved, lm.rejected, lm.pending
                        ),
                        None => println!("  LLM findings: {} surfaced, none decided yet", lm.proposed),
                    }
                }
                if h.stale {
                    eprintln!("  ⚠ the loop may be stale — run `deja waiser run` or wire the SessionEnd hook");
                } else if h.pending > 0 {
                    println!("  review with: deja waiser list");
                }
            }
        }
    }
    Ok(())
}

/// `deja init --db <file> [--template blank|demo|coding-agent] [--ns N]` —
/// seed a working backend and print the wiring snippet (never writes settings).
fn run_init(
    m: DejaDB,
    ns: &str,
    flags: &HashMap<String, String>,
    _positional: &[String],
) -> Result<(), String> {
    let template = flag(flags, "template").unwrap_or_else(|| "blank".to_string());
    let mut m = m;
    let seeded = match template.as_str() {
        "blank" => 0,
        "demo" => seed_demo(&mut m, ns)?,
        "coding-agent" | "support-agent" => {
            m.add(&Fact::new("agent", "instruction", "review pending recommendations before acting").namespace(ns))
                .map_err(|e| e.to_string())?;
            1
        }
        other => return Err(format!("unknown --template '{other}' (blank|demo|coding-agent|support-agent)")),
    };

    println!("initialized backend (template: {template}, seeded {seeded} grain(s))");
    if template == "demo" {
        println!("next: deja waiser run --db <file>   # ~4 recommendations across analyzers");
    }
    // Print the Claude Code hook snippet — deja never edits your settings.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "deja".to_string());
    eprintln!("\nClaude Code hooks (paste into settings.json — absolute path baked in):");
    eprintln!("  UserPromptSubmit → {exe} recall-hook --ns {ns} --with-waiser");
    eprintln!("  Stop             → {exe} capture-stop --ns {ns}");
    eprintln!("  SessionEnd       → {exe} waiser run --min-new 20 --min-new-errors 3 --quiet --ns {ns}");
    Ok(())
}

/// Seed the demo corpus: planted duplicates, a contradiction, and a stale
/// grain, so the first `deja waiser run` fires several analyzers at once.
fn seed_demo(m: &mut DejaDB, ns: &str) -> Result<usize, String> {
    let past = 1_000_000_000_000; // year 2001 — safely elapsed
    let grains = [
        Fact::new("acme", "tier", "Enterprise").namespace(ns),
        Fact::new("acme", "tier", "Enterprise").namespace(ns),
        Fact::new("acme", "deploy_target", "us-east-1").namespace(ns),
        Fact::new("acme", "deploy_target", "eu-west-1").namespace(ns),
        Fact::new("promo", "active", "true").namespace(ns).valid_to(past),
    ];
    for g in &grains {
        m.add(g).map_err(|e| e.to_string())?;
    }
    Ok(grains.len())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("deja: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> (HashMap<String, String>, Vec<String>) {
        parse_args(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn valueless_long_flag_does_not_swallow_short_flag() {
        // Regression: in `serve --mcp -d mem.db`, the valueless `--mcp` must not
        // consume `-d` as its value — otherwise --db is never set and serve
        // silently falls back to the default memory file.
        let (flags, pos) = args(&["--mcp", "-d", "mem.db", "--ns", "caller"]);
        assert_eq!(flags.get("mcp").map(String::as_str), Some("true"));
        assert_eq!(flags.get("db").map(String::as_str), Some("mem.db"));
        assert_eq!(flags.get("ns").map(String::as_str), Some("caller"));
        assert!(pos.is_empty(), "no stray positionals: {pos:?}");
    }

    #[test]
    fn db_flag_equivalent_across_form_and_order() {
        for a in [
            &["--mcp", "-d", "mem.db"][..],
            &["-d", "mem.db", "--mcp"][..],
            &["--mcp", "--db", "mem.db"][..],
            &["--db", "mem.db", "--mcp"][..],
        ] {
            let (flags, _) = args(a);
            assert_eq!(flags.get("db").map(String::as_str), Some("mem.db"), "args: {a:?}");
            assert_eq!(flags.get("mcp").map(String::as_str), Some("true"), "args: {a:?}");
        }
    }

    #[test]
    fn adjacent_valueless_flags_stay_true() {
        let (flags, _) = args(&["--mcp", "--no-destructive-ops"]);
        assert_eq!(flags.get("mcp").map(String::as_str), Some("true"));
        assert_eq!(flags.get("no-destructive-ops").map(String::as_str), Some("true"));
    }
}
