// Every question and known-issue filter must validate and run in the real
// engine (the single-threaded WASM build, in Node). Needs a built snapshot:
//   RECEIPTS_SNAPSHOT_DIR=<snapshot dir> npm test
// and web/public/pkg/st (tools/wasm/build.sh st). Skipped otherwise.

import { describe, expect, it } from "vitest";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { KNOWN_ISSUE_FILTERS, QUESTIONS, complaintTypesPlan, knownIssuePlan, type Year } from "./plans";

const dir = process.env.RECEIPTS_SNAPSHOT_DIR;
const pkg = fileURLToPath(new URL("../public/pkg/st/", import.meta.url));
const ready = !!dir && existsSync(join(pkg, "receipts_wasm.js"));

if (!ready) {
  describe.skip("plans in the real engine (set RECEIPTS_SNAPSHOT_DIR and build web/public/pkg/st)", () => {
    it("needs a snapshot", () => {});
  });
} else describe("plans in the real engine", async () => {
  const mod = await import(/* @vite-ignore */ join(pkg, "receipts_wasm.js"));
  mod.initSync({ module: readFileSync(join(pkg, "receipts_wasm_bg.wasm")) });
  const engine = new mod.Engine();
  const report = JSON.parse(
    engine.loadSnapshot(
      readFileSync(join(dir!, "manifest.json"), "utf8"),
      readFileSync(join(dir!, "data.arrow")),
      readFileSync(join(dir!, "cleaning_log.arrow")),
      readFileSync(join(dir!, "rejects.arrow")),
    ),
  );
  const snap: string = report.snapshot_hash;
  const types = JSON.parse(engine.run(JSON.stringify(complaintTypesPlan(snap))));
  const noise = types.output.rows.map((r: unknown[]) => r[0]).filter((t: string) => t?.startsWith("Noise"));

  for (const q of QUESTIONS) {
    for (const year of ["2024", "2025", "both"] as Year[]) {
      it(`${q.id} (${year}) validates and runs`, () => {
        const plan = q.plan(snap, { year, complaintTypes: noise.length ? noise : ["Noise - Residential"] });
        const v = JSON.parse(engine.validatePlan(JSON.stringify(plan)));
        expect(v.error).toBeUndefined();
        const r = JSON.parse(engine.run(JSON.stringify(plan)));
        expect(r.ok).toBe(true);
        const names = r.output.columns.map((c: { name: string }) => c.name);
        expect(names).toContain(q.measure);
        expect(names).toContain(q.label);
        // Executions hold their tables; drop them as the app does.
        engine.dropExecution(r.execution);
      });
    }
  }

  for (const id of Object.keys(KNOWN_ISSUE_FILTERS)) {
    it(`known issue ${id} selects rows`, () => {
      const rows: Uint32Array = engine.matchingRows(JSON.stringify(knownIssuePlan(snap, id)));
      const expected = report.known_issues.find((k: { id: string }) => k.id === id)?.count;
      // The filter must agree with the count the snapshot builder computed.
      expect(rows.length).toBe(expected);
    });
  }
});
