//! Differential test: random valid plans over random tables, executed by the
//! vectorized engine and by a deliberately naive row-at-a-time interpreter
//! written from the semantics in `docs/engine/plan.md`. They share no code
//! beyond the plan types and core calendar helpers.

use proptest::prelude::*;
use receipts_core::time::{MICROS_PER_DAY, civil_from_days, days_from_civil};
use receipts_core::{Bitmap, Column, ColumnData, ColumnType, ContentHash};
use receipts_exec::{SourceProvider, Table, execute};
use receipts_plan::*;
use std::cmp::Ordering;
use std::ops::Not;
use std::sync::Arc;

const SNAP: ContentHash = ContentHash([7; 32]);

#[derive(Clone, Debug)]
enum V {
    Null,
    Bool(bool),
    I(i64),
    F(f64),
    S(String),
    T(i64),
    G(f32, f32),
}

impl PartialEq for V {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (V::Null, V::Null) => true,
            (V::Bool(a), V::Bool(b)) => a == b,
            (V::I(a), V::I(b)) | (V::T(a), V::T(b)) => a == b,
            (V::F(a), V::F(b)) => a == b,
            (V::S(a), V::S(b)) => a == b,
            (V::G(a, b), V::G(c, d)) => a == c && b == d,
            _ => false,
        }
    }
}

/// Order for non-null values of comparable types.
fn vcmp(a: &V, b: &V) -> Ordering {
    match (a, b) {
        (V::I(x), V::I(y)) | (V::T(x), V::T(y)) => x.cmp(y),
        // Test integers are small, so converting to f64 is exact.
        (V::I(x), V::F(y)) => (*x as f64).partial_cmp(y).unwrap(),
        (V::F(x), V::I(y)) => x.partial_cmp(&(*y as f64)).unwrap(),
        (V::F(x), V::F(y)) => x.partial_cmp(y).unwrap(),
        (V::S(x), V::S(y)) => x.as_bytes().cmp(y.as_bytes()),
        (V::Bool(x), V::Bool(y)) => x.cmp(y),
        _ => panic!("incomparable {a:?} {b:?}"),
    }
}

/// Order with missing values smallest.
fn vcmp_nulls_first(a: &V, b: &V) -> Ordering {
    match (a, b) {
        (V::Null, V::Null) => Ordering::Equal,
        (V::Null, _) => Ordering::Less,
        (_, V::Null) => Ordering::Greater,
        _ => vcmp(a, b),
    }
}

#[derive(Clone, Debug)]
struct RTable {
    names: Vec<String>,
    rows: Vec<Vec<V>>,
}

impl RTable {
    fn idx(&self, name: &str) -> usize {
        self.names.iter().position(|n| n == name).unwrap()
    }
}

