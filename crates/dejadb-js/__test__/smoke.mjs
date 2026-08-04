// End-to-end smoke test for the `dejadb` napi-rs binding.
//
// Drives the real compiled native addon against a fresh temp database. The
// FFI convention is "scalars in, JSON strings out", so every structured return
// is JSON.parse'd and asserted on shape + content — mirroring
// crates/dejadb-py/tests/test_dejadb.py. Run: `node --test __test__`.
//
// Every method returns a promise, so every call here is awaited. Rejections
// are asserted with `assert.rejects`, not `assert.throws`: a promise-returning
// method reports a bad argument by rejecting, and `assert.throws` would pass
// vacuously against an unhandled rejection.

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

test('every store method returns a promise', async () => {
  const m = makeDb()
  const p = m.addFact('john', 'prefers', 'tea')
  assert.ok(p instanceof Promise, 'addFact must return a promise')
  await p
  assert.ok(m.recall('john') instanceof Promise, 'recall must return a promise')
  assert.ok(m.stats() instanceof Promise, 'stats must return a promise')
})

test('a store call does not block the event loop', async () => {
  // The reason this surface is async at all. A timer scheduled before a long
  // call must still fire while that call is in flight; with the old
  // synchronous methods it could not run until the call returned.
  const m = makeDb('bulk')
  const rows = Array.from({ length: 400 }, (_, i) =>
    JSON.stringify({ subject: `user${i % 20}`, relation: 'prefers', object: `value ${i}` }),
  ).join('\n')

  let tickedDuringCall = false
  const timer = setInterval(() => {
    tickedDuringCall = true
  }, 5)

  const report = JSON.parse(await m.migrate('jsonl', rows, null, 'bulk'))
  clearInterval(timer)

  assert.equal(report.added, 400)
  assert.ok(tickedDuringCall, 'a timer must fire while a store call is in flight')
})

test('addFact returns a 64-hex content address', async () => {
  const m = makeDb()
  const h = await m.addFact('john', 'prefers', 'tea', 0.95)
  assert.equal(typeof h, 'string')
  assert.equal(h.length, HEX64)
  assert.match(h, /^[0-9a-f]{64}$/)
})

test('recall roundtrip parses and carries the fields', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')

  const rows = JSON.parse(await m.recall('john'))
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

test('recall relation filter narrows results', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  await m.addFact('john', 'speaks', 'german')

  assert.equal(JSON.parse(await m.recall('john')).length, 2)

  const speaks = JSON.parse(await m.recall('john', 'speaks'))
  assert.equal(speaks.length, 1)
  assert.equal(speaks[0].fields.object, 'german')
})

test('add() generic grain from JSON fields', async () => {
  const m = makeDb()
  const h = await m.add(
    'fact',
    JSON.stringify({ subject: 'alice', relation: 'likes', object: 'coffee', confidence: 0.8 }),
  )
  assert.equal(h.length, HEX64)
  const rows = JSON.parse(await m.recall('alice'))
  assert.equal(rows[0].fields.object, 'coffee')
})

test('cal RECALL returns the grains wire payload', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')

  const payload = JSON.parse(await m.cal('RECALL facts WHERE subject = "john"'))
  assert.equal(payload.type, 'grains')
  assert.ok(Array.isArray(payload.grains))
  assert.equal(payload.grains.length, 1)

  const grain = payload.grains[0]
  assert.equal(grain.grain_type, 'fact')
  assert.equal(grain.fields.object, 'tea')
  assert.equal(grain.hash.length, HEX64)
})

test('cal COUNT pipeline', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  await m.addFact('john', 'speaks', 'german')

  const payload = JSON.parse(await m.cal('RECALL facts WHERE subject = "john" | COUNT'))
  assert.equal(payload.type, 'count')
  assert.equal(payload.count, 2)
})

test('stats() returns a parseable JSON object', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  const s = JSON.parse(await m.stats())
  assert.equal(typeof s.grains, 'number')
  assert.ok(s.grains >= 1)
})

test('bad input rejects with a JS Error', async () => {
  const m = makeDb()
  // CAL structurally has no DELETE token -> parse/exec error surfaces as Error.
  await assert.rejects(() => m.cal('DELETE sha256:abc'), Error)
  // Malformed JSON fields for add() -> Error.
  await assert.rejects(() => m.add('fact', 'not-json'), Error)
  // Invalid content address for forget() -> Error.
  await assert.rejects(() => m.forget('nothex'), Error)
})

