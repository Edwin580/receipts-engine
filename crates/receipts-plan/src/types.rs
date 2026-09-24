use receipts_core::ColumnType;
use receipts_core::time::format_naive_timestamp;
use std::fmt;

/// One column of a node's output.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Field {
    pub name: String,
    pub ty: ColumnType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, ty: ColumnType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
        }
    }
}

/// The ordered columns a node produces. Names are unique.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// A constant in an expression.
///
/// `F64` is always finite: NaN and infinities can't be written in JSON and
/// are refused when a plan is parsed.
#[derive(Clone, PartialEq, Debug)]
pub enum Literal {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    Str(String),
    /// Naive NYC wall-clock microseconds, like `ColumnType::Timestamp`.
    Timestamp(i64),
}

impl Literal {
    /// `None` for `Null`, which has no type of its own.
    pub fn value_type(&self) -> Option<ValueType> {
        Some(match self {
            Self::Null => return None,
            Self::Bool(_) => ValueType::Bool,
            Self::I64(_) => ValueType::I64,
            Self::F64(_) => ValueType::F64,
            Self::Str(_) => ValueType::Str,
            Self::Timestamp(_) => ValueType::Timestamp,
        })
    }
}

impl fmt::Display for Literal {
    /// The plain-English form used by `describe()`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("missing"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::I64(i) => write!(f, "{i}"),
            Self::F64(x) => f.write_str(&receipts_core::canonical_json::format_es_number(*x)),
            Self::Str(s) => write!(f, "\"{s}\""),
            Self::Timestamp(t) => {
                let s = format_naive_timestamp(*t);
                let s = s.strip_suffix(".000000").unwrap_or(&s);
                // Midnight reads better as a bare date.
                match s.strip_suffix("T00:00:00") {
                    Some(date) => f.write_str(date),
                    None => f.write_str(&s.replace('T', " ")),
                }
            }
        }
    }
}

/// The type of an expression's value. Strings are `Str` whether they come
/// from a dictionary column or a literal; a computed string column is stored
/// as `DictUtf8`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ValueType {
    I64,
    F64,
    Bool,
    Timestamp,
    Str,
    Geo,
}

impl ValueType {
    pub fn of_column(ty: ColumnType) -> Self {
        match ty {
            ColumnType::I64 => Self::I64,
            ColumnType::F64 => Self::F64,
            ColumnType::Bool => Self::Bool,
            ColumnType::Timestamp => Self::Timestamp,
            ColumnType::DictUtf8 => Self::Str,
            ColumnType::Geo => Self::Geo,
        }
    }

    pub fn column_type(self) -> ColumnType {
        match self {
            Self::I64 => ColumnType::I64,
            Self::F64 => ColumnType::F64,
            Self::Bool => ColumnType::Bool,
            Self::Timestamp => ColumnType::Timestamp,
            Self::Str => ColumnType::DictUtf8,
            Self::Geo => ColumnType::Geo,
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, Self::I64 | Self::F64)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::I64 => "integer",
            Self::F64 => "decimal",
            Self::Bool => "true/false",
            Self::Timestamp => "timestamp",
            Self::Str => "text",
            Self::Geo => "location",
        }
    }
}
