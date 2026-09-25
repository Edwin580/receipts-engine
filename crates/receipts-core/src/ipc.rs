//! A minimal Arrow IPC *file* reader for the snapshot format (ADR 0001),
//! with no dependencies, so the WASM engine can load snapshots without the
//! arrow crates.
//!
//! It reads exactly what `receipts-snapshot` writes: little-endian record
//! batches of Int, FloatingPoint, Utf8, Bool, Timestamp and Struct columns,
//! and dictionary-encoded Utf8 (including delta dictionaries), either
//! uncompressed or with LZ4-frame buffer compression (ADR 0001). Anything else is an error. Every read is bounds-checked:
//! malformed input returns `Err`, never panics.
//!
//! Format references: Arrow columnar spec ("IPC File Format") and
//! `format/{File,Message,Schema}.fbs`.

use crate::{Bitmap, Column, ColumnData};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Read;

pub type Result<T> = std::result::Result<T, String>;

const MAGIC: &[u8] = b"ARROW1";

// ---- A bounds-checked FlatBuffers reader ----

/// Position of element `k` of `size` bytes from `start`, without overflow
/// (positions come from the file, and `usize` is 32 bits in WASM).
fn element(start: usize, k: usize, size: usize) -> Result<usize> {
    k.checked_mul(size)
        .and_then(|off| start.checked_add(off))
        .ok_or_else(|| "offset overflow".to_string())
}

#[derive(Clone, Copy)]
struct Buf<'a>(&'a [u8]);

impl<'a> Buf<'a> {
    fn bytes(self, pos: usize, len: usize) -> Result<&'a [u8]> {
        pos.checked_add(len)
            .and_then(|end| self.0.get(pos..end))
            .ok_or_else(|| format!("read of {len} bytes at {pos} is out of bounds"))
    }
    fn u8(self, pos: usize) -> Result<u8> {
        Ok(self.bytes(pos, 1)?[0])
    }
    fn u16(self, pos: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(
            self.bytes(pos, 2)?.try_into().expect("2 bytes"),
        ))
    }
    fn i16(self, pos: usize) -> Result<i16> {
        Ok(self.u16(pos)? as i16)
    }
    fn u32(self, pos: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.bytes(pos, 4)?.try_into().expect("4 bytes"),
        ))
    }
    fn i32(self, pos: usize) -> Result<i32> {
        Ok(self.u32(pos)? as i32)
    }
    fn i64(self, pos: usize) -> Result<i64> {
        Ok(i64::from_le_bytes(
            self.bytes(pos, 8)?.try_into().expect("8 bytes"),
        ))
    }
    /// Follows a `uoffset` stored at `pos`.
    fn follow(self, pos: usize) -> Result<usize> {
        pos.checked_add(self.u32(pos)? as usize)
            .ok_or_else(|| "offset overflow".to_string())
    }
    fn root(self) -> Result<Table<'a>> {
        Ok(Table {
            buf: self,
            pos: self.follow(0)?,
        })
    }
}

#[derive(Clone, Copy)]
struct Table<'a> {
    buf: Buf<'a>,
    pos: usize,
}

