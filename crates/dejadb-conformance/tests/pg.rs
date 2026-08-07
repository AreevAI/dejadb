//! Postgres runner: the IDENTICAL conformance case list against a
//! schema-per-test Postgres backend, plus the Pg-only single-writer test.
//!
//! Needs a reachable server (pgvector image recommended):
//!   docker run --rm -d -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16
//!   export DATABASE_URL=postgres://postgres:postgres@127.0.0.1:5432/postgres
//! Skips (loudly) without DATABASE_URL/DEJADB_PG_URL — except under CI=true,
//! where a missing database is a hard failure so a broken job can never look
//! like a skipped one.
#![cfg(feature = "postgres")]

use dejadb_conformance::{cases, Backend, PgBackend};

fn pg_url() -> Option<String> {
    std::env::var("DEJADB_PG_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
        .filter(|u| u.starts_with("postgres"))
}

fn backend() -> Option<PgBackend> {
    match pg_url() {
        Some(url) => Some(PgBackend::new(&url)),
        None => {
            if std::env::var("CI").as_deref() == Ok("true") {
                panic!(
                    "CI=true but no DATABASE_URL — the postgres job must not silently skip"
                );
            }
            eprintln!(
                "skipping: no DATABASE_URL/DEJADB_PG_URL (start one: docker run --rm -d \
                 -p 5432:5432 -e POSTGRES_PASSWORD=postgres pgvector/pgvector:pg16)"
            );
            None
        }
    }
}

macro_rules! conformance {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() {
            let Some(b) = backend() else { return };
            cases::$name(&b);
        }
    )*};
}

conformance!(
    // add / recall / reopen
    add_recall_roundtrip,
    unknown_terms_short_circuit_empty,
    recall_orders_newest_first_and_honors_k,
    reopen_preserves_state_and_counters,
    // supersede × forget
    supersede_returns_only_head,
    forget_clears_head_row,
    forget_new_head_does_not_resurrect_old,
    forget_superseded_old_keeps_new_head,
    double_supersede_is_a_local_conflict,
    forget_missing_grain_is_not_found,
    add_if_novel_dedupes_current_value,
    reasserting_superseded_value_is_novel,
    // heads / forks / merge
    concurrent_supersede_forks_then_merges,
    provisional_head_election_is_deterministic,
    same_supersede_replay_stays_idempotent,
    open_forks_enumerates_and_clears_on_merge,
    fork_then_forget_one_tip_resolves_fork,
    merge_requires_an_open_fork,
    forget_tip_reelection_ignores_link_rows,
    supersede_changed_key_reelection_ignores_link_rows,
    supersede_changed_relation_reconciles_old_key,
    // oplog / bundles / PITR
    supersede_two_hop_replication_converges,
    merge_replicates_as_fork_closure,
    merge_heads_closure_logged,
    forget_replicates_as_tombstone,
    changes_since_cursor_pages_in_order,
    pitr_max_hlc_cutoff_is_inclusive,
    // CAS blobs + hybrid legs
    cas_blob_roundtrip_and_gc,
    bm25_leg_finds_text,
    vector_leg_roundtrip,
);

/// Single-writer-per-memory is ENFORCED on this backend: a second open of
/// the same schema while a writer holds it is a clean STO-E002, not the
/// undefined behavior two file handles produce.
#[test]
fn second_writer_gets_store_busy() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let first = b.open_named("busy");
    let err = match dejadb_store::DejaDB::open_postgres(&url, &b.schema_for("busy")) {
        Err(e) => e,
        Ok(_) => panic!("second writer must be refused"),
    };
    let msg = err.to_string();
    assert!(msg.starts_with("STO-E002"), "expected STO-E002, got: {msg}");
    drop(first);
    // lock released with the first handle — the schema opens again
    let _again = b.open_named("busy");
}

/// File-backend capabilities are refused with clear errors, not ignored.
#[test]
fn file_only_capabilities_are_rejected() {
    let Some(b) = backend() else { return };
    let url = pg_url().unwrap();
    let opts = dejadb_store::DejaDbOptions {
        encryption_key: Some([7u8; 32]),
        ..Default::default()
    };
    let err = match dejadb_store::DejaDB::open_postgres_with(&url, &b.schema_for("caps"), opts) {
        Err(e) => e,
        Ok(_) => panic!("encryption_key must be rejected on the postgres backend"),
    };
    assert!(err.to_string().contains("file-backend capability"), "{err}");
}
