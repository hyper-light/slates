//! A bit vector with rank as the Fast Succinct Trie uses it (research/35 §1; Zhang et al. SIGMOD
//! 2018 §2.6): one level of lookup table, a running count of set bits at the start of every block
//! of `B` bits, then a popcount within the block. The paper's select, on S-LOUDS alone, is a
//! stored node start in the trie instead (research/62 §1).

use crate::error::{Error, Malformed};

fn corrupt() -> Error {
    Error::Corruption {
        what: "a succinct trie's bit vector",
        why: Malformed::OutOfRange,
    }
}

/// A bit vector, its bits in 64-bit words, least significant first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bits {
    words: Vec<u64>,
    len: usize,
    /// Words a rank block holds (`B / 64`), a power of two, and its log: a position's block is a
    /// shift, where a division by a count read at run time is the slowest step of a rank.
    block_words: usize,
    block_shift: u32,
    /// The set bits before each block.
    ranks: Vec<u32>,
    ones: usize,
}

/// A bit vector as a builder fills it: appended to, and set in place.
#[derive(Clone, Debug, Default)]
pub struct Grow {
    words: Vec<u64>,
    len: usize,
}

impl Grow {
    /// Appends `b`. Every bit past the length is clear (a word is added clear, and only `set`
    /// writes, within the length), so a clear bit is appended by the length alone.
    pub fn push(&mut self, b: bool) {
        let i = self.len;
        if i.is_multiple_of(64) {
            self.words.push(0);
        }
        self.len = i.saturating_add(1);
        if b && let Some(w) = self.words.last_mut() {
            *w |= 1u64 << (i % 64);
        }
    }

    /// Sets bit `i`, within the length, to `b`.
    pub fn set(&mut self, i: usize, b: bool) {
        if i >= self.len {
            return;
        }
        if let Some(w) = self.words.get_mut(i / 64) {
            if b {
                *w |= 1u64 << (i % 64);
            } else {
                *w &= !(1u64 << (i % 64));
            }
        }
    }

