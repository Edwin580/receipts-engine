# Receipts

Click a number computed from New York City 311 data and see the requests it
came from, every step in between, and what the number becomes if you leave
some of them out. The query engine is my own, written in Rust and compiled
to WebAssembly, and it runs in your browser.

**Try it: https://edwin580.github.io/receipts-engine/**

The demo loads one month of real 311 requests (July 2025, 315,883 rows,
10.7 MB) so it stays quick on a phone. Nothing to install, no account.
On the page you can:

- change the question (mean or median hours to close, or a count, for any
  common complaint type, by borough, agency or channel) and watch the plan
  re-run;
- click any number to trace it back to its source rows, with each step's
  row count and a link from every request to the same record in the city's
  API;
- drag a slider to leave out the requests that move the number most, and
  see the exact new value;
- flip bits in the data file and watch the engine refuse to load it.

## Run the demo locally

```
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.128 --locked
tools/wasm/build.sh site               # builds web/site/pkg
python3 -m http.server -d web/site 8000   # then open http://localhost:8000
node --test web/site/test/*.test.mjs   # page logic and the real snapshot
```

The page is static files: `web/site/index.html`, `app.js` (talks to the
engine), `lib.js` (builds plans from the controls; no DOM), `style.css`,
the ET Book fonts, and the demo snapshot in `web/site/data/`. It has no
JavaScript dependencies and no bundler. `.github/workflows/pages.yml`
builds the WASM package and publishes `web/site` on every push to `main`.

The demo snapshot was made with the CLI below:

```
receipts-snapshot fetch --out raw/jul2025 --from 2025-07-01 --to 2025-08-01
receipts-snapshot build --raw raw/jul2025 --out snapshots/jul2025
```

The page uses the single threaded build because GitHub Pages can't send the
COOP/COEP headers that WASM threads need.

## Status

M0 (snapshot pipeline) implemented and verified against the
live Socrata API on 2026-09-24: 7,111,809 rows, snapshot `3b42e46a…` (see
`docs/benchmarks/m0.md`). M1 (plans), M2 (lineage) and M3 (counterfactuals) merged. M4 (the engine
in the browser: WASM, verify-on-load, threads) implemented, awaiting
review (see `docs/engine/wasm-api.md`, `docs/benchmarks/m4.md`). The demo
page in `web/site` stands in for the M5 frontend.

## Snapshot CLI

```
cargo build --release -p receipts-snapshot
B=target/release/receipts-snapshot

$B fetch  --out raw/nyc311                    # 2024-01-01..2026-01-01; set SOCRATA_APP_TOKEN to avoid throttling
$B build  --raw raw/nyc311 --out snapshots/nyc311
$B verify snapshots/nyc311/<hash16>

$B synth  --out raw/synth --rows 1000000      # synthetic 311-like data, no network needed
python3 tools/oracle/snapshot_oracle.py raw/synth snapshots/synth/<hash16>   # needs pyarrow
```

Docs: `docs/snapshot/schema.md` (format and hashing),
`docs/snapshot/cleaning-rules.md`, `docs/benchmarks/m0.md`.

## Engine (M1)

Plans are built in Rust or loaded from JSON (`docs/engine/plan.md`),
validated against a snapshot's schema, hashed, described in plain English,
and executed natively:

Every execution also records lineage, so any output row can be traced
back to its source rows, and any source row forward to the results it
feeds (`docs/engine/lineage.md`). Counterfactuals re-derive a result
without chosen rows, exactly (`docs/engine/counterfactuals.md`).

```
cargo run --release -p receipts-bench --bin m1 -- snapshots/nyc311/<hash16>   # plans
cargo run --release -p receipts-bench --bin m2 -- snapshots/nyc311/<hash16>   # lineage
cargo run --release -p receipts-bench --bin m3 -- snapshots/nyc311/<hash16>   # counterfactuals
```

## Engine in the browser (M4)

```
tools/wasm/build.sh                     # web/pkg/st (stable) and web/pkg/mt (threads; pinned nightly)
node web/engine-test/serve.mjs snapshots/nyc311/<hash16>     # then open /engine-test/bench.html?build=mt
node web/engine-test/parity.mjs snapshots/nyc311/<hash16>    # WASM == native
```

Needs `rustup target add wasm32-unknown-unknown`, the nightly in
`tools/wasm/build.sh` (with `rust-src`), and `wasm-bindgen-cli` at the
version pinned in `Cargo.toml`.

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
  receipts-bench/     benchmarks on real snapshots                 (M1)
web/
  site/               the demo page (static; see above)
  engine-test/        browser benchmark, parity test, dev server   (M4)
  pkg/                WASM builds (generated, not committed)
                      React + TS frontend                          (M5)
tools/wasm/           build script for both WASM packages          (M4)
docs/
  adr/                architecture decision records
  snapshot/           snapshot schema, manifest example, cleaning rules
  engine/             plans (M1), lineage (M2), counterfactuals (M3), WASM API (M4)
  benchmarks/         measured numbers per milestone
```

## Dependencies

Every dependency is proposed before it's added. Approved for M0:

| Crate | Used by | Why |
|---|---|---|
| `blake3` | core, snapshot | Content hashing (required by the spec). |
| `serde_json` | core, plan, snapshot | Canonical JSON (core); plan JSON (plan); Socrata JSON and the manifest (snapshot). The `float_roundtrip` feature is on (M1, ADR 0008) so decimals parse exactly. |
| `arrow-array`, `arrow-buffer`, `arrow-schema`, `arrow-ipc` | snapshot | Writing and verifying Arrow IPC natively. The WASM reader is decided in M4 (ADR 0001). |
| `ureq` (rustls) | snapshot | Small, synchronous HTTPS client for Socrata paging. |
| `serde` | snapshot | Manifest and `fetch.json` structs. |
| `clap` | snapshot | CLI arguments. |
| `anyhow` | snapshot | Error context in the offline CLI. |
| `proptest` (dev) | core | Property tests. |

Approved for M4 (2026-09-25):

| Crate | Used by | Why |
|---|---|---|
| `wasm-bindgen` (pinned `=0.2.128`) | wasm | JS bindings; pinned so `wasm-bindgen-cli` matches. |
| `lz4_flex` (M5) | core; also via `arrow-ipc`'s `lz4` feature | LZ4 frame decoding of snapshot buffers in WASM (ADR 0001). |
| `rayon` | core, exec, wasm (feature `parallel`) | Deterministic data parallelism (ADR 0005). |
| `wasm-bindgen-rayon` | wasm (wasm32 + `parallel`) | Rayon's thread pool on Web Workers. |

`roaring` was proposed for M0 but wasn't needed: the manifest counts rule
applications, and the cleaning log lists rows directly. It will come with
lineage in M2.

Python dev tooling (not a build dependency): `pyarrow`, for the differential
oracle.

## Checks

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node --test web/site/test/*.test.mjs   # after tools/wasm/build.sh site
```
