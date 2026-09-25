import { describe, expect, it } from "vitest";
import { barScale } from "./components/BarChart";
import { Builder, QUESTIONS, hours, knownIssuePlan, leadingRow, titleCase, yearFilter } from "./plans";

const SNAP = "3b".repeat(32);
const params = { year: "2025" as const, complaintTypes: ["Noise - Residential"] };

describe("plan builder", () => {
  it("chains steps through their inputs", () => {
    const p = new Builder(SNAP).filter({ column: "x" }).limit(3).build();
    expect(p.nodes.map((n) => n.input)).toEqual([undefined, 0, 1]);
    expect(p.output).toBe(2);
    expect(p.nodes[2]).toEqual({ op: "limit", count: "3", input: 1 });
  });

  it("scopes a year as a half-open range", () => {
    expect(JSON.stringify(yearFilter("2025"))).toContain("2026-01-01T00:00:00.000000");
    expect(yearFilter("both")).toBeUndefined();
  });

  it("builds every question with its measure and label in the output", () => {
    for (const q of QUESTIONS) {
      const p = q.plan(SNAP, params);
      expect(p.nodes[0]).toEqual({ op: "scan", snapshot: SNAP });
      const agg = p.nodes.find((n) => n.op === "aggregate") as { group_by: string[]; aggregates: { name: string }[] };
      expect(agg.group_by).toContain(q.label);
      expect(agg.aggregates.map((a) => a.name)).toContain(q.measure);
    }
  });

  it("selects known issues", () => {
    expect(knownIssuePlan(SNAP, "KI-created-midnight")?.nodes).toHaveLength(2);
    expect(knownIssuePlan(SNAP, "KI-zip-not-5-digits")).toBeUndefined();
  });
});

describe("headlines", () => {
  it("skip groups too small to mean anything", () => {
    expect(leadingRow([35, 443530, 200686])).toBe(1);
    expect(leadingRow([5000, 20])).toBe(0);
    expect(leadingRow([3, 4])).toBe(0);
    expect(leadingRow([null, null])).toBe(0);
  });
});

describe("formatting", () => {
  it("reads durations the way people say them", () => {
    expect(hours(0.5)).toBe("30 minutes");
    expect(hours(1.5486)).toBe("1.5 hours");
    expect(hours(72)).toBe("3.0 days");
    expect(titleCase("STATEN ISLAND")).toBe("Staten Island");
  });
});

describe("bar scale", () => {
  it("is linear for comparable values and logarithmic across orders of magnitude", () => {
    const lin = barScale([1, 2, 4]);
    expect(lin.log).toBe(false);
    expect(lin(2)).toBe(50);
    const log = barScale([1.3, 46, 7845]);
    expect(log.log).toBe(true);
    expect(log(1.3)).toBeCloseTo(6);
    expect(log(7845)).toBeCloseTo(100);
    expect(log(0)).toBe(0);
  });
});
