//! The bitstreams of Zstandard (RFC 8878): a forward reader for FSE table descriptions
//! (§4.1.1), and the backward reader every FSE- and Huffman-coded stream is read with
//! (§3.1.1.3.2.1.2, §4.2.2).
//!
//! A writer appends each value at the stream's bit position, least significant bit first, so
//! the stream is one little-endian bit string; it ends with a 1 bit and zero padding to the byte.
//! The backward reader starts below that 1 bit and takes each value from the top: `n` bits read
//! are the bits `[pos - n, pos)`, the lowest at `pos - n`. Reading past the start yields zero
//! bits and leaves `pos` negative, which is how the Huffman weights' decoder (§4.2.1.2) learns
//! the stream ended.

use super::Corrupt;

/// Reads `bits` (at most 57) from `bytes` starting at bit `at`, little-endian; bits outside the
/// slice read as zero.
fn bits_at(bytes: &[u8], at: u64, bits: u32) -> u64 {
    if bits == 0 {
        return 0;
    }
    let byte = usize::try_from(at / 8).unwrap_or(usize::MAX);
    let shift = u32::try_from(at % 8).unwrap_or(0);
    let mut word = [0u8; 8];
    if let Some(src) = bytes.get(byte..) {
        let n = src.len().min(8);
        if let (Some(dst), Some(src)) = (word.get_mut(..n), src.get(..n)) {
            dst.copy_from_slice(src);
        }
    }
    let value = u64::from_le_bytes(word) >> shift;
    value
        & u64::MAX
            .checked_shr(64u32.saturating_sub(bits))
            .unwrap_or(0)
}

/// A forward reader over a little-endian bit string (§4.1.1's table descriptions).
pub(super) struct Forward<'a> {
    bytes: &'a [u8],
    pos: u64,
}

impl<'a> Forward<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The next `bits` (at most 32) without consuming them; past the end reads zero.
    pub(super) fn peek(&self, bits: u32) -> u32 {
        u32::try_from(bits_at(self.bytes, self.pos, bits)).unwrap_or(u32::MAX)
    }

    /// Consumes `bits`, refusing to pass the end of the bytes.
    pub(super) fn skip(&mut self, bits: u32) -> Result<(), Corrupt> {
        let pos = self
            .pos
            .checked_add(u64::from(bits))
            .ok_or(Corrupt::Truncated)?;
        let len = u64::try_from(self.bytes.len())
            .unwrap_or(u64::MAX)
            .saturating_mul(8);
        if pos > len {
            return Err(Corrupt::Truncated);
        }
        self.pos = pos;
        Ok(())
    }

    /// The bytes consumed, the last partly used one included (§4.1.1: "the bitstream consumes a
    /// round number of bytes").
    pub(super) fn bytes_used(&self) -> usize {
        usize::try_from(self.pos.div_ceil(8)).unwrap_or(usize::MAX)
    }
}

/// A backward reader over one stream (§3.1.1.3.2.1.2, §4.2.2).
///
/// It holds eight of the stream's bytes as one little-endian word, as the reference's
/// `BIT_DStream_t` holds its container (zstd 1.5.7 `lib/common/bitstream.h`), with the count of
/// the word's bits still unread below the position. Once fewer than 32 remain and lower bytes
/// exist, it steps the word back by the whole bytes read, so a read of up to 32 bits is a shift
/// and a mask.
pub(super) struct Backward<'a> {
    bytes: &'a [u8],
    /// The first of the eight bytes `word` holds; `word` reads zero past the stream's end.
    start: usize,
    word: u64,
    /// Bits of `word` not yet read: 0 to 64 while `start > 0` (at least 32 after a checked
    /// read, at least what [`Self::ensure`] asked for before ensured reads); once `start` is 0,
    /// the bits left in the stream, negative once a read went past its start.
    within: i64,
}

/// The fewest bits the word holds below the position unless it starts at the stream's first
/// byte: the most one read takes.
const READ_MAX: i64 = 32;

