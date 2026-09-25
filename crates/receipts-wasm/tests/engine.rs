//! The JS-facing engine, exercised natively: verify-on-load (including
//! tampering), plan validation, runs, traces and counterfactuals.

use receipts_snapshot::manifest::{CLEANING_LOG_FILE, DATA_FILE, MANIFEST_FILE, REJECTS_FILE};
use receipts_snapshot::raw::Scope;
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::{assemble, synth};
use receipts_wasm::Engine;
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

struct Files {
    manifest: String,
    data: Vec<u8>,
    log: Vec<u8>,
    rejects: Vec<u8>,
}

/// A 70k-row synthetic snapshot (two chunks, every cleaning rule firing).
fn files() -> &'static Files {
    static F: OnceLock<Files> = OnceLock::new();
    F.get_or_init(|| {
        let base = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("wasm-engine-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let scope = Scope::from_dates("created_date", "2024-01-01", "2026-01-01").unwrap();
        let records = synth::generate_311(70_000, 21, &scope);
        synth::write_raw(
            &base.join("raw"),
            &NYC_311,
            &scope,
            &synth::metadata_for(&NYC_311, 1_768_435_200),
            synth::paginate(&records, 10_000),
        )
        .unwrap();
        let built = assemble::build(&NYC_311, &base.join("raw"), &base.join("snap")).unwrap();
        let d = built.dir;
        Files {
            manifest: fs::read_to_string(d.join(MANIFEST_FILE)).unwrap(),
            data: fs::read(d.join(DATA_FILE)).unwrap(),
            log: fs::read(d.join(CLEANING_LOG_FILE)).unwrap(),
            rejects: fs::read(d.join(REJECTS_FILE)).unwrap(),
        }
    })
}

fn loaded() -> (Engine, String) {
    let f = files();
    let mut e = Engine::new();
    let report = e
        .load_snapshot(&f.manifest, &f.data, &f.log, &f.rejects)
        .unwrap();
    let hash = report["snapshot_hash"].as_str().unwrap().to_string();
    (e, hash)
}

fn noise_plan(snapshot: &str) -> String {
    json!({
        "format": "receipts-plan/1",
        "nodes": [
            {"op": "scan", "snapshot": snapshot},
            {"op": "filter", "input": 0, "predicate": {"call": "and", "args": [
                {"call": "eq", "args": [{"column": "complaint_type"}, {"literal": {"str": "Noise - Residential"}}]},
                {"call": "ge", "args": [{"column": "closed_date"}, {"column": "created_date"}]}
            ]}},
            {"op": "map", "input": 1, "name": "hours", "expr": {"call": "div", "args": [
                {"call": "sub", "args": [{"column": "closed_date"}, {"column": "created_date"}]},
                {"literal": {"i64": "3600000000"}}
            ]}},
            {"op": "aggregate", "input": 2, "group_by": ["borough"], "aggregates": [
                {"name": "n", "fn": "count"},
                {"name": "median_hours", "fn": "median", "column": "hours"}
            ]},
            {"op": "sort", "input": 3, "keys": [{"column": "n", "order": "desc"}]}
        ],
        "output": 4
    })
    .to_string()
}

#[test]
fn loads_and_reports() {
    let f = files();
    let (_, hash) = loaded();
    let m: Value = serde_json::from_str(&f.manifest).unwrap();
    assert_eq!(hash, m["snapshot_hash"].as_str().unwrap());
}

#[test]
fn rejects_tampered_files() {
    let f = files();
    let load = |manifest: &str, data: &[u8], log: &[u8], rejects: &[u8]| {
        Engine::new()
            .load_snapshot(manifest, data, log, rejects)
            .unwrap_err()
    };
    // One byte of the data body (well past the schema, inside a column).
    let mut data = f.data.clone();
    let at = data.len() / 2;
    data[at] ^= 0x01;
    let err = load(&f.manifest, &data, &f.log, &f.rejects);
    assert!(
        err.contains("hash") || err.contains("data.arrow") || err.contains("order"),
        "{err}"
    );
    // An edited manifest.
    let edited = f.manifest.replacen("\"row_count\"", "\"row_count_\"", 1);
    assert!(load(&edited, &f.data, &f.log, &f.rejects).contains("manifest_hash"));
    // Swapped side tables.
    assert!(load(&f.manifest, &f.data, &f.rejects, &f.log).contains("cleaning_log"));
    // Truncated data.
    assert!(
        load(&f.manifest, &f.data[..f.data.len() - 7], &f.log, &f.rejects).contains("data.arrow")
    );
}

