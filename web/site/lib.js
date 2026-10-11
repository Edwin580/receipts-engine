// Pure helpers for the page: plans built from the question, area names and
// formatting. No DOM and no engine calls, so `node --test web/site/test`
// covers them directly.

const HOUR_US = "3600000000";

export const BOROUGHS = ["BRONX", "BROOKLYN", "MANHATTAN", "QUEENS", "STATEN ISLAND"];

/** Community districts are numbered 1-18; higher codes are parks and other special areas. */
const MAX_DISTRICT = 18;

/** "BRONX" -> "the Bronx", "STATEN ISLAND" -> "Staten Island". */
export function boroughName(b, withArticle = false) {
  const name = b.toLowerCase().replace(/\b\w/g, (c) => c.toUpperCase());
  return b === "BRONX" && withArticle ? "the Bronx" : name;
}

/** "04 BRONX" -> {borough: "BRONX", number: 4}; null for special areas and "Unspecified". */
export function parseDistrict(code) {
  const m = /^(\d{2}) (.+)$/.exec(code ?? "");
  if (!m) return null;
  const number = Number(m[1]);
  if (number < 1 || number > MAX_DISTRICT || !BOROUGHS.includes(m[2])) return null;
  return { borough: m[2], number };
}

/** "04 BRONX" -> "Bronx district 4". */
export function districtName(code) {
  const d = parseDistrict(code);
  return d ? `${boroughName(d.borough)} district ${d.number}` : code;
}

/**
 * Where a question is about:
 *   {kind: "city"} | {kind: "borough", value: "BRONX"}
 *   | {kind: "district", value: "04 BRONX"} | {kind: "zip", value: "10451"}
 */
export function areaName(area, withArticle = false) {
  switch (area.kind) {
    case "city": return withArticle ? "the whole city" : "Citywide";
    case "borough": return boroughName(area.value, withArticle);
    case "district": return districtName(area.value);
    case "zip": return `ZIP ${area.value}`;
    default: throw new Error(`unknown area ${area.kind}`);
  }
}

/** The area as a menu value and back: "city", "borough:BRONX", "district:04 BRONX", "zip:10451". */
export function areaKey(area) {
  return area.kind === "city" ? "city" : `${area.kind}:${area.value}`;
}
export function parseAreaKey(key) {
  if (!key || key === "city") return { kind: "city" };
  const i = key.indexOf(":");
  const kind = key.slice(0, i);
  const value = key.slice(i + 1);
  if (!["borough", "district", "zip"].includes(kind) || !value) return { kind: "city" };
  if (kind === "zip" && !/^\d{5}$/.test(value)) return { kind: "city" };
  return { kind, value };
}

const AREA_COLUMN = { borough: "borough", district: "community_board", zip: "incident_zip" };

const eq = (column, value) => ({ call: "eq", args: [{ column }, { literal: { str: value } }] });

/** A plan built one step at a time; each step reads the one before it. */
function chain(snapshot) {
  const nodes = [{ op: "scan", snapshot }];
  return {
    add(node) { nodes.push({ ...node, input: nodes.length - 1 }); return this; },
    filter(predicate) { return this.add({ op: "filter", predicate }); },
    done() { return { format: "receipts-plan/1", nodes, output: nodes.length - 1 }; },
  };
}

/** Complaints of one type in one area. */
function complaints(snapshot, { type, area }) {
  const p = chain(snapshot);
  if (type) p.filter(eq("complaint_type", type));
  if (area && area.kind !== "city") p.filter(eq(AREA_COLUMN[area.kind], area.value));
  return p;
}

/**
 * ... that were closed, with how long each took. Complaints closed before
 * they were opened are dropped as a separate step, so the explanation shows
 * how many there were.
 */
function closed(snapshot, q) {
  return complaints(snapshot, q)
    .filter({ call: "not", args: [{ call: "is_null", args: [{ column: "closed_date" }] }] })
    .filter({ call: "ge", args: [{ column: "closed_date" }, { column: "created_date" }] })
    .add({
      op: "map",
      name: "hours_to_close",
      expr: { call: "div", args: [
        { call: "sub", args: [{ column: "closed_date" }, { column: "created_date" }] },
        { literal: { i64: HOUR_US } },
      ] },
    });
}

