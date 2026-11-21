//! The trie of a Fast Succinct Trie in LOUDS-DS (Zhang et al. SIGMOD 2018 §2.2–§2.4;
//! research/35): its upper levels in LOUDS-Dense, a node as a 256-bit bitmap of its labels and
//! one of the labels whose branch goes on, so a child is found by one bit test and one rank; its
//! lower levels in LOUDS-Sparse, a node's labels as bytes with a bit a label for whether its branch
//! goes on and a bit for whether it is the node's first. The levels are cut where the dense part
//! would pass 1/64 of the sparse part (the paper's R = 64).
//!
//! Where the paper marks a node whose path is itself a key with a 0xFF label (sparse) or a bit in
//! a third bitmap (dense), a bit a node marks it here at every level: a bit where the label took
//! eight, and no rule needed to tell it from a real 0xFF byte.
//!
//! The dense part's nodes are numbered breadth first, the root 0; the branch that goes on at a
//! label leads to node `1 + (branches going on before it)`, or, past the dense nodes, to a node of
//! the first sparse level. The sparse part is kept a level at a time, as the builder lays it out:
//! the nodes of a level are the branches going on in the level above, in order, so the branch at
//! label `p` leads to the next level's node `rank1(has_child, p) − 1`, and rank runs within a
//! level. Where the paper finds a node's first label by select on S-LOUDS, each node's start is
//! kept (`Starts`). The trie is finished without the paper's assembly of the levels into one
//! sequence (one pass over every label, milliseconds at half a million keys, in one slice): each
//! level's words are copied whole and indexed. Values are held a level at a time in node order: a
//! node's own key's value, then its labels' that end a key.

use std::ops::ControlFlow;

use super::bits::{Bits, Grow};
use super::packed::{MAX_WIDTH, Packed, width_of};
use crate::error::{Error, Malformed};

fn corrupt() -> Error {
    Error::Corruption {
        what: "a succinct trie",
        why: Malformed::OutOfRange,
    }
}

/// Rank block bits for LOUDS-Sparse: a block in one cache line, 6.25% over the bits; for
/// LOUDS-Dense, a word, so a rank costs one popcount (research/35 §1).
const SPARSE_BLOCK: usize = 512;
const DENSE_BLOCK: usize = 64;

/// The dense part's bits at most this share of the sparse part's (the paper's R).
const DENSE_RATIO: usize = 64;

/// Bits a dense node takes: two 256-bit bitmaps and its own-key bit.
const DENSE_NODE_BITS: usize = 513;
/// Bits a sparse label takes: its byte, its has-child and node-start bits.
const SPARSE_LABEL_BITS: usize = 10;

/// Nodes a block of [`Starts`]: the most whose extra labels always fit 16 bits. A node holds at
/// most 256 labels, one a byte value, so a block's 256 nodes hold at most 256 × 255 = 65,280
/// labels past their first, where the next block starts included.
const STARTS_BLOCK: usize = 256;

/// The extras a block keeps: one a node, and where the next block's first node starts, so a
/// node's labels end within its own block's entries.
const STARTS_ENTRIES: usize = STARTS_BLOCK + 1;

/// Labels a node can hold: one a byte value.
const NODE_LABELS: usize = 256;

/// The set bits of `bits` before position `p`.
fn before(bits: &Bits, p: usize) -> usize {
    p.checked_sub(1).map_or(0, |q| bits.rank1(q))
}

/// A static trie of byte keys, each with a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Trie {
    /// The bytes every key starts with, kept as bytes: the trie holds the rest of each key, so a
    /// descent does not walk the shared prefix a level a byte (16-byte number keys share their
    /// first five, object names their tenant). Prefix truncation, as the memtable's nodes do.
    prefix: Vec<u8>,
    dense: Dense,
    /// The levels below the dense part, the first one's nodes the dense part's last branches.
    sparse: Sparse,
    /// Keys held.
    len: usize,
}

/// The upper levels: a node is 256 bits of each bitmap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Dense {
    labels: Bits,
    has_child: Bits,
    /// A bit a node: whether the node's path is itself a key.
    prefix: Bits,
    nodes: usize,
    /// Branches going on in the dense levels.
    children: usize,
    /// The dense levels' values in node order.
    values: Packed,
}

/// The levels below the dense part, each sequence a level after the one above: the labels and
/// their branch and node-start bits, a bit a node for whether its path is itself a key, and the
/// values in node order, a node's own key's before its labels' that end keys. Nodes are numbered
/// across the levels breadth first, the first level's the dense part's last branches, so the
/// branch at label `p` goes on to node `roots + (branches going on before p)`, and a node's value
/// or a label's is one rank of each vector over the whole part. A level so costs its labels' and
/// nodes' bits, where a record and buffers of its own cost ~438 bytes a level whatever it held
/// (research/67): two keys parting late in a small branch made it hundreds of levels deep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sparse {
    labels: Vec<u8>,
    has_child: Bits,
    louds: Bits,
    /// Where each node's labels start, from `louds`.
    starts: Starts,
    /// A bit a node.
    prefix: Bits,
    values: Packed,
    /// The first level's nodes, which no branch of the sparse part leads to.
    roots: usize,
    /// The levels: what a descent's steps are bounded by.
    levels: usize,
}

/// A block of [`Starts`] whose nodes each hold one label: it keeps no extras.
const NO_EXTRA: u32 = u32::MAX;

/// The position of each node's first label in the sparse part, and past the last node its end:
/// a node's labels are its block's entry and two adjacent extras, where select on S-LOUDS scanned
/// from a sample every 64 node starts by popcounts, the most of a leaf lookup's instructions
/// (research/62 §1). A node starts at its block's first node's position (every
/// [`STARTS_BLOCK`]th), plus one label for each node before it in the block, plus those nodes'
/// labels past their first, in 16 bits; a block's extras run one past its last node, to where the
/// next block starts, so a node's end is the extra beside its start's. A block whose nodes each
/// hold one label, a run of single-child chains, keeps no extras: there a node starts at its own
/// number past the block's first and holds one label (research/62 §2). Built from S-LOUDS when
/// the trie is built or read, so no byte written changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Starts {
    /// The nodes, not counting the end past the last.
    nodes: usize,
    /// Each block's first node's position, and where its extras begin in `extra` ([`NO_EXTRA`]
    /// for none).
    blocks: Vec<(u32, u32)>,
    /// Each node's labels past one a node before it in its block, and the next block's first's
    /// (the last block's: the end's), for the blocks that keep extras.
    extra: Vec<u16>,
}

impl Starts {
    /// The starts of the nodes `louds` marks the first labels of, in a part of `labels` labels.
    /// A node of more labels than byte values is corrupt: it cannot be built. The blocks' extras
    /// are counted first, so they are taken once at their size.
    fn of(louds: &Bits, labels: usize) -> Result<Self, Error> {
        let nodes = louds.ones();
        let blocks = nodes.saturating_add(1).div_ceil(STARTS_BLOCK);
        // The extras' length first, from each block's first node's start alone, in one pass over
        // the words: a block keeps extras when the nodes it holds span more labels than one each,
        // to where the next block starts, the last block's to the part's end.
        let (mut need, mut prev) = (0usize, None);
        louds.select_each(
            (0..blocks)
                .map(|b| b.saturating_mul(STARTS_BLOCK))
                .filter(|&rank| rank < nodes),
            |_, at| {
                if prev.is_some_and(|p: usize| at.saturating_sub(p) > STARTS_BLOCK) {
                    need = need.saturating_add(STARTS_ENTRIES);
                }
                prev = Some(at);
            },
        );
        // The block of the last node runs to the end: it holds its nodes and the end's entry.
        if let Some(p) = prev {
            let first = (nodes.saturating_sub(1) / STARTS_BLOCK).saturating_mul(STARTS_BLOCK);
            let held = nodes.saturating_sub(first);
            if labels.saturating_sub(p) > held {
                need = need.saturating_add(held.saturating_add(1));
            }
        }
        let mut starts = Self {
            nodes,
            blocks: Vec::with_capacity(nodes.div_ceil(STARTS_BLOCK).saturating_add(1)),
            extra: Vec::with_capacity(need),
        };
        Self::each_block(louds, labels, nodes, |first, extras| {
            let first = u32::try_from(first).map_err(|_| corrupt())?;
            let at = match extras {
                Some(e) => {
                    let at = u32::try_from(starts.extra.len()).map_err(|_| corrupt())?;
                    starts.extra.extend_from_slice(e);
                    at
                }
                None => NO_EXTRA,
            };
            starts.blocks.push((first, at));
            Ok(())
        })?;
        Ok(starts)
    }

