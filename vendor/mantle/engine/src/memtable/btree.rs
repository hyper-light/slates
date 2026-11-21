//! A shard's memtable (docs/design/engine-structure.md §8, step E3): a B-tree over entries kept in
//! one byte arena. A shard has one worker (step E2), so the tree takes no lock and makes no atomic
//! step; SplinterDB's memtable is a concurrent B-tree because its threads share one
//! (research/34 §1).
//!
//! Every entry is appended to the arena (key length, value length, operation, key, value) and the
//! nodes hold offsets into it: an insert allocates nothing per entry, and the arena's bound is the
//! memtable's size `m` (CLAUDE.md §2), past which an insert is refused so the shard packs the
//! memtable into a branch. A key put again is appended and its slot repointed: the newest entry
//! wins, and the bytes of the one it replaced stay in the arena until the memtable is packed.
//!
//! The tree is a B-tree of minimum degree [`DEGREE`] (every node but the root holds
//! `DEGREE − 1` to `2·DEGREE − 1` entries), split on the way down so an insert is one pass
//! (Cormen et al., *Introduction to Algorithms*, §18.2). An in-order walk gives the entries in key
//! order, what a branch's builder takes.

use crate::branch::Op;
use crate::branch::filter::{Filter, Keys, hash};
use crate::error::{Error, Malformed};
use crate::util::coding::{get_varint32_ptr, put_varint32};
use std::cmp::Ordering;

/// Measured: the tree's minimum degree, the smallest at which put and get latency stop improving
/// (benches/memtable.rs; docs/design/constants.md records the runs at 8, 16 and 32).
pub const DEGREE: usize = 16;
const MAX_KEYS: usize = 2 * DEGREE - 1;
/// The most bytes an entry's head takes in the arena: the operation (1), then the key's and the
/// value's lengths as varints of at most 3 and 5 bytes (a key is below 64 KiB, a value below
/// 4 GiB). Object-store metadata's keys and values are mostly under 128 bytes, a byte each.
const ENTRY_HEAD_MAX: usize = 9;
/// No node.
const NONE: u32 = u32::MAX;

#[derive(Clone, Debug)]
struct Node {
    len: usize,
    leaf: bool,
    /// The bytes every key the node may hold shares: the common prefix of the separators around
    /// it in its parent, 0 where a side is open (prefix truncation by fence keys; Graefe, *Modern
    /// B-Tree Techniques*, Foundations and Trends in Databases 3(4), 2011, §3.5). A search reaches
    /// a node only with a key between its fences, so the key shares the prefix too.
    prefix: usize,
    entries: [u32; MAX_KEYS],
    /// Each entry's key head past the prefix ([`head`]), so most comparisons read the node and not
    /// the arena: heads from the key's start were all equal for keys that share their first bytes
    /// (16-byte keys of numbers below 2^32, object names under one tenant).
    heads: [u32; MAX_KEYS],
    children: [u32; MAX_KEYS + 1],
}

impl Node {
    fn new(leaf: bool, prefix: usize) -> Self {
        Self {
            len: 0,
            leaf,
            prefix,
            entries: [0; MAX_KEYS],
            heads: [0; MAX_KEYS],
            children: [NONE; MAX_KEYS + 1],
        }
    }
}

/// A memtable: the arena, its bound, and the tree over it.
#[derive(Debug)]
pub struct BTreeMem {
    arena: Vec<u8>,
    limit: usize,
    nodes: Vec<Node>,
    root: u32,
    len: usize,
    /// The largest key's entry, and the leaf holding it: a key above it whose leaf has room is
    /// appended there without a descent, as RocksDB's skiplist inserts a sequence through its
    /// remembered splice.
    last: Option<u32>,
    right: u32,
    /// A filter of the keys inserted (RocksDB's `memtable_whole_key_filtering`): a get of a key
    /// the memtable lacks, at 10 M keys about 95% of gets, answers by one probe instead of a
    /// descent. Sized at each clear for the entries the last fill held, so a memtable's first
    /// fill has none; a fill past that size only raises its false positives.
    filter: Option<Filter>,
}

