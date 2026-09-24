use std::fmt;

/// Identifies one source dataset snapshot within a session. Assigned by the
/// snapshot registry; a `SourceId` only has meaning next to a snapshot hash.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct SourceId(pub u16);

/// Stable identity of a source row: `[source_id: u16 | reserved: u16 | row_index: u32]`,
/// most significant bits first.
///
/// `row_index` is assigned once, at snapshot time, after the snapshot's
/// canonical sort (see `docs/snapshot/schema.md`). It never changes for a
/// given snapshot hash. Because the source id occupies the high bits, sorting
/// `RowId`s groups rows by source and then by row index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RowId(u64);

impl RowId {
    const SOURCE_SHIFT: u32 = 48;
    const RESERVED_SHIFT: u32 = 32;

    pub const fn new(source: SourceId, row_index: u32) -> Self {
        Self(((source.0 as u64) << Self::SOURCE_SHIFT) | row_index as u64)
    }

    /// Decodes raw bits. Returns `None` if the reserved bits are set, so that
    /// ids minted by a future format version are rejected rather than misread.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if (bits >> Self::RESERVED_SHIFT) & 0xFFFF != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    pub const fn to_bits(self) -> u64 {
        self.0
    }

    pub const fn source(self) -> SourceId {
        SourceId((self.0 >> Self::SOURCE_SHIFT) as u16)
    }

    pub const fn row_index(self) -> u32 {
        self.0 as u32
    }
}

impl fmt::Debug for RowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RowId({}:{})", self.source().0, self.row_index())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_and_unpacks() {
        for (s, r) in [(0, 0), (1, 42), (u16::MAX, u32::MAX), (7, 1 << 31)] {
            let id = RowId::new(SourceId(s), r);
            assert_eq!(id.source(), SourceId(s));
            assert_eq!(id.row_index(), r);
            assert_eq!(RowId::from_bits(id.to_bits()), Some(id));
        }
    }

    #[test]
    fn layout_is_source_then_reserved_then_row() {
        let id = RowId::new(SourceId(0xABCD), 0x1234_5678);
        assert_eq!(id.to_bits(), 0xABCD_0000_1234_5678);
    }

    #[test]
    fn rejects_reserved_bits() {
        assert_eq!(RowId::from_bits(0x0000_0001_0000_0000), None);
    }

    #[test]
    fn orders_by_source_then_row() {
        let a = RowId::new(SourceId(1), u32::MAX);
        let b = RowId::new(SourceId(2), 0);
        assert!(a < b);
    }
}
