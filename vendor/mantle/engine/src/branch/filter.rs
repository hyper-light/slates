//! A branch's filter: a blocked Bloom filter (Putze, Sanders and Singler, "Cache-, hash- and
//! space-efficient Bloom filters", WEA 2007) over the branch's keys, so a point read skips a
//! branch that cannot hold its key without reading a page. Every probe of a key falls in one
//! 512-bit block, one cache line, so a check costs one cache miss.
//!
//! Maplets routing a key to its pivot's branches in one lookup were built, measured and removed
//! (docs/research/43-read-accelerators.md): a point read asks each branch's filter.

use crate::error::Error;
use crate::util::xxhash::xxh3_64bits;

/// Cited: bits a key. A Bloom filter of `b` bits a key and `k = b·ln 2` probes has a
/// false-positive rate of about `0.6185^b`, 0.8% at 10 (Broder and Mitzenmacher, "Network
/// applications of Bloom filters: a survey", Internet Mathematics 1(4), 2004, §2.1); 10 is
/// RocksDB's default `bits_per_key`.
pub const BITS_PER_KEY: usize = 10;
/// Derived: probes a key, `round(BITS_PER_KEY · ln 2)` = 7, the rate's minimum.
const PROBES: u32 = 7;
/// A block's 64-bit words: 512 bits, a cache line.
const BLOCK_WORDS: usize = 8;

/// A key's filter hash: computed once a lookup, checked against every branch on its path.
pub fn hash(key: &[u8]) -> u64 {
    xxh3_64bits(key)
}

/// The high 32 bits of a key's filter hash, all a filter's block and probes use: what a memtable
/// keeps of each key so a branch built from it hashes no key again.
pub fn hash32(hash: u64) -> u32 {
    u32::try_from(hash >> 32).unwrap_or(0)
}

/// The filter: its blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    blocks: Vec<[u64; BLOCK_WORDS]>,
}

/// The keys a filter is made for, before they are added: known exactly (a memtable packed), or
/// at most (a compaction, whose duplicates merge).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    Exactly(u64),
    AtMost(u64),
}

/// Blocks for `keys` keys at [`BITS_PER_KEY`] bits each, at least one.
fn blocks_for(keys: u64) -> usize {
    usize::try_from(keys)
        .unwrap_or(usize::MAX)
        .saturating_mul(BITS_PER_KEY)
        .div_ceil(BLOCK_WORDS.saturating_mul(64))
        .max(1)
}

/// Blocks for `keys`: an upper bound gets the least count at or above it whose divisors are dense
/// ([`dense_at_least`]), so the filter folds to within twice what the keys it gets need
/// ([`Filter::fit`]).
fn blocks_of(keys: Keys) -> usize {
    match keys {
        Keys::Exactly(n) => blocks_for(n),
        Keys::AtMost(n) => dense_at_least(blocks_for(n)),
    }
}

/// Whether each of `n`'s divisors, in order, is at most twice the one before. Its divisors past
/// `√n` are the cofactors of those below, their ratios the same in reverse, so the divisors up to
/// `√n` and the step from the largest of them to its cofactor decide it; a gap past twice the last
/// divisor found ends the scan.
fn dense(n: usize) -> bool {
    let mut last = 1usize;
    let mut d = 2usize;
    while d.checked_mul(d).is_some_and(|square| square <= n) {
        // Nothing divided `n` from past the last divisor through twice it.
        if last.checked_mul(2).is_none_or(|twice| d > twice) {
            return false;
        }
        if n.checked_rem(d) == Some(0) {
            last = d;
        }
        let Some(next) = d.checked_add(1) else {
            return false;
        };
        d = next;
    }
    n.checked_div(last)
        .zip(last.checked_mul(2))
        .is_some_and(|(cofactor, twice)| cofactor <= twice)
}