    /// Bit `i`.
    pub fn get(&self, i: usize) -> bool {
        i < self.len
            && self
                .words
                .get(i / 64)
                .is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    /// The last bit, if any.
    pub fn last(&self) -> Option<bool> {
        self.len.checked_sub(1).map(|i| self.get(i))
    }

    /// Sets the last bit, if any, to `b`.
    pub fn set_last(&mut self, b: bool) {
        if let Some(i) = self.len.checked_sub(1) {
            self.set(i, b);
        }
    }

    /// Empties it, keeping its words' room.
    pub fn clear(&mut self) {
        self.words.clear();
        self.len = 0;
    }

    /// Room for `bits` bits in all, taken once where the total is known.
    pub fn reserve_bits(&mut self, bits: usize) {
        self.words
            .reserve_exact(bits.div_ceil(64).saturating_sub(self.words.len()));
    }

    /// Appends `other`'s bits, a word at a time: each of its words split across the two words of
    /// this vector it lands in. Bits past either length are clear, so the words' high parts carry
    /// none past the new length.
    pub fn append(&mut self, other: &Grow) {
        let shift = self.len % 64;
        let len = self.len.saturating_add(other.len);
        // The words the new length takes: a word past them would hold only clear bits, and
        // pushed, would grow a buffer taken at its size.
        let need = len.div_ceil(64);
        let words = other.words.get(..other.len.div_ceil(64)).unwrap_or(&[]);
        if shift == 0 {
            self.words.extend_from_slice(words);
        } else {
            let back = 64usize.saturating_sub(shift);
            for &w in words {
                if let Some(last) = self.words.last_mut() {
                    *last |= w << shift;
                }
                if self.words.len() < need {
                    self.words.push(w >> back);
                }
            }
        }
        self.len = len;
    }

    /// Grows to `len` bits, the new ones clear.
    pub fn grow_to(&mut self, len: usize) {
        if len > self.len {
            self.words.resize(len.div_ceil(64), 0);
            self.len = len;
        }
    }

    /// The bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no bit is held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Bits {
    /// The vector of `bits`, with rank blocks of `block_bits` (a multiple of 64): 64 for LOUDS-
    /// Dense, 512 for LOUDS-Sparse.
    pub fn new(bits: &[bool], block_bits: usize) -> Result<Self, Error> {
        let mut g = Grow::default();
        for &b in bits {
            g.push(b);
        }
        Self::from_grow(g, block_bits)
    }

    /// A copy of the vector a builder fills, its words copied whole: the builder keeps its
    /// buffers for its next trie.
    pub fn copy_of(bits: &Grow, block_bits: usize) -> Result<Self, Error> {
        Self::from_grow(bits.clone(), block_bits)
    }

    /// The vector a builder filled, its words taken as they are.
    pub fn from_grow(bits: Grow, block_bits: usize) -> Result<Self, Error> {
        let block_words = block_bits
            .checked_div(64)
            .filter(|&w| w.is_power_of_two())
            .ok_or(Error::InvalidArgument {
                what: "a rank block that is not a power of two of words",
            })?;
        let mut v = Self {
            words: bits.words,
            len: bits.len,
            block_words,
            block_shift: block_words.trailing_zeros(),
            ranks: Vec::new(),
            ones: 0,
        };
        v.index()?;
        Ok(v)
    }

    fn index(&mut self) -> Result<(), Error> {
        let mut ones = 0usize;
        // A count a block of words: taken once at its size.
        self.ranks
            .reserve_exact(self.words.len().div_ceil(self.block_words));
        for (i, w) in self.words.iter().enumerate() {
            if i.checked_rem(self.block_words) == Some(0) {
                self.ranks.push(u32::try_from(ones).map_err(|_| corrupt())?);
            }
            ones = ones.saturating_add(w.count_ones() as usize);
        }
        self.ones = ones;
        Ok(())
    }

    /// The bits.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector holds no bits.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The set bits.
    pub fn ones(&self) -> usize {
        self.ones
    }

    /// Bit `i`.
    pub fn get(&self, i: usize) -> bool {
        i < self.len
            && self
                .words
                .get(i / 64)
                .is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    /// The set bits in `[0, i]`, as the paper counts rank (`rank1(i)` includes position i).
    pub fn rank1(&self, i: usize) -> usize {
        if self.len == 0 {
            return 0;
        }
        let i = i.min(self.len.saturating_sub(1));
        let word = i / 64;
        // The shift is below 64: the block is a power of two of words.
        let block = word.wrapping_shr(self.block_shift);
        let first = word & !self.block_words.wrapping_sub(1);
        let r = self.ranks.get(block).map_or(0, |&r| r as usize);
        let r = self.words.get(first..word).map_or(r, |words| {
            words
                .iter()
                .fold(r, |r, w| r.saturating_add(w.count_ones() as usize))
        });
        // The bits up to i's within its word: all of them shifted down past the rest.
        let mask = u64::MAX.wrapping_shr(u32::try_from(63 ^ (i % 64)).unwrap_or(0));
        r.saturating_add(
            self.words
                .get(word)
                .map_or(0, |w| (w & mask).count_ones() as usize),
        )
    }

    /// For each of `ranks`, ascending, the position of the set bit of that rank (the `k`th set
    /// bit for `k`, counted from 0), in one pass over the words: each word's count of set bits
    /// passed, and the bit found within the word that holds it. A rank past the set bits ends
    /// the pass.
    pub fn select_each(
        &self,
        ranks: impl IntoIterator<Item = usize>,
        mut found: impl FnMut(usize, usize),
    ) {
        let mut ranks = ranks.into_iter().peekable();
        let mut seen = 0usize;
        for (i, &w) in self.words.iter().enumerate() {
            let ones = w.count_ones() as usize;
            while let Some(&k) = ranks.peek() {
                let Some(within) = k.checked_sub(seen).filter(|&r| r < ones) else {
                    break;
                };
                // The `within`th set bit of the word: the lower ones cleared.
                let mut bits = w;
                for _ in 0..within {
                    bits &= bits.wrapping_sub(1);
                }
                found(
                    k,
                    i.saturating_mul(64)
                        .saturating_add(bits.trailing_zeros() as usize),
                );
                ranks.next();
            }
            seen = seen.saturating_add(ones);
        }
    }

    /// The first set bit at or past `i`, by words: a node's end in LOUDS-Sparse is usually in
    /// the word its start is.
    pub fn next_one(&self, i: usize) -> Option<usize> {
        if i >= self.len {
            return None;
        }
        let mut word = i / 64;
        let mut bits = self.words.get(word)? & (u64::MAX << (i % 64));
        loop {
            if bits != 0 {
                let p = word
                    .checked_mul(64)?
                    .checked_add(bits.trailing_zeros() as usize)?;
                return if p < self.len { Some(p) } else { None };
            }
            word = word.checked_add(1)?;
            bits = *self.words.get(word)?;
        }
    }

    /// The last set bit at or before `i`, by words.
    pub fn prev_one(&self, i: usize) -> Option<usize> {
        let i = i.min(self.len.checked_sub(1)?);
        let mut word = i / 64;
        let keep = 63usize.saturating_sub(i % 64);
        let mut bits = self.words.get(word)? & (u64::MAX >> keep);
        loop {
            if bits != 0 {
                return word
                    .checked_mul(64)?
                    .checked_add(63usize.saturating_sub(bits.leading_zeros() as usize));
            }
            word = word.checked_sub(1)?;
            bits = *self.words.get(word)?;
        }
    }

    /// Appends the vector to `out`: its length in bits, its rank block in words, its words. The
    /// rank table is rebuilt when it is read.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&u64::try_from(self.len).unwrap_or(u64::MAX).to_le_bytes());
        out.extend_from_slice(
            &u64::try_from(self.block_words)
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        for w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    /// A vector [`Self::encode`] wrote at the start of `bytes`, and the bytes it took.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let word = |at: usize| -> Result<u64, Error> {
            bytes
                .get(at..)
                .and_then(<[u8]>::first_chunk::<8>)
                .map(|b| u64::from_le_bytes(*b))
                .ok_or(Error::Corruption {
                    what: "a succinct trie's bit vector",
                    why: Malformed::Truncated,
                })
        };
        let len = usize::try_from(word(0)?).map_err(|_| corrupt())?;
        let block_words = usize::try_from(word(8)?).map_err(|_| corrupt())?;
        // A block is a power of two of words, as every writer gives it: a rank shifts to it.
        if !block_words.is_power_of_two() {
            return Err(corrupt());
        }
        let n = len.div_ceil(64);
        let mut words = Vec::with_capacity(n.min(bytes.len() / 8));
        let mut at = 16usize;
        for _ in 0..n {
            words.push(word(at)?);
            at = at.checked_add(8).ok_or(corrupt())?;
        }
        // Bits past the length are zero, as `new` leaves them.
        if let (Some(last), r) = (words.last(), len % 64)
            && r != 0
            && last >> r != 0
        {
            return Err(corrupt());
        }
        let mut v = Self {
            words,
            len,
            block_words,
            block_shift: block_words.trailing_zeros(),
            ranks: Vec::new(),
            ones: 0,
        };
        v.index()?;
        Ok((v, at))
    }

    /// Bytes held, for the trie's memory accounting.
    pub fn bytes(&self) -> usize {
        self.words
            .len()
            .saturating_mul(8)
            .saturating_add(self.ranks.len().saturating_mul(4))
    }

    /// The bytes its buffers hold in memory: its words' and its rank samples' capacities.
    pub fn held_bytes(&self) -> usize {
        self.words
            .capacity()
            .saturating_mul(8)
            .saturating_add(self.ranks.capacity().saturating_mul(4))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors appended one to another hold the bits pushed one at a time, at every length and
    /// every offset within a word.
    #[test]
    fn appended_vectors_hold_their_bits_in_order() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut bit = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for case in 0..200 {
            let (mut whole, mut pushed, mut all) = (Grow::default(), Grow::default(), Vec::new());
            for _ in 0..(bit() % 6) {
                let mut part = Grow::default();
                for _ in 0..(bit() % 200) {
                    let b = bit() % 3 == 0;
                    part.push(b);
                    pushed.push(b);
                    all.push(b);
                }
                whole.append(&part);
            }
            assert_eq!(whole.len(), all.len(), "case {case}");
            for (i, &b) in all.iter().enumerate() {
                assert_eq!(whole.get(i), b, "case {case} bit {i}");
            }
            assert_eq!(whole.words, pushed.words, "case {case} words");
        }
    }

    fn naive_rank(bits: &[bool], i: usize) -> usize {
        bits[..=i.min(bits.len() - 1)]
            .iter()
            .filter(|&&b| b)
            .count()
    }

    /// A rank block is a power of two of words, as a rank shifts to it: any other is refused,
    /// built or read.
    #[test]
    fn rank_blocks_not_a_power_of_two_of_words_are_refused() {
        for block in [0, 32, 192, 320] {
            assert!(matches!(
                Bits::new(&[true], block),
                Err(Error::InvalidArgument { .. })
            ));
        }
        let mut out = Vec::new();
        Bits::new(&[true, false, true], 512)
            .unwrap()
            .encode(&mut out);
        assert_eq!(Bits::decode(&out).unwrap().0.rank1(2), 2);
        for words in [0u64, 3, 6] {
            out[8..16].copy_from_slice(&words.to_le_bytes());
            assert!(matches!(
                Bits::decode(&out),
                Err(Error::Corruption {
                    why: Malformed::OutOfRange,
                    ..
                })
            ));
        }
    }

    #[test]
    fn rank_and_scans_agree_with_counting_on_every_position() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for len in [1usize, 63, 64, 65, 511, 512, 513, 2_000, 9_999] {
            for density in [1u64, 3, 17, 50, 97] {
                let bits: Vec<bool> = (0..len)
                    .map(|_| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        x % 100 < density
                    })
                    .collect();
                for block in [64, 128, 512, 4096] {
                    let v = Bits::new(&bits, block).unwrap();
                    assert_eq!(v.ones(), bits.iter().filter(|&&b| b).count());
                    for i in 0..len {
                        assert_eq!(v.get(i), bits[i]);
                        assert_eq!(
                            v.rank1(i),
                            naive_rank(&bits, i),
                            "len {len} block {block} i {i}"
                        );
                    }
                    for i in 0..len {
                        let back = (0..=i).rev().find(|&j| bits[j]);
                        assert_eq!(v.prev_one(i), back, "len {len} prev from {i}");
                        let want = (i..len).find(|&j| bits[j]);
                        assert_eq!(v.next_one(i), want, "len {len} next from {i}");
                    }
                }
            }
        }
    }
}
