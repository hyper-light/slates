//! The store's page cache (docs/design/engine-structure.md §5, step E8): verified node pages'
//! payloads kept in memory, so a point read that hits costs no I/O. With uncached I/O the device
//! serves every read the cache does not, as SplinterDB's own cache does (research/34 §1).
//!
//! Eviction is S3-FIFO (Yang et al., "FIFO queues are all you need for cache eviction", SOSP
//! 2023): a small FIFO that new pages enter, a main FIFO, and a ghost FIFO of addresses recently
//! evicted from the small one. A page read twice while in the small queue moves to main; one
//! evicted from it is remembered in the ghost, and enters main directly if read again; a page
//! leaving main with reads left goes round again with one read fewer. The parameters are the
//! reference implementation's (libCacheSim `S3FIFO.c`: small queue 10% of the cache, ghost 90%,
//! promotion at two reads, a page's count capped at 3).
//!
//! Pages lie in chunks of memory. A new chunk has room for as many pages as all before it together
//! (a table that doubles, CLRS §17.4), so a cache that grows to `n` pages allocates O(log n)
//! times; it holds at most one tuning step of the limit (`TUNE_STEP_PERCENT`, Luo and Carey, PVLDB
//! 2021 §5.4), so a step that lowers the limit gives back whole chunks, and at most what the limit
//! has left. A chunk's pages become slots in idle time (`reserve`), each written once as it is
//! made, as many as the reads since the last such time asked for; a read that finds none makes
//! its own (`make_now`), and leaves its page out only at the limit or while a chunk retires. The
//! cache takes the memory its reads use, not its limit (measured: reserving up to a 32 GiB limit
//! as reads came made 7.74 allocations and 31.7 KB of allocation a get over a 10M-key store, and
//! more than halved its gets; research/50 §9). Resizing sets the page limit;
//! a cache above it is brought down a few pages an operation (`trim`) by the rule above, and the
//! chunks above it are given back the last first, its pages dropped. A freed slot is the next
//! page's, so a warm cache allocates nothing. The index and the ghost grow a few buckets a write
//! (`util::incmap`), so no read rehashes them.
//!
//! The ghost names the addresses evicted within the last `ghost_cap` evictions. A ghost hit
//! leaves a hole rather than keeping an older address longer, as libCacheSim's ghost FIFO does,
//! so the ghost may name fewer: what keeps it a map with no queue to compact.
//!
//! Every queue and map is bounded by the cache's page count, and the slot table by twice the
//! largest limit: a loan's fresh memory goes in the slots of chunks whose memory was given back
//! before it adds any (docs/research/65 §5). A page written at an address is dropped from the
//! cache (`forget`), so an address reused after its extent is freed never reads the page it held
//! before.

use crate::port::mmap::PageBuf;
use std::num::{NonZeroU32, NonZeroUsize};

use crate::shard_db::TUNE_STEP_PERCENT;
use crate::util::incmap::{IncMap, SWEEP};

/// Cited: the small queue's share of the cache, in tenths (libCacheSim `S3FIFO.c`
/// `small-size-ratio=0.10`).
const SMALL_TENTHS: usize = 1;
/// Cited: the ghost queue's length against the cache's, in tenths (libCacheSim
/// `ghost-size-ratio=0.90`).
const GHOST_TENTHS: usize = 9;
/// Cited: reads in the small queue that move a page to main (libCacheSim
/// `move-to-main-threshold=2`).
const PROMOTE: u8 = 2;
/// Cited: a page's read count's cap (libCacheSim `S3FIFO.c`, `MIN(freq, 3)`).
const MAX_FREQ: u8 = 3;

/// Queue steps an insert may take to free a slot: S3-FIFO's amortized cost of an eviction. A
/// page moves from small to main at most once and goes round main at most `MAX_FREQ` times
/// before it is freed, each a step, so evictions take `MAX_FREQ + 2` steps a slot on average
/// (Yang et al., SOSP '23, §4); a bound of that keeps up with inserts over time while no one
/// insert sweeps the queues.
const EVICT_STEPS: usize = MAX_FREQ as usize + 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Queue {
    Small,
    Main,
}

/// A slot's index plus one, so an absent queue link occupies no extra word.
fn slot_link(i: usize) -> Option<NonZeroUsize> {
    i.checked_add(1).and_then(NonZeroUsize::new)
}

fn slot_index(link: NonZeroUsize) -> Option<usize> {
    link.get().checked_sub(1)
}

/// A queue's newest and oldest slots. Each live slot is linked once, so forgetting an address
/// removes its entry without leaving a queue to compact on a later admission.
#[derive(Debug, Default)]
struct Fifo {
    newest: Option<NonZeroUsize>,
    oldest: Option<NonZeroUsize>,
}

/// One cached page's slot.
#[derive(Clone, Copy, Debug)]
struct Slot {
    address: u64,
    freq: u8,
    queue: Queue,
    /// Whether the slot holds a page.
    live: bool,
    newer: Option<NonZeroUsize>,
    older: Option<NonZeroUsize>,
    /// Its page's place: the chunk, the byte it starts at there, and its payload's length.
    chunk: usize,
    at: usize,
    len: usize,
}

/// Memory a cache lends a maintenance worker for the pages its job writes ([`Cache::lend`]):
/// the worker keeps each page's payload in it as it queues the page ([`Kept::keep`]), a page at
/// each step of the cache's page size, and the shard's cache takes it back filled ([`Cache::adopt`]).
/// A worker's pages are so cached where they were written, as a SplinterDB pack fills pages in its
/// cache (clockcache.c `clockcache_alloc`), by handing the memory over and back. The memory is
/// chunks whose pages were all forgotten, already mapped, before any fresh memory; its room is
/// taken when lent, so keeping a page allocates nothing.
#[derive(Debug)]
pub struct Kept {
    page: usize,
    /// Fresh pages lent beyond the chunks', which the cache counts until they come back.
    fresh: usize,
    bufs: Vec<Loaned>,
    /// The buffer the next page goes in.
    at: usize,
    pages: Vec<(u64, usize)>,
    /// Pages written that the room lent could not hold.
    missed: usize,
}

/// One buffer of a loan: a chunk's memory lent again, or fresh.
#[derive(Debug)]
struct Loaned {
    chunk: Option<usize>,
    buf: PageBuf,
}

impl Kept {
    /// Keeps `payload`, written at `address`, while the room lent holds a page more: each buffer
    /// filled in turn, within the capacity it was lent with.
    pub fn keep(&mut self, address: u64, payload: &[u8]) {
        if payload.len() <= self.page {
            while let Some(l) = self.bufs.get_mut(self.at) {
                let Some(end) = l.buf.len().checked_add(self.page) else {
                    break;
                };
                if end <= l.buf.capacity()
                    && l.buf.extend_from_slice(payload)
                    && l.buf.resize(end, 0)
                {
                    self.pages.push((address, payload.len()));
                    return;
                }
                self.at = self.at.saturating_add(1);
            }
        }
        self.missed = self.missed.saturating_add(1);
    }

    /// The pages kept.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// Whether no page was kept.
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// What a chunk's memory holds: pages made for reads, a loan's pages, or memory waiting to be lent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    /// Made for reads: its free slots are on the free list.
    Grown,
    /// A loan came back in it, this many of its pages live. Its freed slots stay its own, so once
    /// every page is forgotten, as a branch merged away forgets its pages together, the whole
    /// chunk is lent again.
    Kept(usize),
    /// Out on loan again.
    Lent,
    /// Every page forgotten: its memory, already mapped, kept for the next loan.
    Spare,
    /// Its memory given back, to a lowered limit or as spare beyond the loans' need; its slots
    /// kept for a later loan's fresh memory.
    Hollow,
    /// Hollow, and out on loan in fresh memory: counted as fresh room until it comes back.
    Refilled,
}

/// A page cache of at most `limit` pages.
#[derive(Debug)]
pub struct Cache {
    page: usize,
    limit: usize,
    /// The pages' memory (the module's rule), each chunk's first slot, and its pages.
    chunks: Vec<PageBuf>,
    starts: Vec<usize>,
    sizes: Vec<usize>,
    slots: Vec<Slot>,
    map: IncMap,
    small: Fifo,
    main: Fifo,
    /// The pages each queue holds.
    small_live: usize,
    main_live: usize,
    /// Evicted pages' addresses, each with the eviction's sequence: in the ghost while within
    /// the last `ghost_cap` evictions.
    ghosts: IncMap,
    ghost_seq: u64,
    /// Slots made that hold no page.
    free: Vec<usize>,
    /// Pages inserted since `reserve` last caught up: the free slots it keeps ready.
    wanted: usize,
    /// The memory the machine has free for a new allocation, asked before each chunk
    /// (hyper-rt's `Facts::memory_available_now`, §4.2's rule for growth), when the owner gives
    /// one; chunks the machine could not spare.
    available: Option<fn() -> Option<u64>>,
    refused: u64,
    /// Slots a read made itself because idle time had not made one.
    grown: u64,
    /// While the chunks have room for more pages than the limit: the last chunk's first slot,
    /// whose pages `trim` drops before the chunk is given back, and the next of its slots to
    /// look at. Its free slots are set aside meanwhile, kept in case the limit grows back first.
    retiring: Option<usize>,
    retire_at: usize,
    retired: Vec<usize>,
    hits: u64,
    misses: u64,
    /// The most queue steps one eviction took.
    evict_steps_most: u64,
    /// Inserts left out because their bounded eviction freed no slot.
    skipped: u64,
    /// Fresh pages of room lent to workers and not yet taken back ([`Self::lend`]).
    lent: usize,
    /// The most pages the loans have had out at once, fresh and spare ([`Self::shed_spare`]).
    out_most: usize,
    /// Each chunk's state, the chunks spare for loans, and the hollow chunks whose slots a loan's
    /// fresh memory takes before any new chunk's ([`Self::lend`]).
    held: Vec<Held>,
    spares: Vec<usize>,
    hollows: Vec<usize>,
    /// The pages the chunks have room for (`sizes` summed), those of spare chunks and those of
    /// chunks out on loan again, kept as chunks change, so [`Self::room`], [`Self::shed_spare`]
    /// and [`Self::out_on_loan`] read a field where they summed every chunk: with a loan's
    /// memory in chunks of [`Self::loan_chunk`] pages, a cache holds tens of thousands.
    room_pages: usize,
    spare_pages: usize,
    lent_pages: usize,
    /// The most pages one buffer of a loan's fresh memory holds, each buffer its own chunk once
    /// back ([`loan_chunk_pages`]).
    loan_chunk: usize,
    /// Slots made in read-made chunks, and the pages of loans' chunks (kept, spare or lent
    /// again): the memory the cache holds ([`Self::bytes`]).
    made: usize,
    loaned: usize,
    /// Loans made, the pages of room they held, and of the pages their workers wrote, those the
    /// cache took back and those the room could not hold ([`Memory`]).
    loans: u64,
    loan_room: u64,
    loan_kept: u64,
    loan_missed: u64,
    /// The store's pages an extent, and each extent's slot hint ([`Self::hint`]): where its
    /// pages lie were they in a row as a loan keeps them. One word an extent, at most one a
    /// store extent, so a landing reads it from a line the CPU holds where the index's control
    /// word and bucket are a miss each at scale.
    extent_pages: u64,
    hints: Vec<Option<NonZeroU32>>,
}

