//! The memtable's skiplist: RocksDB's `memtable/inlineskiplist.h` (docs/research/24 §4.1), for
//! one writer and any number of concurrent readers, without `unsafe`.
//!
//! A skip list (Pugh, "Skip Lists: A Probabilistic Alternative to Balanced Trees", CACM 33(6),
//! 1990) of byte-string keys. As in RocksDB's `InlineSkipList`, each node is one run of the
//! arena: a header word (height and key length), the node's `height` links, then the key, so a
//! visit touches one place in memory. Links are the words of the arena ([`memory::arena`]),
//! holding a node's [`Addr`] or [`NIL`]; the writer stores a new node's own links and key with
//! relaxed stores and then publishes it by a release store into its predecessors' links, and
//! readers load links with acquire ordering, RocksDB's `SetNext`/`Next` orderings
//! [R memtable/inlineskiplist.h:379-407]. A reader therefore sees each node whole or not at all,
//! and an iterator created before an insert may or may not see it, never a part of it.
//!
//! RocksDB also inserts concurrently from several writers with compare-and-swap
//! (`InsertConcurrently`, [R memtable/inlineskiplist.h:913-920, :1134-1172]). The port does not:
//! a range's batches come from one apply thread (docs/research/24 §4.1 DECISION).
//!
//! **Ownership.** The list's state is in two parts. [`ListShared`] (the head's links, the
//! height, the order) and the arena's [`Store`] are what readers touch; [`ListWriter`] (the
//! insert splice and the height generator) and the arena's [`Allocator`] are the writer's. An
//! owner holds all four ([`InlineSkipList`], or a memtable holding two lists over one arena)
//! and lends them: readers borrow the shared parts (`&`), the one writer borrows its parts
//! mutably, and [`InlineSkipList::split`] lends both at once so a writer and readers run on
//! scoped threads (`std::thread::scope`). The borrow checker is the single-writer rule, and the
//! owner outliving every borrow is what keeps a reader's node addresses valid: blocks are freed
//! only when the owner is dropped. No reference counting is involved.
//!
//! **Orderings.** Each is the C++ one, and each is justified here:
//! - A node's key and its own links are written with relaxed stores before it is published;
//!   publication is a release store of its address into each predecessor's link, level 0
//!   first. A reader loads links with acquire ordering, so having read a node's address it
//!   sees every store made before that release — the node's header, key and links — whole
//!   (release/acquire synchronization, C++ [atomics.order]; Rust's atomics follow it).
//! - The node's own links, stored relaxed before publication, are visible to any reader that
//!   reached the node through an acquire load, by the same edge.
//! - `max_height` is read and written relaxed, as RocksDB does: a reader that sees an old,
//!   lower height starts its search lower and still finds every published node through the
//!   acquire-loaded lower links; one that sees a new, higher height finds the head's link at
//!   that level NIL or a published node, and descends.
//! - The arena's block directory is `OnceLock`s, whose `get` synchronizes with `set`; a node's
//!   block is created before any address in it is published.
//!
//! [`memory::arena`]: crate::memory::arena

use std::cmp::Ordering;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use crate::error::Error;
use crate::memory::arena::{Addr, Allocator, Arena, MIN_BLOCK_SIZE, Store};

/// `kMaxPossibleHeight` [R memtable/inlineskiplist.h:70].
pub const MAX_POSSIBLE_HEIGHT: usize = 32;
/// The default `max_height` [R memtable/inlineskiplist.h:76-78].
pub const DEFAULT_MAX_HEIGHT: usize = 12;
/// The default `branching_factor` [R memtable/inlineskiplist.h:76-78]: a node reaches each
/// further level with probability 1/4, Pugh's recommended p for speed and space (1990, §4).
pub const DEFAULT_BRANCHING_FACTOR: u32 = 4;

