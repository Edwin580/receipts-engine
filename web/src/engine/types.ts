// Shapes of the engine's JSON results (see docs/engine/wasm-api.md).

export type Cell = string | number | boolean | null | [number, number];

export interface ColumnInfo {
  name: string;
  type: string;
  nullable: boolean;
}

export interface KnownIssue {
  id: string;
  columns: string[];
  count: number;
  sentence: string;
}

export interface LoadReport {
  snapshot_hash: string;
  title: string;
  rows: number;
  columns: ColumnInfo[];
  known_issues: KnownIssue[];
  verify_ms: number;
  timings: Record<string, number>;
}

export interface StepResult {
  sentence: string;
  rows: number;
}

export interface RunResult {
  ok: true;
  execution: number;
  plan_hash: string;
  ms: number;
  steps: StepResult[];
  output: { columns: ColumnInfo[]; total_rows: number; rows: Cell[][] };
  method?: { kind: "unchanged" | "incremental" | "rerun"; groups_recomputed?: number };
  excluded?: number;
}

export interface RunError {
  ok: false;
  error: string;
  step?: number;
}

export interface Trace {
  source: number;
  ms: number;
  path: { step: number; rows: number }[];
  source_rows: number;
}

export interface SourceRecords {
  columns: string[];
  rows: Cell[][];
}

export interface Contributions {
  column: string;
  current: number | null;
  exact: boolean;
  contributing_rows: number;
  rows_that_change_it: number;
  ms: number;
  top: { source_row: number; without: number | null; removes_group: boolean }[];
}

export interface EngineInfo {
  build: "st" | "mt";
  threads: number;
}

export interface LoadProgress {
  phase: "download" | "verify";
  loaded: number;
  total: number;
}