    /// Each block of the `nodes` nodes' starts, and the end past the last, in order: its first
    /// node's position, and its extras when a node of it holds more than one label, the last of
    /// them where the next block starts.
    fn each_block(
        louds: &Bits,
        labels: usize,
        nodes: usize,
        mut block: impl FnMut(usize, Option<&[u16]>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut extras = [0u16; STARTS_ENTRIES];
        let (mut first, mut held_more) = (0usize, false);
        let mut extra = 0usize;
        let mut prev = None;
        let mut from = 0usize;
        for n in 0..=nodes {
            let at = if n < nodes {
                louds.next_one(from).ok_or(corrupt())?
            } else {
                labels
            };
            if let Some(p) = prev {
                let held = at.checked_sub(p).ok_or(corrupt())?;
                if held > NODE_LABELS {
                    return Err(corrupt());
                }
                extra = extra.saturating_add(held.saturating_sub(1));
            }
            let within = n % STARTS_BLOCK;
            if within == 0 {
                if n > 0 {
                    // The block before ends where this one starts: its last extra.
                    let e = u16::try_from(extra).map_err(|_| corrupt())?;
                    held_more |= e > 0;
                    *extras.get_mut(STARTS_BLOCK).ok_or(corrupt())? = e;
                    block(first, if held_more { Some(&extras[..]) } else { None })?;
                }
                first = at;
                extra = 0;
                held_more = false;
            }
            let e = u16::try_from(extra).map_err(|_| corrupt())?;
            held_more |= e > 0;
            *extras.get_mut(within).ok_or(corrupt())? = e;
            prev = Some(at);
            from = at.checked_add(1).ok_or(corrupt())?;
        }
        let last = extras
            .get(..(nodes % STARTS_BLOCK).saturating_add(1))
            .ok_or(corrupt())?;
        block(first, if held_more { Some(last) } else { None })
    }

    /// Where node `n` starts; for the node past the last, the part's end.
    fn at(&self, n: usize) -> Option<usize> {
        if n > self.nodes {
            return None;
        }
        let (start, extras) = *self.blocks.get(n / STARTS_BLOCK)?;
        let within = n % STARTS_BLOCK;
        let extra = if extras == NO_EXTRA {
            0
        } else {
            usize::from(*self.extra.get((extras as usize).checked_add(within)?)?)
        };
        (start as usize).checked_add(within)?.checked_add(extra)
    }

    /// Node `n`'s labels, where it starts and where the next node does: its block's entry and two
    /// adjacent extras, the block's extras running one past its last node.
    fn range(&self, n: usize) -> Option<(usize, usize)> {
        if n >= self.nodes {
            return None;
        }
        let (start, extras) = *self.blocks.get(n / STARTS_BLOCK)?;
        let within = n % STARTS_BLOCK;
        let start = (start as usize).checked_add(within)?;
        let next = start.checked_add(1)?;
        if extras == NO_EXTRA {
            // Every node of the block holds one label.
            return Some((start, next));
        }
        let [a, b] = *self
            .extra
            .get((extras as usize).checked_add(within)?..)?
            .first_chunk::<2>()?;
        Some((
            start.checked_add(usize::from(a))?,
            next.checked_add(usize::from(b))?,
        ))
    }

    /// Bytes held.
    fn bytes(&self) -> usize {
        self.blocks
            .len()
            .saturating_mul(8)
            .saturating_add(self.extra.len().saturating_mul(2))
    }

    /// The bytes its buffers hold in memory, their capacities.
    fn held_bytes(&self) -> usize {
        self.blocks
            .capacity()
            .saturating_mul(8)
            .saturating_add(self.extra.capacity().saturating_mul(2))
    }
}

impl Sparse {
    /// Node `n`: its first label's position and the next node's; none past the last.
    fn range(&self, n: usize) -> (usize, usize) {
        self.starts
            .range(n)
            .unwrap_or((self.labels.len(), self.labels.len()))
    }

    /// The node the branch at label `p` goes on to.
    fn child(&self, p: usize) -> usize {
        self.roots.saturating_add(before(&self.has_child, p))
    }

    /// The value of the key the branch at label `p`, of node `n`, ends: after the keys ended by
    /// the labels before it and the nodes' own keys up to `n`'s.
    fn value_at(&self, n: usize, p: usize) -> Option<u32> {
        let ends_before = p.saturating_sub(before(&self.has_child, p));
        self.values
            .get(ends_before.saturating_add(self.prefix.rank1(n)))
    }

    /// The value of node `n`'s own key: before its labels' values.
    fn value_of(&self, n: usize) -> Option<u32> {
        let (start, _) = self.range(n);
        let ends_before = start.saturating_sub(before(&self.has_child, start));
        self.values
            .get(ends_before.saturating_add(before(&self.prefix, n)))
    }
}

/// A node: dense, by its number; sparse, by its number across the sparse levels.
#[derive(Clone, Copy, Debug)]
enum Node {
    Dense(usize),
    Sparse(usize),
}

/// One level of the trie as the builder fills it.
#[derive(Clone, Debug, Default)]
struct Level {
    labels: Vec<u8>,
    has_child: Grow,
    louds: Grow,
    /// A bit a node: whether its path is itself a key.
    prefix: Grow,
    /// The level's values in node order: a node's own key's, then its labels' that end keys.
    values: Packed,
}

impl Level {
    /// Empties the level, keeping its buffers, its values to be `width` bits.
    fn reset(&mut self, width: u32) -> Result<(), Error> {
        self.labels.clear();
        self.has_child.clear();
        self.louds.clear();
        self.prefix.clear();
        self.values.reset(width)
    }

    /// Adds `key`'s label at depth `d`, the first of a new node when `first`, with `value` when
    /// the label ends the key. Inlined, as `TrieBuilder::level` and `Packed::push` are: out of
    /// line, the three calls took 77 of a range filter's 412 instructions a key to build
    /// (research/67 §7).
    #[inline(always)]
    fn push(&mut self, key: &[u8], d: usize, first: bool, value: u32) -> Result<(), Error> {
        let last = d.saturating_add(1) == key.len();
        self.labels.push(key.get(d).copied().unwrap_or(0));
        self.has_child.push(!last);
        self.louds.push(first);
        if last {
            self.values.push(value)?;
        }
        Ok(())
    }
}

/// The most bytes a trie builder holds of its own having taken keys whose longest is `longest`
/// bytes, `labels` labels in all and `values` values of `width` bits. Each level a key reaches
/// keeps its record, at most twice over for its list's doubling, and its buffers' first
/// allocations: eight label bytes and four words in each of its three bit vectors and its
/// values (std's least capacities). Past those, a label is its byte and three bits (its child
/// and node bits and its node's prefix bit), and a value its bits, each at most twice over for
/// its buffer's doubling. A builder given whole keys ([`TrieBuilder::add`]) keeps the last,
/// grown to at most twice the longest. Pinned by `tests/trie_memory.rs` at the densest keys,
/// the longest and the most labels keys of a length can make.
pub fn builder_bytes(longest: usize, labels: u64, values: u64, width: u32) -> Option<u64> {
    let first = u64::try_from(size_of::<u64>())
        .ok()?
        .checked_mul(16)?
        .checked_add(8)?;
    let level = u64::try_from(size_of::<Level>())
        .ok()?
        .checked_mul(2)?
        .checked_add(first)?;
    let longest = u64::try_from(longest).ok()?;
    longest
        .checked_add(1)?
        .checked_mul(level)?
        .checked_add(labels.checked_mul(11)?.div_ceil(4))?
        .checked_add(values.checked_mul(u64::from(width))?.div_ceil(4))?
        .checked_add(longest.checked_mul(2)?)
}

/// The most bytes a finished trie holds ([`Trie::held_bytes`]) of at most `labels` labels in at
/// most `nodes` nodes and `values` values of `width` bits, the levels its keys share at most
/// `prefix` bytes. Its sparse levels take each buffer once at its size: a label's byte and its
/// child and node bits, a node's own-key bit, a rank entry each eight words of each vector, its
/// node starts (an entry each block of 256 nodes and one past them, and 257 extras a block at
/// most) and its values packed. Its dense levels hold at most a 64th of the bits the sparse levels
/// below them would ([`TrieBuilder::cutoff`]), their three vectors and their values grown by
/// doubling from std's least capacity of four words, with a rank entry each word; its prefix is
/// collected as it grows, from eight bytes. Pinned by `tests/resident_memory.rs` at the shapes
/// that fill each part most.
pub fn most_bytes(labels: u64, nodes: u64, values: u64, width: u32, prefix: u64) -> Option<u64> {
    let words = |bits: u64| bits.div_ceil(64);
    let block = u64::try_from(SPARSE_BLOCK / 64).ok()?;
    // A sparse vector of `bits` bits: its words, and a rank entry each block of them.
    let vector = |bits: u64| -> Option<u64> {
        let w = words(bits);
        w.checked_mul(8)?
            .checked_add(w.div_ceil(block).checked_mul(4)?)
    };
    let sparse = labels
        .checked_add(vector(labels)?)?
        .checked_add(vector(labels)?)?
        .checked_add(vector(nodes)?)?;
    let blocks = nodes
        .div_ceil(u64::try_from(STARTS_BLOCK).ok()?)
        .checked_add(1)?;
    let starts = blocks.checked_mul(8)?.checked_add(
        blocks
            .checked_mul(u64::try_from(STARTS_ENTRIES).ok()?)?
            .checked_mul(2)?,
    )?;
    // The values: the sparse levels' taken at their size, the dense levels' grown by doubling,
    // each part's last word partly used.
    let packed = words(values.checked_mul(u64::from(width))?);
    let held_values = packed
        .checked_add(1)?
        .checked_mul(2)?
        .checked_add(4)?
        .checked_mul(8)?;
    // The dense levels' three vectors: their bits at most a 64th of the sparse bits, each
    // vector's last word partly used, each grown to at most twice its words or four.
    let dense = words(
        labels
            .checked_mul(u64::try_from(SPARSE_LABEL_BITS).ok()?)?
            .div_ceil(u64::try_from(DENSE_RATIO).ok()?),
    )
    .checked_add(3)?;
    let dense = dense
        .checked_mul(2)?
        .checked_add(12)?
        .checked_mul(8)?
        .checked_add(dense.checked_mul(4)?)?;
    sparse
        .checked_add(starts)?
        .checked_add(held_values)?
        .checked_add(dense)?
        .checked_add(prefix.checked_mul(2)?.checked_add(8)?)
}

/// Builds a trie of keys given in strictly ascending order, each with a value of at most a
/// width of bits, one pass and no key held but the last. Its levels keep their buffers when it
/// is reset, so a builder used again for a trie no larger allocates nothing as keys are added.
#[derive(Clone, Debug, Default)]
pub struct TrieBuilder {
    levels: Vec<Level>,
    /// Levels in use; those past it are kept for their buffers.
    depth: usize,
    width: u32,
    prev: Vec<u8>,
    /// The last key's length: all [`Self::add_after`] keeps of it.
    prev_len: usize,
    /// Whether the keys come by [`Self::add_after`]: a builder takes them one way only.
    after: bool,
    keys: usize,
    /// The largest value given, and whether the trie's values take only the bits it needs.
    max: u32,
    fit: bool,
}

/// The bits a trie's values take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Width {
    /// Exactly these: a range filter's suffix bits, which its queries read at that width.
    Bits(u32),
    /// The bits the largest value needs, up to 32: a leaf index's page numbers, whose largest
    /// is known only at the end. The values are packed again to that width when the trie is
    /// finished, which the index's few values make cheap.
    Fit,
}