impl<'a> Table<'a> {
    /// Absolute position of field `id`, or `None` if absent.
    fn field(&self, id: usize) -> Result<Option<usize>> {
        let soffset = self.buf.i32(self.pos)? as i64;
        let vtable = usize::try_from(self.pos as i64 - soffset)
            .map_err(|_| "vtable offset out of range".to_string())?;
        let vt_len = self.buf.u16(vtable)? as usize;
        let slot = 4 + 2 * id;
        if slot + 2 > vt_len {
            return Ok(None);
        }
        let off = self.buf.u16(vtable + slot)? as usize;
        Ok((off != 0).then_some(self.pos + off))
    }
    fn i64(&self, id: usize, default: i64) -> Result<i64> {
        self.field(id)?.map_or(Ok(default), |p| self.buf.i64(p))
    }
    fn i32(&self, id: usize, default: i32) -> Result<i32> {
        self.field(id)?.map_or(Ok(default), |p| self.buf.i32(p))
    }
    fn i16(&self, id: usize, default: i16) -> Result<i16> {
        self.field(id)?.map_or(Ok(default), |p| self.buf.i16(p))
    }
    fn u8(&self, id: usize, default: u8) -> Result<u8> {
        self.field(id)?.map_or(Ok(default), |p| self.buf.u8(p))
    }
    fn bool(&self, id: usize) -> Result<bool> {
        Ok(self.u8(id, 0)? != 0)
    }
    fn table(&self, id: usize) -> Result<Option<Table<'a>>> {
        self.field(id)?
            .map(|p| {
                Ok(Table {
                    buf: self.buf,
                    pos: self.buf.follow(p)?,
                })
            })
            .transpose()
    }
    /// `(first element position, length)`.
    fn vector(&self, id: usize) -> Result<Option<(usize, usize)>> {
        self.field(id)?
            .map(|p| {
                let v = self.buf.follow(p)?;
                let len = self.buf.u32(v)? as usize;
                Ok((v + 4, len))
            })
            .transpose()
    }
    fn tables(&self, id: usize) -> Result<Vec<Table<'a>>> {
        let Some((start, n)) = self.vector(id)? else {
            return Ok(Vec::new());
        };
        (0..n)
            .map(|k| {
                Ok(Table {
                    buf: self.buf,
                    pos: self.buf.follow(element(start, k, 4)?)?,
                })
            })
            .collect()
    }
    fn str(&self, id: usize) -> Result<Option<&'a str>> {
        self.field(id)?
            .map(|p| {
                let s = self.buf.follow(p)?;
                let len = self.buf.u32(s)? as usize;
                std::str::from_utf8(self.buf.bytes(s + 4, len)?)
                    .map_err(|_| "string is not UTF-8".to_string())
            })
            .transpose()
    }
}

// ---- Schema ----

/// Column type, as far as this reader supports.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum IpcType {
    Int {
        bits: u8,
        signed: bool,
    },
    Float32,
    Float64,
    Utf8,
    Bool,
    /// Always microseconds without a time zone (ADR 0003).
    TimestampMicros,
    Struct,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IpcField {
    pub name: String,
    pub nullable: bool,
    /// The value type; for a dictionary-encoded field, the dictionary's.
    pub ty: IpcType,
    /// Dictionary id and index type (always unsigned or signed ints).
    pub dictionary: Option<(i64, IpcType)>,
    pub children: Vec<IpcField>,
}

fn parse_field(t: Table<'_>) -> Result<IpcField> {
    let name = t.str(0)?.unwrap_or("").to_string();
    let nullable = t.bool(1)?;
    let type_type = t.u8(2, 0)?;
    let ty_table = t.table(3)?;
    let int = |tt: Option<Table<'_>>| -> Result<IpcType> {
        let tt = tt.ok_or("Int type without parameters")?;
        let bits = tt.i32(0, 0)?;
        Ok(IpcType::Int {
            bits: u8::try_from(bits).map_err(|_| format!("bad int width {bits}"))?,
            signed: tt.bool(1)?,
        })
    };
    let ty = match type_type {
        2 => int(ty_table)?,
        3 => match ty_table.map(|x| x.i16(0, 0)).transpose()?.unwrap_or(0) {
            1 => IpcType::Float32,
            2 => IpcType::Float64,
            p => return Err(format!("unsupported float precision {p}")),
        },
        5 => IpcType::Utf8,
        6 => IpcType::Bool,
        10 => {
            let tt = ty_table.ok_or("Timestamp without parameters")?;
            if tt.i16(0, 0)? != 2 || tt.str(1)?.is_some() {
                return Err(format!(
                    "{name}: only zone-less microsecond timestamps are supported"
                ));
            }
            IpcType::TimestampMicros
        }
        13 => IpcType::Struct,
        other => return Err(format!("{name}: unsupported Arrow type id {other}")),
    };
    let dictionary = t
        .table(4)?
        .map(|d| -> Result<(i64, IpcType)> { Ok((d.i64(0, 0)?, int(d.table(1)?)?)) })
        .transpose()?;
    let children = t
        .tables(5)?
        .into_iter()
        .map(parse_field)
        .collect::<Result<_>>()?;
    Ok(IpcField {
        name,
        nullable,
        ty,
        dictionary,
        children,
    })
}

// ---- Arrays ----

