// Parity test: the single-threaded WASM build must produce exactly what the
// native engine produces for the same snapshot and plan.
//
//   node web/engine-test/parity.mjs <snapshot dir> [plan.json]
//
// Needs web/public/pkg/st (tools/wasm/build.sh st) and target/release/receipts-engine
// (cargo build --release -p receipts-wasm --bin receipts-engine). Node
// built-ins only.

import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import assert from "node:assert/strict";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..", "..");
const [dir, planPath = join(here, "noise-by-borough.json")] = process.argv.slice(2);
if (!dir) {
  console.error("usage: node parity.mjs <snapshot dir> [plan.json]");
  process.exit(2);
}

const { initSync, Engine } = await import(join(root, "web/public/pkg/st/receipts_wasm.js"));
initSync({ module: readFileSync(join(root, "web/public/pkg/st/receipts_wasm_bg.wasm")) });

// Timings differ between runs; everything else must match exactly.
const strip = (v) =>
  JSON.parse(JSON.stringify(v, (k, x) => (/(^ms$|_ms$|^timings$)/.test(k) ? undefined : x)));

const native = JSON.parse(
  execFileSync(join(root, "target/release/receipts-engine"), [dir, planPath, "0"], {
    maxBuffer: 1 << 28,
  }).toString(),
);

const engine = new Engine();
const t0 = performance.now();
const load = JSON.parse(
  engine.loadSnapshot(
    readFileSync(join(dir, "manifest.json"), "utf8"),
    readFileSync(join(dir, "data.arrow")),
    readFileSync(join(dir, "cleaning_log.arrow")),
    readFileSync(join(dir, "rejects.arrow")),
  ),
);
const loadMs = performance.now() - t0;
const plan = readFileSync(planPath, "utf8").replaceAll("$SNAPSHOT", load.snapshot_hash);
const run = JSON.parse(engine.run(plan));
const trace = JSON.parse(engine.traceBack(run.execution, 0));
const rows = engine.traceBackRows(run.execution, 0);

assert.deepEqual(strip(load), strip(native.load), "load report differs");
assert.deepEqual(strip(run), strip(native.run), "run result differs");
assert.deepEqual(strip(trace), strip(native.trace), "trace differs");
assert.deepEqual(Array.from(rows.slice(0, 5)), native.trace_first_rows, "trace rows differ");

// Counterfactual round trip inside WASM: excluding a group's rows removes it.
const cf = JSON.parse(engine.exclude(run.execution, rows));
assert.equal(cf.ok, true);
assert.equal(cf.output.total_rows, run.output.total_rows - 1, "the excluded group should disappear");
assert.equal(cf.method.kind, "incremental");

// Errors come back as thrown JS errors with a sentence.
assert.throws(() => engine.traceBack(run.execution, 1e6), /past the output/);
const bad = JSON.parse(engine.validatePlan(plan.replace('"borough"]', '"borugh"]')));
assert.equal(bad.ok, false);

console.log(
  JSON.stringify({
    ok: true,
    rows: load.rows,
    wasm_load_ms: Math.round(loadMs),
    wasm_run_ms: Math.round(run.ms),
    native_load_ms: Math.round(native.load_ms),
    native_run_ms: Math.round(native.run.ms),
  }),
);
