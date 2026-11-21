//! A branch (docs/design/engine-structure.md §4, step E3): an immutable B-tree of store pages
//! packed in one pass from entries in ascending key order, as SplinterDB packs a memtable or a
//! compaction's output (research/34 §1). Its index entries carry their subtree's entry count, so
//! a range's size is known without reading its leaves.
//!
//! A page's payload, little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0 | kind: 1 leaf, 2 index |
//! | 1..3 | entries, `n` |
//! | 3..5 | the prefix every key of the page shares, its length `p` |
//! | 5..5+p | the prefix |
//! | then `2n` | each entry's offset in the payload, in key order |
//! | then `4n` | each entry's head: its suffix's first 4 bytes, big-endian, zero-padded |
//! | then | the entries |
//!
//! A leaf entry is the key's suffix length (2 bytes), the operation (1), the value's length (2),
//! the suffix and the value. An index entry is the suffix length (2), the child's page (8), the
//! entries under the child (8) and the suffix of the child's first key. The prefix is the longest
//! one the page's first and last keys share, which, the keys being sorted, every key between them
//! shares: object-store keys are paths, and their shared prefixes are most of each key.
//!
//! A search runs over the heads, two cache lines for a page's entries, and reads an entry only
//! where its head ties the key's: the entries themselves lie across the page, a cache miss each,
//! as the memtable's nodes keep heads past their prefixes (memtable/btree.rs).

pub mod filter;
pub mod merge;
pub(crate) mod point;

use crate::error::{Error, Malformed};
use crate::fst::surf::{Surf, SurfBuilder};
use crate::fst::trie::{Trie, TrieBuilder, Width};
use crate::store::{MIN_PAGE, Run, Span, Store};
use hyper_block::block::BlockFile;
use std::cmp::Ordering;

/// What an entry does to its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// The key holds the value.
    Put,
    /// The key is deleted: a tombstone shadowing older branches.
    Delete,
}

impl Op {
    pub(crate) fn byte(self) -> u8 {
        match self {
            Self::Put => 1,
            Self::Delete => 2,
        }
    }

    pub(crate) fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Put),
            2 => Some(Self::Delete),
            _ => None,
        }
    }
}

const LEAF: u8 = 1;
const INDEX: u8 = 2;
/// The page payload's fixed head: kind, entries, prefix length.
const HEAD: usize = 5;
/// A leaf entry's fixed bytes: suffix length, operation, value length.
const LEAF_FIXED: usize = 5;
/// An index entry's fixed bytes: suffix length, child page, entries under the child.
const INDEX_FIXED: usize = 18;
/// An entry's offset in the payload.
const OFFSET: usize = 2;
/// An entry's head: its suffix's first 4 bytes.
const KEY_HEAD: usize = 4;
/// The table bytes an entry takes: its offset and its head.
const SLOT: usize = OFFSET + KEY_HEAD;
/// The least bytes a leaf entry takes: its slot and its fixed bytes, its suffix and value empty.
const LEAST_ENTRY: usize = SLOT + LEAF_FIXED;

/// A suffix's head: its first 4 bytes, big-endian, zero-padded. Heads that differ order their
/// suffixes as the suffixes order; equal heads say nothing, and the suffixes are compared.
fn key_head(suffix: &[u8]) -> u32 {
    // A suffix of a head's bytes or more, almost every one: one load.
    if let Some(b) = suffix.first_chunk::<KEY_HEAD>() {
        return u32::from_be_bytes(*b);
    }
    let mut b = [0u8; KEY_HEAD];
    for (d, s) in b.iter_mut().zip(suffix) {
        *d = *s;
    }
    u32::from_be_bytes(b)
}

/// A built branch: its root page, its height (1 for a lone leaf), its entries, and the extents
/// holding its pages, each held once by the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// The root page.
    pub root: u64,
    /// Levels from the root to the leaves, 1 for a branch of one leaf.
    pub height: u8,
    /// Entries in the branch.
    pub count: u64,
    /// The extents holding its pages.
    pub extents: Vec<u64>,
    /// Its keys' filter.
    pub filter: filter::Filter,
    /// Where the filter's pages start in the branch's pages (counted in extent order), how
    /// many there are, and the filter's bytes: what reading it back takes.
    pub filter_start: u64,
    pub filter_pages: u32,
    pub filter_bytes: u64,
    /// The entries in each of its tree pages, in page order, 0 for an index page: a walk in
    /// page order passes over the index pages between leaves by them, reading none. Written
    /// after the filter in its pages.
    pub counts: Vec<u16>,
    /// Its leaves' separators in a succinct trie, each to its leaf's page number: the leaf a key
    /// falls in by one `floor` (docs/design/engine-structure.md §5; research/35). Written after
    /// its pages' entry counts in its pages.
    pub index: Trie,
    pub index_bytes: u64,
    /// Its keys' range filter, deletions with them: a scan passes over a branch it rules out
    /// without reading it (SuRF; docs/design/engine-structure.md §5, E6; research/35 §2).
    /// Written after its leaf index in its pages.
    pub range: Surf,
    pub range_bytes: u64,
    /// Gets that asked its filter since it was built or loaded: its visits, which with its
    /// entries set the false-positive rate its filter's memory is worth (research/39 §2).
    /// Not part of its descriptor.
    pub probes: u64,
    /// The extents its trunk had taken in branches once it took this one, none for a branch
    /// loaded: what a compaction reading it later counts its pages' age in the kernel's page
    /// cache from (`Trunk::take_reuse`, docs/research/65 §8). Not part of its descriptor.
    pub taken: u64,
}

fn corrupt(why: Malformed) -> Error {
    Error::Corruption {
        what: "a branch page",
        why,
    }
}

fn shared(a: &[u8], b: &[u8]) -> usize {
    crate::util::keys::common_prefix(a, b)
}

fn u16_of(n: usize) -> Result<[u8; 2], Error> {
    u16::try_from(n)
        .map(u16::to_le_bytes)
        .map_err(|_| corrupt(Malformed::TooLarge))
}

/// A staged entry's place in its page's buffer: the bytes of its key it shares with the page's
/// first key, and where its staged key and its rest end. Packed in a word as 16, 24 and 24 bits: a
/// key is at most half a page ([`Builder::fits`]) and a page at most 64 KiB, so its shared bytes
/// stay below 2^16, and the buffer holds at most a page's payload and its first key, within 2^24.
#[derive(Clone, Copy)]
struct Ends {
    shared: usize,
    key: usize,
    rest: usize,
}

impl Ends {
    const SHARED_BITS: u32 = 16;
    const END_BITS: u32 = 24;
    const END_MASK: u64 = (1 << Self::END_BITS) - 1;

    /// The word, none when a field passes its bits.
    fn packed(self) -> Option<u64> {
        let fits = |n: usize, bits: u32| u64::try_from(n).ok().filter(|&n| n >> bits == 0);
        let shared = fits(self.shared, Self::SHARED_BITS)?;
        let key = fits(self.key, Self::END_BITS)?;
        let rest = fits(self.rest, Self::END_BITS)?;
        Some(shared << (2 * Self::END_BITS) | key << Self::END_BITS | rest)
    }

    fn of(word: u64) -> Self {
        let field = |n: u64| usize::try_from(n).unwrap_or(0);
        Self {
            shared: field(word >> (2 * Self::END_BITS)),
            key: field((word >> Self::END_BITS) & Self::END_MASK),
            rest: field(word & Self::END_MASK),
        }
    }
}

/// One page being filled: its entries back to back in one buffer, until it is full. Its first
/// entry's key is staged whole, and each later entry's key only past the bytes it shares with
/// the first, which its encoding reads back from the first: the page's keys being in order, a
/// key shares with the first what every pair between them shares, the page's prefix as that key
/// leaves it. A staged page so holds no more than its encoding and its first key, where staging
/// keys whole held hundreds of pages' bytes for keys sharing long prefixes (4.3 MB a 24 KiB page
/// of 2,132-byte keys).
#[derive(Debug, Default)]
pub(crate) struct Page {
    /// Each entry's staged key, then its bytes after its suffix length (operation and value, or
    /// child and count).
    bytes: Vec<u8>,
    /// For each entry, packed in a word: the bytes of its key it shares with the first and does
    /// not stage, and where its staged key and its rest end in `bytes` ([`Ends`]).
    ends: Vec<u64>,
    /// Where the first entry's key ends in `bytes`.
    first_end: usize,
    /// The keys' bytes and the rests' bytes, each summed: what the page's size counts.
    key_bytes: usize,
    rest_bytes: usize,
    prefix: usize,
    /// Leaf entries' total under the page (1 each), or an index page's children's totals.
    total: u64,
}

/// The bytes a page being filled holds for pages of `capacity` payload bytes, its two buffers
/// as [`Page::with_capacity`] reserves them, which filling never grows: its staged bytes, at most
/// the payload, and a word an entry for each entry the payload has room for.
pub fn page_bytes(capacity: usize) -> Option<usize> {
    let entries = capacity.checked_div(SLOT.checked_add(2)?)?;
    capacity.checked_add(entries.checked_mul(std::mem::size_of::<u64>())?)
}

/// The most bytes a branch builder holds while it builds a branch of at most `entries` entries
/// whose keys are at most `key` bytes, at pages of `capacity` payload bytes and extents of
/// `extent` bytes, its range filter keeping `range_bits` bits a key. Its levels number at most
/// the height of a tree of `entries` leaves, each index page holding two entries at least
/// ([`fits`]), and one more a write may open above; each level is a page being filled
/// ([`page_bytes`]), its record in the level lists twice over for their doubling, and its first
/// key's buffer. It keeps the last key added; each key buffer grows to at most twice the longest.
/// It holds a page's payload to encode into and an extent's run to write with, its filter for
/// `entries` keys ([`filter::most_bytes`]), and each page's entry count and each extent's number
/// (at most one a page, a page holding an entry at least: so at most twice as many pages as
/// entries, the index pages above them counted), in lists grown at most twice over and copied
/// once exactly into the branch. Its two tries, the range filter of its keys and its leaves'
/// index, hold a level a key byte ([`trie::builder_bytes`]) and labels the bytes every key shares
/// once (at most a key's) and past them at most their depth a key or a leaf, a leaf an entry at
/// least (`fst::surf`, [`crate::store::RANGE_FILTER_DEPTH`],
/// [`crate::store::LEAF_INDEX_DEPTH`]); the range filter's values its suffix bits. The seal
/// finishes each and encodes it beside its builder, each again at most its builder's bytes. A
/// separator cut short is kept, a key buffer more. Pinned by `tests/builder_memory.rs` at the
/// densest keys and the longest.
pub fn builder_bytes(
    capacity: usize,
    extent: usize,
    key: usize,
    entries: u64,
    range_bits: u32,
) -> Option<u64> {
    let bytes = |n: usize| u64::try_from(n).ok();
    let key_buffer = bytes(key)?.checked_mul(2)?;
    let levels = u64::from(entries.max(2).ilog2()).checked_add(3)?;
    let level = bytes(page_bytes(capacity)?)?
        .checked_add(bytes(std::mem::size_of::<Page>())?.checked_mul(2)?)?
        .checked_add(key_buffer)?;
    // Each page's count (two bytes) and extent number (eight), twice over, and once exactly.
    let pages = entries.checked_mul(2)?;
    let lists = pages.checked_mul(30)?;
    // A leaf holds an entry at least: as many separators as entries at most.
    let tries = tries_bytes(key, entries, entries, range_bits)?.checked_mul(3)?;
    levels
        .checked_mul(level)?
        .checked_add(key_buffer)?
        .checked_add(key_buffer)?
        .checked_add(bytes(capacity)?)?
        .checked_add(bytes(extent)?)?
        .checked_add(bytes(filter::most_bytes(filter::Keys::AtMost(entries))?)?)?
        .checked_add(lists)?
        .checked_add(tries)
}

