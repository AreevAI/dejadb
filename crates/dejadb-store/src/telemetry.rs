//! Telemetry sidecar — `<file>.telemetry.db`.
//!
//! Disposable, rebuildable, **never-syncing** evidence about recall behavior:
//! which grains get retrieved, which queries return nothing, how often. It
//! feeds the telemetry-fed Waiser analyzers (cold grains, coverage gaps,
//! budget pressure) — the signals that turn memory *hygiene* into memory
//! *utility*.
//!
//! Design constraints (proposal §8):
//! - A **separate Turso db file**, encrypted under the SAME key as the main
//!   file (crypto-erasure covers it; a plaintext sidecar holding query text +
//!   top-hits would outlive erased grains).
//! - Recall-path writes are **buffered and non-blocking**: the hot path only
//!   pushes a compact event into an in-memory ring; every SQLite write happens
//!   off-path (flush on write ops / close / explicit). Nothing lands inside
//!   the benched recall (~136µs) or 50ms voice-cadence budgets.
//! - **Host-only** mode (`off | aggregate | full`), never persisted in the
//!   main file — telemetry is host config, not a file-truth.
//! - It **never syncs** (the hub carries the memory file only) and is
//!   rebuildable, so losing it costs evidence detail, never state.

use crate::{db_err, hex32, now_ms, pi, pt};
use dejadb_core::{Hash, Result};
use std::collections::VecDeque;
use tokio::runtime::Runtime;
use turso::{Builder, Connection, Database, Value};

/// How much recall telemetry to retain. Host-only; never a file-truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TelemetryMode {
    /// No sidecar, no writes — behaves exactly as pre-telemetry DejaDB. The
    /// library default (a bare `open()` never enables telemetry); agent-facing
    /// hosts opt into `Aggregate`.
    #[default]
    Off,
    /// Rollups only (grain-access counts, query stats, budget counters).
    /// Bounded and cheap.
    Aggregate,
    /// Aggregate + a per-recall ring log (query text + top hits) for the
    /// console Sessions view. Higher volume; encrypted at rest.
    Full,
}

impl TelemetryMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TelemetryMode::Off => "off",
            TelemetryMode::Aggregate => "aggregate",
            TelemetryMode::Full => "full",
        }
    }
    /// Parse a host-supplied mode string; `None` on an unknown value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "" => Some(TelemetryMode::Off),
            "aggregate" | "agg" | "on" => Some(TelemetryMode::Aggregate),
            "full" => Some(TelemetryMode::Full),
            _ => None,
        }
    }
    fn records_log(&self) -> bool {
        matches!(self, TelemetryMode::Full)
    }
}

/// One recall, captured on the hot path and buffered in memory. Kept minimal:
/// the only heap work on the recall path is these few short-string clones plus
/// one `Vec<Hash>` (Hash is `Copy`, so no per-hash allocation).
pub struct RecallEvent {
    pub ts_ms: i64,
    pub ns: String,
    pub subject: Option<String>,
    pub relation: Option<String>,
    pub query: Option<String>,
    pub n_results: usize,
    pub latency_us: i64,
    pub hashes: Vec<Hash>,
}

impl RecallEvent {
    /// Stable per-intent key: same recall intent → same key, so rollups
    /// accumulate. Free-text query is lower-cased/trimmed; structural recalls
    /// key on subject+relation.
    fn query_key(&self) -> String {
        format!(
            "{}\u{1}{}\u{1}{}\u{1}{}",
            self.ns,
            self.subject.as_deref().unwrap_or(""),
            self.relation.as_deref().unwrap_or(""),
            self.query.as_deref().unwrap_or("").trim().to_lowercase(),
        )
    }
    /// A short human-readable sample of the recall intent (for the console /
    /// coverage-gap surfacing).
    fn sample(&self) -> String {
        self.query
            .as_deref()
            .or(self.subject.as_deref())
            .unwrap_or("")
            .to_string()
    }
    /// Whether this recall counts as a free-text "question" (the coverage-gap
    /// signal). Structural subject-only recalls feed grain-access, not queries.
    fn is_query(&self) -> bool {
        self.query.as_deref().map(|q| !q.trim().is_empty()).unwrap_or(false)
    }
}

/// Per-grain recall rollup (feeds `cold_grains`).
#[derive(Debug, Clone)]
pub struct AccessStat {
    pub hash: String,
    pub ns: String,
    pub recall_count: i64,
    pub last_ms: i64,
}

