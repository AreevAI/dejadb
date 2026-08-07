//! Backend-parameterized conformance suite for the DejaDB store.
//!
//! Every case in [`cases`] is a plain function over [`Backend`] pinning a
//! storage-semantics contract — fork/head election, oplog replication,
//! tombstones, novelty — that must hold IDENTICALLY on every storage backend.
//! The Turso runner lives in `tests/turso.rs`; a Postgres runner adds a
//! `Backend` impl whose `open_named` maps names to schemas instead of files
//! and runs the same case list unchanged.
//!
//! Determinism rules (from the dejadb-testing skill) apply to every case:
//! isolation via the backend's unit (tempdir file / schema), fixed
//! `created_at` wherever an ordering or election is asserted, and no
//! assertions on unordered result order.

use std::path::Path;

use dejadb_core::types::{Fact, Grain};
use dejadb_store::DejaDB;

pub mod cases;

/// A conformance backend: a factory of isolated stores.
///
/// One `Backend` instance = one isolation unit (a tempdir, a schema prefix).
/// `open_named` maps a stable name to the same underlying storage every time,
/// which is how reopen semantics (counter re-seeding, meta honoring) are
/// exercised — callers must drop the previous handle before reopening a name
/// (single writer per memory holds on every backend).
pub trait Backend {
    /// Open (or reopen) the store identified by `name` within this backend's
    /// isolation unit.
    fn open_named(&self, name: &str) -> DejaDB;

    /// A scratch directory for interchange files (bundles). Backend-neutral:
    /// bundles are files regardless of where the store lives.
    fn scratch(&self) -> &Path;

    /// Label used in failure messages.
    fn name(&self) -> &'static str;

    fn open(&self) -> DejaDB {
        self.open_named("default")
    }
}

/// The embedded (Turso file) backend: names map to files in one tempdir.
pub struct TursoBackend {
    dir: tempfile::TempDir,
}

impl TursoBackend {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self { dir: tempfile::TempDir::new().expect("tempdir") }
    }
}

impl Backend for TursoBackend {
    fn open_named(&self, name: &str) -> DejaDB {
        let path = self.dir.path().join(format!("{name}.db"));
        DejaDB::open(path.to_str().expect("utf8 path")).expect("open turso store")
    }

    fn scratch(&self) -> &Path {
        self.dir.path()
    }

    fn name(&self) -> &'static str {
        "turso"
    }
}

/// Standard test fact: namespaced, mid confidence.
pub fn fact(ns: &str, s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some(ns.to_string());
    f
}

/// A fact with a pinned `created_at` — REQUIRED whenever the case asserts an
/// election or ordering outcome (provisional head = max (created_at, hash)).
pub fn fact_at(ns: &str, s: &str, r: &str, o: &str, created_ms: i64) -> Fact {
    let mut f = fact(ns, s, r, o);
    f.common.created_at = Some(created_ms);
    f
}
