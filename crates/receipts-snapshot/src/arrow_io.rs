//! Reading and writing the snapshot's Arrow IPC files (ADR 0001).
//!
//! `data.arrow` has one record batch per 65,536-row chunk. Dictionary columns
//! share one dictionary across all batches, so the IPC file carries it once.

use crate::clean::{LogEntry, Reject};
use crate::manifest::FORMAT_VERSION;
use crate::rules::Rule;
use anyhow::{Context, Result, bail, ensure};
use arrow_array::types::UInt32Type;
use arrow_array::{
    Array, ArrayRef, BooleanArray, DictionaryArray, Float32Array, Float64Array, Int64Array,
    RecordBatch, StringArray, StructArray, TimestampMicrosecondArray, UInt16Array, UInt32Array,
};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer};
use arrow_ipc::CompressionType;
use arrow_ipc::reader::FileReader;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use receipts_core::hash::chunk_ranges;
use receipts_core::{Bitmap, Column, ColumnData, ContentHash};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;

pub const META_FORMAT: &str = "receipts.format_version";
pub const META_TABLE: &str = "receipts.table";
pub const META_SNAPSHOT_HASH: &str = "receipts.snapshot_hash";

/// How Arrow IPC buffers are stored. The snapshot hash covers logical
/// content, so compression never changes it (ADR 0001).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Compression {
    None,
    /// Each buffer is one LZ4 frame; decoded in the browser (M5).
    #[default]
    Lz4,
}

impl Compression {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4_frame",
        }
    }
}

fn metadata(table: &str, snapshot_hash: &ContentHash) -> HashMap<String, String> {
    HashMap::from([
        (META_FORMAT.to_string(), FORMAT_VERSION.to_string()),
        (META_TABLE.to_string(), table.to_string()),
        (META_SNAPSHOT_HASH.to_string(), snapshot_hash.to_hex()),
    ])
}

fn null_buffer(b: Option<&Bitmap>) -> Option<NullBuffer> {
    b.map(|b| {
        NullBuffer::new(BooleanBuffer::new(
            Buffer::from(b.as_bytes().to_vec()),
            0,
            b.len(),
        ))
    })
}

fn geo_fields() -> Fields {
    Fields::from(vec![
        Field::new("lat", DataType::Float32, false),
        Field::new("lon", DataType::Float32, false),
    ])
}

fn to_arrow(col: &Column, nullable: bool) -> Result<(Field, ArrayRef)> {
    let nulls = null_buffer(col.validity.as_ref());
    let array: ArrayRef = match &col.data {
        ColumnData::I64(v) => Arc::new(Int64Array::new(v.clone().into(), nulls)),
        ColumnData::F64(v) => Arc::new(Float64Array::new(v.clone().into(), nulls)),
        ColumnData::Bool(b) => Arc::new(BooleanArray::new(
            BooleanBuffer::new(Buffer::from(b.as_bytes().to_vec()), 0, b.len()),
            nulls,
        )),
        ColumnData::Timestamp(v) => {
            Arc::new(TimestampMicrosecondArray::new(v.clone().into(), nulls))
        }
        ColumnData::DictUtf8 { codes, dictionary } => {
            let keys = UInt32Array::new(codes.clone().into(), nulls);
            let values = Arc::new(StringArray::from(dictionary.clone()));
            Arc::new(DictionaryArray::<UInt32Type>::try_new(keys, values)?)
        }
        ColumnData::Geo { lat, lon } => Arc::new(StructArray::try_new(
            geo_fields(),
            vec![
                Arc::new(Float32Array::from(lat.clone())),
                Arc::new(Float32Array::from(lon.clone())),
            ],
            nulls,
        )?),
    };
    ensure!(
        nullable || col.null_count() == 0,
        "column {} is not nullable but has nulls",
        col.name
    );
    Ok((
        Field::new(&col.name, array.data_type().clone(), nullable),
        array,
    ))
}

/// Writes `data.arrow`: one record batch per chunk.
pub fn write_data(
    path: &Path,
    columns: &[(&Column, bool)],
    snapshot_hash: &ContentHash,
    compression: Compression,
) -> Result<()> {
    let (fields, arrays): (Vec<_>, Vec<_>) = columns
        .iter()
        .map(|(c, nullable)| to_arrow(c, *nullable))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .unzip();
    let schema: SchemaRef = Arc::new(Schema::new_with_metadata(
        fields,
        metadata("data", snapshot_hash),
    ));
    let rows = columns.first().map_or(0, |(c, _)| c.len());
    let batches = chunk_ranges(rows).map(|r| {
        let cols = arrays.iter().map(|a| a.slice(r.start, r.len())).collect();
        RecordBatch::try_new(schema.clone(), cols)
    });
    write_file(path, &schema, batches, compression)
}