/// No node: the end of a level.
pub const NIL: u64 = u64::MAX;
/// The head, before every node; its links live beside the list, not in the arena.
const HEAD: u64 = u64::MAX - 1;
/// Bytes in a word.
const WORD: usize = 8;
/// Bits of the header word below the key length: the height.
const HEIGHT_BITS: u32 = 8;
/// The height's bits of the header word, the low `HEIGHT_BITS`.
const HEIGHT_MASK: u64 = 0xFF;
/// The SplitMix64 increment (Steele, Lea and Flood, OOPSLA 2014), stepping the height
/// generator.
const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
/// SplitMix64's first finalizer multiplier (Steele, Lea and Flood, OOPSLA 2014; the published
/// generator's mix).
const MIX_1: u64 = 0xbf58_476d_1ce4_e5b9;
/// SplitMix64's second finalizer multiplier, as `MIX_1`.
const MIX_2: u64 = 0x94d0_49bb_1331_11eb;

/// A node's key, read from the arena.
#[derive(Clone, Copy)]
pub struct StoredKey<'a> {
    store: &'a Store,
    addr: Addr,
    len: usize,
}

impl std::fmt::Debug for StoredKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredKey")
            .field("addr", &self.addr)
            .field("len", &self.len)
            .finish()
    }
}

impl<'a> StoredKey<'a> {
    /// The key's length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The key's word `index` (bytes `8·index..8·index + 8`, little-endian), for comparators
    /// that decode fixed-width fields. `None` past the node.
    #[inline]
    pub fn word(&self, index: usize) -> Option<u64> {
        Some(
            self.store
                .word_at(self.addr, index)?
                .load(AtomicOrdering::Relaxed),
        )
    }

    /// The little-endian u64 in bytes `[from, from + 8)` of the key, which need not be
    /// word-aligned: the low bytes from one word, the high ones from the next. `None` past the
    /// key.
    #[inline]
    pub fn read_u64_le(&self, from: usize) -> Option<u64> {
        if from.checked_add(WORD)? > self.len {
            return None;
        }
        let index = from / WORD;
        let shift = u32::try_from((from % WORD).checked_mul(8)?).ok()?;
        let low = self.word(index)?;
        if shift == 0 {
            return Some(low);
        }
        let high = self.word(index.checked_add(1)?)?;
        Some((low >> shift) | (high << (u64::BITS.checked_sub(shift)?)))
    }

    /// Compares bytes `[from, from + len)` of the key with `other` as `memcmp` then length.
    #[inline]
    pub fn compare_range(&self, from: usize, len: usize, other: &[u8]) -> Ordering {
        match self.addr.offset_by(from) {
            Ok(at) => self.store.compare_bytes(at, len, other),
            Err(_) => Ordering::Less,
        }
    }

    /// Appends bytes `[from, from + len)` of the key to `out`.
    pub fn read_range(&self, from: usize, len: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        self.store.read_bytes(self.addr.offset_by(from)?, len, out)
    }

    /// The whole key, copied.
    pub fn to_vec(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        self.store.read_bytes(self.addr, self.len, &mut out)?;
        Ok(out)
    }
}

/// The order of a list's keys. `compare(stored, key)` orders a node's key against a key in the
/// same encoding, given as bytes; it is RocksDB's `Comparator::operator()(const char*,
/// DecodedType)`.
pub trait KeyComparator: Sync {
    fn compare(&self, stored: StoredKey<'_>, key: &[u8]) -> Ordering;
}

/// `Splice` [R memtable/inlineskiplist.h:339-350]: for each level below `height`, a node before
/// a key and the node after it. An insert hint, and the finger of a batched lookup.
#[derive(Debug, Clone)]
pub struct Splice {
    height: usize,
    prev: [u64; MAX_POSSIBLE_HEIGHT + 1],
    next: [u64; MAX_POSSIBLE_HEIGHT + 1],
}

impl Default for Splice {
    fn default() -> Self {
        Self::new()
    }
}

impl Splice {
    /// An empty splice, which the first insert or lookup using it fills.
    pub const fn new() -> Self {
        Self {
            height: 0,
            prev: [HEAD; MAX_POSSIBLE_HEIGHT + 1],
            next: [NIL; MAX_POSSIBLE_HEIGHT + 1],
        }
    }

    fn prev(&self, level: usize) -> u64 {
        self.prev.get(level).copied().unwrap_or(HEAD)
    }

