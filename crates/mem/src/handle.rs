//! Generational handles: an index and a generation, `Copy`, typed, and meaningful only on the
//! shard that owns the slab (§4.2, ownership facts).
//!
//! A handle never dangles: the slab it names checks the generation on every access and refuses a
//! stale one with `StaleHandle`. When a handle crosses shards it is packed into an [`Encoded`]
//! word with the owning shard's id, in the same layout the runtime's waker uses
//! (`shard:16 | slot:24 | generation:24`, §4.3), so the receiver can route it back without a
//! lookup. The packed generation keeps its low 24 bits; a slot would have to be reused sixteen
//! million times between the encode and the check for a stale encoded handle to pass, which the
//! full 32-bit generation on the owning shard makes a counted, not silent, event.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

/// A typed generational handle into one shard's slab.
pub struct Handle<T> {
  index: u32,
  generation: u32,
  _t: PhantomData<fn() -> T>,
}

impl<T> Handle<T> {
  /// Builds a handle; slabs are the only callers.
  pub(crate) const fn new(index: u32, generation: u32) -> Self {
    Self {
      index,
      generation,
      _t: PhantomData,
    }
  }

  /// The slot index.
  pub const fn index(&self) -> u32 {
    self.index
  }

  /// The generation the handle was issued under.
  pub const fn generation(&self) -> u32 {
    self.generation
  }

  /// Packs the handle with its owning shard for transport across shards.
  pub const fn encode(&self, shard: u16) -> Option<Encoded> {
    Encoded::pack(shard, self.index, self.generation)
  }
}

impl<T> Clone for Handle<T> {
  fn clone(&self) -> Self {
    *self
  }
}
impl<T> Copy for Handle<T> {}

impl<T> PartialEq for Handle<T> {
  fn eq(&self, other: &Self) -> bool {
    self.index == other.index && self.generation == other.generation
  }
}
impl<T> Eq for Handle<T> {}

impl<T> Hash for Handle<T> {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.index.hash(state);
    self.generation.hash(state);
  }
}

impl<T> fmt::Debug for Handle<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "Handle({}@{})", self.index, self.generation)
  }
}

/// A handle packed with its shard: `shard:16 | slot:24 | generation:24`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Encoded(u64);

/// Format: the bit widths of the packed layout (§4.3, `RawWakerData`).
const SHARD_BITS: u32 = 16;
/// Format: slot bits.
const SLOT_BITS: u32 = 24;
/// Format: generation bits.
const GENERATION_BITS: u32 = 24;

impl Encoded {
  /// The largest slot index the packed form can carry.
  pub const MAX_SLOT: u32 = (1 << SLOT_BITS) - 1;

  /// Packs the three fields, refusing a slot index that does not fit.
  pub const fn pack(shard: u16, slot: u32, generation: u32) -> Option<Encoded> {
    if slot > Self::MAX_SLOT {
      return None;
    }
    let generation = generation & ((1 << GENERATION_BITS) - 1);
    Some(Encoded(
      ((shard as u64) << (SLOT_BITS + GENERATION_BITS))
        | ((slot as u64) << GENERATION_BITS)
        | generation as u64,
    ))
  }

  /// The raw word.
  pub const fn word(self) -> u64 {
    self.0
  }

  /// A packed handle from its raw word.
  pub const fn from_word(word: u64) -> Encoded {
    Encoded(word)
  }

  /// The owning shard.
  pub fn shard(self) -> u16 {
    // The shift leaves exactly SHARD_BITS bits, so the conversion cannot fail.
    u16::try_from(self.0 >> (SLOT_BITS + GENERATION_BITS)).unwrap_or(u16::MAX)
  }

  /// The slot index.
  pub fn slot(self) -> u32 {
    // Masked to SLOT_BITS bits, so the conversion cannot fail.
    u32::try_from((self.0 >> GENERATION_BITS) & ((1 << SLOT_BITS) - 1)).unwrap_or(u32::MAX)
  }

  /// The low 24 bits of the generation.
  pub fn generation(self) -> u32 {
    // Masked to GENERATION_BITS bits, so the conversion cannot fail.
    u32::try_from(self.0 & ((1 << GENERATION_BITS) - 1)).unwrap_or(u32::MAX)
  }

  /// Whether this packed handle names the same slot and generation as `handle`.
  pub fn matches<T>(self, handle: Handle<T>) -> bool {
    self.slot() == handle.index
      && self.generation() == (handle.generation & ((1 << GENERATION_BITS) - 1))
  }
}

impl fmt::Debug for Encoded {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(
      f,
      "Encoded(shard {} slot {} gen {})",
      self.shard(),
      self.slot(),
      self.generation()
    )
  }
}

#[allow(clippy::assertions_on_constants)]
const _: () = assert!(SHARD_BITS + SLOT_BITS + GENERATION_BITS == u64::BITS);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_handle_is_copy_typed_and_compares_by_index_and_generation() {
    let a: Handle<String> = Handle::new(7, 3);
    let b = a;
    assert_eq!(a, b);
    assert_ne!(a, Handle::new(7, 4));
    assert_eq!(format!("{a:?}"), "Handle(7@3)");
  }

  #[test]
  fn encoding_round_trips_and_refuses_an_oversized_slot() {
    let h: Handle<u8> = Handle::new(123_456, 0xABCD_EF01);
    let e = h.encode(42).unwrap();
    assert_eq!(e.shard(), 42);
    assert_eq!(e.slot(), 123_456);
    assert_eq!(e.generation(), 0xCD_EF01);
    assert!(e.matches(h));
    assert!(!e.matches(Handle::<u8>::new(123_456, 0xABCD_EF02)));
    assert_eq!(Encoded::from_word(e.word()), e);
    assert!(
      Handle::<u8>::new(Encoded::MAX_SLOT + 1, 0)
        .encode(0)
        .is_none()
    );
  }
}