impl<'a> Backward<'a> {
    /// A reader positioned below the stream's final 1 bit. A stream that is empty or whose last
    /// byte is zero has no such bit and is corrupt.
    pub(super) fn new(bytes: &'a [u8]) -> Result<Self, Corrupt> {
        let last = *bytes.last().ok_or(Corrupt::Bitstream)?;
        if last == 0 {
            return Err(Corrupt::Bitstream);
        }
        // The highest set bit of the last byte is the end marker; the word's last byte is the
        // stream's, so the bits below it are those of the bytes before it and the marker's.
        let marker = i64::from(7u32.saturating_sub(last.leading_zeros()));
        let start = bytes.len().saturating_sub(8);
        let held =
            i64::try_from(bytes.len().saturating_sub(start)).map_err(|_| Corrupt::Bitstream)?;
        let within = held
            .checked_sub(1)
            .and_then(|b| b.checked_mul(8))
            .and_then(|b| b.checked_add(marker))
            .ok_or(Corrupt::Bitstream)?;
        let mut reader = Self {
            bytes,
            start,
            word: 0,
            within,
        };
        reader.fetch();
        if reader.start > 0 && reader.within < READ_MAX {
            reader.load();
        }
        Ok(reader)
    }

    /// Reads the word at `start`.
    #[inline(always)]
    fn fetch(&mut self) {
        if let Some(word) = self
            .bytes
            .get(self.start..)
            .and_then(<[u8]>::first_chunk::<8>)
        {
            self.word = u64::from_le_bytes(*word);
            return;
        }
        self.fetch_tail();
    }

    /// [`Self::fetch`] for a stream shorter than eight bytes: those past its end read as zero.
    #[cold]
    fn fetch_tail(&mut self) {
        let mut word = [0u8; 8];
        if let Some(src) = self.bytes.get(self.start..) {
            let n = src.len().min(8);
            if let (Some(dst), Some(src)) = (word.get_mut(..n), src.get(..n)) {
                dst.copy_from_slice(src);
            }
        }
        self.word = u64::from_le_bytes(word);
    }

    /// Steps the word back by the whole bytes read from it, as far as the stream's first byte.
    #[inline(always)]
    fn load(&mut self) {
        // `within` is 0 to 64 here, so the bytes read are 0 to 8.
        let read = usize::try_from(64i64.wrapping_sub(self.within) >> 3).unwrap_or(0);
        let back = read.min(self.start);
        self.start = self.start.wrapping_sub(back);
        // `back` is at most 8.
        self.within = self
            .within
            .wrapping_add(i64::try_from(back).unwrap_or(0).wrapping_mul(8));
        self.fetch();
    }

    /// The `bits` (at most 32) below the position without consuming them; bits below the start
    /// read as zero.
    #[inline(always)]
    pub(super) fn peek(&self, bits: u32) -> u32 {
        let n = i64::from(bits);
        if self.within >= n {
            // 0 <= within - n <= 64, and only a 0-bit read reaches 64, which the mask of 0 bits
            // reads as 0 whatever the shift: masked to 6 bits the shift needs no branch, nor
            // does a 0-bit read (the sequence codes' extra bits are 0 as often as not). Both
            // masks put the conversions in range.
            let shift = u32::try_from(self.within.wrapping_sub(n) & 63).unwrap_or(0);
            let value = (self.word >> shift) & low_mask(bits) & u64::from(u32::MAX);
            return u32::try_from(value).unwrap_or(0);
        }
        self.peek_short(bits)
    }

    /// [`Self::peek`] where fewer than `bits` remain: the word starts at the stream's first byte,
    /// and the missing low bits are zero.
    #[cold]
    fn peek_short(&self, bits: u32) -> u32 {
        let value = if self.within > 0 {
            let have = u32::try_from(self.within).unwrap_or(0);
            (self.word & low_mask(have)) << bits.wrapping_sub(have)
        } else {
            0
        };
        u32::try_from(value & low_mask(bits)).unwrap_or(u32::MAX)
    }

    /// Reads and consumes `bits` (at most 32).
    #[inline(always)]
    pub(super) fn read(&mut self, bits: u32) -> u32 {
        let value = self.peek(bits);
        self.skip(bits);
        value
    }

