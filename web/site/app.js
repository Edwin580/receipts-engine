// The demo page. Loads the single threaded engine build and one snapshot,
// then drives the engine's JS API (docs/engine/wasm-api.md) from the
// controls. Every number shown comes from an engine call made here.

import init, { Engine } from "./pkg/receipts_wasm.js";
import {
  METRICS, buildPlan, topTypesPlan, fmtInt, fmtValue, hoursBetween, fmtDuration, fmtWhen,
  findGroup, flipBit, shortHash, reasonKey, fmtPrecise,
} from "./lib.js";

const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) e.append(k);
  return e;
};
const ms = (x) => (x < 10 ? x.toFixed(1) : Math.round(x).toString()) + " ms";
const SOURCE_ROW = (key) => `https://data.cityofnewyork.us/resource/erm2-nwe9.json?unique_key=${key}`;
const PAGE = 10;
const MOVERS = 10;

const state = {
  engine: null,
  report: null,
  files: null,
  query: { type: null, groupBy: "borough", metric: "mean", dropBackwards: true },
  plan: null,
  base: null,       // run() result
  cf: null,         // exclude() result, or null
  key: undefined,   // group value of the selected row
  ids: null,        // source rows behind the selected number
  shown: 0,
  movers: null,     // contributions() result
};

function setStatus(text, error = false) {
  const s = $("status");
  s.textContent = text;
  s.classList.toggle("error", error);
}

async function fetchBytes(url, onProgress) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`couldn't download ${url} (HTTP ${res.status})`);
  const total = Number(res.headers.get("Content-Length")) || 0;
  if (!res.body || !total) return new Uint8Array(await res.arrayBuffer());
  const out = new Uint8Array(total);
  const reader = res.body.getReader();
  let at = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    if (at + value.length > total) {
      // Content-Length was for an encoded body; fall back to growing.
      const grown = new Uint8Array(Math.max(total * 2, at + value.length));
      grown.set(out.subarray(0, at));
      return finishGrowing(reader, grown, at, value, onProgress, total);
    }
    out.set(value, at);
    at += value.length;
    onProgress(at, total);
  }
  return out.subarray(0, at);
}

async function finishGrowing(reader, buf, at, first, onProgress, total) {
  let chunks = [buf.subarray(0, at), first];
  at += first.length;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    at += value.length;
    onProgress(Math.min(at, total), total);
  }
  const out = new Uint8Array(at);
  let o = 0;
  for (const c of chunks) { out.set(c, o); o += c.length; }
  return out;
}

async function load() {
  await init();
  setStatus("Downloading one month of NYC 311 requests.");
  const mb = (n) => (n / 1e6).toFixed(1);
  const t0 = performance.now();
  const [manifest, data, log, rejects] = await Promise.all([
    fetch("data/manifest.json").then((r) => r.text()),
    fetchBytes("data/data.arrow", (a, b) => setStatus(`Downloading one month of NYC 311 requests: ${mb(a)} of ${mb(b)} MB.`)),
    fetchBytes("data/cleaning_log.arrow", () => {}),
    fetchBytes("data/rejects.arrow", () => {}),
  ]);
  const downloadMs = performance.now() - t0;
  setStatus("Checking every hash.");
  await new Promise((r) => setTimeout(r, 0));

  state.engine = new Engine();
  state.files = { manifest, data, log, rejects };
  state.report = JSON.parse(state.engine.loadSnapshot(manifest, data, log, rejects));
  const m = JSON.parse(manifest);
  state.manifest = m;
  const r = state.report;
  const [from, to] = m.scope.sentence.match(/\d{4}-\d{2}-\d{2}/g) ?? [];
  const last = to ? new Date(Date.parse(to + "T00:00:00Z") - 86_400_000).toISOString().slice(0, 10) : "";
  setStatus(
    `Demo data: ${fmtInt(r.rows)} real requests opened ${from} to ${last}. ` +
    `Downloaded ${mb(data.length)} MB in ${ms(downloadMs)}, then checked every hash and loaded it in ${ms(r.verify_ms)}.`,
  );
  $("scope").textContent =
    `${m.scope.sentence.replace(/\.$/, "")}. I fetched them from the city's API on ${m.fetch.finished_at.slice(0, 10)}.`;

  // The complaint type menu is itself a query.
  const top = JSON.parse(state.engine.run(JSON.stringify(topTypesPlan(r.snapshot_hash, 15))));
  const typeSel = $("type");
  typeSel.append(el("option", { value: "" }, "all"));
  for (const [name, n] of top.output.rows) typeSel.append(el("option", { value: name }, `${name} (${fmtInt(n)})`));
  state.engine.dropExecution(top.execution);
  const preferred = top.output.rows.find(([n]) => n === "Noise - Residential") ?? top.output.rows[0];
  typeSel.value = preferred[0];
  state.query.type = preferred[0];

  const backwards = (r.known_issues || []).find((k) => k.id === "KI-closed-before-created");
  if (backwards) $("backwards-note").textContent = ` (${fmtInt(backwards.count)} in this month, across all types)`;

  for (const id of ["metric", "type", "group", "drop-backwards"]) $(id).disabled = false;
  setupVerify();
  runQuery();
  fitSelects();
  document.fonts?.ready.then(fitSelects);
}