fn eval(e: &Expr, t: &RTable, row: &[V]) -> Result<V, String> {
    Ok(match e {
        Expr::Column(c) => row[t.idx(c)].clone(),
        Expr::Literal(l) => match l {
            Literal::Null => V::Null,
            Literal::Bool(b) => V::Bool(*b),
            Literal::I64(i) => V::I(*i),
            Literal::F64(x) => V::F(*x),
            Literal::Str(s) => V::S(s.clone()),
            Literal::Timestamp(x) => V::T(*x),
        },
        Expr::Compare(op, a, b) => {
            let (a, b) = (eval(a, t, row)?, eval(b, t, row)?);
            if a == V::Null || b == V::Null {
                V::Null
            } else {
                let o = vcmp(&a, &b);
                V::Bool(match op {
                    CmpOp::Eq => o == Ordering::Equal,
                    CmpOp::Ne => o != Ordering::Equal,
                    CmpOp::Lt => o == Ordering::Less,
                    CmpOp::Le => o != Ordering::Greater,
                    CmpOp::Gt => o == Ordering::Greater,
                    CmpOp::Ge => o != Ordering::Less,
                })
            }
        }
        Expr::And(args) => {
            let vals = args
                .iter()
                .map(|a| eval(a, t, row))
                .collect::<Result<Vec<_>, _>>()?;
            if vals.contains(&V::Bool(false)) {
                V::Bool(false)
            } else if vals.contains(&V::Null) {
                V::Null
            } else {
                V::Bool(true)
            }
        }
        Expr::Or(args) => {
            let vals = args
                .iter()
                .map(|a| eval(a, t, row))
                .collect::<Result<Vec<_>, _>>()?;
            if vals.contains(&V::Bool(true)) {
                V::Bool(true)
            } else if vals.contains(&V::Null) {
                V::Null
            } else {
                V::Bool(false)
            }
        }
        Expr::Not(a) => match eval(a, t, row)? {
            V::Bool(b) => V::Bool(!b),
            _ => V::Null,
        },
        Expr::IsNull(a) => V::Bool(eval(a, t, row)? == V::Null),
        Expr::InList(a, values) => {
            let a = eval(a, t, row)?;
            if a == V::Null {
                V::Null
            } else {
                let lits: Vec<V> = values
                    .iter()
                    .map(|l| eval(&Expr::Literal(l.clone()), t, row))
                    .collect::<Result<_, _>>()?;
                V::Bool(lits.iter().any(|l| vcmp(&a, l) == Ordering::Equal))
            }
        }
        Expr::Arith(op, a, b) => {
            let (a, b) = (eval(a, t, row)?, eval(b, t, row)?);
            let checked = |r: Option<i64>| r.ok_or_else(|| "overflow".to_string());
            match (a, b) {
                (V::Null, _) | (_, V::Null) => V::Null,
                (V::I(x), V::I(y)) if *op != ArithOp::Div => V::I(checked(match op {
                    ArithOp::Add => x.checked_add(y),
                    ArithOp::Sub => x.checked_sub(y),
                    _ => x.checked_mul(y),
                })?),
                (V::T(x), V::T(y)) => V::I(checked(x.checked_sub(y))?),
                (V::T(x), V::I(y)) => V::T(checked(if *op == ArithOp::Add {
                    x.checked_add(y)
                } else {
                    x.checked_sub(y)
                })?),
                (V::I(x), V::T(y)) => V::T(checked(x.checked_add(y))?),
                (x, y) => {
                    let f = |v: V| match v {
                        V::I(i) => i as f64,
                        V::F(f) => f,
                        other => panic!("not numeric: {other:?}"),
                    };
                    let (x, y) = (f(x), f(y));
                    let r = match op {
                        ArithOp::Add => x + y,
                        ArithOp::Sub => x - y,
                        ArithOp::Mul => x * y,
                        ArithOp::Div => {
                            if y == 0.0 {
                                return Ok(V::Null);
                            }
                            x / y
                        }
                    };
                    if r.is_finite() { V::F(r) } else { V::Null }
                }
            }
        }
        Expr::DateTrunc(unit, a) => match eval(a, t, row)? {
            V::T(x) => {
                let day = x.div_euclid(MICROS_PER_DAY);
                let (y, m, _) = civil_from_days(day);
                let start = match unit {
                    TimeUnit::Day => day,
                    TimeUnit::Week => {
                        // Walk back to Monday; 1970-01-05 was a Monday.
                        let mut d = day;
                        while (d - 4).rem_euclid(7) != 0 {
                            d -= 1;
                        }
                        d
                    }
                    TimeUnit::Month => days_from_civil(y, m, 1),
                    TimeUnit::Year => days_from_civil(y, 1, 1),
                };
                V::T(start * MICROS_PER_DAY)
            }
            _ => V::Null,
        },
        Expr::Lat(a) => match eval(a, t, row)? {
            V::G(lat, _) => V::F(lat as f64),
            _ => V::Null,
        },
        Expr::Lon(a) => match eval(a, t, row)? {
            V::G(_, lon) => V::F(lon as f64),
            _ => V::Null,
        },
    })
}

