// What both pages share: DOM helpers, loading the engine and a snapshot
// with progress, and sizing the sentence menus.

import init, { Engine } from "./pkg/receipts_wasm.js";
import { periodName } from "./lib.js";

export const $ = (id) => document.getElementById(id);

export const el = (tag, attrs = {}, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v);
  }
  for (const k of kids.flat()) if (k !== null && k !== undefined && k !== false) e.append(k);
  return e;
};

export function setStatus(text, error = false) {
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

/**
 * Loads the engine and the snapshot in `dir`, checking every hash.
 * Returns {engine, report, manifest, period, run, rows}; `run` returns a
 * parsed result whose execution the caller drops, `rows` just the rows.
 */
export async function loadSnapshot(dir, what) {
  await init();
  const mb = (n) => (n / 1e6).toFixed(1);
  setStatus(`Downloading ${what}.`);
  const manifestRes = await fetch(`${dir}/manifest.json`);
  if (!manifestRes.ok) throw new Error(`the data isn't available right now (HTTP ${manifestRes.status})`);
  const [manifest, data, log, rejects] = await Promise.all([
    manifestRes.text(),
    fetchBytes(`${dir}/data.arrow`, (a, b) => setStatus(`Downloading ${what}: ${mb(a)} of ${mb(b)} MB.`)),
    fetchBytes(`${dir}/cleaning_log.arrow`, () => {}),
    fetchBytes(`${dir}/rejects.arrow`, () => {}),
  ]);
  setStatus("Checking the download.");
  await new Promise((r) => setTimeout(r, 0));

  const engine = new Engine();
  const report = JSON.parse(engine.loadSnapshot(manifest, data, log, rejects));
  const m = JSON.parse(manifest);
  const [from, to] = m.scope.sentence.match(/\d{4}-\d{2}-\d{2}/g) ?? [];
  const run = (plan) => {
    const r = JSON.parse(engine.run(JSON.stringify(plan)));
    if (!r.ok) throw new Error(r.error);
    return r;
  };
  const rows = (plan) => {
    const r = run(plan);
    engine.dropExecution(r.execution);
    return r.output.rows;
  };
  return { engine, report, manifest: m, period: from && to ? periodName(from, to) : "", run, rows };
}

// A select is as wide as its longest option; size each to its chosen one
// so the sentence reads without gaps.
export function fitSelects() {
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