#[test]
fn validates_with_sentences_and_errors() {
    let (e, hash) = loaded();
    let v = e.validate_plan(&noise_plan(&hash));
    assert_eq!(v["ok"], true);
    assert_eq!(v["steps"].as_array().unwrap().len(), 5);
    assert!(
        v["steps"][1]["sentence"]
            .as_str()
            .unwrap()
            .starts_with("Keep only rows where complaint_type is \"Noise - Residential\"")
    );
    let bad = noise_plan(&hash).replace("\"borough\"]", "\"borugh\"]");
    let v = e.validate_plan(&bad);
    assert_eq!(v["ok"], false);
    assert_eq!(v["step"], 3);
    assert_eq!(v["error"], "there is no column named \"borugh\"");
    assert_eq!(e.validate_plan("{")["ok"], false);
}

#[test]
fn runs_traces_and_excludes() {
    let (mut e, hash) = loaded();
    let r = e.run(&noise_plan(&hash));
    assert_eq!(r["ok"], true, "{r}");
    let id = r["execution"].as_u64().unwrap() as u32;
    let rows = r["output"]["rows"].as_array().unwrap();
    assert!(!rows.is_empty());
    // Trace of each group has exactly n rows.
    for (i, row) in rows.iter().enumerate() {
        let (summary, source) = e.trace_back(id, i as u32).unwrap();
        assert_eq!(source.len() as u64, row[1].as_u64().unwrap());
        assert_eq!(summary["path"].as_array().unwrap().len(), 5);
    }
    let (_, first_group) = e.trace_back(id, 0).unwrap();
    let recs = e.source_records(&hash, &first_group[..3]).unwrap();
    assert_eq!(recs["rows"].as_array().unwrap().len(), 3);

    // Excluding two rows of the first group lowers its count by two.
    let a = e.exclude(id, &first_group[..1]).unwrap();
    assert_eq!(a["method"]["kind"], "incremental");
    let a_id = a["execution"].as_u64().unwrap() as u32;
    let b = e.exclude(a_id, &first_group[1..2]).unwrap();
    let both = e.exclude(id, &first_group[..2]).unwrap();
    assert_eq!(b["output"], both["output"]);
    assert_eq!(b["excluded"], 2);
    let n0 = rows[0][1].as_u64().unwrap();
    let group = rows[0][0].clone();
    let after = both["output"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r[0] == group)
        .unwrap();
    assert_eq!(after[1].as_u64().unwrap(), n0 - 2);

    // Contributions: the top movers are real what-ifs.
    let c = e.contributions(id, 0, "median_hours", 3).unwrap();
    assert_eq!(c["contributing_rows"].as_u64().unwrap(), n0);
    assert_eq!(c["exact"], true);
    assert!(e.contributions(id, 0, "borough", 3).is_err());

    // Rows behind a filter plan, e.g. to exclude them in a what-if.
    let midnight = json!({"format": "receipts-plan/1", "nodes": [
        {"op": "scan", "snapshot": hash},
        {"op": "filter", "input": 0, "predicate": {"call": "eq", "args": [
            {"column": "created_date"},
            {"call": "date_trunc", "unit": "day", "args": [{"column": "created_date"}]}]}}
    ], "output": 1})
    .to_string();
    let rows_at_midnight = e.matching_rows(&midnight).unwrap();
    assert!(!rows_at_midnight.is_empty());
    assert!(rows_at_midnight.windows(2).all(|w| w[0] < w[1]));
    assert!(e.matching_rows("{}").is_err());

    // Dropping the base breaks derived exclusions with a clear message.
    assert!(e.drop_execution(id));
    assert!(e.exclude(a_id, &[0]).unwrap_err().contains("dropped"));
}
