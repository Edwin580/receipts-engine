//! The engine behind the JS API: loaded snapshots and live executions,
//! with every call returning JSON (see `docs/engine/wasm-api.md`). Plain
//! Rust, so it runs and is tested natively too.

use crate::clock::now_ms;
use crate::verify::{VerifiedSnapshot, verify};
use receipts_cf::{CfValue, Method, contributions, exclude};
use receipts_core::time::format_naive_timestamp;
use receipts_core::{Column, ColumnData, ContentHash, SourceId};
use receipts_exec::{Execution, SourceProvider, Table, execute};
use receipts_plan::{Catalog, Field, NodeId, Schema, SourceInfo, ValidPlan, describe, validate};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// Output rows returned inline by `run`; the rest are fetched by `rows`.
pub const INLINE_ROWS: usize = 500;

#[derive(Debug)]
struct Snapshot {
    source_id: SourceId,
    title: String,
    table: Arc<Table>,
}

#[derive(Debug)]
struct Live {
    plan: ValidPlan,
    execution: Execution,
    /// For a counterfactual: the execution it was derived from, and every
    /// snapshot row left out.
    base: Option<u32>,
    excluded: Vec<u32>,
}

#[derive(Default, Debug)]
pub struct Engine {
    snapshots: HashMap<ContentHash, Snapshot>,
    executions: HashMap<u32, Live>,
    next_id: u32,
}

impl Catalog for Engine {
    fn source(&self, s: &ContentHash) -> Option<SourceInfo> {
        self.snapshots.get(s).map(|x| SourceInfo {
            title: x.title.clone(),
            schema: x.table.schema().clone(),
        })
    }
}

impl SourceProvider for Engine {
    fn table(&self, s: &ContentHash) -> Option<Arc<Table>> {
        self.snapshots.get(s).map(|x| x.table.clone())
    }
    fn source_id(&self, s: &ContentHash) -> SourceId {
        self.snapshots.get(s).map_or(SourceId(0), |x| x.source_id)
    }
}

/// A JSON value for one cell. Integers beyond 2^53 become strings so no
/// digit is lost in JavaScript; timestamps are ISO-like strings.
fn cell(c: &Column, i: usize) -> Value {
    if !c.is_valid(i) {
        return Value::Null;
    }
    match &c.data {
        ColumnData::I64(v) => {
            let x = v[i];
            if x.unsigned_abs() <= 1 << 53 {
                json!(x)
            } else {
                json!(x.to_string())
            }
        }
        ColumnData::F64(v) => json!(v[i]),
        ColumnData::Bool(b) => json!(b.get(i)),
        ColumnData::Timestamp(v) => json!(format_naive_timestamp(v[i])),
        ColumnData::DictUtf8 { codes, dictionary } => json!(dictionary[codes[i] as usize]),
        ColumnData::Geo { lat, lon } => json!([lat[i], lon[i]]),
    }
}

fn schema_json(s: &Schema) -> Value {
    s.fields
        .iter()
        .map(|f: &Field| json!({ "name": f.name, "type": format!("{:?}", f.ty), "nullable": f.nullable }))
        .collect()
}

fn rows_json(t: &Table, rows: impl Iterator<Item = usize>) -> Value {
    rows.map(|i| Value::Array(t.columns().iter().map(|c| cell(c, i)).collect()))
        .collect()
}

