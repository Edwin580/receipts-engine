// Loads the engine and one snapshot of NYC 311 data, then answers the
// question in the form: how long the city took to close complaints of one
// type in one area. Every number on the page comes from an engine call made
// here (docs/engine/wasm-api.md); the plans are built in lib.js.

import init, { Engine } from "./pkg/receipts_wasm.js";
import {
  BOROUGHS, MIN_RANKED, waitPlan, keyPlan, fasterPlan, questionFor, whyNotCounted, toSearch, fromSearch, comparePlan, countsPlan, listPlan, topTypesPlan, districtsPlan,
  parseDistrict, districtName, boroughName, areaName, areaKey, parseAreaKey, rankOf, ordinal,
  fmtInt, fmtDuration, fmtWhen, hoursBetween, periodName, typeName, shortHash,
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
const CITY_RECORD = (key) => `https://data.cityofnewyork.us/resource/erm2-nwe9.json?unique_key=${key}`;
const PAGE = 10;
const LIST_MAX = 200;
const TYPES = 30;

const state = {
  engine: null,
  snapshot: null,
  period: "",
  fetchedAt: "",
  q: { type: null, area: { kind: "city" } },
  complaint: null,  // the complaint number being checked, if any
  order: "slowest",
  list: [],
  columns: [],
  shown: 0,
};

function setStatus(text, error = false) {
  const s = $("status");
  s.textContent = text;
  s.classList.toggle("error", error);
  s.hidden = !text;
}

async function fetchBytes(url, onProgress) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`couldn't download ${url} (HTTP ${res.status})`);
  const total = Number(res.headers.get("Content-Length")) || 0;
  if (!res.body || !total) return new Uint8Array(await res.arrayBuffer());
  const chunks = [];
  let at = 0;
  const reader = res.body.getReader();
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    at += value.length;
    // Content-Length can be for an encoded body; don't report past it.
    onProgress(Math.min(at, total), total);
  }
  const out = new Uint8Array(at);
  let o = 0;
  for (const c of chunks) { out.set(c, o); o += c.length; }
  return out;
}

/** Runs a plan and returns the parsed result. The caller drops the execution. */
function run(plan) {
  const r = JSON.parse(state.engine.run(JSON.stringify(plan)));
  if (!r.ok) throw new Error(r.error);
  return r;
}
/** Runs a plan for its output alone. */
function rows(plan) {
  const r = run(plan);
  state.engine.dropExecution(r.execution);
  return r.output.rows;
}