    fn next(&self, level: usize) -> u64 {
        self.next.get(level).copied().unwrap_or(NIL)
    }

    fn set(&mut self, level: usize, prev: u64, next: u64) {
        if let (Some(p), Some(n)) = (self.prev.get_mut(level), self.next.get_mut(level)) {
            *p = prev;
            *n = next;
        }
    }
}

/// What readers share, beside the arena's store: the head's links, the height and the order.
#[derive(Debug)]
pub struct ListShared<C> {
    head: [AtomicU64; MAX_POSSIBLE_HEIGHT],
    /// `max_height_`: the tallest node's height. Relaxed; see the module's orderings.
    max_height: AtomicUsize,
    cmp: C,
}

impl<C> ListShared<C> {
    /// An empty list ordered by `cmp`.
    pub fn new(cmp: C) -> Self {
        Self {
            head: std::array::from_fn(|_| AtomicU64::new(NIL)),
            max_height: AtomicUsize::new(1),
            cmp,
        }
    }
}

/// The writer's own state: its shape parameters, the splice of its last insert and its height
/// generator.
#[derive(Debug, Clone)]
pub struct ListWriter {
    max_height: usize,
    /// `kScaledInverseBranching_`: a 32-bit draw below this raises a node's height.
    scaled_inverse_branching: u64,
    /// `seq_splice_`: the splice of the last plain insert.
    seq_splice: Splice,
    rng: u64,
}

/// `2^32 / branching_factor`, the threshold a 32-bit draw falls below with probability
/// `1 / branching_factor`: RocksDB's `kScaledInverseBranching_` [R memtable/inlineskiplist.h:837,
/// 565], over this list's 32-bit draws where RocksDB's are `Random`'s 31-bit ones. `None` for a
/// factor of 0.
fn scaled_inverse(branching_factor: u32) -> Option<u64> {
    1u64.checked_shl(u32::BITS)?
        .checked_div(u64::from(branching_factor))
}

impl Default for ListWriter {
    fn default() -> Self {
        Self {
            max_height: DEFAULT_MAX_HEIGHT,
            // The default factor is a constant above 1, so the division is defined.
            scaled_inverse_branching: scaled_inverse(DEFAULT_BRANCHING_FACTOR).unwrap_or(u64::MAX),
            seq_splice: Splice::new(),
            rng: 0,
        }
    }
}

impl ListWriter {
    /// A writer for a list of at most `max_height` levels, each reached with probability
    /// `1 / branching_factor` [R memtable/inlineskiplist.h:585-605].
    pub fn with_shape(max_height: usize, branching_factor: u32) -> Result<Self, Error> {
        if max_height == 0 || max_height > MAX_POSSIBLE_HEIGHT || branching_factor < 2 {
            return Err(Error::InvalidArgument {
                what: "skiplist height or branching factor out of range",
            });
        }
        let scaled_inverse_branching =
            scaled_inverse(branching_factor).ok_or(Error::InvalidArgument {
                what: "skiplist branching factor out of range",
            })?;
        Ok(Self {
            max_height,
            scaled_inverse_branching,
            seq_splice: Splice::new(),
            rng: 0,
        })
    }

    /// `RandomHeight` [R memtable/inlineskiplist.h:558-574]: 1, plus one per successive draw
    /// below `1/branching`. RocksDB draws from a thread-local generator seeded per thread; a
    /// list here draws from its own SplitMix64 stream, so a list built twice from the same
    /// inserts has the same shape (the simulator's replay needs it).
    fn random_height(&mut self) -> usize {
        let mut height = 1;
        while height < self.max_height && height < MAX_POSSIBLE_HEIGHT {
            self.rng = self.rng.wrapping_add(GOLDEN_GAMMA);
            let mut z = self.rng;
            z = (z ^ (z >> 30)).wrapping_mul(MIX_1);
            z = (z ^ (z >> 27)).wrapping_mul(MIX_2);
            z ^= z >> 31;
            if z >> u32::BITS >= self.scaled_inverse_branching {
                break;
            }
            height = height.saturating_add(1);
        }
        height
    }