/// The most bytes a branch's two tries take, built or as their builders hold them: its range
/// filter over `entries` keys of at most `key` bytes keeping `range_bits` suffix bits, and its
/// leaf index over `leaves` separators, its values pages up to 32 bits. Each holds a level a key
/// byte at most and as labels the bytes every key shares once (at most a key's) and past them at
/// most its depth a key or a separator, never past the key
/// ([`crate::store::RANGE_FILTER_DEPTH`], [`crate::store::LEAF_INDEX_DEPTH`];
/// [`trie::builder_bytes`]). Below the data its branch holds for any keys: a key's or a leaf's
/// share is bounded by the depth, not by the key's length (research/67). Pinned by
/// `tests/branch_tries.rs`.
pub fn tries_bytes(key: usize, entries: u64, leaves: u64, range_bits: u32) -> Option<u64> {
    let bytes = |n: usize| u64::try_from(n).ok();
    let labels = |n: u64, depth: usize| -> Option<u64> {
        let past = n.checked_mul(bytes(depth.min(key))?)?;
        Some(
            bytes(key)?
                .checked_add(past)?
                .min(n.checked_mul(bytes(key)?)?),
        )
    };
    crate::fst::trie::builder_bytes(
        key,
        labels(entries, crate::store::RANGE_FILTER_DEPTH)?,
        entries,
        range_bits,
    )?
    .checked_add(crate::fst::trie::builder_bytes(
        key,
        labels(leaves, crate::store::LEAF_INDEX_DEPTH)?,
        leaves,
        u32::BITS,
    )?)
}

/// The most entries a leaf of `capacity` payload bytes holds: past its head, each takes at least
/// [`LEAST_ENTRY`] bytes.
pub fn most_entries(capacity: usize) -> Option<u64> {
    u64::try_from(capacity.checked_sub(HEAD)?.checked_div(LEAST_ENTRY)?).ok()
}

/// A range filter's depth for keys of at most `key` bytes: no entry runs past the key.
fn range_depth(key: usize) -> Option<u64> {
    u64::try_from(crate::store::RANGE_FILTER_DEPTH.min(key)).ok()
}

/// The most a branch's range filter rises ([`crate::fst::surf::most_bytes`]): over `entries`
/// keys of at most `key` bytes in `leaves` leaves of `capacity` payload bytes. Each key steps from
/// what it shares with the key before to what it shares with the key after, at most the depth;
/// with both in its leaf, both are at least the leaf's prefix and at most the key, so the step is
/// at most the key's suffix there, which the leaf stores past the key's [`LEAST_ENTRY`] bytes. The
/// steps start and end at none, so they rise half their sum: at most half the steps' depth a key,
/// and at most half the suffixes the leaves hold and the depth for each leaf's first and last key
/// (research/68 §2).
fn range_rises(key: usize, entries: u64, leaves: u64, capacity: usize) -> Option<u64> {
    let depth = range_depth(key)?;
    let payload = leaves.checked_mul(u64::try_from(capacity.checked_sub(HEAD)?).ok()?)?;
    let suffixes = payload.checked_sub(entries.checked_mul(u64::try_from(LEAST_ENTRY).ok()?)?)?;
    let by_steps = entries.checked_mul(depth)? / 2;
    let by_suffixes = (suffixes / 2).checked_add(leaves.checked_mul(depth)?)?;
    Some(by_steps.min(by_suffixes))
}

/// The most bytes a built branch holds in memory while it lives ([`crate::trunk::Memory`]): of
/// `entries` keys of at most `key` bytes in `leaves` leaves of `capacity` payload bytes, `pages`
/// tree pages and `extents` extents, its range filter keeping `range_bits` bits a key. Its point
/// filter fitted to its keys ([`filter::fitted_most_bytes`]), its range filter of a label a key and
/// its rises ([`range_rises`], [`crate::fst::surf::most_bytes`]), its leaf index of a separator a
/// leaf, each at most its depth and a byte past what every key shares, its values of 32 bits
/// ([`crate::fst::trie::most_bytes`]), its tree pages' entry counts and its extents' numbers taken
/// at their size, and its record ([`held_most`]). Pinned by `tests/resident_memory.rs` at the
/// densest keys, the longest, the shapes that most fill each trie and a filter made for more keys
/// than its branch got.
pub fn resident_bytes(
    capacity: usize,
    key: usize,
    entries: u64,
    leaves: u64,
    pages: u64,
    extents: u64,
    range_bits: u32,
) -> Option<u64> {
    let rises = range_rises(key, entries, leaves, capacity)?;
    let range =
        crate::fst::surf::most_bytes(entries, entries.checked_add(rises)?, rises, key, range_bits)?;
    held_most(key, entries, range, leaves, pages, extents)
}

/// The most bytes of resident metadata a branch holds for each extent of `extent_pages` pages of
/// `capacity` payload bytes it holds, at keys of at most `key` bytes and a range filter keeping
/// `range_bits` bits a key: each part at its most for any count of entries the extent's pages hold
/// as leaves ([`most_entries`]). The range filter's rises grow with the entries by half the depth
/// each and fall by half their [`LEAST_ENTRY`] bytes each from half the pages' payload and the
/// depth a page ([`range_rises`]): they, and the labels, a key and its rises, are most where the
/// two meet. Each part of a branch's bound ([`resident_bytes`]) grows at most in proportion to its
/// entries, leaves, pages and extents, its constants once a branch, so a branch of `n` extents
/// holds at most `n` times this (research/68 §2).
pub fn resident_per_extent(
    capacity: usize,
    extent_pages: u32,
    key: usize,
    range_bits: u32,
) -> Option<u64> {
    let pages = u64::from(extent_pages);
    let entries = most_entries(capacity)?.checked_mul(pages)?;
    let depth = range_depth(key)?;
    let payload = pages.checked_mul(u64::try_from(capacity.checked_sub(HEAD)?).ok()?)?;
    // `n d / 2` meets `(payload - 11 n) / 2 + pages d` at `n = (payload + 2 pages d) / (d + 11)`.
    let reach = payload.checked_add(pages.checked_mul(depth)?.checked_mul(2)?)?;
    let over = depth
        .checked_add(u64::try_from(LEAST_ENTRY).ok()?)?
        .checked_mul(2)?;
    let rises = reach.checked_mul(depth)?.checked_div(over)?;
    let labels = reach
        .checked_mul(depth.checked_add(2)?)?
        .checked_div(over)?;
    let range = crate::fst::surf::most_bytes(entries, labels, rises, key, range_bits)?;
    held_most(key, entries, range, pages, pages, 1)
}

/// A branch's resident metadata at most ([`resident_bytes`]), its range filter at most `range`
/// bytes.
fn held_most(
    key: usize,
    entries: u64,
    range: u64,
    leaves: u64,
    pages: u64,
    extents: u64,
) -> Option<u64> {
    let point = u64::try_from(filter::fitted_most_bytes(entries)?).ok()?;
    let k = u64::try_from(key).ok()?;
    let reach = u64::try_from(crate::store::LEAF_INDEX_DEPTH)
        .ok()?
        .min(k)
        .checked_add(1)?
        .min(k);
    let separators = leaves.checked_mul(reach)?;
    let index = crate::fst::trie::most_bytes(separators, separators, leaves, u32::BITS, k)?;
    point
        .checked_add(range)?
        .checked_add(index)?
        .checked_add(pages.checked_mul(2)?)?
        .checked_add(extents.checked_mul(8)?)?
        .checked_add(u64::try_from(size_of::<Branch>()).ok()?)
}

impl Page {
    /// A page with room reserved for the most a page of `capacity` payload bytes can hold, so
    /// filling it never grows a buffer. Its staged bytes are within the payload: the first key
    /// is the page's prefix, which the payload holds once, and its suffix; each later key's own
    /// bytes are at most its suffix, since it shares with the first at least the prefix the page
    /// ends with; and the rests are the payload's own. Each entry takes at least its offset, head
    /// and suffix length of the payload ([`Page::size_with`]), so its ends' words are within the
    /// payload's room too.
    fn with_capacity(capacity: usize) -> Self {
        let entries = capacity.checked_div(SLOT.saturating_add(2)).unwrap_or(0);
        Self {
            bytes: Vec::with_capacity(capacity),
            ends: Vec::with_capacity(entries),
            first_end: 0,
            key_bytes: 0,
            rest_bytes: 0,
            prefix: 0,
            total: 0,
        }
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    /// The first entry's key.
    fn first(&self) -> &[u8] {
        self.bytes.get(..self.first_end).unwrap_or(&[])
    }

    /// The first entry's bytes after its key.
    fn first_rest(&self) -> &[u8] {
        let end = self
            .ends
            .first()
            .map_or(self.first_end, |&e| Ends::of(e).rest);
        self.bytes.get(self.first_end..end).unwrap_or(&[])
    }

    /// The payload the page's entries take with `prefix` and one more entry of `key` and
    /// `rest` bytes after its suffix.
    fn size_with(&self, prefix: usize, key: usize, rest: usize) -> Option<usize> {
        let n = self.len().checked_add(1)?;
        let keys = self.key_bytes.checked_add(key)?;
        let suffixes = keys.checked_sub(n.checked_mul(prefix)?)?;
        HEAD.checked_add(prefix)?
            .checked_add(n.checked_mul(SLOT.checked_add(2)?)?)?
            .checked_add(suffixes)?
            .checked_add(self.rest_bytes)?
            .checked_add(rest)
    }

    /// Stages an entry whose key leaves the page's prefix `prefix`, which a later entry's key
    /// shares with the first: its ends are counted before a byte is, so a refusal stages nothing.
    fn push(
        &mut self,
        key: &[u8],
        head: &[u8],
        tail: &[u8],
        under: u64,
        prefix: usize,
    ) -> Result<(), Error> {
        let first = self.ends.is_empty();
        let shared = if first { 0 } else { prefix };
        let own = key.get(shared..).ok_or(corrupt(Malformed::Truncated))?;
        let key_end = self.bytes.len().checked_add(own.len());
        let rest = head.len().checked_add(tail.len());
        let rest_end = key_end.zip(rest).and_then(|(k, r)| k.checked_add(r));
        let (Some(ends), Some(rest)) = (
            key_end
                .zip(rest_end)
                .and_then(|(key, rest)| Ends { shared, key, rest }.packed()),
            rest,
        ) else {
            return Err(corrupt(Malformed::TooLarge));
        };
        self.bytes.extend_from_slice(own);
        self.bytes.extend_from_slice(head);
        self.bytes.extend_from_slice(tail);
        self.ends.push(ends);
        // The first entry's key is staged whole at the buffer's start.
        if first {
            self.first_end = own.len();
        }
        self.key_bytes = self.key_bytes.saturating_add(key.len());
        self.rest_bytes = self.rest_bytes.saturating_add(rest);
        self.prefix = prefix;
        self.total = self.total.saturating_add(under);
        Ok(())
    }

    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
        self.first_end = 0;
        self.key_bytes = 0;
        self.rest_bytes = 0;
        self.prefix = 0;
        self.total = 0;
    }