/// Where a cache's memory is, in pages, and what its loans held ([`Cache::memory`]): what
/// sizes a loan (research/50 §13).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Memory {
    /// Pages cached.
    pub live: u64,
    /// Slots made for reads that hold no page.
    pub free: u64,
    /// Slots of loans' chunks that hold no page while others of their chunk do.
    pub dead: u64,
    /// Loans' chunks none of whose pages is cached, kept for the next loan.
    pub spare: u64,
    /// Out on loan: chunks lent again, and fresh room (not yet in [`Cache::bytes`]).
    pub lent: u64,
    /// The slot table's entries, hollow chunks' among them: at most twice the largest limit.
    pub slots: u64,
    /// Loans made, the pages of room they held, and of the pages their workers wrote, those
    /// taken back cached and those the room could not hold.
    pub loans: u64,
    pub loan_room: u64,
    pub loan_kept: u64,
    pub loan_missed: u64,
}

/// The most pages one buffer of a loan's fresh memory holds ([`Cache::lend`]). A job's pages
/// leave the cache an extent at a time, as the store frees their extents (`forget_run`), and a
/// loan keeps a job's pages in the order it writes them, an extent's in a row: a buffer of whole
/// extents leaves with its extents, where a buffer the size of a whole loan stays, its freed pages
/// dead slots, until the last of its extents leaves (a third of the cache at 1B, docs/research/65
/// §5). On Linux a buffer is at least one huge page and lies on huge pages
/// ([`crate::port::mmap::PageBuf`]), and a 64 GB cache of them is 32K buffers, each its own
/// mapping, half the default 65,530 mappings a process may hold (proc(5),
/// `/proc/sys/vm/max_map_count`).
fn loan_chunk_pages(page: usize, extent_pages: u64) -> usize {
    let extent = usize::try_from(extent_pages).unwrap_or(usize::MAX).max(1);
    #[cfg(target_os = "linux")]
    {
        let huge = crate::port::mmap::huge_page_size()
            .checked_div(page.max(1))
            .unwrap_or(0);
        extent.saturating_mul(huge.div_ceil(extent).max(1))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = page;
        extent
    }
}

impl Cache {
    /// A cache of `pages` pages of `page` bytes, at least one, for a store of `extent_pages`
    /// pages an extent. Its memory is taken as reads want it ([`Self::reserve`]).
    pub fn new(pages: usize, page: usize, extent_pages: u64) -> Self {
        Self {
            page,
            limit: pages.max(1),
            chunks: Vec::new(),
            starts: Vec::new(),
            sizes: Vec::new(),
            slots: Vec::new(),
            map: IncMap::new(),
            small: Fifo::default(),
            main: Fifo::default(),
            small_live: 0,
            main_live: 0,
            ghosts: IncMap::new(),
            ghost_seq: 0,
            free: Vec::new(),
            wanted: 0,
            available: None,
            refused: 0,
            grown: 0,
            retiring: None,
            retire_at: 0,
            retired: Vec::new(),
            hits: 0,
            misses: 0,
            evict_steps_most: 0,
            skipped: 0,
            lent: 0,
            out_most: 0,
            held: Vec::new(),
            spares: Vec::new(),
            hollows: Vec::new(),
            room_pages: 0,
            spare_pages: 0,
            lent_pages: 0,
            loan_chunk: loan_chunk_pages(page, extent_pages),
            made: 0,
            loaned: 0,
            loans: 0,
            loan_room: 0,
            loan_kept: 0,
            loan_missed: 0,
            extent_pages,
            hints: Vec::new(),
        }
    }

    fn ghost_cap(pages: usize) -> usize {
        pages.saturating_mul(GHOST_TENTHS) / 10
    }

    fn small_cap(&self) -> usize {
        (self.limit.saturating_mul(SMALL_TENTHS) / 10).max(1)
    }

    /// The payload cached for `address`, appended to `out`; false when it is not cached.
    pub fn get(&mut self, address: u64, out: &mut Vec<u8>) -> bool {
        self.map.settle();
        let Some(i) = self.slot_of(address) else {
            self.misses = self.misses.saturating_add(1);
            return false;
        };
        let Some(slot) = self.slots.get_mut(i) else {
            return false;
        };
        slot.freq = slot.freq.saturating_add(1).min(MAX_FREQ);
        let Some(payload) = self.payload(i) else {
            return false;
        };
        out.extend_from_slice(payload);
        self.hits = self.hits.saturating_add(1);
        true
    }

    /// The payload slot `i` holds.
    fn payload(&self, i: usize) -> Option<&[u8]> {
        let s = self.slots.get(i)?;
        self.chunks
            .get(s.chunk)?
            .get(s.at..s.at.checked_add(s.len)?)
    }

    /// The payload cached for `address`, lent in place and counted as a read; none when it is
    /// not cached. A point read parses the page where it lies and copies only what it returns.
    pub fn get_ref(&mut self, address: u64) -> Option<&[u8]> {
        // A phase of hits writes nothing: the read moves the index's migration on.
        self.map.settle();
        let Some(i) = self.slot_of(address) else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        let slot = self.slots.get_mut(i)?;
        slot.freq = slot.freq.saturating_add(1).min(MAX_FREQ);
        self.hits = self.hits.saturating_add(1);
        self.payload(i)
    }

    /// The slot holding `address`, counted as a read: a reader that reads the page in place,
    /// looking at it again with [`Self::page_at`] before each read.
    pub fn lookup(&mut self, address: u64) -> Option<usize> {
        self.map.settle();
        let Some(i) = self.slot_of(address) else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        let slot = self.slots.get_mut(i)?;
        slot.freq = slot.freq.saturating_add(1).min(MAX_FREQ);
        self.hits = self.hits.saturating_add(1);
        Some(i)
    }

    /// The payload slot `i` holds, if it still holds `address`: the cache may have given the
    /// slot to another page since [`Self::lookup`].
    pub fn page_at(&self, i: usize, address: u64) -> Option<&[u8]> {
        self.slots
            .get(i)
            .filter(|s| s.live && s.address == address)?;
        self.payload(i)
    }

    /// The payload cached for `address`, appended to `out`, without counting a read: a scan's
    /// look, which must not promote what it passes.
    pub fn peek(&self, address: u64, out: &mut Vec<u8>) -> bool {
        let Some(i) = self.slot_of(address) else {
            return false;
        };
        match self.payload(i) {
            Some(payload) => {
                out.extend_from_slice(payload);
                true
            }
            None => false,
        }
    }

    /// The slot holding `address`, for a reader that reads it in place ([`Self::page_at`]),
    /// without counting or promoting the look: a scan's look at a page it runs on to.
    pub fn slot_peek(&self, address: u64) -> Option<usize> {
        self.slot_touched(address)
            .filter(|&i| self.payload(i).is_some())
    }

    /// Whether the page can be read from memory, without counting or promoting the look.
    pub fn contains(&self, address: u64) -> bool {
        self.slot_of(address)
            .and_then(|i| self.payload(i))
            .is_some()
    }

    /// Caches `payload` for `address`, evicting by the module's rule if the cache is full. A
    /// payload longer than a slot is not cached.
    pub fn insert(&mut self, address: u64, payload: &[u8]) {
        if payload.len() > self.page {
            return;
        }
        self.forget(address);
        // Admission is optional: the page is the caller's whether or not the cache keeps it. An
        // eviction may cost a step for each page the queues cycle, so it is bounded at the
        // policy's own amortized cost, and an insert whose bound frees no slot leaves the page
        // out; the queues keep their place, so the next insert goes on from there.
        self.wanted = self.wanted.saturating_add(1);
        if self.held() >= self.limit && !self.evict(EVICT_STEPS) {
            self.skipped = self.skipped.saturating_add(1);
            return;
        }
        // A slot made in idle time, else one made now: its page touched here, and at most
        // O(log n) times over the cache's growth a chunk taken (the module's rule). Idle time
        // makes the slots the reads since asked for, so a read seldom does.
        let Some(i) = self.take_free().or_else(|| self.make_now()) else {
            self.skipped = self.skipped.saturating_add(1);
            return;
        };
        let Some(&Slot { chunk, at, .. }) = self.slots.get(i) else {
            return;
        };
        let Some(room) = at
            .checked_add(payload.len())
            .and_then(|end| self.chunks.get_mut(chunk)?.get_mut(at..end))
        else {
            self.free.push(i);
            return;
        };
        room.copy_from_slice(payload);
        let to_main = self.forget_ghost(address);
        let queue = if to_main { Queue::Main } else { Queue::Small };
        if let Some(slot) = self.slots.get_mut(i) {
            *slot = Slot {
                address,
                freq: 0,
                queue,
                live: true,
                newer: None,
                older: None,
                chunk,
                at,
                len: payload.len(),
            };
        }
        self.push(queue, i);
        self.map
            .insert(address, u64::try_from(i).unwrap_or(u64::MAX));
    }

    /// Pages held.
    fn held(&self) -> usize {
        self.small_live.saturating_add(self.main_live)
    }

    /// A made slot holding no page, none in a retiring chunk: those met are set aside.
    fn take_free(&mut self) -> Option<usize> {
        while let Some(i) = self.free.pop() {
            if self.retiring.is_some_and(|first| i >= first) {
                // Room was taken for the chunk's slots when it began to retire.
                self.retired.push(i);
                continue;
            }
            return Some(i);
        }
        None
    }

    /// A slot made for a read when idle time has not made one: none at the limit, past it before
    /// `trim` brings the room down, or while a chunk retires. The limit holds the chunks' room
    /// ([`Self::make_slot`]), not the slot table's length, which counts hollow chunks' slots with
    /// no memory behind them: counted, they froze a 1B store's cache 17 GiB under its limit
    /// (docs/research/65 §5).
    fn make_now(&mut self) -> Option<usize> {
        if self.retiring.is_some() || self.room() > self.limit {
            return None;
        }
        self.grown = self.grown.saturating_add(1);
        if self.make_slot() {
            self.take_free()
        } else {
            None
        }
    }

    /// Pages the chunks have room for, made into slots or not.
    fn room(&self) -> usize {
        self.room_pages
    }

    /// One more slot, its page written once so the OS maps it now: the last chunk's next page,
    /// or a new chunk's first. False when the limit or the memory refuses one.
    fn make_slot(&mut self) -> bool {
        let full = match (self.chunks.last(), self.sizes.last(), self.held.last()) {
            (Some(c), Some(&pages), Some(Held::Grown)) => {
                c.len() >= pages.saturating_mul(self.page)
            }
            _ => true,
        };
        if full && !self.new_chunk() {
            return false;
        }
        let Some(chunk) = self.chunks.len().checked_sub(1) else {
            return false;
        };
        let Some(c) = self.chunks.get_mut(chunk) else {
            return false;
        };
        let at = c.len();
        let Some(end) = at.checked_add(self.page) else {
            return false;
        };
        // Within the room taken for the chunk: no reallocation, the page written once.
        if !c.resize(end, 0) {
            return false;
        }
        let i = self.slots.len();
        self.slots.push(Slot {
            address: 0,
            freq: 0,
            queue: Queue::Small,
            live: false,
            newer: None,
            older: None,
            chunk,
            at,
            len: 0,
        });
        self.free.push(i);
        self.made = self.made.saturating_add(1);
        true
    }

