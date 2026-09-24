use proptest::prelude::*;
use receipts_core::canonical_json::to_canonical_json;
use receipts_core::time::parse_naive_timestamp;
use receipts_core::{ColumnType, ContentHash};
use receipts_plan::json::{expr_from_json, expr_to_json, from_json, to_json};
use receipts_plan::*;
use std::ops::Not;

const SNAP: ContentHash = ContentHash([0x3b; 32]);

struct Nyc311;

impl Catalog for Nyc311 {
    fn source(&self, snapshot: &ContentHash) -> Option<SourceInfo> {
        (*snapshot == SNAP).then(|| SourceInfo {
            title: "NYC 311 Service Requests".into(),
            schema: nyc311_schema(),
        })
    }
}

fn nyc311_schema() -> Schema {
    use ColumnType::*;
    Schema::new(vec![
        Field::new("unique_key", I64, false),
        Field::new("created_date", Timestamp, false),
        Field::new("closed_date", Timestamp, true),
        Field::new("agency", DictUtf8, true),
        Field::new("complaint_type", DictUtf8, true),
        Field::new("borough", DictUtf8, true),
        Field::new("location", Geo, true),
    ])
}

fn ts(s: &str) -> Expr {
    lit(Literal::Timestamp(parse_naive_timestamp(s).unwrap()))
}

/// The headline example: median hours to close noise complaints, by borough.
fn noise_plan() -> Plan {
    Plan::scan(SNAP)
        .filter(Expr::And(vec![
            col("complaint_type").in_list(vec![
                Literal::Str("Noise - Residential".into()),
                Literal::Str("Noise - Street/Sidewalk".into()),
            ]),
            col("closed_date").is_null().not(),
            col("closed_date").ge(col("created_date")),
        ]))
        .map(
            "hours_to_close",
            col("closed_date")
                .arith(ArithOp::Sub, col("created_date"))
                .arith(ArithOp::Div, lit_i64(3_600_000_000)),
        )
        .aggregate(
            &["borough"],
            vec![
                Aggregate::new("requests", AggFunc::Count, None),
                Aggregate::new("median_hours", AggFunc::Median, Some("hours_to_close")),
            ],
        )
        .sort(vec![SortKey::desc("median_hours")])
        .limit(10)
}

#[test]
fn validates_and_infers_schemas() {
    let v = validate(noise_plan(), &Nyc311).unwrap();
    let out = v.schema(v.output());
    let names: Vec<_> = out.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["borough", "requests", "median_hours"]);
    assert_eq!(
        out.fields[1],
        Field::new("requests", ColumnType::I64, false)
    );
    assert_eq!(
        out.fields[2],
        Field::new("median_hours", ColumnType::F64, true)
    );
    let mapped = v.schema(NodeId(2));
    assert_eq!(
        mapped.field("hours_to_close"),
        Some(&Field::new("hours_to_close", ColumnType::F64, true))
    );
}

#[test]
fn describes_every_step_in_plain_english() {
    let v = validate(noise_plan(), &Nyc311).unwrap();
    let got = describe(&v);
    let want = [
        "Start from NYC 311 Service Requests (snapshot 3b3b3b3b3b3b3b3b).",
        "Keep only rows where complaint_type is one of \"Noise - Residential\", \"Noise - Street/Sidewalk\" \
         and closed_date is present and closed_date is on or after created_date. \
         Rows where this can't be decided because a value is missing are dropped.",
        "Add a column hours_to_close: (closed_date − created_date) ÷ 3600000000. \
         Dividing by zero gives a missing value.",
        "Group the rows by borough and, for each group, compute the number of rows (as requests) \
         and the median of hours_to_close (as median_hours). \
         Missing values are left out of these calculations. \
         Rows with a missing group value form their own group.",
        "Sort by median_hours (largest first). Missing values count as the smallest.",
        "Keep only the first 10 rows.",
    ];
    assert_eq!(got, want);
}

#[test]
fn renders_times_and_negation() {
    let s = nyc311_schema();
    let e = Expr::Or(vec![
        col("created_date").lt(ts("2024-07-01T00:00:00")),
        col("closed_date").cmp(CmpOp::Gt, ts("2025-01-01T08:30:00")),
    ])
    .not();
    assert_eq!(
        e.render(&s),
        "not (created_date is before 2024-07-01 or closed_date is after 2025-01-01 08:30:00)"
    );
    assert_eq!(
        col("created_date").date_trunc(TimeUnit::Week).render(&s),
        "the week (starting Monday) of created_date"
    );
}

