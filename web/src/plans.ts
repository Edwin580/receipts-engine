// Plan JSON builders (docs/engine/plan.md §3) and the questions the app asks.
// Every number the app shows comes from one of these plans.

export type Expr = Record<string, unknown>;
export type Node = Record<string, unknown>;
export interface Plan {
  format: "receipts-plan/1";
  nodes: Node[];
  output: number;
}

export const col = (column: string): Expr => ({ column });
export const str = (s: string): Expr => ({ literal: { str: s } });
export const int = (i: number | bigint): Expr => ({ literal: { i64: String(i) } });
export const float = (x: number): Expr => ({ literal: { f64: x } });
export const ts = (iso: string): Expr => ({ literal: { timestamp: iso } });
export const call = (name: string, ...args: Expr[]): Expr => ({ call: name, args });
export const and = (...args: Expr[]): Expr => call("and", ...args);
export const not = (e: Expr): Expr => call("not", e);
export const isNull = (e: Expr): Expr => call("is_null", e);
export const oneOf = (e: Expr, values: string[]): Expr => ({
  call: "in",
  args: [e],
  values: values.map((s) => ({ str: s })),
});
export const month = (e: Expr): Expr => ({ call: "date_trunc", unit: "month", args: [e] });
export const day = (e: Expr): Expr => ({ call: "date_trunc", unit: "day", args: [e] });

/** Builds a linear plan: each step reads the previous one. */
export class Builder {
  private nodes: Node[];
  constructor(snapshot: string) {
    this.nodes = [{ op: "scan", snapshot }];
  }
  private push(node: Node): this {
    this.nodes.push({ ...node, input: this.nodes.length - 1 });
    return this;
  }
  filter(predicate: Expr) {
    return this.push({ op: "filter", predicate });
  }
  map(name: string, expr: Expr) {
    return this.push({ op: "map", name, expr });
  }
  aggregate(group_by: string[], aggregates: { name: string; fn: string; column?: string }[]) {
    return this.push({ op: "aggregate", group_by, aggregates });
  }
  sort(keys: { column: string; order: "asc" | "desc" }[]) {
    return this.push({ op: "sort", keys });
  }
  limit(count: number) {
    return this.push({ op: "limit", count: String(count) });
  }
  build(): Plan {
    return { format: "receipts-plan/1", nodes: this.nodes, output: this.nodes.length - 1 };
  }
}

export type Year = "2024" | "2025" | "both";

export function yearFilter(year: Year): Expr | undefined {
  if (year === "both") return undefined;
  const next = String(Number(year) + 1);
  return and(
    call("ge", col("created_date"), ts(`${year}-01-01T00:00:00.000000`)),
    call("lt", col("created_date"), ts(`${next}-01-01T00:00:00.000000`)),
  );
}

const HOURS = call("div", call("sub", col("closed_date"), col("created_date")), int(3_600_000_000));
const CLOSED_AFTER_OPENED = and(not(isNull(col("closed_date"))), call("ge", col("closed_date"), col("created_date")));

export interface QuestionParams {
  year: Year;
  complaintTypes: string[];
}

export interface Question {
  id: string;
  title: string;
  /** Column shown as bars and used for receipts and what-ifs. */
  measure: string;
  /** Column that names each bar. */
  label: string;
  unit: "hours" | "requests";
  usesComplaintTypes: boolean;
  plan: (snapshot: string, p: QuestionParams) => Plan;
  /** One sentence describing the answer's leading row (see `leadingRow`). */
  headline: (label: string, value: number, requests: number, p: QuestionParams) => string;
}

function scoped(snapshot: string, p: QuestionParams): Builder {
  const b = new Builder(snapshot);
  const y = yearFilter(p.year);
  return y ? b.filter(y) : b;
}

const period = (p: QuestionParams) => (p.year === "both" ? "in 2024–2025" : `in ${p.year}`);

