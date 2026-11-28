//! The runs a radix tree's nodes keep their labels and child lists in (A-920 unit 0, main's ruling of 2026-11-27 23:08):
//! a store the arena owns, so a node's label and child list are no allocations of their own and a release gives their
//! runs back to the store, never to the allocator.
//!
//! **Why.** A wide import's retired scratch member released 9,002 labels and 903 child lists to the heap, and the
//! remains it left freed about 110,000 more blocks; their frees emptied the allocator's regions while a shard allocated,
//! and the allocator's return of about 50 MB of pages stalled that shard off its CPU for 1.7-4.6 ms (debug; GAPS,
//! A-920). Objects live in arenas (CLAUDE.md §3); a node's bytes are the arena's.
//!
//! **What.** Items of one kind (`u8` for labels, `(u8, slot)` for child lists) in chunks that are never reallocated, so
//! no growth copies a store and no free of a grown buffer returns pages mid-step. A run is a place in one chunk, its
//! capacity a power of two (its class) and its length up to that. A freed run goes on its class's free list and the
//! next run of that class takes it before the store bumps into its current chunk, and only then adds a chunk.
//!
//! **Its bound** (segregated fit, Wilson, Johnstone, Neely and Boles, "Dynamic Storage Allocation: A Survey and Critical
//! Review", IWMM 1995, tier A). A run wastes less than half its class. A class's free runs never outnumber that class's
//! peak live runs, since each was live once. A chunk left part-used when the store bumps into a new one wastes at most
//! one run's room of each class it passes. So the store holds under 2 × Σ over classes of the peak live items, plus
//! the classes times a chunk. A run wider than a chunk takes a chunk of its own class's size, kept for its class.
//!
//! **What it reports.** Runs reused from a free list, runs bumped into a chunk, chunks added, and the store's bytes
//! exactly (its chunks' items), so the reuse path cannot be silently dead and the bytes can be charged.
//!
//! **No unsafe, no panics.** Every chunk and item is reached with `get`; a run naming no chunk reads as empty.

/// A place in a [`Store`]: its chunk, its first item, its length, and its class (its capacity, `1 << class`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Run {
  chunk: u32,
  at: u32,
  len: u32,
  class: u8,
}

impl Run {
  /// The empty run, holding no room.
  pub(super) const EMPTY: Run = Run {
    chunk: 0,
    at: 0,
    len: 0,
    class: 0,
  };

  /// Its length.
  pub(super) fn len(self) -> usize {
    usize::try_from(self.len).unwrap_or(usize::MAX)
  }

  /// Whether it holds no item.
  pub(super) fn is_empty(self) -> bool {
    self.len == 0
  }

  /// Its capacity: its class's.
  fn capacity(self) -> usize {
    if self.len == 0 && self.class == 0 {
      return 0;
    }
    1usize
      .checked_shl(u32::from(self.class))
      .unwrap_or(usize::MAX)
  }
}

/// The class a run of `len` items takes: the least power of two at least `len`.
fn class_of(len: usize) -> u8 {
  let capacity = len.max(1).checked_next_power_of_two().unwrap_or(usize::MAX);
  u8::try_from(capacity.trailing_zeros()).unwrap_or(u8::MAX)
}

/// The items of one kind a radix tree's nodes keep in runs (module docs).
#[derive(Debug)]
pub(super) struct Store<T> {
  /// The chunks, each its full length from its making on, never reallocated.
  chunks: Vec<Vec<T>>,
  /// The items a chunk holds (module docs, "Its bound").
  chunk_items: usize,
  /// Each class's freed runs, by chunk and first item.
  free: Vec<Vec<(u32, u32)>>,
  /// The chunk bumped into now and its next free item.
  bump: Option<(u32, u32)>,
  /// Runs taken from a free list.
  reused: u64,
  /// Runs bumped into a chunk.
  bumped: u64,
  /// Chunks added.
  added: u64,
}

impl<T: Copy + Default> Store<T> {
  /// An empty store whose chunks hold `chunk_items` items each.
  pub(super) fn new(chunk_items: usize) -> Store<T> {
    Store {
      chunks: Vec::new(),
      chunk_items: chunk_items.max(1),
      free: Vec::new(),
      bump: None,
      reused: 0,
      bumped: 0,
      added: 0,
    }
  }

  /// Runs taken from a free list, runs bumped into a chunk, and chunks added.
  pub(super) fn counts(&self) -> (u64, u64, u64) {
    (self.reused, self.bumped, self.added)
  }

