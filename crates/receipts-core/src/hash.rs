//! Content hashing over the canonical logical encoding of
//! `docs/snapshot/schema.md` §6 (ADR 0002).
//!
//! Every hash uses BLAKE3 in `derive_key` mode, with a distinct context
//! string per kind of object. A chunk hash can therefore never equal a column
//! or snapshot hash. The encoding reads logical values, not file bytes, so
//! hashes don't depend on how a snapshot was written.

use crate::{Column, ColumnData, ColumnType};
use std::fmt;
use std::ops::Range;

/// Rows per chunk: the unit of hashing, streaming load, and IPC record batches.
pub const CHUNK_ROWS: usize = 1 << 16;

pub mod context {
    pub const CHUNK: &str = "receipts snapshot v1 chunk";
    pub const DICTIONARY: &str = "receipts snapshot v1 dictionary";
    pub const COLUMN: &str = "receipts snapshot v1 column";
    pub const SNAPSHOT: &str = "receipts snapshot v1 snapshot";
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub const ZERO: Self = Self([0; 32]);

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentHash({})", self.to_hex())
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// A BLAKE3 hasher with a domain context and little-endian, length-prefixed
/// helpers. Snapshot-specific tables (cleaning log, rejects) are hashed with
/// it too, under their own contexts.
#[derive(Debug)]
pub struct ContentHasher(blake3::Hasher);

impl ContentHasher {
    pub fn new(context: &str) -> Self {
        Self(blake3::Hasher::new_derive_key(context))
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.update(&[v]);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    /// Raw bytes, with no length prefix. Only for fixed-size or trailing data.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// `len:u32 ‖ bytes`.
    ///
    /// # Panics
    /// If `bytes` is 4 GiB or longer.
    pub fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.u32(u32::try_from(bytes.len()).expect("hashed field exceeds 4 GiB"));
        self.0.update(bytes);
        self
    }

    /// `0:u8` for `None`, else `1:u8 ‖ len:u32 ‖ bytes`.
    pub fn opt_bytes(&mut self, bytes: Option<&[u8]>) -> &mut Self {
        match bytes {
            None => self.u8(0),
            Some(b) => self.u8(1).bytes(b),
        }
    }

    pub fn hash(&mut self, h: &ContentHash) -> &mut Self {
        self.0.update(&h.0);
        self
    }

    pub fn finish(&self) -> ContentHash {
        ContentHash(*self.0.finalize().as_bytes())
    }
}

/// The row ranges of each chunk for a column of `len` rows.
pub fn chunk_ranges(len: usize) -> impl ExactSizeIterator<Item = Range<usize>> {
    (0..len.div_ceil(CHUNK_ROWS)).map(move |k| k * CHUNK_ROWS..((k + 1) * CHUNK_ROWS).min(len))
}

/// `type_tag:u8 ‖ rows:u32 ‖ validity ‖ values`, with null slots zeroed.
///
/// # Panics
/// If `rows` is out of bounds for the column.
pub fn chunk_hash(col: &Column, rows: Range<usize>) -> ContentHash {
    assert!(rows.end <= col.len(), "chunk range out of bounds");
    let n = rows.len();
    let valid = |i: usize| col.is_valid(i);
    let mut buf = Vec::with_capacity(n.div_ceil(8) + n * 8);
    push_bits(&mut buf, rows.clone().map(valid));
    match &col.data {
        ColumnData::I64(v) | ColumnData::Timestamp(v) => {
            for i in rows {
                buf.extend_from_slice(&if valid(i) { v[i] } else { 0 }.to_le_bytes());
            }
        }
        ColumnData::F64(v) => {
            for i in rows {
                let bits = if valid(i) { v[i].to_bits() } else { 0 };
                buf.extend_from_slice(&bits.to_le_bytes());
            }
        }
        ColumnData::Bool(b) => push_bits(&mut buf, rows.map(|i| valid(i) && b.get(i))),
        ColumnData::DictUtf8 { codes, .. } => {
            for i in rows {
                buf.extend_from_slice(&if valid(i) { codes[i] } else { 0 }.to_le_bytes());
            }
        }
        ColumnData::Geo { lat, lon } => {
            for axis in [lat, lon] {
                for i in rows.clone() {
                    let bits = if valid(i) { axis[i].to_bits() } else { 0 };
                    buf.extend_from_slice(&bits.to_le_bytes());
                }
            }
        }
    }
    ContentHasher::new(context::CHUNK)
        .u8(col.data.column_type().tag())
        .u32(u32::try_from(n).expect("chunk longer than u32::MAX rows"))
        .raw(&buf)
        .finish()
}

fn push_bits(buf: &mut Vec<u8>, bits: impl Iterator<Item = bool>) {
    let mut byte = 0u8;
    let mut used = 0;
    for bit in bits {
        byte |= u8::from(bit) << used;
        used += 1;
        if used == 8 {
            buf.push(byte);
            byte = 0;
            used = 0;
        }
    }
    if used > 0 {
        buf.push(byte);
    }
}

/// `n:u32 ‖ (len:u32 ‖ utf8)*`.
pub fn dictionary_hash(dictionary: &[String]) -> ContentHash {
    let mut h = ContentHasher::new(context::DICTIONARY);
    h.u32(u32::try_from(dictionary.len()).expect("dictionary exceeds u32::MAX entries"));
    for entry in dictionary {
        h.bytes(entry.as_bytes());
    }
    h.finish()
}

/// All hashes for one column.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnHashes {
    pub chunks: Vec<ContentHash>,
    pub dictionary: Option<ContentHash>,
    pub column: ContentHash,
}

