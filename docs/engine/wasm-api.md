# The engine in the browser (WASM API)

Status: **implemented (M4)** in `crates/receipts-wasm`, with the IPC reader
in `receipts-core::ipc`. Decisions: ADR 0001 (reader), ADR 0005 (threads),
ADR 0011 (verify on load).

## 1. Builds

`tools/wasm/build.sh` produces two packages with `wasm-bindgen --target web`:

| Package | Toolchain | Threads | Size | Needs |
|---|---|---|---|---|
| `web/public/pkg/st` | pinned stable (1.94.1) | 1 | 0.78 MB | any modern browser |
| `web/public/pkg/mt` | pinned `nightly-2026-09-20` + `build-std` | rayon pool via `wasm-bindgen-rayon` | 1.27 MB | cross-origin isolation (`COOP: same-origin`, `COEP: require-corp`) |

Pick `mt` when `crossOriginIsolated` is true and fall back to `st`
otherwise. Both give **identical** results: parallel code only splits
work whose result is order-independent (chunk hashing, batch decoding,
expression evaluation, filter selection, stable sort). Aggregation stays
sequential, so decimal sums round the same way. CI runs the property
tests with and without the `parallel` feature.

The threaded build needs shared memory, which current nightlies no longer
turn on implicitly with `+atomics`. The build script passes
`--shared-memory --import-memory --max-memory=4GiB` and the TLS exports
explicitly.

## 2. API

All structured results are JSON strings. Row lists are `Uint32Array`s.
Errors are thrown as JS `Error`s with a plain-English message.

```js
import init, { Engine, initThreadPool } from "./pkg/mt/receipts_wasm.js";
await init();
await initThreadPool(navigator.hardwareConcurrency);   // mt only

const engine = new Engine();
const report = JSON.parse(engine.loadSnapshot(manifestText, dataBytes, logBytes, rejectsBytes));
// {snapshot_hash, title, rows, columns, known_issues, verify_ms, timings}

const v = JSON.parse(engine.validatePlan(planJson));
// {ok, plan_hash, steps: [{op, sentence, node_hash, columns}]} or {ok: false, error, step}

const run = JSON.parse(engine.run(planJson));
// {ok, execution, plan_hash, ms, steps: [{sentence, rows}], output: {columns, total_rows, rows (first 500)}}
engine.rows(run.execution, 500, 500);                  // more output rows

engine.traceBack(run.execution, row);                  // {source, ms, path: [{step, rows}], source_rows}
const ids = engine.traceBackRows(run.execution, row);  // Uint32Array of snapshot rows
engine.sourceRecords(report.snapshot_hash, ids.subarray(0, 20));  // the receipts

const cf = JSON.parse(engine.exclude(run.execution, ids.subarray(0, 1)));
// like run, plus {method: {kind: unchanged|incremental|rerun, ...}, excluded}
// Excluding from a counterfactual composes: its base execution minus the union.

engine.contributions(run.execution, row, "median_hours", 10);
// {current, exact, contributing_rows, rows_that_change_it, top: [{source_row, without, removes_group}]}

engine.matchingRows(planJson);   // Uint32Array: snapshot rows behind a plan's whole output (e.g. a known-issue filter)
engine.dropExecution(run.execution);   // executions hold their tables: drop the ones no longer shown
```

Cells are rendered for JavaScript. Integers beyond 2^53 become strings,
timestamps become `YYYY-MM-DDTHH:MM:SS.ffffff` strings, locations become
`[lat, lon]`, and missing values become `null`.

## 3. Verify on load (ADR 0011)

`loadSnapshot` trusts nothing it was given. Before the engine will use a
snapshot, it re-derives every hash in `docs/snapshot/schema.md` §6 from the
bytes:
- the manifest hash;
- every chunk, dictionary and column hash;
- the cleaning-log and rejects hashes;
- the `snapshot_hash` itself.

It also checks the sort order that `RowId`s depend on. A flipped byte, an
edited manifest, swapped side files or a truncated download are all
refused with a sentence saying what didn't match. The reader is a
dependency-free Arrow IPC decoder. Every read is bounds-checked, and every
size taken from the file is overflow-checked.

## 4. Testing

- `crates/receipts-wasm/tests/engine.rs` covers load, tamper detection,
  validation messages, runs, traces, composed exclusions and
  contributions, natively.
- `crates/receipts-snapshot/tests/ipc_reader.rs` checks that the reader
  decodes exactly what `arrow-ipc` writes: every column, and every hash in
  the manifest. It also runs corrupted and truncated files that must error,
  not panic, and a regression test for absurd lengths (a bug the corruption
  test found).
- `web/engine-test/parity.mjs` (Node) checks that the stable WASM build's
  load report, run output, plan hash and trace equal the native engine's
  (`receipts-engine` CLI), byte for byte after removing timings. It
  passes on the real 7.1M-row snapshot.
- `web/engine-test/browser-bench.mjs` runs `bench.html` in headless Chrome
  through the DevTools protocol, for both builds. It uses only Node
  built-ins. CI runs parity and both browser builds on an 80k-row
  synthetic snapshot.