fn median(mut v: Vec<f64>) -> V {
    if v.is_empty() {
        return V::Null;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 {
        V::F(v[n / 2])
    } else {
        let (a, b) = (v[n / 2 - 1], v[n / 2]);
        let m = (a + b) / 2.0;
        V::F(if m.is_finite() { m } else { a / 2.0 + b / 2.0 })
    }
}

fn run_reference(plan: &Plan, source: &RTable) -> Result<RTable, String> {
    let mut results: Vec<RTable> = Vec::new();
    for op in &plan.nodes {
        let input = op.input().map(|i| results[i.index()].clone());
        let out = match op {
            Op::Scan { .. } => source.clone(),
            Op::Filter { predicate, .. } => {
                let t = input.unwrap();
                let mut rows = Vec::new();
                for r in &t.rows {
                    if eval(predicate, &t, r)? == V::Bool(true) {
                        rows.push(r.clone());
                    }
                }
                RTable { rows, ..t }
            }
            Op::Map { name, expr, .. } => {
                let mut t = input.unwrap();
                let mut rows = Vec::new();
                for r in &t.rows {
                    let mut r = r.clone();
                    r.push(eval(expr, &t, &r)?);
                    rows.push(r);
                }
                t.names.push(name.clone());
                RTable { rows, ..t }
            }
            Op::Project { columns, .. } => {
                let t = input.unwrap();
                let idx: Vec<usize> = columns.iter().map(|c| t.idx(c)).collect();
                RTable {
                    names: columns.clone(),
                    rows: t
                        .rows
                        .iter()
                        .map(|r| idx.iter().map(|&i| r[i].clone()).collect())
                        .collect(),
                }
            }
            Op::Sort { keys, .. } => {
                let mut t = input.unwrap();
                let idx: Vec<(usize, bool)> = keys
                    .iter()
                    .map(|k| (t.idx(&k.column), k.descending))
                    .collect();
                t.rows.sort_by(|a, b| {
                    for &(i, desc) in &idx {
                        let o = vcmp_nulls_first(&a[i], &b[i]);
                        if o != Ordering::Equal {
                            return if desc { o.reverse() } else { o };
                        }
                    }
                    Ordering::Equal
                });
                t
            }
            Op::Limit { count, .. } => {
                let mut t = input.unwrap();
                t.rows.truncate(*count as usize);
                t
            }
            Op::Aggregate {
                group_by,
                aggregates,
                ..
            } => {
                let t = input.unwrap();
                let gi: Vec<usize> = group_by.iter().map(|g| t.idx(g)).collect();
                let mut groups: Vec<(Vec<V>, Vec<usize>)> = Vec::new();
                if gi.is_empty() {
                    groups.push((vec![], (0..t.rows.len()).collect()));
                } else {
                    for (ri, r) in t.rows.iter().enumerate() {
                        let key: Vec<V> = gi.iter().map(|&i| r[i].clone()).collect();
                        match groups.iter_mut().find(|(k, _)| *k == key) {
                            Some((_, rows)) => rows.push(ri),
                            None => groups.push((key, vec![ri])),
                        }
                    }
                    groups.sort_by(|(a, _), (b, _)| {
                        a.iter()
                            .zip(b)
                            .map(|(x, y)| vcmp_nulls_first(x, y))
                            .find(|o| *o != Ordering::Equal)
                            .unwrap_or(Ordering::Equal)
                    });
                }
                let mut rows = Vec::new();
                for (key, members) in &groups {
                    let mut row = key.clone();
                    for a in aggregates {
                        let vals: Vec<V> = match &a.column {
                            Some(c) => {
                                let i = t.idx(c);
                                members
                                    .iter()
                                    .map(|&r| t.rows[r][i].clone())
                                    .filter(|v| *v != V::Null)
                                    .collect()
                            }
                            None => vec![],
                        };
                        let as_f = |v: &V| match v {
                            V::I(i) => *i as f64,
                            V::F(f) => *f,
                            _ => panic!(),
                        };
                        row.push(match a.func {
                            AggFunc::Count => V::I(members.len() as i64),
                            AggFunc::CountNonNull => V::I(vals.len() as i64),
                            AggFunc::Sum | AggFunc::Mean if vals.is_empty() => V::Null,
                            AggFunc::Sum | AggFunc::Mean => match &vals[0] {
                                V::I(_) => {
                                    let s: i128 = vals
                                        .iter()
                                        .map(|v| if let V::I(i) = v { *i as i128 } else { 0 })
                                        .sum();
                                    if a.func == AggFunc::Sum {
                                        V::I(i64::try_from(s).map_err(|_| "overflow".to_string())?)
                                    } else {
                                        V::F(s as f64 / vals.len() as f64)
                                    }
                                }
                                _ => {
                                    let mut s = 0.0;
                                    for v in &vals {
                                        s += as_f(v);
                                    }
                                    let r = if a.func == AggFunc::Sum {
                                        s
                                    } else {
                                        s / vals.len() as f64
                                    };
                                    if r.is_finite() { V::F(r) } else { V::Null }
                                }
                            },
                            AggFunc::Median => median(vals.iter().map(as_f).collect()),
                            AggFunc::Min | AggFunc::Max => {
                                let want = if a.func == AggFunc::Min {
                                    Ordering::Less
                                } else {
                                    Ordering::Greater
                                };
                                let mut best: Option<&V> = None;
                                for v in &vals {
                                    if best.is_none_or(|b| vcmp(v, b) == want) {
                                        best = Some(v);
                                    }
                                }
                                best.cloned().unwrap_or(V::Null)
                            }
                        });
                    }
                    rows.push(row);
                }
                let mut names = group_by.clone();
                names.extend(aggregates.iter().map(|a| a.name.clone()));
                RTable { names, rows }
            }
        };
        results.push(out);
    }
    Ok(results.swap_remove(plan.output.index()))
}

// ---- Converting between the engine's columns and reference rows ----

fn schema() -> Schema {
    use ColumnType::*;
    Schema::new(vec![
        Field::new("k", I64, false),
        Field::new("x", I64, true),
        Field::new("f", F64, true),
        Field::new("t", Timestamp, false),
        Field::new("u", Timestamp, true),
        Field::new("s", DictUtf8, true),
        Field::new("b", Bool, true),
        Field::new("g", Geo, true),
    ])
}

fn to_engine(rows: &[Vec<V>]) -> Table {
    let s = schema();
    let n = rows.len();
    let cols = s
        .fields
        .iter()
        .enumerate()
        .map(|(ci, f)| {
            let vals: Vec<&V> = rows.iter().map(|r| &r[ci]).collect();
            let validity = Bitmap::from_bools(vals.iter().map(|v| **v != V::Null));
            let validity = (validity.count_zeros() > 0).then_some(validity);
            let data = match f.ty {
                ColumnType::I64 => ColumnData::I64(
                    vals.iter()
                        .map(|v| if let V::I(i) = v { *i } else { 0 })
                        .collect(),
                ),
                ColumnType::Timestamp => ColumnData::Timestamp(
                    vals.iter()
                        .map(|v| if let V::T(i) = v { *i } else { 0 })
                        .collect(),
                ),
                ColumnType::F64 => ColumnData::F64(
                    vals.iter()
                        .map(|v| if let V::F(x) = v { *x } else { 0.0 })
                        .collect(),
                ),
                ColumnType::Bool => ColumnData::Bool(Bitmap::from_bools(
                    vals.iter().map(|v| matches!(v, V::Bool(true))),
                )),
                ColumnType::DictUtf8 => {
                    let mut dictionary: Vec<String> = vals
                        .iter()
                        .filter_map(|v| {
                            if let V::S(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    // Also an unused entry, as after a filter.
                    dictionary.push("zzz-unused".into());
                    dictionary.sort();
                    dictionary.dedup();
                    let codes = vals
                        .iter()
                        .map(|v| match v {
                            V::S(s) => dictionary.binary_search(s).unwrap() as u32,
                            _ => 0,
                        })
                        .collect();
                    ColumnData::DictUtf8 { codes, dictionary }
                }
                ColumnType::Geo => ColumnData::Geo {
                    lat: vals
                        .iter()
                        .map(|v| if let V::G(a, _) = v { *a } else { 0.0 })
                        .collect(),
                    lon: vals
                        .iter()
                        .map(|v| if let V::G(_, b) = v { *b } else { 0.0 })
                        .collect(),
                },
            };
            Arc::new(Column::new(f.name.clone(), data, validity))
        })
        .collect();
    let t = Table::new(s, cols).unwrap();
    assert_eq!(t.len(), n);
    t
}

fn from_engine(t: &Table) -> RTable {
    let names = t.schema().fields.iter().map(|f| f.name.clone()).collect();
    let rows = (0..t.len())
        .map(|i| {
            t.columns()
                .iter()
                .map(|c| {
                    if !c.is_valid(i) {
                        return V::Null;
                    }
                    match &c.data {
                        ColumnData::I64(v) => V::I(v[i]),
                        ColumnData::Timestamp(v) => V::T(v[i]),
                        ColumnData::F64(v) => V::F(v[i]),
                        ColumnData::Bool(b) => V::Bool(b.get(i)),
                        ColumnData::DictUtf8 { codes, dictionary } => {
                            V::S(dictionary[codes[i] as usize].clone())
                        }
                        ColumnData::Geo { lat, lon } => V::G(lat[i], lon[i]),
                    }
                })
                .collect()
        })
        .collect();
    RTable { names, rows }
}

struct One(Arc<Table>);

impl SourceProvider for One {
    fn table(&self, s: &ContentHash) -> Option<Arc<Table>> {
        (*s == SNAP).then(|| self.0.clone())
    }
}

impl Catalog for One {
    fn source(&self, s: &ContentHash) -> Option<SourceInfo> {
        (*s == SNAP).then(|| SourceInfo {
            title: "test".into(),
            schema: schema(),
        })
    }
}

// ---- Random tables and plans ----

const T0: i64 = 1_704_067_200_000_000; // 2024-01-01

fn arb_row() -> impl Strategy<Value = Vec<V>> {
    let nullable = |s: BoxedStrategy<V>| prop_oneof![1 => Just(V::Null), 4 => s].boxed();
    let floats = prop::sample::select(vec![0.0, -0.0, 0.5, -1.25, 3.0, 7.0, 1e308, -1e308, 2.5e-7]);
    (
        (-5i64..5).prop_map(V::I),
        nullable((-20i64..20).prop_map(V::I).boxed()),
        nullable(floats.prop_map(V::F).boxed()),
        (0i64..400 * MICROS_PER_DAY).prop_map(|d| V::T(T0 + d)),
        nullable(
            (-3 * MICROS_PER_DAY..3 * MICROS_PER_DAY)
                .prop_map(|d| V::T(T0 + 30 * MICROS_PER_DAY + d))
                .boxed(),
        ),
        nullable(
            prop::sample::select(vec!["", "A", "B", "b", "Noise", "Ñ"])
                .prop_map(|s| V::S(s.into()))
                .boxed(),
        ),
        nullable(any::<bool>().prop_map(V::Bool).boxed()),
        nullable(
            (40.5f32..40.9, -74.2f32..-73.7)
                .prop_map(|(a, b)| V::G(a, b))
                .boxed(),
        ),
    )
        .prop_map(|(k, x, f, t, u, s, b, g)| vec![k, x, f, t, u, s, b, g])
}

fn num_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        Just(col("k")),
        Just(col("x")),
        Just(col("f")),
        (-3i64..4).prop_map(lit_i64),
        prop::sample::select(vec![0.0, 0.5, -2.0, 1e308]).prop_map(lit_f64),
        Just(Expr::Lat(Box::new(col("g")))),
        Just(col("u").arith(ArithOp::Sub, col("t"))),
    ];
    leaf.prop_recursive(2, 8, 2, |inner| {
        (
            prop::sample::select(ArithOp::ALL.to_vec()),
            inner.clone(),
            inner,
        )
            .prop_map(|(op, a, b)| a.arith(op, b))
    })
}

fn ts_expr() -> impl Strategy<Value = Expr> {
    prop_oneof![
        Just(col("t")),
        Just(col("u")),
        (
            prop::sample::select(TimeUnit::ALL.to_vec()),
            prop::sample::select(vec!["t", "u"])
        )
            .prop_map(|(unit, c)| col(c).date_trunc(unit)),
        (-2i64..3).prop_map(|d| col("t").arith(ArithOp::Add, lit_i64(d * MICROS_PER_DAY))),
    ]
}

fn ts_lit() -> impl Strategy<Value = Expr> {
    (0i64..400).prop_map(|d| lit(Literal::Timestamp(T0 + d * MICROS_PER_DAY)))
}

fn str_lit() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["", "A", "B", "Noise", "Z", "a"]).prop_map(String::from)
}

