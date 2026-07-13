// End-to-end smoke test for the `dejadb` napi-rs binding.
//
// Drives the real compiled native addon against a fresh temp database. The
// FFI convention is "scalars in, JSON strings out", so every structured return
// is JSON.parse'd and asserted on shape + content — mirroring
// crates/dejadb-py/tests/test_dejadb.py. Run: `node --test __test__`.

import test from 'node:test'
import assert from 'node:assert/strict'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { mkdtempSync } from 'node:fs'

import { DejaDb } from '../index.js'

const HEX64 = 64 // length of a SHA-256 content address in hex

function makeDb(ns = 'caller') {
  const dir = mkdtempSync(join(tmpdir(), 'dejadb-js-'))
  return new DejaDb(join(dir, 'test.db'), ns)
}

test('module exposes the DejaDb class', () => {
  assert.equal(typeof DejaDb, 'function')
})

test('addFact returns a 64-hex content address', () => {
  const m = makeDb()
  const h = m.addFact('john', 'prefers', 'tea', 0.95)
  assert.equal(typeof h, 'string')
  assert.equal(h.length, HEX64)
  assert.match(h, /^[0-9a-f]{64}$/)
})

test('recall roundtrip parses and carries the fields', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')

  const rows = JSON.parse(m.recall('john'))
  assert.ok(Array.isArray(rows))
  assert.equal(rows.length, 1)

  const row = rows[0]
  for (const key of ['hash', 'type', 'fields']) assert.ok(key in row)
  assert.equal(row.type, 'fact')
  assert.equal(row.hash.length, HEX64)
  assert.equal(row.fields.subject, 'john')
  assert.equal(row.fields.relation, 'prefers')
  assert.equal(row.fields.object, 'tea')
})

test('recall relation filter narrows results', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')
  m.addFact('john', 'speaks', 'german')

  assert.equal(JSON.parse(m.recall('john')).length, 2)

  const speaks = JSON.parse(m.recall('john', 'speaks'))
  assert.equal(speaks.length, 1)
  assert.equal(speaks[0].fields.object, 'german')
})

test('add() generic grain from JSON fields', () => {
  const m = makeDb()
  const h = m.add(
    'fact',
    JSON.stringify({ subject: 'alice', relation: 'likes', object: 'coffee', confidence: 0.8 }),
  )
  assert.equal(h.length, HEX64)
  const rows = JSON.parse(m.recall('alice'))
  assert.equal(rows[0].fields.object, 'coffee')
})

test('cal RECALL returns the grains wire payload', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')

  const payload = JSON.parse(m.cal('RECALL facts WHERE subject = "john"'))
  assert.equal(payload.type, 'grains')
  assert.ok(Array.isArray(payload.grains))
  assert.equal(payload.grains.length, 1)

  const grain = payload.grains[0]
  assert.equal(grain.grain_type, 'fact')
  assert.equal(grain.fields.object, 'tea')
  assert.equal(grain.hash.length, HEX64)
})

test('cal COUNT pipeline', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')
  m.addFact('john', 'speaks', 'german')

  const payload = JSON.parse(m.cal('RECALL facts WHERE subject = "john" | COUNT'))
  assert.equal(payload.type, 'count')
  assert.equal(payload.count, 2)
})

test('stats() returns a parseable JSON object', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')
  const s = JSON.parse(m.stats())
  assert.equal(typeof s.grains, 'number')
  assert.ok(s.grains >= 1)
})

test('bad input throws a JS Error', () => {
  const m = makeDb()
  // CAL structurally has no DELETE token -> parse/exec error surfaces as Error.
  assert.throws(() => m.cal('DELETE sha256:abc'), Error)
  // Malformed JSON fields for add() -> Error.
  assert.throws(() => m.add('fact', 'not-json'), Error)
  // Invalid content address for forget() -> Error.
  assert.throws(() => m.forget('nothex'), Error)
})

test('memoryTool create/view over /memories', () => {
  const m = makeDb()
  const created = m.memoryTool(
    JSON.stringify({
      command: 'create',
      path: '/memories/prefs.md',
      file_text: 'Dark roast only.',
    }),
  )
  assert.match(created, /Created \/memories\/prefs\.md/)
  const listing = m.memoryTool(JSON.stringify({ command: 'view', path: '/memories' }))
  assert.match(listing, /\/memories\/prefs\.md/)
  const body = m.memoryTool(JSON.stringify({ command: 'view', path: '/memories/prefs.md' }))
  assert.match(body, /Dark roast only\./)
})

test('migrate mem0 export + history builds a supersession chain', () => {
  const m = makeDb('main')
  const history = JSON.stringify([
    { memory_id: 'm-1', event: 'ADD', new_memory: 'Works at Acme', created_at: '2024-03-01T10:00:00Z' },
    { memory_id: 'm-1', event: 'UPDATE', new_memory: 'Works at Initech', created_at: '2024-06-01T10:00:00Z' },
  ])
  const rep = JSON.parse(m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep.added, 1)
  assert.equal(rep.superseded, 1)

  const head = JSON.parse(m.latest('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(head.fields.context.content, 'Works at Initech')
  const versions = JSON.parse(m.history('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(versions.length, 2)

  // re-run is a no-op, not an error
  const rep2 = JSON.parse(m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep2.added, 0)
})

test('reindexText succeeds on a text-indexed file', () => {
  const m = makeDb()
  m.addFact('john', 'prefers', 'tea')
  assert.equal(typeof m.reindexText(), 'number')
})

test('passphrase constructor rejects a wrong key on reopen', () => {
  const dir = mkdtempSync(join(tmpdir(), 'dejadb-js-enc-'))
  const path = join(dir, 'enc.db')
  {
    const m = new DejaDb(path, 'caller', 'correct horse battery staple')
    m.addFact('john', 'prefers', 'tea')
  }
  // wrong passphrase or no passphrase must not open the file
  assert.throws(() => new DejaDb(path, 'caller', 'wrong passphrase'), Error)
  assert.throws(() => new DejaDb(path, 'caller'), Error)
  // correct passphrase reads it back
  const m = new DejaDb(path, 'caller', 'correct horse battery staple')
  assert.equal(JSON.parse(m.recall('john')).length, 1)
})