/// `name ‖ type_tag:u8 ‖ (dictionary_hash | 32 zero bytes) ‖ n_chunks:u32 ‖ chunk_hash*`.
pub fn hash_column(col: &Column) -> ColumnHashes {
    let chunks: Vec<_> = chunk_ranges(col.len())
        .map(|r| chunk_hash(col, r))
        .collect();
    let dictionary = match &col.data {
        ColumnData::DictUtf8 { dictionary, .. } => Some(dictionary_hash(dictionary)),
        _ => None,
    };
    let mut h = ContentHasher::new(context::COLUMN);
    h.bytes(col.name.as_bytes())
        .u8(col.data.column_type().tag())
        .hash(&dictionary.unwrap_or(ContentHash::ZERO))
        .u32(chunks.len() as u32);
    for c in &chunks {
        h.hash(c);
    }
    ColumnHashes {
        column: h.finish(),
        chunks,
        dictionary,
    }
}

/// The identity of a snapshot's content. `descriptor` is the canonical JSON
/// of `{rules_version, scope, sort_key}`. `extra` holds the hashes of the
/// snapshot's side tables (cleaning log, rejects), in a fixed order.
pub fn snapshot_hash(
    source_id: u16,
    row_count: u32,
    descriptor: &str,
    columns: &[ContentHash],
    extra: &[ContentHash],
) -> ContentHash {
    let mut h = ContentHasher::new(context::SNAPSHOT);
    h.u16(source_id)
        .u32(row_count)
        .bytes(descriptor.as_bytes())
        .u32(columns.len() as u32);
    for c in columns {
        h.hash(c);
    }
    h.u32(extra.len() as u32);
    for e in extra {
        h.hash(e);
    }
    h.finish()
}

