//! SeeClickFix fetch -> build -> verify, against a fake API.

mod common;

use common::*;
use receipts_snapshot::raw::{RawDir, Scope};
use receipts_snapshot::seeclickfix::{self, ScfOptions};
use receipts_snapshot::socrata::{Transport, TransportError};
use receipts_snapshot::spec::WESTCHESTER_SCF;
use receipts_snapshot::{assemble, verify};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;

/// Serves SeeClickFix-shaped pages from memory, honouring `page` and
/// `per_page`, with optional scripted failures.
struct FakeScf {
    issues: Vec<Value>,
    failures: VecDeque<TransportError>,
    urls: Vec<String>,
}

fn param(url: &str, name: &str) -> Option<String> {
    url.split_once('?')?.1.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

impl Transport for FakeScf {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, TransportError> {
        self.urls.push(url.to_string());
        if let Some(e) = self.failures.pop_front() {
            return Err(e);
        }
        let page: usize = param(url, "page").unwrap().parse().unwrap();
        let per: usize = param(url, "per_page").unwrap().parse().unwrap();
        let start = (page - 1) * per;
        let slice: Vec<&Value> = self.issues.iter().skip(start).take(per).collect();
        let more = start + per < self.issues.len();
        let body = json!({
            "issues": slice,
            "metadata": { "pagination": {
                "entries": self.issues.len(),
                "page": page,
                "next_page": if more { json!(page + 1) } else { Value::Null },
            }},
        });
        Ok(serde_json::to_vec(&body).unwrap())
    }
}

fn issue(id: u64, org: &str, created: &str, closed: Option<&str>) -> Value {
    json!({
        "id": id,
        "status": if closed.is_some() { "Closed" } else { "Open" },
        "summary": "Potholes",
        "created_at": created,
        "closed_at": closed,
        "address": format!("{id} Main St Valhalla, New York, 10595"),
        "lat": 41.07,
        "lng": -73.77,
        "html_url": format!("https://seeclickfix.com/issues/{id}"),
        "request_type": { "id": 1, "title": "Potholes", "organization": org },
    })
}

fn issues() -> Vec<Value> {
    vec![
        // The night before the scope, in UTC terms inside the wider window:
        // its local time is out of scope, so the build drops it (CR-12).
        issue(1, "Mount Pleasant, NY", "2024-12-31T23:30:00-05:00", None),
        issue(
            2,
            "Mount Pleasant, NY",
            "2025-01-02T08:00:00-05:00",
            Some("2025-01-02T10:30:00-05:00"),
        ),
        issue(3, "Wilton, CT", "2025-01-03T08:00:00-05:00", None),
        issue(
            4,
            "Public Works Issues",
            "2025-07-04T09:00:00-04:00",
            Some("2025-07-08T09:00:00-04:00"),
        ),
        issue(5, "Greenburgh, NY", "2025-07-05T12:00:00-04:00", None),
        issue(5, "Greenburgh, NY", "2025-07-05T12:00:00-04:00", None),
        issue(
            6,
            "Peekskill, NY",
            "2025-11-02T01:30:00-05:00",
            Some("2025-11-03T01:30:00-05:00"),
        ),
        json!({ "id": 7, "status": "Open", "created_at": "2025-08-01T10:00:00-04:00", "request_type": {} }),
    ]
}

fn opts() -> ScfOptions {
    let scope = Scope::from_dates("created_at", "2025-01-01", "2026-01-01").unwrap();
    let mut o = ScfOptions::new(&WESTCHESTER_SCF, scope);
    o.base_url = "https://fake".into();
    o.per_page = 3;
    o.initial_backoff = Duration::from_secs(1);
    o
}

#[test]
fn fetches_builds_and_verifies() {
    let mut fake = FakeScf {
        issues: issues(),
        // SeeClickFix's rate limit answers 403; it is retried.
        failures: VecDeque::from([TransportError::Status(403, "Forbidden".into())]),
        urls: Vec::new(),
    };
    let mut slept = Vec::new();
    let raw = scratch("scf-raw");
    let record = seeclickfix::fetch(
        &WESTCHESTER_SCF,
        &opts(),
        &mut fake,
        &mut |d| slept.push(d),
        &raw,
    )
    .unwrap();

    // 8 issues at 3 a page: pages 1-3, plus the retried first request.
    assert_eq!(fake.urls.len(), 4);
    assert!(
        fake.urls
            .iter()
            .all(|u| u.contains("sort=created_at&sort_direction=ASC"))
    );
    assert_eq!(slept[0], Duration::from_secs(1), "backoff before the retry");
    // Kept: 1, 2, 4, 5, 6 (3 is in Connecticut, 7 has no organization,
    // the second 5 is a duplicate).
    assert_eq!(record.raw_records, 5);
    let source: Value =
        serde_json::from_slice(&std::fs::read(raw.join("source.json")).unwrap()).unwrap();
    assert_eq!(source["duplicates"], 1);
    assert_eq!(
        source["skipped_by_organization"],
        json!({ "(none)": 1, "Wilton, CT": 1 })
    );
    assert_eq!(source["responses"].as_array().unwrap().len(), 3);
    for r in source["responses"].as_array().unwrap() {
        let body = std::fs::read(raw.join(r["file"].as_str().unwrap())).unwrap();
        assert_eq!(
            blake3::hash(&body).to_hex().as_str(),
            r["blake3"].as_str().unwrap()
        );
    }
    RawDir::open(&raw).unwrap();

    let built = assemble::build(&WESTCHESTER_SCF, &raw, &scratch("scf-snap")).unwrap();
    let m = &built.manifest;
    assert_eq!(m.source.source_id, 2);
    assert_eq!(m.row_count, 4, "issue 1 is out of scope by local time");
    let towns = &m.schema.iter().find(|c| c.name == "town").unwrap();
    assert_eq!(towns.null_count, 0);
    assert!(
        m.known_issues
            .iter()
            .any(|k| k.id == "KI-location-outside-westchester" && k.count == 0)
    );
    assert!(
        !m.known_issues
            .iter()
            .any(|k| k.id == "KI-location-outside-nyc")
    );
    verify::verify(&built.dir).unwrap();

    // Times are local wall clock: 2025-07-04 09:00 -04:00 stays 09:00.
    let t = receipts_core::time::parse_naive_timestamp("2025-07-04T09:00:00").unwrap();
    let data = std::fs::read(built.dir.join("data.arrow")).unwrap();
    let file = receipts_core::ipc::read_file(&data).unwrap();
    let created = file
        .core_column(file.column_index("created_date").unwrap())
        .unwrap();
    let receipts_core::ColumnData::Timestamp(v) = &created.data else {
        panic!()
    };
    assert!(v.contains(&t));
}

#[test]
fn gives_up_after_max_attempts_and_on_client_errors() {
    let mut o = opts();
    o.max_attempts = 3;
    let mut fake = FakeScf {
        issues: issues(),
        failures: VecDeque::from([
            TransportError::Status(403, "Forbidden".into()),
            TransportError::Status(403, "Forbidden".into()),
            TransportError::Status(403, "Forbidden".into()),
        ]),
        urls: Vec::new(),
    };
    let err = seeclickfix::fetch(
        &WESTCHESTER_SCF,
        &o,
        &mut fake,
        &mut |_| {},
        &scratch("scf-fail"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("403"), "{err:#}");
    assert_eq!(fake.urls.len(), 3);

    let mut fake = FakeScf {
        issues: issues(),
        failures: VecDeque::from([TransportError::Status(400, "bad query".into())]),
        urls: Vec::new(),
    };
    assert!(
        seeclickfix::fetch(
            &WESTCHESTER_SCF,
            &opts(),
            &mut fake,
            &mut |_| {},
            &scratch("scf-400")
        )
        .is_err()
    );
    assert_eq!(fake.urls.len(), 1, "a 400 is not retried");
}

#[test]
fn no_town_requests_is_an_error() {
    let mut fake = FakeScf {
        issues: vec![issue(3, "Wilton, CT", "2025-01-03T08:00:00-05:00", None)],
        failures: VecDeque::new(),
        urls: Vec::new(),
    };
    let err = seeclickfix::fetch(
        &WESTCHESTER_SCF,
        &opts(),
        &mut fake,
        &mut |_| {},
        &scratch("scf-empty"),
    )
    .unwrap_err();
    assert!(err.to_string().contains("no requests"), "{err}");
}
