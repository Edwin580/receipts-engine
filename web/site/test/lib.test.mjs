// node --test web/site/test
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  waitPlan, comparePlan, countsPlan, listPlan, topTypesPlan, districtsPlan, ORDERS, keyPlan, fasterPlan, questionFor, whyNotCounted, toSearch, fromSearch, townsPlan, townListPlan,
  parseDistrict, districtName, boroughName, areaName, areaKey, parseAreaKey, rankOf, ordinal,
  fmtInt, fmtDuration, fmtWhen, hoursBetween, periodName, typeName, shortHash,
} from "../lib.js";

const S = "ab".repeat(32);
const AREAS = [
  { kind: "city" },
  { kind: "borough", value: "BRONX" },
  { kind: "district", value: "04 BRONX" },
  { kind: "zip", value: "10451" },
];

function assertChain(p) {
  assert.equal(p.format, "receipts-plan/1");
  assert.equal(p.nodes[0].op, "scan");
  assert.equal("input" in p.nodes[0], false);
  p.nodes.slice(1).forEach((n, i) => assert.equal(n.input, i));
  assert.equal(p.output, p.nodes.length - 1);
}

test("plans: every node reads the one before it, and the output is the last", () => {
  for (const area of AREAS) {
    for (const type of [null, "Noise - Residential"]) {
      const q = { type, area };
      assertChain(waitPlan(S, q));
      assertChain(countsPlan(S, q));
      for (const order of Object.keys(ORDERS)) assertChain(listPlan(S, q, order, 10));
    }
  }
  assertChain(comparePlan(S, { type: "Rodent", borough: "QUEENS" }));
  assertChain(comparePlan(S, { type: "Rodent", borough: null }));
  assertChain(topTypesPlan(S, 5));
  assertChain(districtsPlan(S));
});

test("plans: the area picks the right column, and citywide has no area filter", () => {
  const filters = (p) => p.nodes.filter((n) => n.op === "filter").map((n) => n.predicate.args[0].column);
  assert.deepEqual(filters(countsPlan(S, { type: "Rodent", area: { kind: "city" } })), ["complaint_type"]);
  assert.deepEqual(filters(countsPlan(S, { type: "Rodent", area: AREAS[1] })), ["complaint_type", "borough"]);
  assert.deepEqual(filters(countsPlan(S, { type: "Rodent", area: AREAS[2] })), ["complaint_type", "community_board"]);
  assert.deepEqual(filters(countsPlan(S, { type: "Rodent", area: AREAS[3] })), ["complaint_type", "incident_zip"]);
});

test("plans: closed-before-opened is its own step, after the closed-date step", () => {
  const ops = waitPlan(S, { type: "Rodent", area: AREAS[0] }).nodes.map((n) => n.predicate?.call ?? n.op);
  assert.deepEqual(ops, ["scan", "eq", "not", "ge", "map", "aggregate"]);
});

test("plans: complaint lists break ties by complaint number and stop at n", () => {
  const p = listPlan(S, { type: null, area: AREAS[0] }, "slowest", 25);
  const sort = p.nodes.at(-2);
  assert.deepEqual(sort.keys.at(-1), { column: "unique_key", order: "asc" });
  assert.deepEqual(p.nodes.at(-1), { op: "limit", input: p.nodes.length - 2, count: "25" });
  assert.throws(() => listPlan(S, { type: null, area: AREAS[0] }, "sideways", 5), /unknown order/);
});

test("districts: codes 1-18 are districts, the rest are not", () => {
  assert.deepEqual(parseDistrict("04 BRONX"), { borough: "BRONX", number: 4 });
  assert.deepEqual(parseDistrict("18 BROOKLYN"), { borough: "BROOKLYN", number: 18 });
  for (const c of ["55 BROOKLYN", "00 QUEENS", "Unspecified BRONX", "0 Unspecified", null, "04 NOWHERE"]) {
    assert.equal(parseDistrict(c), null, String(c));
  }
  assert.equal(districtName("04 BRONX"), "Bronx district 4");
  assert.equal(districtName("01 STATEN ISLAND"), "Staten Island district 1");
  assert.equal(districtName("55 BROOKLYN"), "55 BROOKLYN");
});

test("areas: names and menu keys round-trip, bad keys fall back to citywide", () => {
  assert.equal(boroughName("BRONX"), "Bronx");
  assert.equal(boroughName("BRONX", true), "the Bronx");
  assert.equal(boroughName("STATEN ISLAND", true), "Staten Island");
  assert.equal(areaName({ kind: "city" }, true), "the whole city");
  assert.equal(areaName({ kind: "zip", value: "10451" }), "ZIP 10451");
  for (const a of AREAS) assert.deepEqual(parseAreaKey(areaKey(a)), a);
  for (const k of ["", "zip:123", "zip:abcde", "planet:Mars", "borough:"]) assert.deepEqual(parseAreaKey(k), { kind: "city" });
});

