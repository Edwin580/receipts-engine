//! Counterfactual engine: "what does this number become without these
//! rows?" See `docs/engine/counterfactuals.md`.
//!
//! - [`exclude`] re-derives a whole execution with some source rows left
//!   out. For plans whose rows reach a single aggregate unchanged (only
//!   filter, map, project and sort before it), it recomputes just the
//!   affected groups and the steps after the aggregate. Otherwise it
//!   re-executes the plan. Both give **identical** results; the
//!   incremental path is only faster.
//! - [`contributions`] answers the question for every contributing row of
//!   one aggregate cell at once (leave-one-out, Tier 2).

use receipts_core::{ColumnData, ContentHash};
use receipts_exec::{
    Exclusion, ExecError, Execution, SourceProvider, Table, aggregate_subset, execute_excluding,
    median_of_sorted, rerun_from,
};
use receipts_lineage::StepLineage;
use receipts_plan::{AggFunc, NodeId, Op, ValidPlan};
use std::cmp::Ordering;
use std::sync::Arc;

/// How a counterfactual was computed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    /// None of the excluded rows reached the aggregate (or the plan doesn't
    /// read that snapshot); the base execution stands.
    Unchanged,
    /// Only the aggregate's affected groups, and the steps after it, were
    /// recomputed.
    Incremental {
        aggregate: NodeId,
        groups_recomputed: usize,
    },
    /// The whole plan was re-executed without the excluded rows.
    Rerun,
}

#[derive(Clone, Debug)]
pub struct Counterfactual {
    pub execution: Execution,
    pub method: Method,
}

/// The plan's single scan and single aggregate, if every step between them
/// keeps rows one to one and in a stable order (filter, map, project,
/// sort). Removing a row before such an aggregate removes exactly that
/// row's contribution, so only its group needs recomputing.
fn incremental_shape(plan: &ValidPlan) -> Option<(NodeId, NodeId)> {
    let nodes = plan.nodes();
    let mut aggs = nodes
        .iter()
        .enumerate()
        .filter(|(_, op)| matches!(op, Op::Aggregate { .. }));
    let (a, _) = aggs.next()?;
    if aggs.next().is_some() {
        return None;
    }
    let mut at = nodes[a].input()?;
    loop {
        match &nodes[at.index()] {
            Op::Scan { .. } => return Some((at, NodeId(a as u32))),
            Op::Filter { input, .. }
            | Op::Map { input, .. }
            | Op::Project { input, .. }
            | Op::Sort { input, .. } => at = *input,
            Op::Limit { .. } | Op::Aggregate { .. } => return None,
        }
    }
}

fn scans_snapshot(plan: &ValidPlan, snapshot: &ContentHash) -> bool {
    plan.nodes()
        .iter()
        .any(|op| matches!(op, Op::Scan { snapshot: s } if s == snapshot))
}

