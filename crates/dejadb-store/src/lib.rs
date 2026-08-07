//! dejadb-store — the embedded Turso-backed store for DejaDB.
//!
//! Implements the store schema: dictionary-encoded 2½-permutation
//! triple indexes (SPO + POS mandatory, OSP selective for entity-valued
//! relations), `entity_latest` materialization, op-log + HLC + tombstones,
//! thread index, and the vaais operation profile (add / recall / batch /
//! supersede / forget) plus bounded graph ops and two-axis `entity_at`.
//!
//! `DejaDB` drives a sync storage seam (`db::Db`); the embedded Turso backend
//! hides its own current-thread runtime behind it, so point ops stay µs-class
//! with no executor hop at the call site, and a second backend can implement
//! the same seam without touching the store logic.

mod db;
#[cfg(feature = "postgres")]
pub mod pg;

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use dejadb_core::error::{Hash, DejaDbError, Result};
use dejadb_core::format::deserialize::{deserialize_blob, DeserializedGrain};
use dejadb_core::format::serialize::serialize_grain;
use dejadb_core::types::Grain;
use dejadb_core::types::{step_action_node, step_action_relation, STEP_ACTION_PREFIX};
use turso::Value;
use zeroize::Zeroize;

use db::{db_err, with_txn, Db, TursoDb};

/// Op-log operation kinds.
pub const OP_ADD: i64 = 1;
pub const OP_SUPERSEDE: i64 = 2;
pub const OP_FORGET: i64 = 3; // tombstone

/// Temporal axis for `entity_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// "What was true in the world at T" — `valid_from`/`valid_to`.
    World,
    /// "What did the agent know at T" — supersession chain walk.
    Knowledge,
}

/// One op-log record, the change-feed unit.
#[derive(Debug, Clone)]
pub struct OpRecord {
    pub op_seq: i64,
    pub hlc: i64,
    pub op: i64,
    pub hash: Hash,
}

/// Traversal direction for `related`. `In` uses the
/// selective OSP index, so it only sees entity-valued relations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    /// Parse the wire spelling used by every binding (`out`/`in`/`both`).
    /// Defaults to `Out` so a caller that omits it walks the graph forwards.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "out" | "" => Some(Self::Out),
            "in" => Some(Self::In),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

impl Axis {
    /// Parse the wire spelling used by every binding.
    ///
    /// `world` = what was true at T (`valid_from`/`valid_to`);
    /// `knowledge` = what the agent knew at T (walks the supersession chain).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "world" | "" => Some(Self::World),
            "knowledge" => Some(Self::Knowledge),
            _ => None,
        }
    }
}

/// Split the comma-separated relation list the bindings take.
///
/// Relations arrive as one scalar string (the FFI convention is scalars in,
/// JSON out), so `"mg:knows, reports_to"` becomes two relations. Empty entries
/// are dropped — a trailing comma is not an anonymous relation.
pub fn parse_relations(csv: &str) -> Vec<String> {
    csv.split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect()
}

/// Result of `bundle_since` — the git-shaped incremental backup (§5.10).
#[derive(Debug, Clone)]
pub struct BundleStats {
    pub ops: usize,
    pub bytes: u64,
    pub last_op_seq: i64,
}

/// Result of `import_bundle`.
#[derive(Debug, Clone, Default)]
pub struct ImportStats {
    pub applied: usize,
    pub skipped: usize,
}

/// Pluggable embedding backend. The host owns the model;
/// multilingual recall quality comes from choosing a multilingual model
/// (e.g. bge-m3 / multilingual-e5) — text reaches the backend as
/// NFC-normalized UTF-8, script untouched (Arabic/Mandarin/English alike).
pub trait EmbedBackend: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    /// Model identifier recorded as file provenance (e.g. "bge-m3").
    /// Backends should override this; it lets a later open detect that the
    /// stored vectors came from a different model.
    fn model(&self) -> &str {
        "unspecified"
    }
}

/// [`EmbedBackend`] that shells out to a host-supplied command per call: the
/// text goes to the child's stdin, stdout must be a JSON array of numbers.
/// This is the dependency-free way to give every surface (CLI `--embed-cmd`,
/// MCP serve, bindings) a real vector leg — the host owns the model, the
/// engine still ships none. One process spawn per embed: fine for turn-level
/// recall and imports, not for the voice per-frame path.
pub struct CommandEmbed {
    argv: Vec<String>,
    dim: usize,
    model: String,
}

impl CommandEmbed {
    /// `cmd` is split on whitespace (no shell interpretation). The command is
    /// probed once here to learn the vector dimension, so a broken command
    /// fails loudly at setup rather than mid-recall.
    pub fn new(cmd: &str, model: Option<&str>) -> Result<Self> {
        let argv: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
        if argv.is_empty() {
            return Err(DejaDbError::Validation("embed command is empty".into()));
        }
        let mut ce = CommandEmbed {
            argv,
            dim: 0,
            model: model.unwrap_or("command").to_string(),
        };
        let probe = ce.run("dimension probe")?;
        if probe.is_empty() {
            return Err(DejaDbError::Validation(
                "embed command returned an empty vector".into(),
            ));
        }
        ce.dim = probe.len();
        Ok(ce)
    }

    fn run(&self, text: &str) -> Result<Vec<f32>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let cmd_err = |e: std::io::Error| {
            DejaDbError::Storage(format!("embed command '{}': {e}", self.argv[0]))
        };
        let mut child = Command::new(&self.argv[0])
            .args(&self.argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(cmd_err)?;
        {
            let mut stdin = child.stdin.take().expect("stdin piped");
            stdin.write_all(text.as_bytes()).map_err(cmd_err)?;
            // dropping stdin closes the pipe so the child sees EOF
        }
        let out = child.wait_with_output().map_err(cmd_err)?;
        if !out.status.success() {
            return Err(DejaDbError::Storage(format!(
                "embed command '{}' exited with {}",
                self.argv[0], out.status
            )));
        }
        serde_json::from_slice::<Vec<f32>>(&out.stdout).map_err(|e| {
            DejaDbError::Validation(format!(
                "embed command output must be a JSON array of numbers: {e}"
            ))
        })
    }
}

impl EmbedBackend for CommandEmbed {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let v = self.run(text)?;
        if v.len() != self.dim {
            return Err(DejaDbError::Validation(format!(
                "embed command returned {} dims, expected {}",
                v.len(),
                self.dim
            )));
        }
        Ok(v)
    }
    fn model(&self) -> &str {
        &self.model
    }
}

/// Pluggable cross-encoder reranker (Tier-2
/// retrieval). Like `EmbedBackend`, the host owns the model: inject a local
/// candle/ONNX cross-encoder (or any scorer) — the engine ships no model and
/// takes no ML dependency. Off by default; with no reranker installed recall
/// behaves exactly as before. Reranking is a **turn-level** refinement (tens
/// of ms), never on the voice per-frame path; `recall_hybrid_tuned` only
/// invokes it inside the deadline and falls back to fusion order otherwise.
pub trait RerankBackend: Send + Sync {
    /// Relevance score for each `(query, doc)` pair, positionally aligned with
    /// `docs`. Higher = more relevant. Scores are only ever compared among
    /// themselves, so raw cross-encoder logits are fine (no normalization
    /// required). Must return exactly `docs.len()` scores.
    fn rerank(&self, query: &str, docs: &[&str]) -> Result<Vec<f32>>;
    /// Model identifier for observability (e.g. "ms-marco-MiniLM-L-6-v2").
    fn model(&self) -> &str {
        "unspecified"
    }
}

/// Pluggable rule-based query expander (Tier-1 retrieval). No LLM, no network.
/// Given a query it returns additional query *variants*; the caller runs one
/// extra BM25 leg per variant and fuses them via RRF, bridging vocabulary gaps
/// ("cell" ↔ "mobile" ↔ "phone") — the poor-man's semantic bridge for the
/// edge/BM25-only profile where no embedder is installed. The built-in
/// [`EnglishExpander`] is **English-only**; multilingual deployments install
/// their own or leave expansion off (it is opt-in per query).
pub trait QueryExpander: Send + Sync {
    /// Query variants to also search, NOT including the original. Empty = no
    /// expansion. Implementations should keep this small and bounded.
    fn expand(&self, query: &str) -> Vec<String>;
}

/// Built-in English query expander: synonym substitution + naive suffix
/// stemming, capped to a handful of variants. Deterministic and allocation-
/// light. English-only by design (see [`QueryExpander`]).
pub struct EnglishExpander {
    /// Cap on the number of variants returned (default 4).
    max_variants: usize,
}

impl Default for EnglishExpander {
    fn default() -> Self {
        Self { max_variants: 4 }
    }
}

impl EnglishExpander {
    pub fn new(max_variants: usize) -> Self {
        Self { max_variants: max_variants.clamp(1, 16) }
    }

    /// Synonyms for a lowercased token (both directions of each group).
    fn synonyms(token: &str) -> &'static [&'static str] {
        // Small, deterministic map. Each group lists the *other* members.
        match token {
            "cell" | "cellphone" => &["mobile", "phone"],
            "mobile" => &["cell", "phone"],
            "phone" => &["cell", "mobile", "telephone"],
            "email" | "e-mail" => &["mail"],
            "buy" | "bought" | "purchased" => &["purchase"],
            "purchase" => &["buy"],
            "car" | "automobile" => &["vehicle"],
            "vehicle" => &["car"],
            "doctor" | "physician" => &["doctor", "physician"],
            "kid" | "kids" | "child" => &["child", "children"],
            "spouse" => &["wife", "husband", "partner"],
            "job" => &["work", "employer"],
            "home" | "house" => &["residence", "address"],
            "birthday" => &["birthdate", "born"],
            "big" => &["large"],
            "small" => &["little"],
            _ => &[],
        }
    }

    /// Naive English suffix stemmer for a single token: strips one common
    /// inflection. Returns the stem only when it differs and stays ≥3 chars.
    fn stem(token: &str) -> Option<String> {
        let lower = token;
        for suf in ["ing", "ed", "es", "s"] {
            if lower.len() > suf.len() + 2 && lower.ends_with(suf) {
                let stem = &lower[..lower.len() - suf.len()];
                if stem.len() >= 3 {
                    return Some(stem.to_string());
                }
            }
        }
        None
    }
}

impl QueryExpander for EnglishExpander {
    fn expand(&self, query: &str) -> Vec<String> {
        let tokens: Vec<String> = query
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            return Vec::new();
        }
        let mut variants: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let original = tokens.join(" ");
        seen.insert(original.clone());

        // 1. Synonym substitution: one variant per (position, synonym).
        for (i, tok) in tokens.iter().enumerate() {
            for syn in Self::synonyms(tok) {
                let mut v = tokens.clone();
                v[i] = (*syn).to_string();
                let s = v.join(" ");
                if seen.insert(s.clone()) {
                    variants.push(s);
                    if variants.len() >= self.max_variants {
                        return variants;
                    }
                }
            }
        }

        // 2. A fully-stemmed variant (all tokens stemmed where possible).
        let stemmed: Vec<String> = tokens
            .iter()
            .map(|t| Self::stem(t).unwrap_or_else(|| t.clone()))
            .collect();
        let s = stemmed.join(" ");
        if seen.insert(s.clone()) {
            variants.push(s);
        }

        variants.truncate(self.max_variants);
        variants
    }
}

/// Post-fusion recall refinements. All default off — a bare
/// `recall_hybrid` behaves exactly as before. Applied inside the recall
/// deadline; each stage degrades to plain fusion order when its backend or
/// data is unavailable (fail-open, never an error).
#[derive(Debug, Clone, Copy, Default)]
pub struct RecallTuning {
    /// Tier-1: run rule-based query expansion (extra BM25 legs, RRF-fused).
    /// Uses the installed [`QueryExpander`], or the built-in [`EnglishExpander`].
    pub query_expansion: bool,
    /// Tier-2: cross-encoder rerank the fused candidate pool via the installed
    /// [`RerankBackend`]. Takes precedence over `diversity_lambda`.
    pub rerank: bool,
    /// Tier-1: MMR diversity reorder. `lambda` in `[0,1]` — 1.0 = pure
    /// relevance, 0.0 = maximum diversity. Requires an embedder + stored
    /// vectors; silently skipped otherwise.
    pub diversity_lambda: Option<f32>,
    /// Widen every leg to the whole supersession chain instead of heads only.
    ///
    /// All three legs are heads-only by default — the structural probe filters
    /// `cur=1`, BM25 drops non-live postings after scoring, and the vector leg
    /// joins on `svt IS NULL`. That is the right default: stale values in a
    /// model's context are the failure mode a memory engine exists to prevent.
    /// This opts a caller that is asking *about the past* back into the rest of
    /// the chain — CAL's `WITH superseded`, audit and drift reads.
    ///
    /// Forgotten grains stay gone: `forget` deletes the index rows outright
    /// rather than flagging them, so nothing tombstoned or crypto-erased can
    /// come back through this door.
    pub include_superseded: bool,
}

/// One extracted fact from a `remember()` extraction callback.
#[derive(Debug, Clone)]
pub struct FactDraft {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub confidence: f64,
}

/// The confidence a draft gets when the source omits one.
pub const DRAFT_DEFAULT_CONFIDENCE: f64 = 0.8;

impl FactDraft {
    /// Parse the `[{subject, relation, object, confidence}]` JSON that every
    /// non-Rust surface uses to hand over pre-extracted facts (`--facts`,
    /// `facts_json`) — a closure cannot cross FFI, so the drafts arrive as
    /// text. Lives here, next to the type, because the CLI, Python, and Node
    /// bindings all need exactly this parse.
    ///
    /// A row missing any of subject/relation/object is an error, not a silent
    /// empty-string Fact. Confidence defaults to
    /// [`DRAFT_DEFAULT_CONFIDENCE`] and is clamped into 0.0–1.0.
    pub fn from_json_array(json: &str) -> Result<Vec<FactDraft>> {
        let arr: Vec<serde_json::Value> = serde_json::from_str(json).map_err(|e| {
            DejaDbError::Validation(format!(
                "facts must be a JSON array of {{subject, relation, object, confidence}}: {e}"
            ))
        })?;
        arr.iter()
            .enumerate()
            .map(|(i, v)| {
                let field = |k: &str| {
                    v.get(k)
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .trim()
                        .to_string()
                };
                let (subject, relation, object) = (field("subject"), field("relation"), field("object"));
                if subject.is_empty() || relation.is_empty() || object.is_empty() {
                    return Err(DejaDbError::Validation(format!(
                        "facts[{i}] needs a non-empty subject, relation, and object"
                    )));
                }
                Ok(FactDraft {
                    subject,
                    relation,
                    object,
                    confidence: v
                        .get("confidence")
                        .and_then(|c| c.as_f64())
                        .unwrap_or(DRAFT_DEFAULT_CONFIDENCE)
                        .clamp(0.0, 1.0),
                })
            })
            .collect()
    }
}

/// Provenance stamped on facts attached to their source grain by
/// [`DejaDB::attach_facts`].
///
/// The default — used by `remember()`'s own extractor seam and by the
/// host-supplied `--facts` path — adds nothing beyond the `derived_from` link:
/// a host asserting its own facts is not relaying a model's claim, so it gets
/// no model attribution and no verification status.
#[derive(Debug, Clone, Default)]
pub struct FactAttribution<'a> {
    /// OMS `verification_status` (`"unverified"` / `"verified"` / …). Facts a
    /// model extracted from free text are `"unverified"` until something
    /// checks them; it is CAL-filterable, so the queue is queryable.
    pub verification_status: Option<&'a str>,
    /// Identifier of the model that produced these drafts, recorded so an
    /// audit can answer "which model wrote this?" from the grain itself.
    ///
    /// It rides in `extra_fields` as `extractor_model` — the same mechanism
    /// Waiser uses to carry structured data on a grain. `GrainCommon` has a
    /// `provenance_chain` field that looks like the natural home, but nothing
    /// in `dejadb-core` serializes it, so a value written there would be
    /// silently dropped at the blob boundary.
    pub extractor_model: Option<&'a str>,
}

/// Who captured a piece of raw content, and where it sits in a conversation.
/// Every field is optional: `deja remember` names an observer, the MCP tool and
/// `capture-stop` carry a session and a role, and a bare call needs none.
#[derive(Debug, Clone, Default)]
pub struct Capture<'a> {
    /// The agent/process that captured the text. Kept in `extra_fields`
    /// (an Event's `role` is the *author*, which is a different question).
    pub observer: Option<&'a str>,
    /// Session/thread id — what puts the Event in the thread index, so a
    /// remembered turn is reachable as part of its transcript.
    pub session_id: Option<&'a str>,
    /// `user` | `assistant` | `system` | `tool`. Unrecognized values are
    /// dropped rather than rejected.
    pub role: Option<&'a str>,
    /// OMS §8.2 `run_id` — the correlation key `run_trace`, `run_yield` and
    /// `runs_touching` read back.
    ///
    /// Without this on the capture path, `run_id` was writable only by
    /// constructing an `Event` in Rust, so the run-history reads were
    /// unreachable by construction from the CLI, MCP and both bindings: they
    /// could ask which grains belong to a run, but nothing they could call ever
    /// put a grain in one.
    pub run_id: Option<&'a str>,
}

/// Result of `DejaDB::remember`.
#[derive(Debug, Clone)]
pub struct RememberResult {
    /// The Event grain holding the raw captured text.
    pub event: Hash,
    pub facts: Vec<Hash>,
}

/// Integrity report (`DejaDB::verify`).
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub integrity: String,
    /// Benign notes from Turso's experimental FTS internal indexes
    /// (integrity_check miscounts them; not data corruption).
    pub fts_notes: Vec<String>,
    pub grains: usize,
    pub hash_mismatches: usize,
    pub undecodable: usize,
}

/// Store statistics (`DejaDB::stats`).
#[derive(Debug, Clone)]
pub struct StoreStats {
    pub grains: usize,
    pub current: usize,
    pub triples: usize,
    pub terms: usize,
    pub ops: usize,
    pub events_indexed: usize,
}

/// One version in a supersession chain (`DejaDB::history`, newest first).
#[derive(Debug, Clone)]
pub struct HistoryEntry {
    pub hash: Hash,
    pub object: String,
    pub created_at: i64,
    pub confidence: f64,
    pub superseded_by: Option<Hash>,
}

/// An open fork: a `(namespace, subject, relation)` that has more than one
/// live head, because two writers superseded the same value concurrently
/// (e.g. edits synced from two edges). The tips coexist — nothing is lost —
/// until an explicit merge closes the fork. `heads[0]` is the deterministic
/// provisional head every node agrees on.
#[derive(Debug, Clone)]
pub struct ForkGroup {
    pub namespace: String,
    pub subject: String,
    pub relation: String,
    pub heads: Vec<Hash>,
}

const BUNDLE_MAGIC: &[u8; 4] = b"MGB1";

/// RRF fusion constant used by `recall_hybrid` (the standard k0 = 60).
/// Exported so observability surfaces can report the effective value.
pub const RRF_K0: f64 = 60.0;

/// Absolute cap on the candidate pool a refinement stage (rerank / MMR)
/// considers. Bounds cross-encoder cost and the MMR pairwise-similarity join
/// regardless of how far a caller over-fetches; a larger requested `k` still
/// widens the pool to at least `k`.
const REFINE_POOL: usize = 64;

/// Semver-ish `a < b` over dotted numeric versions. Non-numeric components
/// (a `-rc1` suffix) compare as 0, which errs toward *not* warning — a false
/// alarm on every open would train people to ignore the one that matters.
fn version_lt(a: &str, b: &str) -> bool {
    let parts = |v: &str| -> Vec<u32> {
        v.split(['.', '-', '+'])
            .map(|p| p.parse::<u32>().unwrap_or(0))
            .collect()
    };
    let (pa, pb) = (parts(a), parts(b));
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (pa.get(i).copied().unwrap_or(0), pb.get(i).copied().unwrap_or(0));
        if x != y {
            return x < y;
        }
    }
    false
}

/// The highest `.mg` grain type byte a DejaDB build before OMS 1.5 could
/// decode. `deserialize_blob` **errors** on an unknown type byte rather than
/// skipping it, so a file carrying a newer grain is not merely partially
/// readable to an older build — the read fails.
const LEGACY_MAX_GRAIN_BYTE: u8 = 0x0B;

/// Reader version stamped into `meta` the first time a grain newer than
/// [`LEGACY_MAX_GRAIN_BYTE`] is written to a file.
///
/// OMS §4.5 guarantees an additive type byte leaves existing *content
/// addresses* valid; it says nothing about older *readers*. This turns the
/// resulting failure from a mid-recall decode error into a statement the file
/// makes about itself, which `open_warnings()` surfaces. It cannot help builds
/// that shipped before the check existed — for those the only safe posture is
/// not to sync a file containing new grain types.
const MIN_READER_VERSION_KEY: &str = "min_reader_version";

/// File-truth: the link indexes (`prov_idx`, `run_idx`, `related_to`
/// cross-links) are built and current for every grain in this file.
///
/// Its absence — not the tables being empty — is what marks a file written
/// before those indexes existed. A file can legitimately have zero rows in all
/// three (no grain carries `derived_from`, `run_id` or `related_to`), so
/// emptiness cannot tell "never indexed" from "nothing to index", and guessing
/// wrong either re-scans the corpus on every open or answers provenance
/// questions with silence forever.
const LINK_INDEX_KEY: &str = "link_index";

/// Bumped when a change to what the link indexes contain requires a rebuild of
/// files stamped with an earlier value.
const LINK_INDEX_VERSION: &str = "1";

/// Open options.
pub struct DejaDbOptions {
    /// Relations whose objects are entities (get OSP reverse-index rows).
    /// Defaults to the OMS `mg:` entity-valued vocabulary.
    pub entity_relations: HashSet<String>,
    /// Populate the FTS text column (BM25 leg). Turso's experimental FTS
    /// costs ~150ms per write txn on segment commits — voice/edge deployments
    /// set this false (structural + vector legs still serve recall; §6).
    pub index_text: bool,
    /// Encryption-at-rest key: 32 bytes → AES-256-GCM via Turso's page cipher.
    /// `None` = plaintext. Host-supplied capability, never persisted in the
    /// file — a bare `open()` cannot supply it, so
    /// encrypted files must be opened with `open_with`/`open_encrypted`.
    /// Destroying the key destroys the memory (crypto-erasure). Covers the
    /// memory database (grains, indexes, op-log, WAL); the `.blobs` CAS
    /// sidecar is not yet encrypted.
    pub encryption_key: Option<[u8; 32]>,
    /// Recall-telemetry retention for the `<file>.telemetry.db` sidecar.
    /// Host-only capability (never persisted in the file, like the embedder):
    /// a bare `open()` leaves it `Off`, so the library default records nothing;
    /// agent-facing hosts opt into `Aggregate`. The sidecar is encrypted under
    /// this same key when `encryption_key` is set. See [`telemetry`].
    pub telemetry: TelemetryMode,
}

