//! M1 benchmark: runs representative plans on a real snapshot directory.
//!
//! ```text
//! cargo run --release -p receipts-bench --bin m1 -- snapshots/nyc311/<hash16>
//! ```
//!
//! Timing uses `std::time`: criterion isn't an approved dependency yet.

use anyhow::{Result, bail};
use receipts_bench::{Loaded, load};
use receipts_core::time::parse_naive_timestamp;
use receipts_core::{Column, ColumnData, ContentHash};
use receipts_exec::{Table, execute};
use receipts_plan::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The first `n` rows: the "5M-row prefix" the budgets are stated for.
fn prefix(t: &Table, n: usize) -> Table {
    fn head<T: Clone>(v: &[T], n: usize) -> Vec<T> {
        v[..n].to_vec()
    }
    let cols = t
        .columns()
        .iter()
        .map(|c| {
            let data = match &c.data {
                ColumnData::I64(v) => ColumnData::I64(head(v, n)),
                ColumnData::Timestamp(v) => ColumnData::Timestamp(head(v, n)),
                ColumnData::F64(v) => ColumnData::F64(head(v, n)),
                ColumnData::DictUtf8 { codes, dictionary } => ColumnData::DictUtf8 {
                    codes: head(codes, n),
                    dictionary: dictionary.clone(),
                },
                ColumnData::Geo { lat, lon } => ColumnData::Geo {
                    lat: head(lat, n),
                    lon: head(lon, n),
                },
                ColumnData::Bool(_) => unimplemented!("no bool columns in 311"),
            };
            let validity = c
                .validity
                .as_ref()
                .map(|v| receipts_core::Bitmap::from_bools(v.iter().take(n)));
            Arc::new(Column::new(c.name.clone(), data, validity))
        })
        .collect();
    Table::new(t.schema().clone(), cols).expect("prefix of a valid table")
}

fn ts(s: &str) -> Expr {
    lit(Literal::Timestamp(
        parse_naive_timestamp(s).expect("valid timestamp"),
    ))
}

fn plans(snap: ContentHash) -> Vec<(&'static str, Plan)> {
    let hours = col("closed_date")
        .arith(ArithOp::Sub, col("created_date"))
        .arith(ArithOp::Div, lit_i64(3_600_000_000));
    vec![
        (
            "filter: one complaint type, count",
            Plan::scan(snap)
                .filter(col("complaint_type").eq(lit_str("Noise - Residential")))
                .aggregate(&[], vec![Aggregate::new("n", AggFunc::Count, None)]),
        ),
        (
            "known issue: closed before created, count",
            Plan::scan(snap)
                .filter(col("closed_date").lt(col("created_date")))
                .aggregate(&[], vec![Aggregate::new("n", AggFunc::Count, None)]),
        ),
        (
            "noise: median hours to close by borough",
            Plan::scan(snap)
                .filter(Expr::And(vec![
                    col("complaint_type").in_list(vec![
                        Literal::Str("Noise - Residential".into()),
                        Literal::Str("Noise - Street/Sidewalk".into()),
                    ]),
                    !col("closed_date").is_null(),
                    col("closed_date").ge(col("created_date")),
                ]))
                .map("hours_to_close", hours.clone())
                .aggregate(
                    &["borough"],
                    vec![
                        Aggregate::new("requests", AggFunc::Count, None),
                        Aggregate::new("median_hours", AggFunc::Median, Some("hours_to_close")),
                        Aggregate::new("mean_hours", AggFunc::Mean, Some("hours_to_close")),
                    ],
                )
                .sort(vec![SortKey::desc("median_hours")]),
        ),
        (
            "top complaint types per month (2025)",
            Plan::scan(snap)
                .filter(col("created_date").ge(ts("2025-01-01T00:00:00")))
                .map("month", col("created_date").date_trunc(TimeUnit::Month))
                .aggregate(
                    &["month", "complaint_type"],
                    vec![Aggregate::new("n", AggFunc::Count, None)],
                )
                .sort(vec![SortKey::desc("n")])
                .limit(10),
        ),
        (
            "all rows: map + group by agency x status",
            Plan::scan(snap).map("hours_to_close", hours).aggregate(
                &["agency", "status"],
                vec![
                    Aggregate::new("n", AggFunc::Count, None),
                    Aggregate::new("closed", AggFunc::CountNonNull, Some("hours_to_close")),
                    Aggregate::new("max_hours", AggFunc::Max, Some("hours_to_close")),
                ],
            ),
        ),
        (
            "full sort by closed_date desc, first 10",
            Plan::scan(snap)
                .sort(vec![
                    SortKey::desc("closed_date"),
                    SortKey::asc("unique_key"),
                ])
                .limit(10),
        ),
    ]
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.0} ms", d.as_secs_f64() * 1e3)
}

fn run(label: &str, src: &Loaded, runs: usize, show: bool) -> Result<()> {
    println!("\n## {label}: {} rows\n", src.table.len());
    println!("| Plan | Median | Min | Output rows |");
    println!("|---|---|---|---|");
    for (name, plan) in plans(src.snapshot) {
        let valid = validate(plan, src).map_err(anyhow::Error::msg)?;
        let mut times = Vec::new();
        let mut out = None;
        for _ in 0..runs {
            let t = Instant::now();
            let e = execute(&valid, src).map_err(anyhow::Error::msg)?;
            times.push(t.elapsed());
            out = Some(e);
        }
        times.sort();
        let e = out.expect("runs >= 1");
        println!(
            "| {name} | {} | {} | {} |",
            fmt_ms(times[times.len() / 2]),
            fmt_ms(times[0]),
            e.output().len()
        );
        if show {
            eprintln!("\n### {name}\nplan hash {}", valid.plan_hash().to_hex());
            for (i, s) in describe(&valid).iter().enumerate() {
                eprintln!("  {i}. {s}  [{} rows]", e.tables[i].len());
            }
            print_table(e.output());
        }
    }
    Ok(())
}

fn print_table(t: &Table) {
    let names: Vec<_> = t.schema().fields.iter().map(|f| f.name.as_str()).collect();
    eprintln!("  {}", names.join(" | "));
    for i in 0..t.len().min(12) {
        let cells: Vec<String> = t
            .columns()
            .iter()
            .map(|c| {
                if !c.is_valid(i) {
                    return "∅".into();
                }
                match &c.data {
                    ColumnData::I64(v) => v[i].to_string(),
                    ColumnData::F64(v) => format!("{:.2}", v[i]),
                    ColumnData::Timestamp(v) => receipts_core::time::format_naive_timestamp(v[i]),
                    ColumnData::DictUtf8 { codes, dictionary } => {
                        dictionary[codes[i] as usize].clone()
                    }
                    ColumnData::Bool(b) => b.get(i).to_string(),
                    ColumnData::Geo { lat, lon } => format!("{},{}", lat[i], lon[i]),
                }
            })
            .collect();
        eprintln!("  {}", cells.join(" | "));
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let Some(dir) = args.get(1) else {
        bail!("usage: m1 <snapshot dir> [runs]");
    };
    let runs: usize = args.get(2).map_or(Ok(5), |s| s.parse())?;
    let t = Instant::now();
    let full = load(&PathBuf::from(dir))?;
    println!("Loaded and verified in {}.", fmt_ms(t.elapsed()));
    run("Full snapshot", &full, runs, true)?;
    if full.table.len() > 5_000_000 {
        let p = Loaded {
            table: Arc::new(prefix(&full.table, 5_000_000)),
            ..full.clone()
        };
        run("5M-row prefix", &p, runs, false)?;
    }
    Ok(())
}
