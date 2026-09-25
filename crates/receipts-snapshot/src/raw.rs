//! The raw directory: exactly what the Socrata API returned, plus a record of
//! how it was fetched. `fetch` writes it and `build` reads it. Keeping the two
//! apart means cleaning can be re-run and tested without the network (ADR 0006).
//!
//! ```text
//! <raw>/
//!   fetch.json      FetchRecord. Written last; its presence marks a complete fetch.
//!   metadata.json   Socrata view metadata (schema, rowsUpdatedAt) at fetch start.
//!   pages/000001.json ...  raw response bodies, in fetch order
//! ```

use anyhow::{Context, Result, bail, ensure};
use receipts_core::ContentHash;
use receipts_core::hash::ContentHasher;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const FETCH_FILE: &str = "fetch.json";
pub const METADATA_FILE: &str = "metadata.json";
pub const PAGES_DIR: &str = "pages";

pub const RAW_PAGES_CONTEXT: &str = "receipts snapshot v1 raw-pages";
pub const METADATA_CONTEXT: &str = "receipts snapshot v1 socrata-metadata";

/// The half-open time range a snapshot covers, on the created timestamp.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Scope {
    pub column: String,
    /// Inclusive, `YYYY-MM-DDTHH:MM:SS`.
    pub gte: String,
    /// Exclusive, `YYYY-MM-DDTHH:MM:SS`.
    pub lt: String,
}

impl Scope {
    /// Scope from two `YYYY-MM-DD` dates.
    pub fn from_dates(column: &str, from: &str, to: &str) -> Result<Self> {
        let scope = Self {
            column: column.to_string(),
            gte: format!("{from}T00:00:00"),
            lt: format!("{to}T00:00:00"),
        };
        let (lo, hi) = scope.bounds()?;
        ensure!(lo < hi, "scope is empty: {from} is not before {to}");
        Ok(scope)
    }

    /// `(gte, lt)` in naive microseconds.
    pub fn bounds(&self) -> Result<(i64, i64)> {
        let parse = |s: &str| {
            receipts_core::time::parse_naive_timestamp(s)
                .with_context(|| format!("invalid scope bound {s:?}"))
        };
        Ok((parse(&self.gte)?, parse(&self.lt)?))
    }

    /// The SoQL predicate. Bounds are validated timestamps, so they need no escaping.
    pub fn soql(&self) -> String {
        format!(
            "{c} >= '{}' AND {c} < '{}'",
            self.gte,
            self.lt,
            c = self.column
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Pagination {
    pub kind: String,
    pub key: String,
    pub page_size: u32,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PageRecord {
    pub file: String,
    pub url: String,
    pub records: u32,
    pub bytes: u64,
    pub blake3: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FetchRecord {
    pub tool_version: String,
    pub portal: String,
    pub dataset_id: String,
    pub endpoint: String,
    pub select: Vec<String>,
    pub scope: Scope,
    #[serde(rename = "where")]
    pub where_: String,
    pub order: String,
    pub pagination: Pagination,
    pub started_at: String,
    pub finished_at: String,
    /// Socrata `rowsUpdatedAt` before and after the fetch. If they differ, the
    /// dataset changed while it was being paged.
    pub rows_updated_at_start: Option<String>,
    pub rows_updated_at_end: Option<String>,
    pub pages: Vec<PageRecord>,
    pub raw_records: u64,
    pub raw_hash: String,
    pub metadata_hash: String,
}

/// An opened, integrity-checked raw directory.
#[derive(Debug)]
pub struct RawDir {
    pub root: PathBuf,
    pub fetch: FetchRecord,
    pub metadata: Vec<u8>,
}

impl RawDir {
    /// Opens a raw directory and checks every page against `fetch.json`
    /// (size, BLAKE3, and the combined `raw_hash`).
    pub fn open(root: &Path) -> Result<Self> {
        let fetch_path = root.join(FETCH_FILE);
        let fetch: FetchRecord =
            serde_json::from_slice(&fs::read(&fetch_path).with_context(|| {
                format!(
                    "{} missing: the fetch is incomplete or this is not a raw directory",
                    fetch_path.display()
                )
            })?)
            .with_context(|| format!("parsing {}", fetch_path.display()))?;
        let metadata = fs::read(root.join(METADATA_FILE)).context("reading metadata.json")?;
        let metadata_hash = ContentHasher::new(METADATA_CONTEXT).raw(&metadata).finish();
        ensure!(
            metadata_hash.to_hex() == fetch.metadata_hash,
            "metadata.json does not match fetch.json"
        );

        let mut raw = RawHasher::new();
        for page in &fetch.pages {
            let bytes = fs::read(root.join(&page.file))
                .with_context(|| format!("reading {}", page.file))?;
            ensure!(
                bytes.len() as u64 == page.bytes,
                "{}: size differs from fetch.json",
                page.file
            );
            ensure!(
                blake3::hash(&bytes).to_hex().as_str() == page.blake3,
                "{}: hash differs from fetch.json",
                page.file
            );
            raw.page(&bytes);
        }
        if raw.finish().to_hex() != fetch.raw_hash {
            bail!("raw_hash in fetch.json does not match the pages");
        }
        Ok(Self {
            root: root.to_path_buf(),
            fetch,
            metadata,
        })
    }

    pub fn read_page(&self, index: usize) -> Result<Vec<u8>> {
        let page = &self.fetch.pages[index];
        fs::read(self.root.join(&page.file)).with_context(|| format!("reading {}", page.file))
    }
}

/// `n_pages:u32 ‖ (len:u64 ‖ BLAKE3(body))*`, fed one page at a time.
#[derive(Debug)]
pub struct RawHasher {
    pages: Vec<(u64, [u8; 32])>,
}

impl RawHasher {
    pub fn new() -> Self {
        Self { pages: Vec::new() }
    }

    pub fn page(&mut self, body: &[u8]) {
        self.pages
            .push((body.len() as u64, *blake3::hash(body).as_bytes()));
    }

    /// Hashes the page count, then each page's length and BLAKE3.
    /// Hashing per-page digests keeps memory flat for multi-GB fetches.
    pub fn finish(&self) -> ContentHash {
        let mut h = ContentHasher::new(RAW_PAGES_CONTEXT);
        h.u32(self.pages.len() as u32);
        for (len, digest) in &self.pages {
            h.u64(*len).raw(digest);
        }
        h.finish()
    }
}

impl Default for RawHasher {
    fn default() -> Self {
        Self::new()
    }
}

pub fn page_file_name(index: usize) -> String {
    format!("{PAGES_DIR}/{:06}.json", index + 1)
}

/// Current UTC time as RFC 3339, second precision.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    epoch_seconds_rfc3339(secs)
}

pub fn epoch_seconds_rfc3339(secs: i64) -> String {
    let s =
        receipts_core::time::format_naive_timestamp(secs * receipts_core::time::MICROS_PER_SECOND);
    format!("{}Z", &s[..19])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_from_dates() {
        let s = Scope::from_dates("created_date", "2024-01-01", "2026-01-01").unwrap();
        assert_eq!(
            s.soql(),
            "created_date >= '2024-01-01T00:00:00' AND created_date < '2026-01-01T00:00:00'"
        );
        assert!(Scope::from_dates("c", "2026-01-01", "2024-01-01").is_err());
        assert!(Scope::from_dates("c", "2024-01-01'; drop", "2026-01-01").is_err());
    }

    #[test]
    fn rfc3339() {
        assert_eq!(epoch_seconds_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_seconds_rfc3339(1_758_758_400), "2025-09-25T00:00:00Z");
    }
}