impl Default for DejaDbOptions {
    fn default() -> Self {
        let ents = [
            "mg:delegates_to",
            "mg:owned_by",
            "mg:assigned_to",
            "mg:depends_on",
            "mg:handed_off_to",
            "mg:capable_of",
            "delegates_to",
            "reports_to",
            "part_of",
            "knows",
        ];
        DejaDbOptions {
            entity_relations: ents.iter().map(|s| s.to_string()).collect(),
            index_text: true,
            encryption_key: None,
            telemetry: TelemetryMode::Off,
        }
    }
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT)",
    "CREATE TABLE IF NOT EXISTS terms(id INTEGER PRIMARY KEY, term TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS grains(
        seq INTEGER PRIMARY KEY,
        hash BLOB,
        ns INTEGER, gtype INTEGER, created_at INTEGER,
        s INTEGER, p INTEGER, o INTEGER,
        vf INTEGER, vt INTEGER,
        svf INTEGER, svt INTEGER,
        superseded_by BLOB, supersedes BLOB,
        text TEXT,
        blob BLOB NOT NULL)",
    "CREATE UNIQUE INDEX IF NOT EXISTS idx_grains_hash ON grains(hash)",
    "CREATE TABLE IF NOT EXISTS embeddings(seq INTEGER PRIMARY KEY, vec BLOB)",
    // BM25 leg — our own inverted index. See `search_text` for why this is not
    // Turso's `USING fts`.
    "CREATE TABLE IF NOT EXISTS fts_vocab(id INTEGER PRIMARY KEY, term TEXT UNIQUE)",
    "CREATE TABLE IF NOT EXISTS fts_post(term INTEGER, seq INTEGER, ns INTEGER, tf INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_fts_post ON fts_post(term, ns)",
    "CREATE INDEX IF NOT EXISTS idx_fts_post_seq ON fts_post(seq)",
    "CREATE TABLE IF NOT EXISTS fts_doc(seq INTEGER PRIMARY KEY, len INTEGER)",
    "CREATE TABLE IF NOT EXISTS triples(ns INTEGER, s INTEGER, p INTEGER, o INTEGER, seq INTEGER, cur INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_spo ON triples(ns,s,p,o,seq)",
    "CREATE INDEX IF NOT EXISTS idx_pos ON triples(ns,p,o,s,seq)",
    "CREATE INDEX IF NOT EXISTS idx_triples_seq ON triples(seq)",
    "CREATE TABLE IF NOT EXISTS osp(ns INTEGER, o INTEGER, s INTEGER, p INTEGER, seq INTEGER, cur INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_osp ON osp(ns,o,s)",
    "CREATE INDEX IF NOT EXISTS idx_osp_seq ON osp(seq)",
    "CREATE TABLE IF NOT EXISTS entity_latest(ns INTEGER, s INTEGER, p INTEGER, o INTEGER, seq INTEGER, hash BLOB, PRIMARY KEY(ns,s,p))",
    "CREATE TABLE IF NOT EXISTS heads(ns INTEGER, s INTEGER, p INTEGER, seq INTEGER, hash BLOB, created_at INTEGER, PRIMARY KEY(ns,s,p,seq))",
    "CREATE TABLE IF NOT EXISTS oplog(op_seq INTEGER PRIMARY KEY, hlc INTEGER, op INTEGER, hash BLOB)",
    "CREATE TABLE IF NOT EXISTS thread_idx(ns INTEGER, session INTEGER, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_thread ON thread_idx(ns, session, seq)",
    // Reverse provenance: parent content address -> the grains derived from it.
    // `derived_from` sits on *every* grain, so indexing it as triples would add
    // a row for a large fraction of the store and inflate the index that recall
    // scans. A narrow table keeps the cost proportional to the question.
    // The parent is the raw 32-byte hash, not a dictionary term — interning one
    // term per grain address would bloat `terms` for no lookup benefit.
    "CREATE TABLE IF NOT EXISTS prov_idx(ns INTEGER, parent BLOB, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_prov ON prov_idx(ns, parent, seq)",
    // Run correlation: `run_id` -> the grains recorded during that run. Run ids
    // repeat across many grains, so this one *is* dictionary-encoded.
    "CREATE TABLE IF NOT EXISTS run_idx(ns INTEGER, run INTEGER, seq INTEGER)",
    "CREATE INDEX IF NOT EXISTS idx_run ON run_idx(ns, run, seq)",
];

fn pi(x: i64) -> Value {
    Value::Integer(x)
}
fn pb(b: Vec<u8>) -> Value {
    Value::Blob(b)
}
fn pt(s: &str) -> Value {
    Value::Text(s.to_string())
}
fn opt_i(v: Option<i64>) -> Value {
    match v {
        Some(x) => Value::Integer(x),
        None => Value::Null,
    }
}

/// Hex-encode a 32-byte key. The engine-open copy lives in db.rs; this one
/// remains only for the byte-order pin below.
#[cfg(test)]
fn hex32(k: &[u8; 32]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(64);
    for b in k {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---- passphrase key derivation (Argon2id) -------------------------------

/// Argon2id parameters for passphrase-derived encryption keys (OWASP 2024).
const KDF_M_COST: u32 = 19_456; // memory in KiB (19 MiB)
const KDF_T_COST: u32 = 2; // iterations
const KDF_P_COST: u32 = 1; // parallelism
const KDF_SALT_LEN: usize = 16;

fn kdf_err<E: std::fmt::Display>(e: E) -> DejaDbError {
    DejaDbError::CryptoError(e.to_string())
}

/// Load the KDF salt/params sidecar at `<db>.kdf`, creating it with a fresh
/// random salt if absent. The salt is not secret, but it must travel with the
/// database file so the same passphrase re-derives the same key.
fn load_or_create_kdf_sidecar(sidecar: &str) -> Result<([u8; KDF_SALT_LEN], u32, u32, u32)> {
    match std::fs::read_to_string(sidecar) {
        Ok(text) => parse_kdf_sidecar(&text, sidecar),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut salt = [0u8; KDF_SALT_LEN];
            getrandom::getrandom(&mut salt).map_err(kdf_err)?;
            let line = format!(
                "v1 argon2id {} {KDF_M_COST} {KDF_T_COST} {KDF_P_COST}\n",
                hex::encode(salt)
            );
            // Atomic create: if another process wrote the sidecar first, do not
            // clobber it — re-read so both derive from the same persisted salt.
            match std::fs::OpenOptions::new().write(true).create_new(true).open(sidecar) {
                Ok(mut f) => {
                    use std::io::Write;
                    f.write_all(line.as_bytes()).map_err(kdf_err)?;
                    Ok((salt, KDF_M_COST, KDF_T_COST, KDF_P_COST))
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let text = std::fs::read_to_string(sidecar).map_err(kdf_err)?;
                    parse_kdf_sidecar(&text, sidecar)
                }
                Err(e) => Err(kdf_err(e)),
            }
        }
        Err(e) => Err(kdf_err(e)),
    }
}

/// Parse and sanity-check a KDF sidecar's contents.
fn parse_kdf_sidecar(text: &str, sidecar: &str) -> Result<([u8; KDF_SALT_LEN], u32, u32, u32)> {
    let toks: Vec<&str> = text.split_whitespace().collect();
    if toks.len() != 6 || toks[0] != "v1" || toks[1] != "argon2id" {
        return Err(kdf_err(format!("malformed KDF sidecar: {sidecar}")));
    }
    let salt_bytes = hex::decode(toks[2]).map_err(kdf_err)?;
    if salt_bytes.len() != KDF_SALT_LEN {
        return Err(kdf_err("KDF salt has wrong length"));
    }
    let mut salt = [0u8; KDF_SALT_LEN];
    salt.copy_from_slice(&salt_bytes);
    let m = toks[3].parse::<u32>().map_err(kdf_err)?;
    let t = toks[4].parse::<u32>().map_err(kdf_err)?;
    let p = toks[5].parse::<u32>().map_err(kdf_err)?;
    // Reject absurd parameters (e.g. a tampered sidecar forcing a multi-GiB
    // allocation → OOM). Bounds are generous but finite.
    if !(8..=1_048_576).contains(&m) || !(1..=16).contains(&t) || !(1..=16).contains(&p) {
        return Err(kdf_err("KDF parameters out of range"));
    }
    Ok((salt, m, t, p))
}

impl DejaDB {
    /// Derive a 32-byte AES-256 key from a passphrase using Argon2id. The salt
    /// and cost parameters live in a non-secret `<path>.kdf` sidecar created on
    /// first use. The returned key zeroizes on drop.
    ///
    /// Losing the passphrase destroys the key (crypto-erasure); losing the
    /// `.kdf` sidecar means the passphrase can no longer re-derive the key, so
    /// back it up alongside the database.
    pub fn derive_key_for(path: &str, passphrase: &str) -> Result<zeroize::Zeroizing<[u8; 32]>> {
        if passphrase.trim().is_empty() {
            return Err(kdf_err("passphrase must not be empty or whitespace-only"));
        }
        let sidecar = format!("{path}.kdf");
        let (salt, m, t, p) = load_or_create_kdf_sidecar(&sidecar)?;
        let params = argon2::Params::new(m, t, p, Some(32)).map_err(kdf_err)?;
        let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        argon
            .hash_password_into(passphrase.as_bytes(), &salt, &mut key[..])
            .map_err(kdf_err)?;
        Ok(key)
    }

    /// Open (or create) an encrypted memory using a passphrase-derived key
    /// (Argon2id + AES-256-GCM at rest). Convenience over
    /// [`DejaDB::derive_key_for`] + [`DejaDB::open_with`].
    pub fn open_with_passphrase(path: &str, passphrase: &str) -> Result<Self> {
        let key = Self::derive_key_for(path, passphrase)?;
        Self::open_with(
            path,
            DejaDbOptions { encryption_key: Some(*key), ..DejaDbOptions::default() },
        )
    }

    /// [`open_with_passphrase`](Self::open_with_passphrase) with a
    /// recall-telemetry sidecar (encrypted under the same passphrase-derived
    /// key). The agent-host binding path.
    pub fn open_with_passphrase_telemetry(
        path: &str,
        passphrase: &str,
        telemetry: TelemetryMode,
    ) -> Result<Self> {
        let key = Self::derive_key_for(path, passphrase)?;
        Self::open_with(
            path,
            DejaDbOptions {
                encryption_key: Some(*key),
                telemetry,
                ..DejaDbOptions::default()
            },
        )
    }
}


fn vec_to_json(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 8);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 { s.push(','); }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// The read-side coercion contract now lives on `db::Row`'s accessors; these
// remain only for the pin below that documents it against raw `Value`s.
#[cfg(test)]
fn v_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

#[cfg(test)]
fn v_blob(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Blob(b) => Some(b.clone()),
        _ => None,
    }
}

#[cfg(test)]
fn v_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Real(r) => Some(*r),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// Comma-separated list of i64 seqs for an inline `IN (...)` clause. Safe:
/// the values are engine-internal seq ids, never user text.
fn seq_csv(seqs: &[i64]) -> String {
    let mut s = String::with_capacity(seqs.len() * 6);
    for (i, x) in seqs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&x.to_string());
    }
    s
}

/// A grain fully prepared for insertion (serialized + extracted + encoded).
struct GrainPrep {
    blob: Vec<u8>,
    hash: Hash,
    ns_id: i64,
    s: Option<i64>,
    p: Option<i64>,
    o: Option<i64>,
    osp: bool,
    session: Option<i64>,
    vf: Option<i64>,
    vt: Option<i64>,
    created: i64,
    gtype: i64,
    text: Option<String>,
    embedding: Option<Vec<f32>>,
    /// `(fts_vocab.id, term frequency)` for this grain's text, and the
    /// document length in tokens. Empty when text indexing is off or deferred.
    tokens: Vec<(i64, i64)>,
    doc_len: i64,
    /// Term-encoded `related_to` cross-links: `(subject = this grain's own
    /// hash, predicate = relation_type, object = target hash)`. Indexed into
    /// `triples`/`osp` for retrieval but deliberately **not** into
    /// `heads`/`entity_latest` — OMS §15.3 is normative that a `related_to`
    /// link is an annotation and MUST NOT change the target's supersession
    /// state. Empty for the overwhelming majority of grains.
    links: Vec<(i64, i64, i64)>,
    /// Dictionary id of `run_id`, for the run index.
    run: Option<i64>,
    /// Raw 32-byte `derived_from` parent address, for reverse provenance.
    parent: Option<Vec<u8>>,
}

/// Extracted index-relevant fields of a grain about to be stored.
struct GrainView {
    ns: String,
    subject: Option<String>,
    relation: Option<String>,
    object: Option<String>,
    session: Option<String>,
    vf: Option<i64>,
    vt: Option<i64>,
    created_at: i64,
    gtype: u8,
    /// `(relation_type, target hash)` pairs from `related_to`.
    links: Vec<(String, String)>,
    /// `run_id` — the only run-scoped correlation key in the grain model.
    run: Option<String>,
    /// `derived_from` — this grain's provenance parent, if any.
    parent: Option<String>,
}

