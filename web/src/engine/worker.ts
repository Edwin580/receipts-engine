/// <reference lib="webworker" />
// The engine lives in this worker so loading 7M rows never blocks the page.
// It picks the threaded WASM build when the page is cross-origin isolated
// and the single-threaded one otherwise; both give identical results.

import type { EngineInfo, LoadProgress } from "./types";

declare const self: DedicatedWorkerGlobalScope;

interface WasmModule {
  default: () => Promise<unknown>;
  initThreadPool?: (threads: number) => Promise<void>;
  Engine: new () => WasmEngine;
}

interface WasmEngine {
  loadSnapshot(manifest: string, data: Uint8Array, log: Uint8Array, rejects: Uint8Array): string;
  validatePlan(plan: string): string;
  run(plan: string): string;
  rows(execution: number, from: number, count: number): string;
  traceBack(execution: number, row: number): string;
  traceBackRows(execution: number, row: number): Uint32Array;
  sourceRecords(snapshot: string, rows: Uint32Array): string;
  exclude(execution: number, rows: Uint32Array): string;
  contributions(execution: number, row: number, column: string, top: number): string;
  matchingRows(plan: string): Uint32Array;
  dropExecution(execution: number): boolean;
}

let engine: WasmEngine | undefined;

function need(): WasmEngine {
  if (!engine) throw new Error("the engine hasn't started yet");
  return engine;
}

async function init(base: string): Promise<EngineInfo> {
  const build = self.crossOriginIsolated ? "mt" : "st";
  const mod = (await import(/* @vite-ignore */ `${base}pkg/${build}/receipts_wasm.js`)) as WasmModule;
  await mod.default();
  let threads = 1;
  if (build === "mt" && mod.initThreadPool) {
    threads = Math.max(1, Math.min(navigator.hardwareConcurrency || 1, 8));
    await mod.initThreadPool(threads);
  }
  engine = new mod.Engine();
  return { build, threads };
}

/** Fetches a file, reporting bytes as they arrive. */
async function download(url: string, onBytes: (n: number) => void): Promise<Uint8Array> {
  const res = await fetch(url);
  if (!res.ok || !res.body) throw new Error(`couldn't download ${url} (HTTP ${res.status})`);
  const length = Number(res.headers.get("Content-Length")) || 0;
  const reader = res.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    size += value.length;
    onBytes(value.length);
  }
  if (length && size !== length) throw new Error(`${url} was cut off (${size} of ${length} bytes)`);
  const out = new Uint8Array(size);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

async function load(snapshotBase: string, progress: (p: LoadProgress) => void): Promise<string> {
  const manifestText = await (await fetch(snapshotBase + "manifest.json")).text();
  const files: { path: string; bytes: number }[] = JSON.parse(manifestText).files ?? [];
  const total = files.reduce((s, f) => s + f.bytes, 0);
  let loaded = 0;
  const tick = (n: number) => {
    loaded += n;
    progress({ phase: "download", loaded, total });
  };
  const [data, log, rejects] = await Promise.all(
    ["data.arrow", "cleaning_log.arrow", "rejects.arrow"].map((f) => download(snapshotBase + f, tick)),
  );
  progress({ phase: "verify", loaded: total, total });
  return need().loadSnapshot(manifestText, data, log, rejects);
}

type Handler = (...args: any[]) => unknown;

const handlers: Record<string, Handler> = {
  init: (base: string) => init(base),
  load: (snapshotBase: string, id: number) =>
    load(snapshotBase, (p) => self.postMessage({ id, progress: p })),
  validate: (plan: string) => need().validatePlan(plan),
  run: (plan: string) => need().run(plan),
  rows: (execution: number, from: number, count: number) => need().rows(execution, from, count),
  traceBack: (execution: number, row: number) => need().traceBack(execution, row),
  traceBackRows: (execution: number, row: number) => need().traceBackRows(execution, row),
  sourceRecords: (snapshot: string, rows: Uint32Array) => need().sourceRecords(snapshot, rows),
  exclude: (execution: number, rows: Uint32Array) => need().exclude(execution, rows),
  contributions: (execution: number, row: number, column: string, top: number) =>
    need().contributions(execution, row, column, top),
  matchingRows: (plan: string) => need().matchingRows(plan),
  drop: (execution: number) => need().dropExecution(execution),
};

self.onmessage = async (e: MessageEvent<{ id: number; method: string; args: unknown[] }>) => {
  const { id, method, args } = e.data;
  try {
    const handler = handlers[method];
    if (!handler) throw new Error(`unknown engine call ${method}`);
    // `load` reports progress under the same id.
    const result = await handler(...(method === "load" ? [...args, id] : args));
    const transfer = result instanceof Uint32Array ? [result.buffer] : [];
    self.postMessage({ id, result }, transfer);
  } catch (err) {
    self.postMessage({ id, error: err instanceof Error ? err.message : String(err) });
  }
};
