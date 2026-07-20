# examples/analyzers — bring-your-own command analyzers

`deja waiser run --analyzer-cmd 'CMD'` registers a subprocess analyzer written
in any language: a live-grain snapshot arrives on stdin, advisory findings go
back on stdout. It runs at **trust class `command`, auto-apply `never`** — a
domain-specific check (PII, house style, compliance) can *surface* an issue a
human then reviews, but can never mutate memory. A failing command skips that
analyzer for the run, never the pass.

| File | What |
|---|---|
| [`pii_scan.py`](pii_scan.py) | Flags facts whose object looks like an email address (protocol documented inline) |

Try it against the demo corpus:

```bash
deja init --db demo.db --template demo
deja add --db demo.db --ns caller --subject support --relation contact --object "help@example.com"
deja waiser run  --db demo.db --analyzer-cmd 'python3 examples/analyzers/pii_scan.py'
deja waiser list --db demo.db          # the [external] finding sits in the queue
```

From the bindings, `analyzer_cmd` on `waiser_run` is the same seam — and the
only custom-analyzer path from Python/Node (which can't implement the Rust
`Analyzer` trait). Full contract: the module doc of `crates/waiser/src/external.rs`
and [`docs/waiser.md`](../../docs/waiser.md#external-analyzers-optional).
