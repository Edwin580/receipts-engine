// The demo's plans against the real engine and the published demo snapshot.
// Needs web/site/pkg (tools/wasm/build.sh site).
//   node --test web/site/test/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { buildPlan, topTypesPlan, METRICS, GROUPS, findGroup, flipBit, hoursBetween } from "../lib.js";

const site = join(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = join(site, "pkg");
if (!existsSync(join(pkg, "receipts_wasm_bg.wasm"))) {
  throw new Error("web/site/pkg is missing: run tools/wasm/build.sh site");
}
const { initSync, Engine } = await import(join(pkg, "receipts_wasm.js"));
initSync({ module: readFileSync(join(pkg, "receipts_wasm_bg.wasm")) });

const data = (f) => readFileSync(join(site, "data", f));
const files = [data("manifest.json").toString(), data("data.arrow"), data("cleaning_log.arrow"), data("rejects.arrow")];
const engine = new Engine();
const report = JSON.parse(engine.loadSnapshot(...files));
const S = report.snapshot_hash;
const run = (plan) => JSON.parse(engine.run(JSON.stringify(plan)));

test("the demo snapshot verifies and matches its manifest", () => {
  const m = JSON.parse(files[0]);
  assert.equal(S, m.snapshot_hash);
  assert.equal(report.rows, m.row_count);
});

const types = run(topTypesPlan(S, 15)).output.rows.map((r) => r[0]);

test("the type menu lists the most common types, most first", () => {
  const r = run(topTypesPlan(S, 15));
  assert.equal(r.output.rows.length, 15);
  for (let i = 1; i < 15; i++) assert.ok(r.output.rows[i - 1][1] >= r.output.rows[i][1]);
});

test("every combination of controls gives a valid plan whose groups add up", () => {
  for (const type of [null, ...types.slice(0, 4)]) {
    for (const groupBy of Object.keys(GROUPS)) {
      for (const metric of Object.keys(METRICS)) {
        for (const dropBackwards of [true, false]) {
          const q = { type, groupBy, metric, dropBackwards };
          const r = run(buildPlan(S, q));
          assert.ok(r.ok, `${JSON.stringify(q)}: ${r.error}`);
          const agg = r.steps.length - 2;
          const total = r.output.rows.reduce((a, row) => a + row[1], 0);
          assert.equal(total, r.steps[agg - 1].rows, JSON.stringify(q));
          engine.dropExecution(r.execution);
        }
      }
    }
  }
});

test("a traced mean equals the mean of its receipts", () => {
  const r = run(buildPlan(S, { type: types[1], groupBy: "borough", metric: "mean", dropBackwards: true }));
  const ids = engine.traceBackRows(r.execution, 0);
  assert.equal(ids.length, r.output.rows[0][1]);
  const recs = JSON.parse(engine.sourceRecords(S, ids));
  const c = (n) => recs.columns.indexOf(n);
  const hours = recs.rows.map((row) => hoursBetween(row[c("created_date")], row[c("closed_date")]));
  assert.ok(recs.rows.every((row) => row[c("borough")] === r.output.rows[0][0] && row[c("complaint_type")] === types[1]));
  const mean = hours.reduce((a, h) => a + h, 0) / hours.length;
  assert.ok(Math.abs(mean - r.output.rows[0][2]) < 1e-9, `${mean} vs ${r.output.rows[0][2]}`);
});

test("leaving out the top movers matches a plan that filters them out", () => {
  const q = { type: types[0], groupBy: "borough", metric: "median", dropBackwards: true };
  const r = run(buildPlan(S, q));
  const i = 1;
  const key = r.output.rows[i][0];
  const mv = JSON.parse(engine.contributions(r.execution, i, "median_hours", 10));
  const rows = new Uint32Array(mv.top.slice(0, 5).map((t) => t.source_row));
  const cf = JSON.parse(engine.exclude(r.execution, rows));
  const after = cf.output.rows[findGroup(cf.output.rows, key)];

  // The same exclusion as a filter on unique_key, run from scratch.
  const recs = JSON.parse(engine.sourceRecords(S, rows));
  const keys = recs.rows.map((row) => ({ i64: String(row[0]) }));
  const plan = buildPlan(S, q);
  plan.nodes.splice(1, 0, { op: "filter", input: 0, predicate: { call: "not", args: [{ call: "in", args: [{ column: "unique_key" }], values: keys }] } });
  plan.nodes.forEach((n, j) => { if (j > 1) n.input = j - 1; });
  plan.output += 1;
  const fresh = run(plan);
  assert.ok(fresh.ok, fresh.error);
  assert.deepEqual(after, fresh.output.rows[findGroup(fresh.output.rows, key)]);
});

test("a flipped bit is refused with a sentence, never a crash", () => {
  const n = files[1].length;
  for (const off of [0, 1000, Math.floor(n / 2), n - 1]) {
    const e = new Engine();
    try {
      e.loadSnapshot(files[0], flipBit(files[1], off), files[2], files[3]);
    } catch (err) {
      assert.match(err.message, /^[\w.]+: /, `offset ${off}: ${err.message}`);
      assert.doesNotMatch(err.message, /Error\(| \{ /);
    } finally {
      e.free();
    }
  }
});
