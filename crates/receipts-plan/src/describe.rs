//! One plain-English sentence (or two) per step, for the Pipeline View.
//! They are generated from the plan, never written by hand, so they can't
//! drift from what the engine does.

use crate::plan::{AggFunc, NodeId, Op, ValidPlan};
use crate::types::{Schema, ValueType};
use receipts_core::ColumnType;

/// Sentences for every node, in node order.
pub fn describe(plan: &ValidPlan) -> Vec<String> {
    (0..plan.nodes().len())
        .map(|i| describe_node(plan, NodeId(i as u32)))
        .collect()
}

pub fn describe_node(plan: &ValidPlan, node: NodeId) -> String {
    let op = &plan.nodes()[node.index()];
    let input = op.input().map(|i| plan.schema(i));
    let input = || input.expect("non-scan ops have an input");
    match op {
        Op::Scan { snapshot } => {
            let title = plan.source(node).map_or("a snapshot", |s| s.title.as_str());
            format!(
                "Start from {title} (snapshot {}).",
                &snapshot.to_hex()[..16]
            )
        }
        Op::Filter { predicate, .. } => {
            let mut s = format!("Keep only rows where {}.", predicate.render(input()));
            if predicate.type_of(input()).is_ok_and(|t| t.nullable) {
                s.push_str(
                    " Rows where this can't be decided because a value is missing are dropped.",
                );
            }
            s
        }
        Op::Map { name, expr, .. } => {
            let mut s = format!("Add a column {name}: {}.", expr.render(input()));
            if expr.has_division() {
                s.push_str(" Dividing by zero gives a missing value.");
            }
            s
        }
        Op::Project { columns, .. } => {
            format!("Keep only the columns {}.", join_and(columns))
        }
        Op::Aggregate {
            group_by,
            aggregates,
            ..
        } => {
            let parts: Vec<String> = aggregates
                .iter()
                .map(|a| {
                    let c = a.column.as_deref().unwrap_or("");
                    let what = match a.func {
                        AggFunc::Count => "the number of rows".to_string(),
                        AggFunc::CountNonNull => format!("the number of rows with {c} present"),
                        AggFunc::Sum => format!("the total of {c}"),
                        AggFunc::Mean => format!("the average of {c}"),
                        AggFunc::Median => format!("the median of {c}"),
                        AggFunc::Min => format!("the {} {c}", extreme(input(), c, false)),
                        AggFunc::Max => format!("the {} {c}", extreme(input(), c, true)),
                    };
                    format!("{what} (as {})", a.name)
                })
                .collect();
            let mut s = if group_by.is_empty() {
                format!("Summarize all rows into one: {}.", join_and(&parts))
            } else if parts.is_empty() {
                format!("List each distinct combination of {}.", join_and(group_by))
            } else {
                format!(
                    "Group the rows by {} and, for each group, compute {}.",
                    join_and(group_by),
                    join_and(&parts)
                )
            };
            let skips_missing = aggregates.iter().any(|a| {
                a.func != AggFunc::Count
                    && a.column
                        .as_deref()
                        .and_then(|c| input().field(c))
                        .is_some_and(|f| f.nullable)
            });
            if skips_missing {
                s.push_str(" Missing values are left out of these calculations.");
            }
            if group_by
                .iter()
                .any(|g| input().field(g).is_some_and(|f| f.nullable))
            {
                s.push_str(" Rows with a missing group value form their own group.");
            }
            s
        }
        Op::Sort { keys, .. } => {
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    let ty = input().field(&k.column).map(|f| f.ty);
                    let order = match (ty, k.descending) {
                        (Some(ColumnType::DictUtf8), false) => "A to Z",
                        (Some(ColumnType::DictUtf8), true) => "Z to A",
                        (Some(ColumnType::Timestamp), false) => "earliest first",
                        (Some(ColumnType::Timestamp), true) => "latest first",
                        (_, false) => "smallest first",
                        (_, true) => "largest first",
                    };
                    format!("{} ({order})", k.column)
                })
                .collect();
            let mut s = format!("Sort by {}.", parts.join(", then by "));
            if keys
                .iter()
                .any(|k| input().field(&k.column).is_some_and(|f| f.nullable))
            {
                s.push_str(" Missing values count as the smallest.");
            }
            s
        }
        Op::Limit { count, .. } => {
            if *count == 1 {
                "Keep only the first row.".into()
            } else {
                format!("Keep only the first {} rows.", thousands(*count))
            }
        }
    }
}

fn extreme(schema: &Schema, column: &str, max: bool) -> &'static str {
    let ty = schema.field(column).map(|f| ValueType::of_column(f.ty));
    match (ty, max) {
        (Some(ValueType::Timestamp), false) => "earliest",
        (Some(ValueType::Timestamp), true) => "latest",
        (Some(ValueType::Str), false) => "alphabetically first",
        (Some(ValueType::Str), true) => "alphabetically last",
        (_, false) => "smallest",
        (_, true) => "largest",
    }
}

fn join_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_string(),
        [init @ .., last] => {
            let init: Vec<&str> = init.iter().map(AsRef::as_ref).collect();
            format!("{} and {}", init.join(", "), last.as_ref())
        }
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