export const QUESTIONS: Question[] = [
  {
    id: "noise-by-borough",
    title: "How long does the city take to close complaints, by borough?",
    measure: "median_hours",
    label: "borough",
    unit: "hours",
    usesComplaintTypes: true,
    plan: (s, p) =>
      scoped(s, p)
        .filter(and(oneOf(col("complaint_type"), p.complaintTypes), CLOSED_AFTER_OPENED))
        .map("hours_to_close", HOURS)
        .aggregate(["borough"], [
          { name: "requests", fn: "count" },
          { name: "median_hours", fn: "median", column: "hours_to_close" },
          { name: "mean_hours", fn: "mean", column: "hours_to_close" },
        ])
        .sort([{ column: "median_hours", order: "desc" }])
        .build(),
    headline: (label, v, n, p) =>
      `${titleCase(label)} is slowest: half of its ${n.toLocaleString("en-US")} complaints ${period(p)} took over ${hours(v)} to close.`,
  },
  {
    id: "top-complaints",
    title: "What do New Yorkers complain about most?",
    measure: "requests",
    label: "complaint_type",
    unit: "requests",
    usesComplaintTypes: false,
    plan: (s, p) =>
      scoped(s, p)
        .aggregate(["complaint_type"], [{ name: "requests", fn: "count" }])
        .sort([{ column: "requests", order: "desc" }])
        .limit(12)
        .build(),
    headline: (label, v, _n, p) => `"${label}" is the top complaint ${period(p)}, with ${v.toLocaleString("en-US")} requests.`,
  },
  {
    id: "agency-speed",
    title: "Which agencies close complaints fastest?",
    measure: "median_hours",
    label: "agency",
    unit: "hours",
    usesComplaintTypes: false,
    plan: (s, p) =>
      scoped(s, p)
        .filter(CLOSED_AFTER_OPENED)
        .map("hours_to_close", HOURS)
        .aggregate(["agency"], [
          { name: "requests", fn: "count" },
          { name: "median_hours", fn: "median", column: "hours_to_close" },
        ])
        .sort([{ column: "median_hours", order: "asc" }])
        .build(),
    headline: (label, v, n, p) =>
      `${label} is fastest ${period(p)}: half of its ${n.toLocaleString("en-US")} requests closed within ${hours(v)}.`,
  },
  {
    id: "closed-before-opened",
    title: "How often is a complaint “closed” before it was opened?",
    measure: "requests",
    label: "agency",
    unit: "requests",
    usesComplaintTypes: false,
    plan: (s, p) =>
      scoped(s, p)
        .filter(call("lt", col("closed_date"), col("created_date")))
        .aggregate(["agency"], [{ name: "requests", fn: "count" }])
        .sort([{ column: "requests", order: "desc" }])
        .build(),
    headline: (label, v, _n, p) =>
      `${label} has the most requests closed before they were opened ${period(p)}: ${v.toLocaleString("en-US")}.`,
  },
];

/**
 * Known issues (from the manifest) that a plan can select, so a reader can
 * exclude them in a what-if. Issues that need functions the engine lacks
 * (text length, "now") are left out.
 */
export const KNOWN_ISSUE_FILTERS: Record<string, { label: string; predicate: Expr }> = {
  "KI-created-midnight": {
    label: "created at exactly midnight (date-only times)",
    predicate: call("eq", col("created_date"), day(col("created_date"))),
  },
  "KI-closed-before-created": {
    label: "closed before they were created",
    predicate: call("lt", col("closed_date"), col("created_date")),
  },
  "KI-borough-unspecified": {
    label: "with borough “Unspecified”",
    predicate: call("eq", col("borough"), str("Unspecified")),
  },
  "KI-closed-before-2010": {
    label: "with a placeholder closed date before 2010",
    predicate: call("lt", col("closed_date"), ts("2010-01-01T00:00:00.000000")),
  },
  "KI-location-outside-nyc": {
    label: "located outside NYC",
    predicate: call(
      "or",
      call("lt", call("lat", col("location")), float(40.45)),
      call("gt", call("lat", col("location")), float(40.95)),
      call("lt", call("lon", col("location")), float(-74.3)),
      call("gt", call("lon", col("location")), float(-73.65)),
    ),
  },
};

export function knownIssuePlan(snapshot: string, id: string): Plan | undefined {
  const f = KNOWN_ISSUE_FILTERS[id];
  return f && new Builder(snapshot).filter(f.predicate).build();
}

/** Distinct complaint types with counts, for the picker. */
export function complaintTypesPlan(snapshot: string): Plan {
  return new Builder(snapshot)
    .aggregate(["complaint_type"], [{ name: "requests", fn: "count" }])
    .sort([{ column: "requests", order: "desc" }])
    .build();
}

/** Groups smaller than this are too small to headline a median. */
export const MIN_HEADLINE_REQUESTS = 100;

/**
 * The row a headline should describe: the first row (the answer is sorted
 * by its measure) with enough records to mean something. A 35-record group
 * shouldn't headline a borough comparison; it still shows in the chart.
 */
export function leadingRow(requests: (number | null)[]): number {
  const i = requests.findIndex((n) => (n ?? 0) >= MIN_HEADLINE_REQUESTS);
  return i >= 0 ? i : 0;
}

export function hours(h: number): string {
  if (h < 1) return `${Math.round(h * 60)} minutes`;
  if (h < 48) return `${h.toFixed(1)} hours`;
  return `${(h / 24).toFixed(1)} days`;
}

export function titleCase(s: string): string {
  return s.toLowerCase().replace(/\b\w/g, (c) => c.toUpperCase());
}