fn write_file(
    path: &Path,
    schema: &Schema,
    batches: impl Iterator<Item = Result<RecordBatch, arrow_schema::ArrowError>>,
    compression: Compression,
) -> Result<()> {
    let file =
        BufWriter::new(File::create(path).with_context(|| format!("creating {}", path.display()))?);
    let options = IpcWriteOptions::default().try_with_compression(match compression {
        Compression::None => None,
        Compression::Lz4 => Some(CompressionType::LZ4_FRAME),
    })?;
    let mut writer = FileWriter::try_new_with_options(file, schema, options)?;
    for batch in batches {
        writer.write(&batch?)?;
    }
    writer.finish()?;
    Ok(())
}

pub fn write_cleaning_log(
    path: &Path,
    log: &[LogEntry],
    snapshot_hash: &ContentHash,
    compression: Compression,
) -> Result<()> {
    let schema = Schema::new_with_metadata(
        vec![
            Field::new("row_index", DataType::UInt32, false),
            Field::new("column", DataType::UInt16, false),
            Field::new("rule_id", DataType::Utf8, false),
            Field::new("raw_value", DataType::Utf8, true),
        ],
        metadata("cleaning_log", snapshot_hash),
    );
    let schema = Arc::new(schema);
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from_iter_values(
                log.iter().map(|e| e.row_index),
            )),
            Arc::new(UInt16Array::from_iter_values(log.iter().map(|e| e.column))),
            Arc::new(StringArray::from_iter_values(
                log.iter().map(|e| e.rule.id()),
            )),
            Arc::new(StringArray::from_iter(
                log.iter().map(|e| e.raw_value.as_deref()),
            )),
        ],
    );
    write_file(path, &schema, std::iter::once(batch), compression)
}

pub fn write_rejects(
    path: &Path,
    rejects: &[Reject],
    snapshot_hash: &ContentHash,
    compression: Compression,
) -> Result<()> {
    let schema = Arc::new(Schema::new_with_metadata(
        vec![
            Field::new("reject_index", DataType::UInt32, false),
            Field::new("raw_unique_key", DataType::Utf8, true),
            Field::new("rule_id", DataType::Utf8, false),
            Field::new("raw_record", DataType::Utf8, false),
        ],
        metadata("rejects", snapshot_hash),
    ));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from_iter_values(0..rejects.len() as u32)),
            Arc::new(StringArray::from_iter(
                rejects.iter().map(|r| r.raw_unique_key.as_deref()),
            )),
            Arc::new(StringArray::from_iter_values(
                rejects.iter().map(|r| r.rule.id()),
            )),
            Arc::new(StringArray::from_iter_values(
                rejects.iter().map(|r| r.raw_record.as_str()),
            )),
        ],
    );
    write_file(path, &schema, std::iter::once(batch), compression)
}

/// A decoded Arrow file.
#[derive(Debug)]
pub struct ReadTable {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

impl ReadTable {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let reader = FileReader::try_new(file, None)
            .with_context(|| format!("reading {}", path.display()))?;
        let schema = reader.schema();
        let batches = reader.collect::<Result<Vec<_>, _>>()?;
        Ok(Self { schema, batches })
    }

    pub fn meta(&self, key: &str) -> Option<&str> {
        self.schema.metadata().get(key).map(String::as_str)
    }

    pub fn batch_lengths(&self) -> Vec<usize> {
        self.batches.iter().map(RecordBatch::num_rows).collect()
    }

