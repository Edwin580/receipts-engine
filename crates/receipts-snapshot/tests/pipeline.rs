mod common;

use common::*;
use receipts_core::ColumnData;
use receipts_core::time::parse_naive_timestamp;
use receipts_snapshot::arrow_io::{self, ReadTable};
use receipts_snapshot::manifest::{CLEANING_LOG_FILE, DATA_FILE, MANIFEST_FILE, REJECTS_FILE};
use receipts_snapshot::rules::Rule;
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::{assemble, synth, verify};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn build_from(records: &[Value], page_size: usize) -> assemble::Built {
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(records, page_size),
    )
    .unwrap();
    assemble::build(&NYC_311, &raw, &scratch("snap")).unwrap()
}

#[test]
fn fixture_outcome() {
    let built = build_from(&fixture(), 5);
    let m = &built.manifest;
    let data = ReadTable::open(&built.dir.join(DATA_FILE)).unwrap();

    // Admitted rows, in (created_date, unique_key) order.
    let keys = match data.column(0).unwrap().data {
        ColumnData::I64(v) => v,
        _ => unreachable!(),
    };
    assert_eq!(keys, vec![101, 109, 100, 110, 111, 105, 108]);
    assert_eq!(m.row_count, 7);

    let created = match data.column(1).unwrap().data {
        ColumnData::Timestamp(v) => v,
        _ => unreachable!(),
    };
    assert_eq!(
        created[4],
        parse_naive_timestamp("2024-06-01T00:00:01.25").unwrap()
    );

    // Text: trimmed, sorted dictionary, nulls for empty strings.
    let agency = data.column(3).unwrap();
    match &agency.data {
        ColumnData::DictUtf8 { codes, dictionary } => {
            assert_eq!(dictionary, &["DOT", "HPD", "NYPD"]);
            assert_eq!(codes[0], 1, "row 0 is key 101 with agency '  HPD ' trimmed");
            assert_eq!(codes[1], 0);
        }
        _ => unreachable!(),
    }
    let descriptor = data.column(5).unwrap();
    assert!(!descriptor.is_valid(0), "empty descriptor -> null (CR-08)");
    assert!(
        !descriptor.is_valid(4),
        "missing descriptor -> null, not logged"
    );
    assert!(
        !data.column(2).unwrap().is_valid(0),
        "unparseable closed_date -> null (CR-06)"
    );

    // Location: lat only -> null (CR-09); JSON numbers accepted.
    let loc = data.column(12).unwrap();
    assert!(!loc.is_valid(0));
    match &loc.data {
        ColumnData::Geo { lat, lon } => {
            assert_eq!((lat[3], lon[3]), (40.75f32, -73.99f32));
            assert_eq!((lat[2], lon[2]), (40.7f32, -73.9f32));
        }
        _ => unreachable!(),
    }

    // Cleaning log: exact entries, sorted by (row, column).
    let (_, log) = arrow_io::read_cleaning_log(&built.dir.join(CLEANING_LOG_FILE)).unwrap();
    let got: Vec<_> = log
        .iter()
        .map(|e| (e.row_index, e.column, e.rule, e.raw_value.as_deref()))
        .collect();
    assert_eq!(
        got,
        vec![
            (0, 2, Rule::InvalidClosed, Some("yesterday")),
            (0, 3, Rule::Trimmed, Some("  HPD ")),
            (0, 5, Rule::EmptyText, Some("")),
            (0, 12, Rule::InvalidLocation, Some(r#"["40.6",null]"#)),
            (4, 6, Rule::EmptyText, Some("   ")),
        ]
    );

    // Rejects: one rule per record, sorted by (key, rule, raw record).
    let (_, rejects) = arrow_io::read_rejects(&built.dir.join(REJECTS_FILE)).unwrap();
    let got: Vec<_> = rejects
        .iter()
        .map(|r| (r.raw_unique_key.as_deref(), r.rule))
        .collect();
    assert_eq!(
        got,
        vec![
            (None, Rule::InvalidKey),
            (Some("0102"), Rule::InvalidKey),
            (Some("103"), Rule::InvalidCreated),
            (Some("104"), Rule::OutsideScope),
            (Some("105"), Rule::DuplicateIdentical),
            (Some("106"), Rule::DuplicateConflicting),
            (Some("106"), Rule::DuplicateConflicting),
            (Some("107"), Rule::DuplicateConflicting),
            (Some("107"), Rule::DuplicateConflicting),
        ]
    );
    for r in &rejects {
        let v: Value = serde_json::from_str(&r.raw_record).unwrap();
        assert!(
            v.get(":id").is_some(),
            "raw record is the full source record"
        );
    }

    let count = |id: &str| m.cleaning.rules.iter().find(|r| r.id == id).unwrap().count;
    assert_eq!(
        [
            "CR-02", "CR-03", "CR-04", "CR-05", "CR-06", "CR-07", "CR-08", "CR-09", "CR-10",
            "CR-11", "CR-12"
        ]
        .map(count),
        [2, 1, 4, 1, 1, 1, 2, 1, 3, 2, 1]
    );
    let issue = |id: &str| m.known_issues.iter().find(|k| k.id == id).unwrap().count;
    assert_eq!(
        issue("KI-closed-before-created"),
        2,
        "keys 108 and 110 (the 1900 placeholder)"
    );
    assert_eq!(issue("KI-closed-before-2010"), 1);
    assert_eq!(issue("KI-created-midnight"), 1);
    assert_eq!(issue("KI-location-outside-nyc"), 1);
    assert_eq!(issue("KI-borough-unspecified"), 1);
    assert_eq!(issue("KI-zip-not-5-digits"), 1);

    verify::verify(&built.dir).unwrap();
}

#[test]
fn snapshot_hash_ignores_fetch_order_and_paging() {
    let scope = scope();
    let mut records = fixture();
    records.extend(synth::generate_311(3000, 11, &scope));
    let baseline = build_from(&records, 1000).manifest;
    let mut rng = synth::Rng::new(5);
    for page_size in [1, 7, 256, 5000] {
        // Fisher-Yates shuffle.
        for i in (1..records.len()).rev() {
            records.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let m = build_from(&records, page_size).manifest;
        assert_eq!(
            m.snapshot_hash, baseline.snapshot_hash,
            "page_size {page_size}"
        );
        assert_eq!(m.schema, baseline.schema);
        assert_eq!(m.cleaning, baseline.cleaning);
        assert_ne!(
            m.fetch.raw_hash, baseline.fetch.raw_hash,
            "the raw pages did change"
        );
    }
}

#[test]
fn content_changes_change_the_hash() {
    let base = build_from(&fixture(), 100).manifest.snapshot_hash;
    let mut edited = fixture();
    edited[0]["status"] = json!("Open");
    assert_ne!(build_from(&edited, 100).manifest.snapshot_hash, base);
    // A change that only affects a rejected record still changes the hash:
    // rejects are provenance too.
    let mut edited = fixture();
    edited[2]["status"] = json!("Open");
    assert_ne!(build_from(&edited, 100).manifest.snapshot_hash, base);
}

#[test]
fn multi_chunk_snapshot_verifies() {
    let records = synth::generate_311(70_000, 3, &scope());
    let built = build_from(&records, 50_000);
    assert_eq!(built.manifest.chunk_count, 2);
    let v = verify::verify(&built.dir).unwrap();
    assert_eq!(v.chunks_checked, 13 * 2);
}

fn expect_verify_error(dir: &Path, needle: &str) {
    let err = format!("{:#}", verify::verify(dir).unwrap_err());
    assert!(err.contains(needle), "expected {needle:?} in {err:?}");
}

#[test]
fn verify_detects_tampering() {
    // Manifest edits.
    let built = build_from(&fixture(), 100);
    let path = built.dir.join(MANIFEST_FILE);
    let original = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        original.replace("\"row_count\": 7", "\"row_count\": 8"),
    )
    .unwrap();
    expect_verify_error(&built.dir, "manifest_hash");
    fs::write(&path, &original).unwrap();
    verify::verify(&built.dir).unwrap();

    // One value changed in data.arrow, rewritten with the correct metadata.
    let data = ReadTable::open(&built.dir.join(DATA_FILE)).unwrap();
    let mut cols: Vec<_> = (0..13)
        .map(|i| (data.column(i).unwrap(), data.schema.field(i).is_nullable()))
        .collect();
    if let ColumnData::I64(v) = &mut cols[0].0.data {
        v[3] += 1_000_000;
    }
    let hash = receipts_core::ContentHash::from_hex(&built.manifest.snapshot_hash).unwrap();
    let refs: Vec<_> = cols.iter().map(|(c, n)| (c, *n)).collect();
    arrow_io::write_data(&built.dir.join(DATA_FILE), &refs, &hash).unwrap();
    expect_verify_error(&built.dir, "unique_key: chunk 0 hash mismatch");

    // A reject quietly dropped.
    let built = build_from(&fixture(), 100);
    let (_, mut rejects) = arrow_io::read_rejects(&built.dir.join(REJECTS_FILE)).unwrap();
    rejects.pop();
    arrow_io::write_rejects(&built.dir.join(REJECTS_FILE), &rejects, &hash_of(&built)).unwrap();
    expect_verify_error(&built.dir, "rejected_rows");

    // A cleaning-log entry altered.
    let built = build_from(&fixture(), 100);
    let (_, mut log) = arrow_io::read_cleaning_log(&built.dir.join(CLEANING_LOG_FILE)).unwrap();
    log[0].raw_value = Some("today".into());
    arrow_io::write_cleaning_log(&built.dir.join(CLEANING_LOG_FILE), &log, &hash_of(&built))
        .unwrap();
    expect_verify_error(&built.dir, "cleaning_log_hash");
}

fn hash_of(built: &assemble::Built) -> receipts_core::ContentHash {
    receipts_core::ContentHash::from_hex(&built.manifest.snapshot_hash).unwrap()
}

#[test]
fn raw_directory_is_integrity_checked() {
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(&fixture(), 5),
    )
    .unwrap();
    let page = raw.join("pages/000002.json");
    let body = fs::read_to_string(&page).unwrap();
    fs::write(&page, body.replace("Closed", "Clozed")).unwrap();
    let err = format!(
        "{:#}",
        assemble::build(&NYC_311, &raw, &scratch("snap")).unwrap_err()
    );
    assert!(err.contains("000002.json"), "{err}");
}

#[test]
fn non_scalar_field_fails_the_build() {
    let mut records = fixture();
    records[0]["agency"] = json!({"name": "NYPD"});
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(&records, 100),
    )
    .unwrap();
    let err = format!(
        "{:#}",
        assemble::build(&NYC_311, &raw, &scratch("snap")).unwrap_err()
    );
    assert!(err.contains("CR-01"), "{err}");
}

