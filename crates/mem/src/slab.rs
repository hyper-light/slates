//! A per-shard slab: typed slots with an intrusive free list and a generation per slot, on
//! segmented storage so slots never move (§4.2; [A: Bonwick USENIX'94]).
//!
//! `insert` pops a free slot in O(1); if none is free it takes a slot from the next segment,
//! which came from the reserve (idle-time pre-allocation) or, on a cold path, from the system
//! allocator. `remove` bumps the slot's generation, so every handle issued for the old occupant
//! is refused from then on with `StaleHandle` (T-0.1). Capacity is bounded by the caller so
//! admission stays a decision, not an accident.

use crate::error::MemError;
use crate::handle::Handle;
use crate::segmented::Segmented;

/// A slot: vacant with a link to the next free slot, or occupied.
#[derive(Debug)]
enum Body<T> {
  Vacant { next_free: Option<u32> },
  Occupied(T),
}

#[derive(Debug)]
struct Slot<T> {
  generation: u32,
  body: Body<T>,
}

/// A typed slab with generational handles.
#[derive(Debug)]
pub struct Slab<T> {
  slots: Segmented<Slot<T>>,
  free_head: Option<u32>,
  len: usize,
  max_slots: usize,
  /// The generation a fresh slot starts at (§4.3 slot reuse): a slab that replaces another under the
  /// same shard id starts every slot past the highest generation the old one issued, so a handle
  /// minted for the old slab can never match a slot of this one. Zero for a first slab.
  generation_base: u32,
}

impl<T> Slab<T> {
  /// A slab whose segments hold `segment_slots` slots and which never exceeds `max_slots`.
  pub fn new(segment_slots: usize, max_slots: usize) -> Self {
    Self::with_generation_base(segment_slots, max_slots, 0)
  }

  /// Reserves the entire admission bound in one allocation (§4.2, AC-0.4). Slots are
  /// initialized only on insertion and never move; the last segment holds exactly the bound,
  /// so power-of-two indexing does not double the reserved memory. Use for a fixed-capacity
  /// arena that must never allocate during insertion, such as the runtime's timer wheel.
  pub fn preallocated(max_slots: usize) -> Self {
    Self {
      slots: Segmented::preallocated(max_slots),
      free_head: None,
      len: 0,
      max_slots,
      generation_base: 0,
    }
  }

  /// [`Slab::new`] whose fresh slots start at `generation_base` (see the field).
  pub fn with_generation_base(
    segment_slots: usize,
    max_slots: usize,
    generation_base: u32,
  ) -> Self {
    Self {
      slots: Segmented::new(segment_slots),
      free_head: None,
      len: 0,
      max_slots,
      generation_base,
    }
  }

  /// One past the highest generation any slot of this slab has reached: what a successor slab under
  /// the same shard id must start from so no handle of this slab names one of its slots.
  pub fn generation_high(&self) -> u32 {
    let mut high = self.generation_base;
    for index in 0..self.slots.len() {
      if let Some(slot) = self.slots.get(index) {
        high = high.max(slot.generation.wrapping_add(1));
      }
    }
    high
  }

  /// Pre-allocates `count` segments so that the next inserts allocate nothing.
  pub fn reserve_segments(&mut self, count: usize) {
    self.slots.reserve_segments(count);
  }

  /// The bytes one slot takes: the value plus its generation and vacancy link (§4.2 "segment,
  /// slab and buddy geometry report usable capacity": what a slot costs, not what its value alone
  /// would).
  pub const fn slot_bytes() -> usize {
    size_of::<Slot<T>>()
  }

  /// The most bytes this slab can ever hold: its bound in slots times a slot's bytes — the
  /// footprint an admission must count for the slab (§4.2 metadata dimension), since its segments
  /// grow toward the bound as slots are used.
  pub const fn max_footprint_bytes(&self) -> usize {
    self.max_slots.saturating_mul(Self::slot_bytes())
  }

  /// Occupied slots.
  pub const fn len(&self) -> usize {
    self.len
  }

  /// Slots the slab's segments hold, occupied or free: its memory in slots.
  pub fn capacity(&self) -> usize {
    self.slots.capacity()
  }

  /// Whether no slot is occupied.
  pub const fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// Whether an insert would succeed — the occupied count is below the bound. Lets a caller that
  /// must not lose its value on a full slab check first, since `insert` consumes the value.
  pub const fn has_room(&self) -> bool {
    self.len < self.max_slots
  }

  /// Slots created so far (occupied or vacant).
  pub const fn slots(&self) -> usize {
    self.slots.len()
  }