fn extract_view(view: &DeserializedGrain) -> GrainView {
    GrainView {
        ns: view.get_str("namespace").unwrap_or("shared").to_string(),
        subject: view.get_str("subject").map(str::to_string),
        relation: view.get_str("relation").map(str::to_string),
        object: view.get_str("object").map(str::to_string),
        session: view.get_str("session_id").map(str::to_string),
        vf: view.get_i64("valid_from"),
        vt: view.get_i64("valid_to"),
        created_at: view.get_i64("created_at").unwrap_or_else(now_ms),
        gtype: view.grain_type as u8,
        run: view.get_str("run_id").map(str::to_string),
        parent: view.get_str("derived_from").map(str::to_string),
        links: view
            .fields
            .get("related_to")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| {
                        Some((
                            l.get("relation_type")?.as_str()?.to_string(),
                            l.get("hash")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// The single text projection the FTS and vector legs index: the grain's
/// explicit `embedding_text` override when present (its documented contract —
/// import pipelines set it to preserve original prose), else
/// "subject relation object" plus any top-level `content`. `None` = nothing
/// to index. Used by the write path, the reranker's candidate text, and the
/// `rebuild_text_index` backfill so all three stay in lockstep.
/// BM25 tuning. Textbook defaults; the corpus here is short documents
/// (a fact projects to "subject relation object"), so `b` matters more than
/// `k1` and neither is worth exposing until someone can show a workload where
/// tuning them helps.
const BM25_K1: f64 = 1.2;
const BM25_B: f64 = 0.75;

/// Longest token kept. Anything longer is a hash, a base64 blob or a URL —
/// never a search term, and unbounded vocabulary growth if indexed.
const MAX_TOKEN_LEN: usize = 64;

/// Split text into index terms.
///
/// Deliberately plain: lowercase, split on anything not alphanumeric, drop
/// what is too long. No stemming and no stopword list, because both make
/// results depend on a language guess — and the same function runs at index
/// time and query time, so whatever it does, the two agree. Grain text is
/// already NFC-normalized by the canonical serializer, so `is_alphanumeric`
/// is enough for non-ASCII scripts.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_TOKEN_LEN)
        .map(|t| t.to_lowercase())
        .collect()
}

/// Tokens with their in-document frequency, plus the document's length in
/// tokens (BM25 needs the length including repeats).
fn token_freqs(text: &str) -> (Vec<(String, i64)>, i64) {
    let tokens = tokenize(text);
    let len = tokens.len() as i64;
    let mut freqs: HashMap<String, i64> = HashMap::new();
    for t in tokens {
        *freqs.entry(t).or_insert(0) += 1;
    }
    (freqs.into_iter().collect(), len)
}

fn projected_text(view: &DeserializedGrain) -> Option<String> {
    if let Some(et) = view.get_str("embedding_text") {
        if !et.trim().is_empty() {
            return Some(et.to_string());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if let (Some(s), Some(r), Some(o)) = (
        view.get_str("subject"),
        view.get_str("relation"),
        view.get_str("object"),
    ) {
        parts.push(format!("{s} {r} {o}"));
    }
    if let Some(c) = view.get_str("content") {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Where the CAS blob payloads live: a `.blobs` fan-out directory next to
/// the memory file (embedded backend), or an in-schema `blobs` table
/// (postgres backend — a connection string has no "next to").
enum BlobStore {
    Fs(std::path::PathBuf),
    #[cfg_attr(not(feature = "postgres"), allow(dead_code))]
    Table,
}

fn fs_blob_path(dir: &std::path::Path, hex: &str) -> std::path::PathBuf {
    dir.join(&hex[..2]).join(&hex[2..])
}

/// The embedded DejaDB store handle — one file per memory.
pub struct DejaDB {
    db: Box<dyn Db>,
    dict: HashMap<String, i64>,
    next_term: i64,
    next_seq: i64,
    next_op: i64,
    hlc_last: i64,
    entity_rels: HashSet<String>,
    index_text: bool,
    /// Lazily-filled token -> `fts_vocab.id` cache. Not preloaded at open:
    /// the text vocabulary is far larger than the triple dictionary and
    /// reading it eagerly would put corpus-sized work back into every open.
    fts_vocab: HashMap<String, i64>,
    /// Live document count and total token length, for BM25's `N` and `avgdl`.
    /// Kept in memory and adjusted on write for the same reason `next_seq` is:
    /// an aggregate over the postings on every search would be O(corpus), the
    /// exact thing this index exists to avoid.
    fts_docs: i64,
    fts_total_len: i64,
    /// Set by [`defer_text_index`](Self::defer_text_index) — suppresses
    /// posting writes during a bulk load until `rebuild_text_index` runs.
    fts_deferred: bool,
    embedder: Option<Box<dyn EmbedBackend>>,
    /// Optional cross-encoder reranker (Tier-2). Host-supplied, off by default.
    reranker: Option<Box<dyn RerankBackend>>,
    /// Optional query expander (Tier-1). `None` falls back to the built-in
    /// English expander when `RecallTuning::query_expansion` is set.
    expander: Option<Box<dyn QueryExpander>>,
    /// Embedding provenance declared by the file (meta table): model + dim
    /// of the vectors already stored, recorded when the first backend is
    /// installed.
    meta_embed: Option<(String, usize)>,
    /// Reconciliation notes from open / set_embedder (file declarations vs
    /// what this session supplied). Never fatal; surfaced by hosts.
    warnings: Vec<String>,
    /// False once the file carries a `min_reader_version` stamp, so the check
    /// on the write path costs one bool after the first newer-than-1.4 grain.
    needs_min_reader_stamp: bool,
    blob_store: BlobStore,
    /// Recall-telemetry sidecar (`<file>.telemetry.db`). `None` when the host
    /// left telemetry `Off` — the recall path then does nothing extra.
    telemetry: Option<Telemetry>,
}

impl DejaDB {
    /// Open honoring the file's own declarations (`meta` table) when
    /// present. A fresh file is stamped with the defaults. This is the
    /// file-truth path: settings like `text_index` travel with the file,
    /// so the same memory behaves identically on any host.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_internal(path, None, TelemetryMode::Off)
    }

    /// Open with explicit options. Explicit options are deliberate: they
    /// re-stamp the file's declarations, and a change to an existing
    /// declaration is recorded in `open_warnings()`.
    pub fn open_with(path: &str, opts: DejaDbOptions) -> Result<Self> {
        let telemetry = opts.telemetry;
        Self::open_internal(path, Some(opts), telemetry)
    }

    /// Open honoring the file's own declarations (like [`open`](Self::open))
    /// but with a recall-telemetry sidecar enabled. Telemetry is host config,
    /// **not** a file-truth, so this deliberately does *not* re-stamp the file's
    /// `index_text`/`entity_relations` declarations — it just attaches the
    /// sidecar (encrypted under the file's key when the file is encrypted; for
    /// an encrypted file, open with [`open_with`](Self::open_with) instead so
    /// the key is supplied).
    pub fn open_with_telemetry(path: &str, telemetry: TelemetryMode) -> Result<Self> {
        Self::open_internal(path, None, telemetry)
    }

    /// Open (or create) an encrypted memory: AES-256-GCM at rest with a
    /// host-supplied 32-byte key (Turso page cipher). The key lives only in
    /// the caller's process — never written to the file — so a bare `open()`
    /// of this path cannot read it, and destroying the key destroys the
    /// memory (crypto-erasure). Default index/relation options otherwise.
    pub fn open_encrypted(path: &str, key: [u8; 32]) -> Result<Self> {
        Self::open_with(
            path,
            DejaDbOptions { encryption_key: Some(key), ..DejaDbOptions::default() },
        )
    }

    fn open_internal(
        path: &str,
        explicit: Option<DejaDbOptions>,
        telemetry_mode: TelemetryMode,
    ) -> Result<Self> {
        // Keep the AEAD key only in a Zeroizing buffer for the duration of the
        // open; the raw copies (options + this local) are wiped once the engine
        // has ingested it, so no unzeroized key bytes linger on the heap.
        let enc_key = explicit
            .as_ref()
            .and_then(|o| o.encryption_key)
            .map(zeroize::Zeroizing::new);
        let dbh: Box<dyn Db> = Box::new(TursoDb::open(path, enc_key.as_deref())?);
        for sql in SCHEMA {
            dbh.execute(sql, vec![])?;
        }
        let mut warnings: Vec<String> = Vec::new();
        if enc_key.is_some() {
            warnings.push(
                "encryption-at-rest ON (AES-256-GCM): the memory database is encrypted; the \
                 .blobs CAS sidecar is NOT yet encrypted — keep sensitive media out of this file \
                 (or avoid put_blob) until blob encryption lands"
                    .into(),
            );
        }
        let blob_dir = std::path::PathBuf::from(format!("{}.blobs", path));
        std::fs::create_dir_all(&blob_dir).map_err(db_err)?;
        // Telemetry sidecar (`<file>.telemetry.db`): opened under the SAME AEAD
        // key as the main file so crypto-erasure covers it. Only when the host
        // asked for it — `Off` opens no sidecar and costs the recall path
        // nothing. The mode is a separate argument, not read from `opts`, so
        // telemetry can be enabled on a declaration-honoring open without
        // re-stamping file-truths.
        let telemetry = match telemetry_mode {
            TelemetryMode::Off => None,
            mode => Some(Telemetry::open(path, enc_key.as_deref(), mode)?),
        };
        Self::finish_open(dbh, explicit, BlobStore::Fs(blob_dir), telemetry, warnings)
    }

    /// Open (or create) a memory in a PostgreSQL schema, honoring the
    /// schema's own `meta` declarations — the server-tier analogue of
    /// [`open`](Self::open). One memory = one schema (the unit of isolation,
    /// `pg_dump -n` export, and `DROP SCHEMA` erasure); single-writer is
    /// ENFORCED via a session advisory lock (`STO-E002` when contended).
    /// CAS blobs live in an in-schema table. The page cipher and the
    /// telemetry sidecar are file-backend capabilities and are rejected here.
    #[cfg(feature = "postgres")]
    pub fn open_postgres(url: &str, schema: &str) -> Result<Self> {
        Self::open_postgres_internal(url, schema, None)
    }

    /// Explicit-options variant of [`open_postgres`](Self::open_postgres) —
    /// re-stamps the schema's declarations and records changes in
    /// [`open_warnings`](Self::open_warnings), mirroring [`open_with`](Self::open_with).
    #[cfg(feature = "postgres")]
    pub fn open_postgres_with(url: &str, schema: &str, opts: DejaDbOptions) -> Result<Self> {
        Self::open_postgres_internal(url, schema, Some(opts))
    }

    #[cfg(feature = "postgres")]
    fn open_postgres_internal(
        url: &str,
        schema: &str,
        explicit: Option<DejaDbOptions>,
    ) -> Result<Self> {
        if let Some(o) = &explicit {
            if o.encryption_key.is_some() {
                return Err(DejaDbError::Validation(
                    "encryption_key is a file-backend capability (page cipher); on the postgres \
                     backend use TDE/pgcrypto at the deployment layer"
                        .into(),
                ));
            }
            if !matches!(o.telemetry, TelemetryMode::Off) {
                return Err(DejaDbError::Validation(
                    "the recall-telemetry sidecar is not yet supported on the postgres backend"
                        .into(),
                ));
            }
        }
        let dbh: Box<dyn Db> = Box::new(pg::PgDb::open(url, schema)?);
        for sql in pg::PG_SCHEMA {
            dbh.execute(sql, vec![])?;
        }
        Self::finish_open(dbh, explicit, BlobStore::Table, None, Vec::new())
    }

    /// Backend-independent tail of every open: meta reconciliation and
    /// stamping, dictionary/counter seeding, and the self-heal passes.
    fn finish_open(
        dbh: Box<dyn Db>,
        explicit: Option<DejaDbOptions>,
        blob_store: BlobStore,
        telemetry: Option<Telemetry>,
        mut warnings: Vec<String>,
    ) -> Result<Self> {
        // ---- file-carried declarations (meta k/v) --------------------
        let meta: HashMap<String, String> = {
            let mut m = HashMap::new();
            for row in dbh.query("SELECT k, v FROM meta", vec![])? {
                if let (Some(k), Some(v)) = (row.text(0), row.text(1)) {
                    m.insert(k.to_string(), v.to_string());
                }
            }
            m
        };
        let declared_text = meta.get("text_index").map(|v| v == "1");
        let declared_rels: Option<HashSet<String>> = meta
            .get("entity_relations")
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .map(|v| v.into_iter().collect());
        let meta_embed = match (
            meta.get("embedding_model"),
            meta.get("embedding_dim").and_then(|d| d.parse::<usize>().ok()),
        ) {
            (Some(m), Some(d)) => Some((m.clone(), d)),
            _ => None,
        };

        let mut opts = match explicit {
            Some(o) => {
                if let Some(d) = declared_text {
                    if d != o.index_text {
                        warnings.push(format!(
                            "file declared text_index={}; explicit open changed it to {} (re-stamped) — \
                             grains written under the old setting keep their old indexing",
                            if d { "on" } else { "off" },
                            if o.index_text { "on" } else { "off" },
                        ));
                    }
                }
                if let Some(ref d) = declared_rels {
                    if *d != o.entity_relations {
                        warnings.push(
                            "file-declared entity_relations differ from explicit options (re-stamped) — \
                             OSP rows indexed under the old set are unchanged"
                                .into(),
                        );
                    }
                }
                o
            }
            None => DejaDbOptions {
                index_text: declared_text.unwrap_or(true),
                entity_relations: declared_rels
                    .unwrap_or_else(|| DejaDbOptions::default().entity_relations),
                encryption_key: None,
                // Telemetry is host config, not a file-truth: a bare `open()`
                // never turns it on.
                telemetry: TelemetryMode::Off,
            },
        };
        // The plaintext key now lives only inside the storage engine (turso keeps
        // its own copy while the database is open); wipe the copy carried in the
        // open options so no unzeroized key bytes linger.
        opts.encryption_key.zeroize();

        // Stamp declarations + create the FTS index if wanted.
        dbh.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('text_index', ?1)",
            vec![pt(if opts.index_text { "1" } else { "0" })],
        )?;
        let mut rels: Vec<&String> = opts.entity_relations.iter().collect();
        rels.sort();
        let rels = serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into());
        dbh.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES ('entity_relations', ?1)",
            vec![pt(&rels)],
        )?;
        // Files written before the BM25 leg moved off Turso's experimental
        // FTS carry an `idx_fts` index that nothing reads any more, and
        // that still taxes every write to this table. Drop it on sight.
        // Absent (the normal case) it errors; that is not a problem.
        let _ = dbh.execute("DROP INDEX idx_fts", vec![]);

        // Load dictionary + counters.
        let mut dict = HashMap::new();
        let mut next_term = 1i64;
        for row in dbh.query("SELECT id, term FROM terms", vec![])? {
            let id = row.i64(0).unwrap_or(0);
            if let Some(t) = row.text(1) {
                dict.insert(t.to_string(), id);
            }
            next_term = next_term.max(id + 1);
        }
        let one = |sql: &'static str| -> Result<i64> {
            Ok(dbh.query(sql, vec![])?.first().and_then(|r| r.i64(0)).unwrap_or(0))
        };
        let next_seq = one("SELECT COALESCE(MAX(seq),0) FROM grains")? + 1;
        let next_op = one("SELECT COALESCE(MAX(op_seq),0) FROM oplog")? + 1;
        let hlc_last = one("SELECT COALESCE(MAX(hlc),0) FROM oplog")?;
        let fts_docs = one("SELECT COUNT(*) FROM fts_doc")?;
        let fts_total_len = one("SELECT COALESCE(SUM(len),0) FROM fts_doc")?;
        let indexed_text = one("SELECT COUNT(*) FROM grains WHERE text IS NOT NULL")?;
        let grain_count = one("SELECT COUNT(*) FROM grains")?;

        let mut store = DejaDB {
            db: dbh,
            dict,
            next_term,
            next_seq,
            next_op,
            hlc_last,
            entity_rels: opts.entity_relations,
            index_text: opts.index_text,
            fts_vocab: HashMap::new(),
            fts_docs,
            fts_total_len,
            fts_deferred: false,
            embedder: None,
            reranker: None,
            expander: None,
            meta_embed,
            warnings,
            needs_min_reader_stamp: !meta.contains_key(MIN_READER_VERSION_KEY),
            blob_store,
            telemetry,
        };

        // The file may declare that it needs a newer reader than this build (a
        // grain type added after this binary was compiled). Say so at open
        // rather than letting a recall fail to decode a blob halfway through.
        if let Some(req) = meta.get(MIN_READER_VERSION_KEY) {
            if version_lt(env!("CARGO_PKG_VERSION"), req) {
                store.warnings.push(format!(
                    "this memory declares min_reader_version {req} but this build is {} — it \
                     contains grain types this version cannot decode, and reads that touch them \
                     will fail. Upgrade dejadb to {req} or later",
                    env!("CARGO_PKG_VERSION")
                ));
            }
        }

        // A file written by an older build has its text column populated but
        // no postings, because the BM25 leg used to be Turso's `USING fts`
        // index. Left alone, every free-text recall would answer "nothing
        // found" — the worst failure available, since it is indistinguishable
        // from an honest empty result. Rebuild once, here, and say so.
        if store.index_text && indexed_text > 0 && store.fts_docs == 0 {
            store.rebuild_text_index()?;
            // Report documents indexed, not the rebuild's backfill count —
            // that counts rows whose text column was NULL, which is zero on
            // exactly the files this branch exists for.
            let indexed = store.fts_docs;
            store.warnings.push(format!(
                "text index rebuilt on open ({indexed} grains): this file was written when the \
                 BM25 leg used Turso's experimental FTS index, which is no longer used. \
                 One-time; later opens skip it"
            ));
        }

        // Same reasoning for the link indexes (`prov_idx`, `run_idx`, and the
        // `related_to` cross-link triples). A file written before they existed
        // answers every provenance and run question with an empty result, which
        // is indistinguishable from an honest "nothing derived from this" — so
        // heal on open rather than leaving it to a `deja reindex` the caller has
        // no way to know they need.
        //
        // The stamp is what makes this decidable: emptiness alone cannot
        // distinguish "never indexed" from "nothing to index". A stamp that
        // does not match the current version heals too, so widening what the
        // indexes hold is a constant bump rather than a migration.
        if meta.get(LINK_INDEX_KEY).map(String::as_str) != Some(LINK_INDEX_VERSION) {
            if grain_count > 0 {
                let rows = store.rebuild_link_indexes()?;
                store.warnings.push(format!(
                    "link indexes rebuilt on open ({rows} rows across {grain_count} grains): this \
                     file was written before reverse provenance, run correlation and related_to \
                     cross-links were indexed. One-time; later opens skip it"
                ));
            } else {
                // Nothing to build, but stamp anyway so the next open of a file
                // that has since been written to does not re-scan it.
                store.meta_put(LINK_INDEX_KEY, LINK_INDEX_VERSION)?;
            }
        }

        Ok(store)
    }

    /// Install an embedding backend; subsequent adds embed their text
    /// and the vector leg joins hybrid recall.
    ///
    /// The first installed backend is recorded in the file's `meta` table
    /// as embedding provenance (model + dim). A later open that injects a
    /// different-dim backend gets a reconciliation warning instead of
    /// silently mixing vector spaces.
    pub fn set_embedder(&mut self, e: Box<dyn EmbedBackend>) {
        let (model, dim) = (e.model().to_string(), e.dim());
        // Make the vector storage usable FIRST (the postgres backend creates
        // its vector(dim) table here and hard-refuses a dim mismatch). On
        // failure the embedder is NOT installed — recall fails soft to the
        // structural/BM25 legs instead of every add failing mid-transaction.
        if let Err(err) = self.db.ensure_embeddings(dim) {
            self.warnings
                .push(format!("vector recall disabled — embedder {model}@{dim} not installed: {err}"));
            return;
        }
        match &self.meta_embed {
            Some((m, d)) => {
                if *d != dim {
                    self.warnings.push(format!(
                        "embedding mismatch: file vectors are {m}@{d}, injected backend is \
                         {model}@{dim} — vector recall may be degraded"
                    ));
                } else if *m != model && m != "unspecified" && model != "unspecified" {
                    self.warnings.push(format!(
                        "embedding model differs: file declares {m}, injected {model} (same dim {dim})"
                    ));
                }
            }
            None => {
                let ok = self
                    .db
                    .execute(
                        "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_model', ?1)",
                        vec![pt(&model)],
                    )
                    .and_then(|_| {
                        self.db.execute(
                            "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_dim', ?1)",
                            vec![pt(&dim.to_string())],
                        )
                    });
                if ok.is_ok() {
                    self.meta_embed = Some((model, dim));
                }
            }
        }
        self.embedder = Some(e);
    }

    /// Install a cross-encoder reranker (Tier-2). Opt-in per query via
    /// `RecallTuning::rerank`; with none installed, requesting rerank is a
    /// no-op (fusion order stands). Host owns the model — no ML dep in-engine.
    pub fn set_reranker(&mut self, r: Box<dyn RerankBackend>) {
        self.reranker = Some(r);
    }

    /// Whether a reranker backend is installed.
    pub fn has_reranker(&self) -> bool {
        self.reranker.is_some()
    }

    /// Install a custom query expander (Tier-1). When unset, requesting
    /// `RecallTuning::query_expansion` falls back to the built-in English
    /// [`EnglishExpander`]. Install your own for other languages/domains.
    pub fn set_query_expander(&mut self, e: Box<dyn QueryExpander>) {
        self.expander = Some(e);
    }

    /// Documents currently carried by the BM25 index.
    ///
    /// [`rebuild_text_index`](Self::rebuild_text_index) returns how many rows
    /// needed their *text column* backfilled, which is zero on a file that was
    /// already populated — this is the number that answers "did the rebuild
    /// actually index anything".
    pub fn indexed_documents(&self) -> i64 {
        self.fts_docs
    }

    /// Whether the BM25 text index is populated on writes (file-declared,
    /// honored or re-stamped at open).
    pub fn index_text_enabled(&self) -> bool {
        self.index_text
    }

    // ── CAL host metadata (saved queries, custom templates) ─────────────
    //
    // These are *not* memories. A saved query or a custom template is
    // host-managed metadata that belongs to the file so it travels with it
    // and works from the CLI and MCP as well as the console — so it rides
    // the `meta` k/v table, not the grain store. One row per entry
    // (`qry:<name>`, `tpl:<name>`) so recording a last-run timestamp does
    // not rewrite the whole set.

    /// Read every `meta` row whose key starts with `prefix`, returning
    /// `(key-without-prefix, value)` pairs.
    pub fn meta_scan(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        // `%` and `_` are LIKE wildcards, so a prefix containing either would
        // silently widen the scan. Escape them and say so with ESCAPE; the
        // `strip_prefix` check below is the backstop, not the mechanism.
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{escaped}%");
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT k, v FROM meta WHERE k LIKE ?1 ESCAPE '\\'",
            vec![pt(&pattern)],
        )? {
            if let (Some(k), Some(v)) = (row.text(0), row.text(1)) {
                if let Some(rest) = k.strip_prefix(prefix) {
                    out.push((rest.to_string(), v.to_string()));
                }
            }
        }
        Ok(out)
    }

    /// Upsert a single `meta` row.
    /// Read one `meta` row. `None` for a missing key — a file that has never
    /// declared something is not an error.
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let rows = self.db.query("SELECT v FROM meta WHERE k = ?1", vec![pt(key)])?;
        Ok(rows.first().and_then(|row| row.text(0)).map(str::to_string))
    }

    pub fn meta_put(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES (?1, ?2)",
            vec![pt(key), pt(value)],
        )?;
        Ok(())
    }

    /// Delete a single `meta` row. A missing key is not an error.
    pub fn meta_delete(&self, key: &str) -> Result<()> {
        self.db.execute("DELETE FROM meta WHERE k = ?1", vec![pt(key)])?;
        Ok(())
    }

    /// Suspend BM25 posting writes ahead of a bulk load.
    ///
    /// Postings cost is per-token, so a bulk load no longer *needs* this the
    /// way it did when the leg was Turso's FTS index — but writing postings
    /// once at the end still beats writing them per row, and importers already
    /// pair this with [`Self::rebuild_text_index`].
    ///
    /// The `text` column keeps populating throughout, so the rebuild has
    /// everything it needs. Crash-safe: deferral is process state, not file
    /// state, so a process that dies mid-load reopens with an incomplete index
    /// and the open-time check rebuilds it.
    ///
    /// Index-layer only — stored blobs are never touched. Returns `false`
    /// when there was nothing to defer (text indexing off, or already
    /// deferred).
    pub fn defer_text_index(&mut self) -> Result<bool> {
        if !self.index_text || self.fts_deferred {
            return Ok(false);
        }
        self.fts_deferred = true;
        Ok(true)
    }

    /// (Re)build the FTS index. Backfills the `text` column for rows written
    /// while text indexing was off — deriving the same `projected_text`
    /// the inline write path uses — then re-creates the index, which indexes
    /// every existing row. Pairs with [`Self::defer_text_index`] around bulk
    /// loads, and turns a file that flipped `--index-text true` after the
    /// fact into a fully searchable one. Index-layer only — stored blobs are
    /// never touched; forgotten grains are gone from `grains` and cannot be
    /// resurrected. Returns the number of rows whose text was backfilled.
    ///
    /// Errors when the file declares text indexing off — reopen with
    /// `index_text: true` (CLI `--index-text true`) first.
    pub fn rebuild_text_index(&mut self) -> Result<usize> {
        if !self.index_text {
            return Err(DejaDbError::Validation(
                "text indexing is off for this file — reopen with --index-text true \
                 (open_with index_text) before rebuilding the FTS index"
                    .to_string(),
            ));
        }
        // 1) Backfill NULL text from the immutable blobs (cheap: no index yet
        //    or index about to be rebuilt; the projection is identical to the
        //    write path's).
        let mut updates: Vec<(i64, String)> = Vec::new();
        for row in self.db.query("SELECT seq, blob FROM grains WHERE text IS NULL", vec![])? {
            let seq = row.i64(0).unwrap_or(0);
            if let Some(b) = row.blob(1) {
                let view = deserialize_blob(&b)?;
                if let Some(t) = projected_text(&view) {
                    updates.push((seq, t));
                }
            }
        }
        let backfilled = updates.len();
        with_txn(self.db.as_ref(), || {
            for (seq, t) in &updates {
                self.db.execute(
                    "UPDATE grains SET text = ?1 WHERE seq = ?2",
                    vec![pt(t), pi(*seq)],
                )?;
            }
            Ok(())
        })?;
        // 2) Rebuild the postings from scratch. Cheaper than reconciling: the
        //    text column is the source of truth and re-tokenizing it is linear
        //    in total text, which is the same work an incremental fixup would
        //    do anyway.
        self.fts_deferred = false;
        self.db.execute("DELETE FROM fts_post", vec![])?;
        self.db.execute("DELETE FROM fts_doc", vec![])?;
        let mut texts: Vec<(i64, i64, String)> = Vec::new();
        for row in self
            .db
            .query("SELECT seq, ns, text FROM grains WHERE text IS NOT NULL", vec![])?
        {
            let seq = row.i64(0).unwrap_or(0);
            let ns = row.i64(1).unwrap_or(0);
            if let Some(t) = row.text(2) {
                texts.push((seq, ns, t.to_string()));
            }
        }

        // Resolve every vocabulary id first (needs `&mut self`), then write the
        // postings in one transaction.
        //
        // `(seq, ns, doc length, [(vocab id, term frequency)])`.
        type PreparedDoc = (i64, i64, i64, Vec<(i64, i64)>);
        let mut prepared: Vec<PreparedDoc> = Vec::with_capacity(texts.len());
        let (mut docs, mut total_len) = (0i64, 0i64);
        for (seq, ns, text) in &texts {
            let (freqs, len) = token_freqs(text);
            if freqs.is_empty() {
                continue;
            }
            let mut ids = Vec::with_capacity(freqs.len());
            for (term, tf) in freqs {
                ids.push((self.fts_term_id(&term)?, tf));
            }
            docs += 1;
            total_len += len;
            prepared.push((*seq, *ns, len, ids));
        }
        with_txn(self.db.as_ref(), || {
            for (seq, ns, len, ids) in &prepared {
                for (term, tf) in ids {
                    self.db.execute_hot(
                        "INSERT INTO fts_post(term,seq,ns,tf) VALUES (?1,?2,?3,?4)",
                        vec![pi(*term), pi(*seq), pi(*ns), pi(*tf)],
                    )?;
                }
                self.db.execute_hot(
                    "INSERT OR REPLACE INTO fts_doc(seq,len) VALUES (?1,?2)",
                    vec![pi(*seq), pi(*len)],
                )?;
            }
            Ok(())
        })?;
        self.fts_docs = docs;
        self.fts_total_len = total_len;
        Ok(backfilled)
    }

    /// Dimension of the installed embedding backend, if any. `None` means
    /// the vector recall leg is off for this store.
    pub fn embedder_dim(&self) -> Option<usize> {
        self.embedder.as_ref().map(|e| e.dim())
    }

    /// Embedding provenance declared by the file (model, dim), if any
    /// vectors were ever written.
    pub fn declared_embedding(&self) -> Option<(&str, usize)> {
        self.meta_embed.as_ref().map(|(m, d)| (m.as_str(), *d))
    }

    /// Reconciliation warnings from open / set_embedder: file declarations
    /// vs what this session supplied. Empty when everything agrees.
    pub fn open_warnings(&self) -> &[String] {
        &self.warnings
    }

    fn next_hlc(&mut self) -> i64 {
        let wall = now_ms() << 16;
        self.hlc_last = if wall > self.hlc_last { wall } else { self.hlc_last + 1 };
        self.hlc_last
    }

    /// Dictionary-encode a term (cached; inserts on miss).
    fn term_id(&mut self, term: &str) -> Result<i64> {
        if let Some(id) = self.dict.get(term) {
            return Ok(*id);
        }
        let id = self.next_term;
        self.next_term += 1;
        self.db
            .execute("INSERT INTO terms(id, term) VALUES (?1, ?2)", vec![pi(id), pt(term)])?;
        self.dict.insert(term.to_string(), id);
        Ok(id)
    }

    fn term_lookup(&self, term: &str) -> Option<i64> {
        self.dict.get(term).copied()
    }

    fn term_str(&self, id: i64) -> Option<String> {
        self.dict
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.clone())
    }

    // ----- write path -----

    /// Add one grain (full txn). Returns its content address.
    pub fn add<G: Grain + 'static>(&mut self, grain: &G) -> Result<Hash> {
        self.add_batch_inner(std::slice::from_ref(&(grain as &dyn AddableDyn)))
            .map(|mut v| v.remove(0))
    }

    /// Batched add — one txn for the whole slice (voice write-back path).
    pub fn add_batch(&mut self, grains: &[&dyn AddableDyn]) -> Result<Vec<Hash>> {
        self.add_batch_inner(grains)
    }

    /// Value-level idempotent add. When the grain carries a full
    /// `(subject, relation, object)` triple and the current head for
    /// `(ns, subject, relation)` already holds this exact object, nothing is
    /// written and the existing head's hash is returned with `false`.
    /// Otherwise it behaves like [`add`](Self::add) and returns `true`.
    ///
    /// This collapses a re-learned *value*, not merely a byte-identical
    /// replay: unlike content addressing it ignores `created_at` and the rest
    /// of the envelope, keying only on `(ns, subject, relation, object)`
    /// against the current provisional head. Grains without a full triple
    /// always insert. Paraphrased near-duplicates are a *different* object and
    /// out of scope here — those need a host-side (embedding) novelty check.
    pub fn add_if_novel<G: Grain + 'static>(&mut self, grain: &G) -> Result<(Hash, bool)> {
        self.add_dyn_if_novel(grain as &dyn AddableDyn)
    }

    fn add_dyn_if_novel(&mut self, grain: &dyn AddableDyn) -> Result<(Hash, bool)> {
        let (blob, _hash) = grain.serialize_dyn()?;
        let gv = extract_view(&deserialize_blob(&blob)?);
        if let (Some(sj), Some(rl), Some(ob)) =
            (gv.subject.as_deref(), gv.relation.as_deref(), gv.object.as_deref())
        {
            // All three terms must already exist for a prior head to match;
            // a never-seen object can't be a duplicate, so we skip the probe.
            if let (Some(ns_id), Some(s_id), Some(p_id), Some(o_id)) = (
                self.term_lookup(&gv.ns),
                self.term_lookup(sj),
                self.term_lookup(rl),
                self.term_lookup(ob),
            ) {
                if let Some(existing) = self.head_hash_for_object(ns_id, s_id, p_id, o_id)? {
                    return Ok((existing, false));
                }
            }
        }
        let h = self
            .add_batch_inner(std::slice::from_ref(&grain))?
            .remove(0);
        Ok((h, true))
    }

    /// Hash of the current provisional head for `(ns, s, p)` iff its object is
    /// exactly `o` — the µs probe behind [`add_if_novel`](Self::add_if_novel).
    fn head_hash_for_object(&mut self, ns: i64, s: i64, p: i64, o: i64) -> Result<Option<Hash>> {
        let rows = self.db.query(
            "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4",
            vec![pi(ns), pi(s), pi(p), pi(o)],
        )?;
        match rows.first().and_then(|row| row.blob(0)) {
            Some(b) => Ok(Some(Hash::try_from_bytes(&b)?)),
            None => Ok(None),
        }
    }

    /// Serialize-side preparation shared by `add_batch` and bundle import.
    fn prep_from_blob(&mut self, blob: Vec<u8>, hash: Hash) -> Result<GrainPrep> {
        let view = deserialize_blob(&blob)?;
        let gv = extract_view(&view);
        let ns_id = self.term_id(&gv.ns)?;
        let (mut s, mut p, mut o, mut osp) = (None, None, None, false);
        if let (Some(sj), Some(rl), Some(ob)) = (&gv.subject, &gv.relation, &gv.object) {
            s = Some(self.term_id(sj)?);
            p = Some(self.term_id(rl)?);
            o = Some(self.term_id(ob)?);
            osp = self.entity_rels.contains(rl.as_str());
        }
        let session = match &gv.session {
            Some(x) => Some(self.term_id(x)?),
            None => None,
        };
        let projected = projected_text(&view);
        let text = if self.index_text { projected.clone() } else { None };
        // Resolving vocabulary ids needs `&mut self`, so it happens here in
        // prep rather than in the insert, which only has the connection.
        let (tokens, doc_len) = match (&text, self.fts_deferred) {
            (Some(t), false) => {
                let (freqs, len) = token_freqs(t);
                let mut ids = Vec::with_capacity(freqs.len());
                for (term, tf) in freqs {
                    ids.push((self.fts_term_id(&term)?, tf));
                }
                (ids, len)
            }
            _ => (Vec::new(), 0),
        };
        let embed_text = projected;
        let embedding = match (&self.embedder, &embed_text) {
            (Some(e), Some(t)) => Some(e.embed(t)?),
            _ => None,
        };
        // Cross-grain links are subject-ed on the grain's own hash, so a link
        // is queryable from either end without inventing a synthetic node.
        let mut links = Vec::with_capacity(gv.links.len());
        if !gv.links.is_empty() {
            let self_id = self.term_id(&hash.to_hex())?;
            for (rel, target) in &gv.links {
                links.push((self_id, self.term_id(rel)?, self.term_id(target)?));
            }
        }
        let run = match &gv.run {
            Some(r) => Some(self.term_id(r)?),
            None => None,
        };
        // A malformed parent address is dropped rather than failing the write:
        // provenance is an index, and an unindexable link must not cost the
        // grain itself.
        let parent = gv
            .parent
            .as_deref()
            .and_then(|p| Hash::from_hex(p).ok())
            .map(|h| h.as_bytes().to_vec());
        Ok(GrainPrep {
            blob,
            hash,
            ns_id,
            s,
            p,
            o,
            osp,
            session,
            vf: gv.vf,
            vt: gv.vt,
            created: gv.created_at,
            gtype: gv.gtype as i64,
            text,
            embedding,
            tokens,
            doc_len,
            links,
            run,
            parent,
        })
    }

    /// Resolve a token to its `fts_vocab` id, assigning one if new.
    ///
    /// Unlike the triple dictionary this is not preloaded at open, so a miss
    /// costs a round trip. The cache makes that a once-per-token-per-process
    /// cost, and real text reuses tokens heavily.
    fn fts_term_id(&mut self, term: &str) -> Result<i64> {
        if let Some(id) = self.fts_vocab.get(term) {
            return Ok(*id);
        }
        self.db
            .execute("INSERT OR IGNORE INTO fts_vocab(term) VALUES (?1)", vec![pt(term)])?;
        let rows = self
            .db
            .query("SELECT id FROM fts_vocab WHERE term = ?1", vec![pt(term)])?;
        let id = match rows.first().and_then(|row| row.i64(0)) {
            Some(id) => id,
            None => return Err(DejaDbError::Storage("fts_vocab insert vanished".into())),
        };
        self.fts_vocab.insert(term.to_string(), id);
        Ok(id)
    }

    fn add_batch_inner(&mut self, grains: &[&dyn AddableDyn]) -> Result<Vec<Hash>> {
        let (preps, first_seq, first_op, hlc0) = self.prep_and_reserve(grains)?;
        // A grain type newer than any pre-1.5 build could decode makes this file
        // unreadable to those builds — `deserialize_blob` errors on an unknown
        // type byte rather than skipping it. Record that requirement in the file
        // so it is a statement the memory makes about itself (byte 2 of the .mg
        // header is the type byte).
        if self.needs_min_reader_stamp
            && preps
                .iter()
                .any(|p| p.blob.get(2).is_some_and(|b| *b > LEGACY_MAX_GRAIN_BYTE))
        {
            self.meta_put(MIN_READER_VERSION_KEY, env!("CARGO_PKG_VERSION"))?;
            self.needs_min_reader_stamp = false;
        }
        let hashes: Vec<Hash> = preps.iter().map(|p| p.hash).collect();
        let (d_docs, d_len) = fts_delta(&preps);
        with_txn(self.db.as_ref(), || {
            insert_prepped(self.db.as_ref(), &preps, first_seq, first_op, hlc0)
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(hashes)
    }

    /// Serialize + dictionary-encode `grains` and reserve their seq/op/hlc
    /// counters — the pre-transaction half of an add, performing NO writes. Kept
    /// separate from [`insert_prepped`] so `supersede`/`merge_heads` can run the
    /// insert body inside their OWN atomic transaction, alongside the index
    /// flip, instead of committing the add first and flipping in a second txn.
    fn prep_and_reserve(
        &mut self,
        grains: &[&dyn AddableDyn],
    ) -> Result<(Vec<GrainPrep>, i64, i64, i64)> {
        // Serialize + extract + dictionary-encode before entering the txn.
        let mut preps = Vec::with_capacity(grains.len());
        for g in grains {
            let (blob, hash) = g.serialize_dyn()?;
            preps.push(self.prep_from_blob(blob, hash)?);
        }
        let first_seq = self.next_seq;
        self.next_seq += preps.len() as i64;
        let first_op = self.next_op;
        self.next_op += preps.len() as i64;
        let hlc0 = self.next_hlc();
        self.hlc_last = hlc0 + preps.len() as i64 - 1;
        Ok((preps, first_seq, first_op, hlc0))
    }

    // ----- read path -----

    /// Backfill the secondary indexes that were added after this file may have
    /// been written: reverse provenance (`prov_idx`), run correlation
    /// (`run_idx`), and the `related_to` cross-link triples.
    ///
    /// Returns the number of index rows written. Idempotent — the tables are
    /// cleared first, so running it twice is not double-counting. Reads every
    /// blob once, so it is a maintenance operation, not a hot path.
    pub fn rebuild_link_indexes(&mut self) -> Result<usize> {
        self.db.execute("DELETE FROM prov_idx", vec![])?;
        self.db.execute("DELETE FROM run_idx", vec![])?;
        let mut rows: Vec<(i64, i64, Vec<u8>)> = Vec::new();
        for row in self
            .db
            .query("SELECT seq, ns, blob FROM grains ORDER BY seq", vec![])?
        {
            let (Some(seq), Some(ns), Some(blob)) = (row.i64(0), row.i64(1), row.blob(2)) else {
                continue;
            };
            rows.push((seq, ns, blob));
        }

        // Resolve dictionary ids first: term interning needs `&mut self`, and
        // the write loop below only has the connection.
        struct Row {
            seq: i64,
            ns: i64,
            run: Option<i64>,
            parent: Option<Vec<u8>>,
            links: Vec<(i64, i64, i64)>,
        }
        let mut plan: Vec<Row> = Vec::with_capacity(rows.len());
        for (seq, ns, blob) in &rows {
            let view = deserialize_blob(blob)?;
            let gv = extract_view(&view);
            let run = match &gv.run {
                Some(r) => Some(self.term_id(r)?),
                None => None,
            };
            let parent = gv
                .parent
                .as_deref()
                .and_then(|p| Hash::from_hex(p).ok())
                .map(|h| h.as_bytes().to_vec());
            let mut links = Vec::with_capacity(gv.links.len());
            if !gv.links.is_empty() {
                let self_id = self.term_id(&view.hash.to_hex())?;
                for (rel, target) in &gv.links {
                    links.push((self_id, self.term_id(rel)?, self.term_id(target)?));
                }
            }
            plan.push(Row { seq: *seq, ns: *ns, run, parent, links });
        }

        let written = with_txn(self.db.as_ref(), || {
            let mut n = 0usize;
            for r in &plan {
                if let Some(run) = r.run {
                    self.db.execute(
                        "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                        vec![pi(r.ns), pi(run), pi(r.seq)],
                    )?;
                    n += 1;
                }
                if let Some(ref p) = r.parent {
                    self.db.execute(
                        "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                        vec![pi(r.ns), pb(p.clone()), pi(r.seq)],
                    )?;
                    n += 1;
                }
                for (ls, lp, lo) in &r.links {
                    // Replayed rather than appended: a re-run must not stack
                    // duplicate edges onto the traversal index.
                    self.db.execute(
                        "DELETE FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4 AND seq=?5",
                        vec![pi(r.ns), pi(*ls), pi(*lp), pi(*lo), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "DELETE FROM osp WHERE ns=?1 AND o=?2 AND s=?3 AND p=?4 AND seq=?5",
                        vec![pi(r.ns), pi(*lo), pi(*ls), pi(*lp), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(r.ns), pi(*ls), pi(*lp), pi(*lo), pi(r.seq)],
                    )?;
                    self.db.execute(
                        "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(r.ns), pi(*lo), pi(*ls), pi(*lp), pi(r.seq)],
                    )?;
                    n += 1;
                }
            }
            Ok(n)
        })?;
        // Declare the file current, so open() stops re-scanning it.
        self.meta_put(LINK_INDEX_KEY, LINK_INDEX_VERSION)?;
        Ok(written)
    }

    /// Reverse provenance: every grain whose `derived_from` is exactly
    /// `parent`, newest first. This is the credit-assignment / episode-unlearn
    /// query — "which lessons were distilled from this observation?" or "what
    /// did the agent learn from this bad session?". Superseded versions are
    /// included so the full derived lineage is visible; the caller can revise
    /// or `forget` each hash.
    ///
    /// Served by `prov_idx`. It used to read and deserialize **every grain in
    /// the store** on each call, which made a provenance question cost the
    /// whole corpus. Files written before that index existed need `reindex`.
    pub fn grains_derived_from(&mut self, parent: &Hash) -> Result<Vec<DeserializedGrain>> {
        let key = parent.as_bytes().to_vec();
        self.db
            .query(
                "SELECT g.blob FROM prov_idx p JOIN grains g ON g.seq = p.seq
                 WHERE p.parent = ?1 ORDER BY p.seq DESC",
                vec![pb(key)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect()
    }

    /// Every grain recorded during `run_id`, newest first — the run's own
    /// transcript.
    ///
    /// `run_id` is the only run-scoped correlation key in the grain model
    /// (OMS §8.2, Event). Pair with [`Self::run_yield`] for what the run
    /// *produced* downstream.
    pub fn run_trace(&mut self, ns: &str, run_id: &str, limit: usize) -> Result<Vec<DeserializedGrain>> {
        let (Some(ns_id), Some(run)) = (self.term_lookup(ns), self.term_lookup(run_id)) else {
            return Ok(Vec::new());
        };
        let limit = limit.min(1024) as i64;
        self.db
            .query(
                "SELECT g.blob FROM run_idx r JOIN grains g ON g.seq = r.seq
                 WHERE r.ns = ?1 AND r.run = ?2 ORDER BY r.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(run), pi(limit)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect()
    }

    /// What a run produced downstream: the grains derived from the run's own
    /// grains — extracted facts, distilled lessons — that are not themselves
    /// part of the run.
    ///
    /// This is the join the two indexes exist for: it crosses from execution
    /// history into semantic memory in one call. A transcript answers "what
    /// happened"; this answers "what did we keep".
    pub fn run_yield(&mut self, ns: &str, run_id: &str, limit: usize) -> Result<Vec<DeserializedGrain>> {
        let trace = self.run_trace(ns, run_id, limit)?;
        let in_run: HashSet<Hash> = trace.iter().map(|g| g.hash).collect();
        let mut seen: HashSet<Hash> = in_run.clone();
        let mut out = Vec::new();
        for g in &trace {
            for child in self.grains_derived_from(&g.hash)? {
                if !in_run.contains(&child.hash) && seen.insert(child.hash) {
                    out.push(child);
                    if out.len() >= limit {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Which runs touched this grain — the reverse join, from a piece of
    /// semantic memory back into execution history.
    ///
    /// Walks the provenance chain in both directions from `hash` (ancestors via
    /// `derived_from`, descendants via the reverse index) and collects the
    /// `run_id` of everything reachable, including the grain itself. Bounded by
    /// `depth` hops, because a long-lived memory's lineage is unbounded.
    ///
    /// Note this records runs that *produced or refined* the grain, not runs
    /// that merely read it — a read leaves no grain, so nothing in an
    /// append-only store can attest to it.
    pub fn runs_touching(&mut self, ns: &str, hash: &Hash, depth: usize) -> Result<Vec<String>> {
        let depth = depth.min(8);
        let mut runs: Vec<String> = Vec::new();
        let mut seen_run: HashSet<String> = HashSet::new();
        let mut visited: HashSet<Hash> = HashSet::new();
        let mut frontier = vec![*hash];
        visited.insert(*hash);

        for _ in 0..=depth {
            let mut next: Vec<Hash> = Vec::new();
            for h in std::mem::take(&mut frontier) {
                let Ok(g) = self.get(&h) else { continue };
                if let Some(r) = g.get_str("run_id") {
                    if seen_run.insert(r.to_string()) {
                        runs.push(r.to_string());
                    }
                }
                // Up: the grain this one was derived from. Namespace-filtered
                // like the downward leg — a lineage walk that stays in `ns` in
                // one direction and leaves it in the other reports runs the
                // caller did not ask about.
                if let Some(p) = g.get_str("derived_from").and_then(|p| Hash::from_hex(p).ok()) {
                    if !visited.contains(&p)
                        && self
                            .get(&p)
                            .is_ok_and(|pg| pg.get_str("namespace") == Some(ns))
                    {
                        visited.insert(p);
                        next.push(p);
                    }
                }
                // Down: grains derived from this one.
                for child in self.grains_derived_from(&h)? {
                    if child.get_str("namespace") == Some(ns) && visited.insert(child.hash) {
                        next.push(child.hash);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(runs)
    }

    /// Recent grains in a namespace, newest first, bounded by `limit`. With
    /// `gtype = None`, every type is returned. This is the "reflect over recent
    /// experience" read path — recent Events / Observations that have no
    /// subject or free-text anchor to hang a structural or BM25 leg on.
    pub fn recent(
        &mut self,
        ns: &str,
        gtype: Option<dejadb_core::types::GrainType>,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recent_inner(ns, gtype, limit, false)
    }

    /// `recent`, restricted to grains nothing has superseded.
    ///
    /// Supersession is index-layer state — the blob is immutable and carries no
    /// marker — so a caller reading grains back cannot tell a stale version from
    /// the head that replaced it. Recall needs that distinction; a scan that
    /// feeds an analyzer generally does not, which is why `recent` keeps its
    /// everything-in-order behaviour and this is a separate entry point.
    pub fn recent_live(
        &mut self,
        ns: &str,
        gtype: Option<dejadb_core::types::GrainType>,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recent_inner(ns, gtype, limit, true)
    }

    fn recent_inner(
        &mut self,
        ns: &str,
        gtype: Option<dejadb_core::types::GrainType>,
        limit: usize,
        live_only: bool,
    ) -> Result<Vec<DeserializedGrain>> {
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let live = if live_only {
            " AND superseded_by IS NULL"
        } else {
            ""
        };
        // The `gtype` column stores the enum ordinal (see `extract_view`:
        // `view.grain_type as u8`), not the .mg header type-byte.
        let gt_ord = gtype.map(|g| g as u8 as i64);
        let rows = match gt_ord {
            Some(gt) => self.db.query(
                &format!("SELECT blob FROM grains WHERE ns=?1 AND gtype=?2{live} ORDER BY seq DESC LIMIT ?3"),
                vec![pi(ns_id), pi(gt), pi(limit as i64)],
            )?,
            None => self.db.query(
                &format!("SELECT blob FROM grains WHERE ns=?1{live} ORDER BY seq DESC LIMIT ?2"),
                vec![pi(ns_id), pi(limit as i64)],
            )?,
        };
        rows.iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect()
    }

    /// Fetch a grain by content address.
    pub fn get(&mut self, hash: &Hash) -> Result<DeserializedGrain> {
        let rows = self.db.query(
            "SELECT blob FROM grains WHERE hash = ?1",
            vec![pb(hash.as_bytes().to_vec())],
        )?;
        let blob = match rows.first() {
            Some(row) => row
                .blob(0)
                .ok_or_else(|| DejaDbError::Storage("blob column not a blob".into()))?,
            None => return Err(DejaDbError::NotFound(*hash)),
        };
        deserialize_blob(&blob)
    }

    /// Structural recall: current grains about `subject` (optionally filtered
    /// by relation), newest first, k-bounded. The voice hot path.
    pub fn recall(
        &mut self,
        ns: &str,
        subject: &str,
        relation: Option<&str>,
        k: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let start = std::time::Instant::now();
        let (ns_id, s_id) = match (self.term_lookup(ns), self.term_lookup(subject)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r) {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        // Probe + blob fetch in ONE statement: the join replaces the old
        // per-seq fetch loop (1 + k round trips), which matters on any
        // backend where a statement is not a function call.
        let rows = match p_id {
            Some(p) => self.db.query_hot(
                "SELECT g.blob FROM triples t JOIN grains g ON g.seq = t.seq
                  WHERE t.ns=?1 AND t.s=?2 AND t.p=?3 AND t.cur=1
                  ORDER BY t.seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            None => self.db.query_hot(
                "SELECT g.blob FROM triples t JOIN grains g ON g.seq = t.seq
                  WHERE t.ns=?1 AND t.s=?2 AND t.cur=1
                  ORDER BY t.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(b) = row.blob(0) {
                out.push(deserialize_blob(&b)?);
            }
        }
        // Structural recall feeds telemetry too, so `cold_grains` doesn't
        // false-positive on grains that are recalled by subject (not query).
        self.record_recall_event(ns, Some(subject), relation, None, &out, start);
        Ok(out)
    }

    /// Current value head for (subject, relation) — the µs point read.
    pub fn latest(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Option<DeserializedGrain>> {
        let (ns_id, s_id, p_id) = match (
            self.term_lookup(ns),
            self.term_lookup(subject),
            self.term_lookup(relation),
        ) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => return Ok(None),
        };
        let hash = self
            .db
            .query_hot(
                "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3",
                vec![pi(ns_id), pi(s_id), pi(p_id)],
            )?
            .first()
            .and_then(|row| row.blob(0));
        match hash {
            Some(h) => {
                let h = Hash::try_from_bytes(&h)?;
                Ok(Some(self.get(&h)?))
            }
            None => Ok(None),
        }
    }

    /// Last `n` events of a session, oldest→newest (transcript tail).
    pub fn thread_tail(&mut self, ns: &str, session: &str, n: usize) -> Result<Vec<DeserializedGrain>> {
        let (ns_id, sess_id) = match (self.term_lookup(ns), self.term_lookup(session)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let rows = self.db.query(
            "SELECT g.blob FROM thread_idx ti JOIN grains g ON g.seq = ti.seq
              WHERE ti.ns=?1 AND ti.session=?2 ORDER BY ti.seq DESC LIMIT ?3",
            vec![pi(ns_id), pi(sess_id), pi(n as i64)],
        )?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows.iter().rev() {
            if let Some(b) = row.blob(0) {
                out.push(deserialize_blob(&b)?);
            }
        }
        Ok(out)
    }

    // ----- evolution path -----

    /// Supersede `old` with `new_grain` (atomic, OMS L2 semantics).
    /// Sets `derived_from` on the new grain; the old grain's blob is never
    /// touched — only its index-layer fields change.
    pub fn supersede<G: Grain + 'static>(&mut self, old: &Hash, new_grain: &mut G) -> Result<Hash> {
        // Old head must exist and be current.
        let rows = self.db.query(
            "SELECT seq, ns, s, p, svt FROM grains WHERE hash = ?1",
            vec![pb(old.as_bytes().to_vec())],
        )?;
        let (old_seq, _ns, old_s, old_p, old_svt) = match rows.first() {
            Some(row) => (
                row.i64(0).unwrap_or(0),
                row.i64(1),
                row.i64(2),
                row.i64(3),
                row.i64(4),
            ),
            None => return Err(DejaDbError::NotFound(*old)),
        };
        if old_svt.is_some() {
            return Err(DejaDbError::SupersessionConflict(*old));
        }

        new_grain.common_mut().derived_from = Some(old.to_hex());
        // Prep the new grain and reserve its counters WITHOUT writing, so the
        // insert body runs inside the SAME transaction as the flip below — the
        // add and the supersession commit atomically. (Previously add() committed
        // in its own txn, then a second txn flipped the old grain; a crash or
        // error between them left the new grain current while the old stayed
        // un-superseded, so recall surfaced both values — the OMS-L2 "atomic"
        // guarantee this method documents.)
        let new_dyn: &dyn AddableDyn = &*new_grain;
        let (preps, first_seq, first_op, hlc0) = self.prep_and_reserve(&[new_dyn])?;
        let new_hash = preps[0].hash;
        let now = now_ms();

        let op_seq = self.next_op;
        self.next_op += 1;
        let hlc = self.next_hlc();
        let (d_docs, d_len) = fts_delta(&preps);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            insert_prepped(dbr, &preps, first_seq, first_op, hlc0)?;
            dbr.execute(
                "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                vec![pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)],
            )?;
            dbr.execute(
                "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                vec![pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())],
            )?;
            dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            // Reconcile the OLD key's head/entity_latest indexes. Normally
            // add(new) handles this because new shares (ns,s,p) with old; but
            // when the new grain carries a DIFFERENT (subject,relation) — or
            // no triple — add reconciles the new key and leaves the old key
            // pointing at the now-superseded grain. Mirror forget so
            // latest()/heads() for the old key don't surface it. Harmless
            // no-op in the common same-key case (old is already out of heads).
            if let (Some(ns), Some(s), Some(p)) = (_ns, old_s, old_p) {
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(old_seq)],
                )?;
                dbr.execute(
                    "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(old_seq)],
                )?;
                // Key the join on (ns,s,p), not seq alone: a link-bearing
                // grain has extra triples rows for the same seq, and an
                // unkeyed join lets the engine pick a link target as t.o.
                let rows = dbr.query(
                    "SELECT t.o, h.seq, h.hash, h.created_at
                     FROM heads h JOIN triples t
                       ON t.seq=h.seq AND t.ns=h.ns AND t.s=h.s AND t.p=h.p
                     WHERE h.ns=?1 AND h.s=?2 AND h.p=?3
                     ORDER BY h.created_at DESC, h.hash DESC LIMIT 1",
                    vec![pi(ns), pi(s), pi(p)],
                )?;
                if let Some(row) = rows.first() {
                    let o = row.i64(0).unwrap_or(0);
                    let sq = row.i64(1).unwrap_or(0);
                    let h = row.blob(2).unwrap_or_default();
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(sq), pb(h)],
                    )?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_SUPERSEDE), pb(new_hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(new_hash)
    }

    /// Forget (erase from hot store) — writes a tombstone to the op-log.
    /// File-level crypto-erasure remains the strong path.
    pub fn forget(&mut self, hash: &Hash) -> Result<()> {
        let rows = self.db.query(
            "SELECT seq, ns, s, p FROM grains WHERE hash = ?1",
            vec![pb(hash.as_bytes().to_vec())],
        )?;
        let (seq, ns, s, p) = match rows.first() {
            Some(row) => (row.i64(0).unwrap_or(0), row.i64(1), row.i64(2), row.i64(3)),
            None => return Err(DejaDbError::NotFound(*hash)),
        };
        // Read the document's length before the delete removes it, so the
        // in-memory BM25 collection stats can be corrected afterwards.
        let doc_len = self
            .db
            .query("SELECT len FROM fts_doc WHERE seq = ?1", vec![pi(seq)])?
            .first()
            .and_then(|row| row.i64(0));
        let op_seq = self.next_op;
        self.next_op += 1;
        let hlc = self.next_hlc();
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            dbr.execute("DELETE FROM triples WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM osp WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM embeddings WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM thread_idx WHERE seq=?1", vec![pi(seq)])?;
            // The join's two indexes, for the same reason every other index
            // is reconciled here. `seq` is re-derived as MAX(seq)+1 on open,
            // so forgetting the newest grain hands its seq to the next write
            // — a surviving row would then re-attach a stranger to this
            // grain's parent or run.
            dbr.execute("DELETE FROM prov_idx WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM run_idx WHERE seq=?1", vec![pi(seq)])?;
            // Postings too, or a forgotten grain's words keep answering
            // free-text recall — a tombstone that leaves the text findable
            // is not a tombstone.
            dbr.execute("DELETE FROM fts_post WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM fts_doc WHERE seq=?1", vec![pi(seq)])?;
            dbr.execute("DELETE FROM grains WHERE seq=?1", vec![pi(seq)])?;
            // Reconcile the head/entity_latest indexes for the cell.
            if let (Some(ns), Some(s), Some(p)) = (ns, s, p) {
                // Forget must drop the grain's fork-tip row too — every other
                // index is reconciled here, but `heads` was left dangling, so
                // heads()/open_forks() kept surfacing a hash whose get() fails
                // (and merge_heads could merge a forgotten tip).
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(seq)],
                )?;
                dbr.execute(
                    "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(ns), pi(s), pi(p), pi(seq)],
                )?;
                // Re-elect the provisional head from the surviving tips using
                // the SAME (created_at, hash) rule as heads()/insert_blob —
                // was `ORDER BY seq DESC`, which can disagree with the
                // provisional-head rule in a 3+-way fork after a forget.
                // Key the join on (ns,s,p), not seq alone: a link-bearing
                // grain has extra triples rows for the same seq, and an
                // unkeyed join lets the engine pick a link target as t.o.
                let rows = dbr.query(
                    "SELECT t.o, h.seq, h.hash, h.created_at
                     FROM heads h JOIN triples t
                       ON t.seq=h.seq AND t.ns=h.ns AND t.s=h.s AND t.p=h.p
                     WHERE h.ns=?1 AND h.s=?2 AND h.p=?3
                     ORDER BY h.created_at DESC, h.hash DESC LIMIT 1",
                    vec![pi(ns), pi(s), pi(p)],
                )?;
                if let Some(row) = rows.first() {
                    let o = row.i64(0).unwrap_or(0);
                    let sq = row.i64(1).unwrap_or(0);
                    let h = row.blob(2).unwrap_or_default();
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(sq), pb(h)],
                    )?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_FORGET), pb(hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })?;
        if let Some(len) = doc_len {
            self.fts_docs = (self.fts_docs - 1).max(0);
            self.fts_total_len = (self.fts_total_len - len).max(0);
        }

        // Scrub the telemetry sidecar so a forgotten grain never lingers there.
        // Best-effort: the main erasure already committed, and the sidecar is
        // encrypted under the same key (crypto-erasure covers any residue) and
        // is rebuildable — a scrub hiccup must not fail an accomplished forget.
        if let Some(tel) = self.telemetry.as_mut() {
            let _ = tel.scrub(hash);
        }
        Ok(())
    }

    /// Append one op-log record (fresh local `op_seq`, caller-supplied `hlc`).
    /// The change-feed / sync primitive for state transitions that happen
    /// outside `add`/`supersede`/`forget` — a `merge_heads` fork-closure and an
    /// imported supersession whose grain already existed locally. Without it
    /// those transitions never replicate and downstream replicas diverge.
    fn log_op(&mut self, op: i64, hash: &Hash, hlc: i64) -> Result<()> {
        let op_seq = self.next_op;
        self.next_op += 1;
        self.db.execute(
            "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
            vec![pi(op_seq), pi(hlc), pi(op), pb(hash.as_bytes().to_vec())],
        )?;
        Ok(())
    }

    // ----- telemetry sidecar (host capability; off the recall path) -----

    /// The active telemetry mode — `Off` when no sidecar is attached.
    pub fn telemetry_mode(&self) -> TelemetryMode {
        self.telemetry
            .as_ref()
            .map(|t| t.mode())
            .unwrap_or(TelemetryMode::Off)
    }

    /// Drain buffered recall telemetry into the sidecar. A no-op when the
    /// buffer is empty; called from write ops and on close — never from recall.
    pub fn telemetry_flush(&mut self) -> Result<()> {
        if let Some(tel) = self.telemetry.as_mut() {
            tel.flush()?;
        }
        Ok(())
    }

    /// Record one assembly-budget sample (feeds the `budget_pressure` analyzer).
    pub fn telemetry_note_budget(&mut self, overflow: bool) -> Result<()> {
        if let Some(tel) = self.telemetry.as_mut() {
            tel.note_budget(overflow)?;
        }
        Ok(())
    }

    /// Grain-access rollups (feeds `cold_grains`). Flushes first so buffered
    /// recalls are counted; empty when telemetry is off.
    pub fn telemetry_access_stats(&mut self, ns: Option<&str>) -> Result<Vec<AccessStat>> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.access_stats(ns),
            None => Ok(Vec::new()),
        }
    }

    /// Query rollups (feeds `coverage_gap`). Flushes first; empty when off.
    pub fn telemetry_query_stats(&mut self, ns: Option<&str>) -> Result<Vec<QueryStat>> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.query_stats(ns),
            None => Ok(Vec::new()),
        }
    }

    /// The assembly-budget rollup (feeds `budget_pressure`). Empty when off.
    pub fn telemetry_budget_stats(&mut self) -> Result<BudgetStat> {
        self.telemetry_flush()?;
        match self.telemetry.as_ref() {
            Some(tel) => tel.budget_stats(),
            None => Ok(BudgetStat::default()),
        }
    }

    // ----- graph ops (bounded, indexed, capped) -----

    /// Execution records for a Workflow grain (OMS §8.4).
    ///
    /// Returns `(node_id, executing grain hash)` for every grain carrying a
    /// `mg:step_action:<node_id>` link to `workflow_hash`, newest first. This is
    /// how a run is reconstructed against its plan: the Workflow grain is
    /// immutable and content-addressed, so it never accumulates run state — the
    /// execution records point back at it instead.
    ///
    /// Pass `node_id` to narrow to one step. Results are capped.
    pub fn step_actions(
        &mut self,
        ns: &str,
        workflow_hash: &Hash,
        node_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, Hash)>> {
        let Some(ns_id) = self.term_lookup(ns) else {
            return Ok(Vec::new());
        };
        let Some(target_id) = self.term_lookup(&workflow_hash.to_hex()) else {
            return Ok(Vec::new());
        };
        // Which predicates count: one exact relation, or every step_action.
        let pred_ids: Vec<i64> = match node_id {
            Some(n) => match self.term_lookup(&step_action_relation(n)) {
                Some(id) => vec![id],
                None => return Ok(Vec::new()),
            },
            None => self
                .terms_with_prefix(STEP_ACTION_PREFIX)
                .into_iter()
                .map(|(id, _)| id)
                .collect(),
        };
        if pred_ids.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.min(1024);
        // The OSP index is keyed (ns, o, s) — the link's object is the workflow
        // hash, so this reads the reverse direction directly.
        let mut rows: Vec<(i64, i64, i64)> = Vec::new();
        for p in &pred_ids {
            for row in self.db.query(
                "SELECT seq, s, p FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1
                 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(target_id), pi(*p), pi(limit as i64)],
            )? {
                let (Some(seq), Some(s), Some(p)) = (row.i64(0), row.i64(1), row.i64(2)) else {
                    continue;
                };
                rows.push((seq, s, p));
            }
        }

        // Each predicate was queried and capped separately, and with no
        // `node_id` the predicate set comes from a dictionary scan whose order
        // is a HashMap's — i.e. no order at all. Sort by seq before truncating,
        // or "newest first" is false across nodes and *which* records survive
        // the cap changes from one process to the next.
        rows.sort_unstable_by_key(|(seq, _, _)| std::cmp::Reverse(*seq));

        let mut result = Vec::with_capacity(rows.len());
        for (_, s, p) in rows {
            let (Some(subj), Some(rel)) = (self.term_str(s), self.term_str(p)) else {
                continue;
            };
            let (Some(node), Ok(h)) = (step_action_node(&rel), Hash::from_hex(&subj)) else {
                continue;
            };
            result.push((node.to_string(), h));
        }
        result.truncate(limit);
        Ok(result)
    }

    /// Dictionary terms starting with `prefix`, as `(id, term)`.
    ///
    /// The relation for an execution record is parameterized by node id, so the
    /// vocabulary cannot be enumerated statically. Bounded by the number of
    /// distinct node ids ever written, not by grain count.
    fn terms_with_prefix(&self, prefix: &str) -> Vec<(i64, String)> {
        self.dict
            .iter()
            .filter(|(term, _)| term.starts_with(prefix))
            .map(|(term, id)| (*id, term.clone()))
            .collect()
    }

    /// Grains that name `object` in their object position, newest first.
    ///
    /// The mirror of an anchored `recall_hybrid(ns, Some(subject), …)`: that
    /// answers "what does X point at", this answers "what points at X". Reads
    /// the OSP reverse index, so — like `related(Direction::In)` and for the
    /// same reason — it sees only relations the file declares as entity
    /// relations (`DejaDbOptions::entity_relations`). Reverse lookup over every
    /// relation would need a third full permutation of `triples`, which is the
    /// index the "2½ permutations" design exists to avoid.
    pub fn grains_by_object(
        &mut self,
        ns: &str,
        object: &str,
        limit: usize,
    ) -> Result<Vec<DeserializedGrain>> {
        let (Some(ns_id), Some(obj_id)) = (self.term_lookup(ns), self.term_lookup(object)) else {
            return Ok(Vec::new());
        };
        let limit = limit.min(1024) as i64;
        self.db
            .query(
                "SELECT g.blob FROM osp o JOIN grains g ON g.seq = o.seq
                 WHERE o.ns = ?1 AND o.o = ?2 AND o.cur = 1 ORDER BY o.seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(obj_id), pi(limit)],
            )?
            .iter()
            .filter_map(|row| row.blob(0))
            .map(|b| deserialize_blob(&b))
            .collect()
    }

    /// Bounded k-hop traversal over the given relations.
    /// Returns reached entity terms (excluding the start), BFS order.
    /// `Direction::In`/`Both` use the selective OSP index, so reverse
    /// expansion only sees entity-valued relations.
    pub fn related(
        &mut self,
        ns: &str,
        start: &str,
        relations: &[&str],
        dir: Direction,
        depth: usize,
        cap: usize,
    ) -> Result<Vec<String>> {
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let start_id = match self.term_lookup(start) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let rel_ids: Vec<i64> = relations.iter().filter_map(|r| self.term_lookup(r)).collect();
        if rel_ids.is_empty() {
            return Ok(Vec::new());
        }
        let depth = depth.min(4);
        let cap = cap.min(512);
        let reached = 'bfs: {
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(start_id);
            let mut order: Vec<i64> = Vec::new();
            let mut frontier = vec![start_id];
            for _ in 0..depth {
                let mut next = Vec::new();
                for node in &frontier {
                    for p in &rel_ids {
                        if matches!(dir, Direction::Out | Direction::Both) {
                            for row in self.db.query(
                                "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                                vec![pi(ns_id), pi(*node), pi(*p)],
                            )? {
                                if let Some(o) = row.i64(0) {
                                    if seen.insert(o) {
                                        order.push(o);
                                        next.push(o);
                                        if order.len() >= cap {
                                            break 'bfs order;
                                        }
                                    }
                                }
                            }
                        }
                        if matches!(dir, Direction::In | Direction::Both) {
                            for row in self.db.query(
                                "SELECT s FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1 LIMIT 64",
                                vec![pi(ns_id), pi(*node), pi(*p)],
                            )? {
                                if let Some(s) = row.i64(0) {
                                    if seen.insert(s) {
                                        order.push(s);
                                        next.push(s);
                                        if order.len() >= cap {
                                            break 'bfs order;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
            order
        };
        Ok(reached.into_iter().filter_map(|id| self.term_str(id)).collect())
    }

    /// Bounded bidirectional-ish path search (forward BFS with parents).
    pub fn path(
        &mut self,
        ns: &str,
        from: &str,
        to: &str,
        relations: &[&str],
        max_depth: usize,
    ) -> Result<Option<Vec<String>>> {
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(None),
        };
        let (a, b) = match (self.term_lookup(from), self.term_lookup(to)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(None),
        };
        let rel_ids: Vec<i64> = relations.iter().filter_map(|r| self.term_lookup(r)).collect();
        if rel_ids.is_empty() {
            return Ok(None);
        }
        let max_depth = max_depth.min(6);
        let parents = {
            let mut parent: HashMap<i64, i64> = HashMap::new();
            let mut q = VecDeque::from([a]);
            let mut found = false;
            let mut hops = 0usize;
            let mut visited: HashSet<i64> = HashSet::from([a]);
            'outer: while !q.is_empty() && hops < max_depth {
                let level: Vec<i64> = q.drain(..).collect();
                for node in level {
                    for p in &rel_ids {
                        for row in self.db.query(
                            "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                            vec![pi(ns_id), pi(node), pi(*p)],
                        )? {
                            if let Some(o) = row.i64(0) {
                                if visited.insert(o) {
                                    parent.insert(o, node);
                                    if o == b {
                                        found = true;
                                        break 'outer;
                                    }
                                    q.push_back(o);
                                    if visited.len() > 2048 {
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                }
                hops += 1;
            }
            if found { Some(parent) } else { None }
        };
        Ok(parents.map(|parent| {
            let mut chain = vec![b];
            let mut cur = b;
            while let Some(pr) = parent.get(&cur) {
                chain.push(*pr);
                cur = *pr;
                if cur == a {
                    break;
                }
            }
            chain.reverse();
            chain.into_iter().filter_map(|id| self.term_str(id)).collect()
        }))
    }

    /// Two-axis as-of read.
    pub fn entity_at(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        t: i64,
        axis: Axis,
    ) -> Result<Option<DeserializedGrain>> {
        match axis {
            Axis::Knowledge => {
                // Walk the supersession chain backward from the head.
                let head = match self.latest(ns, subject, relation)? {
                    Some(g) => g.hash,
                    None => return Ok(None),
                };
                let mut cur = head;
                loop {
                    let rows = self.db.query(
                        "SELECT svf, supersedes, blob FROM grains WHERE hash = ?1",
                        vec![pb(cur.as_bytes().to_vec())],
                    )?;
                    let (svf, sup, blob) = match rows.first() {
                        Some(row) => (row.i64(0), row.blob(1), row.blob(2)),
                        None => return Ok(None),
                    };
                    if svf.unwrap_or(i64::MIN) <= t {
                        return Ok(Some(deserialize_blob(&blob.unwrap_or_default())?));
                    }
                    match sup {
                        Some(prev) => cur = Hash::try_from_bytes(&prev)?,
                        None => return Ok(None),
                    }
                }
            }
            Axis::World => {
                // Current knowledge filtered by world validity at T.
                let (ns_id, s_id, p_id) = match (
                    self.term_lookup(ns),
                    self.term_lookup(subject),
                    self.term_lookup(relation),
                ) {
                    (Some(a), Some(b), Some(c)) => (a, b, c),
                    _ => return Ok(None),
                };
                let blob = self
                    .db
                    .query(
                        "SELECT g.blob FROM triples tr JOIN grains g ON g.seq = tr.seq
                         WHERE tr.ns=?1 AND tr.s=?2 AND tr.p=?3
                           AND g.svt IS NULL
                           AND (g.vf IS NULL OR g.vf <= ?4)
                           AND (g.vt IS NULL OR g.vt > ?4)
                         ORDER BY tr.seq DESC LIMIT 1",
                        vec![pi(ns_id), pi(s_id), pi(p_id), pi(t)],
                    )?
                    .first()
                    .and_then(|row| row.blob(0));
                match blob {
                    Some(b) => Ok(Some(deserialize_blob(&b)?)),
                    None => Ok(None),
                }
            }
        }
    }

    /// Whether a grain with this content address exists.
    pub fn has(&mut self, hash: &Hash) -> Result<bool> {
        self.has_grain(hash)
    }

    /// BM25 leg over grain text (facts as "s r o", event content). Returns
    /// live-grain seqs, best match first.
    ///
    /// This is our own inverted index rather than Turso's `USING fts`, and the
    /// reason is measurable: with that index in place, a single-row `INSERT`
    /// costs time proportional to the rows already stored — 1.6 ms at 500,
    /// 64 ms at 4,000, still climbing — and a `MATCH` lookup costs the same
    /// whether one row matches or every row does, which is what an index is
    /// supposed to avoid. Batching does not amortize it. Reproduced against
    /// bare `turso` with no DejaDB code involved, and identical on
    /// `0.8.0-pre.2`, so it is not something a version bump fixes:
    /// <https://github.com/tursodatabase/turso/issues/8170>.
    ///
    /// **When that issue is fixed**, this whole layer is deletable — see the
    /// removal note in `docs/facts/bm25-index.md`. It is deliberately kept to
    /// one table pair plus this function so that stays a small change.
    ///
    /// Postings are read per query term (`idx_fts_post` covers `(term, ns)`),
    /// scored in memory, and only the top candidates are checked for
    /// liveness — so cost tracks the number of documents containing the query
    /// terms, not the size of the file.
    pub fn search_text(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_text_inner(ns, query, k, false)
    }

    /// `search_text`, scoring the whole chain instead of the live heads.
    /// Superseded grains keep their postings (supersede only flips index
    /// state), so the historical text is still there to be found — the liveness
    /// filter is the only thing hiding it.
    pub fn search_text_all(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_text_inner(ns, query, k, true)
    }

    fn search_text_inner(
        &mut self,
        ns: &str,
        query: &str,
        k: usize,
        include_superseded: bool,
    ) -> Result<Vec<i64>> {
        if !self.index_text || k == 0 {
            return Ok(Vec::new()); // BM25 leg disabled (edge profile)
        }
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let terms = tokenize(query);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // `N` counts every indexed document, including ones later superseded.
        // Their postings are still present and only filtered at the end, so
        // using the live count here would make idf disagree with the postings
        // actually being scored. The difference is immaterial to ranking.
        let n_docs = self.fts_docs.max(1) as f64;
        let avgdl = if self.fts_docs > 0 {
            (self.fts_total_len as f64 / self.fts_docs as f64).max(1.0)
        } else {
            1.0
        };

        // One postings pull for ALL distinct tokens (document length comes
        // back with the posting). Scoring still iterates the query's tokens
        // per OCCURRENCE below, so a repeated token contributes twice exactly
        // as it did when each occurrence was its own query.
        let mut distinct: Vec<&String> = Vec::new();
        {
            let mut seen: HashSet<&str> = HashSet::new();
            for t in &terms {
                if seen.insert(t.as_str()) {
                    distinct.push(t);
                }
            }
        }
        let mut by_term: HashMap<String, Vec<(i64, i64, i64)>> = HashMap::new();
        if distinct.len() == 1 || !self.db.prefers_batched_reads() {
            // In-process: one cached indexed probe per distinct token.
            for t in &distinct {
                for row in &self.db.query_hot(
                    "SELECT v.term, p.seq, p.tf, d.len FROM fts_post p
                      JOIN fts_vocab v ON v.id = p.term
                      JOIN fts_doc d ON d.seq = p.seq
                     WHERE v.term = ?1 AND p.ns = ?2",
                    vec![pt(t), pi(ns_id)],
                )? {
                    if let (Some(t), Some(seq), Some(tf), Some(dl)) =
                        (row.text(0), row.i64(1), row.i64(2), row.i64(3))
                    {
                        by_term.entry(t.to_string()).or_default().push((seq, tf, dl.max(1)));
                    }
                }
            }
        } else {
            // Networked backend: all tokens in one round trip.
            let placeholders: Vec<String> =
                (2..distinct.len() + 2).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT v.term, p.seq, p.tf, d.len FROM fts_post p
                  JOIN fts_vocab v ON v.id = p.term
                  JOIN fts_doc d ON d.seq = p.seq
                 WHERE p.ns = ?1 AND v.term IN ({})",
                placeholders.join(",")
            );
            let mut params = vec![pi(ns_id)];
            params.extend(distinct.iter().map(|t| pt(t)));
            for row in &self.db.query(&sql, params)? {
                if let (Some(t), Some(seq), Some(tf), Some(dl)) =
                    (row.text(0), row.i64(1), row.i64(2), row.i64(3))
                {
                    by_term.entry(t.to_string()).or_default().push((seq, tf, dl.max(1)));
                }
            }
        }

        let mut scores: HashMap<i64, f64> = HashMap::new();
        for term in terms {
            let Some(postings) = by_term.get(&term) else {
                continue;
            };
            let df = postings.len() as f64;
            // Robertson/Sparck-Jones idf, +1 inside the log so a term present
            // in every document scores 0 rather than negative.
            let idf = (1.0 + (n_docs - df + 0.5) / (df + 0.5)).ln();
            for (seq, tf, dl) in postings {
                let tf = *tf as f64;
                let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * (*dl as f64 / avgdl));
                *scores.entry(*seq).or_insert(0.0) += idf * (tf * (BM25_K1 + 1.0)) / (tf + norm);
            }
        }
        if scores.is_empty() {
            return Ok(Vec::new());
        }

        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        // Ties broken by seq descending, so equal-scoring matches come back
        // newest-first and the order is stable across runs.
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
            .then(b.0.cmp(&a.0)));
        if include_superseded {
            ranked.truncate(k);
            return Ok(ranked.into_iter().map(|(s, _)| s).collect());
        }
        // Over-fetch before the liveness filter: superseded grains keep their
        // postings, so trimming to k first could return fewer than k live hits
        // when a chain has been updated often.
        ranked.truncate(k.saturating_mul(4).max(k));
        let live = self.live_seqs(ranked.iter().map(|(s, _)| *s))?;
        Ok(ranked
            .into_iter()
            .filter(|(s, _)| live.contains(s))
            .map(|(s, _)| s)
            .take(k)
            .collect())
    }

    /// Which of `seqs` are still live (present and not superseded). One query
    /// for the whole candidate set — the ids come from our own tables, so
    /// inlining them carries no injection risk and avoids a bind per id.
    fn live_seqs(&mut self, seqs: impl Iterator<Item = i64>) -> Result<HashSet<i64>> {
        let list: Vec<String> = seqs.map(|s| s.to_string()).collect();
        if list.is_empty() {
            return Ok(HashSet::new());
        }
        let sql = format!(
            "SELECT seq FROM grains WHERE svt IS NULL AND seq IN ({})",
            list.join(",")
        );
        Ok(self
            .db
            .query(&sql, vec![])?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect())
    }

    /// Which of `hashes` have been superseded, and by what.
    ///
    /// Supersession is index-layer state — the blob is immutable and carries no
    /// marker — so a caller holding recalled grains cannot tell a stale version
    /// from the head that replaced it. A recall widened to the whole chain
    /// (`RecallTuning::include_superseded`) needs that distinction to label what
    /// it returns: handing a model an outdated value that *looks* current is a
    /// worse answer than not returning the history at all.
    ///
    /// Indexed point reads on a cached statement, one per candidate. Only the
    /// deliberately-widened path calls this; heads-only recall never pays for it.
    pub fn supersession_map(&mut self, hashes: &[Hash]) -> Result<HashMap<Hash, Hash>> {
        let mut out = HashMap::new();
        if hashes.is_empty() {
            return Ok(out);
        }
        if !self.db.prefers_batched_reads() {
            // In-process: indexed point reads on the cached statement.
            for h in hashes {
                if let Some(sup) = self
                    .db
                    .query_hot(
                        "SELECT superseded_by FROM grains WHERE hash = ?1",
                        vec![pb(h.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| row.blob(0))
                    .and_then(|b| Hash::try_from_bytes(&b).ok())
                {
                    out.insert(*h, sup);
                }
            }
            return Ok(out);
        }
        // Networked backend: the whole candidate set in one round trip.
        let placeholders: Vec<String> = (1..hashes.len() + 1).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT hash, superseded_by FROM grains
              WHERE superseded_by IS NOT NULL AND hash IN ({})",
            placeholders.join(",")
        );
        let params: Vec<Value> = hashes.iter().map(|h| pb(h.as_bytes().to_vec())).collect();
        for row in &self.db.query(&sql, params)? {
            if let (Some(h), Some(sup)) = (
                row.blob(0).and_then(|b| Hash::try_from_bytes(&b).ok()),
                row.blob(1).and_then(|b| Hash::try_from_bytes(&b).ok()),
            ) {
                out.insert(h, sup);
            }
        }
        Ok(out)
    }

    /// Vector leg: cosine top-k over embedded grain text (brute force —
    /// exact search at per-memory scale, per M0 measurements).
    /// Semantic nearest-neighbours to `text` among current grains, optionally
    /// scoped to `(subject, relation)`, returned as `(hash, cosine_similarity)`
    /// most-similar first. This is the **advise** half of a write-time novelty
    /// gate: a reflection harness calls it before writing a distilled lesson
    /// and, if the top similarity clears its own threshold, *supersedes* the
    /// near-duplicate instead of adding a paraphrase — the paraphrase-rot the
    /// exact-value idempotent add (`add_if_novel`) can't catch. It never
    /// mutates: the host stays in control (advise, don't drop).
    ///
    /// Novelty is a vector operation, so this **requires an installed
    /// embedder** and errors loudly without one rather than silently returning
    /// nothing. `text` is embedded as-is and compared against each grain's
    /// stored embedding (subject·relation·object + content); scoping to
    /// `(subject, relation)` keeps the constant prefix out of the way so the
    /// object phrasing dominates the score.
    pub fn nearest_semantic(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        text: &str,
        k: usize,
    ) -> Result<Vec<(Hash, f32)>> {
        let Some(embedder) = &self.embedder else {
            return Err(DejaDbError::Validation(
                "novelty check requires an embedder (e.g. --embed-cmd); none installed".into(),
            ));
        };
        let Some(ns_id) = self.term_lookup(ns) else {
            return Ok(Vec::new());
        };
        let qjson = vec_to_json(&embedder.embed(text)?);
        // A named subject/relation that was never interned can have no
        // neighbours — short-circuit rather than scan.
        let s_id = match subject {
            Some(s) => match self.term_lookup(s) {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r) {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        let base = "SELECT g.hash, vector_distance_cos(e.vec, vector32(?2)) AS dist \
                    FROM embeddings e JOIN grains g ON g.seq = e.seq \
                    WHERE g.ns = ?1 AND g.svt IS NULL";
        let rows = match (s_id, p_id) {
            (Some(s), Some(p)) => self.db.query(
                &format!("{base} AND g.s = ?3 AND g.p = ?4 ORDER BY dist LIMIT ?5"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(p), pi(k as i64)],
            )?,
            (Some(s), None) => self.db.query(
                &format!("{base} AND g.s = ?3 ORDER BY dist LIMIT ?4"),
                vec![pi(ns_id), pt(&qjson), pi(s), pi(k as i64)],
            )?,
            _ => self.db.query(
                &format!("{base} ORDER BY dist LIMIT ?3"),
                vec![pi(ns_id), pt(&qjson), pi(k as i64)],
            )?,
        };
        let mut out = Vec::new();
        for row in rows {
            let h = row.blob(0).and_then(|b| Hash::try_from_bytes(&b).ok());
            // vector_distance_cos is cosine *distance* (1 − similarity).
            let dist = row.f64(1).unwrap_or(1.0);
            if let Some(h) = h {
                out.push((h, (1.0 - dist) as f32));
            }
        }
        Ok(out)
    }

    pub fn search_vector(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_vector_inner(ns, query, k, false)
    }

    /// `search_vector` over the whole chain rather than the live heads —
    /// the vector half of `RecallTuning::include_superseded`.
    pub fn search_vector_all(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        self.search_vector_inner(ns, query, k, true)
    }

    fn search_vector_inner(
        &mut self,
        ns: &str,
        query: &str,
        k: usize,
        include_superseded: bool,
    ) -> Result<Vec<i64>> {
        let (Some(embedder), Some(ns_id)) = (&self.embedder, self.term_lookup(ns)) else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed(query)?;
        let qjson = vec_to_json(&qv);
        Ok(self
            .db
            .query(
                if include_superseded {
                    "SELECT e.seq FROM embeddings e JOIN grains g ON g.seq = e.seq
                     WHERE g.ns = ?1
                     ORDER BY vector_distance_cos(e.vec, vector32(?2)) LIMIT ?3"
                } else {
                    "SELECT e.seq FROM embeddings e JOIN grains g ON g.seq = e.seq
                     WHERE g.ns = ?1 AND g.svt IS NULL
                     ORDER BY vector_distance_cos(e.vec, vector32(?2)) LIMIT ?3"
                },
                vec![pi(ns_id), pt(&qjson), pi(k as i64)],
            )?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect())
    }

    /// Hybrid recall: structural leg + BM25 leg fused
    /// with Reciprocal Rank Fusion; optional deadline makes it fail-open
    /// (returns whatever is gathered when the budget expires). This is the
    /// plain path — see [`recall_hybrid_tuned`](Self::recall_hybrid_tuned) for
    /// the Tier-1/Tier-2 refinements (query expansion, MMR, rerank).
    pub fn recall_hybrid(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        k: usize,
        deadline: Option<std::time::Duration>,
    ) -> Result<Vec<DeserializedGrain>> {
        self.recall_hybrid_tuned(ns, subject, relation, query, k, deadline, RecallTuning::default())
    }

    /// Hybrid recall with post-fusion refinements. Same three
    /// legs and RRF fusion as [`recall_hybrid`](Self::recall_hybrid), plus the
    /// opt-in `tuning` stages:
    ///
    /// - **query expansion** (Tier-1): extra BM25 legs from rule-based query
    ///   variants, RRF-fused — bridges vocabulary gaps with no embedder.
    /// - **rerank** (Tier-2): a cross-encoder re-scores a widened candidate
    ///   pool via the installed [`RerankBackend`]. Takes precedence over MMR.
    /// - **diversity** (Tier-1): MMR reorders the pool to cut near-duplicates,
    ///   using the query embedding + stored candidate vectors.
    ///
    /// Every stage is fail-open: past the deadline, or with its backend/data
    /// absent, it degrades to plain fusion order rather than erroring. All
    /// default off, so this is a strict superset of `recall_hybrid`.
    #[allow(clippy::too_many_arguments)] // tuning knobs are intentionally explicit params
    pub fn recall_hybrid_tuned(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        k: usize,
        deadline: Option<std::time::Duration>,
        tuning: RecallTuning,
    ) -> Result<Vec<DeserializedGrain>> {
        let start = std::time::Instant::now();
        let over = |start: &std::time::Instant| match deadline {
            Some(d) => start.elapsed() >= d,
            None => false,
        };

        // A refinement stage reranks/reorders a candidate pool, so fetch a
        // wider net per leg when one is active.
        let refine = tuning.rerank || tuning.diversity_lambda.is_some();
        let leg_k = if refine {
            k.max(REFINE_POOL)
        } else {
            k.saturating_mul(2)
        };

        // leg 1: structural (the voice hot path — always runs first)
        let structural: Vec<i64> = match subject {
            Some(s) => self.recall_seqs(ns, s, relation, leg_k, tuning.include_superseded)?,
            None => Vec::new(),
        };
        // leg 2: BM25 — plus Tier-1 query-expansion variant legs. Skipped when
        // the deadline is already spent.
        let mut fts_legs: Vec<Vec<i64>> = Vec::new();
        if let Some(q) = query {
            if !over(&start) {
                // Fail-open: a BM25 leg that errors (e.g. the raw user query
                // trips the FTS query-grammar on `:` / quotes / parens — common
                // in DIDs, namespaces, timestamps) degrades to an empty leg,
                // exactly like a deadline-skipped one. recall_hybrid must never
                // error — the structural/vector legs still answer.
                fts_legs.push(
                    self.search_text_inner(ns, q, leg_k, tuning.include_superseded)
                        .unwrap_or_default(),
                );
                if tuning.query_expansion && self.index_text {
                    for variant in self.expand_query(q) {
                        if over(&start) {
                            break;
                        }
                        let hits = self
                            .search_text_inner(ns, &variant, leg_k, tuning.include_superseded)
                            .unwrap_or_default();
                        if !hits.is_empty() {
                            fts_legs.push(hits);
                        }
                    }
                }
            }
        }
        // leg 3: vector (multilingual path — CJK text that whitespace
        // tokenization can't serve rides this leg)
        let vecs: Vec<i64> = match query {
            Some(q) if self.embedder.is_some() && !over(&start) => {
                // Fail-open: e.g. a query embedded at a different dim than the
                // file's stored vectors errors inside vector_distance_cos (the
                // store permits a mismatched embedder with only a warning) —
                // degrade the vector leg rather than failing the whole recall.
                self.search_vector_inner(ns, q, leg_k, tuning.include_superseded)
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        if structural.is_empty() && fts_legs.iter().all(|l| l.is_empty()) && vecs.is_empty() {
            // Record the miss too: an empty-result query is the coverage-gap
            // signal, not a no-op.
            self.record_recall_event(ns, subject, relation, query, &[], start);
            return Ok(Vec::new());
        }

        // RRF fusion (k0 = 60, the standard constant) across every leg.
        let mut scores: HashMap<i64, f64> = HashMap::new();
        for (rank, seq) in structural.iter().enumerate() {
            *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
        }
        for leg in &fts_legs {
            for (rank, seq) in leg.iter().enumerate() {
                *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
            }
        }
        for (rank, seq) in vecs.iter().enumerate() {
            *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
        }
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });

        // Refinement stage: rerank wins over diversity when both are asked for.
        let ordered: Vec<i64> = if let Some(q) =
            query.filter(|_| tuning.rerank && self.reranker.is_some() && !over(&start))
        {
            self.rerank_pool(q, &ranked, k)?
        } else if let (Some(lambda), Some(q)) = (tuning.diversity_lambda, query) {
            if self.embedder.is_some() && !over(&start) {
                self.mmr_pool(q, &ranked, lambda, k)?
            } else {
                ranked.iter().take(k).map(|(s, _)| *s).collect()
            }
        } else {
            ranked.iter().take(k).map(|(s, _)| *s).collect()
        };

        let mut out = Vec::new();
        if self.db.prefers_batched_reads() && !ordered.is_empty() && !over(&start) {
            // Networked backend: one batched blob pull for the whole ranked
            // set; the deadline still bounds per-candidate deserialization,
            // so partial results beat a blown budget exactly as before.
            let blobs = self.blobs_by_seqs(&ordered)?;
            for seq in ordered {
                if over(&start) {
                    break;
                }
                if let Some(b) = blobs.get(&seq) {
                    out.push(deserialize_blob(b)?);
                }
            }
        } else {
            for seq in ordered {
                if over(&start) {
                    break; // fail-open: partial results beat a blown budget
                }
                if let Some(b) = self.blob_by_seq(seq)? {
                    out.push(deserialize_blob(&b)?);
                }
            }
        }

        // Telemetry: capture the recall (buffered, non-blocking). See
        // `record_recall_event` — the only recall-path work is an in-memory
        // push, and `Off` telemetry makes it a single branch.
        self.record_recall_event(ns, subject, relation, query, &out, start);
        Ok(out)
    }

    /// Buffer one recall into the telemetry sidecar. **Non-blocking**: no
    /// SQLite I/O runs here, so this stays inside the recall latency budget;
    /// the buffer drains off-path (writes / close / explicit flush). When the
    /// host left telemetry `Off`, `self.telemetry` is `None` and this is a
    /// single branch that allocates nothing. Called on BOTH recall exit paths
    /// (including the empty-result early return — an empty query is the
    /// coverage-gap signal, so it must be recorded, not dropped).
    #[inline]
    fn record_recall_event(
        &mut self,
        ns: &str,
        subject: Option<&str>,
        relation: Option<&str>,
        query: Option<&str>,
        out: &[DeserializedGrain],
        start: std::time::Instant,
    ) {
        if let Some(tel) = self.telemetry.as_mut() {
            tel.record(RecallEvent {
                ts_ms: now_ms(),
                ns: ns.to_string(),
                subject: subject.map(str::to_string),
                relation: relation.map(str::to_string),
                query: query.map(str::to_string),
                n_results: out.len(),
                latency_us: start.elapsed().as_micros() as i64,
                hashes: out.iter().map(|g| g.hash).collect(),
            });
        }
    }

    /// Query variants for Tier-1 expansion: the installed [`QueryExpander`],
    /// or the built-in [`EnglishExpander`] when none is set.
    fn expand_query(&self, q: &str) -> Vec<String> {
        match &self.expander {
            Some(e) => e.expand(q),
            None => EnglishExpander::default().expand(q),
        }
    }

    /// Text used to rerank a candidate — the same [`projected_text`] shape
    /// the FTS/embed legs index, derived from the grain so it works even
    /// when `index_text` is off.
    fn candidate_text(&mut self, seq: i64) -> Result<String> {
        let Some(b) = self.blob_by_seq(seq)? else {
            return Ok(String::new());
        };
        let g = deserialize_blob(&b)?;
        Ok(projected_text(&g).unwrap_or_default())
    }

    /// Tier-2: cross-encoder rerank a widened candidate pool. Fetches the
    /// top-N fused candidates' text, scores each `(query, doc)` pair via the
    /// installed reranker, and returns the top-`k` seqs by score. Fail-open —
    /// a backend error or a length mismatch falls back to fusion order.
    fn rerank_pool(&mut self, query: &str, ranked: &[(i64, f64)], k: usize) -> Result<Vec<i64>> {
        let pool_n = ranked.len().min(k.max(REFINE_POOL));
        let pool: Vec<i64> = ranked.iter().take(pool_n).map(|(s, _)| *s).collect();
        if pool.is_empty() {
            return Ok(pool);
        }
        let mut docs: Vec<String> = Vec::with_capacity(pool.len());
        for &seq in &pool {
            docs.push(self.candidate_text(seq)?);
        }
        let refs: Vec<&str> = docs.iter().map(|s| s.as_str()).collect();
        let reranker = self.reranker.as_ref().expect("caller checked reranker present");
        match reranker.rerank(query, &refs) {
            Ok(scores) if scores.len() == refs.len() => {
                let mut scored: Vec<(i64, f32)> = pool.iter().copied().zip(scores).collect();
                // stable-ish: higher score first, then lower seq for ties
                scored.sort_by(|a, b| {
                    b.1.partial_cmp(&a.1)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then(a.0.cmp(&b.0))
                });
                Ok(scored.into_iter().take(k).map(|(s, _)| s).collect())
            }
            // Backend failed or returned the wrong shape: keep fusion order.
            _ => Ok(pool.into_iter().take(k).collect()),
        }
    }

    /// Tier-1: MMR diversity reorder. Greedy Maximal Marginal Relevance over
    /// the embedded candidates in the fused pool — `lambda·rel − (1−lambda)·
    /// max_sim_to_selected`, where `rel` is cosine-to-query and `sim` is
    /// candidate-to-candidate cosine (both via `vector_distance_cos`).
    /// Candidates lacking vectors keep fusion order after the MMR set.
    fn mmr_pool(
        &mut self,
        query: &str,
        ranked: &[(i64, f64)],
        lambda: f32,
        k: usize,
    ) -> Result<Vec<i64>> {
        let lambda = lambda.clamp(0.0, 1.0);
        let pool_n = ranked.len().min(k.max(REFINE_POOL));
        let pool: Vec<i64> = ranked.iter().take(pool_n).map(|(s, _)| *s).collect();
        if pool.len() < 2 {
            return Ok(pool);
        }
        let qv = match &self.embedder {
            Some(e) => e.embed(query)?,
            None => return Ok(pool.into_iter().take(k).collect()),
        };
        let qjson = vec_to_json(&qv);
        let rel = self.vec_rel_map(&pool, &qjson)?;
        // MMR is only meaningful with ≥2 embedded candidates.
        let embedded: Vec<i64> = pool.iter().copied().filter(|s| rel.contains_key(s)).collect();
        if embedded.len() < 2 {
            return Ok(pool.into_iter().take(k).collect());
        }
        let sim = self.vec_pairwise_map(&embedded)?;
        let sim_of = |a: i64, b: i64| -> f32 {
            if a == b {
                1.0
            } else {
                let key = if a < b { (a, b) } else { (b, a) };
                *sim.get(&key).unwrap_or(&0.0)
            }
        };

        let target = k.min(embedded.len());
        let mut selected: Vec<i64> = Vec::with_capacity(target);
        let mut remaining: Vec<i64> = embedded.clone();
        while selected.len() < target && !remaining.is_empty() {
            let mut best_idx = 0usize;
            let mut best_score = f32::MIN;
            for (i, &c) in remaining.iter().enumerate() {
                let relevance = *rel.get(&c).unwrap_or(&0.0);
                let max_sim = selected
                    .iter()
                    .map(|&s| sim_of(c, s))
                    .fold(0.0f32, f32::max);
                let mmr = lambda * relevance - (1.0 - lambda) * max_sim;
                if mmr > best_score {
                    best_score = mmr;
                    best_idx = i;
                }
            }
            selected.push(remaining.remove(best_idx));
        }
        // Fill remaining slots with non-embedded candidates in fusion order.
        for s in pool {
            if selected.len() >= k {
                break;
            }
            if !selected.contains(&s) {
                selected.push(s);
            }
        }
        selected.truncate(k);
        Ok(selected)
    }

    /// Cosine relevance (1 − distance) of each embedded candidate to the query
    /// vector. Candidates without stored vectors are simply absent from the map.
    fn vec_rel_map(&mut self, seqs: &[i64], qjson: &str) -> Result<HashMap<i64, f32>> {
        if seqs.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT seq, vector_distance_cos(vec, vector32(?1)) FROM embeddings WHERE seq IN ({})",
            seq_csv(seqs)
        );
        let mut out = HashMap::new();
        for row in self.db.query(&sql, vec![pt(qjson)])? {
            if let (Some(s), Some(d)) = (row.i64(0), row.f64(1)) {
                out.insert(s, 1.0 - d as f32);
            }
        }
        Ok(out)
    }

    /// Pairwise cosine similarity (1 − distance) among embedded candidates,
    /// keyed `(a, b)` with `a < b`. One upper-triangle self-join query.
    fn vec_pairwise_map(&mut self, seqs: &[i64]) -> Result<HashMap<(i64, i64), f32>> {
        if seqs.len() < 2 {
            return Ok(HashMap::new());
        }
        let csv = seq_csv(seqs);
        let sql = format!(
            "SELECT a.seq, b.seq, vector_distance_cos(a.vec, b.vec) \
             FROM embeddings a JOIN embeddings b ON a.seq < b.seq \
             WHERE a.seq IN ({csv}) AND b.seq IN ({csv})"
        );
        let mut out = HashMap::new();
        for row in self.db.query(&sql, vec![])? {
            if let (Some(a), Some(b), Some(d)) = (row.i64(0), row.i64(1), row.f64(2)) {
                out.insert((a, b), 1.0 - d as f32);
            }
        }
        Ok(out)
    }

    /// Structural leg. `all_versions` drops the `cur=1` predicate so the whole
    /// supersession chain comes back instead of the heads — the widened scan
    /// behind `RecallTuning::include_superseded`. It gets its own pair of cached
    /// statements so the heads-only hot path keeps its prepared plans.
    fn recall_seqs(
        &mut self,
        ns: &str,
        subject: &str,
        relation: Option<&str>,
        k: usize,
        all_versions: bool,
    ) -> Result<Vec<i64>> {
        let (ns_id, s_id) = match (self.term_lookup(ns), self.term_lookup(subject)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let p_id = match relation {
            Some(r) => match self.term_lookup(r) {
                Some(x) => Some(x),
                None => return Ok(Vec::new()),
            },
            None => None,
        };
        // Each probe variant is its own SQL literal, so each keeps its own
        // prepared-statement cache entry — the heads-only hot path never loses
        // its plan to the widened scan.
        let rows = match (p_id, all_versions) {
            (Some(p), true) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            (Some(p), false) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 ORDER BY seq DESC LIMIT ?4",
                vec![pi(ns_id), pi(s_id), pi(p), pi(k as i64)],
            )?,
            (None, true) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 ORDER BY seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
            (None, false) => self.db.query_hot(
                "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND cur=1 ORDER BY seq DESC LIMIT ?3",
                vec![pi(ns_id), pi(s_id), pi(k as i64)],
            )?,
        };
        Ok(rows.iter().filter_map(|row| row.i64(0)).collect())
    }

    fn blob_by_seq(&mut self, seq: i64) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_hot("SELECT blob FROM grains WHERE seq = ?1", vec![pi(seq)])?
            .first()
            .and_then(|row| row.blob(0)))
    }

    /// Batched blob fetch for a candidate set — one round trip via an inline
    /// id list (engine-internal seq ids, same rationale as `live_seqs`).
    /// Only for backends that `prefers_batched_reads`: on the embedded engine
    /// a parameterized IN over the PK is a table scan (measured ~8x on the
    /// voice frame path), so the in-process path keeps its point-read loop.
    fn blobs_by_seqs(&mut self, seqs: &[i64]) -> Result<HashMap<i64, Vec<u8>>> {
        let mut out = HashMap::new();
        if seqs.is_empty() {
            return Ok(out);
        }
        let sql = format!("SELECT seq, blob FROM grains WHERE seq IN ({})", seq_csv(seqs));
        for row in &self.db.query(&sql, vec![])? {
            if let (Some(s), Some(b)) = (row.i64(0), row.blob(1)) {
                out.insert(s, b);
            }
        }
        Ok(out)
    }

    /// Distinct subjects holding `relation` in `ns` (POS-index scan).
    /// Backs directory-style listings (memory-tool `view` on a dir).
    pub fn subjects_with_relation(&mut self, ns: &str, relation: &str) -> Result<Vec<String>> {
        let (ns_id, p_id) = match (self.term_lookup(ns), self.term_lookup(relation)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let ids: Vec<i64> = self
            .db
            .query(
                "SELECT DISTINCT s FROM triples WHERE ns=?1 AND p=?2 AND cur=1",
                vec![pi(ns_id), pi(p_id)],
            )?
            .iter()
            .filter_map(|row| row.i64(0))
            .collect();
        let mut subjects: Vec<String> = ids.into_iter().filter_map(|id| self.term_str(id)).collect();
        subjects.sort();
        Ok(subjects)
    }

    /// Store raw content as an Event grain and return its hash. The first half
    /// of `remember()`, split out so a caller whose extraction can *fail* (an
    /// LLM call over the network) lands the raw text first and still has it
    /// after a failed or garbage extraction. Losing the source text to a flaky
    /// model call is the worst failure mode available to a memory engine.
    ///
    /// This is the one place raw remembered text is written, shared by
    /// `deja remember`, both bindings, the MCP `dejadb_remember` tool, and
    /// `capture-stop` — so the same input produces the same grain on every
    /// surface.
    pub fn capture(&mut self, ns: &str, content: &str, meta: &Capture<'_>) -> Result<Hash> {
        use dejadb_core::types::{Event, Role};
        let mut e = Event::new(content);
        e.common.namespace = Some(ns.to_string());
        e.session_id = meta.session_id.map(str::to_string);
        e.role = meta.role.and_then(Role::from_str);
        e.run_id = meta.run_id.filter(|r| !r.is_empty()).map(str::to_string);
        // Event has no observer field (it models a transcript turn, where
        // `role` is the author). Who *captured* the turn is still worth
        // keeping, so it rides in extra_fields, which round-trips through the
        // blob.
        if let Some(observer) = meta.observer.filter(|o| !o.is_empty()) {
            e.common
                .extra_fields
                .insert("observer".to_string(), serde_json::json!(observer));
        }
        self.add(&e)
    }

    /// Store each draft as a Fact carrying `derived_from` provenance back to
    /// `source`, plus whatever `attr` supplies. The second half of
    /// `remember()`; call it after an out-of-band extraction (see
    /// [`DejaDB::capture`]).
    pub fn attach_facts(
        &mut self,
        ns: &str,
        source: &Hash,
        drafts: &[FactDraft],
        attr: &FactAttribution<'_>,
    ) -> Result<Vec<Hash>> {
        let source_hex = source.to_hex();
        let mut facts = Vec::with_capacity(drafts.len());
        for draft in drafts {
            let mut fact = dejadb_core::types::Fact::new(&draft.subject, &draft.relation, &draft.object);
            fact.common.confidence = draft.confidence.clamp(0.0, 1.0);
            fact.common.namespace = Some(ns.to_string());
            fact.common.derived_from = Some(source_hex.clone());
            fact.common.source_type = Some("derived".to_string());
            if let Some(status) = attr.verification_status {
                fact.common.verification_status = Some(status.to_string());
            }
            if let Some(model) = attr.extractor_model {
                fact.common
                    .extra_fields
                    .insert("extractor_model".to_string(), serde_json::json!(model));
            }
            facts.push(self.add(&fact)?);
        }
        Ok(facts)
    }

    /// The `remember()` seam: store raw content as an
    /// Event grain, run the caller-supplied extraction function
    /// (typically an LLM callback — the host owns the model relationship),
    /// and store each returned draft as a Fact with `derived_from`
    /// provenance back to that Event.
    ///
    /// This is the in-process shape, where the extractor cannot fail. A caller
    /// with a *fallible* extractor composes [`DejaDB::capture`] and
    /// [`DejaDB::attach_facts`] instead — what the CLI and the bindings do on
    /// their `--model` / `--llm-cmd` path.
    #[allow(clippy::type_complexity)] // extractor is a plain callback; a type alias would not clarify
    pub fn remember(
        &mut self,
        ns: &str,
        content: &str,
        observer: &str,
        extractor: Option<&dyn Fn(&str) -> Vec<FactDraft>>,
    ) -> Result<RememberResult> {
        let event = self.capture(ns, content, &Capture { observer: Some(observer), ..Default::default() })?;
        let drafts = extractor.map(|f| f(content)).unwrap_or_default();
        let facts = self.attach_facts(ns, &event, &drafts, &FactAttribution::default())?;
        Ok(RememberResult { event, facts })
    }

    /// Total number of grains in the hot store.
    pub fn count(&mut self) -> Result<usize> {
        Ok(self
            .db
            .query("SELECT COUNT(*) FROM grains", vec![])?
            .first()
            .and_then(|row| row.i64(0))
            .unwrap_or(0) as usize)
    }

    /// Open supersession tips for (subject, relation) — normally one; more
    /// than one means a fork (v4 grain-git model). Ordered provisional-first.
    /// Enumerate every open fork in the file — each `(ns, subject, relation)`
    /// whose `heads` table holds more than one live tip. This is the honest
    /// structural conflict signal: a true fork only arises from concurrent
    /// supersession of the same value (typically edits synced from two
    /// writers). Recall never surfaces this to stay off the hot path; operators
    /// call `deja forks` to find and merge them. Not a hot path (scans the
    /// heads table + reverse term lookups).
    pub fn open_forks(&mut self) -> Result<Vec<ForkGroup>> {
        let groups: Vec<(i64, i64, i64)> = self
            .db
            .query(
                "SELECT ns, s, p FROM heads GROUP BY ns, s, p HAVING COUNT(*) > 1",
                vec![],
            )?
            .iter()
            .filter_map(|row| match (row.i64(0), row.i64(1), row.i64(2)) {
                (Some(ns), Some(s), Some(p)) => Some((ns, s, p)),
                _ => None,
            })
            .collect();

        let mut forks = Vec::new();
        for (ns_id, s_id, p_id) in groups {
            let (Some(namespace), Some(subject), Some(relation)) =
                (self.term_str(ns_id), self.term_str(s_id), self.term_str(p_id))
            else {
                continue;
            };
            let heads = self
                .heads(&namespace, &subject, &relation)?
                .into_iter()
                .map(|(h, _)| h)
                .collect();
            forks.push(ForkGroup {
                namespace,
                subject,
                relation,
                heads,
            });
        }
        Ok(forks)
    }

    pub fn heads(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Vec<(Hash, i64)>> {
        let (Some(ns_id), Some(s_id), Some(p_id)) = (
            self.term_lookup(ns),
            self.term_lookup(subject),
            self.term_lookup(relation),
        ) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT hash, created_at FROM heads WHERE ns=?1 AND s=?2 AND p=?3
             ORDER BY created_at DESC, hash DESC",
            vec![pi(ns_id), pi(s_id), pi(p_id)],
        )? {
            let h = row.blob(0).unwrap_or_default();
            let c = row.i64(1).unwrap_or(0);
            if let Ok(h) = Hash::try_from_bytes(&h) {
                out.push((h, c));
            }
        }
        Ok(out)
    }

    /// Close a fork: write `merged` superseding EVERY open tip, with all
    /// parents recorded in the provenance chain (git merge commit).
    pub fn merge_heads<G: Grain + 'static>(
        &mut self,
        ns: &str,
        subject: &str,
        relation: &str,
        merged: &mut G,
    ) -> Result<Hash> {
        let tips = self.heads(ns, subject, relation)?;
        if tips.len() < 2 {
            return Err(DejaDbError::Validation(format!(
                "merge_heads needs an open fork; {} head(s) present",
                tips.len()
            )));
        }
        // parents: provisional head as derived_from; ALL tips recorded in
        // context.merge_parents (context is serialized into the .mg blob;
        // provenance_chain is index-layer in this port)
        merged.common_mut().derived_from = Some(tips[0].0.to_hex());
        let parents: Vec<String> = tips.iter().map(|(h, _)| h.to_hex()).collect();
        let mut ctx = match merged.common().context.clone() {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        ctx.insert("merge_parents".into(), serde_json::json!(parents));
        merged.common_mut().context = Some(serde_json::Value::Object(ctx));
        // Insert the merge grain AND close every tip in ONE transaction. A merge
        // that committed the add first and then closed tips in separate
        // (autocommit) statements could crash mid-loop, leaving heads={merge}
        // while some parent tips stayed cur=1 (heads/recall disagreement).
        let merged_dyn: &dyn AddableDyn = &*merged;
        let (preps, first_seq, first_op, hlc0) = self.prep_and_reserve(&[merged_dyn])?;
        let merge_hash = preps[0].hash;
        let now = now_ms();
        // Reserve the fork-closure OP_SUPERSEDE slot (written inside the txn so it
        // replicates; the merge grain's context.merge_parents lets import close
        // all tips — see superseded_parents + the import OP_SUPERSEDE branch).
        let op_seq = self.next_op;
        self.next_op += 1;
        let hlc = self.next_hlc();
        let (d_docs, d_len) = fts_delta(&preps);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            insert_prepped(dbr, &preps, first_seq, first_op, hlc0)?; // OP_ADD; collapses heads to {merge}
            for (tip, _) in &tips {
                let seq = dbr
                    .query(
                        "SELECT seq, svt FROM grains WHERE hash=?1",
                        vec![pb(tip.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| {
                        let seq = row.i64(0).unwrap_or(0);
                        row.i64(1).is_none().then_some(seq)
                    });
                if let Some(seq) = seq {
                    dbr.execute(
                        "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                        vec![pb(merge_hash.as_bytes().to_vec()), pi(now), pi(seq)],
                    )?;
                    dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(seq)])?;
                    dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(seq)])?;
                }
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc), pi(OP_SUPERSEDE), pb(merge_hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })?;
        self.fts_docs += d_docs;
        self.fts_total_len += d_len;
        Ok(merge_hash)
    }

    /// Supersession-chain history for (namespace, subject, relation),
    /// newest first — the HISTORY statement's backing read (§5.13).
    pub fn history(&mut self, ns: &str, subject: &str, relation: &str) -> Result<Vec<HistoryEntry>> {
        let head = match self.latest(ns, subject, relation)? {
            Some(g) => g.hash,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        let mut cur = Some(head);
        while let Some(h) = cur {
            let rows = self.db.query(
                "SELECT blob, superseded_by, supersedes FROM grains WHERE hash = ?1",
                vec![pb(h.as_bytes().to_vec())],
            )?;
            let (blob, sup_by, supersedes) = match rows.first() {
                Some(row) => (row.blob(0), row.blob(1), row.blob(2)),
                None => break,
            };
            if let Some(b) = blob {
                let g = deserialize_blob(&b)?;
                out.push(HistoryEntry {
                    hash: h,
                    object: g.get_str("object").unwrap_or_default().to_string(),
                    created_at: g.get_i64("created_at").unwrap_or(0),
                    confidence: g.get_f64("confidence").unwrap_or(0.0),
                    superseded_by: sup_by.and_then(|b| Hash::try_from_bytes(&b).ok()),
                });
            }
            cur = supersedes.and_then(|b| Hash::try_from_bytes(&b).ok());
            if out.len() > 512 {
                break; // chain-length safety cap
            }
        }
        Ok(out)
    }

    /// Verify store integrity: Turso's own integrity check plus a full
    /// content-address re-verification (every blob re-hashed and compared
    /// to its stored hash — the tamper-evidence read).
    ///
    /// Scope: this detects *modification* of readable grains, not *removal*.
    /// WAL corruption makes the engine roll back to the last consistent
    /// state, and a truncated-but-consistent store verifies `ok`; detecting
    /// that requires an external anchor (stream segments, bundles, a
    /// replica). See docs/security-model.md "Known limitations".
    pub fn verify(&mut self) -> Result<VerifyReport> {
        // Collect every integrity line; Turso's experimental FTS keeps
        // internal dir indexes that integrity_check miscounts — classify
        // those as benign notes (candidate upstream report), never as
        // corruption. Content-address verification below is the real
        // tamper-evidence check and is unaffected.
        let mut real: Vec<String> = Vec::new();
        let mut fts_notes: Vec<String> = Vec::new();
        for row in self.db.query("PRAGMA integrity_check", vec![])? {
            if let Some(s) = row.text(0) {
                if s == "ok" {
                    continue;
                } else if s.contains("__turso_internal_fts") {
                    fts_notes.push(s.to_string());
                } else {
                    real.push(s.to_string());
                }
            }
        }
        let integrity = if real.is_empty() { "ok".to_string() } else { real.join("; ") };
        let rows: Vec<(Vec<u8>, Vec<u8>)> = self
            .db
            .query("SELECT hash, blob FROM grains", vec![])?
            .iter()
            .map(|row| (row.blob(0).unwrap_or_default(), row.blob(1).unwrap_or_default()))
            .collect();
        let mut report = VerifyReport {
            integrity,
            fts_notes,
            grains: rows.len(),
            hash_mismatches: 0,
            undecodable: 0,
        };
        for (stored, blob) in rows {
            match deserialize_blob(&blob) {
                Ok(g) => {
                    if g.hash.as_bytes().as_slice() != stored.as_slice() {
                        report.hash_mismatches += 1;
                    }
                }
                Err(_) => report.undecodable += 1,
            }
        }
        Ok(report)
    }

    /// Store statistics (CLI `stats`).
    pub fn stats(&mut self) -> Result<StoreStats> {
        let one = |sql: &'static str| -> Result<i64> {
            Ok(self
                .db
                .query(sql, vec![])?
                .first()
                .and_then(|row| row.i64(0))
                .unwrap_or(0))
        };
        Ok(StoreStats {
            grains: one("SELECT COUNT(*) FROM grains")? as usize,
            current: one("SELECT COUNT(*) FROM grains WHERE svt IS NULL")? as usize,
            triples: one("SELECT COUNT(*) FROM triples")? as usize,
            terms: one("SELECT COUNT(*) FROM terms")? as usize,
            ops: one("SELECT COUNT(*) FROM oplog")? as usize,
            events_indexed: one("SELECT COUNT(*) FROM thread_idx")? as usize,
        })
    }

    /// Op-log cursor read — the change feed (backs sync + UIs).
    pub fn changes_since(&mut self, after_op_seq: i64, limit: usize) -> Result<Vec<OpRecord>> {
        let mut out = Vec::new();
        for row in self.db.query(
            "SELECT op_seq, hlc, op, hash FROM oplog WHERE op_seq > ?1 ORDER BY op_seq LIMIT ?2",
            vec![pi(after_op_seq), pi(limit as i64)],
        )? {
            out.push(OpRecord {
                op_seq: row.i64(0).unwrap_or(0),
                hlc: row.i64(1).unwrap_or(0),
                op: row.i64(2).unwrap_or(0),
                hash: Hash::try_from_bytes(&row.blob(3).unwrap_or_default())?,
            });
        }
        Ok(out)
    }
}

impl DejaDB {
    // ----- CAS blob store (`.blobs` fan-out dir / in-schema table) -----

    /// Store bytes in the per-memory CAS; returns the `cas://sha256:` URI.
    /// Idempotent — content addressing dedupes by construction.
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<String> {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let hex = hex::encode(digest);
        match &self.blob_store {
            BlobStore::Fs(dir) => {
                let path = fs_blob_path(dir, &hex);
                if !path.exists() {
                    std::fs::create_dir_all(path.parent().unwrap()).map_err(db_err)?;
                    let tmp = path.with_extension("tmp");
                    std::fs::write(&tmp, bytes).map_err(db_err)?;
                    std::fs::rename(&tmp, &path).map_err(db_err)?;
                }
            }
            BlobStore::Table => {
                self.db.execute(
                    "INSERT OR IGNORE INTO blobs(hash, body) VALUES (?1, ?2)",
                    vec![pb(digest.to_vec()), pb(bytes.to_vec())],
                )?;
            }
        }
        Ok(format!("cas://sha256:{hex}"))
    }

    /// Fetch bytes by `cas://sha256:` URI, verifying the hash on read.
    pub fn get_blob(&mut self, uri: &str) -> Result<Vec<u8>> {
        use sha2::{Digest, Sha256};
        let hex = uri
            .strip_prefix("cas://sha256:")
            .ok_or_else(|| DejaDbError::Validation(format!("not a cas uri: {uri}")))?;
        // Validate before anything byte-slices `hex[..2]` — an untrusted or
        // truncated URI (e.g. from an imported content_ref) must return Err,
        // never panic on a short or non-char-boundary slice.
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DejaDbError::Validation(format!("malformed cas uri: {uri}")));
        }
        let bytes = match &self.blob_store {
            BlobStore::Fs(dir) => std::fs::read(fs_blob_path(dir, hex))
                .map_err(|_| DejaDbError::Storage(format!("blob missing: {uri}")))?,
            BlobStore::Table => {
                let raw = hex::decode(hex).map_err(db_err)?;
                self.db
                    .query("SELECT body FROM blobs WHERE hash = ?1", vec![pb(raw)])?
                    .first()
                    .and_then(|row| row.blob(0))
                    .ok_or_else(|| DejaDbError::Storage(format!("blob missing: {uri}")))?
            }
        };
        if hex::encode(Sha256::digest(&bytes)) != hex {
            return Err(DejaDbError::Storage(format!("blob corrupt: {uri}")));
        }
        Ok(bytes)
    }

    /// Remove CAS blobs not referenced by any live grain's `content_refs`.
    /// Returns the number of blobs removed.
    pub fn gc_blobs(&mut self) -> Result<usize> {
        // Collect referenced hashes from live grains.
        let blobs: Vec<Vec<u8>> = self
            .db
            .query("SELECT blob FROM grains", vec![])?
            .iter()
            .filter_map(|row| row.blob(0))
            .collect();
        let mut referenced: HashSet<String> = HashSet::new();
        for b in &blobs {
            if let Ok(view) = deserialize_blob(b) {
                if let Some(refs) = view.fields.get("content_refs").and_then(|v| v.as_array()) {
                    for r in refs {
                        // inner keys may be compact ("u") or expanded ("uri")
                        let uri = r
                            .get("uri")
                            .and_then(|u| u.as_str())
                            .or_else(|| r.get("u").and_then(|u| u.as_str()));
                        if let Some(hex) = uri.and_then(|u| u.strip_prefix("cas://sha256:")) {
                            referenced.insert(hex.to_string());
                        }
                    }
                }
            }
        }
        let mut removed = 0usize;
        match &self.blob_store {
            BlobStore::Fs(dir) => {
                if let Ok(shards) = std::fs::read_dir(dir) {
                    for shard in shards.flatten() {
                        let prefix = shard.file_name().to_string_lossy().to_string();
                        if let Ok(files) = std::fs::read_dir(shard.path()) {
                            for f in files.flatten() {
                                let rest = f.file_name().to_string_lossy().to_string();
                                let hex = format!("{prefix}{rest}");
                                if !referenced.contains(&hex)
                                    && std::fs::remove_file(f.path()).is_ok()
                                {
                                    removed += 1;
                                }
                            }
                        }
                    }
                }
            }
            BlobStore::Table => {
                let stored: Vec<Vec<u8>> = self
                    .db
                    .query("SELECT hash FROM blobs", vec![])?
                    .iter()
                    .filter_map(|row| row.blob(0))
                    .collect();
                for raw in stored {
                    if !referenced.contains(&hex::encode(&raw)) {
                        self.db.execute("DELETE FROM blobs WHERE hash = ?1", vec![pb(raw)])?;
                        removed += 1;
                    }
                }
            }
        }
        Ok(removed)
    }

    // ----- bundle: git-shaped incremental backup / fast-forward sync (§5.10) -----

    /// Export all ops after `after_op_seq` to a bundle file.
    /// Record: op(u8) · hlc(i64 LE) · hash(32) · blob_len(u32 LE) · blob.
    /// Blobs of later-forgotten grains export as len 0 — the importer
    /// relies on the subsequent tombstone for net-equivalence.
    pub fn bundle_since(&mut self, after_op_seq: i64, path: &str) -> Result<BundleStats> {
        let ops = self.changes_since(after_op_seq, usize::MAX / 2)?;
        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(BUNDLE_MAGIC);
        let mut last = after_op_seq;
        for rec in &ops {
            let blob: Option<Vec<u8>> = if rec.op == OP_FORGET {
                None
            } else {
                self.db
                    .query_hot(
                        "SELECT blob FROM grains WHERE hash = ?1",
                        vec![pb(rec.hash.as_bytes().to_vec())],
                    )?
                    .first()
                    .and_then(|row| row.blob(0))
            };
            out.push(rec.op as u8);
            out.extend_from_slice(&rec.hlc.to_le_bytes());
            out.extend_from_slice(rec.hash.as_bytes());
            let b = blob.unwrap_or_default();
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(&b);
            last = rec.op_seq;
        }
        std::fs::write(path, &out).map_err(db_err)?;
        Ok(BundleStats {
            ops: ops.len(),
            bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            last_op_seq: last,
        })
    }

    fn blob_by_hash(&mut self, hash: &Hash) -> Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_hot(
                "SELECT blob FROM grains WHERE hash = ?1",
                vec![pb(hash.as_bytes().to_vec())],
            )?
            .first()
            .and_then(|row| row.blob(0)))
    }

    fn has_grain(&mut self, hash: &Hash) -> Result<bool> {
        Ok(!self
            .db
            .query_hot(
                "SELECT 1 FROM grains WHERE hash = ?1",
                vec![pb(hash.as_bytes().to_vec())],
            )?
            .is_empty())
    }

    /// Insert one already-serialized grain (bundle import path).
    fn insert_blob(&mut self, blob: Vec<u8>, hash: Hash, op: i64, hlc_in: i64) -> Result<()> {
        let pr = self.prep_from_blob(blob, hash)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let op_seq = self.next_op;
        self.next_op += 1;
        self.hlc_last = self.hlc_last.max(hlc_in);
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            dbr.execute(
                "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
                vec![
                    pi(seq),
                    pb(pr.hash.as_bytes().to_vec()),
                    pi(pr.ns_id),
                    pi(pr.gtype),
                    pi(pr.created),
                    opt_i(pr.s),
                    opt_i(pr.p),
                    opt_i(pr.o),
                    opt_i(pr.vf),
                    opt_i(pr.vt),
                    pi(pr.created),
                    match &pr.text { Some(t) => pt(t), None => Value::Null },
                    pb(pr.blob.clone()),
                ],
            )?;
            if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
                dbr.execute(
                    "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)],
                )?;
                if pr.osp {
                    dbr.execute(
                        "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        vec![pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)],
                    )?;
                }
                // import path: UNION into heads (never collapse other
                // tips — that's the local single-writer semantic only)
                dbr.execute(
                    "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    vec![pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)],
                )?;
                // provisional election for entity_latest: replace only if
                // (created_at, hash) beats the current head — deterministic
                // on every node, no coordination.
                let cur = dbr
                    .query(
                        "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                         ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                         WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                        vec![pi(pr.ns_id), pi(s), pi(p)],
                    )?
                    .first()
                    .map(|row| (row.i64(0).unwrap_or(0), row.blob(1).unwrap_or_default()));
                let wins = match &cur {
                    Some((c, h)) => (pr.created, pr.hash.as_bytes().as_slice()) > (*c, h.as_slice()),
                    None => true,
                };
                if wins {
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq), pb(pr.hash.as_bytes().to_vec())],
                    )?;
                }
            }
            // Cross-grain `related_to` links — same treatment as the local
            // write path: triples + osp for retrieval, never heads or
            // entity_latest (OMS §15.3).
            for (ls, lp, lo) in &pr.links {
                dbr.execute(
                    "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(*ls), pi(*lp), pi(*lo), pi(seq)],
                )?;
                dbr.execute(
                    "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(*lo), pi(*ls), pi(*lp), pi(seq)],
                )?;
            }
            if let Some(sess) = pr.session {
                dbr.execute(
                    "INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pi(sess), pi(seq)],
                )?;
            }
            if let Some(run) = pr.run {
                dbr.execute(
                    "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pi(run), pi(seq)],
                )?;
            }
            if let Some(ref parent) = pr.parent {
                dbr.execute(
                    "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                    vec![pi(pr.ns_id), pb(parent.clone()), pi(seq)],
                )?;
            }
            if let Some(ref emb) = pr.embedding {
                dbr.execute(
                    "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                    vec![pi(seq), pt(&vec_to_json(emb))],
                )?;
            }
            dbr.execute(
                "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                vec![pi(op_seq), pi(hlc_in), pi(op), pb(pr.hash.as_bytes().to_vec())],
            )?;
            Ok(())
        })
    }

    /// Apply the index-layer supersession flip old → new (import path).
    /// Returns whether anything changed (false = idempotent no-op).
    /// One transaction: a crash mid-flip must not leave the fork model
    /// half-registered (this ran statement-by-statement in autocommit before
    /// the Db seam).
    fn apply_supersede_flip(&mut self, old: &Hash, new_hash: &Hash) -> Result<bool> {
        let dbr = self.db.as_ref();
        with_txn(dbr, || {
            let rows = dbr.query(
                "SELECT seq, svt, ns, s, p FROM grains WHERE hash = ?1",
                vec![pb(old.as_bytes().to_vec())],
            )?;
            let (old_seq, old_svt, old_ns, old_s, old_p) = match rows.first() {
                Some(row) => (
                    row.i64(0).unwrap_or(0),
                    row.i64(1),
                    row.i64(2).unwrap_or(0),
                    row.i64(3),
                    row.i64(4),
                ),
                None => return Ok(false), // partial history — fast-forward tolerates
            };
            if old_svt.is_some() {
                // v4 grain-git: old head already superseded. Same superseder →
                // idempotent replay. Different superseder → a FORK: both tips
                // stay alive as heads; entity_latest gets the provisional head
                // (created_at, then hash — deterministic on every node).
                let existing = dbr
                    .query(
                        "SELECT superseded_by FROM grains WHERE seq=?1",
                        vec![pi(old_seq)],
                    )?
                    .first()
                    .and_then(|row| row.blob(0));
                if existing.as_deref() == Some(new_hash.as_bytes().as_slice()) {
                    return Ok(false); // same supersede — idempotent
                }
                // incoming tip row
                let inc = dbr
                    .query(
                        "SELECT seq, ns, s, p, o, created_at FROM grains WHERE hash=?1",
                        vec![pb(new_hash.as_bytes().to_vec())],
                    )?
                    .first()
                    .map(|row| {
                        (
                            row.i64(0).unwrap_or(0),
                            row.i64(1).unwrap_or(0),
                            row.i64(2).unwrap_or(0),
                            row.i64(3).unwrap_or(0),
                            row.i64(4).unwrap_or(0),
                            row.i64(5).unwrap_or(0),
                        )
                    });
                let Some((inc_seq, ns, s, p, o, inc_created)) = inc else { return Ok(false) };
                dbr.execute(
                    "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    vec![pi(ns), pi(s), pi(p), pi(inc_seq), pb(new_hash.as_bytes().to_vec()), pi(inc_created)],
                )?;
                // provisional election vs current entity_latest head
                let cur = dbr
                    .query(
                        "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                         ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                         WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                        vec![pi(ns), pi(s), pi(p)],
                    )?
                    .first()
                    .map(|row| (row.i64(0).unwrap_or(0), row.blob(1).unwrap_or_default()));
                let incoming_wins = match &cur {
                    Some((c_created, c_hash)) => {
                        (inc_created, new_hash.as_bytes().as_slice()) > (*c_created, c_hash.as_slice())
                    }
                    None => true,
                };
                if incoming_wins {
                    dbr.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        vec![pi(ns), pi(s), pi(p), pi(o), pi(inc_seq), pb(new_hash.as_bytes().to_vec())],
                    )?;
                }
                return Ok(true); // fork registered
            }
            let now = now_ms();
            dbr.execute(
                "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                vec![pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)],
            )?;
            dbr.execute(
                "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                vec![pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())],
            )?;
            dbr.execute("UPDATE triples SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            dbr.execute("UPDATE osp SET cur=0 WHERE seq=?1", vec![pi(old_seq)])?;
            if let (Some(s), Some(p)) = (old_s, old_p) {
                dbr.execute(
                    "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                    vec![pi(old_ns), pi(s), pi(p), pi(old_seq)],
                )?;
            }
            Ok(true)
        })
    }

    /// Import a bundle (idempotent; fast-forward replay in op order).
    pub fn import_bundle(&mut self, path: &str) -> Result<ImportStats> {
        self.import_bundle_until(path, None)
    }

    /// Import, applying only ops with `hlc <= max_hlc` when set — the
    /// point-in-time restore primitive (§5.10b): replay history to T.
    pub fn import_bundle_until(&mut self, path: &str, max_hlc: Option<i64>) -> Result<ImportStats> {
        let data = std::fs::read(path).map_err(db_err)?;
        if data.len() < 4 || &data[..4] != BUNDLE_MAGIC {
            return Err(DejaDbError::Format("not a MGB1 bundle".into()));
        }
        let mut stats = ImportStats::default();
        let mut i = 4usize;
        while i < data.len() {
            if i + 1 + 8 + 32 + 4 > data.len() {
                return Err(DejaDbError::Format("truncated bundle record".into()));
            }
            let op = data[i] as i64;
            i += 1;
            let hlc = i64::from_le_bytes(data[i..i + 8].try_into().unwrap());
            i += 8;
            let hash = Hash::try_from_bytes(&data[i..i + 32])?;
            i += 32;
            let len = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            i += 4;
            if i.checked_add(len).is_none_or(|end| end > data.len()) {
                return Err(DejaDbError::Format("truncated bundle blob".into()));
            }
            let blob = data[i..i + len].to_vec();
            i += len;

            if let Some(t) = max_hlc {
                if hlc > t {
                    stats.skipped += 1;
                    continue; // beyond the requested point in time
                }
            }
            match op {
                OP_ADD => {
                    if self.has_grain(&hash)? || blob.is_empty() {
                        // exists already, or pruned (forgotten later at source)
                        stats.skipped += 1;
                        continue;
                    }
                    self.insert_blob(blob, hash, op, hlc)?;
                    stats.applied += 1;
                }
                OP_SUPERSEDE => {
                    // supersede() double-logs (OP_ADD for the new grain, then
                    // OP_SUPERSEDE); the grain may thus already exist here —
                    // the flip must still be applied idempotently.
                    let exists = self.has_grain(&hash)?;
                    let bytes: Option<Vec<u8>> = if !blob.is_empty() {
                        Some(blob)
                    } else if exists {
                        self.blob_by_hash(&hash)?
                    } else {
                        None
                    };
                    match bytes {
                        None => stats.skipped += 1,
                        Some(bb) => {
                            let mut changed = false;
                            let inserted = !exists;
                            if !exists {
                                // insert_blob logs its own OP_SUPERSEDE row.
                                self.insert_blob(bb.clone(), hash, op, hlc)?;
                                changed = true;
                            }
                            if let Ok(view) = deserialize_blob(&bb) {
                                // Close EVERY tip this grain supersedes: the
                                // linear derived_from parent and, for a merge
                                // commit, all merge_parents — else the other
                                // parents stay open and the replica forks.
                                for old in superseded_parents(&view) {
                                    changed |= self.apply_supersede_flip(&old, &hash)?;
                                }
                            }
                            // If the grain already existed (its OP_ADD twin was
                            // imported first) insert_blob didn't run, so the flip
                            // is missing from our op-log — record it here so a
                            // re-export (the B->C hop) carries the supersession
                            // instead of shipping two bare adds that fork.
                            // Idempotent replays return changed=false above.
                            if changed && !inserted {
                                self.log_op(OP_SUPERSEDE, &hash, hlc)?;
                            }
                            if changed {
                                stats.applied += 1;
                            } else {
                                stats.skipped += 1;
                            }
                        }
                    }
                }
                OP_FORGET => match self.forget(&hash) {
                    Ok(()) => stats.applied += 1,
                    Err(DejaDbError::NotFound(_)) => stats.skipped += 1,
                    Err(e) => return Err(e),
                },
                _ => return Err(DejaDbError::Format(format!("unknown bundle op {op}"))),
            }
        }
        Ok(stats)
    }
}