/// Per-query rollup (feeds `coverage_gap` / dead-query surfacing).
#[derive(Debug, Clone)]
pub struct QueryStat {
    pub key: String,
    pub ns: String,
    pub sample: String,
    pub run_count: i64,
    pub last_ms: i64,
    pub empty_count: i64,
    pub sum_results: i64,
}

/// Assembly-budget rollup (feeds `budget_pressure`).
#[derive(Debug, Clone, Default)]
pub struct BudgetStat {
    pub sample_count: i64,
    pub overflow_count: i64,
}

const TELEM_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT)",
    "CREATE TABLE IF NOT EXISTS recall_log(
        id INTEGER PRIMARY KEY,
        ts_ms INTEGER, ns TEXT, subject TEXT, query TEXT,
        n_results INTEGER, latency_us INTEGER, top_hashes TEXT)",
    "CREATE INDEX IF NOT EXISTS idx_recall_ts ON recall_log(ts_ms)",
    "CREATE TABLE IF NOT EXISTS grain_access(
        hash TEXT PRIMARY KEY, ns TEXT,
        recall_count INTEGER, first_ms INTEGER, last_ms INTEGER)",
    "CREATE TABLE IF NOT EXISTS query_stat(
        qkey TEXT PRIMARY KEY, ns TEXT, sample TEXT,
        run_count INTEGER, last_ms INTEGER, empty_count INTEGER, sum_results INTEGER)",
    "CREATE TABLE IF NOT EXISTS budget_stat(
        id INTEGER PRIMARY KEY, sample_count INTEGER, overflow_count INTEGER)",
];

/// In-memory buffer cap (drop-oldest). Telemetry is disposable — a pure-recall
/// loop that never flushes bounds memory here rather than stalling recall.
const RING_CAP: usize = 8192;
/// Recall-log retention (90 days), pruned opportunistically on flush.
const LOG_RETAIN_MS: i64 = 90 * 24 * 3600 * 1000;
/// Recall-log row cap (~64 MiB proxy), newest-kept, pruned on flush.
const LOG_ROW_CAP: i64 = 200_000;

/// The telemetry sidecar: its own Turso db + connection, an in-memory recall
/// buffer, and the recorded mode. Driven by `DejaDB` through the shared
/// runtime.
pub struct Telemetry {
    _db: Database,
    conn: Connection,
    mode: TelemetryMode,
    buf: VecDeque<RecallEvent>,
    prune_countdown: u32,
}

impl Telemetry {
    /// Open (or create) `<path>.telemetry.db`. `key` reuses the main file's
    /// AEAD key so crypto-erasure covers the sidecar too. Never called for
    /// `TelemetryMode::Off`.
    pub fn open(rt: &Runtime, path: &str, key: Option<&[u8; 32]>, mode: TelemetryMode) -> Result<Self> {
        let sidecar = format!("{}.telemetry.db", path);
        let (db, conn) = rt.block_on(async {
            let mut b = Builder::new_local(&sidecar).experimental_index_method(true);
            if let Some(k) = key {
                let hexkey = zeroize::Zeroizing::new(hex32(k));
                b = b.experimental_encryption(true).with_encryption(turso::EncryptionOpts {
                    cipher: "aes256gcm".to_string(),
                    hexkey: (*hexkey).clone(),
                });
            }
            let db = b.build().await.map_err(db_err)?;
            let conn = db.connect().map_err(db_err)?;
            for sql in TELEM_SCHEMA {
                conn.execute(sql, ()).await.map_err(db_err)?;
            }
            conn.execute(
                "INSERT OR REPLACE INTO meta(k, v) VALUES ('schema_version', '1')",
                (),
            )
            .await
            .map_err(db_err)?;
            Ok::<_, dejadb_core::DejaDbError>((db, conn))
        })?;
        Ok(Telemetry {
            _db: db,
            conn,
            mode,
            buf: VecDeque::new(),
            prune_countdown: 0,
        })
    }

    pub fn mode(&self) -> TelemetryMode {
        self.mode
    }

    /// Hot-path capture: push a recall event into the in-memory ring. **No
    /// I/O** — this is all that runs inside the recall latency budget. Oldest
    /// events are dropped past the cap so recall never stalls on flush.
    #[inline]
    pub fn record(&mut self, ev: RecallEvent) {
        self.buf.push_back(ev);
        while self.buf.len() > RING_CAP {
            self.buf.pop_front();
        }
    }