fn bool_expr() -> impl Strategy<Value = Expr> {
    let cmp = || prop::sample::select(CmpOp::ALL.to_vec());
    let leaf = prop_oneof![
        (cmp(), num_expr(), num_expr()).prop_map(|(op, a, b)| a.cmp(op, b)),
        (cmp(), ts_expr(), prop_oneof![ts_expr(), ts_lit()]).prop_map(|(op, a, b)| a.cmp(op, b)),
        (cmp(), str_lit()).prop_map(|(op, s)| col("s").cmp(op, lit_str(&s))),
        (cmp(), str_lit()).prop_map(|(op, s)| lit_str(&s).cmp(op, col("s"))),
        prop::sample::select(vec!["x", "f", "u", "s", "b", "g", "k"])
            .prop_map(|c| col(c).is_null()),
        prop::collection::vec(str_lit().prop_map(Literal::Str), 1..3)
            .prop_map(|v| col("s").in_list(v)),
        prop::collection::vec((-5i64..5).prop_map(Literal::I64), 1..3)
            .prop_map(|v| col("x").in_list(v)),
        prop::collection::vec(
            prop::sample::select(vec![0.5, -0.0, 3.0]).prop_map(Literal::F64),
            1..3
        )
        .prop_map(|v| col("f").in_list(v)),
        Just(col("b")),
        any::<bool>().prop_map(|b| lit(Literal::Bool(b))),
        Just(col("b").cmp(CmpOp::Lt, lit(Literal::Bool(true)))),
    ];
    leaf.prop_recursive(3, 12, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::And),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::Or),
            inner.prop_map(|e| e.not()),
        ]
    })
}