  /// The bound on slots.
  pub const fn max_slots(&self) -> usize {
    self.max_slots
  }

  /// Stores a value and returns its handle, or `SlabFull` at the bound.
  pub fn insert(&mut self, value: T) -> Result<Handle<T>, MemError> {
    if let Some(index) = self.free_head {
      let slot = self
        .slots
        .get_mut(usize::try_from(index).unwrap_or(usize::MAX))
        .ok_or(MemError::SlabFull {
          capacity: self.max_slots,
        })?;
      let next = match slot.body {
        Body::Vacant { next_free } => next_free,
        Body::Occupied(_) => None,
      };
      slot.body = Body::Occupied(value);
      self.free_head = next;
      self.len += 1;
      return Ok(Handle::new(index, slot.generation));
    }
    if self.slots.len() >= self.max_slots {
      return Err(MemError::SlabFull {
        capacity: self.max_slots,
      });
    }
    let generation = self.generation_base;
    let index = self.slots.push(Slot {
      generation,
      body: Body::Occupied(value),
    });
    self.len += 1;
    let index = u32::try_from(index).map_err(|_| MemError::SlabFull {
      capacity: self.max_slots,
    })?;
    Ok(Handle::new(index, generation))
  }

  /// Places `value` at exactly `index` with `generation` and returns the handle `(index,
  /// generation)`. For rebuilding a slab from a durable image where the handle *is* the identity —
  /// a snapshot id is its slot and generation (§4.8), so a client's id must resolve to the same
  /// slot after recovery: the caller replays the live entries in ascending index order into a fresh
  /// slab and each lands at the exact slot a client still holds, gaps (destroyed entries) becoming
  /// reusable vacant slots on the free list. The contract is append-extending — `index` must be at
  /// or beyond the slots created so far — so no free-list surgery is needed; a lower index (a
  /// duplicate or out-of-order entry, only possible from a corrupt image) is refused with
  /// `OutOfRange`, and an index at or past the bound with `SlabFull`.
  pub fn insert_at(
    &mut self,
    index: u32,
    generation: u32,
    value: T,
  ) -> Result<Handle<T>, MemError> {
    let target = usize::try_from(index).unwrap_or(usize::MAX);
    if target >= self.max_slots {
      return Err(MemError::SlabFull {
        capacity: self.max_slots,
      });
    }
    if target < self.slots.len() {
      return Err(MemError::OutOfRange {
        offset: target,
        len: self.slots.len(),
      });
    }
    // Fill the gap [slots.len(), index) with vacant slots threaded into the free list, so a
    // destroyed entry's slot is reused by a later insert exactly as a fresh slab would reuse it.
    while self.slots.len() < target {
      let at = self.slots.push(Slot {
        generation: 0,
        body: Body::Vacant {
          next_free: self.free_head,
        },
      });
      self.free_head = Some(u32::try_from(at).unwrap_or(u32::MAX));
    }
    // The gap is filled, so this push lands at exactly `target`.
    self.slots.push(Slot {
      generation,
      body: Body::Occupied(value),
    });
    self.len += 1;
    Ok(Handle::new(index, generation))
  }

  /// The value behind a live handle.
  pub fn get(&self, handle: Handle<T>) -> Result<&T, MemError> {
    match self
      .slots
      .get(usize::try_from(handle.index()).unwrap_or(usize::MAX))
    {
      Some(Slot {
        generation,
        body: Body::Occupied(value),
      }) if *generation == handle.generation() => Ok(value),
      _ => Err(stale(handle)),
    }
  }

  /// The value behind a live handle, mutably.
  pub fn get_mut(&mut self, handle: Handle<T>) -> Result<&mut T, MemError> {
    match self
      .slots
      .get_mut(usize::try_from(handle.index()).unwrap_or(usize::MAX))
    {
      Some(Slot {
        generation,
        body: Body::Occupied(value),
      }) if *generation == handle.generation() => Ok(value),
      _ => Err(stale(handle)),
    }
  }

  /// The current generation of a slot by index (occupied or vacant), for structures that link
  /// slots by index and rebuild the handle to check liveness; `None` beyond the slots created.
  pub fn generation_at(&self, index: u32) -> Option<u32> {
    self
      .slots
      .get(usize::try_from(index).unwrap_or(usize::MAX))
      .map(|slot| slot.generation)
  }

  /// Whether the handle is live.
  pub fn contains(&self, handle: Handle<T>) -> bool {
    self.get(handle).is_ok()
  }

