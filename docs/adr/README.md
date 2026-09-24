# Architecture Decision Records

Short records of non-obvious decisions. Copy `0000-template.md` to add one.
Status values: Proposed → Accepted → (Superseded by NNNN).

| # | Title | Status |
|---|---|---|
| 0001 | Snapshot storage: Arrow IPC file, fixed 64Ki-row chunks, no Parquet | Proposed |
| 0002 | Content hashing over a canonical logical encoding, not IPC bytes | Proposed |
| 0003 | Timestamps are naive NYC wall-clock microseconds | Proposed |
| 0004 | Snapshots coerce, plans judge (minimal cleaning, everything logged) | Proposed |
| 0005 | WASM threads need a pinned nightly toolchain for one build target | Proposed (decide at M4) |
