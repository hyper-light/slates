//! The memtable's memory: RocksDB's `memory/arena.{h,cc}` (docs/research/24 §4.1), rebuilt so
//! that one writer and many readers share it without `unsafe`.
//!
//! RocksDB's `Arena` hands out raw `char*` from blocks it allocates, and the skiplist links
//! nodes by raw pointer. Here a block is a slice of `AtomicU64` words, allocated zeroed: from a
//! page up mapped by the OS, zero until written and resident only where written (port/mmap.rs,
//! RocksDB's `MemMapping::AllocateLazyZeroed`), below it on the heap and never moved or freed while the arena lives; an allocation is an [`Addr`],
//! the block's number and a byte offset in it. The writer stores bytes into words with relaxed
//! atomic stores; a reader sees them once it has loaded, with acquire ordering, the link that
//! the writer stored with release ordering after writing them — the orderings of RocksDB's
//! `InlineSkipList` [R memtable/inlineskiplist.h:379-407].
//!
//! Blocks are found through a segmented directory whose segment `s` holds `2^s` slots,
//! each segment allocated once when first needed and never moved, so a reader indexes it while
//! the writer adds blocks (the resizable array of Dechev, Pirkelbauer and Stroustrup, "Lock-free
//! dynamically resizable arrays", OPODIS 2006, with a single writer in place of their CAS).
//!
//! The accounting is RocksDB's: an inline first block of 2 KiB, blocks of the configured size,
//! an allocation larger than a quarter block given a block of its own, aligned allocations from
//! a block's front and unaligned ones from its back. The alignment unit is the 8-byte word,
//! RocksDB's `alignof(max_align_t)` on aarch64 macOS and x86_64 Linux being 8 and 16; nothing
//! the port stores needs more than a word. Huge-page blocks (`MAP_HUGETLB`, Linux only) are not
//! used.

use crate::port::mmap::{LazyZeroed, page_size};
use std::cmp::Ordering;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use crate::error::Error;

/// `Arena::kInlineSize` [R memory/arena.h:31]: the first block's size.
pub const INLINE_SIZE: usize = 2048;
/// `Arena::kMinBlockSize` [R memory/arena.h:32].
pub const MIN_BLOCK_SIZE: usize = 4096;
/// `Arena::kMaxBlockSize` [R memory/arena.h:33].
pub const MAX_BLOCK_SIZE: usize = 2 << 30;
/// The alignment of `allocate_aligned`: one storage word.
pub const ALIGN_UNIT: usize = WORD;

/// Bytes in a storage word.
const WORD: usize = 8;
/// Bits of an address that hold the byte offset in its block: 64 GiB, above the largest
/// allocation the memtable makes (a key under 4 GiB and a value under 4 GiB, docs/research/24
/// §1.4, with their headers).
const OFFSET_BITS: u32 = 36;
/// Directory segments: segment `s` holds `2^s` slots, so `s` runs to the top bit of the largest
/// block number, which an address's bits above [`OFFSET_BITS`] carry: one segment a bit, and one
/// for block 0. The directory then holds exactly the blocks an address can name.
const DIRECTORY_SEGMENTS: usize = (u64::BITS - OFFSET_BITS + 1) as usize;

/// The place of an allocation: block number above [`OFFSET_BITS`], byte offset below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Addr(u64);

impl Addr {
    fn new(block: usize, offset: usize) -> Result<Self, Error> {
        let block = u64::try_from(block).map_err(|_| too_large())?;
        let offset = u64::try_from(offset).map_err(|_| too_large())?;
        if offset >> OFFSET_BITS != 0 || block >> (u64::BITS - OFFSET_BITS) != 0 {
            return Err(too_large());
        }
        Ok(Self((block << OFFSET_BITS) | offset))
    }

    /// The address as a word, for storing in a link.
    pub const fn to_raw(self) -> u64 {
        self.0
    }

    /// An address read back from a link.
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    fn block(self) -> usize {
        usize::try_from(self.0 >> OFFSET_BITS).unwrap_or(usize::MAX)
    }

