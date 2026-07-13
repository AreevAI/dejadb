//! dejadb-store — the embedded Turso-backed store for DejaDB.
//!
//! Implements the store schema: dictionary-encoded 2½-permutation
//! triple indexes (SPO + POS mandatory, OSP selective for entity-valued
//! relations), `entity_latest` materialization, op-log + HLC + tombstones,
//! thread index, and the vaais operation profile (add / recall / batch /
//! supersede / forget) plus bounded graph ops and two-axis `entity_at`.
//!
//! Sync facade over the async turso crate: `DejaDB` owns a current-thread
//! runtime; point ops measured at µs-class through this path in M0.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use dejadb_core::error::{Hash, DejaDbError, Result};
use dejadb_core::format::deserialize::{deserialize_blob, DeserializedGrain};
use dejadb_core::format::serialize::serialize_grain;
use dejadb_core::types::Grain;
use turso::{Builder, Connection, Value};

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
    /// Tier-1: MMR diversity reorder. `lambda` in [0,1] — 1.0 = pure
    /// relevance, 0.0 = maximum diversity. Requires an embedder + stored
    /// vectors; silently skipped otherwise.
    pub diversity_lambda: Option<f32>,
}

/// One extracted fact from a `remember()` extraction callback.
#[derive(Debug, Clone)]
pub struct FactDraft {
    pub subject: String,
    pub relation: String,
    pub object: String,
    pub confidence: f64,
}

/// Result of `DejaDB::remember`.
#[derive(Debug, Clone)]
pub struct RememberResult {
    pub observation: Hash,
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

/// Hex-encode a 32-byte key for Turso `PRAGMA hexkey`.
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

fn db_err<E: std::fmt::Display>(e: E) -> DejaDbError {
    DejaDbError::Storage(e.to_string())
}

fn v_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        _ => None,
    }
}

fn v_blob(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Blob(b) => Some(b.clone()),
        _ => None,
    }
}

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
    }
}

/// The single text projection the FTS and vector legs index: the grain's
/// explicit `embedding_text` override when present (its documented contract —
/// import pipelines set it to preserve original prose), else
/// "subject relation object" plus any top-level `content`. `None` = nothing
/// to index. Used by the write path, the reranker's candidate text, and the
/// `rebuild_text_index` backfill so all three stay in lockstep.
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

/// The embedded DejaDB store handle — one file per memory.
pub struct DejaDB {
    rt: tokio::runtime::Runtime,
    _db: turso::Database,
    conn: Connection,
    dict: HashMap<String, i64>,
    next_term: i64,
    next_seq: i64,
    next_op: i64,
    hlc_last: i64,
    entity_rels: HashSet<String>,
    index_text: bool,
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
    blob_dir: std::path::PathBuf,
    // cached hot-path statements (lazily prepared)
    st_probe_sp: Option<turso::Statement>,
    st_probe_s: Option<turso::Statement>,
    st_fetch_seq: Option<turso::Statement>,
    st_latest: Option<turso::Statement>,
}

async fn ensure_stmt<'a>(
    slot: &'a mut Option<turso::Statement>,
    conn: &Connection,
    sql: &str,
) -> Result<&'a mut turso::Statement> {
    if slot.is_none() {
        *slot = Some(conn.prepare(sql).await.map_err(db_err)?);
    }
    Ok(slot.as_mut().unwrap())
}

