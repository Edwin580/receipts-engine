import type { Cell } from "./engine/types";
import { hours } from "./plans";

export const count = (n: number) => n.toLocaleString("en-US");

export function bytes(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(2)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(0)} MB`;
  return `${(n / 1e3).toFixed(0)} kB`;
}

/** A value in a result table, formatted for its column. */
export function cell(value: Cell, column: string): string {
  if (value === null) return "—";
  if (typeof value === "number") {
    if (column.endsWith("_hours")) return hours(value);
    return Number.isInteger(value) ? count(value) : value.toFixed(2);
  }
  if (Array.isArray(value)) return `${value[0].toFixed(5)}, ${value[1].toFixed(5)}`;
  if (typeof value === "string" && /^\d{4}-\d\d-\d\dT/.test(value)) {
    return value.replace("T", " ").replace(/\.0+$/, "").replace(/\.(\d+?)0+$/, ".$1");
  }
  return String(value);
}

export const short = (hash: string) => hash.slice(0, 12);
