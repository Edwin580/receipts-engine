import { useEffect, useState } from "react";
import type { EngineClient } from "../engine/client";
import type { Contributions, KnownIssue, SourceRecords, Trace } from "../engine/types";
import { cell, count, short } from "../format";
import { KNOWN_ISSUE_FILTERS, hours, type Plan, type Question } from "../plans";

const SHOWN_COLUMNS = ["unique_key", "created_date", "closed_date", "agency", "complaint_type", "descriptor", "borough"];
const PAGE = 25;

interface Props {
  engine: EngineClient;
  snapshot: string;
  question: Question;
  plan: Plan;
  planHash: string;
  execution: number;
  row: number;
  label: string;
  value: number | null;
  trace: Trace | null;
  knownIssues: KnownIssue[];
  excludedIssues: Set<string>;
  excludedRows: Set<number>;
  onToggleIssue: (id: string) => void;
  onToggleRow: (row: number) => void;
  onClearExclusions: () => void;
  baseValue: number | null;
}

const show = (v: number | null, q: Question) =>
  v === null ? "no value" : q.unit === "hours" ? hours(v) : count(v);

/** Everything behind one number: its records, its recipe, and what-ifs. */
export function ReceiptPanel(p: Props) {
  const [records, setRecords] = useState<SourceRecords | null>(null);
  const [ids, setIds] = useState<Uint32Array | null>(null);
  const [page, setPage] = useState(0);
  const [contrib, setContrib] = useState<Contributions | null>(null);
  // unique_key of each "biggest effect" row, for people to look up.
  const [moverKeys, setMoverKeys] = useState<Map<number, string>>(new Map());
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let live = true;
    setRecords(null);
    setContrib(null);
    setPage(0);
    p.engine.traceBackRows(p.execution, p.row).then((rows) => live && setIds(rows));
    p.engine
      .contributions(p.execution, p.row, p.question.measure, 5)
      .then(async (c) => {
        if (!live) return;
        setContrib(c);
        const rows = Uint32Array.from(c.top.map((t) => t.source_row));
        const recs = rows.length ? await p.engine.sourceRecords(p.snapshot, rows) : null;
        const k = recs?.columns.indexOf("unique_key") ?? -1;
        if (live && recs && k >= 0) {
          setMoverKeys(new Map(c.top.map((t, i) => [t.source_row, String(recs.rows[i][k])])));
        }
      })
      .catch(() => live && setContrib(null));
    return () => {
      live = false;
    };
  }, [p.engine, p.execution, p.row, p.question.measure]);

  useEffect(() => {
    if (!ids) return;
    let live = true;
    p.engine
      .sourceRecords(p.snapshot, ids.slice(page * PAGE, (page + 1) * PAGE))
      .then((r) => live && setRecords(r));
    return () => {
      live = false;
    };
  }, [ids, page, p.engine, p.snapshot]);

  const issues = p.knownIssues.filter((k) => KNOWN_ISSUE_FILTERS[k.id] && k.count > 0);
  const changed = p.baseValue !== p.value;
  const receipt = {
    question: p.question.title,
    answer: { [p.question.label]: p.label, [p.question.measure]: p.value },
    snapshot_hash: p.snapshot,
    plan_hash: p.planHash,
    source_rows: p.trace?.source_rows,
    excluded: { known_issues: [...p.excludedIssues], rows: [...p.excludedRows] },
    plan: p.plan,
  };

  return (
    <aside className="receipt" aria-label="Receipt" data-testid="receipt">
      <header>
        <p className="eyebrow">Receipt</p>
        <h2>
          {p.label}: <span data-testid="receipt-value">{show(p.value, p.question)}</span>
        </h2>
        {changed && (
          <p className="delta" data-testid="receipt-delta">
            Was {show(p.baseValue, p.question)} before your exclusions.
          </p>
        )}
        <p>
          Built from <strong data-testid="receipt-rows">{count(p.trace?.source_rows ?? 0)}</strong> records in
          the snapshot.
        </p>
      </header>

      {contrib && (
        <section>
          <h3>Can one record change it?</h3>
          {contrib.rows_that_change_it === 0 ? (
            <p>
              No. Removing any single one of these {count(contrib.contributing_rows)} records leaves the value
              unchanged.
            </p>
          ) : (
            <>
              <p>
                {count(contrib.rows_that_change_it)} of {count(contrib.contributing_rows)} records move it when
                removed{contrib.exact ? "" : " (approximately; excluding them re-computes it exactly)"}. The
                biggest effects:
              </p>
              <ul className="movers">
                {contrib.top.map((t) => (
                  <li key={t.source_row}>
                    Without request {moverKeys.get(t.source_row) ?? `row ${count(t.source_row)}`}:{" "}
                    {t.removes_group ? "this group disappears" : show(t.without, p.question)}
                    <button className="link" onClick={() => p.onToggleRow(t.source_row)}>
                      {p.excludedRows.has(t.source_row) ? "include" : "exclude"}
                    </button>
                  </li>
                ))}
              </ul>
            </>
          )}
        </section>
      )}

      <section>
        <h3>What if you leave out…</h3>
        {issues.length === 0 && <p>This snapshot has no known issues to exclude.</p>}
        {issues.map((k) => (
          <label key={k.id} className="check">
            <input
              type="checkbox"
              checked={p.excludedIssues.has(k.id)}
              onChange={() => p.onToggleIssue(k.id)}
              data-testid={`issue-${k.id}`}
            />
            {count(k.count)} records {KNOWN_ISSUE_FILTERS[k.id].label}
          </label>
        ))}
        {(p.excludedIssues.size > 0 || p.excludedRows.size > 0) && (
          <button className="link" onClick={p.onClearExclusions}>
            Include everything again
          </button>
        )}
      </section>

      <section>
        <h3>The records</h3>
        {!records && <p className="muted">Loading records…</p>}
        {records && (
          <div className="table-wrap">
            <table className="records">
              <thead>
                <tr>
                  <th scope="col">
                    <span className="sr-only">Exclude</span>
                  </th>
                  {SHOWN_COLUMNS.map((c) => (
                    <th key={c} scope="col">
                      {c.replace(/_/g, " ")}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {records.rows.map((r, i) => {
                  const source = ids![page * PAGE + i];
                  return (
                    <tr key={source}>
                      <td>
                        <input
                          type="checkbox"
                          aria-label={`Exclude record ${source}`}
                          checked={p.excludedRows.has(source)}
                          onChange={() => p.onToggleRow(source)}
                        />
                      </td>
                      {SHOWN_COLUMNS.map((c) => (
                        <td key={c}>{cell(r[records.columns.indexOf(c)], c)}</td>
                      ))}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        {ids && ids.length > PAGE && (
          <div className="pager">
            <button disabled={page === 0} onClick={() => setPage(page - 1)}>
              Previous
            </button>
            <span>
              {count(page * PAGE + 1)}–{count(Math.min(ids.length, (page + 1) * PAGE))} of {count(ids.length)}
            </span>
            <button disabled={(page + 1) * PAGE >= ids.length} onClick={() => setPage(page + 1)}>
              Next
            </button>
          </div>
        )}
      </section>

      <footer>
        <dl>
          <dt>Snapshot</dt>
          <dd title={p.snapshot}>{short(p.snapshot)}</dd>
          <dt>Plan</dt>
          <dd title={p.planHash}>{short(p.planHash)}</dd>
        </dl>
        <button
          onClick={() =>
            navigator.clipboard?.writeText(JSON.stringify(receipt, null, 2)).then(() => {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            })
          }
        >
          {copied ? "Copied" : "Copy receipt"}
        </button>
      </footer>
    </aside>
  );
}
