//! deja — the DejaDB CLI (brand DejaDB, package dejadb, run `deja`).
//!
//! Thin shell over dejadb-store + dejadb-cal. One memory = one file.

mod corpus;

use std::collections::HashMap;
use std::process::ExitCode;

use dejadb_cal::{CalExecutor, CalExecutorConfig, DejaDbFacade};
use dejadb_core::error::Hash;
use dejadb_core::types::{ContentBlock, Event, Fact, Grain, TokenUsage, Tool};
use dejadb_store::{Axis, DejaDB, Direction};
use dejadb_loop::{now_ms, DejaDbSubstrate};
use deja_loop::{Decision, Engine, ObserverType, Policy, RecStatus, RunOptions, ScopeSet, Severity};

const USAGE: &str = "\
deja — embedded memory engine for AI agents (OMS + CAL on Turso)

USAGE:
  deja <command> [--db <memory.db>] [options]   (-d = --db)
  deja --version | -V                 print the version and exit
  deja help | --help | -h             show this help

COMMANDS:
  init     [--template blank|demo|coding-agent] [--ns NS]   seed a backend +
           print the Claude Code hook snippet (never writes your settings)
  loop     <run|reflect|list|show|approve|reject|apply|rollback|analyzers|policy>
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
           [--policy FILE] grants auto-apply (else $DEJA_LOOP_POLICY); `policy` prints it
  add      <subject> <relation> <object>       store a fact (positional)
           [--subject S --relation R --object O] [--ns NS] [--confidence C]
           [--idempotent]   no-op if this exact value is already the head
  record-tool-call --name NAME [--input JSON] --result TEXT [--is-error]
           [--thread ID] [--call-id ID] [--ns NS]   append one tool invocation
  run-manifest --run-id ID --config JSON   persist a reproducible harness
           configuration and its run link in agent:harness
  recall   <subject> | --subject S   [--relation R] [--ns NS] [-k N]
           [--render sml|toon|markdown|plain|json] [--budget TOKENS]
  cal      <QUERY> [--ns NS]          execute a CAL statement
  corpus   --select '<READ CAL>' [--out FILE] [--recipient ID]
                                      stream governed OpenAI chat
                                      JSONL and record its provenance registry
  search   --query TEXT [--subject S] [-k N]   hybrid recall (BM25 + structural, RRF)
  history  --subject S --relation R [--ns NS]
  provenance <source-hash>            grains distilled from a source (reverse)
  forks                               open forks (>1 head for a subject+relation)
  merge    --subject S --relation R --object O   close a fork with a resolved value
  subject-report <subject> [--ns NS] [--text-mentions] [--out FILE] [--bundle FILE]
                                      DSAR read (GDPR Art. 15/20): everything
                                      forget-subject WOULD erase — exact +
                                      partition keys, history — as JSONL
                                      (stdout or --out), and optionally a
                                      portable .mgb bundle (--bundle)
  forget-subject <subject> [--ns NS] [--text-mentions] --yes   erase EVERY
                                      grain referencing an identity — exact +
                                      partition keys (pat, pat#visit1), history
                                      included, + its dictionary entries;
                                      --text-mentions also erases grains whose
                                      indexed text mentions the identity;
                                      replicates as tombstones
  purge-older-than <days> [--ns NS] [--type event] --yes   retention sweep:
                                      erase grains older than N days
                                      (--ns \"\" sweeps every namespace)
  retention <set|list|clear|sweep> [--days N] [--ns NS] [--type event]
           [--because \"why\"] [--yes]  declarative storage limitation: the
                                      policy is a file-truth that travels
                                      with the memory; `set` declares,
                                      `sweep --yes` enforces (audited)
  blobs encrypt                       encrypt CAS attachments written before
                                      sidecar encryption landed (needs the
                                      memory's key; new blobs are sealed on
                                      write). Idempotent
  audit export [--since MS] [--until MS] [--out FILE]   accountability
                                      evidence as JSONL: every destructive
                                      op (who/what/why/how many) + the loop
                                      lifecycle chain, hash-chain verified
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
  stream   --to DIR [--interval-ms N] [--once] [--checkpoint] [--retain 30d]
                                      continuous op-log shipping; --checkpoint
                                      opens a new generation with a full
                                      snapshot, --retain drops generations
                                      older than the window (so erasure
                                      reaches archives — see docs/gdpr.md)
  restore  --from DIR [--until-hlc T]            rebuild from stream dir (PITR)
  follow   --from DIR [--interval-ms N] [--once]  subscribe: apply new segments
                                                  (org/category distribution)
  related  --start TERM --relations R1,R2 [--direction out|in|both]
           [--depth N] [--limit N]    walk the entity graph (bounded k-hop).
                                      in/both only see relations the file
                                      declares entity-valued
  entity-at --subject S --relation R --at MS [--axis world|knowledge]
                                      as-of read: what was true at T (world)
                                      or what was known at T (knowledge)
  step-actions --workflow HASH [--node ID] [--limit N]
                                      execution records for a workflow —
                                      which grains ran which of its nodes
  run-trace --run-id ID [--limit N]   what a run recorded, and what it
                                      produced downstream (facts/lessons)
  runs-touching --hash H [--depth N]  which runs produced or refined a grain
                                      (walks provenance both ways)
  verify                              integrity + content-address recheck
  stats                               store counters
  serve    --mcp [--ns NS] [--mount alias=path,...] [--no-destructive-ops] [--lock-ns NS]  MCP server on stdio
                                      (--mount adds read-only files for
                                       cross-file ASSEMBLE; ns \"alias.inner\")
  repl     [--ns NS]                  interactive CAL console in the terminal
  remember --content TEXT [--facts JSON] [--observer ID]
           [--session-id ID] [--run-id ID] [--role user|assistant|system|tool]
           [--model SPEC | --llm-cmd CMD] [--extract-hint TEXT]
           [--ground-model SPEC | --ground-cmd CMD]
           [--min-confidence F] [--dry-run]
                                      store free text as an Event, then
                                      attach the facts distilled from it.
                                      --facts is host-supplied and skips the
                                      model; --model/--llm-cmd extract with an
                                      LLM (stamped verification_status
                                      \"unverified\" unless --ground-* checks
                                      them). --dry-run prints, stores nothing.
  hook     claude-code               print settings.json hook snippet
                                      (auto recall-before-prompt + capture-on-stop)
  memtool  '<COMMAND-JSON>'           Anthropic memory-tool ops on grains
  ui       [--addr HOST:PORT] [--allow-remote] [--token-env VAR] [--no-destructive-ops]  web console (default 127.0.0.1:7437)
  hub      --token-env VAR [--dir DIR] [--addr HOST:PORT] [--allow-remote]
           [--retain 30d]             sync hub: many apps, one shared memory
                                      (segment push/pull; default 127.0.0.1:7438);
                                      --retain checkpoints at startup and drops
                                      segments older than the window

Namespace defaults to \"shared\". Exit code 0 on success.
--db is optional for one-shot commands: it falls back to $DEJADB_DB, then
~/.dejadb/default.db. `serve` and `ui` require an explicit --db (or $DEJADB_DB).
Files carry their own settings (text index, entity relations, embedding
provenance) in an internal meta table; a bare open honors them.
--index-text true|false explicitly re-stamps the file's declaration.
--assembly-manifest-sample-rate 0..1 samples ASSEMBLE provenance into
agent:harness (host-only, default 0/off).

Principals: add --as <principal> to cal/repl/serve/subject-report/forget-subject/
purge-older-than to run the session under the file's grants for that
principal (fail closed; see GRANT in docs/cal-reference.md §9). --auth FILE
validates the principal against a credential map, and `deja ui --auth FILE`
serves the console in multi-principal mode (tokens → principals,
unauthenticated = read-only \"anonymous\").

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

fn assembly_manifest_sample_rate(flags: &HashMap<String, String>) -> Result<f64, String> {
    let Some(raw) = flag(flags, "assembly-manifest-sample-rate") else {
        return Ok(0.0);
    };
    let rate: f64 = raw
        .parse()
        .map_err(|_| "--assembly-manifest-sample-rate must be a number from 0 to 1".to_string())?;
    if !rate.is_finite() || !(0.0..=1.0).contains(&rate) {
        return Err("--assembly-manifest-sample-rate must be from 0 to 1".into());
    }
    Ok(rate)
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
/// Open a memory on the postgres backend from a `postgres://…?schema=<name>`
/// DSN, mirroring the file-backend branches (explicit options re-stamp;
/// telemetry rides the memory's schema). Encryption keys don't apply here —
/// the `--passphrase-env` path derives them from a `.kdf` sidecar next to a
/// FILE, so a DSN is rejected before derivation.
#[cfg(feature = "postgres")]
fn open_postgres_store(
    db: &str,
    tel_mode: dejadb_store::TelemetryMode,
    explicit_index: Option<&str>,
) -> Result<DejaDB, String> {
    let (url, schema) = dejadb_store::pg::split_schema_url(db).map_err(|e| e.to_string())?;
    if let Some(v) = explicit_index {
        let o = dejadb_store::DejaDbOptions {
            index_text: !matches!(v, "false" | "0" | "off" | "no"),
            telemetry: tel_mode,
            ..Default::default()
        };
        DejaDB::open_postgres_with(&url, &schema, o)
    } else if tel_mode != dejadb_store::TelemetryMode::Off {
        DejaDB::open_postgres_with_telemetry(&url, &schema, tel_mode)
    } else {
        DejaDB::open_postgres(&url, &schema)
    }
    .map_err(|e| e.to_string())
}

#[cfg(not(feature = "postgres"))]
fn open_postgres_store(
    _db: &str,
    _tel_mode: dejadb_store::TelemetryMode,
    _explicit_index: Option<&str>,
) -> Result<DejaDB, String> {
    Err("this build lacks the postgres backend — reinstall with \
         `cargo install dejadb --features postgres` (or build with --features postgres)"
        .into())
}

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

/// Bind the session principal from `--as` (validated against `--auth`'s
/// credential map when one is given): rights become exactly what the file's
/// grant grains cover for that principal — fail closed. Absent `--as`, the
/// local session stays the owner.
fn apply_principal(
    facade: dejadb_cal::DejaDbFacade,
    flags: &std::collections::HashMap<String, String>,
) -> Result<dejadb_cal::DejaDbFacade, String> {
    let Some(p) = flag(flags, "as") else {
        return Ok(facade);
    };
    if let Some(path) = flag(flags, "auth") {
        let text = std::fs::read_to_string(&path).map_err(|e| format!("--auth {path}: {e}"))?;
        let map = dejadb_core::authz::CredentialMap::from_json(&text).map_err(|e| e.to_string())?;
        map.knows_principal(&p).map_err(|e| e.to_string())?;
    }
    let bound = facade.with_principal(&p).map_err(|e| e.to_string())?;
    eprintln!("deja: session bound to principal '{p}' (rights from the file's grants)");
    Ok(bound)
}

/// Record a Tier-2 audit Observation for a host-level erasure (GDPR
/// Art. 5(2)/30). The CLI is a host: REQ-ERASE-5 keeps the audit grain out
/// of the *engine* so an erasure never writes the identity back, and leaves
/// the record to whoever invoked it — which for `deja forget-subject` /
/// `purge-older-than` is this. Uses the same builder as the CAL path so
/// `deja audit export` reads one shape, and the principal is `--as` when
/// given (else the local owner session).
///
/// Best-effort by construction: the erasure has already committed and
/// replicated. A failure to append the audit grain is reported loudly on
/// stderr but must not turn an accomplished erasure into an error exit —
/// that would tell the operator the deletion failed when it did not.
fn write_erasure_audit(
    m: &mut dejadb_store::DejaDB,
    flags: &HashMap<String, String>,
    verb: &str,
    target: &str,
    count: usize,
    stale_exports: &[dejadb_store::CorpusExportRegistry],
) {
    let principal = flag(flags, "as").unwrap_or_else(|| "local:owner".to_string());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let because = flag(flags, "because");
    let mut obs = dejadb_core::authz::audit_observation(
        &principal,
        verb,
        target,
        because.as_deref(),
        count,
        now,
    );
    if !stale_exports.is_empty() {
        let mut context = obs
            .common
            .context
            .take()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        context.insert(
            "stale_corpora".into(),
            serde_json::to_value(stale_exports).unwrap_or_else(|_| serde_json::json!([])),
        );
        obs.common.context = Some(serde_json::Value::Object(context));
        for export in stale_exports {
            let recipient = export
                .recipient
                .as_deref()
                .map(|id| format!(" for recipient {id}"))
                .unwrap_or_default();
            eprintln!(
                "deja: corpus {} at {}{} is stale and must be retired or re-derived",
                export.export_id, export.destination, recipient
            );
        }
    }
    if let Err(e) = m.add(&obs) {
        eprintln!("deja: warning: erasure succeeded but its audit record failed to write: {e}");
    }
}

/// True when a `host:port` (or bare host) names a loopback interface. Used to
/// refuse binding the unauthenticated `ui` console to a routable address.
/// Print any saved query or template the file carries that this build cannot
/// load, the same way `open_warnings` is printed.
///
/// Forces the registries to rehydrate first — they load lazily, so a verb that
/// never touches one would otherwise report nothing and the operator would find
/// out by noticing a saved query had gone missing.
fn report_meta_warnings(facade: &dejadb_cal::DejaDbFacade) {
    use dejadb_cal::CalStoreFacade;
    let _ = facade.list_queries();
    let _ = facade.list_templates();
    for w in facade.meta_warnings() {
        eprintln!("deja: warning: {w}");
    }
}

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

/// Map one Claude Code transcript message onto OMS Event content blocks.
/// Tool-result bodies are bounded so a single command cannot exceed the grain
/// size cap; every elision is explicit and records the original byte length.
fn transcript_content_blocks(
    content: &serde_json::Value,
    turn: usize,
) -> (String, Vec<ContentBlock>, Vec<serde_json::Value>) {
    const TOOL_RESULT_CHARS: usize = 32 * 1024;

    fn truncate(s: &str, max: usize) -> (String, bool) {
        if s.chars().count() <= max {
            (s.to_string(), false)
        } else {
            (s.chars().take(max).collect(), true)
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
    let input_blocks: Vec<&serde_json::Value> = match content {
        serde_json::Value::String(_) => Vec::new(),
        serde_json::Value::Array(blocks) => blocks.iter().collect(),
        _ => Vec::new(),
    };
    if let serde_json::Value::String(s) = content {
        return (
            s.clone(),
            vec![ContentBlock::Text { text: s.clone() }],
            Vec::new(),
        );
    }

    let mut rendered = Vec::new();
    let mut typed = Vec::new();
    let mut elisions = Vec::new();
    for (block_index, b) in input_blocks.into_iter().enumerate() {
        match b["type"].as_str() {
            Some("text") => {
                if let Some(text) = b["text"].as_str() {
                    rendered.push(text.to_string());
                    typed.push(ContentBlock::Text { text: text.to_string() });
                }
            }
            Some("tool_use") => {
                let name = b["name"].as_str().unwrap_or("tool").to_string();
                let id = b["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("synthetic:{turn}:{block_index}"));
                let input = b.get("input").cloned().unwrap_or(serde_json::Value::Null);
                rendered.push(format!("[tool_use {name}] {input}"));
                typed.push(ContentBlock::ToolUse { id, name, input });
            }
            Some("tool_result") => {
                let original = tool_result_body(&b["content"]);
                let original_bytes = original.len();
                let (body, elided) = truncate(&original, TOOL_RESULT_CHARS);
                let is_error = b.get("is_error").and_then(serde_json::Value::as_bool);
                let flag = if is_error == Some(true) { " ERROR" } else { "" };
                rendered.push(format!("[tool_result{flag}] {body}"));
                typed.push(ContentBlock::ToolResult {
                    tool_use_id: b["tool_use_id"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("synthetic:{turn}:{block_index}")),
                    content: body,
                    is_error,
                });
                if elided {
                    elisions.push(serde_json::json!({
                        "block_index": block_index,
                        "kind": "tool_result",
                        "original_bytes": original_bytes,
                        "kept_chars": TOOL_RESULT_CHARS,
                    }));
                }
            }
            // Bare `{text: ...}` blocks occur in older transcripts.
            _ => {
                if let Some(text) = b["text"].as_str() {
                    rendered.push(text.to_string());
                    typed.push(ContentBlock::Text { text: text.to_string() });
                }
            }
        }
    }
    (rendered.join("\n"), typed, elisions)
}

/// The turn's own timestamp in epoch milliseconds, if the transcript carries
/// one — epoch integers, numeric strings, or the ISO-8601 form Claude Code
/// writes.
///
/// `capture-stop` pins `created_at` from this so a turn's content address is
/// stable across hook firings. Without a stable stamp the cumulative transcript
/// re-hashes every turn on every firing, and the dedup that makes the capture
/// idempotent stops working.
fn transcript_turn_ms(record: &serde_json::Value) -> Option<i64> {
    let value = record.get("timestamp").or_else(|| record.get("created_at"))?;
    if let Some(ms) = value.as_i64() {
        return Some(ms);
    }
    let text = value.as_str()?;
    if let Ok(ms) = text.parse::<i64>() {
        return Some(ms);
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

fn transcript_token_usage(message: &serde_json::Value) -> Option<TokenUsage> {
    let usage = message.get("usage")?.as_object()?;
    let u32_field = |primary: &str, alias: &str| {
        usage
            .get(primary)
            .or_else(|| usage.get(alias))
            .and_then(serde_json::Value::as_u64)
            .map(|n| n.min(u32::MAX as u64) as u32)
    };
    Some(TokenUsage {
        input_tokens: u32_field("input_tokens", "input").unwrap_or(0),
        output_tokens: u32_field("output_tokens", "output").unwrap_or(0),
        cache_read_tokens: u32_field("cache_read_tokens", "cache_read_input_tokens"),
        cache_creation_tokens: u32_field(
            "cache_creation_tokens",
            "cache_creation_input_tokens",
        ),
    })
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
                for (record_index, record) in extract_tool_records(&v).into_iter().enumerate() {
                    use sha2::{Digest, Sha256};
                    let synthetic_call_id = || {
                        let mut digest = Sha256::new();
                        digest.update(file.as_bytes());
                        digest.update([0]);
                        digest.update(line.as_bytes());
                        digest.update((i as u64).to_le_bytes());
                        digest.update((record_index as u64).to_le_bytes());
                        format!("import:{}", &hex::encode(digest.finalize())[..24])
                    };
                    let mut tool = Tool::new(&record.name)
                        .is_error(record.is_error)
                        // Unknown source time is pinned, not replaced with the
                        // import wall clock, so rerunning the same import can
                        // actually deduplicate it.
                        .created_at(record.created_at.unwrap_or(0));
                    if let Some(input) = record.input {
                        tool = tool.input(input);
                    }
                    if !record.content.is_empty() {
                        tool = tool.content(&record.content);
                    }
                    // The per-record digest is stable across reruns but
                    // distinguishes identical retries on separate lines.
                    let call_id = record.call_id.unwrap_or_else(synthetic_call_id);
                    tool = tool.tool_call_id(&call_id);
                    let tool = tool.namespace(ns);
                    let (_, hash) = dejadb_core::format::serialize::serialize_grain(&tool)
                        .map_err(|e| e.to_string())?;
                    if m.get(&hash).is_ok() {
                        rep.skipped += 1;
                    } else {
                        m.add(&tool).map_err(|e| e.to_string())?;
                        rep.added += 1;
                    }
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

#[derive(Debug, Clone, PartialEq)]
struct ImportedToolRecord {
    name: String,
    input: Option<serde_json::Value>,
    content: String,
    is_error: bool,
    call_id: Option<String>,
    created_at: Option<i64>,
}

/// Extract typed tool records from one tool-log JSONL
/// line: a direct `{tool_name, content, is_error}` record, an OpenAI
/// `role:"tool"` result, or an assistant message carrying a `tool_calls` array.
fn extract_tool_records(v: &serde_json::Value) -> Vec<ImportedToolRecord> {
    let stringify = |x: &serde_json::Value| match x {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let created_at = v
        .get("created_at")
        .or_else(|| v.get("timestamp"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))
        });
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
                let input = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .or_else(|| c.get("input"))
                    .map(|value| match value {
                        serde_json::Value::String(raw) => serde_json::from_str(raw)
                            .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
                        other => other.clone(),
                    });
                let is_error = c
                    .get("is_error")
                    .and_then(|e| e.as_bool())
                    .unwrap_or_else(|| c.get("error").is_some());
                let call_id = c
                    .get("id")
                    .or_else(|| c.get("tool_call_id"))
                    .and_then(|id| id.as_str())
                    .map(str::to_string);
                Some(ImportedToolRecord {
                    name: name.to_string(),
                    input,
                    content: String::new(),
                    is_error,
                    call_id,
                    created_at,
                })
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
            let input = v.get("input").or_else(|| v.get("arguments")).map(|value| match value {
                serde_json::Value::String(raw) => serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.clone())),
                other => other.clone(),
            });
            let call_id = v
                .get("tool_call_id")
                .or_else(|| v.get("call_id"))
                .or_else(|| v.get("id"))
                .and_then(|id| id.as_str())
                .map(str::to_string);
            vec![ImportedToolRecord {
                name: name.to_string(),
                input,
                content,
                is_error,
                call_id,
                created_at,
            }]
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
    let db = resolve_db(&flags, matches!(cmd.as_str(), "serve" | "ui" | "hub"))?;
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
      "command": "{exe} recall-hook --db {db} --ns {ns} --with-loop"
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
    let is_pg_url = db.starts_with("postgres://") || db.starts_with("postgresql://");
    let enc_key = match flag(&flags, "passphrase-env") {
        Some(_) if is_pg_url => {
            return Err(
                "--passphrase-env applies to file-backed memories (page cipher + .kdf \
                 sidecar); on the postgres backend use TDE/pgcrypto at the deployment layer"
                    .into(),
            );
        }
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
    let mut m = if is_pg_url {
        open_postgres_store(&db, tel_mode, explicit_index.as_deref())?
    } else {
        if explicit_index.is_some() || enc_key.is_some() {
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
        .map_err(|e| {
            let mut msg = e.to_string();
            // A .kdf sidecar means the file was created encrypted; opening it
            // without a key fails with an unhelpful "not a database" — say why.
            if enc_key.is_none() && std::path::Path::new(&format!("{db}.kdf")).exists() {
                msg.push_str(&format!(
                    " — {db}.kdf exists, so this memory is encrypted; pass --passphrase-env <VAR>"
                ));
            }
            msg
        })?
    };
    for w in m.open_warnings() {
        eprintln!("deja: warning: {w}");
    }
    // Host-scoped trajectory context for recall telemetry. The same flag is
    // also consumed by run-manifest/run-trace commands; it is not a file
    // declaration.
    m.set_run_id(flag(&flags, "run-id").as_deref());

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
        "record-tool-call" => {
            let name = flag(&flags, "name")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "usage: deja record-tool-call --name NAME --result TEXT [--input JSON] [--is-error] [--thread ID] [--call-id ID]".to_string())?;
            let result = flag(&flags, "result")
                .or_else(|| positional.get(1).cloned())
                .ok_or_else(|| "record-tool-call requires --result TEXT".to_string())?;
            let input = flag(&flags, "input");
            let is_error = flag(&flags, "is-error")
                .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                .unwrap_or(false);
            let facade = DejaDbFacade::with_session(m, Some(ns.clone()), None);
            let facade = apply_principal(facade, &flags)?;
            let hash = facade
                .record_tool_call(
                    &ns,
                    &name,
                    input.as_deref(),
                    &result,
                    is_error,
                    flag(&flags, "thread").as_deref(),
                    flag(&flags, "call-id").as_deref(),
                )
                .map_err(|e| e.to_string())?;
            println!("{hash}");
        }
        "run-manifest" => {
            let run_id = flag(&flags, "run-id")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| "run-manifest requires --run-id ID".to_string())?;
            let config = flag(&flags, "config")
                .ok_or_else(|| "run-manifest requires --config JSON".to_string())?;
            let facade = DejaDbFacade::with_session(m, Some(ns), None);
            let facade = apply_principal(facade, &flags)?;
            let (config_hash, link_hash) = facade
                .record_run_manifest(&run_id, &config)
                .map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::json!({
                    "run_id": run_id,
                    "config_hash": config_hash.to_hex(),
                    "link_hash": link_hash.to_hex(),
                })
            );
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
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                assembly_manifest_sample_rate: assembly_manifest_sample_rate(&flags)?,
                ..CalExecutorConfig::default()
            })
            .with_governance(std::sync::Arc::new(dejadb_loop::LoopGovernance::new()));
            let res = ex.execute(&query, &facade).map_err(|e| e.to_string())?;
            let payload = serde_json::to_string_pretty(&res.result).map_err(|e| e.to_string())?;
            println!("{payload}");
            for w in res.warnings {
                eprintln!("warning: {w}");
            }
        }
        "corpus" => {
            use dejadb_cal::classify::{classify, StatementClass};
            use dejadb_cal::executor::CalResultPayload;
            use std::io::BufWriter;

            let selector = flag(&flags, "select")
                .or_else(|| positional.first().cloned())
                .ok_or_else(|| {
                    "usage: deja corpus --select '<READ CAL>' [--out FILE] [--recipient ID]"
                        .to_string()
                })?;
            let parsed = dejadb_cal::parse(&selector).map_err(|e| e.to_string())?;
            if classify(&parsed.statement) != StatementClass::Read {
                return Err("corpus selector must classify as a read-only CAL statement".into());
            }
            let facade = DejaDbFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            // The selector's read grant and the immutable registry write are
            // separate capabilities. Fail before creating an output file.
            facade.authorize_corpus_export().map_err(|e| e.to_string())?;
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: false,
                ..CalExecutorConfig::default()
            });
            let result = ex.execute(&selector, &facade).map_err(|e| e.to_string())?;
            let grains = match result.result {
                CalResultPayload::Grains { grains, .. }
                | CalResultPayload::Assembled { grains, .. }
                | CalResultPayload::Formatted { grains, .. }
                | CalResultPayload::MultiFormatted { grains, .. } => grains,
                other => {
                    return Err(format!(
                        "corpus selector must return grains, got {}",
                        serde_json::to_value(other)
                            .ok()
                            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
                            .unwrap_or_else(|| "non-grain result".into())
                    ));
                }
            };
            let destination = flag(&flags, "out").unwrap_or_else(|| "stdout".to_string());
            let recipient = flag(&flags, "recipient");
            let summary = if destination == "stdout" {
                let stdout = std::io::stdout();
                corpus::write_jsonl(grains, BufWriter::new(stdout.lock()))?
            } else {
                let file = std::fs::File::create(&destination)
                    .map_err(|e| format!("{destination}: {e}"))?;
                corpus::write_jsonl(grains, BufWriter::new(file))?
            };
            let exported_at = now_ms();
            let manifest = facade
                .record_corpus_export(
                    &selector,
                    &destination,
                    recipient.as_deref(),
                    exported_at,
                    &summary.subject_fingerprints,
                    &summary.source_hashes,
                )
                .map_err(|e| e.to_string())?;
            eprintln!(
                "corpus export: {} row(s), {} source grain(s), manifest {}",
                summary.rows,
                summary.source_hashes.len(),
                manifest
            );
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
            // Litestream-shaped: a full snapshot as segment 0 of each
            // generation, then incrementing delta segments. Cursor,
            // generation, and NEXT SEGMENT NUMBER live in the target dir,
            // so any machine holding the dir can restore.
            //
            // The segment number is tracked in CURSOR rather than derived
            // by counting directory entries: retention deletes old
            // generations, and a counted number would regress and
            // overwrite live segments.
            let to = need(&flags, "to")?;
            let interval: u64 = flag(&flags, "interval-ms").and_then(|v| v.parse().ok()).unwrap_or(500);
            let once = flags.contains_key("once");
            let checkpoint = flags.contains_key("checkpoint");
            let retain_ms = match flag(&flags, "retain") {
                Some(v) => Some(
                    parse_duration(&v)
                        .ok_or_else(|| format!("--retain takes a duration like 30d, got '{v}'"))?,
                ),
                None => None,
            };
            std::fs::create_dir_all(&to).map_err(|e| e.to_string())?;
            let cursor_path = format!("{to}/CURSOR");
            let (mut gen_id, mut cursor, mut seg) = read_stream_cursor(&cursor_path, &to);
            // A checkpoint (explicit, or the first run in an empty dir)
            // opens a NEW generation whose segment 0 is a full snapshot of
            // the live — already-erased — store. That is what lets
            // retention drop older generations without losing history:
            // everything still reachable is in this snapshot.
            if gen_id.is_empty() || checkpoint {
                let g = format!(
                    "{:016x}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as u64
                );
                std::fs::create_dir_all(format!("{to}/gen-{g}")).map_err(|e| e.to_string())?;
                let snap = format!("{to}/gen-{g}/segment-{:08}.mgb", 0);
                let st = m.bundle_since(0, &snap).map_err(|e| e.to_string())?;
                gen_id = g;
                cursor = st.last_op_seq;
                seg = 1;
                std::fs::write(&cursor_path, format!("{gen_id} {cursor} {seg}"))
                    .map_err(|e| e.to_string())?;
                eprintln!("checkpoint: full snapshot of {} ops → {snap}", st.ops);
                if let Some(window) = retain_ms {
                    let dropped = prune_generations(&to, &gen_id, window)?;
                    if dropped > 0 {
                        eprintln!(
                            "retention: dropped {dropped} generation(s) older than the window \
                             (their contents are in this checkpoint, minus anything erased since)"
                        );
                    }
                }
            }
            loop {
                let ops_now = m.changes_since(cursor, 1).map_err(|e| e.to_string())?;
                if !ops_now.is_empty() {
                    let seg_path = format!("{to}/gen-{gen_id}/segment-{seg:08}.mgb");
                    let st = m.bundle_since(cursor, &seg_path).map_err(|e| e.to_string())?;
                    cursor = st.last_op_seq;
                    seg += 1;
                    std::fs::write(&cursor_path, format!("{gen_id} {cursor} {seg}"))
                        .map_err(|e| e.to_string())?;
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
            // The follower cursor lives beside a FILE db; a DSN has no
            // "beside", so a postgres-backed follower keeps its cursor in
            // the stream dir, keyed by the memory's schema.
            #[cfg(feature = "postgres")]
            let fcur_path = if is_pg_url {
                let (_, schema) =
                    dejadb_store::pg::split_schema_url(&db).map_err(|e| e.to_string())?;
                format!("{from}/{schema}.follow")
            } else {
                format!("{db}.follow")
            };
            #[cfg(not(feature = "postgres"))]
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
                    // Re-baseline: the generation this follower was tracking
                    // may have aged out under `--retain`. Its segments are
                    // gone, but segment 0 of the CURRENT generation is a full
                    // snapshot — so restart from there rather than stalling
                    // forever on a missing file. Replay is idempotent, so
                    // re-importing already-held grains is a no-op.
                    if !fgen.is_empty()
                        && fgen != gen_id
                        && !std::path::Path::new(&format!("{from}/gen-{fgen}")).exists()
                    {
                        eprintln!(
                            "follow: generation {fgen} has aged out of the stream dir — \
                             re-baselining from generation {gen_id}'s snapshot"
                        );
                    }
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
            println!(
                "text index rebuilt: {} grains indexed ({n} needed their text backfilled)",
                m.indexed_documents()
            );
            // Secondary indexes added after 1.0: reverse provenance, run
            // correlation, and related_to cross-links. A file written before
            // they existed answers provenance and run questions with nothing
            // until this runs.
            let links = m.rebuild_link_indexes().map_err(|e| e.to_string())?;
            println!("link indexes rebuilt: {links} rows (provenance, runs, cross-links)");
        }
        "related" => {
            let start = flag(&flags, "start").ok_or("related requires --start")?;
            let rels = dejadb_store::parse_relations(
                &flag(&flags, "relations").ok_or("related requires --relations")?,
            );
            if rels.is_empty() {
                return Err("--relations must name at least one relation".to_string());
            }
            let dir = Direction::parse(&flag(&flags, "direction").unwrap_or_default())
                .ok_or("--direction must be one of: out, in, both")?;
            let depth: usize = flag(&flags, "depth")
                .map_or(Ok(2), |d| d.parse())
                .map_err(|_| "--depth must be a number")?;
            let cap: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let refs: Vec<&str> = rels.iter().map(String::as_str).collect();
            let reached = m
                .related(&ns, &start, &refs, dir, depth, cap)
                .map_err(|e| e.to_string())?;
            if reached.is_empty() {
                println!("(nothing reachable from {start})");
            }
            for r in reached {
                println!("{r}");
            }
        }
        "entity-at" => {
            let subject = flag(&flags, "subject").ok_or("entity-at requires --subject")?;
            let relation = flag(&flags, "relation").ok_or("entity-at requires --relation")?;
            let at: i64 = flag(&flags, "at")
                .ok_or("entity-at requires --at (epoch ms)")?
                .parse()
                .map_err(|_| "--at must be epoch milliseconds")?;
            let axis = Axis::parse(&flag(&flags, "axis").unwrap_or_default())
                .ok_or("--axis must be one of: world, knowledge")?;
            match m
                .entity_at(&ns, &subject, &relation, at, axis)
                .map_err(|e| e.to_string())?
            {
                Some(g) => println!("{}", serde_json::to_string_pretty(&g).unwrap_or_default()),
                None => println!("(nothing known for {subject} {relation} at {at})"),
            }
        }
        "step-actions" => {
            let wf = flag(&flags, "workflow").ok_or("step-actions requires --workflow")?;
            let wf = Hash::from_hex(&wf).map_err(|e| e.to_string())?;
            let node = flag(&flags, "node");
            let limit: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let rows = m
                .step_actions(&ns, &wf, node.as_deref(), limit)
                .map_err(|e| e.to_string())?;
            if rows.is_empty() {
                println!("(no execution records for {})", wf.to_hex());
            }
            for (n, h) in rows {
                println!("{n}\t{}", h.to_hex());
            }
        }
        "run-trace" => {
            let run = flag(&flags, "run-id").ok_or("run-trace requires --run-id")?;
            let limit: usize = flag(&flags, "limit")
                .map_or(Ok(64), |l| l.parse())
                .map_err(|_| "--limit must be a number")?;
            let trace = m.run_trace(&ns, &run, limit).map_err(|e| e.to_string())?;
            let produced = m.run_yield(&ns, &run, limit).map_err(|e| e.to_string())?;
            if trace.is_empty() {
                println!("(no grains recorded for run {run})");
            }
            println!("recorded during {run}: {} grain(s)", trace.len());
            for g in &trace {
                println!("  {} {:?}", g.hash.to_hex(), g.grain_type);
            }
            println!("produced by {run}: {} grain(s)", produced.len());
            for g in &produced {
                println!("  {} {:?}", g.hash.to_hex(), g.grain_type);
            }
        }
        "runs-touching" => {
            let h = flag(&flags, "hash").ok_or("runs-touching requires --hash")?;
            let h = Hash::from_hex(&h).map_err(|e| e.to_string())?;
            let depth: usize = flag(&flags, "depth")
                .map_or(Ok(4), |d| d.parse())
                .map_err(|_| "--depth must be a number")?;
            let runs = m.runs_touching(&ns, &h, depth).map_err(|e| e.to_string())?;
            if runs.is_empty() {
                println!("(no run produced or refined {})", h.to_hex());
            }
            for r in runs {
                println!("{r}");
            }
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
            report_meta_warnings(&facade);
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
            let facade = apply_principal(facade, &flags)?;
            let mut server = dejadb_mcp::McpServer::new(facade, None)
                .assembly_manifest_sample_rate(assembly_manifest_sample_rate(&flags)?);
            if flags.contains_key("no-destructive-ops") {
                server = server.allow_destructive_ops(false);
                eprintln!("deja: destructive operations disabled (read-only session)");
            }
            if let Some(lock) = flag(&flags, "lock-ns") {
                server = server.lock_namespace(lock.clone());
                eprintln!("deja: namespace locked to '{lock}' (per-call namespace ignored)");
            }
            // Host loop policy (--policy FILE or $DEJA_LOOP_POLICY): the
            // dejadb_loop tool honors the same grants as the CLI run. Host
            // config set at process start — never controllable by the client.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_loop_policy(p);
                eprintln!("deja: loop host policy attached to dejadb_loop");
            }
            server.serve_stdio().map_err(|e| e.to_string())?;
        }
        "capture-stop" => {
            // Claude Code Stop-hook: JSON on stdin with session_id +
            // transcript_path. Store every ordered turn as a typed Event.
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
            // Claude's Stop hook points at the cumulative transcript, so every
            // firing re-reads turns already captured. Identify a turn by its
            // own content address rather than by its position in the thread: a
            // positional watermark counts grains any other writer put in the
            // same `(namespace, session)` — `deja remember --session-id`, a
            // binding, a second hook — and every foreign grain silently skips a
            // real turn. Re-adding a stored grain is already a free no-op, so
            // pinning `created_at` from the transcript makes the whole chain a
            // pure function of the transcript and the rerun idempotent by
            // construction.
            let policy_version = flag(&flags, "policy-version");
            let mut parent: Option<String> = None;
            let mut stored = 0;
            let mut skipped = 0;
            for (turn, line) in transcript.lines().enumerate() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                let message = &v["message"];
                let Some(role) = message["role"].as_str() else { continue };
                let Some(role) = dejadb_core::types::Role::from_str(role) else {
                    continue;
                };
                let (text, blocks, elisions) =
                    transcript_content_blocks(&message["content"], turn);
                if text.trim().is_empty() && blocks.is_empty() {
                    continue;
                }
                let mut event = Event::new(&text)
                    .namespace(&ns)
                    .session(session.clone())
                    // Deterministic stamp: the transcript's own time when it
                    // has one, else the turn's position. Never the wall clock —
                    // that would re-hash every turn on every hook firing and
                    // defeat the dedup this relies on.
                    .created_at(transcript_turn_ms(&v).unwrap_or(turn as i64))
                    .role(role)
                    .content_blocks(blocks);
                if let Some(p) = parent.clone() {
                    event = event.parent_message(p);
                }
                if let Some(model) = message["model"].as_str().or_else(|| v["model"].as_str()) {
                    event = event.model(model.to_string());
                }
                if let Some(stop) = message["stop_reason"]
                    .as_str()
                    .or_else(|| v["stop_reason"].as_str())
                {
                    event = event.stop_reason(stop.to_string());
                }
                if let Some(usage) = transcript_token_usage(message) {
                    event = event.token_usage(usage);
                }
                if !elisions.is_empty() {
                    event
                        .common
                        .extra_fields
                        .insert("elisions".into(), serde_json::Value::Array(elisions));
                }
                // The policy in force when the turn happened. `deja corpus`
                // reports it under `binding.policy_versions`, which is how a
                // training row states which governance regime produced it —
                // without a producer that binding was always empty.
                if let Some(policy) = policy_version.as_deref() {
                    event.common.context = Some(serde_json::json!({
                        "policy_version": policy,
                    }));
                }
                // A hook capture is not a run; run_id deliberately stays None.
                let (_, hash) = dejadb_core::format::serialize::serialize_grain(&event)
                    .map_err(|e| e.to_string())?;
                if m.get(&hash).is_ok() {
                    skipped += 1;
                } else {
                    m.add(&event).map_err(|e| e.to_string())?;
                    stored += 1;
                }
                parent = Some(hash.to_hex());
            }
            println!("captured {stored} events for session {session} ({skipped} already stored)");
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
            // --with-loop additionally injects the pending recommendation
            // queue (a compact, capped block) so the loop closes into the
            // agent's context instead of waiting to be polled.
            let with_loop = flag(&flags, "with-loop").is_some();
            let grains = m
                .recall_hybrid(&ns, None, None, Some(&query), k, None)
                .map_err(|e| e.to_string())?;
            if grains.is_empty() && !with_loop {
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
            if with_loop {
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
                        "\nDeja Loop: {} pending recommendation(s) for this memory (review with `deja loop list`, act with approve/apply/reject --because):",
                        pending.len()
                    );
                    for r in pending.iter().take(CAP) {
                        let origin = match r.origin {
                            deja_loop::Origin::Llm { .. } => " [llm]",
                            deja_loop::Origin::Command { .. } => " [external]",
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
        "remember" => run_remember(m, &ns, &flags)?,
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
            report_meta_warnings(&facade);
            let facade = apply_principal(facade, &flags)?;
            let ex = CalExecutor::new(CalExecutorConfig {
                allow_destructive_ops: !flags.contains_key("no-destructive-ops"),
                assembly_manifest_sample_rate: assembly_manifest_sample_rate(&flags)?,
                ..CalExecutorConfig::default()
            })
            .with_governance(std::sync::Arc::new(dejadb_loop::LoopGovernance::new()));
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
            report_meta_warnings(&facade);
            let mut server = dejadb_server::UiServer::new(facade, db.clone());
            if let Some(path) = flag(&flags, "auth") {
                let text = std::fs::read_to_string(&path)
                    .map_err(|e| format!("--auth {path}: {e}"))?;
                let map = dejadb_core::authz::CredentialMap::from_json(&text)
                    .map_err(|e| e.to_string())?;
                eprintln!(
                    "deja: credential map loaded ({} token(s)); rights come from the \
                     file's grant grains; unauthenticated requests run as 'anonymous'",
                    map.tokens.len()
                );
                server = server.with_credentials(map);
            }
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
            // Host loop policy (--policy FILE or $DEJA_LOOP_POLICY): a
            // console-triggered loop run honors the same grants as the CLI
            // run. Never grantable from the console itself.
            if let Some(p) = load_policy(&flags)? {
                server = server.with_loop_policy(p);
                eprintln!("deja: loop host policy attached to the console's loop routes");
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
        // dejad: the sync hub. Unlike `ui`, this is a network service by
        // design — other apps push and pull segments against one shared
        // memory — so the token is mandatory, not optional. Reads stay open
        // (same contract as the library `into_hub`); every write needs the key,
        // and a pushed segment can only ever *add* grains.
        "hub" => {
            let addr = flag(&flags, "addr").unwrap_or_else(|| "127.0.0.1:7438".to_string());
            let dir = flag(&flags, "dir").unwrap_or_else(|| "./segments".to_string());
            let var = flag(&flags, "token-env").ok_or_else(|| {
                "deja hub requires --token-env <VAR>: hub writes are gated by a shared key. \
                 Generate one (openssl rand -hex 16), export it, and pass the variable name."
                    .to_string()
            })?;
            let token = std::env::var(&var)
                .map_err(|_| format!("--token-env {var}: environment variable is not set"))?;
            if token.trim().is_empty() {
                return Err(format!("--token-env {var}: token is empty"));
            }
            if !addr_is_loopback(&addr) && flag(&flags, "allow-remote").is_none() {
                return Err(format!(
                    "refusing to bind the hub to a non-loopback address ({addr}). It is \
                     authenticated, but still plaintext HTTP — the token and every memory \
                     crossing the wire are in the clear. Terminate TLS in front of it, or \
                     pass --allow-remote to accept that risk."
                ));
            }
            // Archive retention (GDPR Art. 17 reach — docs/gdpr.md §3). The
            // hub imports every pushed segment into its live store, so its
            // `.mgb` files are archive, not source of truth: a subject
            // erased in the live store still sits in the segments that
            // carried it. Sweeping at startup writes a fresh checkpoint
            // from the (already-erased) store, then drops segments older
            // than the window.
            if let Some(v) = flag(&flags, "retain") {
                let window = parse_duration(&v)
                    .ok_or_else(|| format!("--retain takes a duration like 30d, got '{v}'"))?;
                let (kept, dropped) = hub_retention_sweep(&mut m, &dir, window)?;
                eprintln!(
                    "deja: retention {v} — wrote a fresh checkpoint ({kept} ops), \
                     dropped {dropped} segment(s) older than the window"
                );
            }
            let facade = dejadb_cal::DejaDbFacade::with_session(m, Some(ns), None);
            report_meta_warnings(&facade);
            let server = dejadb_server::UiServer::new(facade, db.clone())
                .into_hub(Some(token), &dir)
                .map_err(|e| format!("hub segment dir '{dir}': {e}"))?;
            let listener = dejadb_server::UiServer::bind(&addr).map_err(|e| e.to_string())?;
            eprintln!(
                "deja: hub writes require the token in ${var} (Authorization: Bearer …); \
                 reads are open"
            );
            eprintln!("deja: segments under {dir}");
            eprintln!(
                "deja hub → http://{}  (Ctrl-C to stop)",
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
        "subject-report" => {
            // The DSAR read (GDPR Art. 15 access / Art. 20 portability):
            // everything forget-subject WOULD erase — the same selector in
            // show-me mode. JSONL to stdout (or --out), optionally plus a
            // portable subject-scoped bundle (--bundle). Read-only: no
            // --yes, and --as needs only the read verb.
            let subject = positional.first().cloned().or_else(|| flag(&flags, "subject")).ok_or(
                "usage: deja subject-report <subject> [--ns NS] [--text-mentions] \
                 [--out FILE] [--bundle FILE]"
                    .to_string(),
            )?;
            let opts = dejadb_store::ErasureOptions {
                text_mentions: flag(&flags, "text-mentions")
                    .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                    .unwrap_or(false),
            };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                dejadb_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(dejadb_core::authz::Verb::Read, &ns)
                    .map_err(|e| e.to_string())?;
            }
            let report = m.subject_report_with(&ns, &subject, opts).map_err(|e| e.to_string())?;
            let mut lines = String::new();
            for g in &report.grains {
                let fields: serde_json::Map<String, serde_json::Value> =
                    g.fields.clone().into_iter().collect();
                let row = serde_json::json!({
                    "hash": g.hash.to_hex(),
                    "type": g.grain_type.as_str(),
                    "fields": fields,
                });
                lines.push_str(&row.to_string());
                lines.push('\n');
            }
            match flag(&flags, "out") {
                Some(path) => {
                    std::fs::write(&path, &lines).map_err(|e| e.to_string())?;
                    println!("wrote {} grains to {path}", report.grains.len());
                }
                None => print!("{lines}"),
            }
            if let Some(bpath) = flag(&flags, "bundle") {
                let stats = m
                    .subject_bundle_with(&ns, &subject, opts, &bpath)
                    .map_err(|e| e.to_string())?;
                eprintln!("bundle: {} records, {} bytes -> {bpath}", stats.ops, stats.bytes);
            }
            eprintln!(
                "subject-report '{subject}' ns '{ns}': {} grains; matched identities: {}",
                report.grains.len(),
                report.identity_names.join(", ")
            );
        }
        "forget-subject" => {
            // Right-to-erasure for one identity: erase every grain holding a
            // structured reference to the subject (history included), its
            // thread events, its dictionary entry, and erased-only
            // vocabulary — with replicating tombstones. Destructive and
            // bulk, so it demands an explicit --yes.
            let subject = positional.first().cloned().or_else(|| flag(&flags, "subject")).ok_or(
                "usage: deja forget-subject <subject> [--ns NS] [--text-mentions] --yes"
                    .to_string(),
            )?;
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            if !flags.contains_key("yes") {
                return Err(format!(
                    "forget-subject erases EVERY grain referencing '{subject}' in namespace \
                     '{ns}' (history included) and replicates the erasure to peers. \
                     Re-run with --yes to proceed."
                ));
            }
            // Same truthiness idiom as --index-text: bare presence enables,
            // an explicit falsy value disables — "--text-mentions false"
            // must not silently widen the erasure.
            let opts = dejadb_store::ErasureOptions {
                text_mentions: flag(&flags, "text-mentions")
                    .map(|v| !matches!(v.as_str(), "false" | "0" | "off" | "no"))
                    .unwrap_or(false),
            };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                dejadb_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(dejadb_core::authz::Verb::Erase, &ns)
                    .map_err(|e| e.to_string())?;
            }
            let rep = m.forget_subject_with(&ns, &subject, opts).map_err(|e| e.to_string())?;
            let stale_exports = m
                .corpus_exports_touching_subject(&subject)
                .unwrap_or_else(|e| {
                    eprintln!("deja: warning: could not consult corpus registry: {e}");
                    Vec::new()
                });
            // The CLI is a host, and hosts audit (REQ-ERASE-5 leaves the
            // record to the host precisely so the engine never writes the
            // identity back). Same builder as the CAL path, so `deja audit
            // export` sees one shape whichever surface erased.
            write_erasure_audit(
                &mut m,
                &flags,
                "erase",
                // A fingerprint, never the identity — the audit grain
                // outlives the erasure and replicates.
                &format!(
                    "subject:{} ns:{ns}",
                    dejadb_core::authz::subject_fingerprint(&subject)
                ),
                rep.grains_erased,
                &stale_exports,
            );
            println!(
                "erased {} grains ({} dictionary entries, {} vocabulary tokens, {} blobs reclaimed)",
                rep.grains_erased, rep.terms_removed, rep.vocab_removed, rep.blobs_reclaimed
            );
        }
        "purge-older-than" => {
            // Retention sweep: erase grains older than N days (created_at),
            // optionally limited to one grain type. --ns "" sweeps every
            // namespace. Destructive and bulk: demands --yes.
            let days: i64 = positional
                .first()
                .and_then(|v| v.parse().ok())
                .or_else(|| flag(&flags, "days").and_then(|v| v.parse().ok()))
                .ok_or("usage: deja purge-older-than <days> [--ns NS] [--type event] --yes".to_string())?;
            let gt = match flag(&flags, "type") {
                Some(t) => Some(
                    dejadb_core::types::GrainType::from_str(&t)
                        .ok_or_else(|| format!("unknown grain type '{t}'"))?,
                ),
                None => None,
            };
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            if !flags.contains_key("yes") {
                let scope = if ns.is_empty() {
                    "across EVERY namespace".to_string()
                } else {
                    format!("in namespace '{ns}'")
                };
                return Err(format!(
                    "purge-older-than erases every{} grain older than {days} days {scope} \
                     and replicates the erasure to peers. Re-run with --yes to proceed.",
                    gt.map(|g| format!(" {g:?}")).unwrap_or_default()
                ));
            }
            let cutoff = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
                - days * 24 * 3600 * 1000;
            let ns_opt = if ns.is_empty() { None } else { Some(ns.as_str()) };
            if let Some(principal) = flag(&flags, "as") {
                let grants = m.authz_grants(&principal).map_err(|e| e.to_string())?;
                // An all-namespace sweep needs erase on `*`.
                dejadb_core::authz::AuthzSet::restricted(&principal, grants)
                    .check(dejadb_core::authz::Verb::Erase, ns_opt.unwrap_or("*"))
                    .map_err(|e| e.to_string())?;
            }
            let exports = m.corpus_exports().unwrap_or_else(|e| {
                eprintln!("deja: warning: could not consult corpus registry: {e}");
                Vec::new()
            });
            let rep = m.forget_older_than(ns_opt, cutoff, gt).map_err(|e| e.to_string())?;
            let stale_exports =
                dejadb_store::exports_touching_hashes(&exports, &rep.erased_hashes);
            write_erasure_audit(
                &mut m,
                &flags,
                "erase",
                &format!(
                    "older_than:{days}d ns:{}{}",
                    if ns.is_empty() { "*" } else { ns.as_str() },
                    flag(&flags, "type").map(|t| format!(" type:{t}")).unwrap_or_default()
                ),
                rep.grains_erased,
                &stale_exports,
            );
            println!(
                "erased {} grains ({} vocabulary tokens, {} blobs reclaimed)",
                rep.grains_erased, rep.vocab_removed, rep.blobs_reclaimed
            );
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
        "loop" => {
            run_loop(m, &ns, &flags, &positional)?;
        }
        "audit" => {
            run_audit(&mut m, &flags, &positional)?;
        }
        "retention" => {
            run_retention(&mut m, &ns, &flags, &positional)?;
        }
        "blobs" => {
            // `deja blobs encrypt` — migrate CAS attachments written before
            // sidecar encryption landed (GDPR Art. 32; docs/gdpr.md §4).
            // New attachments are sealed on write, so this is a one-time
            // catch-up for existing files. Idempotent.
            let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("");
            if sub_cmd != "encrypt" {
                return Err("usage: deja blobs encrypt --db <file> --passphrase-env VAR".into());
            }
            let (encrypted, already) = m.encrypt_blobs().map_err(|e| e.to_string())?;
            println!(
                "encrypted {encrypted} attachment(s); {already} were already encrypted"
            );
        }
        other => return Err(format!("unknown command '{other}' — try `deja help`")),
    }
    Ok(())
}

/// `deja retention <set|list|clear|sweep>` — declarative storage limitation
/// (GDPR Art. 5(1)(e)).
///
/// Policy is a **file-truth** (`retention:<ns>` meta rows), so it travels
/// with the memory: a copy, a sync, or a restore on another host still says
/// "events here live 90 days". Declaring is separate from enforcing —
/// `set` never deletes anything; `sweep` does, and demands `--yes` like
/// every other bulk erasure. The governed alternative is the loop's
/// `retention_sweep` analyzer, which proposes the same purge through review
/// and audit instead of performing it.
fn run_retention(
    m: &mut DejaDB,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("list");
    match sub_cmd {
        "set" => {
            let days: f64 = flag(flags, "days")
                .and_then(|v| v.parse().ok())
                .or_else(|| positional.get(1).and_then(|v| v.parse().ok()))
                .ok_or_else(|| {
                    "usage: deja retention set --days N [--ns NS] [--type event] \
                     [--because \"why\"]"
                        .to_string()
                })?;
            let policy = dejadb_store::RetentionPolicy {
                days,
                grain_type: flag(flags, "type"),
                because: flag(flags, "because"),
            };
            m.set_retention_policy(ns, &policy).map_err(|e| e.to_string())?;
            println!(
                "retention policy for '{ns}': {days} days{}{}",
                policy.grain_type.map(|t| format!(", type {t}")).unwrap_or_default(),
                policy.because.map(|b| format!(" — {b}")).unwrap_or_default()
            );
            eprintln!(
                "deja: declared, not enforced — run `deja retention sweep --yes` (cron), \
                 or let the loop's retention_sweep analyzer propose it for review"
            );
        }
        "list" => {
            let policies = m.retention_policies().map_err(|e| e.to_string())?;
            if policies.is_empty() {
                println!("no retention policies declared in this memory");
            }
            for (pns, p) in policies {
                println!(
                    "{}",
                    serde_json::json!({
                        "namespace": pns,
                        "days": p.days,
                        "type": p.grain_type,
                        "because": p.because,
                    })
                );
            }
        }
        "clear" => {
            m.clear_retention_policy(ns).map_err(|e| e.to_string())?;
            println!("cleared the retention policy for '{ns}'");
        }
        "sweep" => {
            if flags.contains_key("no-destructive-ops") {
                return Err(
                    "destructive operations are disabled for this invocation \
                     (--no-destructive-ops)"
                        .into(),
                );
            }
            let policies = m.retention_policies().map_err(|e| e.to_string())?;
            if policies.is_empty() {
                println!("no retention policies declared — nothing to sweep");
                return Ok(());
            }
            if !flags.contains_key("yes") {
                let plan: Vec<String> = policies
                    .iter()
                    .map(|(pns, p)| {
                        format!(
                            "  {pns}: erase grains older than {} days{}",
                            p.days,
                            p.grain_type.as_deref().map(|t| format!(" (type {t})")).unwrap_or_default()
                        )
                    })
                    .collect();
                return Err(format!(
                    "retention sweep would apply {} policy/policies:\n{}\nRe-run with --yes \
                     to proceed.",
                    policies.len(),
                    plan.join("\n")
                ));
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let exports = m.corpus_exports().unwrap_or_else(|e| {
                eprintln!("deja: warning: could not consult corpus registry: {e}");
                Vec::new()
            });
            let results = m.sweep_retention(now).map_err(|e| e.to_string())?;
            let mut total = 0usize;
            for (pns, days, rep) in &results {
                total += rep.grains_erased;
                println!(
                    "{pns}: erased {} grains older than {days} days ({} vocabulary tokens, \
                     {} blobs reclaimed)",
                    rep.grains_erased, rep.vocab_removed, rep.blobs_reclaimed
                );
                // Audited like every other host-level erasure, one record per
                // policy so the evidence names which rule fired — but only
                // when something was actually erased. A destruction trail
                // records destructions; a nightly no-op sweep would
                // otherwise add 365 immutable, replicating grains a year per
                // policy for zero information.
                if rep.grains_erased > 0 {
                    let stale_exports =
                        dejadb_store::exports_touching_hashes(&exports, &rep.erased_hashes);
                    write_erasure_audit(
                        m,
                        flags,
                        "erase",
                        &format!("retention:{days}d ns:{pns}"),
                        rep.grains_erased,
                        &stale_exports,
                    );
                }
            }
            println!("retention sweep: {total} grains erased across {} policies", results.len());
        }
        other => {
            return Err(format!(
                "unknown retention subcommand '{other}' — usage: deja retention \
                 <set|list|clear|sweep>"
            ))
        }
    }
    Ok(())
}

/// Checkpoint the hub's segment dir and drop segments older than the
/// window. Returns `(ops_in_checkpoint, segments_dropped)`.
///
/// Order matters: the checkpoint is written FIRST, so the dir never passes
/// through a state where old segments are gone and no snapshot has replaced
/// them. The checkpoint is named so it sorts after ordinary pushes and is
/// itself subject to the window on later sweeps.
fn hub_retention_sweep(
    m: &mut DejaDB,
    dir: &str,
    window_ms: i64,
) -> Result<(usize, usize), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now();
    let stamp = now
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let snap = format!("{dir}/checkpoint-{stamp:013}.mgb");
    let st = m.bundle_since(0, &snap).map_err(|e| e.to_string())?;
    let mut dropped = 0usize;
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        if path == std::path::Path::new(&snap) || path.extension().is_none_or(|x| x != "mgb") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|md| md.modified()) else { continue };
        let age_ms = now.duration_since(modified).map(|d| d.as_millis() as i64).unwrap_or(0);
        if age_ms > window_ms {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
            dropped += 1;
        }
    }
    Ok((st.ops, dropped))
}

/// Read a stream dir's `CURSOR`: `"<gen_id> <op_seq> <next_seg>"`.
///
/// The third field is what keeps retention safe. Dirs written before it
/// existed carry only two fields; for those we fall back to counting
/// segment files, which is correct precisely because such a dir has never
/// been pruned (counting is what pruning breaks).
fn read_stream_cursor(cursor_path: &str, to: &str) -> (String, i64, u32) {
    let Ok(s) = std::fs::read_to_string(cursor_path) else {
        return (String::new(), 0, 0);
    };
    let mut parts = s.trim().splitn(3, ' ');
    let gen_id = parts.next().unwrap_or("").to_string();
    let cursor: i64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let seg = match parts.next().and_then(|v| v.parse::<u32>().ok()) {
        Some(n) => n,
        None => std::fs::read_dir(format!("{to}/gen-{gen_id}"))
            .map(|d| d.filter_map(|e| e.ok()).filter(|e| e.path().extension().is_some_and(|x| x == "mgb")).count() as u32)
            .unwrap_or(0),
    };
    (gen_id, cursor, seg)
}

/// Drop whole generations older than `window_ms`, never individual segments
/// inside one — a hole in a live generation strands every follower at the
/// gap, while dropping a whole generation is a re-baseline the follower
/// handles. The current generation is always kept.
///
/// This is what makes the Art. 17 archive guarantee true: each generation's
/// segment 0 is a snapshot of the already-erased store, so once the
/// pre-erasure generations age out, the erased bytes are gone from the
/// archive as well as the live store. See docs/gdpr.md §3.
fn prune_generations(to: &str, current_gen: &str, window_ms: i64) -> Result<usize, String> {
    let now = std::time::SystemTime::now();
    let mut dropped = 0usize;
    for entry in std::fs::read_dir(to).map_err(|e| e.to_string())?.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !path.is_dir() || !name.starts_with("gen-") || name == format!("gen-{current_gen}") {
            continue;
        }
        // Age by the NEWEST segment in the generation: a generation is only
        // expendable once everything in it has aged out.
        let newest = std::fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .flatten()
            .filter_map(|f| f.metadata().ok().and_then(|md| md.modified().ok()))
            .max();
        let Some(newest) = newest else { continue };
        let age_ms = now.duration_since(newest).map(|d| d.as_millis() as i64).unwrap_or(0);
        if age_ms > window_ms {
            std::fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
            dropped += 1;
        }
    }
    Ok(dropped)
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

/// Resolve an LLM backend from a `--<prefix>cmd` / `--<prefix>model` flag pair,
/// the same two ways `deja loop run` attaches one: a subprocess (the
/// zero-dependency escape hatch) or a built-in HTTP provider whose key is read
/// from the environment, never from the command line. The subprocess wins when
/// both are given. Both fail at construction, before anything is written.
fn resolve_llm(
    flags: &HashMap<String, String>,
    cmd_flag: &str,
    model_flag: &str,
) -> Result<Option<Box<dyn deja_loop::LlmBackend>>, String> {
    if let Some(cmd) = flag(flags, cmd_flag) {
        let label = flag(flags, "llm-model");
        let llm = deja_loop::CommandLlm::new(&cmd, label.as_deref()).map_err(|e| e.to_string())?;
        return Ok(Some(Box::new(llm)));
    }
    if let Some(spec) = flag(flags, model_flag) {
        let base = flag(flags, "llm-base-url");
        let key_env = flag(flags, "llm-api-key-env");
        let llm = dejadb_llm::resolve(&spec, base.as_deref(), key_env.as_deref())
            .map_err(|e| e.to_string())?;
        return Ok(Some(llm));
    }
    Ok(None)
}

/// `deja remember` — store free text as an Event, then attach the facts
/// distilled from it.
///
/// Three routes to those facts, in precedence order:
///
/// - `--facts JSON` — the host already extracted them; no model is called and
///   the facts carry no model attribution (the host is asserting them).
/// - `--llm-cmd CMD` — a subprocess backend.
/// - `--model provider:name` — a built-in HTTP backend, key from the env.
///
/// The raw text is written **before** the model is called, so a failed or
/// garbage extraction never costs the raw text. Model-extracted facts are
/// stamped `verification_status = "unverified"` (CAL-filterable, so they can be
/// reviewed) unless `--ground-model`/`--ground-cmd` runs a separate entailment
/// pass over them — a different call from the one that proposed them, the same
/// proposer ≠ scorer rule the Deja Loop verifier follows. Facts the grounder does
/// not support are dropped; survivors are stamped `"verified"`.
fn run_remember(mut m: DejaDB, ns: &str, flags: &HashMap<String, String>) -> Result<(), String> {
    let content = need(flags, "content")?;
    let observer = flag(flags, "observer").unwrap_or_else(|| "cli".to_string());
    let dry_run = flag(flags, "dry-run").is_some();
    let hint = flag(flags, "extract-hint");
    let min_confidence = match flag(flags, "min-confidence") {
        Some(s) => s
            .parse::<f64>()
            .map_err(|e| format!("bad --min-confidence: {e}"))?,
        None => 0.0,
    };
    // The store drops an unrecognized role (forward-compatible, which MCP
    // clients rely on), but a typo'd flag silently losing data is a bad trade
    // for a human at a terminal — reject it before anything runs.
    let role = flags.get("role").map(String::as_str);
    if let Some(r) = role.filter(|r| dejadb_core::types::Role::from_str(r).is_none()) {
        return Err(format!("--role {r:?}: expected user|assistant|system|tool"));
    }

    // Host-supplied drafts short-circuit the model entirely.
    let explicit = match flag(flags, "facts") {
        Some(j) => Some(dejadb_store::FactDraft::from_json_array(&j).map_err(|e| e.to_string())?),
        None => None,
    };
    let llm = match explicit {
        Some(_) => None,
        None => resolve_llm(flags, "llm-cmd", "model")?,
    };
    let grounder = match llm {
        Some(_) => resolve_llm(flags, "ground-cmd", "ground-model")?,
        None => None,
    };
    let model = llm.as_ref().map(|l| l.model().to_string());

    // --dry-run: extract and print, write nothing. For iterating on
    // --extract-hint without leaving drafts behind in the memory.
    if dry_run {
        let (proposed, facts) = match &llm {
            Some(l) => {
                let (proposed, facts, _) = extract_drafts(
                    l.as_ref(),
                    grounder.as_deref(),
                    "dry-run",
                    &content,
                    hint.as_deref(),
                    min_confidence,
                )?;
                (proposed, facts)
            }
            None => {
                let f = explicit.unwrap_or_default();
                (f.len(), f)
            }
        };
        println!(
            "{}",
            serde_json::json!({
                "dry_run": true,
                "model": model,
                "proposed": proposed,
                "facts": facts.iter().map(draft_json).collect::<Vec<_>>(),
            })
        );
        return Ok(());
    }

    // The raw text lands first and unconditionally: whatever the model does
    // next, the source of the extraction is already durable.
    let capture = dejadb_store::Capture {
        observer: Some(observer.as_str()),
        session_id: flags.get("session-id").map(String::as_str),
        role,
        run_id: flags.get("run-id").map(String::as_str),
    };
    let event = m
        .capture(ns, &content, &capture)
        .map_err(|e| e.to_string())?;
    let report = |facts: &[Hash], extra: serde_json::Value| {
        let mut out = serde_json::json!({
            "event": event.to_hex(),
            "facts": facts.iter().map(|h| h.to_hex()).collect::<Vec<_>>(),
        });
        if let (Some(obj), Some(extra)) = (out.as_object_mut(), extra.as_object()) {
            obj.extend(extra.clone());
        }
        println!("{out}");
    };
    // A model call that fails still leaves the raw text stored — print the
    // hash so the caller can retry the extraction against it, then exit
    // non-zero so the missing facts are not mistaken for "nothing to extract".
    let failed = |e: String| -> String {
        report(&[], serde_json::json!({ "error": e }));
        e
    };

    let (proposed, drafts, status) = match &llm {
        None => {
            let d = explicit.unwrap_or_default();
            (d.len(), d, None)
        }
        Some(l) => match extract_drafts(
            l.as_ref(),
            grounder.as_deref(),
            &event.to_hex(),
            &content,
            hint.as_deref(),
            min_confidence,
        ) {
            Ok((proposed, drafts, grounded)) => (
                proposed,
                drafts,
                Some(if grounded { "verified" } else { "unverified" }),
            ),
            Err(e) => return Err(failed(e)),
        },
    };

    let attribution = dejadb_store::FactAttribution {
        verification_status: status,
        extractor_model: model.as_deref(),
    };
    let facts = m
        .attach_facts(ns, &event, &drafts, &attribution)
        .map_err(|e| e.to_string())?;
    let extra = match &model {
        // What the model proposed vs what survived the confidence floor and
        // the grounder — a silent drop would read as "the model found less".
        Some(model) => serde_json::json!({
            "model": model,
            "verification_status": status,
            "proposed": proposed,
            "dropped": proposed.saturating_sub(facts.len()),
        }),
        None => serde_json::json!({}),
    };
    report(&facts, extra);
    Ok(())
}

/// Run the shared extract → confidence floor → ground pipeline and convert the
/// survivors to store drafts. Warns on stderr when a response hit the per-call
/// cap, so a truncated extraction is never silent.
fn extract_drafts(
    llm: &dyn deja_loop::LlmBackend,
    grounder: Option<&dyn deja_loop::LlmBackend>,
    source: &str,
    content: &str,
    hint: Option<&str>,
    min_confidence: f64,
) -> Result<(usize, Vec<dejadb_store::FactDraft>, bool), String> {
    let ex =
        dejadb_llm::extract_pipeline(llm, grounder, source, content, hint, min_confidence)
            .map_err(|e| e.to_string())?;
    if ex.proposed >= dejadb_llm::extract::MAX_FACTS {
        eprintln!(
            "note: extraction hit the {}-fact cap; some facts may have been dropped",
            dejadb_llm::extract::MAX_FACTS
        );
    }
    let drafts = ex
        .facts
        .into_iter()
        .map(|f| dejadb_store::FactDraft {
            subject: f.subject,
            relation: f.relation,
            object: f.object,
            confidence: f.confidence,
        })
        .collect();
    Ok((ex.proposed, drafts, ex.grounded))
}

fn draft_json(d: &dejadb_store::FactDraft) -> serde_json::Value {
    serde_json::json!({
        "subject": d.subject,
        "relation": d.relation,
        "object": d.object,
        "confidence": d.confidence,
    })
}

/// Load the host policy from `--policy FILE` or `$DEJA_LOOP_POLICY` (§6.2).
fn load_policy(flags: &HashMap<String, String>) -> Result<Option<Policy>, String> {
    let path = flag(flags, "policy").or_else(|| std::env::var("DEJA_LOOP_POLICY").ok());
    match path {
        Some(p) => {
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{p}: {e}"))?;
            Ok(Some(Policy::from_json(&s).map_err(|e| e.to_string())?))
        }
        None => Ok(None),
    }
}

/// `deja audit export [--since MS] [--until MS] [--out FILE]` — the
/// accountability evidence bundle (GDPR Art. 5(2)/30, and the Art. 33
/// breach-notification input).
///
/// Two hash-chained trails already exist as grains; this formats them,
/// nothing more. Tier-2 destruction Observations live in `agent:authz`
/// (one per FORGET / FORGET SUBJECT / PURGE, from CAL or the CLI alike),
/// and the loop's lifecycle audit rides as `loop_audit` Facts in
/// `deja-loop`. Output is JSONL — one record per line, oldest first — so
/// extracting a window is a pipe, not a parser.
///
/// **The chain is verified, not assumed.** Each loop audit record carries
/// `derived_from = [rec_hash, previous_audit_hash]`; a record whose
/// predecessor is absent from the export means the trail was truncated,
/// and evidence that cannot say so is worse than none. Breaks are reported
/// on stderr and marked on the record.
fn run_audit(
    m: &mut DejaDB,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("export");
    if sub_cmd != "export" {
        return Err(format!(
            "unknown audit subcommand '{sub_cmd}' — usage: deja audit export \
             [--since MS] [--until MS] [--out FILE]"
        ));
    }
    let parse_ms = |k: &str| -> Result<Option<i64>, String> {
        match flag(flags, k) {
            None => Ok(None),
            Some(v) => v
                .parse::<i64>()
                .map(Some)
                .map_err(|_| format!("--{k} takes epoch milliseconds, got '{v}'")),
        }
    };
    let since = parse_ms("since")?;
    let until = parse_ms("until")?;
    let in_window = |at: i64| since.is_none_or(|s| at >= s) && until.is_none_or(|u| at <= u);

    // Tier-2 destruction audit: Observations in the reserved authz namespace.
    let mut rows: Vec<(i64, serde_json::Value)> = Vec::new();
    let authz = m
        .recent(
            dejadb_core::authz::AUTHZ_NS,
            Some(dejadb_core::types::GrainType::Observation),
            usize::MAX / 2,
        )
        .map_err(|e| e.to_string())?;
    for g in authz {
        let ctx = g.fields.get("context").cloned().unwrap_or(serde_json::Value::Null);
        if ctx.get("audit").and_then(|v| v.as_str()) != Some("tier2") {
            continue;
        }
        let at = g.fields.get("created_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if !in_window(at) {
            continue;
        }
        rows.push((
            at,
            serde_json::json!({
                "trail": "destruction",
                "hash": g.hash.to_hex(),
                "at_ms": at,
                "principal": g.fields.get("observer_id"),
                "observer_type": g.fields.get("observer_type"),
                "verb": ctx.get("verb"),
                // For subject erasures this is `subject:<fingerprint> ns:<ns>`:
                // verify it against a candidate identity by recomputing
                // sha256(id)[..8] in hex; it deliberately cannot be mined
                // for who was erased. `subject_ref` names the scheme.
                "target": ctx.get("target"),
                "subject_ref": ctx.get("subject_ref"),
                "because": ctx.get("because"),
                "grains_erased": ctx.get("grains_erased"),
                "stale_corpora": ctx.get("stale_corpora"),
            }),
        ));
    }

    // Loop lifecycle audit: `loop_audit` Facts carrying the engine's
    // field-map in `object`, hash-chained through `derived_from`.
    let mut chain_prev: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut loop_rows: Vec<(i64, serde_json::Value, Option<String>)> = Vec::new();
    let loop_grains = m
        .recent("deja-loop", Some(dejadb_core::types::GrainType::Fact), usize::MAX / 2)
        .map_err(|e| e.to_string())?;
    for g in loop_grains {
        if g.get_str("relation") != Some("loop_audit") {
            continue;
        }
        let Some(body) = g.get_str("object").and_then(|o| {
            serde_json::from_str::<serde_json::Value>(o).ok()
        }) else {
            continue;
        };
        let at = body.get("at_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        if !in_window(at) {
            continue;
        }
        chain_prev.insert(g.hash.to_hex());
        // derived_from = [rec_hash, previous_audit_hash?]
        let prev = body
            .get("derived_from")
            .and_then(|v| v.as_array())
            .and_then(|a| a.get(1))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        loop_rows.push((
            at,
            serde_json::json!({
                "trail": "loop",
                "hash": g.hash.to_hex(),
                "at_ms": at,
                "rec_hash": body.get("rec_hash"),
                "from_status": body.get("from_status"),
                "to_status": body.get("to_status"),
                "actor": body.get("actor"),
                "observer_type": body.get("observer_type"),
                "because": body.get("because"),
                "previous_audit": prev.clone(),
            }),
            prev,
        ));
    }
    // Chain verification: a named predecessor absent from this export means
    // the trail is truncated. Report it rather than emitting a clean-looking
    // file.
    let mut breaks = 0usize;
    for (at, mut row, prev) in loop_rows {
        if let Some(p) = prev {
            if !chain_prev.contains(&p) {
                breaks += 1;
                row["chain_break"] = serde_json::json!(true);
            }
        }
        rows.push((at, row));
    }

    rows.sort_by_key(|(at, _)| *at);
    let mut out = String::new();
    for (_, row) in &rows {
        out.push_str(&row.to_string());
        out.push('\n');
    }
    match flag(flags, "out") {
        Some(path) => {
            std::fs::write(&path, &out).map_err(|e| e.to_string())?;
            println!("wrote {} audit records to {path}", rows.len());
        }
        None => print!("{out}"),
    }
    if breaks > 0 {
        eprintln!(
            "deja: warning: {breaks} loop audit record(s) name a predecessor absent from this \
             export — the chain is truncated (widen --since, or the trail was tampered with)"
        );
    }
    eprintln!("audit export: {} records", rows.len());
    Ok(())
}

/// `deja loop <run|list|show|approve|reject|apply|rollback|analyzers|policy|status>`.
fn run_loop(
    m: DejaDB,
    ns: &str,
    flags: &HashMap<String, String>,
    positional: &[String],
) -> Result<(), String> {
    let sub_cmd = positional.first().map(|s| s.as_str()).unwrap_or("status");
    let mut sub = DejaDbSubstrate::new(m, Some(ns.to_string()));
    // Host policy (--policy FILE or $DEJA_LOOP_POLICY) — the only place
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
        let llm = deja_loop::CommandLlm::new(&cmd, model.as_deref()).map_err(|e| e.to_string())?;
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
        let g = deja_loop::CommandLlm::new(&cmd, None).map_err(|e| e.to_string())?;
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
        let a = deja_loop::CommandAnalyzer::new(&cmd).map_err(|e| e.to_string())?;
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
                triggering_actor: Some(actor.clone()),
            };
            let res = engine.run(&mut sub, &opts, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&res).map_err(|e| e.to_string())?);
            } else if res.ran() {
                if !flags.contains_key("quiet") {
                    eprintln!(
                        "loop: ran — proposed {} ({} deduped, {} auto-applied) across {} analyzer(s)",
                        res.stored,
                        res.deduped,
                        res.auto_applied,
                        res.analyzers_run.len()
                    );
                }
                if res.stored > 0 {
                    eprintln!("loop: {} new — deja loop list", res.stored);
                }
            } else if !flags.contains_key("quiet") {
                eprintln!("loop: skipped ({:?})", res.skip_reason);
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
                eprintln!("no recommendations — run `deja loop run` first");
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
                    eprintln!("loop: pending recommendation(s) at or above severity '{sev}'");
                    std::process::exit(2);
                }
            }
        }
        "show" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| "usage: deja loop show <hash>".to_string())?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            let recs = engine.recommendations(&sub, None).map_err(|e| e.to_string())?;
            let r = recs.iter().find(|r| r.hash == hash).unwrap();
            let mut out = serde_json::json!({
                "hash": r.hash,
                "status": r.status.as_str(),
                "severity": r.severity.as_str(),
                "analyzer": r.analyzer,
                "origin": r.origin,
                "target_ref": r.target_ref,
                "summary": r.summary.render(),
                "destructive": r.destructive,
                "rollbackable": r.rollbackable,
                "evidence": r.evidence,
                "dedup_key": r.dedup_key,
                "confidence": r.confidence,
            });
            // The reviewable change itself — `show` is the review surface, so
            // the proposal must be visible before approve/apply. Flattened
            // (`proposal` kind tag + its fields, e.g. `cal`) exactly like the
            // stored grain body; the outcome metric and any LLM guidance ride
            // along when present.
            let o = out.as_object_mut().unwrap();
            if let Ok(serde_json::Value::Object(p)) = serde_json::to_value(&r.proposal) {
                o.extend(p);
            }
            if let Some(m) = &r.metric {
                o.insert("metric".into(), serde_json::to_value(m).map_err(|e| e.to_string())?);
            }
            if let Some(gd) = &r.guidance {
                o.insert("guidance".into(), serde_json::Value::from(gd.clone()));
            }
            println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
        }
        "approve" | "reject" => {
            let prefix = positional
                .get(1)
                .ok_or_else(|| format!("usage: deja loop {sub_cmd} <hash> --because \"...\""))?;
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
                .ok_or_else(|| "usage: deja loop apply <hash> --because \"...\"".to_string())?;
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
                .ok_or_else(|| "usage: deja loop rollback <hash> --because \"...\"".to_string())?;
            let because = need(flags, "because")?;
            let hash = resolve_hash(&engine, &sub, prefix)?;
            engine
                .rollback(&mut sub, &hash, &actor, observer, &scopes, &because, now)
                .map_err(|e| e.to_string())?;
            eprintln!("rolled back {}", short(&hash));
        }
        "analyzers" => {
            // One row per registered analyzer: id, tier, trust class
            // (builtin vs an external command — advisory only), default state,
            // title. Same lowercase trust strings as the console's Setup view.
            for a in engine.analyzers() {
                let m = a.manifest();
                println!(
                    "{:<28}  {:?}  {:<7}  on={}  {}",
                    m.id,
                    m.tier,
                    format!("{:?}", m.trust_class).to_lowercase(),
                    m.default_on,
                    m.title
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
        // Bare `deja loop` (or an unknown subcommand) prints a health summary.
        _ => {
            let h = engine.health(&sub, now).map_err(|e| e.to_string())?;
            if json {
                println!("{}", serde_json::to_string(&h).map_err(|e| e.to_string())?);
            } else {
                println!(
                    "loop: {} recommendation(s) — {} pending, {} applied",
                    h.total, h.pending, h.applied
                );
                match h.last_run_ms {
                    None => println!("  never run — deja loop run"),
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
                    eprintln!("  ⚠ the loop may be stale — run `deja loop run` or wire the SessionEnd hook");
                } else if h.pending > 0 {
                    println!("  review with: deja loop list");
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
        println!("next: deja loop run --db <file>   # ~4 recommendations across analyzers");
    }
    // Print the Claude Code hook snippet — deja never edits your settings.
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "deja".to_string());
    eprintln!("\nClaude Code hooks (paste into settings.json — absolute path baked in):");
    eprintln!("  UserPromptSubmit → {exe} recall-hook --ns {ns} --with-loop");
    eprintln!("  Stop             → {exe} capture-stop --ns {ns}");
    eprintln!("  SessionEnd       → {exe} loop run --min-new 20 --min-new-errors 3 --quiet --ns {ns}");
    Ok(())
}

/// Seed the demo corpus: planted duplicates, a contradiction, and a stale
/// grain, so the first `deja loop run` fires several analyzers at once.
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

    #[test]
    fn openai_tool_import_preserves_arguments_error_and_call_id() {
        let rows = extract_tool_records(&serde_json::json!({
            "created_at": 42,
            "tool_calls": [{
                "id": "call-7",
                "is_error": true,
                "function": {
                    "name": "charge",
                    "arguments": "{\"amount\":42}"
                }
            }]
        }));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "charge");
        assert_eq!(rows[0].input, Some(serde_json::json!({"amount": 42})));
        assert!(rows[0].content.is_empty(), "arguments are not a result");
        assert!(rows[0].is_error);
        assert_eq!(rows[0].call_id.as_deref(), Some("call-7"));
        assert_eq!(rows[0].created_at, Some(42));
    }
}
