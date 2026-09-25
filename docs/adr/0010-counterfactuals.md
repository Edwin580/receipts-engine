# 0010. Counterfactuals: exact incremental recomputation, with full re-run as the fallback

- Status: Proposed (M3, implemented; awaiting review)
- Date: 2026-09-25
- Milestone: M3

## Context
The core promise is "what the number becomes if you exclude any of those
rows." Classic deletion propagation updates aggregates algebraically
(`sum − x`, `count − 1`). That is fast, but for decimals it differs from
a recomputation in the last bits. A receipt that says "without these rows
the mean is 4.0528" must match what re-running the plan would say, or
the tool contradicts itself.

## Decision
1. **One entry point, `exclude`, that always returns the exact answer.**
   It picks the cheapest exact method: unchanged, incremental, or full
   re-run (`docs/engine/counterfactuals.md`).
2. **Incremental means recomputing only affected groups**, from their
   surviving members, in original row order, with the executor's own
   aggregate code. It is not algebraic updates. The cost is proportional
   to the affected groups' sizes, not the table, and the result is
   bit-identical to a full run. The steps after the aggregate re-run on
   the small aggregate output.
3. **The incremental shape is narrow on purpose:** one aggregate, and
   only row-preserving, order-stable steps before it (`filter`, `map`,
   `project`, `sort`). A `limit` before the aggregate falls back to a full
   re-run, because excluding a row there pulls in another one.
4. **Leave-one-out contributions (Tier 2) are a separate API** because
   they answer n what-ifs at once. They are exact for counts, integer
   sums and means, medians and extremes. Decimal sums and means use
   `total − x` and are marked `exact: false`. Those are for ranking rows
   by influence, and `exclude` confirms any single value shown as fact.
5. Lineage gains `SourceSubset` (a scan minus excluded rows, still naming
   original rows) and groups that don't cover every input row.

## Consequences
- Excluding any number of rows from the real noise plan takes 5–130 ms
  incrementally, against 1.1–2.5 s re-run (`docs/benchmarks/m3.md`).
- Plans outside the incremental shape still get exact answers, just at
  full re-run cost. Joins (future) will need their own incremental rule or
  will use the re-run.
- In the incremental path, the Pipeline View's row counts before the
  aggregate still include the excluded rows. The UI should show the
  exclusion at the aggregate step, or ask for a re-run when it wants
  per-step counts.