    /// The page's payload written into the front of `out`, its length returned. Its sections'
    /// sizes are counted from the staged sums first and each section is cut from `out` once, so
    /// an entry's writes are copies into slices it owns.
    fn encode_into(&self, kind: u8, out: &mut [u8]) -> Result<usize, Error> {
        let too_large = || corrupt(Malformed::TooLarge);
        let n = self.len();
        let prefix = self
            .first()
            .get(..self.prefix)
            .ok_or(corrupt(Malformed::Truncated))?;
        let suffixes = self
            .key_bytes
            .checked_sub(n.checked_mul(self.prefix).ok_or_else(too_large)?)
            .ok_or(corrupt(Malformed::Truncated))?;
        // The head, the prefix, an offset and a key head an entry; then each entry's suffix
        // length, rest and suffix.
        let bodies = HEAD
            .checked_add(prefix.len())
            .and_then(|b| b.checked_add(n.checked_mul(SLOT)?))
            .ok_or_else(too_large)?;
        let total = n
            .checked_mul(2)
            .and_then(|l| l.checked_add(bodies))
            .and_then(|t| t.checked_add(self.rest_bytes))
            .and_then(|t| t.checked_add(suffixes))
            .ok_or_else(too_large)?;
        let out = out.get_mut(..total).ok_or(Error::InvalidArgument {
            what: "a page payload longer than the page",
        })?;
        let (head, out) = out.split_at_mut_checked(HEAD).ok_or_else(too_large)?;
        let ([n0, n1], [p0, p1]) = (u16_of(n)?, u16_of(self.prefix)?);
        head.copy_from_slice(&[kind, n0, n1, p0, p1]);
        let (pre, out) = out
            .split_at_mut_checked(prefix.len())
            .ok_or_else(too_large)?;
        pre.copy_from_slice(prefix);
        let (table, out) = out
            .split_at_mut_checked(n.checked_mul(OFFSET).ok_or_else(too_large)?)
            .ok_or_else(too_large)?;
        let (heads, mut body) = out
            .split_at_mut_checked(n.checked_mul(KEY_HEAD).ok_or_else(too_large)?)
            .ok_or_else(too_large)?;
        let mut at = bodies;
        let (table, heads) = (
            table.as_chunks_mut::<OFFSET>().0,
            heads.as_chunks_mut::<KEY_HEAD>().0,
        );
        let first = self.first();
        // Each entry's staged key and rest lie after the one before it's.
        let mut start = 0usize;
        for ((slot, head), &word) in table.iter_mut().zip(heads.iter_mut()).zip(&self.ends) {
            let e = Ends::of(word);
            let own = self
                .bytes
                .get(start..e.key)
                .ok_or(corrupt(Malformed::Truncated))?;
            let rest = self
                .bytes
                .get(e.key..e.rest)
                .ok_or(corrupt(Malformed::Truncated))?;
            start = e.rest;
            // The suffix past the page's prefix, in two pieces: the first's bytes from the
            // prefix to what the entry shares with the first, then its own past the prefix.
            // The first entry shares nothing and stages its key whole, so its suffix is its
            // own past the prefix; a later one shares at least the prefix and stages past it.
            let (Some(a), Some(b)) = (
                first.get(self.prefix..e.shared.max(self.prefix)),
                own.get(self.prefix.saturating_sub(e.shared)..),
            ) else {
                return Err(corrupt(Malformed::Truncated));
            };
            let suffix = a.len().checked_add(b.len()).ok_or_else(too_large)?;
            *slot = u16_of(at)?;
            let (len, tail) = std::mem::take(&mut body)
                .split_at_mut_checked(2)
                .ok_or_else(too_large)?;
            len.copy_from_slice(&u16_of(suffix)?);
            let (r, tail) = tail
                .split_at_mut_checked(rest.len())
                .ok_or_else(too_large)?;
            r.copy_from_slice(rest);
            let (k, tail) = tail.split_at_mut_checked(suffix).ok_or_else(too_large)?;
            let (ka, kb) = k.split_at_mut_checked(a.len()).ok_or_else(too_large)?;
            // An entry sharing just the prefix with the first has no first piece to copy.
            if !a.is_empty() {
                ka.copy_from_slice(a);
            }
            kb.copy_from_slice(b);
            // The head read from the suffix as written, whole.
            *head = key_head(k).to_be_bytes();
            body = tail;
            at = at
                .checked_add(2)
                .and_then(|a| a.checked_add(rest.len()))
                .and_then(|a| a.checked_add(suffix))
                .ok_or_else(too_large)?;
        }
        Ok(total)
    }
}

/// An entry for a level's page: its key, the bytes after its suffix length (a head and a tail,
/// back to back), and the leaf entries under it.
#[derive(Clone, Copy)]
struct Entry<'a> {
    key: &'a [u8],
    head: &'a [u8],
    tail: &'a [u8],
    under: u64,
}

/// Packs entries given in strictly ascending key order into a branch's pages in `store`.
#[derive(Debug)]
pub struct Builder {
    /// Level 0 the leaves, then each index level.
    levels: Vec<Page>,
    /// The extent pages are being written into, and the next page in it.
    extent: Option<(u64, u32)>,
    extents: Vec<u64>,
    /// Each level's written page's first key, for its entry in the level above: a buffer a
    /// level, since writing a page may write the level above's in turn.
    firsts: Vec<Vec<u8>>,
    /// Pages for levels the tree grows, kept from earlier builders ([`Store::take_lists`]).
    spare_pages: Vec<Page>,
    /// Each tree page's entries as it is written, 0 for an index page ([`Branch::counts`]).
    counts: Vec<u16>,
    /// Each leaf's separator to its page number, added as the leaf is written, and the index
    /// built of them at the seal, encoded ([`Branch::index`]).
    separators: TrieBuilder,
    index: Vec<u8>,
    index_trie: Option<Trie>,
    /// Each key added, deletions too, and the range filter built of them at the seal, encoded
    /// ([`Branch::range`]).
    keys: SurfBuilder,
    range: Vec<u8>,
    range_filter: Option<Surf>,
    /// The last key added, whole, its first `last_len` bytes: what the next is compared with
    /// and the range filter cuts by, written past the bytes each key shares with the one before
    /// it. The buffer only grows, to the longest key added, so a write is that copy alone.
    last: Vec<u8>,
    last_len: usize,
    /// What the first key of the leaf being filled shares with the key before it, the last
    /// leaf's last: the leaf's separator keeps one byte more.
    leaf_shared: usize,
    /// The bytes every key shares, as the builder is told ([`Self::set_shared`]): separators
    /// run at most [`crate::store::LEAF_INDEX_DEPTH`] bytes past them.
    shared: usize,
    /// The last separator when it was cut short, its prefix: the leaves whose first keys start
    /// with it are in its group, which the index holds as one entry.
    group: Vec<u8>,
    in_group: bool,
    count: u64,
    payload: Vec<u8>,
    capacity: usize,
    /// The branch's filter, each key added as it is.
    filter: filter::Filter,
    /// The pages written in runs of an extent ([`Store::queue_page`]).
    run: Run,
    /// Pages issued so far, in extent order.
    issued: u64,
    /// Key and value bytes added: a filter page's worth in a compaction's budget.
    entry_bytes: u64,
    /// The tree written, once [`Self::seal`] has run: what is left is its filter's pages.
    sealed: Option<Sealed>,
}

/// A sealed builder's fixed root and height, and its filter's pages: the first, the bytes in
/// all, the bytes written, and the pages written.
/// A written page's entry for the level above: its first key, its address and subtree count as
/// the entry's head, and that count.
#[derive(Debug)]
struct Separator {
    first: Vec<u8>,
    head: [u8; 16],
    total: u64,
}

#[derive(Debug, Clone, Copy)]
struct Sealed {
    root: u64,
    height: u8,
    filter_start: u64,
    total: usize,
    at: usize,
    pages: u32,
}

/// Whether a branch page of `capacity` payload bytes holds an entry of a `key`-byte key and a
/// `value`-byte value alone: a leaf of the page's fixed bytes, the entry's slot and fixed bytes, its
/// key twice and its value, and an index page above with room for its key thrice. A builder
/// refuses an entry no empty page of its holds ([`Builder::add`]).
pub fn fits(capacity: usize, key: usize, value: usize) -> bool {
    page_needs(key, value).is_some_and(|needs| needs <= capacity)
}

/// The least page, a multiple of `align` and at least [`MIN_PAGE`], whose branch pages each hold
/// an entry of a `key`-byte key and a `value`-byte value alone ([`fits`]); `None` where no branch
/// page does, its payload past what offsets of 16 bits reach ([`Builder::new`]).
pub fn least_page(align: usize, key: usize, value: usize) -> Option<usize> {
    let page = page_needs(key, value)?
        .checked_add(crate::store::page::HEADER)?
        .max(MIN_PAGE)
        .checked_next_multiple_of(align)?;
    if crate::store::page::capacity(page) <= usize::from(u16::MAX) {
        Some(page)
    } else {
        None
    }
}

/// The payload bytes an empty leaf needs for an entry of these lengths, and an index page above
/// it: the larger of the two.
fn page_needs(key: usize, value: usize) -> Option<usize> {
    let leaf = HEAD
        .checked_add(SLOT)?
        .checked_add(LEAF_FIXED)?
        .checked_add(key.checked_mul(2)?)?
        .checked_add(value)?;
    let index = HEAD
        .checked_add(SLOT.checked_add(INDEX_FIXED)?.checked_mul(2)?)?
        .checked_add(key.checked_mul(3)?)?;
    Some(leaf.max(index))
}

impl Builder {
    /// States that every key this builder will take shares its first `shared` bytes, before the
    /// first is added: the range filter's depth counts past them (`fst::surf`), so keys sharing a
    /// long first part (a bucket, a directory) keep that depth of their own.
    pub fn set_shared(&mut self, shared: usize) -> Result<(), Error> {
        if self.count > 0 {
            return Err(Error::InvalidArgument {
                what: "a branch's shared bytes stated after a key",
            });
        }
        self.shared = shared;
        self.keys.share(shared)
    }
}

impl Builder {
    /// A builder in `store`, for `keys` entries: pages of the store's payload capacity, which
    /// offsets of 16 bits must reach.
    pub fn new<F: BlockFile>(store: &mut Store<F>, keys: filter::Keys) -> Result<Self, Error> {
        let capacity = store.page_capacity();
        if capacity > usize::from(u16::MAX) {
            return Err(Error::InvalidArgument {
                what: "a branch page past 64 KiB",
            });
        }
        // The filter first, sized by the keys the caller names: a count the allocator refuses is
        // refused before the store's lists are taken, so they stay pooled for the next builder.
        let filter = filter::Filter::try_new(keys)?;
        let lists = store.take_lists();
        let mut separators = lists.separators;
        separators.reset(Width::Fit)?;
        let mut keys_filter = lists.keys;
        keys_filter.reset(
            store.range_filter_bits(),
            crate::store::RANGE_FILTER_DEPTH,
            0,
        )?;
        let mut spare_pages = lists.pages;
        let mut levels = lists.levels;
        levels.push(
            spare_pages
                .pop()
                .unwrap_or_else(|| Page::with_capacity(capacity)),
        );
        let mut payload = lists.payload;
        payload.reserve(capacity);
        Ok(Self {
            levels,
            extent: None,
            extents: lists.extents,
            firsts: lists.firsts,
            spare_pages,
            counts: lists.counts,
            separators,
            index: lists.index,
            index_trie: None,
            keys: keys_filter,
            range: lists.range,
            range_filter: None,
            last: lists.last,
            last_len: 0,
            leaf_shared: 0,
            shared: 0,
            group: Vec::new(),
            in_group: false,
            count: 0,
            payload,
            capacity,
            filter,
            run: store.run()?,
            issued: 0,
            entry_bytes: 0,
            sealed: None,
        })
    }

    /// Whether an entry of these lengths fits an empty page of this builder ([`fits`]).
    pub fn fits(&self, key: usize, value: usize) -> bool {
        fits(self.capacity, key, value)
    }

    fn page<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<u64, Error> {
        let (extent, next) = match self.extent {
            Some((e, n)) if store.address(e, n).is_ok() => (e, n),
            _ => {
                let e = store.allocate_extent()?;
                self.extents.push(e);
                (e, 0)
            }
        };
        self.extent = Some((extent, next.saturating_add(1)));
        self.issued = self.issued.saturating_add(1);
        store.address(extent, next)
    }