    /// Whether the machine has twice `bytes` free for the cache to take them (the gate a chunk
    /// meets, §4.2); true when no reading is given. A refusal is counted.
    fn machine_spares(&mut self, bytes: usize) -> bool {
        let spares = self.available.is_none_or(|read| {
            read().is_none_or(|free| {
                u64::try_from(bytes)
                    .ok()
                    .and_then(|b| b.checked_mul(2))
                    .is_some_and(|need| free >= need)
            })
        });
        if !spares {
            self.refused = self.refused.saturating_add(1);
        }
        spares
    }

    /// Room for up to `pages` pages a worker will write, as a [`Kept`] to fill: spare chunks'
    /// memory first, already mapped, then fresh memory within the limit, less the room the chunks
    /// and earlier loans hold, while the machine has it as a chunk of the cache's own needs it.
    /// Fresh memory fills hollow chunks' slots first, a whole chunk each, then new buffers of at
    /// most [`Self::loan_chunk`] pages, each its own chunk once back, so the pages a later job's
    /// forgetting frees together free their chunk together. A new buffer is lent only once no
    /// hollow chunk is left, which bounds the slot table (the module's rule): room under the
    /// limit short of the next hollow chunk's slots lends no fresh memory. None while a chunk
    /// retires.
    pub fn lend(&mut self, pages: usize) -> Option<Kept> {
        if self.retiring.is_some() || pages == 0 {
            return None;
        }
        let mut bufs = Vec::new();
        let mut room = 0usize;
        while room < pages {
            let Some(c) = self.spares.pop() else {
                break;
            };
            if self.held.get(c) != Some(&Held::Spare) {
                continue;
            }
            let (Some(chunk), Some(&size), Some(h)) = (
                self.chunks.get_mut(c),
                self.sizes.get(c),
                self.held.get_mut(c),
            ) else {
                continue;
            };
            if bufs.try_reserve(1).is_err() {
                self.spares.push(c);
                break;
            }
            let mut buf = std::mem::take(chunk);
            buf.clear();
            *h = Held::Lent;
            self.spare_pages = self.spare_pages.saturating_sub(size);
            self.lent_pages = self.lent_pages.saturating_add(size);
            room = room.saturating_add(size);
            bufs.push(Loaned {
                chunk: Some(c),
                buf,
            });
        }
        // The fresh memory: hollow chunks whole, the last hollowed first, while the room under
        // the limit holds the next; new buffers for the rest only once none is left.
        let mut budget = self
            .limit
            .saturating_sub(self.room().saturating_add(self.lent));
        let mut need = pages.saturating_sub(room);
        let (mut refills, mut planned) = (0usize, 0usize);
        for &c in self.hollows.iter().rev() {
            let span = self.span(c);
            if need == 0 || span == 0 || span > budget {
                break;
            }
            refills = refills.saturating_add(1);
            planned = planned.saturating_add(span);
            budget = budget.saturating_sub(span);
            need = need.saturating_sub(span);
        }
        let new = if refills == self.hollows.len() {
            need.min(budget)
        } else {
            0
        };
        // Fresh pages lent, each counted in `lent` until the loan comes back.
        let mut fresh = 0usize;
        if let Some(bytes) = planned.saturating_add(new).checked_mul(self.page)
            && bytes > 0
            && self.machine_spares(bytes)
        {
            for _ in 0..refills {
                let Some(c) = self.hollows.pop() else {
                    break;
                };
                let span = self.span(c);
                let buf = span.checked_mul(self.page).and_then(PageBuf::with_capacity);
                let Some(buf) = buf.filter(|_| bufs.try_reserve(1).is_ok()) else {
                    self.hollows.push(c);
                    break;
                };
                if let Some(h) = self.held.get_mut(c) {
                    *h = Held::Refilled;
                }
                self.lent = self.lent.saturating_add(span);
                fresh = fresh.saturating_add(span);
                room = room.saturating_add(span);
                bufs.push(Loaned {
                    chunk: Some(c),
                    buf,
                });
            }
            let mut left = if self.hollows.is_empty() { new } else { 0 };
            while left > 0 {
                let n = left.min(self.loan_chunk.max(1));
                let Some(bytes) = n.checked_mul(self.page) else {
                    break;
                };
                let Some(buf) =
                    PageBuf::with_capacity(bytes).filter(|_| bufs.try_reserve(1).is_ok())
                else {
                    break;
                };
                self.lent = self.lent.saturating_add(n);
                fresh = fresh.saturating_add(n);
                room = room.saturating_add(n);
                left = left.saturating_sub(n);
                bufs.push(Loaned { chunk: None, buf });
            }
        }
        let mut list = Vec::new();
        if room == 0 || list.try_reserve_exact(room).is_err() {
            self.give_back(bufs, fresh);
            return None;
        }
        self.loans = self.loans.saturating_add(1);
        self.loan_room = self
            .loan_room
            .saturating_add(u64::try_from(room).unwrap_or(u64::MAX));
        self.out_most = self.out_most.max(self.out_on_loan());
        Some(Kept {
            page: self.page,
            fresh,
            bufs,
            at: 0,
            pages: list,
            missed: 0,
        })
    }

    /// A loan's buffers back unread: chunks' memory spare again, fresh memory dropped and its
    /// room no longer lent, a refilled chunk hollow again.
    fn give_back(&mut self, bufs: Vec<Loaned>, fresh: usize) {
        self.lent = self.lent.saturating_sub(fresh);
        for Loaned { chunk, buf } in bufs {
            if let Some(c) = chunk
                && let (Some(slot), Some(&size), Some(h)) = (
                    self.chunks.get_mut(c),
                    self.sizes.get(c),
                    self.held.get_mut(c),
                )
            {
                if *h == Held::Refilled {
                    *h = Held::Hollow;
                    self.hollows.push(c);
                    continue;
                }
                *slot = buf;
                *h = Held::Spare;
                self.lent_pages = self.lent_pages.saturating_sub(size);
                self.spare_pages = self.spare_pages.saturating_add(size);
                self.spares.push(c);
            }
        }
        self.shed_spare();
    }

    /// The pages out on loan now: fresh room lent, and spare chunks lent again.
    fn out_on_loan(&self) -> usize {
        self.lent.saturating_add(self.lent_pages)
    }

    /// Gives back spare chunks' memory beyond what the loans may take: the most they have had out
    /// at once less what is out now. Spare memory holds no page; kept, it serves a loan without
    /// the OS mapping it again, and no loan has needed more of it. Loaned chunks whose pages
    /// were all forgotten, as branches merged away forget theirs, otherwise gather spare beyond
    /// any loan's need (10 GiB at 100M, docs/research/65 §5); this bounds them.
    fn shed_spare(&mut self) {
        let need = self.out_most.saturating_sub(self.out_on_loan());
        while self.spare_pages > need {
            let Some(c) = self.spares.pop() else {
                break;
            };
            if self.held.get(c) == Some(&Held::Spare)
                && let (Some(chunk), Some(size), Some(h)) = (
                    self.chunks.get_mut(c),
                    self.sizes.get_mut(c),
                    self.held.get_mut(c),
                )
            {
                *chunk = PageBuf::new();
                self.loaned = self.loaned.saturating_sub(*size);
                self.spare_pages = self.spare_pages.saturating_sub(*size);
                self.room_pages = self.room_pages.saturating_sub(*size);
                *size = 0;
                *h = Held::Hollow;
                self.hollows.push(c);
            }
        }
        self.settle_retirement();
    }

    /// Takes back memory lent with the pages a worker kept in it: each buffer its chunk again,
    /// or a new chunk, whose pages enter the queues as inserts do (the small queue, or main for an
    /// address the ghost names); pages past the limit are evicted by the rule an insert meets. A
    /// buffer with no page in it is spare again; a loan back while a chunk retires is given up.
    pub fn adopt(&mut self, kept: Kept) {
        let Kept {
            fresh,
            bufs,
            pages,
            missed,
            ..
        } = kept;
        self.loan_missed = self
            .loan_missed
            .saturating_add(u64::try_from(missed).unwrap_or(u64::MAX));
        if self.retiring.is_some() {
            self.give_back(bufs, fresh);
            return;
        }
        self.lent = self.lent.saturating_sub(fresh);
        let page = self.page.max(1);
        let mut kept_pages = pages.into_iter();
        for Loaned { chunk, buf } in bufs {
            let count = buf.len().checked_div(page).unwrap_or(0);
            let c = match chunk {
                Some(c) => {
                    if self.held.get(c) == Some(&Held::Refilled)
                        && let Some(size) = self.sizes.get_mut(c)
                    {
                        // A hollow chunk's slots, its memory the loan's fresh room.
                        *size = buf.capacity().checked_div(page).unwrap_or(0);
                        self.room_pages = self.room_pages.saturating_add(*size);
                        self.loaned = self.loaned.saturating_add(*size);
                    }
                    if let Some(slot) = self.chunks.get_mut(c) {
                        *slot = buf;
                    }
                    c
                }
                None => {
                    let size = buf.capacity().checked_div(page).unwrap_or(0);
                    if count == 0
                        || self.slots.try_reserve(size).is_err()
                        || self.chunks.try_reserve(1).is_err()
                        || self.starts.try_reserve(1).is_err()
                        || self.sizes.try_reserve(1).is_err()
                        || self.held.try_reserve(1).is_err()
                    {
                        // Its pages go uncached; the memory is dropped.
                        for _ in 0..count {
                            kept_pages.next();
                        }
                        continue;
                    }
                    let c = self.chunks.len();
                    self.starts.push(self.slots.len());
                    self.sizes.push(size);
                    self.room_pages = self.room_pages.saturating_add(size);
                    self.chunks.push(buf);
                    self.held.push(Held::Kept(0));
                    self.loaned = self.loaned.saturating_add(size);
                    for k in 0..size {
                        self.slots.push(Slot {
                            address: 0,
                            freq: 0,
                            queue: Queue::Small,
                            live: false,
                            newer: None,
                            older: None,
                            chunk: c,
                            at: k.saturating_mul(page),
                            len: 0,
                        });
                    }
                    c
                }
            };
            let first = self.starts.get(c).copied().unwrap_or(0);
            let mut live = 0usize;
            for (k, (address, len)) in kept_pages.by_ref().take(count).enumerate() {
                self.forget(address);
                let to_main = self.forget_ghost(address);
                let queue = if to_main { Queue::Main } else { Queue::Small };
                let i = first.saturating_add(k);
                let Some(s) = self.slots.get_mut(i) else {
                    break;
                };
                *s = Slot {
                    address,
                    freq: 0,
                    queue,
                    live: true,
                    newer: None,
                    older: None,
                    chunk: c,
                    at: k.saturating_mul(page),
                    len,
                };
                self.push(queue, i);
                self.map
                    .insert(address, u64::try_from(i).unwrap_or(u64::MAX));
                self.hint(address, i);
                live = live.saturating_add(1);
            }
            self.loan_kept = self
                .loan_kept
                .saturating_add(u64::try_from(live).unwrap_or(u64::MAX));
            if let (Some(h), Some(&size)) = (self.held.get_mut(c), self.sizes.get(c)) {
                if *h == Held::Lent {
                    self.lent_pages = self.lent_pages.saturating_sub(size);
                }
                *h = if live == 0 {
                    self.spares.push(c);
                    self.spare_pages = self.spare_pages.saturating_add(size);
                    Held::Spare
                } else {
                    Held::Kept(live)
                };
            }
        }
        for _ in 0..self.held().saturating_sub(self.limit) {
            if !self.evict(EVICT_STEPS) {
                break;
            }
        }
        self.shed_spare();
    }

