//! Sealed blocks: runs of bytes one shard writes once and any shard of the process reads in place while their writer
//! keeps them (D-7, "immutable chunk reads from any core"; A-273 step 3). A green's merge engine keeps its content in
//! them (`slates-merge`'s history and current values), so a replica of the green on a shard that holds its works reads
//! the owner's bytes where they are instead of holding a copy (A-64: content once).
//!
//! **The writer** ([`SealedBlocks`]) owns every block's memory. [`SealedBlocks::seal`] moves a run of bytes into a new
//! block, which nothing writes again; [`SealedBlocks::release`] says the writer names it no more. A block no reader may
//! read is freed at its release. With readers, the release is stamped with the writer's feed position (the position of
//! the next message it sends them) and the block is kept, its slot unreused, until every reader has passed that
//! position ([`SealedBlocks::acknowledge`]).
//!
//! **A reader** ([`BlockReader`]) is registered by [`SealedBlocks::join`] and reads only the blocks in its own live
//! table: those live at its join, then, message by message, the blocks the message sealed, admitted before the reader
//! replays it ([`BlockReader::admit`]), less the blocks it released, taken out as the reader passes its position
//! ([`BlockReader::pass`]). A block not in that table (released and passed, never admitted, or of another writer) is
//! refused, counted, and never read.
//!
//! **Why a read is sound.** A block's bytes are written once, by [`SealedBlocks::seal`], before the writer names the block
//! in any value that leaves it ([`Joined`], [`Changes`]); such a value reaches a reader's thread only through whatever
//! moved it there (a channel, a spawn), which orders the writer's writes before the reader's reads. The writer frees a
//! block only once every reader registered before its release has passed the release's position, and a reader takes a
//! block out of its live table no later than it passes that position, in [`BlockReader::pass`], which takes `&mut self`:
//! no slice a read lent out of `&self` outlives it. So every block in a reader's live table is allocated and unchanged
//! for as long as a read of it lasts. The addresses a reader reads come only from its own live table, filled only from
//! [`Joined`] and [`Changes`], which only the writer makes; a [`Block`] is only a name, so a forged one is at worst the
//! name of another live block or of none.
//!
//! **Moving between threads.** Every value here is plain data. A block's address is an integer the writer exposed when it
//! sealed the block, and a reader turns it back into a pointer with `with_exposed_provenance` [B: std::ptr, "Exposed
//! Provenance"], so a writer, a reader and every value between them are `Send` with no `unsafe impl`, and they move
//! between shards as messages do (D-8). [`Joined`] is neither `Clone` nor `Copy`, so one registration is one reader; a
//! reader that is done gives back a [`Left`], since a reader the writer forgot while it still read would read a block
//! the writer freed.
//!
//! **What it costs.** A block is one allocation of its bytes, as a `Vec<u8>` chunk was; a reader's live table is one
//! entry a live block (its generation, address and length), the metadata A-273 counted a replica holding. No atomic is
//! touched by a read, a seal or a release: the writer's id, taken once when a store is made, is the only one (D-7).
//!
//! **Bounds (A-273 step 4).** Nothing here grows without a bound and a typed refusal at it:
//! - Readers: a join past the limit its writer's holder gives (one reader a shard that may read, the machine's) is
//!   refused ([`JoinRefused`]).
//! - The bytes kept for readers: past the limit the writer's holder gives ([`SealedBlocks::set_held_limit`], the room
//!   its memory class leaves), the slowest reader is evicted, one at a time: every later [`Changes`] names it, and it
//!   refuses each ([`ReaderRefusal::Evicted`]) until its holder makes it leave. It keeps its frees until then, since it
//!   may still read: the bound costs a reader, never a read of freed bytes.
//! - The blocks recorded for the next [`Changes`]: one message's worth. A writer that records a second message's
//!   changes before taking the first ([`SealedBlocks::evict_all`], called by its holder at the next change) evicts every
//!   reader, which could not be fed in order, and records nothing more until a reader joins again.
//! - A reader's live table: the writer's live blocks, its own bound.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

/// The next writer's id: process-unique, so a reader refuses values of another writer's store.
static NEXT_WRITER: AtomicU64 = AtomicU64::new(1);

/// A sealed block's name: its slot in its writer's store and which use of that slot it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Block {
  slot: usize,
  generation: u32,
}

impl Block {
  /// A name no writer gives: what a reader that could not adopt a block names in its place, refused by every read.
  pub const NONE: Block = Block {
    slot: usize::MAX,
    generation: u32::MAX,
  };
}

/// A live block's bytes: its generation, its address (exposed when it was sealed) and its length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Place {
  generation: u32,
  address: usize,
  len: usize,
}

/// One slot of a writer's store: the generation of its current or next block, and where that block's bytes are while it
/// is allocated.
#[derive(Debug)]
struct Slot {
  generation: u32,
  place: Option<(usize, usize)>,
}

