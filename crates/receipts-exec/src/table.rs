use receipts_core::{Bitmap, Column, ColumnData};
use receipts_plan::Schema;
use std::sync::Arc;

/// A materialized node result: equal-length columns matching `schema`.
/// Columns are shared, so steps that pass a column through don't copy it.
#[derive(Clone, Debug)]
pub struct Table {
    schema: Schema,
    columns: Vec<Arc<Column>>,
    len: usize,
}

impl Table {
    /// Checks that the columns match the schema: same count, names, types,
    /// and no nulls in a non-nullable column.
    pub fn new(schema: Schema, columns: Vec<Arc<Column>>) -> Result<Self, String> {
        if schema.len() != columns.len() {
            return Err(format!(
                "schema has {} columns, table has {}",
                schema.len(),
                columns.len()
            ));
        }
        let len = columns.first().map_or(0, |c| c.len());
        for (f, c) in schema.fields.iter().zip(&columns) {
            if c.name != f.name {
                return Err(format!("column {:?} where schema has {:?}", c.name, f.name));
            }
            if c.data.column_type() != f.ty {
                return Err(format!("column {:?} has the wrong type", f.name));
            }
            if c.len() != len {
                return Err(format!(
                    "column {:?} has {} rows, expected {len}",
                    f.name,
                    c.len()
                ));
            }
            if !f.nullable && c.null_count() > 0 {
                return Err(format!("column {:?} has nulls but isn't nullable", f.name));
            }
        }
        Ok(Self {
            schema,
            columns,
            len,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn columns(&self) -> &[Arc<Column>] {
        &self.columns
    }

    pub fn column(&self, name: &str) -> Option<&Arc<Column>> {
        self.schema.index_of(name).map(|i| &self.columns[i])
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// The given rows, in the given order.
    pub fn gather(&self, rows: &[u32]) -> Table {
        let columns = self
            .columns
            .iter()
            .map(|c| Arc::new(gather(c, rows)))
            .collect();
        Table {
            schema: self.schema.clone(),
            columns,
            len: rows.len(),
        }
    }

    /// Builds a table from rows of several tables with the same schema:
    /// output row `i` is row `picks[i].1` of `tables[picks[i].0]`. Text
    /// columns with different dictionaries are merged into one sorted
    /// dictionary.
    pub fn interleave(tables: &[&Table], picks: &[(usize, u32)]) -> Result<Table, String> {
        let first = tables.first().ok_or("nothing to interleave")?;
        if tables.iter().any(|t| t.schema != first.schema) {
            return Err("interleaved tables must share a schema".into());
        }
        let columns = (0..first.columns.len())
            .map(|ci| {
                let cols: Vec<&Column> = tables.iter().map(|t| t.columns[ci].as_ref()).collect();
                Arc::new(interleave_column(&cols, picks))
            })
            .collect();
        Table::new(first.schema.clone(), columns)
    }
}

fn interleave_column(cols: &[&Column], picks: &[(usize, u32)]) -> Column {
    let valid = |&(t, r): &(usize, u32)| cols[t].is_valid(r as usize);
    let validity = validity_from(picks.len(), |i| valid(&picks[i]));
    fn take<T: Copy>(parts: &[&Vec<T>], picks: &[(usize, u32)]) -> Vec<T> {
        picks.iter().map(|&(t, r)| parts[t][r as usize]).collect()
    }
    let data =
        match &cols[0].data {
            ColumnData::I64(_) => ColumnData::I64(take(
                &cols
                    .iter()
                    .map(|c| match &c.data {
                        ColumnData::I64(v) => v,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
                picks,
            )),
            ColumnData::Timestamp(_) => ColumnData::Timestamp(take(
                &cols
                    .iter()
                    .map(|c| match &c.data {
                        ColumnData::Timestamp(v) => v,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
                picks,
            )),
            ColumnData::F64(_) => ColumnData::F64(take(
                &cols
                    .iter()
                    .map(|c| match &c.data {
                        ColumnData::F64(v) => v,
                        _ => unreachable!(),
                    })
                    .collect::<Vec<_>>(),
                picks,
            )),
            ColumnData::Bool(_) => ColumnData::Bool(Bitmap::from_bools(picks.iter().map(
                |&(t, r)| match &cols[t].data {
                    ColumnData::Bool(b) => b.get(r as usize),
                    _ => unreachable!(),
                },
            ))),
            ColumnData::Geo { .. } => {
                let part = |t: usize| match &cols[t].data {
                    ColumnData::Geo { lat, lon } => (lat, lon),
                    _ => unreachable!(),
                };
                ColumnData::Geo {
                    lat: picks.iter().map(|&(t, r)| part(t).0[r as usize]).collect(),
                    lon: picks.iter().map(|&(t, r)| part(t).1[r as usize]).collect(),
                }
            }
            ColumnData::DictUtf8 { .. } => {
                let dicts: Vec<(&Vec<u32>, &Vec<String>)> = cols
                    .iter()
                    .map(|c| match &c.data {
                        ColumnData::DictUtf8 { codes, dictionary } => (codes, dictionary),
                        _ => unreachable!(),
                    })
                    .collect();
                if dicts.iter().all(|(_, d)| *d == dicts[0].1) {
                    ColumnData::DictUtf8 {
                        codes: picks.iter().map(|&(t, r)| dicts[t].0[r as usize]).collect(),
                        dictionary: dicts[0].1.clone(),
                    }
                } else {
                    // Union of the dictionaries, still sorted, so code order
                    // stays string order.
                    let mut dictionary: Vec<String> =
                        dicts.iter().flat_map(|(_, d)| d.iter().cloned()).collect();
                    dictionary.sort();
                    dictionary.dedup();
                    let remap: Vec<Vec<u32>> = dicts
                        .iter()
                        .map(|(_, d)| {
                            d.iter()
                                .map(|s| dictionary.binary_search(s).expect("in union") as u32)
                                .collect()
                        })
                        .collect();
                    ColumnData::DictUtf8 {
                        codes: picks
                            .iter()
                            .map(|&(t, r)| remap[t][dicts[t].0[r as usize] as usize])
                            .collect(),
                        dictionary,
                    }
                }
            }
        };
    Column::new(cols[0].name.clone(), data, validity)
}

/// Validity bitmap from a per-row predicate, or `None` when every row is
/// valid (the `Column` convention).
pub(crate) fn validity_from(n: usize, valid: impl Fn(usize) -> bool) -> Option<Bitmap> {
    let bitmap = Bitmap::from_bools((0..n).map(valid));
    (bitmap.count_zeros() > 0).then_some(bitmap)
}

/// `out[i] = col[indices[i]]`.
pub(crate) fn gather(col: &Column, indices: &[u32]) -> Column {
    gather_opt(col, indices.iter().map(|&i| Some(i)), indices.len())
}

/// Like `gather`, with `None` producing a null.
pub(crate) fn gather_opt(
    col: &Column,
    indices: impl Iterator<Item = Option<u32>> + Clone,
    n: usize,
) -> Column {
    fn pick<T: Copy + Default>(v: &[T], idx: impl Iterator<Item = Option<u32>>) -> Vec<T> {
        idx.map(|i| i.map_or(T::default(), |i| v[i as usize]))
            .collect()
    }
    let data = match &col.data {
        ColumnData::I64(v) => ColumnData::I64(pick(v, indices.clone())),
        ColumnData::Timestamp(v) => ColumnData::Timestamp(pick(v, indices.clone())),
        ColumnData::F64(v) => ColumnData::F64(pick(v, indices.clone())),
        ColumnData::Bool(b) => ColumnData::Bool(Bitmap::from_bools(
            indices
                .clone()
                .map(|i| i.is_some_and(|i| b.get(i as usize))),
        )),
        ColumnData::DictUtf8 { codes, dictionary } => ColumnData::DictUtf8 {
            codes: pick(codes, indices.clone()),
            dictionary: dictionary.clone(),
        },
        ColumnData::Geo { lat, lon } => ColumnData::Geo {
            lat: pick(lat, indices.clone()),
            lon: pick(lon, indices.clone()),
        },
    };
    let has_missing = indices.clone().any(|i| i.is_none());
    let validity = if col.validity.is_none() && !has_missing {
        None
    } else {
        let bits = Bitmap::from_bools(indices.map(|i| i.is_some_and(|i| col.is_valid(i as usize))));
        (bits.count_zeros() > 0).then_some(bits)
    };
    debug_assert_eq!(data.len(), n);
    Column::new(col.name.clone(), data, validity)
}

pub(crate) fn renamed(col: Column, name: &str) -> Column {
    Column {
        name: name.into(),
        ..col
    }
}