    /// Adds an entry; keys must strictly ascend.
    pub fn add<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        key: &[u8],
        op: Op,
        value: &[u8],
    ) -> Result<(), Error> {
        self.add_inner(store, key, op, value, None)
    }

    /// A packed memtable's entry, whose filter hash was computed when it was written.
    pub(crate) fn add_hashed<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        key: &[u8],
        op: Op,
        value: &[u8],
        hash32: u32,
    ) -> Result<(), Error> {
        self.add_inner(store, key, op, value, Some(hash32))
    }

    fn add_inner<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        key: &[u8],
        op: Op,
        value: &[u8],
        hash32: Option<u32>,
    ) -> Result<(), Error> {
        if self.sealed.is_some() {
            return Err(Error::InvalidArgument {
                what: "an entry added to a sealed branch",
            });
        }
        let prev = if self.count > 0 {
            Some(
                self.last
                    .get(..self.last_len)
                    .ok_or(corrupt(Malformed::Truncated))?,
            )
        } else {
            None
        };
        // The bytes the key shares with the one before it: where the order check looks, what
        // the range filter cuts keys by, and with the page's prefix, the page's prefix after.
        let common = match prev {
            Some(prev) => {
                let common = shared(prev, key);
                let greater = match (prev.get(common), key.get(common)) {
                    (None, Some(_)) => true,
                    (Some(a), Some(b)) => b > a,
                    _ => false,
                };
                if !greater {
                    return Err(corrupt(Malformed::OutOfOrder));
                }
                common
            }
            None => 0,
        };
        let past = key.get(common..).ok_or(corrupt(Malformed::Truncated))?;
        if !self.fits(key.len(), value.len()) {
            return Err(Error::LimitExceeded {
                what: "an entry's bytes in a branch page",
                limit: u64::try_from(self.capacity).unwrap_or(u64::MAX),
            });
        }
        // The range filter takes the key before this one, its neighbours now known, before the
        // entry lands and may write the leaf that holds it.
        self.keys.push(prev, common)?;
        let [low, high] = u16_of(value.len())?;
        let head = [op.byte(), low, high];
        // Append the entry to its page directly, without staging the value in another buffer:
        // the leaf being filled takes it when it has room, and only a full leaf walks up the
        // levels its written page reaches.
        if !self.leaf_push(key, &head, value, common)? {
            let entry = Entry {
                key,
                head: &head,
                tail: value,
                under: 1,
            };
            self.insert(store, 0, entry, Some(common))?;
        }
        self.count = self.count.saturating_add(1);
        let h32 = hash32.unwrap_or_else(|| filter::hash32(filter::hash(key)));
        // The filter's block and probes use these high bits alone.
        self.filter.insert(u64::from(h32) << u32::BITS);
        self.entry_bytes = self.entry_bytes.saturating_add(
            u64::try_from(key.len().saturating_add(value.len())).unwrap_or(u64::MAX),
        );
        if self.last.len() < key.len() {
            self.last.resize(key.len(), 0);
        }
        self.last
            .get_mut(common..key.len())
            .ok_or(corrupt(Malformed::TooLarge))?
            .copy_from_slice(past);
        self.last_len = key.len();
        Ok(())
    }

    /// Stages a leaf entry, `common` bytes shared with the key before it, in the leaf being
    /// filled when that leaf is empty or has room for it, as [`Self::insert`] does at level 0:
    /// false, staging nothing, when the leaf must be written first.
    fn leaf_push(
        &mut self,
        key: &[u8],
        head: &[u8],
        value: &[u8],
        common: usize,
    ) -> Result<bool, Error> {
        let capacity = self.capacity;
        let Some(page) = self.levels.first_mut() else {
            return Ok(false);
        };
        let prefix = if page.len() == 0 {
            key.len()
        } else {
            page.prefix.min(common)
        };
        let rest = head
            .len()
            .checked_add(value.len())
            .ok_or(corrupt(Malformed::TooLarge))?;
        let size = page
            .size_with(prefix, key.len(), rest)
            .ok_or(corrupt(Malformed::TooLarge))?;
        if size > capacity && page.len() > 0 {
            return Ok(false);
        }
        page.push(key, head, value, 1, prefix)?;
        Ok(true)
    }

    /// Adds an entry to level `level`. A level whose page the entry would overflow writes the
    /// page first, and the written page's entry goes to the level above the same way: one pass up
    /// the levels, each full page written before its own entry lands, so pages are issued
    /// bottom-up as they fill. A written page leaves its level empty, so the pass ends at the
    /// first level with room, at most one above the levels there are.
    fn insert<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        level: usize,
        entry: Entry<'_>,
        common: Option<usize>,
    ) -> Result<(), Error> {
        let mut level = level;
        // The entry for the level in hand: the caller's, then each written page's.
        let mut up: Option<Separator> = None;
        for _ in 0..=self.levels.len() {
            let Entry {
                key,
                head,
                tail,
                under,
            } = match &up {
                None => entry,
                Some(s) => Entry {
                    key: &s.first,
                    head: &s.head,
                    tail: &[],
                    under: s.total,
                },
            };
            let page = self.levels.get(level).ok_or(corrupt(Malformed::TooLarge))?;
            let mut prefix = match (&up, common) {
                _ if page.len() == 0 => key.len(),
                // The caller's entry follows the page's last key, its shared bytes with that key
                // `common`. Keys in order share with the page's first what each pair between
                // them shares at least: the page's prefix and that, the lesser.
                (None, Some(common)) => page.prefix.min(common),
                _ => page.prefix.min(shared(page.first(), key)),
            };
            let rest = head
                .len()
                .checked_add(tail.len())
                .ok_or(corrupt(Malformed::TooLarge))?;
            let size = page
                .size_with(prefix, key.len(), rest)
                .ok_or(corrupt(Malformed::TooLarge))?;
            let written = if size > self.capacity && page.len() > 0 {
                prefix = key.len();
                Some(self.write_page(store, level)?)
            } else {
                None
            };
            self.levels
                .get_mut(level)
                .ok_or(corrupt(Malformed::TooLarge))?
                .push(key, head, tail, under, prefix)?;
            // A leaf begun by the caller's entry: its first key shares `common` with the last
            // leaf's last.
            if let (0, Some(_), Some(common)) = (level, &written, common) {
                self.leaf_shared = common;
            }
            // A separator pushed here came from the level below, whose first-key buffer it is.
            if let Some(s) = up.take() {
                self.give_first(level.saturating_sub(1), s.first);
            }
            match written {
                Some(s) => {
                    up = Some(s);
                    level = level.checked_add(1).ok_or(corrupt(Malformed::TooLarge))?;
                }
                None => return Ok(()),
            }
        }
        Err(corrupt(Malformed::TooLarge))
    }

    /// Writes level `level`'s page and adds its entry to the level above.
    fn write<F: BlockFile>(&mut self, store: &mut Store<F>, level: usize) -> Result<(), Error> {
        let s = self.write_page(store, level)?;
        let above = level.checked_add(1).ok_or(corrupt(Malformed::TooLarge))?;
        let separator = Entry {
            key: &s.first,
            head: &s.head,
            tail: &[],
            under: s.total,
        };
        let inserted = self.insert(store, above, separator, None);
        self.give_first(level, s.first);
        inserted
    }

    /// Returns level `level`'s first-key buffer, kept for its next page.
    fn give_first(&mut self, level: usize, first: Vec<u8>) {
        if let Some(f) = self.firsts.get_mut(level) {
            *f = first;
        }
    }

    /// Writes level `level`'s page, leaving the level empty, and returns its entry for the level
    /// above.
    fn write_page<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        level: usize,
    ) -> Result<Separator, Error> {
        let kind = if level == 0 { LEAF } else { INDEX };
        let page = self.levels.get(level).ok_or(corrupt(Malformed::TooLarge))?;
        if self.firsts.len() <= level {
            self.firsts.resize_with(level.saturating_add(1), Vec::new);
        }
        let mut first = std::mem::take(
            self.firsts
                .get_mut(level)
                .ok_or(corrupt(Malformed::TooLarge))?,
        );
        first.clear();
        first.extend_from_slice(page.first());
        let total = page.total;
        let entries = if level == 0 { page.len() } else { 0 };
        let address = self.page(store)?;
        // The page just issued is the tree's next: its number is the counts so far.
        if u64::try_from(self.counts.len()).ok() != self.issued.checked_sub(1) {
            return Err(corrupt(Malformed::CountMismatch));
        }
        self.counts
            .push(u16::try_from(entries).map_err(|_| corrupt(Malformed::TooLarge))?);
        // Encoded straight into the page's place in the run's buffer.
        let page = self.levels.get(level).ok_or(corrupt(Malformed::TooLarge))?;
        store.queue_page_with(&mut self.run, address, |dst| page.encode_into(kind, dst))?;
        if level == 0 {
            // The leaf's separator: the shortest prefix of its first key above the last leaf's
            // last key, the empty key for the first leaf.
            // The first leaf's separator is its first key: it shares the prefix every key of the
            // branch has, so the index keeps that prefix as bytes ([`Branch::leaf_in`] places a
            // key below it in the first leaf).
            let mut cut = first.len();
            if !self.separators.is_empty() {
                cut = self.leaf_shared.saturating_add(1).min(first.len());
            }
            let page_no = u32::try_from(
                self.issued
                    .checked_sub(1)
                    .ok_or(corrupt(Malformed::CountMismatch))?,
            )
            .map_err(|_| corrupt(Malformed::TooLarge))?;
            // Each entry is its leaf's page, doubled, its low bit set for a separator cut short:
            // that entry stands for a group of leaves, the last its page, which the index pages
            // tell apart (`Branch::leaf_in`). A separator runs at most the leaf index's depth past
            // the bytes every key shares; a leaf whose first key starts with the last group's
            // prefix joins it.
            let entry = |page: u32, whole: bool| {
                page.checked_mul(2)
                    .and_then(|v| v.checked_add(u32::from(whole)))
                    .ok_or(corrupt(Malformed::TooLarge))
            };
            if self.in_group && first.starts_with(&self.group) {
                self.separators.set_last(entry(page_no, true)?)?;
            } else {
                let reach = self.shared.saturating_add(crate::store::LEAF_INDEX_DEPTH);
                let whole = cut > reach;
                let cut = cut.min(reach);
                let separator = first.get(..cut).ok_or(corrupt(Malformed::TooLarge))?;
                self.separators.add(separator, entry(page_no, whole)?)?;
                self.in_group = whole;
                if whole {
                    self.group.clear();
                    self.group.extend_from_slice(separator);
                }
            }
        }
        if let Some(page) = self.levels.get_mut(level) {
            page.clear();
        }
        let above = level.checked_add(1).ok_or(corrupt(Malformed::TooLarge))?;
        if self.levels.len() <= above {
            // A level holds at most a page of entries before it writes; the height is bounded
            // by the entries' count, logarithmically.
            let page = self
                .spare_pages
                .pop()
                .unwrap_or_else(|| Page::with_capacity(self.capacity));
            self.levels.push(page);
        }
        let mut head = [0u8; 16];
        let (child, under) = head.split_at_mut(8);
        child.copy_from_slice(&address.to_le_bytes());
        under.copy_from_slice(&total.to_le_bytes());
        Ok(Separator { first, head, total })
    }

    /// Writes every page left and returns the branch; a builder given no entry is refused.
    pub fn finish<F: BlockFile>(mut self, store: &mut Store<F>) -> Result<Branch, Error> {
        self.seal(store)?;
        self.write_filter(store, u64::MAX)?;
        self.into_branch(store)
    }

    /// Writes the tree's last pages: from here its root and height are fixed, and only its
    /// filter's pages are left to write ([`Self::write_filter`]). A builder given no entry is
    /// refused.
    pub fn seal<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        if self.sealed.is_some() {
            return Ok(());
        }
        if self.count == 0 {
            return Err(Error::InvalidArgument {
                what: "a branch with no entries",
            });
        }
        let t = store.clock();
        let sealed = self.seal_tree(store);
        store.note_seal(t);
        sealed
    }

    /// [`Self::seal`]'s work: the tree's last pages written, the index and filters built.
    fn seal_tree<F: BlockFile>(&mut self, store: &mut Store<F>) -> Result<(), Error> {
        let mut level = 0usize;
        loop {
            let top = level.checked_add(1) == Some(self.levels.len());
            let lone = self.levels.get(level).is_some_and(|p| p.len() == 1) && level > 0;
            if top && lone {
                // The level's single entry names the root: the page below it.
                let page = self.levels.get(level).ok_or(corrupt(Malformed::TooLarge))?;
                let root = page
                    .first_rest()
                    .first_chunk::<8>()
                    .map(|b| u64::from_le_bytes(*b))
                    .ok_or(corrupt(Malformed::Truncated))?;
                self.filter.fit(self.count);
                self.build_index()?;
                // The filter's pages follow the tree's in the branch's own extents.
                self.sealed = Some(Sealed {
                    root,
                    height: u8::try_from(level).map_err(|_| corrupt(Malformed::TooLarge))?,
                    filter_start: self.issued,
                    total: self
                        .filter
                        .bytes()
                        .saturating_add(self.counts.len().saturating_mul(2))
                        .saturating_add(self.index.len())
                        .saturating_add(self.range.len()),
                    at: 0,
                    pages: 0,
                });
                return Ok(());
            }
            if self.levels.get(level).is_some_and(|p| p.len() > 0) {
                self.write(store, level)?;
            }
            level = level.checked_add(1).ok_or(corrupt(Malformed::TooLarge))?;
        }
    }

    /// The filter pages left to write: every one before [`Self::seal`], as the filter stands.
    pub fn filter_pages_left(&self) -> u64 {
        let (total, at) = self
            .sealed
            .map_or((self.unsealed_bytes(), 0), |s| (s.total, s.at));
        u64::try_from(total.saturating_sub(at).div_ceil(self.capacity.max(1))).unwrap_or(u64::MAX)
    }

    /// What follows the tree before the seal, as far as known: the filter's bytes and each
    /// page's count. The leaf index and range filter are sized only when built.
    fn unsealed_bytes(&self) -> usize {
        self.filter
            .bytes()
            .saturating_add(self.counts.len().saturating_mul(2))
    }

    /// The keys a filter page is worth in a compaction's budget: as many bytes as a page, at
    /// the branch's mean entry, at least one.
    pub fn page_keys(&self) -> u64 {
        let capacity = u64::try_from(self.capacity).unwrap_or(u64::MAX);
        capacity
            .saturating_mul(self.count)
            .checked_div(self.entry_bytes.max(1))
            .unwrap_or(1)
            .max(1)
    }

    /// Writes up to `pages` of the sealed filter's pages, each copied from its blocks; true once
    /// all are written, the run then written out.
    pub fn write_filter<F: BlockFile>(
        &mut self,
        store: &mut Store<F>,
        pages: u64,
    ) -> Result<bool, Error> {
        let mut s = self.sealed.ok_or(Error::InvalidArgument {
            what: "a filter written before its branch was sealed",
        })?;
        let mut chunk = std::mem::take(&mut self.payload);
        let mut written = 0u64;
        while written < pages && s.at < s.total {
            chunk.clear();
            let end = s.at.saturating_add(self.capacity).min(s.total);
            self.stream_bytes(s.at..end, &mut chunk);
            let address = self.page(store)?;
            store.queue_page(&mut self.run, address, &chunk)?;
            s.at = end;
            s.pages = s.pages.saturating_add(1);
            written = written.saturating_add(1);
        }
        self.payload = chunk;
        self.sealed = Some(s);
        if s.at < s.total {
            return Ok(false);
        }
        store.write_run(&mut self.run)?;
        Ok(true)
    }

    /// Bytes `range` of what follows the tree in the branch's pages: the filter's bytes, each
    /// tree page's entry count (two bytes each), the leaf index, then the range filter.
    fn stream_bytes(&self, range: std::ops::Range<usize>, out: &mut Vec<u8>) {
        let fb = self.filter.bytes();
        let cb = self.counts.len().saturating_mul(2);
        if range.start < fb {
            self.filter.copy_bytes(range.start..range.end.min(fb), out);
        }
        let from = range.start.max(fb).saturating_sub(fb);
        let to = range.end.saturating_sub(fb).min(cb);
        for at in from..to {
            let count = self.counts.get(at / 2).copied().unwrap_or(0).to_le_bytes();
            out.push(count.get(at % 2).copied().unwrap_or(0));
        }
        let base = fb.saturating_add(cb);
        let ib = self.index.len();
        let from = range.start.max(base).saturating_sub(base);
        let to = range.end.saturating_sub(base).min(ib);
        if from < to {
            out.extend_from_slice(self.index.get(from..to).unwrap_or(&[]));
        }
        let base = base.saturating_add(ib);
        let rb = self.range.len();
        let from = range.start.max(base).saturating_sub(base);
        let to = range.end.saturating_sub(base).min(rb);
        if from < to {
            out.extend_from_slice(self.range.get(from..to).unwrap_or(&[]));
        }
    }

    /// The index of the leaves' separators and the keys' range filter, built and encoded at the
    /// seal.
    fn build_index(&mut self) -> Result<(), Error> {
        let trie = self.separators.finish()?;
        self.index.clear();
        trie.encode(&mut self.index);
        self.index_trie = Some(trie);
        let range = self.keys.finish(if self.count > 0 {
            Some(
                self.last
                    .get(..self.last_len)
                    .ok_or(corrupt(Malformed::Truncated))?,
            )
        } else {
            None
        })?;
        self.range.clear();
        range.encode(&mut self.range);
        self.range_filter = Some(range);
        Ok(())
    }

    pub fn into_branch<F: BlockFile>(self, store: &mut Store<F>) -> Result<Branch, Error> {
        let s = self
            .sealed
            .filter(|s| s.at >= s.total)
            .ok_or(Error::InvalidArgument {
                what: "a branch taken before its filter was written",
            })?;
        store.give_run(self.run);
        let filter_bytes =
            u64::try_from(self.filter.bytes()).map_err(|_| corrupt(Malformed::TooLarge))?;
        let branch = Ok(Branch {
            root: s.root,
            height: s.height,
            count: self.count,
            // Exactly sized: the working lists go back to the store for the next builder.
            extents: self.extents.as_slice().to_vec(),
            filter: self.filter,
            filter_start: s.filter_start,
            filter_pages: s.pages,
            filter_bytes,
            counts: self.counts.as_slice().to_vec(),
            index_bytes: u64::try_from(self.index.len())
                .map_err(|_| corrupt(Malformed::TooLarge))?,
            index: self.index_trie.ok_or(corrupt(Malformed::CountMismatch))?,
            range_bytes: u64::try_from(self.range.len())
                .map_err(|_| corrupt(Malformed::TooLarge))?,
            range: self.range_filter.ok_or(corrupt(Malformed::CountMismatch))?,
            probes: 0,
            taken: 0,
        });
        // Every level's page back among the spares, for the next builder's levels.
        let mut pages = self.spare_pages;
        let mut levels = self.levels;
        pages.append(&mut levels);
        store.give_lists(crate::store::Lists {
            extents: self.extents,
            counts: self.counts,
            separators: self.separators,
            keys: self.keys,
            range: self.range,
            index: self.index,
            levels,
            pages,
            firsts: self.firsts,
            last: self.last,
            payload: self.payload,
        });
        branch
    }
}