/// The least count at or above `need` whose divisors are each at most twice the one before
/// ([`dense`]): a filter of so many blocks folds to any of its divisors ([`Filter::fit`]), so to
/// less than twice any smaller count. The search ends by `2^a ⌈need / 2^a⌉`, the least `a` with
/// `⌈need / 2^a⌉ <= 2^(a + 1)`: its powers of two and the quotient's doublings each at most double
/// the one before, so it is such a count; the search measured 0.1% above `need` on average and at
/// most 36 counts tried below 20,000 blocks (research/68 §1).
fn dense_at_least(need: usize) -> usize {
    let mut bound = need;
    for a in 0..usize::BITS {
        let Some(step) = 1usize.checked_shl(a) else {
            break;
        };
        let quotient = need.div_ceil(step);
        if step.checked_mul(2).is_some_and(|most| quotient <= most) {
            bound = quotient.checked_mul(step).unwrap_or(need);
            break;
        }
    }
    (need..=bound).find(|&n| dense(n)).unwrap_or(bound)
}

/// The bytes a filter made for `keys` holds: its blocks, reserved exactly ([`Filter::try_new`]).
pub fn most_bytes(keys: Keys) -> Option<usize> {
    blocks_of(keys).checked_mul(BLOCK_WORDS.checked_mul(8)?)
}

/// The most bytes a branch's filter of `keys` keys holds once fitted to them ([`Filter::fit`]):
/// twice the blocks they need, a filter made for exactly them holding just those and one made for
/// a bound folding to fewer than twice. The blocks a count of keys needs are at most the sum of
/// what its parts need, so twice them, stated for each part, covers the whole.
pub fn fitted_most_bytes(keys: u64) -> Option<usize> {
    blocks_for(keys)
        .checked_mul(2)?
        .checked_mul(BLOCK_WORDS.checked_mul(8)?)
}

/// The block for a hash: its high 32 bits scaled into the block count (Lemire, "Fast random
/// integer generation in an interval", ACM TOMACS 29(1), 2019).
fn block_of(h: u64, blocks: usize) -> usize {
    let n = u64::try_from(blocks).unwrap_or(u64::MAX);
    usize::try_from(((h >> 32).wrapping_mul(n)) >> 32).unwrap_or(0)
}

/// The probes' bit positions within a block, from the hash's high 32 bits remixed by the golden
/// ratio each probe, its top 9 bits a position (RocksDB's FastLocalBloom derives its probes so,
/// from the low 32). The high 32 are what a memtable keeps of each key ([`hash32`]), so a filter
/// can be rebuilt at any size from them without reading the keys (research/39 §3).
fn probes(h: u64) -> impl Iterator<Item = (usize, u64)> {
    let x = u32::try_from(h >> 32).unwrap_or(0);
    // Each remix from `x` by the multiplier's power, the chained remix's value (`x·G·G = x·G²`
    // mod 2^32): the probes wait on no other probe's multiply.
    POWERS.into_iter().map(move |g| {
        let bit = x.wrapping_mul(g) >> 23;
        (usize::try_from(bit >> 6).unwrap_or(0), 1u64 << (bit & 63))
    })
}

/// The golden-ratio multiplier's powers 1 to [`PROBES`], mod 2^32: probe `i`'s remix.
const POWERS: [u32; PROBES as usize] = [
    power(1),
    power(2),
    power(3),
    power(4),
    power(5),
    power(6),
    power(7),
];

/// The golden-ratio multiplier to the `n`th power, mod 2^32.
const fn power(n: u32) -> u32 {
    let mut g: u32 = 1;
    let mut i = 0;
    while i < n {
        g = g.wrapping_mul(0x9E37_79B9);
        i = i.wrapping_add(1);
    }
    g
}

impl Filter {
    /// No filter at all, no memory: a branch's descriptor sent to a merge, which never asks it
    /// ([`super::Branch::for_merge`]).
    pub fn none() -> Self {
        Self { blocks: Vec::new() }
    }

    /// An empty filter for `keys`: sized for them when known exactly; when only bounded, for
    /// the bound, at a count of blocks [`Self::fit`] folds to within twice what the keys it gets
    /// need once their count is known.
    pub fn new(keys: Keys) -> Self {
        Self {
            blocks: vec![[0u64; BLOCK_WORDS]; blocks_of(keys)],
        }
    }

