//! A range filter of a sorted run's keys (SuRF, Zhang et al. SIGMOD 2018 §3; research/35 §2): a
//! succinct trie of each key cut to the prefix that tells it from its neighbours, with the key's
//! next `n` bits after the cut as its value (SuRF-Real). It answers whether a range may hold a key
//! with no false negative.
//!
//! An entry, prefix `p` and suffix `s`, stands for the keys that start with `p` and whose next `n`
//! bits, zero past the key's end, are `s`: in byte order, one interval, from `p` followed by `s`'s
//! bits (trailing zero bytes dropped, which sort first) to before `p` followed by `s + 1`. Every key
//! is in its entry's interval, so a range meeting no interval holds no key. The entries a range
//! `[from, end)` may meet are the stored prefixes of `from`, which the trie alone cannot order
//! against it, and the least stored prefix at or past `from`, whose interval starts lowest of all
//! the rest; each is checked against the range by its bits.
//!
//! SuRF's cut has no bound: keys in pairs sharing all but a last byte make a label of each byte
//! (Zhang et al., TODS 2020 §7, its worst case). So an entry runs at most `depth` bytes past the
//! bytes every key of the run shares, which the builder is told, its reach, and one cut short there
//! is whole: it stands for every key that starts with it, and the keys after it that start with it
//! are its own and take no entry. A whole entry is the reach long with every suffix bit set, so the
//! filter tells it by its length and value: an entry whose own cut falls at the reach with those
//! bits is read as whole too, the wider interval. The trie so holds at most `depth` levels below the
//! shared bytes and `depth` labels a key (research/67). Cutting keys at one depth keeps their order,
//! so the filter still lets through every range holding a key; it lets through more that hold none
//! (Proteus, Knorr et al. SIGMOD 2022 §4.1-§4.2).

use std::cmp::Ordering;
use std::ops::ControlFlow;

use super::packed::MAX_WIDTH;
use super::trie::{Trie, TrieBuilder, Width};
use crate::error::{Error, Malformed};

/// Bytes a suffix of at most 32 bits spans.
const SUFFIX_BYTES: usize = 4;

/// The `n` bits of `key` after its first `at` bytes, zero past its end, most significant first.
fn suffix_of(key: &[u8], at: usize, n: u32) -> u32 {
    let bytes = n.div_ceil(8) as usize;
    let mut v = 0u64;
    for i in 0..bytes {
        let b = at
            .checked_add(i)
            .and_then(|j| key.get(j))
            .copied()
            .unwrap_or(0);
        v = (v << 8) | u64::from(b);
    }
    let spare = u32::try_from(bytes.saturating_mul(8))
        .unwrap_or(0)
        .saturating_sub(n);
    u32::try_from(v >> spare).unwrap_or(u32::MAX)
}

/// `s`'s `n` bits as bytes, left aligned, trailing zero bytes dropped: what follows the prefix in
/// the least key of an entry's interval.
fn suffix_bytes(s: u32, n: u32) -> ([u8; SUFFIX_BYTES], usize) {
    let bytes = n.div_ceil(8) as usize;
    let spare = u32::try_from(bytes.saturating_mul(8))
        .unwrap_or(0)
        .saturating_sub(n);
    // `s` left aligned in its bytes, those at the top of a word; no bytes, no bits.
    let top = u32::try_from(64usize.saturating_sub(bytes.saturating_mul(8))).unwrap_or(64);
    let aligned = u64::from(s)
        .checked_shl(spare)
        .and_then(|v| v.checked_shl(top))
        .unwrap_or(0);
    let all = aligned.to_be_bytes();
    let mut out = [0u8; SUFFIX_BYTES];
    out.copy_from_slice(all.get(..SUFFIX_BYTES).unwrap_or(&[0; SUFFIX_BYTES]));
    let mut len = bytes;
    while len > 0 && out.get(len.saturating_sub(1)) == Some(&0) {
        len = len.saturating_sub(1);
    }
    (out, len)
}

/// A value of `n` bits, every one set: a whole entry's.
fn all_set(n: u32) -> u32 {
    u32::MAX.checked_shr(32u32.saturating_sub(n)).unwrap_or(0)
}