/// A page's decoded head: its kind, entries, prefix, and where its offsets start.
struct View<'a> {
    page: &'a [u8],
    kind: u8,
    n: usize,
    prefix: &'a [u8],
    table: usize,
    /// The entries' heads, `KEY_HEAD` bytes each.
    heads: &'a [u8],
}

impl<'a> View<'a> {
    fn new(page: &'a [u8]) -> Result<Self, Error> {
        let [kind, n0, n1, p0, p1] = *page
            .first_chunk::<HEAD>()
            .ok_or(corrupt(Malformed::Truncated))?;
        if kind != LEAF && kind != INDEX {
            return Err(corrupt(Malformed::UnknownTag(kind)));
        }
        let n = usize::from(u16::from_le_bytes([n0, n1]));
        let p = usize::from(u16::from_le_bytes([p0, p1]));
        let table = HEAD.checked_add(p).ok_or(corrupt(Malformed::TooLarge))?;
        let prefix = page.get(HEAD..table).ok_or(corrupt(Malformed::Truncated))?;
        if n == 0 {
            return Err(corrupt(Malformed::CountMismatch));
        }
        let heads_at = table
            .checked_add(n.checked_mul(OFFSET).ok_or(corrupt(Malformed::TooLarge))?)
            .ok_or(corrupt(Malformed::TooLarge))?;
        let heads = page
            .get(
                heads_at
                    ..heads_at
                        .checked_add(
                            n.checked_mul(KEY_HEAD)
                                .ok_or(corrupt(Malformed::TooLarge))?,
                        )
                        .ok_or(corrupt(Malformed::TooLarge))?,
            )
            .ok_or(corrupt(Malformed::Truncated))?;
        Ok(Self {
            page,
            kind,
            n,
            prefix,
            table,
            heads,
        })
    }

    /// Entry `i`'s suffix and the bytes before it (operation and value, or child and count).
    fn entry(&self, i: usize) -> Result<(&'a [u8], &'a [u8]), Error> {
        let (suffix, rest) = self.ranges(i)?;
        Ok((
            self.page.get(suffix).ok_or(corrupt(Malformed::Truncated))?,
            self.page.get(rest).ok_or(corrupt(Malformed::Truncated))?,
        ))
    }

    /// Where entry `i`'s suffix and its rest (operation and value, or child and count) lie in
    /// the page.
    fn ranges(&self, i: usize) -> Result<(std::ops::Range<usize>, std::ops::Range<usize>), Error> {
        entry_ranges(self.page, self.kind, self.table, i)
    }

    /// `key` against the page's key `i`: the prefix first, then the suffix.
    fn compare(&self, key: &[u8], i: usize) -> Result<Ordering, Error> {
        let (suffix, _) = self.entry(i)?;
        let head = key.get(..self.prefix.len().min(key.len())).unwrap_or(key);
        Ok(match head.cmp(self.prefix) {
            Ordering::Equal if key.len() >= self.prefix.len() => {
                key.get(self.prefix.len()..).unwrap_or(&[]).cmp(suffix)
            }
            Ordering::Equal => Ordering::Less,
            other => other,
        })
    }

    /// Entry `i`'s head.
    fn head(&self, i: usize) -> u32 {
        let at = i.saturating_mul(KEY_HEAD);
        self.heads
            .get(at..at.saturating_add(KEY_HEAD))
            .and_then(<[u8]>::first_chunk::<KEY_HEAD>)
            .map_or(0, |b| u32::from_be_bytes(*b))
    }

    /// The last entry whose key is at most `key`, if any. The prefix is compared once: a key
    /// below it is below every entry, one above it past every entry. Then a binary search over
    /// the heads, reading an entry only where its head ties the key's.
    fn floor(&self, key: &[u8]) -> Result<Option<usize>, Error> {
        Ok(self.bound(key, true)?.checked_sub(1))
    }

    /// The first entry at least `key`, or the page's entry count past its last.
    fn lower_bound(&self, key: &[u8]) -> Result<usize, Error> {
        self.bound(key, false)
    }

    /// A boundary by halving, passing entries equal to `key` only for an upper bound.
    fn bound(&self, key: &[u8], upper: bool) -> Result<usize, Error> {
        let p = self.prefix.len();
        let suffix = match key.get(..p.min(key.len())).unwrap_or(key).cmp(self.prefix) {
            Ordering::Less => return Ok(0),
            Ordering::Greater => return Ok(self.n),
            Ordering::Equal => match key.get(p..) {
                Some(s) => s,
                // The key is a proper prefix of the page's prefix: below every entry.
                None => return Ok(0),
            },
        };
        let kh = key_head(suffix);
        let (mut lo, mut hi) = (0usize, self.n);
        while lo < hi {
            let mid = lo.saturating_add(hi.saturating_sub(lo) / 2);
            let order = match kh.cmp(&self.head(mid)) {
                Ordering::Equal => suffix.cmp(self.entry(mid)?.0),
                other => other,
            };
            if order == Ordering::Less || (!upper && order == Ordering::Equal) {
                hi = mid;
            } else {
                lo = mid.saturating_add(1);
            }
        }
        Ok(lo)
    }
}

/// An entry's ranges in a page whose header has been read: a cursor keeps its leaf's table
/// offset while the page stays, so stepping decodes the entry alone.
fn entry_ranges(
    page: &[u8],
    kind: u8,
    table: usize,
    i: usize,
) -> Result<(std::ops::Range<usize>, std::ops::Range<usize>), Error> {
    let slot = table
        .checked_add(i.checked_mul(OFFSET).ok_or(corrupt(Malformed::TooLarge))?)
        .ok_or(corrupt(Malformed::TooLarge))?;
    let at = page
        .get(slot..)
        .and_then(<[u8]>::first_chunk::<2>)
        .map(|b| usize::from(u16::from_le_bytes(*b)))
        .ok_or(corrupt(Malformed::Truncated))?;
    let len = page
        .get(at..)
        .and_then(<[u8]>::first_chunk::<2>)
        .map(|b| usize::from(u16::from_le_bytes(*b)))
        .ok_or(corrupt(Malformed::Truncated))?;
    let body = at.checked_add(2).ok_or(corrupt(Malformed::TooLarge))?;
    let fixed = if kind == LEAF {
        let vlen = page
            .get(body.saturating_add(1)..)
            .and_then(<[u8]>::first_chunk::<2>)
            .map(|b| usize::from(u16::from_le_bytes(*b)))
            .ok_or(corrupt(Malformed::Truncated))?;
        3usize
            .checked_add(vlen)
            .ok_or(corrupt(Malformed::TooLarge))?
    } else {
        16
    };
    let suffix_at = body
        .checked_add(fixed)
        .ok_or(corrupt(Malformed::TooLarge))?;
    let suffix_end = suffix_at
        .checked_add(len)
        .ok_or(corrupt(Malformed::TooLarge))?;
    if suffix_end > page.len() {
        return Err(corrupt(Malformed::Truncated));
    }
    Ok((suffix_at..suffix_end, body..suffix_at))
}