    /// [`Self::new`] for keys a caller names: a count the allocator refuses is refused, not a
    /// panic.
    pub fn try_new(keys: Keys) -> Result<Self, Error> {
        let count = blocks_of(keys);
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(count)
            .map_err(|_| Error::LimitExceeded {
                what: "a branch filter's blocks (the allocator refused them)",
                limit: match keys {
                    Keys::Exactly(n) | Keys::AtMost(n) => n,
                },
            })?;
        blocks.resize(count, [0u64; BLOCK_WORDS]);
        Ok(Self { blocks })
    }

    /// Empties the filter for `keys`, as [`Self::new`] sizes it, keeping its allocation.
    pub fn reset(&mut self, keys: Keys) {
        self.blocks.clear();
        self.blocks.resize(blocks_of(keys), [0u64; BLOCK_WORDS]);
    }

    /// Adds a key of hash `h`.
    pub fn insert(&mut self, h: u64) {
        let count = self.blocks.len();
        if let Some(block) = self.blocks.get_mut(block_of(h, count)) {
            for (word, mask) in probes(h) {
                if let Some(w) = block.get_mut(word) {
                    *w |= mask;
                }
            }
        }
    }

    /// Folds the filter to the fewest of its blocks' divisors that still give `keys` keys
    /// [`BITS_PER_KEY`] bits each: blocks `r i .. r i + r` OR into block `i`, in place, `r` the
    /// fold. A key's block at `n / r` blocks is its block at `n` divided by `r`
    /// (`⌊⌊x·n / 2^32⌋ / r⌋ = ⌊x·(n/r) / 2^32⌋`, for any `n` and any `r` dividing it) and its
    /// probes within a block do not depend on `n`, so the folded filter is the one its keys would
    /// have made at that size. A bounded filter's divisors are each at most twice the one before
    /// ([`dense_at_least`]), so it folds to fewer than twice the blocks its keys need.
    pub fn fit(&mut self, keys: u64) {
        let need = blocks_for(keys);
        let len = self.blocks.len();
        // No divisor but `len` itself reaches past `len / 2`: a filter made for exactly its keys
        // folds no further.
        if need.checked_mul(2).is_none_or(|twice| twice > len) {
            self.blocks.shrink_to_fit();
            return;
        }
        // The least divisor at or above `need`, from the divisors up to `√len` and their
        // cofactors.
        let mut least = len;
        let mut d = 1usize;
        while d.checked_mul(d).is_some_and(|square| square <= len) {
            if len.checked_rem(d) == Some(0) {
                for divisor in [d, len.checked_div(d).unwrap_or(len)] {
                    if divisor >= need && divisor < least {
                        least = divisor;
                    }
                }
            }
            let Some(next) = d.checked_add(1) else {
                break;
            };
            d = next;
        }
        let fold = len.checked_div(least).unwrap_or(1);
        if fold > 1 {
            for i in 0..least {
                let from = i.saturating_mul(fold);
                let mut block = [0u64; BLOCK_WORDS];
                for source in self
                    .blocks
                    .get(from..from.saturating_add(fold))
                    .unwrap_or(&[])
                {
                    for (w, s) in block.iter_mut().zip(source) {
                        *w |= s;
                    }
                }
                // Block `i` is read before it is written: `i <= from`.
                if let Some(dst) = self.blocks.get_mut(i) {
                    *dst = block;
                }
            }
            self.blocks.truncate(least);
        }
        self.blocks.shrink_to_fit();
    }

    /// Appends the bytes `range` of [`Self::to_bytes`]'s stream to `out`, from the blocks as they
    /// are: a page of the filter written without the stream copied first.
    pub fn copy_bytes(&self, range: std::ops::Range<usize>, out: &mut Vec<u8>) {
        // The words from the range's first, found by index: skipping through the blocks cost
        // every block before it for each page written, 100 µs and more a page of a compaction's
        // filter, on the put that paid for it.
        let first = range.start / 8;
        let words = self.blocks.as_flattened().get(first..).unwrap_or(&[]);
        for (k, w) in words.iter().enumerate() {
            let at = first.saturating_add(k).saturating_mul(8);
            if at >= range.end {
                break;
            }
            let bytes = w.to_le_bytes();
            let lo = range.start.saturating_sub(at).min(8);
            let hi = range.end.saturating_sub(at).min(8);
            out.extend_from_slice(bytes.get(lo..hi).unwrap_or(&[]));
        }
    }

