// The page's plans against the real engine and the published snapshot.
// Needs web/site/pkg (tools/wasm/build.sh site).
//   node --test web/site/test/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import {
  BOROUGHS, waitPlan, comparePlan, countsPlan, listPlan, topTypesPlan, districtsPlan, ORDERS, parseDistrict, hoursBetween,
  keyPlan, fasterPlan, questionFor, whyNotCounted,
} from "../lib.js";

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
const median = (xs) => {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};

test("the snapshot verifies and matches its manifest", () => {
  const m = JSON.parse(files[0]);
  assert.equal(S, m.snapshot_hash);
  assert.equal(report.rows, m.row_count);
});

const types = rows(topTypesPlan(S, 30)).map((r) => r[0]);
const districts = rows(districtsPlan(S)).map((r) => r[0]).filter(parseDistrict);

test("the menus: 30 types most first, and every borough has districts", () => {
  const t = rows(topTypesPlan(S, 30));
  assert.equal(t.length, 30);
  for (let i = 1; i < t.length; i++) assert.ok(t[i - 1][1] >= t[i][1]);
  for (const b of BOROUGHS) assert.ok(districts.some((d) => parseDistrict(d).borough === b), b);
});

test("every question runs, and the counts are consistent", () => {
  const areas = [
    { kind: "city" },
    ...BOROUGHS.map((value) => ({ kind: "borough", value })),
    ...districts.slice(0, 10).map((value) => ({ kind: "district", value })),
    { kind: "zip", value: "10451" },
    { kind: "zip", value: "00000" },
  ];
  for (const type of types.slice(0, 6)) {
    for (const area of areas) {
      const q = { type, area };
      const [closed, med, p90] = rows(waitPlan(S, q))[0];
      const [filed, withDate] = rows(countsPlan(S, q))[0];
      assert.ok(filed >= withDate && withDate >= closed, JSON.stringify({ q, filed, withDate, closed }));
      assert.equal(med === null, closed === 0);
      if (med !== null) assert.ok(med >= 0 && p90 >= med, JSON.stringify({ q, med, p90 }));
    }
  }
});

test("a district's median and 90th percentile match the complaints it traces back to", () => {
  const q = { type: "Noise - Residential", area: { kind: "district", value: "04 BRONX" } };
  const r = run(waitPlan(S, q));
  const [closed, med, p90] = r.output.rows[0];
  const ids = engine.traceBackRows(r.execution, 0);
  assert.equal(ids.length, closed);
  const recs = JSON.parse(engine.sourceRecords(S, ids));
  const c = (n) => recs.columns.indexOf(n);
  assert.ok(recs.rows.every((row) => row[c("community_board")] === "04 BRONX" && row[c("complaint_type")] === q.type));
  const hours = recs.rows.map((row) => hoursBetween(row[c("created_date")], row[c("closed_date")]));
  assert.ok(hours.every((h) => h >= 0));
  assert.ok(Math.abs(median(hours) - med) < 1e-9, `${median(hours)} vs ${med}`);
  // Nearest rank: at least 90% at or below it, and it is one of the times.
  const sorted = [...hours].sort((a, b) => a - b);
  const exact = sorted[Math.ceil(0.9 * sorted.length) - 1];
  assert.ok(Math.abs(exact - p90) < 1e-9, `${exact} vs ${p90}`);
  assert.ok(sorted.filter((h) => h <= p90 + 1e-9).length >= 0.9 * sorted.length);
  engine.dropExecution(r.execution);
});

test("the comparison table adds up to the borough", () => {
  for (const type of types.slice(0, 3)) {
    for (const borough of BOROUGHS) {
      const total = rows(comparePlan(S, { type, borough })).reduce((a, r) => a + r[1], 0);
      assert.equal(total, rows(waitPlan(S, { type, area: { kind: "borough", value: borough } }))[0][0], `${type} ${borough}`);
    }
    const byBorough = rows(comparePlan(S, { type, borough: null }));
    for (let i = 1; i < byBorough.length; i++) assert.ok(byBorough[i - 1][2] >= byBorough[i][2]);
  }
});

test("complaint lists are in order and match the answer's complaints", () => {
  const q = { type: "Noise - Residential", area: { kind: "district", value: "04 BRONX" } };
  const [closed] = rows(waitPlan(S, q))[0];
  for (const order of Object.keys(ORDERS)) {
    const r = run(listPlan(S, q, order, 400));
    const col = Object.fromEntries(r.output.columns.map((c, i) => [c.name, i]));
    const list = r.output.rows;
    assert.equal(r.output.total_rows, Math.min(400, closed));
    assert.equal(list.length, r.output.total_rows);
    assert.ok(list.every((row) => row[col.community_board] === "04 BRONX"));
    const key = order === "recent" ? (row) => -Date.parse(row[col.created_date].slice(0, 23) + "Z")
      : order === "slowest" ? (row) => -row[col.hours_to_close] : (row) => row[col.hours_to_close];
    for (let i = 1; i < list.length; i++) assert.ok(key(list[i - 1]) <= key(list[i]), `${order} row ${i}`);
    engine.dropExecution(r.execution);
  }
});