impl Branch {
    /// The descriptor a merge of this branch reads, for a maintenance worker: where its pages
    /// are and its leaf index, without its filters, which only point reads and scans ask and
    /// which are most of a descriptor's memory.
    pub fn for_merge(&self) -> Self {
        Self {
            root: self.root,
            height: self.height,
            count: self.count,
            extents: self.extents.clone(),
            filter: filter::Filter::none(),
            filter_start: self.filter_start,
            filter_pages: self.filter_pages,
            filter_bytes: self.filter_bytes,
            counts: self.counts.clone(),
            index: self.index.clone(),
            index_bytes: self.index_bytes,
            range: Surf::default(),
            range_bytes: self.range_bytes,
            probes: 0,
            taken: self.taken,
        }
    }
}

impl Branch {
    /// The entry for `key`, if the branch has one: its operation, and its value into `value`.
    /// One page read a level.
    pub fn get<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        self.get_hashed(store, key, filter::hash(key), value)
    }

    /// [`Self::get`] with the key's filter hash already taken: a branch the filter rules out is
    /// answered without a page read.
    pub fn get_hashed<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
        hash: u64,
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        if !self.filter.may_contain(hash) {
            return Ok(None);
        }
        // The leaf by the index, from memory: the one page read, parsed where the store holds
        // it, only the value copied out; within a group cut short, the index pages read first.
        let address = match self.leaf_in(key)? {
            Some(page) => self.page_address(store, u64::from(page))?,
            None => self.descend_from_root(store, key)?,
        };
        store.with_page(address, |page| point::read_value(page, key, value))
    }

    /// The attached owner's point lookup, retaining its one resolved leaf across each
    /// completion wait. The borrowed cursor returns its owners if the future is canceled.
    pub(crate) async fn get_hashed_async<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
        hash: u64,
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        let Some(read) = self.prepare_get(store, key, hash)? else {
            return Ok(None);
        };
        let mut point = point::BorrowedPoint::new(read, store);
        point.read(key, value).await
    }

    /// Starts one candidate branch's lookup. A rejected filter has no device work or cursor;
    /// the caller retains its candidate/probe position rather than restarting the lookup.
    pub(crate) fn prepare_get<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
        hash: u64,
    ) -> Result<Option<point::PointRead>, Error> {
        if !self.filter.may_contain(hash) {
            return Ok(None);
        }
        self.prepare_leaf(store, key).map(Some)
    }

    /// The read of the leaf `key` falls in, its filter not asked: [`Self::prepare_get`] once
    /// the filter passes it.
    pub(crate) fn prepare_leaf<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
    ) -> Result<point::PointRead, Error> {
        Ok(match self.leaf_in(key)? {
            Some(page) => point::PointRead::new(self.page_address(store, u64::from(page))?, 0),
            // Down from the root: an index page a level above the leaf.
            None => point::PointRead::new(self.root, usize::from(self.height).saturating_sub(1)),
        })
    }
}

impl Branch {
    /// The branch's descriptor, as a trunk page stores it: root, height, count, the filter's
    /// place and size, the leaf index's and range filter's sizes, then the extents.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        out.extend_from_slice(&self.root.to_le_bytes());
        out.push(self.height);
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.filter_start.to_le_bytes());
        out.extend_from_slice(&self.filter_pages.to_le_bytes());
        out.extend_from_slice(&self.filter_bytes.to_le_bytes());
        out.extend_from_slice(&self.index_bytes.to_le_bytes());
        out.extend_from_slice(&self.range_bytes.to_le_bytes());
        let n = u32::try_from(self.extents.len()).map_err(|_| corrupt(Malformed::TooLarge))?;
        out.extend_from_slice(&n.to_le_bytes());
        for e in &self.extents {
            out.extend_from_slice(&e.to_le_bytes());
        }
        Ok(())
    }

    /// A descriptor from the front of `bytes`, its filter read back from `store`; with the
    /// bytes it took.
    pub fn decode<F: BlockFile>(
        store: &mut Store<F>,
        bytes: &[u8],
    ) -> Result<(Self, usize), Error> {
        let mut at = 0usize;
        let mut take = |n: usize| -> Result<&[u8], Error> {
            let end = at.checked_add(n).ok_or(corrupt(Malformed::TooLarge))?;
            let s = bytes.get(at..end).ok_or(corrupt(Malformed::Truncated))?;
            at = end;
            Ok(s)
        };
        let u64_of = |s: &[u8]| {
            s.first_chunk::<8>()
                .map(|b| u64::from_le_bytes(*b))
                .ok_or(corrupt(Malformed::Truncated))
        };
        let u32_of = |s: &[u8]| {
            s.first_chunk::<4>()
                .map(|b| u32::from_le_bytes(*b))
                .ok_or(corrupt(Malformed::Truncated))
        };
        let root = u64_of(take(8)?)?;
        let height = *take(1)?.first().ok_or(corrupt(Malformed::Truncated))?;
        let count = u64_of(take(8)?)?;
        let filter_start = u64_of(take(8)?)?;
        let filter_pages = u32_of(take(4)?)?;
        let filter_bytes = u64_of(take(8)?)?;
        let index_bytes = u64_of(take(8)?)?;
        let range_bytes = u64_of(take(8)?)?;
        let n = usize::try_from(u32_of(take(4)?)?).map_err(|_| corrupt(Malformed::TooLarge))?;
        let mut extents = Vec::with_capacity(n.min(bytes.len() / 8));
        for _ in 0..n {
            extents.push(u64_of(take(8)?)?);
        }
        // The filter's pages: its place in the branch's pages, counted in extent order, up to
        // the range filter's end.
        let per = u64::from(store.extent_pages());
        let held = filter_bytes
            .checked_add(
                filter_start
                    .checked_mul(2)
                    .ok_or(corrupt(Malformed::TooLarge))?,
            )
            .and_then(|n| n.checked_add(index_bytes))
            .and_then(|n| n.checked_add(range_bytes))
            .ok_or(corrupt(Malformed::TooLarge))?;
        let capacity = u64::try_from(store.page_capacity())
            .unwrap_or(u64::MAX)
            .max(1);
        let pages_held = held.div_ceil(capacity).min(u64::from(filter_pages));
        let mut filter = Vec::with_capacity(usize::try_from(held).unwrap_or(0));
        for i in 0..pages_held {
            let index = filter_start
                .checked_add(i)
                .ok_or(corrupt(Malformed::TooLarge))?;
            let extent = *extents
                .get(
                    usize::try_from(index.checked_div(per).ok_or(corrupt(Malformed::TooLarge))?)
                        .map_err(|_| corrupt(Malformed::TooLarge))?,
                )
                .ok_or(corrupt(Malformed::OutOfRange))?;
            let page = u32::try_from(index.checked_rem(per).ok_or(corrupt(Malformed::TooLarge))?)
                .map_err(|_| corrupt(Malformed::TooLarge))?;
            let address = store.address(extent, page)?;
            store.read_page(address, &mut filter)?;
        }
        // The filter's bytes, then two bytes for each tree page's entry count.
        let fb = usize::try_from(filter_bytes).map_err(|_| corrupt(Malformed::TooLarge))?;
        let tree = usize::try_from(filter_start).map_err(|_| corrupt(Malformed::TooLarge))?;
        let ib = usize::try_from(index_bytes).map_err(|_| corrupt(Malformed::TooLarge))?;
        let rb = usize::try_from(range_bytes).map_err(|_| corrupt(Malformed::TooLarge))?;
        let cb = tree.checked_mul(2).ok_or(corrupt(Malformed::TooLarge))?;
        // The pages hold exactly the stream: nothing follows the range filter.
        let want = usize::try_from(held).map_err(|_| corrupt(Malformed::TooLarge))?;
        if filter.len() != want {
            return Err(corrupt(Malformed::CountMismatch));
        }
        let (filter_part, rest) = filter.split_at(fb);
        let (count_part, rest) = rest.split_at(cb);
        let (index_part, range_part) = rest.split_at(ib);
        let (index, used) = Trie::decode(index_part)?;
        if used != ib {
            return Err(corrupt(Malformed::CountMismatch));
        }
        let (range, used) = Surf::decode(range_part)?;
        if used != rb || range.keys() != count {
            return Err(corrupt(Malformed::CountMismatch));
        }
        let counts: Vec<u16> = count_part
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let filter =
            filter::Filter::from_bytes(filter_part).ok_or(corrupt(Malformed::CountMismatch))?;
        Ok((
            Self {
                root,
                height,
                count,
                extents,
                filter,
                filter_start,
                filter_pages,
                filter_bytes,
                counts,
                index,
                index_bytes,
                range,
                range_bytes,
                probes: 0,
                taken: 0,
            },
            at,
        ))
    }
}

/// A cursor over a branch's entries that walks its leaves in page order. Placed at a page number
/// and an entry, it needs no path from the root: a branch's tree pages precede its filter's in its
/// extents, and each page names its kind, so the next leaf is the next leaf page.
#[derive(Debug)]
pub struct RunCursor {
    /// The leaf's page number in the branch's pages, and the entry in it.
    page_no: u64,
    /// The store address of the page held, none before the first: a page number names a page
    /// only within its branch, so a cursor placed in another branch reads its page.
    loaded: Option<u64>,
    index: usize,
    n: usize,
    /// The prefix's end and entry table's start, read once when the leaf lands.
    table: usize,
    page: Vec<u8>,
    span: Span,
    /// The payload in the span when it was read there; a cache hit uses `page` instead.
    span_page: Option<std::ops::Range<usize>>,
    /// The cache slot an async scan's leaf is read in place in, and the page's address: looked
    /// at again before each read, since the cache may give the slot to another page meanwhile.
    cached: Option<(usize, u64)>,
    key: Vec<u8>,
    /// Whether `key` begins with the held leaf's prefix, written when its first entry was
    /// decoded: each entry after writes only its suffix past it.
    prefixed: bool,
    /// Where the entry's value lies in the page: read in place, not copied out, since the page
    /// stays until the cursor moves to another. A leaf read in the cache is out of reach of
    /// [`Self::value`], so its entry's value is copied, `copied` saying so.
    value: std::ops::Range<usize>,
    value_copy: Vec<u8>,
    copied: bool,
    op: Op,
    valid: bool,
}

/// Where a descent through a branch's index pages goes from a page: the leaf it is, or the
/// child whose keys a key falls among, by its address.
pub(crate) enum Descent {
    Leaf,
    Child(u64),
}

/// The step of a descent from `page` for `key`: an index page's child whose first key is the
/// greatest at most `key`, its first for a key below them all.
pub(crate) fn descend(page: &[u8], key: &[u8]) -> Result<Descent, Error> {
    let view = View::new(page)?;
    if view.kind == LEAF {
        return Ok(Descent::Leaf);
    }
    let i = view.floor(key)?.unwrap_or(0);
    let (_, rest) = view.entry(i)?;
    let child = rest
        .first_chunk::<8>()
        .map(|b| u64::from_le_bytes(*b))
        .ok_or(corrupt(Malformed::Truncated))?;
    Ok(Descent::Child(child))
}

impl Branch {
    /// The page number of the leaf `key` falls in, from the leaf index in memory: the greatest
    /// separator at most `key`, the first leaf for a key below every separator. None when `key`
    /// starts with a separator cut short, which holds a group of leaves: the index pages tell
    /// which ([`Self::leaf_for`]).
    pub fn leaf_in(&self, key: &[u8]) -> Result<Option<u32>, Error> {
        Ok(match self.index.floor_with(key) {
            Some((v, prefix)) if v & 1 == 1 && prefix => None,
            Some((v, _)) => Some(v >> 1),
            // The first leaf is the first page the builder wrote.
            None => Some(0),
        })
    }

    /// The page number of the leaf `key` falls in, reading the index pages down from the root
    /// when the leaf index holds its group of leaves cut short.
    pub fn leaf_for<F: BlockFile>(&self, store: &mut Store<F>, key: &[u8]) -> Result<u32, Error> {
        match self.leaf_in(key)? {
            Some(page) => Ok(page),
            None => {
                let leaf = self.descend_from_root(store, key)?;
                self.page_of(store, leaf)
            }
        }
    }

