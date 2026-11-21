//! A shard's memtable (docs/design/engine-structure.md §8, step E3), keeping order only when
//! order is asked for.
//!
//! Every entry is appended to one byte arena (operation, key length, value length, key, value),
//! as the B-tree memtable kept them, and a hash index maps each key's hash to its newest entry:
//! an insert is an append and an index write, and a get one index lookup. Nothing is sorted as
//! entries arrive (RocksDB's vector memtable defers its sort the same way; FloDB, Balmau et al.,
//! EuroSys '17, puts a hash table in front of its ordered memtable for the same reason).
//!
//! Order is kept in sorted runs. The entries appended since the last sort began form the tail.
//! The memtable's owner starts a sort of the tail as the memtable fills ([`HashMem::start_sort`],
//! paid a bounded slice at a time through [`HashMem::debt`] and [`HashMem::pay`]), so a full
//! memtable is nearly in order when it is packed; [`HashMem::seal`] sorts the tail at once when a
//! scan needs it. Runs are merged within a level, as the logarithmic method of Bentley and Saxe
//! ("Decomposable searching problems I: static-to-dynamic transformation", J. Algorithms 1(4),
//! 1980) merges them, de-amortized as Overmars and van Leeuwen spread its rebuilds, so a walk
//! merges logarithmically many (research/40).
//!
//! Runs hold each entry's offset beside eight bytes of its key (AlphaSort's key-prefix sort:
//! Nyberg, Barclay, Cvetanovic, Gray and Lomet, "AlphaSort: A RISC Machine Sort", SIGMOD 1994):
//! two entries whose prefixes differ are ordered without reading the arena, so a sort, merge,
//! seek or walk reads runs in sequence and an entry's arena bytes only to hand it out. The eight
//! bytes start past the prefix every key in the memtable shares with its first key, so keys under
//! one common prefix (a range's keys) still differ in them. A tail is sorted by its prefixes with
//! a least-significant-digit radix sort, a byte a pass, skipping bytes with one value.
//!
//! A key written again leaves its older entry in the arena and in whatever run holds it; the
//! arena is only appended to, so a larger offset is a newer entry, and merges and walks keep the
//! newest of equal keys.
//!
//! A memtable a worker will pack keeps a copy of its arena as it is written (its mirror,
//! [`HashMem::mirror_into`]), which the worker sorts itself by the same radix sort of prefixes
//! ([`Sorter`]): the shard then orders a memtable only for the scans that read it (research/40
//! §4), as RocksDB's vector memtable sorts only when first iterated (`memtable/vectorrep.cc`).

use crate::branch::Op;
use crate::branch::filter::hash;
use crate::error::{Error, Malformed};
use crate::memtable::index::KeyIndex;
use crate::util::coding::{get_varint32_ptr, put_varint32s};
use std::cmp::Ordering;

/// The most bytes an entry's head takes in the arena: the operation (1), then the key's and the
/// value's lengths as varints of at most 3 and 5 bytes (a key is below 64 KiB, a value below
/// 4 GiB).
const ENTRY_HEAD_MAX: usize = 9;

fn corrupt() -> Error {
    Error::Corruption {
        what: "a memtable",
        why: Malformed::OutOfRange,
    }
}

/// An entry as order work moves it: eight bytes of its key from its run's skip, beside its
/// offset in the arena and the high bits of its filter hash, for packing.
#[derive(Clone, Copy, Debug, Default)]
struct Keyed {
    prefix: u64,
    entry: u32,
    hash32: u32,
}

/// Eight bytes of `key` from `skip`, big-endian, zero past its end. Of two keys that share their
/// first `skip` bytes, the one with the smaller prefix is the smaller key; equal prefixes say
/// nothing (a key ending inside them pads with zeros).
fn prefix(key: &[u8], skip: usize) -> u64 {
    let mut bytes = [0u8; 8];
    let rest = key.get(skip..).unwrap_or(&[]);
    let n = rest.len().min(bytes.len());
    if let (Some(dst), Some(src)) = (bytes.get_mut(..n), rest.get(..n)) {
        dst.copy_from_slice(src);
    }
    u64::from_be_bytes(bytes)
}

/// A sorted run: entries in key order with no key twice, every key sharing the memtable's first
/// key's first `skip` bytes, their prefixes taken from there.
#[derive(Debug, Default)]
struct Run {
    skip: usize,
    keys: Vec<Keyed>,
}

/// A merge of two sorted runs of one level ([`level`]) into one, a slice at a time: where each
/// stands, and the output.
#[derive(Debug, Default)]
struct Merge {
    a: Run,
    b: Run,
    i: usize,
    j: usize,
    out: Run,
    level: u32,
}

/// The level of a run of `len` entries: runs of one level are within twice each other's size.
fn level(len: usize) -> u32 {
    usize::BITS.saturating_sub(len.leading_zeros())
}

/// The entries a sort of `n` reads at most: a count, eight scatter passes, the tie scan and the
/// dedupe, each reading every entry ([`Stage`]).
fn sort_reads(n: usize) -> usize {
    n.saturating_mul(11)
}

/// The runs a memtable holds at most, a merge's two inputs counted as its one output: the
/// counter keeps two runs of a level at most and a level's merge of two more ([`HashMem::seal`]),
/// over a level for each bit of a length.
const MAX_RUNS: usize = 4 * usize::BITS as usize;

/// Where a sort stands ([`Sort`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Stage {
    /// Taking again the prefixes taken before the shared prefix shrank.
    #[default]
    Rekey,
    /// Counting the values of each byte of the prefixes.
    Count,
    /// Scattering by one byte of the prefixes, least significant first.
    Scatter,
    /// Ordering by key the entries whose prefixes tie.
    Ties,
    /// Keeping the newest of each key.
    Dedupe,
}

/// A least-significant-digit radix sort of the tail by its prefixes, a byte a pass, a slice at a
/// time: stable, so entries of one prefix stay in arrival order, and a byte with one value across
/// the tail costs no pass. The entries whose prefixes tie are then ordered by key ([`Group`]) and
/// the newest of each key kept in place below `kept`. `at` is where the stage stands.
#[derive(Debug)]
struct Sort {
    src: Vec<Keyed>,
    dst: Vec<Keyed>,
    skip: usize,
    stage: Stage,
    at: usize,
    /// Entries `[0, stale)` were keyed before the shared prefix shrank.
    stale: usize,
    /// How many prefixes hold each value at each byte, most significant byte first.
    counts: Box<[[u32; 256]; 8]>,
    /// The bytes to scatter by, least significant first, `npass` of them, the pass at `pass`, and
    /// where each value of its byte goes next.
    passes: [usize; 8],
    npass: usize,
    pass: usize,
    next: [u32; 256],
    /// The start of the prefix the tie scan is at, and the tie being ordered.
    lo: usize,
    group: Option<Group>,
    kept: usize,
    /// Moves a put's entry pays the sort while the memtable fills ([`HashMem::order_put`]):
    /// its debt at the start over its own entries, so it ends as the next chunk is due.
    rate: usize,
}

/// A bottom-up merge sort by key of entries `[lo, hi)` whose prefixes tie, through the same
/// entries of `dst`: runs of `width` merged pairwise, the pair at `at`, its halves' cursors;
/// `flip` while the merged entries are in `dst`, copied back from `copied` once sorted.
#[derive(Debug)]
struct Group {
    lo: usize,
    hi: usize,
    width: usize,
    at: usize,
    i: usize,
    j: usize,
    flip: bool,
    copied: usize,
}

impl Sort {
    /// Readies pass `pass`: each value of its byte starts after the entries of smaller values.
    fn start_pass(&mut self) {
        let Some(counts) = self
            .passes
            .get(self.pass)
            .filter(|_| self.pass < self.npass)
            .and_then(|&byte| self.counts.get(byte))
        else {
            return;
        };
        let mut sum = 0u32;
        for (next, &count) in self.next.iter_mut().zip(counts.iter()) {
            *next = sum;
            sum = sum.saturating_add(count);
        }
    }
}

/// An entry's operation byte, key and value lengths, and where its key starts in `arena`.
fn parse(arena: &[u8], entry: u32) -> Option<(u8, usize, usize, usize)> {
    let at = usize::try_from(entry).ok()?;
    let op = *arena.get(at)?;
    let mut pos = at.checked_add(1)?;
    let (klen, used) = get_varint32_ptr(arena.get(pos..)?).ok()?;
    pos = pos.checked_add(used)?;
    let (vlen, used) = get_varint32_ptr(arena.get(pos..)?).ok()?;
    pos = pos.checked_add(used)?;
    Some((
        op,
        usize::try_from(klen).ok()?,
        usize::try_from(vlen).ok()?,
        pos,
    ))
}

/// An entry's key, operation and value, and where the next entry starts.
type EntryAt<'a> = (&'a [u8], Op, &'a [u8], usize);

/// An entry's key, operation and value in `arena`, and where the next entry starts.
fn read_at(arena: &[u8], entry: u32) -> Result<EntryAt<'_>, Error> {
    let (op, klen, vlen, start) = parse(arena, entry).ok_or(corrupt())?;
    let op = match op {
        1 => Op::Put,
        2 => Op::Delete,
        _ => return Err(corrupt()),
    };
    let mid = start.checked_add(klen).ok_or(corrupt())?;
    let end = mid.checked_add(vlen).ok_or(corrupt())?;
    let key = arena.get(start..mid).ok_or(corrupt())?;
    let value = arena.get(mid..end).ok_or(corrupt())?;
    Ok((key, op, value, end))
}

/// An entry's key, operation and value in `arena`.
fn read(arena: &[u8], entry: u32) -> Result<(&[u8], Op, &[u8]), Error> {
    read_at(arena, entry).map(|(key, op, value, _)| (key, op, value))
}

/// An entry's key in `arena`.
fn key(arena: &[u8], entry: u32) -> &[u8] {
    parse(arena, entry)
        .and_then(|(_, klen, _, start)| arena.get(start..start.checked_add(klen)?))
        .unwrap_or(&[])
}

/// `a`, of a run at skip `sa`, against `b`, of a run at `sb`, by key: by their prefixes when
/// those were taken from the same place and differ, else by the keys themselves.
fn cmp(arena: &[u8], a: Keyed, sa: usize, b: Keyed, sb: usize) -> Ordering {
    if a.entry == b.entry {
        return Ordering::Equal;
    }
    if sa == sb && a.prefix != b.prefix {
        return a.prefix.cmp(&b.prefix);
    }
    key(arena, a.entry).cmp(key(arena, b.entry))
}

/// Entries of one run at `skip` by key, the newer (larger offset) after the older of equal keys.
fn order(arena: &[u8], a: Keyed, b: Keyed, skip: usize) -> Ordering {
    cmp(arena, a, skip, b, skip).then(a.entry.cmp(&b.entry))
}

/// Byte `byte` of a prefix, the most significant first.
fn digit(prefix: u64, byte: usize) -> usize {
    usize::from(prefix.to_be_bytes().get(byte).copied().unwrap_or(0))
}

/// A memtable's copied arena sorted where it is packed (research/40 §4): every entry keyed by
/// eight bytes of its key past the bytes every key shares and by its filter hash, as the
/// memtable keys its tail, then put in order by the memtable's own sort ([`Sort`]: a radix sort
/// of the prefixes, ties by key, the newest of each key kept), and walked in key order. Its
/// buffers are kept for the next arena it sorts.
#[derive(Debug, Default)]
pub struct Sorter {
    keyed: Vec<Keyed>,
    dst: Vec<Keyed>,
    counts: Option<Box<[[u32; 256]; 8]>>,
}

impl Sorter {
    /// Sorts the entries of `arena`, a memtable's arena as written, every key of which shares its
    /// first `skip` bytes with the first: the entries left once the newest of each key is kept.
    pub fn sort(&mut self, arena: &[u8], skip: usize) -> Result<usize, Error> {
        let too_large = || Error::LimitExceeded {
            what: "entries of a memtable's sort",
            limit: u64::try_from(arena.len()).unwrap_or(u64::MAX),
        };
        let mut src = std::mem::take(&mut self.keyed);
        src.clear();
        let mut at = 0usize;
        while at < arena.len() {
            let entry = u32::try_from(at).map_err(|_| corrupt())?;
            let (key, _, _, end) = read_at(arena, entry)?;
            src.try_reserve(1).map_err(|_| too_large())?;
            src.push(Keyed {
                prefix: prefix(key, skip),
                entry,
                hash32: crate::branch::filter::hash32(hash(key)),
            });
            at = end;
        }
        let n = src.len();
        let mut dst = std::mem::take(&mut self.dst);
        dst.clear();
        dst.try_reserve_exact(n).map_err(|_| too_large())?;
        // The scatter's scratch, written in full before it is read.
        dst.resize(n, Keyed::default());
        let counts = match self.counts.take() {
            Some(mut c) => {
                for byte in c.iter_mut() {
                    byte.fill(0);
                }
                c
            }
            None => Box::new([[0; 256]; 8]),
        };
        let mut s = Sort {
            src,
            dst,
            skip,
            stage: Stage::Rekey,
            at: 0,
            stale: 0,
            counts,
            passes: [0; 8],
            npass: 0,
            pass: 0,
            next: [0; 256],
            lo: 0,
            group: None,
            kept: 0,
            rate: 0,
        };
        // An unbounded budget ends the sort in one call: each stage reads every entry once.
        let (done, _) = sort_step(arena, &mut s, usize::MAX);
        self.keyed = s.src;
        self.dst = s.dst;
        self.counts = Some(s.counts);
        if !done {
            return Err(corrupt());
        }
        Ok(self.keyed.len())
    }