    /// A filter over keys of `hashes`, at [`BITS_PER_KEY`] bits each, at least one block.
    pub fn build(hashes: &[u64]) -> Self {
        let mut f = Self::new(Keys::Exactly(
            u64::try_from(hashes.len()).unwrap_or(u64::MAX),
        ));
        for &h in hashes {
            f.insert(h);
        }
        f
    }

    /// Whether a key of hash `h` may be in the branch: false only when it is not.
    /// The first word of the block `h` falls in, read and nothing decided: a lookup reads every
    /// branch's block on its path before it asks any, so their cache misses overlap rather than
    /// follow one another (the loads are independent; the asks then find them in the cache).
    pub fn touch(&self, h: u64) -> u64 {
        self.blocks
            .get(block_of(h, self.blocks.len()))
            .and_then(|b| b.first().copied())
            .unwrap_or(0)
    }

    pub fn may_contain(&self, h: u64) -> bool {
        let Some(block) = self.blocks.get(block_of(h, self.blocks.len())) else {
            return true;
        };
        probes(h).all(|(word, mask)| block.get(word).is_some_and(|w| w & mask != 0))
    }

    /// The filter as bytes: each block's words, little-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.blocks
            .iter()
            .flat_map(|b| b.iter().flat_map(|w| w.to_le_bytes()))
            .collect()
    }

    /// A filter from [`Self::to_bytes`]'s bytes: whole blocks, at least one.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let block = BLOCK_WORDS.checked_mul(8)?;
        if bytes.is_empty() || !bytes.len().is_multiple_of(block) {
            return None;
        }
        let blocks = bytes
            .chunks_exact(block)
            .map(|chunk| {
                let mut b = [0u64; BLOCK_WORDS];
                for (w, c) in b.iter_mut().zip(chunk.as_chunks::<8>().0) {
                    *w = u64::from_le_bytes(*c);
                }
                b
            })
            .collect();
        Some(Self { blocks })
    }

    /// The filter's bytes in memory.
    pub fn bytes(&self) -> usize {
        self.blocks
            .len()
            .saturating_mul(BLOCK_WORDS.saturating_mul(8))
    }

    /// The bytes its blocks hold in memory, their capacity: its bytes, the blocks being
    /// reserved exactly and shrunk to fit.
    pub fn held_bytes(&self) -> usize {
        self.blocks
            .capacity()
            .saturating_mul(BLOCK_WORDS.saturating_mul(8))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_built_in_is_found_and_the_false_positive_rate_is_near_the_bound() {
        let keys: Vec<Vec<u8>> = (0..100_000u32)
            .map(|i| format!("key-{i}").into_bytes())
            .collect();
        let hashes: Vec<u64> = keys.iter().map(|k| hash(k)).collect();
        let f = Filter::build(&hashes);
        assert!(hashes.iter().all(|&h| f.may_contain(h)));
        let false_positives = (0..100_000u32)
            .filter(|i| f.may_contain(hash(format!("other-{i}").as_bytes())))
            .count();
        // The bound for 10 bits a key is 0.8% unblocked; blocking costs a little. Over 100,000
        // absent keys, 2% is far past any rate a working filter gives.
        println!("false positives {false_positives} of 100000");
        assert!(false_positives < 2_000, "{false_positives}");
        assert_eq!(
            f.bytes(),
            100_000usize.saturating_mul(10).div_ceil(512) * 64
        );
    }

    /// A filter of `blocks` blocks with `hashes` added, made at that size directly.
    fn made_at(blocks: usize, hashes: &[u64]) -> Filter {
        let mut f = Filter {
            blocks: vec![[0u64; BLOCK_WORDS]; blocks],
        };
        for &h in hashes {
            f.insert(h);
        }
        f
    }

    #[test]
    fn a_bounded_count_is_the_least_dense_one_at_or_above_the_bound() {
        // Against every divisor listed: each at most twice the one before, and no count between
        // the bound and the one chosen so.
        let listed = |n: usize| (1..=n).filter(|d| n.is_multiple_of(*d)).collect::<Vec<_>>();
        let dense_listed = |n: usize| listed(n).windows(2).all(|w| w[1] <= 2 * w[0]);
        for need in 1..2_000usize {
            let chosen = dense_at_least(need);
            assert!(chosen >= need && dense_listed(chosen), "{need} -> {chosen}");
            assert!(
                (need..chosen).all(|n| !dense_listed(n)),
                "{need} -> {chosen}"
            );
            assert_eq!(dense(need), dense_listed(need), "{need}");
        }
    }

    #[test]
    fn a_bounded_filter_fitted_to_its_keys_is_the_one_they_make_at_under_twice_their_blocks() {
        // Bounded at 400,000 keys: 7,813 blocks, at least; 30,000 keys added need 586.
        let mut f = Filter::new(Keys::AtMost(400_000));
        assert_eq!(f.bytes(), dense_at_least(7_813) * 64);
        let hashes: Vec<u64> = (0..30_000u32)
            .map(|i| hash(format!("key-{i}").as_bytes()))
            .collect();
        for &h in &hashes {
            f.insert(h);
        }
        f.fit(30_000);
        let blocks = f.bytes() / 64;
        assert!((586..2 * 586).contains(&blocks), "{blocks} blocks");
        assert_eq!(f, made_at(blocks, &hashes));
        assert!(hashes.iter().all(|&h| f.may_contain(h)));
        // Every count of keys a bound of 4,000 can get: the fitted filter is the one those keys
        // make at its size, under twice the blocks they need and within what is stated.
        let hashes: Vec<u64> = (0..4_000u32).map(|i| hash(&i.to_le_bytes())).collect();
        for keys in (1..=4_000usize).step_by(7) {
            let mut f = Filter::new(Keys::AtMost(4_000));
            for &h in &hashes[..keys] {
                f.insert(h);
            }
            f.fit(keys as u64);
            let need = blocks_for(keys as u64);
            let blocks = f.bytes() / 64;
            assert!(
                need <= blocks && blocks < 2 * need,
                "{keys} keys: {blocks} blocks"
            );
            assert!(f.bytes() <= fitted_most_bytes(keys as u64).unwrap());
            assert_eq!(f, made_at(blocks, &hashes[..keys]), "{keys} keys");
        }
        // A filter made for exactly its keys stays as it is.
        let mut g = Filter::new(Keys::Exactly(1_000));
        g.fit(1_000);
        assert_eq!(g.bytes(), 20 * 64);
    }

    #[test]
    fn a_filter_rebuilt_from_the_keys_32_bit_hashes_is_the_one_built_from_the_keys() {
        // What a memtable keeps of each key (hash32) rebuilds its filter exactly, at this size
        // or any other, without the keys (research/39 §3).
        let hashes: Vec<u64> = (0..50_000u32)
            .map(|i| hash(format!("k{i}").as_bytes()))
            .collect();
        let kept: Vec<u64> = hashes.iter().map(|&h| u64::from(hash32(h)) << 32).collect();
        assert_eq!(Filter::build(&hashes), Filter::build(&kept));
    }

    #[test]
    fn copied_ranges_are_the_byte_stream() {
        let mut f = Filter::new(Keys::Exactly(5_000));
        for i in 0..5_000u32 {
            f.insert(hash(&i.to_le_bytes()));
        }
        let stream = f.to_bytes();
        // Ranges cut at a page's capacity, which falls inside a word and inside a block.
        for cap in [4_076usize, 4_096, 7, 64, 63] {
            let mut joined = Vec::new();
            for start in (0..stream.len()).step_by(cap) {
                f.copy_bytes(start..(start + cap).min(stream.len()), &mut joined);
            }
            assert_eq!(joined, stream, "capacity {cap}");
        }
    }
}
