//! # dejadb-waiser
//!
//! The DejaDB substrate adapter for the [`waiser`] engine. It implements
//! [`waiser::OmsSubstrate`] over [`dejadb_cal::DejaDbFacade`], so the governed
//! self-improvement loop runs against real DejaDB `.mg`/Turso memory files.
//!
//! ```no_run
//! use dejadb_waiser::{DejaDbSubstrate, now_ms};
//! use dejadb_store::DejaDB;
//! use waiser::{Engine, RunOptions};
//!
//! let store = DejaDB::open("agent.db").unwrap();
//! let mut sub = DejaDbSubstrate::new(store, None);
//! let engine = Engine::with_builtins();
//! let result = engine.run(&mut sub, &RunOptions::default(), now_ms()).unwrap();
//! println!("proposed {} recommendation(s)", result.stored);
//! ```

mod substrate;

pub use substrate::{BorrowedSubstrate, DejaDbSubstrate};

use std::time::{SystemTime, UNIX_EPOCH};

/// Wall-clock now in epoch milliseconds — the `now_ms` the engine's `run`,
/// `review`, `apply`, and `rollback` take. Kept out of `waiser` itself so the
/// engine stays deterministic (the caller supplies the clock).
///
/// `WAISER_NOW_MS` (epoch ms) overrides the wall clock — the simulation seam
/// that makes a run through the real binary a pure function of (file, policy,
/// time). The golden E2E suite uses it to pin analyzer output and to step time
/// across outcome-review horizons and rejection cooldowns without sleeping.
/// A set-but-unparseable value panics: the caller asked for simulated time,
/// and silently running at wall time instead would defeat the point.
pub fn now_ms() -> i64 {
    if let Ok(v) = std::env::var("WAISER_NOW_MS") {
        return v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("WAISER_NOW_MS is set but not epoch milliseconds: {v:?}"));
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
