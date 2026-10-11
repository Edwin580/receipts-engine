//! Fetching SeeClickFix requests (API v2) into a raw directory that `build`
//! reads like any other.
//!
//! SeeClickFix returns nested JSON with UTC offsets, so each request is
//! reduced by [`flatten`] to the flat record `spec::WESTCHESTER_SCF`
//! describes: its id, local created and closed times, town, request type,
//! status, ZIP and location. Those records are the raw pages. The responses
//! themselves are kept verbatim under `source/`, listed with their BLAKE3
//! in `source.json`, together with how many requests each organization
//! outside the town list had (they are left out).
//!
//! Paging is by page number over a fixed window in the past, oldest first
//! (`sort=created_at&sort_direction=ASC`), so later requests can't shift the
//! pages. A request seen twice is kept once and counted. SeeClickFix answers
//! bursts with HTTP 403, so 403 is retried like 429, with a long backoff.

use crate::raw::{
    self, FETCH_FILE, FetchRecord, METADATA_CONTEXT, METADATA_FILE, PAGES_DIR, PageRecord,
    Pagination, RawHasher, Scope,
};
use crate::socrata::{Transport, TransportError};
use crate::spec::{DatasetSpec, WESTCHESTER_TOWNS};
use anyhow::{Context, Result, bail, ensure};
use receipts_core::hash::ContentHasher;
use receipts_core::time::{MICROS_PER_DAY, format_naive_timestamp, parse_naive_timestamp};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::Duration;

pub const SOURCE_DIR: &str = "source";
pub const SOURCE_FILE: &str = "source.json";
/// Flattened records per raw page.
const RECORDS_PER_PAGE: usize = 10_000;
/// A runaway-pagination guard: 100 requests a page, so 2 million requests.
const MAX_PAGES: u32 = 20_000;

#[derive(Clone, Debug)]
pub struct ScfOptions {
    pub base_url: String,
    pub scope: Scope,
    /// `(min, max)` latitude and longitude of the area to ask for.
    pub lat: (f32, f32),
    pub lon: (f32, f32),
    pub per_page: u32,
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    /// Wait between pages, to stay under SeeClickFix's rate limit.
    pub pause: Duration,
}

impl ScfOptions {
    pub fn new(spec: &DatasetSpec, scope: Scope) -> Self {
        Self {
            base_url: "https://seeclickfix.com".into(),
            scope,
            lat: spec.area.lat,
            lon: spec.area.lon,
            per_page: 100,
            max_attempts: 8,
            initial_backoff: Duration::from_secs(30),
            pause: Duration::from_secs(6),
        }
    }

    /// The query without the page number. The window is a day wider than
    /// the scope on each side, in UTC, so every request whose local time is
    /// in scope is fetched; `build` drops the rest by scope (CR-12).
    pub fn query(&self) -> Result<String> {
        let (lo, hi) = self.scope.bounds()?;
        let utc = |micros: i64| format!("{}Z", &format_naive_timestamp(micros)[..19]);
        Ok(format!(
            "min_lat={}&max_lat={}&min_lng={}&max_lng={}&after={}&before={}\
             &status=open,acknowledged,closed,archived&sort=created_at&sort_direction=ASC&per_page={}",
            self.lat.0,
            self.lat.1,
            self.lon.0,
            self.lon.1,
            utc(lo - MICROS_PER_DAY),
            utc(hi + MICROS_PER_DAY),
            self.per_page,
        ))
    }
}

/// The town that handles requests from a SeeClickFix organization.
pub fn town_of(organization: &str) -> Option<&'static str> {
    WESTCHESTER_TOWNS
        .iter()
        .find(|(org, _)| *org == organization)
        .map(|(_, town)| *town)
}

/// `2025-12-31T11:16:02-05:00` -> `2025-12-31T11:16:02`: the local wall
/// clock, which is what NYC data publishes too. Anything that doesn't look
/// like an ISO time with an offset is returned unchanged, so the build's
/// cleaning rules see it and reject it (CR-05) rather than it vanishing here.
pub fn local_time(s: &str) -> String {
    let b = s.as_bytes();
    let has_offset = b.len() >= 20 && matches!(b[19], b'+' | b'-' | b'Z' | b'.');
    if has_offset && parse_naive_timestamp(&s[..19]).is_some() {
        s[..19].to_string()
    } else {
        s.to_string()
    }
}