/// Decoded values of one column (or dictionary), across all batches.
#[derive(Clone, PartialEq, Debug)]
pub enum Values {
    I64(Vec<i64>),
    U32(Vec<u32>),
    U16(Vec<u16>),
    F32(Vec<f32>),
    F64(Vec<f64>),
    Bool(Bitmap),
    Utf8(Vec<String>),
    /// Dictionary indices into the field's dictionary.
    Dict(Vec<u32>),
    Struct(Vec<Array>),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Array {
    pub values: Values,
    /// `None` when there are no nulls.
    pub validity: Option<Bitmap>,
    pub len: usize,
}

impl Array {
    fn empty(field: &IpcField) -> Result<Self> {
        let values = if field.dictionary.is_some() {
            Values::Dict(Vec::new())
        } else {
            match &field.ty {
                IpcType::Int {
                    bits: 64,
                    signed: true,
                }
                | IpcType::TimestampMicros => Values::I64(Vec::new()),
                IpcType::Int {
                    bits: 32,
                    signed: false,
                } => Values::U32(Vec::new()),
                IpcType::Int {
                    bits: 16,
                    signed: false,
                } => Values::U16(Vec::new()),
                IpcType::Float32 => Values::F32(Vec::new()),
                IpcType::Float64 => Values::F64(Vec::new()),
                IpcType::Bool => Values::Bool(Bitmap::new()),
                IpcType::Utf8 => Values::Utf8(Vec::new()),
                IpcType::Struct => Values::Struct(
                    field
                        .children
                        .iter()
                        .map(Array::empty)
                        .collect::<Result<_>>()?,
                ),
                IpcType::Int { bits, signed } => {
                    return Err(format!(
                        "{}: unsupported int{bits} (signed: {signed})",
                        field.name
                    ));
                }
            }
        };
        Ok(Self {
            values,
            validity: None,
            len: 0,
        })
    }

    /// Concatenates batches of one column (all of the same type).
    fn concat(field: &IpcField, parts: Vec<Array>) -> Result<Array> {
        let mut out = Array::empty(field)?;
        if parts.iter().any(|p| p.validity.is_some()) {
            let mut v = Bitmap::new();
            for p in &parts {
                match &p.validity {
                    Some(b) => v.extend(b),
                    None => v.extend(&Bitmap::ones(p.len)),
                }
            }
            out.validity = Some(v);
        }
        out.len = parts.iter().map(|p| p.len).sum();
        out.values = match out.values {
            Values::Struct(_) => {
                let mut cols: Vec<Vec<Array>> = field.children.iter().map(|_| Vec::new()).collect();
                for p in parts {
                    let Values::Struct(cs) = p.values else {
                        return Err(format!(
                            "{}: batches disagree on the column type",
                            field.name
                        ));
                    };
                    for (c, a) in cols.iter_mut().zip(cs) {
                        c.push(a);
                    }
                }
                Values::Struct(
                    cols.into_iter()
                        .zip(&field.children)
                        .map(|(c, f)| Array::concat(f, c))
                        .collect::<Result<_>>()?,
                )
            }
            mut acc => {
                // Reserve once: repeated growth copies the column (and, in
                // WASM, grows linear memory) many times over.
                let total = out.len;
                match &mut acc {
                    Values::I64(a) => a.reserve_exact(total),
                    Values::U32(a) | Values::Dict(a) => a.reserve_exact(total),
                    Values::U16(a) => a.reserve_exact(total),
                    Values::F32(a) => a.reserve_exact(total),
                    Values::F64(a) => a.reserve_exact(total),
                    Values::Utf8(a) => a.reserve_exact(total),
                    Values::Bool(_) | Values::Struct(_) => {}
                }
                for p in parts {
                    match (&mut acc, p.values) {
                        (Values::I64(a), Values::I64(b)) => a.extend(b),
                        (Values::U32(a), Values::U32(b)) | (Values::Dict(a), Values::Dict(b)) => {
                            a.extend(b)
                        }
                        (Values::U16(a), Values::U16(b)) => a.extend(b),
                        (Values::F32(a), Values::F32(b)) => a.extend(b),
                        (Values::F64(a), Values::F64(b)) => a.extend(b),
                        (Values::Utf8(a), Values::Utf8(b)) => a.extend(b),
                        (Values::Bool(a), Values::Bool(b)) => a.extend(&b),
                        _ => {
                            return Err(format!(
                                "{}: batches disagree on the column type",
                                field.name
                            ));
                        }
                    }
                }
                acc
            }
        };
        Ok(out)
    }
}

/// Reads buffers of one record batch in field order.
struct BatchReader<'a> {
    body: &'a [u8],
    nodes: Vec<(i64, i64)>,
    buffers: Vec<(i64, i64)>,
    /// Buffers are LZ4 frames, each prefixed with its uncompressed length.
    lz4: bool,
    next_node: usize,
    next_buffer: usize,
}

