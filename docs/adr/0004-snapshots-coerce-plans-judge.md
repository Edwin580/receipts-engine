# 0004. Snapshots coerce, plans judge

- Status: Accepted (M0 review, 2026-09-24)
- Date: 2026-09-24
- Milestone: M0

## Context
"Cleaning is itself lineage." Any change made during snapshotting is
invisible in the Pipeline View and can't be undone as a counterfactual.
Suppose the snapshot "fixes" closed dates that come before created dates.
Every resolution-time claim built on it then depends on a choice the reader
can't see.

## Decision
The snapshot step only makes values *representable*: parse, trim, null when
unparseable, reject when unidentifiable, and log every change
(docs/snapshot/cleaning-rules.md). Plausibility checks are counted as
`known_issues` in the manifest. They are applied by explicit plan `Filter`s,
each with a `describe()` sentence and a live row count.

## Consequences
- Plans are a little longer. In exchange, every exclusion is visible,
  counted, and reversible.
- A naive plan can produce a nonsense number, for example a mean resolution
  time that includes 1900-01-01. The UI shows `known_issues` next to claims on
  the affected columns, so this is easy to spot.
- Rejected records still count as provenance. They are kept in
  `rejects.arrow` and included in the snapshot hash.