impl DejaDB {
    /// Open honoring the file's own declarations (`meta` table) when
    /// present. A fresh file is stamped with the defaults. This is the
    /// file-truth path: settings like `text_index` travel with the file,
    /// so the same memory behaves identically on any host.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_internal(path, None)
    }

    /// Open with explicit options. Explicit options are deliberate: they
    /// re-stamp the file's declarations, and a change to an existing
    /// declaration is recorded in `open_warnings()`.
    pub fn open_with(path: &str, opts: DejaDbOptions) -> Result<Self> {
        Self::open_internal(path, Some(opts))
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

    fn open_internal(path: &str, explicit: Option<DejaDbOptions>) -> Result<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(db_err)?;
        let enc_key = explicit.as_ref().and_then(|o| o.encryption_key);
        let (db, conn) = rt.block_on(async {
            let mut b = Builder::new_local(path).experimental_index_method(true);
            if let Some(k) = &enc_key {
                // Provide the AEAD key at BUILD time so the encrypted file
                // header decrypts on the first read. (PRAGMA-after-connect
                // works for create but not reopen — the header is read at
                // connect, before a PRAGMA could set the key.)
                // Wipe our hex rendering of the key after the builder copies it;
                // the storage engine necessarily retains its own copy while the
                // database is open.
                let hexkey = zeroize::Zeroizing::new(hex32(k));
                b = b.experimental_encryption(true).with_encryption(turso::EncryptionOpts {
                    cipher: "aes256gcm".to_string(),
                    hexkey: (*hexkey).clone(),
                });
            }
            let db = b.build().await.map_err(db_err)?;
            let conn = db.connect().map_err(db_err)?;
            for sql in SCHEMA {
                conn.execute(sql, ()).await.map_err(db_err)?;
            }
            Ok::<_, DejaDbError>((db, conn))
        })?;

        // ---- file-carried declarations (meta k/v) --------------------
        let meta: HashMap<String, String> = rt.block_on(async {
            let mut m = HashMap::new();
            let mut rows = conn.query("SELECT k, v FROM meta", ()).await.map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let (Value::Text(k), Value::Text(v)) = (
                    row.get_value(0).map_err(db_err)?,
                    row.get_value(1).map_err(db_err)?,
                ) {
                    m.insert(k, v);
                }
            }
            Ok::<_, DejaDbError>(m)
        })?;
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

        let mut warnings: Vec<String> = Vec::new();
        if enc_key.is_some() {
            warnings.push(
                "encryption-at-rest ON (AES-256-GCM): the memory database is encrypted; the \
                 .blobs CAS sidecar is NOT yet encrypted — keep sensitive media out of this file \
                 (or avoid put_blob) until blob encryption lands"
                    .into(),
            );
        }
        let opts = match explicit {
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
            },
        };

        // Stamp declarations + create the FTS index if wanted.
        rt.block_on(async {
            conn.execute(
                "INSERT OR REPLACE INTO meta(k, v) VALUES ('text_index', ?1)",
                (pt(if opts.index_text { "1" } else { "0" }),),
            )
            .await
            .map_err(db_err)?;
            let mut rels: Vec<&String> = opts.entity_relations.iter().collect();
            rels.sort();
            let rels = serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into());
            conn.execute(
                "INSERT OR REPLACE INTO meta(k, v) VALUES ('entity_relations', ?1)",
                (pt(&rels),),
            )
            .await
            .map_err(db_err)?;
            if opts.index_text {
                // FTS index only when the BM25 leg is wanted: Turso's
                // experimental tantivy index costs ~150ms per write txn in
                // commit bookkeeping even for NULL text rows (measured in
                // the voice-loop bench) — voice/edge profiles skip it.
                conn.execute("CREATE INDEX IF NOT EXISTS idx_fts ON grains USING fts (text)", ())
                    .await
                    .map_err(db_err)?;
            }
            Ok::<_, DejaDbError>(())
        })?;

        // Load dictionary + counters.
        let (dict, next_term, next_seq, next_op, hlc_last) = rt.block_on(async {
            let mut dict = HashMap::new();
            let mut next_term = 1i64;
            {
                let mut rows = conn.query("SELECT id, term FROM terms", ()).await.map_err(db_err)?;
                while let Some(row) = rows.next().await.map_err(db_err)? {
                    let id = v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0);
                    if let Value::Text(t) = row.get_value(1).map_err(db_err)? {
                        dict.insert(t, id);
                    }
                    next_term = next_term.max(id + 1);
                }
            }
            let one = |sql: &'static str| {
                let conn = conn.clone();
                async move {
                    let mut rows = conn.query(sql, ()).await.map_err(db_err)?;
                    let v = match rows.next().await.map_err(db_err)? {
                        Some(row) => v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                        None => 0,
                    };
                    Ok::<i64, DejaDbError>(v)
                }
            };
            let next_seq = one("SELECT COALESCE(MAX(seq),0) FROM grains").await? + 1;
            let next_op = one("SELECT COALESCE(MAX(op_seq),0) FROM oplog").await? + 1;
            let hlc_last = one("SELECT COALESCE(MAX(hlc),0) FROM oplog").await?;
            Ok::<_, DejaDbError>((dict, next_term, next_seq, next_op, hlc_last))
        })?;

        let blob_dir = std::path::PathBuf::from(format!("{}.blobs", path));
        std::fs::create_dir_all(&blob_dir).map_err(db_err)?;

        Ok(DejaDB {
            rt,
            _db: db,
            conn,
            dict,
            next_term,
            next_seq,
            next_op,
            hlc_last,
            entity_rels: opts.entity_relations,
            index_text: opts.index_text,
            embedder: None,
            reranker: None,
            expander: None,
            meta_embed,
            warnings,
            blob_dir,
            st_probe_sp: None,
            st_probe_s: None,
            st_fetch_seq: None,
            st_latest: None,
        })
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
                let conn = &self.conn;
                let ok = self.rt.block_on(async {
                    conn.execute(
                        "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_model', ?1)",
                        (pt(&model),),
                    )
                    .await
                    .map_err(db_err)?;
                    conn.execute(
                        "INSERT OR REPLACE INTO meta(k, v) VALUES ('embedding_dim', ?1)",
                        (pt(&dim.to_string()),),
                    )
                    .await
                    .map_err(db_err)?;
                    Ok::<_, DejaDbError>(())
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

    /// Whether the BM25 text index is populated on writes (file-declared,
    /// honored or re-stamped at open).
    pub fn index_text_enabled(&self) -> bool {
        self.index_text
    }

    /// Drop the FTS index ahead of a bulk load. Turso's experimental FTS
    /// costs ~150ms of commit bookkeeping per write transaction while the
    /// index exists — a tax bulk imports cannot amortize. With the index
    /// dropped, the `text` column keeps populating at full write speed; call
    /// [`Self::rebuild_text_index`] after the load to re-create the index
    /// (Turso indexes all existing rows at CREATE INDEX time — milliseconds,
    /// not per-row). Crash-safe: if the process dies in between, the next
    /// open re-creates the index and backfills it.
    ///
    /// Index-layer only — stored blobs are never touched. Returns `false`
    /// when there was nothing to defer (text indexing off, or already
    /// deferred).
    pub fn defer_text_index(&mut self) -> Result<bool> {
        if !self.index_text {
            return Ok(false);
        }
        let conn = &self.conn;
        self.rt.block_on(async {
            match conn.execute("DROP INDEX idx_fts", ()).await {
                Ok(_) => Ok(true),
                Err(e) => {
                    // Already absent (e.g. defer called twice) is not an error.
                    let s = e.to_string().to_ascii_lowercase();
                    if s.contains("no such index") {
                        Ok(false)
                    } else {
                        Err(db_err(e))
                    }
                }
            }
        })
    }

    /// (Re)build the FTS index. Backfills the `text` column for rows written
    /// while text indexing was off — deriving the same [`projected_text`]
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
        let conn = &self.conn;
        let rows: Vec<(i64, Vec<u8>)> = self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query("SELECT seq, blob FROM grains WHERE text IS NULL", ())
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let seq = v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0);
                if let Value::Blob(b) = row.get_value(1).map_err(db_err)? {
                    out.push((seq, b));
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        let mut updates: Vec<(i64, String)> = Vec::new();
        for (seq, blob) in &rows {
            let view = deserialize_blob(blob)?;
            if let Some(t) = projected_text(&view) {
                updates.push((*seq, t));
            }
        }
        let backfilled = updates.len();
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                for (seq, t) in &updates {
                    conn.execute(
                        "UPDATE grains SET text = ?1 WHERE seq = ?2",
                        (pt(t), pi(*seq)),
                    )
                    .await
                    .map_err(db_err)?;
                }
                Ok::<_, DejaDbError>(())
            }
            .await;
            match r {
                Ok(()) => conn.execute("COMMIT", ()).await.map_err(db_err).map(|_| ()),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(e)
                }
            }
        })?;
        // 2) Re-create the index — Turso backfills all existing rows here.
        self.rt.block_on(async {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_fts ON grains USING fts (text)",
                (),
            )
            .await
            .map_err(db_err)
        })?;
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
        let conn = &self.conn;
        self.rt.block_on(async {
            conn.execute("INSERT INTO terms(id, term) VALUES (?1, ?2)", (pi(id), pt(term)))
                .await
                .map_err(db_err)
        })?;
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
        let conn = &self.conn;
        let bytes = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND o=?4",
                    (pi(ns), pi(s), pi(p), pi(o)),
                )
                .await
                .map_err(db_err)?;
            Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                None => None,
            })
        })?;
        match bytes {
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
        let embed_text = projected;
        let embedding = match (&self.embedder, &embed_text) {
            (Some(e), Some(t)) => Some(e.embed(t)?),
            _ => None,
        };
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
        })
    }

    fn add_batch_inner(&mut self, grains: &[&dyn AddableDyn]) -> Result<Vec<Hash>> {
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

        let conn = &self.conn;
        let hashes: Vec<Hash> = preps.iter().map(|p| p.hash).collect();
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                let mut st_g = conn
                    .prepare(
                        "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
                         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
                    )
                    .await
                    .map_err(db_err)?;
                let mut st_t = conn
                    .prepare("INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)")
                    .await
                    .map_err(db_err)?;
                let mut st_o = conn
                    .prepare("INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)")
                    .await
                    .map_err(db_err)?;
                let mut st_e = conn
                    .prepare("INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)")
                    .await
                    .map_err(db_err)?;
                let mut st_l = conn
                    .prepare("INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)")
                    .await
                    .map_err(db_err)?;
                let mut st_th = conn
                    .prepare("INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)")
                    .await
                    .map_err(db_err)?;
                for (i, pr) in preps.iter().enumerate() {
                    let seq = first_seq + i as i64;
                    st_g.execute((
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
                    ))
                    .await
                    .map_err(db_err)?;
                    if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
                        st_t.execute((pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)))
                            .await
                            .map_err(db_err)?;
                        if pr.osp {
                            st_o.execute((pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)))
                                .await
                                .map_err(db_err)?;
                        }
                        st_e.execute((
                            pi(pr.ns_id),
                            pi(s),
                            pi(p),
                            pi(o),
                            pi(seq),
                            pb(pr.hash.as_bytes().to_vec()),
                        ))
                        .await
                        .map_err(db_err)?;
                        conn.execute(
                            "DELETE FROM heads WHERE ns=?1 AND s=?2 AND p=?3",
                            (pi(pr.ns_id), pi(s), pi(p)),
                        )
                        .await
                        .map_err(db_err)?;
                        conn.execute(
                            "INSERT INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                            (pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                    if let Some(sess) = pr.session {
                        st_th
                            .execute((pi(pr.ns_id), pi(sess), pi(seq)))
                            .await
                            .map_err(db_err)?;
                    }
                    if let Some(ref emb) = pr.embedding {
                        conn.execute(
                            "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                            (pi(seq), pt(&vec_to_json(emb))),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                    st_l.execute((
                        pi(first_op + i as i64),
                        pi(hlc0 + i as i64),
                        pi(OP_ADD),
                        pb(pr.hash.as_bytes().to_vec()),
                    ))
                    .await
                    .map_err(db_err)?;
                }
                Ok::<(), DejaDbError>(())
            }
            .await;
            match r {
                Ok(()) => conn.execute("COMMIT", ()).await.map_err(db_err).map(|_| ()),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(e)
                }
            }
        })?;
        Ok(hashes)
    }

    // ----- read path -----

    /// Reverse provenance: every grain whose `derived_from` is exactly
    /// `parent`, newest first. This is the credit-assignment / episode-unlearn
    /// query — "which lessons were distilled from this observation?" or "what
    /// did the agent learn from this bad session?". Superseded versions are
    /// included so the full derived lineage is visible; the caller can revise
    /// or `forget` each hash. Provenance is not a hot path, so this scans
    /// stored grains rather than maintaining a dedicated index.
    pub fn grains_derived_from(&mut self, parent: &Hash) -> Result<Vec<DeserializedGrain>> {
        let parent_hex = parent.to_hex();
        let conn = &self.conn;
        let blobs = self.rt.block_on(async {
            let mut rows = conn
                .query("SELECT blob FROM grains ORDER BY seq DESC", ())
                .await
                .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(b) = v_blob(&row.get_value(0).map_err(db_err)?) {
                    out.push(b);
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        let mut result = Vec::new();
        for b in &blobs {
            let g = deserialize_blob(b)?;
            if g.get_str("derived_from") == Some(parent_hex.as_str()) {
                result.push(g);
            }
        }
        Ok(result)
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
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        // The `gtype` column stores the enum ordinal (see `extract_view`:
        // `view.grain_type as u8`), not the .mg header type-byte.
        let gt_ord = gtype.map(|g| g as u8 as i64);
        let conn = &self.conn;
        let blobs = self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = match gt_ord {
                Some(gt) => conn
                    .query(
                        "SELECT blob FROM grains WHERE ns=?1 AND gtype=?2 ORDER BY seq DESC LIMIT ?3",
                        (pi(ns_id), pi(gt), pi(limit as i64)),
                    )
                    .await
                    .map_err(db_err)?,
                None => conn
                    .query(
                        "SELECT blob FROM grains WHERE ns=?1 ORDER BY seq DESC LIMIT ?2",
                        (pi(ns_id), pi(limit as i64)),
                    )
                    .await
                    .map_err(db_err)?,
            };
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(b) = v_blob(&row.get_value(0).map_err(db_err)?) {
                    out.push(b);
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        blobs.iter().map(|b| deserialize_blob(b)).collect()
    }

    /// Fetch a grain by content address.
    pub fn get(&mut self, hash: &Hash) -> Result<DeserializedGrain> {
        let conn = &self.conn;
        let blob = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT blob FROM grains WHERE hash = ?1",
                    (pb(hash.as_bytes().to_vec()),),
                )
                .await
                .map_err(db_err)?;
            match rows.next().await.map_err(db_err)? {
                Some(row) => v_blob(&row.get_value(0).map_err(db_err)?)
                    .ok_or_else(|| DejaDbError::Storage("blob column not a blob".into())),
                None => Err(DejaDbError::NotFound(*hash)),
            }
        })?;
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
        let conn = &self.conn;
        let rt = &self.rt;
        let slot_sp = &mut self.st_probe_sp;
        let slot_s = &mut self.st_probe_s;
        let slot_f = &mut self.st_fetch_seq;
        let blobs = rt.block_on(async {
            let mut out = Vec::new();
            let mut seqs: Vec<i64> = Vec::new();
            match p_id {
                Some(p) => {
                    let st = ensure_stmt(
                        slot_sp,
                        conn,
                        "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 ORDER BY seq DESC LIMIT ?4",
                    )
                    .await?;
                    let mut rows = st
                        .query((pi(ns_id), pi(s_id), pi(p), pi(k as i64)))
                        .await
                        .map_err(db_err)?;
                    while let Some(row) = rows.next().await.map_err(db_err)? {
                        if let Some(x) = v_i64(&row.get_value(0).map_err(db_err)?) {
                            seqs.push(x);
                        }
                    }
                }
                None => {
                    let st = ensure_stmt(
                        slot_s,
                        conn,
                        "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND cur=1 ORDER BY seq DESC LIMIT ?3",
                    )
                    .await?;
                    let mut rows = st
                        .query((pi(ns_id), pi(s_id), pi(k as i64)))
                        .await
                        .map_err(db_err)?;
                    while let Some(row) = rows.next().await.map_err(db_err)? {
                        if let Some(x) = v_i64(&row.get_value(0).map_err(db_err)?) {
                            seqs.push(x);
                        }
                    }
                }
            }
            let st_f = ensure_stmt(slot_f, conn, "SELECT blob FROM grains WHERE seq = ?1").await?;
            for seq in seqs {
                let mut rows = st_f.query((pi(seq),)).await.map_err(db_err)?;
                if let Some(row) = rows.next().await.map_err(db_err)? {
                    if let Some(b) = v_blob(&row.get_value(0).map_err(db_err)?) {
                        out.push(b);
                    }
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        blobs.iter().map(|b| deserialize_blob(b)).collect()
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
        let conn = &self.conn;
        let rt = &self.rt;
        let slot = &mut self.st_latest;
        let hash = rt.block_on(async {
            let st = ensure_stmt(
                slot,
                conn,
                "SELECT hash FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3",
            )
            .await?;
            let mut rows = st
                .query((pi(ns_id), pi(s_id), pi(p_id)))
                .await
                .map_err(db_err)?;
            Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                None => None,
            })
        })?;
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
        let conn = &self.conn;
        let blobs = self.rt.block_on(async {
            let mut seqs = Vec::new();
            {
                let mut rows = conn
                    .query(
                        "SELECT seq FROM thread_idx WHERE ns=?1 AND session=?2 ORDER BY seq DESC LIMIT ?3",
                        (pi(ns_id), pi(sess_id), pi(n as i64)),
                    )
                    .await
                    .map_err(db_err)?;
                while let Some(row) = rows.next().await.map_err(db_err)? {
                    if let Some(x) = v_i64(&row.get_value(0).map_err(db_err)?) {
                        seqs.push(x);
                    }
                }
            }
            let mut out = Vec::new();
            for seq in seqs.into_iter().rev() {
                let mut rows = conn
                    .query("SELECT blob FROM grains WHERE seq = ?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                if let Some(row) = rows.next().await.map_err(db_err)? {
                    if let Some(b) = v_blob(&row.get_value(0).map_err(db_err)?) {
                        out.push(b);
                    }
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        blobs.iter().map(|b| deserialize_blob(b)).collect()
    }

    // ----- evolution path -----

    /// Supersede `old` with `new_grain` (atomic, OMS L2 semantics).
    /// Sets `derived_from` on the new grain; the old grain's blob is never
    /// touched — only its index-layer fields change.
    pub fn supersede<G: Grain + 'static>(&mut self, old: &Hash, new_grain: &mut G) -> Result<Hash> {
        // Old head must exist and be current.
        let conn = &self.conn;
        let old_row = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT seq, ns, s, p, svt FROM grains WHERE hash = ?1",
                    (pb(old.as_bytes().to_vec()),),
                )
                .await
                .map_err(db_err)?;
            match rows.next().await.map_err(db_err)? {
                Some(row) => {
                    let seq = v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0);
                    let ns = v_i64(&row.get_value(1).map_err(db_err)?);
                    let s = v_i64(&row.get_value(2).map_err(db_err)?);
                    let p = v_i64(&row.get_value(3).map_err(db_err)?);
                    let svt = v_i64(&row.get_value(4).map_err(db_err)?);
                    Ok::<_, DejaDbError>(Some((seq, ns, s, p, svt)))
                }
                None => Ok(None),
            }
        })?;
        let (old_seq, _ns, old_s, old_p, old_svt) = match old_row {
            Some(x) => x,
            None => return Err(DejaDbError::NotFound(*old)),
        };
        if old_svt.is_some() {
            return Err(DejaDbError::SupersessionConflict(*old));
        }

        new_grain.common_mut().derived_from = Some(old.to_hex());
        let new_hash = self.add(new_grain)?;
        let now = now_ms();

        let op_seq = self.next_op;
        self.next_op += 1;
        let hlc = self.next_hlc();
        let conn = &self.conn;
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                conn.execute(
                    "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                    (pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)),
                )
                .await
                .map_err(db_err)?;
                conn.execute(
                    "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                    (pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())),
                )
                .await
                .map_err(db_err)?;
                conn.execute("UPDATE triples SET cur=0 WHERE seq=?1", (pi(old_seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute("UPDATE osp SET cur=0 WHERE seq=?1", (pi(old_seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute(
                    "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                    (pi(op_seq), pi(hlc), pi(OP_SUPERSEDE), pb(new_hash.as_bytes().to_vec())),
                )
                .await
                .map_err(db_err)?;
                Ok::<(), DejaDbError>(())
            }
            .await;
            match r {
                Ok(()) => conn.execute("COMMIT", ()).await.map_err(db_err).map(|_| ()),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(e)
                }
            }
        })?;
        let _ = (old_s, old_p);
        Ok(new_hash)
    }

    /// Forget (erase from hot store) — writes a tombstone to the op-log.
    /// File-level crypto-erasure remains the strong path.
    pub fn forget(&mut self, hash: &Hash) -> Result<()> {
        let conn = &self.conn;
        let row = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT seq, ns, s, p FROM grains WHERE hash = ?1",
                    (pb(hash.as_bytes().to_vec()),),
                )
                .await
                .map_err(db_err)?;
            match rows.next().await.map_err(db_err)? {
                Some(row) => Ok::<_, DejaDbError>(Some((
                    v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                    v_i64(&row.get_value(1).map_err(db_err)?),
                    v_i64(&row.get_value(2).map_err(db_err)?),
                    v_i64(&row.get_value(3).map_err(db_err)?),
                ))),
                None => Ok(None),
            }
        })?;
        let (seq, ns, s, p) = match row {
            Some(x) => x,
            None => return Err(DejaDbError::NotFound(*hash)),
        };
        let op_seq = self.next_op;
        self.next_op += 1;
        let hlc = self.next_hlc();
        let conn = &self.conn;
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                conn.execute("DELETE FROM triples WHERE seq=?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute("DELETE FROM osp WHERE seq=?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute("DELETE FROM embeddings WHERE seq=?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute("DELETE FROM thread_idx WHERE seq=?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                conn.execute("DELETE FROM grains WHERE seq=?1", (pi(seq),))
                    .await
                    .map_err(db_err)?;
                // entity_latest fallback: newest remaining current triple, if any.
                if let (Some(ns), Some(s), Some(p)) = (ns, s, p) {
                    conn.execute(
                        "DELETE FROM entity_latest WHERE ns=?1 AND s=?2 AND p=?3 AND seq=?4",
                        (pi(ns), pi(s), pi(p), pi(seq)),
                    )
                    .await
                    .map_err(db_err)?;
                    let mut rows = conn
                        .query(
                            "SELECT t.o, t.seq, g.hash FROM triples t JOIN grains g ON g.seq=t.seq
                             WHERE t.ns=?1 AND t.s=?2 AND t.p=?3 AND t.cur=1 ORDER BY t.seq DESC LIMIT 1",
                            (pi(ns), pi(s), pi(p)),
                        )
                        .await
                        .map_err(db_err)?;
                    if let Some(row) = rows.next().await.map_err(db_err)? {
                        let o = v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0);
                        let sq = v_i64(&row.get_value(1).map_err(db_err)?).unwrap_or(0);
                        let h = v_blob(&row.get_value(2).map_err(db_err)?).unwrap_or_default();
                        conn.execute(
                            "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                            (pi(ns), pi(s), pi(p), pi(o), pi(sq), pb(h)),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                }
                conn.execute(
                    "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                    (pi(op_seq), pi(hlc), pi(OP_FORGET), pb(hash.as_bytes().to_vec())),
                )
                .await
                .map_err(db_err)?;
                Ok::<(), DejaDbError>(())
            }
            .await;
            match r {
                Ok(()) => conn.execute("COMMIT", ()).await.map_err(db_err).map(|_| ()),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(e)
                }
            }
        })
    }

    // ----- graph ops (bounded, indexed, capped) -----

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
        let conn = &self.conn;
        let reached = self.rt.block_on(async {
            let mut seen: HashSet<i64> = HashSet::new();
            seen.insert(start_id);
            let mut order: Vec<i64> = Vec::new();
            let mut frontier = vec![start_id];
            for _ in 0..depth {
                let mut next = Vec::new();
                for node in &frontier {
                    for p in &rel_ids {
                        if matches!(dir, Direction::Out | Direction::Both) {
                            let mut rows = conn
                                .query(
                                    "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                                    (pi(ns_id), pi(*node), pi(*p)),
                                )
                                .await
                                .map_err(db_err)?;
                            while let Some(row) = rows.next().await.map_err(db_err)? {
                                if let Some(o) = v_i64(&row.get_value(0).map_err(db_err)?) {
                                    if seen.insert(o) {
                                        order.push(o);
                                        next.push(o);
                                        if order.len() >= cap {
                                            return Ok::<_, DejaDbError>(order);
                                        }
                                    }
                                }
                            }
                        }
                        if matches!(dir, Direction::In | Direction::Both) {
                            let mut rows = conn
                                .query(
                                    "SELECT s FROM osp WHERE ns=?1 AND o=?2 AND p=?3 AND cur=1 LIMIT 64",
                                    (pi(ns_id), pi(*node), pi(*p)),
                                )
                                .await
                                .map_err(db_err)?;
                            while let Some(row) = rows.next().await.map_err(db_err)? {
                                if let Some(s) = v_i64(&row.get_value(0).map_err(db_err)?) {
                                    if seen.insert(s) {
                                        order.push(s);
                                        next.push(s);
                                        if order.len() >= cap {
                                            return Ok::<_, DejaDbError>(order);
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
            Ok(order)
        })?;
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
        let conn = &self.conn;
        let parents = self.rt.block_on(async {
            let mut parent: HashMap<i64, i64> = HashMap::new();
            let mut q = VecDeque::from([a]);
            let mut found = false;
            let mut hops = 0usize;
            let mut visited: HashSet<i64> = HashSet::from([a]);
            'outer: while !q.is_empty() && hops < max_depth {
                let level: Vec<i64> = q.drain(..).collect();
                for node in level {
                    for p in &rel_ids {
                        let mut rows = conn
                            .query(
                                "SELECT o FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 LIMIT 64",
                                (pi(ns_id), pi(node), pi(*p)),
                            )
                            .await
                            .map_err(db_err)?;
                        while let Some(row) = rows.next().await.map_err(db_err)? {
                            if let Some(o) = v_i64(&row.get_value(0).map_err(db_err)?) {
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
            Ok::<_, DejaDbError>(if found { Some(parent) } else { None })
        })?;
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
                    let conn = &self.conn;
                    let row = self.rt.block_on(async {
                        let mut rows = conn
                            .query(
                                "SELECT svf, supersedes, blob FROM grains WHERE hash = ?1",
                                (pb(cur.as_bytes().to_vec()),),
                            )
                            .await
                            .map_err(db_err)?;
                        match rows.next().await.map_err(db_err)? {
                            Some(row) => {
                                let svf = v_i64(&row.get_value(0).map_err(db_err)?);
                                let sup = v_blob(&row.get_value(1).map_err(db_err)?);
                                let blob = v_blob(&row.get_value(2).map_err(db_err)?);
                                Ok::<_, DejaDbError>(Some((svf, sup, blob)))
                            }
                            None => Ok(None),
                        }
                    })?;
                    let (svf, sup, blob) = match row {
                        Some(x) => x,
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
                let conn = &self.conn;
                let blob = self.rt.block_on(async {
                    let mut rows = conn
                        .query(
                            "SELECT g.blob FROM triples tr JOIN grains g ON g.seq = tr.seq
                             WHERE tr.ns=?1 AND tr.s=?2 AND tr.p=?3
                               AND g.svt IS NULL
                               AND (g.vf IS NULL OR g.vf <= ?4)
                               AND (g.vt IS NULL OR g.vt > ?4)
                             ORDER BY tr.seq DESC LIMIT 1",
                            (pi(ns_id), pi(s_id), pi(p_id), pi(t)),
                        )
                        .await
                        .map_err(db_err)?;
                    Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                        Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                        None => None,
                    })
                })?;
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

    /// BM25 leg: FTS `MATCH` over grain text (facts as "s r o", event
    /// content). Returns current-grain seqs in match order.
    pub fn search_text(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        if !self.index_text {
            return Ok(Vec::new()); // BM25 leg disabled (edge profile)
        }
        let ns_id = match self.term_lookup(ns) {
            Some(x) => x,
            None => return Ok(Vec::new()),
        };
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT seq FROM grains WHERE text MATCH ?1 AND ns = ?2 AND svt IS NULL LIMIT ?3",
                    (pt(query), pi(ns_id), pi(k as i64)),
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(s) = v_i64(&row.get_value(0).map_err(db_err)?) {
                    out.push(s);
                }
            }
            Ok(out)
        })
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
        let conn = &self.conn;
        self.rt.block_on(async {
            let base = "SELECT g.hash, vector_distance_cos(e.vec, vector32(?2)) AS dist \
                        FROM embeddings e JOIN grains g ON g.seq = e.seq \
                        WHERE g.ns = ?1 AND g.svt IS NULL";
            let mut rows = match (s_id, p_id) {
                (Some(s), Some(p)) => {
                    conn.query(
                        &format!("{base} AND g.s = ?3 AND g.p = ?4 ORDER BY dist LIMIT ?5"),
                        (pi(ns_id), pt(&qjson), pi(s), pi(p), pi(k as i64)),
                    )
                    .await
                }
                (Some(s), None) => {
                    conn.query(
                        &format!("{base} AND g.s = ?3 ORDER BY dist LIMIT ?4"),
                        (pi(ns_id), pt(&qjson), pi(s), pi(k as i64)),
                    )
                    .await
                }
                _ => {
                    conn.query(
                        &format!("{base} ORDER BY dist LIMIT ?3"),
                        (pi(ns_id), pt(&qjson), pi(k as i64)),
                    )
                    .await
                }
            }
            .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let h = v_blob(&row.get_value(0).map_err(db_err)?)
                    .and_then(|b| Hash::try_from_bytes(&b).ok());
                // vector_distance_cos is cosine *distance* (1 − similarity).
                let dist = v_f64(&row.get_value(1).map_err(db_err)?).unwrap_or(1.0);
                if let Some(h) = h {
                    out.push((h, (1.0 - dist) as f32));
                }
            }
            Ok(out)
        })
    }

    pub fn search_vector(&mut self, ns: &str, query: &str, k: usize) -> Result<Vec<i64>> {
        let (Some(embedder), Some(ns_id)) = (&self.embedder, self.term_lookup(ns)) else {
            return Ok(Vec::new());
        };
        let qv = embedder.embed(query)?;
        let qjson = vec_to_json(&qv);
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT e.seq FROM embeddings e JOIN grains g ON g.seq = e.seq
                     WHERE g.ns = ?1 AND g.svt IS NULL
                     ORDER BY vector_distance_cos(e.vec, vector32(?2)) LIMIT ?3",
                    (pi(ns_id), pt(&qjson), pi(k as i64)),
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(s) = v_i64(&row.get_value(0).map_err(db_err)?) {
                    out.push(s);
                }
            }
            Ok(out)
        })
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
            Some(s) => self.recall_seqs(ns, s, relation, leg_k)?,
            None => Vec::new(),
        };
        // leg 2: BM25 — plus Tier-1 query-expansion variant legs. Skipped when
        // the deadline is already spent.
        let mut fts_legs: Vec<Vec<i64>> = Vec::new();
        if let Some(q) = query {
            if !over(&start) {
                fts_legs.push(self.search_text(ns, q, leg_k)?);
                if tuning.query_expansion && self.index_text {
                    for variant in self.expand_query(q) {
                        if over(&start) {
                            break;
                        }
                        let hits = self.search_text(ns, &variant, leg_k)?;
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
                self.search_vector(ns, q, leg_k)?
            }
            _ => Vec::new(),
        };
        if structural.is_empty() && fts_legs.iter().all(|l| l.is_empty()) && vecs.is_empty() {
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
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(b.0.cmp(&a.0)));

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
        for seq in ordered {
            if over(&start) {
                break; // fail-open: partial results beat a blown budget
            }
            if let Some(b) = self.blob_by_seq(seq)? {
                out.push(deserialize_blob(&b)?);
            }
        }
        Ok(out)
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
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = HashMap::new();
            let mut rows = conn.query(&sql, (pt(qjson),)).await.map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let seq = v_i64(&row.get_value(0).map_err(db_err)?);
                let dist = v_f64(&row.get_value(1).map_err(db_err)?);
                if let (Some(s), Some(d)) = (seq, dist) {
                    out.insert(s, 1.0 - d as f32);
                }
            }
            Ok(out)
        })
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
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = HashMap::new();
            let mut rows = conn.query(&sql, ()).await.map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let a = v_i64(&row.get_value(0).map_err(db_err)?);
                let b = v_i64(&row.get_value(1).map_err(db_err)?);
                let dist = v_f64(&row.get_value(2).map_err(db_err)?);
                if let (Some(a), Some(b), Some(d)) = (a, b, dist) {
                    out.insert((a, b), 1.0 - d as f32);
                }
            }
            Ok(out)
        })
    }

    fn recall_seqs(&mut self, ns: &str, subject: &str, relation: Option<&str>, k: usize) -> Result<Vec<i64>> {
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
        let conn = &self.conn;
        let rt = &self.rt;
        let slot_sp = &mut self.st_probe_sp;
        let slot_s = &mut self.st_probe_s;
        rt.block_on(async {
            let mut seqs = Vec::new();
            let mut rows = match p_id {
                Some(p) => {
                    let st = ensure_stmt(
                        slot_sp,
                        conn,
                        "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND p=?3 AND cur=1 ORDER BY seq DESC LIMIT ?4",
                    )
                    .await?;
                    st.query((pi(ns_id), pi(s_id), pi(p), pi(k as i64))).await.map_err(db_err)?
                }
                None => {
                    let st = ensure_stmt(
                        slot_s,
                        conn,
                        "SELECT seq FROM triples WHERE ns=?1 AND s=?2 AND cur=1 ORDER BY seq DESC LIMIT ?3",
                    )
                    .await?;
                    st.query((pi(ns_id), pi(s_id), pi(k as i64))).await.map_err(db_err)?
                }
            };
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(x) = v_i64(&row.get_value(0).map_err(db_err)?) {
                    seqs.push(x);
                }
            }
            Ok(seqs)
        })
    }

    fn blob_by_seq(&mut self, seq: i64) -> Result<Option<Vec<u8>>> {
        let conn = &self.conn;
        let rt = &self.rt;
        let slot = &mut self.st_fetch_seq;
        rt.block_on(async {
            let st = ensure_stmt(slot, conn, "SELECT blob FROM grains WHERE seq = ?1").await?;
            let mut rows = st
                .query((pi(seq),))
                .await
                .map_err(db_err)?;
            Ok(match rows.next().await.map_err(db_err)? {
                Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                None => None,
            })
        })
    }

    /// Distinct subjects holding `relation` in `ns` (POS-index scan).
    /// Backs directory-style listings (memory-tool `view` on a dir).
    pub fn subjects_with_relation(&mut self, ns: &str, relation: &str) -> Result<Vec<String>> {
        let (ns_id, p_id) = match (self.term_lookup(ns), self.term_lookup(relation)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Vec::new()),
        };
        let conn = &self.conn;
        let ids = self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT DISTINCT s FROM triples WHERE ns=?1 AND p=?2 AND cur=1",
                    (pi(ns_id), pi(p_id)),
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(s) = v_i64(&row.get_value(0).map_err(db_err)?) {
                    out.push(s);
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
        let mut subjects: Vec<String> = ids.into_iter().filter_map(|id| self.term_str(id)).collect();
        subjects.sort();
        Ok(subjects)
    }

    /// The `remember()` seam: store raw content as an
    /// Observation grain, run the caller-supplied extraction function
    /// (typically an LLM callback — the host owns the model relationship),
    /// and store each returned draft as a Fact with `derived_from`
    /// provenance back to the observation.
    #[allow(clippy::type_complexity)] // extractor is a plain callback; a type alias would not clarify
    pub fn remember(
        &mut self,
        ns: &str,
        content: &str,
        observer: &str,
        extractor: Option<&dyn Fn(&str) -> Vec<FactDraft>>,
    ) -> Result<RememberResult> {
        use dejadb_core::types::Observation;
        let mut obs = Observation::new(observer, "llm");
        obs.common.namespace = Some(ns.to_string());
        obs.common.context = Some(serde_json::json!({ "content": content }));
        let observation = self.add(&obs)?;

        let mut facts = Vec::new();
        if let Some(f) = extractor {
            for draft in f(content) {
                let mut fact = dejadb_core::types::Fact::new(&draft.subject, &draft.relation, &draft.object);
                fact.common.confidence = draft.confidence.clamp(0.0, 1.0);
                fact.common.namespace = Some(ns.to_string());
                fact.common.derived_from = Some(observation.to_hex());
                fact.common.source_type = Some("derived".to_string());
                facts.push(self.add(&fact)?);
            }
        }
        Ok(RememberResult { observation, facts })
    }

    /// Total number of grains in the hot store.
    pub fn count(&mut self) -> Result<usize> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut rows = conn
                .query("SELECT COUNT(*) FROM grains", ())
                .await
                .map_err(db_err)?;
            Ok(match rows.next().await.map_err(db_err)? {
                Some(row) => v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0) as usize,
                None => 0,
            })
        })
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
        let conn = &self.conn;
        let groups: Vec<(i64, i64, i64)> = self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT ns, s, p FROM heads GROUP BY ns, s, p HAVING COUNT(*) > 1",
                    (),
                )
                .await
                .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let ns = v_i64(&row.get_value(0).map_err(db_err)?);
                let s = v_i64(&row.get_value(1).map_err(db_err)?);
                let p = v_i64(&row.get_value(2).map_err(db_err)?);
                if let (Some(ns), Some(s), Some(p)) = (ns, s, p) {
                    out.push((ns, s, p));
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;

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
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT hash, created_at FROM heads WHERE ns=?1 AND s=?2 AND p=?3
                     ORDER BY created_at DESC, hash DESC",
                    (pi(ns_id), pi(s_id), pi(p_id)),
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let h = v_blob(&row.get_value(0).map_err(db_err)?).unwrap_or_default();
                let c = v_i64(&row.get_value(1).map_err(db_err)?).unwrap_or(0);
                if let Ok(h) = Hash::try_from_bytes(&h) {
                    out.push((h, c));
                }
            }
            Ok(out)
        })
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
        let merge_hash = self.add(merged)?; // add() collapses heads to {merge}
        let now = now_ms();
        let conn = &self.conn;
        for (tip, _) in &tips {
            let seq = self.rt.block_on(async {
                let mut rows = conn
                    .query("SELECT seq, svt FROM grains WHERE hash=?1", (pb(tip.as_bytes().to_vec()),))
                    .await
                    .map_err(db_err)?;
                Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                    Some(row) => {
                        let seq = v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0);
                        let svt = v_i64(&row.get_value(1).map_err(db_err)?);
                        (svt.is_none()).then_some(seq)
                    }
                    None => None,
                })
            })?;
            if let Some(seq) = seq {
                self.rt.block_on(async {
                    conn.execute(
                        "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                        (pb(merge_hash.as_bytes().to_vec()), pi(now), pi(seq)),
                    )
                    .await
                    .map_err(db_err)?;
                    conn.execute("UPDATE triples SET cur=0 WHERE seq=?1", (pi(seq),))
                        .await
                        .map_err(db_err)?;
                    conn.execute("UPDATE osp SET cur=0 WHERE seq=?1", (pi(seq),))
                        .await
                        .map_err(db_err)?;
                    Ok::<(), DejaDbError>(())
                })?;
            }
        }
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
            let conn = &self.conn;
            let row = self.rt.block_on(async {
                let mut rows = conn
                    .query(
                        "SELECT blob, superseded_by, supersedes FROM grains WHERE hash = ?1",
                        (pb(h.as_bytes().to_vec()),),
                    )
                    .await
                    .map_err(db_err)?;
                Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                    Some(row) => Some((
                        v_blob(&row.get_value(0).map_err(db_err)?),
                        v_blob(&row.get_value(1).map_err(db_err)?),
                        v_blob(&row.get_value(2).map_err(db_err)?),
                    )),
                    None => None,
                })
            })?;
            let (blob, sup_by, supersedes) = match row {
                Some(x) => x,
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
    pub fn verify(&mut self) -> Result<VerifyReport> {
        let conn = &self.conn;
        let (integrity, fts_notes, rows) = self.rt.block_on(async {
            // Collect every integrity line; Turso's experimental FTS keeps
            // internal dir indexes that integrity_check miscounts — classify
            // those as benign notes (candidate upstream report), never as
            // corruption. Content-address verification below is the real
            // tamper-evidence check and is unaffected.
            let mut real: Vec<String> = Vec::new();
            let mut fts_notes: Vec<String> = Vec::new();
            {
                let mut rows = conn.query("PRAGMA integrity_check", ()).await.map_err(db_err)?;
                while let Some(row) = rows.next().await.map_err(db_err)? {
                    if let Value::Text(s) = row.get_value(0).map_err(db_err)? {
                        if s == "ok" {
                            continue;
                        } else if s.contains("__turso_internal_fts") {
                            fts_notes.push(s);
                        } else {
                            real.push(s);
                        }
                    }
                }
            }
            let integ = if real.is_empty() { "ok".to_string() } else { real.join("; ") };
            let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            let mut rows = conn.query("SELECT hash, blob FROM grains", ()).await.map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                let h = v_blob(&row.get_value(0).map_err(db_err)?).unwrap_or_default();
                let b = v_blob(&row.get_value(1).map_err(db_err)?).unwrap_or_default();
                out.push((h, b));
            }
            Ok::<_, DejaDbError>((integ, fts_notes, out))
        })?;
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
        let conn = &self.conn;
        self.rt.block_on(async {
            let one = |sql: &'static str| {
                let conn = conn.clone();
                async move {
                    let mut rows = conn.query(sql, ()).await.map_err(db_err)?;
                    Ok::<i64, DejaDbError>(match rows.next().await.map_err(db_err)? {
                        Some(row) => v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                        None => 0,
                    })
                }
            };
            Ok(StoreStats {
                grains: one("SELECT COUNT(*) FROM grains").await? as usize,
                current: one("SELECT COUNT(*) FROM grains WHERE svt IS NULL").await? as usize,
                triples: one("SELECT COUNT(*) FROM triples").await? as usize,
                terms: one("SELECT COUNT(*) FROM terms").await? as usize,
                ops: one("SELECT COUNT(*) FROM oplog").await? as usize,
                events_indexed: one("SELECT COUNT(*) FROM thread_idx").await? as usize,
            })
        })
    }

    /// Op-log cursor read — the change feed (backs sync + UIs).
    pub fn changes_since(&mut self, after_op_seq: i64, limit: usize) -> Result<Vec<OpRecord>> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT op_seq, hlc, op, hash FROM oplog WHERE op_seq > ?1 ORDER BY op_seq LIMIT ?2",
                    (pi(after_op_seq), pi(limit as i64)),
                )
                .await
                .map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                out.push(OpRecord {
                    op_seq: v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                    hlc: v_i64(&row.get_value(1).map_err(db_err)?).unwrap_or(0),
                    op: v_i64(&row.get_value(2).map_err(db_err)?).unwrap_or(0),
                    hash: Hash::try_from_bytes(&v_blob(&row.get_value(3).map_err(db_err)?).unwrap_or_default())?,
                });
            }
            Ok(out)
        })
    }
}

