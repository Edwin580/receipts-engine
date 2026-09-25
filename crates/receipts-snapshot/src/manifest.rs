//! `manifest.json` (docs/snapshot/schema.md §4). Integers and strings only,
//! so the manifest can be canonicalized and hashed.

use crate::raw::{Pagination, Scope};
use crate::rules::Action;
use anyhow::{Context, Result};
use receipts_core::ColumnType;
use receipts_core::canonical_json::to_canonical_json;
use receipts_core::hash::ContentHasher;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FORMAT_VERSION: &str = "receipts-snapshot/1";
pub const MANIFEST_CONTEXT: &str = "receipts snapshot v1 manifest";
pub const DATA_FILE: &str = "data.arrow";
pub const CLEANING_LOG_FILE: &str = "cleaning_log.arrow";
pub const REJECTS_FILE: &str = "rejects.arrow";
pub const MANIFEST_FILE: &str = "manifest.json";

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: String,
    pub snapshot_hash: String,
    pub manifest_hash: String,
    pub source: Source,
    pub fetch: Fetch,
    pub build: Build,
    pub scope: ScopeInfo,
    pub row_count: u32,
    pub chunk_rows: u32,
    pub chunk_count: u32,
    pub sort_key: Vec<String>,
    pub schema: Vec<ColumnInfo>,
    pub excluded_columns: Vec<Excluded>,
    pub cleaning: Cleaning,
    pub known_issues: Vec<KnownIssue>,
    pub files: Vec<FileInfo>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Source {
    pub source_id: u16,
    pub dataset: String,
    pub portal: String,
    pub dataset_id: String,
    pub source_url: String,
    pub terms_url: String,
    pub rows_updated_at: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Fetch {
    pub started_at: String,
    pub finished_at: String,
    pub endpoint: String,
    pub select: Vec<String>,
    #[serde(rename = "where")]
    pub where_: String,
    pub order: String,
    pub pagination: Pagination,
    pub pages: u32,
    pub raw_records: u64,
    pub raw_hash: String,
    pub metadata_hash: String,
    pub rows_updated_at_start: Option<String>,
    pub rows_updated_at_end: Option<String>,
    pub tool_version: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Build {
    pub tool_version: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScopeInfo {
    pub sentence: String,
    pub predicate: Scope,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub index: u16,
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub nullable: bool,
    pub source_fields: Vec<String>,
    pub description: String,
    pub null_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dictionary_size: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dictionary_hash: Option<String>,
    pub column_hash: String,
    pub chunk_hashes: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Excluded {
    pub field: String,
    pub reason: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cleaning {
    pub rules_version: u32,
    pub rules: Vec<RuleCount>,
    pub rejected_rows: u32,
    pub cleaning_log_rows: u32,
    pub cleaning_log_hash: String,
    pub rejects_hash: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RuleCount {
    pub id: String,
    pub action: Action,
    pub count: u64,
    pub counts: String,
    pub description: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct KnownIssue {
    pub id: String,
    pub columns: Vec<String>,
    pub count: u32,
    pub sentence: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub bytes: u64,
    /// Arrow IPC buffer compression: `"lz4_frame"` or `"none"`. Absent in
    /// manifests written before M5, which are uncompressed.
    #[serde(default = "no_compression")]
    pub compression: String,
}

fn no_compression() -> String {
    "none".into()
}

pub fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::I64 => "i64",
        ColumnType::F64 => "f64",
        ColumnType::Bool => "bool",
        ColumnType::Timestamp => "timestamp",
        ColumnType::DictUtf8 => "utf8_dict",
        ColumnType::Geo => "geo",
    }
}

/// BLAKE3 (manifest context) of the canonical JSON of `manifest` with the
/// `manifest_hash` field removed.
pub fn manifest_hash(manifest: &Value) -> Result<String> {
    let mut v = manifest.clone();
    v.as_object_mut()
        .context("manifest is not a JSON object")?
        .remove("manifest_hash");
    let canonical = to_canonical_json(&v)?;
    Ok(ContentHasher::new(MANIFEST_CONTEXT)
        .raw(canonical.as_bytes())
        .finish()
        .to_hex())
}

/// The JSON that `snapshot_hash` binds besides the data: what was fetched,
/// how it was cleaned, and how rows are ordered.
pub fn descriptor(
    scope: &Scope,
    sort_key: &[String],
    portal: &str,
    dataset_id: &str,
    rules_version: u32,
) -> Result<String> {
    let v = serde_json::json!({
        "rules_version": rules_version,
        "scope": scope,
        "sort_key": sort_key,
        "source": { "portal": portal, "dataset_id": dataset_id },
    });
    Ok(to_canonical_json(&v)?)
}