/// Parses a RecordBatch table (also the payload of a DictionaryBatch).
fn batch_reader<'a>(rb: &Table<'a>, body: &'a [u8]) -> Result<BatchReader<'a>> {
    let lz4 = match rb.table(3)? {
        None => false,
        Some(c) => {
            if c.u8(0, 0)? != 0 {
                return Err("only LZ4_FRAME compression is supported".into());
            }
            if c.u8(1, 0)? != 0 {
                return Err("only per-buffer compression is supported".into());
            }
            true
        }
    };
    let structs = |id: usize| -> Result<Vec<(i64, i64)>> {
        let Some((start, n)) = rb.vector(id)? else {
            return Ok(Vec::new());
        };
        (0..n)
            .map(|k| {
                let p = element(start, k, 16)?;
                Ok((rb.buf.i64(p)?, rb.buf.i64(p + 8)?))
            })
            .collect()
    };
    Ok(BatchReader {
        body,
        nodes: structs(1)?,
        buffers: structs(2)?,
        lz4,
        next_node: 0,
        next_buffer: 0,
    })
}

/// Arrow's compressed buffer: `uncompressed length: i64` (or -1 when the
/// rest is stored raw), then one LZ4 frame.
fn decompress(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let prefix: [u8; 8] = raw
        .get(..8)
        .ok_or("compressed buffer is shorter than its length prefix")?
        .try_into()
        .expect("8 bytes");
    let len = i64::from_le_bytes(prefix);
    if len == -1 {
        return Ok(raw[8..].to_vec());
    }
    let len = usize::try_from(len).map_err(|_| "negative uncompressed length")?;
    let mut out = Vec::new();
    // Read at most one byte past the promised size, so a lying prefix is
    // caught without decoding an arbitrarily large frame.
    lz4_flex::frame::FrameDecoder::new(&raw[8..])
        .take(len as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("LZ4 frame: {e}"))?;
    if out.len() != len {
        return Err(format!(
            "LZ4 buffer decoded to {} bytes, expected {len}",
            out.len()
        ));
    }
    Ok(out)
}