    /// `Insert(key)` into `list`, whose nodes `alloc` places in `store`: adds `key`, returning
    /// `false` (and adding nothing) when an equal key is in the list.
    pub fn insert<C: KeyComparator>(
        &mut self,
        list: &ListShared<C>,
        store: &Store,
        alloc: &mut Allocator,
        key: &[u8],
    ) -> Result<bool, Error> {
        let mut splice = std::mem::take(&mut self.seq_splice);
        let result = self.insert_with_splice(list, store, alloc, key, &mut splice, false);
        self.seq_splice = splice;
        result
    }

    /// `InsertWithHint`: as `insert`, starting from `hint`, the splice of this caller's last
    /// insert. Cheap when keys arrive near the last one, sequential keys above all.
    pub fn insert_with_hint<C: KeyComparator>(
        &mut self,
        list: &ListShared<C>,
        store: &Store,
        alloc: &mut Allocator,
        key: &[u8],
        hint: &mut Splice,
    ) -> Result<bool, Error> {
        self.insert_with_splice(list, store, alloc, key, hint, true)
    }

    /// `Insert<UseCAS = false>(key, splice, allow_partial_splice_fix)` [R
    /// memtable/inlineskiplist.h:1027-1225].
    fn insert_with_splice<C: KeyComparator>(
        &mut self,
        list: &ListShared<C>,
        store: &Store,
        alloc: &mut Allocator,
        key: &[u8],
        splice: &mut Splice,
        allow_partial_splice_fix: bool,
    ) -> Result<bool, Error> {
        let height = self.random_height();
        let shared = ListRef { store, list };
        let mut max_height = shared.max_height();
        if height > max_height {
            list.max_height.store(height, AtomicOrdering::Relaxed);
            max_height = height;
        }

        let mut recompute_height = 0;
        if splice.height < max_height {
            // The splice is unused, or the list has grown taller since.
            splice.set(max_height, HEAD, NIL);
            splice.height = max_height;
            recompute_height = max_height;
        } else {
            // Find the lowest level whose bracket still holds the key, recomputing the levels
            // below it.
            while recompute_height < max_height {
                let prev = splice.prev(recompute_height);
                let next = splice.next(recompute_height);
                if shared.next(prev, recompute_height) != next {
                    // Nodes were inserted inside this bracket since; move up.
                    recompute_height = recompute_height.saturating_add(1);
                } else if prev != HEAD && !shared.key_is_after_node(key, prev) {
                    // The key is before the splice.
                    if allow_partial_splice_fix {
                        while splice.prev(recompute_height) == prev && recompute_height < max_height
                        {
                            recompute_height = recompute_height.saturating_add(1);
                        }
                    } else {
                        recompute_height = max_height;
                    }
                } else if shared.key_is_after_node(key, next) {
                    // The key is after the splice.
                    if allow_partial_splice_fix {
                        while splice.next(recompute_height) == next && recompute_height < max_height
                        {
                            recompute_height = recompute_height.saturating_add(1);
                        }
                    } else {
                        recompute_height = max_height;
                    }
                } else {
                    // This level brackets the key.
                    break;
                }
            }
        }
        if recompute_height > 0 {
            shared.recompute_splice_levels(key, splice, recompute_height);
        }

        // Tighten each level the new node joins; RocksDB does this as it links, level by level,
        // and linking a lower level cannot change a higher level's bracket, so doing it first is
        // the same search. Then the duplicate check, before anything is allocated.
        for i in 0..height {
            if i >= recompute_height && shared.next(splice.prev(i), i) != splice.next(i) {
                let (p, n) = shared.find_splice_for_level(key, splice.prev(i), NIL, i);
                splice.set(i, p, n);
            }
        }
        let next0 = splice.next(0);
        let prev0 = splice.prev(0);
        if (next0 != NIL && shared.compare_node(next0, key) != Ordering::Greater)
            || (prev0 != HEAD && shared.compare_node(prev0, key) != Ordering::Less)
        {
            return Ok(false);
        }

        let node = allocate_node(store, alloc, height, key)?;
        for i in 0..height {
            if let Some(link) = shared.link(node, i) {
                link.store(splice.next(i), AtomicOrdering::Relaxed);
            }
        }
        for i in 0..height {
            // Publishing: the release store makes the node's key and links visible to a
            // reader that loads this link.
            if let Some(link) = shared.link(splice.prev(i), i) {
                link.store(node, AtomicOrdering::Release);
            }
            splice.set(i, node, splice.next(i));
        }
        Ok(true)
    }
}

/// Allocates and fills a node of `height` levels holding `key`, unlinked.
fn allocate_node(
    store: &Store,
    alloc: &mut Allocator,
    height: usize,
    key: &[u8],
) -> Result<u64, Error> {
    let prefix = height
        .checked_add(1)
        .and_then(|w| w.checked_mul(WORD))
        .ok_or(Error::InvalidArgument {
            what: "skiplist node too large",
        })?;
    let total = prefix
        .checked_add(key.len())
        .ok_or(Error::InvalidArgument {
            what: "skiplist node too large",
        })?;
    let addr = alloc.allocate_aligned(store, total)?;
    let len = u64::try_from(key.len()).map_err(|_| Error::InvalidArgument {
        what: "skiplist key too large",
    })?;
    if len >> (u64::BITS - HEIGHT_BITS) != 0 {
        return Err(Error::InvalidArgument {
            what: "skiplist key too large",
        });
    }
    let header = (len << HEIGHT_BITS) | u64::try_from(height).unwrap_or(0);
    alloc.write(store, addr, &header.to_le_bytes())?;
    alloc.write(store, addr.offset_by(prefix)?, key)?;
    Ok(addr.to_raw())
}

/// A read view of a list: its shared state and the store holding its nodes, borrowed from
/// their owner. `Copy`, so any number of readers on any threads hold one.
#[derive(Debug)]
pub struct ListRef<'a, C> {
    store: &'a Store,
    list: &'a ListShared<C>,
}