// A select is as wide as its longest option; size each to its chosen one
// so the sentence reads without gaps.
function fitSelects() {
  const probe = el("span", { style: "position:absolute;visibility:hidden;white-space:pre" });
  document.body.append(probe);
  for (const id of ["metric", "type", "group"]) {
    const sel = $(id);
    const cs = getComputedStyle(sel);
    for (const k of ["fontFamily", "fontSize", "fontStyle", "fontWeight", "letterSpacing"]) probe.style[k] = cs[k];
    probe.textContent = sel.options[sel.selectedIndex].text;
    sel.style.width = `${Math.ceil(probe.getBoundingClientRect().width + parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight)) + 4}px`;
  }
  probe.remove();
}

function readControls() {
  state.query = {
    type: $("type").value || null,
    groupBy: $("group").value,
    metric: $("metric").value,
    dropBackwards: $("drop-backwards").checked,
  };
  $("judgement-line").hidden = state.query.metric === "count";
}

function dropCf() {
  if (state.cf) state.engine.dropExecution(state.cf.execution);
  state.cf = null;
}

function runQuery() {
  readControls();
  fitSelects();
  dropCf();
  if (state.base) state.engine.dropExecution(state.base.execution);
  state.plan = buildPlan(state.report.snapshot_hash, state.query);
  const run = JSON.parse(state.engine.run(JSON.stringify(state.plan)));
  if (!run.ok) {
    state.base = null;
    setStatus(`The engine refused the plan: ${run.error}`, true);
    return;
  }
  state.base = run;
  const rows = run.output.rows;
  // Keep the same group selected if it still exists.
  // Otherwise start with the largest group, which makes the best example.
  let i = findGroup(rows, state.key);
  if (i < 0) i = rows.reduce((best, r, j) => (r[1] > rows[best][1] ? j : best), 0);
  renderTable();
  $("run-caption").textContent =
    `Ran ${run.steps.length} steps over ${fmtInt(run.steps[0].rows)} rows in ${ms(run.ms)}. ` +
    "Click a number to see where it came from.";
  $("plan-json").textContent = JSON.stringify(state.plan, null, 1);
  for (const s of ["receipts", "whatif", "verify"]) $(s).hidden = false;
  if (rows.length) select(i);
}

function valueColumn() {
  return state.base.output.columns.length - 1;
}