  /// The bytes the store's chunks hold, exactly.
  pub(super) fn bytes(&self) -> usize {
    self
      .chunks
      .iter()
      .map(Vec::len)
      .fold(0usize, usize::saturating_add)
      .saturating_mul(size_of::<T>())
  }

  /// The items of `run`.
  pub(super) fn get(&self, run: Run) -> &[T] {
    if run.is_empty() {
      return &[];
    }
    let at = usize::try_from(run.at).unwrap_or(usize::MAX);
    self
      .chunks
      .get(usize::try_from(run.chunk).unwrap_or(usize::MAX))
      .and_then(|chunk| chunk.get(at..at.saturating_add(run.len())))
      .unwrap_or_default()
  }

  /// The items of `run`, to change in place.
  pub(super) fn get_mut(&mut self, run: Run) -> &mut [T] {
    if run.is_empty() {
      return &mut [];
    }
    let at = usize::try_from(run.at).unwrap_or(usize::MAX);
    self
      .chunks
      .get_mut(usize::try_from(run.chunk).unwrap_or(usize::MAX))
      .and_then(|chunk| chunk.get_mut(at..at.saturating_add(run.len())))
      .unwrap_or_default()
  }

  /// Room for a run of class `class`: a freed one of the class, else the current chunk's next, else a new chunk's
  /// (a chunk of its own for a class wider than a chunk). Its length is zero.
  fn room(&mut self, class: u8) -> Run {
    let capacity = 1usize.checked_shl(u32::from(class)).unwrap_or(usize::MAX);
    if let Some((chunk, at)) = self.free.get_mut(usize::from(class)).and_then(Vec::pop) {
      self.reused = self.reused.saturating_add(1);
      return Run {
        chunk,
        at,
        len: 0,
        class,
      };
    }
    self.bumped = self.bumped.saturating_add(1);
    if capacity > self.chunk_items {
      return self.chunk_of(capacity, class);
    }
    if let Some((chunk, next)) = self.bump {
      let next_end = usize::try_from(next)
        .unwrap_or(usize::MAX)
        .saturating_add(capacity);
      if next_end <= self.chunk_items {
        self.bump = Some((chunk, u32::try_from(next_end).unwrap_or(u32::MAX)));
        return Run {
          chunk,
          at: next,
          len: 0,
          class,
        };
      }
    }
    let run = self.chunk_of(self.chunk_items, class);
    self.bump = Some((run.chunk, u32::try_from(capacity).unwrap_or(u32::MAX)));
    run
  }

  /// A new chunk of `items` items, and a run of class `class` at its start.
  fn chunk_of(&mut self, items: usize, class: u8) -> Run {
    self.chunks.push(vec![T::default(); items]);
    self.added = self.added.saturating_add(1);
    Run {
      chunk: u32::try_from(self.chunks.len().saturating_sub(1)).unwrap_or(u32::MAX),
      at: 0,
      len: 0,
      class,
    }
  }

  /// A new run holding `items`.
  pub(super) fn alloc(&mut self, items: &[T]) -> Run {
    if items.is_empty() {
      return Run::EMPTY;
    }
    let mut run = self.room(class_of(items.len()));
    run.len = u32::try_from(items.len()).unwrap_or(u32::MAX);
    if let Some(place) = self.place_mut(run) {
      place.copy_from_slice(items);
    }
    run
  }

  /// The items `run` names, to write, at its length.
  fn place_mut(&mut self, run: Run) -> Option<&mut [T]> {
    let at = usize::try_from(run.at).ok()?;
    self
      .chunks
      .get_mut(usize::try_from(run.chunk).ok()?)?
      .get_mut(at..at.checked_add(run.len())?)
  }

  /// Gives `run`'s room back to its class's free list.
  pub(super) fn free(&mut self, run: Run) {
    if run.capacity() == 0 {
      return;
    }
    let class = usize::from(run.class);
    if self.free.len() <= class {
      self.free.resize_with(class.saturating_add(1), Vec::new);
    }
    if let Some(list) = self.free.get_mut(class) {
      list.push((run.chunk, run.at));
    }
  }

  /// A new run holding `run`'s first `len` items, `run` left as it was.
  pub(super) fn prefix(&mut self, run: Run, len: usize) -> Run {
    let len = len.min(run.len());
    if len == 0 {
      return Run::EMPTY;
    }
    let mut copy = self.room(class_of(len));
    copy.len = u32::try_from(len).unwrap_or(u32::MAX);
    self.copy_items(run, 0, copy, 0, len);
    copy
  }