test('memoryTool create/view over /memories', async () => {
  const m = makeDb()
  const created = await m.memoryTool(
    JSON.stringify({
      command: 'create',
      path: '/memories/prefs.md',
      file_text: 'Dark roast only.',
    }),
  )
  assert.match(created, /Created \/memories\/prefs\.md/)
  const listing = await m.memoryTool(JSON.stringify({ command: 'view', path: '/memories' }))
  assert.match(listing, /\/memories\/prefs\.md/)
  const body = await m.memoryTool(JSON.stringify({ command: 'view', path: '/memories/prefs.md' }))
  assert.match(body, /Dark roast only\./)
})

test('migrate mem0 export + history builds a supersession chain', async () => {
  const m = makeDb('main')
  const history = JSON.stringify([
    { memory_id: 'm-1', event: 'ADD', new_memory: 'Works at Acme', created_at: '2024-03-01T10:00:00Z' },
    { memory_id: 'm-1', event: 'UPDATE', new_memory: 'Works at Initech', created_at: '2024-06-01T10:00:00Z' },
  ])
  const rep = JSON.parse(await m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep.added, 1)
  assert.equal(rep.superseded, 1)

  const head = JSON.parse(await m.latest('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(head.fields.context.content, 'Works at Initech')
  const versions = JSON.parse(await m.history('mem0/m-1', 'mem0_memory', 'main'))
  assert.equal(versions.length, 2)

  // re-run is a no-op, not an error
  const rep2 = JSON.parse(await m.migrate('mem0-history', history, null, 'main'))
  assert.equal(rep2.added, 0)
})

test('latest resolves null when there is no head', async () => {
  const m = makeDb()
  assert.equal(await m.latest('nobody', 'prefers'), null)
})

test('reindexText succeeds on a text-indexed file', async () => {
  const m = makeDb()
  await m.addFact('john', 'prefers', 'tea')
  assert.equal(typeof (await m.reindexText()), 'number')
})

test('passphrase constructor rejects a wrong key on reopen', async () => {
  const dir = mkdtempSync(join(tmpdir(), 'dejadb-js-enc-'))
  const path = join(dir, 'enc.db')
  {
    const m = new DejaDb(path, 'caller', 'correct horse battery staple')
    await m.addFact('john', 'prefers', 'tea')
  }
  // wrong passphrase or no passphrase must not open the file. The constructor
  // stays synchronous, so these still throw rather than reject.
  assert.throws(() => new DejaDb(path, 'caller', 'wrong passphrase'), Error)
  assert.throws(() => new DejaDb(path, 'caller'), Error)
  // correct passphrase reads it back
  const m = new DejaDb(path, 'caller', 'correct horse battery staple')
  assert.equal(JSON.parse(await m.recall('john')).length, 1)
})

test('waiser: record tool calls, run, review, apply', async () => {
  const m = makeDb('caller')
  // 4 failures + 1 success for one tool → tool-failure clustering fires.
  // Distinct payloads per call: grains are content-addressed, so four
  // byte-identical failures recorded inside the same millisecond hash to the
  // same address and the fourth is rejected. This test is about the Waiser
  // loop, so it records four distinguishable failures.
  for (let i = 0; i < 4; i++) {
    await m.recordToolCall('stripe_refund', `rate_limited 429 (attempt ${i})`, true)
  }
  await m.recordToolCall('stripe_refund', 'ok', false)

  const run = JSON.parse(await m.waiserRun())
  assert.equal(run.outcome, 'ran')
  assert.ok(run.stored >= 1)

  const pending = JSON.parse(await m.recommendations())
  const tf = pending.find((r) => r.analyzer.startsWith('waiser.tool_failure'))
  assert.ok(tf, 'a tool-failure recommendation')
  assert.ok(tf.summary.includes('rate_limited'), 'signature is non-empty')

  const applied = JSON.parse(await m.applyRecommendation(tf.hash, 'retries belong in the client'))
  assert.equal(applied.hash, tf.hash)

  // A second bare run is idempotent (dedup).
  assert.equal(JSON.parse(await m.waiserRun()).stored, 0)

  // The Verify gate's record parses (empty until checkpoints elapse) and an
  // applied recommendation rolls back with a mandatory reason.
  assert.ok(Array.isArray(JSON.parse(await m.waiserOutcomes())))
  const rb = JSON.parse(await m.rollbackRecommendation(tf.hash, 'the lesson did not help'))
  assert.equal(rb.status, 'rolled_back')

  // A full-memory sweep (reflect semantics) still runs after the rollback.
  const sweep = await m.waiserRun(null, null, null, null, null, null, null, null, true)
  assert.equal(JSON.parse(sweep).outcome, 'ran')
})