test("leaving out one complaint gives what a fresh query without it gives", () => {
  const q = { type: types[0], area: { kind: "borough", value: "QUEENS" } };
  const r = run(waitPlan(S, q));
  const c = JSON.parse(engine.contributions(r.execution, 0, "median_hours", 3));
  assert.equal(c.exact, true);
  for (const t of c.top) {
    const key = JSON.parse(engine.sourceRecords(S, new Uint32Array([t.source_row]))).rows[0][0];
    const plan = waitPlan(S, q);
    plan.nodes.splice(1, 0, { op: "filter", input: 0, predicate: { call: "ne", args: [{ column: "unique_key" }, { literal: { i64: String(key) } }] } });
    plan.nodes.forEach((n, j) => { if (j > 1) n.input = j - 1; });
    plan.output += 1;
    assert.equal(rows(plan)[0][1], t.without);
  }
  engine.dropExecution(r.execution);
});

// The fingerprint covers the values, not the file's bytes: a change that
// alters no value (alignment padding, say) loads and re-derives the same
// fingerprint. Anything else is refused with a sentence, never a crash.
/** What the page does for "Check it": the record, its question, and where it stands. */
function lookUp(key) {
  const r = run(keyPlan(S, key));
  const ids = engine.traceBackRows(r.execution, 0);
  engine.dropExecution(r.execution);
  if (ids.length === 0) return null;
  const recs = JSON.parse(engine.sourceRecords(S, ids));
  const rec = Object.fromEntries(recs.columns.map((c, i) => [c, recs.rows[0][i]]));
  const q = questionFor(rec);
  const plan = waitPlan(S, q);
  const w = run(plan);
  const trace = JSON.parse(engine.traceForward(w.execution, ids[0]));
  engine.dropExecution(w.execution);
  return { ids, rec, q, trace, why: whyNotCounted(plan, trace.dropped), closed: w.output.rows[0][0] };
}

test("looking up a complaint finds it, counts it, or says why not", () => {
  assert.equal(lookUp("1"), null);

  const counted = lookUp("65671892");
  assert.equal(counted.ids.length, 1);
  assert.equal(counted.rec.unique_key, 65671892);
  assert.deepEqual(counted.q, { type: "Noise - Residential", area: { kind: "district", value: "04 BRONX" } });
  assert.equal(counted.why, null);
  assert.equal(counted.trace.output_row, 0);

  // "N of them were closed faster" matches the complaints themselves.
  const hours = hoursBetween(counted.rec.created_date, counted.rec.closed_date);
  const faster = rows(fasterPlan(S, counted.q, hours))[0][0];
  const w = run(waitPlan(S, counted.q));
  const recs = JSON.parse(engine.sourceRecords(S, engine.traceBackRows(w.execution, 0)));
  engine.dropExecution(w.execution);
  const c = (n) => recs.columns.indexOf(n);
  const all = recs.rows.map((row) => hoursBetween(row[c("created_date")], row[c("closed_date")]));
  assert.equal(faster, all.filter((h) => h < hours).length);

  // Still open, and closed before it was filed.
  const open = rows({ format: "receipts-plan/1", nodes: [
    { op: "scan", snapshot: S },
    { op: "filter", input: 0, predicate: { call: "is_null", args: [{ column: "closed_date" }] } },
    { op: "limit", input: 1, count: "3" },
  ], output: 2 });
  for (const row of open) assert.equal(lookUp(String(row[0])).why, "open");
  const backwards = rows({ format: "receipts-plan/1", nodes: [
    { op: "scan", snapshot: S },
    { op: "filter", input: 0, predicate: { call: "lt", args: [{ column: "closed_date" }, { column: "created_date" }] } },
    { op: "limit", input: 1, count: "3" },
  ], output: 2 });
  assert.ok(backwards.length > 0);
  for (const row of backwards) assert.equal(lookUp(String(row[0])).why, "backwards");
});

test("a changed byte is refused, or changed no value", () => {
  const n = files[1].length;
  let refused = 0;
  const offsets = [0, n - 1, ...Array.from({ length: 40 }, (_, i) => Math.floor((n * (i + 0.5)) / 40))];
  for (const off of offsets) {
    const e = new Engine();
    const bad = new Uint8Array(files[1]);
    bad[off] ^= 1;
    try {
      const r = JSON.parse(e.loadSnapshot(files[0], bad, files[2], files[3]));
      assert.equal(r.snapshot_hash, S, `offset ${off}`);
    } catch (err) {
      if (err.code === "ERR_ASSERTION") throw err;
      assert.match(err.message, /^[\w.]+: /, `offset ${off}: ${err.message}`);
      refused++;
    } finally {
      e.free();
    }
  }
  assert.ok(refused >= offsets.length - 4, `only ${refused} of ${offsets.length} refused`);
});
