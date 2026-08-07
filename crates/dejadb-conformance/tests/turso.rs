//! Turso runner: every conformance case against the embedded file backend.
//! The Postgres runner (feature-gated, Phase 2) reuses this exact list.

use dejadb_conformance::{cases, TursoBackend};

macro_rules! conformance {
    ($($name:ident),* $(,)?) => {$(
        #[test]
        fn $name() { cases::$name(&TursoBackend::new()); }
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
);
