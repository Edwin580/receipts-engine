# Counterfactuals

Status: **implemented (M3)** in `crates/receipts-cf`, with hooks in
`receipts-exec` (`execute_excluding`, `aggregate_subset`, `rerun_from`)
and `receipts-lineage` (`SourceSubset`, partial groups). Decisions:
ADR 0010.

"What does this number become if you exclude these rows?" Lineage (M2)
says which rows *contributed* to a number. A counterfactual says what
happens *without* them, including effects lineage can't see: a row that
drops out of a `limit`, or a group that disappears.

## 1. `exclude(plan, base, sources, snapshot, rows)`

Re-derives a whole execution without the given snapshot rows. The result
is an ordinary `Execution` with tables and lineage, so the UI can trace and
display it exactly like the original. The method is chosen automatically:

| Method | When | Cost |
|---|---|---|
| `Unchanged` | none of the rows reach the aggregate (e.g. they were filtered out), or the plan doesn't read that snapshot | nothing |
| `Incremental` | exactly one aggregate, and only `filter`, `map`, `project` or `sort` before it | recompute only the affected groups, then re-run the steps after the aggregate |
| `Rerun` | anything else, e.g. a `limit` before the aggregate (excluding a row pulls another one in) | full re-execution on the snapshot minus the rows |

**Both paths give identical results, bit for bit.** The incremental path
recomputes affected groups from their surviving members, in their original
row order, with the executor's own aggregate code. A full run would
aggregate those members in that same order, so even decimal sums round the
same way. It doesn't use `sum − x`.

In the incremental path, steps *before* the aggregate keep their original
tables. The excluded rows simply belong to no group. Their row counts in
the Pipeline View are therefore the original ones, and the aggregate step
is where the exclusion shows. A full re-run records the scan as a
`SourceSubset`, so traces in either path name original snapshot rows.

## 2. `contributions(plan, base, row, column)` (Tier 2)

For one aggregate cell, this returns what it would be without **each** of
its contributing rows (leave-one-out), all at once. The UI uses it to rank
rows by influence ("these 5 complaints move the median the most").

| Function | Leave-one-out | Exact? |
|---|---|---|
| `count`, `count_non_null` | n − 1, or the count minus 1 if the row's value is present | yes |
| integer `sum`, `mean` | exact 128-bit total minus the value | yes (same formula as the executor) |
| `median` | from the sorted values: remove rank r, take the middle | yes |
| `min`, `max` (integer, decimal, timestamp) | changes only when removing the extreme itself | yes |
| decimal `sum`, `mean` | `total − x` | **no**, within n·ε·Σ\|x\| of a re-run |
| `min`, `max` of text / true-false | not supported (error) | — |

`removes_group` marks rows whose exclusion empties their group. The group
then disappears from the output instead of showing a zero. `exact: false`
tells the UI to confirm a decimal what-if with `exclude` before stating
it as fact.

## 3. How it's verified

- **`exclude` equals a full re-execution** (`receipts-cf/tests/cf.rs`):
  random plans (with or without a filter, map, sort or limit before the
  aggregate, random groups and aggregates, and a sort or limit after it)
  over random tables. The decimal values include ±1e16 and 0.1/0.2/0.3,
  which make sums order-sensitive. The exclusion sets are random, and
  both output bits and output lineage must match. In 20,000 cases,
  8,585 took the incremental path and 2,574 the re-run path.
- **Every contribution equals a real single-row exclusion.** Exact
  functions must match exactly. Decimal sums and means must fall within the
  floating-point summation bound.
- **Real snapshot** (`receipts-bench --bin m3`): six exclusions, from one
  row to 100,000 rows, ran incrementally and as full re-runs, with
  identical results. Leave-one-out values for the Bronx median over
  443,530 rows were spot-checked against real exclusions. See
  `docs/benchmarks/m3.md`.
