# Erasure model — bulk deletion requirements

> **Status note (2026-08-10).** This document's original "OMS deviation"
> framing is retired: CAL 1.3 (spec draft `release/oms-v1.6`) brings both
> bulk operations into the language as authorization-gated Tier-2
> statements — `FORGET SUBJECT "<id>" [WITH text_mentions] BECAUSE "…"` and
> `PURGE OLDER THAN <n><d|h|m> [TYPE t] [IN "<ns>"] BECAUSE "…"` — gated by
> the session's `erase` grant and recorded as audit Observations. The
> host-level store/binding/CLI operations below remain, now as the same
> machinery the CAL statements call. The requirements are unchanged.

DejaDB's base removal model follows OMS: a single-grain tombstone (`FORGET
<hash>`) plus memory-level erasure (delete
the file / crypto-erase its key on the embedded backend, `DROP SCHEMA …
CASCADE` on the postgres backend). Regulated deployments — healthcare-class
privacy regimes with right-to-erasure and retention obligations (GDPR /
PDPL-family) — need two operations that sit BETWEEN those granularities.
This document is the requirement record for them.

## Requirements

- **REQ-ERASE-1 (right to erasure).** A host MUST be able to erase every
  grain holding a structured reference to one identity — the full
  supersession history, not just the live heads — including the identity's
  dictionary entry (the identifier string is itself erasable data), the
  erased grains' own value strings once unreferenced, vocabulary tokens
  that occurred only in the erased text, telemetry rows keyed on the
  identity string, and CAS attachments only those grains referenced.
  Result: `DejaDB::forget_subject(ns, subject)`, exposed as
  `forget_subject` / `forgetSubject` in the bindings and
  `deja forget-subject … --yes` in the CLI.
- **REQ-ERASE-2 (retention).** A host MUST be able to erase every grain
  older than a cutoff (`created_at`), optionally scoped to a namespace and
  grain type, suitable for a nightly sweep. Result:
  `DejaDB::forget_older_than(ns, cutoff_ms, grain_type)`, exposed as
  `forget_older_than` / `forgetOlderThan` and `deja purge-older-than <days>
  … --yes`.
- **REQ-ERASE-3 (replication).** Bulk erasure MUST reach replicas. Each
  erased grain gets its own ordinary op-log tombstone, so a bulk erasure
  replays over the existing bundle/sync machinery exactly like individual
  forgets — no new wire format, idempotent on replay.
- **REQ-ERASE-4 (atomicity under concurrency).** Enumeration and deletion
  run in one transaction after the multi-writer serialization point, so a
  concurrently-committed grain cannot slip between the census and the
  deletes, and a failure erases nothing.
- **REQ-ERASE-5 (auditability without re-identification).** The operation
  returns an `ErasureReport` (counts only: grains, dictionary entries,
  vocabulary tokens, blobs). It deliberately contains no identity material;
  the HOST decides what to log. The engine writes no audit grain of its own
  — an engine-written record naming the subject would re-introduce the
  reference being erased.
- **REQ-ERASE-6 (surface discipline).** Neither operation is reachable from
  CAL text. The CAL grammar — an OMS conformance contract — is unchanged:
  `FORGET USER`/`FORGET SCOPE`/`PURGE` remain refused by the parser, and the
  facade stubs (`cal_forget_user`, `cal_forget_scope`) remain unwired. Bulk
  erasure is a host-level library/CLI capability, gated by the host (the
  CLI demands an explicit `--yes` and honors `--no-destructive-ops`). An
  empty subject is refused outright — with prefix matching it would select
  everything, and an unset variable must never read as "erase all".
- **REQ-ERASE-7 (partition keys).** Hosts model composite records as
  identity-prefixed keys (`pat#visit1`, `pat:thread-2`). Identity matching
  MUST cover them: any dictionary term equal to the identity or starting
  with it followed by a non-alphanumeric separator, case-exactly and
  identically on every backend — and never a longer word (`patricia` is
  not `pat`). Applies to subject/object positions, sessions, and run ids
  alike, and the matched key strings are tombstoned with the identity.