  /// A new run holding `run`'s items from `skip` on, `run` left as it was.
  pub(super) fn duplicate(&mut self, run: Run, skip: usize) -> Run {
    let len = run.len().saturating_sub(skip);
    if len == 0 {
      return Run::EMPTY;
    }
    let mut copy = self.room(class_of(len));
    copy.len = u32::try_from(len).unwrap_or(u32::MAX);
    self.copy_items(run, skip, copy, 0, len);
    copy
  }

  /// `len` items from `from`'s item `from_at` written at `to`'s item `to_at`: within one chunk or between two.
  fn copy_items(&mut self, from: Run, from_at: usize, to: Run, to_at: usize, len: usize) {
    let source = usize::try_from(from.at)
      .unwrap_or(usize::MAX)
      .saturating_add(from_at);
    let target = usize::try_from(to.at)
      .unwrap_or(usize::MAX)
      .saturating_add(to_at);
    let (from_chunk, to_chunk) = (
      usize::try_from(from.chunk).unwrap_or(usize::MAX),
      usize::try_from(to.chunk).unwrap_or(usize::MAX),
    );
    if from_chunk == to_chunk {
      if let Some(chunk) = self.chunks.get_mut(from_chunk)
        && source.saturating_add(len) <= chunk.len()
        && target.saturating_add(len) <= chunk.len()
      {
        chunk.copy_within(source..source.saturating_add(len), target);
      }
      return;
    }
    let (low, high) = (from_chunk.min(to_chunk), from_chunk.max(to_chunk));
    let Some((before, after)) = self.chunks.split_at_mut_checked(high) else {
      return;
    };
    let (Some(low_chunk), Some(high_chunk)) = (before.get_mut(low), after.first_mut()) else {
      return;
    };
    let (source_chunk, target_chunk) = if from_chunk < to_chunk {
      (&*low_chunk, high_chunk)
    } else {
      (&*high_chunk, low_chunk)
    };
    if let (Some(items), Some(place)) = (
      source_chunk.get(source..source.saturating_add(len)),
      target_chunk.get_mut(target..target.saturating_add(len)),
    ) {
      place.copy_from_slice(items);
    }
  }

  /// A new run holding `first`'s items then `second`'s; both runs freed.
  pub(super) fn joined(&mut self, first: Run, second: Run) -> Run {
    let (a, b) = (first.len(), second.len());
    let len = a.saturating_add(b);
    if len == 0 {
      self.free(first);
      self.free(second);
      return Run::EMPTY;
    }
    let mut joined = self.room(class_of(len));
    joined.len = u32::try_from(len).unwrap_or(u32::MAX);
    self.copy_items(first, 0, joined, 0, a);
    self.copy_items(second, 0, joined, a, b);
    self.free(first);
    self.free(second);
    joined
  }

  /// A new run holding `run`'s items then `items`; `run` freed.
  pub(super) fn appended(&mut self, run: Run, items: &[T]) -> Run {
    let (a, b) = (run.len(), items.len());
    let len = a.saturating_add(b);
    if len == 0 {
      self.free(run);
      return Run::EMPTY;
    }
    let mut grown = self.room(class_of(len));
    grown.len = u32::try_from(len).unwrap_or(u32::MAX);
    self.copy_items(run, 0, grown, 0, a);
    if let Some(place) = self.get_mut(grown).get_mut(a..len) {
      place.copy_from_slice(items);
    }
    self.free(run);
    grown
  }

  /// A new run holding `items` then `run`'s; `run` freed.
  pub(super) fn prepended(&mut self, items: &[T], run: Run) -> Run {
    let head = self.alloc(items);
    self.joined(head, run)
  }

  /// `run` without its first `skip` items: a new run, `run` freed.
  #[cfg(test)]
  pub(super) fn suffix(&mut self, run: Run, skip: usize) -> Run {
    let tail = self.duplicate(run, skip);
    self.free(run);
    tail
  }

  /// `run` with `item` put at `at`, the items after it moved up one: in place while its class has room, else a run
  /// of the next class, `run` freed.
  pub(super) fn inserted(&mut self, run: Run, at: usize, item: T) -> Run {
    let len = run.len();
    let at = at.min(len);
    let grown = if len < run.capacity() {
      Run {
        len: run.len.saturating_add(1),
        ..run
      }
    } else {
      let mut grown = self.room(class_of(len.saturating_add(1)));
      grown.len = u32::try_from(len.saturating_add(1)).unwrap_or(u32::MAX);
      self.copy_items(run, 0, grown, 0, at);
      self.copy_items(run, at, grown, at.saturating_add(1), len.saturating_sub(at));
      self.free(run);
      if let Some(slot) = self.get_mut(grown).get_mut(at) {
        *slot = item;
      }
      return grown;
    };
    let place = self.get_mut(grown);
    if at < place.len() {
      place.copy_within(at..len, at.saturating_add(1));
    }
    if let Some(slot) = place.get_mut(at) {
      *slot = item;
    }
    grown
  }