    /// Into `ends`, for each of `cuts` (ascending), how many of the entries the last sort kept
    /// lie below it: where a walk in key order ends a part at each cut. A binary search over
    /// the sorted entries each, reading the keys it compares from `arena`.
    pub fn ends_below(
        &self,
        arena: &[u8],
        cuts: &[Vec<u8>],
        ends: &mut Vec<usize>,
    ) -> Result<(), Error> {
        ends.clear();
        for cut in cuts {
            let (mut lo, mut hi) = (0usize, self.keyed.len());
            while lo < hi {
                let mid = lo.midpoint(hi);
                let entry = self.keyed.get(mid).ok_or(corrupt())?.entry;
                let (key, _, _) = read(arena, entry)?;
                if key < cut.as_slice() {
                    lo = mid.checked_add(1).ok_or(corrupt())?;
                } else {
                    hi = mid;
                }
            }
            ends.push(lo);
        }
        Ok(())
    }

    /// [`Self::each`] with each entry's arena offset: `each(key, op, value, hash, offset)`.
    pub fn each_at(
        &self,
        arena: &[u8],
        mut each: impl FnMut(&[u8], Op, &[u8], u32, u32) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for k in &self.keyed {
            let (key, op, value) = read(arena, k.entry)?;
            each(key, op, value, k.hash32, k.entry)?;
        }
        Ok(())
    }

    /// Each entry the last sort kept, in key order: `each(key, op, value, hash)`, the hash the
    /// key's 32-bit filter hash.
    pub fn each(
        &self,
        arena: &[u8],
        mut each: impl FnMut(&[u8], Op, &[u8], u32) -> Result<(), Error>,
    ) -> Result<(), Error> {
        for k in &self.keyed {
            let (key, op, value) = read(arena, k.entry)?;
            each(key, op, value, k.hash32)?;
        }
        Ok(())
    }

    /// The bytes its buffers hold, at their capacities.
    pub fn memory(&self) -> usize {
        self.keyed
            .capacity()
            .saturating_add(self.dst.capacity())
            .saturating_mul(size_of::<Keyed>())
            .saturating_add(if self.counts.is_some() {
                size_of::<[[u32; 256]; 8]>()
            } else {
                0
            })
    }
}

/// Entry buffers a memtable shed for another's fill ([`HashMem::shed`]), and the most entries its
/// tail held.
#[derive(Debug, Default)]
pub struct Bufs {
    bufs: Vec<Vec<Keyed>>,
    high: usize,
}

/// A memtable: the arena, its bound, the index of newest entries, the collisions the index
/// cannot hold, the sorted runs and the unsorted tail, and the work in progress on them.
#[derive(Debug)]
pub struct HashMem {
    arena: Vec<u8>,
    limit: usize,
    /// The most bytes the arena has held: its pages written so far, which a clear keeps.
    touched: usize,
    /// Each key's newest entry, by the key's hash: keys of one hash are told apart by their
    /// bytes ([`KeyIndex`]).
    index: KeyIndex,
    len: usize,
    /// The first key written, and how many of its first bytes every key written since shares.
    first: Vec<u8>,
    skip: usize,
    /// Sorted runs, oldest first.
    runs: Vec<Run>,
    /// Entries since the last run, their prefixes from `skip`, except the first `tail_stale`,
    /// taken before `skip` last shrank.
    tail: Vec<Keyed>,
    tail_stale: usize,
    /// Entries of the tail whose share of the merges was paid as they were written
    /// ([`Self::order_put`]): a seal pays only the rest.
    prepaid: usize,
    /// The merges in progress, at most one a level.
    merges: Vec<Merge>,
    sort: Option<Sort>,
    /// Entry buffers finished sorts, merges and fills freed, for the next: a buffer is taken
    /// at the smallest capacity that holds what it is for, so a memtable's fills, sorts and
    /// merges reuse what the last ones grew and allocate nothing once grown. At most
    /// [`MAX_RUNS`] are kept, the smallest dropped past that: no more are ever in use at once.
    bufs: Vec<Vec<Keyed>>,
    /// The longest the tail has grown: what the next fill's tail is given room for.
    tail_high: usize,
    /// A copy of the arena, written with it, for a worker to sort and pack ([`Sorter`]); whole
    /// while `mirrored`, which it is only if given before the first entry and never short of
    /// room, as it is reserved to the arena's bound.
    mirror: Vec<u8>,
    mirrored: bool,
    /// A finished sort's histogram, for the next.
    counts: Option<Box<[[u32; 256]; 8]>>,
    /// Closed to writes for packing: every entry is to be sorted, the tail as soon as the sort in
    /// progress ends.
    closed: bool,
    /// Segments of key ranges a scan's walk merged from the runs, disjoint, each holding the
    /// newest entry of every key in it: a seek into one walks it alone (adaptive merging; Graefe
    /// and Kuno, EDBT 2010). A segment joins the next when no key lies between, so a range is a
    /// chain of them. A write into one drops it. A slab: a dropped segment's node goes to the
    /// next, and a clear keeps every node for the next fill.
    hot: Vec<Hot>,
    /// The slab's free nodes, and the root of the treap ordering the rest by first key
    /// (Seidel and Aragon, Algorithmica 1996): a segment found, added or dropped in expected
    /// logarithmic steps, its links in its node, moving and allocating nothing.
    hot_free: Vec<u32>,
    hot_root: u32,
    /// Every segment's entries, each a slice appended in turn, and how many of them live
    /// segments hold. A dropped segment leaves a hole until the pool would pass twice the
    /// memtable's entries; the live ones, at most one an entry since they are disjoint, are then
    /// moved down in order, so each move frees at least half the pool and costs a constant an
    /// entry recorded (the doubling argument; Tarjan, SIAM J. Algebraic Discrete Methods 1985).
    /// A clear keeps the pool's buffer, so segments come and go without allocating once grown.
    hot_pool: Vec<Keyed>,
    hot_live: usize,
    /// The live segments in the order their entries were appended, oldest first: the order the
    /// pool is moved down in.
    hot_oldest: u32,
    hot_newest: u32,
}

/// No node of the merged ranges' treap ([`HashMem::hot_root`]).
const NIL: u32 = u32::MAX;

/// A segment of a key range a scan's walk merged ([`HashMem::hot`]): its entries in the pool,
/// the newest of each key from its first to its last; whether the next segment follows with no
/// key between; its links in the treap and in the pool's append order.
#[derive(Debug)]
struct Hot {
    start: usize,
    len: usize,
    joins_next: bool,
    left: u32,
    right: u32,
    older: u32,
    newer: u32,
}

impl Hot {
    fn empty() -> Self {
        Self {
            start: 0,
            len: 0,
            joins_next: false,
            left: NIL,
            right: NIL,
            older: NIL,
            newer: NIL,
        }
    }
}

/// Segment `h`'s entries in `pool`.
fn seg<'a>(pool: &'a [Keyed], h: &Hot) -> &'a [Keyed] {
    pool.get(h.start..h.start.saturating_add(h.len))
        .unwrap_or(&[])
}

/// Slab index of node `id`.
fn ix(id: u32) -> usize {
    usize::try_from(id).unwrap_or(usize::MAX)
}

/// Node `id`'s treap priority: a hash of its slab index, independent of the keys it orders.
fn priority(id: u32) -> u64 {
    hash(&id.to_le_bytes())
}

/// Segment `id`'s first key.
fn hot_lo<'a>(arena: &'a [u8], pool: &[Keyed], hot: &[Hot], id: u32) -> &'a [u8] {
    hot.get(ix(id))
        .and_then(|h| seg(pool, h).first())
        .map_or(&[], |k| key(arena, k.entry))
}

/// Where the next node a treap walk picks hangs: a new root, or a node's left or right link.
#[derive(Clone, Copy)]
enum Slot {
    Root,
    Left(u32),
    Right(u32),
}

/// Hangs `x` at `slot`; a root slot is `root`.
fn hang(hot: &mut [Hot], root: &mut u32, slot: Slot, x: u32) {
    match slot {
        Slot::Root => *root = x,
        Slot::Left(n) => {
            if let Some(h) = hot.get_mut(ix(n)) {
                h.left = x;
            }
        }
        Slot::Right(n) => {
            if let Some(h) = hot.get_mut(ix(n)) {
                h.right = x;
            }
        }
    }
}

/// Splits treap `t` into the ranges whose first key is below `k` (or not above it, with
/// `or_equal`) and the rest: one walk down from `t`, each node hung on the side it goes to and
/// the walk going on into the side it leaves open, a step a node. `None` when the walk passes
/// the slab's size, which no treap allows.
fn split(
    arena: &[u8],
    pool: &[Keyed],
    hot: &mut [Hot],
    t: u32,
    k: &[u8],
    or_equal: bool,
) -> Option<(u32, u32)> {
    let (mut below, mut rest) = (NIL, NIL);
    let (mut low, mut high) = (Slot::Root, Slot::Root);
    let mut t = t;
    for _ in 0..=hot.len() {
        if t == NIL {
            hang(hot, &mut below, low, NIL);
            hang(hot, &mut rest, high, NIL);
            return Some((below, rest));
        }
        let lo = hot_lo(arena, pool, hot, t);
        let goes_left = lo < k || (or_equal && lo == k);
        let node = hot.get(ix(t))?;
        let (left, right) = (node.left, node.right);
        if goes_left {
            hang(hot, &mut below, low, t);
            low = Slot::Right(t);
            t = right;
        } else {
            hang(hot, &mut rest, high, t);
            high = Slot::Left(t);
            t = left;
        }
    }
    None
}

/// Joins treaps `a` and `b`, every first key of `a` below every one of `b`: one walk down `a`'s
/// right spine and `b`'s left spine, the higher priority hung first, a step a node. `None` when
/// the walk passes the slab's size, which no treap allows.
fn join(hot: &mut [Hot], a: u32, b: u32) -> Option<u32> {
    let mut root = NIL;
    let mut slot = Slot::Root;
    let (mut a, mut b) = (a, b);
    for _ in 0..=hot.len() {
        if a == NIL {
            hang(hot, &mut root, slot, b);
            return Some(root);
        }
        if b == NIL {
            hang(hot, &mut root, slot, a);
            return Some(root);
        }
        if priority(a) > priority(b) {
            hang(hot, &mut root, slot, a);
            slot = Slot::Right(a);
            a = hot.get(ix(a))?.right;
        } else {
            hang(hot, &mut root, slot, b);
            slot = Slot::Left(b);
            b = hot.get(ix(b))?.left;
        }
    }
    None
}

/// Bytes an entry of a `key`-byte key and a `value`-byte value takes of a memtable's arena, its
/// head with them: [`HashMem::insert_hashed`] refuses one past the memtable's bound, and the shard
/// rotates only a memtable that holds something, so a memtable holds the largest entry its
/// engine takes, alone, or that entry is refused whenever it comes.
pub fn entry_bytes(key: usize, value: usize) -> Option<usize> {
    ENTRY_HEAD_MAX.checked_add(key)?.checked_add(value)
}

impl HashMem {
    /// An empty memtable whose arena holds at most `limit` bytes, at most 4 GiB (offsets are 32
    /// bits).
    pub fn new(limit: usize) -> Result<Self, Error> {
        if u32::try_from(limit).is_err() {
            return Err(Error::InvalidArgument {
                what: "a memtable past 4 GiB",
            });
        }
        // The arena's whole bound is reserved once: address space a page backs only when written,
        // so the arena never reallocates and copies as it fills, and a cleared memtable reuses it.
        Ok(Self {
            arena: Vec::with_capacity(limit),
            limit,
            touched: 0,
            index: KeyIndex::default(),
            len: 0,
            first: Vec::new(),
            skip: 0,
            runs: Vec::new(),
            tail: Vec::new(),
            tail_stale: 0,
            prepaid: 0,
            merges: Vec::new(),
            sort: None,
            bufs: Vec::new(),
            tail_high: 0,
            mirror: Vec::new(),
            mirrored: false,
            counts: None,
            closed: false,
            hot: Vec::new(),
            hot_free: Vec::new(),
            hot_root: NIL,
            hot_pool: Vec::new(),
            hot_live: 0,
            hot_oldest: NIL,
            hot_newest: NIL,
        })
    }

