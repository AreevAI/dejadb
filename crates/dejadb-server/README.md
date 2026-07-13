# dejadb-server

Web console and sync-hub server for DejaDB.

`dejadb-server` is the opt-in HTTP surface for DejaDB. It powers the local
inspection console (`deja ui`) — a JSON API plus a single embedded HTML page
for browsing memories, exploring the graph, and running queries — built on a
deliberately minimal std-only HTTP/1.1 server that binds loopback with no auth.
The same crate provides the sync-hub mode, which adds bearer-token
authentication and lets multiple channels push and pull memory segments so they
can share one memory file. It is an inspection and sync surface, not part of the
recall hot path.

Part of [DejaDB](https://github.com/AreevAI/dejadb) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/dejadb/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
