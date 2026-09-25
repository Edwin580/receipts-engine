mod common;

use common::*;
use receipts_snapshot::socrata::{self, FetchOptions, Transport, TransportError};
use receipts_snapshot::spec::NYC_311;
use receipts_snapshot::{assemble, synth};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::time::Duration;

/// Serves Socrata-shaped responses from memory, honouring `:id` keyset
/// paging and `$limit`, with optional scripted failures.
struct FakeSocrata {
    metadata: Value,
    records: Vec<Value>,
    failures: VecDeque<TransportError>,
    urls: Vec<String>,
}

impl FakeSocrata {
    fn new(mut records: Vec<Value>) -> Self {
        records.sort_by(|a, b| a[":id"].as_str().cmp(&b[":id"].as_str()));
        Self {
            metadata: metadata(),
            records,
            failures: VecDeque::new(),
            urls: Vec::new(),
        }
    }
}

fn param(url: &str, name: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == name).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

impl Transport for FakeSocrata {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, TransportError> {
        self.urls.push(url.to_string());
        if let Some(e) = self.failures.pop_front() {
            return Err(e);
        }
        if url.contains("/api/views/") {
            return Ok(serde_json::to_vec(&self.metadata).unwrap());
        }
        assert_eq!(param(url, "$order").as_deref(), Some(":id"));
        let where_ = param(url, "$where").unwrap();
        assert!(
            where_.starts_with("created_date >= '2024-01-01T00:00:00'")
                || where_.starts_with("(created_date >= ")
        );
        let after = where_
            .split_once(":id > '")
            .map(|(_, rest)| rest.trim_end_matches('\'').replace("''", "'"));
        let limit: usize = param(url, "$limit").unwrap().parse().unwrap();
        let page: Vec<&Value> = self
            .records
            .iter()
            .filter(|r| {
                after
                    .as_deref()
                    .is_none_or(|a| r[":id"].as_str().unwrap() > a)
            })
            .take(limit)
            .collect();
        Ok(serde_json::to_vec(&page).unwrap())
    }
}

fn opts(page_size: u32) -> FetchOptions {
    let mut o = FetchOptions::new(&NYC_311, scope());
    o.base_url = "https://fake".into();
    o.page_size = page_size;
    o.initial_backoff = Duration::from_millis(1);
    o
}

#[test]
fn keyset_paging_fetches_everything_once() {
    let records = fixture();
    let mut fake = FakeSocrata::new(records.clone());
    let out = scratch("fetch");
    let mut slept = Vec::new();
    let rec = socrata::fetch(&NYC_311, &opts(4), &mut fake, &mut |d| slept.push(d), &out).unwrap();
    // 16 records at 4 per page: 4 full pages, then an empty one to confirm the end.
    assert_eq!(
        rec.pages.iter().map(|p| p.records).collect::<Vec<_>>(),
        vec![4, 4, 4, 4, 0]
    );
    assert_eq!(rec.raw_records, 16);
    assert!(slept.is_empty());
    assert!(
        param(&rec.pages[1].url, "$where")
            .unwrap()
            .ends_with(":id > 'row-04'")
    );
    assert_eq!(
        param(&rec.pages[0].url, "$select")
            .unwrap()
            .split(',')
            .next(),
        Some(":id")
    );

    // The fetched raw directory builds the same snapshot as the records themselves.
    let via_fetch = assemble::build(&NYC_311, &out, &scratch("snap"))
        .unwrap()
        .manifest;
    let raw = scratch("raw");
    synth::write_raw(
        &raw,
        &NYC_311,
        &scope(),
        &metadata(),
        synth::paginate(&records, 16),
    )
    .unwrap();
    let direct = assemble::build(&NYC_311, &raw, &scratch("snap"))
        .unwrap()
        .manifest;
    assert_eq!(via_fetch.snapshot_hash, direct.snapshot_hash);
    assert_eq!(via_fetch.fetch.pagination.kind, "keyset");
}

#[test]
fn retries_transient_errors_with_backoff() {
    let mut fake = FakeSocrata::new(fixture());
    fake.failures.extend([
        TransportError::Status(429, "slow down".into()),
        TransportError::Status(503, "unavailable".into()),
        TransportError::Network("reset".into()),
    ]);
    let mut slept = Vec::new();
    let rec = socrata::fetch(
        &NYC_311,
        &opts(100),
        &mut fake,
        &mut |d| slept.push(d),
        &scratch("fetch"),
    )
    .unwrap();
    assert_eq!(rec.raw_records, 16);
    assert_eq!(
        slept,
        vec![
            Duration::from_millis(1),
            Duration::from_millis(2),
            Duration::from_millis(4)
        ]
    );
}

#[test]
fn gives_up_on_client_errors_and_after_max_attempts() {
    let mut fake = FakeSocrata::new(fixture());
    fake.failures
        .push_back(TransportError::Status(400, "bad query".into()));
    let err = socrata::fetch(
        &NYC_311,
        &opts(100),
        &mut fake,
        &mut |_| {},
        &scratch("fetch"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 400"));
    assert_eq!(fake.urls.len(), 1, "4xx is not retried");

    let mut fake = FakeSocrata::new(fixture());
    fake.failures
        .extend((0..10).map(|_| TransportError::Status(500, "down".into())));
    let err = socrata::fetch(
        &NYC_311,
        &opts(100),
        &mut fake,
        &mut |_| {},
        &scratch("fetch"),
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 500"));
    assert_eq!(fake.urls.len(), 6);
}

#[test]
fn schema_drift_stops_before_paging() {
    let mut fake = FakeSocrata::new(fixture());
    fake.metadata["columns"]
        .as_array_mut()
        .unwrap()
        .retain(|c| c["fieldName"] != "status");
    let out = scratch("fetch");
    let err = socrata::fetch(&NYC_311, &opts(100), &mut fake, &mut |_| {}, &out).unwrap_err();
    assert!(format!("{err:#}").contains("CR-01"));
    assert_eq!(fake.urls.len(), 1);
    assert!(
        !out.join("fetch.json").exists(),
        "an aborted fetch never looks complete"
    );
}

#[test]
fn refuses_non_empty_output() {
    let out = scratch("fetch");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("something"), "x").unwrap();
    let mut fake = FakeSocrata::new(fixture());
    assert!(socrata::fetch(&NYC_311, &opts(100), &mut fake, &mut |_| {}, &out).is_err());
    let _ = json!(null);
}
