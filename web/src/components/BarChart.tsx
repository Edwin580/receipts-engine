import type { Cell } from "../engine/types";
import { cell } from "../format";

interface Props {
  rows: Cell[][];
  labelIndex: number;
  valueIndex: number;
  valueColumn: string;
  /** Column with each bar's record count, shown next to its label (-1: none). */
  countIndex: number;
  selected: number | null;
  onSelect: (row: number) => void;
  /** Values before a what-if, by label, drawn as a faint outline. */
  before?: Map<string, number>;
}

/** Horizontal bars; each bar is a button that opens its receipt. */
export function BarChart({ rows, labelIndex, valueIndex, valueColumn, countIndex, selected, onSelect, before }: Props) {
  const values = rows.map((r) => (typeof r[valueIndex] === "number" ? (r[valueIndex] as number) : 0));
  const all = [...values, ...(before ? [...before.values()] : [])];
  const width = barScale(all);
  const lo = Math.min(...all.filter((v) => v > 0));
  const hi = Math.max(...all);
  return (
    <>
    <ol className="bars" aria-label="Answer">
      {rows.map((r, i) => {
        const label = String(r[labelIndex] ?? "(missing)");
        const prior = before?.get(label);
        return (
          <li key={label}>
            <button
              className={`bar-row${selected === i ? " selected" : ""}`}
              onClick={() => onSelect(i)}
              aria-pressed={selected === i}
              data-testid={`bar-${i}`}
            >
              <span className="bar-label">
                {label}
                {countIndex >= 0 && typeof r[countIndex] === "number" && (
                  <span className="bar-count"> · {cell(r[countIndex], "requests")}</span>
                )}
              </span>
              <span className="bar-track">
                {prior !== undefined && prior !== values[i] && (
                  <span className="bar-before" style={{ width: `${width(prior)}%` }} />
                )}
                <span className="bar-fill" style={{ width: `${width(values[i])}%` }} />
              </span>
              <span className="bar-value">{cell(r[valueIndex], valueColumn)}</span>
            </button>
          </li>
        );
      })}
    </ol>
    {width.log && (
      <p className="muted small" data-testid="log-scale">
        Bar lengths use a logarithmic scale: values range from {cell(lo, valueColumn)} to {cell(hi, valueColumn)}.
      </p>
    )}
    </>
  );
}

/**
 * Bar length in percent. Linear, unless values span more than 50×: then a
 * log scale keeps the small bars visible (and the chart says so).
 */
export function barScale(values: number[]): ((v: number) => number) & { log: boolean } {
  const positive = values.filter((v) => v > 0);
  const max = Math.max(1e-9, ...values);
  const min = positive.length ? Math.min(...positive) : max;
  const log = max / min > 50;
  const f = (v: number) => {
    if (v <= 0) return 0;
    if (!log) return (v / max) * 100;
    // Smallest value gets a visible 6%, the largest 100%.
    return 6 + (94 * (Math.log(v) - Math.log(min))) / (Math.log(max) - Math.log(min));
  };
  return Object.assign(f, { log });
}
