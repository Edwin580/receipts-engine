//! Counterfactual properties on random plans and tables:
//!
//! 1. `exclude` (incremental where possible) equals a full re-execution
//!    without the rows: same output values, bit for bit, and same lineage.
//! 2. Every leave-one-out contribution equals what excluding that one row
//!    actually does to the aggregate cell.

use proptest::prelude::*;
use receipts_cf::{CfValue, Method, contributions, exclude};
use receipts_core::{Bitmap, Column, ColumnData, ColumnType, ContentHash, SourceId};
use receipts_exec::{Exclusion, Execution, SourceProvider, Table, execute, execute_excluding};
use receipts_plan::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SNAP: ContentHash = ContentHash([9; 32]);

fn schema() -> Schema {
    use ColumnType::*;
    Schema::new(vec![
        Field::new("k", I64, false),
        Field::new("x", I64, true),
        Field::new("f", F64, true),
        Field::new("s", DictUtf8, true),
        Field::new("t", Timestamp, false),
    ])
}

#[derive(Clone, Debug)]
struct Row {
    k: i64,
    x: Option<i64>,
    f: Option<f64>,
    s: Option<&'static str>,
    t: i64,
}

fn table(rows: &[Row]) -> Table {
    let validity = |v: Vec<bool>| {
        let b = Bitmap::from_bools(v);
        (b.count_zeros() > 0).then_some(b)
    };
    let mut dictionary: Vec<String> = rows.iter().filter_map(|r| r.s.map(String::from)).collect();
    dictionary.sort();
    dictionary.dedup();
    let cols = vec![
        Column::new(
            "k",
            ColumnData::I64(rows.iter().map(|r| r.k).collect()),
            None,
        ),
        Column::new(
            "x",
            ColumnData::I64(rows.iter().map(|r| r.x.unwrap_or(0)).collect()),
            validity(rows.iter().map(|r| r.x.is_some()).collect()),
        ),
        Column::new(
            "f",
            ColumnData::F64(rows.iter().map(|r| r.f.unwrap_or(0.0)).collect()),
            validity(rows.iter().map(|r| r.f.is_some()).collect()),
        ),
        Column::new(
            "s",
            ColumnData::DictUtf8 {
                codes: rows
                    .iter()
                    .map(|r| {
                        r.s.map_or(0, |s| {
                            dictionary.binary_search(&s.to_string()).unwrap() as u32
                        })
                    })
                    .collect(),
                dictionary,
            },
            validity(rows.iter().map(|r| r.s.is_some()).collect()),
        ),
        Column::new(
            "t",
            ColumnData::Timestamp(rows.iter().map(|r| r.t).collect()),
            None,
        ),
    ];
    Table::new(schema(), cols.into_iter().map(Arc::new).collect()).unwrap()
}

struct Src(Arc<Table>);

impl SourceProvider for Src {
    fn table(&self, s: &ContentHash) -> Option<Arc<Table>> {
        (*s == SNAP).then(|| self.0.clone())
    }
    fn source_id(&self, _: &ContentHash) -> SourceId {
        SourceId(1)
    }
}

impl Catalog for Src {
    fn source(&self, s: &ContentHash) -> Option<SourceInfo> {
        (*s == SNAP).then(|| SourceInfo {
            title: "t".into(),
            schema: schema(),
        })
    }
}

fn arb_row() -> impl Strategy<Value = Row> {
    (
        0i64..3,
        prop::option::weighted(0.8, -5i64..6),
        // Values whose sums depend on the order of addition.
        prop::option::weighted(
            0.8,
            prop::sample::select(vec![0.1, 0.2, 0.3, 1e16, -1e16, 2.5, -0.0]),
        ),
        prop::option::weighted(0.8, prop::sample::select(vec!["A", "B", "C"])),
        0i64..1_000_000,
    )
        .prop_map(|(k, x, f, s, t)| Row { k, x, f, s, t })
}

