# Golden dataset tests

Deterministic integration tests: import a committed, known dataset and
validate the *exact* data DejaDB produces — content hashes, recall sets and
ordering, CAL payloads, render text, cross-surface parity. Modeled on
`areev/tests/golden/`.

## Why golden tests (vs the existing suites)

| Existing tests | Golden dataset tests |
|---|---|
| Create grains inline per test | Import one committed dataset with known hashes |
| "did I get results?" | "did I get the RIGHT grains, in the RIGHT order?" |
| One surface at a time | Same assertion across CLI and MCP |
| Ephemeral values | Frozen content addresses — the OMS conformance canary |

## Layout

```
tests/
├── golden_tests.rs        ← 33 tests + 5 known-bug regressions + bless
└── golden/
    ├── mod.rs             ← paths, `deja` runner, per-test bundle import
    ├── generator.rs       ← builds the dataset (pinned epoch 2026-01-15)
    └── dataset/
        ├── golden.bundle  ← committed export (39 grains incl. 1 forgotten)
        ├── manifest.json  ← per-grain hash + type + ns + description
        └── renders/       ← golden render text: recall (4 formats) +
                             ASSEMBLE grid (sml/toon/markdown) + budgeted
```

## Running

```bash
cargo test -p dejadb-cli --test golden_tests
```

Each test imports its own copy of the bundle into a temp dir — DejaDB is
single-writer-per-file and every `deja` call is its own process, so tests
cannot share one memory file under parallel execution.

## The dataset (39 grains, base epoch 2026-01-15 UTC)

| Slice | ns | Purpose |
|---|---|---|
| 10 john facts | personal | entity recall, relation filters, renders |
| 8 bob facts | work | namespace isolation, cross-ns ASSEMBLE |
| 1 unicode fact (rené/café münchen) | personal | NFC canonicalization anchor |
| 10 events (2 sessions × 5) | shared | thread indexing, seeded BM25 tokens |
| 2 goals | work | non-triple grain type |
| kim status ×3 (supersession chain) | personal | HISTORY, head-only recall |
| dave/erin/fay drinks + acme industry | personal | WITH-option targets (dedup, graph hop) |
| 1 forgotten grain | work | tombstone survives export/import |

Every `created_at` is a fixed offset from the base epoch — no `now()`
anywhere — so every content hash is reproducible on any machine.

## Semantics the suite pins (learned, not assumed)

- Recall ordering is **insertion recency (op_seq desc)**, not created_at.
- A forgotten grain ships in the bundle as a zero-length blob and does
  **not** materialize as a row on import (38 rows from 39 generated).
- NFC: composed and decomposed spellings of the same text produce the
  **same** content address.
- Clause order matters: `LIMIT` before `WITH`, `BUDGET` before `FORMAT`.
- ASSEMBLE `FORMAT json` returns a `grains` payload, not rendered text.

## Known bugs found by combination probing (Suite 9, #[ignore]d)

Each has an ignored regression test asserting the *correct* behavior —
un-ignore it as part of the fix:

1. Pipeline stages (`COUNT`) and `LIMIT` written **after a WITH clause**
   are silently dropped (EXPLAIN confirms they vanish from the plan).
2. `BUDGET` written **after FORMAT** in ASSEMBLE is silently dropped
   (works in the documented BUDGET-then-FORMAT order).
3. `WITH superseded` is a silent no-op on the structural recall path
   (executor maps it to `exclude_superseded=false`; the store leg ignores it).
4. `OR` across subject equalities silently returns only the first subject.

## Changing the dataset

1. Edit `generator.rs` (keep every timestamp pinned).
2. `cargo test -p dejadb-cli --test golden_tests -- --ignored bless`
3. `GOLDEN_BLESS=1 cargo test -p dejadb-cli --test golden_tests render`
4. Review and commit the diff in `dataset/` — the diff IS the review.

If `golden_manifest_hashes_stable` fails **without** a dataset edit,
canonical serialization changed — that is a frozen-format / OMS conformance
break (root CLAUDE.md invariant #2), not a test to appease.
