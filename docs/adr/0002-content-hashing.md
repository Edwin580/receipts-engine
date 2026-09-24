# 0002. Content hashing over a canonical logical encoding, not IPC bytes

- Status: Accepted (M0 review, 2026-09-24)
- Date: 2026-09-24
- Milestone: M0

## Context
Receipts cite `snapshot_hash`, so it has to be stable. Arrow IPC bytes aren't
stable: they vary with padding, alignment, metadata key order, writer version,
and the contents of null slots, which Arrow leaves undefined.

## Decision
Hash a byte encoding we define ourselves (docs/snapshot/schema.md §6):
- BLAKE3 in `derive_key` mode, with one context string per kind of hashed
  object (chunk, dictionary, column, snapshot, ...). This is BLAKE3's built-in
  domain separation, and it replaces the ad-hoc prefix tags in the first draft,
- little-endian values,
- validity bitmap stored explicitly,
- null slots zeroed.

The hashes form a tree: chunk → column → snapshot. `snapshot_hash` covers
content, schema, scope, cleaning rules version, cleaning log, and rejects. It
does not cover fetch timestamps or tool version; `manifest_hash` covers those.

## Consequences
- Two fetches with identical content get the same hash. Changing the writer
  or its version doesn't change the hash.
- A single chunk can be verified on its own, which allows streaming or range
  loading.
- The hasher reads logical values, not file bytes. It costs one extra pass,
  but BLAKE3 runs at several GB/s, so this is negligible.
- The encoding is versioned (`v1` in each context string). Changing it means
  a new format version. A known-answer test pins the chunk encoding.
