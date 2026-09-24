# Receipts

Row-level provenance for public data. Click a number derived from NYC open
data and see the source rows that produced it, every transform in between,
and what the number becomes if you exclude any of those rows. The engine is
custom Rust compiled to WASM and runs entirely in the browser.

**Status:** M0 (snapshot pipeline), awaiting review of the schema and
cleaning rules.

## Layout

```
crates/
  receipts-core/      column store, types, RowId, hashing          (M0–M1)
  receipts-plan/      logical plan DAG, validation, plan hashing   (M1)
  receipts-exec/      operators + lineage capture                  (M1–M3)
  receipts-lineage/   LineageStore, backward/forward tracing       (M2)
  receipts-cf/        counterfactual / deletion propagation        (M3)
  receipts-circuit/   how-provenance circuits (expert mode)        (M7)
  receipts-wasm/      wasm-bindgen API surface                     (M4)
  receipts-snapshot/  offline snapshot CLI                         (M0)
  receipts-bench/     criterion benches                            (M1)
web/                  React + TS frontend                          (M5, not created yet)
docs/
  adr/                architecture decision records
  snapshot/           snapshot schema, manifest example, cleaning rules
```

## Dependencies

Every dependency is proposed before it's added. None have been added yet.
Proposed for M0 (`receipts-snapshot` / `receipts-core`):

| Crate | Why |
|---|---|
| `blake3` | Content hashing (required by the spec). |
| `arrow-array`, `arrow-schema`, `arrow-ipc` | Writing Arrow IPC natively. Only the writer side. The WASM reader is decided in M4 (ADR 0001). |
| `ureq` (rustls) | Small, synchronous HTTPS client for Socrata paging. Avoids pulling in tokio just for an offline CLI. |
| `serde`, `serde_json` | Parsing Socrata JSON and writing the manifest. |
| `roaring` | Per-rule affected-row bitmaps (and the engine's lineage later). |
| `clap` | CLI arguments. |
| `anyhow` | Error context in the CLI binary only. Libraries use typed errors. |

## Checks

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