function renderTable() {
  const { metric, groupBy } = state.query;
  const base = state.base;
  const vc = valueColumn();
  $("group-head").textContent = { borough: "borough", channel: "reported by", agency: "agency" }[groupBy];
  $("value-head").textContent = { mean: "mean hours", median: "median hours", count: "requests" }[metric];
  // For count the value column is the requests column; don't show it twice.
  $("result").querySelector("thead th:nth-child(2)").hidden = metric === "count";

  const cfRows = state.cf ? state.cf.output.rows : null;
  const max = Math.max(...base.output.rows.map((r) => r[vc] ?? 0), 1e-9);
  const tbody = $("result").querySelector("tbody");
  tbody.replaceChildren();
  base.output.rows.forEach((row, i) => {
    const [key, requests] = row;
    const value = row[vc];
    const after = cfRows ? cfRows[findGroup(cfRows, key)] : row;
    const gone = after === undefined;
    const changed = !gone && cfRows && (after[vc] !== value || after[1] !== requests);
    const shown = gone ? null : after[vc];
    const label = key ?? "(missing)";
    const valueCell = el("td", { class: "num" },
      gone ? el("span", { class: "gone" }, "gone")
        : el("button", {
          type: "button", class: "cell",
          "aria-label": `${METRICS[metric].label} for ${label}: ${fmtValue(metric, shown)}. Show where it came from.`,
          onclick: () => select(i),
        }, fmtValue(metric, shown)),
      changed ? el("span", { class: "was" }, `was ${fmtValue(metric, value)}`) : null,
    );
    const tr = el("tr", { class: key === state.key ? "selected" : "" },
      el("td", { class: "group" }, label),
      metric === "count" ? null : el("td", { class: "num" }, fmtInt(gone ? 0 : after[1]),
        changed && after[1] !== requests ? el("span", { class: "was" }, `was ${fmtInt(requests)}`) : null),
      valueCell,
      el("td", { class: "bar", "aria-hidden": "true" }, el("span", { style: `width:${(100 * Math.max(0, shown ?? 0)) / max}%` })),
    );
    tbody.append(tr);
  });
}

function select(i) {
  dropCf();
  const base = state.base;
  const row = base.output.rows[i];
  state.key = row[0];
  const { metric } = state.query;
  const vc = valueColumn();

  const trace = JSON.parse(state.engine.traceBack(base.execution, i));
  state.ids = state.engine.traceBackRows(base.execution, i);
  const label = row[0] ?? "requests with no " + state.query.groupBy;
  $("selected-line").replaceChildren(
    `You picked `, el("b", {}, label), `: ${METRICS[metric].label} `,
    el("b", {}, fmtValue(metric, row[vc])), metric === "count" ? ". " : ` over ${fmtInt(row[1])} requests. `,
    `The engine traced it back to ${fmtInt(trace.source_rows)} rows of the snapshot in ${ms(trace.ms)}.`,
  );

  // Pipeline: every step's row count, and how many of those rows feed this number.
  const traced = new Map(trace.path.map((p) => [p.step, p.rows]));
  const ol = $("pipeline");
  ol.replaceChildren(...base.steps.map((s, k) => el("li", {},
    el("span", {}, s.sentence),
    el("span", { class: "counts" }, `${fmtInt(s.rows)} rows`,
      traced.has(k) ? el("span", { class: "traced" }, `${fmtInt(traced.get(k))} feed it`) : null),
  )));

  state.shown = 0;
  $("receipt-list").replaceChildren();
  $("receipts-caption").textContent =
    `In snapshot order, which is by creation time. Each number links to the same request in the city's own API, so you can check it.`;
  showMore();

  // What if.
  $("whatif-body").hidden = metric === "count";
  $("whatif-count").hidden = metric !== "count";
  state.movers = null;
  if (metric !== "count") {
    state.movers = JSON.parse(state.engine.contributions(base.execution, i, METRICS[metric].column, MOVERS));
    const k = $("k");
    k.max = String(state.movers.top.length);
    k.value = "0";
    renderMovers();
  }
  renderTable();
}

