---
name: dejadb-server-console
description: Playbook for dejadb-server — the hand-rolled std-only HTTP/1.1 server, its three auth modes (loopback-unauthenticated ui / with_auth token / into_hub bearer), the drive-by/body-cap/Origin security invariants, and the embedded console.html (whose design source is the Paper file "DejaDB"). Use before editing crates/dejadb-server/src/{lib.rs,console.html} or adding an endpoint, and always re-read docs/security-model.md when touching auth, bind, or the request surface.
---

# The server, hub & console

`dejadb-server` is a **hand-rolled, std-only** HTTP/1.1 server —
`std::net::TcpListener`, **one request per connection**, no framework, no async
runtime (invariant 6: dependency-light). It serves the web console and the
`dejad` sync hub. The console is a single embedded file:
`const CONSOLE_HTML = include_str!("console.html")` (`lib.rs:17`).

## The three modes (auth is the load-bearing distinction)

- **`ui` (default)** — binds **loopback** and is **unauthenticated**. Fine for a
  local console; never expose it off-host.
- **`with_auth(token)`** (`lib.rs:103`; CLI `deja ui --token-env VAR`) — requires
  the token on **every** request. Browsers via the native HTTP Basic prompt (any
  username, password = token); scripts via `Authorization: Bearer`. A 401 must
  carry `WWW-Authenticate: Basic` so browsers prompt. Base64 for Basic is
  **hand-rolled** (no dep) — keep it correct.
- **`into_hub(token, dir)`** (`lib.rs:123`) — the separate `dejad` hub: **bearer
  auth on POSTs** + the `/api/segment*` surface only (reads stay open) for
  segment push/pull. `into_hub` is not `ui` with auth — different route set.

## Security invariants — do not regress

- **Body cap 1 MiB** — reject larger bodies before buffering.
- **Origin check** — cross-origin **POSTs** are rejected (drive-by protection);
  a browser on another site must not be able to mutate a loopback console.
- **Read-only `GET /api/config`** reports effective config + file-vs-host
  reconciliation warnings — keep it read-only.
- One request per connection; parse defensively (this is an untrusted-input
  surface — see the fuzz/robustness posture in [[dejadb-invariants]]).
- Any change to auth, bind address, or the request parser → **re-read
  `docs/security-model.md`** and keep it accurate.

## Adding an endpoint

1. Route it in `handle_request` (`lib.rs:175`) — match method + path.
2. Enforce the mode's auth **first** (a new hub POST needs bearer; a new console
   route needs the `with_auth` check when a token is set).
3. Apply the body cap + Origin check for any POST.
4. Return proper status + headers (401 carries `WWW-Authenticate: Basic`).
5. If the endpoint reflects store state as JSON, follow the store contract; a
   new *operation* (not just a read) fans out via [[dejadb-add-operation]].

## The console (console.html)

One embedded HTML file, **vanilla JS**, no build step, no external assets
(dependency-light): memories / graph / query tabs, light + dark themes, a JSON
tree viewer, and a grain inspector. **Design source of truth is the Paper file
"DejaDB"** — reproduce visual changes there (or read exact values from it via
the Paper tools) rather than eyeballing; keep the embedded file and the Paper
design in sync.

## The gate — before you commit

```bash
cargo test -p dejadb-server
```
- `tests/http_smoke.rs` — the request/response surface + auth behavior.
- `tests/multichannel_tests.rs` — the **§8 acceptance test**: voice + WhatsApp +
  email sharing one memory through the hub (push/pull). Keep it green; it is the
  end-to-end proof the hub replicates correctly.

Then run the [[dejadb-invariants]] gate. If you touched the request parser or
auth, treat it as a security-sensitive change and review accordingly.
