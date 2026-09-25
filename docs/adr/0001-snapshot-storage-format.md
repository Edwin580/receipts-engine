# 0001. Snapshot storage: Arrow IPC file, fixed 64Ki-row chunks, no Parquet

- Status: Accepted (M0 review, 2026-09-24)
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

## M4 update (2026-09-25)
- **Reader:** a small custom IPC reader (`receipts-core::ipc`, no
  dependencies) instead of `arrow-ipc` in WASM. The whole engine is
  0.78 MB. Tests check that it decodes exactly what `arrow-ipc` writes.
- **Warm load:** 1.65–1.80 s for 7.1M rows with the threaded build,
  including re-hashing everything (ADR 0011), so the 2 s budget holds.
  The single-threaded fallback takes 3.2–3.6 s.
- **Compression is still open.** It depends on the M5 static host
  (`Content-Encoding` from the host, or LZ4 decoded in WASM). A cold load
  of 497 MB uncompressed isn't acceptable over a real network.

## M5 update (2026-09-25): LZ4 inside the files
- **Decision:** Arrow IPC buffer compression with `LZ4_FRAME`, written by
  `arrow-ipc` (its `lz4` feature) and decoded by `receipts-core::ipc` with
  `lz4_flex` (pure Rust, so it works in WASM). It is the default for
  `build`; `--compression none` still works. This works on any host,
  including Cloudflare R2, which doesn't compress binary objects on its own.
- The snapshot hash covers logical content, so compression doesn't change
  it (the real snapshot keeps `3b42e46a…`). `manifest.files[]` gains a
  `compression` field, so the manifest hash does change.
- **Real snapshot:** `data.arrow` shrinks from 497 MB to **239 MB** (48%).
  Threaded warm load gets *faster* (1.20–1.35 s, from 1.65–1.80 s), because
  there are fewer bytes to copy and decompression runs in parallel with the
  batch decode. The single-threaded fallback gets a little slower
  (3.6–4.2 s, from 3.2–3.6 s). Memory drops by 0.2 GB.
- **Still large for a cold load:** 239 MB is about 10 s at 200 Mbit/s. gzip
  on top of LZ4 would bring it to 174 MB. Uploading the objects to R2
  with `Content-Encoding: gzip` would let browsers undo that transparently,
  which is an easy later win. Loading only the columns a plan needs is a
  larger one.