    /// [`Self::new`], its index sized for `entries`, as many as the memtable before it held: it
    /// fills without the index growing or migrating, as a cleared spare refills
    /// ([`KeyIndex::for_entries`]).
    pub fn new_for(limit: usize, entries: usize) -> Result<Self, Error> {
        let mut mem = Self::new(limit)?;
        mem.index = KeyIndex::for_entries(entries);
        Ok(mem)
    }

    /// Reads `hash`'s first index group ([`KeyIndex::touch`]).
    #[inline]
    pub fn touch(&self, hash: u64) -> u64 {
        self.index.touch(hash)
    }

    /// Gives up the entry buffers a memtable read through its index alone no longer needs: its
    /// tail, its sorted runs' and merges' buffers and its sort's, for a memtable starting its
    /// fill ([`Self::take_shed`]), which then grows none of them. A packed memtable kept for gets
    /// (the shard's kept memtables) is never walked again.
    pub fn shed(&mut self) -> Bufs {
        self.tail_high = self.tail_high.max(self.tail.len());
        let mut bufs = std::mem::take(&mut self.bufs);
        bufs.push(std::mem::take(&mut self.tail));
        while let Some(r) = self.runs.pop() {
            bufs.push(r.keys);
        }
        while let Some(m) = self.merges.pop() {
            bufs.push(m.a.keys);
            bufs.push(m.b.keys);
            bufs.push(m.out.keys);
        }
        if let Some(sort) = self.sort.take() {
            bufs.push(sort.src);
            bufs.push(sort.dst);
        }
        Bufs {
            bufs,
            high: self.tail_high,
        }
    }

    /// Takes the buffers another memtable shed ([`Self::shed`]) for this one's fill, its tail
    /// the one that held the most entries: an empty memtable's tail grows no more than that
    /// one's did.
    pub fn take_shed(&mut self, shed: Bufs) {
        let Bufs { bufs, high } = shed;
        for b in bufs {
            self.give_buf(b);
        }
        if self.tail.is_empty() {
            self.tail_high = self.tail_high.max(high);
            let tail = std::mem::take(&mut self.tail);
            self.give_buf(tail);
            self.tail = self.take_buf(self.tail_high);
        }
    }

    /// Empties the memtable for its next fill, keeping its arena and every entry buffer, the
    /// tail given room for as long as it has grown.
    pub fn clear(&mut self) {
        self.drop_hot();
        self.touched = self.touched.max(self.arena.len());
        self.arena.clear();
        self.mirror.clear();
        self.mirrored = self.mirror.capacity() >= self.limit;
        self.index.clear();
        self.len = 0;
        self.first.clear();
        self.skip = 0;
        self.tail_stale = 0;
        self.prepaid = 0;
        self.closed = false;
        self.tail_high = self.tail_high.max(self.tail.len());
        let tail = std::mem::take(&mut self.tail);
        self.give_buf(tail);
        while let Some(r) = self.runs.pop() {
            self.give_buf(r.keys);
        }
        while let Some(m) = self.merges.pop() {
            self.give_buf(m.a.keys);
            self.give_buf(m.b.keys);
            self.give_buf(m.out.keys);
        }
        if let Some(s) = self.sort.take() {
            self.give_buf(s.src);
            self.give_buf(s.dst);
        }
        self.tail = self.take_buf(self.tail_high);
    }

