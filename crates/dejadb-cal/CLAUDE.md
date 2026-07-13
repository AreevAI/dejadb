# dejadb-cal

CAL ("Context Assembly Language") — lexer, parser, executor, multi-source
ASSEMBLE, templates, saved queries, and the `DejaDbFacade` that binds it all
to `dejadb-store` (~30k lines). CAL syntax is an OMS conformance contract —
**do not invent new CAL syntax** without a spec-level decision.

`executor.rs` (~10k lines) and `parser.rs` (~9.4k lines) are the two biggest
files in the repo — navigate with grep and offset reads, never full reads.

## Pipeline

text → `parse()` (parser.rs:91): length check → bidi rejection → NFC → lex →
recursive-descent parse → `CalQuery` AST → `CalExecutor::execute`
(executor.rs): LET eval → `execute_statement` (big match) → `apply_pipeline`
→ `apply_format_clause` → `CalResultPayload`.

Two entry points must stay in sync: `execute` (text) and `execute_parsed`
(JSON-CAL AST) duplicate the LET/pipeline/format sequence.

## The safety pillar: destruction is narrow and gated

The only destructive CAL statement is `FORGET <hash>` (a single-grain
tombstone → `DejaDB::forget`). Everything else stays blocked:
1. **Lexer**: `is_destructive_keyword` (lexer.rs) hard-blocks 26 words
   (DELETE, ERASE, INSERT, CREATE, GRANT, …) — DELETE has no token at all.
2. **Parser**: `parse_statement` fast-rejects those idents with CAL-E002.
   `FORGET <hash>` parses (`parse_forget`); `FORGET USER/SCOPE` and `PURGE` are
   AST/token forms the text parser still refuses. `DROP` accepts only
   TEMPLATE/QUERY.
3. **Execution gate**: FORGET/DROP/PURGE run only when
   `CalExecutorConfig::allow_destructive_ops` is set (**default true**; the CLI
   `--no-destructive-ops` flag flips it off). When off they return
   `Ok(Unsupported)`. When scopes are enforced (server path), FORGET also
   requires the `admin` scope.
Saved-query bodies get an extra `check_statement_read_only` pass (FORGET/PURGE
are refused there regardless of the gate). Only the `Hash` FORGET target is
store-backed; `User`/`Scope` erasure is an unimplemented stub.

Security invariants in the lexer: **S-1** bidi-control rejection
(`check_bidi`, U+202A–202E / U+2066–2069) and **S-6** NFC normalization —
both run before tokenization; `compute_query_hash` NFC-normalizes again for
the audit hash.

## Module map

- `lexer.rs` — Logos DFA, S-1/S-6, destructive-keyword list.
- `ast.rs` — `CalStatement` (22 variants), `PipelineStage`, `Condition`,
  `WithOption` (~35 recall flags), FORMAT clause.
- `parser.rs` — hand-written recursive descent. Hard limits are consts at the
  top (~line 52): MAX_QUERY_LENGTH 64KB, MAX_NESTING_DEPTH 8, MAX_LIMIT 1000,
  MAX_PIPELINE_STAGES 5. Condition precedence via layered fns
  (`parse_condition_or` → `_and` → `_unary` → `_primary`).
- `executor.rs` — `CalExecutor`, per-statement executors (`execute_recall`,
  `execute_assemble`, …), pipeline + format application.
- `facade.rs` — `CalStoreFacade` **trait** (object-safe): the executor's only
  store access. Tier-2 destructive methods default to Err.
- `dejadb_facade.rs` — concrete `DejaDbFacade` over `dejadb_store::DejaDB`
  (Mutex-wrapped). `with_session(store, ns, user)` = session scoping.
  **Read-only mounts**: `mount(alias, store)`; `recall` routes
  `"alias.inner"` namespaces to the mount — writes only ever hit the session
  store, so mounts are read-only by construction.
- `assemble.rs` — `AssembleEngine`: multi-source ASSEMBLE, dedup, 2000-grain
  cap, per-source budget weights, chars/4 token estimate.
- `templates.rs` — Mustache-subset engine (closed variable set, 10 filters,
  F1–F7 security invariants, 1MB output cap). `queries.rs` — saved queries
  (100/namespace, 8KB body cap).
- `store_types.rs` — the dejadb-store contract: `RecallParams`, `SearchHit`,
  `AddOptions`, etc. Facade methods speak exclusively in these types.
- `errors.rs` — `CalError` (thiserror); **CAL-Exxx codes live inside the
  `#[error]` display strings**, not a separate code fn. E001–E019 parse,
  E020–E022 type, E030+ exec.

## Adding a language feature (touch in this order)

lexer.rs (token) → ast.rs (variant) → parser.rs (parse fn + dispatch) →
executor.rs (payload variant + match arm + executor fn) → errors.rs (new
CAL-Exxx) → facade.rs trait + dejadb_facade.rs impl (if store access) →
json.rs (wire form) → store_types.rs (if the store contract grows) → tests →
`CalCapabilities::default` supported_statements list.

## Gotchas

- `CalResultPayload::Unsupported` is returned as **Ok** for Tier-1 runtime
  failures (bad grain type, unresolved param) — check the payload, not just
  Ok/Err.
- FORGET/PURGE/REVERT exist in the AST/facade/executor but are unreachable
  from text (parser rejects; REVERT always returns Unsupported). AST coverage
  ≠ reachable surface.
- ADD requires a `REASON`/`BECAUSE` clause (missing → CAL-E018) and uses
  repeated `SET field = value`.
- Many keywords double as field names (ON, WHEN, PRIORITY, SCOPE) via
  `is_word_token` — extensive tests guard this; keep them green.
- The `cal` cargo feature is default-on and always enabled here (gates
  alias normalization + DESCRIBE capability listing).

## Tests

`cargo test -p dejadb-cal` (~700 inline unit tests in parser/executor/lexer/
assemble). `tests/cal_integration.rs` = text → executor → facade → real store
end-to-end incl. destructive-reject; `tests/assemble_mount_tests.rs` =
multi-source ASSEMBLE across a mounted org replica.
