//! Shared plumbing for the golden dataset tests: committed-file paths, the
//! `deja` subprocess helper, and the lazily imported golden memory that all
//! read-only tests share.

pub mod generator;

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

/// Hashes of a `grains`-typed CAL payload, insertion order preserved.
pub fn grain_hashes(payload: &serde_json::Value) -> Vec<String> {
    payload["grains"]
        .as_array()
        .expect("grains array")
        .iter()
        .map(|g| g["hash"].as_str().expect("hash").to_string())
        .collect()
}
