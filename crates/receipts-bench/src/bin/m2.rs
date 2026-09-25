//! M2 benchmark: lineage capture and tracing on a real snapshot.
//!
//! ```text
//! cargo run --release -p receipts-bench --bin m2 -- snapshots/nyc311/<hash16>
//! ```
//!
//! Besides timings, it checks lineage against the data itself: every source
//! row behind a group must satisfy the plan's filter and carry the group's
//! key, and the number of rows traced must equal the group's count.

use anyhow::{Result, bail, ensure};
use receipts_bench::load;
use receipts_core::{Column, ColumnData};
use receipts_exec::execute;
use receipts_plan::*;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn noise_plan(snap: receipts_core::ContentHash) -> Plan {
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

fn text(c: &Column, i: usize) -> Option<&str> {
    match &c.data {
        ColumnData::DictUtf8 { codes, dictionary } => c
            .is_valid(i)
            .then(|| dictionary[codes[i] as usize].as_str()),
        _ => None,
    }
}

fn ts(c: &Column, i: usize) -> Option<i64> {
    match &c.data {
        ColumnData::Timestamp(v) => c.is_valid(i).then(|| v[i]),
        _ => None,
    }
}

fn main() -> Result<()> {
    let Some(dir) = std::env::args().nth(1) else {
        bail!("usage: m2 <snapshot dir>");
    };
    let src = load(&PathBuf::from(dir))?;
    let valid = validate(noise_plan(src.snapshot), &src).map_err(anyhow::Error::msg)?;

    let mut times = Vec::new();
    let mut exec = None;
    for _ in 0..5 {
        let t = Instant::now();
        let e = execute(&valid, &src).map_err(anyhow::Error::msg)?;
        times.push(t.elapsed());
        exec = Some(e);
    }
    times.sort();
    let e = exec.expect("ran");
    let lineage = &e.lineage;
    println!("## Noise plan with lineage capture\n");
    println!("- execute (median of 5): {}", ms(times[2]));
    println!(
        "- lineage held: {:.1} MB across {} steps",
        lineage.heap_bytes() as f64 / 1e6,
        lineage.len()
    );
    for (i, t) in e.tables.iter().enumerate() {
        println!(
            "  - step {i}: {} rows, {:.1} MB of mapping",
            t.len(),
            lineage.step(i).heap_bytes() as f64 / 1e6
        );
    }

    // Backward: every output row (borough).
    let out = e.output();
    let out_node = e.output.index();
    let borough = out.column("borough").expect("borough");
    let requests = out.column("requests").expect("requests");
    let ColumnData::I64(requests) = &requests.data else {
        bail!("requests is an integer column")
    };
    let source = src.table.as_ref();
    let s_borough = source.column("borough").expect("col");
    let s_type = source.column("complaint_type").expect("col");
    let s_created = source.column("created_date").expect("col");
    let s_closed = source.column("closed_date").expect("col");
    println!("\n## Backward traces (one output row each)\n");
    println!("| Borough | Source rows | Cold | Cached |");
    println!("|---|---|---|---|");
    for (o, &count) in requests.iter().enumerate() {
        let t = Instant::now();
        let trace = lineage.backward_row(out_node, o as u32);
        let cold = t.elapsed();
        let t = Instant::now();
        let again = lineage.backward_row(out_node, o as u32);
        let cached = t.elapsed();
        ensure!(std::sync::Arc::ptr_eq(&trace, &again), "cache miss");
        let name = text(borough, o).unwrap_or("(missing)");
        let rows = trace.source_rows();
        ensure!(
            rows.len() as i64 == count,
            "{name}: traced {} rows, count says {}",
            rows.len(),
            count
        );
        for &r in rows {
            let r = r as usize;
            ensure!(
                text(s_borough, r) == text(borough, o),
                "{name}: row {r} has another borough"
            );
            ensure!(
                matches!(
                    text(s_type, r),
                    Some("Noise - Residential" | "Noise - Street/Sidewalk")
                ),
                "{name}: row {r} isn't a noise complaint"
            );
            ensure!(
                ts(s_closed, r).is_some_and(|c| c >= ts(s_created, r).expect("not null")),
                "{name}: row {r} fails the closed >= created filter"
            );
        }
        println!(
            "| {name} | {} | {} | {} |",
            rows.len(),
            ms(cold),
            ms(cached)
        );
    }
    println!("\nEvery traced row passed the filter and has its group's borough.");

    // Forward: one Bronx source row, and every source row at once.
    let bronx = lineage.backward_row(out_node, 0);
    let row = bronx.source_rows()[bronx.source_rows().len() / 2];
    println!("\n## Forward traces\n");
    for label in ["first call (builds inverse maps)", "second call"] {
        let t = Instant::now();
        let hit = lineage.forward(0, &[row], out_node);
        println!(
            "- one source row, {label}: {} → output rows {hit:?}",
            ms(t.elapsed())
        );
        ensure!(hit == vec![0], "forward trace should reach the Bronx row");
    }
    let all: Vec<u32> = (0..source.len() as u32).collect();
    let t = Instant::now();
    let hit = lineage.forward(0, &all, out_node);
    println!(
        "- all {} source rows: {} → {} output rows",
        all.len(),
        ms(t.elapsed()),
        hit.len()
    );
    let t = Instant::now();
    let everything = lineage.backward(out_node, &(0..out.len() as u32).collect::<Vec<_>>());
    println!(
        "- backward from all output rows: {} → {} source rows",
        ms(t.elapsed()),
        everything.source_rows().len()
    );
    let first = everything.row_ids().next().expect("some row");
    println!("- first RowId: {first:?}");
    Ok(())
}
