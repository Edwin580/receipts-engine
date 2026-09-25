//! Physical operators over the column store. Lineage capture (Tier 1) is
//! added in M2 and aggregate contributions (Tier 2) in M3.
//!
//! `execute` runs a validated plan and keeps every step's result, so the
//! Pipeline View can show a row count per step.

mod eval;
mod ops;
mod table;

pub use ops::median_of_sorted;
pub use table::Table;

use eval::{Evaluator, materialize, selection};
use receipts_core::{Column, ContentHash};
use receipts_plan::{NodeId, Op, ValidPlan};
use std::fmt;
use std::sync::Arc;
use table::{gather, renamed};

/// Supplies the loaded snapshot tables that `Scan` steps read.
pub trait SourceProvider {
    fn table(&self, snapshot: &ContentHash) -> Option<Arc<Table>>;
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExecError {
    pub node: NodeId,
    pub message: String,
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "step {}: {}", self.node.0, self.message)
    }
}

impl std::error::Error for ExecError {}

/// Every step's result, indexed like the plan's nodes.
#[derive(Clone, Debug)]
pub struct Execution {
    pub tables: Vec<Arc<Table>>,
    pub output: NodeId,
}

impl Execution {
    pub fn output(&self) -> &Arc<Table> {
        &self.tables[self.output.index()]
    }
    pub fn table(&self, node: NodeId) -> &Arc<Table> {
        &self.tables[node.index()]
    }
}

pub fn execute(plan: &ValidPlan, sources: &dyn SourceProvider) -> Result<Execution, ExecError> {
    let mut tables: Vec<Arc<Table>> = Vec::with_capacity(plan.nodes().len());
    for (i, op) in plan.nodes().iter().enumerate() {
        let node = NodeId(i as u32);
        let err = |message: String| ExecError { node, message };
        let input = op.input().map(|id| tables[id.index()].clone());
        let table = run(plan, node, op, input.as_deref(), sources).map_err(err)?;
        tables.push(table);
    }
    Ok(Execution {
        tables,
        output: plan.output(),
    })
}

fn run(
    plan: &ValidPlan,
    node: NodeId,
    op: &Op,
    input: Option<&Table>,
    sources: &dyn SourceProvider,
) -> Result<Arc<Table>, String> {
    let schema = plan.schema(node).clone();
    let input = || input.expect("non-scan ops have an input");
    let col = |name: &str| {
        input()
            .column(name)
            .expect("validated: column exists")
            .as_ref()
    };
    let columns: Vec<Arc<Column>> = match op {
        Op::Scan { snapshot } => {
            let t = sources
                .table(snapshot)
                .ok_or_else(|| format!("snapshot {} is not loaded", snapshot.to_hex()))?;
            if *t.schema() != schema {
                return Err("the loaded snapshot's columns don't match its catalog entry".into());
            }
            return Ok(t);
        }
        Op::Filter { predicate, .. } => {
            let t = input();
            let d = Evaluator { table: t }.eval(predicate)?;
            let keep = selection(&d, t.len());
            if keep.len() == t.len() {
                t.columns().to_vec()
            } else {
                t.columns()
                    .iter()
                    .map(|c| Arc::new(gather(c, &keep)))
                    .collect()
            }
        }
        Op::Map { name, expr, .. } => {
            let t = input();
            let d = Evaluator { table: t }.eval(expr)?;
            let mut cols = t.columns().to_vec();
            cols.push(Arc::new(materialize(d, t.len(), name)));
            cols
        }
        Op::Project { columns, .. } => columns
            .iter()
            .map(|c| input().column(c).expect("validated").clone())
            .collect(),
        Op::Sort { keys, .. } => {
            let t = input();
            let keys: Vec<(&Column, bool)> = keys
                .iter()
                .map(|k| (col(&k.column), k.descending))
                .collect();
            let perm = ops::sort_permutation(t.len(), &keys);
            t.columns()
                .iter()
                .map(|c| Arc::new(gather(c, &perm)))
                .collect()
        }
        Op::Limit { count, .. } => {
            let t = input();
            let n = (*count).min(t.len() as u64) as usize;
            if n == t.len() {
                t.columns().to_vec()
            } else {
                let rows: Vec<u32> = (0..n as u32).collect();
                t.columns()
                    .iter()
                    .map(|c| Arc::new(gather(c, &rows)))
                    .collect()
            }
        }
        Op::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            let t = input();
            let keys: Vec<&Column> = group_by.iter().map(|g| col(g)).collect();
            let groups = ops::group(t.len(), &keys);
            let order: Vec<u32> = if keys.is_empty() {
                vec![0]
            } else {
                ops::group_order(&groups, &keys)
            };
            let mut out: Vec<Arc<Column>> = ops::key_columns(&keys, &groups, &order)
                .into_iter()
                .map(Arc::new)
                .collect();
            for a in aggregates {
                let values = ops::aggregate(a.func, a.column.as_deref().map(col), &groups)?;
                out.push(Arc::new(renamed(gather(&values, &order), &a.name)));
            }
            out
        }
    };
    Table::new(schema, columns)
        .map(Arc::new)
        .map_err(|e| format!("internal error: {e}"))
}