    /// Takes back a loan its job did not fill: a job abandoned, or ended in error.
    pub fn unlend(&mut self, kept: Kept) {
        let Kept { fresh, bufs, .. } = kept;
        self.give_back(bufs, fresh);
    }

    /// A slot no longer holding a page: a read-made chunk's goes on the free list; a loan's
    /// chunk counts it, and once none of its pages is live the chunk is spare for the next loan.
    fn vacate(&mut self, i: usize) {
        let Some(c) = self.slots.get(i).map(|s| s.chunk) else {
            return;
        };
        match self.held.get_mut(c) {
            Some(Held::Kept(live)) => {
                *live = live.saturating_sub(1);
                if *live == 0 {
                    if let Some(h) = self.held.get_mut(c) {
                        *h = Held::Spare;
                    }
                    let size = self.sizes.get(c).copied().unwrap_or(0);
                    self.spare_pages = self.spare_pages.saturating_add(size);
                    self.spares.push(c);
                    self.shed_spare();
                }
            }
            Some(Held::Grown) | None => self.free.push(i),
            Some(_) => {}
        }
    }

    /// The pages the next chunk takes by the module's rule: as many as the chunks before it hold,
    /// a tuning step at most, within the limit.
    fn next_chunk_pages(&self) -> usize {
        let room = self.room();
        let step = (self.limit.saturating_mul(TUNE_STEP_PERCENT) / 100).max(1);
        room.max(1).min(step).min(self.limit.saturating_sub(room))
    }

    /// The bytes the cache's next chunk takes ([`Self::next_chunk_pages`]): the most it grows by
    /// at once.
    pub fn next_chunk_bytes(&self) -> usize {
        self.next_chunk_pages().saturating_mul(self.page)
    }

    /// Takes a new chunk by the module's rule, with room in the slot and free lists for its
    /// pages, so making them reallocates nothing. False when the limit has no room left or the
    /// memory refuses it.
    fn new_chunk(&mut self) -> bool {
        let pages = self.next_chunk_pages();
        if pages == 0 {
            return false;
        }
        let Some(bytes) = pages.checked_mul(self.page) else {
            return false;
        };
        // The machine keeps a chunk's room free after the cache takes one: what a tuning step
        // moves, so the next step finds memory to move. No reading sets no bound.
        if !self.machine_spares(bytes) {
            return false;
        }
        let Some(chunk) = PageBuf::with_capacity(bytes) else {
            return false;
        };
        if self.slots.try_reserve_exact(pages).is_err()
            || self.free.try_reserve_exact(pages).is_err()
            || self.chunks.try_reserve(1).is_err()
            || self.starts.try_reserve(1).is_err()
            || self.sizes.try_reserve(1).is_err()
            || self.held.try_reserve(1).is_err()
        {
            return false;
        }
        self.starts.push(self.slots.len());
        self.sizes.push(pages);
        self.room_pages = self.room_pages.saturating_add(pages);
        self.chunks.push(chunk);
        self.held.push(Held::Grown);
        true
    }

    fn push(&mut self, queue: Queue, i: usize) {
        let Some(link) = slot_link(i) else {
            return;
        };
        let q = match queue {
            Queue::Small => &mut self.small,
            Queue::Main => &mut self.main,
        };
        let older = q.newest;
        q.newest = Some(link);
        if q.oldest.is_none() {
            q.oldest = Some(link);
        }
        if let Some(slot) = self.slots.get_mut(i) {
            slot.newer = None;
            slot.older = older;
        }
        if let Some(slot) = older
            .and_then(slot_index)
            .and_then(|j| self.slots.get_mut(j))
        {
            slot.newer = Some(link);
        }
        match queue {
            Queue::Small => self.small_live = self.small_live.saturating_add(1),
            Queue::Main => self.main_live = self.main_live.saturating_add(1),
        }
    }

    /// Unlinks live slot `i`, touching only its neighbours and the queue's ends.
    fn unlink(&mut self, i: usize) {
        let Some(slot) = self.slots.get(i).copied().filter(|s| s.live) else {
            return;
        };
        let Some(link) = slot_link(i) else {
            return;
        };
        let q = match slot.queue {
            Queue::Small => &mut self.small,
            Queue::Main => &mut self.main,
        };
        if q.newest == Some(link) {
            q.newest = slot.older;
        }
        if q.oldest == Some(link) {
            q.oldest = slot.newer;
        }
        if let Some(s) = slot
            .newer
            .and_then(slot_index)
            .and_then(|j| self.slots.get_mut(j))
        {
            s.older = slot.older;
        }
        if let Some(s) = slot
            .older
            .and_then(slot_index)
            .and_then(|j| self.slots.get_mut(j))
        {
            s.newer = slot.newer;
        }
        if let Some(s) = self.slots.get_mut(i) {
            s.newer = None;
            s.older = None;
        }
        match slot.queue {
            Queue::Small => self.small_live = self.small_live.saturating_sub(1),
            Queue::Main => self.main_live = self.main_live.saturating_sub(1),
        }
    }

    /// Drops `address` from the cache: a page written there replaces what was cached.
    pub fn forget(&mut self, address: u64) {
        if let Some(i) = self
            .map
            .remove(address)
            .and_then(|i| usize::try_from(i).ok())
        {
            self.release(i);
        }
    }

    /// Frees slot `i` and removes its queue entry.
    fn release(&mut self, i: usize) {
        self.unlink(i);
        if let Some(slot) = self.slots.get_mut(i)
            && slot.live
        {
            slot.live = false;
            self.vacate(i);
        }
    }

    /// The slot holding `address`.
    fn slot_of(&self, address: u64) -> Option<usize> {
        self.map.get(address).and_then(|i| usize::try_from(i).ok())
    }

    /// [`Self::slot_of`] for a reader about to read the page: the slot its extent's hint names
    /// ([`Self::hint`]) if it holds the page, the slot and the page's first line read together;
    /// else the index's control word and first bucket read together ([`IncMap::get_touched`]),
    /// and, once the slot is known, its page's first line started while the slot itself is read.
    /// A scan landing on a leaf waited on four misses in a row: the control word, the bucket,
    /// the slot and the page (445 ns of a 10M seek's 1.1 µs placing; research/61 §2); the index
    /// read in pairs leaves two, the hint one.
    fn slot_touched(&self, address: u64) -> Option<usize> {
        if let Some(i) = self.hinted(address) {
            self.touch_page(i);
            // The test `page_at` makes: a slot live at the address is the page's one slot, every
            // removal from the index clearing `live` and every insert forgetting the page first.
            if self
                .slots
                .get(i)
                .is_some_and(|s| s.live && s.address == address)
            {
                return Some(i);
            }
        }
        let i = self
            .map
            .get_touched(address)
            .and_then(|i| usize::try_from(i).ok())?;
        self.touch_page(i);
        Some(i)
    }

    /// Starts slot `i`'s page's first line, the page's place taken from the layout every chunk
    /// keeps (its slots in a row from its first, a page apart: `make_slot`, `adopt`), so the
    /// page's miss overlaps the slot's.
    fn touch_page(&self, i: usize) {
        let c = self
            .starts
            .partition_point(|&first| first <= i)
            .checked_sub(1);
        if let Some(c) = c
            && let (Some(&first), Some(chunk)) = (self.starts.get(c), self.chunks.get(c))
            && let Some(byte) = i
                .checked_sub(first)
                .and_then(|k| k.checked_mul(self.page))
                .and_then(|at| chunk.get(at))
        {
            std::hint::black_box(*byte);
        }
    }

    /// The slot `address`'s extent hint names for it, holding the page or not.
    fn hinted(&self, address: u64) -> Option<usize> {
        let extent = usize::try_from(address.checked_div(self.extent_pages)?).ok()?;
        let hint = (*self.hints.get(extent)?)?;
        let within = address.checked_rem(self.extent_pages)?;
        let i = u64::from(hint.get())
            .checked_add(within)?
            .checked_sub(self.extent_pages)?;
        usize::try_from(i).ok()
    }

    /// Names slot `i`, a loan's page at `address`, as its extent's place: a job writes an
    /// extent's pages in a row (`Store::write_page_paced`) and a loan keeps them in a row, so the
    /// extent's other pages lie in the slots beside it. The hint is the slot the extent's first
    /// page would hold plus the extent's pages, so a row that starts before the cache's first
    /// slot has one too, and a hint is never 0. The hints grow to the extent, which the store's
    /// own extents bound; a hint the memory refuses is not kept, the index finding the page as
    /// before.
    fn hint(&mut self, address: u64, i: usize) {
        let (Some(extent), Some(within)) = (
            address.checked_div(self.extent_pages),
            address.checked_rem(self.extent_pages),
        ) else {
            return;
        };
        let hint = u64::try_from(i)
            .ok()
            .and_then(|i| i.checked_add(self.extent_pages)?.checked_sub(within))
            .and_then(|h| u32::try_from(h).ok())
            .and_then(NonZeroU32::new);
        let (Ok(extent), Some(hint)) = (usize::try_from(extent), hint) else {
            return;
        };
        if let Some(len) = extent.checked_add(1)
            && len > self.hints.len()
        {
            if self
                .hints
                .try_reserve(len.saturating_sub(self.hints.len()))
                .is_err()
            {
                return;
            }
            self.hints.resize(len, None);
        }
        if let Some(h) = self.hints.get_mut(extent) {
            *h = Some(hint);
        }
    }

    /// The slot holding `address` with its payload, counted as a read and promoted, the look
    /// made as [`Self::slot_touched`] makes it; none, and nothing counted, when no slot holds
    /// it: a scan's landing, whose miss is counted where the page is then read.
    pub fn lookup_held(&mut self, address: u64) -> Option<usize> {
        self.map.settle();
        let i = self.slot_touched(address)?;
        self.payload(i)?;
        let slot = self.slots.get_mut(i)?;
        slot.freq = slot.freq.saturating_add(1).min(MAX_FREQ);
        self.hits = self.hits.saturating_add(1);
        Some(i)
    }

    /// Frees one slot within `steps` queue steps: from the small queue while it holds its
    /// share, else from main. Whether one was freed.
    fn evict(&mut self, steps: usize) -> bool {
        let held = self.held();
        let mut taken = 0u64;
        for _ in 0..steps {
            if self.held() < held {
                break;
            }
            self.step();
            taken = taken.saturating_add(1);
        }
        self.evict_steps_most = self.evict_steps_most.max(taken);
        self.held() < held
    }

    /// One queue step of eviction: the small queue's oldest page while that queue holds its
    /// share, else main's.
    fn step(&mut self) {
        if self.small_live >= self.small_cap() || self.main_live == 0 {
            self.evict_small();
        } else {
            self.evict_main();
        }
    }