    fn offset(self) -> usize {
        usize::try_from(self.0 & ((1 << OFFSET_BITS) - 1)).unwrap_or(usize::MAX)
    }

    /// The address `bytes` further on in the same block.
    pub fn offset_by(self, bytes: usize) -> Result<Self, Error> {
        Self::new(
            self.block(),
            self.offset().checked_add(bytes).ok_or_else(too_large)?,
        )
    }
}

fn too_large() -> Error {
    Error::LimitExceeded {
        what: "arena address",
        limit: 1 << OFFSET_BITS,
    }
}

/// A block's words: below a page on the heap, zeroed as they are allocated; from a page up mapped
/// zeroed by the OS and made resident only where written (port/mmap.rs, RocksDB's
/// `MemMapping::AllocateLazyZeroed`), so a large block costs memory only as the memtable fills it.
#[derive(Debug)]
enum Block {
    Heap(Box<[AtomicU64]>),
    Mapped(LazyZeroed),
}

impl Block {
    /// `words` zeroed words, or the allocator's or OS's refusal.
    fn zeroed(words: usize) -> Result<Self, Error> {
        let refused = || Error::LimitExceeded {
            what: "memtable arena block (the allocator refused it)",
            limit: u64::try_from(words.saturating_mul(WORD)).unwrap_or(u64::MAX),
        };
        if words.saturating_mul(WORD) >= page_size() {
            return LazyZeroed::allocate(words).map(Self::Mapped);
        }
        let mut heap = Vec::new();
        heap.try_reserve_exact(words).map_err(|_| refused())?;
        heap.extend(std::iter::repeat_with(|| AtomicU64::new(0)).take(words));
        Ok(Self::Heap(heap.into_boxed_slice()))
    }

    #[inline]
    fn words(&self) -> &[AtomicU64] {
        match self {
            Self::Heap(words) => words,
            Self::Mapped(mapping) => mapping.words(),
        }
    }
}
type Segment = Box<[OnceLock<Block>]>;

/// The blocks, shared by the writer and every reader.
#[derive(Debug)]
pub struct Store {
    segments: [OnceLock<Segment>; DIRECTORY_SEGMENTS],
    /// This store's identity, which its [`Allocator`] checks on every call: an allocator
    /// handed another arena's store would otherwise write into that arena's published nodes.
    id: u64,
}

/// The source of store identities. Relaxed suffices: only uniqueness matters, and
/// `fetch_add` on one atomic is totally ordered.
static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(0);

/// The segment and the slot in it of block `i`.
fn locate(i: usize) -> Option<(usize, usize)> {
    // Block i is the (i + 1)-th slot: segment s holds slots 2^s to 2^(s+1) − 1 of that count.
    let j = i.checked_add(1)?;
    let s = usize::try_from(usize::BITS.checked_sub(1)?.checked_sub(j.leading_zeros())?).ok()?;
    let first = 1usize.checked_shl(u32::try_from(s).ok()?)?;
    Some((s, j.checked_sub(first)?))
}

impl Store {
    fn new() -> Self {
        Self {
            segments: std::array::from_fn(|_| OnceLock::new()),
            id: NEXT_STORE_ID.fetch_add(1, AtomicOrdering::Relaxed),
        }
    }

    /// The words of block `i`, once the writer has created it.
    #[inline]
    fn block(&self, i: usize) -> Option<&[AtomicU64]> {
        let (s, slot) = locate(i)?;
        Some(self.segments.get(s)?.get()?.get(slot)?.get()?.words())
    }

    /// The slot for block `i`, creating its segment if needed. Writer only.
    fn slot(&self, i: usize) -> Option<&OnceLock<Block>> {
        let (s, slot) = locate(i)?;
        let segment = self.segments.get(s)?.get_or_init(|| {
            let len = 1usize
                .checked_shl(u32::try_from(s).unwrap_or(0))
                .unwrap_or(0);
            (0..len).map(|_| OnceLock::new()).collect()
        });
        segment.get(slot)
    }

