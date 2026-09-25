# Architecture Decision Records

Short records of non-obvious decisions. Copy `0000-template.md` to add one.
Status values: Proposed → Accepted → (Superseded by NNNN).

| # | Title | Status |
|---|---|---|
| 0001 | Snapshot storage: Arrow IPC file, fixed 64Ki-row chunks, no Parquet | Accepted |
| 0002 | Content hashing over a canonical logical encoding, not IPC bytes | Accepted |
| 0003 | Timestamps are naive NYC wall-clock microseconds | Accepted |
| 0004 | Snapshots coerce, plans judge (minimal cleaning, everything logged) | Accepted |
| 0005 | WASM threads need a pinned nightly toolchain for one build target | Proposed (decide at M4) |
| 0006 | Split the snapshot pipeline into `fetch` and `build` via a raw directory | Accepted |
| 0007 | Keyset pagination on Socrata's `:id`, not `unique_key` or offsets | Accepted (verified live 2026-09-24) |
| 0008 | Plan model: linear typed operators, SQL null semantics, Merkle plan hashes | Proposed (M1) |
| 0009 | Lineage: always-on per-step mappings, composed on demand | Proposed (M2) |
