//! Native front end to the same `Engine` the browser uses, for parity tests
//! and timing:
//!
//! ```text
//! receipts-engine <snapshot dir> <plan.json> [row-to-trace]
//! ```
//!
//! Prints one JSON object: the load report, the run, and a trace.

use receipts_wasm::Engine;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    let (Some(dir), Some(plan)) = (args.get(1), args.get(2)) else {
        return Err("usage: receipts-engine <snapshot dir> <plan.json> [row]".into());
    };
    let dir = PathBuf::from(dir);
    let read = |f: &str| fs::read(dir.join(f)).map_err(|e| format!("{f}: {e}"));
    let manifest = String::from_utf8(read("manifest.json")?).map_err(|e| e.to_string())?;
    let (data, log, rejects) = (
        read("data.arrow")?,
        read("cleaning_log.arrow")?,
        read("rejects.arrow")?,
    );
    let mut engine = Engine::new();
    let t = Instant::now();
    let load = engine.load_snapshot(&manifest, &data, &log, &rejects)?;
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    let plan = fs::read_to_string(plan).map_err(|e| e.to_string())?;
    let plan = plan.replace("$SNAPSHOT", load["snapshot_hash"].as_str().unwrap_or(""));
    let run = engine.run(&plan);
    let mut out = json!({ "load": load, "load_ms": load_ms, "run": run });
    if let (Some(row), Some(id)) = (args.get(3), run["execution"].as_u64()) {
        let row: u32 = row.parse().map_err(|_| "row must be a number")?;
        let (summary, rows) = engine.trace_back(id as u32, row)?;
        out["trace"] = summary;
        out["trace_first_rows"] = json!(rows.iter().take(5).collect::<Vec<_>>());
    }
    // Timings differ between runs and builds; parity checks strip them.
    println!("{out}");
    Ok(())
}