/// Re-derives `base` (an execution of `plan`) without the given rows of
/// `snapshot`.
pub fn exclude(
    plan: &ValidPlan,
    base: &Execution,
    sources: &dyn SourceProvider,
    snapshot: ContentHash,
    rows: &[u32],
) -> Result<Counterfactual, ExecError> {
    if !scans_snapshot(plan, &snapshot) || rows.is_empty() {
        return Ok(Counterfactual {
            execution: base.clone(),
            method: Method::Unchanged,
        });
    }
    let Some((scan, agg)) = incremental_shape(plan) else {
        return Ok(Counterfactual {
            execution: execute_excluding(plan, sources, Exclusion { snapshot, rows })?,
            method: Method::Rerun,
        });
    };
    let lineage = &base.lineage;
    let agg_in = plan.nodes()[agg.index()]
        .input()
        .expect("aggregates read a step");
    let removed = lineage.forward(scan.index(), rows, agg_in.index());
    if removed.is_empty() {
        return Ok(Counterfactual {
            execution: base.clone(),
            method: Method::Unchanged,
        });
    }
    let groups = lineage.forward(scan.index(), rows, agg.index());
    let StepLineage::Group {
        offsets,
        rows: members,
    } = lineage.step(agg.index())
    else {
        unreachable!("aggregates record group lineage")
    };
    let input = &base.tables[agg_in.index()];
    let mut is_removed = vec![false; input.len()];
    for &r in &removed {
        is_removed[r as usize] = true;
    }
    let group_members = |p: usize| &members[offsets[p] as usize..offsets[p + 1] as usize];

    // Surviving members of affected groups, ascending: the order a full run
    // would aggregate them in, so sums round identically.
    let mut survivors: Vec<u32> = Vec::new();
    let mut is_affected = vec![false; offsets.len() - 1];
    let mut survives = vec![false; offsets.len() - 1];
    for &p in &groups {
        is_affected[p as usize] = true;
        let before = survivors.len();
        survivors.extend(
            group_members(p as usize)
                .iter()
                .filter(|&&m| !is_removed[m as usize]),
        );
        survives[p as usize] = survivors.len() > before;
    }
    survivors.sort_unstable();
    let (mini, mini_lineage) = aggregate_subset(plan, agg, input, &survivors)?;
    let StepLineage::Group {
        offsets: mini_offsets,
        rows: mini_members,
    } = &mini_lineage
    else {
        unreachable!()
    };

    // Groups stay in key order: unaffected groups keep their row, affected
    // ones take the next recomputed row (the mini result is in key order
    // too), and emptied groups disappear. With no group_by there is always
    // exactly one row.
    let Op::Aggregate { group_by, .. } = &plan.nodes()[agg.index()] else {
        unreachable!()
    };
    let always_one_row = group_by.is_empty();
    let mut picks = Vec::new();
    let mut new_offsets = vec![0u32];
    let mut new_members = Vec::new();
    let mut q = 0usize;
    for p in 0..offsets.len() - 1 {
        if !is_affected[p] {
            picks.push((0, p as u32));
            new_members.extend_from_slice(group_members(p));
        } else if survives[p] || always_one_row {
            picks.push((1, q as u32));
            new_members.extend_from_slice(
                &mini_members[mini_offsets[q] as usize..mini_offsets[q + 1] as usize],
            );
            q += 1;
        } else {
            continue;
        }
        new_offsets.push(new_members.len() as u32);
    }
    debug_assert_eq!(q, mini.len(), "every recomputed group is placed");
    let base_agg = base.tables[agg.index()].as_ref();
    let table = Table::interleave(&[base_agg, &mini], &picks)
        .map_err(|message| ExecError { node: agg, message })?;
    let execution = rerun_from(
        plan,
        base,
        agg,
        Arc::new(table),
        StepLineage::Group {
            offsets: new_offsets,
            rows: new_members,
        },
        sources,
    )?;
    Ok(Counterfactual {
        execution,
        method: Method::Incremental {
            aggregate: agg,
            groups_recomputed: groups.len(),
        },
    })
}

/// A value of an aggregate cell.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CfValue {
    Null,
    I64(i64),
    F64(f64),
}

/// What an aggregate cell would be without one of its contributing rows.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Contribution {
    /// Snapshot row index.
    pub source_row: u32,
    /// The cell's value without this row.
    pub without: CfValue,
    /// Excluding this row empties its group, so the row disappears from
    /// the aggregate's output (only with `group_by`).
    pub removes_group: bool,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Contributions {
    pub aggregate: NodeId,
    /// Row of the aggregate's output.
    pub group: u32,
    pub column: String,
    pub current: CfValue,
    /// False for decimal sums and means: leave-one-out values are computed
    /// as `sum - x`, which can differ from a re-run in the last bits. Every
    /// other function is exact. [`exclude`] always is.
    pub exact: bool,
    /// One entry per contributing row, in the group's row order.
    pub rows: Vec<Contribution>,
}

