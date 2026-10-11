// The Westchester page: how long the towns that use SeeClickFix took to
// close residents' requests, by request type and town. Same engine and
// plans as the New York City page (../lib.js), on the towns' snapshot.

import { $, el, setStatus, loadSnapshot, fitSelects } from "../page.js";
import {
  MIN_RANKED, waitPlan, townsPlan, townListPlan, countsPlan, listPlan, topTypesPlan,
  areaName, areaKey, parseAreaKey, toSearch, fromSearch, rankOf, ordinal,
  fmtInt, fmtDuration, fmtWhen, hoursBetween, shortHash,
} from "../lib.js";

const REQUEST = (id) => `https://seeclickfix.com/issues/${id}`;
const PAGE = 10;
const LIST_MAX = 200;
const TYPES = 25;
const ALL = { kind: "city" };

const state = {
  engine: null,
  snapshot: null,
  period: "",
  towns: [],
  q: { type: null, area: ALL },
  order: "slowest",
  list: [],
  columns: [],
  shown: 0,
};
let run;
let rows;

const where = (area) => (area.kind === "town" ? area.value : "all five towns");

async function load() {
  const loaded = await loadSnapshot("data", "the towns' records");
  ({ run, rows } = loaded);
  const { report, manifest: m } = loaded;
  state.engine = loaded.engine;
  state.snapshot = report.snapshot_hash;
  state.period = loaded.period;
  $("data-line").textContent =
    `${fmtInt(report.rows)} requests filed from ${state.period}, downloaded from SeeClickFix on ${m.fetch.finished_at.slice(0, 10)}. ` +
    `Before using them, this page checked the download against its published fingerprint (${shortHash(report.snapshot_hash)}).`;

  const types = rows(topTypesPlan(state.snapshot, TYPES));
  const typeSel = $("type");
  for (const [name] of types) typeSel.append(el("option", { value: name }, name));
  state.towns = rows(townListPlan(state.snapshot)).map(([t]) => t).filter(Boolean);
  const areaSel = $("area");
  areaSel.append(el("option", { value: "city" }, "all five towns"));
  for (const t of state.towns) areaSel.append(el("option", { value: areaKey({ kind: "town", value: t }) }, t));

  // A shared link holds the question; anything unknown falls back.
  const want = fromSearch(location.search);
  state.q.type = types.some(([n]) => n === want.type) ? want.type : types[0][0];
  state.q.area = want.area.kind === "town" && state.towns.includes(want.area.value) ? want.area : ALL;
  typeSel.value = state.q.type;
  areaSel.value = areaKey(state.q.area);

  typeSel.addEventListener("change", () => { state.q.type = typeSel.value; answer(); });
  areaSel.addEventListener("change", () => { state.q.area = parseAreaKey(areaSel.value); answer(); });
  $("order").addEventListener("change", () => { state.order = $("order").value; listRequests(); fitSelects(); });
  $("more").addEventListener("click", showMore);
  $("copy-link").addEventListener("click", copyLink);
  typeSel.disabled = false;
  areaSel.disabled = false;
  setStatus("");
  answer();
  document.fonts?.ready.then(fitSelects);
}