impl<C> Clone for ListRef<'_, C> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<C> Copy for ListRef<'_, C> {}

impl<'a, C: KeyComparator> ListRef<'a, C> {
    /// A view of `list`, whose nodes are in `store`.
    pub fn new(store: &'a Store, list: &'a ListShared<C>) -> Self {
        Self { store, list }
    }

    fn max_height(&self) -> usize {
        self.list
            .max_height
            .load(AtomicOrdering::Relaxed)
            .clamp(1, MAX_POSSIBLE_HEIGHT)
    }

    /// The link word of `node` at `level`.
    #[inline]
    fn link(&self, node: u64, level: usize) -> Option<&'a AtomicU64> {
        if node == HEAD {
            self.list.head.get(level)
        } else {
            self.store
                .word_at(Addr::from_raw(node), level.checked_add(1)?)
        }
    }

    /// `Next(level)`, acquire.
    #[inline]
    fn next(&self, node: u64, level: usize) -> u64 {
        self.link(node, level)
            .map_or(NIL, |l| l.load(AtomicOrdering::Acquire))
    }

    /// The key of a node (not the head or NIL).
    #[inline]
    fn key(&self, node: u64) -> StoredKey<'a> {
        let addr = Addr::from_raw(node);
        let header = self
            .store
            .word(addr)
            .map_or(0, |w| w.load(AtomicOrdering::Relaxed));
        let height = usize::try_from(header & HEIGHT_MASK).unwrap_or(0);
        let len = usize::try_from(header >> HEIGHT_BITS).unwrap_or(0);
        let key_addr = height
            .checked_add(1)
            .and_then(|w| w.checked_mul(WORD))
            .and_then(|b| addr.offset_by(b).ok())
            .unwrap_or(addr);
        StoredKey {
            store: self.store,
            addr: key_addr,
            len,
        }
    }

    #[inline]
    fn compare_node(&self, node: u64, key: &[u8]) -> Ordering {
        self.list.cmp.compare(self.key(node), key)
    }

    /// `KeyIsAfterNode`: `key` sorts after node `n`; NIL is after everything.
    #[inline]
    fn key_is_after_node(&self, key: &[u8], n: u64) -> bool {
        n != NIL && self.compare_node(n, key) == Ordering::Less
    }

    /// `FindSpliceForLevel` [R memtable/inlineskiplist.h:945-972]: from `before` (before the
    /// key) along `level` to the last node before the key and the first not before it, or
    /// `after` if reached first.
    #[inline]
    fn find_splice_for_level(
        &self,
        key: &[u8],
        before: u64,
        after: u64,
        level: usize,
    ) -> (u64, u64) {
        let mut before = before;
        loop {
            let next = self.next(before, level);
            if next == after || !self.key_is_after_node(key, next) {
                return (before, next);
            }
            before = next;
        }
    }

    /// `RecomputeSpliceLevels` [R memtable/inlineskiplist.h:1016-1025].
    fn recompute_splice_levels(&self, key: &[u8], splice: &mut Splice, recompute_level: usize) {
        for i in (0..recompute_level).rev() {
            let upper = i.saturating_add(1);
            let (p, n) = self.find_splice_for_level(key, splice.prev(upper), splice.next(upper), i);
            splice.set(i, p, n);
        }
    }

    /// `FindGreaterOrEqual` [R memtable/inlineskiplist.h:606-660]: the first node at or after
    /// `key`, or NIL.
    fn find_greater_or_equal(&self, key: &[u8]) -> u64 {
        let mut x = HEAD;
        let mut level = self.max_height().saturating_sub(1);
        let mut last_bigger = NIL;
        loop {
            let next = self.next(x, level);
            let cmp = if next == NIL || next == last_bigger {
                Ordering::Greater
            } else {
                self.compare_node(next, key)
            };
            if cmp == Ordering::Equal || (cmp == Ordering::Greater && level == 0) {
                return next;
            } else if cmp == Ordering::Less {
                x = next;
            } else {
                last_bigger = next;
                level = level.saturating_sub(1);
            }
        }
    }

    /// `FindLessThan` [R memtable/inlineskiplist.h:662-700]: the last node before `key`, or
    /// HEAD.
    fn find_less_than(&self, key: &[u8]) -> u64 {
        let mut x = HEAD;
        let mut level = self.max_height().saturating_sub(1);
        let mut last_not_after = NIL;
        loop {
            let next = self.next(x, level);
            if next != last_not_after && self.key_is_after_node(key, next) {
                x = next;
            } else {
                if level == 0 {
                    return x;
                }
                last_not_after = next;
                level = level.saturating_sub(1);
            }
        }
    }

    /// `FindLast` [R memtable/inlineskiplist.h:702-720]: the last node, or HEAD.
    fn find_last(&self) -> u64 {
        let mut x = HEAD;
        let mut level = self.max_height().saturating_sub(1);
        loop {
            let next = self.next(x, level);
            if next == NIL {
                if level == 0 {
                    return x;
                }
                level = level.saturating_sub(1);
            } else {
                x = next;
            }
        }
    }

    /// `Contains`: whether a key equal to `key` is in the list.
    pub fn contains(&self, key: &[u8]) -> bool {
        let x = self.find_greater_or_equal(key);
        x != NIL && self.compare_node(x, key) == Ordering::Equal
    }

    /// An iterator, not yet positioned.
    pub fn iter(&self) -> Iter<'a, C> {
        Iter {
            list: *self,
            node: NIL,
            scratch: Vec::new(),
        }
    }

    /// `FindGreaterOrEqualWithFinger` [R memtable/inlineskiplist.h:1226-1306]: as
    /// `find_greater_or_equal`, starting from the finger of the previous, smaller key.
    fn find_greater_or_equal_with_finger(&self, key: &[u8], finger: &mut Splice) -> u64 {
        let max_height = self.max_height();
        let start_level;
        if finger.height == 0 {
            start_level = max_height.saturating_sub(1);
            finger.set(start_level, HEAD, NIL);
            finger.height = max_height;
        } else {
            // The list may have grown taller since the last search.
            while finger.height < max_height {
                finger.set(finger.height, HEAD, NIL);
                finger.height = finger.height.saturating_add(1);
            }
            // The lowest level whose bracket still holds the key.
            let mut level = 0;
            while level < max_height.saturating_sub(1)
                && self.key_is_after_node(key, finger.next(level))
            {
                level = level.saturating_add(1);
            }
            if self.key_is_after_node(key, finger.next(level)) {
                finger.set(level, HEAD, NIL);
            }
            start_level = level;
        }
        let (p, n) = self.find_splice_for_level(
            key,
            finger.prev(start_level),
            finger.next(start_level),
            start_level,
        );
        finger.set(start_level, p, n);
        for level in (0..start_level).rev() {
            let upper = level.saturating_add(1);
            let (p, n) =
                self.find_splice_for_level(key, finger.prev(upper), finger.next(upper), level);
            finger.set(level, p, n);
        }
        finger.next(0)
    }

    /// `MultiGet` [R memtable/inlineskiplist.h:1308-1352]: for each key of `keys`, which must be
    /// in non-decreasing order, calls `callback(i, entry)` on the first entry at or after it and
    /// then on each following entry while `callback` returns true. Consecutive keys reuse the
    /// search path of the one before, so a sorted batch costs O(log d) per key for keys d
    /// entries apart.
    pub fn multi_get(
        &self,
        keys: &[&[u8]],
        mut callback: impl FnMut(usize, StoredKey<'a>) -> bool,
    ) {
        let mut finger = Splice::new();
        for (i, key) in keys.iter().enumerate() {
            let mut node = self.find_greater_or_equal_with_finger(key, &mut finger);
            while node != NIL && callback(i, self.key(node)) {
                node = self.next(node, 0);
                // Track the walk in `next[0]` only: `prev[0]` must stay before a repeated key,
                // which sorts before the entries this walk passed.
                if let Some(n) = finger.next.get_mut(0) {
                    *n = node;
                }
            }
        }
    }

    /// `TEST_Validate` [R memtable/inlineskiplist.h:1354-1395]: every level in strictly
    /// ascending order, and every node of a level also in the level below.
    pub fn validate(&self) -> Result<(), Error> {
        let bad = |what| Err(Error::InvalidArgument { what });
        let max_height = self.max_height();
        for level in 0..max_height {
            let mut prev = NIL;
            let mut lower = self.next(HEAD, level.saturating_sub(1));
            let mut node = self.next(HEAD, level);
            while node != NIL {
                if prev != NIL {
                    let key = self.key(node).to_vec()?;
                    if self.compare_node(prev, &key) != Ordering::Less {
                        return bad("skiplist level out of order");
                    }
                }
                if level > 0 {
                    while lower != NIL && lower != node {
                        lower = self.next(lower, level.saturating_sub(1));
                    }
                    if lower != node {
                        return bad("skiplist node missing from the level below");
                    }
                }
                prev = node;
                node = self.next(node, level);
            }
        }
        Ok(())
    }
}