impl TrieBuilder {
    /// A builder for values of `width`.
    pub fn new(width: Width) -> Result<Self, Error> {
        let mut b = Self::default();
        b.reset(width)?;
        Ok(b)
    }

    /// Empties the builder for another trie, its values of `width`, its buffers kept.
    pub fn reset(&mut self, width: Width) -> Result<(), Error> {
        let (width, fit) = match width {
            Width::Bits(w) => (w, false),
            Width::Fit => (MAX_WIDTH, true),
        };
        if width > MAX_WIDTH {
            return Err(Error::InvalidArgument {
                what: "trie values wider than 32 bits",
            });
        }
        // Each level is emptied as it comes back into use.
        self.fit = fit;
        self.width = width;
        self.depth = 0;
        self.prev.clear();
        self.prev_len = 0;
        self.after = false;
        self.keys = 0;
        self.max = 0;
        // The root: a node with no labels yet.
        self.level(0)?.prefix.push(false);
        Ok(())
    }

    /// The keys added.
    pub fn len(&self) -> usize {
        self.keys
    }

    /// Whether no key has been added.
    pub fn is_empty(&self) -> bool {
        self.keys == 0
    }

    /// Level `d`, the levels down to it in use.
    #[inline(always)]
    fn level(&mut self, d: usize) -> Result<&mut Level, Error> {
        while self.depth <= d {
            if self.levels.len() <= self.depth {
                self.levels.push(Level::default());
            }
            let width = self.width;
            self.levels
                .get_mut(self.depth)
                .ok_or(corrupt())?
                .reset(width)?;
            self.depth = self.depth.saturating_add(1);
        }
        self.levels.get_mut(d).ok_or(corrupt())
    }

    /// Adds `key`, greater than every key added, with `value`.
    pub fn add(&mut self, key: &[u8], value: u32) -> Result<(), Error> {
        if self.after {
            return Err(Error::InvalidArgument {
                what: "a trie's keys given both with and without their shared prefixes",
            });
        }
        if self.keys > 0 && key <= self.prev.as_slice() {
            return Err(Error::InvalidArgument {
                what: "a trie's keys not strictly ascending",
            });
        }
        let lcp = if self.keys == 0 {
            0
        } else {
            crate::util::keys::common_prefix(&self.prev, key)
        };
        self.push_key(key, lcp, value)?;
        self.prev.clear();
        self.prev.extend_from_slice(key);
        Ok(())
    }

    /// [`Self::add`] for a caller that holds the keys: `key` shares its first `lcp` bytes with
    /// the key added before it, and the caller has checked it is the greater. The builder keeps
    /// no copy of it, so a range filter's keys are copied once, by the branch builder that holds
    /// them ([`super::surf::SurfBuilder::push`]).
    pub fn add_after(&mut self, key: &[u8], lcp: usize, value: u32) -> Result<(), Error> {
        if self.keys > 0 && !self.after {
            return Err(Error::InvalidArgument {
                what: "a trie's keys given both with and without their shared prefixes",
            });
        }
        // Greater than a key of `prev_len` bytes it shares `lcp` with: past it when it extends
        // it, so never a prefix of it.
        if self.keys > 0 && (lcp > self.prev_len || lcp >= key.len()) {
            return Err(Error::InvalidArgument {
                what: "a trie's keys not strictly ascending",
            });
        }
        self.after = true;
        self.push_key(key, lcp, value)
    }

    /// Gives the key added last `value` in place of the one it came with: its value is the last of
    /// the level its last label is on, or the root's own for the empty key.
    pub fn set_last(&mut self, value: u32) -> Result<(), Error> {
        if self.keys == 0 || width_of(value) > self.width {
            return Err(Error::InvalidArgument {
                what: "a trie's last value set with no key, or wider than its width",
            });
        }
        let level = self
            .levels
            .get_mut(self.prev_len.saturating_sub(1))
            .ok_or(corrupt())?;
        level.values.pop().ok_or(corrupt())?;
        level.values.push(value)?;
        self.max = self.max.max(value);
        Ok(())
    }

    /// Adds `key`, sharing `lcp` bytes with the key before it, to the levels it reaches.
    fn push_key(&mut self, key: &[u8], lcp: usize, value: u32) -> Result<(), Error> {
        if width_of(value) > self.width {
            return Err(Error::InvalidArgument {
                what: "a trie value wider than its width",
            });
        }
        let first_key = self.keys == 0;
        self.keys = self.keys.checked_add(1).ok_or(corrupt())?;
        self.max = self.max.max(value);
        if key.is_empty() {
            // The empty key sorts first: the root's own.
            self.prev_len = 0;
            let root = self.level(0)?;
            root.prefix.set(0, true);
            return root.values.push(value);
        }
        if !first_key && lcp == self.prev_len && self.prev_len > 0 {
            // The previous key is a prefix of this one: its last label goes on, and the node
            // it now leads to holds it as its own key, its value the level's last.
            let d = self.prev_len.saturating_sub(1);
            let l = self.level(d)?;
            l.has_child.set_last(true);
            let ended = l.values.pop().ok_or(corrupt())?;
            let l = self.level(lcp)?;
            l.prefix.push(true);
            l.values.push(ended)?;
            l.push(key, lcp, true, value)?;
        } else {
            // A new label in the node the previous key's branch at this level is in, or the
            // root's first.
            let l = self.level(lcp)?;
            let first = lcp == 0 && l.labels.is_empty();
            l.push(key, lcp, first, value)?;
        }
        for d in lcp.saturating_add(1)..key.len() {
            let l = self.level(d)?;
            l.prefix.push(false);
            l.push(key, d, true, value)?;
        }
        self.prev_len = key.len();
        Ok(())
    }

    /// The trie of the keys added, its dense part cut where the ratio puts it.
    pub fn finish(&self) -> Result<Trie, Error> {
        self.finish_at(self.cutoff())
    }

    /// The levels in use.
    fn used(&self) -> &[Level] {
        self.levels.get(..self.depth).unwrap_or(&[])
    }

    /// The leading levels every key passes through alone: one node, not itself a key, with one
    /// label whose branch goes on. Their labels are the keys' shared prefix.
    fn shared(&self) -> usize {
        self.used()
            .iter()
            .take_while(|l| {
                l.prefix.len() == 1 && !l.prefix.get(0) && l.labels.len() == 1 && l.has_child.get(0)
            })
            .count()
    }

    /// The levels below the shared prefix: the trie of the rest of each key.
    fn kept(&self) -> &[Level] {
        self.used().get(self.shared()..).unwrap_or(&[])
    }

    /// The deepest level the dense part may reach: the largest `l` whose levels above it, dense,
    /// take at most 1/64 of the sparse levels from it down (Zhang et al. §2.4).
    fn cutoff(&self) -> usize {
        let dense = |l: &Level| l.prefix.len().saturating_mul(DENSE_NODE_BITS);
        let sparse = |l: &Level| l.labels.len().saturating_mul(SPARSE_LABEL_BITS);
        let mut below: usize = self.kept().iter().map(sparse).sum();
        let mut above = 0usize;
        let mut cut = 0usize;
        for (l, level) in self.kept().iter().enumerate() {
            above = above.saturating_add(dense(level));
            below = below.saturating_sub(sparse(level));
            if above.saturating_mul(DENSE_RATIO) <= below {
                cut = l.saturating_add(1);
            } else {
                break;
            }
        }
        cut
    }

