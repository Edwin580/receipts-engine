# 0011. The browser verifies every snapshot hash on load

- Status: Proposed (M4, implemented; awaiting review)
- Date: 2026-09-25
- Milestone: M4

## Context
Receipts cite a `snapshot_hash`. If the browser only trusted the
manifest's claim, a stale cache, a truncated download, or a tampered
static host would produce receipts citing a hash the data doesn't have.
Re-deriving every hash costs time on each load: BLAKE3 over 497 MB plus
decoding.

## Decision
`Engine.loadSnapshot` re-derives the manifest hash, every chunk,
dictionary and column hash, the cleaning-log and rejects hashes, and
`snapshot_hash`, and checks the sort order. It refuses the snapshot on any
mismatch, and there is no "skip verification" flag.

## Consequences
- Load on 7.1M rows: about 1.7 s threaded, of which hashing takes
  0.4 s. Single-threaded, it takes about 3.4 s, of which hashing takes
  1.1 s (`docs/benchmarks/m4.md`).
- A later warm-cache path (M5) may keep the decoded columns in IndexedDB,
  keyed by `snapshot_hash`. Anything re-read from there must be verified
  again, or stored with an integrity check that is just as strong.
- The reader and verifier are written from the spec, independently of
  the `arrow-ipc` writer, so they double as a second implementation of the
  format. Tests check that both agree.
