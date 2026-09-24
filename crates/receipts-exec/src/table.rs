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