    /// Levels above `cutoff` dense, the rest kept a level each as built: their words copied
    /// whole and indexed, their values as they are when they take the bits the largest needs.
    fn finish_at(&self, cutoff: usize) -> Result<Trie, Error> {
        let width = if self.fit {
            width_of(self.max)
        } else {
            self.width
        };
        let mut d_labels = Grow::default();
        let mut d_child = Grow::default();
        let mut d_prefix = Grow::default();
        let mut d_values = Packed::new(width)?;
        let mut dense_children = 0usize;
        let kept = self.kept();
        let (upper, lower) = kept.split_at(cutoff.min(kept.len()));
        for l in upper {
            let mut at = 0usize;
            for node in 0..l.prefix.len() {
                let base = d_labels.len();
                let next = base.checked_add(256).ok_or(corrupt())?;
                d_labels.grow_to(next);
                d_child.grow_to(next);
                d_prefix.push(l.prefix.get(node));
                // The node's labels: the first, then each not starting another node.
                let mut first = true;
                while at < l.labels.len() && (first || !l.louds.get(at)) {
                    let label = l.labels.get(at).copied().ok_or(corrupt())?;
                    let goes_on = l.has_child.get(at);
                    let pos = base.saturating_add(usize::from(label));
                    d_labels.set(pos, true);
                    d_child.set(pos, goes_on);
                    dense_children = dense_children.saturating_add(usize::from(goes_on));
                    first = false;
                    at = at.saturating_add(1);
                }
            }
            for i in 0..l.values.len() {
                d_values.push(l.values.get(i).ok_or(corrupt())?)?;
            }
        }
        // The levels below, one sequence each: labels and nodes counted first, so each buffer
        // is taken once at its size.
        let (labels, nodes, values) =
            lower.iter().fold((0usize, 0usize, 0usize), |(a, n, v), l| {
                (
                    a.saturating_add(l.labels.len()),
                    n.saturating_add(l.prefix.len()),
                    v.saturating_add(l.values.len()),
                )
            });
        let mut s_labels = Vec::with_capacity(labels);
        let (mut s_child, mut s_louds, mut s_prefix) =
            (Grow::default(), Grow::default(), Grow::default());
        s_child.reserve_bits(labels);
        s_louds.reserve_bits(labels);
        s_prefix.reserve_bits(nodes);
        let mut s_values = Packed::new(width)?;
        s_values.reserve(values)?;
        for l in lower {
            s_labels.extend_from_slice(&l.labels);
            s_child.append(&l.has_child);
            s_louds.append(&l.louds);
            s_prefix.append(&l.prefix);
            if l.values.width() == width {
                s_values.append(&l.values)?;
            } else {
                // A leaf index's page numbers, held at full width until the largest was known:
                // a value a leaf, few.
                for i in 0..l.values.len() {
                    s_values.push(l.values.get(i).ok_or(corrupt())?)?;
                }
            }
        }
        if d_values.len().saturating_add(s_values.len()) != self.keys {
            return Err(corrupt());
        }
        let louds = Bits::from_grow(s_louds, SPARSE_BLOCK)?;
        let sparse = Sparse {
            starts: Starts::of(&louds, s_labels.len())?,
            labels: s_labels,
            has_child: Bits::from_grow(s_child, SPARSE_BLOCK)?,
            louds,
            prefix: Bits::from_grow(s_prefix, SPARSE_BLOCK)?,
            values: s_values,
            roots: lower.first().map_or(0, |l| l.prefix.len()),
            levels: lower.len(),
        };
        let prefix = self
            .used()
            .iter()
            .take(self.shared())
            .filter_map(|l| l.labels.first().copied())
            .collect();
        Ok(Trie {
            prefix,
            dense: Dense {
                nodes: d_prefix.len(),
                labels: Bits::from_grow(d_labels, DENSE_BLOCK)?,
                has_child: Bits::from_grow(d_child, DENSE_BLOCK)?,
                prefix: Bits::from_grow(d_prefix, DENSE_BLOCK)?,
                children: dense_children,
                values: d_values,
            },
            sparse,
            len: self.keys,
        })
    }
}

impl Trie {
    /// The trie of `keys` (strictly ascending) with `values`, one a key.
    pub fn build(keys: &[&[u8]], values: &[u32]) -> Result<Self, Error> {
        Self::builder(keys, values)?.finish()
    }

    fn builder(keys: &[&[u8]], values: &[u32]) -> Result<TrieBuilder, Error> {
        if keys.len() != values.len() {
            return Err(Error::InvalidArgument {
                what: "a trie's keys not one value a key",
            });
        }
        let mut b = TrieBuilder::new(Width::Bits(width_of(
            values.iter().copied().max().unwrap_or(0),
        )))?;
        for (k, v) in keys.iter().zip(values) {
            b.add(k, *v)?;
        }
        Ok(b)
    }

    /// The root.
    fn root(&self) -> Node {
        if self.dense.nodes > 0 {
            Node::Dense(0)
        } else {
            Node::Sparse(0)
        }
    }

    // ------------------------------------------------------------------ dense

    /// The value of the key the dense branch at `pos` ends.
    fn dense_value_at(&self, pos: usize) -> Option<u32> {
        let d = &self.dense;
        let ends_before = before(&d.labels, pos).saturating_sub(before(&d.has_child, pos));
        let prefixes = d.prefix.rank1(pos / 256);
        d.values.get(ends_before.saturating_add(prefixes))
    }

    /// The value of dense node `n`'s own key.
    fn dense_value_of(&self, n: usize) -> Option<u32> {
        let d = &self.dense;
        let start = n.checked_mul(256)?;
        let ends_before = before(&d.labels, start).saturating_sub(before(&d.has_child, start));
        d.values
            .get(ends_before.saturating_add(before(&d.prefix, n)))
    }

    /// The node the dense branch at `pos` goes on to: dense, or the first sparse level's.
    fn dense_child(&self, pos: usize) -> Node {
        let g = before(&self.dense.has_child, pos).saturating_add(1);
        match g.checked_sub(self.dense.nodes) {
            Some(i) => Node::Sparse(i),
            None => Node::Dense(g),
        }
    }

    /// The greatest label of dense node `n` below `pos` (exclusive), as a position.
    fn dense_below(&self, n: usize, pos: usize) -> Option<usize> {
        let q = self.dense.labels.prev_one(pos.checked_sub(1)?)?;
        if q >= n.checked_mul(256)? {
            Some(q)
        } else {
            None
        }
    }

    /// The least label of dense node `n` above `pos` (exclusive), as a position.
    fn dense_above(&self, n: usize, pos: usize) -> Option<usize> {
        let q = self.dense.labels.next_one(pos.checked_add(1)?)?;
        if q < n.checked_add(1)?.checked_mul(256)? {
            Some(q)
        } else {
            None
        }
    }

    // ------------------------------------------------------------------ sparse

    /// The label at `p`.
    fn label(&self, p: usize) -> Option<u8> {
        self.sparse.labels.get(p).copied()
    }

    // ------------------------------------------------------------------ both

    /// Whether node `n`'s path is itself a key.
    fn is_key(&self, n: Node) -> bool {
        match n {
            Node::Dense(n) => self.dense.prefix.get(n),
            Node::Sparse(n) => self.sparse.prefix.get(n),
        }
    }

    /// The value of node `n`'s own key.
    fn value_of(&self, n: Node) -> Option<u32> {
        match n {
            Node::Dense(n) => self.dense_value_of(n),
            Node::Sparse(n) => self.sparse.value_of(n),
        }
    }

    /// The node a fallback's branch goes on to, or the value of the key it ends.
    fn step(&self, at: Fallback) -> Result<Node, Option<u32>> {
        match at {
            Fallback::Node(n) => Err(self.value_of(n)),
            Fallback::Dense(pos) => {
                if self.dense.has_child.get(pos) {
                    Ok(self.dense_child(pos))
                } else {
                    Err(self.dense_value_at(pos))
                }
            }
            Fallback::Sparse(n, p) => {
                if self.sparse.has_child.get(p) {
                    Ok(Node::Sparse(self.sparse.child(p)))
                } else {
                    Err(self.sparse.value_at(n, p))
                }
            }
        }
    }

    /// The greatest key held, written to `out` in place of what it held, and its value; none for
    /// a trie holding no key. Down the last branch of each node: a node's own key is the least
    /// beneath it, so it is the greatest only where no branch leaves the node.
    pub fn last(&self, out: &mut Vec<u8>) -> Option<u32> {
        out.clear();
        if self.len == 0 {
            return None;
        }
        out.extend_from_slice(&self.prefix);
        let mut node = self.root();
        // A level a step: at most every level, dense and sparse.
        for _ in 0..=self.sparse.levels.saturating_add(self.dense.nodes) {
            let Some(at) = self.last_label(node) else {
                return self.value_of(node);
            };
            match at {
                Fallback::Dense(pos) => out.push(u8::try_from(pos % 256).ok()?),
                Fallback::Sparse(_, p) => out.push(self.label(p)?),
                Fallback::Node(_) => {}
            }
            match self.step(at) {
                Ok(child) => node = child,
                Err(v) => return v,
            }
        }
        None
    }