fn err(plan: Plan) -> String {
    validate(plan, &Nyc311).unwrap_err().to_string()
}

#[test]
fn explains_invalid_plans() {
    let scan = || Plan::scan(SNAP);
    assert_eq!(
        err(scan().filter(col("borugh").eq(lit_str("BRONX")))),
        "step 1: there is no column named \"borugh\""
    );
    assert_eq!(
        err(scan().filter(col("borough").eq(lit_i64(1)))),
        "step 1: can't compare text with integer (eq)"
    );
    assert_eq!(
        err(scan().filter(col("borough").eq(lit(Literal::Null)))),
        "step 1: a bare null has no type here; use is_null to test for missing values"
    );
    assert_eq!(
        err(scan().filter(col("unique_key"))),
        "step 1: the filter condition must be true/false, not integer"
    );
    assert_eq!(
        err(scan().map("borough", lit_i64(1))),
        "step 1: there is already a column named \"borough\"; choose another name"
    );
    assert_eq!(
        err(scan().aggregate(
            &["borough"],
            vec![Aggregate::new("m", AggFunc::Mean, Some("borough"))]
        )),
        "step 1: can't take the mean of \"borough\", a text column"
    );
    assert_eq!(
        err(scan().aggregate(
            &[],
            vec![Aggregate::new("n", AggFunc::Count, Some("borough"))]
        )),
        "step 1: count counts rows and takes no column; use count_non_null"
    );
    assert_eq!(
        err(scan().aggregate(&["location"], vec![])),
        "step 1: can't group by the location column \"location\""
    );
    assert_eq!(
        err(scan().aggregate(
            &["borough"],
            vec![Aggregate::new("borough", AggFunc::Count, None)]
        )),
        "step 1: the column name \"borough\" is used twice"
    );
    assert_eq!(
        err(scan().map(
            "x",
            col("created_date").arith(ArithOp::Add, col("closed_date"))
        )),
        "step 1: can't add timestamp and timestamp"
    );
    assert_eq!(
        err(scan().filter(col("unique_key").ge(lit_f64(f64::NAN)))),
        "step 1: NaN is not a usable number"
    );
    assert_eq!(
        err(Plan::scan(ContentHash([1; 32]))),
        format!("step 0: snapshot {} is not loaded", "01".repeat(32))
    );

    let mut dangling = scan().limit(5);
    dangling.output = NodeId(0);
    assert_eq!(
        err(dangling),
        "step 1: this step doesn't lead to the output"
    );
    let backwards = Plan {
        nodes: vec![Op::Limit {
            input: NodeId(0),
            count: 1,
        }],
        output: NodeId(0),
    };
    assert_eq!(
        err(backwards),
        "step 0: reads step 0, which doesn't come before it"
    );
}

#[test]
fn json_round_trips_and_is_canonical() {
    let plan = noise_plan();
    let json = to_json(&plan);
    assert_eq!(from_json(&json).unwrap(), plan);
    let canonical = to_canonical_json(&json).unwrap();
    let reparsed: serde_json::Value = serde_json::from_str(&canonical).unwrap();
    assert_eq!(from_json(&reparsed).unwrap(), plan);
}

#[test]
fn json_parsing_is_strict() {
    let mut json = to_json(&noise_plan());
    json["nodes"][5]["extra"] = serde_json::json!(1);
    assert_eq!(
        from_json(&json).unwrap_err().to_string(),
        "step 5: unexpected key \"extra\""
    );
    let bad = serde_json::json!({ "call": "eq", "args": [{ "column": "a" }] });
    assert_eq!(
        expr_from_json(&bad).unwrap_err(),
        "eq takes 2 argument(s), got 1"
    );
    let bad = serde_json::json!({ "literal": { "i64": "01" } });
    assert_eq!(
        expr_from_json(&bad).unwrap_err(),
        "\"01\" is not an integer"
    );
    let bad = serde_json::json!({ "format": "receipts-plan/2", "nodes": [], "output": 0 });
    assert_eq!(
        from_json(&bad).unwrap_err().to_string(),
        "unsupported plan format \"receipts-plan/2\""
    );
}

