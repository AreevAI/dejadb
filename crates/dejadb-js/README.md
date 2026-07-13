# dejadb-js

Node.js (napi-rs) bindings for DejaDB, the embedded memory engine for AI agents.

`dejadb-js` is the napi-rs native addon that exposes DejaDB to Node.js as the
`dejadb` package. It mirrors the Python binding with the same thin,
version-stable FFI convention: scalar arguments in, JSON strings out for
anything structured, and errors thrown as JavaScript `Error`s. Because the
underlying engine is native, this is a compiled Node addon rather than WASM. One
memory is one file, opened with a namespace, giving JavaScript agents durable
add / recall / supersede / forget over content-addressed memory.

```js
const { DejaDb } = require('dejadb')

const mem = new DejaDb('caller.db', 'caller') // 3rd arg: passphrase for AES-256 at rest
const h = mem.addFact('john', 'prefers', 'tea', 0.95)
console.log(mem.recall('john')) // JSON string, newest-first

mem.setEmbedderCommand('python3 embed.py')   // vector recall via a host command
mem.migrate('mem0', exportJson, historyJson) // import from mem0/Zep/Letta/… (docs/migrate.md)
mem.memoryTool('{"command": "view", "path": "/memories"}') // Anthropic memory-tool backend
```

Part of [DejaDB](https://github.com/AreevAI/dejadb) — an embedded memory engine for AI agents. See the [architecture overview](https://github.com/AreevAI/dejadb/blob/main/ARCHITECTURE.md).

Licensed under MIT OR Apache-2.0.
