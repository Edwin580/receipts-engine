//! `verify`: re-derives every hash in a snapshot directory from its files and
//! checks it against the manifest. The data files are decoded, not trusted.

use crate::arrow_io::{self, META_SNAPSHOT_HASH, ReadTable};
use crate::clean;
use crate::manifest::{self, CLEANING_LOG_FILE, DATA_FILE, MANIFEST_FILE, Manifest, REJECTS_FILE};
use anyhow::{Context, Result, bail, ensure};
use receipts_core::hash::{CHUNK_ROWS, chunk_ranges, hash_column, snapshot_hash};
use receipts_core::{ColumnData, ContentHash};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct Verified {
    pub manifest: Manifest,
    pub columns_checked: usize,
    pub chunks_checked: usize,
}

pub fn verify(dir: &Path) -> Result<Verified> {
    let bytes = fs::read(dir.join(MANIFEST_FILE)).context("reading manifest.json")?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let m: Manifest =
        serde_json::from_value(value.clone()).context("manifest.json does not match the schema")?;
    ensure!(
        m.format_version == manifest::FORMAT_VERSION,
        "unsupported format {}",
        m.format_version
    );
    ensure!(
        manifest::manifest_hash(&value)? == m.manifest_hash,
        "manifest_hash does not match manifest.json"
    );

    let data = ReadTable::open(&dir.join(DATA_FILE))?;
    ensure!(
        data.meta(META_SNAPSHOT_HASH) == Some(m.snapshot_hash.as_str()),
        "data.arrow belongs to another snapshot"
    );
    ensure!(
        m.chunk_rows as usize == CHUNK_ROWS,
        "unexpected chunk_rows {}",
        m.chunk_rows
    );
    let expected_batches: Vec<usize> = chunk_ranges(m.row_count as usize)
        .map(|r| r.len())
        .collect();
    ensure!(
        data.batch_lengths() == expected_batches,
        "data.arrow batches are not {CHUNK_ROWS}-row chunks"
    );
    ensure!(
        expected_batches.len() == m.chunk_count as usize,
        "chunk_count mismatch"
    );
    ensure!(
        data.schema.fields().len() == m.schema.len(),
        "column count mismatch"
    );

    let mut column_hashes = Vec::new();
    let mut chunks_checked = 0;
    for (i, info) in m.schema.iter().enumerate() {
        let col = data.column(i)?;
        let field = data.schema.field(i);
        ensure!(
            col.name == info.name,
            "column {i} is {:?}, manifest says {:?}",
            col.name,
            info.name
        );
        ensure!(
            manifest::type_name(col.data.column_type()) == info.type_,
            "{}: type mismatch",
            info.name
        );
        ensure!(
            field.is_nullable() == info.nullable,
            "{}: nullability mismatch",
            info.name
        );
        ensure!(
            col.null_count() as u32 == info.null_count,
            "{}: null_count mismatch",
            info.name
        );
        if let ColumnData::DictUtf8 { codes, dictionary } = &col.data {
            ensure!(
                Some(dictionary.len() as u32) == info.dictionary_size,
                "{}: dictionary size mismatch",
                info.name
            );
            ensure!(
                dictionary
                    .windows(2)
                    .all(|w| w[0].as_bytes() < w[1].as_bytes()),
                "{}: dictionary is not strictly sorted",
                info.name
            );
            ensure!(
                (0..codes.len())
                    .all(|r| !col.is_valid(r) || (codes[r] as usize) < dictionary.len()),
                "{}: dictionary code out of range",
                info.name
            );
        }
        let h = hash_column(&col);
        for (k, (got, want)) in h.chunks.iter().zip(&info.chunk_hashes).enumerate() {
            ensure!(
                &got.to_hex() == want,
                "{}: chunk {k} hash mismatch",
                info.name
            );
        }
        ensure!(
            h.chunks.len() == info.chunk_hashes.len(),
            "{}: chunk count mismatch",
            info.name
        );
        ensure!(
            h.dictionary.map(|d| d.to_hex()) == info.dictionary_hash,
            "{}: dictionary hash mismatch",
            info.name
        );
        ensure!(
            h.column.to_hex() == info.column_hash,
            "{}: column hash mismatch",
            info.name
        );
        chunks_checked += h.chunks.len();
        column_hashes.push(h.column);
    }

    check_sorted(&data, &m)?;

    let (log_table, log) = arrow_io::read_cleaning_log(&dir.join(CLEANING_LOG_FILE))?;
    ensure!(
        log_table.meta(META_SNAPSHOT_HASH) == Some(m.snapshot_hash.as_str()),
        "cleaning_log.arrow belongs to another snapshot"
    );
    ensure!(
        log.len() as u32 == m.cleaning.cleaning_log_rows,
        "cleaning_log_rows mismatch"
    );
    let log_hash = clean::cleaning_log_hash(&log);
    ensure!(
        log_hash.to_hex() == m.cleaning.cleaning_log_hash,
        "cleaning_log_hash mismatch"
    );

    let (rej_table, rejects) = arrow_io::read_rejects(&dir.join(REJECTS_FILE))?;
    ensure!(
        rej_table.meta(META_SNAPSHOT_HASH) == Some(m.snapshot_hash.as_str()),
        "rejects.arrow belongs to another snapshot"
    );
    ensure!(
        rejects.len() as u32 == m.cleaning.rejected_rows,
        "rejected_rows mismatch"
    );
    let rejects_hash = clean::rejects_hash(&rejects);
    ensure!(
        rejects_hash.to_hex() == m.cleaning.rejects_hash,
        "rejects_hash mismatch"
    );

    let descriptor = manifest::descriptor(
        &m.scope.predicate,
        &m.sort_key,
        &m.source.portal,
        &m.source.dataset_id,
        m.cleaning.rules_version,
    )?;
    let snapshot = snapshot_hash(
        m.source.source_id,
        m.row_count,
        &descriptor,
        &column_hashes,
        &[log_hash, rejects_hash],
    );
    if snapshot.to_hex() != m.snapshot_hash {
        bail!(
            "snapshot_hash mismatch: files hash to {snapshot}, manifest says {}",
            m.snapshot_hash
        );
    }
    ensure!(
        ContentHash::from_hex(&m.snapshot_hash).is_some(),
        "malformed snapshot_hash"
    );
    Ok(Verified {
        columns_checked: column_hashes.len(),
        chunks_checked,
        manifest: m,
    })
}

/// Rows must be in strictly increasing `sort_key` order (the key makes ties
/// impossible), because RowIds are assigned from that order.
fn check_sorted(data: &ReadTable, m: &Manifest) -> Result<()> {
    let columns = m
        .sort_key
        .iter()
        .map(|name| {
            let i = m
                .schema
                .iter()
                .position(|c| &c.name == name)
                .with_context(|| format!("sort key {name} not in schema"))?;
            match data.column(i)?.data {
                ColumnData::I64(v) | ColumnData::Timestamp(v) => Ok(v),
                _ => bail!("sort key {name} is not an integer or timestamp column"),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    for r in 1..m.row_count as usize {
        let prev: Vec<i64> = columns.iter().map(|c| c[r - 1]).collect();
        let cur: Vec<i64> = columns.iter().map(|c| c[r]).collect();
        ensure!(
            prev < cur,
            "rows {} and {r} are not in sort_key order",
            r - 1
        );
    }
    Ok(())
}
