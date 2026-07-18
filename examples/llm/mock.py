#!/usr/bin/env python3
"""A deterministic mock --llm-cmd backend: no model, no network — echoes a
canned response per op so you can test the wiring (and CI can exercise the LLM
path). NOT for real use. Cites the first evidence hash so the DISCOVER draft
survives waiser's cite-check."""
import json
import sys

req = json.load(sys.stdin)
op = req.get("op")

if op == "probe":
    print(json.dumps({"model": "mock"}))
elif op == "discover":
    ev = req.get("evidence", [])
    if ev:
        print(json.dumps({"recommendations": [{
            "summary": "mock: a human should double-check this cluster",
            "target": "entity:test/mock",
            "guidance": "mock guidance note",
            "evidence": [ev[0]["hash"]],
        }]}))
    else:
        print(json.dumps({"recommendations": []}))
elif op == "enrich":
    # Add a guidance note to the first finding, if any.
    f = req.get("findings", [])
    notes = [{"target": f[0]["target"], "guidance": "mock: consider the latest value"}] if f else []
    print(json.dumps({"notes": notes}))
else:
    print("{}")
