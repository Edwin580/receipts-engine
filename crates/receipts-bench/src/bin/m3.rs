//! M3 benchmark: counterfactuals on a real snapshot.
//!
//! ```text
//! cargo run --release -p receipts-bench --bin m3 -- snapshots/nyc311/<hash16>
//! ```
//!
//! Each exclusion runs twice, incrementally (`receipts_cf::exclude`) and as
//! a full re-execution, and the outputs must be identical.

use anyhow::{Result, bail, ensure};
use receipts_bench::load;
use receipts_cf::{CfValue, Method, contributions, exclude};
use receipts_core::time::MICROS_PER_DAY;
use receipts_core::{ColumnData, ContentHash};
use receipts_exec::{Exclusion, Table, execute, execute_excluding};
use receipts_plan::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn noise_plan(snap: ContentHash) -> Plan {
    Plan::scan(snap)
        .filter(Expr::And(vec![
            col("complaint_type").in_list(vec![
                Literal::Str("Noise - Residential".into()),
                Literal::Str("Noise - Street/Sidewalk".into()),
            ]),
            !col("closed_date").is_null(),
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
                Aggregate::new("mean_hours", AggFunc::Mean, Some("hours_to_close")),
            ],
        )
        .sort(vec![SortKey::desc("median_hours")])
}

fn ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1e3;
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1e3)
    } else {
        format!("{ms:.1} ms")
    }
}

