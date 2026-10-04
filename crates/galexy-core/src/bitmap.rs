//! Fixed-size bitset (no heap required).

/// A compile-time-sized bitmap over `N` bits.
///
/// All allocation-free; intended as the backing store for frame allocators
/// and other fixed-capacity bookkeeping.
pub struct Bitmap<const N: usize> {
    bits: [u64; N],
}

/// Number of bits per backing word.
const WORD_BITS: usize = 64;

impl<const N: usize> Bitmap<N> {
    /// Creates a bitmap with every bit cleared.
    pub const fn new() -> Self {
        Bitmap { bits: [0; N] }
    }

    /// Number of bits this bitmap can hold.
    pub const fn capacity(&self) -> usize {
        N * WORD_BITS
    }

    fn bit_indices(index: usize) -> (usize, u64) {
        (index / WORD_BITS, 1 << (index % WORD_BITS))
    }

    /// Sets every bit to `value`.
    pub fn fill(&mut self, value: bool) {
        self.bits.fill(if value { !0 } else { 0 });
    }

    /// Sets bit `index` to `value`.
    ///
    /// Panics if `index` is out of bounds (static sizing makes this a bug).
    pub fn set(&mut self, index: usize, value: bool) {
        assert!(index < self.capacity(), "bitmap index out of bounds");
        let (word, mask) = Self::bit_indices(index);
        if value {
            self.bits[word] |= mask;
        } else {
            self.bits[word] &= !mask;
        }
    }

    /// Reads bit `index`.
    ///
    /// Panics if `index` is out of bounds (static sizing makes this a bug).
    pub fn test(&self, index: usize) -> bool {
        assert!(index < self.capacity(), "bitmap index out of bounds");
        let (word, mask) = Self::bit_indices(index);
        self.bits[word] & mask != 0
    }

    /// Returns the index of the first cleared bit, if any.
    pub fn first_clear(&self) -> Option<usize> {
        for (word, &bits) in self.bits.iter().enumerate() {
            let complement = !bits;
            if complement != 0 {
                let bit = complement.trailing_zeros() as usize;
                return Some(word * WORD_BITS + bit);
            }
        }
        None
    }

    /// Returns the number of set bits.
    pub fn count_set(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }
}

impl<const N: usize> Default for Bitmap<N> {
    fn default() -> Self {
        Self::new()
    }
}
