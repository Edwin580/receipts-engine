//! Verify-on-load: a snapshot's bytes are decoded and every hash is
//! re-derived before the engine will use it (schema.md §6). This is the
//! browser-side counterpart of `receipts-snapshot verify`, written against
//! the spec with no arrow dependency.

use receipts_core::canonical_json::to_canonical_json;
use receipts_core::hash::{
    CHUNK_ROWS, ContentHasher, chunk_ranges, cleaning_log_hash, hash_column, rejects_hash,
    snapshot_hash,
};
use receipts_core::ipc::{IpcFile, Values, read_file};
use receipts_core::{Column, ColumnData, ColumnType, ContentHash, SourceId};
use serde_json::{Value, json};

pub const FORMAT_VERSION: &str = "receipts-snapshot/1";
const MANIFEST_CONTEXT: &str = "receipts snapshot v1 manifest";
const META_SNAPSHOT_HASH: &str = "receipts.snapshot_hash";

/// A snapshot whose files matched its manifest.
#[derive(Debug)]
pub struct VerifiedSnapshot {
    pub snapshot: ContentHash,
    pub source_id: SourceId,
    pub title: String,
    pub manifest: Value,
    /// Columns with their manifest nullability.
    pub columns: Vec<(Column, bool)>,
    /// Milliseconds per phase: decode, hash, order, side tables.
    pub timings: Value,
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Result<&'a str, String> {
    let mut at = v;
    for p in path {
        at = at
            .get(p)
            .ok_or_else(|| format!("manifest: missing {}", path.join(".")))?;
    }
    at.as_str()
        .ok_or_else(|| format!("manifest: {} is not a string", path.join(".")))
}

fn u64_at(v: &Value, path: &[&str]) -> Result<u64, String> {
    let mut at = v;
    for p in path {
        at = at
            .get(p)
            .ok_or_else(|| format!("manifest: missing {}", path.join(".")))?;
    }
    at.as_u64()
        .ok_or_else(|| format!("manifest: {} is not a whole number", path.join(".")))
}

fn type_name(t: ColumnType) -> &'static str {
    match t {
        ColumnType::I64 => "i64",
        ColumnType::F64 => "f64",
        ColumnType::Bool => "bool",
        ColumnType::Timestamp => "timestamp",
        ColumnType::DictUtf8 => "utf8_dict",
        ColumnType::Geo => "geo",
    }
}

fn check_meta(file: &IpcFile, name: &str, snapshot: &str) -> Result<(), String> {
    if file.meta(META_SNAPSHOT_HASH) != Some(snapshot) {
        return Err(format!("{name} belongs to another snapshot"));
    }
    Ok(())
}