fn sortable() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["k", "x", "f", "t", "u", "s", "b"])
}

#[derive(Clone, Debug)]
enum Step {
    Filter(Expr),
    MapNum(Expr),
    MapTs(Expr),
    MapBool(Expr),
    Sort(Vec<(&'static str, bool)>),
    Limit(u64),
}

fn arb_step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => bool_expr().prop_map(Step::Filter),
        2 => num_expr().prop_map(Step::MapNum),
        1 => ts_expr().prop_map(Step::MapTs),
        1 => bool_expr().prop_map(Step::MapBool),
        2 => prop::collection::vec((sortable(), any::<bool>()), 1..3).prop_map(Step::Sort),
        1 => (0u64..30).prop_map(Step::Limit),
    ]
}

#[derive(Clone, Debug)]
struct AggStep {
    group_by: Vec<&'static str>,
    aggs: Vec<(AggFunc, Option<&'static str>)>,
    then_sort_desc: Option<bool>,
    then_limit: Option<u64>,
}

fn arb_agg() -> impl Strategy<Value = AggStep> {
    let agg = prop_oneof![
        Just((AggFunc::Count, None)),
        prop::sample::select(vec!["k", "x", "f", "t", "s", "b", "g"])
            .prop_map(|c| (AggFunc::CountNonNull, Some(c))),
        (
            prop::sample::select(vec![AggFunc::Sum, AggFunc::Mean, AggFunc::Median]),
            prop::sample::select(vec!["k", "x", "f"])
        )
            .prop_map(|(f, c)| (f, Some(c))),
        (
            prop::sample::select(vec![AggFunc::Min, AggFunc::Max]),
            sortable()
        )
            .prop_map(|(f, c)| (f, Some(c))),
    ];
    (
        prop::sample::subsequence(vec!["s", "b", "x", "k", "f", "u"], 0..3),
        prop::collection::vec(agg, 0..4),
        prop::option::of(any::<bool>()),
        prop::option::of(0u64..6),
    )
        .prop_map(|(group_by, aggs, then_sort_desc, then_limit)| AggStep {
            group_by,
            aggs,
            then_sort_desc,
            then_limit,
        })
}

fn build_plan(steps: &[Step], agg: &Option<AggStep>) -> Plan {
    let mut p = Plan::scan(SNAP);
    for (i, s) in steps.iter().enumerate() {
        p = match s {
            Step::Filter(e) => p.filter(e.clone()),
            Step::MapNum(e) | Step::MapTs(e) | Step::MapBool(e) => {
                p.map(&format!("m{i}"), e.clone())
            }
            Step::Sort(keys) => p.sort(
                keys.iter()
                    .map(|(c, d)| {
                        if *d {
                            SortKey::desc(c)
                        } else {
                            SortKey::asc(c)
                        }
                    })
                    .collect(),
            ),
            Step::Limit(n) => p.limit(*n),
        };
    }
    if let Some(a) = agg {
        let aggs: Vec<Aggregate> = a
            .aggs
            .iter()
            .enumerate()
            .map(|(i, (f, c))| Aggregate::new(&format!("a{i}"), *f, *c))
            .collect();
        let first_out = a
            .group_by
            .first()
            .map(|s| s.to_string())
            .or_else(|| aggs.first().map(|x| x.name.clone()));
        p = p.aggregate(&a.group_by, aggs);
        if let (Some(desc), Some(c)) = (a.then_sort_desc, first_out) {
            p = p.sort(vec![SortKey {
                column: c,
                descending: desc,
            }]);
        }
        if let Some(n) = a.then_limit {
            p = p.limit(n);
        }
    }
    p
}

proptest! {
    #![proptest_config(ProptestConfig {
        // PROPTEST_CASES overrides, for longer runs.
        cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(2000),
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn engine_matches_reference(
        rows in prop::collection::vec(arb_row(), 0..40),
        steps in prop::collection::vec(arb_step(), 0..4),
        agg in prop::option::of(arb_agg()),
    ) {
        let plan = build_plan(&steps, &agg);
        let src = One(Arc::new(to_engine(&rows)));
        // Some random aggregate steps are invalid (e.g. no groups and no
        // aggregates); those are validation's business, not execution's.
        let Ok(valid) = validate(plan.clone(), &src) else {
            return Ok(());
        };
        let reference = run_reference(
            &plan,
            &RTable { names: schema().fields.iter().map(|f| f.name.clone()).collect(), rows },
        );
        let engine = execute(&valid, &src);
        match (engine, reference) {
            (Ok(e), Ok(r)) => {
                let got = from_engine(e.output());
                prop_assert_eq!(&got.names, &r.names);
                prop_assert_eq!(got.rows, r.rows, "plan: {:#?}", plan);
                // Every intermediate row count is consistent with the table.
                for t in &e.tables {
                    for c in t.columns() {
                        prop_assert_eq!(c.len(), t.len());
                    }
                }
            }
            (Err(e), Err(r)) => prop_assert!(e.message.contains("overflow") && r == "overflow"),
            (e, r) => prop_assert!(false, "engine {:?} vs reference {:?}", e.map(|_| ()), r.map(|_| ())),
        }
    }
}