/// `InlineSkipList::Iterator` [R memtable/inlineskiplist.h:175-235].
pub struct Iter<'a, C> {
    list: ListRef<'a, C>,
    node: u64,
    /// The current key, copied for `prev`'s search.
    scratch: Vec<u8>,
}

impl<'a, C: KeyComparator> Iter<'a, C> {
    /// `Valid`.
    pub fn valid(&self) -> bool {
        self.node != NIL && self.node != HEAD
    }

    /// `key`: the current entry. Only meaningful when `valid`.
    pub fn key(&self) -> StoredKey<'a> {
        self.list.key(self.node)
    }

    /// `Next`.
    pub fn next(&mut self) {
        if self.valid() {
            self.node = self.list.next(self.node, 0);
        }
    }

    /// `Prev`: there are no back links, so it searches for the last node before this one.
    pub fn prev(&mut self) {
        if !self.valid() {
            return;
        }
        self.scratch.clear();
        if self
            .list
            .key(self.node)
            .read_range(0, self.list.key(self.node).len(), &mut self.scratch)
            .is_err()
        {
            self.node = NIL;
            return;
        }
        let x = self.list.find_less_than(&self.scratch);
        self.node = if x == HEAD { NIL } else { x };
    }

    /// `Seek`: the first entry at or after `target`.
    pub fn seek(&mut self, target: &[u8]) {
        self.node = self.list.find_greater_or_equal(target);
    }

    /// `SeekForPrev`: the last entry at or before `target`.
    pub fn seek_for_prev(&mut self, target: &[u8]) {
        self.seek(target);
        if !self.valid() {
            self.seek_to_last();
        }
        while self.valid() && self.list.compare_node(self.node, target) == Ordering::Greater {
            self.prev();
        }
    }

    /// `SeekToFirst`.
    pub fn seek_to_first(&mut self) {
        self.node = self.list.next(HEAD, 0);
    }

    /// `SeekToLast`.
    pub fn seek_to_last(&mut self) {
        let x = self.list.find_last();
        self.node = if x == HEAD { NIL } else { x };
    }
}