pub fn verify(
    manifest_json: &str,
    data: &[u8],
    cleaning_log: &[u8],
    rejects: &[u8],
) -> Result<VerifiedSnapshot, String> {
    let m: Value =
        serde_json::from_str(manifest_json).map_err(|e| format!("manifest.json: {e}"))?;
    if str_at(&m, &["format_version"])? != FORMAT_VERSION {
        return Err("unsupported snapshot format".into());
    }
    // Manifest hash: canonical JSON without the field itself.
    let mut without = m.clone();
    without
        .as_object_mut()
        .ok_or("manifest is not an object")?
        .remove("manifest_hash");
    let canonical = to_canonical_json(&without).map_err(|e| e.to_string())?;
    let mut h = ContentHasher::new(MANIFEST_CONTEXT);
    h.raw(canonical.as_bytes());
    if h.finish().to_hex() != str_at(&m, &["manifest_hash"])? {
        return Err("manifest_hash does not match manifest.json".into());
    }
    let snapshot_hex = str_at(&m, &["snapshot_hash"])?;
    let snapshot = ContentHash::from_hex(snapshot_hex).ok_or("malformed snapshot_hash")?;
    let row_count = u64_at(&m, &["row_count"])? as usize;
    if u64_at(&m, &["chunk_rows"])? as usize != CHUNK_ROWS {
        return Err("unexpected chunk_rows".into());
    }

    let t0 = crate::clock::now_ms();
    let file = read_file(data).map_err(|e| format!("data.arrow: {e}"))?;
    check_meta(&file, "data.arrow", snapshot_hex)?;
    let expected: Vec<usize> = chunk_ranges(row_count).map(|r| r.len()).collect();
    if file.batch_lengths != expected {
        return Err(format!(
            "data.arrow batches are not {CHUNK_ROWS}-row chunks"
        ));
    }
    let schema = m
        .get("schema")
        .and_then(Value::as_array)
        .ok_or("manifest: schema is not a list")?;
    if schema.len() != file.fields.len() {
        return Err("column count mismatch".into());
    }
    let nullable_in_file: Vec<bool> = file.fields.iter().map(|f| f.nullable).collect();
    let decoded = file
        .into_core_columns()
        .map_err(|e| format!("data.arrow: {e}"))?;
    let t1 = crate::clock::now_ms();
    let mut columns = Vec::with_capacity(schema.len());
    let mut column_hashes = Vec::with_capacity(schema.len());
    for ((i, info), col) in schema.iter().enumerate().zip(decoded) {
        let name = str_at(info, &["name"])?;
        if col.name != name {
            return Err(format!(
                "column {i} is {:?}, manifest says {name:?}",
                col.name
            ));
        }
        if type_name(col.data.column_type()) != str_at(info, &["type"])? {
            return Err(format!("{name}: type mismatch"));
        }
        let nullable = info
            .get("nullable")
            .and_then(Value::as_bool)
            .ok_or("nullable")?;
        if nullable_in_file[i] != nullable {
            return Err(format!("{name}: nullability mismatch"));
        }
        if col.null_count() as u64 != u64_at(info, &["null_count"])? {
            return Err(format!("{name}: null_count mismatch"));
        }
        let hashes = hash_column(&col);
        let chunks = info
            .get("chunk_hashes")
            .and_then(Value::as_array)
            .ok_or("chunk_hashes")?;
        if chunks.len() != hashes.chunks.len()
            || chunks
                .iter()
                .zip(&hashes.chunks)
                .any(|(a, b)| a.as_str() != Some(b.to_hex().as_str()))
        {
            return Err(format!("{name}: a chunk hash does not match"));
        }
        if let Some(d) = hashes.dictionary
            && info.get("dictionary_hash").and_then(Value::as_str) != Some(d.to_hex().as_str())
        {
            return Err(format!("{name}: dictionary_hash mismatch"));
        }
        if hashes.column.to_hex() != str_at(info, &["column_hash"])? {
            return Err(format!("{name}: column_hash mismatch"));
        }
        column_hashes.push(hashes.column);
        columns.push((col, nullable));
    }
    let t2 = crate::clock::now_ms();
    check_sorted(&m, &columns)?;
    let t3 = crate::clock::now_ms();

    let log = read_file(cleaning_log).map_err(|e| format!("cleaning_log.arrow: {e}"))?;
    check_meta(&log, "cleaning_log.arrow", snapshot_hex)?;
    let log_hash = match (&log.columns[..], log.columns.first().map(|c| &c.values)) {
        ([rows, cols, rules, raws], Some(Values::U32(r))) => {
            let (Values::U16(c), Values::Utf8(rule), Values::Utf8(raw)) =
                (&cols.values, &rules.values, &raws.values)
            else {
                return Err("cleaning_log.arrow: unexpected column types".into());
            };
            let valid = |i: usize| raws.validity.as_ref().is_none_or(|v| v.get(i));
            let _ = rows;
            cleaning_log_hash((0..r.len()).map(|i| {
                (
                    r[i],
                    c[i],
                    rule[i].as_str(),
                    valid(i).then(|| raw[i].as_str()),
                )
            }))
        }
        _ => return Err("cleaning_log.arrow: unexpected columns".into()),
    };
    if log_hash.to_hex() != str_at(&m, &["cleaning", "cleaning_log_hash"])? {
        return Err("cleaning_log_hash mismatch".into());
    }
    let rej = read_file(rejects).map_err(|e| format!("rejects.arrow: {e}"))?;
    check_meta(&rej, "rejects.arrow", snapshot_hex)?;
    let rejects_hash = match &rej.columns[..] {
        [_, keys, rules, records] => {
            let (Values::Utf8(k), Values::Utf8(r), Values::Utf8(rec)) =
                (&keys.values, &rules.values, &records.values)
            else {
                return Err("rejects.arrow: unexpected column types".into());
            };
            let valid = |i: usize| keys.validity.as_ref().is_none_or(|v| v.get(i));
            rejects_hash((0..k.len()).map(|i| {
                (
                    valid(i).then(|| k[i].as_str()),
                    r[i].as_str(),
                    rec[i].as_str(),
                )
            }))
        }
        _ => return Err("rejects.arrow: unexpected columns".into()),
    };
    if rejects_hash.to_hex() != str_at(&m, &["cleaning", "rejects_hash"])? {
        return Err("rejects_hash mismatch".into());
    }

    let descriptor = to_canonical_json(&json!({
        "rules_version": u64_at(&m, &["cleaning", "rules_version"])?,
        "scope": m.get("scope").and_then(|s| s.get("predicate")).ok_or("scope.predicate")?,
        "sort_key": m.get("sort_key").ok_or("sort_key")?,
        "source": {
            "portal": str_at(&m, &["source", "portal"])?,
            "dataset_id": str_at(&m, &["source", "dataset_id"])?,
        },
    }))
    .map_err(|e| e.to_string())?;
    let source_id =
        u16::try_from(u64_at(&m, &["source", "source_id"])?).map_err(|_| "source_id")?;
    let derived = snapshot_hash(
        source_id,
        u32::try_from(row_count).map_err(|_| "row_count")?,
        &descriptor,
        &column_hashes,
        &[log_hash, rejects_hash],
    );
    if derived != snapshot {
        return Err(format!(
            "snapshot_hash mismatch: files hash to {derived}, manifest says {snapshot_hex}"
        ));
    }
    let t4 = crate::clock::now_ms();
    Ok(VerifiedSnapshot {
        timings: json!({
            "decode_ms": t1 - t0,
            "hash_ms": t2 - t1,
            "sort_check_ms": t3 - t2,
            "side_tables_ms": t4 - t3,
        }),
        snapshot,
        source_id: SourceId(source_id),
        title: str_at(&m, &["source", "dataset"])?.to_string(),
        manifest: m,
        columns,
    })
}

/// Rows are strictly increasing on the sort key; RowIds depend on it.
fn check_sorted(m: &Value, columns: &[(Column, bool)]) -> Result<(), String> {
    let keys = m
        .get("sort_key")
        .and_then(Value::as_array)
        .ok_or("sort_key")?;
    let cols: Vec<&Vec<i64>> = keys
        .iter()
        .map(|k| {
            let k = k.as_str().ok_or("sort key is not a string")?;
            let (c, _) = columns
                .iter()
                .find(|(c, _)| c.name == k)
                .ok_or("sort key not in schema")?;
            match &c.data {
                ColumnData::I64(v) | ColumnData::Timestamp(v) => Ok(v),
                _ => Err("sort key is not an integer or timestamp column".to_string()),
            }
        })
        .collect::<Result<_, String>>()?;
    let n = columns.first().map_or(0, |(c, _)| c.len());
    for r in 1..n {
        let ord = cols
            .iter()
            .map(|c| c[r - 1].cmp(&c[r]))
            .find(|o| o.is_ne())
            .unwrap_or(std::cmp::Ordering::Equal);
        if ord != std::cmp::Ordering::Less {
            return Err(format!("rows {} and {r} are not in sort_key order", r - 1));
        }
    }
    Ok(())
}
