//! The built-in user-key comparators of RocksDB's `util/comparator.cc`: `BytewiseComparator` and
//! `ReverseBytewiseComparator`.
//!
//! RocksDB takes any `Comparator` object; the port supports these two and no user-defined
//! timestamps (docs/research/24 §1.3 DECISION), so the comparator is an enum and a call is a
//! match instead of a virtual call. A MANIFEST naming another comparator is refused at open (P6).

use std::cmp::Ordering;

/// `memcmp` then length: the order of `[u8]`, compared eight bytes at a time as big-endian
/// words, whose order is the bytes' order, so that a key compare is a few loads inline rather
/// than a call to the platform's `memcmp`.
#[inline]
pub fn bytewise(a: &[u8], b: &[u8]) -> Ordering {
    let (mut x, mut y) = (a, b);
    while let (Some((xw, xr)), Some((yw, yr))) =
        (x.split_first_chunk::<8>(), y.split_first_chunk::<8>())
    {
        let (u, v) = (u64::from_be_bytes(*xw), u64::from_be_bytes(*yw));
        if u != v {
            return u.cmp(&v);
        }
        x = xr;
        y = yr;
    }
    for (p, q) in x.iter().zip(y) {
        if p != q {
            return p.cmp(q);
        }
    }
    x.len().cmp(&y.len())
}

/// A user-key order, by the name RocksDB records in the MANIFEST and OPTIONS files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Comparator {
    /// `leveldb.BytewiseComparator`: `memcmp` order, shorter first on a common prefix.
    #[default]
    Bytewise,
    /// `rocksdb.ReverseBytewiseComparator`: the reverse of bytewise.
    ReverseBytewise,
}

impl Comparator {
    /// The comparator's name as RocksDB writes it [R util/comparator.cc:33, :155].
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bytewise => "leveldb.BytewiseComparator",
            Self::ReverseBytewise => "rocksdb.ReverseBytewiseComparator",
        }
    }

    /// The comparator a stored name denotes, or `None` for a name the port does not support.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Bytewise, Self::ReverseBytewise]
            .into_iter()
            .find(|c| c.name() == name)
    }

    /// `Compare` [R util/comparator.cc:36-38, :159-161]. `Slice::compare` is `memcmp` then
    /// length, which is Rust's slice order.
    #[inline]
    pub fn compare(self, a: &[u8], b: &[u8]) -> Ordering {
        match self {
            Self::Bytewise => bytewise(a, b),
            Self::ReverseBytewise => bytewise(b, a),
        }
    }

    /// `Equal`: both orders are equal only on equal bytes
    /// (`CanKeysWithDifferentByteContentsBeEqual` is false for both).
    #[inline]
    pub fn equal(self, a: &[u8], b: &[u8]) -> bool {
        a == b
    }

    /// `FindShortestSeparator` [R util/comparator.cc:42-97, :163-207]: shortens `start` to a key
    /// in `[start, limit)` when one exists, and otherwise leaves it.
    pub fn find_shortest_separator(self, start: &mut Vec<u8>, limit: &[u8]) {
        let diff_index = start.iter().zip(limit).take_while(|(a, b)| a == b).count();
        let min_length = start.len().min(limit.len());
        if diff_index >= min_length {
            // One is a prefix of the other: not shortened.
            return;
        }
        let (Some(&start_byte), Some(&limit_byte)) = (start.get(diff_index), limit.get(diff_index))
        else {
            return;
        };
        match self {
            Self::Bytewise => {
                if start_byte >= limit_byte {
                    // Limit is below start, or start is already the shortest.
                    return;
                }
                // `diff_index < limit.len() - 1`, written without the subtraction.
                let limit_has_more = diff_index.checked_add(1).is_some_and(|n| n < limit.len());
                if limit_has_more || start_byte.checked_add(1).is_some_and(|b| b < limit_byte) {
                    // start_byte < limit_byte <= 0xFF, so the increment does not wrap.
                    if let Some(byte) = start.get_mut(diff_index) {
                        *byte = start_byte.wrapping_add(1);
                    }
                    start.truncate(diff_index.saturating_add(1));
                } else {
                    // Incrementing this byte would reach limit; skip it and increment the
                    // first byte after it that is not 0xFF.
                    let next = start
                        .iter()
                        .enumerate()
                        .skip(diff_index.saturating_add(1))
                        .find(|&(_, &b)| b < 0xFF)
                        .map(|(i, _)| i);
                    if let Some(i) = next {
                        if let Some(byte) = start.get_mut(i) {
                            *byte = byte.wrapping_add(1);
                        }
                        start.truncate(i.saturating_add(1));
                    }
                }
            }
            Self::ReverseBytewise => {
                // `diff_index < start.len() - 1`: start has a byte after the differing one.
                let start_has_more = diff_index.checked_add(1).is_some_and(|n| n < start.len());
                if start_byte > limit_byte && start_has_more {
                    start.truncate(diff_index.saturating_add(1));
                }
            }
        }
    }

    /// `FindShortSuccessor` [R util/comparator.cc:99-111, :209-211]: the shortest key at or
    /// above `key`. The reverse order leaves the key as it is.
    pub fn find_short_successor(self, key: &mut Vec<u8>) {
        if self == Self::ReverseBytewise {
            return;
        }
        if let Some(i) = key.iter().position(|&b| b != 0xFF) {
            if let Some(byte) = key.get_mut(i) {
                *byte = byte.wrapping_add(1);
            }
            key.truncate(i.saturating_add(1));
        }
        // A run of 0xFF is left alone.
    }

    /// `IsSameLengthImmediateSuccessor` [R util/comparator.cc:113-137, :213-221]: whether `t`
    /// is the key of `s`'s length that immediately follows it. Always false for the reverse
    /// order, as RocksDB returns.
    pub fn is_same_length_immediate_successor(self, s: &[u8], t: &[u8]) -> bool {
        if self == Self::ReverseBytewise || s.len() != t.len() || s.is_empty() {
            return false;
        }
        let Some(diff) = s.iter().zip(t).position(|(a, b)| a != b) else {
            return false;
        };
        let (Some(&byte_s), Some(&byte_t)) = (s.get(diff), t.get(diff)) else {
            return false;
        };
        byte_s != 0xFF
            && byte_s.wrapping_add(1) == byte_t
            && s.iter()
                .zip(t)
                .skip(diff.saturating_add(1))
                .all(|(&a, &b)| a == 0xFF && b == 0x00)
    }
}
