/// Logical column types. Physical layouts are described in
/// `docs/snapshot/schema.md`; every type carries a validity bitmap for nulls.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ColumnType {
    I64,
    F64,
    Bool,
    /// Microseconds since 1970-01-01T00:00:00 of a *naive* wall-clock time
    /// (no time zone), matching DuckDB `TIMESTAMP`. See ADR 0003.
    Timestamp,
    /// UTF-8 strings, dictionary-encoded with `u32` codes into a dictionary
    /// sorted by byte order, so code order equals string order.
    DictUtf8,
    /// Latitude and longitude as two `f32` arrays sharing one validity bitmap.
    Geo,
}