/// Whether the least key of the entry `prefix`, `s` is below `end` (no end: always).
fn starts_below(prefix: &[u8], s: u32, n: u32, end: Option<&[u8]>) -> bool {
    let Some(end) = end else { return true };
    let (bytes, len) = suffix_bytes(s, n);
    let lo = prefix.iter().chain(bytes.get(..len).unwrap_or(&[]));
    lo.cmp(end.iter()) == Ordering::Less
}

/// A run's range filter.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Surf {
    trie: Trie,
    /// Suffix bits an entry keeps.
    suffix: u32,
    /// The bytes every key shares and the depth past them: an entry this long with every suffix
    /// bit set is whole, standing for every key that starts with it.
    reach: usize,
    /// The keys filtered: an entry each, but one for all a whole entry holds.
    keys: u64,
}

impl Surf {
    /// Whether `[from, end)` (no end: unbounded) may hold a key: false only when it holds none.
    /// `scratch` holds the least entry's prefix, so a check allocates nothing once it has grown.
    pub fn may_hold(&self, from: &[u8], end: Option<&[u8]>, scratch: &mut Vec<u8>) -> bool {
        if end.is_some_and(|e| e <= from) {
            return false;
        }
        let n = self.suffix;
        let whole = all_set(n);
        let found = self.trie.seek(from, scratch, |len, v| {
            // A stored prefix of `from`: a whole one holds every key starting with it, `from`'s
            // place among them; another holds those at or past `from` whose bits after the prefix
            // are at least `from`'s.
            let p = from.get(..len).unwrap_or(&[]);
            if (len == self.reach && v == whole)
                || (v >= suffix_of(from, len, n) && starts_below(p, v, n, end))
            {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        });
        match found {
            ControlFlow::Break(()) => true,
            ControlFlow::Continue(None) => false,
            // A whole entry's interval starts at its prefix.
            ControlFlow::Continue(Some(v)) if scratch.len() == self.reach && v == whole => {
                starts_below(scratch, 0, 0, end)
            }
            ControlFlow::Continue(Some(s)) => starts_below(scratch, s, n, end),
        }
    }

    /// Its greatest entry's prefix, written to `out`, which the greatest key filtered starts
    /// with (a prefix cut short of the key, or whole and holding it); false with none.
    pub fn greatest(&self, out: &mut Vec<u8>) -> bool {
        self.trie.last(out).is_some()
    }

    /// The suffix bits an entry keeps.
    pub fn suffix_bits(&self) -> u32 {
        self.suffix
    }

    /// The keys filtered.
    pub fn keys(&self) -> u64 {
        self.keys
    }

    /// Its entries: a key each, but one for all the keys a whole entry holds.
    pub fn len(&self) -> usize {
        self.trie.len()
    }

    /// Whether it filters no key.
    pub fn is_empty(&self) -> bool {
        self.trie.is_empty()
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        self.trie.bytes()
    }

    /// The bytes the filter holds in memory ([`Trie::held_bytes`]).
    pub fn held_bytes(&self) -> usize {
        self.trie.held_bytes()
    }

    /// Appends the filter to `out`: its suffix bits, the keys it filters, its reach, then its
    /// trie.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(u8::try_from(self.suffix).unwrap_or(u8::MAX));
        out.extend_from_slice(&self.keys.to_le_bytes());
        out.extend_from_slice(&u64::try_from(self.reach).unwrap_or(u64::MAX).to_le_bytes());
        self.trie.encode(out);
    }

    /// A filter [`Self::encode`] wrote at the start of `bytes`, and the bytes it took.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let corrupt = |why| Error::Corruption {
            what: "a range filter",
            why,
        };
        let suffix = u32::from(*bytes.first().ok_or(corrupt(Malformed::Truncated))?);
        if suffix > MAX_WIDTH {
            return Err(corrupt(Malformed::OutOfRange));
        }
        let word = |at: usize| {
            bytes
                .get(at..)
                .and_then(<[u8]>::first_chunk::<8>)
                .map(|b| u64::from_le_bytes(*b))
                .ok_or(corrupt(Malformed::Truncated))
        };
        let keys = word(1)?;
        // A reach past what this machine addresses is past every key's length: no entry is whole.
        let reach = usize::try_from(word(9)?).unwrap_or(usize::MAX);
        let (trie, used) = Trie::decode(bytes.get(17..).unwrap_or(&[]))?;
        // Each entry holds one key at least.
        if u64::try_from(trie.len()).map_or(true, |n| n > keys) {
            return Err(corrupt(Malformed::CountMismatch));
        }
        Ok((
            Self {
                trie,
                suffix,
                reach,
                keys,
            },
            used.checked_add(17).ok_or(corrupt(Malformed::TooLarge))?,
        ))
    }
}