/// The last run of exactly five digits in an address, such as its ZIP.
pub fn zip_of(address: &str) -> Option<String> {
    let b = address.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i - start == 5 {
                found = Some(address[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    found
}

/// One SeeClickFix request as a flat record of the source fields in
/// `spec::WESTCHESTER_SCF`, or `Err(organization)` if its organization is
/// not one of the towns.
pub fn flatten(issue: &Value) -> std::result::Result<Value, String> {
    let rt = issue.get("request_type");
    let org = rt
        .and_then(|r| r.get("organization"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let Some(town) = town_of(org) else {
        return Err(if org.is_empty() {
            "(none)".into()
        } else {
            org.into()
        });
    };
    let text = |v: Option<&Value>| match v {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::String(n.to_string()),
        _ => Value::Null,
    };
    let time = |key: &str| match issue.get(key) {
        Some(Value::String(s)) => Value::String(local_time(s)),
        _ => Value::Null,
    };
    let kind = rt
        .and_then(|r| r.get("title"))
        .filter(|t| t.is_string())
        .or_else(|| issue.get("summary"));
    let mut r = Map::new();
    r.insert("id".into(), text(issue.get("id")));
    r.insert("created_at".into(), time("created_at"));
    r.insert("closed_at".into(), time("closed_at"));
    r.insert("town".into(), Value::String(town.into()));
    r.insert("request_type".into(), text(kind));
    r.insert("status".into(), text(issue.get("status")));
    r.insert(
        "zip".into(),
        issue
            .get("address")
            .and_then(Value::as_str)
            .and_then(zip_of)
            .map_or(Value::Null, Value::String),
    );
    r.insert("lat".into(), text(issue.get("lat")));
    r.insert("lng".into(), text(issue.get("lng")));
    Ok(Value::Object(r))
}

/// Fetches every request in scope into `out`, which must not exist yet or
/// be empty.
pub fn fetch(
    spec: &DatasetSpec,
    opts: &ScfOptions,
    transport: &mut dyn Transport,
    sleep: &mut dyn FnMut(Duration),
    out: &Path,
) -> Result<FetchRecord> {
    ensure!(opts.per_page > 0, "page size must be positive");
    ensure!(opts.max_attempts > 0, "max attempts must be positive");
    if out.exists() {
        ensure!(
            fs::read_dir(out)?.next().is_none(),
            "{} is not empty; refusing to overwrite a raw directory",
            out.display()
        );
    }
    fs::create_dir_all(out.join(PAGES_DIR))?;
    fs::create_dir_all(out.join(SOURCE_DIR))?;
    let started_at = raw::now_rfc3339();
    let started_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let query = opts.query()?;
    let endpoint = format!("{}/api/v2/issues", opts.base_url);

    let mut kept: BTreeMap<u64, Value> = BTreeMap::new();
    let mut skipped: BTreeMap<String, u64> = BTreeMap::new();
    let mut duplicates = 0u64;
    let mut seen = BTreeSet::new();
    let mut responses = Vec::new();
    let mut page = 1u32;
    loop {
        ensure!(page <= MAX_PAGES, "more than {MAX_PAGES} pages; stopping");
        if page > 1 {
            sleep(opts.pause);
        }
        let url = format!("{endpoint}?{query}&page={page}");
        let body = get_with_retry(transport, sleep, opts, &url)?;
        let parsed: Value =
            serde_json::from_slice(&body).with_context(|| format!("page {page} is not JSON"))?;
        let issues = parsed
            .get("issues")
            .and_then(Value::as_array)
            .with_context(|| format!("page {page} has no issues list"))?;
        let file = format!("{SOURCE_DIR}/{page:06}.json");
        fs::write(out.join(&file), &body)?;
        responses.push(json!({
            "file": file,
            "url": url,
            "issues": issues.len(),
            "bytes": body.len(),
            "blake3": blake3::hash(&body).to_hex().to_string(),
        }));
        for issue in issues {
            let id = issue
                .get("id")
                .and_then(Value::as_u64)
                .with_context(|| format!("page {page}: a request has no numeric id"))?;
            if !seen.insert(id) {
                duplicates += 1;
                continue;
            }
            match flatten(issue) {
                Ok(r) => {
                    kept.insert(id, r);
                }
                Err(org) => *skipped.entry(org).or_default() += 1,
            }
        }
        let next = parsed
            .pointer("/metadata/pagination/next_page")
            .is_some_and(|v| !v.is_null());
        if !next || issues.is_empty() {
            break;
        }
        page += 1;
    }

    // The raw pages: the kept records in id order.
    let records: Vec<&Value> = kept.values().collect();
    let mut hasher = RawHasher::new();
    let mut pages = Vec::new();
    for (i, chunk) in records.chunks(RECORDS_PER_PAGE).enumerate() {
        let body = serde_json::to_vec(chunk)?;
        let file = raw::page_file_name(i);
        fs::write(out.join(&file), &body)?;
        hasher.page(&body);
        pages.push(PageRecord {
            file,
            url: format!("{endpoint}?{query} (flattened)"),
            records: chunk.len() as u32,
            bytes: body.len() as u64,
            blake3: blake3::hash(&body).to_hex().to_string(),
        });
    }
    if pages.is_empty() {
        bail!("SeeClickFix returned no requests for the towns in scope");
    }

    let metadata = serde_json::to_vec(&crate::synth::metadata_for(spec, started_epoch))?;
    fs::write(out.join(METADATA_FILE), &metadata)?;
    let source = json!({
        "endpoint": endpoint,
        "query": query,
        "responses": responses,
        "requests_kept": kept.len(),
        "duplicates": duplicates,
        "skipped_by_organization": skipped,
        "towns": WESTCHESTER_TOWNS.iter().map(|(o, t)| json!({ "organization": o, "town": t })).collect::<Vec<_>>(),
    });
    fs::write(out.join(SOURCE_FILE), serde_json::to_vec_pretty(&source)?)?;

    let record = FetchRecord {
        tool_version: crate::TOOL_VERSION.into(),
        portal: spec.portal.into(),
        dataset_id: spec.dataset_id.into(),
        endpoint,
        select: spec.source_fields().map(String::from).collect(),
        scope: opts.scope.clone(),
        where_: format!("{query}; organizations in the town list"),
        order: "created_at".into(),
        pagination: Pagination {
            kind: "page".into(),
            key: "page".into(),
            page_size: opts.per_page,
        },
        started_at,
        finished_at: raw::now_rfc3339(),
        rows_updated_at_start: None,
        rows_updated_at_end: None,
        raw_records: kept.len() as u64,
        raw_hash: hasher.finish().to_hex(),
        metadata_hash: ContentHasher::new(METADATA_CONTEXT)
            .raw(&metadata)
            .finish()
            .to_hex(),
        pages,
    };
    fs::write(out.join(FETCH_FILE), serde_json::to_vec_pretty(&record)?)?;
    eprintln!(
        "{} requests kept, {} from other organizations left out, {} duplicates",
        kept.len(),
        skipped.values().sum::<u64>(),
        duplicates
    );
    Ok(record)
}

/// SeeClickFix rate-limits with 403 as well as 429, so both are retried.
fn retryable(e: &TransportError) -> bool {
    match e {
        TransportError::Status(code, _) => {
            *code == 403 || *code == 429 || (500..600).contains(code)
        }
        TransportError::Network(_) => true,
    }
}

fn get_with_retry(
    transport: &mut dyn Transport,
    sleep: &mut dyn FnMut(Duration),
    opts: &ScfOptions,
    url: &str,
) -> Result<Vec<u8>> {
    let mut backoff = opts.initial_backoff;
    for attempt in 1..=opts.max_attempts {
        match transport.get(url) {
            Ok(body) => return Ok(body),
            Err(e) if retryable(&e) && attempt < opts.max_attempts => {
                let short = e.to_string().chars().take(80).collect::<String>();
                eprintln!("attempt {attempt} failed ({short}); retrying in {backoff:?}");
                sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(600));
            }
            Err(e) => return Err(e).with_context(|| format!("GET {url}")),
        }
    }
    unreachable!("max_attempts is at least 1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_times_keep_the_wall_clock() {
        assert_eq!(
            local_time("2025-12-31T11:16:02-05:00"),
            "2025-12-31T11:16:02"
        );
        assert_eq!(
            local_time("2025-07-01T09:00:00-04:00"),
            "2025-07-01T09:00:00"
        );
        assert_eq!(local_time("2025-07-01T09:00:00Z"), "2025-07-01T09:00:00");
        // Not a time: passed through for the cleaning rules to reject.
        assert_eq!(local_time("yesterday"), "yesterday");
        assert_eq!(
            local_time("2025-13-01T09:00:00-04:00"),
            "2025-13-01T09:00:00-04:00"
        );
    }

    #[test]
    fn zips_are_the_last_five_digit_run() {
        assert_eq!(
            zip_of("332 Walnut St Yonkers, NY, 10701, USA").as_deref(),
            Some("10701")
        );
        assert_eq!(
            zip_of("20 Cotswold Way Scarsdale NY 10583, United States").as_deref(),
            Some("10583")
        );
        assert_eq!(
            zip_of("12345 Long Rd Valhalla, NY").as_deref(),
            Some("12345")
        );
        assert_eq!(zip_of("Trevor Park"), None);
        assert_eq!(zip_of("446–698 Old Kensico Rd"), None);
        assert_eq!(zip_of("PO 105701"), None);
    }

    #[test]
    fn flatten_maps_towns_and_keeps_only_listed_ones() {
        let issue = json!({
            "id": 20702308,
            "status": "Archived",
            "summary": "Potholes",
            "created_at": "2025-12-31T11:16:02-05:00",
            "closed_at": "2026-01-05T15:21:23-05:00",
            "address": "Sleepy Hollow Road Ext Briarcliff Manor, New York, 10510",
            "lat": 41.11766651155245,
            "lng": -73.84323651615681,
            "request_type": { "title": "Potholes", "organization": "Mount Pleasant, NY" }
        });
        assert_eq!(
            flatten(&issue).unwrap(),
            json!({
                "id": "20702308",
                "created_at": "2025-12-31T11:16:02",
                "closed_at": "2026-01-05T15:21:23",
                "town": "Mount Pleasant",
                "request_type": "Potholes",
                "status": "Archived",
                "zip": "10510",
                "lat": "41.11766651155245",
                "lng": "-73.84323651615681"
            })
        );
        let mut mv = issue.clone();
        mv["request_type"]["organization"] = json!("Public Works Issues");
        assert_eq!(flatten(&mv).unwrap()["town"], "Mount Vernon");
        let mut open = issue.clone();
        open["closed_at"] = Value::Null;
        assert_eq!(flatten(&open).unwrap()["closed_at"], Value::Null);
        let mut ct = issue.clone();
        ct["request_type"]["organization"] = json!("Wilton, CT");
        assert_eq!(flatten(&ct).unwrap_err(), "Wilton, CT");
        let mut none = issue.clone();
        none["request_type"] = json!({});
        assert_eq!(flatten(&none).unwrap_err(), "(none)");
        // No type title: the summary stands in.
        let mut untitled = issue;
        untitled["request_type"] = json!({ "organization": "Greenburgh, NY" });
        assert_eq!(flatten(&untitled).unwrap()["request_type"], "Potholes");
    }

    #[test]
    fn query_widens_the_scope_by_a_day_in_utc() {
        let scope = Scope::from_dates("created_at", "2025-01-01", "2026-01-01").unwrap();
        let q = ScfOptions::new(&crate::spec::WESTCHESTER_SCF, scope)
            .query()
            .unwrap();
        assert!(q.contains("after=2024-12-31T00:00:00Z"), "{q}");
        assert!(q.contains("before=2026-01-02T00:00:00Z"), "{q}");
        assert!(q.contains("sort=created_at&sort_direction=ASC"), "{q}");
        assert!(q.contains("min_lat=40.88"), "{q}");
    }
}