  /// Removes the value behind a live handle; the slot's generation moves on.
  pub fn remove(&mut self, handle: Handle<T>) -> Result<T, MemError> {
    let index = usize::try_from(handle.index()).unwrap_or(usize::MAX);
    let slot = self.slots.get_mut(index).ok_or_else(|| stale(handle))?;
    if slot.generation != handle.generation() || matches!(slot.body, Body::Vacant { .. }) {
      return Err(stale(handle));
    }
    let body = std::mem::replace(
      &mut slot.body,
      Body::Vacant {
        next_free: self.free_head,
      },
    );
    slot.generation = slot.generation.wrapping_add(1);
    self.free_head = Some(handle.index());
    self.len -= 1;
    match body {
      Body::Occupied(value) => Ok(value),
      Body::Vacant { .. } => Err(stale(handle)),
    }
  }

  /// Frees a slot without moving its value out: the value is dropped in place, so a large
  /// slot (a 4 KiB directory block) costs no copy. Measured: moving 27,842 such blocks out of
  /// a slab through `remove` cost 2.4 ns per released object of a 10^6-file destroy
  /// (2026-09-05, `cargo run --release -p slates-vfs --example vfs_bench`).
  pub fn discard(&mut self, handle: Handle<T>) -> Result<(), MemError> {
    let index = usize::try_from(handle.index()).unwrap_or(usize::MAX);
    let slot = self.slots.get_mut(index).ok_or_else(|| stale(handle))?;
    if slot.generation != handle.generation() || matches!(slot.body, Body::Vacant { .. }) {
      return Err(stale(handle));
    }
    slot.body = Body::Vacant {
      next_free: self.free_head,
    };
    slot.generation = slot.generation.wrapping_add(1);
    self.free_head = Some(handle.index());
    self.len -= 1;
    Ok(())
  }

  /// Iterates live entries mutably as (handle, value).
  pub fn iter_mut_all(&mut self) -> impl Iterator<Item = (Handle<T>, &mut T)> {
    let generations: Vec<(usize, u32)> = self
      .slots
      .iter()
      .enumerate()
      .map(|(i, s)| (i, s.generation))
      .collect();
    self
      .slots
      .iter_mut_indexed()
      .zip(generations)
      .filter_map(|((_, slot), (i, generation))| match &mut slot.body {
        Body::Occupied(value) => Some((
          Handle::new(u32::try_from(i).unwrap_or(u32::MAX), generation),
          value,
        )),
        Body::Vacant { .. } => None,
      })
  }

  /// Iterates live entries as (handle, value).
  pub fn iter(&self) -> impl Iterator<Item = (Handle<T>, &T)> {
    self
      .slots
      .iter()
      .enumerate()
      .filter_map(|(i, slot)| match &slot.body {
        Body::Occupied(value) => Some((
          Handle::new(u32::try_from(i).unwrap_or(u32::MAX), slot.generation),
          value,
        )),
        Body::Vacant { .. } => None,
      })
  }
}