    /// The word at a word-aligned address.
    #[inline]
    pub fn word(&self, addr: Addr) -> Option<&AtomicU64> {
        self.block(addr.block())?.get(addr.offset() / WORD)
    }

    /// The word `index` words past a word-aligned address.
    #[inline]
    pub fn word_at(&self, addr: Addr, index: usize) -> Option<&AtomicU64> {
        self.block(addr.block())?
            .get((addr.offset() / WORD).checked_add(index)?)
    }

    /// The words `[addr, addr + words)` of a word-aligned address.
    #[inline]
    pub fn words(&self, addr: Addr, words: usize) -> Option<&[AtomicU64]> {
        let start = addr.offset() / WORD;
        self.block(addr.block())?
            .get(start..start.checked_add(words)?)
    }

    /// The words from the word-aligned `addr` to the end of its block: a node's words, found
    /// once so its fields are read without resolving the block again.
    #[inline]
    pub fn words_from(&self, addr: Addr) -> Option<&[AtomicU64]> {
        self.block(addr.block())?.get(addr.offset() / WORD..)
    }

    /// Appends the `len` bytes at `addr` to `out`.
    pub fn read_bytes(&self, addr: Addr, len: usize, out: &mut Vec<u8>) -> Result<(), Error> {
        let block = self.block(addr.block()).ok_or_else(unmapped)?;
        read_words(block, addr.offset(), len, out)
    }

    /// Compares the `len` bytes at `addr` with `other` as `memcmp` then length, the order of
    /// RocksDB's `Slice::compare`.
    #[inline]
    pub fn compare_bytes(&self, addr: Addr, len: usize, other: &[u8]) -> Ordering {
        match self.block(addr.block()) {
            Some(block) => compare_words(block, addr.offset(), len, other),
            None => Ordering::Less,
        }
    }
}

/// The eight bytes at byte offset `at` of `words` as a little-endian u64, whether or not `at`
/// is on a word; bytes past the words read as zero.
#[inline]
fn u64_at(words: &[AtomicU64], at: usize) -> u64 {
    let index = at / WORD;
    let load = |i: usize| words.get(i).map_or(0, |w| w.load(AtomicOrdering::Relaxed));
    let shift = u32::try_from((at % WORD).saturating_mul(8)).unwrap_or(0);
    let low = load(index);
    if shift == 0 {
        return low;
    }
    let high = load(index.saturating_add(1));
    (low >> shift)
        | high
            .checked_shl(u64::BITS.saturating_sub(shift))
            .unwrap_or(0)
}

/// Appends bytes `[from, from + len)` of `words` (each little-endian) to `out`.
pub fn read_words(
    words: &[AtomicU64],
    from: usize,
    len: usize,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let end = from.checked_add(len).ok_or_else(unmapped)?;
    if end > words.len().saturating_mul(WORD) {
        return Err(unmapped());
    }
    let mut at = from;
    while at < end {
        let n = end.saturating_sub(at).min(WORD);
        let bytes = u64_at(words, at).to_le_bytes();
        out.extend_from_slice(bytes.get(..n).unwrap_or(&[]));
        at = at.saturating_add(n);
    }
    Ok(())
}

/// Compares bytes `[from, from + len)` of `words` with `other` as `memcmp` then length, eight
/// bytes at a time: byte i of a little-endian word is bits 8i..8i+8, so the byte-swapped word
/// orders as the bytes do, and bytes past the compared run are shifted out. Bytes past the
/// words compare as zero; callers pass runs inside what they wrote.
#[inline]
pub fn compare_words(words: &[AtomicU64], from: usize, len: usize, other: &[u8]) -> Ordering {
    let common = len.min(other.len());
    let mut done = 0usize;
    while done < common {
        let n = common.saturating_sub(done).min(WORD);
        let mut buf = [0u8; WORD];
        if let Some(chunk) = other.get(done..done.saturating_add(n)) {
            for (dst, src) in buf.iter_mut().zip(chunk) {
                *dst = *src;
            }
        }
        let theirs = u64::from_be_bytes(buf);
        let shift = u32::try_from(WORD.saturating_sub(n).saturating_mul(8)).unwrap_or(0);
        let mine = u64_at(words, from.saturating_add(done)).swap_bytes();
        let mine = mine
            .checked_shr(shift)
            .and_then(|v| v.checked_shl(shift))
            .unwrap_or(0);
        match mine.cmp(&theirs) {
            Ordering::Equal => {}
            unequal => return unequal,
        }
        done = done.saturating_add(n);
    }
    len.cmp(&other.len())
}

