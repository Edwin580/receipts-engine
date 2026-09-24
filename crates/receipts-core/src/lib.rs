//! Core types shared by every engine crate: row identity, column types, and
//! (from M1) the column store and content hashing.

mod row_id;
mod types;

pub use row_id::{RowId, SourceId};
pub use types::ColumnType;