/// For output row `row` of the plan and aggregate column `column`: the
/// cell's value without each of its contributing rows.
pub fn contributions(
    plan: &ValidPlan,
    base: &Execution,
    row: u32,
    column: &str,
) -> Result<Contributions, String> {
    let (_, agg) = incremental_shape(plan).ok_or(
        "contributions need a plan with one aggregate and only filter, map, project or sort before it",
    )?;
    let Op::Aggregate {
        group_by,
        aggregates,
        ..
    } = &plan.nodes()[agg.index()]
    else {
        unreachable!()
    };
    let spec = aggregates
        .iter()
        .find(|a| a.name == column)
        .ok_or_else(|| format!("\"{column}\" is not an aggregate of step {}", agg.0))?;
    let lineage = &base.lineage;
    let trace = lineage.backward_row(base.output.index(), row);
    let at_agg = trace
        .path
        .iter()
        .find(|s| s.node == agg.index())
        .ok_or("that output row doesn't come from the aggregate")?;
    let [group] = at_agg.rows[..] else {
        return Err("that output row combines several aggregate rows".into());
    };
    let StepLineage::Group { offsets, rows } = lineage.step(agg.index()) else {
        unreachable!()
    };
    let members = &rows[offsets[group as usize] as usize..offsets[group as usize + 1] as usize];
    let agg_in = lineage.input(agg.index()).expect("aggregates read a step");
    let source_rows: Vec<u32> = members
        .iter()
        .map(|&m| {
            lineage
                .source_row(agg_in, m)
                .expect("one-to-one steps below the aggregate")
        })
        .collect();
    let removes_group = !group_by.is_empty() && members.len() == 1;
    let cell = &base.tables[agg.index()]
        .column(column)
        .expect("aggregate column")
        .clone();
    let current = value_at(cell, group as usize);
    let input_table = &base.tables[agg_in];
    let col = spec
        .column
        .as_deref()
        .map(|c| input_table.column(c).expect("validated").clone());

    let n = members.len();
    let valid = |m: u32| col.as_ref().is_some_and(|c| c.is_valid(m as usize));
    let non_null = members.iter().filter(|&&m| valid(m)).count();
    let mut exact = true;
    let without: Vec<CfValue> = match spec.func {
        AggFunc::Count => vec![CfValue::I64(n as i64 - 1); n],
        AggFunc::CountNonNull => members
            .iter()
            .map(|&m| CfValue::I64((non_null - valid(m) as usize) as i64))
            .collect(),
        AggFunc::Sum | AggFunc::Mean => {
            let c = col.as_ref().expect("sum/mean read a column");
            let mean = spec.func == AggFunc::Mean;
            match &c.data {
                ColumnData::I64(v) => {
                    let total: i128 = members
                        .iter()
                        .filter(|&&m| valid(m))
                        .map(|&m| v[m as usize] as i128)
                        .sum();
                    members
                        .iter()
                        .map(|&m| {
                            let (s, k) = if valid(m) {
                                (total - v[m as usize] as i128, non_null - 1)
                            } else {
                                (total, non_null)
                            };
                            if k == 0 {
                                CfValue::Null
                            } else if mean {
                                CfValue::F64(s as f64 / k as f64)
                            } else {
                                i64::try_from(s).map_or(CfValue::Null, CfValue::I64)
                            }
                        })
                        .collect()
                }
                ColumnData::F64(v) => {
                    exact = false;
                    let mut total = 0.0;
                    for &m in members.iter().filter(|&&m| valid(m)) {
                        total += v[m as usize];
                    }
                    members
                        .iter()
                        .map(|&m| {
                            let (s, k) = if valid(m) {
                                (total - v[m as usize], non_null - 1)
                            } else {
                                (total, non_null)
                            };
                            let r = if mean { s / k as f64 } else { s };
                            if k == 0 || !r.is_finite() {
                                CfValue::Null
                            } else {
                                CfValue::F64(r)
                            }
                        })
                        .collect()
                }
                _ => unreachable!("validated: numeric sum/mean"),
            }
        }
        AggFunc::Median | AggFunc::Min | AggFunc::Max => {
            let c = col.as_ref().expect("reads a column");
            // (value, member position) for non-null members, sorted by value.
            let (mut ranked, as_float): (Vec<(f64, usize)>, bool) = match &c.data {
                ColumnData::I64(v) | ColumnData::Timestamp(v) => (
                    members
                        .iter()
                        .enumerate()
                        .filter(|&(_, &m)| valid(m))
                        .map(|(i, &m)| (v[m as usize] as f64, i))
                        .collect(),
                    spec.func == AggFunc::Median,
                ),
                ColumnData::F64(v) => (
                    members
                        .iter()
                        .enumerate()
                        .filter(|&(_, &m)| valid(m))
                        .map(|(i, &m)| (v[m as usize], i))
                        .collect(),
                    true,
                ),
                _ => {
                    return Err(format!(
                        "contributions to the {} of text or true/false values aren't supported",
                        spec.func.name()
                    ));
                }
            };
            if spec.func != AggFunc::Median && !as_float {
                // Integers and timestamps: keep exact i64 values.
                let (ColumnData::I64(v) | ColumnData::Timestamp(v)) = &c.data else {
                    unreachable!()
                };
                let mut vals: Vec<(i64, usize)> = members
                    .iter()
                    .enumerate()
                    .filter(|&(_, &m)| valid(m))
                    .map(|(i, &m)| (v[m as usize], i))
                    .collect();
                vals.sort();
                return Ok(finish(
                    agg,
                    group,
                    column,
                    current,
                    true,
                    &source_rows,
                    removes_group,
                    extreme_without(&vals, n, spec.func == AggFunc::Max, CfValue::I64),
                ));
            }
            // Stable by position, so equal values keep row order.
            ranked.sort_by(|a, b| {
                a.0.partial_cmp(&b.0)
                    .unwrap_or(Ordering::Equal)
                    .then(a.1.cmp(&b.1))
            });
            if spec.func == AggFunc::Median {
                let sorted: Vec<f64> = ranked.iter().map(|x| x.0).collect();
                let all = median_of_sorted(&sorted).map_or(CfValue::Null, CfValue::F64);
                let mut out = vec![all; n];
                for (rank, &(_, pos)) in ranked.iter().enumerate() {
                    out[pos] = median_without(&sorted, rank).map_or(CfValue::Null, CfValue::F64);
                }
                out
            } else {
                extreme_without(&ranked, n, spec.func == AggFunc::Max, CfValue::F64)
            }
        }
    };
    Ok(finish(
        agg,
        group,
        column,
        current,
        exact,
        &source_rows,
        removes_group,
        without,
    ))
}