/// Whole words holding `bytes`.
fn round_up_words(bytes: usize) -> usize {
    (bytes / WORD).saturating_add(usize::from(!bytes.is_multiple_of(WORD)))
}

fn unmapped() -> Error {
    Error::InvalidArgument {
        what: "arena address outside every block",
    }
}

/// `Arena`: the blocks ([`Store`]) and the allocator that fills them, owned together.
///
/// Readers borrow the store (`&Store`, [`Arena::store`]); the one writer borrows the
/// allocator mutably. [`Arena::split`] hands out both at once, so a writer on one thread and
/// readers on others (under `std::thread::scope`) share the arena through borrows of its owner:
/// the store is `Sync` because its blocks are atomic words and its directory slots are
/// `OnceLock`s, and the borrow checker holds the single-writer rule.
#[derive(Debug)]
pub struct Arena {
    store: Store,
    alloc: Allocator,
}

/// The allocating side of an arena: RocksDB's `Arena` less its blocks. Every method that
/// allocates or writes takes `&mut self`, so there is one writer.
#[derive(Debug)]
pub struct Allocator {
    /// The [`Store`] this allocator fills.
    store_id: u64,
    block_size: usize,
    memory_limit: Option<usize>,
    /// The block aligned and unaligned allocations come from, and their positions in it.
    current: usize,
    aligned_ptr: usize,
    unaligned_ptr: usize,
    alloc_bytes_remaining: usize,
    /// Blocks other than the inline one, and how many of them held one large allocation.
    blocks: usize,
    irregular_block_num: usize,
    next_block: usize,
    blocks_memory: usize,
}

/// The directory bytes RocksDB counts per block as `sizeof(char*)`: here a directory slot.
const SLOT_BYTES: usize = std::mem::size_of::<OnceLock<Block>>();

impl Arena {
    /// An arena of `block_size` blocks (optimized as RocksDB does) that refuses to allocate a
    /// block that would take its allocated bytes past `memory_limit`.
    pub fn new(block_size: usize, memory_limit: Option<usize>) -> Result<Self, Error> {
        let store = Store::new();
        let alloc = Allocator::new(&store, block_size, memory_limit)?;
        Ok(Self { store, alloc })
    }

    /// `Arena::OptimizeBlockSize` [R memory/arena.cc:18-30].
    pub fn optimize_block_size(block_size: usize) -> usize {
        Allocator::optimize_block_size(block_size)
    }

    /// The blocks, for readers.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// The allocator.
    pub fn allocator(&self) -> &Allocator {
        &self.alloc
    }

    /// The blocks for readers and the allocator for the one writer, borrowed together.
    pub fn split(&mut self) -> (&Store, &mut Allocator) {
        (&self.store, &mut self.alloc)
    }

    /// `Allocate`.
    pub fn allocate(&mut self, bytes: usize) -> Result<Addr, Error> {
        self.alloc.allocate(&self.store, bytes)
    }

    /// `AllocateAligned`.
    pub fn allocate_aligned(&mut self, bytes: usize) -> Result<Addr, Error> {
        self.alloc.allocate_aligned(&self.store, bytes)
    }

    /// Writes `bytes` at `addr`.
    pub fn write(&mut self, addr: Addr, bytes: &[u8]) -> Result<(), Error> {
        self.alloc.write(&self.store, addr, bytes)
    }

    pub fn approximate_memory_usage(&self) -> usize {
        self.alloc.approximate_memory_usage()
    }

    pub fn memory_allocated_bytes(&self) -> usize {
        self.alloc.memory_allocated_bytes()
    }

