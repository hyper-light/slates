//! Content by identity (§4.17.1 "Objects" item 1; §4.11; D-6): every object's raw bytes, the bytes its tool hashes to
//! name it, are kept once, named by their BLAKE3 identity, however many objects, tools and repositories name them.
//!
//! **One copy.** [`ContentStore::put`] hashes the bytes and, when the store already holds that identity, counts one
//! more holder and stores nothing ([`ContentCounters::deduplicated`], the non-vacuity counter the tests read). So a
//! git blob, the same file's Mercurial file revision and a Subversion text are one content, and a fork's objects are
//! its parent's. The identity is always the store's own hash of the bytes it keeps: a caller cannot name content by an
//! identity it computed, so a stored content always holds the bytes its identity names.
//!
//! **Held by count.** Each holder (a tail row, a table, a quarantined object; see the crate's docs) takes one count and
//! gives it back once. The content is freed with its last count, and [`ContentStore::release`] hands the freed bytes
//! back, so a caller whose content names other content (a table naming its rows' objects) can release what it named.
//!
//! **Within a budget.** The store counts the bytes it keeps, and the bytes structures beside it charge to it (an
//! index's tail, [`ContentStore::charge`]), against a budget the caller derives from its shard's memory (§4.2) and may
//! derive again as pressure moves ([`ContentStore::set_budget`]). A put or a charge past it is refused,
//! [`ContentRefused::OverBudget`], with nothing changed; a lowered budget refuses new bytes until releases bring the
//! store under it, and never drops what is held.
//!
//! The map is ordered (`BTreeMap`) so that every walk of it is deterministic.

use std::collections::BTreeMap;

/// Format: a content identity's bytes, BLAKE3's output length.
pub const IDENTITY_BYTES: usize = blake3::OUT_LEN;

/// A content identity: BLAKE3 of the content's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId(pub [u8; IDENTITY_BYTES]);

impl ContentId {
  /// The identity of `bytes`.
  pub fn of(bytes: &[u8]) -> ContentId {
    ContentId(*blake3::hash(bytes).as_bytes())
  }

  /// The identity whose bytes are `bytes`, or `None` when `bytes` is not an identity's length.
  pub fn from_slice(bytes: &[u8]) -> Option<ContentId> {
    <[u8; IDENTITY_BYTES]>::try_from(bytes).ok().map(ContentId)
  }
}

/// Why the store refused a request. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentRefused {
  /// Keeping `need` more bytes would pass the budget, which has `room` left.
  OverBudget {
    /// The bytes the request needed.
    need: u64,
    /// The bytes the budget had left.
    room: u64,
  },
  /// The store holds no content of the identity named.
  NotHeld,
  /// A holder count, or a byte count, would pass `u64::MAX`.
  CountExhausted,
  /// A discharge named more bytes than were charged.
  NotCharged,
}

/// What the store has done, for the tests' non-vacuity checks and the owner's accounting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContentCounters {
  /// Contents stored new.
  pub stored: u64,
  /// Puts answered by a content already held: bytes not stored again.
  pub deduplicated: u64,
  /// Contents freed with their last holder.
  pub freed: u64,
}

/// What a put did: the content's identity, and whether its bytes were stored new or were held already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stored {
  /// The identity.
  pub identity: ContentId,
  /// Whether the bytes were stored new.
  pub fresh: bool,
}

/// One content: its bytes and its holders.
#[derive(Debug)]
struct Held {
  bytes: Box<[u8]>,
  holders: u64,
}

/// The content store (module docs).
#[derive(Debug)]
pub struct ContentStore {
  entries: BTreeMap<ContentId, Held>,
  budget: u64,
  /// The bytes of every content held.
  content_bytes: u64,
  /// The bytes structures beside the store charged to it.
  charged_bytes: u64,
  counters: ContentCounters,
}

/// A byte count as the store keeps it.
fn width(len: usize) -> Result<u64, ContentRefused> {
  u64::try_from(len).map_err(|_| ContentRefused::CountExhausted)
}

impl ContentStore {
  /// An empty store whose budget is `budget` bytes.
  pub fn new(budget: u64) -> ContentStore {
    ContentStore {
      entries: BTreeMap::new(),
      budget,
      content_bytes: 0,
      charged_bytes: 0,
      counters: ContentCounters::default(),
    }
  }