fn render(t: &Table) -> Vec<String> {
    (0..t.len())
        .map(|i| {
            t.columns()
                .iter()
                .map(|c| {
                    if !c.is_valid(i) {
                        return "∅".to_string();
                    }
                    match &c.data {
                        ColumnData::I64(v) => v[i].to_string(),
                        ColumnData::F64(v) => format!("{:.4}", v[i]),
                        ColumnData::DictUtf8 { codes, dictionary } => {
                            dictionary[codes[i] as usize].clone()
                        }
                        _ => "?".into(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect()
}

fn bits(t: &Table) -> Vec<Vec<u64>> {
    (0..t.len())
        .map(|i| {
            t.columns()
                .iter()
                .map(|c| match &c.data {
                    _ if !c.is_valid(i) => u64::MAX,
                    ColumnData::I64(v) => v[i] as u64,
                    ColumnData::F64(v) => v[i].to_bits(),
                    ColumnData::DictUtf8 { codes, .. } => codes[i] as u64,
                    _ => 0,
                })
                .collect()
        })
        .collect()
}

/// Deterministic pseudo-random sample (splitmix64), so runs are repeatable.
fn sample(n: u32, k: usize, seed: u64) -> Vec<u32> {
    let mut s = seed;
    (0..k)
        .map(|_| {
            s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = s;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) % n as u64) as u32
        })
        .collect()
}

fn main() -> Result<()> {
    let Some(dir) = std::env::args().nth(1) else {
        bail!("usage: m3 <snapshot dir>");
    };
    let src = load(&PathBuf::from(dir))?;
    let valid = validate(noise_plan(src.snapshot), &src).map_err(anyhow::Error::msg)?;
    let t = Instant::now();
    let base = execute(&valid, &src).map_err(anyhow::Error::msg)?;
    println!("Base execution: {}\n", ms(t.elapsed()));
    for line in render(base.output()) {
        println!("    {line}");
    }

    let n = src.table.len() as u32;
    let created = src.table.column("created_date").expect("col");
    let ColumnData::Timestamp(created) = &created.data else {
        bail!("created_date is a timestamp")
    };
    let midnight: Vec<u32> = (0..n)
        .filter(|&i| created[i as usize].rem_euclid(MICROS_PER_DAY) == 0)
        .collect();
    let bronx_trace = base.lineage.backward_row(base.output.index(), 0);
    let bronx_rows = bronx_trace.source_rows();
    let unspecified = {
        let b = base.output().column("borough").expect("col");
        let ColumnData::DictUtf8 { codes, dictionary } = &b.data else {
            bail!("text")
        };
        let row = (0..base.output().len())
            .find(|&i| dictionary[codes[i] as usize] == "Unspecified")
            .expect("an Unspecified group");
        base.lineage
            .backward_row(base.output.index(), row as u32)
            .source_rows()
            .to_vec()
    };
    let cases: Vec<(&str, Vec<u32>)> = vec![
        (
            "one Bronx noise complaint",
            vec![bronx_rows[bronx_rows.len() / 3]],
        ),
        ("one row the plan filters out", vec![0]),
        ("1,000 random rows", sample(n, 1000, 7)),
        (
            "every row created at exactly midnight (KI-created-midnight)",
            midnight.clone(),
        ),
        (
            "all 54 'Unspecified' noise rows (group disappears)",
            unspecified,
        ),
        ("100,000 random rows", sample(n, 100_000, 11)),
    ];

    println!("\n| Exclusion | Rows | Method | Incremental | Full re-run | Identical |");
    println!("|---|---|---|---|---|---|");
    for (label, rows) in &cases {
        let t = Instant::now();
        let cf = exclude(&valid, &base, &src, src.snapshot, rows).map_err(anyhow::Error::msg)?;
        let inc = t.elapsed();
        let t = Instant::now();
        let full = execute_excluding(
            &valid,
            &src,
            Exclusion {
                snapshot: src.snapshot,
                rows,
            },
        )
        .map_err(anyhow::Error::msg)?;
        let rerun = t.elapsed();
        let same = bits(cf.execution.output()) == bits(full.output());
        ensure!(same, "{label}: incremental and full results differ");
        let method = match cf.method {
            Method::Unchanged => "unchanged".to_string(),
            Method::Incremental {
                groups_recomputed, ..
            } => format!("incremental ({groups_recomputed} groups)"),
            Method::Rerun => "re-run".into(),
        };
        println!(
            "| {label} | {} | {method} | {} | {} | yes |",
            rows.len(),
            ms(inc),
            ms(rerun)
        );
        if label.starts_with("every row created at exactly midnight") {
            println!("\nWithout the {} midnight rows:\n", rows.len());
            for line in render(cf.execution.output()) {
                println!("    {line}");
            }
            println!();
        }
    }

    println!("\n## Contributions: Bronx median hours (leave-one-out)\n");
    let t = Instant::now();
    let c = contributions(&valid, &base, 0, "median_hours").map_err(anyhow::Error::msg)?;
    let took = t.elapsed();
    let CfValue::F64(current) = c.current else {
        bail!("median is a decimal")
    };
    let mut deltas: Vec<f64> = c
        .rows
        .iter()
        .map(|r| match r.without {
            CfValue::F64(v) => v - current,
            _ => 0.0,
        })
        .collect();
    deltas.sort_by(|a: &f64, b: &f64| a.partial_cmp(b).expect("finite"));
    let changed = deltas.iter().filter(|d| **d != 0.0).count();
    println!(
        "- {} contributing rows in {} (exact: {})",
        c.rows.len(),
        ms(took),
        c.exact
    );
    println!(
        "- current median {current:.6} h; removing one row moves it by {:+.6} to {:+.6} h; {changed} rows move it at all",
        deltas[0],
        deltas[deltas.len() - 1]
    );
    // Spot-check three rows against real exclusions.
    for &i in &[0usize, c.rows.len() / 2, c.rows.len() - 1] {
        let r = c.rows[i];
        let cf = exclude(&valid, &base, &src, src.snapshot, &[r.source_row])
            .map_err(anyhow::Error::msg)?;
        let agg = cf.execution.table(NodeId(3));
        let median = agg.column("median_hours").expect("col");
        let ColumnData::F64(v) = &median.data else {
            bail!("decimal")
        };
        ensure!(
            r.without == CfValue::F64(v[c.group as usize]),
            "row {} disagrees with a real exclusion",
            r.source_row
        );
    }
    println!("- spot-checked 3 rows against real exclusions: identical");
    let t = Instant::now();
    let c = contributions(&valid, &base, 0, "mean_hours").map_err(anyhow::Error::msg)?;
    println!(
        "- mean_hours contributions for the same group: {} (exact: {}, decimal sums use sum − x)",
        ms(t.elapsed()),
        c.exact
    );
    Ok(())
}