fn cf_value(v: CfValue) -> Value {
    match v {
        CfValue::Null => Value::Null,
        CfValue::I64(i) => json!(i),
        CfValue::F64(x) => json!(x),
    }
}

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    /// Verifies and loads a snapshot. Returns a report with timings.
    pub fn load_snapshot(
        &mut self,
        manifest: &str,
        data: &[u8],
        cleaning_log: &[u8],
        rejects: &[u8],
    ) -> Result<Value, String> {
        let t0 = now_ms();
        let VerifiedSnapshot {
            snapshot,
            source_id,
            title,
            manifest,
            columns,
            timings,
        } = verify(manifest, data, cleaning_log, rejects)?;
        let t1 = now_ms();
        let fields = columns
            .iter()
            .map(|(c, nullable)| Field::new(c.name.clone(), c.data.column_type(), *nullable))
            .collect();
        let table = Table::new(
            Schema::new(fields),
            columns.into_iter().map(|(c, _)| Arc::new(c)).collect(),
        )?;
        let report = json!({
            "snapshot_hash": snapshot.to_hex(),
            "title": title,
            "rows": table.len(),
            "columns": schema_json(table.schema()),
            "known_issues": manifest.get("known_issues").cloned().unwrap_or(Value::Null),
            "verify_ms": t1 - t0,
            "timings": timings,
        });
        self.snapshots.insert(
            snapshot,
            Snapshot {
                source_id,
                title,
                table: Arc::new(table),
            },
        );
        Ok(report)
    }

    fn parse_valid(&self, plan_json: &str) -> Result<ValidPlan, Value> {
        let v: Value = serde_json::from_str(plan_json)
            .map_err(|e| json!({ "ok": false, "error": format!("plan is not JSON: {e}") }))?;
        let plan = receipts_plan::json::from_json(&v)
            .map_err(|e| json!({ "ok": false, "error": e.message, "step": e.node.map(|n| n.0) }))?;
        validate(plan, self)
            .map_err(|e| json!({ "ok": false, "error": e.message, "step": e.node.map(|n| n.0) }))
    }

    /// Validation result, plan hash, and one sentence and schema per step.
    pub fn validate_plan(&self, plan_json: &str) -> Value {
        match self.parse_valid(plan_json) {
            Err(e) => e,
            Ok(p) => json!({
                "ok": true,
                "plan_hash": p.plan_hash().to_hex(),
                "steps": describe(&p).into_iter().enumerate().map(|(i, s)| json!({
                    "op": p.nodes()[i].name(),
                    "sentence": s,
                    "node_hash": p.node_hash(NodeId(i as u32)).to_hex(),
                    "columns": schema_json(p.schema(NodeId(i as u32))),
                })).collect::<Vec<_>>(),
            }),
        }
    }

    fn register(&mut self, live: Live, ms: f64, extra: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let e = &live.execution;
        let out = e.output();
        let mut v = json!({
            "ok": true,
            "execution": id,
            "plan_hash": live.plan.plan_hash().to_hex(),
            "ms": ms,
            "steps": describe(&live.plan).into_iter().enumerate().map(|(i, s)| json!({
                "sentence": s,
                "rows": e.tables[i].len(),
            })).collect::<Vec<_>>(),
            "output": {
                "columns": schema_json(out.schema()),
                "total_rows": out.len(),
                "rows": rows_json(out, 0..out.len().min(INLINE_ROWS)),
            },
        });
        if let (Value::Object(a), Value::Object(b)) = (&mut v, extra) {
            a.extend(b);
        }
        self.executions.insert(id, live);
        v
    }

    pub fn run(&mut self, plan_json: &str) -> Value {
        let plan = match self.parse_valid(plan_json) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let t0 = now_ms();
        match execute(&plan, self) {
            Err(e) => json!({ "ok": false, "error": e.message, "step": e.node.0 }),
            Ok(execution) => {
                let ms = now_ms() - t0;
                self.register(
                    Live {
                        plan,
                        execution,
                        base: None,
                        excluded: Vec::new(),
                    },
                    ms,
                    json!({}),
                )
            }
        }
    }

    fn live(&self, id: u32) -> Result<&Live, String> {
        self.executions
            .get(&id)
            .ok_or_else(|| format!("no execution {id}"))
    }

    /// Output rows `[from, from + count)`.
    pub fn rows(&self, id: u32, from: usize, count: usize) -> Result<Value, String> {
        let out = self.live(id)?.execution.output();
        let end = from.saturating_add(count).min(out.len());
        Ok(rows_json(out, from.min(end)..end))
    }

    /// Row counts at every step for one output row, and its source rows.
    pub fn trace_back(&self, id: u32, row: u32) -> Result<(Value, Vec<u32>), String> {
        let live = self.live(id)?;
        let e = &live.execution;
        if row as usize >= e.output().len() {
            return Err(format!("row {row} is past the output"));
        }
        let t0 = now_ms();
        let trace = e.lineage.backward_row(e.output.index(), row);
        let summary = json!({
            "source": trace.source.0,
            "ms": now_ms() - t0,
            "path": trace.path.iter().map(|s| json!({ "step": s.node, "rows": s.rows.len() })).collect::<Vec<_>>(),
            "source_rows": trace.source_rows().len(),
        });
        Ok((summary, trace.source_rows().to_vec()))
    }

    /// Source records (all columns) for snapshot row indices.
    pub fn source_records(&self, snapshot_hex: &str, rows: &[u32]) -> Result<Value, String> {
        let hash = ContentHash::from_hex(snapshot_hex).ok_or("bad snapshot hash")?;
        let t = &self
            .snapshots
            .get(&hash)
            .ok_or("snapshot not loaded")?
            .table;
        if let Some(r) = rows.iter().find(|&&r| r as usize >= t.len()) {
            return Err(format!("row {r} is past the snapshot"));
        }
        Ok(json!({
            "columns": t.schema().fields.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
            "rows": rows_json(t, rows.iter().map(|&r| r as usize)),
        }))
    }

    /// The execution re-derived without the given snapshot rows.
    pub fn exclude(&mut self, id: u32, rows: &[u32]) -> Result<Value, String> {
        let live = self.live(id)?;
        // Exclusions compose: a counterfactual of a counterfactual is the
        // original execution without the union of both row sets.
        let base_id = live.base.unwrap_or(id);
        let mut all: Vec<u32> = live.excluded.iter().chain(rows).copied().collect();
        all.sort_unstable();
        all.dedup();
        let base = self
            .live(base_id)
            .map_err(|_| "the execution this counterfactual came from was dropped".to_string())?;
        let snapshot = base
            .plan
            .nodes()
            .iter()
            .find_map(|op| match op {
                receipts_plan::Op::Scan { snapshot } => Some(*snapshot),
                _ => None,
            })
            .ok_or("the plan has no scan")?;
        let t0 = now_ms();
        let cf = exclude(&base.plan, &base.execution, self, snapshot, &all)
            .map_err(|e| e.to_string())?;
        let ms = now_ms() - t0;
        let method = match cf.method {
            Method::Unchanged => json!({ "kind": "unchanged" }),
            Method::Incremental {
                groups_recomputed, ..
            } => json!({ "kind": "incremental", "groups_recomputed": groups_recomputed }),
            Method::Rerun => json!({ "kind": "rerun" }),
        };
        let plan = base.plan.clone();
        Ok(self.register(
            Live {
                plan,
                execution: cf.execution,
                base: Some(base_id),
                excluded: all.clone(),
            },
            ms,
            json!({ "method": method, "excluded": all.len() }),
        ))
    }

    /// Leave-one-out values for one aggregate cell. `top` limits the rows
    /// returned to those that move the value most.
    pub fn contributions(
        &self,
        id: u32,
        row: u32,
        column: &str,
        top: usize,
    ) -> Result<Value, String> {
        let live = self.live(id)?;
        let t0 = now_ms();
        let c = contributions(&live.plan, &live.execution, row, column)?;
        let ms = now_ms() - t0;
        let as_f = |v: CfValue| match v {
            CfValue::I64(i) => Some(i as f64),
            CfValue::F64(x) => Some(x),
            CfValue::Null => None,
        };
        let current = as_f(c.current);
        let delta = |v: CfValue| match (as_f(v), current) {
            (Some(a), Some(b)) => (a - b).abs(),
            (None, None) => 0.0,
            _ => f64::INFINITY,
        };
        let moving = c
            .rows
            .iter()
            .filter(|r| r.removes_group || delta(r.without) > 0.0)
            .count();
        let mut ranked: Vec<_> = c.rows.iter().collect();
        ranked.sort_by(|a, b| {
            delta(b.without)
                .partial_cmp(&delta(a.without))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.source_row.cmp(&b.source_row))
        });
        Ok(json!({
            "column": c.column,
            "current": cf_value(c.current),
            "exact": c.exact,
            "contributing_rows": c.rows.len(),
            "rows_that_change_it": moving,
            "ms": ms,
            "top": ranked.iter().take(top).map(|r| json!({
                "source_row": r.source_row,
                "without": cf_value(r.without),
                "removes_group": r.removes_group,
            })).collect::<Vec<_>>(),
        }))
    }

    pub fn drop_execution(&mut self, id: u32) -> bool {
        self.executions.remove(&id).is_some()
    }

    pub fn execution_count(&self) -> usize {
        self.executions.len()
    }
}
