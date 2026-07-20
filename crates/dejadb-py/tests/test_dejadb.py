"""End-to-end tests for the `dejadb` PyO3 bindings.

These drive the real compiled extension module (built with
`maturin develop -m crates/dejadb-py/Cargo.toml`) against a fresh
per-test temp database (pytest `tmp_path`). The FFI convention is
"scalars in, JSON strings out", so every structured return is parsed
with `json.loads` and asserted on shape + content. Nothing here asserts
on wall-clock values, so the suite is deterministic.
"""

import json

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

def test_remember_returns_observation(tmp_path):
    m = make_db(tmp_path)
    res = json.loads(m.remember("John likes tea"))
    assert "observation" in res
    assert len(res["observation"]) == HEX64
    assert isinstance(res["facts"], list)


def test_remember_with_prelinked_facts(tmp_path):
    m = make_db(tmp_path)
    facts = json.dumps(
        [{"subject": "john", "relation": "likes", "object": "tea", "confidence": 0.9}]
    )
    res = json.loads(m.remember("John likes tea", facts_json=facts))
    assert len(res["facts"]) == 1
    assert all(len(h) == HEX64 for h in res["facts"])


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
    for _ in range(4):
        m.record_tool_call("stripe_refund", "rate_limited 429", True)
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