impl Drop for DejaDB {
    fn drop(&mut self) {
        // Persist any buffered recall telemetry on close. Best-effort: the
        // sidecar is disposable and rebuildable, so a failed final flush costs
        // evidence detail, never state — never let it surface from a destructor.
        let _ = self.telemetry_flush();
    }
}

pub mod asyncdb;
pub mod memory_tool;
pub mod migrate;
pub mod telemetry;

pub use asyncdb::AsyncDejaDB;
pub use telemetry::{AccessStat, BudgetStat, QueryStat, RecallEvent, Telemetry, TelemetryMode};

/// The insert body of an add — the grain row, triples/osp, entity_latest, head
/// collapse+insert, thread index, embedding, and the OP_ADD op-log row — WITHOUT
/// `BEGIN`/`COMMIT`. Runs inside a transaction the caller already opened, so
/// `supersede`/`merge_heads` can perform the grain insert AND the index flip in
/// ONE atomic transaction (previously they committed the add, then flipped in a
/// separate txn — a crash or error between the two left a torn state: the new
/// grain durable and current while the old grain stayed un-superseded). The
/// caller reserves `first_seq`/`first_op`/`hlc0` via [`DejaDB::prep_and_reserve`]
/// and owns the surrounding transaction.
/// `(documents, total token length)` a batch adds to the BM25 collection
/// statistics. Applied by the caller after its transaction commits, so a
/// rolled-back insert cannot leave the in-memory counters ahead of the file.
fn fts_delta(preps: &[GrainPrep]) -> (i64, i64) {
    let docs = preps.iter().filter(|p| !p.tokens.is_empty()).count() as i64;
    (docs, preps.iter().map(|p| p.doc_len).sum())
}