/// The most bytes a range filter holds ([`Surf::held_bytes`]) over at most `keys` keys of at most
/// `key` bytes, keeping `suffix` bits a key, at most `labels` labels in at most `rises + 1` nodes.
/// What a key shares with the key after it, past the bytes every key shares and at most the
/// depth, starts and ends at none; a key's entry runs one byte past the larger of its two, or ends
/// at the reach or with the key, so past what it shares with the key before it holds at most one
/// label and the rise to what it shares with the key after: `keys` and the rises at most, a key
/// whole entries hold among them, holding none. Of a key's labels, those a later key's go on from
/// lead to a node: at most its rise, so the nodes are at most the rises and the root
/// (research/68 §2).
pub fn most_bytes(keys: u64, labels: u64, rises: u64, key: usize, suffix: u32) -> Option<u64> {
    let key = u64::try_from(key).ok()?;
    super::trie::most_bytes(labels, rises.checked_add(1)?, keys, suffix, key)
}

/// Builds a [`Surf`] from keys in strictly ascending order, one held at a time: a key's cut
/// needs the next key, so each is added to the trie when the next arrives. Reset, it keeps its
/// buffers, so one used again for a run no larger allocates nothing as keys are added.
#[derive(Clone, Debug, Default)]
pub struct SurfBuilder {
    trie: TrieBuilder,
    suffix: u32,
    /// The bytes every key shares, as the builder is told, and the most an entry runs: those and
    /// the depth past them.
    shared: usize,
    reach: usize,
    /// The bytes the key held shares with the key before it, always less than the reach: one
    /// sharing that much is the whole entry's own.
    shared_before: usize,
    /// Whether a key is held, waiting for the next to be cut.
    pending: bool,
    /// The keys whole entries hold past the ones they were cut from.
    absorbed: u64,
}

impl SurfBuilder {
    /// A builder keeping `suffix` bits a key, its entries running at most `depth` bytes past the
    /// `shared` bytes every key will share.
    pub fn new(suffix: u32, depth: usize, shared: usize) -> Result<Self, Error> {
        let mut b = Self::default();
        b.reset(suffix, depth, shared)?;
        Ok(b)
    }

    /// Empties the builder for another run, keeping `suffix` bits a key and entries of at most
    /// `depth` bytes (one at least) past the `shared` bytes every key of the run will share, its
    /// buffers kept.
    pub fn reset(&mut self, suffix: u32, depth: usize, shared: usize) -> Result<(), Error> {
        if depth == 0 {
            return Err(Error::InvalidArgument {
                what: "a range filter's depth none",
            });
        }
        // Past 32 suffix bits, refused by the values' width.
        self.trie.reset(Width::Bits(suffix))?;
        self.suffix = suffix;
        self.shared = shared;
        self.reach = shared.saturating_add(depth);
        self.shared_before = 0;
        self.pending = false;
        self.absorbed = 0;
        Ok(())
    }

    /// States that every key of the run will share its first `shared` bytes, before the first
    /// key comes: the depth counts past them.
    pub fn share(&mut self, shared: usize) -> Result<(), Error> {
        if self.pending || !self.trie.is_empty() {
            return Err(Error::InvalidArgument {
                what: "a range filter's shared bytes stated after a key",
            });
        }
        self.reach = shared.saturating_add(self.reach.saturating_sub(self.shared));
        self.shared = shared;
        Ok(())
    }

