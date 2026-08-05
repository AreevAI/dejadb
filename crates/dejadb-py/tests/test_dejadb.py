"""End-to-end tests for the `dejadb` PyO3 bindings.

These drive the real compiled extension module (built with
`maturin develop -m crates/dejadb-py/Cargo.toml`) against a fresh
per-test temp database (pytest `tmp_path`). The FFI convention is
"scalars in, JSON strings out", so every structured return is parsed
with `json.loads` and asserted on shape + content. No test asserts on a
wall-clock threshold, so the suite is deterministic — the one test that
touches the clock (`test_store_calls_release_the_gil`) asserts thread
interleaving and skips itself rather than fail if the call it needs to
observe finishes too quickly.
"""

import json
import sys
import threading
import time

import pytest

import dejadb


HEX64 = 64  # length of a SHA-256 content address in hex


def make_db(tmp_path, ns="caller"):
    """Open a brand-new one-file memory in a temp dir."""
    return dejadb.DejaDB(str(tmp_path / "test.db"), ns=ns)


# --------------------------------------------------------------------------
# module surface
# --------------------------------------------------------------------------

def test_module_exposes_class_and_version():
    assert hasattr(dejadb, "DejaDB")
    assert isinstance(dejadb.__version__, str)
    assert dejadb.__version__  # non-empty


# --------------------------------------------------------------------------
# add / recall roundtrip
# --------------------------------------------------------------------------

def test_add_fact_returns_content_address(tmp_path):
    m = make_db(tmp_path)
    h = m.add_fact("john", "prefers", "tea", confidence=0.95)
    assert isinstance(h, str)
    assert len(h) == HEX64
    int(h, 16)  # a valid 64-hex content address