/// A writer's store of sealed blocks (module docs).
#[derive(Debug)]
pub struct SealedBlocks {
  /// Its process-unique id.
  writer: u64,
  slots: Vec<Slot>,
  /// Slots whose blocks were freed, reused first.
  free: Vec<usize>,
  /// Each registered reader's id and the position it has passed: every release stamped below it.
  readers: BTreeMap<u64, u64>,
  next_reader: u64,
  /// The position a release is stamped with now: the position of the next [`Changes`] the writer takes.
  position: u64,
  /// Released blocks kept for readers, in release order, each with the position its release was stamped with.
  held: VecDeque<(u64, Block)>,
  /// The blocks sealed and released since the last [`Changes`], recorded while a reader is registered.
  sealed_since: Vec<(Block, Place)>,
  released_since: Vec<Block>,
  /// The bytes of the blocks the writer names, and of those kept only for readers.
  live_bytes: usize,
  held_bytes: usize,
  /// The frees kept for a reader since the store was made: the non-vacuity counter of the deferral.
  frees_held: u64,
  /// The bytes kept for readers past which the slowest is evicted (module docs, "Bounds"); unbounded until its holder
  /// sets it.
  held_limit: usize,
  /// Readers evicted and not yet left: each keeps its frees (it may read until it leaves) and refuses every message.
  evicted: std::collections::BTreeSet<u64>,
  /// Readers evicted since the store was made, for the bytes kept and for changes left unfed: the bounds' counters.
  evicted_lagging: u64,
  evicted_unfed: u64,
}

impl Default for SealedBlocks {
  fn default() -> SealedBlocks {
    SealedBlocks::new()
  }
}

/// A reader's registration (move-only): the writer, the reader's id, the position it starts at, and every block live then.
#[derive(Debug)]
pub struct Joined {
  writer: u64,
  reader: u64,
  position: u64,
  live: Vec<(Block, Place)>,
}

/// The blocks one message of the writer's feed sealed and released, at its position: what a reader admits before it
/// replays the message and passes once it has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Changes {
  writer: u64,
  position: u64,
  sealed: Vec<(Block, Place)>,
  released: Vec<Block>,
  /// The readers evicted and not yet left, each of which refuses it.
  evicted: Vec<u64>,
}

/// Why a writer refused a reader's join: it has `limit` readers already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinRefused {
  /// The readers it has.
  pub readers: usize,
  /// The most it takes.
  pub limit: usize,
}

/// A reader has passed every position below `position`: what it sends its writer, which frees what every reader passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Passed {
  writer: u64,
  reader: u64,
  position: u64,
}

/// A reader is done: the writer stops keeping blocks for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Left {
  writer: u64,
  reader: u64,
}

impl Changes {
  /// Its position in the writer's feed.
  pub fn position(&self) -> u64 {
    self.position
  }

  /// The blocks it sealed, in the order the writer sealed them: the order a reader replaying the same steps adopts them.
  pub fn sealed(&self) -> impl Iterator<Item = Block> + '_ {
    self.sealed.iter().map(|(block, _)| *block)
  }

  /// The blocks it released.
  pub fn released(&self) -> &[Block] {
    &self.released
  }
}

impl SealedBlocks {
  /// An empty store.
  pub fn new() -> SealedBlocks {
    SealedBlocks {
      writer: NEXT_WRITER.fetch_add(1, Ordering::Relaxed),
      slots: Vec::new(),
      free: Vec::new(),
      readers: BTreeMap::new(),
      next_reader: 0,
      position: 0,
      held: VecDeque::new(),
      sealed_since: Vec::new(),
      released_since: Vec::new(),
      live_bytes: 0,
      held_bytes: 0,
      frees_held: 0,
      held_limit: usize::MAX,
      evicted: std::collections::BTreeSet::new(),
      evicted_lagging: 0,
      evicted_unfed: 0,
    }
  }

  /// Moves `bytes` into a new block, which nothing writes again, and names it.
  pub fn seal(&mut self, bytes: Vec<u8>) -> Block {
    let len = bytes.len();
    let address = Box::into_raw(bytes.into_boxed_slice())
      .cast::<u8>()
      .expose_provenance();
    let slot = match self.free.pop() {
      Some(slot) => slot,
      None => {
        self.slots.push(Slot {
          generation: 0,
          place: None,
        });
        self.slots.len().saturating_sub(1)
      }
    };
    let generation = match self.slots.get_mut(slot) {
      Some(entry) => {
        entry.place = Some((address, len));
        entry.generation
      }
      None => 0,
    };
    let block = Block { slot, generation };
    self.live_bytes = self.live_bytes.saturating_add(len);
    if self.feeding() {
      self.sealed_since.push((
        block,
        Place {
          generation,
          address,
          len,
        },
      ));
    }
    block
  }

  /// The bytes of `block`, which this writer sealed and has not freed; `None` for any other name.
  pub fn bytes(&self, block: Block) -> Option<&[u8]> {
    let slot = self.slots.get(block.slot)?;
    if slot.generation != block.generation {
      return None;
    }
    let (address, len) = slot.place?;
    // SAFETY: the slot holds the block this writer sealed at `address` and has not freed: `free_slot` clears `place`
    // before it frees, and nothing writes a block after `seal`. The returned slice borrows `self`, so no free (which
    // takes `&mut self`) can happen while it lives.
    Some(unsafe { read_in_place(address, len) })
  }

