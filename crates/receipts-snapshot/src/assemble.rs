//! `build`: raw directory → snapshot directory.

use crate::arrow_io::{self, Compression};
use crate::clean::{self, Cleaned};
use crate::known_issues::known_issues;
use crate::manifest::{
    self, Build, CLEANING_LOG_FILE, Cleaning, ColumnInfo, DATA_FILE, Excluded, FORMAT_VERSION,
    Fetch, FileInfo, MANIFEST_FILE, Manifest, REJECTS_FILE, RuleCount, ScopeInfo, Source,
};
use crate::raw::RawDir;
use crate::rules::{RULES_VERSION, Rule};
use crate::socrata::{ViewMetadata, check_schema};
use crate::spec::DatasetSpec;
use anyhow::{Context, Result, ensure};
use receipts_core::hash::{CHUNK_ROWS, hash_column, snapshot_hash};
use receipts_core::time::MICROS_PER_SECOND;
use receipts_core::{ColumnData, ContentHash};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Built {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

/// Builds a snapshot from `raw` into `<out_root>/<snapshot_hash[..16]>/`,
/// with LZ4-compressed Arrow files.
pub fn build(spec: &DatasetSpec, raw_dir: &Path, out_root: &Path) -> Result<Built> {
    build_with(spec, raw_dir, out_root, Compression::default())
}

/// [`build`] with a choice of file compression. The snapshot hash is the
/// same either way.
pub fn build_with(
    spec: &DatasetSpec,
    raw_dir: &Path,
    out_root: &Path,
    compression: Compression,
) -> Result<Built> {
    let mut phase = Phases::start();
    let raw = RawDir::open(raw_dir)?;
    phase.done("check raw pages");
    ensure!(
        raw.fetch.portal == spec.portal && raw.fetch.dataset_id == spec.dataset_id,
        "raw directory is {}/{}, not {}/{}",
        raw.fetch.portal,
        raw.fetch.dataset_id,
        spec.portal,
        spec.dataset_id
    );
    let meta = ViewMetadata::parse(&raw.metadata)?;
    check_schema(spec, &meta)?;
    let cleaned = clean::clean(spec, &raw)?;
    phase.done("parse + clean + sort");
    let row_count = u32::try_from(cleaned.row_count()).context("more than u32::MAX rows")?;

    let column_hashes: Vec<_> = cleaned.columns.iter().map(hash_column).collect();
    phase.done("hash columns");
    let log_hash = clean::cleaning_log_hash(&cleaned.log);
    let rejects_hash = clean::rejects_hash(&cleaned.rejects);
    let sort_key: Vec<String> = [spec.created_column(), spec.key_column()]
        .iter()
        .map(|&i| spec.columns[i].name.to_string())
        .collect();
    let descriptor = manifest::descriptor(
        &raw.fetch.scope,
        &sort_key,
        spec.portal,
        spec.dataset_id,
        RULES_VERSION,
    )?;
    let snapshot = snapshot_hash(
        spec.source_id,
        row_count,
        &descriptor,
        &column_hashes.iter().map(|h| h.column).collect::<Vec<_>>(),
        &[log_hash, rejects_hash],
    );

    let dir = out_root.join(&snapshot.to_hex()[..16]);
    ensure!(
        !dir.exists(),
        "{} already exists (same content hash prefix); nothing to do",
        dir.display()
    );
    let tmp = out_root.join(format!(".tmp-{}", snapshot.to_hex()));
    if tmp.exists() {
        fs::remove_dir_all(&tmp)?;
    }
    fs::create_dir_all(&tmp)?;

    let with_nullability: Vec<_> = cleaned
        .columns
        .iter()
        .zip(spec.columns.iter().map(|c| c.kind.nullable()))
        .collect();
    arrow_io::write_data(
        &tmp.join(DATA_FILE),
        &with_nullability,
        &snapshot,
        compression,
    )?;
    arrow_io::write_cleaning_log(
        &tmp.join(CLEANING_LOG_FILE),
        &cleaned.log,
        &snapshot,
        compression,
    )?;
    arrow_io::write_rejects(
        &tmp.join(REJECTS_FILE),
        &cleaned.rejects,
        &snapshot,
        compression,
    )?;
    phase.done("write arrow files");

    let schema = cleaned
        .columns
        .iter()
        .zip(spec.columns)
        .zip(&column_hashes)
        .enumerate()
        .map(|(i, ((col, cs), h))| ColumnInfo {
            index: i as u16,
            name: cs.name.to_string(),
            type_: manifest::type_name(cs.kind.column_type()).to_string(),
            nullable: cs.kind.nullable(),
            source_fields: cs.source_fields.iter().map(|s| s.to_string()).collect(),
            description: cs.description.to_string(),
            null_count: col.null_count() as u32,
            dictionary_size: match &col.data {
                ColumnData::DictUtf8 { dictionary, .. } => Some(dictionary.len() as u32),
                _ => None,
            },
            dictionary_hash: h.dictionary.map(|d| d.to_hex()),
            column_hash: h.column.to_hex(),
            chunk_hashes: h.chunks.iter().map(ContentHash::to_hex).collect(),
        })
        .collect();

    let files = [DATA_FILE, CLEANING_LOG_FILE, REJECTS_FILE]
        .iter()
        .map(|f| {
            Ok(FileInfo {
                path: f.to_string(),
                bytes: fs::metadata(tmp.join(f))?.len(),
                compression: compression.name().to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let f = &raw.fetch;
    let mut m = Manifest {
        format_version: FORMAT_VERSION.into(),
        snapshot_hash: snapshot.to_hex(),
        manifest_hash: String::new(),
        source: Source {
            source_id: spec.source_id,
            dataset: spec.dataset.into(),
            portal: spec.portal.into(),
            dataset_id: spec.dataset_id.into(),
            source_url: spec.source_url.into(),
            terms_url: spec.terms_url.into(),
            rows_updated_at: meta.rows_updated_at_rfc3339(),
        },
        fetch: Fetch {
            started_at: f.started_at.clone(),
            finished_at: f.finished_at.clone(),
            endpoint: f.endpoint.clone(),
            select: f.select.clone(),
            where_: f.where_.clone(),
            order: f.order.clone(),
            pagination: f.pagination.clone(),
            pages: f.pages.len() as u32,
            raw_records: f.raw_records,
            raw_hash: f.raw_hash.clone(),
            metadata_hash: f.metadata_hash.clone(),
            rows_updated_at_start: f.rows_updated_at_start.clone(),
            rows_updated_at_end: f.rows_updated_at_end.clone(),
            tool_version: f.tool_version.clone(),
        },
        build: Build {
            tool_version: crate::TOOL_VERSION.into(),
        },
        scope: ScopeInfo {
            sentence: spec
                .scope_sentence
                .replace("{from}", &f.scope.gte[..10])
                .replace("{to}", &f.scope.lt[..10]),
            predicate: f.scope.clone(),
        },
        row_count,
        chunk_rows: CHUNK_ROWS as u32,
        chunk_count: row_count.div_ceil(CHUNK_ROWS as u32),
        sort_key,
        schema,
        excluded_columns: spec
            .excluded
            .iter()
            .map(|(field, reason)| Excluded {
                field: field.to_string(),
                reason: reason.to_string(),
            })
            .collect(),
        cleaning: Cleaning {
            rules_version: RULES_VERSION,
            rules: rule_counts(&cleaned),
            rejected_rows: cleaned.rejects.len() as u32,
            cleaning_log_rows: cleaned.log.len() as u32,
            cleaning_log_hash: log_hash.to_hex(),
            rejects_hash: rejects_hash.to_hex(),
        },
        known_issues: known_issues(&cleaned.columns, meta.rows_updated_at),
        files,
    };
    m.manifest_hash = manifest::manifest_hash(&serde_json::to_value(&m)?)?;
    fs::write(tmp.join(MANIFEST_FILE), serde_json::to_vec_pretty(&m)?)?;
    fs::rename(&tmp, &dir).with_context(|| format!("moving snapshot into {}", dir.display()))?;
    Ok(Built { dir, manifest: m })
}

fn rule_counts(c: &Cleaned) -> Vec<RuleCount> {
    Rule::ALL
        .iter()
        .map(|&rule| {
            let count = match rule {
                Rule::SchemaDrift => 0,
                Rule::LocationToF32 => c
                    .columns
                    .iter()
                    .filter(|col| matches!(col.data, ColumnData::Geo { .. }))
                    .map(|col| (col.len() - col.null_count()) as u64)
                    .sum(),
                Rule::FractionalSeconds => c
                    .columns
                    .iter()
                    .filter_map(|col| match &col.data {
                        ColumnData::Timestamp(v) => Some(
                            (0..v.len())
                                .filter(|&i| {
                                    col.is_valid(i) && v[i].rem_euclid(MICROS_PER_SECOND) != 0
                                })
                                .count() as u64,
                        ),
                        _ => None,
                    })
                    .sum(),
                r if r.logged() => c.log.iter().filter(|e| e.rule == r).count() as u64,
                r => c.rejects.iter().filter(|e| e.rule == r).count() as u64,
            };
            RuleCount {
                id: rule.id().into(),
                action: rule.action(),
                count,
                counts: rule.counts().into(),
                description: rule.description().into(),
            }
        })
        .collect()
}

/// Prints how long each build phase took, to stderr.
#[derive(Debug)]
struct Phases(std::time::Instant);

impl Phases {
    fn start() -> Self {
        Self(std::time::Instant::now())
    }

    fn done(&mut self, name: &str) {
        eprintln!("  {name:<22} {:>8.2?}", self.0.elapsed());
        self.0 = std::time::Instant::now();
    }
}