async function load() {
  await init();
  const mb = (n) => (n / 1e6).toFixed(1);
  setStatus("Downloading the city's data.");
  const [manifest, data, log, rejects] = await Promise.all([
    fetch("data/manifest.json").then((r) => r.text()),
    fetchBytes("data/data.arrow", (a, b) => setStatus(`Downloading the city's data: ${mb(a)} of ${mb(b)} MB.`)),
    fetchBytes("data/cleaning_log.arrow", () => {}),
    fetchBytes("data/rejects.arrow", () => {}),
  ]);
  setStatus("Checking the download.");
  await new Promise((r) => setTimeout(r, 0));

  state.engine = new Engine();
  const report = JSON.parse(state.engine.loadSnapshot(manifest, data, log, rejects));
  state.snapshot = report.snapshot_hash;
  const m = JSON.parse(manifest);
  const [from, to] = m.scope.sentence.match(/\d{4}-\d{2}-\d{2}/g) ?? [];
  state.period = from && to ? periodName(from, to) : "";
  state.fetchedAt = m.fetch.finished_at;
  $("data-line").textContent =
    `${fmtInt(report.rows)} complaints filed in ${state.period}, downloaded from the city on ${m.fetch.finished_at.slice(0, 10)}. ` +
    `Before using them, this page checked the download against its published fingerprint (${shortHash(report.snapshot_hash)}).`;

  // The menus are themselves queries.
  const types = rows(topTypesPlan(state.snapshot, TYPES));
  const typeSel = $("type");
  for (const [name] of types) typeSel.append(el("option", { value: name }, typeName(name)));
  state.q.type = (types.find(([n]) => n === "Noise - Residential") ?? types[0])[0];
  typeSel.value = state.q.type;

  const districts = rows(districtsPlan(state.snapshot)).map(([code]) => code).filter(parseDistrict);
  const areaSel = $("area");
  areaSel.append(el("option", { value: "city" }, "the whole city"));
  for (const b of BOROUGHS) {
    const g = el("optgroup", { label: boroughName(b) });
    g.append(el("option", { value: areaKey({ kind: "borough", value: b }) }, `all of ${boroughName(b, true)}`));
    for (const code of districts.filter((c) => parseDistrict(c).borough === b).sort()) {
      g.append(el("option", { value: areaKey({ kind: "district", value: code }) }, districtName(code)));
    }
    areaSel.append(g);
  }
  areaSel.append(el("option", { value: "zip" }, "a ZIP code"));

  typeSel.addEventListener("change", () => { forgetComplaint(); state.q.type = typeSel.value; answer(); });
  areaSel.addEventListener("change", () => {
    forgetComplaint();
    if (areaSel.value === "zip") {
      $("zip-wrap").hidden = false;
      $("zip").focus();
      if (/^\d{5}$/.test($("zip").value)) setArea({ kind: "zip", value: $("zip").value });
      fitSelects();
      return;
    }
    $("zip-wrap").hidden = true;
    setArea(parseAreaKey(areaSel.value));
  });
  $("zip").addEventListener("input", () => {
    const v = $("zip").value.trim();
    if (/^\d{5}$/.test(v)) { forgetComplaint(); setArea({ kind: "zip", value: v }); }
  });
  $("order").addEventListener("change", () => { state.order = $("order").value; listComplaints(); fitSelects(); });
  $("more").addEventListener("click", showMore);
  $("lookup").addEventListener("submit", (ev) => { ev.preventDefault(); lookUp($("key").value); });
  $("copy-link").addEventListener("click", copyLink);

  typeSel.disabled = false;
  areaSel.disabled = false;
  $("lookup").hidden = false;
  setStatus("");

  // A shared link holds the question, and maybe a complaint to check.
  const want = fromSearch(location.search);
  // A type outside the menu is used only if the data has it.
  if (want.type && ![...typeSel.options].some((o) => o.value === want.type)
      && rows(countsPlan(state.snapshot, { type: want.type, area: { kind: "city" } }))[0][0] === 0) {
    want.type = null;
  }
  if (want.type) {
    const known = (a) => a.kind === "zip" || [...areaSel.options].some((o) => o.value === areaKey(a));
    setQuestion({ type: want.type, area: known(want.area) ? want.area : { kind: "city" } });
  } else {
    answer();
  }
  if (want.complaint) {
    $("key").value = want.complaint;
    lookUp(want.complaint);
  }
  document.fonts?.ready.then(fitSelects);
}

function forgetComplaint() {
  $("yours").hidden = true;
  state.complaint = null;
}

/** Keeps the address bar on the current question, so it can be shared. */
function updateUrl() {
  history.replaceState(null, "", (toSearch(state.q, state.complaint) || location.pathname) + location.hash);
}

async function copyLink() {
  const b = $("copy-link");
  try {
    await navigator.clipboard.writeText(location.href);
    b.textContent = "Link copied";
  } catch {
    b.textContent = "Couldn't copy; the address bar has the link";
  }
  setTimeout(() => { b.textContent = "Copy a link to this answer"; }, 2500);
}

/** Points the menus at `q`, adding its type if the menu doesn't list it. */
function setQuestion(q) {
  const typeSel = $("type");
  if (![...typeSel.options].some((o) => o.value === q.type)) {
    typeSel.append(el("option", { value: q.type }, typeName(q.type)));
  }
  typeSel.value = q.type;
  state.q.type = q.type;
  setArea(q.area);
}