  /// The writer names `block` no more: freed now when no reader may read it, else kept until every reader has passed
  /// the position its release is stamped with. A name it never sealed, or freed already, is ignored.
  pub fn release(&mut self, block: Block) {
    let Some(len) = self.place_of(block).map(|(_, len)| len) else {
      return;
    };
    self.live_bytes = self.live_bytes.saturating_sub(len);
    if self.readers.is_empty() {
      self.free_slot(block);
      return;
    }
    if self.feeding() {
      self.released_since.push(block);
    }
    self.held.push_back((self.position, block));
    self.held_bytes = self.held_bytes.saturating_add(len);
    self.frees_held = self.frees_held.saturating_add(1);
    if self.held_bytes > self.held_limit && self.evicted.is_empty() {
      self.evict_slowest();
    }
  }

  /// Whether a reader may still take the next [`Changes`]: one is registered and not evicted.
  fn feeding(&self) -> bool {
    self.readers.len() > self.evicted.len()
  }

  /// Evicts the reader that has passed the least, the one holding the most (module docs, "Bounds"); ties go to the
  /// earliest registered.
  fn evict_slowest(&mut self) {
    let slowest = self
      .readers
      .iter()
      .filter(|(reader, _)| !self.evicted.contains(reader))
      .min_by_key(|(reader, passed)| (**passed, **reader))
      .map(|(reader, _)| *reader);
    if let Some(reader) = slowest {
      self.evicted.insert(reader);
      self.evicted_lagging = self.evicted_lagging.saturating_add(1);
    }
  }

  /// The bytes kept for readers past which the slowest is evicted: the room the writer's memory class leaves for them,
  /// derived by its holder.
  pub fn set_held_limit(&mut self, bytes: usize) {
    self.held_limit = bytes;
    if self.held_bytes > self.held_limit && self.evicted.is_empty() {
      self.evict_slowest();
    }
  }

  /// Whether changes are recorded that no [`Changes`] has taken yet: the writer's holder takes them after every change
  /// it makes, so one found at the next change was left unfed.
  pub fn has_pending(&self) -> bool {
    !self.sealed_since.is_empty() || !self.released_since.is_empty()
  }

  /// Evicts every reader, which could not be fed in order once a change went unfed, and drops what was recorded for
  /// them (module docs, "Bounds"). Each keeps its frees until it leaves. Returns how many it evicted.
  pub fn evict_all(&mut self) -> usize {
    let before = self.evicted.len();
    self.evicted.extend(self.readers.keys().copied());
    let evicted = self.evicted.len().saturating_sub(before);
    self.evicted_unfed = self
      .evicted_unfed
      .saturating_add(u64::try_from(evicted).unwrap_or(u64::MAX));
    self.sealed_since.clear();
    self.released_since.clear();
    evicted
  }

  /// Readers evicted since the store was made: for the bytes kept for them, and for changes left unfed.
  pub fn evictions(&self) -> (u64, u64) {
    (self.evicted_lagging, self.evicted_unfed)
  }

  /// The blocks sealed and released since the last [`Changes`], at the writer's position, which then moves past it: what
  /// the writer's next feed message carries to every reader.
  pub fn take_changes(&mut self) -> Changes {
    let changes = Changes {
      writer: self.writer,
      position: self.position,
      sealed: std::mem::take(&mut self.sealed_since),
      released: std::mem::take(&mut self.released_since),
      evicted: self.evicted.iter().copied().collect(),
    };
    self.position = self.position.saturating_add(1);
    changes
  }

  /// The position the writer's next [`Changes`] carries.
  pub fn position(&self) -> u64 {
    self.position
  }

  /// Registers a reader now: it may read every block the writer names now, and has passed every position before the
  /// writer's ([`SealedBlocks::position`]). Refused when the writer has `limit` readers already, evicted ones that have
  /// not left among them (they still hold their frees).
  pub fn join(&mut self, limit: usize) -> Result<Joined, JoinRefused> {
    if self.readers.len() >= limit {
      return Err(JoinRefused {
        readers: self.readers.len(),
        limit,
      });
    }
    let reader = self.next_reader;
    self.next_reader = self.next_reader.saturating_add(1);
    self.readers.insert(reader, self.position);
    let held: std::collections::BTreeSet<Block> =
      self.held.iter().map(|(_, block)| *block).collect();
    let live = self
      .slots
      .iter()
      .enumerate()
      .filter_map(|(slot, entry)| {
        let (address, len) = entry.place?;
        let block = Block {
          slot,
          generation: entry.generation,
        };
        (!held.contains(&block)).then_some((
          block,
          Place {
            generation: entry.generation,
            address,
            len,
          },
        ))
      })
      .collect();
    Ok(Joined {
      writer: self.writer,
      reader,
      position: self.position,
      live,
    })
  }