    /// The small queue's oldest page moves to main if it was read `PROMOTE` times, else is
    /// freed and remembered in the ghost.
    fn evict_small(&mut self) {
        let Some(i) = self.small.oldest.and_then(slot_index) else {
            return;
        };
        let Some(slot) = self.slots.get(i).copied() else {
            return;
        };
        self.unlink(i);
        if slot.freq >= PROMOTE {
            if let Some(s) = self.slots.get_mut(i) {
                s.queue = Queue::Main;
            }
            self.push(Queue::Main, i);
            return;
        }
        if let Some(s) = self.slots.get_mut(i) {
            s.live = false;
        }
        self.map.remove(slot.address);
        self.vacate(i);
        self.remember_ghost(slot.address);
    }

    /// The ghost's length in evictions.
    fn window(&self) -> u64 {
        u64::try_from(Self::ghost_cap(self.limit)).unwrap_or(u64::MAX)
    }

    /// Remembers `address` in the ghost, and sweeps a few of its entries out of the window.
    fn remember_ghost(&mut self, address: u64) {
        let window = self.window();
        if window == 0 {
            return;
        }
        self.ghost_seq = self.ghost_seq.wrapping_add(1);
        self.ghosts.insert(address, self.ghost_seq);
        let now = self.ghost_seq;
        self.ghosts
            .sweep(SWEEP, |_, seq| now.wrapping_sub(seq) < window);
    }

    /// Forgets `address` from the ghost; true if it was there.
    fn forget_ghost(&mut self, address: u64) -> bool {
        let window = self.window();
        let now = self.ghost_seq;
        self.ghosts
            .remove(address)
            .is_some_and(|seq| now.wrapping_sub(seq) < window)
    }

    /// Main's oldest page goes round again with one read fewer if it has any, else is freed.
    fn evict_main(&mut self) {
        let Some(i) = self.main.oldest.and_then(slot_index) else {
            return;
        };
        let Some(slot) = self.slots.get(i).copied() else {
            return;
        };
        self.unlink(i);
        if slot.freq > 0 {
            if let Some(s) = self.slots.get_mut(i) {
                s.freq = s.freq.saturating_sub(1);
            }
            self.push(Queue::Main, i);
            return;
        }
        if let Some(s) = self.slots.get_mut(i) {
            s.live = false;
        }
        self.map.remove(slot.address);
        self.vacate(i);
    }

    /// Sets the cache's limit to `pages` pages, at least one. Growing takes effect at once, a
    /// retirement the raised limit no longer asks for ending with it; a cache above a lowered
    /// limit is brought down by `trim`, and meanwhile an insert evicts a page for each it adds, so
    /// the pages held only fall. The ghost's window follows the limit.
    pub fn resize(&mut self, pages: usize) {
        self.limit = pages.max(1);
        self.settle_retirement();
    }

    /// Up to `n` queue steps toward the limit: pages freed by the cache's own rule (S3-FIFO's
    /// evictions), then up to `n` freed buffers dropped. True while the cache is still above it.
    pub fn trim(&mut self, n: usize) -> bool {
        // `n` queue steps, not `n` evictions: an eviction may take a step for each page the
        // queues cycle.
        for _ in 0..n {
            if self.held() <= self.limit {
                break;
            }
            self.step();
        }
        // Spare loans' memory goes first: it holds no page.
        while self.room() > self.limit {
            let Some(c) = self.spares.pop() else {
                break;
            };
            if self.held.get(c) == Some(&Held::Spare)
                && let (Some(chunk), Some(size), Some(h)) = (
                    self.chunks.get_mut(c),
                    self.sizes.get_mut(c),
                    self.held.get_mut(c),
                )
            {
                *chunk = PageBuf::new();
                self.loaned = self.loaned.saturating_sub(*size);
                self.spare_pages = self.spare_pages.saturating_sub(*size);
                self.room_pages = self.room_pages.saturating_sub(*size);
                *size = 0;
                *h = Held::Hollow;
                self.hollows.push(c);
            }
        }
        // A retirement the room left no longer asks for ends before another chunk's begins.
        self.settle_retirement();
        // Then the chunks above the limit, the last first: its slots looked at `n` a call, the
        // pages they hold dropped, and the chunk given back once none is left. A chunk out on
        // loan waits for its memory to come back.
        let mut looked = 0usize;
        while self.room() > self.limit && looked < n {
            if matches!(self.held.last(), Some(Held::Lent | Held::Refilled)) {
                break;
            }
            let (Some(&first), Some(&pages)) = (self.starts.last(), self.sizes.last()) else {
                break;
            };
            if self.retiring.is_none() {
                if self.retired.try_reserve_exact(pages).is_err() {
                    break;
                }
                self.retiring = Some(first);
                self.retire_at = self.slots.len();
            }
            while looked < n && self.retire_at > first {
                self.retire_at = self.retire_at.saturating_sub(1);
                looked = looked.saturating_add(1);
                let i = self.retire_at;
                if let Some(address) = self.slots.get(i).filter(|s| s.live).map(|s| s.address) {
                    self.map.remove(address);
                    self.release(i);
                }
            }
            if self.retire_at > first {
                break;
            }
            // Every page of the chunk dropped: its slots leave the free list and it is given back.
            self.free.retain(|&i| i < first);
            self.retired.clear();
            match self.held.last() {
                Some(Held::Grown) => {
                    self.made = self
                        .made
                        .saturating_sub(self.slots.len().saturating_sub(first));
                }
                Some(Held::Hollow) => {}
                Some(Held::Spare) => {
                    self.loaned = self.loaned.saturating_sub(pages);
                    self.spare_pages = self.spare_pages.saturating_sub(pages);
                }
                _ => self.loaned = self.loaned.saturating_sub(pages),
            }
            self.room_pages = self.room_pages.saturating_sub(pages);
            self.slots.truncate(first);
            self.chunks.pop();
            self.starts.pop();
            self.sizes.pop();
            self.held.pop();
            let gone = self.chunks.len();
            self.spares.retain(|&c| c != gone);
            self.hollows.retain(|&c| c != gone);
            self.retiring = None;
        }
        self.over() > 0
    }

    /// A retirement the limit no longer asks for ends: the slots it set aside are free again.
    /// While one lasts the cache lends nothing and gives up each loan that comes back, so it ends
    /// wherever the room falls or the limit rises (`resize`, `trim`, `shed_spare`), not only when
    /// idle time reserves slots, which a busy shard may not reach for minutes (docs/research/65
    /// §5).
    fn settle_retirement(&mut self) {
        if self.retiring.is_some() && self.room() <= self.limit {
            self.retiring = None;
            self.free.append(&mut self.retired);
        }
    }

    /// Slots wanted and not made: the pages inserted since [`Self::reserve`] last caught up that
    /// the free slots do not cover, within the limit; none while chunks retire.
    pub fn unreserved(&self) -> usize {
        if self.retiring.is_some() {
            return 0;
        }
        self.wanted
            .saturating_sub(self.free.len())
            .min(self.makeable())
    }

    /// Slots reads could still make: the last chunk's room not yet made, when reads made it, and
    /// the room under the limit no chunk holds ([`Self::make_slot`]); none past the limit.
    fn makeable(&self) -> usize {
        if self.room() > self.limit {
            return 0;
        }
        let unmade = match (self.held.last(), self.sizes.last(), self.starts.last()) {
            (Some(Held::Grown), Some(&size), Some(&first)) => {
                size.saturating_sub(self.slots.len().saturating_sub(first))
            }
            _ => 0,
        };
        unmade.saturating_add(self.limit.saturating_sub(self.room()))
    }

    /// The slots chunk `c` has: up to the next chunk's first, or the table's end for the last.
    fn span(&self, c: usize) -> usize {
        let Some(&first) = self.starts.get(c) else {
            return 0;
        };
        let end = c
            .checked_add(1)
            .and_then(|next| self.starts.get(next).copied())
            .unwrap_or(self.slots.len());
        end.saturating_sub(first)
    }

    /// Makes up to `n` of the slots wanted, each its page written once so the OS maps it now: a
    /// read that misses later takes a made slot, with no allocation and no first-touch page
    /// fault on its path. Run in idle time; the slots made.
    pub fn reserve(&mut self, n: usize) -> usize {
        self.settle_retirement();
        let mut made = 0usize;
        while made < n && self.unreserved() > 0 {
            if !self.make_slot() {
                // No slot can be made now (the limit's room held by chunks, loans' among them,
                // or the machine's memory refused): what is wanted is no longer owed, until the
                // next insert wants again. Owed, idle time asked for it forever.
                self.wanted = 0;
                break;
            }
            made = made.saturating_add(1);
        }
        if self.unreserved() == 0 {
            self.wanted = 0;
        }
        made
    }

    /// The bytes its index and ghost take.
    pub fn index_bytes(&self) -> usize {
        self.map.bytes().saturating_add(self.ghosts.bytes())
    }

    /// Pages held, or pages the chunks have room for, above the limit: what `trim` has left
    /// to do.
    pub fn over(&self) -> usize {
        self.held().max(self.room()).saturating_sub(self.limit)
    }

    /// Whether `address` was evicted recently enough that the ghost still names it: a miss on
    /// it is a read a larger cache would have saved.
    pub fn in_ghost(&self, address: u64) -> bool {
        let window = self.window();
        self.ghosts
            .get(address)
            .is_some_and(|seq| self.ghost_seq.wrapping_sub(seq) < window)
    }

    /// Addresses the ghost may hold: the pages a miss on a ghost stands for.
    pub fn ghost_pages(&self) -> usize {
        Self::ghost_cap(self.limit)
    }

    /// Pages the cache may hold.
    pub fn pages(&self) -> usize {
        self.limit
    }

    /// Reads served, and reads missed.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// The most queue steps one eviction took.
    pub fn evict_steps_most(&self) -> u64 {
        self.evict_steps_most
    }

    /// Inserts left out because their bounded eviction freed no slot, or no slot was made.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// Slots reads made themselves, in a chunk's room, when idle time had not made one.
    pub fn grown(&self) -> u64 {
        self.grown
    }

    /// Asks `read` for the memory the machine has free before each chunk is taken.
    pub fn set_available(&mut self, read: fn() -> Option<u64>) {
        self.available = Some(read);
    }

    /// Chunks the machine had no memory free for.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// The cache's bytes in memory: the slots made for reads, holding a page or not, and the
    /// loans' chunks, kept, spare or lent again; a read-made chunk's room not yet made is not
    /// in memory.
    pub fn bytes(&self) -> usize {
        self.made
            .saturating_add(self.loaned)
            .saturating_mul(self.page)
    }