- **REQ-ERASE-8 (search symmetry, opt-in).** Whatever the engine can FIND
  by the identity's tokens, it can ERASE by them: with `text_mentions`
  enabled, grains whose indexed text contains every token of the identity
  join the erasure set (the BM25 inverted index is the mechanism, so this
  needs text indexing on and fully built — requesting it with indexing off
  or a deferred/unrebuilt index is an error, not a silent partial erasure). Opt-in, never default: token matching
  over-reaches for identifiers that are ordinary words ("may", "mark"),
  and erasing every grain containing a common word is the wrong failure
  mode. For distinctive identifiers (contact codes, record numbers) it
  closes the prose-mention gap.

## The OMS deviation, stated plainly

OMS models removal as per-grain tombstones and treats grain immutability +
file-level erasure as the privacy story. **Subject-scoped and age-scoped
bulk erasure are DejaDB extensions that go beyond the spec**, added because
"can't delete" is a compliance problem, not a feature, for personal data.
The deviation is deliberately shaped to be conformance-neutral:

- Wire format, content addressing, and grain immutability are untouched —
  erasure is a batch of ordinary tombstones plus index/dictionary hygiene.
- The CAL surface is byte-identical to before (REQ-ERASE-6), so CAL-level
  OMS conformance claims are unaffected.
- A peer implementing plain OMS interoperates: it sees ordinary `OP_FORGET`
  records and converges (it will simply keep its own dictionary entries —
  dictionary hygiene is local).

## Scope contract (what "about a subject" means)

`forget_subject` erases grains found through **dictionary-indexed
references** in the namespace — for the identity itself AND every
partition-style key carrying it as a boundary-guarded prefix
(REQ-ERASE-7):

- triple **subject** position (the grain is about the identity),
- triple **object** position (the grain points at the identity —
  over-deletion is the safe direction for erasure),
- **thread events** whose session id is the identity (or an
  identity-prefixed key),
- **run records** whose run id is the identity (`run_trace` /
  `runs_touching` must go empty for an erased identity),
- and, with `text_mentions` enabled (REQ-ERASE-8), grains whose **indexed
  text** contains every token of the identity — search symmetry through
  the engine's own inverted index.

Dictionary hygiene rides the same transaction: every term the erased
grains touched (their subject/relation/object VALUES, session and run
ids) is **tombstoned** — the string replaced with an unrecallable
placeholder — once nothing references it. Tombstoning rather than
deleting is deliberate: under concurrent writers another instance may
hold the id in its cache, and a dangling id would silently corrupt its
next write, while a tombstoned id stays referentially valid and merely
makes the old name unfindable. `fts_vocab` tokens left with no postings
are removed, telemetry rows keyed on any matched identity string —
partition keys included — (query rollups, the recall ring log) are
scrubbed, and CAS attachments are reclaimed
**targeted** — only the erased grains' own references, checked for
surviving users under the write serialization, never a store-wide gc
that could race another writer's in-flight upload. (Residual window: a
concurrent upload of byte-identical content in the erasure instant; and
`gc_blobs`, the explicit full-store sweep, still requires quiescent
writers — both documented on the APIs.)

Residual limits, stated honestly: text-mention matching reaches exactly
what the index reaches — grains whose text was never indexed (`index_text`
off, or written before indexing) and identity forms the tokenizer splits
differently (a phone number renders as its digit runs) are matched only
through their structured references. `user_id` inside a grain body is not
an index; use subject/session/run for erasable identity scoping.
**Distinctive identifiers in structured fields remain the contract**;
partition keys and text mentions are the widening, not a substitute.

## Backend notes

- **Postgres**: rows are gone at commit; physical space follows autovacuum.
  Telemetry tables ride the same schema and are scrubbed per hash. Memory-
  level erasure is `drop_postgres_schema`.
- **Embedded (file)**: the same logical guarantees, with the standing
  physical-remnant caveat (`docs/security-model.md`): deleted pages can
  linger in the file/WAL until compaction, so **crypto-erasure remains the
  strong path** for whole-memory destruction; `forget_subject` is the
  surgical tool within a memory that keeps living.
- Erase-then-rewrite of the SAME identity from a different, still-open
  handle is outside the contract on the multi-writer backend (its cached
  dictionary id may be stale); route post-erasure writes through a fresh
  handle or the erasing instance.

## Testing

Conformance cases (both backends, one list): `subject_erasure_is_complete`
(history + object references + thread events + dictionary + vocabulary,
survivor untouched, text unfindable afterwards), `subject_erasure_replicates`
(tombstones reach a peer, idempotent replay), `retention_erases_only_older`
(exclusive cutoff, type scoping). Binding smokes drive both operations over
live Postgres.