  /// A reader passed every position below `passed`'s: blocks every reader has passed are freed. A reader never moves
  /// back, and another writer's or an unknown reader's is ignored.
  pub fn acknowledge(&mut self, passed: Passed) {
    if passed.writer != self.writer {
      return;
    }
    if let Some(position) = self.readers.get_mut(&passed.reader) {
      *position = (*position).max(passed.position);
    }
    self.free_passed();
  }

  /// A reader is done: the writer keeps nothing for it from now on.
  pub fn leave(&mut self, left: Left) {
    if left.writer != self.writer {
      return;
    }
    self.readers.remove(&left.reader);
    self.evicted.remove(&left.reader);
    self.free_passed();
    if self.readers.is_empty() {
      self.sealed_since.clear();
      self.released_since.clear();
    }
  }

  /// The bytes of the blocks the writer names.
  pub fn live_bytes(&self) -> usize {
    self.live_bytes
  }

  /// The bytes of the blocks kept only for readers: the writer's memory, charged with its own.
  pub fn held_bytes(&self) -> usize {
    self.held_bytes
  }

  /// The frees kept for a reader since the store was made: the non-vacuity counter of the deferral.
  pub fn frees_held(&self) -> u64 {
    self.frees_held
  }

  /// The readers registered now.
  pub fn readers(&self) -> usize {
    self.readers.len()
  }

  /// The bytes of the kept blocks **recounted** from the kept list itself: the oracle [`SealedBlocks::held_bytes`] is
  /// checked against.
  pub fn held_bytes_recounted(&self) -> usize {
    self
      .held
      .iter()
      .filter_map(|(_, block)| self.place_of(*block))
      .map(|(_, len)| len)
      .fold(0, usize::saturating_add)
  }

  /// Where `block` is while it is allocated.
  fn place_of(&self, block: Block) -> Option<(usize, usize)> {
    let slot = self.slots.get(block.slot)?;
    (slot.generation == block.generation)
      .then_some(slot.place)
      .flatten()
  }

  /// Frees each kept block every reader has passed, oldest first (releases are in position order).
  fn free_passed(&mut self) {
    let passed = self.readers.values().copied().min().unwrap_or(u64::MAX);
    while let Some(&(position, block)) = self.held.front() {
      if position >= passed {
        break;
      }
      self.held.pop_front();
      if let Some((_, len)) = self.place_of(block) {
        self.held_bytes = self.held_bytes.saturating_sub(len);
      }
      self.free_slot(block);
    }
  }

  /// Frees `block`'s memory and puts its slot up for reuse under the next generation.
  fn free_slot(&mut self, block: Block) {
    let Some(slot) = self.slots.get_mut(block.slot) else {
      return;
    };
    if slot.generation != block.generation {
      return;
    }
    let Some((address, len)) = slot.place.take() else {
      return;
    };
    slot.generation = slot.generation.wrapping_add(1);
    self.free.push(block.slot);
    free_in_place(address, len);
  }
}

impl Drop for SealedBlocks {
  /// Frees every block, unless a reader is still registered: then nothing is freed, since that reader may still read
  /// any block it holds live. The writer's readers leave before it is dropped (`slates-merge`'s replicas do); one that
  /// did not costs this store's memory, never a read of freed bytes.
  fn drop(&mut self) {
    if !self.readers.is_empty() {
      return;
    }
    for slot in &mut self.slots {
      if let Some((address, len)) = slot.place.take() {
        free_in_place(address, len);
      }
    }
  }
}

/// Why a reader refused a [`Changes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReaderRefusal {
  /// It is another writer's.
  OtherWriter,
  /// The writer evicted this reader (module docs, "Bounds"): it takes nothing more, and its holder makes it leave.
  Evicted,
  /// It is not the position the reader takes next: `expected`, sent `sent`.
  OutOfOrder {
    /// The position the reader admits or passes next.
    expected: u64,
    /// The position it was given.
    sent: u64,
  },
}

/// A reader of one writer's sealed blocks (module docs).
#[derive(Debug)]
pub struct BlockReader {
  writer: u64,
  reader: u64,
  /// The position it admits next.
  next: u64,
  /// The position it admitted and has not passed.
  admitted: Option<u64>,
  /// Its live table: by slot, the block it may read there.
  live: BTreeMap<usize, Place>,
  /// Reads it refused: a name not in its live table.
  refused: std::cell::Cell<u64>,
}

impl BlockReader {
  /// The reader `joined` registered.
  pub fn new(joined: Joined) -> BlockReader {
    BlockReader {
      writer: joined.writer,
      reader: joined.reader,
      next: joined.position,
      admitted: None,
      live: joined
        .live
        .into_iter()
        .map(|(block, place)| (block.slot, place))
        .collect(),
      refused: std::cell::Cell::new(0),
    }
  }

