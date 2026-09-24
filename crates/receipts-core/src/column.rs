use crate::{Bitmap, ColumnType};

/// Values of one column, in the physical layout of `docs/snapshot/schema.md`.
///
/// This is the minimal representation the snapshot builder needs for writing
/// and hashing. M1 builds the engine's column store on top of it.
#[derive(Clone, PartialEq, Debug)]
pub enum ColumnData {
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bool(Bitmap),
    Timestamp(Vec<i64>),
    /// `codes[i]` indexes `dictionary`. The dictionary is sorted by byte order
    /// and has no duplicates, so code order is string order.
    DictUtf8 {
        codes: Vec<u32>,
        dictionary: Vec<String>,
    },
    Geo {
        lat: Vec<f32>,
        lon: Vec<f32>,
    },
}

impl ColumnData {
    pub fn column_type(&self) -> ColumnType {
        match self {
            Self::I64(_) => ColumnType::I64,
            Self::F64(_) => ColumnType::F64,
            Self::Bool(_) => ColumnType::Bool,
            Self::Timestamp(_) => ColumnType::Timestamp,
            Self::DictUtf8 { .. } => ColumnType::DictUtf8,
            Self::Geo { .. } => ColumnType::Geo,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::I64(v) | Self::Timestamp(v) => v.len(),
            Self::F64(v) => v.len(),
            Self::Bool(b) => b.len(),
            Self::DictUtf8 { codes, .. } => codes.len(),
            Self::Geo { lat, .. } => lat.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A named column with an optional validity bitmap (`None` = no nulls).
#[derive(Clone, PartialEq, Debug)]
pub struct Column {
    pub name: String,
    pub data: ColumnData,
    pub validity: Option<Bitmap>,
}

impl Column {
    /// # Panics
    /// If the validity bitmap length differs from the data length, or if a
    /// geo column's lat and lon lengths differ.
    pub fn new(name: impl Into<String>, data: ColumnData, validity: Option<Bitmap>) -> Self {
        if let Some(v) = &validity {
            assert_eq!(
                v.len(),
                data.len(),
                "validity length must match data length"
            );
        }
        if let ColumnData::Geo { lat, lon } = &data {
            assert_eq!(lat.len(), lon.len(), "geo lat/lon lengths must match");
        }
        Self {
            name: name.into(),
            data,
            validity,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn is_valid(&self, row: usize) -> bool {
        self.validity.as_ref().is_none_or(|v| v.get(row))
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, Bitmap::count_zeros)
    }
}