    /// Consumes `bits` (at most 32) already peeked.
    #[inline(always)]
    pub(super) fn skip(&mut self, bits: u32) {
        // `within` stays above -2^32 - 64: it goes negative only once `start` is 0, and a stream
        // read past its start is done.
        self.within = self.within.wrapping_sub(i64::from(bits));
        if self.within < READ_MAX && self.start > 0 {
            self.load();
        }
    }

    /// Steps the word back by the whole bytes read, whether or not enough remain: at least 57
    /// bits are then readable unless the word starts at the stream's first byte. A loop whose
    /// reads vary in length takes this once an iteration over [`Self::ensure`], whose test it
    /// would mispredict (`ZSTD_decodeSequence` reloads once a sequence).
    #[inline(always)]
    pub(super) fn reload(&mut self) {
        self.load();
    }

    /// Makes at least `bits` (at most 57) readable without a refill: refills the word unless
    /// that many remain or it starts at the stream's first byte, where reads past the start
    /// read zero. A refill leaves at least 57 (64 less a partly read byte).
    #[inline(always)]
    pub(super) fn ensure(&mut self, bits: u32) {
        if self.within < i64::from(bits) && self.start > 0 {
            self.load();
        }
    }

    /// Reads and consumes `bits`, which an [`Self::ensure`] since the last refill covers with
    /// the reads after it: no refill is checked for.
    #[inline(always)]
    pub(super) fn read_ensured(&mut self, bits: u32) -> u32 {
        let value = self.peek(bits);
        self.within = self.within.wrapping_sub(i64::from(bits));
        value
    }

    /// Whether every bit was read and none past the start: a stream must be consumed exactly
    /// (§3.1.1.3.2.1.2, §4.2.2).
    pub(super) fn finished(&self) -> bool {
        self.start == 0 && self.within == 0
    }

    /// Whether a read went past the start (§4.2.1.2's end condition).
    pub(super) fn overflowed(&self) -> bool {
        self.within < 0
    }
}

/// The low `bits` (at most 63) set.
#[inline(always)]
fn low_mask(bits: u32) -> u64 {
    (1u64 << (bits & 63)).wrapping_sub(1)
}

/// A writer of a little-endian bit string onto the end of a buffer, each value's least
/// significant bit first: the encoder's side of [`Forward`] and [`Backward`]. Bits gather in a
/// 64-bit word that [`Writer::flush`] stores whole, advancing by the bytes it completed, as the
/// reference's `BIT_CStream_t` does (zstd 1.5.7 lib/common/bitstream.h): the buffer is grown
/// ahead of the bytes written and cut back when the stream ends. A stream for the backward
/// reader ends with [`Writer::close`]'s 1 bit and zero padding (§3.1.1.3.2.1.2, §4.2.2).
#[derive(Debug)]
pub(super) struct Writer<'a> {
    bytes: &'a mut Vec<u8>,
    /// Where the next whole byte goes; `bytes` holds at least 8 bytes from here.
    at: usize,
    /// Bits not yet stored as whole bytes, the oldest lowest.
    acc: u64,
    held: u32,
}

impl<'a> Writer<'a> {
    /// A writer appending to `bytes`, with room ahead for `bits` bits.
    pub(super) fn new(bytes: &'a mut Vec<u8>, bits: usize) -> Self {
        let at = bytes.len();
        bytes.resize(at.saturating_add(bits.div_ceil(8)).saturating_add(8), 0);
        Self {
            bytes,
            at,
            acc: 0,
            held: 0,
        }
    }

    /// Appends the low `bits` (at most 32) of `value`, storing only if the word would overflow:
    /// a caller that flushes as the reference's `BIT_addBits` asks never takes that branch, and
    /// one that does not still loses no bit.
    #[inline(always)]
    pub(super) fn add_held(&mut self, value: u64, bits: u32) {
        if self.held.wrapping_add(bits) > 64 {
            self.flush_unasked();
        }
        // `held` is below 64 here unless `bits` is 0, when the masked value is 0.
        self.acc |= (value & low_mask(bits)) << (self.held & 63);
        self.held = self.held.wrapping_add(bits);
    }