#[test]
fn plan_hash_is_pinned() {
    // Pins the v1 encoding: changing the JSON form, the canonicalization or
    // the hash context changes this value, and every stored receipt with it.
    let v = validate(noise_plan(), &Nyc311).unwrap();
    assert_eq!(
        v.plan_hash().to_hex(),
        "c1c9afe69b2aeea89601bb39355c6104823f44a7e5d59fa6eb782fa9f86d4fd6"
    );
}

#[test]
fn every_detail_changes_the_hash() {
    let hash = |p: Plan| validate(p, &Nyc311).unwrap().plan_hash();
    let base = hash(noise_plan());
    let variants = [
        noise_plan().limit(10),
        {
            let mut p = noise_plan();
            p.nodes[5] = Op::Limit {
                input: NodeId(4),
                count: 11,
            };
            p
        },
        {
            let mut p = noise_plan();
            p.nodes[4] = Op::Sort {
                input: NodeId(3),
                keys: vec![SortKey::asc("median_hours")],
            };
            p
        },
        {
            // Integer 1 and decimal 1.0 are different literals.
            let mut p = noise_plan();
            p.nodes[2] = Op::Map {
                input: NodeId(1),
                name: "hours_to_close".into(),
                expr: col("closed_date")
                    .arith(ArithOp::Sub, col("created_date"))
                    .arith(ArithOp::Div, lit_f64(3_600_000_000.0)),
            };
            p
        },
    ];
    for v in variants {
        assert_ne!(hash(v), base);
    }
    // Node hashes are prefixes: the shared first steps hash the same.
    let a = validate(noise_plan(), &Nyc311).unwrap();
    let b = validate(noise_plan().limit(3), &Nyc311).unwrap();
    for i in 0..6 {
        assert_eq!(a.node_hash(NodeId(i)), b.node_hash(NodeId(i)));
    }
}

fn arb_literal() -> impl Strategy<Value = Literal> {
    prop_oneof![
        Just(Literal::Null),
        any::<bool>().prop_map(Literal::Bool),
        any::<i64>().prop_map(Literal::I64),
        any::<f64>()
            .prop_filter("finite", |x| x.is_finite())
            .prop_map(|x| Literal::F64(if x == 0.0 { 0.0 } else { x })),
        "\\PC{0,8}".prop_map(Literal::Str),
        (-62_135_596_800_000_000i64..253_402_300_799_000_000).prop_map(Literal::Timestamp),
    ]
}

fn arb_expr() -> impl Strategy<Value = Expr> {
    let leaf = prop_oneof![
        "[a-z_]{1,6}".prop_map(Expr::Column),
        arb_literal().prop_map(Expr::Literal),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            (
                prop::sample::select(CmpOp::ALL.to_vec()),
                inner.clone(),
                inner.clone()
            )
                .prop_map(|(op, a, b)| Expr::Compare(op, Box::new(a), Box::new(b))),
            (
                prop::sample::select(ArithOp::ALL.to_vec()),
                inner.clone(),
                inner.clone()
            )
                .prop_map(|(op, a, b)| Expr::Arith(op, Box::new(a), Box::new(b))),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::And),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Expr::Or),
            inner.clone().prop_map(|a| Expr::Not(Box::new(a))),
            inner.clone().prop_map(|a| Expr::IsNull(Box::new(a))),
            inner.clone().prop_map(|a| Expr::Lat(Box::new(a))),
            (inner.clone(), prop::collection::vec(arb_literal(), 1..4))
                .prop_map(|(a, v)| Expr::InList(Box::new(a), v)),
            (prop::sample::select(TimeUnit::ALL.to_vec()), inner)
                .prop_map(|(u, a)| Expr::DateTrunc(u, Box::new(a))),
        ]
    })
}

proptest! {
    #[test]
    fn any_expr_round_trips_through_canonical_json(e in arb_expr()) {
        let canonical = to_canonical_json(&expr_to_json(&e)).unwrap();
        let back = expr_from_json(&serde_json::from_str(&canonical).unwrap()).unwrap();
        prop_assert_eq!(back, e);
    }
}