    pub fn allocated_and_unused(&self) -> usize {
        self.alloc.allocated_and_unused()
    }

    pub fn irregular_block_num(&self) -> usize {
        self.alloc.irregular_block_num()
    }

    pub fn block_size(&self) -> usize {
        self.alloc.block_size()
    }

    pub fn is_in_inline_block(&self) -> bool {
        self.alloc.is_in_inline_block()
    }
}

impl Allocator {
    /// `Arena::OptimizeBlockSize` [R memory/arena.cc:18-30]: clamped to
    /// [`MIN_BLOCK_SIZE`, `MAX_BLOCK_SIZE`] and rounded up to the alignment unit.
    pub fn optimize_block_size(block_size: usize) -> usize {
        let b = block_size.clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
        round_up_words(b).saturating_mul(WORD)
    }

    /// An allocator over `store` (which must be empty), with its inline first block.
    fn new(store: &Store, block_size: usize, memory_limit: Option<usize>) -> Result<Self, Error> {
        let mut arena = Self {
            store_id: store.id,
            block_size: Self::optimize_block_size(block_size),
            memory_limit,
            current: 0,
            aligned_ptr: 0,
            unaligned_ptr: INLINE_SIZE,
            alloc_bytes_remaining: INLINE_SIZE,
            blocks: 0,
            irregular_block_num: 0,
            next_block: 0,
            blocks_memory: 0,
        };
        arena.current = arena.new_block(store, INLINE_SIZE)?;
        Ok(arena)
    }

    /// Refuses a store other than this allocator's own.
    fn check(&self, store: &Store) -> Result<(), Error> {
        if store.id == self.store_id {
            Ok(())
        } else {
            Err(Error::InvalidArgument {
                what: "arena allocator used with another arena's store",
            })
        }
    }

    /// Creates block `next_block` of `bytes` (rounded up to whole words) and returns its number.
    fn new_block(&mut self, store: &Store, bytes: usize) -> Result<usize, Error> {
        self.check(store)?;
        let words = round_up_words(bytes);
        let size = words.saturating_mul(WORD);
        let total = self.blocks_memory.checked_add(size).ok_or_else(too_large)?;
        if let Some(limit) = self.memory_limit
            && total > limit
        {
            return Err(Error::LimitExceeded {
                what: "memtable arena bytes",
                limit: u64::try_from(limit).unwrap_or(u64::MAX),
            });
        }
        let block = Block::zeroed(words)?;
        let index = self.next_block;
        let slot = store.slot(index).ok_or_else(too_large)?;
        if slot.set(block).is_err() {
            return Err(Error::InvalidArgument {
                what: "arena block created twice",
            });
        }
        self.next_block = index.checked_add(1).ok_or_else(too_large)?;
        self.blocks_memory = total;
        if index > 0 {
            self.blocks = self.blocks.saturating_add(1);
        }
        Ok(index)
    }

    /// `AllocateFallback` [R memory/arena.cc:53-93].
    fn allocate_fallback(
        &mut self,
        store: &Store,
        bytes: usize,
        aligned: bool,
    ) -> Result<Addr, Error> {
        if bytes > self.block_size / 4 {
            self.irregular_block_num = self.irregular_block_num.saturating_add(1);
            // More than a quarter block: a block of its own, so the current block's leftover is
            // not wasted.
            let block = self.new_block(store, bytes)?;
            return Addr::new(block, 0);
        }
        let size = self.block_size;
        let block = self.new_block(store, size)?;
        self.current = block;
        self.alloc_bytes_remaining = size.saturating_sub(bytes);
        if aligned {
            self.aligned_ptr = bytes;
            self.unaligned_ptr = size;
            Addr::new(block, 0)
        } else {
            self.aligned_ptr = 0;
            self.unaligned_ptr = size.saturating_sub(bytes);
            Addr::new(block, self.unaligned_ptr)
        }
    }

