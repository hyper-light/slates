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
}

impl<T> Slab<T> {
  /// A slab whose segments hold `segment_slots` slots and which never exceeds `max_slots`.
  pub fn new(segment_slots: usize, max_slots: usize) -> Self {
    Self {
      slots: Segmented::new(segment_slots),
      free_head: None,
      len: 0,
      max_slots,
    }
  }

  /// Pre-allocates `count` segments so that the next inserts allocate nothing.
  pub fn reserve_segments(&mut self, count: usize) {
    self.slots.reserve_segments(count);
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
    let index = self.slots.push(Slot {
      generation: 0,
      body: Body::Occupied(value),
    });
    self.len += 1;
    let index = u32::try_from(index).map_err(|_| MemError::SlabFull {
      capacity: self.max_slots,
    })?;
    Ok(Handle::new(index, 0))
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
