// The Westchester page's plans against the engine and the towns' snapshot
// in web/site/westchester/data: synthetic in CI, the real one at deploy.
//   node --test web/site/test/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { waitPlan, townsPlan, townListPlan, countsPlan, listPlan, topTypesPlan, ORDERS, hoursBetween } from "../lib.js";

const site = join(dirname(fileURLToPath(import.meta.url)), "..");
const dir = join(site, "westchester", "data");
const TOWNS = ["Mount Pleasant", "Greenburgh", "Mount Vernon", "North Castle", "Peekskill"];

if (!existsSync(join(dir, "manifest.json"))) {
  test("Westchester data", { skip: "no snapshot in web/site/westchester/data (receipts-snapshot synth/fetch --source westchester)" }, () => {});
} else {
  const { initSync, Engine } = await import(join(site, "pkg", "receipts_wasm.js"));
  initSync({ module: readFileSync(join(site, "pkg", "receipts_wasm_bg.wasm")) });
  const f = (n) => readFileSync(join(dir, n));
  const engine = new Engine();
  const report = JSON.parse(engine.loadSnapshot(f("manifest.json").toString(), f("data.arrow"), f("cleaning_log.arrow"), f("rejects.arrow")));
  const S = report.snapshot_hash;
  const run = (plan) => {
    const r = JSON.parse(engine.run(JSON.stringify(plan)));
    assert.ok(r.ok, r.error);
    return r;
  };
  const rows = (plan) => {
    const r = run(plan);
    engine.dropExecution(r.execution);
    return r.output.rows;
  };
  const towns = rows(townListPlan(S)).map((r) => r[0]);
  const types = rows(topTypesPlan(S, 25)).map((r) => r[0]);

  test("the snapshot is the towns' and lists only those towns", () => {
    const m = JSON.parse(f("manifest.json"));
    assert.equal(m.source.source_id, 2);
    assert.ok(towns.length > 0);
    for (const t of towns) assert.ok(TOWNS.includes(t), t);
  });

  test("every question runs and the counts are consistent", () => {
    for (const type of types.slice(0, 8)) {
      let sum = 0;
      for (const area of [{ kind: "city" }, ...towns.map((value) => ({ kind: "town", value }))]) {
        const [closed, med, p90] = rows(waitPlan(S, { type, area }))[0];
        const [filed, withDate] = rows(countsPlan(S, { type, area }))[0];
        assert.ok(filed >= withDate && withDate >= closed);
        assert.equal(med === null, closed === 0);
        if (med !== null) assert.ok(p90 >= med);
        if (area.kind === "town") sum += closed;
      }
      // The towns add up to the whole, and so does the comparison table.
      const all = rows(waitPlan(S, { type, area: { kind: "city" } }))[0][0];
      assert.equal(sum, all, type);
      assert.equal(rows(townsPlan(S, type)).reduce((a, r) => a + r[1], 0), all, type);
    }
  });

  test("a town's median matches the requests it traces back to", () => {
    const q = { type: types[0], area: { kind: "town", value: towns[0] } };
    const r = run(waitPlan(S, q));
    const [closed, med] = r.output.rows[0];
    const recs = JSON.parse(engine.sourceRecords(S, engine.traceBackRows(r.execution, 0)));
    engine.dropExecution(r.execution);
    const c = (n) => recs.columns.indexOf(n);
    assert.equal(recs.rows.length, closed);
    assert.ok(recs.rows.every((row) => row[c("town")] === towns[0] && row[c("complaint_type")] === types[0]));
    const h = recs.rows.map((row) => hoursBetween(row[c("created_date")], row[c("closed_date")])).sort((a, b) => a - b);
    const m = h.length % 2 ? h[h.length >> 1] : (h[h.length / 2 - 1] + h[h.length / 2]) / 2;
    assert.ok(Math.abs(m - med) < 1e-9, `${m} vs ${med}`);
  });

  test("request lists stay in their town", () => {
    for (const order of Object.keys(ORDERS)) {
      const r = run(listPlan(S, { type: null, area: { kind: "town", value: towns[0] } }, order, 50));
      const t = r.output.columns.findIndex((c) => c.name === "town");
      assert.ok(r.output.rows.every((row) => row[t] === towns[0]));
      engine.dropExecution(r.execution);
    }
  });
}
