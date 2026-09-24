# 0006. Split the snapshot pipeline into `fetch` and `build` via a raw directory

- Status: Accepted (M0)
- Date: 2026-09-24
- Milestone: M0

## Context
The approved plan had one command and an optional `--keep-raw` flag. Two
things came up during implementation:
- The container that builds this project can't reach the Socrata API. Tests
  and benchmarks have to run on recorded or synthetic pages.
- Cleaning rules will change (`rules_version`). Re-fetching about 2.8 GB to
  re-run cleaning is slow and not reproducible: the source changes daily.

## Decision
- `fetch` writes a raw directory: the exact response bodies, the Socrata
  metadata, and `fetch.json`. `fetch.json` records the query, timestamps, and
  each page's size and BLAKE3, and it is written last, so it marks a
  completed fetch.
- `build` reads only the raw directory. It first checks every page against
  `fetch.json`.
- `synth` writes raw directories of synthetic 311-like records, including
  every anomaly the rules handle, for tests and benchmarks.
- Raw directories are local working state and are not published. The
  manifest pins them with `raw_hash`.

## Consequences
- The same raw directory always builds the same snapshot. CI and the
  differential oracle run with no network.
- A two-year fetch needs about 2.8 GB of local disk (measured on synthetic
  records of realistic width).
- Resuming a failed fetch isn't supported yet. A failed fetch leaves no
  `fetch.json` and has to be restarted. Pages are retried with exponential
  backoff (6 attempts), so this should be rare.