    /// Where `key` stands against the shared prefix: below every key, past every key, or
    /// starting with the prefix, with the rest of it.
    fn against<'k>(&self, key: &'k [u8]) -> Against<'k> {
        let p = self.prefix.len();
        let n = key.len().min(p);
        match key.get(..n).cmp(&self.prefix.get(..n)) {
            std::cmp::Ordering::Less => Against::Below,
            std::cmp::Ordering::Greater => Against::Above,
            // A key that ends inside the prefix is below every key.
            std::cmp::Ordering::Equal => match key.get(p..) {
                Some(rest) => Against::Within(rest),
                None => Against::Below,
            },
        }
    }

    /// The value `key` holds.
    pub fn get(&self, key: &[u8]) -> Option<u32> {
        match self.against(key) {
            Against::Within(rest) => self.get_rest(rest),
            Against::Below | Against::Above => None,
        }
    }

    /// The value the rest of a key past the prefix holds.
    fn get_rest(&self, key: &[u8]) -> Option<u32> {
        let mut node = self.root();
        for (d, &b) in key.iter().enumerate() {
            let last = d.saturating_add(1) == key.len();
            let at = match node {
                Node::Dense(n) => {
                    let pos = n.checked_mul(256)?.checked_add(usize::from(b))?;
                    if !self.dense.labels.get(pos) {
                        return None;
                    }
                    Fallback::Dense(pos)
                }
                Node::Sparse(n) => {
                    let (start, end) = self.sparse.range(n);
                    let labels = self.sparse.labels.get(start..end)?;
                    let i = labels.binary_search(&b).ok()?;
                    Fallback::Sparse(n, start.checked_add(i)?)
                }
            };
            match self.step(at) {
                Ok(child) => node = child,
                Err(v) => return if last { v } else { None },
            }
        }
        if self.is_key(node) {
            self.value_of(node)
        } else {
            None
        }
    }

    /// The value of the greatest key at most `key`: the leaf a key falls in when the keys are
    /// the leaves' separators. One descent, remembering the nearest smaller branch passed; if
    /// the key's path ends above every key beneath it, the rightmost key under that branch.
    pub fn floor(&self, key: &[u8]) -> Option<u32> {
        self.floor_with(key).map(|(v, _)| v)
    }

    /// [`Self::floor`], with whether the key found is a prefix of `key` (or `key` itself): a
    /// key that stands for every key starting with it holds `key` only then.
    pub fn floor_with(&self, key: &[u8]) -> Option<(u32, bool)> {
        match self.against(key) {
            Against::Within(rest) => self.floor_rest(rest),
            Against::Below => None,
            Against::Above => self.greatest().map(|v| (v, false)),
        }
    }

    /// The value of the least key.
    pub fn least(&self) -> Option<u32> {
        let mut node = self.root();
        // Down the first branch of each node: at most a level a step.
        for _ in 0..=self.sparse.levels.saturating_add(self.dense.nodes) {
            if self.is_key(node) {
                return self.value_of(node);
            }
            match self.step(self.first_label(node)?) {
                Ok(child) => node = child,
                Err(v) => return v,
            }
        }
        None
    }

    /// The value of the greatest key.
    fn greatest(&self) -> Option<u32> {
        let root = self.root();
        match self.last_label(root) {
            Some(f) => self.rightmost(f),
            None if self.is_key(root) => self.value_of(root),
            None => None,
        }
    }

    /// [`Self::floor`] of the rest of a key past the prefix: a level a byte, the dense levels
    /// first, then the sparse ones with each level's vectors found once (research/62 §2).
    fn floor_rest(&self, key: &[u8]) -> Option<(u32, bool)> {
        let mut node = self.root();
        let mut bytes = key.iter();
        // The nearest point below which every key is less than `key`.
        let mut fallback: Option<Fallback> = None;
        while let Node::Dense(n) = node {
            let Some(&b) = bytes.next() else {
                break;
            };
            let d = &self.dense;
            if d.prefix.get(n) {
                fallback = Some(Fallback::Node(node));
            }
            let pos = n.checked_mul(256)?.checked_add(usize::from(b))?;
            if let Some(q) = self.dense_below(n, pos) {
                fallback = Some(Fallback::Dense(q));
            }
            if !d.labels.get(pos) {
                return self.fall_back(fallback?);
            }
            if !d.has_child.get(pos) {
                // A key ending here: `key` itself, or a prefix of it.
                return self.dense_value_at(pos).map(|v| (v, true));
            }
            node = self.dense_child(pos);
        }
        if let Node::Sparse(mut n) = node {
            let s = &self.sparse;
            for &b in bytes {
                if s.prefix.get(n) {
                    fallback = Some(Fallback::Node(Node::Sparse(n)));
                }
                let (start, end) = s.range(n);
                let labels = s.labels.get(start..end).unwrap_or(&[]);
                let (found, i) = match labels.binary_search(&b) {
                    Ok(i) => (true, i),
                    Err(i) => (false, i),
                };
                if let Some(smaller) = i.checked_sub(1) {
                    fallback = Some(Fallback::Sparse(n, start.saturating_add(smaller)));
                }
                if !found {
                    return self.fall_back(fallback?);
                }
                let p = start.saturating_add(i);
                if !s.has_child.get(p) {
                    // A key ending here: `key` itself, or a prefix of it.
                    return s.value_at(n, p).map(|v| (v, true));
                }
                n = s.child(p);
            }
            node = Node::Sparse(n);
        }
        // `key` ends at `node`: its own key, if any, is `key`; every other beneath is greater.
        if self.is_key(node) {
            return self.value_of(node).map(|v| (v, true));
        }
        self.fall_back(fallback?)
    }

    /// The floor where a descent left `key`'s path: a node's own key on the path is a prefix of
    /// `key`; the greatest key under a smaller branch is not.
    fn fall_back(&self, at: Fallback) -> Option<(u32, bool)> {
        let prefix = matches!(at, Fallback::Node(_));
        self.rightmost(at).map(|v| (v, prefix))
    }

    /// Node `n`'s greatest label.
    fn last_label(&self, n: Node) -> Option<Fallback> {
        match n {
            Node::Dense(n) => Some(Fallback::Dense(
                self.dense_below(n, n.checked_add(1)?.checked_mul(256)?)?,
            )),
            Node::Sparse(n) => {
                let (start, end) = self.sparse.range(n);
                if end > start {
                    Some(Fallback::Sparse(n, end.saturating_sub(1)))
                } else {
                    None
                }
            }
        }
    }

    /// The value of the greatest key at or under `from`.
    fn rightmost(&self, from: Fallback) -> Option<u32> {
        let mut at = from;
        // Down the last branch of each node: at most a level a step.
        for _ in 0..=self.sparse.levels.saturating_add(self.dense.nodes) {
            if let Fallback::Sparse(n, p) = at {
                return self.rightmost_sparse(n, p);
            }
            match self.step(at) {
                Ok(child) => at = self.last_label(child)?,
                Err(v) => return v,
            }
        }
        None
    }

    /// [`Self::rightmost`] from label `p` of sparse node `n`: down the last label of each node.
    fn rightmost_sparse(&self, mut n: usize, mut p: usize) -> Option<u32> {
        let s = &self.sparse;
        // A level a step.
        for _ in 0..=s.levels {
            if !s.has_child.get(p) {
                return s.value_at(n, p);
            }
            n = s.child(p);
            let (start, end) = s.range(n);
            p = end.checked_sub(1).filter(|&q| q >= start)?;
        }
        None
    }

    /// The value of the least key at least `key`, the key itself written to `out`.
    pub fn ceil(&self, key: &[u8], out: &mut Vec<u8>) -> Option<u32> {
        match self.seek(key, out, |_, _| ControlFlow::Continue(())) {
            ControlFlow::Continue(v) => v,
            ControlFlow::Break(()) => None,
        }
    }

    /// One descent along `key`: `prefix` is given each stored key that is a proper prefix of
    /// `key`, shortest first, as its length and value, and may stop the descent; then the value
    /// of the least key at least `key`, the key written to `out`. The prefixes are what a range
    /// filter of truncated keys cannot order against `key` by the trie alone (research/35 §2).
    pub fn seek<F>(
        &self,
        key: &[u8],
        out: &mut Vec<u8>,
        mut prefix: F,
    ) -> ControlFlow<(), Option<u32>>
    where
        F: FnMut(usize, u32) -> ControlFlow<()>,
    {
        out.clear();
        let p = self.prefix.len();
        match self.against(key) {
            Against::Above => ControlFlow::Continue(None),
            // Every key is past `key`, and none is a prefix of it: the least is the answer.
            Against::Below => {
                out.extend_from_slice(&self.prefix);
                self.seek_rest(&[], out, |_, _| ControlFlow::Continue(()))
            }
            Against::Within(rest) => {
                out.extend_from_slice(&self.prefix);
                self.seek_rest(rest, out, |len, v| prefix(len.saturating_add(p), v))
            }
        }
    }

    /// [`Self::seek`] of the rest of a key past the prefix, the key found appended to `out`.
    fn seek_rest<F>(
        &self,
        key: &[u8],
        out: &mut Vec<u8>,
        mut prefix: F,
    ) -> ControlFlow<(), Option<u32>>
    where
        F: FnMut(usize, u32) -> ControlFlow<()>,
    {
        let mut node = self.root();
        // The nearest branch passed whose label is greater than the key's byte: the least key
        // beneath it is the least greater than `key` that the descent leaves behind.
        let mut greater: Option<(Fallback, usize)> = None;
        for (d, &b) in key.iter().enumerate() {
            if self.is_key(node)
                && let Some(v) = self.value_of(node)
            {
                prefix(d, v)?;
            }
            let last = d.saturating_add(1) == key.len();
            let found = match node {
                Node::Dense(n) => {
                    let Some(pos) = n
                        .checked_mul(256)
                        .and_then(|s| s.checked_add(usize::from(b)))
                    else {
                        break;
                    };
                    if let Some(q) = self.dense_above(n, pos) {
                        greater = Some((Fallback::Dense(q), d));
                    }
                    if !self.dense.labels.get(pos) {
                        break;
                    }
                    Fallback::Dense(pos)
                }
                Node::Sparse(n) => {
                    let (start, end) = self.sparse.range(n);
                    let labels = self.sparse.labels.get(start..end).unwrap_or(&[]);
                    let (found, i) = match labels.binary_search(&b) {
                        Ok(i) => (true, i),
                        Err(i) => (false, i),
                    };
                    let above = if found { i.saturating_add(1) } else { i };
                    if above < labels.len() {
                        greater = Some((Fallback::Sparse(n, start.saturating_add(above)), d));
                    }
                    if !found {
                        break;
                    }
                    Fallback::Sparse(n, start.saturating_add(i))
                }
            };
            match self.step(found) {
                Err(v) => {
                    if last {
                        out.extend_from_slice(key);
                        return ControlFlow::Continue(v);
                    }
                    if let Some(v) = v {
                        prefix(d.saturating_add(1), v)?;
                    }
                    break;
                }
                Ok(child) => node = child,
            }
            if last {
                // `key` ends at the node the branch leads to: its own key is `key`, and every
                // other key beneath it is greater, the least its first label's.
                out.extend_from_slice(key);
                if self.is_key(node) {
                    return ControlFlow::Continue(self.value_of(node));
                }
                let first = self.first_label(node);
                return ControlFlow::Continue(first.and_then(|f| self.leftmost(f, out)));
            }
        }
        if key.is_empty() {
            let n = self.root();
            if self.is_key(n) {
                return ControlFlow::Continue(self.value_of(n));
            }
            return ControlFlow::Continue(self.first_label(n).and_then(|f| self.leftmost(f, out)));
        }
        ControlFlow::Continue(greater.and_then(|(at, d)| {
            out.extend_from_slice(key.get(..d)?);
            self.leftmost(at, out)
        }))
    }

    /// Node `n`'s least label.
    fn first_label(&self, n: Node) -> Option<Fallback> {
        match n {
            Node::Dense(n) => {
                let start = n.checked_mul(256)?;
                let q = self.dense.labels.next_one(start)?;
                if q < start.checked_add(256)? {
                    Some(Fallback::Dense(q))
                } else {
                    None
                }
            }
            Node::Sparse(n) => {
                let (start, end) = self.sparse.range(n);
                if start < end {
                    Some(Fallback::Sparse(n, start))
                } else {
                    None
                }
            }
        }
    }

    /// The value of the least key at or under the label at `from`, its labels appended to `out`.
    fn leftmost(&self, from: Fallback, out: &mut Vec<u8>) -> Option<u32> {
        let mut at = from;
        // Down the first branch of each node: at most a level a step.
        for _ in 0..=self.sparse.levels.saturating_add(self.dense.nodes) {
            match at {
                Fallback::Dense(pos) => out.push(u8::try_from(pos % 256).ok()?),
                Fallback::Sparse(_, p) => out.push(self.label(p)?),
                Fallback::Node(_) => {}
            }
            let child = match self.step(at) {
                Ok(child) => child,
                Err(v) => return v,
            };
            // A node's own key is the least beneath it.
            at = if self.is_key(child) {
                Fallback::Node(child)
            } else {
                self.first_label(child)?
            };
        }
        None
    }

    /// Appends the trie to `out`: the shared prefix, the dense part's three bit vectors, counts
    /// and values, then
    /// the sparse levels', each its labels, three bit vectors and values.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let put = |out: &mut Vec<u8>, n: usize| {
            out.extend_from_slice(&u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes());
        };
        put(out, self.prefix.len());
        out.extend_from_slice(&self.prefix);
        let d = &self.dense;
        d.labels.encode(out);
        d.has_child.encode(out);
        d.prefix.encode(out);
        put(out, d.nodes);
        put(out, d.children);
        d.values.encode(out);
        let s = &self.sparse;
        put(out, s.levels);
        put(out, s.roots);
        put(out, s.labels.len());
        out.extend_from_slice(&s.labels);
        s.has_child.encode(out);
        s.louds.encode(out);
        s.prefix.encode(out);
        s.values.encode(out);
    }

    /// A trie [`Self::encode`] wrote at the start of `bytes`, and the bytes it took. The parts
    /// are checked to agree: a dense node is 256 bits; each level has a has-child and a
    /// node-start bit a label and a prefix bit a node, its nodes the branches going on above it;
    /// and its values are its keys, each a node's own or a label's that ends.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), Error> {
        let mut at = 0usize;
        let bits = |at: &mut usize| -> Result<Bits, Error> {
            let (b, used) = Bits::decode(bytes.get(*at..).ok_or(corrupt())?)?;
            *at = at.checked_add(used).ok_or(corrupt())?;
            Ok(b)
        };
        let num = |at: &mut usize| -> Result<usize, Error> {
            let v = bytes
                .get(*at..)
                .and_then(<[u8]>::first_chunk::<8>)
                .map(|b| u64::from_le_bytes(*b))
                .ok_or(corrupt())?;
            *at = at.checked_add(8).ok_or(corrupt())?;
            usize::try_from(v).map_err(|_| corrupt())
        };
        let packed = |at: &mut usize| -> Result<Packed, Error> {
            let (v, used) = Packed::decode(bytes.get(*at..).ok_or(corrupt())?)?;
            *at = at.checked_add(used).ok_or(corrupt())?;
            Ok(v)
        };
        let p = num(&mut at)?;
        let p_end = at.checked_add(p).ok_or(corrupt())?;
        let prefix = bytes.get(at..p_end).ok_or(corrupt())?.to_vec();
        at = p_end;
        let labels_d = bits(&mut at)?;
        let child_d = bits(&mut at)?;
        let prefix_d = bits(&mut at)?;
        let nodes = num(&mut at)?;
        let children = num(&mut at)?;
        let values_d = packed(&mut at)?;
        if labels_d.len() != nodes.saturating_mul(256)
            || child_d.len() != labels_d.len()
            || prefix_d.len() != nodes
            || child_d.ones() != children
            || values_d.len()
                != labels_d
                    .ones()
                    .saturating_sub(children)
                    .saturating_add(prefix_d.ones())
        {
            return Err(corrupt());
        }
        let levels = num(&mut at)?;
        let roots = num(&mut at)?;
        let n = num(&mut at)?;
        let end = at.checked_add(n).ok_or(corrupt())?;
        let labels = bytes.get(at..end).ok_or(corrupt())?.to_vec();
        at = end;
        let has_child = bits(&mut at)?;
        let louds = bits(&mut at)?;
        let prefix_s = bits(&mut at)?;
        let values = packed(&mut at)?;
        // The first level's nodes: the dense part's branches past its own nodes, or the root
        // alone; with no level, none, unless the trie is empty.
        let expect = children.saturating_add(1).saturating_sub(nodes);
        let first_ok = if levels == 0 {
            roots == 0 && labels.is_empty() && prefix_s.is_empty() && (expect == 0 || nodes == 0)
        } else {
            roots == expect
        };
        // Every node holds a label but a root that is a key alone (the empty key, or one key all
        // shared), and is the first level's or a branch's that goes on.
        let starts_ok = if labels.is_empty() {
            prefix_s.len() <= 1
        } else {
            prefix_s.len() == louds.ones()
        };
        if !first_ok
            || !starts_ok
            || has_child.len() != labels.len()
            || louds.len() != labels.len()
            || prefix_s.len() != roots.saturating_add(has_child.ones())
            || levels > prefix_s.len()
            || values.len()
                != labels
                    .len()
                    .saturating_sub(has_child.ones())
                    .saturating_add(prefix_s.ones())
        {
            return Err(corrupt());
        }
        let starts = Starts::of(&louds, labels.len())?;
        // Breadth first: a node past the first level is reached by a branch before its own
        // labels, so a descent only moves down.
        for node in roots..prefix_s.len() {
            let start = starts.at(node).ok_or(corrupt())?;
            if before(&has_child, start) <= node.saturating_sub(roots) {
                return Err(corrupt());
            }
        }
        let len = values_d.len().checked_add(values.len()).ok_or(corrupt())?;
        let sparse = Sparse {
            labels,
            has_child,
            louds,
            starts,
            prefix: prefix_s,
            values,
            roots,
            levels,
        };
        Ok((
            Self {
                prefix,
                dense: Dense {
                    labels: labels_d,
                    has_child: child_d,
                    prefix: prefix_d,
                    nodes,
                    children,
                    values: values_d,
                },
                sparse,
                len,
            },
            at,
        ))
    }

    /// The keys held.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the trie holds no key.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bytes held.
    pub fn bytes(&self) -> usize {
        let (d, s) = (&self.dense, &self.sparse);
        self.prefix
            .len()
            .saturating_add(d.labels.bytes())
            .saturating_add(d.has_child.bytes())
            .saturating_add(d.prefix.bytes())
            .saturating_add(d.values.bytes())
            .saturating_add(s.labels.len())
            .saturating_add(s.has_child.bytes())
            .saturating_add(s.louds.bytes())
            .saturating_add(s.starts.bytes())
            .saturating_add(s.prefix.bytes())
            .saturating_add(s.values.bytes())
    }

    /// The bytes the trie holds in memory: each buffer's capacity, where [`Self::bytes`] counts
    /// what the buffers' contents take; its records are the trie's own.
    pub fn held_bytes(&self) -> usize {
        let (d, s) = (&self.dense, &self.sparse);
        self.prefix
            .capacity()
            .saturating_add(d.labels.held_bytes())
            .saturating_add(d.has_child.held_bytes())
            .saturating_add(d.prefix.held_bytes())
            .saturating_add(d.values.held_bytes())
            .saturating_add(s.labels.capacity())
            .saturating_add(s.has_child.held_bytes())
            .saturating_add(s.louds.held_bytes())
            .saturating_add(s.starts.held_bytes())
            .saturating_add(s.prefix.held_bytes())
            .saturating_add(s.values.held_bytes())
    }

    /// The labels held below the shared prefix: the dense part's and every sparse level's.
    #[cfg(test)]
    pub(crate) fn labels(&self) -> usize {
        self.dense
            .labels
            .ones()
            .saturating_add(self.sparse.labels.len())
    }

    /// The sparse levels below the shared prefix.
    #[cfg(test)]
    pub(crate) fn sparse_levels(&self) -> usize {
        self.sparse.levels
    }

    /// The bits a value takes.
    #[cfg(test)]
    fn value_width(&self) -> u32 {
        self.dense.values.width()
    }
}