    /// Where the memory is: a walk of the chunks, for reports.
    pub fn memory(&self) -> Memory {
        let pages = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let live = self.small_live.saturating_add(self.main_live);
        let (mut kept, mut dead, mut spare, mut lent) = (0usize, 0usize, 0usize, self.lent);
        for (h, &size) in self.held.iter().zip(&self.sizes) {
            match *h {
                Held::Kept(l) => {
                    kept = kept.saturating_add(l);
                    dead = dead.saturating_add(size.saturating_sub(l));
                }
                Held::Spare => spare = spare.saturating_add(size),
                Held::Lent => lent = lent.saturating_add(size),
                Held::Grown | Held::Hollow | Held::Refilled => {}
            }
        }
        Memory {
            live: pages(live),
            free: pages(self.made.saturating_sub(live.saturating_sub(kept))),
            dead: pages(dead),
            spare: pages(spare),
            lent: pages(lent),
            slots: pages(self.slots.len()),
            loans: self.loans,
            loan_room: self.loan_room,
            loan_kept: self.loan_kept,
            loan_missed: self.loan_missed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    /// A store's pages an extent: few, so one loan's pages span several extents.
    const EXTENT: u64 = 4;

    fn page(a: u64) -> Vec<u8> {
        format!("page {a}").into_bytes()
    }

    /// A landing takes the slot its extent's hint names only while that slot holds the page:
    /// a loan's pages, kept in a row across extents and from an extent's middle, are each named
    /// by their extent's hint; a page forgotten and read again into a slot of the reads' own, or
    /// a hinted slot given to another page, is found by the index or not at all.
    #[test]
    fn a_landing_trusts_an_extent_s_hint_only_while_its_slot_holds_the_page() {
        let mut c = Cache::new(64, 16, EXTENT);
        let mut k = c.lend(8).unwrap();
        for a in 3..11u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        for a in 3..11u64 {
            assert!(c.hinted(a).is_some(), "page {a} has no hint");
            assert_eq!(c.hinted(a), c.slot_of(a), "page {a}");
            let i = c.lookup_held(a).unwrap();
            assert_eq!(c.page_at(i, a), Some(&page(a)[..]));
        }
        // Page 5 written again and read into a slot of the reads' own: its extent's hint names
        // the loan's slot, which holds no page now.
        c.forget(5);
        assert_eq!(c.lookup_held(5), None);
        assert_eq!(c.slot_peek(5), None);
        c.insert(5, b"again");
        let i = c.lookup_held(5).unwrap();
        assert_ne!(c.hinted(5), Some(i));
        assert_eq!(c.page_at(i, 5), Some(&b"again"[..]));
        // The loan's pages all forgotten, its chunk lent again and filled with another extent's
        // pages: the old extents' hints name slots that now hold those.
        for a in 3..11u64 {
            c.forget(a);
        }
        let mut k = c.lend(8).unwrap();
        assert_eq!(k.fresh, 0, "the spare chunk was not lent");
        for a in 20..28u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        for a in 3..11u64 {
            assert_eq!(
                c.lookup_held(a),
                None,
                "page {a} found after it was forgotten"
            );
            assert_eq!(c.slot_peek(a), None);
        }
        c.insert(9, b"nine");
        let i = c.slot_peek(9).unwrap();
        assert_eq!(c.page_at(i, 9), Some(&b"nine"[..]));
        for a in 20..28u64 {
            let i = c.lookup_held(a).unwrap();
            assert_eq!(c.page_at(i, a), Some(&page(a)[..]));
        }
    }

    #[test]
    fn a_worker_s_pages_come_back_cached_in_the_memory_lent() {
        let mut c = Cache::new(8, 16, EXTENT);
        let mut k = c.lend(4).unwrap();
        for a in 0..3u64 {
            k.keep(100 + a, &page(a));
        }
        assert_eq!(k.len(), 3);
        c.adopt(k);
        let mut out = Vec::new();
        for a in 0..3u64 {
            out.clear();
            assert!(c.get(100 + a, &mut out));
            assert_eq!(out, page(a));
        }
        // The memory held is the room lent, four pages, one of them empty.
        assert_eq!(c.bytes(), 4 * 16);
        assert_eq!(c.lent, 0);
    }

    #[test]
    fn memory_names_where_the_pages_are_and_what_the_loans_held() {
        let mut c = Cache::new(64, 16, EXTENT);
        for a in 100..103u64 {
            c.insert(a, &page(a));
        }
        let mut k = c.lend(4).unwrap();
        for a in 0..6u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        let m = c.memory();
        assert_eq!(
            (m.loans, m.loan_room, m.loan_kept, m.loan_missed),
            (1, 4, 4, 2)
        );
        assert_eq!((m.live, m.dead, m.spare, m.lent), (7, 0, 0, 0));
        // Every page of memory is named once: made slots, live or free, and the loan's chunk.
        let named = |m: Memory| m.live + m.free + m.dead + m.spare + m.lent;
        assert_eq!(named(m), c.bytes() as u64 / 16);
        c.forget(0);
        let m = c.memory();
        assert_eq!((m.live, m.dead), (6, 1));
        assert_eq!(named(m), c.bytes() as u64 / 16);
        for a in 1..4u64 {
            c.forget(a);
        }
        let m = c.memory();
        assert_eq!((m.live, m.dead, m.spare), (3, 0, 4));
        assert_eq!(named(m), c.bytes() as u64 / 16);
        // A spare chunk is lent whole.
        let k = c.lend(2).unwrap();
        let m = c.memory();
        assert_eq!((m.spare, m.lent, m.loans, m.loan_room), (0, 4, 2, 8));
        assert_eq!(named(m), c.bytes() as u64 / 16);
        c.unlend(k);
        assert_eq!(c.memory().spare, 4);
    }

    #[test]
    fn a_loan_s_chunk_whose_pages_are_all_forgotten_is_lent_again_already_mapped() {
        let mut c = Cache::new(8, 16, EXTENT);
        let mut k = c.lend(4).unwrap();
        for a in 0..4u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        let bytes = c.bytes();
        for a in 0..3u64 {
            c.forget(a);
        }
        // One page still live: the chunk is not lent.
        let fresh = c.lend(4).unwrap();
        assert_eq!(fresh.fresh, 4, "a chunk with a live page was lent");
        c.unlend(fresh);
        c.forget(3);
        let mut k = c.lend(4).unwrap();
        assert_eq!(k.fresh, 0, "the spare chunk was not lent");
        for a in 10..13u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        assert_eq!(c.bytes(), bytes, "recycling took memory");
        let mut out = Vec::new();
        for a in 10..13u64 {
            out.clear();
            assert!(c.get(a, &mut out));
            assert_eq!(out, page(a));
        }
        for a in 0..4u64 {
            assert!(!c.get(a, &mut out));
        }
    }

    #[test]
    fn reads_take_no_slot_of_a_loan_s_chunk_so_it_empties_whole() {
        let mut c = Cache::new(64, 16, EXTENT);
        let mut k = c.lend(4).unwrap();
        for a in 0..4u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        c.forget(0);
        // Reads insert into chunks of their own, never the loan's freed slot.
        for a in 100..140u64 {
            c.insert(a, &page(a));
        }
        for a in 1..4u64 {
            c.forget(a);
        }
        let k = c.lend(4).unwrap();
        assert_eq!(k.fresh, 0, "the loan's chunk did not empty whole");
        c.unlend(k);
        let mut out = Vec::new();
        for a in 100..140u64 {
            out.clear();
            if c.get(a, &mut out) {
                assert_eq!(out, page(a));
            }
        }
    }

    #[test]
    fn a_lowered_limit_gives_back_spare_loans_before_a_live_page() {
        let mut c = Cache::new(16, 16, EXTENT);
        let mut k = c.lend(8).unwrap();
        for a in 0..8u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        let mut k = c.lend(4).unwrap();
        for a in 20..24u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        for a in 0..8u64 {
            c.forget(a);
        }
        c.resize(4);
        while c.trim(64) {}
        // The spare chunk's eight pages went; the four live pages stayed.
        assert_eq!(c.bytes(), 4 * 16);
        let mut out = Vec::new();
        for a in 20..24u64 {
            out.clear();
            assert!(c.get(a, &mut out));
            assert_eq!(out, page(a));
        }
    }

    /// Chunks whose memory went back keep their slots in the table, and the room they gave back
    /// is the reads' again, made in idle time and by the reads themselves, up to the limit: a 1B
    /// store's cache, its fill's loans forgotten and their memory given back, took no page more
    /// 17 GiB under its limit (docs/research/65 §5).
    #[test]
    fn reads_take_the_room_hollowed_loan_chunks_gave_back() {
        let mut c = Cache::new(64, 16, EXTENT);
        let mut k = c.lend(48).unwrap();
        for a in 1000..1048u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        for a in 1000..1000 + 48 {
            c.forget(a);
        }
        // A lowered limit gives the spare chunks' memory back; it rises again.
        c.resize(1);
        while c.trim(64) {}
        assert_eq!(c.bytes(), 0);
        assert_eq!(c.slots.len(), 48, "the hollow chunks' slots left the table");
        c.resize(64);
        c.wanted = 32;
        assert_eq!(
            c.reserve(usize::MAX),
            32,
            "idle time made fewer slots than wanted"
        );
        for a in 0..64u64 {
            c.insert(a, &page(a));
        }
        assert_eq!(
            c.memory().live,
            64,
            "reads were refused room under the limit"
        );
        let mut out = Vec::new();
        for a in 0..64u64 {
            out.clear();
            assert!(c.get(a, &mut out));
            assert_eq!(out, page(a));
        }
    }

    /// Loans that come and go, each forgotten and its memory given back to a lowered limit,
    /// reuse the slots of the chunks given back: their pages come back cached, and the slot table
    /// stays within twice the largest limit however many loans pass.
    #[test]
    fn loans_that_come_and_go_reuse_the_slots_of_memory_given_back() {
        let mut c = Cache::new(64, 16, EXTENT);
        c.loan_chunk = 4;
        let mut out = Vec::new();
        for round in 0..1000u64 {
            let mut k = c.lend(32).unwrap();
            let first = 10_000 + round * 32;
            for a in first..first + 32 {
                k.keep(a, &page(a));
            }
            c.adopt(k);
            for a in first..first + 32 {
                out.clear();
                assert!(c.get(a, &mut out), "round {round}: page {a} not cached");
                assert_eq!(out, page(a));
            }
            for a in first..first + 32 {
                c.forget(a);
            }
            c.resize(1);
            while c.trim(64) {}
            c.resize(64);
            assert!(
                c.slots.len() <= 2 * 64,
                "round {round}: {} slots",
                c.slots.len()
            );
        }
    }

    /// Idle reservation that can make no slot stops owing them: the machine refusing the cache
    /// a chunk ends `reserve` with nothing left owed, rather than leaving slots wanted that
    /// idle time asks for again and again.
    #[test]
    fn slots_none_can_be_made_for_are_not_owed() {
        let mut c = Cache::new(8, 16, EXTENT);
        c.set_available(|| Some(0));
        c.wanted = 8;
        assert!(c.unreserved() > 0);
        assert_eq!(c.reserve(8), 0);
        assert_eq!(c.unreserved(), 0, "slots owed that none can be made for");
    }

    #[test]
    fn a_loan_holds_its_room_until_it_comes_back() {
        let mut c = Cache::new(8, 16, EXTENT);
        let a = c.lend(6).unwrap();
        let b = c.lend(6).unwrap();
        assert_eq!((a.fresh, b.fresh), (6, 2));
        assert!(c.lend(1).is_none());
        c.unlend(a);
        assert_eq!(c.lend(9).unwrap().fresh, 6);
    }

    #[test]
    fn keeping_stops_at_the_room_lent_and_skips_what_a_slot_cannot_hold() {
        let mut c = Cache::new(8, 16, EXTENT);
        let mut k = c.lend(2).unwrap();
        k.keep(1, &[7; 17]);
        for a in 2..6u64 {
            k.keep(a, &page(a));
        }
        assert_eq!(k.len(), 2);
        assert_eq!(k.bufs[0].buf.capacity(), 2 * 16);
        c.adopt(k);
        let mut out = Vec::new();
        assert!(!c.get(1, &mut out));
        assert!(c.get(2, &mut out) && c.get(3, &mut out));
        assert!(!c.get(4, &mut out));
    }

    #[test]
    fn adopted_pages_past_a_lowered_limit_are_evicted_and_a_kept_address_replaces_the_cached() {
        let mut c = Cache::new(8, 16, EXTENT);
        c.insert(1, b"old");
        let mut k = c.lend(4).unwrap();
        for a in 1..5u64 {
            k.keep(a, &page(a));
        }
        c.resize(3);
        c.adopt(k);
        assert!(c.held() <= 3);
        let mut out = Vec::new();
        if c.get(1, &mut out) {
            assert_eq!(out, page(1));
        }
    }

    #[test]
    fn nothing_is_lent_while_a_chunk_retires() {
        let mut c = warm(1000, 64);
        for a in 0..1000u64 {
            c.insert(a, &page(a));
        }
        c.resize(500);
        // A step a call: the last chunk's retirement begins once the pages are within the limit.
        for _ in 0..2000 {
            if c.retiring.is_some() {
                break;
            }
            c.trim(1);
        }
        assert!(c.retiring.is_some());
        assert!(c.lend(1).is_none());
    }

    /// A retirement ends once a raised limit holds the chunks' room, with no trim or idle
    /// reservation after: while it lasted the cache lent nothing and gave up each loan that came
    /// back, so a limit that fell and rose again on a shard too busy for idle work left a 1B fill's
    /// cache taking none of the pages its compactions wrote (docs/research/65 §5).
    #[test]
    fn a_retirement_ends_once_a_raised_limit_holds_the_room() {
        let mut c = warm(1000, 64);
        for a in 0..1000u64 {
            c.insert(a, &page(a));
        }
        c.resize(500);
        for _ in 0..2000 {
            if c.retiring.is_some() {
                break;
            }
            c.trim(1);
        }
        assert!(c.retiring.is_some());
        c.resize(1100);
        assert!(c.retiring.is_none());
        let mut k = c.lend(4).expect("a loan within the raised limit");
        for a in 2000..2004u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        for a in 2000..2004u64 {
            let mut out = Vec::new();
            assert!(c.get(a, &mut out), "page {a} was not cached");
            assert_eq!(out, page(a));
        }
        // What the retirement had not dropped reads back exactly.
        for a in 0..1000u64 {
            let mut out = Vec::new();
            if c.get(a, &mut out) {
                assert_eq!(out, page(a));
            }
        }
    }

    /// A retirement ends once the spare loan memory a trim gives back brings the room within the
    /// limit: the trim that hollows the spare chunk ends it, where it was ended only before the
    /// spare memory went, and lasted until some later trim or idle reservation.
    #[test]
    fn a_retirement_ends_once_spare_memory_given_back_brings_the_room_within_the_limit() {
        let mut c = Cache::new(1000, 64, EXTENT);
        // A loan's chunk of 200 pages first, then 800 read slots after it: one buffer of 200
        // pages on every platform, where a buffer is a huge page's pages on Linux and an
        // extent's elsewhere (`loan_chunk_pages`).
        c.loan_chunk = 200;
        let mut k = c.lend(200).unwrap();
        for a in 5000..5200u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        c.wanted = 800;
        c.reserve(usize::MAX);
        for a in 0..800u64 {
            c.insert(a, &page(a));
        }
        assert_eq!(c.room(), 1000);
        // A lowered limit: the last chunk, of read slots, begins to retire.
        c.resize(900);
        for _ in 0..2000 {
            if c.retiring.is_some() {
                break;
            }
            c.trim(1);
        }
        assert!(c.retiring.is_some());
        // The loan's pages forgotten, as branches merged away forget theirs: its chunk is spare,
        // and the next trim gives its memory back.
        for a in 5000..5200u64 {
            c.forget(a);
        }
        c.trim(1);
        assert!(c.room() <= 900, "room {}", c.room());
        assert!(c.retiring.is_none(), "the retirement outlived its need");
        // The 100 pages under the limit are short of the hollow chunk's 200 slots, which fresh
        // memory takes whole while a hollow chunk is left: the limit raised, a loan takes it.
        assert!(
            c.lend(4).is_none(),
            "fresh memory short of a hollow chunk's slots was lent"
        );
        c.resize(1000);
        let mut k = c.lend(4).expect("a loan within the limit");
        assert_eq!(k.fresh, 200, "the hollow chunk was not refilled whole");
        for a in 6000..6004u64 {
            k.keep(a, &page(a));
        }
        c.adopt(k);
        let mut out = Vec::new();
        assert!(c.get(6003, &mut out), "the loan's page was not cached");
        assert_eq!(out, page(6003));
    }

    /// A cache whose slots are all made, as idle time leaves one its reads have filled.
    fn warm(pages: usize, page: usize) -> Cache {
        let mut c = Cache::new(pages, page, EXTENT);
        c.wanted = pages;
        c.reserve(usize::MAX);
        c
    }

    /// Whatever reads, loans, forgetting, limits and trims came, a read never returns a page older
    /// than the address's newest. Loans come in buffers of two pages, so a loan's pages come back in several chunks; the
    /// chunks' room, their spare pages and those out on loan, kept as fields, are their sums
    /// over the chunks after every step, and a chunk retires only while the room is past the
    /// limit. Every hollow chunk is listed for the next loan's fresh memory, and the slot table
    /// stays within twice the largest limit.
    #[test]
    fn the_fields_the_slot_table_and_the_hollow_list_hold_through_every_step() {
        let mut c = warm(64, 32);
        c.loan_chunk = 2;
        let mut largest = 64usize;
        let mut latest: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut out = Vec::new();
        for step in 0..40_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let address = x % 160;
            let payload: Vec<u8> = (0..(1 + x % 24))
                .map(|b| ((b ^ address ^ u64::from(step)) & 0xff) as u8)
                .collect();
            match x % 13 {
                0..=3 => {
                    c.insert(address, &payload);
                    latest.insert(address, payload);
                }
                4..=6 => {
                    out.clear();
                    if c.get(address, &mut out) {
                        assert_eq!(
                            Some(&out),
                            latest.get(&address),
                            "step {step} read {address}"
                        );
                    }
                }
                7 => {
                    c.forget(address);
                    latest.remove(&address);
                }
                8 => {
                    // A grant: the extent's pages forgotten whole.
                    let first = address - address % EXTENT;
                    for a in first..first + EXTENT {
                        c.forget(a);
                        latest.remove(&a);
                    }
                }
                9 => {
                    // A job's pages back in the memory lent, an extent's row from its middle.
                    if let Some(mut k) = c.lend(1 + usize::try_from((x >> 20) % 6).unwrap()) {
                        for a in address..address + 1 + (x >> 30) % 6 {
                            let p: Vec<u8> = (0..(1 + a % 16))
                                .map(|b| u8::try_from((b ^ a) & 0xff).unwrap())
                                .collect();
                            let before = k.len();
                            k.keep(a, &p);
                            if k.len() > before {
                                latest.insert(a, p);
                            }
                        }
                        c.adopt(k);
                    }
                }
                10 => {
                    let pages = 1 + usize::try_from((x >> 20) % 96).unwrap();
                    c.resize(pages);
                    largest = largest.max(pages);
                }
                _ => {
                    c.trim(1 + usize::try_from((x >> 20) % 8).unwrap());
                }
            }
            let sum = |want: fn(&Held) -> bool| -> usize {
                c.held
                    .iter()
                    .zip(&c.sizes)
                    .filter(|(h, _)| want(h))
                    .map(|(_, &s)| s)
                    .sum()
            };
            assert_eq!(
                c.room_pages,
                c.sizes.iter().sum::<usize>(),
                "step {step}: room"
            );
            assert_eq!(
                c.spare_pages,
                sum(|h| matches!(h, Held::Spare)),
                "step {step}: spare"
            );
            assert_eq!(
                c.lent_pages,
                sum(|h| matches!(h, Held::Lent)),
                "step {step}: lent"
            );
            assert!(
                c.retiring.is_none() || c.room() > c.limit,
                "step {step}: a retirement past its need, room {} limit {}",
                c.room(),
                c.limit
            );
            assert!(
                c.sizes
                    .iter()
                    .zip(&c.held)
                    .all(|(&s, h)| *h == Held::Grown || s <= 2),
                "step {step}: a loan's chunk past two pages"
            );
            assert!(
                c.slots.len() <= 2 * largest,
                "step {step}: {} slots beside the largest limit {largest}",
                c.slots.len()
            );
            assert_eq!(
                c.hollows.len(),
                c.held.iter().filter(|h| **h == Held::Hollow).count(),
                "step {step}: a hollow chunk unlisted"
            );
            assert!(
                c.hollows.iter().all(|&h| c.held[h] == Held::Hollow),
                "step {step}: a listed chunk not hollow"
            );
            assert!(
                !c.held.contains(&Held::Refilled),
                "step {step}: a chunk refilled with no loan out"
            );
            // Spare memory never outgrows what the loans have had out at once.
            let spare: usize = c
                .held
                .iter()
                .zip(&c.sizes)
                .filter(|(h, _)| matches!(h, Held::Spare))
                .map(|(_, &s)| s)
                .sum();
            assert!(
                spare <= c.out_most - c.out_on_loan(),
                "step {step}: spare {spare} beside {} out of the most {}",
                c.out_on_loan(),
                c.out_most
            );
            // Each live slot is its address's one entry in the index, and each entry a live slot.
            let entries = c.map.entries();
            assert!(entries.iter().all(|&(a, i)| {
                let i = usize::try_from(i).unwrap();
                c.slots[i].live && c.slots[i].address == a
            }));
            assert_eq!(
                entries.len(),
                c.slots.iter().filter(|s| s.live).count(),
                "step {step}"
            );
        }
    }

    /// Loans adopted with their pages kept, one after another, then every page forgotten, as
    /// branches merged away forget theirs: the chunks they leave spare hold no more than the most
    /// the loans had out at once, the rest of their memory given back, and the next loan takes the
    /// spare rather than fresh memory.
    #[test]
    fn spare_loan_memory_holds_no_more_than_the_loans_had_out_at_once() {
        let mut c = Cache::new(1 << 10, 64, EXTENT);
        let spare = |c: &Cache| -> usize {
            c.held
                .iter()
                .zip(&c.sizes)
                .filter(|(h, _)| matches!(h, Held::Spare))
                .map(|(_, &s)| s)
                .sum()
        };
        let mut addresses = Vec::new();
        for loan in 0..8u64 {
            let mut k = c.lend(16).unwrap();
            for a in loan * 16..loan * 16 + 16 {
                k.keep(a, &[u8::try_from(a % 251).unwrap(); 8]);
                addresses.push(a);
            }
            c.adopt(k);
        }
        assert_eq!(c.out_most, 16, "one loan out at a time");
        assert_eq!(spare(&c), 0, "every loan's pages live");
        let before = c.bytes();
        for &a in &addresses {
            c.forget(a);
        }
        assert!(spare(&c) <= 16, "spare {} beside a most of 16", spare(&c));
        assert_eq!(
            before - c.bytes(),
            7 * 16 * 64,
            "seven loans' memory given back"
        );
        let k = c.lend(16).unwrap();
        assert_eq!(
            k.fresh, 0,
            "the next loan took fresh memory beside the spare"
        );
        c.unlend(k);
    }

    #[test]
    fn a_cache_takes_memory_only_as_reads_want_it_in_chunks_and_never_on_a_read() {
        // A cache of a million pages takes memory as its reads come, not its limit: the first
        // read makes its own slot, and idle time then makes as many as the reads since wanted.
        let mut c = Cache::new(1 << 20, 64, EXTENT);
        assert_eq!(c.unreserved(), 0);
        c.insert(1, &page(1));
        assert!(c.contains(1));
        assert_eq!(c.slots.len(), 1);
        assert_eq!(c.reserve(usize::MAX), 1);
        assert_eq!(c.unreserved(), 0);
        // The next read takes the slot idle time made; the one after makes its own. Each chunk
        // has room for as many pages as those before it.
        for a in 2..4u64 {
            c.insert(a, &page(a));
        }
        assert!(c.contains(2) && c.contains(3));
        assert_eq!(c.grown(), 2);
        assert_eq!(c.sizes, vec![1, 1, 2]);
        assert_eq!(c.bytes(), c.slots.len() * 64);
        assert_eq!(c.slots.len(), 3);
        // Chunks double up to a tuning step of the limit, so a cache filled to its limit took
        // O(log n) of them plus the steps.
        let mut c = Cache::new(4096, 64, EXTENT);
        c.wanted = 4096;
        c.reserve(usize::MAX);
        assert_eq!(c.slots.len(), 4096);
        let step = 4096 * TUNE_STEP_PERCENT / 100;
        assert!(c.sizes.iter().all(|&p| p <= step), "{:?}", c.sizes);
        assert!(c.chunks.len() <= 13 + 4096 / step + 1, "{}", c.chunks.len());
    }

    #[test]
    fn a_lowered_limit_gives_back_whole_chunks_the_last_first() {
        let mut c = warm(1000, 64);
        for a in 0..1000u64 {
            c.insert(a, &page(a));
        }
        let before = c.chunks.len();
        c.resize(500);
        while c.trim(64) {}
        assert!(c.room() <= 500, "{}", c.room());
        assert!(c.chunks.len() < before);
        // What it holds reads back exactly; what it dropped misses.
        for a in 0..1000u64 {
            let mut out = Vec::new();
            if c.get(a, &mut out) {
                assert_eq!(out, page(a));
            }
        }
        assert!(c.held() <= 500);
        // Grown back, it makes slots again as reads want them.
        c.resize(1000);
        for a in 1000..1100u64 {
            c.insert(a, &page(a));
        }
        c.reserve(usize::MAX);
        for a in 1000..1100u64 {
            c.insert(a, &page(a));
        }
        assert!(c.contains(1099));
    }

    #[test]
    fn a_cached_page_reads_back_and_a_forgotten_one_misses() {
        let mut c = warm(10, 64);
        c.insert(7, &page(7));
        let mut out = Vec::new();
        assert!(c.get(7, &mut out));
        assert_eq!(out, page(7));
        c.forget(7);
        assert!(!c.get(7, &mut Vec::new()));
        // Rewritten: the new payload, never the old.
        c.insert(7, b"old");
        c.insert(7, b"new");
        let mut out = Vec::new();
        assert!(c.get(7, &mut out));
        assert_eq!(out, b"new");
    }

    #[test]
    fn repeated_rewrites_and_invalidations_keep_serving_current_pages() {
        let mut c = warm(64, 64);
        for a in 0..64u64 {
            c.insert(a, &page(a));
        }
        for _ in 0..3 {
            for a in 0..6u64 {
                assert!(c.get(a, &mut Vec::new()));
            }
        }
        // Rewrite pages throughout a full queue, including its newest page. Every rewrite
        // replaces a cached address, which used to leave another entry for later cleanup.
        for round in 0..512u64 {
            let a = 6 + round % 58;
            let payload = format!("rewrite {round}");
            c.insert(a, payload.as_bytes());
            let mut out = Vec::new();
            assert!(c.get(a, &mut out));
            assert_eq!(out, payload.as_bytes());
        }
        // A stream still admits its newest pages and retains the pages read again.
        let mut admitted = 0;
        for a in 64..256u64 {
            c.insert(a, &page(a));
            let mut out = Vec::new();
            if c.get(a, &mut out) {
                admitted += 1;
                assert_eq!(out, page(a));
            }
        }
        assert!(admitted > 0);
        for a in 0..6u64 {
            let mut out = Vec::new();
            assert!(c.get(a, &mut out));
            assert_eq!(out, page(a));
        }
        // Remove pages from main and small; the freed slots must serve only their new values.
        for a in [0, 3, 5, 250, 253, 255] {
            c.forget(a);
            assert!(!c.get(a, &mut Vec::new()));
            c.insert(a, b"replacement");
            let mut out = Vec::new();
            assert!(c.get(a, &mut out));
            assert_eq!(out, b"replacement");
        }
    }

    #[test]
    fn an_insert_into_a_cache_read_hot_takes_a_bounded_step_count_and_still_admits() {
        // Every page read to the most frequency: freeing a slot takes a step a page round
        // main, which no one insert pays. Each takes at most the bound, the pages it does not
        // admit are only left out, and the hand goes on, so new pages are admitted again.
        let n = 2048u64;
        let mut c = warm(usize::try_from(n).unwrap(), 64);
        for a in 0..n {
            c.insert(a, &page(a));
        }
        for _ in 0..usize::from(MAX_FREQ) + 2 {
            for a in 0..n {
                assert!(c.get(a, &mut Vec::new()));
            }
        }
        let mut admitted = 0u64;
        for a in n..4 * n {
            c.insert(a, &page(a));
            if c.get(a, &mut Vec::new()) {
                admitted += 1;
            }
        }
        assert!(
            c.evict_steps_most() <= EVICT_STEPS as u64,
            "{}",
            c.evict_steps_most()
        );
        assert!(
            c.skipped() > 0 && admitted > 0,
            "skipped {} admitted {admitted}",
            c.skipped()
        );
        // Whatever it holds reads back exactly.
        for a in 0..4 * n {
            let mut out = Vec::new();
            if c.get(a, &mut out) {
                assert_eq!(out, page(a));
            }
        }
    }

    #[test]
    fn a_scan_does_not_flush_pages_read_again() {
        // Ten slots: pages 0..3 read three times each move to main; a scan of 1,000 pages read
        // once passes through the small queue and leaves them.
        let mut c = warm(10, 64);
        for a in 0..3u64 {
            c.insert(a, &page(a));
        }
        for _ in 0..3 {
            for a in 0..3u64 {
                assert!(c.get(a, &mut Vec::new()));
            }
        }
        for a in 100..1_100u64 {
            c.insert(a, &page(a));
        }
        for a in 0..3u64 {
            let mut out = Vec::new();
            assert!(c.get(a, &mut out), "page {a} flushed by a scan");
            assert_eq!(out, page(a));
        }
        // Bounded: never more pages than the limit, nor a ghost past a few times its window.
        assert!(c.map.len() <= 10);
        assert!(c.ghosts.len() <= 4 * 16);
    }

    #[test]
    fn a_page_read_again_after_its_eviction_enters_main() {
        let mut c = warm(10, 64);
        // Fifteen pages into ten slots: the five oldest, 0 among them, leave the small queue
        // unread for the ghost, which holds nine.
        for a in 0..15u64 {
            c.insert(a, &page(a));
        }
        assert_eq!(c.ghosts.len(), 5);
        // Inserted again, page 0 enters main.
        assert!(c.in_ghost(0));
        c.insert(0, &page(0));
        let i = c.slot_of(0).unwrap();
        assert_eq!(c.slots[i].queue, Queue::Main);
        assert!(!c.in_ghost(0));
    }

    #[test]
    fn the_ghost_names_what_an_eager_window_of_evictions_does_and_stays_bounded() {
        let mut c = warm(40, 64);
        let cap = Cache::ghost_cap(40);
        // The eager window: the last `cap` evictions, newest at the front, a forgotten address
        // leaving a hole where it stands.
        let mut model: VecDeque<Option<u64>> = VecDeque::new();
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for step in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let address = x % 64;
            let named = model.contains(&Some(address));
            if x.is_multiple_of(3) {
                for e in model.iter_mut() {
                    if *e == Some(address) {
                        *e = None;
                    }
                }
                assert_eq!(c.forget_ghost(address), named, "step {step}");
            } else if !named {
                if model.len() >= cap {
                    model.pop_back();
                }
                model.push_front(Some(address));
                c.remember_ghost(address);
            }
            for a in 0..64u64 {
                assert_eq!(c.in_ghost(a), model.contains(&Some(a)), "step {step} {a}");
            }
            assert!(c.ghosts.len() <= 4 * cap, "step {step}: {}", c.ghosts.len());
        }
    }