fn stale<T>(handle: Handle<T>) -> MemError {
  MemError::StaleHandle {
    index: handle.index(),
    generation: handle.generation(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_machine::stats::Xorshift;
  use std::collections::HashSet;

  fn shuffled_handles(slab: &mut Slab<u64>) -> Vec<Handle<u64>> {
    let mut handles: Vec<Handle<u64>> = (0..256).map(|i| slab.insert(i).unwrap()).collect();
    let mut rng = Xorshift::new(Xorshift::SEED);
    for i in (1..handles.len()).rev() {
      handles.swap(i, rng.below(i + 1));
    }
    handles
  }

  #[test]
  fn freeing_in_random_order_refuses_every_stale_handle() {
    let mut slab: Slab<u64> = Slab::new(16, 256);
    let handles = shuffled_handles(&mut slab);
    assert_eq!(slab.len(), 256);
    assert!(matches!(
      slab.insert(0),
      Err(MemError::SlabFull { capacity: 256 })
    ));
    for h in &handles {
      let value = slab.remove(*h).unwrap();
      assert!(
        matches!(slab.get(*h), Err(MemError::StaleHandle { .. })),
        "freed {h:?} holding {value}"
      );
      assert!(matches!(slab.remove(*h), Err(MemError::StaleHandle { .. })));
    }
    assert!(slab.is_empty());
  }

  #[test]
  fn refilling_reuses_slots_and_never_repeats_a_handle() {
    let mut slab: Slab<u64> = Slab::new(16, 256);
    let handles = shuffled_handles(&mut slab);
    let mut seen: HashSet<Handle<u64>> = handles.iter().copied().collect();
    for h in &handles {
      slab.remove(*h).unwrap();
    }
    for i in 0..256u64 {
      let h = slab.insert(i + 1000).unwrap();
      assert!(seen.insert(h), "duplicate handle {h:?}");
      assert_eq!(*slab.get(h).unwrap(), i + 1000);
    }
    assert_eq!(slab.slots(), 256, "slots were reused, not grown");
    assert!(handles.iter().all(|h| !slab.contains(*h)));
  }

  #[test]
  fn insert_at_rebuilds_exact_slots_and_leaves_the_gaps_reusable() {
    // A slab rebuilt from a durable image (§4.8): live entries were at slots 1 and 3 (0 and 2 were
    // destroyed). Replaying them in ascending order into a fresh slab must land them at exactly
    // those handles — the identity a client still holds — and make the gaps reusable.
    let mut slab: Slab<u64> = Slab::new(16, 256);
    let h1 = slab.insert_at(1, 0, 111).unwrap();
    let h3 = slab.insert_at(3, 2, 333).unwrap();
    assert_eq!((h1.index(), h1.generation()), (1, 0));
    assert_eq!((h3.index(), h3.generation()), (3, 2));
    assert_eq!(*slab.get(h1).unwrap(), 111);
    assert_eq!(
      *slab.get(h3).unwrap(),
      333,
      "generation two was placed exactly"
    );
    assert_eq!(slab.len(), 2, "only the two live entries are occupied");
    // The gap slots 0 and 2 are on the free list, so the next inserts reuse them, not a fourth slot.
    let g_a = slab.insert(900).unwrap();
    let g_b = slab.insert(901).unwrap();
    let reused: HashSet<u32> = [g_a.index(), g_b.index()].into_iter().collect();
    assert_eq!(
      reused,
      HashSet::from([0, 2]),
      "the destroyed slots were reused"
    );
    assert_eq!(
      slab.slots(),
      4,
      "no slot beyond the rebuilt extent was grown"
    );
  }

  #[test]
  fn insert_at_refuses_a_colliding_or_out_of_bound_index() {
    let mut slab: Slab<u64> = Slab::new(16, 256);
    slab.insert_at(1, 0, 111).unwrap();
    slab.insert_at(3, 2, 333).unwrap();
    // A lower (out-of-order or duplicate) index — only a corrupt image — is refused, not overwritten.
    assert!(matches!(
      slab.insert_at(1, 5, 7),
      Err(MemError::OutOfRange { .. })
    ));
    // The bound still holds.
    assert!(matches!(
      slab.insert_at(256, 0, 7),
      Err(MemError::SlabFull { capacity: 256 })
    ));
  }

  #[test]
  fn get_mut_and_iter_see_live_entries_only() {
    let mut slab: Slab<String> = Slab::new(4, 8);
    let a = slab.insert("a".into()).unwrap();
    let b = slab.insert("b".into()).unwrap();
    slab.get_mut(a).unwrap().push('!');
    slab.remove(b).unwrap();
    let live: Vec<(Handle<String>, &String)> = slab.iter().collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].0, a);
    assert_eq!(live[0].1, "a!");
    assert_eq!(slab.max_slots(), 8);
  }

  #[test]
  fn a_reserved_segment_makes_inserts_allocation_free() {
    let mut slab: Slab<u8> = Slab::new(8, 64);
    slab.reserve_segments(1);
    for i in 0..8 {
      slab.insert(i).unwrap();
    }
    assert_eq!(slab.slots(), 8);
  }
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in
// tests) recognizes only a bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64, Ordering};

  use super::*;
  use crate::handle::Encoded;
  use crate::loom_bounds;
  use crate::mpsc::MpscRing;
  use crate::ring::SpscRing;

  /// Shape: the owning shard's id in the packed word; the peer routes the word back by it.
  const SHARD: u16 = 3;
  /// Shape: the rings' capacity, the smallest power of two; each ring carries one word.
  const RING_CAPACITY: usize = 2;
  /// Shape: the slab's segment and bound, one slot, so the reuse must land in the freed slot.
  const SLOTS: usize = 1;
  /// Format: the slot's first occupant and its replacement, distinct so a read tells them apart.
  const FIRST: u64 = 0x11;
  /// Format: the replacement.
  const SECOND: u64 = 0x22;

  /// Requests that found their handle live, across every explored interleaving.
  static LIVE_HITS: AtomicU64 = AtomicU64::new(0);
  /// Requests refused as stale, across every explored interleaving.
  static STALE_MISSES: AtomicU64 = AtomicU64::new(0);

  /// Applies a request that came back from the peer: the packed word names a handle; a live one
  /// reads the occupant it was issued for, a stale one is the typed refusal, and nothing else.
  fn apply(slab: &Slab<u64>, word: u64) {
    let encoded = Encoded::from_word(word);
    assert_eq!(encoded.shard(), SHARD, "routed back to its owner");
    let handle: Handle<u64> = Handle::from_raw(encoded.slot(), encoded.generation());
    match slab.get(handle) {
      Ok(value) => {
        assert_eq!(
          *value, FIRST,
          "a live handle names the occupant it was issued for"
        );
        LIVE_HITS.fetch_add(1, Ordering::Relaxed);
      }
      Err(MemError::StaleHandle { index, generation }) => {
        assert_eq!((index, generation), (handle.index(), handle.generation()));
        STALE_MISSES.fetch_add(1, Ordering::Relaxed);
      }
      Err(other) => panic!("a stale handle is refused as such, not {other:?}"),
    }
  }

  /// T-0.1 under loom (AC-0.7, the handle core): the owning shard issues a handle and publishes
  /// it to a peer thread over a ring; the peer's request naming the handle travels back over the
  /// shard's multi-producer ring while the shard frees the slot and reuses it. In every
  /// interleaving the request either finds the handle live and reads the first occupant, or is
  /// refused as stale with the handle's own index and generation; it never reads the new
  /// occupant. Both outcomes are reached in some interleaving.
  ///
  /// The handle is published before the peer starts and the shard yields once after starting
  /// it, so both orders are explored: the yield makes "the peer completes before the shard's
  /// first look" loom's initial schedule, and the other order comes from loom letting the
  /// shard's acquire load read the older sequence (nothing synchronizes the two threads before
  /// the join). Without both, loom never reached the live outcome (measured 2026-09-13: a peer
  /// spinning for the publication is re-run only when the shard yields, 19 interleavings; and
  /// loom's partial-order reduction backtracks only the shard's last look at the requests, the
  /// drain after the reuse, 5 interleavings). The return path is the concurrent one, and the
  /// one the doctrine is about.
  #[test]
  fn a_handle_returning_after_its_slot_was_reused_is_a_typed_miss_never_the_new_occupant() {
    loom_bounds::explore(
      "handles: a handle crossing threads against its slot's reuse",
      || {
        let to_peer: &'static SpscRing = Box::leak(Box::new(SpscRing::new(RING_CAPACITY).unwrap()));
        let from_peer: &'static MpscRing =
          Box::leak(Box::new(MpscRing::new(RING_CAPACITY).unwrap()));
        let (mut publish, receive) = to_peer.split();
        let mut slab: Slab<u64> = Slab::new(SLOTS, SLOTS);
        let first = slab.insert(FIRST).unwrap();
        publish.push(first.encode(SHARD).unwrap().word()).unwrap();
        let peer = loom::thread::spawn(move || {
          let mut receive = receive;
          let word = receive.pop().expect("published before the peer started");
          // The request back names the handle by the packed word it arrived as.
          let mut pending = word;
          while let Err(back) = from_peer.push(pending) {
            pending = back;
            loom::thread::yield_now();
          }
        });
        let mut requests = from_peer.consumer();
        let mut applied = 0;
        // Give the peer its first chance to run before the shard's first look (the doc above).
        loom::thread::yield_now();
        // First chance: the request may already be back.
        if let Some(word) = requests.pop() {
          apply(&slab, word);
          applied += 1;
        }
        // The slot is freed and reused under a new generation.
        assert_eq!(slab.remove(first).unwrap(), FIRST);
        let second = slab.insert(SECOND).unwrap();
        assert_eq!(second.index(), first.index(), "the freed slot was reused");
        assert_ne!(second.generation(), first.generation());
        // Second chance: the one request is applied exactly once.
        while applied < 1 {
          match requests.pop() {
            Some(word) => {
              apply(&slab, word);
              applied += 1;
            }
            None => loom::thread::yield_now(),
          }
        }
        peer.join().unwrap();
        assert_eq!(
          *slab.get(second).unwrap(),
          SECOND,
          "the new occupant is untouched"
        );
        assert!(matches!(slab.get(first), Err(MemError::StaleHandle { .. })));
      },
    );
    assert!(
      LIVE_HITS.load(Ordering::Relaxed) > 0,
      "some interleaving applied the request while the handle was live"
    );
    assert!(
      STALE_MISSES.load(Ordering::Relaxed) > 0,
      "some interleaving applied the request after the slot's reuse"
    );
  }
}