    /// Takes the next key, given the key before it (`before`, none for the first) and the
    /// bytes the two share (`shared`): the caller holds both, and has checked the next is the
    /// greater, as the branch builder does as it adds an entry, so the filter keeps no copy of
    /// a key. A key goes into the trie once the next is known, cut one byte past what it shares
    /// with either neighbour: here `before`, and the last at [`Self::finish`]. A key sharing the
    /// reach with the key before starts with the whole entry that key is or is held by, and is
    /// that entry's own: it is not held. Keys sharing fewer bytes than every key was to are
    /// refused: the depth would not bound them.
    pub fn push(&mut self, before: Option<&[u8]>, shared: usize) -> Result<(), Error> {
        match before {
            Some(key) => {
                if shared < self.shared {
                    return Err(Error::InvalidArgument {
                        what: "a range filter's key without the bytes every key was to share",
                    });
                }
                if self.pending {
                    self.cut(key, shared)?;
                } else if self.trie.is_empty() {
                    return Err(Error::InvalidArgument {
                        what: "a range filter given a key before its first",
                    });
                }
            }
            None if !self.pending && self.trie.is_empty() => {}
            None => {
                return Err(Error::InvalidArgument {
                    what: "a range filter given a key without the one before it",
                });
            }
        }
        if shared >= self.reach {
            self.pending = false;
            self.absorbed = self.absorbed.saturating_add(1);
        } else {
            self.shared_before = shared;
            self.pending = true;
        }
        Ok(())
    }

    /// Adds `key`, the held one, to the trie, cut one byte past what it shares with either
    /// neighbour (`shared_after` with the next), or at the reach when that is shorter, whole, its
    /// suffix bits all set. Two neighbours cut so differ at the byte the whole keys first differ
    /// at, `shared_before`, which is so what the trie is told they share: a key starting with a
    /// whole entry is the entry's own, so one that is not parts from it within it.
    fn cut(&mut self, key: &[u8], shared_after: usize) -> Result<(), Error> {
        // Past the reach only when the next key shares the reach with this one, which it then
        // starts with: a key the reach long is whole too, as it holds the next.
        let cut = self.shared_before.max(shared_after).saturating_add(1);
        let (len, value) = if cut <= self.reach {
            let natural = cut.min(key.len());
            (natural, suffix_of(key, natural, self.suffix))
        } else {
            (self.reach, all_set(self.suffix))
        };
        self.trie
            .add_after(key.get(..len).unwrap_or(&[]), self.shared_before, value)
    }

