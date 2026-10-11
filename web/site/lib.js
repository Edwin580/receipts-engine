// Pure helpers for the demo page: building plans from the page's controls,
// formatting cells, and corrupting bytes for the verify demo. No DOM and no
// engine calls, so `node --test web/site/test` covers them directly.

export const METRICS = {
  mean: { label: "mean hours to close", fn: "mean", column: "mean_hours" },
  median: { label: "median hours to close", fn: "median", column: "median_hours" },
  count: { label: "number of requests", fn: null, column: "requests" },
};

export const GROUPS = {
  borough: "borough",
  channel: "how it was reported",
  agency: "agency",
};

const HOUR_US = "3600000000";

/**
 * Builds a receipts-plan/1 plan (docs/engine/plan.md) from the controls.
 * Each judgement is its own filter step so the pipeline view shows how many
 * rows it removed.
 *
 * @param {string} snapshot  snapshot hash
 * @param {{type: string|null, groupBy: string, metric: string, dropBackwards: boolean}} q
 */
export function buildPlan(snapshot, q) {
  if (!(q.metric in METRICS)) throw new Error(`unknown metric ${q.metric}`);
  if (!(q.groupBy in GROUPS)) throw new Error(`unknown grouping ${q.groupBy}`);
  const nodes = [{ op: "scan", snapshot }];
  const add = (node) => nodes.push({ ...node, input: nodes.length - 1 }) - 1;

  if (q.type) {
    add({ op: "filter", predicate: { call: "eq", args: [{ column: "complaint_type" }, { literal: { str: q.type } }] } });
  }
  const timed = q.metric !== "count";
  if (timed) {
    add({ op: "filter", predicate: { call: "not", args: [{ call: "is_null", args: [{ column: "closed_date" }] }] } });
    if (q.dropBackwards) {
      add({ op: "filter", predicate: { call: "ge", args: [{ column: "closed_date" }, { column: "created_date" }] } });
    }
    add({
      op: "map",
      name: "hours_to_close",
      expr: { call: "div", args: [
        { call: "sub", args: [{ column: "closed_date" }, { column: "created_date" }] },
        { literal: { i64: HOUR_US } },
      ] },
    });
  }
  const m = METRICS[q.metric];
  const aggregates = [{ name: "requests", fn: "count" }];
  if (timed) aggregates.push({ name: m.column, fn: m.fn, column: "hours_to_close" });
  add({ op: "aggregate", group_by: [q.groupBy], aggregates });
  const out = add({ op: "sort", keys: [{ column: m.column, order: "desc" }] });

  // The scan has no input.
  delete nodes[0].input;
  return { format: "receipts-plan/1", nodes, output: out };
}

/** Plan for the complaint-type menu: the most common types, most first. */
export function topTypesPlan(snapshot, n) {
  return {
    format: "receipts-plan/1",
    nodes: [
      { op: "scan", snapshot },
      { op: "filter", input: 0, predicate: { call: "not", args: [{ call: "is_null", args: [{ column: "complaint_type" }] }] } },
      { op: "aggregate", input: 1, group_by: ["complaint_type"], aggregates: [{ name: "n", fn: "count" }] },
      { op: "sort", input: 2, keys: [{ column: "n", order: "desc" }, { column: "complaint_type", order: "asc" }] },
      { op: "limit", input: 3, count: String(n) },
    ],
    output: 4,
  };
}

/** 1234567 -> "1,234,567". */
export function fmtInt(n) {
  const s = String(Math.trunc(Math.abs(n)));
  return (n < 0 ? "-" : "") + s.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** A metric value for the table: integers with separators, hours to 1 decimal. */
export function fmtValue(metric, v) {
  if (v === null || v === undefined) return "none";
  if (metric === "count") return fmtInt(v);
  return v >= 1000 ? fmtInt(Math.round(v)) : v.toFixed(1);
}

/** Three decimals, for small changes a table's one decimal would hide. */
export function fmtPrecise(v) {
  return v === null || v === undefined ? "none" : v.toFixed(3);
}

/** Microseconds between two engine timestamps ("YYYY-MM-DDTHH:MM:SS.ffffff"). */
export function hoursBetween(created, closed) {
  if (!created || !closed) return null;
  // Both are naive NYC wall clock (ADR 0003); parse as UTC so no time zone
  // is applied to either side.
  const t = (s) => Date.parse(s.slice(0, 23) + "Z");
  return (t(closed) - t(created)) / 3_600_000;
}

/** "2.8 hours", "3.1 days", "closed 2 hours before it was opened". */
export function fmtDuration(hours) {
  if (hours === null) return "still open";
  const a = Math.abs(hours);
  const s = a < 48 ? `${a.toFixed(1)} hours` : `${(a / 24).toFixed(1)} days`;
  return hours < 0 ? `closed ${s} before it was opened` : s;
}

/** "2025-07-04T22:14:05.000000" -> "2025-07-04 22:14". */
export function fmtWhen(ts) {
  return ts ? ts.slice(0, 10) + " " + ts.slice(11, 16) : "";
}

/** Index of the output row whose first column equals `key`, or -1. */
export function findGroup(rows, key) {
  return rows.findIndex((r) => r[0] === key);
}

/** A copy of `bytes` with the lowest bit of byte `offset` flipped. */
export function flipBit(bytes, offset) {
  if (offset < 0 || offset >= bytes.length) throw new RangeError(`offset ${offset} is outside the file`);
  // new Uint8Array(x) copies; Buffer.prototype.slice would share memory.
  const copy = new Uint8Array(bytes);
  copy[offset] ^= 1;
  return copy;
}

/** "a1b2c3d4e5f6…" for long hashes. */
export function shortHash(h, n = 12) {
  return h.length > n ? h.slice(0, n) + "…" : h;
}

/** Groups engine error messages that differ only in their numbers. */
export function reasonKey(message) {
  return message.replace(/\d+/g, "#");
}