    /// The address of the leaf `key` falls in, read down from the root through the index pages:
    /// one page a level.
    fn descend_from_root<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        key: &[u8],
    ) -> Result<u64, Error> {
        let mut address = self.root;
        for _ in 0..self.height {
            match store.with_page(address, |page| descend(page, key))? {
                Descent::Leaf => return Ok(address),
                Descent::Child(child) => address = child,
            }
        }
        Err(corrupt(Malformed::CountMismatch))
    }

    /// The page number of the page at `address`: its extent's place among the branch's extents,
    /// and its place in the extent.
    pub(crate) fn page_of<F: BlockFile>(
        &self,
        store: &Store<F>,
        address: u64,
    ) -> Result<u32, Error> {
        let per = u64::from(store.extent_pages());
        let extent = store.extent_of(address);
        let within = address
            .checked_rem(per)
            .ok_or(corrupt(Malformed::OutOfRange))?;
        let i = self
            .extents
            .iter()
            .position(|&e| e == extent)
            .ok_or(corrupt(Malformed::OutOfRange))?;
        u64::try_from(i)
            .ok()
            .and_then(|i| i.checked_mul(per))
            .and_then(|p| p.checked_add(within))
            .and_then(|p| u32::try_from(p).ok())
            .ok_or(corrupt(Malformed::TooLarge))
    }

    /// The last leaf `key` can fall in, from memory alone: a group's last for a key within it.
    fn leaf_most(&self, key: &[u8]) -> u32 {
        self.index.floor(key).map_or(0, |v| v >> 1)
    }

    /// The pages a merge of `[from, end)` writes from this branch, at most ([`Self::merge_size`]).
    pub fn pages_between(&self, from: &[u8], end: Option<&[u8]>) -> usize {
        self.merge_size(from, end).0
    }

    /// What a merge of `[from, end)` takes from this branch at most: the pages it writes, and
    /// the entries of the pages the range falls in, which hold every entry the branch has in it.
    /// The pages: the tree's pages from the leaf `from` falls in to the one `end` does (the last
    /// without an end), index pages between them included, since keys kept in order pack into no
    /// more leaves than held them (a page's entries fit again once any are dropped, so filling
    /// each page in turn is optimal); a part-filled index page at each level above the leaves,
    /// which an output however small has of its own; and the trailer's pages (filter, entry
    /// counts, leaf index, range filter) in the share of the keys or of the pages the range holds,
    /// whichever is larger, since its filters grow with keys and its index with leaves, rounded
    /// up, and one more for where the trailer's sections fall.
    pub fn merge_size(&self, from: &[u8], end: Option<&[u8]>) -> (usize, u64) {
        let total = self.counts.len();
        let page = |p: Result<u32, Error>| p.ok().and_then(|p| usize::try_from(p).ok());
        // Within a group the leaf is one of its leaves or the one before: from memory alone, the
        // first leaf is the least it can be, and the group's last the most.
        let first = page(Ok(self.leaf_in(from).ok().flatten().unwrap_or(0))).unwrap_or(0);
        let last = end
            .and_then(|e| page(Ok(self.leaf_most(e))))
            .unwrap_or(total.saturating_sub(1))
            .min(total.saturating_sub(1));
        let tree = last.saturating_sub(first).saturating_add(1).min(total);
        let keys = self
            .counts
            .get(first..=last)
            .map_or(0u64, |c| c.iter().map(|&n| u64::from(n)).sum());
        let trailer = u64::from(self.filter_pages);
        let share = |part: u64, whole: u64| {
            trailer
                .saturating_mul(part)
                .saturating_add(whole.saturating_sub(1))
                .checked_div(whole)
                .unwrap_or(trailer)
        };
        let trailer = share(keys, self.count).max(share(
            u64::try_from(tree).unwrap_or(u64::MAX),
            u64::try_from(total).unwrap_or(u64::MAX),
        ));
        let pages = tree
            .saturating_add(usize::from(self.height).saturating_sub(1))
            .saturating_add(usize::try_from(trailer).unwrap_or(usize::MAX))
            .saturating_add(1);
        (pages, keys)
    }

    /// The address of page `page_no`, counted through the branch's extents in order.
    pub fn page_address<F: BlockFile>(&self, store: &Store<F>, page_no: u64) -> Result<u64, Error> {
        let per = u64::from(store.extent_pages());
        let extent = usize::try_from(
            page_no
                .checked_div(per)
                .ok_or(corrupt(Malformed::TooLarge))?,
        )
        .ok()
        .and_then(|i| self.extents.get(i))
        .ok_or(corrupt(Malformed::OutOfRange))?;
        let within = u32::try_from(page_no.checked_rem(per).unwrap_or(0))
            .map_err(|_| corrupt(Malformed::TooLarge))?;
        store.address(*extent, within)
    }

    /// The entries in tree page `page_no`, 0 for an index page.
    fn count_of(&self, page_no: u64) -> Result<usize, Error> {
        usize::try_from(page_no)
            .ok()
            .and_then(|p| self.counts.get(p))
            .map(|&c| usize::from(c))
            .ok_or(corrupt(Malformed::OutOfRange))
    }

    /// A cursor at the first entry at or past `from`, for a scan, which may stop after a page:
    /// its leaf by the index from memory, then the leaves in page order, read a page and then
    /// more while the scan runs on.
    pub fn seek<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        from: &[u8],
    ) -> Result<RunCursor, Error> {
        let c = RunCursor::new(store)?;
        self.place_at(store, c, from)
    }

    /// [`Self::seek`] for a compaction, which reads the branch to its end: an extent a read.
    pub fn seek_sequential<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        from: &[u8],
    ) -> Result<RunCursor, Error> {
        let c = RunCursor::new_sequential(store)?;
        self.place_at(store, c, from)
    }

    /// Places `c` at the first entry at or past `from`.
    fn place_at<F: BlockFile>(
        &self,
        store: &mut Store<F>,
        mut c: RunCursor,
        from: &[u8],
    ) -> Result<RunCursor, Error> {
        let placed = (|| {
            let page_no = self.leaf_for(store, from)?;
            c.land_page(self, store, u64::from(page_no))?;
            let (i, n) = c.page_lower_bound(store, from)?;
            if i >= n {
                c.next_leaf(self, store)
            } else {
                c.index = i;
                c.load_in(store)
            }
        })();
        match placed {
            Ok(()) => Ok(c),
            Err(e) => {
                c.give_back(store);
                Err(e)
            }
        }
    }
}

/// The most bytes a cursor holds, all from its store's pools, at pages of `page_size` bytes in
/// extents of `extent_pages`, with the device's issuer taking `batches` reads at once (one with
/// no issuer): its span's extent buffer and its list of reads in flight
/// ([`crate::store::span_reads_bytes`]); a page the span may hold prepared; and its three page
/// buffers, its leaf, its key and a value copy, which an entry fitting a page never grows
/// ([`RunCursor::new`]). Pinned by `tests/cursor_memory.rs`.
pub fn cursor_bytes(page_size: usize, extent_pages: u32, batches: usize) -> Option<u64> {
    let bytes = |n: usize| u64::try_from(n).ok();
    let page = bytes(page_size)?;
    page.checked_mul(u64::from(extent_pages))?
        .checked_add(bytes(crate::store::span_reads_bytes(batches)?)?)?
        .checked_add(page.checked_mul(4)?)
}

/// One bounded move while positioning a private compaction cursor.
pub(crate) enum Initial {
    Waiting,
    More,
    Done,
}

impl RunCursor {
    /// A cursor at no entry yet, its page and span from `store`'s pools, for a scan: [`Self::place`]
    /// puts it on one.
    pub fn new<F: BlockFile>(store: &mut Store<F>) -> Result<Self, Error> {
        let span = store.span()?;
        Ok(Self::with_span(store, span))
    }

    /// [`Self::new`] for a compaction: its leaves read an extent a call.
    pub fn new_sequential<F: BlockFile>(store: &mut Store<F>) -> Result<Self, Error> {
        let span = store.span_sequential()?;
        Ok(Self::with_span(store, span))
    }

    fn with_span<F: BlockFile>(store: &mut Store<F>, span: Span) -> Self {
        Self {
            page_no: 0,
            loaded: None,
            index: 0,
            n: 0,
            table: 0,
            page: store.take_page(),
            span,
            span_page: None,
            cached: None,
            // An entry fits a page, so buffers of a page from the pool never grow.
            key: store.take_page(),
            prefixed: false,
            value: 0..0,
            value_copy: store.take_page(),
            copied: false,
            op: Op::Put,
            valid: false,
        }
    }

    /// Gives the cursor's page, key and value buffers and span back to `store`'s pools.
    pub fn give_back<F: BlockFile>(self, store: &mut Store<F>) {
        store.give_page(self.page);
        store.give_page(self.key);
        store.give_page(self.value_copy);
        store.give_span(self.span);
    }

    /// Whether the cursor is at an entry.
    pub fn valid(&self) -> bool {
        self.valid
    }

    /// The entry's key, operation and value.
    pub fn key(&self) -> &[u8] {
        &self.key
    }

    pub fn op(&self) -> Op {
        self.op
    }

    pub fn value(&self) -> &[u8] {
        if self.copied {
            return &self.value_copy;
        }
        self.page_bytes()
            .and_then(|page| page.get(self.value.clone()))
            .unwrap_or(&[])
    }

    /// In the leaf held, wherever it lies: the index of its first entry at least `key` (its
    /// entries when none is), and its entries.
    pub fn page_lower_bound<F: BlockFile>(
        &self,
        store: &Store<F>,
        key: &[u8],
    ) -> Result<(usize, usize), Error> {
        let view = View::new(self.page_in(store)?)?;
        Ok((view.lower_bound(key)?, view.n))
    }