impl DejaDB {
    // ----- CAS blob sidecar -----

    fn blob_path(&self, hex: &str) -> std::path::PathBuf {
        self.blob_dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Store bytes in the per-memory CAS; returns the `cas://sha256:` URI.
    /// Idempotent — content addressing dedupes by construction.
    pub fn put_blob(&mut self, bytes: &[u8]) -> Result<String> {
        use sha2::{Digest, Sha256};
        let hex = hex::encode(Sha256::digest(bytes));
        let path = self.blob_path(&hex);
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap()).map_err(db_err)?;
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, bytes).map_err(db_err)?;
            std::fs::rename(&tmp, &path).map_err(db_err)?;
        }
        Ok(format!("cas://sha256:{hex}"))
    }

    /// Fetch bytes by `cas://sha256:` URI, verifying the hash on read.
    pub fn get_blob(&mut self, uri: &str) -> Result<Vec<u8>> {
        use sha2::{Digest, Sha256};
        let hex = uri
            .strip_prefix("cas://sha256:")
            .ok_or_else(|| DejaDbError::Validation(format!("not a cas uri: {uri}")))?;
        let bytes = std::fs::read(self.blob_path(hex))
            .map_err(|_| DejaDbError::Storage(format!("blob missing: {uri}")))?;
        if hex::encode(Sha256::digest(&bytes)) != hex {
            return Err(DejaDbError::Storage(format!("blob corrupt: {uri}")));
        }
        Ok(bytes)
    }

    /// Remove CAS blobs not referenced by any live grain's `content_refs`.
    /// Returns the number of blobs removed.
    pub fn gc_blobs(&mut self) -> Result<usize> {
        // Collect referenced hashes from live grains.
        let conn = &self.conn;
        let blobs: Vec<Vec<u8>> = self.rt.block_on(async {
            let mut out = Vec::new();
            let mut rows = conn.query("SELECT blob FROM grains", ()).await.map_err(db_err)?;
            while let Some(row) = rows.next().await.map_err(db_err)? {
                if let Some(b) = v_blob(&row.get_value(0).map_err(db_err)?) {
                    out.push(b);
                }
            }
            Ok::<_, DejaDbError>(out)
        })?;
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
        if let Ok(shards) = std::fs::read_dir(&self.blob_dir) {
            for shard in shards.flatten() {
                let prefix = shard.file_name().to_string_lossy().to_string();
                if let Ok(files) = std::fs::read_dir(shard.path()) {
                    for f in files.flatten() {
                        let rest = f.file_name().to_string_lossy().to_string();
                        let hex = format!("{prefix}{rest}");
                        if !referenced.contains(&hex) && std::fs::remove_file(f.path()).is_ok() {
                            removed += 1;
                        }
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
        let conn = &self.conn;
        let rt = &self.rt;
        let mut out: Vec<u8> = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(BUNDLE_MAGIC);
        let mut last = after_op_seq;
        for rec in &ops {
            let blob: Option<Vec<u8>> = if rec.op == OP_FORGET {
                None
            } else {
                rt.block_on(async {
                    let mut rows = conn
                        .query(
                            "SELECT blob FROM grains WHERE hash = ?1",
                            (pb(rec.hash.as_bytes().to_vec()),),
                        )
                        .await
                        .map_err(db_err)?;
                    Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                        Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                        None => None,
                    })
                })?
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
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT blob FROM grains WHERE hash = ?1",
                    (pb(hash.as_bytes().to_vec()),),
                )
                .await
                .map_err(db_err)?;
            Ok::<_, DejaDbError>(match rows.next().await.map_err(db_err)? {
                Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                None => None,
            })
        })
    }

    fn has_grain(&mut self, hash: &Hash) -> Result<bool> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let mut rows = conn
                .query(
                    "SELECT 1 FROM grains WHERE hash = ?1",
                    (pb(hash.as_bytes().to_vec()),),
                )
                .await
                .map_err(db_err)?;
            Ok(rows.next().await.map_err(db_err)?.is_some())
        })
    }

    /// Insert one already-serialized grain (bundle import path).
    fn insert_blob(&mut self, blob: Vec<u8>, hash: Hash, op: i64, hlc_in: i64) -> Result<()> {
        let pr = self.prep_from_blob(blob, hash)?;
        let seq = self.next_seq;
        self.next_seq += 1;
        let op_seq = self.next_op;
        self.next_op += 1;
        self.hlc_last = self.hlc_last.max(hlc_in);
        let conn = &self.conn;
        self.rt.block_on(async {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                conn.execute(
                    "INSERT INTO grains(seq,hash,ns,gtype,created_at,s,p,o,vf,vt,svf,svt,superseded_by,supersedes,text,blob)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,NULL,?12,?13)",
                    (
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
                    ),
                )
                .await
                .map_err(db_err)?;
                if let (Some(s), Some(p), Some(o)) = (pr.s, pr.p, pr.o) {
                    conn.execute(
                        "INSERT INTO triples(ns,s,p,o,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                        (pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq)),
                    )
                    .await
                    .map_err(db_err)?;
                    if pr.osp {
                        conn.execute(
                            "INSERT INTO osp(ns,o,s,p,seq,cur) VALUES (?1,?2,?3,?4,?5,1)",
                            (pi(pr.ns_id), pi(o), pi(s), pi(p), pi(seq)),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                    // import path: UNION into heads (never collapse other
                    // tips — that's the local single-writer semantic only)
                    conn.execute(
                        "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                        (pi(pr.ns_id), pi(s), pi(p), pi(seq), pb(pr.hash.as_bytes().to_vec()), pi(pr.created)),
                    )
                    .await
                    .map_err(db_err)?;
                    // provisional election for entity_latest: replace only if
                    // (created_at, hash) beats the current head — deterministic
                    // on every node, no coordination.
                    let cur = {
                        let mut rows = conn
                            .query(
                                "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                                 ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                                 WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                                (pi(pr.ns_id), pi(s), pi(p)),
                            )
                            .await
                            .map_err(db_err)?;
                        match rows.next().await.map_err(db_err)? {
                            Some(row) => Some((
                                v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                                v_blob(&row.get_value(1).map_err(db_err)?).unwrap_or_default(),
                            )),
                            None => None,
                        }
                    };
                    let wins = match &cur {
                        Some((c, h)) => (pr.created, pr.hash.as_bytes().as_slice()) > (*c, h.as_slice()),
                        None => true,
                    };
                    if wins {
                        conn.execute(
                            "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                            (pi(pr.ns_id), pi(s), pi(p), pi(o), pi(seq), pb(pr.hash.as_bytes().to_vec())),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                }
                if let Some(sess) = pr.session {
                    conn.execute(
                        "INSERT INTO thread_idx(ns,session,seq) VALUES (?1,?2,?3)",
                        (pi(pr.ns_id), pi(sess), pi(seq)),
                    )
                    .await
                    .map_err(db_err)?;
                }
                if let Some(ref emb) = pr.embedding {
                    conn.execute(
                        "INSERT INTO embeddings(seq, vec) VALUES (?1, vector32(?2))",
                        (pi(seq), pt(&vec_to_json(emb))),
                    )
                    .await
                    .map_err(db_err)?;
                }
                conn.execute(
                    "INSERT INTO oplog(op_seq,hlc,op,hash) VALUES (?1,?2,?3,?4)",
                    (pi(op_seq), pi(hlc_in), pi(op), pb(pr.hash.as_bytes().to_vec())),
                )
                .await
                .map_err(db_err)?;
                Ok::<(), DejaDbError>(())
            }
            .await;
            match r {
                Ok(()) => conn.execute("COMMIT", ()).await.map_err(db_err).map(|_| ()),
                Err(e) => {
                    let _ = conn.execute("ROLLBACK", ()).await;
                    Err(e)
                }
            }
        })
    }

    /// Apply the index-layer supersession flip old → new (import path).
    /// Returns whether anything changed (false = idempotent no-op).
    fn apply_supersede_flip(&mut self, old: &Hash, new_hash: &Hash) -> Result<bool> {
        let conn = &self.conn;
        self.rt.block_on(async {
            let old_row = {
                let mut rows = conn
                    .query(
                        "SELECT seq, svt FROM grains WHERE hash = ?1",
                        (pb(old.as_bytes().to_vec()),),
                    )
                    .await
                    .map_err(db_err)?;
                match rows.next().await.map_err(db_err)? {
                    Some(row) => Some((
                        v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                        v_i64(&row.get_value(1).map_err(db_err)?),
                    )),
                    None => None,
                }
            };
            let (old_seq, old_svt) = match old_row {
                Some(x) => x,
                None => return Ok(false), // partial history — fast-forward tolerates
            };
            if old_svt.is_some() {
                // v4 grain-git: old head already superseded. Same superseder →
                // idempotent replay. Different superseder → a FORK: both tips
                // stay alive as heads; entity_latest gets the provisional head
                // (created_at, then hash — deterministic on every node).
                let existing = {
                    let mut rows = conn
                        .query("SELECT superseded_by FROM grains WHERE seq=?1", (pi(old_seq),))
                        .await
                        .map_err(db_err)?;
                    match rows.next().await.map_err(db_err)? {
                        Some(row) => v_blob(&row.get_value(0).map_err(db_err)?),
                        None => None,
                    }
                };
                if existing.as_deref() == Some(new_hash.as_bytes().as_slice()) {
                    return Ok(false); // same supersede — idempotent
                }
                // incoming tip row
                let inc = {
                    let mut rows = conn
                        .query(
                            "SELECT seq, ns, s, p, o, created_at FROM grains WHERE hash=?1",
                            (pb(new_hash.as_bytes().to_vec()),),
                        )
                        .await
                        .map_err(db_err)?;
                    match rows.next().await.map_err(db_err)? {
                        Some(row) => Some((
                            v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                            v_i64(&row.get_value(1).map_err(db_err)?).unwrap_or(0),
                            v_i64(&row.get_value(2).map_err(db_err)?).unwrap_or(0),
                            v_i64(&row.get_value(3).map_err(db_err)?).unwrap_or(0),
                            v_i64(&row.get_value(4).map_err(db_err)?).unwrap_or(0),
                            v_i64(&row.get_value(5).map_err(db_err)?).unwrap_or(0),
                        )),
                        None => None,
                    }
                };
                let Some((inc_seq, ns, s, p, o, inc_created)) = inc else { return Ok(false) };
                conn.execute(
                    "INSERT OR REPLACE INTO heads(ns,s,p,seq,hash,created_at) VALUES (?1,?2,?3,?4,?5,?6)",
                    (pi(ns), pi(s), pi(p), pi(inc_seq), pb(new_hash.as_bytes().to_vec()), pi(inc_created)),
                )
                .await
                .map_err(db_err)?;
                // provisional election vs current entity_latest head
                let cur = {
                    let mut rows = conn
                        .query(
                            "SELECT h.created_at, h.hash FROM heads h JOIN entity_latest e
                             ON e.ns=h.ns AND e.s=h.s AND e.p=h.p AND e.seq=h.seq
                             WHERE e.ns=?1 AND e.s=?2 AND e.p=?3",
                            (pi(ns), pi(s), pi(p)),
                        )
                        .await
                        .map_err(db_err)?;
                    match rows.next().await.map_err(db_err)? {
                        Some(row) => Some((
                            v_i64(&row.get_value(0).map_err(db_err)?).unwrap_or(0),
                            v_blob(&row.get_value(1).map_err(db_err)?).unwrap_or_default(),
                        )),
                        None => None,
                    }
                };
                let incoming_wins = match &cur {
                    Some((c_created, c_hash)) => {
                        (inc_created, new_hash.as_bytes().as_slice()) > (*c_created, c_hash.as_slice())
                    }
                    None => true,
                };
                if incoming_wins {
                    conn.execute(
                        "INSERT OR REPLACE INTO entity_latest(ns,s,p,o,seq,hash) VALUES (?1,?2,?3,?4,?5,?6)",
                        (pi(ns), pi(s), pi(p), pi(o), pi(inc_seq), pb(new_hash.as_bytes().to_vec())),
                    )
                    .await
                    .map_err(db_err)?;
                }
                return Ok(true); // fork registered
            }
            let now = now_ms();
            conn.execute(
                "UPDATE grains SET superseded_by=?1, svt=?2 WHERE seq=?3",
                (pb(new_hash.as_bytes().to_vec()), pi(now), pi(old_seq)),
            )
            .await
            .map_err(db_err)?;
            conn.execute(
                "UPDATE grains SET supersedes=?1 WHERE hash=?2",
                (pb(old.as_bytes().to_vec()), pb(new_hash.as_bytes().to_vec())),
            )
            .await
            .map_err(db_err)?;
            conn.execute("UPDATE triples SET cur=0 WHERE seq=?1", (pi(old_seq),))
                .await
                .map_err(db_err)?;
            conn.execute("UPDATE osp SET cur=0 WHERE seq=?1", (pi(old_seq),))
                .await
                .map_err(db_err)?;
            conn.execute("DELETE FROM heads WHERE seq=?1", (pi(old_seq),))
                .await
                .map_err(db_err)?;
            Ok::<bool, DejaDbError>(true)
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
            if i + len > data.len() {
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
                            if !exists {
                                self.insert_blob(bb.clone(), hash, op, hlc)?;
                                changed = true;
                            }
                            if let Ok(view) = deserialize_blob(&bb) {
                                if let Some(df) = view.get_str("derived_from") {
                                    if let Ok(old) = Hash::from_hex(df) {
                                        changed |= self.apply_supersede_flip(&old, &hash)?;
                                    }
                                }
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

pub mod memory_tool;
pub mod migrate;

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
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(b.0.cmp(&a.0)));
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
