/// A growable bitmap stored LSB-first in bytes: bit `i` lives in
/// `bytes[i / 8] >> (i % 8)`. This is the Arrow validity layout, and also the
/// layout the content hash uses (`docs/snapshot/schema.md` §6). Bits past
/// `len` in the last byte are always zero.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Bitmap {
    bytes: Vec<u8>,
    len: usize,
}

impl Bitmap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bits: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bits.div_ceil(8)),
            len: 0,
        }
    }

    pub fn from_bools(bits: impl IntoIterator<Item = bool>) -> Self {
        let mut bitmap = Self::new();
        for bit in bits {
            bitmap.push(bit);
        }
        bitmap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, bit: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit {
            self.bytes[self.len / 8] |= 1 << (self.len % 8);
        }
        self.len += 1;
    }

    /// # Panics
    /// If `i >= len`.
    pub fn get(&self, i: usize) -> bool {
        assert!(
            i < self.len,
            "bit {i} out of range for bitmap of {}",
            self.len
        );
        self.bytes[i / 8] >> (i % 8) & 1 == 1
    }

    pub fn count_ones(&self) -> usize {
        self.bytes.iter().map(|b| b.count_ones() as usize).sum()
    }

    pub fn count_zeros(&self) -> usize {
        self.len - self.count_ones()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = bool> + '_ {
        (0..self.len).map(|i| self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_and_padding_is_zero() {
        let b = Bitmap::from_bools([true, false, true, true, false, false, false, false, true]);
        assert_eq!(b.as_bytes(), &[0b0000_1101, 0b0000_0001]);
        assert_eq!(b.len(), 9);
        assert_eq!(b.count_ones(), 4);
        assert_eq!(b.count_zeros(), 5);
        assert!(b.get(8));
        assert!(!b.get(7));
    }

    #[test]
    #[should_panic]
    fn get_out_of_range_panics() {
        Bitmap::from_bools([true]).get(1);
    }
}
