# 0007. Keyset pagination on Socrata's `:id`, not `unique_key` or offsets

- Status: Accepted (M0). **Verified against the live API on 2026-09-24** (see Verification).
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
- Before verification, this was based on Socrata documentation and common
  client practice. The fallback, had `:id >` been rejected, was `$order=:id`
  with `$offset` plus the before/after `rowsUpdatedAt` check. It was not
  needed.

## Verification (2026-09-24)
- **`:id >` in `$where` works together with `$order=:id`.** One week
  (2024-01-01..08) fetched with `--page-size 5000` took 13 pages; the full
  two-year scope took 143 pages of 50,000. Neither hit an HTTP error or the
  no-progress guard.
- **No rows skipped or repeated.** Both page sums equal the API's own
  `$select=count(*)` for the same `$where` (60,496 and 7,111,809). No `:id`
  appears twice.
- **Independent of page size.** Refetching the week with `--page-size 997`
  (61 pages) returned the same `:id`s in the same order and built the same
  `snapshot_hash`.
- **`:id` is opaque, and its order isn't string order.** Values look like
  `row-napw_xfji.nnxw`. `$order=:id` sorts them by an internal key, not
  lexically (half the adjacent pairs in the week are out of string order).
  The server applies the same order to `>`, so paging is correct. Clients
  must not compare or sort `:id` themselves; `fetch` only passes the last
  value back, so nothing changes.
- `rowsUpdatedAt` was unchanged across the 16-minute full fetch.