fn insert_prepped(
    db: &dyn Db,
    preps: &[GrainPrep],
    first_seq: i64,
    first_op: i64,
    hlc0: i64,
) -> Result<()> {
    // Every statement here is a fixed literal on the write hot path — the
    // backend's `_hot` cache makes each a prepare-once (this used to re-prepare
    // eight statements per call).
    for (i, pr) in preps.iter().enumerate() {
        let seq = first_seq + i as i64;
        db.execute_hot(
            "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
            vec![
                pi(seq),
                pb(pr.hash.as_bytes().to_vec()),
                pi(pr.ns_id),
                pi(pr.gtype),
                pi(pr.created),
                opt_i(pr.s),
                opt_i(pr.p),
                opt_i(pr.o),
                opt_i(pr.vf),
                opt_i(pr.vt),
                pi(pr.created),
                match &pr.text { Some(t) => pt(t), None => Value::Null },
                pb(pr.blob.clone()),
            ],
        )?;
        // BM25 postings: one row per distinct token. Cost is proportional to
        // the grain's own length, not to how much is already stored.
        if !pr.tokens.is_empty() {
            for (term, tf) in &pr.tokens {
                db.execute_hot(
                    "INSERT INTO fts_post(term,seq,ns,tf) VALUES (?1,?2,?3,?4)",
                    vec![pi(*term), pi(seq), pi(pr.ns_id), pi(*tf)],
                )?;
            }
            db.execute_hot(
                "INSERT OR REPLACE INTO fts_doc(seq,len) VALUES (?1,?2)",
                vec![pi(seq), pi(pr.doc_len)],
            )?;
        }
        if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
            db.execute_hot(
                "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)],
            )?;
            if pr.osp {
                db.execute_hot(
                    "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                    vec![pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)],
                )?;
            }
            db.execute_hot(
                "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq), pb(pr.hash.as_bytes().to_vec())],
            )?;
            db.execute_hot(
                "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3",
                vec![pi(pr.ns_id), pi(s), pi(p)],
            )?;
            db.execute_hot(
                "INSERT INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                vec![pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)],
            )?;
        }
        // Cross-grain `related_to` links (OMS §6.1), e.g. the §8.4 execution
        // record `mg:step_action:<node>` pointing a Tool grain at the Workflow
        // it executed a step of. Indexed into triples + osp so both directions
        // are traversable — but NOT into heads/entity_latest: §15.3 is
        // normative that such a link must not alter the target's supersession
        // state, and heads is exactly that state. osp is unconditional here
        // because a link's object is always a grain hash, i.e. always an
        // entity, regardless of the file's `entity_relations` declaration.
        for (ls, lp, lo) in &pr.links {
            db.execute_hot(
                "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(*ls), pi(*lp), pi(*lo), pi(seq)],
            )?;
            db.execute_hot(
                "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                vec![pi(pr.ns_id), pi(*lo), pi(*ls), pi(*lp), pi(seq)],
            )?;
        }
        if let Some(sess) = pr.session {
            db.execute_hot(
                "INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pi(sess), pi(seq)],
            )?;
        }
        // Run correlation + reverse provenance. Both are plain index rows: they
        // record where a grain came from and which run recorded it, and neither
        // participates in supersession.
        if let Some(run) = pr.run {
            db.execute_hot(
                "INSERT INTO run_idx(ns,run,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pi(run), pi(seq)],
            )?;
        }
        if let Some(ref parent) = pr.parent {
            db.execute_hot(
                "INSERT INTO prov_idx(ns,parent,seq) VALUES (?1,?2,?3)",
                vec![pi(pr.ns_id), pb(parent.clone()), pi(seq)],
            )?;
        }
        if let Some(ref emb) = pr.embedding {
            db.execute_hot(
                "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                vec![pi(seq), pt(&vec_to_json(emb))],
            )?;
        }
        db.execute_hot(
            "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
            vec![pi(first_op + i as i64), pi(hlc0 + i as i64), pi(OP_ADD), pb(pr.hash.as_bytes().to_vec())],
        )?;
    }
    Ok(())
}

