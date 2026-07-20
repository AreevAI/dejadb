# dejadb-waiser

The **DejaDB substrate adapter** for the [`waiser`](../waiser) engine: it
implements `waiser::OmsSubstrate` over `dejadb_cal::DejaDbFacade`, so the
governed self-improvement loop runs against real DejaDB `.mg`/Turso memory
files.

`waiser` itself has zero DejaDB dependencies (it talks to the `OmsSubstrate`
trait). This crate is the glue that binds the two — and, per proposal §10, it
stays in the DejaDB repo even after the engine is lifted to its own repo, so
DejaDB remains the reference substrate. The CLI (`deja waiser`), server, and
bindings all sit on top of this adapter.

Not published during the churn phase (`publish = false`).

Licensed under MIT OR Apache-2.0.