    /// `Allocate` [R memory/arena.h:99-108]: `bytes` from the back of the current block.
    pub fn allocate(&mut self, store: &Store, bytes: usize) -> Result<Addr, Error> {
        if bytes == 0 {
            return Err(Error::InvalidArgument {
                what: "zero-byte arena allocation",
            });
        }
        if bytes <= self.alloc_bytes_remaining {
            self.unaligned_ptr = self.unaligned_ptr.saturating_sub(bytes);
            self.alloc_bytes_remaining = self.alloc_bytes_remaining.saturating_sub(bytes);
            return Addr::new(self.current, self.unaligned_ptr);
        }
        self.allocate_fallback(store, bytes, false)
    }

    /// `AllocateAligned` [R memory/arena.cc:109-143]: `bytes` from the front of the current
    /// block, starting on a word.
    pub fn allocate_aligned(&mut self, store: &Store, bytes: usize) -> Result<Addr, Error> {
        let current_mod = self.aligned_ptr % ALIGN_UNIT;
        let slop = if current_mod == 0 {
            0
        } else {
            ALIGN_UNIT.saturating_sub(current_mod)
        };
        let needed = bytes.checked_add(slop).ok_or_else(too_large)?;
        if needed <= self.alloc_bytes_remaining {
            let result = Addr::new(self.current, self.aligned_ptr.saturating_add(slop))?;
            self.aligned_ptr = self.aligned_ptr.saturating_add(needed);
            self.alloc_bytes_remaining = self.alloc_bytes_remaining.saturating_sub(needed);
            return Ok(result);
        }
        // A new block starts on a word.
        self.allocate_fallback(store, bytes, true)
    }

    /// Writes `bytes` at `addr`. A word shared with a neighbouring allocation is rewritten with
    /// that neighbour's bytes unchanged: only this writer stores, so no update is lost, and a
    /// reader's load sees the neighbour's bytes the same before and after.
    pub fn write(&mut self, store: &Store, addr: Addr, bytes: &[u8]) -> Result<(), Error> {
        self.check(store)?;
        let block = store.block(addr.block()).ok_or_else(unmapped)?;
        let mut offset = addr.offset();
        let mut rest = bytes;
        while !rest.is_empty() {
            let word = block.get(offset / WORD).ok_or_else(unmapped)?;
            let from = offset % WORD;
            let take = WORD.saturating_sub(from).min(rest.len());
            let (chunk, tail) = rest.split_at_checked(take).ok_or_else(unmapped)?;
            if take == WORD {
                // A whole word of this allocation: no neighbour's bytes to keep.
                let whole = <[u8; WORD]>::try_from(chunk).map_err(|_| unmapped())?;
                word.store(u64::from_le_bytes(whole), AtomicOrdering::Relaxed);
            } else {
                let mut current = word.load(AtomicOrdering::Relaxed).to_le_bytes();
                for (dst, src) in current.iter_mut().skip(from).zip(chunk) {
                    *dst = *src;
                }
                word.store(u64::from_le_bytes(current), AtomicOrdering::Relaxed);
            }
            rest = tail;
            offset = offset.saturating_add(take);
        }
        Ok(())
    }

    /// `ApproximateMemoryUsage` [R memory/arena.h:62-65]: allocated bytes plus the directory
    /// slot of each block, less what the current block has left.
    pub fn approximate_memory_usage(&self) -> usize {
        self.blocks_memory
            .saturating_add(self.blocks.saturating_mul(SLOT_BYTES))
            .saturating_sub(self.alloc_bytes_remaining)
    }

    /// `MemoryAllocatedBytes`: the bytes of every block, the inline one included.
    pub fn memory_allocated_bytes(&self) -> usize {
        self.blocks_memory
    }

    /// `AllocatedAndUnused`: what the current block has left.
    pub fn allocated_and_unused(&self) -> usize {
        self.alloc_bytes_remaining
    }

    /// `IrregularBlockNum`: blocks holding one allocation above a quarter block.
    pub fn irregular_block_num(&self) -> usize {
        self.irregular_block_num
    }

    /// `BlockSize`.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// `IsInInlineBlock`: no block but the inline one yet.
    pub fn is_in_inline_block(&self) -> bool {
        self.blocks == 0
    }
}
