// A typed, promise-based front to the engine worker.

import type {
  Contributions,
  EngineInfo,
  LoadProgress,
  LoadReport,
  RunError,
  RunResult,
  SourceRecords,
  Trace,
} from "./types";
import type { Plan } from "../plans";

type Pending = {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  progress?: (p: LoadProgress) => void;
};

export class EngineClient {
  private worker: Worker;
  private next = 1;
  private pending = new Map<number, Pending>();

  constructor() {
    this.worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
    this.worker.onmessage = (e: MessageEvent) => {
      const { id, result, error, progress } = e.data;
      const p = this.pending.get(id);
      if (!p) return;
      if (progress) {
        p.progress?.(progress);
        return;
      }
      this.pending.delete(id);
      if (error !== undefined) p.reject(new Error(error));
      else p.resolve(result);
    };
  }

  private call<T>(method: string, args: unknown[], progress?: (p: LoadProgress) => void): Promise<T> {
    const id = this.next++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject, progress });
      this.worker.postMessage({ id, method, args });
    });
  }

  private json<T>(method: string, ...args: unknown[]): Promise<T> {
    return this.call<string>(method, args).then((s) => JSON.parse(s) as T);
  }

  init(base: string): Promise<EngineInfo> {
    return this.call("init", [base]);
  }

  async load(snapshotBase: string, progress: (p: LoadProgress) => void): Promise<LoadReport> {
    return JSON.parse(await this.call<string>("load", [snapshotBase], progress));
  }

  validate(plan: Plan) {
    return this.json<{ ok: boolean; plan_hash?: string; error?: string }>("validate", JSON.stringify(plan));
  }

  run(plan: Plan) {
    return this.json<RunResult | RunError>("run", JSON.stringify(plan));
  }

  traceBack(execution: number, row: number) {
    return this.json<Trace>("traceBack", execution, row);
  }

  traceBackRows(execution: number, row: number) {
    return this.call<Uint32Array>("traceBackRows", [execution, row]);
  }

  sourceRecords(snapshot: string, rows: Uint32Array) {
    return this.json<SourceRecords>("sourceRecords", snapshot, rows);
  }

  exclude(execution: number, rows: Uint32Array) {
    return this.json<RunResult>("exclude", execution, rows);
  }

  contributions(execution: number, row: number, column: string, top: number) {
    return this.json<Contributions>("contributions", execution, row, column, top);
  }

  matchingRows(plan: Plan) {
    return this.call<Uint32Array>("matchingRows", [JSON.stringify(plan)]);
  }

  drop(execution: number) {
    return this.call<boolean>("drop", [execution]);
  }
}