  /// The bytes of `block` where its writer keeps them, or `None` (counted) when it is not in this reader's live table.
  pub fn read(&self, block: Block) -> Option<&[u8]> {
    let Some(place) = self
      .live
      .get(&block.slot)
      .filter(|place| place.generation == block.generation)
    else {
      self.refused.set(self.refused.get().saturating_add(1));
      return None;
    };
    // SAFETY: `place` is in this reader's live table, filled only from the writer's own `Joined` and `Changes`, so it is
    // a block the writer sealed at that address and length. The writer frees a block only once every reader registered
    // before its release has passed the release's position, and this reader takes the block out of its live table no
    // later than it passes that position (`pass`, `&mut self`): the slice borrows `self`, so the block stays allocated
    // and unwritten while it lives. The value that carried the address reached this thread through whatever moved it
    // here, which orders the writer's writes of the bytes before this read.
    Some(unsafe { read_in_place(place.address, place.len) })
  }

  /// Admits the blocks `changes` sealed, so the reader may read them while it replays the message: refused unchanged
  /// when it is another writer's or not the position the reader takes next.
  pub fn admit(&mut self, changes: &Changes) -> Result<(), ReaderRefusal> {
    if changes.writer != self.writer {
      return Err(ReaderRefusal::OtherWriter);
    }
    if changes.evicted.contains(&self.reader) {
      return Err(ReaderRefusal::Evicted);
    }
    if self.admitted.is_some() || changes.position != self.next {
      return Err(ReaderRefusal::OutOfOrder {
        expected: self.next,
        sent: changes.position,
      });
    }
    for (block, place) in &changes.sealed {
      self.live.insert(block.slot, *place);
    }
    self.admitted = Some(changes.position);
    Ok(())
  }

  /// Takes the blocks `changes` released out of the live table and passes its position: what the writer frees by. Refused
  /// unchanged when it is not the message admitted last.
  pub fn pass(&mut self, changes: &Changes) -> Result<Passed, ReaderRefusal> {
    if changes.writer != self.writer {
      return Err(ReaderRefusal::OtherWriter);
    }
    if self.admitted != Some(changes.position) {
      return Err(ReaderRefusal::OutOfOrder {
        expected: self.next,
        sent: changes.position,
      });
    }
    for block in &changes.released {
      if self
        .live
        .get(&block.slot)
        .is_some_and(|place| place.generation == block.generation)
      {
        self.live.remove(&block.slot);
      }
    }
    self.admitted = None;
    self.next = changes.position.saturating_add(1);
    Ok(Passed {
      writer: self.writer,
      reader: self.reader,
      position: self.next,
    })
  }

  /// The position it admits next: it has passed every position below it.
  pub fn position(&self) -> u64 {
    self.next
  }

  /// Reads refused since it was made: a name not in its live table.
  pub fn refused(&self) -> u64 {
    self.refused.get()
  }

  /// The blocks it may read now.
  pub fn live_blocks(&self) -> usize {
    self.live.len()
  }

  /// The reader is done: what its writer takes to keep nothing for it.
  pub fn leave(self) -> Left {
    Left {
      writer: self.writer,
      reader: self.reader,
    }
  }
}

/// The `len` bytes at `address`, a block's exposed address.
///
/// # Safety
///
/// `address` and `len` are a block's place as its writer sealed it, and the block stays allocated and unwritten for as
/// long as the returned slice lives.
unsafe fn read_in_place<'a>(address: usize, len: usize) -> &'a [u8] {
  // SAFETY: the caller's contract: `address` is the exposed start of a live allocation of `len` bytes (a boxed slice the
  // writer sealed, non-null and aligned even when empty), initialized when it was sealed and unwritten since.
  unsafe { std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<u8>(address), len) }
}

/// Frees the `len` bytes at `address`, a block's exposed address, which nothing reads any more.
fn free_in_place(address: usize, len: usize) {
  let start = std::ptr::with_exposed_provenance_mut::<u8>(address);
  // SAFETY: `address` and `len` are a boxed slice `seal` gave up with `Box::into_raw` and exposed; the writer frees it once
  // (it takes the place out of its slot first), and only when no reader can read it (its release passed by every reader,
  // or no reader registered): nothing reads it after this.
  drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(start, len)) });
}

#[cfg(test)]
mod tests {
  //! The store's oracle: blocks read back as sealed, by the writer and its readers; a release frees at once with no
  //! reader and waits for every reader's pass with one; a reader reads only what its live table holds; a reader that
  //! leaves holds no free; and across threads a reader reads in place while its writer frees behind it only what it
  //! has passed.
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

  use super::*;

  /// Shape: the readers a test's writer takes, more than any plain test registers; the generated histories meet it.
  const READERS: usize = 8;

