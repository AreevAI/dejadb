//! Shared plumbing for the golden dataset tests: committed-file paths, the
//! `deja` subprocess helper, and the lazily imported golden memory that all
//! read-only tests share.
//!
//! Compiled into BOTH golden test binaries (`golden_tests`,
//! `golden_waiser_tests`); each uses its own subset, so per-binary dead-code
//! lints are expected noise, not rot.
#![allow(dead_code)]

pub mod generator;
pub mod waiser_generator;

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// `tests/golden/dataset/` inside the crate — where the committed artifacts
/// (golden.bundle, manifest.json, renders/) live.
pub fn dataset_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/dataset")
}

pub fn bundle_path() -> PathBuf {
    dataset_dir().join("golden.bundle")
}

pub fn manifest_path() -> PathBuf {
    dataset_dir().join("manifest.json")
}

/// Load the committed manifest.
pub fn manifest() -> generator::Manifest {
    let raw = std::fs::read_to_string(manifest_path())
        .expect("missing manifest.json — run the ignored bless_golden_dataset test first");
    generator::Manifest::from_json(&serde_json::from_str(&raw).expect("manifest.json parse"))
}

/// Run the real `deja` binary; returns (success, stdout, stderr).
pub fn deja(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(args)
        .output()
        .expect("spawn deja");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Run `deja` with the waiser clock pinned to `now_ms` (`WAISER_NOW_MS` — the
/// simulation seam) and the ambient waiser environment scrubbed, so a
/// developer's own `WAISER_POLICY` can never leak into golden output. Returns
/// (exit_code, stdout, stderr) — waiser's `--fail-on` gate makes exit codes
/// part of the contract, so the code is surfaced rather than collapsed to a
/// bool.
pub fn deja_at(now_ms: i64, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_deja"))
        .args(args)
        .env("WAISER_NOW_MS", now_ms.to_string())
        .env_remove("WAISER_POLICY")
        .env_remove("DEJADB_DB")
        .output()
        .expect("spawn deja");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// A private import of the golden bundle. Every test gets its own copy:
/// DejaDB enforces single-writer-per-file with an exclusive lock, and each
/// `deja` invocation is its own process, so tests sharing one file would
/// collide when cargo runs them in parallel. Import is ~50ms at this size.
pub struct GoldenDb {
    _dir: TempDir,
    pub db: String,
}

pub fn import_golden() -> GoldenDb {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("golden.db").to_str().unwrap().to_string();
    let (ok, _out, err) = deja(&[
        "import",
        "--db",
        &db,
        "--bundle",
        bundle_path().to_str().unwrap(),
    ]);
    assert!(ok, "golden bundle import failed: {err}");
    GoldenDb { _dir: dir, db }
}

impl GoldenDb {
    /// Run a CAL query against this import and parse the JSON payload.
    pub fn cal(&self, ns: &str, query: &str) -> serde_json::Value {
        let (ok, out, err) = deja(&["cal", query, "--db", &self.db, "--ns", ns]);
        assert!(ok, "cal failed for {query:?}: {err}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("cal output not JSON ({e}): {out}"))
    }
}

// --- waiser golden dataset ---------------------------------------------------

pub fn waiser_bundle_path() -> PathBuf {
    dataset_dir().join("waiser.bundle")
}

pub fn waiser_manifest_path() -> PathBuf {
    dataset_dir().join("waiser-manifest.json")
}

/// `tests/golden/dataset/waiser/` — committed golden outputs of the waiser
/// surfaces (run results, queue listings, show payloads, registry/policy pins).
pub fn waiser_golden_dir() -> PathBuf {
    dataset_dir().join("waiser")
}

/// Load the committed waiser manifest.
pub fn waiser_manifest() -> generator::Manifest {
    let raw = std::fs::read_to_string(waiser_manifest_path())
        .expect("missing waiser-manifest.json — run the ignored bless_waiser_golden_dataset test");
    generator::Manifest::from_json(&serde_json::from_str(&raw).expect("waiser-manifest.json parse"))
}

/// A private import of the waiser golden bundle (same isolation rationale as
/// `import_golden`).
pub fn import_waiser_golden() -> GoldenDb {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("waiser-golden.db").to_str().unwrap().to_string();
    let (ok, _out, err) = deja(&[
        "import",
        "--db",
        &db,
        "--bundle",
        waiser_bundle_path().to_str().unwrap(),
    ]);
    assert!(ok, "waiser golden bundle import failed: {err}");
    GoldenDb { _dir: dir, db }
}

/// Compare `actual` against the committed golden file, or rewrite it under
/// `GOLDEN_BLESS=1` — the same bless contract the render goldens use.
pub fn assert_golden(path: &std::path::Path, actual: &str) {
    if std::env::var("GOLDEN_BLESS").is_ok() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, actual).unwrap();
        eprintln!("blessed {}", path.display());
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("missing {} — bless with GOLDEN_BLESS=1", path.display()));
    assert_eq!(actual, expected, "output drifted from {}", path.display());
}

/// Hashes of a `grains`-typed CAL payload, insertion order preserved.
pub fn grain_hashes(payload: &serde_json::Value) -> Vec<String> {
    payload["grains"]
        .as_array()
        .expect("grains array")
        .iter()
        .map(|g| g["hash"].as_str().expect("hash").to_string())
        .collect()
}