    /// Moves to the next entry of `branch`, the one the cursor walks.
    pub fn next<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
    ) -> Result<(), Error> {
        if !self.valid {
            return Ok(());
        }
        self.index = self.index.saturating_add(1);
        if self.index < self.n {
            if let Some((slot, address)) = self.cached
                && store.cache_page(slot, address).is_none()
            {
                // The cache gave the leaf's slot away: read the leaf again.
                self.loaded = None;
                self.prefixed = false;
                return self.land(branch, store, self.page_no, self.index);
            }
            return self.load_in(store);
        }
        self.next_leaf(branch, store)
    }

    /// A compaction cursor with its first logical leaf selected, without reading it.
    pub(crate) fn prepare<F: BlockFile>(
        branch: &Branch,
        store: &mut Store<F>,
        from: &[u8],
    ) -> Result<Self, Error> {
        let page = branch.leaf_for(store, from)?;
        let mut cursor = Self::new_sequential(store)?;
        cursor.page_no = u64::from(page);
        Ok(cursor)
    }

    /// Positions one leaf of a private opening cursor. A gap after its final key can require
    /// another leaf, which is checked for readiness on the next move too.
    pub(crate) fn begin<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        from: &[u8],
        yield_io: bool,
    ) -> Result<Initial, Error> {
        let address = branch.page_address(store, self.page_no)?;
        if yield_io && !store.ready(&mut self.span, address)? {
            return Ok(Initial::Waiting);
        }
        self.land(branch, store, self.page_no, 0)?;
        let (index, entries) = self.page_lower_bound(store, from)?;
        if index < entries {
            self.index = index;
            self.load_in(store)?;
            return Ok(Initial::Done);
        }
        self.valid = false;
        match self.next_leaf_page(branch)? {
            Some(page) => {
                self.page_no = page;
                Ok(Initial::More)
            }
            None => Ok(Initial::Done),
        }
    }

    /// Positions an attached scan cursor through demand completion, keeping the selected
    /// leaf in this future across a wait. Its owner returns the cursor on cancellation.
    pub(crate) async fn seek_async<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        from: &[u8],
    ) -> Result<(), Error> {
        let page = match branch.leaf_in(from)? {
            Some(page) => u64::from(page),
            None => self.descend_async(branch, store, from).await?,
        };
        self.land_async(branch, store, page, 0).await?;
        let (index, entries) = {
            let view = View::new(self.page_in(store)?)?;
            (view.lower_bound(from)?, view.n)
        };
        if index >= entries {
            self.next_leaf_async(branch, store).await
        } else {
            self.index = index;
            self.load_in(store)
        }
    }

    pub(crate) async fn next_async<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
    ) -> Result<(), Error> {
        if !self.valid {
            return Ok(());
        }
        self.index = self.index.saturating_add(1);
        if self.index < self.n {
            if let Some((slot, address)) = self.cached
                && store.cache_page(slot, address).is_none()
            {
                // The cache gave the leaf's slot away: read the leaf again.
                self.loaded = None;
                self.prefixed = false;
                return self
                    .land_async(branch, store, self.page_no, self.index)
                    .await;
            }
            return self.load_in(store);
        }
        self.next_leaf_async(branch, store).await
    }

    async fn land_async<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        page: u64,
        index: usize,
    ) -> Result<(), Error> {
        let address = branch.page_address(store, page)?;
        let held = self.loaded == Some(address)
            && self
                .cached
                .is_none_or(|(slot, a)| store.cache_page(slot, a).is_some());
        if !held {
            self.loaded = None;
            self.prefixed = false;
            self.valid = false;
            self.span_page = None;
            self.cached = None;
            self.page.clear();
            // A leaf the cache holds is read where it lies; another is read into the cursor.
            match store.cache_lookup(address) {
                Some(slot) => self.cached = Some((slot, address)),
                None => {
                    store
                        .read_page_async(&mut self.span, address, &mut self.page)
                        .await?
                }
            }
            let (n, table) = {
                let view = View::new(self.page_in(store)?)?;
                if view.kind != LEAF {
                    return Err(corrupt(Malformed::CountMismatch));
                }
                (view.n, view.table)
            };
            self.n = n;
            self.table = table;
            self.loaded = Some(address);
        }
        self.page_no = page;
        self.index = index;
        self.load_in(store)
    }

    async fn next_leaf_async<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
    ) -> Result<(), Error> {
        match self.next_leaf_page(branch)? {
            Some(page) => self.land_async(branch, store, page, 0).await,
            None => {
                self.valid = false;
                Ok(())
            }
        }
    }

    /// Reads leaf `page` unless it is the one held, and loads entry `index`.
    fn land<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        page: u64,
        index: usize,
    ) -> Result<(), Error> {
        self.land_page(branch, store, page)?;
        self.index = index;
        self.load_in(store)
    }

    /// Reads leaf `page` unless it is the one held, loading no entry: a seek finds its entry in
    /// the leaf's table first ([`Branch::place_at`]), so an entry loaded on landing was decoded
    /// and copied for nothing.
    fn land_page<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        page: u64,
    ) -> Result<(), Error> {
        let address = branch.page_address(store, page)?;
        let held = self.loaded == Some(address)
            && self
                .cached
                .is_none_or(|(slot, a)| store.cache_page(slot, a).is_some());
        if !held {
            self.loaded = None;
            self.prefixed = false;
            self.valid = false;
            self.span_page = None;
            self.cached = None;
            self.page.clear();
            // A scan's leaf the cache holds is read where it lies, as the paced path reads it,
            // not copied into the cursor; another is read through the span.
            match store.cache_slot_ahead(&self.span, address) {
                Some(slot) => self.cached = Some((slot, address)),
                None => {
                    self.span_page =
                        store.read_page_ahead_ref(&mut self.span, address, &mut self.page)?;
                }
            }
            let view = View::new(self.page_in(store)?)?;
            if view.kind != LEAF {
                return Err(corrupt(Malformed::CountMismatch));
            }
            let (n, table) = (view.n, view.table);
            self.n = n;
            self.table = table;
            self.loaded = Some(address);
            // A compaction's span reads the next extents ahead while this one is merged.
            let per = u64::from(store.extent_pages());
            let extent = page.checked_div(per).unwrap_or(0);
            for k in 1..=u64::try_from(self.span.ahead_extents()).unwrap_or(1) {
                let next = extent
                    .checked_add(k)
                    .and_then(|e| e.checked_mul(per))
                    .filter(|&p| p < branch.filter_start);
                let Some(next) = next else {
                    break;
                };
                let ahead = branch.page_address(store, next)?;
                store.prefetch(&mut self.span, ahead)?;
            }
        }
        self.page_no = page;
        Ok(())
    }

    /// Whether moving to the next entry of `branch` reads no page that has not landed
    /// ([`Store::ready`]): the next entry is in the leaf held, there is none, or the next leaf
    /// is held or read ahead.
    pub fn next_ready<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
    ) -> Result<bool, Error> {
        if !self.valid || self.index.saturating_add(1) < self.n {
            return Ok(true);
        }
        let Some(page) = self.next_leaf_page(branch)? else {
            return Ok(true);
        };
        let address = branch.page_address(store, page)?;
        store.ready(&mut self.span, address)
    }

    /// The page number of the next leaf after the one held, none before the filter's pages.
    fn next_leaf_page(&self, branch: &Branch) -> Result<Option<u64>, Error> {
        let mut page = self.page_no;
        // At most the tree's pages.
        for _ in 0..=branch.counts.len() {
            page = page.saturating_add(1);
            if page >= branch.filter_start {
                return Ok(None);
            }
            if branch.count_of(page)? > 0 {
                return Ok(Some(page));
            }
        }
        Err(corrupt(Malformed::CountMismatch))
    }

    /// The next leaf page after this one: the next page whose entry count is not 0, so index
    /// pages are passed over unread; none before the filter's pages leaves the cursor invalid.
    fn next_leaf<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
    ) -> Result<(), Error> {
        match self.next_leaf_page(branch)? {
            Some(page) => self.land(branch, store, page, 0),
            None => {
                self.valid = false;
                Ok(())
            }
        }
    }

    fn page_data(&self) -> Result<&[u8], Error> {
        self.page_bytes().ok_or(corrupt(Malformed::Truncated))
    }

    fn page_bytes(&self) -> Option<&[u8]> {
        match &self.span_page {
            Some(range) => self.span.payload(range),
            None => Some(&self.page),
        }
    }

    /// The page number of the leaf `key` falls in, read down from the root through the index
    /// pages, a page a level, into the cursor's own page: it holds no leaf after.
    async fn descend_async<F: BlockFile>(
        &mut self,
        branch: &Branch,
        store: &mut Store<F>,
        key: &[u8],
    ) -> Result<u64, Error> {
        self.loaded = None;
        self.prefixed = false;
        self.valid = false;
        self.span_page = None;
        self.cached = None;
        let mut address = branch.root;
        for _ in 0..branch.height {
            let next = match store.cache_lookup(address) {
                Some(slot) => descend(
                    store
                        .cache_page(slot, address)
                        .ok_or(corrupt(Malformed::Truncated))?,
                    key,
                )?,
                None => {
                    self.page.clear();
                    store
                        .read_page_async(&mut self.span, address, &mut self.page)
                        .await?;
                    descend(&self.page, key)?
                }
            };
            match next {
                Descent::Leaf => return branch.page_of(store, address).map(u64::from),
                Descent::Child(child) => address = child,
            }
        }
        Err(corrupt(Malformed::CountMismatch))
    }

    /// The leaf held: in its cache slot, the span, or the cursor's own page.
    fn page_in<'p, F: BlockFile>(&'p self, store: &'p Store<F>) -> Result<&'p [u8], Error> {
        match self.cached {
            Some((slot, address)) => store
                .cache_page(slot, address)
                .ok_or(corrupt(Malformed::Truncated)),
            None => self.page_data(),
        }
    }

    /// Loads entry `index` of the leaf held, wherever it lies.
    fn load_in<F: BlockFile>(&mut self, store: &Store<F>) -> Result<(), Error> {
        let Some((slot, address)) = self.cached else {
            return self.load();
        };
        let page = store
            .cache_page(slot, address)
            .ok_or(corrupt(Malformed::Truncated))?;
        let (op, value) = Self::decode(
            page,
            self.table,
            self.index,
            &mut self.key,
            &mut self.prefixed,
        )?;
        self.value_copy.clear();
        self.value_copy
            .extend_from_slice(page.get(value).ok_or(corrupt(Malformed::Truncated))?);
        self.copied = true;
        self.op = op;
        self.valid = true;
        Ok(())
    }

    fn load(&mut self) -> Result<(), Error> {
        let page = match &self.span_page {
            Some(range) => self
                .span
                .payload(range)
                .ok_or(corrupt(Malformed::Truncated))?,
            None => self.page.as_slice(),
        };
        let (op, value) = Self::decode(
            page,
            self.table,
            self.index,
            &mut self.key,
            &mut self.prefixed,
        )?;
        self.op = op;
        self.value = value;
        self.copied = false;
        self.valid = true;
        Ok(())
    }

    /// Entry `index` of leaf `page` (its entry table at `table`): its operation and where its
    /// value lies, its key written into `key`. The leaf's prefix, the same for each of its
    /// entries, is written once, `prefixed` then set; each entry after replaces only the suffix.
    fn decode(
        page: &[u8],
        table: usize,
        index: usize,
        key: &mut Vec<u8>,
        prefixed: &mut bool,
    ) -> Result<(Op, std::ops::Range<usize>), Error> {
        let (suffix, rest) = entry_ranges(page, LEAF, table, index)?;
        let tag = page.get(rest.start).copied().unwrap_or(0);
        let op = Op::from_byte(tag).ok_or(corrupt(Malformed::UnknownTag(tag)))?;
        // The leaf's prefix ends where its table starts: read once, by its first entry decoded.
        if *prefixed {
            key.truncate(table.saturating_sub(HEAD));
        } else {
            let prefix = page.get(HEAD..table).ok_or(corrupt(Malformed::Truncated))?;
            key.clear();
            key.extend_from_slice(prefix);
            *prefixed = true;
        }
        key.extend_from_slice(page.get(suffix).ok_or(corrupt(Malformed::Truncated))?);
        // The value follows the operation and its 2-byte length.
        let from = rest
            .start
            .checked_add(3)
            .ok_or(corrupt(Malformed::TooLarge))?;
        if from > rest.end {
            return Err(corrupt(Malformed::Truncated));
        }
        Ok((op, from..rest.end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `i`th of keys of `len` bytes in order: `common` bytes all share, then `i` big-endian,
    /// then filler.
    fn key(len: usize, common: usize, i: u64) -> Vec<u8> {
        let mut k = vec![b'p'; common.min(len)];
        let count = i.to_be_bytes();
        let mut at = 0usize;
        while k.len() < len {
            k.push(count.get(at).copied().unwrap_or(b'z'));
            at += 1;
        }
        k
    }

    /// A page filled to its payload, at the densest keys and the longest, those sharing long
    /// prefixes and those sharing none, as a leaf and as an index page, stages no more bytes than
    /// its payload and never grows the buffers it was made with, which [`page_bytes`] states.
    #[test]
    fn a_staged_page_holds_no_more_than_its_payload() {
        for (len, common, value, index) in [
            (8usize, 0usize, 0usize, false),
            (16, 6, 100, false),
            (2_132, 2_124, 0, false),
            (2_132, 0, 0, false),
            (2_132, 2_124, 18_699, false),
            (8, 0, 0, true),
            (2_132, 2_124, 0, true),
        ] {
            let capacity = crate::store::page::capacity(least_page(4096, len, value).unwrap());
            let mut page = Page::with_capacity(capacity);
            let reserved = (page.bytes.capacity(), page.ends.capacity());
            let value = vec![b'v'; value];
            let head: Vec<u8> = if index { vec![0; 16] } else { vec![1, 0, 0] };
            let tail: &[u8] = if index { &[] } else { &value };
            let mut prev: Option<Vec<u8>> = None;
            let mut staged = 0usize;
            for i in 0u64.. {
                let k = key(len, common, i);
                let prefix = match &prev {
                    None => k.len(),
                    Some(p) => page.prefix.min(shared(p, &k)),
                };
                let size = page
                    .size_with(prefix, k.len(), head.len() + tail.len())
                    .unwrap();
                if size > capacity {
                    break;
                }
                page.push(&k, &head, tail, 1, prefix).unwrap();
                assert!(
                    page.bytes.len() <= capacity,
                    "{len}-byte keys sharing {common}: {} staged past {capacity}",
                    page.bytes.len()
                );
                assert_eq!((page.bytes.capacity(), page.ends.capacity()), reserved);
                staged += 1;
                prev = Some(k);
            }
            assert!(staged >= 1, "{len}-byte keys: none staged");
            assert_eq!(
                page_bytes(capacity),
                Some(reserved.0 + reserved.1 * std::mem::size_of::<u64>())
            );
        }
    }
}