/// `InlineSkipList`: a list that owns its arena, its shared state and its writer. Reads borrow
/// it (`&self`), the one writer borrows it mutably, and [`InlineSkipList::split`] lends a writer
/// and a reader view at once for scoped threads.
#[derive(Debug)]
pub struct InlineSkipList<C> {
    arena: Arena,
    list: ListShared<C>,
    writer: ListWriter,
}

/// The writing handle of a split [`InlineSkipList`]: its writer state and allocator, borrowed
/// mutably, and the shared state it publishes into.
#[derive(Debug)]
pub struct Inserter<'a, C> {
    store: &'a Store,
    alloc: &'a mut Allocator,
    list: &'a ListShared<C>,
    writer: &'a mut ListWriter,
}

impl<C: KeyComparator> Inserter<'_, C> {
    /// `Insert`.
    pub fn insert(&mut self, key: &[u8]) -> Result<bool, Error> {
        self.writer.insert(self.list, self.store, self.alloc, key)
    }

    /// `InsertWithHint`.
    pub fn insert_with_hint(&mut self, key: &[u8], hint: &mut Splice) -> Result<bool, Error> {
        self.writer
            .insert_with_hint(self.list, self.store, self.alloc, key, hint)
    }
}

impl<C: KeyComparator> InlineSkipList<C> {
    /// An empty list over an arena of RocksDB's default block, `Arena::kMinBlockSize`
    /// [R memory/arena.h:48], with the default height and branching.
    pub fn new(cmp: C) -> Result<Self, Error> {
        Self::with_writer(cmp, ListWriter::default())
    }