    #[test]
    fn a_resized_cache_serves_only_what_it_was_given_and_keeps_its_bounds() {
        let mut c = warm(64, 32);
        let mut most = 64;
        let mut latest: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut out = Vec::new();
        let mut held = 0;
        for step in 0..30_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let address = x % 200;
            match x % 11 {
                0..=4 => {
                    let payload: Vec<u8> = (0..(1 + x % 32))
                        .map(|b| ((b ^ address) & 0xff) as u8)
                        .collect();
                    c.insert(address, &payload);
                    latest.insert(address, payload);
                }
                5..=8 => {
                    out.clear();
                    if c.get(address, &mut out) {
                        assert_eq!(
                            Some(&out),
                            latest.get(&address),
                            "step {step} read {address}"
                        );
                    }
                }
                9 if x & (1 << 41) == 0 => {
                    c.forget(address);
                    latest.remove(&address);
                }
                9 => {
                    c.reserve(1 + usize::try_from((x >> 20) % 16).unwrap());
                }
                _ if x & (1 << 40) == 0 => {
                    let pages = 1 + usize::try_from((x >> 20) % 128).unwrap();
                    c.resize(pages);
                    most = most.max(pages);
                }
                _ => {
                    c.trim(1 + usize::try_from((x >> 20) % 8).unwrap());
                }
            }
            // The map names live slots holding their own addresses; free, emptied and live
            // slots are all the slots; the queues' live counts are the pages held, within the
            // limit with the freed buffers; the ghost keeps its share.
            let live = c.slots.iter().filter(|s| s.live).count();
            assert_eq!(c.map.len(), live, "step {step}");
            assert!(c.map.entries().into_iter().all(|(a, i)| {
                let i = usize::try_from(i).unwrap();
                c.slots[i].live && c.slots[i].address == a
            }));
            assert_eq!(
                c.free.len() + c.retired.len() + live,
                c.slots.len(),
                "step {step}"
            );
            assert!(c.free.iter().all(|&i| i < c.slots.len()), "step {step}");
            assert_eq!(c.small_live + c.main_live, live, "step {step}");
            // Never above the limit by more than it was, nor growing while above it.
            assert!(live <= c.limit.max(held), "step {step}");
            held = live;
            if c.over() == 0 {
                assert!(c.slots.len() <= c.limit, "step {step}");
            }
            assert!(c.slots.len() <= most, "step {step}");
            assert!(
                c.ghosts.len() <= 4 * Cache::ghost_cap(most).max(16),
                "step {step}"
            );
            assert_eq!(
                c.bytes(),
                (live + c.free.len() + c.retired.len()) * 32,
                "step {step}"
            );
        }
    }
}