    /// Off-path drain: persist buffered events into the rollups (and the ring
    /// log in `full` mode) in one transaction. Called from write ops / close /
    /// explicit flush — never from the recall path.
    pub fn flush(&mut self, rt: &Runtime) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let events: Vec<RecallEvent> = self.buf.drain(..).collect();
        let records_log = self.mode.records_log();
        let conn = &self.conn;
        let prune = {
            self.prune_countdown = self.prune_countdown.saturating_sub(1);
            self.prune_countdown == 0
        };
        if prune {
            self.prune_countdown = 32; // prune the ring roughly every 32 flushes
        }
        rt.block_on(async move {
            conn.execute("BEGIN", ()).await.map_err(db_err)?;
            let r = async {
                for ev in &events {
                    let qkey = ev.query_key();
                    // grain-access rollup (all recalls)
                    for h in &ev.hashes {
                        let hex = h.to_hex();
                        conn.execute(
                            "INSERT OR REPLACE INTO grain_access(hash, ns, recall_count, first_ms, last_ms)
                             VALUES (?1, ?2,
                               COALESCE((SELECT recall_count FROM grain_access WHERE hash=?1),0)+1,
                               COALESCE((SELECT first_ms FROM grain_access WHERE hash=?1),?3),
                               ?3)",
                            (pt(&hex), pt(&ev.ns), pi(ev.ts_ms)),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                    // query rollup (free-text "questions" only)
                    if ev.is_query() {
                        let empty = if ev.n_results == 0 { 1 } else { 0 };
                        conn.execute(
                            "INSERT OR REPLACE INTO query_stat(qkey, ns, sample, run_count, last_ms, empty_count, sum_results)
                             VALUES (?1, ?2, ?3,
                               COALESCE((SELECT run_count FROM query_stat WHERE qkey=?1),0)+1,
                               ?4,
                               COALESCE((SELECT empty_count FROM query_stat WHERE qkey=?1),0)+?5,
                               COALESCE((SELECT sum_results FROM query_stat WHERE qkey=?1),0)+?6)",
                            (
                                pt(&qkey),
                                pt(&ev.ns),
                                pt(&ev.sample()),
                                pi(ev.ts_ms),
                                pi(empty),
                                pi(ev.n_results as i64),
                            ),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                    // per-recall ring log (full mode only)
                    if records_log {
                        let hashes: Vec<String> = ev.hashes.iter().map(|h| h.to_hex()).collect();
                        let top = serde_json::to_string(&hashes).unwrap_or_else(|_| "[]".into());
                        conn.execute(
                            "INSERT INTO recall_log(ts_ms, ns, subject, query, n_results, latency_us, top_hashes)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            (
                                pi(ev.ts_ms),
                                pt(&ev.ns),
                                pt(ev.subject.as_deref().unwrap_or("")),
                                pt(ev.query.as_deref().unwrap_or("")),
                                pi(ev.n_results as i64),
                                pi(ev.latency_us),
                                pt(&top),
                            ),
                        )
                        .await
                        .map_err(db_err)?;
                    }
                }
                if records_log && prune {
                    let cutoff = now_ms() - LOG_RETAIN_MS;
                    conn.execute("DELETE FROM recall_log WHERE ts_ms < ?1", (pi(cutoff),))
                        .await
                        .map_err(db_err)?;
                    conn.execute(
                        "DELETE FROM recall_log WHERE id NOT IN
                         (SELECT id FROM recall_log ORDER BY id DESC LIMIT ?1)",
                        (pi(LOG_ROW_CAP),),
                    )
                    .await
                    .map_err(db_err)?;
                }
                Ok::<(), dejadb_core::DejaDbError>(())
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

    /// Record one assembly-budget sample (whether it overflowed). Off the
    /// recall path — called from the ASSEMBLE/context budget check.
    pub fn note_budget(&mut self, rt: &Runtime, overflow: bool) -> Result<()> {
        let conn = &self.conn;
        let ov = if overflow { 1 } else { 0 };
        rt.block_on(async move {
            conn.execute(
                "INSERT OR REPLACE INTO budget_stat(id, sample_count, overflow_count)
                 VALUES (1,
                   COALESCE((SELECT sample_count FROM budget_stat WHERE id=1),0)+1,
                   COALESCE((SELECT overflow_count FROM budget_stat WHERE id=1),0)+?1)",
                (pi(ov),),
            )
            .await
            .map_err(db_err)?;
            Ok::<(), dejadb_core::DejaDbError>(())
        })
    }

    /// FORGET hook: synchronously scrub telemetry that references a forgotten
    /// grain, so the sidecar never outlives an erased grain. Drops the
    /// grain-access row, any ring-log rows that surfaced it, and any buffered
    /// events that named it.
    pub fn scrub(&mut self, rt: &Runtime, hash: &Hash) -> Result<()> {
        let hex = hash.to_hex();
        self.buf.retain(|ev| !ev.hashes.iter().any(|h| h == hash));
        let conn = &self.conn;
        let like = format!("%{}%", hex);
        rt.block_on(async move {
            conn.execute("DELETE FROM grain_access WHERE hash=?1", (pt(&hex),))
                .await
                .map_err(db_err)?;
            conn.execute("DELETE FROM recall_log WHERE top_hashes LIKE ?1", (pt(&like),))
                .await
                .map_err(db_err)?;
            Ok::<(), dejadb_core::DejaDbError>(())
        })
    }

    // ---- readers (consumed by the telemetry-fed analyzers + console) ----

    /// Grain-access rollups, optionally scoped to a namespace.
    pub fn access_stats(&self, rt: &Runtime, ns: Option<&str>) -> Result<Vec<AccessStat>> {
        let conn = &self.conn;
        rt.block_on(async move {
            let mut out = Vec::new();
            let mut rows = match ns {
                Some(n) => conn
                    .query(
                        "SELECT hash, ns, recall_count, last_ms FROM grain_access WHERE ns=?1",
                        (pt(n),),
                    )
                    .await
                    .map_err(db_err)?,
                None => conn
                    .query("SELECT hash, ns, recall_count, last_ms FROM grain_access", ())
                    .await
                    .map_err(db_err)?,
            };
            while let Some(row) = rows.next().await.map_err(db_err)? {
                out.push(AccessStat {
                    hash: text(&row.get_value(0).map_err(db_err)?),
                    ns: text(&row.get_value(1).map_err(db_err)?),
                    recall_count: int(&row.get_value(2).map_err(db_err)?),
                    last_ms: int(&row.get_value(3).map_err(db_err)?),
                });
            }
            Ok(out)
        })
    }

    /// Query rollups, optionally scoped to a namespace.
    pub fn query_stats(&self, rt: &Runtime, ns: Option<&str>) -> Result<Vec<QueryStat>> {
        let conn = &self.conn;
        rt.block_on(async move {
            let mut out = Vec::new();
            let sql = "SELECT qkey, ns, sample, run_count, last_ms, empty_count, sum_results FROM query_stat";
            let mut rows = match ns {
                Some(n) => conn
                    .query(&format!("{sql} WHERE ns=?1"), (pt(n),))
                    .await
                    .map_err(db_err)?,
                None => conn.query(sql, ()).await.map_err(db_err)?,
            };
            while let Some(row) = rows.next().await.map_err(db_err)? {
                out.push(QueryStat {
                    key: text(&row.get_value(0).map_err(db_err)?),
                    ns: text(&row.get_value(1).map_err(db_err)?),
                    sample: text(&row.get_value(2).map_err(db_err)?),
                    run_count: int(&row.get_value(3).map_err(db_err)?),
                    last_ms: int(&row.get_value(4).map_err(db_err)?),
                    empty_count: int(&row.get_value(5).map_err(db_err)?),
                    sum_results: int(&row.get_value(6).map_err(db_err)?),
                });
            }
            Ok(out)
        })
    }

    /// The assembly-budget rollup.
    pub fn budget_stats(&self, rt: &Runtime) -> Result<BudgetStat> {
        let conn = &self.conn;
        rt.block_on(async move {
            let mut rows = conn
                .query("SELECT sample_count, overflow_count FROM budget_stat WHERE id=1", ())
                .await
                .map_err(db_err)?;
            match rows.next().await.map_err(db_err)? {
                Some(row) => Ok(BudgetStat {
                    sample_count: int(&row.get_value(0).map_err(db_err)?),
                    overflow_count: int(&row.get_value(1).map_err(db_err)?),
                }),
                None => Ok(BudgetStat::default()),
            }
        })
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(t) => t.clone(),
        _ => String::new(),
    }
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Integer(i) => *i,
        _ => 0,
    }
}