/// A key against a trie's shared prefix ([`Trie::against`]).
enum Against<'k> {
    Below,
    Above,
    Within(&'k [u8]),
}

/// Where a descent falls back to: a dense label position, a sparse label (the node it is of, its
/// position), or a node whose own key it is.
#[derive(Clone, Copy, Debug)]
enum Fallback {
    Dense(usize),
    /// A sparse node and the position of one of its labels.
    Sparse(usize, usize),
    Node(Node),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn rng(x: &mut u64) -> u64 {
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        *x
    }

    /// Keys from a small alphabet with 0x00 and 0xFF in it, lengths 0 to 6: prefixes of one
    /// another, shared paths, every edge.
    fn key(x: &mut u64) -> Vec<u8> {
        let len = (rng(x) % 7) as usize;
        (0..len)
            .map(|_| [0x00, 0x01, b'a', b'b', 0x7f, 0xfe, 0xff][(rng(x) % 7) as usize])
            .collect()
    }

    #[test]
    fn gets_and_floors_agree_with_an_ordered_map() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for case in 0..400 {
            let n = (rng(&mut x) % 300) as usize;
            let map: BTreeMap<Vec<u8>, u32> = (0..n)
                .map(|i| (key(&mut x), u32::try_from(i).unwrap()))
                .collect();
            let keys: Vec<&[u8]> = map.keys().map(Vec::as_slice).collect();
            let values: Vec<u32> = map.values().copied().collect();
            let queries: Vec<Vec<u8>> = (0..300).map(|_| key(&mut x)).collect();
            // At every cut between the dense and sparse parts, all sparse to all dense.
            for cutoff in 0..=8 {
                let t = Trie::builder(&keys, &values)
                    .unwrap()
                    .finish_at(cutoff)
                    .unwrap();
                assert_eq!(t.len(), map.len());
                // The node starts' extras counted before they were taken: exactly their room.
                assert_eq!(
                    t.sparse.starts.extra.capacity(),
                    t.sparse.starts.extra.len(),
                    "case {case} cut {cutoff}"
                );
                let mut out = Vec::new();
                t.encode(&mut out);
                assert_eq!(Trie::decode(&out).unwrap(), (t.clone(), out.len()));
                for (k, v) in &map {
                    assert_eq!(t.get(k), Some(*v), "case {case} cut {cutoff} get {k:?}");
                }
                let mut last = vec![9u8];
                assert_eq!(
                    t.last(&mut last),
                    map.values().next_back().copied(),
                    "case {case} cut {cutoff} last"
                );
                if let Some(k) = map.keys().next_back() {
                    assert_eq!(&last, k, "case {case} cut {cutoff} last key");
                }
                for q in &queries {
                    assert_eq!(
                        t.get(q),
                        map.get(q).copied(),
                        "case {case} cut {cutoff} get {q:?}"
                    );
                    let below = map.range(..=q.clone()).next_back();
                    let want = below.map(|(_, v)| *v);
                    assert_eq!(t.floor(q), want, "case {case} cut {cutoff} floor {q:?}");
                    assert_eq!(
                        t.floor_with(q),
                        below.map(|(k, v)| (*v, q.starts_with(k))),
                        "case {case} cut {cutoff} floor_with {q:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn ceilings_and_prefixes_agree_with_an_ordered_map() {
        let mut x = 0x5851_f42d_4c95_7f2du64;
        for case in 0..400 {
            let n = (rng(&mut x) % 300) as usize;
            let map: BTreeMap<Vec<u8>, u32> = (0..n)
                .map(|i| (key(&mut x), u32::try_from(i).unwrap()))
                .collect();
            let keys: Vec<&[u8]> = map.keys().map(Vec::as_slice).collect();
            let values: Vec<u32> = map.values().copied().collect();
            let mut queries: Vec<Vec<u8>> = (0..300).map(|_| key(&mut x)).collect();
            queries.extend(map.keys().cloned());
            for cutoff in 0..=8 {
                let t = Trie::builder(&keys, &values)
                    .unwrap()
                    .finish_at(cutoff)
                    .unwrap();
                let mut out = Vec::new();
                for q in &queries {
                    let want = map.range(q.clone()..).next();
                    let mut seen = Vec::new();
                    let got = t.seek(q, &mut out, |len, v| {
                        seen.push((len, v));
                        ControlFlow::Continue(())
                    });
                    assert_eq!(
                        got,
                        ControlFlow::Continue(want.map(|(_, v)| *v)),
                        "case {case} cut {cutoff} ceil {q:?}"
                    );
                    if let Some((k, _)) = want {
                        assert_eq!(&out, k, "case {case} cut {cutoff} ceil key {q:?}");
                    }
                    let prefixes: Vec<(usize, u32)> = (0..q.len())
                        .filter_map(|l| map.get(&q[..l]).map(|v| (l, *v)))
                        .collect();
                    assert_eq!(seen, prefixes, "case {case} cut {cutoff} prefixes {q:?}");
                    // Stopped at the first prefix, the descent says so.
                    if !prefixes.is_empty() {
                        assert_eq!(
                            t.seek(q, &mut out, |_, _| ControlFlow::Break(())),
                            ControlFlow::Break(())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_builder_reset_builds_what_a_new_one_does() {
        let mut x = 0x1234_5678_9abc_def1u64;
        let mut reused = TrieBuilder::new(Width::Fit).unwrap();
        for case in 0..200 {
            let n = (rng(&mut x) % 300) as usize;
            let map: BTreeMap<Vec<u8>, u32> = (0..n)
                .map(|_| (key(&mut x), (rng(&mut x) % 5_000) as u32))
                .collect();
            let keys: Vec<&[u8]> = map.keys().map(Vec::as_slice).collect();
            let values: Vec<u32> = map.values().copied().collect();
            let fresh = Trie::build(&keys, &values).unwrap();
            reused.reset(Width::Fit).unwrap();
            for (k, v) in &map {
                reused.add(k, *v).unwrap();
            }
            assert_eq!(reused.finish().unwrap(), fresh, "case {case}");
            // Values packed at the bits the largest needs.
            let most = values.iter().copied().max().unwrap_or(0);
            assert_eq!(fresh.value_width(), width_of(most), "case {case}");
        }
    }

    #[test]
    fn keys_under_a_shared_prefix_answer_as_an_ordered_map() {
        let mut x = 0x0bad_5eed_1234_5678u64;
        let shared: [&[u8]; 4] = [b"", b"\x00\x00\x00\x00\x00", b"t/0000/", b"\xff\xff"];
        for case in 0..300 {
            let p = shared[case % shared.len()];
            let n = 1 + (rng(&mut x) % 200) as usize;
            let map: BTreeMap<Vec<u8>, u32> = (0..n)
                .map(|i| {
                    let mut k = p.to_vec();
                    k.extend(key(&mut x));
                    (k, u32::try_from(i).unwrap())
                })
                .collect();
            let keys: Vec<&[u8]> = map.keys().map(Vec::as_slice).collect();
            let values: Vec<u32> = map.values().copied().collect();
            // Queries under the prefix, outside it on either side, and cut inside it.
            let mut queries: Vec<Vec<u8>> = (0..200)
                .map(|i| {
                    let mut q = match i % 4 {
                        0 => p.to_vec(),
                        1 => p.get(..p.len() / 2).unwrap_or(&[]).to_vec(),
                        _ => Vec::new(),
                    };
                    q.extend(key(&mut x));
                    q
                })
                .collect();
            queries.extend(map.keys().cloned());
            queries.push(p.to_vec());
            for cutoff in 0..=4 {
                let t = Trie::builder(&keys, &values)
                    .unwrap()
                    .finish_at(cutoff)
                    .unwrap();
                let mut bytes = Vec::new();
                t.encode(&mut bytes);
                assert_eq!(Trie::decode(&bytes).unwrap(), (t.clone(), bytes.len()));
                assert_eq!(t.least(), map.values().next().copied(), "case {case} least");
                let mut out = Vec::new();
                for q in &queries {
                    assert_eq!(t.get(q), map.get(q).copied(), "case {case} get {q:?}");
                    let floor = map.range(..=q.clone()).next_back().map(|(_, v)| *v);
                    assert_eq!(t.floor(q), floor, "case {case} floor {q:?}");
                    let want = map.range(q.clone()..).next();
                    let mut seen = Vec::new();
                    let got = t.seek(q, &mut out, |len, v| {
                        seen.push((len, v));
                        ControlFlow::Continue(())
                    });
                    assert_eq!(
                        got,
                        ControlFlow::Continue(want.map(|(_, v)| *v)),
                        "case {case} ceil {q:?}"
                    );
                    if let Some((k, _)) = want {
                        assert_eq!(&out, k, "case {case} ceil key {q:?}");
                    }
                    let prefixes: Vec<(usize, u32)> = (0..q.len())
                        .filter_map(|l| map.get(&q[..l]).map(|v| (l, *v)))
                        .collect();
                    assert_eq!(seen, prefixes, "case {case} prefixes {q:?}");
                }
            }
        }
    }

    #[test]
    fn a_truncated_trie_is_refused_typed() {
        let keys: Vec<Vec<u8>> = (0u32..500)
            .map(|n| format!("k{n:04}").into_bytes())
            .collect();
        let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let values: Vec<u32> = (0..500).collect();
        let t = Trie::build(&refs, &values).unwrap();
        let mut out = Vec::new();
        t.encode(&mut out);
        for cut in 0..out.len() {
            assert!(Trie::decode(&out[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn unordered_or_unpaired_keys_are_refused() {
        assert!(Trie::build(&[b"b", b"a"], &[1, 2]).is_err());
        assert!(Trie::build(&[b"a", b"a"], &[1, 2]).is_err());
        assert!(Trie::build(&[b"a"], &[]).is_err());
    }

    /// Every two-byte key: a sparse level of 256 nodes of 256 labels each, its last node's start
    /// 65,280 past its block's, the most 16 bits are asked to hold.
    fn every_two_bytes() -> Vec<Vec<u8>> {
        (0..=u16::MAX).map(|n| n.to_be_bytes().to_vec()).collect()
    }

    /// The trie of `keys` with each one's index as its value, every level from `cutoff` sparse.
    fn sparse_from(keys: &[Vec<u8>], cutoff: usize) -> Trie {
        let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
        let values: Vec<u32> = (0..u32::try_from(keys.len()).unwrap()).collect();
        Trie::builder(&refs, &values)
            .unwrap()
            .finish_at(cutoff)
            .unwrap()
    }

    #[test]
    fn wide_nodes_over_blocks_of_starts_agree_with_an_ordered_map() {
        // Two- and three-byte keys under three first bytes: nodes of 1 to 256 labels, the third
        // level's 768 nodes three blocks of starts, and some keys prefixes of others.
        let mut keys = Vec::new();
        for h in 0u8..3 {
            keys.push(vec![h]);
            for a in 0u8..=255 {
                if a % 3 == 0 {
                    keys.push(vec![h, a]);
                }
                let labels = if a % 5 == 0 {
                    256
                } else {
                    usize::from(a).wrapping_mul(7) % 256 + 1
                };
                for b in 0..labels {
                    keys.push(vec![h, a, u8::try_from(b).unwrap()]);
                }
            }
        }
        keys.sort();
        let map: BTreeMap<Vec<u8>, u32> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), u32::try_from(i).unwrap()))
            .collect();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut queries: Vec<Vec<u8>> = (0..3_000)
            .map(|_| {
                let len = (rng(&mut x) % 5) as usize;
                (0..len).map(|_| (rng(&mut x) % 4) as u8 * 85).collect()
            })
            .collect();
        queries.extend(keys.iter().step_by(37).cloned());
        queries.extend(keys.iter().step_by(41).map(|k| {
            let mut k = k.clone();
            k.push(0);
            k
        }));
        for cutoff in 0..=2 {
            let t = sparse_from(&keys, cutoff);
            assert!(t.sparse.starts.blocks.len() >= 4, "cut {cutoff}");
            let mut out = Vec::new();
            t.encode(&mut out);
            assert_eq!(Trie::decode(&out).unwrap(), (t.clone(), out.len()));
            for q in &queries {
                assert_eq!(t.get(q), map.get(q).copied(), "cut {cutoff} get {q:?}");
                let floor = map.range(..=q.clone()).next_back().map(|(_, v)| *v);
                assert_eq!(t.floor(q), floor, "cut {cutoff} floor {q:?}");
                let mut found = Vec::new();
                let ceil = map.range(q.clone()..).next();
                assert_eq!(
                    t.ceil(q, &mut found),
                    ceil.map(|(_, v)| *v),
                    "cut {cutoff} ceil {q:?}"
                );
                if let Some((k, _)) = ceil {
                    assert_eq!(&found, k, "cut {cutoff} ceil key {q:?}");
                }
            }
        }
        // At the bound: the last node of a full block starts 65,280 labels past its block.
        let full = every_two_bytes();
        let t = sparse_from(&full, 0);
        // The root's 256 labels, then the second level's 256 nodes of 256 each: node 255 is the
        // second level's 255th, its block's extras the 255 labels past one of each node before,
        // and its last extra where node 256, the next block's first, starts.
        assert_eq!(t.sparse.starts.extra.get(255), Some(&65_025));
        assert_eq!(t.sparse.starts.extra.get(256), Some(&65_280));
        assert_eq!(t.sparse.starts.at(255), Some(65_280));
        assert_eq!(t.sparse.starts.range(255), Some((65_280, 65_536)));
        assert_eq!(t.sparse.starts.at(256), Some(65_536));
        assert_eq!(t.sparse.starts.range(256), Some((65_536, 65_792)));
        assert_eq!(t.sparse.starts.range(257), None);
        for (i, k) in full.iter().enumerate().step_by(97) {
            assert_eq!(t.floor(k), Some(u32::try_from(i).unwrap()));
        }
    }

    #[test]
    fn a_node_of_more_labels_than_bytes_is_refused_typed() {
        // Every two-byte key, all sparse, then the second level's node starts cleared but the
        // first: one node of 65,536 labels, which no trie can hold.
        let t = sparse_from(&every_two_bytes(), 0);
        let mut out = Vec::new();
        t.encode(&mut out);
        // The sparse part's has-child and node-start vectors each begin with their length, the
        // root's 256 labels and the second level's 65,536 bits, then their rank block, 8 words;
        // the second such header is the node starts'.
        let header: Vec<u8> = [65_792u64, 8]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let at = out
            .windows(header.len())
            .enumerate()
            .filter(|(_, w)| *w == header.as_slice())
            .map(|(i, _)| i)
            .nth(1)
            .unwrap();
        // Past the root's four words, the second level's first node start kept, the rest
        // cleared.
        let words = at + header.len() + 4 * 8;
        for w in 0..1024 {
            let word: u64 = if w == 0 { 1 } else { 0 };
            out[words + w * 8..words + w * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        assert!(matches!(
            Trie::decode(&out),
            Err(Error::Corruption {
                why: Malformed::OutOfRange,
                ..
            })
        ));
        // And the node starts themselves refuse a node of 257 labels, whatever surrounds them.
        let mut bits = vec![false; 300];
        bits[0] = true;
        bits[257] = true;
        assert!(matches!(
            Starts::of(&Bits::new(&bits, SPARSE_BLOCK).unwrap(), bits.len()),
            Err(Error::Corruption {
                why: Malformed::OutOfRange,
                ..
            })
        ));
    }

    /// FNV-1a 64 of `bytes`.
    fn fnv(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }

    #[test]
    fn the_bytes_written_are_format_sevens() {
        // The sparse levels written as one sequence each (the trunk image's format 7,
        // research/67): the tries of these keys encode to these bytes, which what a reader keeps
        // beside them (each node's start) does not change.
        let mut out = Vec::new();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let small: BTreeMap<Vec<u8>, u32> = (0..300).map(|i| (key(&mut x), i)).collect();
        let small: Vec<Vec<u8>> = small.into_keys().collect();
        for cutoff in 0..=8 {
            sparse_from(&small, cutoff).encode(&mut out);
        }
        let numbers: Vec<Vec<u8>> = (0u64..5_000)
            .map(|n| {
                let mut k = (n * 3).to_be_bytes().to_vec();
                k.extend_from_slice(b"00000000");
                k
            })
            .collect();
        let mut names: Vec<Vec<u8>> = (0u64..3_000)
            .map(|n| format!("tenant-{:02}/photos/{n:06}.jpg", n % 37).into_bytes())
            .collect();
        names.sort();
        for keys in [&numbers, &names, &every_two_bytes()] {
            let refs: Vec<&[u8]> = keys.iter().map(Vec::as_slice).collect();
            let values: Vec<u32> = (0..u32::try_from(keys.len()).unwrap()).collect();
            Trie::build(&refs, &values).unwrap().encode(&mut out);
            sparse_from(keys, 0).encode(&mut out);
        }
        assert_eq!(
            (out.len(), fnv(&out)),
            (716_905, 12_837_620_006_256_584_756)
        );
    }
}