test("rank: only real districts with enough complaints count, slowest is 1st", () => {
  const rows = [
    ["26 BRONX", 2, 9], ["08 BRONX", 496, 3.5], ["02 BRONX", 5, 3.0], ["04 BRONX", 862, 2.5],
    ["Unspecified BRONX", 30, 2.0], ["03 BRONX", 466, 0.7],
  ];
  assert.deepEqual(rankOf(rows, "08 BRONX", true), { rank: 1, of: 3 });
  assert.deepEqual(rankOf(rows, "04 BRONX", true), { rank: 2, of: 3 });
  assert.equal(rankOf(rows, "02 BRONX", true), null);
  assert.deepEqual(rankOf([["Unspecified", 50, 9], ["BRONX", 50, 2], ["QUEENS", 50, 1]], "QUEENS", false), { rank: 2, of: 2 });
  assert.deepEqual([1, 2, 3, 4, 11, 12, 13, 21, 22, 101, 111].map(ordinal),
    ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st", "111th"]);
});

test("formatting", () => {
  assert.equal(fmtInt(1234567), "1,234,567");
  assert.equal(fmtInt(-1000), "-1,000");
  assert.equal(fmtDuration(null), "still open");
  assert.equal(fmtDuration(0.004), "1 minute");
  assert.equal(fmtDuration(0.5), "30 minutes");
  assert.equal(fmtDuration(1), "1 hour");
  assert.equal(fmtDuration(0.995), "1 hour");
  assert.equal(fmtDuration(0.98), "59 minutes");
  assert.equal(fmtDuration(2.51125), "2.5 hours");
  assert.equal(fmtDuration(27.02), "27 hours");
  assert.equal(fmtDuration(72), "3 days");
  assert.equal(fmtDuration(24 * 30), "4.3 weeks");
  assert.equal(fmtDuration(-2), "closed 2 hours before it was opened");
  assert.equal(hoursBetween("2025-07-04T22:00:00.000000", "2025-07-05T01:30:00.000000"), 3.5);
  assert.equal(hoursBetween("2025-07-04T22:00:00.000000", null), null);
  assert.equal(fmtWhen("2025-07-04T22:14:05.000000"), "Jul 4, 2025 10:14 pm");
  assert.equal(fmtWhen("2025-07-04T00:05:00.000000"), "Jul 4, 2025 12:05 am");
  assert.equal(fmtWhen(null), "");
  assert.equal(periodName("2025-07-01", "2025-08-01"), "July 2025");
  assert.equal(periodName("2025-07-01", "2025-10-01"), "July 1 to September 30, 2025");
  assert.equal(periodName("2024-10-01", "2025-10-01"), "October 1, 2024 to September 30, 2025");
  assert.equal(typeName("HEAT/HOT WATER"), "Heat/Hot Water");
  assert.equal(typeName("Noise - Residential"), "Noise - Residential");
  assert.equal(shortHash("a".repeat(64)), "aaaaaaaaaaaa…");
});

test("looking up a complaint: number checks, its question, and why it isn't counted", () => {
  assertChain(keyPlan(S, "65671892"));
  assertChain(fasterPlan(S, { type: "Rodent", area: AREAS[2] }, 3.5));
  assert.deepEqual(keyPlan(S, "007").nodes[1].predicate.args[1], { literal: { i64: "7" } });
  for (const bad of ["", "1e5", "-3", "12 34", "1234567890123456789"]) assert.throws(() => keyPlan(S, bad), /not a complaint number/);

  const rec = (complaint_type, community_board, borough) => ({ complaint_type, community_board, borough });
  assert.deepEqual(questionFor(rec("Rodent", "04 BRONX", "BRONX")), { type: "Rodent", area: { kind: "district", value: "04 BRONX" } });
  assert.deepEqual(questionFor(rec("Rodent", "55 BROOKLYN", "BROOKLYN")), { type: "Rodent", area: { kind: "borough", value: "BROOKLYN" } });
  assert.deepEqual(questionFor(rec("Rodent", "0 Unspecified", "Unspecified")), { type: "Rodent", area: { kind: "city" } });

  const plan = waitPlan(S, { type: "Rodent", area: AREAS[2] });
  const at = (op) => plan.nodes.findIndex((n) => n.predicate?.call === op);
  const filter = (step, value) => ({ step, sentence: "", reason: { kind: "filter", value } });
  assert.equal(whyNotCounted(plan, null), null);
  assert.equal(whyNotCounted(plan, filter(at("not"), false)), "open");
  assert.equal(whyNotCounted(plan, filter(at("ge"), null)), "backwards");
  assert.equal(whyNotCounted(plan, filter(1, false)), "elsewhere");
  assert.equal(whyNotCounted(plan, { step: 0, sentence: "", reason: { kind: "excluded" } }), "other");
});

test("links: the question round-trips through the URL, bad parts fall back", () => {
  for (const area of AREAS) {
    for (const complaint of [null, "65671892"]) {
      const q = { type: "HEAT/HOT WATER", area };
      assert.deepEqual(fromSearch(toSearch(q, complaint)), { ...q, complaint });
    }
  }
  assert.equal(toSearch({ type: "Rodent", area: { kind: "city" } }), "?type=Rodent");
  assert.equal(toSearch({ type: null, area: { kind: "city" } }), "");
  assert.deepEqual(fromSearch(""), { type: null, area: { kind: "city" }, complaint: null });
  assert.deepEqual(fromSearch("?type=&area=zip:12&complaint=12x"), { type: null, area: { kind: "city" }, complaint: null });
  assert.deepEqual(fromSearch("?area=borough%3AQUEENS").area, { kind: "borough", value: "QUEENS" });
});

test("towns: an area kind of their own", () => {
  const town = { kind: "town", value: "Mount Pleasant" };
  assert.equal(areaName(town), "Mount Pleasant");
  assert.deepEqual(parseAreaKey(areaKey(town)), town);
  assert.deepEqual(fromSearch(toSearch({ type: "Potholes", area: town })).area, town);
  const filters = waitPlan(S, { type: "Potholes", area: town }).nodes.filter((n) => n.op === "filter");
  assert.equal(filters[1].predicate.args[0].column, "town");
  assertChain(townsPlan(S, "Potholes"));
  assertChain(townListPlan(S));
  assert.deepEqual(townsPlan(S, "Potholes").nodes.at(-2).group_by, ["town"]);
});