  /// A-273 step 3: a sealed block reads back as sealed, an empty one too; a release with no reader frees at once and its
  /// slot is reused under a new generation, so the old name reads nothing.
  #[test]
  fn a_block_reads_back_as_sealed_and_a_release_with_no_reader_frees_it() {
    let mut store = SealedBlocks::new();
    let a = store.seal(b"alpha".to_vec());
    let empty = store.seal(Vec::new());
    assert_eq!(store.bytes(a), Some(&b"alpha"[..]));
    assert_eq!(store.bytes(empty), Some(&b""[..]));
    assert_eq!(store.live_bytes(), 5);
    store.release(a);
    assert_eq!(store.bytes(a), None, "freed at once");
    assert_eq!((store.live_bytes(), store.held_bytes()), (0, 0));
    let b = store.seal(b"beta".to_vec());
    assert_eq!(b.slot, a.slot, "the slot is reused");
    assert_ne!(b.generation, a.generation);
    assert_eq!(store.bytes(a), None);
    assert_eq!(store.bytes(b), Some(&b"beta"[..]));
    assert_eq!(store.frees_held(), 0);
  }

  /// A-273 step 3: with a reader, a released block stays readable to it until it passes the message that released it,
  /// and is freed at its acknowledgement, not before; the reader reads the blocks a message sealed once admitted.
  #[test]
  fn a_released_block_stays_until_every_reader_passes_its_release() {
    let mut store = SealedBlocks::new();
    let kept = store.seal(b"kept".to_vec());
    let mut reader = BlockReader::new(store.join(READERS).unwrap());
    assert_eq!(reader.read(kept), Some(&b"kept"[..]));
    let fresh = store.seal(b"fresh".to_vec());
    store.release(kept);
    let changes = store.take_changes();
    assert_eq!(changes.sealed().collect::<Vec<_>>(), vec![fresh]);
    assert_eq!(changes.released(), &[kept]);
    assert_eq!(reader.read(fresh), None, "not yet admitted");
    reader.admit(&changes).unwrap();
    assert_eq!(reader.read(fresh), Some(&b"fresh"[..]));
    assert_eq!(reader.read(kept), Some(&b"kept"[..]), "still the reader's");
    assert_eq!((store.held_bytes(), store.frees_held()), (4, 1));
    assert_eq!(store.held_bytes(), store.held_bytes_recounted());
    let passed = reader.pass(&changes).unwrap();
    assert_eq!(reader.read(kept), None, "passed: refused, never read");
    assert_eq!(reader.refused(), 2);
    assert_eq!(store.held_bytes(), 4, "freed only at the acknowledgement");
    store.acknowledge(passed);
    assert_eq!(store.held_bytes(), 0);
    assert_eq!(store.held_bytes_recounted(), 0);
    assert_eq!(store.bytes(fresh), Some(&b"fresh"[..]));
  }

  /// A-273 step 3: changes out of order, twice, or of another writer are refused unchanged; a block of another writer
  /// is not read.
  #[test]
  fn changes_out_of_order_or_of_another_writer_are_refused() {
    let mut store = SealedBlocks::new();
    let mut other = SealedBlocks::new();
    let mut reader = BlockReader::new(store.join(READERS).unwrap());
    let first = store.take_changes();
    let second = store.take_changes();
    assert_eq!(
      reader.admit(&second),
      Err(ReaderRefusal::OutOfOrder {
        expected: 0,
        sent: 1
      })
    );
    assert_eq!(
      reader.admit(&other.take_changes()),
      Err(ReaderRefusal::OtherWriter)
    );
    reader.admit(&first).unwrap();
    assert!(reader.admit(&first).is_err(), "admitted twice");
    assert!(
      reader.pass(&second).is_err(),
      "passed before it was admitted"
    );
    reader.pass(&first).unwrap();
    let foreign = other.seal(b"x".to_vec());
    assert_eq!(reader.read(foreign).map(<[u8]>::len), None);
    assert_eq!(reader.read(Block::NONE), None);
  }

  /// A-273 step 3: a reader that leaves holds no free, and one that joins later never sees a block released before it
  /// joined, nor keeps it: a block released before a reader joined is kept for the readers before it alone.
  #[test]
  fn a_reader_that_leaves_holds_no_free_and_a_late_one_sees_only_what_is_live() {
    let mut store = SealedBlocks::new();
    let gone = store.seal(b"gone".to_vec());
    let stays = store.seal(b"stays".to_vec());
    let early = BlockReader::new(store.join(READERS).unwrap());
    store.release(gone);
    let _ = store.take_changes();
    assert_eq!(store.held_bytes(), 4);
    let late = BlockReader::new(store.join(READERS).unwrap());
    assert_eq!(
      late.read(gone),
      None,
      "released before the late reader joined"
    );
    assert_eq!(late.read(stays), Some(&b"stays"[..]));
    assert_eq!(late.live_blocks(), 1);
    store.leave(early.leave());
    assert_eq!(store.held_bytes(), 0, "kept for the early reader alone");
    assert_eq!(store.bytes(gone), None);
    store.leave(late.leave());
    assert_eq!(store.readers(), 0);
    assert_eq!(store.live_bytes(), 5);
  }