    /// The filter of the keys pushed, `last` the last of them (none when there was none).
    pub fn finish(&mut self, last: Option<&[u8]>) -> Result<Surf, Error> {
        match (last, self.pending) {
            (Some(key), true) => {
                self.cut(key, 0)?;
                self.pending = false;
            }
            // The last key a whole entry's own, or no key at all.
            (Some(_), false) if !self.trie.is_empty() => {}
            (None, false) if self.trie.is_empty() => {}
            _ => {
                return Err(Error::InvalidArgument {
                    what: "a range filter finished without its last key",
                });
            }
        }
        let trie = self.trie.finish()?;
        let keys = u64::try_from(trie.len())
            .ok()
            .and_then(|entries| entries.checked_add(self.absorbed))
            .ok_or(Error::InvalidArgument {
                what: "a range filter of more keys than a count holds",
            })?;
        Ok(Surf {
            trie,
            suffix: self.suffix,
            reach: self.reach,
            keys,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// Keys from a small alphabet with 0x00 and 0xFF in it, lengths 0 to 6.
    fn key(x: &mut u64) -> Vec<u8> {
        let len = (rng(x) % 7) as usize;
        (0..len)
            .map(|_| [0x00, 0x01, b'a', b'b', 0x7f, 0xfe, 0xff][(rng(x) % 7) as usize])
            .collect()
    }

    fn lcp(a: &[u8], b: &[u8]) -> usize {
        a.iter().zip(b).take_while(|(x, y)| x == y).count()
    }

    /// The bytes every key of `set` shares: its first's with its last.
    fn shared_of(set: &BTreeSet<Vec<u8>>) -> usize {
        match (set.first(), set.last()) {
            (Some(a), Some(b)) => lcp(a, b),
            _ => 0,
        }
    }

    fn surf_at(set: &BTreeSet<Vec<u8>>, suffix: u32, depth: usize) -> Surf {
        let mut b = SurfBuilder::new(suffix, depth, shared_of(set)).unwrap();
        let mut before: Option<&Vec<u8>> = None;
        for k in set {
            let shared = before.map_or(0, |p| lcp(p, k));
            b.push(before.map(Vec::as_slice), shared).unwrap();
            before = Some(k);
        }
        b.finish(before.map(Vec::as_slice)).unwrap()
    }

    /// A filter whose entries are never cut short.
    fn surf(set: &BTreeSet<Vec<u8>>, suffix: u32) -> Surf {
        surf_at(set, suffix, usize::MAX)
    }

    /// The filter the rule gives `keys`, which share their first `shared` bytes, cut from the
    /// whole keys: each one byte past its longest prefix shared with a neighbour, or `depth` bytes
    /// past the shared ones, its reach, when that is shorter, whole then; a key after a whole
    /// entry that starts with it takes none.
    fn by_rule(keys: &[&Vec<u8>], suffix: u32, depth: usize, shared: usize) -> Surf {
        let reach = shared.saturating_add(depth);
        let mut entries: Vec<(&[u8], u32)> = Vec::new();
        let mut whole: Option<&[u8]> = None;
        for (i, k) in keys.iter().enumerate() {
            if whole.is_some_and(|p| k.starts_with(p)) {
                continue;
            }
            whole = None;
            let before = i.checked_sub(1).map_or(0, |j| lcp(keys[j], k));
            let after = keys.get(i + 1).map_or(0, |n| lcp(k, n));
            let cut = before.max(after).saturating_add(1);
            if cut > reach {
                // Whole: every suffix bit set.
                let p = &k[..reach];
                entries.push((p, all_set(suffix)));
                whole = Some(p);
            } else {
                let natural = cut.min(k.len());
                entries.push((&k[..natural], suffix_of(k, natural, suffix)));
            }
        }
        let mut t = TrieBuilder::new(Width::Bits(suffix)).unwrap();
        for (p, v) in entries {
            t.add(p, v).unwrap();
        }
        Surf {
            trie: t.finish().unwrap(),
            suffix,
            reach,
            keys: keys.len() as u64,
        }
    }

    /// The filter a stream of keys and shared prefixes builds is byte for byte the one the rule
    /// cuts from the whole keys, at every depth and with none.
    #[test]
    fn a_filter_pushed_with_shared_prefixes_encodes_as_the_rule_cuts_whole_keys() {
        let mut x = 0x5bd1_e995_9e37_79b9u64;
        for case in 0..200u32 {
            let mut set = BTreeSet::new();
            for _ in 0..(rng(&mut x) % 300) {
                set.insert(key(&mut x));
            }
            let keys: Vec<&Vec<u8>> = set.iter().collect();
            for suffix in [0u32, 5, 8, 13] {
                for depth in [1usize, 2, 3, usize::MAX] {
                    let (mut want, mut got) = (Vec::new(), Vec::new());
                    by_rule(&keys, suffix, depth, shared_of(&set)).encode(&mut want);
                    surf_at(&set, suffix, depth).encode(&mut got);
                    assert_eq!(
                        got,
                        want,
                        "case {case} suffix {suffix} depth {depth} keys {}",
                        keys.len()
                    );
                }
            }
        }
    }

    /// Keys in pairs sharing all but a last byte, each pair parting from the one before at its
    /// first bytes (SuRF's worst case): cut `depth` bytes past the bytes every key shares, each
    /// pair is one whole entry, the trie at most `depth` levels and labels a pair deep below them,
    /// and a range holding either key of a pair is let through.
    #[test]
    fn pairs_cut_at_a_depth_take_at_most_its_levels_and_labels() {
        let pairs: BTreeSet<Vec<u8>> = (0u64..4_000)
            .map(|i| {
                let mut k = (i / 2).to_be_bytes().to_vec();
                k.resize(2_131, b'x');
                k.push(if i % 2 == 0 { b'a' } else { b'b' });
                k
            })
            .collect();
        assert_eq!(surf(&pairs, 8).len(), 4_000);
        let mut scratch = Vec::new();
        // Two bytes past the shared ones reach past a pair's number, so no pair falls in
        // another's entry.
        for depth in [2usize, 16, 64] {
            let f = surf_at(&pairs, 8, depth);
            assert_eq!(f.len(), 2_000, "depth {depth}");
            assert!(f.trie.sparse_levels() <= depth, "depth {depth}");
            assert!(f.trie.labels() <= 2_000 * depth, "depth {depth}");
            for k in &pairs {
                let mut after = k.clone();
                after.push(0);
                assert!(f.may_hold(k, Some(&after), &mut scratch), "depth {depth}");
            }
        }
    }

    /// A key not starting with the bytes every key was to share is refused.
    #[test]
    fn a_key_without_the_shared_bytes_is_refused() {
        let mut b = SurfBuilder::new(8, 4, 3).unwrap();
        b.push(None, 0).unwrap();
        assert!(b.push(Some(b"abcd"), 3).is_ok());
        assert!(b.push(Some(b"abce"), 2).is_err());
    }

    #[test]
    fn suffix_bits_and_bytes_agree() {
        assert_eq!(suffix_of(b"ab", 0, 8), u32::from(b'a'));
        assert_eq!(suffix_of(b"ab", 1, 4), u32::from(b'b') >> 4);
        assert_eq!(suffix_of(b"ab", 1, 12), u32::from(b'b') << 4);
        assert_eq!(suffix_of(b"ab", 2, 32), 0);
        assert_eq!(suffix_bytes(u32::from(b'b') << 4, 12), ([b'b', 0, 0, 0], 1));
        assert_eq!(suffix_bytes(0x0_1, 12), ([0, 0x10, 0, 0], 2));
        assert_eq!(suffix_bytes(0, 8), ([0; 4], 0));
    }

    #[test]
    fn a_range_holding_a_key_is_never_ruled_out() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for case in 0..300 {
            let n = (rng(&mut x) % 200) as usize;
            let set: BTreeSet<Vec<u8>> = (0..n).map(|_| key(&mut x)).collect();
            let mut scratch = Vec::new();
            for (suffix, depth) in [0u32, 1, 4, 8, 12, 31]
                .into_iter()
                .flat_map(|s| [1usize, 2, 4, usize::MAX].map(|d| (s, d)))
            {
                let f = surf_at(&set, suffix, depth);
                if depth == usize::MAX {
                    assert_eq!(f.len(), set.len());
                }
                let mut out = Vec::new();
                f.encode(&mut out);
                assert_eq!(Surf::decode(&out).unwrap(), (f.clone(), out.len()));
                for k in &set {
                    let mut after = k.clone();
                    after.push(0);
                    assert!(
                        f.may_hold(k, Some(&after), &mut scratch),
                        "case {case} depth {depth} {k:?}"
                    );
                }
                for _ in 0..300 {
                    let (a, b) = (key(&mut x), key(&mut x));
                    let (from, end) = if a <= b { (a, b) } else { (b, a) };
                    let bounded = !rng(&mut x).is_multiple_of(4);
                    let holds = if bounded {
                        set.range(from.clone()..end.clone()).next().is_some()
                    } else {
                        set.range(from.clone()..).next().is_some()
                    };
                    let end = if bounded { Some(end.as_slice()) } else { None };
                    let may = f.may_hold(&from, end, &mut scratch);
                    assert!(
                        may || !holds,
                        "case {case} suffix {suffix} depth {depth} [{from:?}, {end:?}) holds a key"
                    );
                }
            }
        }
    }

    #[test]
    fn suffix_bits_rule_out_ranges_the_prefixes_cannot() {
        // Keys sharing long prefixes, queried between them: the more suffix bits, the fewer
        // ranges let through that hold nothing, and with the whole key kept, none.
        let keys: BTreeSet<Vec<u8>> = (0u32..2_000)
            .map(|n| format!("user/{:08}/object", n * 7).into_bytes())
            .collect();
        let mut scratch = Vec::new();
        let mut through = Vec::new();
        for suffix in [0u32, 8, 16, 31] {
            let f = surf(&keys, suffix);
            let mut n = 0;
            for m in 0u32..14_000 {
                let from = format!("user/{m:08}/object").into_bytes();
                let mut end = from.clone();
                end.push(0);
                let holds = keys.contains(&from);
                let may = f.may_hold(&from, Some(&end), &mut scratch);
                assert!(may || !holds, "{m}");
                if may && !holds {
                    n += 1;
                }
            }
            through.push(n);
        }
        assert!(
            through.windows(2).all(|w| w[1] <= w[0]),
            "false positives by suffix bits {through:?}"
        );
        assert!(through[0] > through[3], "{through:?}");
    }
}