impl<'a> BatchReader<'a> {
    fn node(&mut self) -> Result<(usize, usize)> {
        let &(len, nulls) = self
            .nodes
            .get(self.next_node)
            .ok_or("record batch has too few field nodes")?;
        self.next_node += 1;
        let len = usize::try_from(len).map_err(|_| "negative length")?;
        let nulls = usize::try_from(nulls).map_err(|_| "negative null count")?;
        Ok((len, nulls))
    }
    fn buffer(&mut self) -> Result<Cow<'a, [u8]>> {
        let &(off, len) = self
            .buffers
            .get(self.next_buffer)
            .ok_or("record batch has too few buffers")?;
        self.next_buffer += 1;
        let (off, len) = (
            usize::try_from(off).map_err(|_| "negative buffer offset")?,
            usize::try_from(len).map_err(|_| "negative buffer length")?,
        );
        let raw = Buf(self.body).bytes(off, len)?;
        Ok(if self.lz4 {
            Cow::Owned(decompress(raw)?)
        } else {
            Cow::Borrowed(raw)
        })
    }

    fn fixed<const N: usize, T>(&mut self, len: usize, from: fn([u8; N]) -> T) -> Result<Vec<T>> {
        let b = self.buffer()?;
        // Lengths come from the file: check before trusting them.
        let need = len
            .checked_mul(N)
            .ok_or_else(|| format!("length {len} is impossibly large"))?;
        if b.len() < need {
            return Err(format!(
                "values buffer holds {} bytes, need {need}",
                b.len()
            ));
        }
        Ok(b[..need]
            .chunks_exact(N)
            .map(|c| from(c.try_into().expect("N bytes")))
            .collect())
    }

    fn array(&mut self, field: &IpcField, dict_values: bool) -> Result<Array> {
        let (len, nulls) = self.node()?;
        let validity_bytes = self.buffer()?;
        let validity = if nulls == 0 {
            None
        } else {
            if validity_bytes.len().saturating_mul(8) < len {
                return Err(format!("{}: validity buffer too short", field.name));
            }
            let bits = Bitmap::from_bytes(&validity_bytes, len);
            if bits.count_zeros() != nulls {
                return Err(format!(
                    "{}: null count doesn't match the validity bitmap",
                    field.name
                ));
            }
            Some(bits)
        };
        let ty = match (&field.dictionary, dict_values) {
            (Some((_, index)), false) => index.clone(),
            _ => field.ty.clone(),
        };
        let values = match ty {
            IpcType::Int {
                bits: 64,
                signed: true,
            }
            | IpcType::TimestampMicros => Values::I64(self.fixed(len, i64::from_le_bytes)?),
            IpcType::Int {
                bits: 32,
                signed: false,
            } => {
                let v = self.fixed(len, u32::from_le_bytes)?;
                if field.dictionary.is_some() && !dict_values {
                    Values::Dict(v)
                } else {
                    Values::U32(v)
                }
            }
            IpcType::Int {
                bits: 16,
                signed: false,
            } => Values::U16(self.fixed(len, u16::from_le_bytes)?),
            IpcType::Float32 => Values::F32(self.fixed(len, f32::from_le_bytes)?),
            IpcType::Float64 => Values::F64(self.fixed(len, f64::from_le_bytes)?),
            IpcType::Bool => {
                let b = self.buffer()?;
                if b.len().saturating_mul(8) < len {
                    return Err(format!("{}: bool buffer too short", field.name));
                }
                Values::Bool(Bitmap::from_bytes(&b, len))
            }
            IpcType::Utf8 => {
                let offsets = self.fixed(
                    len.checked_add(1).ok_or("length is impossibly large")?,
                    i32::from_le_bytes,
                )?;
                let data = self.buffer()?;
                let mut out = Vec::with_capacity(len);
                for w in offsets.windows(2) {
                    let (a, b) = (
                        usize::try_from(w[0]).map_err(|_| "negative string offset")?,
                        usize::try_from(w[1]).map_err(|_| "negative string offset")?,
                    );
                    let bytes = data.get(a..b).ok_or("string offsets out of bounds")?;
                    out.push(
                        std::str::from_utf8(bytes)
                            .map_err(|_| format!("{}: string is not UTF-8", field.name))?
                            .to_string(),
                    );
                }
                Values::Utf8(out)
            }
            IpcType::Struct => Values::Struct(
                field
                    .children
                    .iter()
                    .map(|c| {
                        let child = self.array(c, false)?;
                        if child.len != len {
                            return Err(format!("{}: struct child length differs", field.name));
                        }
                        Ok(child)
                    })
                    .collect::<Result<_>>()?,
            ),
            IpcType::Int { bits, signed } => {
                return Err(format!(
                    "{}: unsupported int{bits} (signed: {signed})",
                    field.name
                ));
            }
        };
        Ok(Array {
            values,
            validity,
            len,
        })
    }
}

/// A decoded IPC file: schema, schema metadata, dictionaries and columns.
#[derive(Clone, PartialEq, Debug)]
pub struct IpcFile {
    pub fields: Vec<IpcField>,
    pub metadata: Vec<(String, String)>,
    pub dictionaries: HashMap<i64, Vec<String>>,
    /// Rows per record batch, in order.
    pub batch_lengths: Vec<usize>,
    pub columns: Vec<Array>,
}

