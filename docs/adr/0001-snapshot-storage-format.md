# 0001. Snapshot storage: Arrow IPC file, fixed 64Ki-row chunks, no Parquet

- Status: Proposed
- Date: 2026-09-24
- Milestone: M0

## Context
The spec allows "Arrow IPC/Parquet". The browser loader has to reach the
2-second warm-load budget, and it is our own Rust→WASM code. Parquet needs
page decoding (RLE/bit-packing, dictionary pages, and usually a codec), and
that work happens on the critical path in WASM. Arrow IPC buffers already have
the in-memory layout we want, so they can be copied or viewed directly.

## Decision
- One Arrow IPC **file** (not stream) per snapshot, so the footer gives
  random access to each record batch.
- Each record batch holds exactly 65,536 rows, except the last. The chunk
  number is `row_index >> 16`.
- One global dictionary per string column, sorted by byte order, with `u32`
  codes.
- No body compression in M0. We measure the cold and warm load in M4, then
  choose between LZ4 frames (requires a pure-Rust decoder in WASM) and relying
  on HTTP `Content-Encoding` from the static host.

## Consequences
- Load is mostly memcpy into WASM memory. Transfer size is larger: about
  300 MB raw for 5M rows. The warm-cache budget is unaffected; cold loads will
  need compression.
- Code-order = string-order makes dictionary sort, min, and max cheap, but
  adding a string means rebuilding the dictionary. That's fine for immutable
  snapshots.
- Parquet export can be added later for interop. It is not the engine format.
- The reader in WASM may be a small custom IPC reader instead of `arrow-ipc`,
  depending on binary size. That decision belongs to M4.
