# 0005. WASM threads need a pinned nightly toolchain for one build target

- Status: Proposed (decide at M4; flagged now because it conflicts with the spec)
- Date: 2026-09-24
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
2. **(Recommended)** All crates stay stable-compatible, and CI checks this.
   Only the threaded `receipts-wasm` artifact is built with a pinned nightly
   (a dated `nightly-YYYY-MM-DD`). A stable single-threaded build is
   maintained as a fallback, which we need anyway for browsers without
   cross-origin isolation.
3. Wait for build-std to reach stable. There is no date for that.

## Consequences (option 2)
- There are two WASM builds to test. The engine code itself doesn't change,
  because rayon falls back to sequential execution.
- The nightly pin is recorded in `rust-toolchain` overrides for that build
  only.