/// Every parent hash a grain supersedes: the linear `derived_from`, plus any
/// `context.merge_parents` recorded by a merge commit. Used by bundle import to
/// close all tips a replicated supersession/merge closes at the source.
fn superseded_parents(view: &DeserializedGrain) -> Vec<Hash> {
    let mut out = Vec::new();
    if let Some(df) = view.get_str("derived_from") {
        if let Ok(h) = Hash::from_hex(df) {
            out.push(h);
        }
    }
    if let Some(arr) = view
        .fields
        .get("context")
        .and_then(|c| c.get("merge_parents"))
        .and_then(|v| v.as_array())
    {
        for p in arr {
            if let Some(h) = p.as_str().and_then(|s| Hash::from_hex(s).ok()) {
                if !out.contains(&h) {
                    out.push(h);
                }
            }
        }
    }
    out
}

/// Object-safe serialization adapter so `add_batch` can take mixed grain types.
pub trait AddableDyn {
    fn serialize_dyn(&self) -> Result<(Vec<u8>, Hash)>;
}

impl<G: Grain + 'static> AddableDyn for G {
    fn serialize_dyn(&self) -> Result<(Vec<u8>, Hash)> {
        serialize_grain(self)
    }
}

#[cfg(test)]
mod tests {
    //! Inline unit tests for the store's pure/internal helpers. These sit
    //! below the black-box integration suite in `tests/` and exercise the
    //! bits that never surface through the public API: dictionary interning,
    //! the HLC counter, RRF fusion math, the Value bridge, and the crypto KDF
    //! helpers. A `tests` child module can reach the crate root's private
    //! items (fns, methods, struct fields), so we test them directly.
    use super::*;
    use tempfile::TempDir;