#[derive(Clone, Debug)]
struct Shape {
    filter: Option<u8>,
    map: bool,
    sort_before: bool,
    limit_before: Option<u64>,
    group_by: Vec<&'static str>,
    aggs: Vec<(AggFunc, Option<&'static str>)>,
    sort_after: bool,
    limit_after: Option<u64>,
}

fn arb_shape() -> impl Strategy<Value = Shape> {
    let agg = prop::sample::select(vec![
        (AggFunc::Count, None),
        (AggFunc::CountNonNull, Some("x")),
        (AggFunc::Sum, Some("x")),
        (AggFunc::Sum, Some("f")),
        (AggFunc::Mean, Some("x")),
        (AggFunc::Mean, Some("f")),
        (AggFunc::Median, Some("x")),
        (AggFunc::Median, Some("f")),
        (AggFunc::Min, Some("x")),
        (AggFunc::Max, Some("f")),
        (AggFunc::Min, Some("t")),
        (AggFunc::Max, Some("t")),
        (AggFunc::Sum, Some("h")),
    ]);
    (
        prop::option::of(0u8..3),
        any::<bool>(),
        any::<bool>(),
        prop::option::weighted(0.15, 0u64..10),
        prop::sample::subsequence(vec!["s", "k"], 0..=2),
        prop::collection::vec(agg, 1..4),
        any::<bool>(),
        prop::option::weighted(0.3, 0u64..3),
    )
        .prop_map(
            |(filter, map, sort_before, limit_before, group_by, aggs, sort_after, limit_after)| {
                Shape {
                    filter,
                    map,
                    sort_before,
                    limit_before,
                    group_by,
                    aggs,
                    sort_after,
                    limit_after,
                }
            },
        )
}

fn build(shape: &Shape) -> Plan {
    let mut p = Plan::scan(SNAP);
    if let Some(f) = shape.filter {
        p = p.filter(match f {
            0 => col("x").cmp(CmpOp::Gt, lit_i64(-2)),
            1 => col("s").in_list(vec![Literal::Str("A".into()), Literal::Str("C".into())]),
            _ => !col("f").is_null(),
        });
    }
    // Always define h so aggregates can use it; `map` decides its formula.
    p = p.map(
        "h",
        if shape.map {
            col("f").arith(ArithOp::Mul, lit_f64(3.0))
        } else {
            col("f")
        },
    );
    if shape.sort_before {
        p = p.sort(vec![SortKey::desc("x")]);
    }
    if let Some(n) = shape.limit_before {
        p = p.limit(n);
    }
    let aggs: Vec<Aggregate> = shape
        .aggs
        .iter()
        .enumerate()
        .map(|(i, (f, c))| Aggregate::new(&format!("a{i}"), *f, *c))
        .collect();
    p = p.aggregate(&shape.group_by, aggs);
    if shape.sort_after {
        p = p.sort(vec![SortKey::desc("a0")]);
    }
    if let Some(n) = shape.limit_after {
        p = p.limit(n);
    }
    p
}

/// Output cells with decimals compared by bit pattern.
fn cells(t: &Table) -> Vec<Vec<String>> {
    (0..t.len())
        .map(|i| {
            t.columns()
                .iter()
                .map(|c| {
                    if !c.is_valid(i) {
                        return "null".to_string();
                    }
                    match &c.data {
                        ColumnData::I64(v) | ColumnData::Timestamp(v) => v[i].to_string(),
                        ColumnData::F64(v) => format!("{:x}", v[i].to_bits()),
                        ColumnData::DictUtf8 { codes, dictionary } => {
                            dictionary[codes[i] as usize].clone()
                        }
                        ColumnData::Bool(b) => b.get(i).to_string(),
                        ColumnData::Geo { .. } => unreachable!(),
                    }
                })
                .collect()
        })
        .collect()
}

fn traces(e: &Execution) -> Vec<Vec<u32>> {
    (0..e.output().len() as u32)
        .map(|o| {
            e.lineage
                .backward(e.output.index(), &[o])
                .source_rows()
                .to_vec()
        })
        .collect()
}

static INCREMENTAL: AtomicUsize = AtomicUsize::new(0);
static RERUN: AtomicUsize = AtomicUsize::new(0);

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1500)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn exclusion_equals_rerun(
        rows in prop::collection::vec(arb_row(), 0..30),
        shape in arb_shape(),
        excluded in prop::collection::vec(0u32..30, 0..6),
    ) {
        let src = Src(Arc::new(table(&rows)));
        let plan = build(&shape);
        let Ok(valid) = validate(plan, &src) else { return Ok(()) };
        let base = execute(&valid, &src).unwrap();
        let cf = exclude(&valid, &base, &src, SNAP, &excluded).unwrap();
        let reference = execute_excluding(&valid, &src, Exclusion { snapshot: SNAP, rows: &excluded }).unwrap();
        prop_assert_eq!(cells(cf.execution.output()), cells(reference.output()), "{:?}", cf.method);
        prop_assert_eq!(traces(&cf.execution), traces(&reference));
        match cf.method {
            Method::Incremental { .. } => { INCREMENTAL.fetch_add(1, Ordering::Relaxed); }
            Method::Rerun => {
                prop_assert!(shape.limit_before.is_some());
                RERUN.fetch_add(1, Ordering::Relaxed);
            }
            Method::Unchanged => {}
        }
    }

    #[test]
    fn contributions_equal_single_exclusions(
        rows in prop::collection::vec(arb_row(), 1..25),
        mut shape in arb_shape(),
        pick in any::<prop::sample::Index>(),
    ) {
        shape.limit_before = None;
        shape.sort_after = false;
        shape.limit_after = None;
        let src = Src(Arc::new(table(&rows)));
        let Ok(valid) = validate(build(&shape), &src) else { return Ok(()) };
        let base = execute(&valid, &src).unwrap();
        if base.output().is_empty() {
            return Ok(());
        }
        let row = pick.index(base.output().len()) as u32;
        let agg = NodeId(valid.nodes().len() as u32 - 1);
        let keys = shape.group_by.len();
        for (i, _) in shape.aggs.iter().enumerate() {
            let name = format!("a{i}");
            let c = contributions(&valid, &base, row, &name).unwrap();
            let base_cells = cells(base.table(agg));
            let key = &base_cells[row as usize][..keys];
            for contribution in &c.rows {
                let cf = exclude(&valid, &base, &src, SNAP, &[contribution.source_row]).unwrap();
                let t = cf.execution.table(agg);
                let after = cells(t);
                let found = after.iter().position(|r| &r[..keys] == key);
                match found {
                    None => prop_assert!(contribution.removes_group, "{:?}", contribution),
                    Some(pos) => {
                        prop_assert!(!contribution.removes_group);
                        let column = t.column(&name).unwrap();
                        let actual = if !column.is_valid(pos) {
                            CfValue::Null
                        } else {
                            match &column.data {
                                ColumnData::I64(v) | ColumnData::Timestamp(v) => CfValue::I64(v[pos]),
                                ColumnData::F64(v) => CfValue::F64(v[pos]),
                                _ => unreachable!(),
                            }
                        };
                        match (c.exact, contribution.without, actual) {
                            (false, CfValue::F64(a), CfValue::F64(b)) => {
                                // Standard bound for summing n terms in
                                // floating point: n * eps * sum of |terms|
                                // (h = 3f at most, so 3 * sum |f| bounds it).
                                let sum_abs: f64 = rows.iter().filter_map(|r| r.f).map(|f| 3.0 * f.abs()).sum();
                                let tol = 4.0 * (rows.len() as f64 + 2.0) * f64::EPSILON * sum_abs + 1e-12;
                                prop_assert!((a - b).abs() <= tol, "{} vs {} (tolerance {})", a, b, tol);
                            }
                            (_, want, got) => prop_assert_eq!(want, got, "{} {:?}", name, contribution),
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn both_paths_are_exercised() {
    // Runs after (or alongside) the property test in the same binary; the
    // counters only prove coverage when that test ran first, so this also
    // runs its own small check.
    let rows: Vec<Row> = (0..10)
        .map(|i| Row {
            k: i % 2,
            x: Some(i),
            f: Some(0.1 * i as f64),
            s: Some(["A", "B"][i as usize % 2]),
            t: i,
        })
        .collect();
    let src = Src(Arc::new(table(&rows)));
    let plan = Plan::scan(SNAP).aggregate(&["s"], vec![Aggregate::new("n", AggFunc::Count, None)]);
    let valid = validate(plan, &src).unwrap();
    let base = execute(&valid, &src).unwrap();
    let cf = exclude(&valid, &base, &src, SNAP, &[0, 2, 4, 6, 8]).unwrap();
    assert!(matches!(
        cf.method,
        Method::Incremental {
            groups_recomputed: 1,
            ..
        }
    ));
    // Group A emptied: only B remains.
    assert_eq!(
        cells(cf.execution.output()),
        vec![vec!["B".to_string(), "5".to_string()]]
    );
    let limited = validate(
        Plan::scan(SNAP)
            .limit(3)
            .aggregate(&[], vec![Aggregate::new("n", AggFunc::Count, None)]),
        &src,
    )
    .unwrap();
    let base = execute(&limited, &src).unwrap();
    let cf = exclude(&limited, &base, &src, SNAP, &[0]).unwrap();
    assert_eq!(cf.method, Method::Rerun);
    // Row 3 moves into the limit, so the count stays 3.
    assert_eq!(cells(cf.execution.output()), vec![vec!["3".to_string()]]);
    let _ = (
        INCREMENTAL.load(Ordering::Relaxed),
        RERUN.load(Ordering::Relaxed),
    );
}
