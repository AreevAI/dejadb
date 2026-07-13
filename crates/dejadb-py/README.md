# dejadb-py

Python bindings for DejaDB, the embedded memory engine for AI agents.

`dejadb-py` is the PyO3 extension module that exposes DejaDB to Python as the
`dejadb` package. It wraps the same facade the CLI and MCP server use, with a
thin, version-stable FFI convention: scalar arguments in, JSON strings out for
anything structured, and errors raised as `ValueError`. One memory is one file,
opened with a namespace, giving Python agents durable add / recall / supersede /
forget over content-addressed memory.

```python
import dejadb

mem = dejadb.DejaDB("caller.db", ns="caller")          # or passphrase="..." for AES-256 at rest
h = mem.add_fact("john", "prefers", "tea", confidence=0.95)
print(mem.recall("john"))  # JSON string, newest-first

mem.set_embedder(my_model.encode, model="bge-m3")      # vector recall via a callback
mem.migrate("mem0", export_json, history_json)         # import from mem0/Zep/Letta/… (docs/migrate.md)
mem.memory_tool('{"command": "view", "path": "/memories"}')  # Anthropic memory-tool backend
```

Part of [DejaDB](https://github.com/AreevAI/dejadb) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/dejadb/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
