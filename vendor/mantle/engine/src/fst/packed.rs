//! Values of a fixed width from 0 to 32 bits packed back to back in 64-bit words, a value
//! crossing a word boundary where it falls: a trie's values at the width its largest needs (a
//! leaf's page number in the bits the branch's pages take, a range filter's suffix in its
//! suffix bits; research/35 §2).

use crate::error::{Error, Malformed};

fn corrupt() -> Error {
    Error::Corruption {
        what: "a succinct trie's values",
        why: Malformed::OutOfRange,
    }
}

/// The widest value: a trie's values are 32-bit.
pub const MAX_WIDTH: u32 = 32;

/// Values of `width` bits each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    width: u32,
    len: usize,
    words: Vec<u64>,
}

/// The bits `v` needs: 0 for 0.
pub fn width_of(v: u32) -> u32 {
    u32::BITS.saturating_sub(v.leading_zeros())
}

impl Packed {
    /// No values yet, each to be `width` bits.
    pub fn new(width: u32) -> Result<Self, Error> {
        if width > MAX_WIDTH {
            return Err(Error::InvalidArgument {
                what: "packed values wider than 32 bits",
            });
        }
        Ok(Self {
            width,
            len: 0,
            words: Vec::new(),
        })
    }

    /// Bits a value takes.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The values held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no value is held.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Words holding `len` values.
    fn words_for(&self, len: usize) -> Option<usize> {
        len.checked_mul(usize::try_from(self.width).ok()?)
            .map(|b| b.div_ceil(64))
    }

    /// Room for `more` values past those held, taken once where the total is known.
    pub fn reserve(&mut self, more: usize) -> Result<(), Error> {
        let words = self
            .len
            .checked_add(more)
            .and_then(|n| self.words_for(n))
            .ok_or(corrupt())?;
        self.words
            .reserve_exact(words.saturating_sub(self.words.len()));
        Ok(())
    }

    /// Appends `v`, which must fit the width. Written into the words in place, as a builder
    /// pushes a value a key: sizing the words and placing the value by its index cost a range
    /// filter's build 18 instructions a key more (research/67 §7).
    #[inline(always)]
    pub fn push(&mut self, v: u32) -> Result<(), Error> {
        if width_of(v) > self.width {
            return Err(Error::InvalidArgument {
                what: "a value wider than its packed width",
            });
        }
        let len = self.len.checked_add(1).ok_or(corrupt())?;
        // Words held are always those `len` values take: a value starting a word starts one, and
        // one past the last word's bits ends in one more, at most 32 bits wide.
        let w = self.width as usize;
        let bit = self.len.checked_mul(w).ok_or(corrupt())?;
        let off = bit % 64;
        let v = u64::from(v);
        if off == 0 {
            if w > 0 {
                self.words.push(v);
            }
        } else {
            let last = self.words.last_mut().ok_or(corrupt())?;
            *last |= v << off;
            if off.saturating_add(w) > 64 {
                self.words.push(v >> 64usize.saturating_sub(off));
            }
        }
        self.len = len;
        Ok(())
    }

    /// Appends `other`'s values, of the same width, a word at a time: each of its words split
    /// across the two words it lands in. Bits past either's values are clear, so a word past the
    /// new length would hold none, and is not taken.
    pub fn append(&mut self, other: &Packed) -> Result<(), Error> {
        if other.width != self.width {
            return Err(Error::InvalidArgument {
                what: "packed values appended at another width",
            });
        }
        let len = self.len.checked_add(other.len).ok_or(corrupt())?;
        let need = self.words_for(len).ok_or(corrupt())?;
        let shift = self
            .len
            .checked_mul(usize::try_from(self.width).map_err(|_| corrupt())?)
            .ok_or(corrupt())?
            % 64;
        let words = other
            .words
            .get(..other.words_for(other.len).ok_or(corrupt())?)
            .ok_or(corrupt())?;
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
        Ok(())
    }

    /// Value `i`.
    pub fn get(&self, i: usize) -> Option<u32> {
        if i >= self.len {
            return None;
        }
        let w = self.width as usize;
        if w == 0 {
            return Some(0);
        }
        let bit = i.checked_mul(w)?;
        let (word, off) = (bit / 64, bit % 64);
        let mut v = self.words.get(word)? >> off;
        if off.saturating_add(w) > 64 {
            v |= self.words.get(word.saturating_add(1))? << (64usize.saturating_sub(off));
        }
        // At most 32 bits wide: the shift is in range.
        u32::try_from(v & (1u64 << w).wrapping_sub(1)).ok()
    }

