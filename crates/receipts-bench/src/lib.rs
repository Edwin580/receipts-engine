//! Benchmarks on real snapshots. The binaries in `src/bin/` time each
//! milestone's operations with `std::time`; criterion benches wait for that
//! dependency to be approved.

use anyhow::{Context, Result};
use receipts_core::{Column, ContentHash, SourceId};
use receipts_exec::{SourceProvider, Table};
use receipts_plan::{Catalog, Field, Schema, SourceInfo};
use receipts_snapshot::arrow_io::ReadTable;
use receipts_snapshot::manifest::DATA_FILE;
use std::path::Path;
use std::sync::Arc;

/// A verified snapshot loaded into memory, usable as both the plan catalog
/// and the executor's source.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub snapshot: ContentHash,
    pub source_id: SourceId,
    pub title: String,
    pub table: Arc<Table>,
}

impl Catalog for Loaded {
    fn source(&self, s: &ContentHash) -> Option<SourceInfo> {
        (*s == self.snapshot).then(|| SourceInfo {
            title: self.title.clone(),
            schema: self.table.schema().clone(),
        })
    }
}

impl SourceProvider for Loaded {
    fn source_id(&self, _: &ContentHash) -> SourceId {
        self.source_id
    }
    fn table(&self, s: &ContentHash) -> Option<Arc<Table>> {
        (*s == self.snapshot).then(|| self.table.clone())
    }
}

/// Verifies every hash in the snapshot directory, then loads `data.arrow`.
pub fn load(dir: &Path) -> Result<Loaded> {
    let verified = receipts_snapshot::verify::verify(dir).context("verifying snapshot")?;
    let m = verified.manifest;
    let data = ReadTable::open(&dir.join(DATA_FILE))?;
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for (i, info) in m.schema.iter().enumerate() {
        let col: Column = data.column(i)?;
        fields.push(Field::new(
            info.name.clone(),
            col.data.column_type(),
            info.nullable,
        ));
        columns.push(Arc::new(col));
    }
    let table = Table::new(Schema::new(fields), columns).map_err(anyhow::Error::msg)?;
    Ok(Loaded {
        snapshot: ContentHash::from_hex(&m.snapshot_hash).context("bad snapshot hash")?,
        source_id: SourceId(m.source.source_id),
        title: m.source.dataset,
        table: Arc::new(table),
    })
}
