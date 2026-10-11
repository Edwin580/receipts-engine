// node --test web/site/test
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  buildPlan, topTypesPlan, fmtInt, fmtValue, hoursBetween, fmtDuration, fmtWhen, findGroup, flipBit, reasonKey, fmtPrecise,
} from "../lib.js";

const S = "ab".repeat(32);

test("plan: every node reads the one before it, and the output is the last", () => {
  for (const metric of ["mean", "median", "count"]) {
    for (const type of [null, "Noise - Residential"]) {
      for (const dropBackwards of [true, false]) {
        const p = buildPlan(S, { type, groupBy: "borough", metric, dropBackwards });
        assert.equal(p.nodes[0].op, "scan");
        assert.equal("input" in p.nodes[0], false);
        p.nodes.slice(1).forEach((n, i) => assert.equal(n.input, i));
        assert.equal(p.output, p.nodes.length - 1);
      }
    }
  }
});

test("plan: count skips the closed-date steps", () => {
  const p = buildPlan(S, { type: null, groupBy: "agency", metric: "count", dropBackwards: true });
  assert.deepEqual(p.nodes.map((n) => n.op), ["scan", "aggregate", "sort"]);
  assert.deepEqual(p.nodes[2].keys, [{ column: "requests", order: "desc" }]);
});

test("plan: the closed-before-opened rule is its own step and can be turned off", () => {
  const on = buildPlan(S, { type: "Noise - Residential", groupBy: "borough", metric: "median", dropBackwards: true });
  const off = buildPlan(S, { type: "Noise - Residential", groupBy: "borough", metric: "median", dropBackwards: false });
  assert.deepEqual(on.nodes.map((n) => n.op), ["scan", "filter", "filter", "filter", "map", "aggregate", "sort"]);
  assert.deepEqual(off.nodes.map((n) => n.op), ["scan", "filter", "filter", "map", "aggregate", "sort"]);
  assert.equal(on.nodes[3].predicate.call, "ge");
  assert.deepEqual(on.nodes[5].aggregates[1], { name: "median_hours", fn: "median", column: "hours_to_close" });
});

test("plan: integer literals are strings (canonical JSON stops at 2^53)", () => {
  const p = buildPlan(S, { type: null, groupBy: "borough", metric: "mean", dropBackwards: true });
  const map = p.nodes.find((n) => n.op === "map");
  assert.deepEqual(map.expr.args[1], { literal: { i64: "3600000000" } });
  assert.equal(topTypesPlan(S, 12).nodes[4].count, "12");
});

test("plan: unknown choices are refused", () => {
  assert.throws(() => buildPlan(S, { type: null, groupBy: "borough", metric: "sum", dropBackwards: true }));
  assert.throws(() => buildPlan(S, { type: null, groupBy: "zip", metric: "mean", dropBackwards: true }));
});

test("formatting", () => {
  assert.equal(fmtInt(0), "0");
  assert.equal(fmtInt(1234567), "1,234,567");
  assert.equal(fmtInt(-1000), "-1,000");
  assert.equal(fmtValue("count", 31415), "31,415");
  assert.equal(fmtValue("mean", 2.345), "2.3");
  assert.equal(fmtValue("mean", 1234.5), "1,235");
  assert.equal(fmtValue("median", null), "none");
  assert.equal(fmtWhen("2025-07-04T22:14:05.000000"), "2025-07-04 22:14");
  assert.equal(fmtWhen(null), "");
  assert.equal(fmtPrecise(2.81234), "2.812");
  assert.equal(fmtPrecise(null), "none");
});

test("durations come from naive timestamps with no time zone applied", () => {
  // 2025-03-09 is a DST change in New York; wall-clock difference is 3 h.
  assert.equal(hoursBetween("2025-03-09T00:30:00.000000", "2025-03-09T03:30:00.000000"), 3);
  assert.equal(hoursBetween("2025-07-01T00:00:00.000000", null), null);
  assert.equal(fmtDuration(2.84), "2.8 hours");
  assert.equal(fmtDuration(72), "3.0 days");
  assert.equal(fmtDuration(-2), "closed 2.0 hours before it was opened");
  assert.equal(fmtDuration(null), "still open");
});

test("findGroup matches on the first column", () => {
  const rows = [["BRONX", 10, 2.5], ["QUEENS", 4, 1], [null, 1, 0]];
  assert.equal(findGroup(rows, "QUEENS"), 1);
  assert.equal(findGroup(rows, null), 2);
  assert.equal(findGroup(rows, "STATEN ISLAND"), -1);
});

test("flipBit changes exactly one bit of a copy", () => {
  const a = new Uint8Array([0, 1, 2, 255]);
  const b = flipBit(a, 3);
  assert.deepEqual([...a], [0, 1, 2, 255]);
  assert.deepEqual([...b], [0, 1, 2, 254]);
  assert.throws(() => flipBit(a, 4), RangeError);
  const buf = Buffer.from([7, 7]);
  flipBit(buf, 0);
  assert.deepEqual([...buf], [7, 7], "a Node Buffer is copied too");
  assert.throws(() => flipBit(a, -1), RangeError);
});

test("reasonKey groups messages that differ only in numbers", () => {
  const a = "data.arrow: a compressed buffer decoded to 3 bytes, but its header says 65536";
  const b = "data.arrow: a compressed buffer decoded to 70 bytes, but its header says 524288";
  assert.equal(reasonKey(a), reasonKey(b));
  assert.notEqual(reasonKey(a), reasonKey("data.arrow: string offsets out of bounds"));
});