impl IpcFile {
    pub fn meta(&self, key: &str) -> Option<&str> {
        self.metadata
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// Converts column `i` to the engine's representation: Int64,
    /// Timestamp(µs), Float64, Bool, dictionary Utf8 (sorted dictionary
    /// with u32 codes), or a struct of two Float32 (a location).
    pub fn core_column(&self, i: usize) -> Result<Column> {
        to_core(&self.fields[i], self.columns[i].clone(), &self.dictionaries)
    }

    /// [`IpcFile::core_column`] for every column, without copying values.
    pub fn into_core_columns(self) -> Result<Vec<Column>> {
        let dictionaries = self.dictionaries;
        self.fields
            .iter()
            .zip(self.columns)
            .map(|(f, a)| to_core(f, a, &dictionaries))
            .collect()
    }
}

fn to_core(
    field: &IpcField,
    array: Array,
    dictionaries: &HashMap<i64, Vec<String>>,
) -> Result<Column> {
    let Array {
        values, validity, ..
    } = array;
    let data = match (&field.ty, &field.dictionary, values) {
        (IpcType::TimestampMicros, None, Values::I64(v)) => ColumnData::Timestamp(v),
        (
            IpcType::Int {
                bits: 64,
                signed: true,
            },
            None,
            Values::I64(v),
        ) => ColumnData::I64(v),
        (IpcType::Float64, None, Values::F64(v)) => ColumnData::F64(v),
        (IpcType::Bool, None, Values::Bool(b)) => ColumnData::Bool(b),
        (IpcType::Utf8, Some((id, _)), Values::Dict(codes)) => {
            // A file with no rows carries no dictionary batch at all.
            let dictionary = match dictionaries.get(id) {
                Some(d) => d.clone(),
                None if codes.is_empty() => Vec::new(),
                None => return Err(format!("{}: dictionary {id} is missing", field.name)),
            };
            if !dictionary
                .windows(2)
                .all(|w| w[0].as_bytes() < w[1].as_bytes())
            {
                return Err(format!(
                    "{}: dictionary is not sorted and distinct",
                    field.name
                ));
            }
            let valid = |r: usize| validity.as_ref().is_none_or(|v| v.get(r));
            if let Some(r) =
                (0..codes.len()).find(|&r| valid(r) && codes[r] as usize >= dictionary.len())
            {
                return Err(format!(
                    "{}: row {r} has a code past the dictionary",
                    field.name
                ));
            }
            ColumnData::DictUtf8 { codes, dictionary }
        }
        (IpcType::Struct, None, Values::Struct(children)) => match <[Array; 2]>::try_from(children)
        {
            Ok(
                [
                    Array {
                        values: Values::F32(lat),
                        ..
                    },
                    Array {
                        values: Values::F32(lon),
                        ..
                    },
                ],
            ) => ColumnData::Geo { lat, lon },
            _ => {
                return Err(format!(
                    "{}: a location is a struct of two float32",
                    field.name
                ));
            }
        },
        _ => return Err(format!("{}: not an engine column type", field.name)),
    };
    if !field.nullable && validity.is_some() {
        return Err(format!("{}: nulls in a non-nullable column", field.name));
    }
    Ok(Column::new(field.name.clone(), data, validity))
}

struct Block {
    offset: usize,
    meta_len: usize,
    body_len: usize,
}

fn blocks(buf: Buf<'_>, footer: &Table<'_>, id: usize) -> Result<Vec<Block>> {
    let Some((start, n)) = footer.vector(id)? else {
        return Ok(Vec::new());
    };
    (0..n)
        .map(|k| {
            let p = element(start, k, 24)?;
            // Bounds-check the whole 24-byte block before reading its parts.
            buf.bytes(p, 24)?;
            let usize_of =
                |x: i64| usize::try_from(x).map_err(|_| "negative block field".to_string());
            Ok(Block {
                offset: usize_of(buf.i64(p)?)?,
                meta_len: usize_of(buf.i32(p + 8)? as i64)?,
                body_len: usize_of(buf.i64(p + 16)?)?,
            })
        })
        .collect()
}

/// `(message table, body)` of the message a footer block points to.
fn message<'a>(bytes: &'a [u8], block: &Block) -> Result<(Table<'a>, &'a [u8])> {
    let all = Buf(bytes);
    let mut at = block.offset;
    let mut len = all.i32(at)?;
    at += 4;
    if len == -1 {
        len = all.i32(at)?;
        at += 4;
    }
    let len = usize::try_from(len).map_err(|_| "negative message length")?;
    let meta = all.bytes(at, len)?;
    let body_at = block
        .offset
        .checked_add(block.meta_len)
        .ok_or("block offset overflow")?;
    let body = all.bytes(body_at, block.body_len)?;
    let msg = Buf(meta).root()?;
    let version = msg.i16(0, 0)?;
    if version < 4 {
        return Err(format!("unsupported IPC metadata version {version}"));
    }
    Ok((msg, body))
}

fn record_batch<'a>(msg: &Table<'a>, body: &'a [u8]) -> Result<(i64, BatchReader<'a>)> {
    let rb = msg.table(2)?.ok_or("message without a header")?;
    Ok((rb.i64(0, 0)?, batch_reader(&rb, body)?))
}