  /// `run` without its item at `at`, the items after it moved down one, in place.
  pub(super) fn removed(&mut self, run: Run, at: usize) -> Run {
    let len = run.len();
    if at >= len {
      return run;
    }
    let place = self.get_mut(run);
    place.copy_within(at.saturating_add(1)..len, at);
    let shorter = Run {
      len: run.len.saturating_sub(1),
      ..run
    };
    if shorter.is_empty() {
      self.free(run);
      return Run::EMPTY;
    }
    shorter
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;

  use super::{Run, Store};

  /// What a store's run model holds: each live run's items, by a key of the test's.
  type Model = BTreeMap<u32, Vec<u8>>;

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(300), ..ProptestConfig::default() }))]

    /// A-920 unit 0. Do: run generated histories of allocations, inserts, removals, suffixes, joins, duplicates and
    /// frees over a store with small chunks, each live run held against a vector model. Expect every live run to read
    /// its model's items after every step, the store's bytes never past the segregated-fit bound over its peak live
    /// items (module docs, "Its bound"), and runs reused whenever a run of a freed class is asked again.
    #[test]
    fn every_run_reads_as_its_model_and_the_store_stays_within_its_bound(
      steps in proptest::collection::vec((0u8..7, 0u32..12, proptest::collection::vec(any::<u8>(), 0..40), any::<u8>()), 0..300),
    ) {
      let mut store: Store<u8> = Store::new(64);
      let mut runs: BTreeMap<u32, Run> = BTreeMap::new();
      let mut model = Model::new();
      let mut peak_live = 0usize;
      for (kind, key, items, item) in steps {
        let run = runs.get(&key).copied();
        let next = match (kind, run) {
          (0, None) => Some((store.alloc(&items), items.clone())),
          (1, Some(run)) => {
            let at = usize::from(item) % (run.len() + 1);
            let mut held = model[&key].clone();
            held.insert(at, item);
            Some((store.inserted(run, at, item), held))
          }
          (2, Some(run)) if !run.is_empty() => {
            let at = usize::from(item) % run.len();
            let mut held = model[&key].clone();
            held.remove(at);
            Some((store.removed(run, at), held))
          }
          (3, Some(run)) => {
            let skip = usize::from(item) % (run.len() + 1);
            let held = model[&key][skip..].to_vec();
            Some((store.suffix(run, skip), held))
          }
          (4, Some(run)) => {
            let mut held = items.clone();
            held.extend_from_slice(&model[&key]);
            Some((store.prepended(&items, run), held))
          }
          (5, Some(run)) => {
            let copy = store.duplicate(run, 0);
            let held = model[&key].clone();
            store.free(run);
            Some((copy, held))
          }
          (6, Some(run)) => {
            store.free(run);
            runs.remove(&key);
            model.remove(&key);
            None
          }
          _ => None,
        };
        if let Some((run, held)) = next {
          runs.insert(key, run);
          model.insert(key, held);
        }
        for (key, run) in &runs {
          prop_assert_eq!(store.get(*run), model[key].as_slice());
        }
        let live: usize = runs.values().map(|run| run.len()).sum();
        peak_live = peak_live.max(live);
        let classes = 8usize;
        prop_assert!(
          store.bytes() <= 2 * classes * peak_live.max(1) + classes * 64 + 64,
          "{} bytes over {} peak live items", store.bytes(), peak_live
        );
      }
    }
  }

  /// A-920 unit 0's reuse. Do: allocate a run, free it, and allocate one of its class again. Expect the second taken
  /// from the free list (one reused), at the first's place, and no new chunk.
  #[test]
  fn a_freed_run_is_taken_again_by_the_next_of_its_class() {
    let mut store: Store<u8> = Store::new(64);
    let first = store.alloc(b"abcde");
    store.free(first);
    let second = store.alloc(b"fghij");
    assert_eq!(
      store.counts(),
      (1, 1, 1),
      "one reused, one bumped, one chunk"
    );
    assert_eq!(store.get(second), b"fghij");
  }
}
