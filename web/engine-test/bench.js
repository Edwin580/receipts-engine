// Browser benchmark for the engine. Query parameters:
//   build=st|mt   single- or multi-threaded package (default st)
//   threads=N     worker threads for mt (default: hardwareConcurrency)
//   snap=/snap/   where manifest.json and the .arrow files are served
// The result is written to window.__result and to the page as JSON.

const params = new URLSearchParams(location.search);
const build = params.get("build") ?? "st";
const snap = params.get("snap") ?? "/snap/";
const out = document.getElementById("out");

const ms = (t0) => Math.round((performance.now() - t0) * 10) / 10;
const median = (xs) => [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)];

async function main() {
  const result = { build, userAgent: navigator.userAgent, crossOriginIsolated, cores: navigator.hardwareConcurrency };
  const mod = await import(`../pkg/${build}/receipts_wasm.js`);
  let t = performance.now();
  const wasm = await mod.default();
  result.init_ms = ms(t);
  if (build === "mt") {
    const threads = Number(params.get("threads") ?? navigator.hardwareConcurrency);
    t = performance.now();
    await mod.initThreadPool(threads);
    result.threads = threads;
    result.thread_pool_ms = ms(t);
  }
  const engine = new mod.Engine();
  result.parallel = mod.Engine.isParallel();

  t = performance.now();
  const [manifest, data, log, rejects] = await Promise.all([
    fetch(snap + "manifest.json").then((r) => r.text()),
    ...["data.arrow", "cleaning_log.arrow", "rejects.arrow"].map((f) =>
      fetch(snap + f).then((r) => r.arrayBuffer()).then((b) => new Uint8Array(b)),
    ),
  ]);
  result.fetch_ms = ms(t);
  result.bytes = data.length;

  t = performance.now();
  const load = JSON.parse(engine.loadSnapshot(manifest, data, log, rejects));
  result.load_ms = ms(t);
  result.load = { rows: load.rows, snapshot_hash: load.snapshot_hash, timings: load.timings };
  const S = load.snapshot_hash;

  const scan = { op: "scan", snapshot: S };
  const plans = {
    "filter one complaint type, count": [
      scan,
      { op: "filter", input: 0, predicate: { call: "eq", args: [{ column: "complaint_type" }, { literal: { str: "Noise - Residential" } }] } },
      { op: "aggregate", input: 1, group_by: [], aggregates: [{ name: "n", fn: "count" }] },
    ],
    "noise: median hours to close by borough": [
      scan,
      { op: "filter", input: 0, predicate: { call: "and", args: [
        { call: "in", args: [{ column: "complaint_type" }], values: [{ str: "Noise - Residential" }, { str: "Noise - Street/Sidewalk" }] },
        { call: "not", args: [{ call: "is_null", args: [{ column: "closed_date" }] }] },
        { call: "ge", args: [{ column: "closed_date" }, { column: "created_date" }] }] } },
      { op: "map", input: 1, name: "hours_to_close", expr: { call: "div", args: [
        { call: "sub", args: [{ column: "closed_date" }, { column: "created_date" }] }, { literal: { i64: "3600000000" } }] } },
      { op: "aggregate", input: 2, group_by: ["borough"], aggregates: [
        { name: "requests", fn: "count" }, { name: "median_hours", fn: "median", column: "hours_to_close" }] },
      { op: "sort", input: 3, keys: [{ column: "median_hours", order: "desc" }] },
    ],
    "top complaint types per month (2025)": [
      scan,
      { op: "filter", input: 0, predicate: { call: "ge", args: [{ column: "created_date" }, { literal: { timestamp: "2025-01-01T00:00:00.000000" } }] } },
      { op: "map", input: 1, name: "month", expr: { call: "date_trunc", unit: "month", args: [{ column: "created_date" }] } },
      { op: "aggregate", input: 2, group_by: ["month", "complaint_type"], aggregates: [{ name: "n", fn: "count" }] },
      { op: "sort", input: 3, keys: [{ column: "n", order: "desc" }] },
      { op: "limit", input: 4, count: "10" },
    ],
    "full sort by closed_date desc, first 10": [
      scan,
      { op: "sort", input: 0, keys: [{ column: "closed_date", order: "desc" }, { column: "unique_key", order: "asc" }] },
      { op: "limit", input: 1, count: "10" },
    ],
  };
  result.plans = {};
  let noise;
  for (const [name, nodes] of Object.entries(plans)) {
    const json = JSON.stringify({ format: "receipts-plan/1", nodes, output: nodes.length - 1 });
    t = performance.now();
    const v = JSON.parse(engine.validatePlan(json));
    const validateMs = ms(t);
    if (!v.ok) throw new Error(`${name}: ${v.error}`);
    const times = [];
    let last;
    for (let i = 0; i < 3; i++) {
      if (last) engine.dropExecution(last.execution);
      t = performance.now();
      last = JSON.parse(engine.run(json));
      times.push(ms(t));
      if (!last.ok) throw new Error(`${name}: ${last.error}`);
    }
    result.plans[name] = { validate_ms: validateMs, run_ms: median(times), output_rows: last.output.total_rows };
    if (name.startsWith("noise")) noise = last;
    else engine.dropExecution(last.execution);
  }

  // Lineage and counterfactuals on the noise plan.
  const id = noise.execution;
  t = performance.now();
  const bronx = engine.traceBackRows(id, 0);
  result.trace_bronx = { rows: bronx.length, cold_ms: ms(t) };
  t = performance.now();
  engine.traceBackRows(id, 0);
  result.trace_bronx.cached_ms = ms(t);
  t = performance.now();
  const recs = JSON.parse(engine.sourceRecords(S, bronx.subarray(0, 20)));
  result.source_records_20_ms = ms(t);
  result.first_receipt = recs.rows[0];

  t = performance.now();
  const one = JSON.parse(engine.exclude(id, bronx.subarray(0, 1)));
  result.exclude_one_bronx = { ms: ms(t), method: one.method };
  // The smallest group: excluding all its rows removes it.
  const smallest = noise.output.rows.reduce((best, r, i, rows) => (r[1] < rows[best][1] ? i : best), 0);
  const uRows = engine.traceBackRows(id, smallest);
  t = performance.now();
  const gone = JSON.parse(engine.exclude(id, uRows));
  result.exclude_group = { ms: ms(t), rows: uRows.length, output_rows_after: gone.output.total_rows, method: gone.method };

  t = performance.now();
  const c = JSON.parse(engine.contributions(id, 0, "median_hours", 5));
  result.contributions_bronx_median = { ms: ms(t), rows: c.contributing_rows, rows_that_change_it: c.rows_that_change_it, exact: c.exact };

  result.wasm_memory_mb = Math.round(wasm.memory.buffer.byteLength / 1e6);
  if (performance.memory) result.js_heap_mb = Math.round(performance.memory.usedJSHeapSize / 1e6);
  result.noise_output = noise.output.rows;
  return result;
}

main()
  .then((r) => {
    window.__result = r;
    out.textContent = JSON.stringify(r, null, 2);
  })
  .catch((e) => {
    window.__result = { error: String(e && e.stack ? e.stack : e) };
    out.textContent = window.__result.error;
  });