function recordLine(cols, r) {
  const c = Object.fromEntries(cols.map((n, j) => [n, r[j]]));
  const hours = hoursBetween(c.created_date, c.closed_date);
  return {
    c,
    what: [c.complaint_type, c.descriptor].filter(Boolean).join(", "),
    meta: [
      `opened ${fmtWhen(c.created_date)}`,
      c.closed_date ? `closed after ${fmtDuration(hours)}` : "not closed",
      c.borough, c.agency, c.channel && c.channel.toLowerCase(),
    ].filter(Boolean).join(" · "),
  };
}

function showMore() {
  const ids = state.ids;
  const next = ids.subarray(state.shown, state.shown + PAGE);
  if (next.length) {
    const recs = JSON.parse(state.engine.sourceRecords(state.report.snapshot_hash, next));
    const list = $("receipt-list");
    recs.rows.forEach((r, j) => {
      const { c, what, meta } = recordLine(recs.columns, r);
      list.append(el("li", {},
        el("a", { class: "key", href: SOURCE_ROW(c.unique_key), title: `Snapshot row ${next[j]}` }, `#${c.unique_key}`),
        " ", what,
        el("span", { class: "meta" }, meta),
      ));
    });
    state.shown += next.length;
  }
  $("receipts-head").textContent = `The requests (${fmtInt(state.shown)} of ${fmtInt(ids.length)} shown)`;
  $("more").hidden = state.shown >= ids.length;
}

function renderMovers() {
  const mv = state.movers;
  const k = Number($("k").value);
  $("k-out").textContent = String(k);
  const metric = state.query.metric;
  const recs = JSON.parse(state.engine.sourceRecords(
    state.report.snapshot_hash, new Uint32Array(mv.top.map((t) => t.source_row))));
  $("movers").replaceChildren(...mv.top.map((t, j) => {
    const { c, what, meta } = recordLine(recs.columns, recs.rows[j]);
    return el("li", { class: j < k ? "out" : "" },
      el("span", {},
        el("span", { class: "what" }, el("a", { class: "key", href: SOURCE_ROW(c.unique_key) }, `#${c.unique_key}`), " ", what),
        el("span", { class: "meta" }, meta)),
      el("span", { class: "without" }, (j < k ? "left out · " : "") + (t.removes_group ? "group gone" : `alone: ${fmtPrecise(t.without)}`)),
    );
  }));
  applyExclusion(k);
}

function applyExclusion(k) {
  dropCf();
  const mv = state.movers;
  const vc = valueColumn();
  const metric = state.query.metric;
  const label = METRICS[metric].label;
  const header =
    `The engine scored all ${fmtInt(mv.contributing_rows)} requests in ${ms(mv.ms)}; ` +
    `${fmtInt(mv.rows_that_change_it)} of them change the number when left out alone. ` +
    `"Alone" is the value without just that one request, to three decimals.` +
    (metric === "median" ? " A median moves by one position per request, so many requests tie." : "");
  if (k === 0) {
    $("effect").replaceChildren(header);
    renderTable();
    return;
  }
  const rows = new Uint32Array(mv.top.slice(0, k).map((t) => t.source_row));
  state.cf = JSON.parse(state.engine.exclude(state.base.execution, rows));
  const before = state.base.output.rows[findGroup(state.base.output.rows, state.key)][vc];
  const j = findGroup(state.cf.output.rows, state.key);
  const after = j < 0 ? null : state.cf.output.rows[j][vc];
  const how = state.cf.method.kind === "incremental"
    ? `recomputed ${state.cf.method.groups_recomputed} group${state.cf.method.groups_recomputed === 1 ? "" : "s"} from their remaining rows`
    : state.cf.method.kind === "rerun" ? "re-ran the plan" : "found nothing to change";
  $("effect").replaceChildren(
    `Without these ${k}, the ${label} goes from `,
    el("span", { class: "num" }, fmtPrecise(before)), " to ",
    el("span", { class: "num" }, after === null ? "nothing (the group is gone)" : fmtPrecise(after)),
    `. The engine ${how} in ${ms(state.cf.ms)}, and the table above shows the new values. `,
    header,
  );
  renderTable();
}