def test_recall_roundtrip(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")

    rows = json.loads(m.recall("john"))
    assert isinstance(rows, list)
    assert len(rows) == 1

    row = rows[0]
    assert {"hash", "type", "fields"} <= set(row)
    assert row["type"] == "fact"
    assert len(row["hash"]) == HEX64
    assert row["fields"]["subject"] == "john"
    assert row["fields"]["relation"] == "prefers"
    assert row["fields"]["object"] == "tea"


def test_recall_relation_filter(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    m.add_fact("john", "speaks", "german")

    everything = json.loads(m.recall("john"))
    assert len(everything) == 2

    speaks = json.loads(m.recall("john", relation="speaks"))
    assert len(speaks) == 1
    assert speaks[0]["fields"]["object"] == "german"


def test_add_generic_grain(tmp_path):
    m = make_db(tmp_path)
    h = m.add(
        "fact",
        json.dumps(
            {
                "subject": "alice",
                "relation": "likes",
                "object": "coffee",
                "confidence": 0.8,
            }
        ),
    )
    assert len(h) == HEX64

    rows = json.loads(m.recall("alice"))
    assert rows[0]["fields"]["object"] == "coffee"


# --------------------------------------------------------------------------
# CAL query language ("JSON string out")
# --------------------------------------------------------------------------

def test_cal_recall_shape(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")

    payload = json.loads(m.cal('RECALL facts WHERE subject = "john"'))
    assert payload["type"] == "grains"
    assert isinstance(payload["grains"], list)
    assert len(payload["grains"]) == 1

    grain = payload["grains"][0]
    assert grain["grain_type"] == "fact"
    assert grain["fields"]["object"] == "tea"
    assert len(grain["hash"]) == HEX64


def test_cal_count_pipeline(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    m.add_fact("john", "speaks", "german")

    payload = json.loads(m.cal('RECALL facts WHERE subject = "john" | COUNT'))
    assert payload["type"] == "count"
    assert payload["count"] == 2


# --------------------------------------------------------------------------
# evolution: supersede / latest / history
# --------------------------------------------------------------------------

def test_supersede_and_latest(tmp_path):
    m = make_db(tmp_path)
    h1 = m.add_fact("john", "prefers", "tea")
    h2 = m.supersede(
        h1,
        "fact",
        json.dumps({"subject": "john", "relation": "prefers", "object": "coffee"}),
    )
    assert h2 != h1
    assert len(h2) == HEX64

    latest = json.loads(m.latest("john", "prefers"))
    assert latest["fields"]["object"] == "coffee"
    assert latest["hash"] == h2


def test_latest_missing_returns_none(tmp_path):
    m = make_db(tmp_path)
    assert m.latest("nobody", "prefers") is None


def test_history_chain(tmp_path):
    m = make_db(tmp_path)
    h1 = m.add_fact("john", "prefers", "tea")
    m.supersede(
        h1,
        "fact",
        json.dumps({"subject": "john", "relation": "prefers", "object": "coffee"}),
    )
    versions = json.loads(m.history("john", "prefers"))
    assert isinstance(versions, list)
    assert len(versions) >= 2
    assert {"hash", "object"} <= set(versions[0])


# --------------------------------------------------------------------------
# remember / stats / verify
# --------------------------------------------------------------------------

def test_remember_returns_event(tmp_path):
    m = make_db(tmp_path)
    res = json.loads(m.remember("John likes tea"))
    assert "event" in res
    assert len(res["event"]) == HEX64
    assert isinstance(res["facts"], list)
    # The raw text is an Event grain — same as the MCP tool and capture-stop.
    rows = json.loads(m.cal("RECALL events"))
    assert rows["grains"][0]["fields"]["content"] == "John likes tea"


def test_remember_threads_a_turn_by_session_and_role(tmp_path):
    m = make_db(tmp_path)
    m.remember("what's the refund policy?", session_id="call-1", role="user")
    rows = json.loads(m.cal("RECALL events"))
    fields = rows["grains"][0]["fields"]
    assert fields["session_id"] == "call-1"
    assert fields["role"] == "user"


def test_remember_with_prelinked_facts(tmp_path):
    m = make_db(tmp_path)
    facts = json.dumps(
        [{"subject": "john", "relation": "likes", "object": "tea", "confidence": 0.9}]
    )
    res = json.loads(m.remember("John likes tea", facts_json=facts))
    assert len(res["facts"]) == 1
    assert all(len(h) == HEX64 for h in res["facts"])
    # Host-supplied facts are the host's own assertion — no model attribution.
    assert "model" not in res


def test_remember_rejects_incomplete_facts(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError, match=r"facts\[0\]"):
        m.remember("note", facts_json=json.dumps([{"subject": "john"}]))


FAKE_LLM_PY = """
import json, sys
d = json.loads(sys.stdin.read())
op = d.get("op", "")
if op == "probe":
    print(json.dumps({"model": "fake-extractor-1"}))
elif op == "extract":
    print(json.dumps({"facts": [
        {"subject": "john", "relation": "prefers", "object": "window seat", "confidence": 0.9},
        {"subject": "john", "relation": "guess", "object": "likes jazz", "confidence": 0.2},
    ]}))
elif op == "ground":
    print(json.dumps({"results": [
        {"id": c["id"], "supported": "guess" not in c["claim"], "reason": "checked"}
        for c in d.get("claims", [])
    ]}))
else:
    print(json.dumps({}))
"""


@pytest.fixture
def fake_llm(tmp_path):
    """A `llm_cmd` string driving a scripted fake — hermetic, no network."""
    script = tmp_path / "fake_llm.py"
    script.write_text(FAKE_LLM_PY)
    return f"{sys.executable} {script}"


def test_remember_extracts_with_a_model(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(m.remember("I always want a window seat.", llm_cmd=fake_llm))
    assert res["model"] == "fake-extractor-1"
    assert res["proposed"] == 2
    assert res["dropped"] == 0
    assert res["verification_status"] == "unverified"
    assert len(res["facts"]) == 2

    # Provenance is on the grains, not just in the return value.
    rows = json.loads(m.cal('RECALL facts WHERE verification_status = "unverified"'))
    assert len(rows["grains"]) == 2
    for g in rows["grains"]:
        assert g["fields"]["derived_from"] == res["event"]
        assert g["fields"]["extractor_model"] == "fake-extractor-1"


def test_remember_grounding_drops_unsupported_facts(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(
        m.remember("I always want a window seat.", llm_cmd=fake_llm, ground_cmd=fake_llm)
    )
    assert res["proposed"] == 2
    assert res["dropped"] == 1
    assert res["verification_status"] == "verified"
    assert len(res["facts"]) == 1


def test_remember_confidence_floor(tmp_path, fake_llm):
    m = make_db(tmp_path)
    res = json.loads(
        m.remember("I always want a window seat.", llm_cmd=fake_llm, min_confidence=0.5)
    )
    assert res["dropped"] == 1
    assert len(res["facts"]) == 1


def test_remember_keeps_the_source_text_when_extraction_fails(tmp_path):
    m = make_db(tmp_path)
    script = tmp_path / "dying_llm.py"
    script.write_text(
        "import json, sys\n"
        "d = json.loads(sys.stdin.read())\n"
        "print(json.dumps({'model': 'dying-1'})) if d.get('op') == 'probe' else sys.exit(3)\n"
    )
    with pytest.raises(ValueError, match="was stored"):
        m.remember("some raw note", llm_cmd=f"{sys.executable} {script}")
    # The raw text survived the failed extraction.
    rows = json.loads(m.cal("RECALL events"))
    assert len(rows["grains"]) == 1


def test_stats_shape(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    s = json.loads(m.stats())
    for key in ("grains", "current", "triples", "terms", "ops"):
        assert key in s
    assert s["grains"] >= 1


def test_verify_ok(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    report = json.loads(m.verify())
    assert report["integrity"] == "ok"
    assert report["grains"] >= 1


# --------------------------------------------------------------------------
# error paths -> ValueError (PyValueError)
# --------------------------------------------------------------------------

def test_bad_hash_raises_valueerror(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.forget("not-a-valid-hash")


def test_bad_json_raises_valueerror(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.add("fact", "{ this is not valid json")


def test_destructive_cal_raises_valueerror(tmp_path):
    # CAL structurally cannot destroy data: DELETE is not a grammar token,
    # so parsing it fails and surfaces as PyValueError.
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.cal("DELETE sha256:abc")


# --------------------------------------------------------------------------
# embedder callback, migration, reindex, encryption
# --------------------------------------------------------------------------

def _toy_embed(text):
    """Deterministic 8-dim embedding: identical text -> identical vector."""
    import hashlib
    h = hashlib.sha256(text.encode()).digest()
    return [b / 255.0 for b in h[:8]]


def test_set_embedder_callback_is_wired_into_writes(tmp_path):
    path = str(tmp_path / "vec.db")
    m = dejadb.DejaDB(path, ns="main")
    m.set_embedder(_toy_embed, model="sha-toy")
    # adds run the callback (a broken one would raise on add)
    m.add_fact("alice", "prefers", "tea", ns="main")
    m.add_fact("bob", "prefers", "coffee", ns="main")
    del m

    # provenance was stamped: reopening with a different-dim embedder warns
    m = dejadb.DejaDB(path, ns="main")
    m.set_embedder(lambda text: [0.0, 1.0, 2.0], model="other")
    warnings = json.loads(m.open_warnings())
    assert any("embedding mismatch" in w for w in warnings), warnings


def test_set_embedder_rejects_bad_callback(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.set_embedder(lambda text: [])  # empty vector
    with pytest.raises(ValueError):
        m.set_embedder(lambda text: "not a vector")


def test_migrate_mem0_history_chain_and_rerun(tmp_path):
    m = make_db(tmp_path, ns="main")
    history = json.dumps([
        {"memory_id": "m-1", "event": "ADD", "new_memory": "Works at Acme",
         "created_at": "2024-03-01T10:00:00Z"},
        {"memory_id": "m-1", "event": "UPDATE", "new_memory": "Works at Initech",
         "created_at": "2024-06-01T10:00:00Z"},
    ])
    rep = json.loads(m.migrate("mem0-history", history, ns="main"))
    assert (rep["added"], rep["superseded"]) == (1, 1)

    head = json.loads(m.latest("mem0/m-1", "mem0_memory", ns="main"))
    assert head["fields"]["context"]["content"] == "Works at Initech"
    versions = json.loads(m.history("mem0/m-1", "mem0_memory", ns="main"))
    assert len(versions) == 2

    # re-run: no duplicates, no error
    rep2 = json.loads(m.migrate("mem0-history", history, ns="main"))
    assert rep2["added"] == 0


def test_migrate_unknown_source_raises(tmp_path):
    m = make_db(tmp_path)
    with pytest.raises(ValueError):
        m.migrate("not-a-source", "{}")


def test_reindex_text_returns_count(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "tea")
    assert isinstance(m.reindex_text(), int)


def test_passphrase_roundtrip_and_wrong_key(tmp_path):
    path = str(tmp_path / "enc.db")
    m = dejadb.DejaDB(path, ns="caller", passphrase="correct horse battery staple")
    m.add_fact("john", "prefers", "tea")
    del m
    with pytest.raises(ValueError):
        dejadb.DejaDB(path, ns="caller", passphrase="wrong")
    with pytest.raises(ValueError):
        dejadb.DejaDB(path, ns="caller")  # encrypted file, no key
    m = dejadb.DejaDB(path, ns="caller", passphrase="correct horse battery staple")
    assert len(json.loads(m.recall("john"))) == 1


def test_open_warnings_is_json_list(tmp_path):
    m = make_db(tmp_path)
    assert isinstance(json.loads(m.open_warnings()), list)


# --------------------------------------------------------------------------
# waiser — the governed self-improvement loop
# --------------------------------------------------------------------------

def test_waiser_loop_rollback_and_outcomes(tmp_path):
    m = make_db(tmp_path)
    # Distinct payloads per call: grains are content-addressed, so four
    # byte-identical failures recorded inside the same millisecond hash to the
    # same address and the fourth is rejected. Whether four identical failures
    # in one millisecond should be four grains is an engine question; this test
    # is about the Waiser loop, so it records four distinguishable ones.
    for i in range(4):
        m.record_tool_call("stripe_refund", f"rate_limited 429 (attempt {i})", True)
    m.record_tool_call("stripe_refund", "ok", False)

    run = json.loads(m.waiser_run())
    assert run["outcome"] == "ran"
    assert run["stored"] >= 1

    pending = json.loads(m.recommendations())
    tf = next(r for r in pending if r["analyzer"].startswith("waiser.tool_failure"))
    assert "rate_limited" in tf["summary"]

    applied = json.loads(m.apply_recommendation(tf["hash"], "codify the lesson"))
    assert applied["rollbackable"] is True

    # The Verify gate's record is a JSON list (empty until checkpoints elapse).
    assert isinstance(json.loads(m.waiser_outcomes()), list)

    rb = json.loads(m.rollback_recommendation(tf["hash"], "the lesson did not help"))
    assert rb["status"] == "rolled_back"

    # A full-memory sweep (the `deja waiser reflect` semantics) still runs.
    sweep = json.loads(m.waiser_run(full_sweep=True))
    assert sweep["outcome"] == "ran"


def test_waiser_policy_file_grants_auto_apply(tmp_path):
    """The bindings honor a host waiser-policy.json (path in, same file the
    CLI takes) — and only value-identical structural curation auto-applies."""
    m = make_db(tmp_path)
    # A case-variant exact duplicate (distinct bytes, same normalized value).
    m.add_fact("acme", "tier", "Enterprise")
    m.add_fact("acme", "tier", "enterprise")

    policy = tmp_path / "waiser-policy.json"
    policy.write_text(json.dumps({
        "auto_apply_enabled": True,
        "auto_apply": [
            {"analyzer": "waiser.duplicate_sweep", "targets": ["memory"], "max_severity": "low"}
        ],
    }))

    # Without the policy nothing auto-applies.
    run = json.loads(m.waiser_run())
    assert run["auto_applied"] == 0

    # The pending consolidation is not re-proposed on a re-run (dedup), so
    # seed a fresh file to see the grant auto-apply end-to-end.
    fresh = dejadb.DejaDB(str(tmp_path / "granted.db"), ns="caller")
    fresh.add_fact("acme", "tier", "Enterprise")
    fresh.add_fact("acme", "tier", "enterprise")
    granted = json.loads(fresh.waiser_run(policy=str(policy)))
    assert granted["auto_applied"] == 1

    bad = tmp_path / "bad-policy.json"
    bad.write_text('{"auto_apply_enabled": true, "surprise": 1}')
    with pytest.raises(ValueError):
        fresh.waiser_run(policy=str(bad))  # unknown keys are rejected


# --------------------------------------------------------------------------
# threading: store calls must not pin the interpreter
# --------------------------------------------------------------------------

def test_store_calls_release_the_gil(tmp_path):
    """A long store call must let other Python threads run.

    The one test here that involves the clock, and it deliberately asserts a
    *structural* property rather than a threshold: while this thread sits
    inside a single native call, another Python thread has to get scheduled at
    least once. Before the bindings released the GIL, every call held it for
    its full duration, so a host that moved writes onto a background thread —
    which is what agent frameworks do precisely so a slow write cannot stall a
    turn — got no isolation at all. Waiting on the store mutex had the same
    shape: block on the lock, keep the GIL, freeze every other thread.
    """
    src = dejadb.DejaDB(str(tmp_path / "src.db"), ns="caller")
    # Seed via migrate: it loads under a deferred text index, so building the
    # fixture stays fast even though the import below is deliberately not.
    payload = "\n".join(
        json.dumps({"subject": f"user{i % 40}", "relation": "prefers", "object": f"value {i}"})
        for i in range(600)
    )
    assert json.loads(src.migrate("jsonl", payload))["added"] == 600
    ops = str(tmp_path / "ops.bundle")
    src.bundle(ops, 0)

    dst = dejadb.DejaDB(str(tmp_path / "dst.db"), ns="caller")

    ticks = []
    stop = threading.Event()

    def sampler():
        # sleep() yields the GIL, so this thread is only ever blocked by
        # someone else holding it — which is exactly what we are testing for.
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.005)

    watcher = threading.Thread(target=sampler, daemon=True)
    watcher.start()
    try:
        while not ticks:  # make sure it is actually running before we start
            time.sleep(0.005)
        started = time.perf_counter()
        dst.import_bundle(ops)  # one long native call
        ended = time.perf_counter()
    finally:
        stop.set()
        watcher.join(timeout=5)

    elapsed = ended - started
    if elapsed < 0.05:
        pytest.skip(f"import_bundle took {elapsed * 1000:.0f} ms — too short to observe interleaving")

    # The property is "the sampler was never locked out for long", not "it ran
    # at least once": a single tick lands on the boundary even when the GIL is
    # held end to end, so counting ticks would pass against a binding that
    # never yields. Measure the worst stall instead. Held for the whole call,
    # the gap equals the call; yielding, it stays at the sampler's own cadence
    # (measured: 3406 ms vs 7 ms across this exact call).
    gaps = [b - a for a, b in zip(ticks, ticks[1:])]
    worst = max(gaps) if gaps else elapsed
    assert worst < elapsed / 4, (
        f"another thread was starved for {worst * 1000:.0f} ms during a "
        f"{elapsed * 1000:.0f} ms store call — the GIL is being held across it"
    )


# --------------------------------------------------------------------------
# index_text, add_batch, search
# --------------------------------------------------------------------------

def test_index_text_defaults_to_the_file_declaration(tmp_path):
    path = str(tmp_path / "decl.db")
    # Explicit -> deliberate re-stamp, no warning on a file with no prior claim.
    off = dejadb.DejaDB(path, ns="caller", index_text=False)
    assert json.loads(off.open_warnings()) == []
    del off

    # Bare reopen honors what the file declares, silently.
    bare = dejadb.DejaDB(path, ns="caller")
    assert json.loads(bare.open_warnings()) == []
    del bare

    # Flipping it back on is a change the operator should see, not discover.
    on = dejadb.DejaDB(path, ns="caller", index_text=True)
    warnings = json.loads(on.open_warnings())
    assert any("text_index" in w for w in warnings), warnings


def test_search_finds_by_free_text(tmp_path):
    m = make_db(tmp_path)
    m.add_fact("john", "prefers", "window seat")
    m.add_fact("mary", "prefers", "aisle seat")

    hits = json.loads(m.search("window"))
    assert [h["fields"]["object"] for h in hits] == ["window seat"]
    assert len(hits[0]["hash"]) == HEX64

    # Anchoring narrows without losing the free-text leg.
    assert json.loads(m.search("seat", subject="mary"))[0]["fields"]["subject"] == "mary"


def test_search_without_any_leg_raises_rather_than_answering_empty(tmp_path):
    # No BM25 index and no embedder: there is nothing to rank on. Returning []
    # would read as "no matching memories", which is a wrong answer rather than
    # an empty one.
    m = dejadb.DejaDB(str(tmp_path / "noleg.db"), ns="caller", index_text=False)
    m.add_fact("john", "prefers", "window seat")
    assert json.loads(m.recall("john"))  # structural recall still works
    with pytest.raises(ValueError, match="text or vector leg"):
        m.search("window")


def test_add_batch_roundtrip(tmp_path):
    m = make_db(tmp_path)
    hashes = json.loads(m.add_batch(json.dumps([
        {"grain_type": "fact", "fields": {"subject": "ann", "relation": "likes", "object": "tea"}},
        {"type": "fact", "fields": {"subject": "bob", "relation": "likes", "object": "coffee"}},
    ])))
    assert len(hashes) == 2
    assert all(len(h) == HEX64 for h in hashes)
    assert json.loads(m.recall("ann"))[0]["fields"]["object"] == "tea"
    assert json.loads(m.recall("bob"))[0]["fields"]["object"] == "coffee"


def test_add_batch_rejects_the_whole_call_and_writes_nothing(tmp_path):
    m = make_db(tmp_path)
    before = json.loads(m.stats())["grains"]
    with pytest.raises(ValueError):
        m.add_batch(json.dumps([
            {"grain_type": "fact", "fields": {"subject": "ann", "relation": "likes", "object": "tea"}},
            {"grain_type": "fact"},  # no fields -> the batch is refused
        ]))
    assert json.loads(m.stats())["grains"] == before