impl ColumnType {
    /// Stable tag used in hashes. Never renumber.
    pub const fn tag(self) -> u8 {
        match self {
            Self::I64 => 1,
            Self::F64 => 2,
            Self::Bool => 3,
            Self::Timestamp => 4,
            Self::DictUtf8 => 5,
            Self::Geo => 6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Bitmap;

    fn ints(values: Vec<i64>, validity: Option<Vec<bool>>) -> Column {
        Column::new(
            "x",
            ColumnData::I64(values),
            validity.map(Bitmap::from_bools),
        )
    }

    #[test]
    fn null_slot_contents_do_not_matter() {
        let a = ints(vec![1, 999, 3], Some(vec![true, false, true]));
        let b = ints(vec![1, -5, 3], Some(vec![true, false, true]));
        assert_eq!(hash_column(&a), hash_column(&b));
    }

    #[test]
    fn nulls_differ_from_zeros() {
        let a = ints(vec![1, 0, 3], Some(vec![true, false, true]));
        let b = ints(vec![1, 0, 3], None);
        assert_ne!(hash_column(&a).column, hash_column(&b).column);
    }

    #[test]
    fn all_valid_bitmap_equals_no_bitmap() {
        let a = ints(vec![1, 2, 3], Some(vec![true; 3]));
        let b = ints(vec![1, 2, 3], None);
        assert_eq!(hash_column(&a), hash_column(&b));
    }

    #[test]
    fn name_type_and_value_changes_change_the_hash() {
        let base = ints(vec![1, 2, 3], None);
        let renamed = Column::new("y", base.data.clone(), None);
        let retyped = Column::new("x", ColumnData::Timestamp(vec![1, 2, 3]), None);
        let edited = ints(vec![1, 2, 4], None);
        let h = hash_column(&base).column;
        for other in [renamed, retyped, edited] {
            assert_ne!(h, hash_column(&other).column);
        }
    }

    #[test]
    fn chunks_split_at_chunk_rows() {
        let n = CHUNK_ROWS * 2 + 5;
        let col = ints((0..n as i64).collect(), None);
        let ranges: Vec<_> = chunk_ranges(n).collect();
        assert_eq!(
            ranges,
            vec![0..CHUNK_ROWS, CHUNK_ROWS..2 * CHUNK_ROWS, 2 * CHUNK_ROWS..n]
        );
        let hashes = hash_column(&col);
        assert_eq!(hashes.chunks.len(), 3);
        // Editing a row in the last chunk only changes that chunk.
        let mut edited = col.clone();
        if let ColumnData::I64(v) = &mut edited.data {
            v[n - 1] = -1;
        }
        let edited = hash_column(&edited);
        assert_eq!(hashes.chunks[..2], edited.chunks[..2]);
        assert_ne!(hashes.chunks[2], edited.chunks[2]);
    }

    #[test]
    fn dictionary_is_part_of_the_column_hash() {
        let mk = |d: &[&str]| {
            Column::new(
                "s",
                ColumnData::DictUtf8 {
                    codes: vec![0, 1],
                    dictionary: d.iter().map(|s| s.to_string()).collect(),
                },
                None,
            )
        };
        assert_ne!(
            hash_column(&mk(&["a", "b"])).column,
            hash_column(&mk(&["a", "c"])).column
        );
        // Length prefixes keep ["ab", ""] and ["a", "b"] apart.
        assert_ne!(
            dictionary_hash(&["ab".into(), "".into()]),
            dictionary_hash(&["a".into(), "b".into()])
        );
    }

    #[test]
    fn geo_and_bool_zero_null_slots() {
        let geo = |lat0: f32| {
            Column::new(
                "g",
                ColumnData::Geo {
                    lat: vec![lat0, 40.7],
                    lon: vec![1.0, -74.0],
                },
                Some(Bitmap::from_bools([false, true])),
            )
        };
        assert_eq!(hash_column(&geo(12.0)), hash_column(&geo(-3.0)));
        let bools = |b0: bool| {
            Column::new(
                "b",
                ColumnData::Bool(Bitmap::from_bools([b0, true])),
                Some(Bitmap::from_bools([false, true])),
            )
        };
        assert_eq!(hash_column(&bools(true)), hash_column(&bools(false)));
    }

    #[test]
    fn hex_round_trip() {
        let h = hash_column(&ints(vec![7], None)).column;
        assert_eq!(ContentHash::from_hex(&h.to_hex()), Some(h));
        assert_eq!(ContentHash::from_hex("zz"), None);
    }

    #[test]
    fn known_answer() {
        // Pins the v1 encoding. If this changes, the format version must change.
        let col = Column::new(
            "unique_key",
            ColumnData::I64(vec![1, 2, 3]),
            Some(Bitmap::from_bools([true, false, true])),
        );
        let mut h = ContentHasher::new(context::CHUNK);
        h.u8(1).u32(3).raw(&[0b101]);
        for v in [1i64, 0, 3] {
            h.raw(&v.to_le_bytes());
        }
        assert_eq!(chunk_hash(&col, 0..3), h.finish());
    }
}