// Verify demo.
function setupVerify() {
  const r = state.report;
  const t = r.timings;
  $("verify-timings").replaceChildren(
    `On this load, decoding the ${fmtInt(r.rows)} rows took ${ms(t.decode_ms)}, re-hashing them took ${ms(t.hash_ms)}, ` +
    `and checking their order took ${ms(t.sort_check_ms)}. Snapshot `,
    el("span", { class: "key", title: r.snapshot_hash }, shortHash(r.snapshot_hash, 16)),
    ` matched. Now break it.`,
  );
  const n = state.files.data.length;
  const off = $("offset");
  off.max = String(n - 1);
  off.value = String(randomOffset(n));
  $("offset-of").textContent = `of data.arrow's ${fmtInt(n)} bytes,`;
  $("random-offset").addEventListener("click", () => {
    off.value = String(randomOffset(n));
    $("tamper-out").textContent = "";
    $("tally").replaceChildren();
  });
  $("tamper").addEventListener("click", tamper);
  $("tamper-many").addEventListener("click", tamperMany);
}

function randomOffset(n) {
  return Math.floor(Math.random() * n);
}

/** Loads the snapshot in a fresh engine with one bit of data.arrow flipped. */
function tryFlip(offset) {
  const f = state.files;
  const bad = flipBit(f.data, offset);
  const probe = new Engine();
  const t0 = performance.now();
  try {
    const rep = JSON.parse(probe.loadSnapshot(f.manifest, bad, f.log, f.rejects));
    return { ok: true, ms: performance.now() - t0, hash: rep.snapshot_hash };
  } catch (e) {
    return { ok: false, ms: performance.now() - t0, message: e.message };
  } finally {
    probe.free();
  }
}

function tamper() {
  const n = state.files.data.length;
  const off = Math.min(n - 1, Math.max(0, Math.floor(Number($("offset").value) || 0)));
  $("offset").value = String(off);
  $("tally").replaceChildren();
  const out = $("tamper-out");
  const r = tryFlip(off);
  out.classList.toggle("refused", !r.ok);
  out.textContent = r.ok
    ? `Loaded in ${ms(r.ms)} with snapshot ${shortHash(r.hash, 16)}. Every hash still matched. Hashes cover the ` +
      "decoded values, not the file layout, so this bit changed no value the engine reads. Try another byte."
    : `Refused in ${ms(r.ms)}: ${r.message}.`;
}

async function tamperMany() {
  const n = state.files.data.length;
  const runs = 25;
  const btn = $("tamper-many");
  btn.disabled = $("tamper").disabled = true;
  const out = $("tamper-out");
  out.classList.remove("refused");
  const tally = new Map();
  let loaded = 0;
  let total = 0;
  for (let i = 0; i < runs; i++) {
    out.textContent = `Flipping bit ${i + 1} of ${runs}.`;
    await new Promise((r) => setTimeout(r, 0));
    const r = tryFlip(randomOffset(n));
    total += r.ms;
    if (r.ok) { loaded++; continue; }
    const key = reasonKey(r.message);
    const t = tally.get(key) ?? { n: 0, example: r.message };
    t.n++;
    tally.set(key, t);
  }
  out.textContent =
    `${runs} random bits, each flipped in a fresh copy of the file: ${runs - loaded} refused, ${loaded} loaded with every ` +
    `hash intact, none crashed. ${ms(total / runs)} per attempt on average. What caught them:`;
  $("tally").replaceChildren(...[...tally.values()].sort((a, b) => b.n - a.n).map((t) =>
    el("li", {}, el("span", { class: "key" }, `${t.n}×`), " ", t.example)));
  btn.disabled = $("tamper").disabled = false;
}

for (const id of ["metric", "type", "group", "drop-backwards"]) $(id).addEventListener("change", runQuery);
$("more").addEventListener("click", showMore);
$("k").addEventListener("input", () => renderMovers());

load().catch((e) => {
  console.error(e);
  setStatus(`Something went wrong loading the engine: ${e.message}`, true);
});