  /// Sets the budget the caller derived again. Nothing held is dropped; past it, new bytes are refused.
  pub fn set_budget(&mut self, budget: u64) {
    self.budget = budget;
  }

  /// The budget.
  pub fn budget(&self) -> u64 {
    self.budget
  }

  /// The bytes counted against the budget: every content's, and every charge's.
  pub fn used(&self) -> u64 {
    self.content_bytes.saturating_add(self.charged_bytes)
  }

  /// The bytes the budget has left.
  pub fn room(&self) -> u64 {
    self.budget.saturating_sub(self.used())
  }

  /// Refuses `need` more bytes unless the budget has room for them.
  fn admit(&self, need: u64) -> Result<(), ContentRefused> {
    let room = self.room();
    if need > room {
      return Err(ContentRefused::OverBudget { need, room });
    }
    Ok(())
  }

  /// Keeps `bytes`, or counts one more holder of the content already kept with their identity; the identity, held once
  /// more by the caller either way.
  pub fn put(&mut self, bytes: &[u8]) -> Result<ContentId, ContentRefused> {
    self.store(bytes).map(|stored| stored.identity)
  }

  /// [`ContentStore::put`], saying whether the bytes were stored new.
  pub fn store(&mut self, bytes: &[u8]) -> Result<Stored, ContentRefused> {
    self.store_in_place_of(bytes, 0)
  }

  /// [`ContentStore::store`] in place of `charged` bytes a structure beside the store charged and now gives back (an
  /// index's tail, written as a table): the charge is given back only when the bytes are kept, so a refusal changes
  /// nothing, and the room counts what the charge frees.
  pub fn store_in_place_of(
    &mut self,
    bytes: &[u8],
    charged: u64,
  ) -> Result<Stored, ContentRefused> {
    let left_charged = self
      .charged_bytes
      .checked_sub(charged)
      .ok_or(ContentRefused::NotCharged)?;
    let identity = ContentId::of(bytes);
    if self.entries.contains_key(&identity) {
      self.hold(&identity)?;
      self.charged_bytes = left_charged;
      self.counters.deduplicated = self.counters.deduplicated.saturating_add(1);
      return Ok(Stored {
        identity,
        fresh: false,
      });
    }
    let need = width(bytes.len())?;
    let room = self
      .budget
      .saturating_sub(self.used().saturating_sub(charged));
    if need > room {
      return Err(ContentRefused::OverBudget { need, room });
    }
    let content_bytes = self
      .content_bytes
      .checked_add(need)
      .ok_or(ContentRefused::CountExhausted)?;
    self.entries.insert(
      identity,
      Held {
        bytes: Box::from(bytes),
        holders: 1,
      },
    );
    self.content_bytes = content_bytes;
    self.charged_bytes = left_charged;
    self.counters.stored = self.counters.stored.saturating_add(1);
    Ok(Stored {
      identity,
      fresh: true,
    })
  }

  /// Counts one more holder of the content `identity` names.
  pub fn hold(&mut self, identity: &ContentId) -> Result<(), ContentRefused> {
    let held = self
      .entries
      .get_mut(identity)
      .ok_or(ContentRefused::NotHeld)?;
    held.holders = held
      .holders
      .checked_add(1)
      .ok_or(ContentRefused::CountExhausted)?;
    Ok(())
  }

  /// Gives back one holder's count of `identity`; the content's bytes when that was its last holder, which frees it.
  pub fn release(&mut self, identity: &ContentId) -> Result<Option<Box<[u8]>>, ContentRefused> {
    let held = self
      .entries
      .get_mut(identity)
      .ok_or(ContentRefused::NotHeld)?;
    if held.holders > 1 {
      held.holders = held.holders.saturating_sub(1);
      return Ok(None);
    }
    let Some(freed) = self.entries.remove(identity) else {
      return Err(ContentRefused::NotHeld);
    };
    let len = width(freed.bytes.len())?;
    self.content_bytes = self.content_bytes.saturating_sub(len);
    self.counters.freed = self.counters.freed.saturating_add(1);
    Ok(Some(freed.bytes))
  }

  /// The bytes of the content `identity` names.
  pub fn get(&self, identity: &ContentId) -> Option<&[u8]> {
    self.entries.get(identity).map(|held| &*held.bytes)
  }

  /// Whether the store holds the content `identity` names.
  pub fn contains(&self, identity: &ContentId) -> bool {
    self.entries.contains_key(identity)
  }

