# Lineage

Status: **implemented (M2)** in `crates/receipts-lineage` (store and
tracing) and `crates/receipts-exec` (capture). Decisions: ADR 0009.

Lineage answers two questions without re-running the plan:

- **Backward:** which source rows produced this number?
- **Forward:** which results does this source row feed into?

## 1. What is captured (Tier 1)

Every step of an execution records how its output rows derive from its
input's rows. This mapping falls out of the operator itself: the executor
already computes it, and M2 keeps it instead of discarding it.

| Step | `StepLineage` | Meaning | Memory |
|---|---|---|---|
| `scan` | `Source { source, len }` | output row `i` is snapshot row `i` | none |
| `map`, `project` | `Identity { len }` | same row | none |
| `limit` | `Identity { len: count }` | the first `count` rows | none |
| `filter` | `Select { rows }`, or `Identity` if every row passes | output `i` is input `rows[i]` | 4 B per kept row |
| `sort` | `Select { rows }` (the permutation) | output `i` is input `rows[i]` | 4 B per row |
| `aggregate` | `Group { offsets, rows }` (CSR, members ascending) | output `g` combines its members | 4 B per input row + 4 B per group |

**Semantics: contributing rows.** A trace returns the rows whose values
flow into an output row. For an aggregate, those are all members of the
group, including members whose value was missing and skipped by `mean`.
Rows that a filter or limit dropped contribute to nothing.

This is deliberately *not* "every row that could change the answer." A
row that a filter removed, or a row just below a `limit` cut-off, can
influence the result if it changes. That is a counterfactual question and
belongs to M3 (`receipts-cf`), which re-derives results with rows removed.

## 2. Tracing

`LineageStore` holds the step mappings and the DAG edges:

- `backward(node, rows) -> Trace` composes the mappings from `node` down
  to the scan. `Trace.path` lists the rows at **every** step, so the
  Pipeline View can highlight a number's rows at each stage, not only at
  the source. `row_ids()` gives the source `RowId`s
  (`[source_id | 0 | row_index]`).
- `backward_row(node, row)` is the single-row case the UI uses when you
  click a number. It is cached in a 256-entry LRU keyed by `(node, row)`.
- `forward(scan, source_rows, node)` walks the other way. Inverse maps
  (input row → output row, input row → group) are built on first use and
  kept.

The store checks its inputs on construction: every mapping must fit its
input's row count, groups must partition their input, and edges must point
backwards.

## 3. How it's verified

- **Differential property test** (`receipts-exec/tests/differential.rs`).
  The reference interpreter carries each row's set of source rows through
  every operator. For random plans over random tables, the test checks
  three things:
  1. The backward trace of every output row equals the reference set
     exactly.
  2. The forward trace of every source row equals the set of output rows
     whose reference set contains it.
  3. Every trace passes through every step.

  CI runs 2,000 cases, and 30,000 were run once. All matched.
- **Unit tests** in `receipts-lineage`: hand-built composition through
  filter → sort → group → limit, exhaustive forward/backward duality on
  that store, caching, and rejection of inconsistent lineage.
- **On the real snapshot** (`receipts-bench --bin m2`): for "median hours
  to close noise complaints, by borough", every group's trace has exactly
  `requests` rows. Every traced row is a noise complaint in that borough
  with `closed_date >= created_date`. See `docs/benchmarks/m2.md`.
