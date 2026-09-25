# 0009. Lineage: always-on per-step mappings, composed on demand

- Status: Proposed (M2, implemented; awaiting review)
- Date: 2026-09-25
- Milestone: M2

## Context
Receipts must trace any displayed number to its source rows, fast enough
to feel instant on click, in a browser with limited memory. There are
three common designs:
1. **Eager, end-to-end:** store source-row sets per output row
   (annotation propagation). Cheap to query, but memory grows with every
   step: an aggregate over 1.2M rows would copy 1.2M ids per step.
2. **Re-execution:** store nothing, and re-run an instrumented plan on
   click. It uses no memory but costs a full plan run per click.
3. **Per-step mappings, composed lazily:** each step keeps only the
   mapping it already computed (selection vector, permutation, group
   membership). A trace composes them on demand.

## Decision
Option 3 (`docs/engine/lineage.md`).
- Capture is **always on**. The mappings are the executor's own
  intermediate vectors, so capture costs little: the noise plan went from
  585 ms to 622 ms, which is within run-to-run noise. On the 7.1M-row
  snapshot it holds 9.4 MB.
- Operators that don't reorder or drop rows (`map`, `project`, `limit`,
  and a filter that keeps everything) store nothing.
- Traces return the rows at every step, not just the source, for the
  Pipeline View.
- Single-row traces are cached (256-entry LRU). Forward inverse maps are
  built lazily.
- Semantics are **contributing rows** (why-provenance). "Rows that could
  change the answer" is M3's counterfactual problem.

## Consequences
- A plan that sorts all 7.1M rows holds a 28 MB permutation. If M4 memory
  is tight, sorts followed by `limit` can keep only the top-k (the same
  optimization the executor wants anyway, see `docs/benchmarks/m1.md`).
- Adding a join (future) needs a two-input mapping. `StepLineage` would
  gain a variant, and `Trace` would become a set of per-source paths. The
  store's API already returns the source with the trace.
- The mappings live as long as the `Execution`. The UI decides when to
  drop old executions.