  /// The holders of the content `identity` names: zero when the store holds none.
  pub fn holders(&self, identity: &ContentId) -> u64 {
    self.entries.get(identity).map_or(0, |held| held.holders)
  }

  /// The contents held.
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Whether the store holds no content.
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// Every content's identity, in order.
  pub fn identities(&self) -> impl Iterator<Item = &ContentId> + '_ {
    self.entries.keys()
  }

  /// Counts `bytes` a structure beside the store keeps against the budget.
  pub fn charge(&mut self, bytes: u64) -> Result<(), ContentRefused> {
    self.admit(bytes)?;
    self.charged_bytes = self
      .charged_bytes
      .checked_add(bytes)
      .ok_or(ContentRefused::CountExhausted)?;
    Ok(())
  }

  /// Gives back `bytes` an earlier [`ContentStore::charge`] counted.
  pub fn discharge(&mut self, bytes: u64) -> Result<(), ContentRefused> {
    self.charged_bytes = self
      .charged_bytes
      .checked_sub(bytes)
      .ok_or(ContentRefused::NotCharged)?;
    Ok(())
  }

  /// The bytes charged now.
  pub fn charged(&self) -> u64 {
    self.charged_bytes
  }

  /// What the store has done.
  pub fn counters(&self) -> ContentCounters {
    self.counters
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-581, §4.17.1 "Objects" item 1. Do: put the same bytes three times and other bytes once. Expect: one content
  /// for the repeated bytes with three holders and its bytes counted once, two deduplicated puts counted, and the
  /// content freed only with its third release, its bytes handed back then.
  #[test]
  fn the_same_bytes_put_three_times_are_one_content_freed_with_its_last_holder() {
    let mut store = ContentStore::new(1 << 20);
    let first = store.put(b"fn main() {}\n").unwrap();
    let again = store.put(b"fn main() {}\n").unwrap();
    let third = store.put(b"fn main() {}\n").unwrap();
    let other = store.put(b"other").unwrap();
    assert_eq!((first, first), (again, third));
    assert_ne!(first, other);
    assert_eq!(store.len(), 2);
    assert_eq!(store.holders(&first), 3);
    assert_eq!(store.used(), 13 + 5);
    assert_eq!(store.counters().deduplicated, 2);
    assert_eq!(store.counters().stored, 2);
    assert_eq!(store.release(&first).unwrap(), None);
    assert_eq!(store.release(&first).unwrap(), None);
    assert_eq!(store.get(&first), Some(&b"fn main() {}\n"[..]));
    assert_eq!(
      store.release(&first).unwrap().as_deref(),
      Some(&b"fn main() {}\n"[..])
    );
    assert!(!store.contains(&first));
    assert_eq!(store.used(), 5);
    assert_eq!(store.release(&first), Err(ContentRefused::NotHeld));
  }

  /// A-581. Do: fill a budget, then put past it, charge past it, and lower it under what is held. Expect: each refusal
  /// names the need and the room and changes nothing; a repeated put still counts a holder (it needs no bytes); a
  /// lowered budget drops nothing and refuses new bytes until a release makes room.
  #[test]
  fn a_put_or_a_charge_past_the_budget_is_refused_and_changes_nothing() {
    let mut store = ContentStore::new(10);
    let eight = store.put(b"12345678").unwrap();
    assert_eq!(
      store.put(b"abc"),
      Err(ContentRefused::OverBudget { need: 3, room: 2 })
    );
    assert_eq!(
      store.charge(3),
      Err(ContentRefused::OverBudget { need: 3, room: 2 })
    );
    assert_eq!((store.len(), store.used()), (1, 8));
    assert_eq!(store.put(b"12345678"), Ok(eight));
    assert_eq!(store.holders(&eight), 2);
    store.charge(2).unwrap();
    assert_eq!(store.room(), 0);
    store.set_budget(4);
    assert_eq!(store.get(&eight), Some(&b"12345678"[..]));
    assert_eq!(
      store.put(b"x"),
      Err(ContentRefused::OverBudget { need: 1, room: 0 })
    );
    store.discharge(2).unwrap();
    assert_eq!(store.discharge(1), Err(ContentRefused::NotCharged));
    store.release(&eight).unwrap();
    store.release(&eight).unwrap();
    assert_eq!(store.used(), 0);
    assert!(store.put(b"x").is_ok());
  }
}