/// A key's head past its first `prefix` bytes ([`Node::prefix`]): the next 4 bytes, big-endian,
/// zero-padded. Heads that differ order their keys
/// as the keys order: at the first byte they differ, either both keys have bytes there, or the
/// shorter has ended (its pad, 0, below the other's byte, a key ending first being the
/// smaller). Equal heads say nothing, and the keys are compared whole.
fn head(key: &[u8], prefix: usize) -> u32 {
    let mut b = [0u8; 4];
    for (d, s) in b.iter_mut().zip(key.get(prefix..).unwrap_or(&[])) {
        *d = *s;
    }
    u32::from_be_bytes(b)
}

fn corrupt() -> Error {
    Error::Corruption {
        what: "a memtable",
        why: Malformed::OutOfRange,
    }
}

impl BTreeMem {
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
            nodes: vec![Node::new(true, 0)],
            root: 0,
            len: 0,
            last: None,
            right: 0,
            filter: None,
        })
    }

    /// Empties the memtable for its next fill, keeping its arena's and nodes' allocations: a
    /// shard reuses one memtable, where a new one each flush left the freed arenas dirty in the
    /// allocator (680 MB of them at 6 M entries, measured by vmmap on macOS).
    pub fn clear(&mut self) {
        let keys = Keys::AtMost(u64::try_from(self.len).unwrap_or(u64::MAX).max(1));
        match self.filter.as_mut() {
            Some(f) => f.reset(keys),
            None => self.filter = Some(Filter::new(keys)),
        }
        self.arena.clear();
        self.nodes.clear();
        self.nodes.push(Node::new(true, 0));
        self.root = 0;
        self.len = 0;
        self.last = None;
        self.right = 0;
    }

    /// Entries the memtable holds, each key once.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes the memtable occupies: the arena's and the nodes' used bytes, each rounded up
    /// to a 4 KiB page. A vector's capacity past its length is address space no page backs until
    /// it is written, so it is not counted, as RocksDB counts the arena blocks it has taken.
    pub fn memory(&self) -> usize {
        let page = |n: usize| n.div_ceil(4096).saturating_mul(4096);
        page(self.arena.len()).saturating_add(page(
            self.nodes.len().saturating_mul(std::mem::size_of::<Node>()),
        ))
    }

    /// Arena bytes left before the bound.
    pub fn room(&self) -> usize {
        self.limit.saturating_sub(self.arena.len())
    }

    /// Arena bytes used: what the bound is against.
    pub fn bytes(&self) -> usize {
        self.arena.len()
    }

    /// An entry's operation byte, key and value lengths, and where its key starts.
    fn parse(&self, entry: u32) -> Option<(u8, usize, usize, usize)> {
        let at = usize::try_from(entry).ok()?;
        let op = *self.arena.get(at)?;
        let mut pos = at.checked_add(1)?;
        let (klen, used) = get_varint32_ptr(self.arena.get(pos..)?).ok()?;
        pos = pos.checked_add(used)?;
        let (vlen, used) = get_varint32_ptr(self.arena.get(pos..)?).ok()?;
        pos = pos.checked_add(used)?;
        Some((
            op,
            usize::try_from(klen).ok()?,
            usize::try_from(vlen).ok()?,
            pos,
        ))
    }

    fn key(&self, entry: u32) -> &[u8] {
        self.parse(entry)
            .and_then(|(_, klen, _, start)| self.arena.get(start..start.checked_add(klen)?))
            .unwrap_or(&[])
    }

    /// An entry's operation and value.
    fn value(&self, entry: u32) -> Result<(Op, &[u8]), Error> {
        let (op, klen, vlen, start) = self.parse(entry).ok_or(corrupt())?;
        let op = match op {
            1 => Op::Put,
            2 => Op::Delete,
            _ => return Err(corrupt()),
        };
        let from = start.checked_add(klen).ok_or(corrupt())?;
        let value = self
            .arena
            .get(from..from.checked_add(vlen).ok_or(corrupt())?)
            .ok_or(corrupt())?;
        Ok((op, value))
    }

    fn node(&self, i: u32) -> Result<&Node, Error> {
        self.nodes
            .get(usize::try_from(i).map_err(|_| corrupt())?)
            .ok_or(corrupt())
    }

    fn node_mut(&mut self, i: u32) -> Result<&mut Node, Error> {
        self.nodes
            .get_mut(usize::try_from(i).map_err(|_| corrupt())?)
            .ok_or(corrupt())
    }

    /// Where `key`, between `n`'s fences, is in node `n`: `Ok(i)` at entry `i`, `Err(i)` below
    /// child `i`.
    fn search(&self, n: &Node, key: &[u8]) -> Result<usize, usize> {
        let kh = head(key, n.prefix);
        let (mut lo, mut hi) = (0usize, n.len);
        while lo < hi {
            let mid = lo.saturating_add(hi.saturating_sub(lo) / 2);
            let h = n.heads.get(mid).copied().unwrap_or(0);
            let order = h.cmp(&kh).then_with(|| {
                let e = n.entries.get(mid).copied().unwrap_or(0);
                self.key(e).cmp(key)
            });
            match order {
                Ordering::Less => lo = mid.saturating_add(1),
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// The newest entry for `key`, of filter hash `hash` ([`crate::branch::filter::hash`]): its
    /// operation and value into `value`; none at once when the filter rules the key out.
    pub fn get_hashed(
        &self,
        key: &[u8],
        hash: u64,
        value: &mut Vec<u8>,
    ) -> Result<Option<Op>, Error> {
        if self.filter.as_ref().is_some_and(|f| !f.may_contain(hash)) {
            return Ok(None);
        }
        self.get(key, value)
    }

    /// The newest entry for `key`: its operation and value into `value`.
    pub fn get(&self, key: &[u8], value: &mut Vec<u8>) -> Result<Option<Op>, Error> {
        let mut at = self.root;
        loop {
            let n = self.node(at)?;
            match self.search(n, key) {
                Ok(i) => {
                    let entry = *n.entries.get(i).ok_or(corrupt())?;
                    let (op, v) = self.value(entry)?;
                    value.clear();
                    value.extend_from_slice(v);
                    return Ok(Some(op));
                }
                Err(i) if !n.leaf => at = *n.children.get(i).ok_or(corrupt())?,
                Err(_) => return Ok(None),
            }
        }
    }

    /// Records `op` of `key` with `value` (empty for a delete). Refused, changing nothing, when
    /// the arena would pass its bound: the shard then packs the memtable into a branch.
    pub fn insert(&mut self, key: &[u8], op: Op, value: &[u8]) -> Result<(), Error> {
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
        self.arena.push(match op {
            Op::Put => 1,
            Op::Delete => 2,
        });
        put_varint32(&mut self.arena, u32::from(klen));
        put_varint32(&mut self.arena, vlen);
        self.arena.extend_from_slice(key);
        self.arena.extend_from_slice(value);
        if let Some(f) = self.filter.as_mut() {
            f.insert(hash(key));
        }
        let above = match self.last {
            None => true,
            Some(last) => key > self.key(last),
        };
        if above && self.last.is_some() {
            let leaf = self.node_mut(self.right)?;
            if leaf.len < MAX_KEYS {
                let at = leaf.len;
                let kh = head(key, leaf.prefix);
                *leaf.entries.get_mut(at).ok_or(corrupt())? = entry;
                *leaf.heads.get_mut(at).ok_or(corrupt())? = kh;
                leaf.len = at.saturating_add(1);
                self.len = self.len.saturating_add(1);
                self.last = Some(entry);
                return Ok(());
            }
        }
        let replaced = self.descend(key, entry, above)?;
        if above || (replaced && self.last.is_some_and(|l| self.key(l) == key)) {
            self.last = Some(entry);
        }
        // The rightmost leaf, after any split the descent made.
        let mut at = self.root;
        loop {
            let n = self.node(at)?;
            if n.leaf {
                break;
            }
            at = *n.children.get(n.len).ok_or(corrupt())?;
        }
        self.right = at;
        Ok(())
    }

    /// Inserts `entry` for `key` by a descent from the root, splitting full nodes on the way;
    /// true when the key was there and its slot took the entry. A key `above` every other splits
    /// the full nodes on its path lopsided ([`Self::split`]). The descent carries each node's
    /// fences, the separators around it, from which a split's halves take their prefixes.
    fn descend(&mut self, key: &[u8], entry: u32, above: bool) -> Result<bool, Error> {
        // A full root splits first: the tree grows at the top, its fences open.
        if self.node(self.root)?.len == MAX_KEYS {
            let old = self.root;
            let mut root = Node::new(false, 0);
            root.children[0] = old;
            self.root = self.push(root)?;
            self.split(self.root, 0, above, (None, None))?;
        }
        let mut at = self.root;
        let mut fences: (Option<u32>, Option<u32>) = (None, None);
        // One level a step: at most the tree's height, bounded by its nodes.
        for _ in 0..=self.nodes.len() {
            let n = self.node(at)?;
            match self.search(n, key) {
                Ok(i) => {
                    // The key is here: its slot takes the new entry (the head is the same).
                    let slot = self.node_mut(at)?.entries.get_mut(i).ok_or(corrupt())?;
                    *slot = entry;
                    return Ok(true);
                }
                Err(i) if n.leaf => {
                    let len = n.len;
                    let kh = head(key, n.prefix);
                    let node = self.node_mut(at)?;
                    node.entries.copy_within(i..len, i.saturating_add(1));
                    node.heads.copy_within(i..len, i.saturating_add(1));
                    *node.entries.get_mut(i).ok_or(corrupt())? = entry;
                    *node.heads.get_mut(i).ok_or(corrupt())? = kh;
                    node.len = len.saturating_add(1);
                    self.len = self.len.saturating_add(1);
                    return Ok(false);
                }
                Err(i) => {
                    let mut child = *n.children.get(i).ok_or(corrupt())?;
                    let mut lo = if i > 0 {
                        n.entries.get(i.saturating_sub(1)).copied()
                    } else {
                        fences.0
                    };
                    let mut hi = if i < n.len {
                        n.entries.get(i).copied()
                    } else {
                        fences.1
                    };
                    if self.node(child)?.len == MAX_KEYS {
                        self.split(at, i, above, (lo, hi))?;
                        // The median rose into slot `i`: the key goes right of it if larger, and
                        // is the median itself if equal.
                        let n = self.node(at)?;
                        let median = *n.entries.get(i).ok_or(corrupt())?;
                        match key.cmp(self.key(median)) {
                            Ordering::Equal => {
                                *self.node_mut(at)?.entries.get_mut(i).ok_or(corrupt())? = entry;
                                return Ok(true);
                            }
                            Ordering::Greater => {
                                child = *n.children.get(i.saturating_add(1)).ok_or(corrupt())?;
                                lo = Some(median);
                            }
                            Ordering::Less => {
                                child = *n.children.get(i).ok_or(corrupt())?;
                                hi = Some(median);
                            }
                        }
                    }
                    fences = (lo, hi);
                    at = child;
                }
            }
        }
        Err(corrupt())
    }

    /// The prefix the keys between fences `lo` and `hi` share: their common prefix, none where a
    /// side is open.
    fn fence_prefix(&self, lo: Option<u32>, hi: Option<u32>) -> usize {
        match (lo, hi) {
            (Some(a), Some(b)) => {
                let (a, b) = (self.key(a), self.key(b));
                crate::util::keys::common_prefix(a, b)
            }
            _ => 0,
        }
    }

    /// Node `at`'s heads taken again at `prefix`, now its prefix.
    fn reprefix(&mut self, at: u32, prefix: usize) -> Result<(), Error> {
        let n = self.node(at)?;
        if n.prefix == prefix {
            return Ok(());
        }
        let mut heads = [0u32; MAX_KEYS];
        for (h, &e) in heads
            .iter_mut()
            .zip(n.entries.get(..n.len).ok_or(corrupt())?)
        {
            *h = head(self.key(e), prefix);
        }
        let n = self.node_mut(at)?;
        n.heads = heads;
        n.prefix = prefix;
        Ok(())
    }

    fn push(&mut self, node: Node) -> Result<u32, Error> {
        let i = u32::try_from(self.nodes.len()).map_err(|_| corrupt())?;
        self.nodes.push(node);
        Ok(i)
    }

    /// Splits `parent`'s full child `i` around a median that rises into `parent` at `i`. An
    /// even split leaves `DEGREE - 1` entries on each side (Cormen et al., §18.2). A split for a
    /// key above every other, `lopsided`, keeps all but the last entry on the left and starts the
    /// right empty: keys arriving in order then fill every node, where even splits leave each
    /// left half full for good (the bulk-loading split of B+-trees). A node's entry count is then
    /// below `DEGREE - 1` only on the rightmost path, which no search depends on.
    /// The child's fences are `fences`: the halves' prefixes are those of their own fences, the
    /// median on one side of each, and their heads are taken again at them.
    fn split(
        &mut self,
        parent: u32,
        i: usize,
        lopsided: bool,
        fences: (Option<u32>, Option<u32>),
    ) -> Result<(), Error> {
        let child = *self.node(parent)?.children.get(i).ok_or(corrupt())?;
        let full = self.node(child)?.clone();
        let keep = if lopsided { MAX_KEYS - 1 } else { DEGREE - 1 };
        let moved = MAX_KEYS.saturating_sub(keep).saturating_sub(1);
        let mut right = Node::new(full.leaf, full.prefix);
        right.len = moved;
        let from = keep.saturating_add(1);
        right
            .entries
            .get_mut(..moved)
            .ok_or(corrupt())?
            .copy_from_slice(full.entries.get(from..).ok_or(corrupt())?);
        right
            .heads
            .get_mut(..moved)
            .ok_or(corrupt())?
            .copy_from_slice(full.heads.get(from..).ok_or(corrupt())?);
        if !full.leaf {
            right
                .children
                .get_mut(..moved.saturating_add(1))
                .ok_or(corrupt())?
                .copy_from_slice(full.children.get(from..).ok_or(corrupt())?);
        }
        let median = *full.entries.get(keep).ok_or(corrupt())?;
        let right = self.push(right)?;
        let left = self.node_mut(child)?;
        left.len = keep;
        let left_prefix = self.fence_prefix(fences.0, Some(median));
        let right_prefix = self.fence_prefix(Some(median), fences.1);
        self.reprefix(child, left_prefix)?;
        self.reprefix(right, right_prefix)?;
        let median_head = head(self.key(median), self.node(parent)?.prefix);
        let p = self.node_mut(parent)?;
        let len = p.len;
        p.entries.copy_within(i..len, i.saturating_add(1));
        p.heads.copy_within(i..len, i.saturating_add(1));
        *p.entries.get_mut(i).ok_or(corrupt())? = median;
        *p.heads.get_mut(i).ok_or(corrupt())? = median_head;
        p.children.copy_within(
            i.saturating_add(1)..len.saturating_add(1),
            i.saturating_add(2),
        );
        *p.children.get_mut(i.saturating_add(1)).ok_or(corrupt())? = right;
        p.len = len.saturating_add(1);
        Ok(())
    }

    /// Every entry in key order: `each(key, op, value)`.
    pub fn walk(
        &self,
        each: impl FnMut(&[u8], Op, &[u8]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut walk = self.walk_start();
        self.walk_some(&mut walk, usize::MAX, each).map(|_| ())
    }

    /// A walk at the first entry, for [`Self::walk_some`].
    pub fn walk_start(&self) -> Walk {
        Walk {
            stack: vec![(self.root, 0)],
        }
    }

    /// A walk at the first entry whose key is at least `from`: a seek. Each inner node on the
    /// path is left at the step that visits its entry at or past `from` once the child below is
    /// walked, and the leaf at its first entry at or past `from`.
    pub fn walk_from(&self, from: &[u8]) -> Result<Walk, Error> {
        let mut walk = Walk::default();
        self.walk_from_into(from, &mut walk)?;
        Ok(walk)
    }

    /// [`Self::walk_from`] into `walk`, its stack's allocation kept: a seek allocates nothing
    /// once the stack has grown to the tree's height.
    pub fn walk_from_into(&self, from: &[u8], walk: &mut Walk) -> Result<(), Error> {
        let stack = &mut walk.stack;
        stack.clear();
        let mut at = self.root;
        // The walk descends one level a step: at most the tree's height.
        for _ in 0..=self.nodes.len() {
            let n = self.node(at)?;
            let (pos, exact) = match self.search(n, from) {
                Ok(i) => (i, true),
                Err(i) => (i, false),
            };
            if n.leaf {
                stack.push((at, pos));
                return Ok(());
            }
            if exact {
                // Entry `pos` is the key itself: it is next, then child `pos + 1`.
                stack.push((at, pos.saturating_mul(2).saturating_add(1)));
                return Ok(());
            }
            // Every key in child `pos` may be at or past `from`, and entry `pos` follows it.
            stack.push((at, pos.saturating_mul(2).saturating_add(1)));
            at = *n.children.get(pos).ok_or(corrupt())?;
        }
        Err(corrupt())
    }

    /// The next `limit` entries of `walk` in key order, `each(key, op, value)`; the entries
    /// visited, fewer than `limit` only at the end. A walk is resumed only over the memtable it
    /// started on, unchanged since: an immutable memtable packed a slice at a time.
    pub fn walk_some(
        &self,
        walk: &mut Walk,
        limit: usize,
        mut each: impl FnMut(&[u8], Op, &[u8]) -> Result<(), Error>,
    ) -> Result<usize, Error> {
        let mut visited = 0usize;
        // (node, next index): a leaf's index is its next entry; an inner node's alternates
        // child, entry.
        while visited < limit {
            let Some(&mut (at, ref mut i)) = walk.stack.last_mut() else {
                break;
            };
            let n = self.node(at)?;
            if n.leaf {
                let Some(&e) = n.entries.get(..n.len).and_then(|es| es.get(*i)) else {
                    walk.stack.pop();
                    continue;
                };
                *i = i.saturating_add(1);
                let (op, v) = self.value(e)?;
                each(self.key(e), op, v)?;
                visited = visited.saturating_add(1);
                continue;
            }
            // Step 2j visits child j, step 2j + 1 entry j.
            let step = *i;
            *i = step.saturating_add(1);
            let j = step / 2;
            if step % 2 == 0 {
                if j > n.len {
                    walk.stack.pop();
                    continue;
                }
                let child = *n.children.get(j).ok_or(corrupt())?;
                walk.stack.push((child, 0));
            } else if j < n.len {
                let e = *n.entries.get(j).ok_or(corrupt())?;
                let (op, v) = self.value(e)?;
                each(self.key(e), op, v)?;
                visited = visited.saturating_add(1);
            }
        }
        Ok(visited)
    }
}

/// A walk's place in a memtable ([`BTreeMem::walk_some`]): the nodes from the root to the next
/// entry and the step each is at, bounded by the tree's height.
#[derive(Debug, Default)]
pub struct Walk {
    stack: Vec<(u32, usize)>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    proptest::proptest! {
        /// Against a `BTreeMap`: any puts and deletes, keys repeated, give the same newest entry
        /// for every key, and the walk gives them in order.
        #[test]
        fn the_memtable_is_an_ordered_map_of_newest_entries(
            ops in proptest::collection::vec(
                (proptest::collection::vec(0u8..6, 0..6), proptest::prelude::any::<bool>(), proptest::collection::vec(proptest::prelude::any::<u8>(), 0..20)),
                0..2000,
            ),
        ) {
            let mut m = BTreeMem::new(1 << 24).unwrap();
            let mut oracle: BTreeMap<Vec<u8>, (Op, Vec<u8>)> = BTreeMap::new();
            for (k, put, v) in &ops {
                let op = if *put { Op::Put } else { Op::Delete };
                let v = if *put { v.clone() } else { Vec::new() };
                m.insert(k, op, &v).unwrap();
                oracle.insert(k.clone(), (op, v));
            }
            proptest::prop_assert_eq!(m.len(), oracle.len());
            let mut value = Vec::new();
            for (k, (op, v)) in &oracle {
                proptest::prop_assert_eq!(m.get(k, &mut value).unwrap(), Some(*op));
                proptest::prop_assert_eq!(&value, v);
            }
            let mut walked = Vec::new();
            m.walk(|k, op, v| { walked.push((k.to_vec(), (op, v.to_vec()))); Ok(()) }).unwrap();
            let expected: Vec<_> = oracle.into_iter().collect();
            proptest::prop_assert_eq!(&walked, &expected);
            // The same walk in slices of every size from 1 up: each visits its limit until the
            // end, and together they visit every entry once, in order.
            // A seek from every key and from between keys reads exactly the oracle's range.
            let probes: Vec<Vec<u8>> = expected
                .iter()
                .flat_map(|(k, _)| {
                    let mut past = k.clone();
                    past.push(0);
                    [k.clone(), past]
                })
                .chain([Vec::new(), vec![0xFF; 7]])
                .collect();
            for from in &probes {
                let mut walk = m.walk_from(from).unwrap();
                let mut got = Vec::new();
                m.walk_some(&mut walk, usize::MAX, |k, op, v| {
                    got.push((k.to_vec(), (op, v.to_vec())));
                    Ok(())
                }).unwrap();
                let want: Vec<_> = expected.iter().filter(|(k, _)| k >= from).cloned().collect();
                proptest::prop_assert_eq!(&got, &want, "from {:?}", from);
            }
            for limit in 1..8usize {
                let mut walk = m.walk_start();
                let mut sliced = Vec::new();
                loop {
                    let n = m.walk_some(&mut walk, limit, |k, op, v| {
                        sliced.push((k.to_vec(), (op, v.to_vec())));
                        Ok(())
                    }).unwrap();
                    if n < limit {
                        break;
                    }
                }
                proptest::prop_assert_eq!(&sliced, &expected);
                proptest::prop_assert_eq!(m.walk_some(&mut walk, limit, |_, _, _| Ok(())).unwrap(), 0);
            }
        }
    }

    proptest::proptest! {
        /// Mostly ascending keys that share their first 8 bytes (so heads tie and the arena
        /// decides), with some out of order and some repeated: the append path, its fallback when
        /// the rightmost leaf is full, and the descent agree with a `BTreeMap`.
        #[test]
        fn ascending_runs_with_shared_heads_stay_an_ordered_map(
            steps in proptest::collection::vec((0u32..5000, 0u8..10), 1..4000),
        ) {
            let mut m = BTreeMem::new(1 << 24).unwrap();
            let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let mut next = 0u32;
            for (jump, kind) in steps {
                let n = if kind < 7 { next = next.saturating_add(1); next } else { jump };
                let key = format!("bucket-1/{n:08}").into_bytes();
                let value = vec![kind; 3];
                m.insert(&key, Op::Put, &value).unwrap();
                oracle.insert(key, value);
            }
            let mut walked = Vec::new();
            m.walk(|k, _, v| { walked.push((k.to_vec(), v.to_vec())); Ok(()) }).unwrap();
            let expected: Vec<_> = oracle.clone().into_iter().collect();
            proptest::prop_assert_eq!(walked, expected);
            let mut v = Vec::new();
            for (k, val) in &oracle {
                proptest::prop_assert_eq!(m.get(k, &mut v).unwrap(), Some(Op::Put));
                proptest::prop_assert_eq!(&v, val);
            }
        }
    }

    /// Every node from `at` down, between `lo` and `hi`: its prefix is its fences' common prefix,
    /// each head is its key's past that prefix, and each key lies between the fences.
    fn check_prefixes(m: &BTreeMem, at: u32, lo: Option<u32>, hi: Option<u32>) {
        let n = m.node(at).unwrap();
        assert_eq!(n.prefix, m.fence_prefix(lo, hi), "node {at}");
        for i in 0..n.len {
            let k = m.key(n.entries[i]);
            assert_eq!(n.heads[i], head(k, n.prefix), "node {at} entry {i}");
            assert!(lo.is_none_or(|l| m.key(l) < k) && hi.is_none_or(|h| k < m.key(h)));
        }
        if !n.leaf {
            for c in 0..=n.len {
                let clo = if c > 0 { Some(n.entries[c - 1]) } else { lo };
                let chi = if c < n.len { Some(n.entries[c]) } else { hi };
                check_prefixes(m, n.children[c], clo, chi);
            }
        }
    }

    proptest::proptest! {
        /// Keys in any order under nested shared prefixes, some keys prefixes of others: every
        /// node's heads are taken past its fences' common prefix, and gets, misses and seeks
        /// agree with a `BTreeMap`.
        #[test]
        fn heads_past_fence_prefixes_keep_an_ordered_map(
            keys in proptest::collection::vec(
                (0usize..4, proptest::collection::vec(0u8..4, 0..5)),
                1..3000,
            ),
        ) {
            const PREFIXES: [&[u8]; 4] = [b"", b"t/", b"t/0000/", b"t/0000/agent-"];
            let mut m = BTreeMem::new(1 << 24).unwrap();
            let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            for (i, (p, tail)) in keys.iter().enumerate() {
                let mut k = PREFIXES[*p].to_vec();
                k.extend(tail.iter().map(|b| b'0' + b));
                let v = (i as u64).to_le_bytes().to_vec();
                m.insert(&k, Op::Put, &v).unwrap();
                oracle.insert(k, v);
            }
            check_prefixes(&m, m.root, None, None);
            let mut v = Vec::new();
            for (k, val) in &oracle {
                proptest::prop_assert_eq!(m.get(k, &mut v).unwrap(), Some(Op::Put));
                proptest::prop_assert_eq!(&v, val);
                let mut past = k.clone();
                past.push(b'9');
                proptest::prop_assert_eq!(
                    m.get(&past, &mut v).unwrap().is_some(),
                    oracle.contains_key(&past)
                );
                let mut walk = m.walk_from(&past).unwrap();
                let mut first = None;
                m.walk_some(&mut walk, 1, |k, _, _| { first = Some(k.to_vec()); Ok(()) }).unwrap();
                proptest::prop_assert_eq!(first, oracle.range(past.clone()..).next().map(|(k, _)| k.clone()));
            }
        }
    }

    #[test]
    fn a_filtered_get_answers_as_the_tree_does() {
        use crate::branch::filter::hash;
        let mut m = BTreeMem::new(1 << 22).unwrap();
        // A first fill of 100 keys sizes the filter at the clear; the refill holds five times
        // as many, past its size, so false positives rise but no key may be missed.
        for n in 0..100u32 {
            m.insert(&n.to_be_bytes(), Op::Put, b"v").unwrap();
        }
        m.clear();
        let mut oracle = BTreeMap::new();
        for n in (0..1_000u32).step_by(2) {
            let v = n.to_le_bytes();
            m.insert(&n.to_be_bytes(), Op::Put, &v).unwrap();
            oracle.insert(n, v.to_vec());
        }
        let mut value = Vec::new();
        for n in 0..1_000u32 {
            let k = n.to_be_bytes();
            let got = m.get_hashed(&k, hash(&k), &mut value).unwrap();
            match oracle.get(&n) {
                Some(v) => {
                    assert_eq!(got, Some(Op::Put), "key {n}");
                    assert_eq!(&value, v, "key {n}");
                }
                None => assert_eq!(got, None, "key {n}"),
            }
        }
    }

    #[test]
    fn a_cleared_memtable_is_empty_and_takes_entries_again() {
        let mut m = BTreeMem::new(1 << 20).unwrap();
        for i in 0..5000u32 {
            m.insert(&i.to_be_bytes(), Op::Put, b"v").unwrap();
        }
        m.clear();
        assert!(m.is_empty());
        assert_eq!(m.bytes(), 0);
        let mut v = Vec::new();
        assert_eq!(m.get(&7u32.to_be_bytes(), &mut v).unwrap(), None);
        m.insert(b"k", Op::Put, b"w").unwrap();
        assert_eq!(m.get(b"k", &mut v).unwrap(), Some(Op::Put));
        assert_eq!(v, b"w");
    }

    #[test]
    fn a_full_memtable_refuses_and_changes_nothing() {
        let mut m = BTreeMem::new(100).unwrap();
        m.insert(b"a", Op::Put, &[1; 40]).unwrap();
        let before = m.bytes();
        assert!(matches!(
            m.insert(b"b", Op::Put, &[2; 60]),
            Err(Error::LimitExceeded { .. })
        ));
        assert_eq!(m.bytes(), before);
        assert_eq!(m.len(), 1);
        let mut v = Vec::new();
        assert_eq!(m.get(b"b", &mut v).unwrap(), None);
    }
}