    /// Reassembles column `index` from all batches.
    pub fn column(&self, index: usize) -> Result<Column> {
        let field = self.schema.field(index);
        let parts: Vec<&ArrayRef> = self.batches.iter().map(|b| b.column(index)).collect();
        let mut valid = Bitmap::new();
        for p in &parts {
            for i in 0..p.len() {
                valid.push(p.is_valid(i));
            }
        }
        let data = match field.data_type() {
            DataType::Int64 => {
                ColumnData::I64(concat::<Int64Array, _>(&parts, |a| a.values().to_vec())?)
            }
            DataType::Float64 => {
                ColumnData::F64(concat::<Float64Array, _>(&parts, |a| a.values().to_vec())?)
            }
            DataType::Boolean => ColumnData::Bool(Bitmap::from_bools(concat::<BooleanArray, _>(
                &parts,
                |a| a.values().iter().collect(),
            )?)),
            DataType::Timestamp(TimeUnit::Microsecond, None) => {
                ColumnData::Timestamp(concat::<TimestampMicrosecondArray, _>(&parts, |a| {
                    a.values().to_vec()
                })?)
            }
            DataType::Dictionary(k, v) if **k == DataType::UInt32 && **v == DataType::Utf8 => {
                let mut dictionary: Option<Vec<String>> = None;
                let mut codes = Vec::new();
                for p in &parts {
                    let d = downcast::<DictionaryArray<UInt32Type>>(p)?;
                    let values = downcast::<StringArray>(d.values())?;
                    let values: Vec<String> = values
                        .iter()
                        .map(|s| s.unwrap_or_default().to_string())
                        .collect();
                    match &dictionary {
                        None => dictionary = Some(values),
                        Some(prev) => ensure!(
                            *prev == values,
                            "column {} changes dictionary between batches",
                            field.name()
                        ),
                    }
                    codes.extend_from_slice(d.keys().values());
                }
                ColumnData::DictUtf8 {
                    codes,
                    dictionary: dictionary.unwrap_or_default(),
                }
            }
            DataType::Struct(f) if *f == geo_fields() => {
                let mut lat = Vec::new();
                let mut lon = Vec::new();
                for p in &parts {
                    let s = downcast::<StructArray>(p)?;
                    lat.extend_from_slice(downcast::<Float32Array>(s.column(0))?.values());
                    lon.extend_from_slice(downcast::<Float32Array>(s.column(1))?.values());
                }
                ColumnData::Geo { lat, lon }
            }
            other => bail!("column {} has unsupported type {other}", field.name()),
        };
        let validity = (valid.count_zeros() > 0).then_some(valid);
        Ok(Column::new(field.name().as_str(), data, validity))
    }

    fn strings(&self, index: usize) -> Result<Vec<Option<String>>> {
        let mut out = Vec::new();
        for b in &self.batches {
            out.extend(
                downcast::<StringArray>(b.column(index))?
                    .iter()
                    .map(|s| s.map(str::to_string)),
            );
        }
        Ok(out)
    }

    fn u32s(&self, index: usize) -> Result<Vec<u32>> {
        concat::<UInt32Array, _>(
            &self
                .batches
                .iter()
                .map(|b| b.column(index))
                .collect::<Vec<_>>(),
            |a| a.values().to_vec(),
        )
    }
}

fn downcast<T: 'static>(a: &ArrayRef) -> Result<&T> {
    a.as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("unexpected array type {}", a.data_type()))
}

fn concat<T: 'static, V>(parts: &[&ArrayRef], f: impl Fn(&T) -> Vec<V>) -> Result<Vec<V>> {
    let mut out = Vec::new();
    for p in parts {
        out.extend(f(downcast::<T>(p)?));
    }
    Ok(out)
}

pub fn read_cleaning_log(path: &Path) -> Result<(ReadTable, Vec<LogEntry>)> {
    let t = ReadTable::open(path)?;
    let rows = t.u32s(0)?;
    let cols = concat::<UInt16Array, _>(
        &t.batches.iter().map(|b| b.column(1)).collect::<Vec<_>>(),
        |a| a.values().to_vec(),
    )?;
    let rules = t.strings(2)?;
    let raws = t.strings(3)?;
    let mut log = Vec::with_capacity(rows.len());
    for i in 0..rows.len() {
        log.push(LogEntry {
            row_index: rows[i],
            column: cols[i],
            rule: rule(&rules[i])?,
            raw_value: raws[i].clone(),
        });
    }
    Ok((t, log))
}

pub fn read_rejects(path: &Path) -> Result<(ReadTable, Vec<Reject>)> {
    let t = ReadTable::open(path)?;
    let index = t.u32s(0)?;
    let keys = t.strings(1)?;
    let rules = t.strings(2)?;
    let raws = t.strings(3)?;
    let mut out = Vec::with_capacity(keys.len());
    for i in 0..keys.len() {
        ensure!(
            index[i] == i as u32,
            "rejects.arrow: reject_index is not 0..n"
        );
        out.push(Reject {
            raw_unique_key: keys[i].clone(),
            rule: rule(&rules[i])?,
            raw_record: raws[i].clone().context("rejects.arrow: null raw_record")?,
        });
    }
    Ok((t, out))
}

fn rule(id: &Option<String>) -> Result<Rule> {
    let id = id.as_deref().context("null rule_id")?;
    Rule::from_id(id).with_context(|| format!("unknown rule id {id:?}"))
}
