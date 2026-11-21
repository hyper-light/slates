//! A shard's cache of hot records, after F2's read cache (Kanellis et al., PVLDB 18, 2025, §7;
//! research/37): replicas of records read from the trunk, so a get of a read-hot key skips the
//! filters, the leaf index and the page search. The trunk keeps every original, so the cache needs
//! no durability of its own.
//!
//! Records live in a log of pages, as the paper's read cache does: appended at the tail, evicted
//! at the head a page at a time. A record read since it was appended is carried to the tail when
//! its page is evicted (the paper's second chance), so the most read-hot records stay. A table
//! maps each key's hash ([`crate::branch::filter::hash`]) to its record. The paper's invariant
//! holds: a write to a key drops its cached record ([`RecordCache::invalidate`]) before the write
//! returns, so the cache never answers with a version older than one written.
//!
//! Pages make a resize cheap: growing only raises the page limit, and shrinking evicts head pages,
//! the work eviction would do anyway; nothing is copied.
//!
//! A record: key length (2), value length (4), flags (1), the key's hash (8), the key, the value.
//! A record that would cross a page's end leaves a skip marker and starts the next page.

use std::collections::VecDeque;

use crate::util::incmap::{IncMap, SWEEP};

/// A record's fixed bytes: key length, value length, flags, hash.
const RECORD_HEAD: usize = 2 + 4 + 1 + 8;
/// A key length no record has: the rest of the ring to its end is empty.
const SKIP: u16 = u16::MAX;
/// Flags: the record is current, and it was read since it was appended.
const LIVE: u8 = 1;
const READ: u8 = 2;

/// Pages an append may evict to make room: one whose records were all read again frees no room
/// (they are carried to the tail, the page they fill as large), so a second, which the first's
/// carrying left behind it, lets an append past one read-hot page; any more and one append could
/// carry the whole cache round (7.7 ms at 16 MiB, review §4).
const EVICT_PAGES: usize = 2;

/// The cache: its pages, the log's tail as an offset that only grows, the offset of the first
/// page's start (a multiple of the page size), and the table.
#[derive(Debug)]
pub struct RecordCache {
    pages: VecDeque<Box<[u8]>>,
    page: usize,
    /// The most pages held: the cache's bytes over the page size.
    most: usize,
    first: u64,
    tail: u64,
    table: IncMap,
    /// Read records carried from an evicted page to the tail: at most a page of them.
    carry: Vec<u8>,
    /// The last evicted page, the next one appended: eviction allocates nothing.
    spare: Option<Box<[u8]>>,
    hits: u64,
    misses: u64,
    /// The hashes of records evicted unread, each with the sequence it was remembered at; one
    /// remembered within the last `live_most` is in the ghost, as many as the cache holds
    /// records. A miss on one is a get a larger cache would have served: what the memory tuner
    /// prices the cache's bytes at (research/36). Older entries are swept a few buckets an
    /// eviction (`incmap::SWEEP`), so no operation pays for the ghost at once.
    ghosts: IncMap,
    ghost_seq: u64,
    ghost_hits: u64,
    /// Records held, and the most held since the cache last shrank: the ghost's length, which a
    /// page evicted at once would otherwise shorten as it fills it.
    live: usize,
    live_most: usize,
}

/// A record's head, read at a page position.
#[derive(Clone, Copy, Debug)]
struct Head {
    klen: u16,
    vlen: u32,
    flags: u8,
    hash: u64,
}

impl Head {
    fn read(b: &[u8]) -> Option<Self> {
        let b = b.get(..RECORD_HEAD)?;
        Some(Self {
            klen: u16::from_le_bytes([*b.first()?, *b.get(1)?]),
            vlen: u32::from_le_bytes(b.get(2..6)?.try_into().ok()?),
            flags: *b.get(6)?,
            hash: u64::from_le_bytes(b.get(7..15)?.try_into().ok()?),
        })
    }

    /// The record's bytes, head included.
    fn size(&self) -> Option<usize> {
        RECORD_HEAD
            .checked_add(usize::from(self.klen))?
            .checked_add(usize::try_from(self.vlen).ok()?)
    }
}

/// A record cache's answer ([`RecordCache::lookup`]): the value cached, or a miss and whether the
/// ghost named its key.
#[derive(Debug)]
pub enum Lookup<'a> {
    Hit(&'a [u8]),
    Miss { ghost: bool },
}