#[allow(clippy::too_many_arguments)]
fn finish(
    aggregate: NodeId,
    group: u32,
    column: &str,
    current: CfValue,
    exact: bool,
    source_rows: &[u32],
    removes_group: bool,
    without: Vec<CfValue>,
) -> Contributions {
    Contributions {
        aggregate,
        group,
        column: column.to_string(),
        current,
        exact,
        rows: source_rows
            .iter()
            .zip(without)
            .map(|(&source_row, without)| Contribution {
                source_row,
                without,
                removes_group,
            })
            .collect(),
    }
}

/// Min (or max) without each member, given the non-null `(value, position)`
/// pairs sorted ascending. Only removing the extreme itself changes it.
fn extreme_without<T: Copy>(
    sorted: &[(T, usize)],
    n: usize,
    max: bool,
    wrap: impl Fn(T) -> CfValue,
) -> Vec<CfValue> {
    let pick = |i: usize| wrap(sorted[i].0);
    let (best, second) = if max {
        (sorted.len().checked_sub(1), sorted.len().checked_sub(2))
    } else {
        (
            (!sorted.is_empty()).then_some(0),
            (sorted.len() > 1).then_some(1),
        )
    };
    let all = best.map_or(CfValue::Null, pick);
    let mut out = vec![all; n];
    if let Some(b) = best {
        out[sorted[b].1] = second.map_or(CfValue::Null, pick);
    }
    out
}

/// Median of `sorted` with the element at `rank` removed, using the same
/// formula as the executor.
fn median_without(sorted: &[f64], rank: usize) -> Option<f64> {
    let n = sorted.len() - 1;
    let at = |k: usize| if k < rank { sorted[k] } else { sorted[k + 1] };
    if n == 0 {
        return None;
    }
    if n % 2 == 1 {
        return Some(at(n / 2));
    }
    median_of_sorted(&[at(n / 2 - 1), at(n / 2)])
}

fn value_at(c: &receipts_core::Column, i: usize) -> CfValue {
    if !c.is_valid(i) {
        return CfValue::Null;
    }
    match &c.data {
        ColumnData::I64(v) | ColumnData::Timestamp(v) => CfValue::I64(v[i]),
        ColumnData::F64(v) => CfValue::F64(v[i]),
        _ => CfValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_without_each_rank() {
        let s = [1.0, 2.0, 4.0, 8.0];
        // Removing 1 -> [2,4,8] -> 4; removing 8 -> [1,2,4] -> 2.
        assert_eq!(median_without(&s, 0), Some(4.0));
        assert_eq!(median_without(&s, 3), Some(2.0));
        assert_eq!(median_without(&s, 1), Some(4.0));
        let s = [1.0, 2.0, 4.0];
        assert_eq!(median_without(&s, 1), Some(2.5));
        assert_eq!(median_without(&[3.0], 0), None);
    }

    #[test]
    fn extremes_change_only_for_the_extreme() {
        let sorted = [(1i64, 2usize), (1, 0), (5, 1)];
        let out = extreme_without(&sorted, 4, false, CfValue::I64);
        // Removing the first 1 leaves the other 1; position 3 is null.
        assert_eq!(
            out,
            vec![
                CfValue::I64(1),
                CfValue::I64(1),
                CfValue::I64(1),
                CfValue::I64(1)
            ]
        );
        let out = extreme_without(&sorted, 4, true, CfValue::I64);
        assert_eq!(out[1], CfValue::I64(1));
        assert_eq!(out[0], CfValue::I64(5));
    }
}