    // ---- CAL host metadata (meta_scan / meta_put / meta_delete) ---------

    #[test]
    fn meta_rows_round_trip_and_scan_by_prefix() {
        let dir = TempDir::new().unwrap();
        let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

        m.meta_put("qry:brief", "{\"body\":\"a\"}").unwrap();
        m.meta_put("qry:wide", "{\"body\":\"b\"}").unwrap();
        m.meta_put("tpl:card", "{\"source\":\"c\"}").unwrap();

        let mut queries = m.meta_scan("qry:").unwrap();
        queries.sort();
        assert_eq!(
            queries,
            vec![
                ("brief".to_string(), "{\"body\":\"a\"}".to_string()),
                ("wide".to_string(), "{\"body\":\"b\"}".to_string()),
            ],
            "scan returns this prefix's rows with the prefix stripped"
        );
        assert_eq!(m.meta_scan("tpl:").unwrap().len(), 1);

        // Upsert, not insert: a second put replaces.
        m.meta_put("qry:brief", "{\"body\":\"z\"}").unwrap();
        assert_eq!(m.meta_scan("qry:").unwrap().len(), 2);

        m.meta_delete("qry:brief").unwrap();
        assert_eq!(m.meta_scan("qry:").unwrap().len(), 1);
        // A missing key is not an error — drop is idempotent.
        m.meta_delete("qry:brief").unwrap();
    }