impl RecordCache {
    /// A cache of `bytes` of records in pages of `page` bytes, the store's page size: a record
    /// larger than a page is not cached. None fit below a page.
    pub fn new(bytes: usize, page: usize) -> Self {
        let page = page.max(RECORD_HEAD);
        Self {
            pages: VecDeque::new(),
            page,
            most: bytes.checked_div(page).unwrap_or(0),
            first: 0,
            tail: 0,
            table: IncMap::new(),
            carry: Vec::new(),
            spare: None,
            hits: 0,
            misses: 0,
            ghosts: IncMap::new(),
            ghost_seq: 0,
            ghost_hits: 0,
            live: 0,
            live_most: 0,
        }
    }

    /// Misses on keys the ghost named: gets a larger cache would have served.
    pub fn ghost_hits(&self) -> u64 {
        self.ghost_hits
    }

    /// Remembers an evicted record's hash, the ghost as long as the records held.
    fn remember(&mut self, hash: u64) {
        self.ghost_seq = self.ghost_seq.wrapping_add(1);
        self.ghosts.insert(hash, self.ghost_seq);
        let (now, window) = (self.ghost_seq, self.window());
        self.ghosts
            .sweep(SWEEP, |_, seq| now.wrapping_sub(seq) < window);
    }

    /// The ghost's length in evictions.
    fn window(&self) -> u64 {
        u64::try_from(self.live_most.max(1)).unwrap_or(u64::MAX)
    }

    /// Whether `hash` was evicted within the ghost's window.
    fn in_ghost(&self, hash: u64) -> bool {
        self.ghosts
            .get(hash)
            .is_some_and(|seq| self.ghost_seq.wrapping_sub(seq) < self.window())
    }

    /// Sets the cache's page limit for `bytes`: growing takes effect at once, and a cache above a
    /// lowered limit is brought down by `trim`.
    pub fn resize(&mut self, bytes: usize) {
        self.most = bytes.checked_div(self.page).unwrap_or(0);
    }

    /// Up to `n` head pages dropped toward the page limit, their records leaving for the ghost.
    /// True while the cache is still above it. Meanwhile an insert evicts a page for each it
    /// adds, so the pages held only fall.
    pub fn trim(&mut self, n: usize) -> bool {
        let over = self.pages.len() > self.most;
        for _ in 0..n {
            if self.pages.len() <= self.most || self.drop_head(false).is_none() {
                break;
            }
        }
        // A smaller cache holds fewer records, so its ghost covers fewer; a larger one keeps the
        // window it had until it fills.
        if over && self.pages.len() <= self.most {
            self.live_most = self.live;
        }
        self.pages.len() > self.most
    }

    /// The bytes its index and ghost take.
    pub fn index_bytes(&self) -> usize {
        self.table.bytes().saturating_add(self.ghosts.bytes())
    }

    /// The bytes of the pages held.
    pub fn held_bytes(&self) -> usize {
        self.pages.len().saturating_mul(self.page)
    }

    /// Pages held above the page limit: what `trim` has left to do.
    pub fn over(&self) -> usize {
        self.pages.len().saturating_sub(self.most)
    }

    /// The cache's bytes: its page limit.
    pub fn bytes(&self) -> usize {
        self.most.saturating_mul(self.page)
    }

