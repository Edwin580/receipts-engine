#![allow(dead_code)]

use receipts_snapshot::raw::Scope;
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::synth;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

pub fn scratch(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "{name}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

pub fn scope() -> Scope {
    Scope::from_dates("created_date", "2024-01-01", "2026-01-01").unwrap()
}

pub fn metadata() -> Value {
    synth::metadata_for(&NYC_311, 1_768_435_200)
}

/// A record with sensible defaults; `extra` overrides or adds fields, and a
/// JSON null in `extra` removes the field (Socrata omits nulls).
pub fn rec(id: &str, key: Option<&str>, created: &str, extra: Value) -> Value {
    let mut r = json!({
        ":id": id,
        "created_date": created,
        "agency": "NYPD",
        "complaint_type": "Noise - Residential",
        "descriptor": "Loud Music/Party",
        "borough": "BROOKLYN",
        "status": "Closed",
    });
    let m = r.as_object_mut().unwrap();
    if let Some(k) = key {
        m.insert("unique_key".into(), json!(k));
    }
    for (k, v) in extra.as_object().unwrap() {
        if v.is_null() {
            m.remove(k);
        } else {
            m.insert(k.clone(), v.clone());
        }
    }
    r
}

/// Covers every cleaning rule and known issue. See `fixture_outcome` in
/// pipeline.rs for the expected result.
pub fn fixture() -> Vec<Value> {
    vec![
        rec(
            "row-01",
            Some("100"),
            "2024-03-01T10:00:00.000",
            json!({"closed_date": "2024-03-02T10:00:00.000", "latitude": "40.7", "longitude": "-73.9", "incident_zip": "11201"}),
        ),
        rec(
            "row-02",
            Some("101"),
            "2024-02-01T08:00:00.000",
            json!({"agency": "  HPD ", "descriptor": "", "closed_date": "yesterday", "latitude": "40.6"}),
        ),
        rec("row-03", None, "2024-03-01T10:00:00.000", json!({})),
        rec("row-04", Some("0102"), "2024-03-01T10:00:00.000", json!({})),
        rec("row-05", Some("103"), "garbage", json!({})),
        rec("row-06", Some("104"), "2023-12-31T23:59:59.000", json!({})),
        rec("row-07", Some("105"), "2024-07-01T12:00:00.000", json!({})),
        rec("row-08", Some("105"), "2024-07-01T12:00:00.000", json!({})),
        rec(
            "row-09",
            Some("106"),
            "2024-08-01T12:00:00.000",
            json!({"status": "Open"}),
        ),
        rec(
            "row-10",
            Some("106"),
            "2024-08-01T12:00:00.000",
            json!({"status": "Closed"}),
        ),
        rec("row-11", Some("107"), "not-a-date", json!({})),
        rec("row-12", Some("107"), "2024-09-01T12:00:00.000", json!({})),
        rec(
            "row-13",
            Some("108"),
            "2025-12-31T23:59:59.999",
            json!({"closed_date": "2025-01-01T00:00:00.000", "latitude": "0", "longitude": "0", "borough": "Unspecified", "incident_zip": "N/A"}),
        ),
        rec(
            "row-14",
            Some("109"),
            "2024-02-01T08:00:00.000",
            json!({"agency": "DOT"}),
        ),
        rec(
            "row-15",
            None,
            "2024-05-05T00:00:00.000",
            json!({"unique_key": 110, "latitude": 40.75, "longitude": -73.99, "closed_date": "1900-01-01T00:00:00.000"}),
        ),
        rec(
            "row-16",
            Some("111"),
            "2024-06-01T00:00:01.250",
            json!({"location_type": "   ", "descriptor": null}),
        ),
    ]
}