    /// `%` and `_` are LIKE wildcards. An unescaped prefix would quietly match
    /// rows it does not own — `a_b:` matching `axb:` — and hand a caller
    /// another namespace's metadata.
    #[test]
    fn meta_scan_treats_like_wildcards_as_literals() {
        let dir = TempDir::new().unwrap();
        let m = DejaDB::open(dir.path().join("m.db").to_str().unwrap()).unwrap();

        m.meta_put("a_b:mine", "1").unwrap();
        m.meta_put("axb:theirs", "2").unwrap();
        m.meta_put("pre%fix:mine", "3").unwrap();
        m.meta_put("preXfix:theirs", "4").unwrap();

        assert_eq!(
            m.meta_scan("a_b:").unwrap(),
            vec![("mine".to_string(), "1".to_string())],
            "`_` must not match an arbitrary character"
        );
        assert_eq!(
            m.meta_scan("pre%fix:").unwrap(),
            vec![("mine".to_string(), "3".to_string())],
            "`%` must not match an arbitrary run"
        );
    }

    // ---- pure string / format helpers ----------------------------------

    #[test]
    fn hex32_roundtrips_to_64_chars() {
        assert_eq!(hex32(&[0u8; 32]), "0".repeat(64));
        assert_eq!(hex32(&[0xffu8; 32]), "f".repeat(64));

        // A distinct byte-per-index pattern renders in order and round-trips.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let hexed = hex32(&key);
        assert_eq!(hexed.len(), 64);
        assert_eq!(&hexed[..2], "00");
        assert_eq!(&hexed[62..], "1f"); // byte 31 == 0x1f
        assert_eq!(hex::decode(&hexed).unwrap(), key.to_vec());
    }

    #[test]
    fn seq_csv_formats_integer_lists() {
        assert_eq!(seq_csv(&[]), "");
        assert_eq!(seq_csv(&[42]), "42");
        assert_eq!(seq_csv(&[1, 2, 3]), "1,2,3");
        // i64s (incl. negatives) render verbatim — no quoting, no spaces.
        assert_eq!(seq_csv(&[-1, 0, 7]), "-1,0,7");
    }

    #[test]
    fn vec_to_json_renders_float_arrays() {
        assert_eq!(vec_to_json(&[]), "[]");
        assert_eq!(vec_to_json(&[1.0]), "[1]");
        assert_eq!(vec_to_json(&[1.0, 2.5, -3.25]), "[1,2.5,-3.25]");
    }

    // ---- Value bridge (SQL <-> Rust) -----------------------------------

    #[test]
    fn value_bridge_roundtrips() {
        // encode helpers produce the right Value variant, decode helpers only
        // accept a matching one.
        assert_eq!(v_i64(&pi(42)), Some(42));
        assert_eq!(v_i64(&pt("x")), None);
        assert_eq!(v_blob(&pb(vec![1, 2, 3])), Some(vec![1, 2, 3]));
        assert_eq!(v_blob(&pi(1)), None);
        // opt_i: Some -> Integer, None -> SQL NULL.
        assert_eq!(v_i64(&opt_i(Some(7))), Some(7));
        assert!(matches!(opt_i(None), Value::Null));
        // v_f64 accepts both Real and Integer.
        assert!((v_f64(&Value::Real(1.5)).unwrap() - 1.5).abs() < 1e-12);
        assert!((v_f64(&pi(3)).unwrap() - 3.0).abs() < 1e-12);
        assert_eq!(v_f64(&pt("x")), None);
    }

    // ---- KDF sidecar parsing (crypto-critical) -------------------------

    fn kdf_line(salt: &[u8], m: u32, t: u32, p: u32) -> String {
        format!("v1 argon2id {} {m} {t} {p}", hex::encode(salt))
    }

    #[test]
    fn parse_kdf_sidecar_accepts_valid() {
        let salt = [7u8; KDF_SALT_LEN];
        let text = kdf_line(&salt, KDF_M_COST, KDF_T_COST, KDF_P_COST);
        let (got_salt, m, t, p) = parse_kdf_sidecar(&text, "x.kdf").unwrap();
        assert_eq!(got_salt, salt);
        assert_eq!((m, t, p), (KDF_M_COST, KDF_T_COST, KDF_P_COST));
        // Tolerant of leading whitespace and a trailing newline.
        assert!(parse_kdf_sidecar(&format!("  {text}\n"), "x.kdf").is_ok());
        // Boundary params are accepted.
        let salt_hex = hex::encode([1u8; KDF_SALT_LEN]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt_hex} 8 1 1"), "x").is_ok());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt_hex} 1048576 16 16"), "x").is_ok());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_malformed() {
        let salt = hex::encode([1u8; KDF_SALT_LEN]);
        // wrong token count (too few / too many)
        assert!(parse_kdf_sidecar("v1 argon2id", "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 1 2 3 4"), "x").is_err());
        // wrong version tag
        assert!(parse_kdf_sidecar(&format!("v2 argon2id {salt} 19456 2 1"), "x").is_err());
        // wrong algorithm
        assert!(parse_kdf_sidecar(&format!("v1 scrypt {salt} 19456 2 1"), "x").is_err());
        // non-hex salt / non-numeric params
        assert!(parse_kdf_sidecar("v1 argon2id zzzz 19456 2 1", "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} m 2 1"), "x").is_err());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_wrong_salt_length() {
        // 8 bytes (too short) and 32 bytes (too long) both rejected; only the
        // 16-byte KDF_SALT_LEN is valid.
        let short = hex::encode([1u8; 8]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {short} 19456 2 1"), "x").is_err());
        let long = hex::encode([1u8; 32]);
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {long} 19456 2 1"), "x").is_err());
    }

    #[test]
    fn parse_kdf_sidecar_rejects_out_of_range_params() {
        let salt = hex::encode([1u8; KDF_SALT_LEN]);
        // m outside [8, 1_048_576] (guards against a tampered multi-GiB cost)
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 7 2 1"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 1048577 2 1"), "x").is_err());
        // t outside [1, 16]
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 0 1"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 17 1"), "x").is_err());
        // p outside [1, 16]
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 2 0"), "x").is_err());
        assert!(parse_kdf_sidecar(&format!("v1 argon2id {salt} 19456 2 17"), "x").is_err());
    }

    // ---- passphrase key derivation (Argon2id) --------------------------

    #[test]
    fn derive_key_rejects_empty_or_whitespace_passphrase() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.db");
        let path = path.to_str().unwrap();
        assert!(DejaDB::derive_key_for(path, "").is_err());
        assert!(DejaDB::derive_key_for(path, "   ").is_err());
        assert!(DejaDB::derive_key_for(path, "\t\n ").is_err());
        // A rejected passphrase must not leave a sidecar behind.
        assert!(!std::path::Path::new(&format!("{path}.kdf")).exists());
    }

    #[test]
    fn derive_key_is_deterministic_for_same_salt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("k.db");
        let path = path.to_str().unwrap();
        // First call mints the .kdf sidecar (fresh salt); the second reuses it.
        let k1 = DejaDB::derive_key_for(path, "correct horse battery staple").unwrap();
        let k2 = DejaDB::derive_key_for(path, "correct horse battery staple").unwrap();
        assert_eq!(*k1, *k2);
        // Same salt, different passphrase -> different key.
        let k3 = DejaDB::derive_key_for(path, "a different passphrase").unwrap();
        assert_ne!(*k1, *k3);
    }

    #[test]
    fn derive_key_differs_across_salts() {
        let dir = TempDir::new().unwrap();
        let p1 = dir.path().join("a.db");
        let p2 = dir.path().join("b.db");
        let (p1, p2) = (p1.to_str().unwrap(), p2.to_str().unwrap());
        // Same passphrase, independent sidecars -> independent random salts ->
        // different keys.
        let k1 = DejaDB::derive_key_for(p1, "same-pass").unwrap();
        let k2 = DejaDB::derive_key_for(p2, "same-pass").unwrap();
        assert_ne!(*k1, *k2);
        assert!(std::path::Path::new(&format!("{p1}.kdf")).exists());
        assert!(std::path::Path::new(&format!("{p2}.kdf")).exists());
    }

    // ---- RRF fusion math -----------------------------------------------

    /// Mirror of the inline reciprocal-rank fusion in `recall_hybrid_tuned`:
    /// each leg contributes `1/(RRF_K0 + rank)`, scores sum across legs, and
    /// ties break by seq id descending. Kept here (rather than extracted from
    /// production) so these tests pin the fusion contract without changing the
    /// recall path; if the inline formula drifts, these expectations should
    /// be updated in lockstep.
    fn rrf_fuse(legs: &[&[i64]]) -> Vec<(i64, f64)> {
        let mut scores: HashMap<i64, f64> = HashMap::new();
        for leg in legs {
            for (rank, seq) in leg.iter().enumerate() {
                *scores.entry(*seq).or_insert(0.0) += 1.0 / (RRF_K0 + rank as f64);
            }
        }
        let mut ranked: Vec<(i64, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.0.cmp(&a.0))
        });
        ranked
    }

    #[test]
    fn rrf_k0_constant_is_pinned() {
        // The standard k0 = 60; observability surfaces export this value.
        assert!((RRF_K0 - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rrf_single_leg_preserves_rank_order() {
        let leg = [10i64, 20, 30];
        let fused = rrf_fuse(&[&leg]);
        assert_eq!(fused.iter().map(|(s, _)| *s).collect::<Vec<_>>(), vec![10, 20, 30]);
        assert!((fused[0].1 - 1.0 / 60.0).abs() < 1e-12);
        assert!((fused[1].1 - 1.0 / 61.0).abs() < 1e-12);
        assert!((fused[2].1 - 1.0 / 62.0).abs() < 1e-12);
        // Contribution strictly decreases with rank.
        assert!(fused[0].1 > fused[1].1 && fused[1].1 > fused[2].1);
    }

    #[test]
    fn rrf_rewards_agreement_across_legs() {
        // seq 1 tops both legs; seq 2 only in leg A; seq 3 only in leg B.
        let a = [1i64, 2];
        let b = [1i64, 3];
        let fused = rrf_fuse(&[&a, &b]);
        // seq 1 accrues 2/60 and must rank first.
        assert_eq!(fused[0].0, 1);
        let top = fused.iter().find(|(s, _)| *s == 1).unwrap().1;
        assert!((top - 2.0 / 60.0).abs() < 1e-12);
        // A doc in only one leg cannot beat the doc endorsed by both.
        let two = fused.iter().find(|(s, _)| *s == 2).unwrap().1;
        assert!(top > two);
    }

    #[test]
    fn rrf_breaks_ties_by_seq_desc() {
        // Two seqs at rank 0 of their own leg -> equal scores; the larger seq
        // id sorts first, matching the production tie-break.
        let l1 = [5i64];
        let l2 = [9i64];
        let fused = rrf_fuse(&[&l1, &l2]);
        assert!((fused[0].1 - fused[1].1).abs() < 1e-12);
        assert_eq!(fused[0].0, 9);
        assert_eq!(fused[1].0, 5);
    }

    // ---- rule-based query expansion (pure, deterministic) --------------

    #[test]
    fn english_expander_substitutes_synonyms() {
        let ex = EnglishExpander::default();
        let v = ex.expand("cell");
        assert!(v.contains(&"mobile".to_string()));
        assert!(v.contains(&"phone".to_string()));
        // The original query is never echoed back as a variant.
        assert!(!v.contains(&"cell".to_string()));
    }

    #[test]
    fn english_expander_stems_and_is_bounded() {
        let ex = EnglishExpander::new(4);
        // Plural -> singular stem bridges the vocabulary gap.
        assert!(ex.expand("cars").contains(&"car".to_string()));
        // Empty query yields no variants.
        assert!(ex.expand("").is_empty());
        // Variant count honors the cap.
        assert!(ex.expand("cell phone email car").len() <= 4);
    }

    // ---- HLC monotonicity + dictionary (need a live store handle) ------

    fn open_tmp() -> (DejaDB, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("unit.db");
        let db = DejaDB::open(path.to_str().unwrap()).unwrap();
        (db, dir)
    }

    #[test]
    fn next_hlc_is_strictly_monotonic_within_one_ms() {
        let (mut db, _d) = open_tmp();
        // Force the "same wall-clock millisecond" branch deterministically:
        // seed hlc_last far in the future so `wall <= hlc_last` on every call,
        // proving the in-memory +1 counter alone keeps HLCs strictly
        // increasing without any wall-clock advance (hence no sleep needed).
        db.hlc_last = (now_ms() + 1_000_000) << 16;
        let a = db.next_hlc();
        let b = db.next_hlc();
        let c = db.next_hlc();
        assert_eq!(b, a + 1);
        assert_eq!(c, b + 1);
    }

    #[test]
    fn next_hlc_tracks_wall_clock_when_it_advances() {
        let (mut db, _d) = open_tmp();
        let before = now_ms();
        db.hlc_last = 0;
        let first = db.next_hlc();
        let after = now_ms();
        // With a zero baseline the wall clock (ms << 16) dominates: the top
        // bits carry the millisecond of the call.
        assert!(first >> 16 >= before && first >> 16 <= after);
    }

    #[test]
    fn next_hlc_never_repeats_over_many_calls() {
        let (mut db, _d) = open_tmp();
        let mut last = db.next_hlc();
        for _ in 0..5000 {
            let n = db.next_hlc();
            assert!(n > last, "HLC must strictly increase: {n} !> {last}");
            last = n;
        }
    }

    #[test]
    fn term_id_interns_and_reverse_scans() {
        let (mut db, _d) = open_tmp();
        let a = db.term_id("alice").unwrap();
        let b = db.term_id("bob").unwrap();
        assert_ne!(a, b);
        // Re-interning a known term is a cache hit -> same id.
        assert_eq!(db.term_id("alice").unwrap(), a);
        // Forward lookup.
        assert_eq!(db.term_lookup("alice"), Some(a));
        assert_eq!(db.term_lookup("bob"), Some(b));
        assert_eq!(db.term_lookup("nobody"), None);
        // Reverse scan (id -> term).
        assert_eq!(db.term_str(a).as_deref(), Some("alice"));
        assert_eq!(db.term_str(b).as_deref(), Some("bob"));
        assert_eq!(db.term_str(999_999), None);
    }

    #[test]
    fn term_ids_persist_and_continue_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dict.db");
        let path = path.to_str().unwrap();
        let (a, b);
        {
            let mut db = DejaDB::open(path).unwrap();
            a = db.term_id("x").unwrap();
            b = db.term_id("y").unwrap();
            assert!(b > a);
        }
        // Reopen: the dictionary reloads, existing terms keep their ids, and a
        // fresh term gets an id beyond the previous max (next_term continues).
        {
            let mut db = DejaDB::open(path).unwrap();
            assert_eq!(db.term_lookup("x"), Some(a));
            assert_eq!(db.term_lookup("y"), Some(b));
            let c = db.term_id("z").unwrap();
            assert!(c > b);
            assert_eq!(db.term_str(c).as_deref(), Some("z"));
        }
    }
}
