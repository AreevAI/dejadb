//! # dejadb-loop
//!
//! The DejaDB substrate adapter for the [`deja_loop`] engine. It implements
//! [`deja_loop::OmsSubstrate`] over [`dejadb_cal::DejaDbFacade`], so the governed
//! self-improvement loop runs against real DejaDB `.mg`/Turso memory files.
//!
//! ```no_run
//! use dejadb_loop::{DejaDbSubstrate, now_ms};
//! use dejadb_store::DejaDB;
//! use deja_loop::{Engine, RunOptions};
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

/// Map a session's grants onto the loop engine's scope set — the one
/// translation between DejaDB's verb model (`dejadb_core::authz`) and
/// `deja_loop::Scope`, so surfaces stop handing out `ScopeSet::all()`
/// unconditionally. Loop verbs are checked against the loop's own namespace
/// (`deja_loop::LOOP_NS`), which a grant covers by naming it or `*`.
/// An owner session maps to every scope — the CLI's local-root-of-trust
/// behavior, unchanged.
pub fn scopes_for(authz: &dejadb_core::authz::AuthzSet) -> deja_loop::ScopeSet {
    use deja_loop::Scope;
    use deja_loop::LOOP_NS;
    use dejadb_core::authz::Verb;
    let mut scopes = Vec::new();
    for (verb, scope) in [
        (Verb::Read, Scope::Read),
        (Verb::Write, Scope::Write),
        (Verb::LoopReview, Scope::Review),
        (Verb::LoopApply, Scope::Apply),
        (Verb::Admin, Scope::Admin),
    ] {
        if authz.allows(verb, LOOP_NS) {
            scopes.push(scope);
        }
    }
    deja_loop::ScopeSet::of(&scopes)
}

/// The observer type an actor label implies, used where no credential record
/// declares one (the credential map will carry an explicit `observer` field
/// when the multi-token surfaces land; this prefix heuristic is the interim
/// derivation — a *statement* must never be able to claim humanity, so the
/// answer always comes from the host-held actor label, not request text).
pub fn observer_for_principal(actor: &str) -> deja_loop::ObserverType {
    for prefix in ["agent:", "bot:", "job:", "svc:", "engine:"] {
        if actor.starts_with(prefix) {
            return deja_loop::ObserverType::Agent;
        }
    }
    deja_loop::ObserverType::Human
}

/// Wall-clock now in epoch milliseconds — the `now_ms` the engine's `run`,
/// `review`, `apply`, and `rollback` take. Kept out of `deja-loop` itself so the
/// engine stays deterministic (the caller supplies the clock).
///
/// `DEJA_LOOP_NOW_MS` (epoch ms) overrides the wall clock — the simulation seam
/// that makes a run through the real binary a pure function of (file, policy,
/// time). The golden E2E suite uses it to pin analyzer output and to step time
/// across outcome-review horizons and rejection cooldowns without sleeping.
/// A set-but-unparseable value panics: the caller asked for simulated time,
/// and silently running at wall time instead would defeat the point.
pub fn now_ms() -> i64 {
    if let Ok(v) = std::env::var("DEJA_LOOP_NOW_MS") {
        return v
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("DEJA_LOOP_NOW_MS is set but not epoch milliseconds: {v:?}"));
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod authz_mapping_tests {
    use super::*;
    use dejadb_core::authz::{AuthzSet, Grant, Verb};

    #[test]
    fn owner_maps_to_every_scope() {
        let scopes = scopes_for(&AuthzSet::owner("user:local"));
        for s in [
            deja_loop::Scope::Read,
            deja_loop::Scope::Write,
            deja_loop::Scope::Review,
            deja_loop::Scope::Apply,
            deja_loop::Scope::Admin,
        ] {
            assert!(scopes.has(s), "{s:?} missing for owner");
        }
    }

    #[test]
    fn loop_verbs_map_one_to_one_and_namespace_scoping_holds() {
        // A reviewer granted on `*` gets exactly Review (plus nothing else).
        let reviewer = AuthzSet::restricted(
            "user:rev",
            vec![Grant { verbs: vec![Verb::LoopReview], namespaces: vec!["*".into()] }],
        );
        let scopes = scopes_for(&reviewer);
        assert!(scopes.has(deja_loop::Scope::Review));
        for s in [
            deja_loop::Scope::Read,
            deja_loop::Scope::Write,
            deja_loop::Scope::Apply,
            deja_loop::Scope::Admin,
        ] {
            assert!(!scopes.has(s), "{s:?} must not be granted");
        }

        // Loop verbs are checked against the loop's own namespace — a grant
        // scoped to some data namespace does not reach the review queue.
        let elsewhere = AuthzSet::restricted(
            "user:misscoped",
            vec![Grant { verbs: vec![Verb::LoopReview], namespaces: vec!["caller".into()] }],
        );
        assert!(!scopes_for(&elsewhere).has(deja_loop::Scope::Review));
        let on_loop_ns = AuthzSet::restricted(
            "user:scoped",
            vec![Grant {
                verbs: vec![Verb::LoopReview],
                namespaces: vec![deja_loop::LOOP_NS.into()],
            }],
        );
        assert!(scopes_for(&on_loop_ns).has(deja_loop::Scope::Review));
    }

    #[test]
    fn observer_derives_from_the_actor_label() {
        use deja_loop::ObserverType;
        assert_eq!(observer_for_principal("agent:mcp"), ObserverType::Agent);
        assert_eq!(observer_for_principal("bot:sweeper"), ObserverType::Agent);
        assert_eq!(observer_for_principal("job:retention"), ObserverType::Agent);
        assert_eq!(observer_for_principal("engine:loop.llm/1"), ObserverType::Agent);
        assert_eq!(observer_for_principal("user:anna"), ObserverType::Human);
        assert_eq!(observer_for_principal("anna"), ObserverType::Human);
    }
}
