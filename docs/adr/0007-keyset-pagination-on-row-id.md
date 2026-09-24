# 0007. Keyset pagination on Socrata's `:id`, not `unique_key` or offsets

- Status: Accepted (M0). Unverified against the live API (see Consequences).
- Date: 2026-09-24
- Milestone: M0

## Context
The draft schema proposed keyset paging on `unique_key`. Two problems:
- With `unique_key > last`, a key that appears in two records (the exact
  case CR-03/CR-04 exist to catch) can straddle a page boundary. The second
  copy would then be skipped silently.
- Offset paging (`$offset`) can skip or repeat rows when the dataset is
  updated during a ~40-minute fetch, and 311 is updated daily.

## Decision
Page with `$order=:id` and `$where=(<scope>) AND :id > '<last :id>'`, and
select `:id` into every record. `:id` is Socrata's unique system row id.
`fetch` also records `rowsUpdatedAt` before and after, and warns if they
differ.

## Consequences
- Duplicate source keys are all fetched, so CR-03/CR-04 can see them.
- Row order in the snapshot doesn't depend on `:id` (rows are re-sorted), so
  the choice of paging key doesn't affect `snapshot_hash`.
- **Unverified:** this environment can't reach the API. That `:id` supports
  `>` in `$where` is based on Socrata documentation and common client
  practice. If it doesn't, the first page after page 1 fails loudly (HTTP 400,
  or the no-progress guard). The fallback is `$order=:id` with `$offset`, plus
  the before/after `rowsUpdatedAt` check.
