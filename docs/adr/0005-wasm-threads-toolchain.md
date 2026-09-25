# 0005. WASM threads need a pinned nightly toolchain for one build target

- Status: **Accepted (M4, 2026-09-25): option 2, both builds**
- Date: 2026-09-24 (proposed), 2026-09-25 (accepted and implemented)
- Milestone: M4

## Context
The spec requires "Rust (stable)" and also `wasm-bindgen-rayon`. For threads,
the standard library itself has to be built with
`-C target-feature=+atomics,+bulk-memory`. On `wasm32-unknown-unknown` that
requires `-Z build-std`, and `-Z build-std` only exists on nightly.

## Options
1. Everything on stable. Ship a single-threaded WASM build and do data
   parallelism another way (several workers, each with its own module
   instance and a partition of the data). This is much more work and gives
   worse memory sharing.
2. **(Chosen)** All crates stay stable-compatible, and CI checks this.
   Only the threaded `receipts-wasm` artifact is built with a pinned nightly
   (a dated `nightly-YYYY-MM-DD`). A stable single-threaded build is
   maintained as a fallback, which we need anyway for browsers without
   cross-origin isolation.
3. Wait for build-std to reach stable. There is no date for that.

## Decision (as implemented)
- `web/public/pkg/st` uses pinned stable. `web/public/pkg/mt` uses `nightly-2026-09-20`
  with `-Z build-std=panic_abort,std`, the `parallel` feature, and
  `wasm-bindgen-rayon` 1.3. `tools/wasm/build.sh` builds both, and CI
  builds both and runs both in headless Chrome.
- New dependencies, approved for M4: `wasm-bindgen` (pinned `=0.2.128`, so
  the CLI matches), `rayon` 1.12, and `wasm-bindgen-rayon` 1.3 (wasm32
  only, behind `parallel`).
- Parallelism is limited to order-independent work, so both builds give
  identical results (`docs/engine/wasm-api.md` §1).

## Consequences
- On 7.1M real rows in headless Chrome with 4 cores, the threaded build
  loads and verifies in about 1.7 s, against about 3.4 s single-threaded.
  Full sorts take 2.2 s against 5.7 s. Typical plans gain 10–30%
  (`docs/benchmarks/m4.md`).
- The nightly build needs explicit linker flags for shared memory and TLS
  exports. It also prints a warning that `+atomics` is an unstable target
  feature "being phased out". When that becomes a hard error, re-pin the
  nightly and follow the replacement flag (rust-lang/rust#162235).
- Serving the threaded build requires COOP/COEP headers.
  `web/engine-test/serve.mjs` shows them. The static host chosen in M5
  must send them, or the app falls back to `st`.
