# Logical plans

Status: **implemented (M1)** in `crates/receipts-plan` (model, validation,
JSON, hashing, `describe()`) and `crates/receipts-exec` (execution).
Decisions: ADR 0008.

A plan is the full recipe for a number: which snapshot, which rows, which
computed columns, which grouping. Every claim Receipts shows is the output of
a plan. The plan hash, together with the snapshot hash, identifies it.

## 1. Operators

A plan is a list of steps (`nodes`) plus the index of the output step.
Each step except `scan` reads one earlier step, so the list is already in
topological order. Every step must lead to the output. Unused steps are an
error, so the plan hash covers the whole plan.

| Op | Reads | Output | Notes |
|---|---|---|---|
| `scan` | a snapshot, by `snapshot_hash` | all its rows and columns | |
| `filter` | predicate (true/false) | rows where the predicate is **true** | missing counts as not true |
| `map` | name, expression | input columns plus one new column | can't replace an existing column |
| `project` | column list | those columns, in that order | |
| `aggregate` | `group_by` columns, aggregates | one row per distinct group | see below |
| `sort` | keys, each asc or desc | same rows, reordered | stable; missing values are smallest |
| `limit` | count | first `count` rows | |

**Aggregate.** With no `group_by` there is exactly one output row, even
with no input rows. Otherwise there is one row per distinct combination of
group values. A missing group value is a group of its own. Rows are sorted
by the group values (ascending, missing first), so the output order is
deterministic. Output columns are the group columns, then the aggregates.

| Function | Input | Output | Over no values |
|---|---|---|---|
| `count` | none (counts rows) | integer | 0 |
| `count_non_null` | any column | integer | 0 |
| `sum` | integer / decimal | integer (exact; overflow is an error) / decimal | missing |
| `mean` | integer / decimal | decimal | missing |
| `median` | integer / decimal | decimal: the middle value, or the mean of the two middle values | missing |
| `min`, `max` | integer, decimal, timestamp, text, bool | same type | missing |

All functions except `count` skip missing values. Decimal sums add values
in row order. Integer sums and means accumulate exactly in 128 bits.
Medians convert integers to decimals first, which is exact below 2^53.

## 2. Expressions and missing values

Types are integer (`i64`), decimal (`f64`), true/false, timestamp (naive
NYC wall clock, ADR 0003), text, and location. Location can only be read
through `lat`/`lon`.

| Expression | Types | Result | Missing when |
|---|---|---|---|
| `eq ne lt le gt ge` | both numeric (compared exactly, even integer vs decimal), both timestamp, both text (byte order), both bool | bool | either side is missing |
| `and`, `or` | bools | bool | Kleene logic: `false and missing = false`, `true or missing = true` |
| `not` | bool | bool | input missing |
| `is_null` | any | bool | never |
| `in` | value and a list of non-null literals of a comparable type | bool | the value is missing |
| `add sub mul` | integer, integer | integer | an input is missing. **Overflow is an error.** |
| `add sub mul div` | numeric, mixed or decimal | decimal | an input is missing, division by zero, or a result too large to represent |
| `sub` | timestamp, timestamp | integer microseconds | an input is missing (overflow is an error) |
| `add`, `sub` | timestamp ± integer µs | timestamp | an input is missing (overflow is an error) |
| `date_trunc` | timestamp; `day`, `week` (ISO, Monday), `month`, `year` | timestamp | input missing |
| `lat`, `lon` | location | decimal | input missing |

Consequences worth knowing:

- Decimal columns never hold NaN or infinity, so ordering is total and
  `-0.0 = 0.0`. They also group together.
- A bare `null` literal can't be compared (`x = null` is always missing
  in SQL, which is almost always a bug). Use `is_null`.
- Validation checks names and types before anything runs, and explains
  each problem in a sentence that names the step, for example `step 1:
  can't compare text with integer (eq)`.
- Validation infers whether each output column can be missing. The
  executor re-checks this when it builds each step's table.

## 3. JSON form