    /// Appends `value`, already below `2^bits`, with no test of the word's room and no mask: for
    /// a caller whose flushes keep the bits held, these included, within the word, as the
    /// reference's `HUF_addBits` relies on its unrolled flushes.
    #[inline(always)]
    pub(super) fn push(&mut self, value: u64, bits: u32) {
        self.acc |= value << (self.held & 63);
        self.held = self.held.wrapping_add(bits);
    }

    /// Appends the low `bits` (at most 32) of `value`, storing whole bytes once 32 bits are held.
    #[inline(always)]
    pub(super) fn add(&mut self, value: u64, bits: u32) {
        self.add_held(value, bits);
        if self.held >= 32 {
            self.flush();
        }
    }

    /// Stores the whole bytes held: the word is written at once and the position advanced past
    /// the complete ones (`BIT_flushBits`).
    #[inline(always)]
    pub(super) fn flush(&mut self) {
        let end = self.at.saturating_add(8);
        if self.bytes.len() < end {
            self.grow(end);
        }
        if let Some(dst) = self.bytes.get_mut(self.at..end) {
            dst.copy_from_slice(&self.acc.to_le_bytes());
        }
        let whole = self.held >> 3;
        self.at = self.at.saturating_add(usize::try_from(whole).unwrap_or(0));
        self.acc = self.acc.checked_shr(whole << 3).unwrap_or(0);
        self.held &= 7;
    }

    /// The flush the word's overflow forces when the caller's flushes did not come in time.
    #[cold]
    #[inline(never)]
    fn flush_unasked(&mut self) {
        self.flush();
    }

    /// Room for a flush past the bound the writer was made with; the buffer's own doubling
    /// keeps repeated growth linear.
    #[cold]
    fn grow(&mut self, end: usize) {
        self.bytes.resize(end, 0);
    }

    /// Ends a stream for the backward reader: a 1 bit, then zeros to the byte.
    pub(super) fn close(mut self) {
        self.add_held(1, 1);
        self.finish();
    }

    /// Ends a forward stream (a table description), its last byte padded with zeros, and cuts
    /// the buffer back to the bytes written.
    pub(super) fn finish(mut self) {
        let partial = usize::from(self.held & 7 != 0);
        self.flush();
        let end = self.at.saturating_add(partial);
        self.bytes.truncate(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// The writer, with no room ahead so every flush grows the buffer, against the bits
        /// written one at a time; the bytes before the stream stay. Flushes come when the
        /// caller's contract asks, at random points besides, or never (`flush_every` past the
        /// values' count), when the writer stores on its own.
        #[test]
        fn the_writer_writes_each_bit_in_order(
            values in proptest::collection::vec((any::<u64>(), 0u32..=32), 0..200),
            flush_every in 1usize..400,
        ) {
            let mut bytes = vec![0xAB];
            let mut w = Writer::new(&mut bytes, 0);
            let mut expected_bits = Vec::new();
            // The caller's part of the contract: no more than 57 bits added between flushes
            // (64 less the 7 a flush may leave).
            let mut since_flush = 0u32;
            let contract = flush_every < 4;
            for (i, &(v, n)) in values.iter().enumerate() {
                if contract && since_flush + n > 57 {
                    w.flush();
                    since_flush = 0;
                }
                w.add_held(v, n);
                since_flush += n;
                expected_bits.extend((0..n).map(|b| (v >> b) & 1 == 1));
                if i % flush_every == 0 {
                    w.flush();
                    since_flush = 0;
                }
            }
            w.close();
            expected_bits.push(true);
            let mut expected = vec![0xAB];
            for chunk in expected_bits.chunks(8) {
                expected.push(
                    chunk.iter().enumerate().fold(0u8, |b, (i, &bit)| b | (u8::from(bit) << i)),
                );
            }
            prop_assert_eq!(bytes, expected);
        }
    }
}