  /// A-273 step 3, across threads: a writer seals a version's blocks and releases the last's while a reader on another
  /// thread reads each message's blocks in place and passes it; the writer frees only what the reader passed, and every
  /// byte the reader read is the byte sealed. (Run under Miri by the memory crate's lane.)
  #[test]
  fn a_reader_on_another_thread_reads_in_place_what_its_writer_frees_behind_it() {
    use std::sync::mpsc::channel;
    /// Shape: the messages the writer sends, each sealing one block and releasing the one before.
    const MESSAGES: u8 = 32;
    let mut store = SealedBlocks::new();
    let joined = store.join(READERS).unwrap();
    let (to_reader, from_writer) = channel::<(Changes, Block, u8)>();
    let (to_writer, from_reader) = channel::<Passed>();
    let reader = std::thread::spawn(move || {
      let mut reader = BlockReader::new(joined);
      let mut previous: Option<(Block, u8)> = None;
      while let Ok((changes, block, byte)) = from_writer.recv() {
        reader.admit(&changes).unwrap();
        assert_eq!(reader.read(block), Some(&[byte; 64][..]));
        if let Some((last, last_byte)) = previous {
          assert_eq!(
            reader.read(last),
            Some(&[last_byte; 64][..]),
            "kept until passed"
          );
        }
        let passed = reader.pass(&changes).unwrap();
        to_writer.send(passed).unwrap();
        previous = Some((block, byte));
      }
      reader.leave()
    });
    let mut last: Option<Block> = None;
    for byte in 0..MESSAGES {
      let block = store.seal(vec![byte; 64]);
      if let Some(last) = last.take() {
        store.release(last);
      }
      to_reader.send((store.take_changes(), block, byte)).unwrap();
      last = Some(block);
      while let Ok(passed) = from_reader.try_recv() {
        store.acknowledge(passed);
      }
    }
    drop(to_reader);
    let left = reader.join().unwrap();
    while let Ok(passed) = from_reader.try_recv() {
      store.acknowledge(passed);
    }
    store.leave(left);
    assert_eq!(store.held_bytes(), 0);
    assert_eq!(store.frees_held(), u64::from(MESSAGES) - 1);
    assert_eq!(store.live_bytes(), 64);
  }

  /// A-273 step 4: a join past the writer's limit is refused, its readers named; once one leaves, a reader joins again.
  #[test]
  fn a_join_past_the_limit_is_refused() {
    let mut store = SealedBlocks::new();
    let first = BlockReader::new(store.join(1).unwrap());
    assert_eq!(
      store.join(1).err(),
      Some(JoinRefused {
        readers: 1,
        limit: 1
      })
    );
    store.leave(first.leave());
    let again = BlockReader::new(store.join(1).unwrap());
    store.leave(again.leave());
  }

  /// A-273 step 4: past the held limit the slowest reader is evicted, one at a time: every later message names it and it
  /// refuses it, while it still reads what it holds; at its leave everything kept for it alone is freed, and a reader
  /// joins again.
  #[test]
  fn the_slowest_reader_is_evicted_past_the_held_limit_and_keeps_its_frees_until_it_leaves() {
    let mut store = SealedBlocks::new();
    let (a, b, c) = (
      store.seal(vec![1; 8]),
      store.seal(vec![2; 8]),
      store.seal(vec![3; 8]),
    );
    let mut slow = BlockReader::new(store.join(READERS).unwrap());
    let mut fast = BlockReader::new(store.join(READERS).unwrap());
    store.set_held_limit(8);
    store.release(a);
    let first = store.take_changes();
    fast.admit(&first).unwrap();
    store.acknowledge(fast.pass(&first).unwrap());
    assert_eq!(store.evictions(), (0, 0), "at the limit, not past it");
    store.release(b);
    assert_eq!(store.evictions(), (1, 0), "past it: the slowest evicted");
    store.release(c);
    assert_eq!(store.evictions(), (1, 0), "one at a time, until it leaves");
    let second = store.take_changes();
    assert_eq!(slow.admit(&second), Err(ReaderRefusal::Evicted));
    assert_eq!(
      slow.read(a),
      Some(&[1u8; 8][..]),
      "it reads what it holds until it leaves"
    );
    fast.admit(&second).unwrap();
    store.acknowledge(fast.pass(&second).unwrap());
    assert_eq!(store.held_bytes(), 24);
    store.leave(slow.leave());
    assert_eq!(store.held_bytes(), 0, "kept for the evicted reader alone");
    assert_eq!(store.held_bytes(), store.held_bytes_recounted());
    let rebuilt = BlockReader::new(store.join(READERS).unwrap());
    assert_eq!(rebuilt.live_blocks(), 0, "it names only what is live");
    store.leave(rebuilt.leave());
    store.leave(fast.leave());
  }

