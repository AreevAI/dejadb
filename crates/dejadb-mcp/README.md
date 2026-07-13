# dejadb-mcp

Model Context Protocol (MCP) stdio server for DejaDB.

`dejadb-mcp` exposes DejaDB to MCP-capable agents and hosts. It serves a small,
memory-semantic tool set — `dejadb_recall`, `dejadb_remember`, `dejadb_add`,
`dejadb_supersede`, `dejadb_forget`, and `dejadb_cal` — over newline-delimited
JSON-RPC 2.0 on stdio, rather than exposing raw SQL. Following the MCP
convention, protocol-level problems are returned as JSON-RPC errors while
tool-execution failures come back as `isError: true` tool results. This lets an
agent read and write durable memory using the same tool-calling interface it
uses for everything else.

Part of [DejaDB](https://github.com/AreevAI/dejadb) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/dejadb/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
