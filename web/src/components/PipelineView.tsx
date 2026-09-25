import type { StepResult, Trace } from "../engine/types";
import { count } from "../format";

interface Props {
  steps: StepResult[];
  trace: Trace | null;
  label: string | null;
}

/**
 * Every step of the plan in plain English, with how many rows it produced,
 * and, for a selected number, how many of those rows it came from.
 */
export function PipelineView({ steps, trace, label }: Props) {
  const traced = new Map(trace?.path.map((p) => [p.step, p.rows]));
  return (
    <section className="pipeline" aria-label="How this was computed">
      <h3>How this was computed</h3>
      <ol>
        {steps.map((s, i) => (
          <li key={i} data-testid={`step-${i}`}>
            <p>{s.sentence}</p>
            <p className="step-rows">
              {count(s.rows)} {s.rows === 1 ? "row" : "rows"}
              {traced.has(i) && label && (
                <span className="step-traced">
                  {" "}
                  · {count(traced.get(i)!)} behind <strong>{label}</strong>
                </span>
              )}
            </p>
          </li>
        ))}
      </ol>
    </section>
  );
}
