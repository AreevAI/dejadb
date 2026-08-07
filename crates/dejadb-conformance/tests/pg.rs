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

// The case list lives in `for_each_conformance_case!` in the crate root —
// one list, every backend, by construction.
macro_rules! pg_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            let Some(b) = backend() else { return };
            cases::$name(&b);
        }
    };
}

dejadb_conformance::for_each_conformance_case!(pg_case);

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