    /// An empty list of at most `max_height` levels, each reached with probability
    /// `1 / branching_factor`.
    pub fn with_shape(cmp: C, max_height: usize, branching_factor: u32) -> Result<Self, Error> {
        Self::with_writer(cmp, ListWriter::with_shape(max_height, branching_factor)?)
    }

    fn with_writer(cmp: C, writer: ListWriter) -> Result<Self, Error> {
        Ok(Self {
            arena: Arena::new(MIN_BLOCK_SIZE, None)?,
            list: ListShared::new(cmp),
            writer,
        })
    }

    /// The read view.
    pub fn view(&self) -> ListRef<'_, C> {
        ListRef::new(self.arena.store(), &self.list)
    }

    /// The writing handle and a read view, borrowed together.
    pub fn split(&mut self) -> (Inserter<'_, C>, ListRef<'_, C>) {
        let (store, alloc) = self.arena.split();
        (
            Inserter {
                store,
                alloc,
                list: &self.list,
                writer: &mut self.writer,
            },
            ListRef::new(store, &self.list),
        )
    }

    /// `Insert`: adds `key`, or returns `false` when an equal key is present.
    pub fn insert(&mut self, key: &[u8]) -> Result<bool, Error> {
        self.split().0.insert(key)
    }

    /// `InsertWithHint`.
    pub fn insert_with_hint(&mut self, key: &[u8], hint: &mut Splice) -> Result<bool, Error> {
        self.split().0.insert_with_hint(key, hint)
    }

    /// `Contains`.
    pub fn contains(&self, key: &[u8]) -> bool {
        self.view().contains(key)
    }

    /// An iterator, not yet positioned.
    pub fn iter(&self) -> Iter<'_, C> {
        self.view().iter()
    }

    /// The arena, for its accounting.
    pub fn arena(&self) -> &Arena {
        &self.arena
    }
}
