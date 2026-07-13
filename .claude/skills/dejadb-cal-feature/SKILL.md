---
name: dejadb-cal-feature
description: Playbook for extending CAL (Context Assembly Language) in dejadb-cal — the ordered chain of files to touch, the OMS-conformance gate on new syntax, preserving the three-layer destructive-op block, and the Ok(Unsupported) result convention. Use before adding a CAL keyword, statement, WITH option, pipeline stage, or format clause, or editing lexer.rs / parser.rs / executor.rs.
---

# Extending CAL

`dejadb-cal` is ~30k lines; `executor.rs` (~10k) and `parser.rs` (~9.4k) are the
two biggest files in the repo — **navigate with grep + offset reads, never full
reads**. CAL is an OMS conformance contract, and its destructive surface is
narrow and gated (only `FORGET <hash>`, off-switchable); both are load-bearing.

## GATE 1 — is this new *syntax*?

New CAL **syntax** (a keyword, statement form, operator) is an **OMS
conformance decision**, not a local change. **STOP** and confirm it is spec-level
approved before writing a token. Adding behavior behind *existing* syntax — a new
`WITH` option value, a new pipeline behavior, a new FORMAT target — is the
normal, safe case and does not need a spec change.

## GATE 2 — keep the destructive surface narrow and gated

The only destructive CAL statement is `FORGET <hash>` (single-grain tombstone),
gated at execution by `CalExecutorConfig::allow_destructive_ops` (default on;
`--no-destructive-ops` flips it off). Preserve the surrounding blocks:
1. **Lexer** (`lexer.rs`): `is_destructive_keyword` hard-blocks 26 words
   (DELETE/ERASE/INSERT/CREATE/GRANT/…); DELETE has no token at all.
2. **Parser** (`parser.rs`): `parse_statement` fast-rejects those idents with
   CAL-E002; `FORGET <hash>` parses (`parse_forget`), but `FORGET USER/SCOPE`
   and `PURGE` stay refused from text.
3. **DROP** is a token but `parse_drop` accepts only DROP TEMPLATE/QUERY.
   Saved-query bodies get an extra `check_statement_read_only` pass.
Do **not** widen this (bulk PURGE from text, user/scope erasure, new destructive
verbs) without a design + OMS-conformance decision — that's a GATE 1 change.
Also preserve the lexer security invariants: **S-1** bidi-control rejection and
**S-6** NFC normalization, both before tokenization.

## Touch in this order

1. **`lexer.rs`** — the token (Logos DFA). Not a destructive word.
2. **`ast.rs`** — the `CalStatement`/`PipelineStage`/`WithOption`/`Condition` variant.
3. **`parser.rs`** — the parse fn + dispatch. Respect the const limits at the top
   (~line 52: MAX_QUERY_LENGTH 64KB, MAX_NESTING_DEPTH 8, MAX_LIMIT 1000,
   MAX_PIPELINE_STAGES 5). Many keywords double as field names via `is_word_token`
   (ON/WHEN/PRIORITY/SCOPE) — extensive tests guard this, keep them green.
4. **`executor.rs`** — payload variant + match arm + executor fn. **Two entry
   points must stay in sync**: `execute` (text) and `execute_parsed` (JSON-CAL
   AST) duplicate the LET → pipeline → format sequence — edit both.
5. **`errors.rs`** — a new `CAL-Exxx`. Codes live **inside the `#[error]`
   Display string** (E001–E019 parse, E020–E022 type, E030+ exec); pick the next
   free number in the range and add a row to `ERROR_CODES.md`. Append-only.
6. **`facade.rs`** (object-safe `CalStoreFacade` trait) + **`dejadb_facade.rs`**
   (concrete `DejaDbFacade`) — only if the feature needs new store access.
   Tier-2 destructive methods default to `Err`; keep it that way.
7. **`json.rs` / `json_build.rs`** — the JSON-CAL wire form for the new variant.
8. **`store_types.rs`** — if the store contract grows (`RecallParams`,
   `SearchHit`, `AddOptions`, …); facade methods speak only these types.
9. **`CalCapabilities::default`** — add to the supported-statements list so
   DESCRIBE reports it.

## Gotcha — Unsupported is returned as Ok

Tier-1 runtime failures (bad grain type, unresolved param) come back as
`CalResultPayload::Unsupported` inside an **`Ok`**, not an `Err`. Check the
payload, not just Ok/Err. FORGET/PURGE/REVERT exist in the AST/facade but are
unreachable from text (REVERT always returns Unsupported) — AST coverage ≠
reachable surface.

## Verify

```bash
cargo test -p dejadb-cal
cargo +nightly fuzz run cal_parse         # if you touched lexer/parser
```

- `tests/cal_integration.rs` runs text → executor → facade → real store,
  including destructive-reject assertions — add cases there.
- Keep `test_error_codes_match_display` / `test_all_error_codes_have_unique_codes`
  green if you added a code.
- Then run the **dejadb-invariants** gate before committing.