/** "9 in 10 were closed within" this many hours: a nearest-rank percentile, so it is one complaint's time. */
export const SLOW_P = 90;

const TIME_AGGREGATES = [
  { name: "complaints", fn: "count" },
  { name: "median_hours", fn: "median", column: "hours_to_close" },
  { name: "p90_hours", fn: "percentile", p: SLOW_P, column: "hours_to_close" },
];

/** One row: how many closed complaints, the median hours to close, and the 90th percentile. */
export function waitPlan(snapshot, q) {
  return closed(snapshot, q).add({ op: "aggregate", group_by: [], aggregates: TIME_AGGREGATES }).done();
}

/**
 * The same, for each district of a borough (or each borough, citywide),
 * slowest first. Used for the comparison table and the rank.
 */
export function comparePlan(snapshot, { type, borough }) {
  const p = closed(snapshot, { type, area: borough ? { kind: "borough", value: borough } : null });
  return p
    .add({ op: "aggregate", group_by: [borough ? "community_board" : "borough"], aggregates: TIME_AGGREGATES })
    .add({ op: "sort", keys: [{ column: "median_hours", order: "desc" }] })
    .done();
}

/** One row: complaints filed, and how many have a closed date. */
export function countsPlan(snapshot, q) {
  return complaints(snapshot, q)
    .add({ op: "aggregate", group_by: [], aggregates: [
      { name: "filed", fn: "count" },
      { name: "closed", fn: "count_non_null", column: "closed_date" },
    ] })
    .done();
}

export const ORDERS = {
  slowest: { label: "took longest", keys: [{ column: "hours_to_close", order: "desc" }] },
  fastest: { label: "closed fastest", keys: [{ column: "hours_to_close", order: "asc" }] },
  recent: { label: "most recent", keys: [{ column: "created_date", order: "desc" }] },
};

/** The closed complaints themselves, in one of ORDERS, at most `n`. */
export function listPlan(snapshot, q, order, n) {
  if (!(order in ORDERS)) throw new Error(`unknown order ${order}`);
  return closed(snapshot, q)
    .add({ op: "sort", keys: [...ORDERS[order].keys, { column: "unique_key", order: "asc" }] })
    .add({ op: "limit", count: String(n) })
    .done();
}

/** Plan for the complaint-type menu: the most common types, most first. */
export function topTypesPlan(snapshot, n) {
  return chain(snapshot)
    .filter({ call: "not", args: [{ call: "is_null", args: [{ column: "complaint_type" }] }] })
    .add({ op: "aggregate", group_by: ["complaint_type"], aggregates: [{ name: "n", fn: "count" }] })
    .add({ op: "sort", keys: [{ column: "n", order: "desc" }, { column: "complaint_type", order: "asc" }] })
    .add({ op: "limit", count: String(n) })
    .done();
}

/** Plan for the area menu: complaints per community district code. */
export function districtsPlan(snapshot) {
  return chain(snapshot)
    .add({ op: "aggregate", group_by: ["community_board"], aggregates: [{ name: "n", fn: "count" }] })
    .add({ op: "sort", keys: [{ column: "community_board", order: "asc" }] })
    .done();
}

/** Districts with fewer closed complaints than this are left out of rankings. */
export const MIN_RANKED = 20;

/**
 * Rank of `key` among compare rows ([group, complaints, median], slowest
 * first), counting only real districts/boroughs with enough complaints.
 * Returns {rank, of} with rank 1 = slowest, or null if `key` isn't ranked.
 */
export function rankOf(rows, key, byDistrict) {
  const ranked = rows.filter(([g, n, m]) =>
    m !== null && n >= MIN_RANKED && (byDistrict ? parseDistrict(g) : BOROUGHS.includes(g)));
  const i = ranked.findIndex((r) => r[0] === key);
  return i < 0 ? null : { rank: i + 1, of: ranked.length };
}

