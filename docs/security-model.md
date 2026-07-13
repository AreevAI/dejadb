# DejaDB Security Model & Threat Model

This document describes DejaDB's trust boundaries, what its defenses do and do
not protect against, and how to deploy it safely. It complements
[SECURITY.md](../SECURITY.md) (which covers vulnerability reporting).

> This model is written to be **honest about current
> limitations** rather than aspirational. Where a protection is partial or
> planned, it says so.

## What we are protecting

The asset is **agent memory** — often personal, long-lived, and sensitive
(conversations, facts about people, decisions, credentials an agent was told).
The primary goals are **confidentiality** (at rest and in transit) and
**integrity** (a grain cannot be silently altered).

## Trust model at a glance

DejaDB is an **embedded** engine, like SQLite. Its baseline trust boundary is
**the local process and the user who runs it**. Everything below is layered on
top of that.

| Surface | Transport | Trust boundary | Auth |
|---|---|---|---|
| Library (`dejadb-*` crates) | in-process | the host program | n/a |
| CLI (`deja`) | local process | the invoking user | filesystem perms |
| MCP server (`serve --mcp`) | stdio | the parent process that spawned it | inherited |
| Web console (`deja ui`) | HTTP/1.1 | **loopback only by default** | none, or `--token-env` (Basic/Bearer on every request) |
| Sync hub (`dejad`) | HTTP/1.1 | networked peers | bearer token (writes + sync) |

## Data at rest

- **Encryption at rest** is optional and off by default. When enabled, the
  memory database (grains, indexes, op-log, and WAL) is encrypted with
  **AES-256-GCM** via the underlying storage engine's page cipher.
- **Key derivation.** The CLI derives the 32-byte key from a passphrase using
  **Argon2id** (OWASP-recommended parameters: 19 MiB memory, 2 iterations).
  The non-secret salt and parameters live in a `<db>.kdf` sidecar created on
  first use. Applications embedding the library may instead supply a raw
  32-byte key directly.
- **Key handling.** Passphrases and derived keys are wrapped in `Zeroizing`
  buffers and wiped from DejaDB's memory after use. (The passphrase is read
  from an environment variable via `--passphrase-env`, never a command-line
  argument, so it does not leak into shell history or the process table.)
- **Crypto-erasure.** Because the key is never written to the file, destroying
  the passphrase (and the derived key) renders the data unrecoverable — a fast,
  durable delete of an entire encrypted memory.

### Known limitations at rest

- ⚠️ **The `.blobs` CAS sidecar is NOT encrypted.** Large binary payloads
  stored via `put_blob` land in a plaintext sidecar directory even when the
  database is encrypted. Keep sensitive media out of blobs until blob
  encryption lands; the engine emits a runtime warning when encryption is on.
- ⚠️ **The encryption feature depends on the storage engine's *experimental*
  AES-GCM implementation** (a pinned Turso dependency). Treat encryption at
  rest as **defense-in-depth**, not a replacement for full-disk encryption on
  the host.
- ⚠️ **Losing the `.kdf` sidecar** means the passphrase can no longer re-derive
  the key. Back the sidecar up alongside the database.

## Data in transit (sync & hub)

- Sync ships **bundles/segments** (`.mgb`) of immutable grains between files and
  peers. Applied grains are re-hashed on import; a grain whose content does not
  match its content address (SHA-256) is rejected.
- The **hub** (`dejad`) requires a **bearer token** on all mutating and
  segment endpoints; the token is compared in **constant time**. Segment names
  are sanitized to a single path component (no directory traversal).
- The **web console** (`deja ui`) is unauthenticated by default (loopback,
  trusted local operator). Pass `--token-env <VAR>` to require a shared secret
  on **every** request — the console page, all reads, and all writes. Browsers
  authenticate through the native HTTP **Basic** prompt (any username; password
  = the token); scripts may send `Authorization: Bearer <token>`. The token is
  compared in constant time, and a `401` carries `WWW-Authenticate: Basic` so
  browsers prompt. Naming an env var (not a flag) keeps the secret out of argv
  and shell history.
- Import is **DoS-hardened**: an untrusted `.mg` blob is size-capped and its
  msgpack framing is validated iteratively before decoding, so a hostile grain
  cannot cause a stack overflow (deep nesting) or a giant pre-allocation (a
  short header claiming a huge length).
- The HTTP server bounds per-connection bytes, caps header size/count, and sets
  read/write timeouts (slowloris mitigation).

### Known limitations in transit

- ⚠️ **No TLS.** All HTTP is plaintext. For any non-loopback deployment, front
  the console/hub with a **TLS-terminating reverse proxy**. The `deja ui`
  console refuses to bind a non-loopback address unless you pass
  `--allow-remote` (and even then warns loudly). `--token-env` authentication
  is **not** a substitute for TLS: the token and all memory still cross the
  wire in the clear, so `--token-env` guards against unauthorized clients but
  not against a network eavesdropper — use it *with* a TLS proxy off-loopback.
- ⚠️ **Integrity, not authenticity.** Content addressing detects corruption and
  tampering, but does **not** verify *who* authored a grain. There is dormant
  scaffolding for COSE signing, but signature verification is not yet enforced
  on import. **Only sync with peers you trust.**

## Input handling

- **CAL** (the query language) has a single, gated destructive verb —
  `FORGET <hash>` (a one-grain tombstone), controlled by the executor's
  `allow_destructive_ops` switch (default on; disable per-process with
  `--no-destructive-ops` for a read-only session, e.g. when serving untrusted
  input over MCP). `DELETE`/`ERASE`/`TRUNCATE`/… are not grammar tokens, there
  is no bulk/namespace erasure from a query, and the server path requires the
  `admin` scope for FORGET. CAL is otherwise hardened against abuse (max query
  length, nesting depth, LET-binding and result-size caps, Unicode bidi-override
  rejection, NFC normalization).
- The store issues **parameterized SQL** exclusively; user strings are
  dictionary-encoded to integer term-ids before reaching the triple queries, so
  there is no SQL-injection surface.
- The **web console** escapes grain-controlled data before rendering it, so a
  synced grain carrying HTML/JS markup is inert in the UI.

## Threats in scope (please report)

- Memory-safety, panics, or resource exhaustion reachable from untrusted `.mg`
  blobs, bundles, or imported segments.
- Injection, path traversal, or auth bypass in CAL, the store, the MCP server,
  or the console/hub.
- Cryptographic weaknesses in the encryption or crypto-erasure paths.
- Secret or data leakage in error messages, logs, or `Debug` output.

## Threats out of scope

- An already-compromised host, physical access, or a malicious local process
  running with the same privileges as DejaDB.
- Confidentiality of the plaintext `.blobs` sidecar (documented limitation).
- Network confidentiality without an operator-provided TLS proxy (by design).
- Forged grain provenance when syncing with an untrusted peer (integrity is
  guaranteed; authenticity is not, until signing lands).

## Roadmap

- Blob (`.blobs`) encryption at rest.
- Enforced grain signing / authenticity verification on import (COSE).
- First-class TLS for the hub.

If you find something that contradicts this document, that is itself worth
reporting — see [SECURITY.md](../SECURITY.md).