    /// Takes off the last value.
    pub fn pop(&mut self) -> Option<u32> {
        let i = self.len.checked_sub(1)?;
        let v = self.get(i)?;
        let w = self.width as usize;
        let bit = i.checked_mul(w)?;
        // Clear its bits: the words past the shorter length go, the last one kept masked.
        self.len = i;
        let words = self.words_for(i)?;
        self.words.truncate(words);
        if bit % 64 != 0
            && let Some(x) = self.words.last_mut()
        {
            *x &= (1u64 << (bit % 64)).wrapping_sub(1);
        }
        Some(v)
    }

    /// Empties it, keeping its words' room, its values to be `width` bits.
    pub fn reset(&mut self, width: u32) -> Result<(), Error> {
        if width > MAX_WIDTH {
            return Err(Error::InvalidArgument {
                what: "packed values wider than 32 bits",
            });
        }
        self.width = width;
        self.len = 0;
        self.words.clear();
        Ok(())
    }

    /// Appends the values to `out`: the width, the count, the words.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(u8::try_from(self.width).unwrap_or(u8::MAX));
        out.extend_from_slice(&u64::try_from(self.len).unwrap_or(u64::MAX).to_le_bytes());
        for w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    /// Values [`Self::encode`] wrote at the start of `bytes`, and the bytes they took.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let truncated = || Error::Corruption {
            what: "a succinct trie's values",
            why: Malformed::Truncated,
        };
        let width = u32::from(*bytes.first().ok_or(truncated())?);
        let len = bytes
            .get(1..)
            .and_then(<[u8]>::first_chunk::<8>)
            .map(|b| u64::from_le_bytes(*b))
            .ok_or(truncated())?;
        let mut v = Self::new(width).map_err(|_| corrupt())?;
        v.len = usize::try_from(len).map_err(|_| corrupt())?;
        let n = v.words_for(v.len).ok_or(corrupt())?;
        let end = n
            .checked_mul(8)
            .and_then(|b| b.checked_add(9))
            .ok_or(corrupt())?;
        let body = bytes.get(9..end).ok_or(truncated())?;
        v.words = body
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| u64::from_le_bytes(*b))
            .collect();
        // Bits past the last value are clear, as `push` leaves them.
        let used = v.len.checked_mul(width as usize).ok_or(corrupt())? % 64;
        if used != 0 && v.words.last().is_some_and(|l| l >> used != 0) {
            return Err(corrupt());
        }
        Ok((v, end))
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.words.len().saturating_mul(8)
    }

    /// The bytes its words hold in memory, their capacity.
    pub fn held_bytes(&self) -> usize {
        self.words.capacity().saturating_mul(8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values appended a sequence at a time are those pushed one at a time, at every width and
    /// every offset within a word.
    #[test]
    fn appended_values_are_the_values_pushed() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for width in 0..=MAX_WIDTH {
            let (mut whole, mut pushed) =
                (Packed::new(width).unwrap(), Packed::new(width).unwrap());
            for _ in 0..6 {
                let mut part = Packed::new(width).unwrap();
                for _ in 0..(next() % 150) {
                    let v = u32::try_from(next() % (1u64 << width)).unwrap();
                    part.push(v).unwrap();
                    pushed.push(v).unwrap();
                }
                whole.append(&part).unwrap();
            }
            assert_eq!(whole, pushed, "width {width}");
        }
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn values_of_every_width_read_back_across_word_boundaries() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for width in 0..=MAX_WIDTH {
            let mask = if width == 32 {
                u32::MAX
            } else {
                (1u32 << width) - 1
            };
            let mut p = Packed::new(width).unwrap();
            let mut want = Vec::new();
            for _ in 0..300 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let v = (x & u64::from(mask)) as u32;
                p.push(v).unwrap();
                want.push(v);
            }
            for (i, v) in want.iter().enumerate() {
                assert_eq!(p.get(i), Some(*v), "width {width} i {i}");
            }
            assert_eq!(p.get(want.len()), None);
            let mut out = Vec::new();
            p.encode(&mut out);
            assert_eq!(Packed::decode(&out).unwrap(), (p.clone(), out.len()));
            for cut in 0..out.len() {
                assert!(
                    Packed::decode(&out[..cut]).is_err(),
                    "width {width} cut {cut}"
                );
            }
            // Popped back to empty, each value comes off as pushed and the rest stay.
            while let Some(v) = p.pop() {
                assert_eq!(Some(v), want.pop());
                for (i, w) in want.iter().enumerate().rev().take(3) {
                    assert_eq!(p.get(i), Some(*w));
                }
                p.push(v).unwrap();
                assert_eq!(p.pop(), Some(v));
            }
            if width < 32 {
                assert!(p.push(mask + 1).is_err());
            }
        }
        assert!(Packed::new(33).is_err());
    }
}
