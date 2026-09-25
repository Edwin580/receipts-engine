//! Fetching from the Socrata SODA 2 API into a raw directory.
//!
//! Paging is keyset on the system row id: `$order=:id` and
//! `$where=(<scope>) AND :id > '<last :id>'`. Offset paging can skip or repeat
//! rows when the dataset is updated between pages. Keyset paging on the
//! source's own key (`unique_key`) would silently drop duplicate keys at page
//! boundaries, and duplicate keys are exactly what CR-03 and CR-04 look for.

use crate::raw::{
    self, FETCH_FILE, FetchRecord, METADATA_CONTEXT, METADATA_FILE, PAGES_DIR, PageRecord,
    Pagination, RawHasher, Scope,
};
use crate::spec::DatasetSpec;
use anyhow::{Context, Result, bail, ensure};
use receipts_core::hash::ContentHasher;
use serde::Deserialize;
use serde_json::value::RawValue;
use std::fmt;
use std::fs;
use std::path::Path;
use std::time::Duration;

pub const ROW_ID_FIELD: &str = ":id";

#[derive(Debug)]
pub enum TransportError {
    Status(u16, String),
    Network(String),
}

impl TransportError {
    fn retryable(&self) -> bool {
        match self {
            Self::Status(code, _) => *code == 429 || (500..600).contains(code),
            Self::Network(_) => true,
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(code, body) => write!(f, "HTTP {code}: {body}"),
            Self::Network(msg) => write!(f, "network error: {msg}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// A blocking HTTP GET. Abstracted so tests can replay recorded pages.
pub trait Transport {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, TransportError>;
}

/// HTTPS via `ureq`, sending `X-App-Token` when a token is configured.
#[derive(Debug)]
pub struct UreqTransport {
    agent: ureq::Agent,
    app_token: Option<String>,
}

impl UreqTransport {
    pub fn new(app_token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(600)))
            .http_status_as_error(false)
            .build()
            .into();
        Self { agent, app_token }
    }
}

impl Transport for UreqTransport {
    fn get(&mut self, url: &str) -> Result<Vec<u8>, TransportError> {
        let mut req = self.agent.get(url).header("Accept", "application/json");
        if let Some(token) = &self.app_token {
            req = req.header("X-App-Token", token);
        }
        let mut resp = req
            .call()
            .map_err(|e| TransportError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(1 << 30)
            .read_to_vec()
            .map_err(|e| TransportError::Network(e.to_string()))?;
        if status == 200 {
            Ok(body)
        } else {
            let snippet = String::from_utf8_lossy(&body[..body.len().min(500)]).into_owned();
            Err(TransportError::Status(status, snippet))
        }
    }
}

#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub base_url: String,
    pub scope: Scope,
    pub page_size: u32,
    pub max_attempts: u32,
    pub initial_backoff: Duration,
}

impl FetchOptions {
    pub fn new(spec: &DatasetSpec, scope: Scope) -> Self {
        Self {
            base_url: format!("https://{}", spec.portal),
            scope,
            page_size: 50_000,
            max_attempts: 6,
            initial_backoff: Duration::from_secs(2),
        }
    }
}

/// The subset of Socrata view metadata (`/api/views/<id>.json`) we use.
#[derive(Debug, Deserialize)]
pub struct ViewMetadata {
    #[serde(rename = "rowsUpdatedAt")]
    pub rows_updated_at: Option<i64>,
    pub columns: Vec<ViewColumn>,
}

#[derive(Debug, Deserialize)]
pub struct ViewColumn {
    #[serde(rename = "fieldName")]
    pub field_name: String,
    #[serde(rename = "dataTypeName")]
    pub data_type_name: String,
}

impl ViewMetadata {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("parsing Socrata view metadata")
    }

    pub fn rows_updated_at_rfc3339(&self) -> Option<String> {
        self.rows_updated_at.map(raw::epoch_seconds_rfc3339)
    }
}

/// CR-01: every selected field exists with an accepted type. Fails on the
/// first problem found, listing every problem.
pub fn check_schema(spec: &DatasetSpec, meta: &ViewMetadata) -> Result<()> {
    let mut problems = Vec::new();
    for (field, accepted) in spec.expected_types() {
        match meta.columns.iter().find(|c| c.field_name == field) {
            None => problems.push(format!("field {field:?} is missing")),
            Some(c) if !accepted.contains(&c.data_type_name.as_str()) => problems.push(format!(
                "field {field:?} has type {:?}, expected one of {accepted:?}",
                c.data_type_name
            )),
            Some(_) => {}
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        bail!(
            "CR-01 schema drift in {}: {}",
            spec.dataset_id,
            problems.join("; ")
        )
    }
}

pub fn select_fields(spec: &DatasetSpec) -> Vec<String> {
    std::iter::once(ROW_ID_FIELD)
        .chain(spec.source_fields())
        .map(String::from)
        .collect()
}

/// Fetches every record in scope into `out`, which must not exist yet or be empty.
pub fn fetch(
    spec: &DatasetSpec,
    opts: &FetchOptions,
    transport: &mut dyn Transport,
    sleep: &mut dyn FnMut(Duration),
    out: &Path,
) -> Result<FetchRecord> {
    ensure!(opts.page_size > 0, "page size must be positive");
    ensure!(opts.max_attempts > 0, "max attempts must be positive");
    if out.exists() {
        ensure!(
            fs::read_dir(out)?.next().is_none(),
            "{} is not empty; refusing to overwrite a raw directory",
            out.display()
        );
    }
    fs::create_dir_all(out.join(PAGES_DIR))?;
    let started_at = raw::now_rfc3339();

    let metadata_url = format!("{}/api/views/{}.json", opts.base_url, spec.dataset_id);
    let metadata = get_with_retry(transport, sleep, opts, &metadata_url)?;
    let meta = ViewMetadata::parse(&metadata)?;
    check_schema(spec, &meta)?;
    fs::write(out.join(METADATA_FILE), &metadata)?;

    let endpoint = format!("{}/resource/{}.json", opts.base_url, spec.dataset_id);
    let select = select_fields(spec);
    let scope_where = opts.scope.soql();
    let mut pages = Vec::new();
    let mut raw_hash = RawHasher::new();
    let mut raw_records = 0u64;
    let mut last_id: Option<String> = None;
    loop {
        let where_ = match &last_id {
            None => scope_where.clone(),
            Some(id) => format!("({scope_where}) AND {ROW_ID_FIELD} > {}", soql_string(id)),
        };
        let url = format!(
            "{endpoint}?$select={}&$where={}&$order={}&$limit={}",
            percent_encode(&select.join(",")),
            percent_encode(&where_),
            percent_encode(ROW_ID_FIELD),
            opts.page_size
        );
        let body = get_with_retry(transport, sleep, opts, &url)?;
        let records: Vec<&RawValue> = serde_json::from_slice(&body)
            .with_context(|| format!("page {} is not a JSON array", pages.len() + 1))?;
        let n = records.len();
        let page_last_id = match records.last() {
            Some(rec) => Some(row_id_of(rec)?),
            None => None,
        };

        let file = raw::page_file_name(pages.len());
        fs::write(out.join(&file), &body)?;
        raw_hash.page(&body);
        raw_records += n as u64;
        pages.push(PageRecord {
            file,
            url,
            records: n as u32,
            bytes: body.len() as u64,
            blake3: blake3::hash(&body).to_hex().to_string(),
        });
        eprintln!("page {:>4}: {n} records ({raw_records} total)", pages.len());

        if n < opts.page_size as usize {
            break;
        }
        ensure!(
            page_last_id != last_id,
            "pagination made no progress past {ROW_ID_FIELD} {last_id:?}"
        );
        last_id = page_last_id;
    }

    let end_meta = ViewMetadata::parse(&get_with_retry(transport, sleep, opts, &metadata_url)?)?;
    let record = FetchRecord {
        tool_version: crate::TOOL_VERSION.to_string(),
        portal: spec.portal.to_string(),
        dataset_id: spec.dataset_id.to_string(),
        endpoint,
        select,
        scope: opts.scope.clone(),
        where_: scope_where,
        order: ROW_ID_FIELD.to_string(),
        pagination: Pagination {
            kind: "keyset".into(),
            key: ROW_ID_FIELD.into(),
            page_size: opts.page_size,
        },
        started_at,
        finished_at: raw::now_rfc3339(),
        rows_updated_at_start: meta.rows_updated_at_rfc3339(),
        rows_updated_at_end: end_meta.rows_updated_at_rfc3339(),
        pages,
        raw_records,
        raw_hash: raw_hash.finish().to_hex(),
        metadata_hash: ContentHasher::new(METADATA_CONTEXT)
            .raw(&metadata)
            .finish()
            .to_hex(),
    };
    if record.rows_updated_at_start != record.rows_updated_at_end {
        eprintln!(
            "warning: the dataset was updated during the fetch ({:?} -> {:?}); rows changed mid-fetch may be missing or stale",
            record.rows_updated_at_start, record.rows_updated_at_end
        );
    }
    fs::write(out.join(FETCH_FILE), serde_json::to_vec_pretty(&record)?)?;
    Ok(record)
}

fn row_id_of(record: &RawValue) -> Result<String> {
    #[derive(Deserialize)]
    struct IdOnly {
        #[serde(rename = ":id")]
        id: Option<String>,
    }
    let parsed: IdOnly =
        serde_json::from_str(record.get()).context("record is not a JSON object")?;
    parsed
        .id
        .context("record has no :id; cannot continue keyset pagination")
}

fn get_with_retry(
    transport: &mut dyn Transport,
    sleep: &mut dyn FnMut(Duration),
    opts: &FetchOptions,
    url: &str,
) -> Result<Vec<u8>> {
    let mut backoff = opts.initial_backoff;
    for attempt in 1..=opts.max_attempts {
        match transport.get(url) {
            Ok(body) => return Ok(body),
            Err(e) if e.retryable() && attempt < opts.max_attempts => {
                eprintln!("attempt {attempt} failed ({e}); retrying in {backoff:?}");
                sleep(backoff);
                backoff *= 2;
            }
            Err(e) => return Err(e).with_context(|| format!("GET {url}")),
        }
    }
    unreachable!("max_attempts is at least 1")
}

/// A SoQL string literal: single-quoted, with `'` doubled.
pub fn soql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Percent-encodes everything except RFC 3986 unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert_eq!(percent_encode("a b:'c'>=é"), "a%20b%3A%27c%27%3E%3D%C3%A9");
        assert_eq!(soql_string("row-a'b"), "'row-a''b'");
    }
}
