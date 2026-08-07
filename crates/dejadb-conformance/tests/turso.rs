//! Turso runner: every conformance case against the embedded file backend.
//! The case list itself lives in `for_each_conformance_case!` in the crate
//! root — one list, every backend, by construction.

macro_rules! turso_case {
    ($name:ident) => {
        #[test]
        fn $name() {
            dejadb_conformance::cases::$name(&dejadb_conformance::TursoBackend::new());
        }
    };
}

dejadb_conformance::for_each_conformance_case!(turso_case);