```json
{"format": "receipts-plan/1",
 "nodes": [
   {"op": "scan", "snapshot": "3b42e46a…"},
   {"op": "filter", "input": 0, "predicate":
     {"call": "and", "args": [
       {"call": "in", "args": [{"column": "complaint_type"}],
        "values": [{"str": "Noise - Residential"}]},
       {"call": "not", "args": [{"call": "is_null", "args": [{"column": "closed_date"}]}]}]}},
   {"op": "map", "input": 1, "name": "hours_to_close", "expr":
     {"call": "div", "args": [
       {"call": "sub", "args": [{"column": "closed_date"}, {"column": "created_date"}]},
       {"literal": {"i64": "3600000000"}}]}},
   {"op": "aggregate", "input": 2, "group_by": ["borough"], "aggregates": [
     {"name": "requests", "fn": "count"},
     {"name": "median_hours", "fn": "median", "column": "hours_to_close"}]},
   {"op": "sort", "input": 3, "keys": [{"column": "median_hours", "order": "desc"}]},
   {"op": "limit", "input": 4, "count": "10"}],
 "output": 5}
```

- Literals carry their type: `{"i64": "…"}`, `{"f64": 1.5}`, `{"str": …}`,
  `{"bool": …}`, `{"timestamp": "YYYY-MM-DDTHH:MM:SS.ffffff"}`, or `null`.
  Integers (and `limit` counts) are decimal strings, because canonical
  JSON numbers stop at 2^53. The integer `1` and the decimal `1.0` are
  therefore different literals with different hashes.
- Parsing is strict. An unknown key, a missing key, the wrong arity, or a
  non-canonical integer spelling (`"01"`, `"-0"`) is an error. A plan that
  parses means exactly what it says.
- Floats round-trip exactly: `serde_json` is built with `float_roundtrip`.

## 4. Hashing

```
node_hash(n) = BLAKE3-derive_key("receipts plan v1 node",
                 len:u32 ‖ canonical_json(node n, with "input" := hex(node_hash(input))))
plan_hash    = node_hash(output)
```

- `canonical_json` is RFC 8785 (`receipts-core`). Floats use ECMAScript
  number formatting, so a browser can recompute the hash.
- Hashes form a Merkle chain. A node's hash identifies the whole
  computation up to that node, not its position in the list, so it can
  key caches of intermediate results (the composed-trace LRU in M2).
  Steps shared by two plans have equal hashes.
- The scan's `snapshot` hash is inside the chain, so a plan hash is only
  meaningful for that snapshot.
- A known-answer test (`plan_hash_is_pinned`) pins the v1 encoding.

## 5. `describe()`

Every step has a generated sentence. It is never written by hand, so it
can't drift from what the engine does. It notes behaviour that could
surprise a reader. Real output for the benchmark plan:

1. Start from NYC 311 Service Requests (snapshot 3b42e46a2d17981f).
2. Keep only rows where complaint_type is one of "Noise - Residential",
   "Noise - Street/Sidewalk" and closed_date is present and closed_date is on
   or after created_date. Rows where this can't be decided because a value is
   missing are dropped.
3. Add a column hours_to_close: (closed_date − created_date) ÷ 3600000000.
   Dividing by zero gives a missing value.
4. Group the rows by borough and, for each group, compute the number of rows
   (as requests), the median of hours_to_close (as median_hours) and the
   average of hours_to_close (as mean_hours). Missing values are left out of
   these calculations. Rows with a missing group value form their own group.
5. Sort by median_hours (largest first). Missing values count as the smallest.

## 6. Execution (`receipts-exec`)

- `execute(plan, sources)` runs a validated plan and keeps **every step's
  table**, so the Pipeline View can show a row count per step. Columns a
  step doesn't change are shared (`Arc`), not copied.
- Operators are column-at-a-time. Text comparisons with a constant use
  dictionary codes (dictionaries are sorted, so code order is string
  order). `in` over text evaluates once per dictionary entry.
- Each operator builds an explicit row mapping: filter a selection
  vector, sort a permutation, aggregate a group id per input row. M2
  lineage capture records these mappings instead of re-deriving them.
- **Correctness:** a differential property test runs random valid plans
  (filters, maps, sorts, limits, aggregates) over random tables. The
  tables include nulls, `-0.0`, values near overflow, and unused dictionary
  entries. The test compares the engine against an independent
  row-at-a-time reference interpreter: 2,000 cases in CI, 30,000 run once.
  It has already found two bugs: an aggregate with no groups crashed, and
  decimal overflow produced missing values in a column typed as never
  missing.
