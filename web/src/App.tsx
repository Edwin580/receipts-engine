import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { EngineClient } from "./engine/client";
import type { EngineInfo, LoadProgress, LoadReport, RunResult, Trace } from "./engine/types";
import { BarChart } from "./components/BarChart";
import { PipelineView } from "./components/PipelineView";
import { ReceiptPanel } from "./components/ReceiptPanel";
import { bytes, count, short } from "./format";
import { QUESTIONS, complaintTypesPlan, knownIssuePlan, leadingRow, type Plan, type Year } from "./plans";

const SNAPSHOT_BASE: string = import.meta.env.VITE_SNAPSHOT_BASE ?? "/snap/";

type Status =
  | { kind: "starting" }
  | { kind: "loading"; progress: LoadProgress | null }
  | { kind: "ready"; report: LoadReport }
  | { kind: "error"; message: string };

export function App() {
  const engine = useMemo(() => new EngineClient(), []);
  const [info, setInfo] = useState<EngineInfo | null>(null);
  const [status, setStatus] = useState<Status>({ kind: "starting" });

  useEffect(() => {
    let live = true;
    (async () => {
      try {
        const i = await engine.init(import.meta.env.BASE_URL);
        if (!live) return;
        setInfo(i);
        setStatus({ kind: "loading", progress: null });
        const report = await engine.load(SNAPSHOT_BASE, (progress) => live && setStatus({ kind: "loading", progress }));
        if (live) setStatus({ kind: "ready", report });
      } catch (e) {
        if (live) setStatus({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      }
    })();
    return () => {
      live = false;
    };
  }, [engine]);

  return (
    <div className="app">
      <header className="top">
        <h1>Receipts</h1>
        <p className="tagline">Click a number from NYC open data. See the records behind it.</p>
        {status.kind === "ready" && <SnapshotBadge report={status.report} info={info} />}
      </header>
      {status.kind === "starting" && <p className="center muted">Starting the engine…</p>}
      {status.kind === "loading" && <Loading progress={status.progress} />}
      {status.kind === "error" && (
        <div className="center error" role="alert">
          <h2>The data couldn't be loaded</h2>
          <p>{status.message}</p>
        </div>
      )}
      {status.kind === "ready" && <Explorer engine={engine} report={status.report} />}
    </div>
  );
}

function SnapshotBadge({ report, info }: { report: LoadReport; info: EngineInfo | null }) {
  return (
    <p className="badge" data-testid="verified" title={report.snapshot_hash}>
      <span className="ok">✓ Verified</span> {report.title} · {count(report.rows)} records · snapshot{" "}
      <code>{short(report.snapshot_hash)}</code>
      {info && (
        <span className="muted">
          {" "}
          · {info.build === "mt" ? `${info.threads} threads` : "1 thread"}
        </span>
      )}
    </p>
  );
}

function Loading({ progress }: { progress: LoadProgress | null }) {
  const pct = progress && progress.total ? Math.round((progress.loaded / progress.total) * 100) : 0;
  return (
    <div className="center loading" role="status" aria-live="polite">
      {(!progress || progress.phase === "download") && (
        <>
          <p>
            Downloading the snapshot{progress ? `: ${bytes(progress.loaded)} of ${bytes(progress.total)}` : "…"}
          </p>
          <progress max={100} value={pct} />
        </>
      )}
      {progress?.phase === "verify" && (
        <p>Checking every fingerprint in the data, so each receipt can be trusted…</p>
      )}
    </div>
  );
}

function Explorer({ engine, report }: { engine: EngineClient; report: LoadReport }) {
  const snap = report.snapshot_hash;
  const [questionId, setQuestionId] = useState(QUESTIONS[0].id);
  const [year, setYear] = useState<Year>("2025");
  const [types, setTypes] = useState<string[]>([]);
  const [chosenTypes, setChosenTypes] = useState<string[]>(["Noise - Residential", "Noise - Street/Sidewalk"]);
  const [base, setBase] = useState<{ run: RunResult; plan: Plan } | null>(null);
  const [whatIf, setWhatIf] = useState<RunResult | null>(null);
  // The selected bar, by label: a what-if can reorder the rows.
  const [selectedKey, setSelectedKey] = useState<string | null>(null);
  const [trace, setTrace] = useState<Trace | null>(null);
  const [excludedIssues, setExcludedIssues] = useState<Set<string>>(new Set());
  const [excludedRows, setExcludedRows] = useState<Set<number>>(new Set());
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const issueRows = useRef(new Map<string, Uint32Array>());
  const question = QUESTIONS.find((q) => q.id === questionId)!;

  // Complaint types for the picker, most common first.
  useEffect(() => {
    engine.run(complaintTypesPlan(snap)).then((r) => {
      if (!r.ok) return;
      setTypes(r.output.rows.map((row) => String(row[0])).filter((t) => t !== "null"));
      engine.drop(r.execution);
    });
  }, [engine, snap]);

  // Run the question whenever it or its parameters change.
  useEffect(() => {
    let live = true;
    const plan = question.plan(snap, { year, complaintTypes: chosenTypes });
    setBusy(true);
    setError(null);
    engine.run(plan).then((r) => {
      if (!live) {
        if (r.ok) engine.drop(r.execution);
        return;
      }
      setBusy(false);
      if (!r.ok) {
        setError(r.error);
        return;
      }
      setBase((old) => {
        if (old) engine.drop(old.run.execution);
        return { run: r, plan };
      });
      const li = r.output.columns.findIndex((c) => c.name === question.label);
      const ri = r.output.columns.findIndex((c) => c.name === "requests");
      const lead = leadingRow(r.output.rows.map((row) => (ri >= 0 ? (row[ri] as number) : null)));
      setSelectedKey(r.output.total_rows ? String(r.output.rows[lead][li]) : null);
      setExcludedIssues(new Set());
      setExcludedRows(new Set());
    });
    return () => {
      live = false;
    };
  }, [engine, snap, question, year, chosenTypes]);

  // Re-derive the answer without the excluded records (exact, incremental
  // where possible).
  useEffect(() => {
    if (!base) return;
    let live = true;
    (async () => {
      if (excludedIssues.size === 0 && excludedRows.size === 0) {
        setWhatIf((old) => {
          if (old) engine.drop(old.execution);
          return null;
        });
        return;
      }
      const rows = new Set<number>(excludedRows);
      for (const id of excludedIssues) {
        let r = issueRows.current.get(id);
        if (!r) {
          const plan = knownIssuePlan(snap, id);
          if (!plan) continue;
          r = await engine.matchingRows(plan);
          issueRows.current.set(id, r);
        }
        r.forEach((x) => rows.add(x));
      }
      const cf = await engine.exclude(base.run.execution, Uint32Array.from(rows));
      if (!live) {
        engine.drop(cf.execution);
        return;
      }
      setWhatIf((old) => {
        if (old) engine.drop(old.execution);
        return cf;
      });
    })().catch((e) => live && setError(String(e)));
    return () => {
      live = false;
    };
  }, [engine, snap, base, excludedIssues, excludedRows]);

  const shown = whatIf ?? base?.run ?? null;
  const labelIndex = shown?.output.columns.findIndex((c) => c.name === question.label) ?? -1;
  const valueIndex = shown?.output.columns.findIndex((c) => c.name === question.measure) ?? -1;
  const found = shown && selectedKey !== null ? shown.output.rows.findIndex((r) => String(r[labelIndex]) === selectedKey) : -1;
  const selected = found >= 0 ? found : null;
  const selectedLabel = selected !== null ? selectedKey : null;
  const select = useCallback(
    (row: number) => shown && setSelectedKey(String(shown.output.rows[row][labelIndex])),
    [shown, labelIndex],
  );

  useEffect(() => {
    if (!shown || selected === null || selected >= shown.output.total_rows) {
      setTrace(null);
      return;
    }
    let live = true;
    engine.traceBack(shown.execution, selected).then((t) => live && setTrace(t));
    return () => {
      live = false;
    };
  }, [engine, shown, selected]);

  const before = useMemo(() => {
    if (!whatIf || !base) return undefined;
    return new Map(
      base.run.output.rows.map((r) => [String(r[labelIndex]), typeof r[valueIndex] === "number" ? (r[valueIndex] as number) : 0]),
    );
  }, [whatIf, base, labelIndex, valueIndex]);

  const baseValue = useMemo(() => {
    if (!base || !selectedLabel) return null;
    const r = base.run.output.rows.find((row) => String(row[labelIndex]) === selectedLabel);
    return r && typeof r[valueIndex] === "number" ? (r[valueIndex] as number) : null;
  }, [base, selectedLabel, labelIndex, valueIndex]);

  const requestsIndex = shown?.output.columns.findIndex((c) => c.name === "requests") ?? -1;
  const lead = shown
    ? leadingRow(shown.output.rows.map((r) => (requestsIndex >= 0 ? (r[requestsIndex] as number) : null)))
    : 0;
  const top = shown?.output.rows[lead];

  return (
    <main className="explorer">
      <nav className="questions" aria-label="Questions">
        {QUESTIONS.map((q) => (
          <label key={q.id} className={`question${q.id === questionId ? " active" : ""}`}>
            <input
              type="radio"
              name="question"
              checked={q.id === questionId}
              onChange={() => {
                setQuestionId(q.id);
                setSelectedKey(null);
              }}
            />
            {q.title}
          </label>
        ))}
        <fieldset className="params">
          <legend>Period</legend>
          {(["2024", "2025", "both"] as Year[]).map((y) => (
            <label key={y} className="pill">
              <input type="radio" name="year" checked={year === y} onChange={() => setYear(y)} />
              {y === "both" ? "2024–2025" : y}
            </label>
          ))}
        </fieldset>
        {question.usesComplaintTypes && (
          <fieldset className="params">
            <legend>Complaint types</legend>
            <select
              multiple
              size={8}
              value={chosenTypes}
              onChange={(e) => {
                const v = [...e.target.selectedOptions].map((o) => o.value);
                if (v.length) setChosenTypes(v);
              }}
              aria-label="Complaint types"
              aria-describedby="types-help"
            >
              {types.map((t) => (
                <option key={t}>{t}</option>
              ))}
            </select>
            <p id="types-help" className="muted small">
              Ctrl- or ⌘-click to choose several.
            </p>
          </fieldset>
        )}
      </nav>

      <section className="answer" aria-busy={busy}>
        <h2>{question.title}</h2>
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
        {shown && top && labelIndex >= 0 && valueIndex >= 0 && (
          <>
            <p className="headline" data-testid="headline">
              {question.headline(String(top[labelIndex]), Number(top[valueIndex]), Number(top[requestsIndex] ?? 0), {
                year,
                complaintTypes: chosenTypes,
              })}
            </p>
            {whatIf?.method && (
              <p className="muted" data-testid="whatif-note">
                Without {count(whatIf.excluded ?? 0)} records · recomputed in {Math.round(whatIf.ms)} ms (
                {whatIf.method.kind}).
              </p>
            )}
            <BarChart
              rows={shown.output.rows}
              labelIndex={labelIndex}
              valueIndex={valueIndex}
              valueColumn={question.measure}
              countIndex={question.measure === "requests" ? -1 : requestsIndex}
              selected={selected}
              before={before}
              onSelect={select}
            />
            <p className="muted small">
              Click a bar to see its records. Answered in {Math.round(shown.ms)} ms.
            </p>
            <PipelineView steps={shown.steps} trace={trace} label={selectedLabel} />
          </>
        )}
        {shown && shown.output.total_rows === 0 && <p>No records match.</p>}
      </section>

      {shown && base && selected !== null && selectedLabel && valueIndex >= 0 && (
        <ReceiptPanel
          engine={engine}
          snapshot={snap}
          question={question}
          plan={base.plan}
          planHash={base.run.plan_hash}
          execution={shown.execution}
          row={selected}
          label={selectedLabel}
          value={typeof shown.output.rows[selected][valueIndex] === "number" ? (shown.output.rows[selected][valueIndex] as number) : null}
          baseValue={baseValue}
          trace={trace}
          knownIssues={report.known_issues ?? []}
          excludedIssues={excludedIssues}
          excludedRows={excludedRows}
          onToggleIssue={(id) =>
            setExcludedIssues((s) => {
              const n = new Set(s);
              if (n.has(id)) n.delete(id);
              else n.add(id);
              return n;
            })
          }
          onToggleRow={(r) =>
            setExcludedRows((s) => {
              const n = new Set(s);
              if (n.has(r)) n.delete(r);
              else n.add(r);
              return n;
            })
          }
          onClearExclusions={() => {
            setExcludedIssues(new Set());
            setExcludedRows(new Set());
          }}
        />
      )}
    </main>
  );
}