function answer() {
  const { q, snapshot } = state;
  const wait = run(waitPlan(snapshot, q));
  const [closed, median, p90] = wait.output.rows[0];
  const [filed, withDate] = rows(countsPlan(snapshot, q))[0];

  $("answer").hidden = false;
  if (closed === 0) {
    $("headline").replaceChildren(filed === 0
      ? `No ${q.type} requests were filed in ${where(q.area)} from ${state.period}.`
      : `None of the ${fmtInt(filed)} ${q.type} requests filed in ${where(q.area)} has been closed.`);
  } else {
    $("headline").replaceChildren(
      el("span", { class: "lead" }, `${q.type} requests in ${where(q.area)}, ${state.period}`),
      "Half were closed within ", el("b", {}, fmtDuration(median)), ".",
      ...(closed >= 10 ? [el("span", { class: "tail" }, "9 in 10 were closed within ", el("b", {}, fmtDuration(p90)), ".")] : []),
    );
  }

  const towns = rows(townsPlan(snapshot, q.type));
  const ctx = [];
  if (closed > 0 && q.area.kind === "town") {
    const all = rows(waitPlan(snapshot, { type: q.type, area: ALL }))[0];
    ctx.push(`Across all five towns, half were closed within ${fmtDuration(all[1])}.`);
    const ranked = towns.filter(([, n, m]) => m !== null && n >= MIN_RANKED);
    const i = ranked.findIndex(([t]) => t === q.area.value);
    if (i >= 0 && ranked.length > 1) {
      ctx.push(`That makes ${q.area.value} the ${i === 0 ? "" : ordinal(i + 1) + " "}slowest of the ${ranked.length} towns with at least ${MIN_RANKED} of these requests.`);
    }
  }
  if (closed > 0 && closed < MIN_RANKED) ctx.push(`Only ${fmtInt(closed)} were closed, so one or two requests can move this a lot.`);
  $("context").textContent = ctx.join(" ");

  const cav = [`Counted: ${fmtInt(closed)} closed request${closed === 1 ? "" : "s"}.`];
  if (filed - withDate > 0) cav.push(`${fmtInt(filed - withDate)} more were still open when the data was downloaded and aren't counted.`);
  if (withDate - closed > 0) cav.push(`${fmtInt(withDate - closed)} marked closed before they were opened are left out.`);
  $("caveats").textContent = cav.join(" ");

  $("how-section").hidden = false;
  $("pipeline").replaceChildren(...wait.steps.map((s) =>
    el("li", {}, el("span", {}, s.sentence), el("span", { class: "counts" }, `${fmtInt(s.rows)} rows`))));
  state.engine.dropExecution(wait.execution);

  renderTowns(towns);
  listRequests();
  fitSelects();
  history.replaceState(null, "", (toSearch(q) || location.pathname) + location.hash);
}

function renderTowns(towns) {
  const shown = towns.filter(([t, , m]) => t && m !== null);
  $("compare-section").hidden = shown.length === 0;
  $("compare-caption").textContent = `${state.q.type} requests, slowest first. Pick one to see it.`;
  const max = Math.max(...shown.map((r) => r[2])) || 1;
  const current = state.q.area.kind === "town" ? state.q.area.value : null;
  $("compare").tBodies[0].replaceChildren(...shown.map(([t, n, m, p90]) => {
    const few = n < MIN_RANKED;
    return el("tr", { class: t === current ? "selected" : "" },
      el("td", { class: "group" }, el("button", {
        type: "button", class: "cell",
        onclick: () => { state.q.area = { kind: "town", value: t }; $("area").value = areaKey(state.q.area); answer(); },
      }, t)),
      el("td", { class: "num" }, fmtInt(n)),
      el("td", few ? { class: "num few", title: "Too few requests to rank" } : { class: "num" }, fmtDuration(m)),
      el("td", { class: few ? "num few tailcol" : "num tailcol" }, n >= 10 ? fmtDuration(p90) : ""),
      el("td", { class: "bar", "aria-hidden": "true" }, el("span", { style: `width:${(100 * m / max).toFixed(1)}%` })),
    );
  }));
}

function listRequests() {
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
    const place = [v("town"), v("incident_zip") && `ZIP ${v("incident_zip")}`].filter(Boolean).join(", ");
    return el("li", {},
      el("span", { class: "what" },
        el("a", { class: "key", href: REQUEST(v("unique_key")), title: "This request on SeeClickFix" }, `#${v("unique_key")}`),
        " ", v("complaint_type")),
      el("span", { class: "took" }, fmtDuration(hoursBetween(v("created_date"), v("closed_date")))),
      el("span", { class: "meta" }, `Filed ${fmtWhen(v("created_date"))}, closed ${fmtWhen(v("closed_date"))}. ${place}.`),
    );
  }));
  state.shown += next.length;
  const more = Math.min(PAGE, state.list.length - state.shown);
  $("more").hidden = more <= 0;
  $("more").textContent = `Show ${more} more`;
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

load().catch((e) => {
  console.error(e);
  setStatus(`Something went wrong: ${e.message}`, true);
});