/** "Check it": find the complaint, switch to its question, say where it stands. */
function lookUp(raw) {
  const out = $("yours");
  out.hidden = false;
  const key = raw.trim().replace(/^#/, "");
  if (!/^\d{1,18}$/.test(key)) {
    out.replaceChildren(el("p", {}, "A complaint number is all digits, like 65671892."));
    return;
  }
  const found = run(keyPlan(state.snapshot, key));
  const ids = state.engine.traceBackRows(found.execution, 0);
  state.engine.dropExecution(found.execution);
  if (ids.length === 0) {
    out.replaceChildren(el("p", {}, `Complaint #${key} isn't in this data, which covers complaints filed in ${state.period}.`));
    state.complaint = null;
    updateUrl();
    return;
  }
  const recs = JSON.parse(state.engine.sourceRecords(state.snapshot, ids.subarray(0, 1)));
  const rec = Object.fromEntries(recs.columns.map((c, i) => [c, recs.rows[0][i]]));
  const q = questionFor(rec);
  setQuestion(q);

  // Where it stands in the answer now on the page.
  const plan = waitPlan(state.snapshot, q);
  const wait = run(plan);
  const trace = JSON.parse(state.engine.traceForward(wait.execution, ids[0]));
  const closed = wait.output.rows[0][0];
  state.engine.dropExecution(wait.execution);

  const what = el("p", {},
    el("a", { class: "key", href: CITY_RECORD(rec.unique_key), title: "The city's record for this complaint" }, `#${rec.unique_key}`),
    ` ${rec.descriptor ?? typeName(rec.complaint_type)}, ${areaName(q.area, true)}. Filed ${fmtWhen(rec.created_date)}` +
    (rec.closed_date ? `, closed ${fmtWhen(rec.closed_date)}.` : "."));
  const why = whyNotCounted(plan, trace.dropped);
  let verdict;
  if (!why) {
    const hours = hoursBetween(rec.created_date, rec.closed_date);
    const faster = rows(fasterPlan(state.snapshot, q, hours))[0][0];
    // Counts, not a rounded percentage: 861 of 862 is not "100%".
    verdict = `It took ${fmtDuration(hours)}, and it is one of the ${fmtInt(closed)} complaints counted above. ` +
      (faster === 0 ? "None of them was closed faster." : `${fmtInt(faster)} of them were closed faster.`);
  } else if (why === "open") {
    // The download time is UTC and the filed time NYC local, so "about".
    const open = hoursBetween(rec.created_date, state.fetchedAt.replace("Z", ""));
    verdict = `It had no closed date when the data was downloaded on ${state.fetchedAt.slice(0, 10)}, about ${fmtDuration(open)} after it was filed, so it isn't counted above. Complaints still open are left out of every answer on this page.`;
  } else if (why === "backwards") {
    verdict = "Its closed date is earlier than its filed date, so it's left out of the count above as a data error.";
  } else {
    verdict = `It isn't counted above. The step that left it out: ${trace.dropped.sentence}`;
  }
  out.replaceChildren(what, el("p", {}, verdict));
  state.complaint = key;
  updateUrl();
}

function setArea(area) {
  state.q.area = area;
  if (area.kind === "zip") {
    $("area").value = "zip";
    $("zip-wrap").hidden = false;
    $("zip").value = area.value;
  } else {
    $("area").value = areaKey(area);
    $("zip-wrap").hidden = true;
  }
  answer();
}

/** The whole page for the current question. */
function answer() {
  const { q, snapshot, engine } = state;
  const what = typeName(q.type);
  const where = areaName(q.area, true);

  const wait = run(waitPlan(snapshot, q));
  const [closed, median, p90] = wait.output.rows[0];
  const [filed, withDate] = rows(countsPlan(snapshot, q))[0];
  const stillOpen = filed - withDate;
  const backwards = withDate - closed;

  $("answer").hidden = false;
  if (closed === 0) {
    $("headline").replaceChildren(filed === 0
      ? `No ${what} complaints were filed in ${where} in ${state.period}.`
      : `None of the ${fmtInt(filed)} ${what} complaints filed in ${where} in ${state.period} has been closed.`);
  } else {
    $("headline").replaceChildren(
      el("span", { class: "lead" }, `${what} complaints in ${where}, ${state.period}`),
      "Half were closed within ", el("b", {}, fmtDuration(median)), ".",
      // replaceChildren would print a null, so leave the line out instead.
      ...(closed >= 10 ? [el("span", { class: "tail" }, "9 in 10 were closed within ", el("b", {}, fmtDuration(p90)), ".")] : []),
    );
  }

  // Context: citywide, and the rank among neighbours.
  const ctx = [];
  if (closed > 0 && q.area.kind !== "city") {
    const city = rows(waitPlan(snapshot, { type: q.type, area: { kind: "city" } }))[0];
    ctx.push(`Citywide, half were closed within ${fmtDuration(city[1])}, and 9 in 10 within ${fmtDuration(city[2])}.`);
  }
  const borough = q.area.kind === "borough" ? q.area.value
    : q.area.kind === "district" ? parseDistrict(q.area.value)?.borough : null;
  const cmp = rows(comparePlan(snapshot, { type: q.type, borough }));
  const slowest = (r) => `${r.rank === 1 ? "" : ordinal(r.rank) + " "}slowest`;
  if (q.area.kind === "district" && closed > 0) {
    const r = rankOf(cmp, q.area.value, true);
    if (r) ctx.push(`That makes it the ${slowest(r)} of ${r.of} districts in ${boroughName(borough, true)} for these complaints.`);
  } else if (q.area.kind === "borough" && closed > 0) {
    const r = rankOf(rows(comparePlan(snapshot, { type: q.type, borough: null })), q.area.value, false);
    if (r) ctx.push(`That makes it the ${slowest(r)} of the ${r.of} boroughs.`);
  }
  if (closed > 0 && closed < MIN_RANKED) ctx.push(`Only ${fmtInt(closed)} were closed, so one or two complaints can move this a lot.`);
  $("context").textContent = ctx.join(" ");

  const cav = [`Counted: ${fmtInt(closed)} closed complaint${closed === 1 ? "" : "s"}.`];
  if (stillOpen > 0) cav.push(`${fmtInt(stillOpen)} more had no closed date when the data was downloaded and aren't counted.`);
  if (backwards > 0) cav.push(`${fmtInt(backwards)} marked closed before they were opened are left out.`);
  $("caveats").textContent = cav.join(" ");

  // How much any one complaint can move the answer.
  $("sway").textContent = "";
  if (closed >= 2) {
    const c = JSON.parse(engine.contributions(wait.execution, 0, "median_hours", 1));
    const top = c.top[0];
    if (top && top.without !== null) {
      const delta = Math.abs(top.without - c.current);
      $("sway").textContent = delta * 60 < 1
        ? "Leaving out any one of these complaints would change the answer above by less than a minute."
        : `Leaving out any one of these complaints would change the answer above by at most ${fmtDuration(delta)}.`;
    }
  }

  $("how-section").hidden = false;
  $("pipeline").replaceChildren(...wait.steps.map((s) =>
    el("li", {}, el("span", {}, s.sentence), el("span", { class: "counts" }, `${fmtInt(s.rows)} rows`))));
  engine.dropExecution(wait.execution);

  renderCompare(cmp, borough);
  listComplaints();
  fitSelects();
  updateUrl();
}

function renderCompare(cmp, borough) {
  const byDistrict = Boolean(borough);
  const shown = cmp.filter(([g, , m]) => m !== null && (byDistrict ? parseDistrict(g) : BOROUGHS.includes(g)));
  $("compare-section").hidden = shown.length === 0;
  $("compare-head").textContent = byDistrict ? `Districts in ${boroughName(borough, true)}` : "The five boroughs";
  $("compare-group").textContent = byDistrict ? "district" : "borough";
  $("compare-caption").textContent = `${typeName(state.q.type)} complaints, slowest first. Pick one to see it.`;
  const max = Math.max(...shown.map((r) => r[2])) || 1;
  const current = state.q.area.kind === "district" || state.q.area.kind === "borough" ? state.q.area.value : null;
  $("compare").tBodies[0].replaceChildren(...shown.map(([g, n, m, p90]) => {
    const area = byDistrict ? { kind: "district", value: g } : { kind: "borough", value: g };
    const few = n < MIN_RANKED;
    return el("tr", { class: g === current ? "selected" : "" },
      el("td", { class: "group" }, el("button", { type: "button", class: "cell", onclick: () => { forgetComplaint(); setArea(area); } },
        byDistrict ? districtName(g) : boroughName(g))),
      el("td", { class: "num" }, fmtInt(n)),
      el("td", few ? { class: "num few", title: "Too few complaints to rank" } : { class: "num" }, fmtDuration(m)),
      el("td", { class: few ? "num few tailcol" : "num tailcol" }, n >= 10 ? fmtDuration(p90) : ""),
      el("td", { class: "bar", "aria-hidden": "true" }, el("span", { style: `width:${(100 * m / max).toFixed(1)}%` })),
    );
  }));
}

function listComplaints() {
  const r = run(listPlan(state.snapshot, state.q, state.order, LIST_MAX));
  state.engine.dropExecution(r.execution);
  state.columns = r.output.columns.map((c) => c.name);
  state.list = r.output.rows;
  state.shown = 0;
  $("list-section").hidden = state.list.length === 0;
  $("list").replaceChildren();
  showMore();
}

function showMore() {
  const col = Object.fromEntries(state.columns.map((n, i) => [n, i]));
  const next = state.list.slice(state.shown, state.shown + PAGE);
  $("list").append(...next.map((r) => {
    const v = (name) => r[col[name]];
    const place = [v("location_type"), v("incident_zip") && `ZIP ${v("incident_zip")}`].filter(Boolean).join(", ");
    return el("li", {},
      el("span", { class: "what" },
        el("a", { class: "key", href: CITY_RECORD(v("unique_key")), title: "The city's record for this complaint" }, `#${v("unique_key")}`),
        " ", v("descriptor") ?? typeName(v("complaint_type"))),
      el("span", { class: "took" }, fmtDuration(hoursBetween(v("created_date"), v("closed_date")))),
      el("span", { class: "meta" }, `Filed ${fmtWhen(v("created_date"))}, closed ${fmtWhen(v("closed_date"))}.${place ? " " + place + "." : ""}`),
    );
  }));
  state.shown += next.length;
  const more = Math.min(PAGE, state.list.length - state.shown);
  $("more").hidden = more <= 0;
  $("more").textContent = `Show ${more} more`;
}

// A select is as wide as its longest option; size each to its chosen one
// so the sentence reads without gaps.
function fitSelects() {
  const probe = el("span", { style: "position:absolute;visibility:hidden;white-space:pre" });
  document.body.append(probe);
  for (const s of document.querySelectorAll(".ask select")) {
    // The computed font shorthand is empty when a property it can't express
    // (font-variant-numeric) is set, so copy the longhands.
    const cs = getComputedStyle(s);
    for (const k of ["fontFamily", "fontSize", "fontStyle", "fontWeight", "letterSpacing"]) probe.style[k] = cs[k];
    probe.textContent = s.selectedOptions[0]?.textContent ?? "";
    const pad = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    s.style.width = `${Math.ceil(probe.getBoundingClientRect().width + pad) + 4}px`;
  }
  probe.remove();
}

load().catch((e) => {
  console.error(e);
  setStatus(`Something went wrong: ${e.message}`, true);
});