  /// A-273 step 4: a change left unfed (the writer's holder made a second change before taking the first's) evicts every
  /// reader and drops what was recorded; nothing more is recorded until a reader joins again, which is fed.
  #[test]
  fn a_change_left_unfed_evicts_every_reader_and_records_nothing_more() {
    let mut store = SealedBlocks::new();
    let mut reader = BlockReader::new(store.join(READERS).unwrap());
    let first = store.seal(vec![3; 4]);
    assert!(store.has_pending());
    assert_eq!(store.evict_all(), 1);
    assert!(
      !store.has_pending(),
      "what was recorded for the evicted is dropped"
    );
    let later = store.seal(vec![4; 4]);
    assert!(
      !store.has_pending(),
      "nothing is recorded while no reader may take it"
    );
    assert_eq!(
      reader.admit(&store.take_changes()),
      Err(ReaderRefusal::Evicted)
    );
    assert_eq!(store.evictions(), (0, 1));
    store.leave(reader.leave());
    let mut again = BlockReader::new(store.join(READERS).unwrap());
    store.release(later);
    assert!(store.has_pending(), "fed again once a reader joined");
    let changes = store.take_changes();
    again.admit(&changes).unwrap();
    store.acknowledge(again.pass(&changes).unwrap());
    assert_eq!(again.read(first), Some(&[3u8; 4][..]));
    assert_eq!(again.read(later), None, "released and passed");
    store.leave(again.leave());
  }

  /// One reader of the generated histories: the reader, the blocks the model says it may read, and the messages sent it
  /// and not yet taken.
  struct Modelled {
    reader: BlockReader,
    live: std::collections::BTreeSet<Block>,
    inbox: VecDeque<Changes>,
  }

  proptest::proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::prelude::ProptestConfig::with_cases(slates_test_seeds::cases(256))))]

    /// A-273 step 3, the lifetime rule over generated histories. Do: generated steps of a writer sealing blocks,
    /// releasing them and taking each message's changes; readers joining, each admitting and passing its oldest
    /// message; acknowledgements delivered late and in any order; readers leaving. Expect, after every step: a reader
    /// reads exactly the blocks the model says are in its live table, each with its sealed bytes, and each still
    /// allocated by its writer (never freed before every reader passed its release); the writer reads every block it
    /// names; its kept bytes equal their recount; and at the end, every reader gone and every block released, nothing is
    /// live or kept.
    #[test]
    #[cfg_attr(miri, ignore)] // generated histories under Miri take minutes; the plain tests above run there
    fn no_reader_reads_a_block_its_writer_freed(
      steps in proptest::collection::vec(
        (0u8..7, proptest::prelude::any::<u8>(), proptest::collection::vec(proptest::prelude::any::<u8>(), 0..48)),
        1..96,
      ),
    ) {
      let mut store = SealedBlocks::new();
      let mut sealed: BTreeMap<Block, Vec<u8>> = BTreeMap::new();
      let mut named: Vec<Block> = Vec::new();
      let mut readers: Vec<Modelled> = Vec::new();
      let mut acks: Vec<Passed> = Vec::new();
      for (kind, pick, bytes) in steps {
        let pick = usize::from(pick);
        match kind {
          0 => {
            let block = store.seal(bytes.clone());
            sealed.insert(block, bytes);
            named.push(block);
          }
          1 if !named.is_empty() => store.release(named.remove(pick % named.len())),
          2 => {
            let changes = store.take_changes();
            for modelled in &mut readers {
              modelled.inbox.push_back(changes.clone());
            }
          }
          3 => match store.join(READERS) {
            Ok(joined) => readers.push(Modelled {
              reader: BlockReader::new(joined),
              live: named.iter().copied().collect(),
              inbox: VecDeque::new(),
            }),
            Err(refused) => proptest::prop_assert_eq!(refused.readers, READERS),
          },
          4 if !readers.is_empty() => {
            let at = pick % readers.len();
            let modelled = &mut readers[at];
            if let Some(changes) = modelled.inbox.pop_front() {
              modelled.reader.admit(&changes).unwrap();
              modelled.live.extend(changes.sealed());
              acks.push(modelled.reader.pass(&changes).unwrap());
              for block in changes.released() {
                modelled.live.remove(block);
              }
            }
          }
          5 if !acks.is_empty() => store.acknowledge(acks.remove(pick % acks.len())),
          6 if !readers.is_empty() => {
            let modelled = readers.remove(pick % readers.len());
            store.leave(modelled.reader.leave());
          }
          _ => {}
        }
        for modelled in &readers {
          for (block, bytes) in &sealed {
            if modelled.live.contains(block) {
              proptest::prop_assert_eq!(modelled.reader.read(*block), Some(bytes.as_slice()));
              proptest::prop_assert!(store.bytes(*block).is_some(), "freed under a reader");
            } else {
              proptest::prop_assert_eq!(modelled.reader.read(*block), None);
            }
          }
        }
        for block in &named {
          proptest::prop_assert_eq!(store.bytes(*block), sealed.get(block).map(Vec::as_slice));
        }
        proptest::prop_assert_eq!(store.held_bytes(), store.held_bytes_recounted());
      }
      for modelled in readers {
        store.leave(modelled.reader.leave());
      }
      for block in named {
        store.release(block);
      }
      proptest::prop_assert_eq!((store.live_bytes(), store.held_bytes()), (0, 0));
    }
  }
}