/// Decodes a complete Arrow IPC file.
pub fn read_file(bytes: &[u8]) -> Result<IpcFile> {
    let n = bytes.len();
    if n < 18 || &bytes[..6] != MAGIC || &bytes[n - 6..] != MAGIC {
        return Err("not an Arrow IPC file (missing ARROW1 magic)".into());
    }
    let buf = Buf(bytes);
    let footer_len = usize::try_from(buf.i32(n - 10)?).map_err(|_| "negative footer length")?;
    let footer_start = (n - 10)
        .checked_sub(footer_len)
        .ok_or("footer length exceeds the file")?;
    let footer_bytes = buf.bytes(footer_start, footer_len)?;
    let footer = Buf(footer_bytes).root()?;
    let schema = footer.table(1)?.ok_or("footer has no schema")?;
    if schema.i16(0, 0)? != 0 {
        return Err("only little-endian files are supported".into());
    }
    let fields: Vec<IpcField> = schema
        .tables(1)?
        .into_iter()
        .map(parse_field)
        .collect::<Result<_>>()?;
    let metadata = schema
        .tables(2)?
        .into_iter()
        .map(|kv| {
            Ok((
                kv.str(0)?.unwrap_or("").to_string(),
                kv.str(1)?.unwrap_or("").to_string(),
            ))
        })
        .collect::<Result<_>>()?;

    let fb = Buf(footer_bytes);
    // Dictionaries, in file order (a delta appends, otherwise replaces).
    let mut dictionaries: HashMap<i64, Vec<String>> = HashMap::new();
    let dict_fields: HashMap<i64, &IpcField> = fields
        .iter()
        .filter_map(|f| f.dictionary.as_ref().map(|(id, _)| (*id, f)))
        .collect();
    for block in blocks(fb, &footer, 2)? {
        let (msg, body) = message(bytes, &block)?;
        if msg.u8(1, 0)? != 2 {
            return Err("dictionary block doesn't hold a DictionaryBatch".into());
        }
        let db = msg.table(2)?.ok_or("empty DictionaryBatch")?;
        let id = db.i64(0, 0)?;
        let is_delta = db.bool(2)?;
        let field = dict_fields
            .get(&id)
            .ok_or_else(|| format!("dictionary {id} isn't used by any column"))?;
        let data = db.table(1)?.ok_or("DictionaryBatch without data")?;
        let mut reader = batch_reader(&data, body)?;
        let values = reader.array(field, true)?;
        let Values::Utf8(strings) = values.values else {
            return Err("only Utf8 dictionaries are supported".into());
        };
        if values.validity.is_some() {
            return Err("dictionaries can't contain nulls".into());
        }
        let entry = dictionaries.entry(id).or_default();
        if !is_delta {
            entry.clear();
        }
        entry.extend(strings);
    }

    // Record batches are independent, so they decode in parallel with the
    // `parallel` feature; results are collected in file order either way.
    let decode = |block: &Block| -> Result<(usize, Vec<Array>)> {
        let (msg, body) = message(bytes, block)?;
        if msg.u8(1, 0)? != 3 {
            return Err("record batch block doesn't hold a RecordBatch".into());
        }
        let (len, mut reader) = record_batch(&msg, body)?;
        let len = usize::try_from(len).map_err(|_| "negative batch length")?;
        let arrays = fields
            .iter()
            .map(|field| {
                let array = reader.array(field, false)?;
                if array.len != len {
                    return Err(format!(
                        "{}: batch column length differs from the batch",
                        field.name
                    ));
                }
                Ok(array)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((len, arrays))
    };
    let record_blocks = blocks(fb, &footer, 3)?;
    #[cfg(feature = "parallel")]
    let decoded: Vec<(usize, Vec<Array>)> = {
        use rayon::prelude::*;
        record_blocks
            .par_iter()
            .map(decode)
            .collect::<Result<_>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let decoded: Vec<(usize, Vec<Array>)> =
        record_blocks.iter().map(decode).collect::<Result<_>>()?;
    let batch_lengths = decoded.iter().map(|(len, _)| *len).collect();
    let mut parts: Vec<Vec<Array>> = fields
        .iter()
        .map(|_| Vec::with_capacity(decoded.len()))
        .collect();
    for (_, arrays) in decoded {
        for (column, array) in parts.iter_mut().zip(arrays) {
            column.push(array);
        }
    }
    let concat = |(f, p): (&IpcField, Vec<Array>)| Array::concat(f, p);
    #[cfg(feature = "parallel")]
    let columns = {
        use rayon::prelude::*;
        fields
            .par_iter()
            .zip(parts)
            .map(concat)
            .collect::<Result<_>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let columns = fields
        .iter()
        .zip(parts)
        .map(concat)
        .collect::<Result<_>>()?;
    Ok(IpcFile {
        fields,
        metadata,
        dictionaries,
        batch_lengths,
        columns,
    })
}