#[test]
fn schema_drift_fails_the_build() {
    let mut meta = metadata();
    let cols = meta["columns"].as_array_mut().unwrap();
    cols.retain(|c| c["fieldName"] != "borough");
    cols.iter_mut()
        .find(|c| c["fieldName"] == "latitude")
        .unwrap()["dataTypeName"] = json!("text");
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &meta,
        synth::paginate(&fixture(), 100),
    )
    .unwrap();
    let err = format!(
        "{:#}",
        assemble::build(&NYC_311, &raw, &scratch("snap")).unwrap_err()
    );
    assert!(
        err.contains("CR-01")
            && err.contains("\"borough\" is missing")
            && err.contains("\"latitude\" has type"),
        "{err}"
    );
}

#[test]
fn empty_scope_builds_an_empty_snapshot() {
    let built = build_from(&[], 100);
    assert_eq!(built.manifest.row_count, 0);
    assert_eq!(built.manifest.chunk_count, 0);
    verify::verify(&built.dir).unwrap();
}

#[test]
fn refuses_to_overwrite() {
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(&fixture(), 100),
    )
    .unwrap();
    let out = scratch("snap");
    assemble::build(&NYC_311, &raw, &out).unwrap();
    assert!(assemble::build(&NYC_311, &raw, &out).is_err());
}