    /// An empty buffer for `need` entries: the free one of least capacity that holds them, else
    /// the largest, grown.
    fn take_buf(&mut self, need: usize) -> Vec<Keyed> {
        let fits = self
            .bufs
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity() >= need)
            .min_by_key(|(_, b)| b.capacity())
            .map(|(i, _)| i);
        let pick = fits.or_else(|| {
            self.bufs
                .iter()
                .enumerate()
                .max_by_key(|(_, b)| b.capacity())
                .map(|(i, _)| i)
        });
        let mut buf = match pick {
            Some(i) if i < self.bufs.len() => self.bufs.swap_remove(i),
            _ => Vec::new(),
        };
        buf.clear();
        buf.reserve(need);
        buf
    }

    /// Keeps `buf` for a later [`Self::take_buf`]: the smallest kept is dropped past
    /// [`MAX_RUNS`] buffers.
    fn give_buf(&mut self, mut buf: Vec<Keyed>) {
        if buf.capacity() == 0 {
            return;
        }
        buf.clear();
        if self.bufs.len() >= MAX_RUNS {
            let smallest = self
                .bufs
                .iter()
                .enumerate()
                .min_by_key(|(_, b)| b.capacity())
                .map(|(i, b)| (i, b.capacity()));
            match smallest {
                Some((i, cap)) if cap < buf.capacity() => {
                    self.bufs.swap_remove(i);
                }
                _ => return,
            }
        }
        self.bufs.push(buf);
    }

    /// Keeps a copy of the arena in `buf` as entries are written, for a worker to sort and pack
    /// ([`Sorter`]): taken only while the memtable is empty, with room for its whole bound
    /// reserved, so the copy holds every entry and never reallocates. The buffer comes back
    /// when it cannot be: entries already written, or its room refused.
    pub fn mirror_into(&mut self, mut buf: Vec<u8>) -> Result<(), Vec<u8>> {
        if self.mirrored {
            return Err(buf);
        }
        if !self.arena.is_empty() {
            return Err(buf);
        }
        buf.clear();
        if buf.capacity() < self.limit && buf.try_reserve_exact(self.limit).is_err() {
            return Err(buf);
        }
        self.mirror = buf;
        self.mirrored = true;
        Ok(())
    }

    /// Whether the memtable keeps a whole copy of its arena for a worker.
    pub fn mirrored(&self) -> bool {
        self.mirrored
    }

    /// The copy of the arena, every entry in it, for a worker to sort: none if the memtable kept
    /// none. The memtable keeps no copy after.
    pub fn take_mirror(&mut self) -> Option<Vec<u8>> {
        if !self.mirrored || self.mirror.len() != self.arena.len() {
            return None;
        }
        self.mirrored = false;
        Some(std::mem::take(&mut self.mirror))
    }

    /// A copy of the arena as it stands into `buf`, for a memtable that kept none as it was
    /// written: refused, typed, when its room cannot be had.
    pub fn copy_arena(&self, buf: &mut Vec<u8>) -> Result<(), Error> {
        buf.clear();
        buf.try_reserve_exact(self.arena.len())
            .map_err(|_| Error::LimitExceeded {
                what: "memory for a copy of a memtable's arena",
                limit: u64::try_from(self.arena.len()).unwrap_or(u64::MAX),
            })?;
        buf.extend_from_slice(&self.arena);
        Ok(())
    }

    /// How many first bytes every key written shares with the first: where its keys' prefixes
    /// start ([`Sorter::sort`]).
    pub fn skip(&self) -> usize {
        self.skip
    }

    /// Entries the memtable holds, each key once.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes the memtable holds: the arena's pages as far as it was ever filled, its
    /// index's table, its entry buffers and its sort's histogram, each at its capacity. A
    /// cleared memtable keeps them all for its next fill (the arena reserved once and written
    /// only so far, the buffers recycled), so what it holds is what it was given, not what it
    /// now uses.
    pub fn memory(&self) -> usize {
        let page = |n: usize| n.div_ceil(4096).saturating_mul(4096);
        let counts = if self.sort.is_some() || self.counts.is_some() {
            size_of::<[[u32; 256]; 8]>()
        } else {
            0
        };
        let sort = self
            .sort
            .as_ref()
            .map_or(0, |s| s.src.capacity().saturating_add(s.dst.capacity()));
        let merge = self.merges.iter().fold(0usize, |sum, m| {
            sum.saturating_add(m.a.keys.capacity())
                .saturating_add(m.b.keys.capacity())
                .saturating_add(m.out.keys.capacity())
        });
        let keyed = self
            .runs
            .iter()
            .map(|r| r.keys.capacity())
            .fold(self.tail.capacity(), usize::saturating_add)
            .saturating_add(
                self.bufs
                    .iter()
                    .fold(0usize, |sum, b| sum.saturating_add(b.capacity())),
            )
            .saturating_add(sort)
            .saturating_add(merge)
            .saturating_add(self.hot_pool.capacity())
            .saturating_mul(size_of::<Keyed>());
        page(self.touched.max(self.arena.len()))
            .saturating_add(page(
                self.mirror
                    .capacity()
                    .min(self.touched.max(self.mirror.len())),
            ))
            .saturating_add(self.index.bytes())
            .saturating_add(keyed)
            .saturating_add(counts)
            .saturating_add(self.first.capacity())
            .saturating_add(self.hot.capacity().saturating_mul(size_of::<Hot>()))
            .saturating_add(self.hot_free.capacity().saturating_mul(size_of::<u32>()))
    }

    /// Arena bytes left before the bound.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.arena.len())
    }

    /// Arena bytes used: what the bound is against.
    pub fn bytes(&self) -> usize {
        self.arena.len()
    }

    fn key(&self, entry: u32) -> &[u8] {
        key(&self.arena, entry)
    }

    /// An entry's key, operation and value.
    fn read(&self, entry: u32) -> Result<(&[u8], Op, &[u8]), Error> {
        read(&self.arena, entry)
    }

    fn cmp(&self, a: Keyed, sa: usize, b: Keyed, sb: usize) -> Ordering {
        cmp(&self.arena, a, sa, b, sb)
    }

    /// The newest entry for `key`, of hash `hash`.
    fn newest(&self, key: &[u8], hash: u64) -> Option<u32> {
        let arena = &self.arena;
        self.index.get(hash, |e| self::key(arena, e) == key)
    }

    /// The entry at arena offset `offset` if its key is `key`: its operation and value into
    /// `value`. None for another key's: the index over kept memtables names an entry by its
    /// key's hash alone (`crate::kept`).
    pub fn get_at(
        &self,
        offset: u32,
        key: &[u8],
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        let (k, op, v) = self.read(offset)?;
        if k != key {
            return Ok(None);
        }
        value.clear();
        value.extend_from_slice(v);
        Ok(Some(op))
    }

    /// Each key's newest entry, its filter hash and arena offset, into `list`: what a packing
    /// worker's walk gives for a memtable it sorted, made here from the index for one the shard
    /// packed itself.
    pub fn key_list(&self, list: &mut crate::kept::KeyList) -> Result<(), Error> {
        list.clear();
        let mut failed = None;
        self.index.each_entry(|e| match self.read(e) {
            Ok((k, _, _)) => list.push(hash(k), e),
            Err(error) => {
                failed.get_or_insert(error);
            }
        });
        failed.map_or(Ok(()), Err)
    }

    /// The newest entry for `key`, of filter hash `hash` ([`crate::branch::filter::hash`]): its
    /// operation and value into `value`.
    pub fn get_hashed(
        &self,
        key: &[u8],
        hash: u64,
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        let Some(e) = self.newest(key, hash) else {
            return Ok(None);
        };
        let (_, op, v) = self.read(e)?;
        value.clear();
        value.extend_from_slice(v);
        Ok(Some(op))
    }

    /// The newest entry for `key`: its operation and value into `value`.
    pub fn get(&self, key: &[u8], value: &mut Vec<u8>) -> Result<Option<Op>, Error> {
        self.get_hashed(key, hash(key), value)
    }

    /// Records `op` of `key` with `value` (empty for a delete). Refused, changing nothing, when
    /// the arena would pass its bound: the shard then packs the memtable into a branch.
    pub fn insert(&mut self, key: &[u8], op: Op, value: &[u8]) -> Result<(), Error> {
        self.insert_hashed(key, hash(key), op, value)
    }

    /// [`Self::insert`] of a key whose hash is `h`.
    pub fn insert_hashed(&mut self, key: &[u8], h: u64, op: Op, value: &[u8]) -> Result<(), Error> {
        let klen = u16::try_from(key.len()).map_err(|_| Error::InvalidArgument {
            what: "a key past 64 KiB",
        })?;
        let vlen = u32::try_from(value.len()).map_err(|_| Error::InvalidArgument {
            what: "a value past 4 GiB",
        })?;
        let size = ENTRY_HEAD_MAX
            .checked_add(key.len())
            .and_then(|s| s.checked_add(value.len()))
            .ok_or(Error::InvalidArgument {
                what: "an entry past the address space",
            })?;
        if self
            .arena
            .len()
            .checked_add(size)
            .is_none_or(|end| end > self.limit)
        {
            return Err(Error::LimitExceeded {
                what: "a memtable's bytes",
                limit: u64::try_from(self.limit).unwrap_or(u64::MAX),
            });
        }
        let entry = u32::try_from(self.arena.len()).map_err(|_| corrupt())?;
        if entry == 0 {
            self.first.clear();
            self.first.extend_from_slice(key);
            self.skip = key.len();
        } else {
            let shared = crate::util::keys::common_prefix(
                self.first.get(..self.skip).unwrap_or(&self.first),
                key,
            );
            if shared < self.skip {
                // The tail's prefixes so far start too far in for the keys to come: its sort
                // takes them again.
                self.skip = shared;
                self.tail_stale = self.tail.len();
            }
        }
        if self.hot_root != NIL {
            self.unmerge(key);
        }
        // The entry's head in one append: its operation's byte (below 128, its own varint) and
        // the key's and value's lengths, a byte each below 128.
        let code = match op {
            Op::Put => 1,
            Op::Delete => 2,
        };
        put_varint32s(&mut self.arena, &[code, u32::from(klen), vlen]);
        self.arena.extend_from_slice(key);
        self.arena.extend_from_slice(value);
        if self.mirrored {
            // Within the room reserved, so the copy never reallocates; past it (never, as the
            // arena's bound is the room) the mirror is no longer whole.
            let bytes = usize::try_from(entry)
                .ok()
                .and_then(|at| self.arena.get(at..))
                .unwrap_or(&[]);
            if self.mirror.len().saturating_add(bytes.len()) <= self.mirror.capacity() {
                self.mirror.extend_from_slice(bytes);
            } else {
                self.mirrored = false;
            }
        }
        let arena = &self.arena;
        let replaced = self.index.put(
            h,
            entry,
            |e| self::key(arena, e) == key,
            |e| hash(self::key(arena, e)),
        );
        if replaced.is_none() {
            self.len = self.len.saturating_add(1);
        }
        self.tail.push(Keyed {
            prefix: prefix(key, self.skip),
            entry,
            hash32: crate::branch::filter::hash32(h),
        });
        Ok(())
    }

    /// Sorts the tail into a run, so a walk sees every entry: what a scan of this memtable asks
    /// first. The tail is what arrived since the last seal; while the memtable is written and
    /// scanned that is a few entries, and a memtable written alone is sorted by
    /// [`Self::start_sort`]'s paced sort instead.
    pub fn seal(&mut self) {
        // A sort in progress is finished here, then the tail since is sorted: the walk needs the
        // order now.
        // Entries whose share of the merges their writes paid are not charged again.
        let sealed = self.tail.len().saturating_sub(self.prepaid);
        self.finish_sort();
        self.start_sort();
        self.finish_sort();
        // Each merge in progress moves twice the entries sealed (Overmars and van Leeuwen's
        // de-amortized logarithmic method). A run of level k holds at least 2^(k-1) entries, so
        // two more reach a level only once 2^k more are sealed, and a level's merge moves fewer
        // than 2^(k+1): it is done first. However often scans seal, each level then holds at most
        // two runs and the merge of two more.
        let mut i = 0;
        while let Some(m) = self.merges.get_mut(i) {
            if merge_step(&self.arena, m, sealed.saturating_mul(2)).0 {
                self.end_merge(i);
            } else {
                i = i.saturating_add(1);
            }
        }
        self.schedule();
    }

    fn finish_sort(&mut self) {
        if let Some(s) = self.sort.as_mut()
            && sort_step(&self.arena, s, usize::MAX).0
        {
            self.end_sort();
        }
    }

    /// Keeps the finished sort's run, and its scratch for the next.
    fn end_sort(&mut self) {
        if let Some(mut s) = self.sort.take() {
            self.runs.push(Run {
                skip: s.skip,
                keys: std::mem::take(&mut s.src),
            });
            self.give_buf(s.dst);
            self.counts = Some(s.counts);
            self.schedule();
            if self.closed {
                self.start_sort();
            }
        }
    }

    /// Keeps finished merge `i`'s run, and its inputs' buffers for the next.
    fn end_merge(&mut self, i: usize) {
        if i < self.merges.len() {
            let mut m = self.merges.swap_remove(i);
            self.runs.push(std::mem::take(&mut m.out));
            self.give_buf(m.a.keys);
            self.give_buf(m.b.keys);
        }
    }

    /// Starts the merges the counter owes: two runs of a level with no merge in progress.
    fn schedule(&mut self) {
        loop {
            let mut pair = None;
            for (i, r) in self.runs.iter().enumerate() {
                let l = level(r.keys.len());
                if self.merges.iter().any(|m| m.level == l) {
                    continue;
                }
                let later = self.runs.get(i.saturating_add(1)..).unwrap_or(&[]);
                if let Some(j) = later.iter().position(|q| level(q.keys.len()) == l) {
                    pair = Some((i, i.saturating_add(1).saturating_add(j), l));
                    break;
                }
            }
            let Some((i, j, l)) = pair else {
                return;
            };
            let b = self.runs.remove(j);
            let a = self.runs.remove(i);
            let need = a.keys.len().saturating_add(b.keys.len());
            let keys = self.take_buf(need);
            let out = Run {
                skip: a.skip.min(b.skip),
                keys,
            };
            self.merges.push(Merge {
                a,
                b,
                i: 0,
                j: 0,
                out,
                level: l,
            });
        }
    }

    /// Closes the memtable to writes, for packing: its order work ([`Self::debt`]) then covers
    /// every entry, the tail written since the sort in progress began included.
    pub fn close(&mut self) {
        self.closed = true;
        self.start_sort();
    }

    /// Whether a walk merges more than one run, or order work is open: what idle time tidies
    /// ([`Self::tidy`]).
    pub fn untidy(&self) -> bool {
        self.runs.len() > 1
            || !self.tail.is_empty()
            || self.sort.is_some()
            || !self.merges.is_empty()
    }

    /// Up to `budget` entries of idle order work: what is owed, then the tail sorted, then the
    /// two smallest runs merged, until one run holds every entry and a seek searches one. Runs
    /// of different levels are never merged as puts pay (the counter merges a level's two), so
    /// a memtable filled alone holds a run a halving of its room; idle time merges them, the
    /// smallest first, at no put's cost. Returns the work done, at least one while untidy.
    pub fn tidy(&mut self, budget: usize) -> usize {
        if self.sort.is_none() && self.merges.is_empty() {
            if !self.tail.is_empty() {
                self.start_sort();
            } else if self.runs.len() > 1 {
                self.merge_smallest();
            } else {
                return 0;
            }
        }
        let done = self.pay(budget).max(1);
        if !self.untidy() {
            // One run: a seek searches it alone, and the merged ranges save nothing.
            self.drop_hot();
        }
        done
    }

    /// The entries [`Self::tidy`] moves to leave one run: the order work owed, the tail's sort, then
    /// the runs merged two smallest first, the order of fewest moves (Huffman, Proc. IRE 1952).
    /// `None` past [`MAX_RUNS`] runs, which the counter never holds.
    pub fn collapse_moves(&self) -> Option<usize> {
        let mut sizes = [0usize; MAX_RUNS];
        let mut k = 0usize;
        let merging = self
            .merges
            .iter()
            .map(|m| m.a.keys.len().saturating_add(m.b.keys.len()));
        let tail = Some(self.tail.len()).filter(|&n| n > 0);
        for n in self
            .runs
            .iter()
            .map(|r| r.keys.len())
            .chain(merging)
            .chain(tail)
        {
            *sizes.get_mut(k)? = n;
            k = k.saturating_add(1);
        }
        let mut moves = self.debt();
        if !self.closed || self.sort.is_none() {
            moves = moves.saturating_add(tail.map_or(0, sort_reads));
        }
        // Two queues: the sizes in order, and the merges' outputs, which come out in order too.
        let sizes = sizes.get_mut(..k)?;
        sizes.sort_unstable();
        let mut merged = [0usize; MAX_RUNS];
        let (mut i, mut j, mut m) = (0usize, 0usize, 0usize);
        let take = |i: &mut usize, j: &mut usize, sizes: &[usize], merged: &[usize], m: usize| {
            let a = sizes.get(*i).copied();
            let b = merged.get(*j).copied().filter(|_| *j < m);
            match (a, b) {
                (Some(a), Some(b)) if b < a => {
                    *j = j.saturating_add(1);
                    b
                }
                (Some(a), _) => {
                    *i = i.saturating_add(1);
                    a
                }
                (None, Some(b)) => {
                    *j = j.saturating_add(1);
                    b
                }
                (None, None) => 0,
            }
        };
        for _ in 1..k {
            let a = take(&mut i, &mut j, sizes, &merged, m);
            let b = take(&mut i, &mut j, sizes, &merged, m);
            let out = a.saturating_add(b);
            *merged.get_mut(m)? = out;
            m = m.saturating_add(1);
            moves = moves.saturating_add(out);
        }
        Some(moves)
    }

    /// The comparisons a seek makes in the runs a walk merges, a binary search of each, and those
    /// it would make were they one run of all their entries.
    pub fn seek_comparisons(&self) -> (usize, usize) {
        let (mut many, mut all) = (0usize, 0usize);
        for (_, run) in self.sources() {
            many = many.saturating_add(usize::try_from(level(run.len())).unwrap_or(usize::MAX));
            all = all.saturating_add(run.len());
        }
        (many, usize::try_from(level(all)).unwrap_or(usize::MAX))
    }

    /// The comparisons a walk's step spends on the runs being many: the heap's sift, two a level
    /// of a heap over every run, none over one.
    pub fn step_comparisons(&self) -> usize {
        let runs = self.sources().count();
        usize::try_from(runs.checked_ilog2().unwrap_or(0))
            .unwrap_or(usize::MAX)
            .saturating_mul(2)
    }

    /// Starts a merge of the two smallest runs, whatever their levels.
    fn merge_smallest(&mut self) {
        let mut order: [Option<(usize, usize)>; 2] = [None, None];
        for (i, r) in self.runs.iter().enumerate() {
            let n = r.keys.len();
            match order {
                [None, _] => order[0] = Some((i, n)),
                [Some((_, a)), None] if n < a => order = [Some((i, n)), order[0]],
                [Some(_), None] => order[1] = Some((i, n)),
                [Some((_, a)), Some(_)] if n < a => order = [Some((i, n)), order[0]],
                [Some(_), Some((_, b))] if n < b => order[1] = Some((i, n)),
                _ => {}
            }
        }
        let [Some((x, _)), Some((y, _))] = order else {
            return;
        };
        let (lo, hi) = (x.min(y), x.max(y));
        let b = self.runs.remove(hi);
        let a = self.runs.remove(lo);
        let need = a.keys.len().saturating_add(b.keys.len());
        let keys = self.take_buf(need);
        let out = Run {
            skip: a.skip.min(b.skip),
            keys,
        };
        let level = level(a.keys.len().max(b.keys.len()));
        self.merges.push(Merge {
            a,
            b,
            i: 0,
            j: 0,
            out,
            level,
        });
    }

    /// The order work `entries` entries just written pay, for a memtable written as it fills:
    /// the tail is sorted in chunks of at most `bound` entries (or, nearer its bound, of the
    /// arena bytes left, `room`, so little is left at rotation), each paid at its own rate over
    /// as many entries as it holds, so it ends as the next is due; a sort still open when the
    /// next chunk is due is finished first, so the tail never passes twice `bound`. Each merge
    /// in progress moves twice the entries written, as [`Self::seal`] pays it. A scan's seal
    /// then sorts at most two chunks, however long the burst before it. Returns the work done.
    pub fn order_put(&mut self, entries: usize, bound: usize, room: usize) -> usize {
        let mut done = 0usize;
        let due = self.tail.len() >= bound.max(1) || self.tail_bytes() >= room;
        if due && self.sort.is_some() {
            done = done.saturating_add(self.finish_sort_counted());
        }
        if due && self.sort.is_none() {
            self.start_sort();
        }
        if let Some(s) = self.sort.as_mut() {
            let budget = s.rate.saturating_mul(entries);
            let (finished, moved) = sort_step(&self.arena, s, budget);
            done = done.saturating_add(moved);
            if finished {
                self.end_sort();
            }
        }
        self.prepaid = self.prepaid.saturating_add(entries).min(self.tail.len());
        let mut i = 0;
        while let Some(m) = self.merges.get_mut(i) {
            let (finished, moved) = merge_step(&self.arena, m, entries.saturating_mul(2));
            done = done.saturating_add(moved);
            if finished {
                self.end_merge(i);
            } else {
                i = i.saturating_add(1);
            }
        }
        self.schedule();
        done
    }

    /// [`Self::finish_sort`], returning the work it did.
    fn finish_sort_counted(&mut self) -> usize {
        let Some(s) = self.sort.as_mut() else {
            return 0;
        };
        let (finished, moved) = sort_step(&self.arena, s, usize::MAX);
        if finished {
            self.end_sort();
        }
        moved
    }

    /// Whether a sort is in progress.
    pub fn sorting(&self) -> bool {
        self.sort.is_some()
    }

    /// The arena bytes the tail's entries take: what has arrived since the last sort began.
    pub fn tail_bytes(&self) -> usize {
        self.tail.first().map_or(0, |k| {
            self.arena
                .len()
                .saturating_sub(usize::try_from(k.entry).unwrap_or(usize::MAX))
        })
    }

    /// Starts sorting the tail a slice at a time, unless a sort is in progress: a debt
    /// ([`Self::debt`]) the owner pays over its puts or the packing, not one operation's. Entries
    /// written meanwhile form the next tail.
    pub fn start_sort(&mut self) {
        if self.sort.is_some() || self.tail.is_empty() {
            return;
        }
        let n = self.tail.len();
        self.tail_high = self.tail_high.max(n);
        // The tail's buffer becomes the sort's; the next tail gets room for as many again.
        let next = self.take_buf(n);
        let src = std::mem::replace(&mut self.tail, next);
        self.prepaid = 0;
        // The scatter's scratch, written in full before it is read.
        let mut dst = self.take_buf(n);
        dst.resize(n, Keyed::default());
        let counts = match self.counts.take() {
            Some(mut c) => {
                for byte in c.iter_mut() {
                    byte.fill(0);
                }
                c
            }
            None => Box::new([[0; 256]; 8]),
        };
        self.sort = Some(Sort {
            src,
            dst,
            skip: self.skip,
            stage: Stage::Rekey,
            at: 0,
            stale: std::mem::take(&mut self.tail_stale),
            counts,
            passes: [0; 8],
            npass: 0,
            pass: 0,
            next: [0; 256],
            lo: 0,
            group: None,
            kept: 0,
            rate: 0,
        });
        if let Some(s) = self.sort.as_mut() {
            let n = s.src.len();
            // The debt as [`Self::debt`] counts it, over the chunk's own entries.
            let debt = sort_reads(n).saturating_add(s.stale).saturating_add(1);
            s.rate = debt.div_ceil(n.max(1));
        }
    }

    /// The order work owed, in entries moved: what is left of the merge and the sort in progress,
    /// and one for each to finish (its run handed over), so the debt is nothing only once no job
    /// is open: a walk opened then sees runs no job will change.
    pub fn debt(&self) -> usize {
        let merge = self.merges.iter().fold(0usize, |sum, m| {
            m.a.keys
                .len()
                .saturating_add(m.b.keys.len())
                .saturating_sub(m.i)
                .saturating_sub(m.j)
                .saturating_add(1)
                .saturating_add(sum)
        });
        let sort = self.sort.as_ref().map_or(0, |s| {
            // Each stage reads every entry once, a pass every byte left to scatter by (all eight
            // until counted), a tie being ordered its own passes and copy; one more to finish.
            let n = s.src.len();
            let left = |stage: Stage| match s.stage.cmp(&stage) {
                Ordering::Less => n,
                Ordering::Equal => n.saturating_sub(s.at),
                Ordering::Greater => 0,
            };
            let rekey = if s.stage == Stage::Rekey {
                s.stale.saturating_sub(s.at)
            } else {
                0
            };
            let scatter = match s.stage {
                Stage::Rekey | Stage::Count => n.saturating_mul(8),
                Stage::Scatter => s
                    .npass
                    .saturating_sub(s.pass)
                    .saturating_mul(n)
                    .saturating_sub(s.at),
                Stage::Ties | Stage::Dedupe => 0,
            };
            let group = s.group.as_ref().map_or(0, |g| {
                let len = g.hi.saturating_sub(g.lo);
                let passes = usize::try_from(
                    usize::BITS
                        .saturating_sub(len.saturating_sub(1).leading_zeros())
                        .saturating_sub(g.width.trailing_zeros()),
                )
                .unwrap_or(0);
                passes
                    .saturating_add(1)
                    .saturating_mul(len)
                    .saturating_sub(g.at)
                    .saturating_sub(g.copied)
            });
            rekey
                .saturating_add(left(Stage::Count))
                .saturating_add(scatter)
                .saturating_add(left(Stage::Ties))
                .saturating_add(group)
                .saturating_add(left(Stage::Dedupe))
                .saturating_add(1)
        });
        // A closed memtable's tail is sorted once the sort in progress ends: a count, eight
        // passes at most, the tie scan and the dedupe, each reading every entry.
        let tail = if self.closed && self.sort.is_some() {
            sort_reads(self.tail.len())
        } else {
            0
        };
        merge.saturating_add(sort).saturating_add(tail)
    }

    /// Pays up to `budget` entries of the order work owed; the work done.
    pub fn pay(&mut self, budget: usize) -> usize {
        let mut done = 0usize;
        // Each round moves an entry or finishes a job: at most the budget and the debt.
        while done < budget {
            if let Some(s) = self.sort.as_mut() {
                let (finished, moved) = sort_step(&self.arena, s, budget.saturating_sub(done));
                done = done.saturating_add(moved.max(1));
                if finished {
                    self.end_sort();
                }
                continue;
            }
            // The lowest level's merge first: the cheapest to finish.
            let Some(i) = (0..self.merges.len())
                .min_by_key(|&i| self.merges.get(i).map_or(u32::MAX, |m| m.level))
            else {
                break;
            };
            let Some(m) = self.merges.get_mut(i) else {
                break;
            };
            let (finished, moved) = merge_step(&self.arena, m, budget.saturating_sub(done));
            done = done.saturating_add(moved.max(1));
            if finished {
                self.end_merge(i);
                self.schedule();
            }
        }
        done
    }

    /// The runs a walk merges, each with its skip: every run, and the inputs of every merge in
    /// progress.
    fn sources(&self) -> impl Iterator<Item = (usize, &[Keyed])> {
        let merging = self.merges.iter().flat_map(|m| {
            [
                (m.a.skip, m.a.keys.as_slice()),
                (m.b.skip, m.b.keys.as_slice()),
            ]
        });
        self.runs
            .iter()
            .map(|r| (r.skip, r.keys.as_slice()))
            .chain(merging)
    }

    /// Every entry in key order: `each(key, op, value)`. Entries in the tail are not walked:
    /// [`Self::seal`] first.
    pub fn walk(
        &self,
        each: impl FnMut(&[u8], Op, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut walk = self.walk_start();
        self.walk_some(&mut walk, usize::MAX, each).map(|_| ())
    }

    /// A walk at the first entry, for [`Self::walk_some`].
    pub fn walk_start(&self) -> Walk {
        let mut walk = Walk::default();
        walk.at.extend(self.sources().map(|_| 0usize));
        self.heapify(&mut walk);
        walk
    }

    /// A walk at the first entry whose key is at least `from`: a seek.
    pub fn walk_from(&self, from: &[u8]) -> Result<Walk, Error> {
        let mut walk = Walk::default();
        self.walk_from_into(from, &mut walk)?;
        Ok(walk)
    }

    /// [`Self::walk_from`] into `walk`, its allocation kept. Over more than one run, a seek into
    /// a merged range walks it alone, and the walk records what it merges from the runs.
    pub fn walk_from_into(&self, from: &[u8], walk: &mut Walk) -> Result<(), Error> {
        walk.hot = None;
        walk.watch = None;
        walk.record.clear();
        walk.stretches.clear();
        walk.run_seeks = 0;
        walk.run_steps = 0;
        walk.recording = self.sources().nth(1).is_some();
        if walk.recording && self.hot_root != NIL {
            match self.hot_floor(from) {
                Some(i) => {
                    let hi = self.hot_hi(i);
                    if hi.is_some_and(|hi| from <= hi) {
                        let at = self.hot.get(ix(i)).map_or(0, |h| {
                            seg(&self.hot_pool, h).partition_point(|k| self.key(k.entry) < from)
                        });
                        walk.hot = Some((i, at));
                        return Ok(());
                    }
                    let next = self.hot_next(i);
                    if self.hot.get(ix(i)).is_some_and(|h| h.joins_next)
                        && let Some(n) = next
                    {
                        walk.hot = Some((n, 0));
                        return Ok(());
                    }
                    walk.watch = next;
                }
                None => walk.watch = self.hot_first(),
            }
        }
        walk.at.clear();
        for (skip, run) in self.sources() {
            walk.at.push(self.seek_run(run, skip, from));
        }
        self.heapify(walk);
        walk.run_seeks = 1;
        walk.stretches.push(Stretch {
            start: 0,
            after: None,
            joins: false,
        });
        Ok(())
    }

    /// Moves `walk` onto the runs past `after`, the last key of merged range `i`: its record
    /// continues that range.
    fn walk_runs_after(&self, walk: &mut Walk, i: u32, after: &[u8]) {
        walk.at.clear();
        for (skip, run) in self.sources() {
            let at = self.seek_run(run, skip, after);
            // A run holds a key once: past it if this is it.
            let past = run.get(at).is_some_and(|k| self.key(k.entry) == after);
            walk.at.push(if past { at.saturating_add(1) } else { at });
        }
        self.heapify(walk);
        walk.hot = None;
        walk.watch = self.hot_next(i);
        walk.run_seeks = walk.run_seeks.saturating_add(1);
        walk.stretches.push(Stretch {
            start: walk.record.len(),
            after: Some(i),
            joins: false,
        });
    }

    /// Segment `i`'s last key.
    fn hot_hi(&self, i: u32) -> Option<&[u8]> {
        self.hot
            .get(ix(i))
            .and_then(|h| seg(&self.hot_pool, h).last())
            .map(|k| self.key(k.entry))
    }

    /// Segment `i`'s first key.
    fn hot_first_key(&self, i: u32) -> &[u8] {
        hot_lo(&self.arena, &self.hot_pool, &self.hot, i)
    }

    /// The segment whose first key is the greatest not above `k`.
    fn hot_floor(&self, k: &[u8]) -> Option<u32> {
        let (mut t, mut best) = (self.hot_root, None);
        while let Some(n) = self.hot.get(ix(t)) {
            if self.hot_first_key(t) <= k {
                best = Some(t);
                t = n.right;
            } else {
                t = n.left;
            }
        }
        best
    }

    /// The segment after segment `i`, by first key.
    fn hot_next(&self, i: u32) -> Option<u32> {
        let k = self.hot_first_key(i);
        let (mut t, mut best) = (self.hot_root, None);
        while let Some(n) = self.hot.get(ix(t)) {
            if self.hot_first_key(t) > k {
                best = Some(t);
                t = n.left;
            } else {
                t = n.right;
            }
        }
        best
    }

    /// The segment before segment `i`, by first key.
    fn hot_prev(&self, i: u32) -> Option<u32> {
        let k = self.hot_first_key(i);
        let (mut t, mut best) = (self.hot_root, None);
        while let Some(n) = self.hot.get(ix(t)) {
            if self.hot_first_key(t) < k {
                best = Some(t);
                t = n.right;
            } else {
                t = n.left;
            }
        }
        best
    }

    /// The first segment by first key.
    fn hot_first(&self) -> Option<u32> {
        let (mut t, mut best) = (self.hot_root, None);
        while let Some(n) = self.hot.get(ix(t)) {
            best = Some(t);
            t = n.left;
        }
        best
    }

    /// A write of `key`: the segment holding it no longer holds its newest entry and goes, and
    /// the segment before no longer joins the next with no key between.
    fn unmerge(&mut self, key: &[u8]) {
        let Some(i) = self.hot_floor(key) else {
            return;
        };
        if self.hot_hi(i).is_some_and(|hi| key <= hi) {
            if let Some(before) = self.hot_prev(i).and_then(|b| self.hot.get_mut(ix(b))) {
                before.joins_next = false;
            }
            self.hot_remove(i);
        } else if let Some(h) = self.hot.get_mut(ix(i)) {
            h.joins_next = false;
        }
    }

    /// Takes segment `i` out of the treap and the append order, its node to the free list.
    fn hot_remove(&mut self, i: u32) {
        let k = hot_lo(&self.arena, &self.hot_pool, &self.hot, i);
        // A walk past the slab's size is no treap: the cache goes whole, the runs being the data.
        let Some((below, rest)) = split(
            &self.arena,
            &self.hot_pool,
            &mut self.hot,
            self.hot_root,
            k,
            false,
        ) else {
            self.drop_hot();
            return;
        };
        let Some((_, above)) = split(&self.arena, &self.hot_pool, &mut self.hot, rest, k, true)
        else {
            self.drop_hot();
            return;
        };
        let Some(root) = join(&mut self.hot, below, above) else {
            self.drop_hot();
            return;
        };
        self.hot_root = root;
        let (older, newer, len) = self
            .hot
            .get(ix(i))
            .map_or((NIL, NIL, 0), |h| (h.older, h.newer, h.len));
        match self.hot.get_mut(ix(older)) {
            Some(o) => o.newer = newer,
            None => self.hot_oldest = newer,
        }
        match self.hot.get_mut(ix(newer)) {
            Some(n) => n.older = older,
            None => self.hot_newest = older,
        }
        self.hot_live = self.hot_live.saturating_sub(len);
        if let Some(h) = self.hot.get_mut(ix(i)) {
            *h = Hot::empty();
        }
        self.hot_free.push(i);
    }

    /// Drops every segment, keeping the slab, the free list and the pool's buffers.
    fn drop_hot(&mut self) {
        if self.hot_root == NIL && self.hot_free.len() == self.hot.len() {
            return;
        }
        self.hot_free.clear();
        for (i, h) in self.hot.iter_mut().enumerate().rev() {
            *h = Hot::empty();
            self.hot_free.push(u32::try_from(i).unwrap_or(NIL));
        }
        self.hot_root = NIL;
        self.hot_pool.clear();
        self.hot_live = 0;
        self.hot_oldest = NIL;
        self.hot_newest = NIL;
    }

    /// Moves the live segments' entries down the pool in their append order, closing the holes
    /// dropped segments left.
    fn hot_compact(&mut self) {
        let mut write = 0usize;
        let mut t = self.hot_oldest;
        while let Some(h) = self.hot.get_mut(ix(t)) {
            if h.start != write {
                let end = h.start.saturating_add(h.len);
                self.hot_pool.copy_within(h.start..end, write);
                h.start = write;
            }
            write = write.saturating_add(h.len);
            t = h.newer;
        }
        self.hot_pool.truncate(write);
    }

    /// A new segment of `keys`, appended to the pool and ordered in the treap: `None`, adding
    /// nothing, when the pool would pass twice the memtable's entries with every hole closed, or
    /// the slab its index space.
    fn hot_add(&mut self, keys: &[Keyed], joins_next: bool) -> Option<u32> {
        let bound = self.len.saturating_mul(2);
        if self.hot_pool.len().saturating_add(keys.len()) > bound {
            self.hot_compact();
            if self.hot_pool.len().saturating_add(keys.len()) > bound {
                return None;
            }
        }
        let id = match self.hot_free.pop() {
            Some(id) => id,
            None => {
                let id = u32::try_from(self.hot.len()).ok().filter(|&id| id != NIL)?;
                self.hot.push(Hot::empty());
                id
            }
        };
        let start = self.hot_pool.len();
        self.hot_pool.extend_from_slice(keys);
        self.hot_live = self.hot_live.saturating_add(keys.len());
        let newest = self.hot_newest;
        if let Some(h) = self.hot.get_mut(ix(id)) {
            *h = Hot {
                start,
                len: keys.len(),
                joins_next,
                older: newest,
                ..Hot::empty()
            };
        }
        match self.hot.get_mut(ix(newest)) {
            Some(n) => n.newer = id,
            None => self.hot_oldest = id,
        }
        self.hot_newest = id;
        let k = hot_lo(&self.arena, &self.hot_pool, &self.hot, id);
        // A walk past the slab's size is no treap: the cache goes whole, adding nothing.
        let Some((below, above)) = split(
            &self.arena,
            &self.hot_pool,
            &mut self.hot,
            self.hot_root,
            k,
            false,
        ) else {
            self.drop_hot();
            return None;
        };
        let Some(root) = join(&mut self.hot, below, id).and_then(|l| join(&mut self.hot, l, above))
        else {
            self.drop_hot();
            return None;
        };
        self.hot_root = root;
        Some(id)
    }

    /// Keeps what `walk`, a walk of this memtable unchanged since its seek, merged from the runs
    /// as segments: a stretch continuing a segment joins it, any other starts a range.
    pub fn adopt(&mut self, walk: &mut Walk) {
        if !walk.recording {
            return;
        }
        if self.sources().nth(1).is_none() {
            self.drop_hot();
            return;
        }
        let mut end = walk.record.len();
        for s in walk.stretches.iter().rev() {
            let keys = walk.record.get(s.start..end).unwrap_or(&[]);
            end = s.start;
            if keys.is_empty() {
                // Nothing between the segment the walk left and the one it reached.
                if let Some(h) = s.after.and_then(|i| self.hot.get_mut(ix(i))) {
                    h.joins_next = s.joins;
                }
                continue;
            }
            if self.hot_add(keys, s.joins).is_some()
                && let Some(h) = s.after.and_then(|i| self.hot.get_mut(ix(i)))
            {
                // The walk left that segment for the key right after its last.
                h.joins_next = true;
            }
        }
        walk.record.clear();
        walk.stretches.clear();
        walk.recording = false;
    }

    /// Source `i` of [`Self::sources`], indexed.
    fn source(&self, i: usize) -> Option<(usize, &[Keyed])> {
        match self.runs.get(i) {
            Some(r) => Some((r.skip, r.keys.as_slice())),
            None => {
                let j = i.checked_sub(self.runs.len())?;
                let m = self.merges.get(j / 2)?;
                let r = if j % 2 == 0 { &m.a } else { &m.b };
                Some((r.skip, r.keys.as_slice()))
            }
        }
    }

    /// Source `i`'s next entry in a walk at `at`, and its run's skip.
    fn head(&self, at: &[usize], i: usize) -> Option<(Keyed, usize)> {
        let (skip, run) = self.source(i)?;
        run.get(*at.get(i)?).map(|&k| (k, skip))
    }

    /// Whether source `a`'s next entry comes before source `b`'s.
    fn before(&self, at: &[usize], a: usize, b: usize) -> bool {
        match (self.head(at, a), self.head(at, b)) {
            (Some((x, sx)), Some((y, sy))) => self.cmp(x, sx, y, sy) == Ordering::Less,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Restores the heap below position `i`.
    fn sift(&self, at: &[usize], heap: &mut [usize], mut i: usize) {
        loop {
            let l = i.saturating_mul(2).saturating_add(1);
            let r = l.saturating_add(1);
            let mut least = i;
            for c in [l, r] {
                if let (Some(&x), Some(&y)) = (heap.get(c), heap.get(least))
                    && self.before(at, x, y)
                {
                    least = c;
                }
            }
            if least == i {
                return;
            }
            heap.swap(i, least);
            i = least;
        }
    }

    /// Builds the walk's heap of the sources with entries left, least next entry first.
    fn heapify(&self, walk: &mut Walk) {
        walk.heap.clear();
        let at = &walk.at;
        walk.heap
            .extend((0..at.len()).filter(|&i| self.head(at, i).is_some()));
        for i in (0..walk.heap.len() / 2).rev() {
            self.sift(&walk.at, &mut walk.heap, i);
        }
    }

    /// Moves the heap's least source past its next entry.
    fn advance(&self, walk: &mut Walk) {
        let Some(&top) = walk.heap.first() else {
            return;
        };
        if let Some(a) = walk.at.get_mut(top) {
            *a = a.saturating_add(1);
        }
        if self.head(&walk.at, top).is_none() {
            walk.heap.swap_remove(0);
        }
        self.sift(&walk.at, &mut walk.heap, 0);
    }

    /// Where `from` falls in `run`, a run at `skip`: its first entry whose key is at least `from`.
    fn seek_run(&self, run: &[Keyed], skip: usize, from: &[u8]) -> usize {
        // Every key of the run starts with the first key's first `skip` bytes: unless `from`
        // starts with them too, it falls before or after the whole run.
        let shared = self.first.get(..skip).unwrap_or(&self.first);
        match from.get(..skip).unwrap_or(from).cmp(shared) {
            Ordering::Less => return 0,
            Ordering::Greater => return run.len(),
            Ordering::Equal => {}
        }
        let p = prefix(from, skip);
        run.partition_point(|k| k.prefix < p || (k.prefix == p && self.key(k.entry) < from))
    }

    /// The next `limit` entries of `walk` in key order, `each(key, op, value)`; the entries
    /// visited, fewer than `limit` only at the end. Of equal keys in several runs the newest is
    /// visited. A walk is resumed only over the memtable it started on, unchanged since.
    pub fn walk_some(
        &self,
        walk: &mut Walk,
        limit: usize,
        mut each: impl FnMut(&[u8], Op, &[u8]) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        self.walk_some_hashed(walk, limit, |key, op, value, _| each(key, op, value))
    }

    /// The shard's packing walk, with the high bits of each entry's hash from its insert.
    pub(crate) fn walk_some_hashed(
        &self,
        walk: &mut Walk,
        limit: usize,
        mut each: impl FnMut(&[u8], Op, &[u8], u32) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        let mut visited = 0usize;
        while visited < limit {
            if let Some((i, at)) = walk.hot {
                let Some(h) = self.hot.get(ix(i)) else {
                    return Err(corrupt());
                };
                if let Some(&e) = seg(&self.hot_pool, h).get(at) {
                    walk.hot = Some((i, at.saturating_add(1)));
                    let (key, op, v) = self.read(e.entry)?;
                    each(key, op, v, e.hash32)?;
                    visited = visited.saturating_add(1);
                    continue;
                }
                if h.joins_next
                    && let Some(next) = self.hot_next(i)
                {
                    walk.hot = Some((next, 0));
                    continue;
                }
                let Some(last) = seg(&self.hot_pool, h).last() else {
                    return Err(corrupt());
                };
                let after = self.key(last.entry);
                self.walk_runs_after(walk, i, after);
                continue;
            }
            // The least key among the runs' next entries, and its newest entry: every run's
            // entry of that key is passed.
            let Some((mut e, mut se)) = walk.heap.first().and_then(|&t| self.head(&walk.at, t))
            else {
                break;
            };
            // The walk reached a merged range: it walks the range, and the stretch recorded ends
            // where the range starts.
            if let Some(w) = walk.watch
                && self.key(e.entry) >= self.hot_first_key(w)
            {
                walk.hot = Some((w, 0));
                walk.watch = None;
                if let Some(s) = walk.stretches.last_mut() {
                    s.joins = true;
                }
                continue;
            }
            self.advance(walk);
            while let Some((k, sk)) = walk.heap.first().and_then(|&t| self.head(&walk.at, t)) {
                if self.cmp(k, sk, e, se) != Ordering::Equal {
                    break;
                }
                if k.entry > e.entry {
                    (e, se) = (k, sk);
                }
                self.advance(walk);
            }
            if walk.recording {
                walk.record.push(e);
            }
            walk.run_steps = walk.run_steps.saturating_add(1);
            let (key, op, v) = self.read(e.entry)?;
            each(key, op, v, e.hash32)?;
            visited = visited.saturating_add(1);
        }
        Ok(visited)
    }
}

/// Up to `budget` entries of sort `s` over `arena`: whether it is done (its run in `src`),
/// and the work.
fn sort_step(arena: &[u8], s: &mut Sort, budget: usize) -> (bool, usize) {
    let n = s.src.len();
    let mut moved = 0usize;
    while moved < budget {
        match s.stage {
            Stage::Rekey => {
                let Some(k) = s.src.get_mut(s.at).filter(|_| s.at < s.stale) else {
                    s.stage = Stage::Count;
                    s.at = 0;
                    continue;
                };
                k.prefix = prefix(key(arena, k.entry), s.skip);
            }
            Stage::Count => {
                let Some(k) = s.src.get(s.at) else {
                    // A byte with one value across the tail orders nothing: no pass.
                    let all = u32::try_from(n).unwrap_or(u32::MAX);
                    s.npass = 0;
                    for byte in (0..8).rev() {
                        let varies = s
                            .counts
                            .get(byte)
                            .is_some_and(|c| c.iter().all(|&m| m != all));
                        if varies && let Some(p) = s.passes.get_mut(s.npass) {
                            *p = byte;
                            s.npass = s.npass.saturating_add(1);
                        }
                    }
                    s.stage = Stage::Scatter;
                    s.pass = 0;
                    s.at = 0;
                    s.start_pass();
                    continue;
                };
                let bytes = k.prefix.to_be_bytes();
                for (counts, &b) in s.counts.iter_mut().zip(bytes.iter()) {
                    if let Some(m) = counts.get_mut(usize::from(b)) {
                        *m = m.saturating_add(1);
                    }
                }
            }
            Stage::Scatter => {
                let Some(&byte) = s.passes.get(s.pass).filter(|_| s.pass < s.npass) else {
                    s.stage = Stage::Ties;
                    s.lo = 0;
                    s.at = 0;
                    continue;
                };
                let Some(&e) = s.src.get(s.at) else {
                    std::mem::swap(&mut s.src, &mut s.dst);
                    s.pass = s.pass.saturating_add(1);
                    s.at = 0;
                    s.start_pass();
                    continue;
                };
                if let Some(next) = s.next.get_mut(digit(e.prefix, byte)) {
                    if let Some(d) = usize::try_from(*next).ok().and_then(|o| s.dst.get_mut(o)) {
                        *d = e;
                    }
                    *next = next.saturating_add(1);
                }
            }
            Stage::Ties => {
                if let Some(mut g) = s.group.take() {
                    let (done, m) = group_step(arena, s, &mut g, budget.saturating_sub(moved));
                    moved = moved.saturating_add(m);
                    if !done {
                        s.group = Some(g);
                    }
                    continue;
                }
                // Entries `[lo, at)` share a prefix: the scan extends the run while the next
                // shares it too, then orders it by key if it holds more than one.
                let next = s.src.get(s.at).map(|e| e.prefix);
                let first = s.src.get(s.lo).map(|e| e.prefix);
                if let Some(p) = next
                    && (s.at == s.lo || first == Some(p))
                {
                    s.at = s.at.saturating_add(1);
                    moved = moved.saturating_add(1);
                    continue;
                }
                if s.at.saturating_sub(s.lo) >= 2 {
                    s.group = Some(Group {
                        lo: s.lo,
                        hi: s.at,
                        width: 1,
                        at: 0,
                        i: 0,
                        j: 0,
                        flip: false,
                        copied: 0,
                    });
                }
                if next.is_none() && s.group.is_none() {
                    s.stage = Stage::Dedupe;
                    s.at = 0;
                    s.kept = 0;
                }
                s.lo = s.at;
                continue;
            }
            Stage::Dedupe => {
                let Some(e) = s.src.get(s.at).copied() else {
                    s.src.truncate(s.kept);
                    return (true, moved);
                };
                let last = s.kept.checked_sub(1).and_then(|l| s.src.get(l).copied());
                match last {
                    Some(l) if cmp(arena, l, s.skip, e, s.skip) == Ordering::Equal => {
                        if let Some(l) = s.kept.checked_sub(1).and_then(|l| s.src.get_mut(l))
                            && e.entry > l.entry
                        {
                            *l = e;
                        }
                    }
                    _ => {
                        if let Some(k) = s.src.get_mut(s.kept) {
                            *k = e;
                        }
                        s.kept = s.kept.saturating_add(1);
                    }
                }
            }
        }
        s.at = s.at.saturating_add(1);
        moved = moved.saturating_add(1);
    }
    (false, moved)
}

/// Up to `budget` entries of ordering tie `g` of `s`; whether it is ordered, and the work.
fn group_step(arena: &[u8], s: &mut Sort, g: &mut Group, budget: usize) -> (bool, usize) {
    let skip = s.skip;
    let (Some(a), Some(b)) = (s.src.get_mut(g.lo..g.hi), s.dst.get_mut(g.lo..g.hi)) else {
        return (true, 0);
    };
    let n = a.len();
    let mut moved = 0usize;
    while moved < budget {
        if g.width >= n {
            if !g.flip {
                return (true, moved);
            }
            // Sorted in `dst`: back into place.
            let (Some(x), Some(y)) = (b.get(g.copied), a.get_mut(g.copied)) else {
                return (true, moved);
            };
            *y = *x;
            g.copied = g.copied.saturating_add(1);
            moved = moved.saturating_add(1);
            continue;
        }
        if g.at >= n {
            // A pass done: the merged runs are the next pass's source, twice as wide.
            g.flip = !g.flip;
            g.width = g.width.saturating_mul(2);
            g.at = 0;
            g.i = 0;
            g.j = 0;
            continue;
        }
        let (from, to): (&[Keyed], &mut [Keyed]) = if g.flip {
            (&*b, &mut *a)
        } else {
            (&*a, &mut *b)
        };
        let lo = g.at;
        let mid = lo.saturating_add(g.width).min(n);
        let hi = mid.saturating_add(g.width).min(n);
        let (x, y) = (lo.saturating_add(g.i), mid.saturating_add(g.j));
        let next = match (from.get(x).copied(), from.get(y).copied()) {
            (Some(l), Some(r)) if x < mid && y < hi => {
                if order(arena, l, r, skip) != Ordering::Greater {
                    g.i = g.i.saturating_add(1);
                    l
                } else {
                    g.j = g.j.saturating_add(1);
                    r
                }
            }
            (Some(l), _) if x < mid => {
                g.i = g.i.saturating_add(1);
                l
            }
            (_, Some(r)) if y < hi => {
                g.j = g.j.saturating_add(1);
                r
            }
            _ => {
                // The pair is merged: the next pair.
                g.at = hi;
                g.i = 0;
                g.j = 0;
                continue;
            }
        };
        if let Some(d) = to.get_mut(lo.saturating_add(g.i).saturating_add(g.j).saturating_sub(1)) {
            *d = next;
        }
        moved = moved.saturating_add(1);
    }
    (false, moved)
}

/// Up to `budget` entries of merge `m` over `arena`: whether it is done (its run in `out`),
/// and the work.
fn merge_step(arena: &[u8], m: &mut Merge, budget: usize) -> (bool, usize) {
    let (sa, sb) = (m.a.skip, m.b.skip);
    let mut moved = 0usize;
    loop {
        let (x, y) = (m.a.keys.get(m.i).copied(), m.b.keys.get(m.j).copied());
        if (x.is_some() || y.is_some()) && moved >= budget {
            break;
        }
        let (mut next, from) = match (x, y) {
            (Some(x), Some(y)) => match cmp(arena, x, sa, y, sb) {
                Ordering::Less => {
                    m.i = m.i.saturating_add(1);
                    (x, sa)
                }
                Ordering::Greater => {
                    m.j = m.j.saturating_add(1);
                    (y, sb)
                }
                Ordering::Equal => {
                    // The same key in both: the newer entry only.
                    m.i = m.i.saturating_add(1);
                    m.j = m.j.saturating_add(1);
                    if x.entry > y.entry { (x, sa) } else { (y, sb) }
                }
            },
            (Some(x), None) => {
                m.i = m.i.saturating_add(1);
                (x, sa)
            }
            (None, Some(y)) => {
                m.j = m.j.saturating_add(1);
                (y, sb)
            }
            (None, None) => return (true, moved),
        };
        if from != m.out.skip {
            next.prefix = prefix(key(arena, next.entry), m.out.skip);
        }
        m.out.keys.push(next);
        moved = moved.saturating_add(1);
    }
    (false, moved)
}

/// A walk's place in a memtable ([`HashMem::walk_some`]): each run's next entry, and a heap of
/// the runs with entries left by their next entry, so a step compares logarithmically many.
#[derive(Debug, Default)]
pub struct Walk {
    at: Vec<usize>,
    heap: Vec<usize>,
    /// In a merged range ([`HashMem::hot`]): which, and its next entry; else the runs' heap.
    hot: Option<(u32, usize)>,
    /// The merged range the walk over the runs reaches next, to walk it instead.
    watch: Option<u32>,
    /// Whether the walk records what it merges from the runs, for [`HashMem::adopt`]: a seek's
    /// walk over more than one run.
    recording: bool,
    /// The newest entries walked from the runs, in key order, and the stretches they make
    /// ([`Stretch`]).
    record: Vec<Keyed>,
    stretches: Vec<Stretch>,
    /// Seeks of the runs and steps of their heap: what the runs being many cost the walk.
    run_seeks: u64,
    run_steps: u64,
}

/// A stretch of a walk's record: where it starts in the record, the merged range it continues
/// with no key between, and whether it ends where the next merged range starts.
#[derive(Clone, Copy, Debug)]
struct Stretch {
    start: usize,
    after: Option<u32>,
    joins: bool,
}

impl Walk {
    /// Seeks and heap steps of the runs since the walk's seek ([`HashMem::walk_from_into`]).
    pub fn run_work(&self) -> (u64, u64) {
        (self.run_seeks, self.run_steps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    proptest::proptest! {
        /// Against a `BTreeMap`: puts, deletes and repeated keys, with seals, paced order work
        /// and sorts between them, give the newest entry for every key, and every walk, from the
        /// start or from any key, gives the oracle's range in order.
        #[test]
        fn the_memtable_is_an_ordered_map_of_newest_entries(
            shared in 0usize..12,
            ops in proptest::collection::vec(
                (proptest::collection::vec(0u8..6, 0..6), proptest::prelude::any::<bool>(), proptest::collection::vec(proptest::prelude::any::<u8>(), 0..20), 0u8..20, 0usize..64),
                0..1500,
            ),
        ) {
            // Keys share a common prefix of `shared` bytes, a few a shorter one, so the bytes
            // keys share shrink mid-fill and keys tie past their eight-byte prefixes.
            let mut m = HashMem::new(1 << 24).unwrap();
            let mut oracle: BTreeMap<Vec<u8>, (Op, Vec<u8>)> = BTreeMap::new();
            for (suffix, put, v, action, cut) in &ops {
                let mut k = vec![9u8; if *cut < 12 { shared.min(*cut) } else { shared }];
                k.extend_from_slice(suffix);
                let k = &k;
                let op = if *put { Op::Put } else { Op::Delete };
                let v = if *put { v.clone() } else { Vec::new() };
                m.insert(k, op, &v).unwrap();
                oracle.insert(k.clone(), (op, v));
                match action {
                    0 => m.seal(),
                    1 => m.start_sort(),
                    2..=5 => { m.pay(usize::from(*action)); }
                    6..=9 => { m.order_put(1, usize::from(*action) - 5, usize::MAX); }
                    _ => {}
                }
            }
            proptest::prop_assert_eq!(m.len(), oracle.len());
            let mut value = Vec::new();
            for (k, (op, v)) in &oracle {
                proptest::prop_assert_eq!(m.get(k, &mut value).unwrap(), Some(*op));
                proptest::prop_assert_eq!(&value, v);
            }
            m.seal();
            let expected: Vec<_> = oracle.into_iter().collect();
            let mut walked = Vec::new();
            m.walk(|k, op, v| { walked.push((k.to_vec(), (op, v.to_vec()))); Ok(()) }).unwrap();
            proptest::prop_assert_eq!(&walked, &expected);
            let probes: Vec<Vec<u8>> = expected
                .iter()
                .flat_map(|(k, _)| {
                    let mut past = k.clone();
                    past.push(0);
                    [k.clone(), past]
                })
                .chain([Vec::new(), vec![0xFF; 7]])
                .collect();
            // A seek lands on the first key at least `from`, and the next few follow in order; the
            // whole walk is checked above.
            for from in &probes {
                let mut walk = m.walk_from(from).unwrap();
                let mut got = Vec::new();
                m.walk_some(&mut walk, 3, |k, op, v| {
                    got.push((k.to_vec(), (op, v.to_vec())));
                    Ok(())
                }).unwrap();
                let want: Vec<_> =
                    expected.iter().filter(|(k, _)| k >= from).take(3).cloned().collect();
                proptest::prop_assert_eq!(&got, &want, "from {:?}", from);
            }
            for limit in 1..6usize {
                let mut walk = m.walk_start();
                let mut sliced = Vec::new();
                loop {
                    let n = m.walk_some(&mut walk, limit, |k, op, v| {
                        sliced.push((k.to_vec(), (op, v.to_vec())));
                        Ok(())
                    }).unwrap();
                    if n < limit { break; }
                }
                proptest::prop_assert_eq!(&sliced, &expected);
            }
        }
    }

    #[test]
    fn no_debt_means_no_job_left_to_change_the_runs() {
        // A merge or sort that has moved every entry still owes its finish: a walk opened when the
        // debt reads nothing must not see its runs change after.
        let mut m = HashMem::new(1 << 20).unwrap();
        for round in 0..200u32 {
            for i in 0..(round % 17 + 1) {
                m.insert(&(round * 31 + i).to_be_bytes(), Op::Put, b"v")
                    .unwrap();
            }
            if round % 3 == 0 {
                m.seal();
            }
            if round % 5 == 0 {
                m.start_sort();
            }
            // Paid a step at a time: whenever the debt reads nothing, no job is open.
            for _ in 0..50 {
                if m.debt() == 0 {
                    assert!(m.merges.is_empty() && m.sort.is_none(), "round {round}");
                    break;
                }
                m.pay(1);
            }
        }
    }

    #[test]
    fn however_often_scans_seal_a_walk_merges_logarithmically_many_runs() {
        // Seals of one entry each, nothing paid between them but what a seal pays: a walk merges
        // at most two runs a level and the two inputs of the level's merge.
        let mut m = HashMem::new(1 << 24).unwrap();
        for i in 0..20_000u32 {
            m.insert(&i.wrapping_mul(2_654_435_761).to_be_bytes(), Op::Put, b"v")
                .unwrap();
            m.seal();
            let levels = usize::try_from(usize::BITS - m.len().leading_zeros()).unwrap();
            assert!(
                m.sources().count() <= 4 * levels,
                "{} runs at {i}",
                m.sources().count()
            );
        }
    }

    #[test]
    fn a_memtable_ordered_as_it_fills_leaves_a_scan_at_most_two_chunks() {
        // Each put pays its order work: however many puts, the tail and the sort in progress
        // each hold at most twice the bound, so a seal sorts at most that much.
        let bound = 64;
        let mut m = HashMem::new(1 << 24).unwrap();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for i in 0..50_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x % 20_000).to_be_bytes();
            m.insert(&k, Op::Put, &i.to_le_bytes()).unwrap();
            want.insert(k.to_vec(), i.to_le_bytes().to_vec());
            m.order_put(1, bound, usize::MAX);
            assert!(m.tail.len() <= 2 * bound, "tail {} at {i}", m.tail.len());
            let open = m.sort.as_ref().map_or(0, |s| s.src.len());
            assert!(open <= 2 * bound, "sort of {open} at {i}");
            let levels = usize::try_from(usize::BITS - m.len().leading_zeros()).unwrap();
            assert!(
                m.sources().count() <= 4 * levels,
                "{} runs at {i}",
                m.sources().count()
            );
        }
        m.seal();
        let mut walked = Vec::new();
        m.walk(|k, _, v| {
            walked.push((k.to_vec(), v.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(walked, want.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn a_cleared_memtable_still_counts_what_it_holds() {
        // Cleared for reuse, it keeps the arena's written pages, its index's table and its
        // buffers: what it holds is not what it now uses.
        let mut m = HashMem::new(1 << 22).unwrap();
        for i in 0..20_000u32 {
            m.insert(&i.to_be_bytes(), Op::Put, &[7; 100]).unwrap();
        }
        m.seal();
        let filled = m.bytes();
        m.clear();
        assert_eq!(m.bytes(), 0);
        assert!(m.memory() >= filled, "{} < {filled}", m.memory());
    }

    #[test]
    fn walks_through_merged_ranges_see_exactly_what_the_runs_hold() {
        // Writes, seals, idle tidying and seeks walked a while, each walk's merging then kept:
        // every walk, through merged ranges, their joins and the runs between, gives the newest
        // entry of each key in order, and some walks run in merged ranges alone.
        let mut m = HashMem::new(1 << 22).unwrap();
        let mut want: BTreeMap<Vec<u8>, (Op, Vec<u8>)> = BTreeMap::new();
        let mut walk = Walk::default();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let (mut walks, mut merged_only) = (0u32, 0u32);
        for round in 0..20_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x % 2_000).to_be_bytes();
            match x % 16 {
                0..=7 => {
                    m.insert(&k, Op::Put, &round.to_le_bytes()).unwrap();
                    want.insert(k.to_vec(), (Op::Put, round.to_le_bytes().to_vec()));
                }
                8 => {
                    m.insert(&k, Op::Delete, &[]).unwrap();
                    want.insert(k.to_vec(), (Op::Delete, Vec::new()));
                }
                9 => m.seal(),
                10 => {
                    m.tidy(7);
                }
                _ => {
                    m.seal();
                    let from = ((x >> 20) % 2_100).to_be_bytes();
                    let limit = 1 + ((x >> 40) % 300) as usize;
                    m.walk_from_into(&from, &mut walk).unwrap();
                    let mut got = Vec::new();
                    m.walk_some(&mut walk, limit, |k, op, v| {
                        got.push((k.to_vec(), op, v.to_vec()));
                        Ok(())
                    })
                    .unwrap();
                    let expect: Vec<_> = want
                        .range(from.to_vec()..)
                        .take(limit)
                        .map(|(k, (op, v))| (k.clone(), *op, v.clone()))
                        .collect();
                    assert_eq!(got, expect, "round {round} from {from:?} limit {limit}");
                    walks += 1;
                    if walk.run_work() == (0, 0) {
                        merged_only += 1;
                    }
                    m.adopt(&mut walk);
                }
            }
        }
        assert!(walks > 0 && merged_only > 0, "{walks} {merged_only}");
    }

    #[test]
    fn idle_tidying_leaves_one_run_holding_every_entry() {
        // Runs of every size, as a memtable filled alone leaves them, and a tail: tidied in
        // slices, one run is left, walked in order with the newest of each key.
        let mut m = HashMem::new(1 << 22).unwrap();
        let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for round in 0..12u32 {
            for _ in 0..(1u32 << round.min(9)) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let k = (x % 5_000).to_be_bytes();
                m.insert(&k, Op::Put, &round.to_le_bytes()).unwrap();
                want.insert(k.to_vec(), round.to_le_bytes().to_vec());
            }
            m.seal();
        }
        m.insert(b"tail", Op::Put, b"t").unwrap();
        want.insert(b"tail".to_vec(), b"t".to_vec());
        assert!(m.untidy());
        // What the rule that pays for tidying prices it at bounds the work tidying does.
        let priced = m.collapse_moves().unwrap();
        let mut slices = 0;
        let mut work = 0;
        while m.untidy() {
            let done = m.tidy(5);
            assert!(done >= 1);
            work += done;
            slices += 1;
            assert!(slices < 1_000_000);
        }
        assert!(work <= priced, "{work} > {priced}");
        assert_eq!(m.collapse_moves(), Some(0));
        assert_eq!(m.sources().count(), 1);
        let mut walked = Vec::new();
        m.walk(|k, _, v| {
            walked.push((k.to_vec(), v.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(walked, want.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn a_closed_memtable_orders_the_tail_written_during_its_last_sort() {
        // A sort began while the memtable filled; more entries arrived; closed, its debt paid,
        // the walk sees every entry.
        let mut m = HashMem::new(1 << 20).unwrap();
        for i in 0..100u32 {
            m.insert(&i.to_be_bytes(), Op::Put, b"a").unwrap();
        }
        m.start_sort();
        m.pay(5);
        for i in 100..150u32 {
            m.insert(&i.to_be_bytes(), Op::Put, b"b").unwrap();
        }
        m.close();
        while m.debt() > 0 {
            m.pay(3);
        }
        let mut walked = 0u32;
        m.walk(|k, _, _| {
            assert_eq!(k, walked.to_be_bytes());
            walked += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(walked, 150);
    }

    #[test]
    fn a_rotated_memtables_sort_is_paid_in_slices_and_walks_in_order() {
        let mut m = HashMem::new(1 << 24).unwrap();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut keys = Vec::new();
        for i in 0..5_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x % 3_000).to_be_bytes();
            m.insert(&k, Op::Put, &i.to_le_bytes()).unwrap();
            keys.push(k);
        }
        m.start_sort();
        let before = m.debt();
        assert!(before > 0);
        // Each slice pays at most its budget, and the debt falls to nothing.
        let mut slices = 0;
        while m.debt() > 0 {
            assert!(m.pay(7) <= 7 + 1);
            slices += 1;
            assert!(slices < 1_000_000);
        }
        let mut walked = Vec::new();
        m.walk(|k, _, v| {
            walked.push((k.to_vec(), v.to_vec()));
            Ok(())
        })
        .unwrap();
        let mut want: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        for (i, k) in keys.iter().enumerate() {
            want.insert(k.to_vec(), u32::try_from(i).unwrap().to_le_bytes().to_vec());
        }
        assert_eq!(walked, want.into_iter().collect::<Vec<_>>());
    }

    proptest::proptest! {
        /// A memtable's mirror sorted on its own gives exactly the memtable's walk: puts, deletes
        /// and keys written again, keys sharing fewer first bytes as the fill goes on and tying
        /// past their eight-byte prefixes, every key's newest entry once, in key order, with its
        /// filter hash.
        #[test]
        fn a_mirror_sorted_apart_gives_the_memtable_s_own_walk(
            shared in 0usize..12,
            ops in proptest::collection::vec(
                (proptest::collection::vec(0u8..6, 0..6), proptest::prelude::any::<bool>(), proptest::collection::vec(proptest::prelude::any::<u8>(), 0..20), 0usize..64),
                1..1500,
            ),
        ) {
            let mut m = HashMem::new(1 << 24).unwrap();
            m.mirror_into(Vec::new()).unwrap();
            for (suffix, put, v, cut) in &ops {
                let mut k = vec![9u8; if *cut < 12 { shared.min(*cut) } else { shared }];
                k.extend_from_slice(suffix);
                let op = if *put { Op::Put } else { Op::Delete };
                let v = if *put { v.clone() } else { Vec::new() };
                m.insert(&k, op, &v).unwrap();
            }
            let skip = m.skip();
            let arena = m.take_mirror().unwrap();
            proptest::prop_assert_eq!(&arena, &m.arena);
            proptest::prop_assert!(!m.mirrored());
            let mut sorter = Sorter::default();
            proptest::prop_assert_eq!(sorter.sort(&arena, skip).unwrap(), m.len());
            let mut sorted = Vec::new();
            sorter.each(&arena, |k, op, v, h| {
                sorted.push((k.to_vec(), op, v.to_vec(), h));
                Ok(())
            }).unwrap();
            m.seal();
            let mut walked = Vec::new();
            let mut walk = m.walk_start();
            m.walk_some_hashed(&mut walk, usize::MAX, |k, op, v, h| {
                walked.push((k.to_vec(), op, v.to_vec(), h));
                Ok(())
            }).unwrap();
            proptest::prop_assert_eq!(&sorted, &walked);
            for (k, _, _, h) in &sorted {
                proptest::prop_assert_eq!(*h, crate::branch::filter::hash32(hash(k)));
            }
            // The sorter's buffers serve the next arena as they are.
            proptest::prop_assert_eq!(sorter.sort(&arena, skip).unwrap(), m.len());
        }
    }

    #[test]
    fn a_mirror_is_taken_only_whole_and_kept_through_a_clear() {
        let mut m = HashMem::new(4096).unwrap();
        m.insert(b"a", Op::Put, b"1").unwrap();
        // Given after an entry, it would miss that entry: refused.
        assert!(m.mirror_into(Vec::new()).is_err());
        assert!(m.take_mirror().is_none());
        m.clear();
        m.mirror_into(Vec::new()).unwrap();
        assert!(m.mirror_into(Vec::new()).is_err(), "a second mirror");
        m.insert(b"b", Op::Put, b"2").unwrap();
        // A clear keeps the mirror, emptied, whole for the next fill.
        m.clear();
        assert!(m.mirrored());
        m.insert(b"c", Op::Delete, b"").unwrap();
        let arena = m.take_mirror().unwrap();
        assert_eq!(arena, m.arena);
        assert!(arena.capacity() >= 4096);
        assert!(m.take_mirror().is_none());
        // Taken, it is gone past a clear too.
        m.clear();
        assert!(!m.mirrored());
    }

    /// A memtable read through its index alone sheds its entry buffers to a new one: that one's
    /// tail holds as many entries as the first's did without growing, its walk reads every
    /// entry in key order, and the first still answers every get.
    #[test]
    fn a_memtable_sheds_its_buffers_to_a_new_one_and_still_answers_gets() {
        let mut a = HashMem::new(1 << 20).unwrap();
        let keys: Vec<Vec<u8>> = (0..2_000u32)
            .map(|n| n.wrapping_mul(2_654_435_761).to_be_bytes().to_vec())
            .collect();
        for k in &keys {
            a.insert(k, Op::Put, k).unwrap();
        }
        a.seal();
        let shed = a.shed();
        let mut b = HashMem::new(1 << 20).unwrap();
        b.take_shed(shed);
        let room = b.tail.capacity();
        assert!(
            room >= keys.len(),
            "a tail of {room} for {} entries",
            keys.len()
        );
        for k in &keys {
            b.insert(k, Op::Put, b"b").unwrap();
        }
        assert_eq!(b.tail.capacity(), room, "the tail grew");
        let mut value = Vec::new();
        for k in &keys {
            assert_eq!(a.get(k, &mut value).unwrap(), Some(Op::Put));
            assert_eq!(&value, k);
        }
        let mut sorted = keys.clone();
        sorted.sort();
        let mut walked = Vec::new();
        b.seal();
        let mut w = b.walk_start();
        b.walk_some_hashed(&mut w, usize::MAX, |k, _, v, _| {
            assert_eq!(v, b"b");
            walked.push(k.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(walked, sorted);
    }
}
