# 0008. Plan model: linear typed operators, SQL null semantics, Merkle plan hashes

- Status: Proposed (M1, implemented; awaiting review)
- Date: 2026-09-24
- Milestone: M1

## Context
M1 needs a plan representation that the Pipeline View can explain step by
step, that receipts can cite by hash, and that M2–M3 can trace lineage
through. The repository had no written M1 spec beyond the crate
descriptions. These choices were made while implementing and are
recorded here for review. The full semantics are in `docs/engine/plan.md`.

## Decision
1. **Seven single-input operators** (`scan filter map project aggregate
   sort limit`) in a node list with explicit input indices. The node-list
   form is already a DAG, so a later `join`/`union` adds a second input
   without a format change. There are no joins in M1: every M1 question is
   about one snapshot.
2. **SQL null semantics**, including Kleene `and`/`or`. A filter keeps only
   *true* rows. `describe()` says so whenever the predicate can be
   missing. Comparing with a bare `null` is a validation error.
3. **No silent numeric surprises.** Integer overflow is an error, not a
   wraparound. Integer division doesn't exist (`div` always gives a
   decimal). Decimal overflow and division by zero give *missing*, so
   decimal columns never hold NaN or infinity. Integer/decimal comparisons
   are exact.
4. **`map` can't overwrite a column.** A derived value always has a new
   name, so the Pipeline View never shows two different meanings under one
   column name.
5. **Typed literals in JSON; integers as strings.** This keeps canonical JSON
   exact (no numbers above 2^53) and keeps `1` distinct from `1.0`.
   Parsing is strict.
6. **Merkle node hashes.** `node_hash` covers the node and its input's
   hash, and `plan_hash = node_hash(output)`. The hash doesn't depend on
   node numbering, and shared prefixes hash the same, which M2's trace
   cache needs.
7. **Canonical JSON floats** follow ECMAScript `Number.prototype.toString`
   (RFC 8785). `serde_json` gains its `float_roundtrip` feature: without it,
   some decimals (for example subnormals) parse one ulp off and a
   saved-then-loaded plan would hash differently. This was found by the
   round-trip property test. It is a feature flag on an approved
   dependency, not a new crate. M0 snapshot hashes are unchanged (the
   one-week real snapshot rebuilds to the same `e0a8bb1d…`).
8. **`execute` keeps every step's table** and each operator's row mapping.
   This costs memory (one filtered copy per filter step) and buys per-step
   row counts now and lineage capture in M2 without re-execution.

## Consequences
- Plans are verbose, but every step has a sentence and a row count.
- Adding an operator or function means extending the JSON form, typing,
  `describe()`, the executor, and the reference interpreter in the
  differential test. That's deliberate friction.
- Open for review: median of integers above 2^53 is approximate
  (converted to decimals); a filter copies the columns it keeps rather than
  holding a selection vector (simpler; revisit if M4 memory is tight).
