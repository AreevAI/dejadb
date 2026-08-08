# dejadb-loop

The **DejaDB substrate adapter** for the [`deja-loop`](../deja-loop) engine: it
implements `deja_loop::OmsSubstrate` over `dejadb_cal::DejaDbFacade`, so the
governed self-improvement loop runs against real DejaDB `.mg`/Turso memory
files.

`deja-loop` itself has zero DejaDB dependencies (it talks to the `OmsSubstrate`
trait). This crate is the glue that binds the two — and, per proposal §10, it
stays in the DejaDB repo even after the engine is lifted to its own repo, so
DejaDB remains the reference substrate. The CLI (`deja loop`), server, and
bindings all sit on top of this adapter.

Not published during the churn phase (`publish = false`).

Licensed under MIT OR Apache-2.0.