    /// Gets served, and gets missed.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    fn page_bytes(&self) -> u64 {
        u64::try_from(self.page).unwrap_or(u64::MAX)
    }

    /// The page and position of an offset.
    fn at(&self, offset: u64) -> Option<(usize, usize)> {
        let rel = offset.checked_sub(self.first)?;
        let index = usize::try_from(rel.checked_div(self.page_bytes())?).ok()?;
        let pos = usize::try_from(rel.checked_rem(self.page_bytes())?).ok()?;
        Some((index, pos))
    }

    /// The bytes of the page holding `offset`, from `offset` on.
    fn bytes_at(&self, offset: u64) -> Option<&[u8]> {
        let (index, pos) = self.at(offset)?;
        self.pages.get(index)?.get(pos..)
    }

    fn set_flags(&mut self, offset: u64, flags: u8) {
        if let Some((index, pos)) = self.at(offset)
            && let Some(f) = self
                .pages
                .get_mut(index)
                .and_then(|p| p.get_mut(pos.saturating_add(6)))
        {
            *f = flags;
        }
    }

    /// The bytes a record of `klen` and `vlen` takes.
    fn size(klen: usize, vlen: usize) -> Option<usize> {
        RECORD_HEAD.checked_add(klen)?.checked_add(vlen)
    }

    /// The record at `offset`, with its key and value, when it is the current one for `key`.
    fn current(&self, offset: u64, key: &[u8]) -> Option<(Head, &[u8])> {
        let b = self.bytes_at(offset)?;
        let h = Head::read(b)?;
        let kend = RECORD_HEAD.checked_add(usize::from(h.klen))?;
        if h.flags & LIVE == 0 || b.get(RECORD_HEAD..kend)? != key {
            return None;
        }
        Some((h, b.get(kend..h.size()?)?))
    }

    /// The value cached for `key` of hash `hash`, if its record is current; counted as read, so
    /// it has a second chance at the head.
    pub fn get(&mut self, key: &[u8], hash: u64) -> Option<&[u8]> {
        match self.lookup(key, hash) {
            Lookup::Hit(value) => Some(value),
            Lookup::Miss { .. } => None,
        }
    }

    /// [`Self::get`], a miss saying whether the ghost named its key: a get a larger cache would
    /// have served.
    pub fn lookup(&mut self, key: &[u8], hash: u64) -> Lookup<'_> {
        // A phase of hits writes nothing: the read moves the index's migration on.
        self.table.settle();
        let Some(offset) = self.table.get(hash) else {
            self.misses = self.misses.saturating_add(1);
            let ghost = self.in_ghost(hash);
            if ghost {
                self.ghost_hits = self.ghost_hits.saturating_add(1);
            }
            return Lookup::Miss { ghost };
        };
        let Some((h, _)) = self.current(offset, key) else {
            self.misses = self.misses.saturating_add(1);
            return Lookup::Miss { ghost: false };
        };
        if h.flags & READ == 0 {
            self.set_flags(offset, h.flags | READ);
        }
        self.hits = self.hits.saturating_add(1);
        match self.current(offset, key) {
            Some((_, value)) => Lookup::Hit(value),
            None => Lookup::Miss { ghost: false },
        }
    }

    /// Drops `key`'s record, if cached: a write to the key makes it stale.
    pub fn invalidate(&mut self, key: &[u8], hash: u64) {
        // Only a live record is dropped: with none, nothing is looked up.
        if self.live == 0 {
            return;
        }
        let Some(offset) = self.table.get(hash) else {
            return;
        };
        if let Some((h, _)) = self.current(offset, key) {
            self.set_flags(offset, h.flags & !LIVE);
            self.table.remove(hash);
            self.live = self.live.saturating_sub(1);
        }
    }

    /// Caches `value` for `key` of hash `hash`, evicting head pages as it needs room. A record
    /// larger than a page is not cached.
    pub fn insert(&mut self, key: &[u8], hash: u64, value: &[u8]) {
        // The key's older record goes first, whether or not this one is cached: a cache that
        // keeps it would answer with a version older than the one just read.
        self.invalidate(key, hash);
        let (Ok(klen), Ok(vlen)) = (u16::try_from(key.len()), u32::try_from(value.len())) else {
            return;
        };
        if klen == SKIP || Self::size(key.len(), value.len()).is_none_or(|s| s > self.page) {
            return;
        }
        if let Some(offset) = self.append(klen, vlen, hash, key, value, LIVE) {
            self.table.insert(hash, offset);
            self.live = self.live.saturating_add(1);
            self.live_most = self.live_most.max(self.live);
            self.ghosts.remove(hash);
        }
    }

    /// Room for `size` bytes at the tail: in the tail page, or in a new page when the limit
    /// allows one, else none.
    fn room(&mut self, size: usize) -> Option<()> {
        let size64 = u64::try_from(size).ok()?;
        let held = self.first.checked_add(
            u64::try_from(self.pages.len())
                .ok()?
                .checked_mul(self.page_bytes())?,
        )?;
        let page_end = held.min(
            self.tail
                .checked_div(self.page_bytes())?
                .checked_add(1)?
                .checked_mul(self.page_bytes())?,
        );
        if self.tail.checked_add(size64)? <= page_end && self.tail < held {
            return Some(());
        }
        if self.tail < held {
            // The rest of the tail page is too short: marked empty, the record starts the next.
            if let Some((index, pos)) = self.at(self.tail)
                && let Some(b) = self
                    .pages
                    .get_mut(index)
                    .and_then(|p| p.get_mut(pos..pos.saturating_add(2)))
            {
                b.copy_from_slice(&SKIP.to_le_bytes());
            }
            self.tail = held;
        }
        // A page just evicted is appended in its place even above a lowered limit: the pages
        // held never grow there, and `trim` brings them down.
        if self.pages.len() >= self.most && self.spare.is_none() {
            return None;
        }
        let page = self
            .spare
            .take()
            .unwrap_or_else(|| vec![0; self.page].into_boxed_slice());
        self.pages.push_back(page);
        Some(())
    }

    /// Appends a record at the tail, evicting head pages until it fits; its offset, none if it
    /// could not.
    fn append(
        &mut self,
        klen: u16,
        vlen: u32,
        hash: u64,
        key: &[u8],
        value: &[u8],
        flags: u8,
    ) -> Option<u64> {
        let size = Self::size(key.len(), value.len())?;
        // Each round evicts a page, at most `EVICT_PAGES` an append: admission is optional (the
        // caller has its record either way), and a cache read whole would otherwise carry every
        // page round once before one freed room. A page whose records were all read again
        // frees nothing but clears their second chance, so the head moves on across appends
        // and later ones find room.
        let mut fits = false;
        for _ in 0..=EVICT_PAGES {
            if self.room(size).is_some() {
                fits = true;
                break;
            }
            self.evict_head()?;
        }
        if !fits {
            // The page this append evicted stays out: appended later, it would grow a cache at
            // or above its limit.
            if self.pages.len() >= self.most {
                self.spare = None;
            }
            return None;
        }
        self.write(klen, vlen, hash, key, value, flags)
    }

    /// Writes a record at the tail, where [`Self::room`] made room for it; its offset.
    fn write(
        &mut self,
        klen: u16,
        vlen: u32,
        hash: u64,
        key: &[u8],
        value: &[u8],
        flags: u8,
    ) -> Option<u64> {
        let size = Self::size(key.len(), value.len())?;
        let offset = self.tail;
        let (index, pos) = self.at(offset)?;
        let dst = self
            .pages
            .get_mut(index)?
            .get_mut(pos..pos.checked_add(size)?)?;
        let (head, rest) = dst.split_at_mut(RECORD_HEAD);
        head.get_mut(..2)?.copy_from_slice(&klen.to_le_bytes());
        head.get_mut(2..6)?.copy_from_slice(&vlen.to_le_bytes());
        *head.get_mut(6)? = flags;
        head.get_mut(7..15)?.copy_from_slice(&hash.to_le_bytes());
        let (k, v) = rest.split_at_mut(key.len());
        k.copy_from_slice(key);
        v.copy_from_slice(value);
        self.tail = self.tail.checked_add(u64::try_from(size).ok()?)?;
        Some(offset)
    }

    /// Evicts the head page: its unread records leave (remembered by the ghost), and its read
    /// ones are carried to the tail with their read flag cleared, their second chance.
    fn evict_head(&mut self) -> Option<()> {
        self.drop_head(true)?;
        let mut carry = std::mem::take(&mut self.carry);
        let mut at = 0usize;
        // Each round appends a carried record: at most a page of them.
        for _ in 0..=self.page {
            let Some(h) = carry.get(at..).and_then(Head::read) else {
                break;
            };
            let size = h.size()?;
            let rec = carry.get(at..at.checked_add(size)?)?;
            let kend = RECORD_HEAD.checked_add(usize::from(h.klen))?;
            let (key, value) = (rec.get(RECORD_HEAD..kend)?, rec.get(kend..)?);
            // Room is there: the records came from one page and the tail has a new one.
            let placed = self
                .room(size)
                .and_then(|()| self.write(h.klen, h.vlen, h.hash, key, value, LIVE));
            if let Some(offset) = placed {
                self.table.insert(h.hash, offset);
            } else {
                self.live = self.live.saturating_sub(1);
            }
            at = at.checked_add(size)?;
        }
        carry.clear();
        self.carry = carry;
        Some(())
    }

    /// Removes the head page and its records from the table; the read ones, when `carry`, are
    /// copied out to be appended again, the rest remembered by the ghost.
    fn drop_head(&mut self, carry: bool) -> Option<()> {
        let page = self.pages.pop_front()?;
        let start = self.first;
        // The page's records end at the tail when it is the tail's page.
        let written = usize::try_from(self.tail.saturating_sub(start))
            .unwrap_or(usize::MAX)
            .min(page.len());
        self.first = self.first.checked_add(self.page_bytes())?;
        if self.tail < self.first {
            self.tail = self.first;
        }
        let mut pos = 0usize;
        // Each round passes a record: at most a page of them.
        for _ in 0..=self.page {
            let Some(b) = page.get(pos..written) else {
                break;
            };
            if b.len() < 2
                || b.first_chunk::<2>()
                    .is_some_and(|m| u16::from_le_bytes(*m) == SKIP)
            {
                break;
            }
            let Some(h) = Head::read(b) else { break };
            let Some(size) = h.size() else { break };
            let offset = start.checked_add(u64::try_from(pos).ok()?)?;
            if h.flags & LIVE != 0 && self.table.get(h.hash) == Some(offset) {
                self.table.remove(h.hash);
                if carry && h.flags & READ != 0 {
                    self.carry.extend_from_slice(b.get(..size)?);
                } else {
                    self.live = self.live.saturating_sub(1);
                    self.remember(h.hash);
                }
            }
            pos = pos.checked_add(size)?;
        }
        if carry {
            self.spare = Some(page);
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch::filter::hash;
    use std::collections::HashMap;

    /// A miss says whether the ghost named its key: of the keys evicted from a full cache, the
    /// ghost names the last evicted, at most as many as the cache ever held, and each such miss
    /// is a ghost hit; a key never cached is not named, and a key cached again is a hit.
    #[test]
    fn a_miss_says_whether_the_ghost_named_its_key() {
        // One page of room: filling past it evicts the first records.
        let mut c = RecordCache::new(256, 256);
        let keys: Vec<[u8; 4]> = (0..64u32).map(u32::to_be_bytes).collect();
        for k in &keys {
            c.insert(k, hash(k), &[7u8; 24]);
        }
        let ghost_before = c.ghost_hits();
        let (mut named, mut unnamed) = (0u64, 0u64);
        let mut last_named = None;
        for k in &keys {
            match c.lookup(k, hash(k)) {
                Lookup::Miss { ghost: true } => {
                    named += 1;
                    last_named = Some(*k);
                }
                Lookup::Miss { ghost: false } => unnamed += 1,
                Lookup::Hit(v) => assert_eq!(v, [7u8; 24]),
            }
        }
        assert_eq!(c.ghost_hits() - ghost_before, named);
        assert!(named >= 1, "the last evicted are not named");
        assert!(
            named <= c.live_most as u64,
            "the ghost holds more than the cache held"
        );
        assert!(unnamed >= 1, "the first evicted are still named");
        let never = 1_000_000u32.to_be_bytes();
        assert!(matches!(
            c.lookup(&never, hash(&never)),
            Lookup::Miss { ghost: false }
        ));
        let again = last_named.unwrap();
        c.insert(&again, hash(&again), &[9u8; 24]);
        assert!(matches!(c.lookup(&again, hash(&again)), Lookup::Hit(v) if v == [9u8; 24]));
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn a_cached_value_is_the_latest_written_or_nothing() {
        for cap in [64usize, 200, 512, 4096] {
            let page = cap.min(256);
            let mut c = RecordCache::new(cap, page);
            // What a get may answer for each key: the value last inserted, or nothing once
            // invalidated (the cache may also have evicted it, so a miss is always allowed).
            let mut latest: HashMap<u32, Option<Vec<u8>>> = HashMap::new();
            let mut x = 0x9e37_79b9_7f4a_7c15u64 ^ cap as u64;
            let mut hits = 0u64;
            let mut pages = 0usize;
            for step in 0..40_000u32 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let n = (x % 48) as u32;
                let key = n.to_be_bytes();
                let h = hash(&key);
                match x % 10 {
                    0..=3 => {
                        let len = (x >> 16) as usize % 60;
                        let value: Vec<u8> = (0..len).map(|i| (i as u32 ^ step) as u8).collect();
                        c.insert(&key, h, &value);
                        latest.insert(n, Some(value));
                    }
                    4 => {
                        c.invalidate(&key, h);
                        latest.insert(n, None);
                    }
                    5 if step % 97 == 0 => {
                        // Resized, the newest records kept: what stays still answers exactly.
                        c.resize(cap / 2 + (x >> 24) as usize % cap);
                    }
                    6 if step % 7 == 0 => {
                        c.trim(1 + (x >> 24) as usize % 4);
                    }
                    _ => {
                        if let Some(v) = c.get(&key, h) {
                            hits += 1;
                            assert_eq!(
                                Some(v),
                                latest.get(&n).and_then(Option::as_deref),
                                "cap {cap} step {step} key {n}"
                            );
                        }
                    }
                }
                // Never above the limit by more than it was, nor growing while above it.
                assert!(c.pages.len() <= c.most.max(pages), "cap {cap} step {step}");
                pages = c.pages.len();
                assert!(
                    c.tail - c.first <= (c.pages.len() * page) as u64,
                    "cap {cap} step {step}"
                );
                for (h, offset) in c.table.entries() {
                    let rec = Head::read(c.bytes_at(offset).unwrap()).unwrap();
                    assert!(
                        rec.flags & LIVE != 0 && rec.hash == h,
                        "cap {cap} step {step}"
                    );
                    assert!(
                        offset >= c.first && offset < c.tail,
                        "cap {cap} step {step}"
                    );
                }
                assert_eq!(c.live, c.table.len(), "cap {cap} step {step}");
                // The ghost stays within a few times its window (see `incmap::SWEEP`).
                assert!(
                    c.ghosts.len() <= 4 * c.live_most.max(16),
                    "cap {cap} step {step}: ghost {} window {}",
                    c.ghosts.len(),
                    c.live_most
                );
            }
            if cap >= 512 {
                assert!(hits > 0, "cap {cap}: the cache served nothing");
            }
        }
    }

    #[test]
    fn a_record_read_again_survives_the_head_once() {
        // Records of 16 + 15 bytes in a page of 124: four fit. The first is read, so when a
        // fifth needs room it is carried to the new tail page and the others are evicted.
        let mut c = RecordCache::new(4 * 31, 4 * 31);
        let keys: Vec<[u8; 4]> = (0..5u32).map(u32::to_be_bytes).collect();
        let value = [7u8; 12];
        for k in &keys[..4] {
            c.insert(k, hash(k), &value);
        }
        assert!(c.get(&keys[0], hash(&keys[0])).is_some());
        c.insert(&keys[4], hash(&keys[4]), &value);
        assert!(
            c.get(&keys[0], hash(&keys[0])).is_some(),
            "the read record was evicted"
        );
        assert!(
            c.get(&keys[1], hash(&keys[1])).is_none(),
            "the unread record stayed"
        );
        assert!(c.get(&keys[4], hash(&keys[4])).is_some());
    }

    #[test]
    fn an_append_into_a_cache_read_whole_carries_a_bounded_number_of_pages() {
        // Sixty-four pages of four records, every one read again: making room would carry every
        // page round once. An append carries at most `EVICT_PAGES` and, finding no room, is left
        // out; every record read stays. Later appends find the pages carried past their second
        // chance and are admitted.
        let pages = 64u32;
        let mut c = RecordCache::new(pages as usize * 4 * 31, 4 * 31);
        let value = [7u8; 12];
        let keys: Vec<[u8; 4]> = (0..pages * 4).map(u32::to_be_bytes).collect();
        for k in &keys {
            c.insert(k, hash(k), &value);
        }
        for k in &keys {
            assert!(c.get(k, hash(k)).is_some());
        }
        let new = (pages * 4).to_be_bytes();
        c.insert(&new, hash(&new), &value);
        assert!(
            c.get(&new, hash(&new)).is_none(),
            "admitted past every page"
        );
        for k in &keys {
            assert!(c.get(k, hash(k)).is_some(), "a record read again was lost");
        }
        let mut admitted = 0;
        for i in pages * 4 + 1..pages * 16 {
            let k = i.to_be_bytes();
            c.insert(&k, hash(&k), &value);
            if c.get(&k, hash(&k)).is_some() {
                admitted += 1;
            }
        }
        assert!(admitted > 0);
    }

    #[test]
    fn a_resize_keeps_the_ghost() {
        // Four records of 31 bytes fill the page; a fifth evicts it, its unread records into the
        // ghost. A tuning step resizes the cache before an evicted key is asked for again: the
        // miss is still one a larger cache would have served.
        let mut c = RecordCache::new(4 * 31, 4 * 31);
        let keys: Vec<[u8; 4]> = (0..5u32).map(u32::to_be_bytes).collect();
        let value = [7u8; 12];
        for k in &keys {
            c.insert(k, hash(k), &value);
        }
        c.resize(5 * 31);
        assert!(!c.trim(usize::MAX));
        assert!(c.get(&keys[0], hash(&keys[0])).is_none());
        assert_eq!(c.ghost_hits(), 1);
    }
}
