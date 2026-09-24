# 0003. Timestamps are naive NYC wall-clock microseconds

- Status: Proposed
- Date: 2026-09-24
- Milestone: M0

## Context
Socrata publishes 311 dates as `floating_timestamp`, which is local NYC wall
clock with no offset. Converting to UTC needs a time zone database. It is
also ambiguous for the repeated hour at the end of DST, and it would shift
every "by day" or "by month" grouping away from what the city publishes.

## Decision
Store `i64` microseconds since `1970-01-01T00:00:00`, interpreting the
wall-clock value as-is, with no zone. This is exactly DuckDB's `TIMESTAMP`,
which is the oracle's type. We parse the fixed Socrata format ourselves (a
civil-date-to-days conversion), so no date/time dependency is needed.

## Consequences
- Truncation to day, month, or hour needs no time zone data, and it matches
  the published data.
- Differences between two timestamps (resolution time) can be off by an hour
  when the interval crosses a DST change. This is documented as a known
  limitation in the UI. It doesn't matter for day-scale metrics.
- Microseconds are lossless for the source's millisecond precision.
