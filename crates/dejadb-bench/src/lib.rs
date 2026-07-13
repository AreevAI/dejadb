//! dejadb-bench — adoption benchmark harnesses.
//!
//! Shared helpers: deterministic dataset generation and percentile
//! reporting. The binaries are the benchmarks:
//!   frame_chart — recall latency across real surfaces vs the 50ms
//!                 audio-frame budget (in-process / HTTP / MCP stdio)
//!   trust_suite — durability + integrity artifacts (kill -9 recovery,
//!                 tamper detection, deletion-remnant scan, PITR restore)

use dejadb_core::types::{Fact, Grain};
use dejadb_store::{AddableDyn, DejaDB};

pub const RELS: [&str; 6] = ["prefers", "lives_in", "speaks", "allergic_to", "reports_to", "status"];

/// Deterministic xorshift so every engine/surface sees the identical
/// dataset and query workload.
pub struct Xorshift(pub u64);
impl Xorshift {
    #[allow(clippy::should_implement_trait)] // deterministic RNG step, not an Iterator
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The bench.rs dataset shape: `n` facts over `subjects` callers.
pub fn gen_facts(rng: &mut Xorshift, n: usize, subjects: u64) -> Vec<(String, &'static str, String)> {
    (0..n)
        .map(|_| {
            let s = format!("caller:{:04}", rng.next() % subjects);
            let r = RELS[(rng.next() % RELS.len() as u64) as usize];
            let o = format!("value-{:05}", rng.next() % 5000);
            (s, r, o)
        })
        .collect()
}

/// Load facts into a store in 500-grain batches under namespace `main`.
pub fn load_facts(m: &mut DejaDB, facts: &[(String, &'static str, String)]) {
    let mut batch: Vec<Fact> = Vec::new();
    for (s, r, o) in facts {
        let mut f = Fact::new(s, r, o).confidence(0.9);
        f.common.namespace = Some("main".to_string());
        batch.push(f);
        if batch.len() == 500 {
            let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
            m.add_batch(&refs).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let refs: Vec<&dyn AddableDyn> = batch.iter().map(|f| f as &dyn AddableDyn).collect();
        m.add_batch(&refs).unwrap();
    }
}

pub struct Pcts {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// Sort + report p50/p95/p99 in µs; prints a markdown row.
pub fn pct(mut ns: Vec<u128>, name: &str) -> Pcts {
    ns.sort_unstable();
    let n = ns.len().max(1);
    let pick = |q: f64| ns[((n as f64 * q) as usize).min(n - 1)] as f64 / 1000.0;
    let p = Pcts { p50: pick(0.5), p95: pick(0.95), p99: pick(0.99) };
    println!("| {name} | {:.1} | {:.1} | {:.1} |", p.p50, p.p95, p.p99);
    p
}

/// Locate the release `deja` binary for surface benches that drive the
/// real CLI (honors CARGO_TARGET_DIR, falls back to the workspace target).
pub fn dejadb_bin() -> std::path::PathBuf {
    let target = std::env::var("CARGO_TARGET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target")
        });
    target.join("release/deja")
}