/** 1 -> "1st", 2 -> "2nd", 12 -> "12th". */
export function ordinal(n) {
  const s = n % 100 >= 11 && n % 100 <= 13 ? "th" : ["th", "st", "nd", "rd"][n % 10] ?? "th";
  return n + s;
}

/** 1234567 -> "1,234,567". */
export function fmtInt(n) {
  const s = String(Math.trunc(Math.abs(n)));
  return (n < 0 ? "-" : "") + s.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** Hours as people say them: "40 minutes", "2.5 hours", "3 days", "5 weeks". */
export function fmtDuration(hours) {
  if (hours === null || hours === undefined) return "still open";
  if (hours < 0) return `closed ${fmtDuration(-hours)} before it was opened`;
  const one = (x, unit) => {
    const r = x >= 10 ? Math.round(x) : Math.round(x * 10) / 10;
    return `${r} ${unit}${r === 1 ? "" : "s"}`;
  };
  if (Math.round(hours * 60) < 60) return one(Math.max(1, Math.round(hours * 60)), "minute");
  if (hours < 48) return one(hours, "hour");
  if (hours < 24 * 21) return one(hours / 24, "day");
  return one(hours / (24 * 7), "week");
}

/** Hours between two engine timestamps ("YYYY-MM-DDTHH:MM:SS.ffffff"). */
export function hoursBetween(created, closed) {
  if (!created || !closed) return null;
  // Both are naive NYC wall clock (ADR 0003); parse as UTC so no time zone
  // is applied to either side.
  const t = (s) => Date.parse(s.slice(0, 23) + "Z");
  return (t(closed) - t(created)) / 3_600_000;
}

/** "2025-07-04T22:14:05.000000" -> "Jul 4, 10:14 pm". */
export function fmtWhen(ts) {
  if (!ts) return "";
  const [y, mo, d] = ts.slice(0, 10).split("-").map(Number);
  let h = Number(ts.slice(11, 13));
  const mi = ts.slice(14, 16);
  const ap = h < 12 ? "am" : "pm";
  h = h % 12 || 12;
  const month = MONTHS[mo - 1].slice(0, 3);
  return `${month} ${d}, ${y} ${h}:${mi} ${ap}`;
}

const MONTHS = ["January", "February", "March", "April", "May", "June", "July",
  "August", "September", "October", "November", "December"];

/**
 * The period a snapshot covers, from its scope's two dates (end exclusive):
 * "July 2025", or "July 1 to September 30, 2025".
 */
export function periodName(from, toExclusive) {
  const a = new Date(from + "T00:00:00Z");
  const b = new Date(Date.parse(toExclusive + "T00:00:00Z") - 86_400_000);
  const whole = a.getUTCDate() === 1 && new Date(b.getTime() + 86_400_000).getUTCDate() === 1;
  if (whole && a.getUTCMonth() === b.getUTCMonth() && a.getUTCFullYear() === b.getUTCFullYear()) {
    return `${MONTHS[a.getUTCMonth()]} ${a.getUTCFullYear()}`;
  }
  const day = (d) => `${MONTHS[d.getUTCMonth()]} ${d.getUTCDate()}`;
  return a.getUTCFullYear() === b.getUTCFullYear()
    ? `${day(a)} to ${day(b)}, ${b.getUTCFullYear()}`
    : `${day(a)}, ${a.getUTCFullYear()} to ${day(b)}, ${b.getUTCFullYear()}`;
}

/** "HEAT/HOT WATER" -> "Heat/Hot Water"; mixed-case names stay as the city wrote them. */
export function typeName(t) {
  return t === t.toUpperCase() ? t.toLowerCase().replace(/\b\w/g, (c) => c.toUpperCase()) : t;
}

/** "a1b2c3d4e5f6…" for long hashes. */
export function shortHash(h, n = 12) {
  return h.length > n ? h.slice(0, n) + "…" : h;
}
