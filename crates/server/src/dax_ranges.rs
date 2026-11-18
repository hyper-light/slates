//! A shard's DAX range table in anchor-owned RAM (§4.6, the Shards form; A-365; Windows' sections, A-448): each range a
//! Shards stream's held DAX backing has answered, with whether it was asked writable, so a successor daemon knows what of
//! each object the anchor held across the death is the file's and what the guest may have stored into.
//!
//! **Why a successor needs it.** A backing is filled a range at a time, as a guest asks (`slates_bridge_core::
//! dax_backing`). A successor that adopts the object must ingest the ranges asked writable, where the guest's stores
//! since its last sync are, and must never fill again a range the guest maps, since a fill racing the guest's live
//! stores would undo them. Neither can be told from the object: a range never filled reads zeros, which a comparison
//! against the file would take for the guest's stores and write over the file, and macOS's shared-memory objects say
//! nothing of which pages were ever touched. So the daemon records each range here, a slot a range, before the ask that
//! filled it is answered: never at the fill's start, since a daemon that dies mid-fill must not leave a half-filled
//! range named filled.
//!
//! **Bounded by derivation.** The table has a slot for each range the shard's fill budget can count
//! (`crate::config::DaemonConfig::dax_range_slots`): every range a backing has filled counts a range against the
//! budget, so the ranges filled never outnumber the slots, and a slot is always free for the next.
//!
//! **Written whole before it is named.** A slot's attachment, inode and range are stored before its flags, so a daemon
//! that dies mid-write leaves a slot no reader takes as used. Forgetting a backing clears its slots' flags. The table
//! sits in the shard's slice of the anchor's content object, at `crate::config::ShardContentLayout::dax_ranges`, and a
//! daemon reads only a table of its own layout: one of another (an upgrade's, or bytes another layout left there) is
//! cleared whole and started again, never misread.
//!
//! Layout (little-endian `u64` words): the layout word ([`LAYOUT`]) and the slot count, then the slots, each the
//! attachment, the inode, the range's index and its flags ([`USED`], [`WRITABLE`]).

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::SparseObject;

use crate::config::{DAX_RANGE_HEADER_BYTES, DAX_RANGE_SLOT_BYTES};

/// Format: the layout this daemon writes, "DAXR" and version 1.
const LAYOUT: u64 = u64::from_le_bytes(*b"DAXR\x01\x00\x00\x00");
/// Format: where the slot count sits, past the layout word.
const AT_SLOTS: usize = size_of::<u64>();
/// Format: where a slot's inode sits, past its attachment.
const AT_INODE: usize = size_of::<u64>();
/// Format: where a slot's range sits.
const AT_RANGE: usize = 2 * size_of::<u64>();
/// Format: where a slot's flags sit, last, so they are written after what they name.
const AT_FLAGS: usize = 3 * size_of::<u64>();
/// Format: the flag of a slot in use.
const USED: u64 = 1;
/// Format: the flag of a range asked writable.
const WRITABLE: u64 = 1 << 1;

/// The content object refused a span of the table: the caller treats the range as unrecorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unrecorded {
  /// Every slot is in use: never expected, since the slots are derived from the fill budget.
  Full,
  /// The content object refused the store.
  Unwritten,
}

/// A backing's ranges as the table holds them: filled, and of those, asked writable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ranges {
  /// The ranges answered, by index.
  pub(crate) filled: BTreeSet<u64>,
  /// The ranges answered writable, by index.
  pub(crate) writable: BTreeSet<u64>,
}

/// The table: where it sits in the content object, its slots, the slot each recorded range holds, and the free ones.
#[derive(Debug)]
pub(crate) struct RangeTable {
  start: usize,
  slots: usize,
  /// The slot of each recorded `(attachment, inode, range)`, and whether it is recorded writable.
  held: BTreeMap<(u64, u64, u64), (usize, bool)>,
  /// Slots given back, reused first.
  freed: Vec<usize>,
  /// The first slot never yet used: every slot from here on is free.
  next: usize,
}

impl RangeTable {
  /// The table of `bytes` at `start` of `object`, with what a previous daemon recorded in it: every range of the
  /// previous daemon's held backings stays recorded until its successor forgets the backings it does not take back. A
  /// table of another layout or slot count, or one no daemon wrote, is cleared whole and started again. `None` when the
  /// table cannot hold a slot or the content object refuses it.
  pub(crate) fn open(object: &mut SparseObject, start: usize, bytes: usize) -> Option<RangeTable> {
    let slots = bytes.checked_sub(DAX_RANGE_HEADER_BYTES)? / DAX_RANGE_SLOT_BYTES;
    if slots == 0 {
      return None;
    }
    let mut table = RangeTable {
      start,
      slots,
      held: BTreeMap::new(),
      freed: Vec::new(),
      next: 0,
    };
    let ours = word(object, start)? == LAYOUT
      && word(object, start.checked_add(AT_SLOTS)?)? == u64::try_from(slots).ok()?;
    if ours {
      table.read(object)?;
    } else {
      object.zero(start, bytes).ok()?;
      object.write(start, &LAYOUT.to_le_bytes()).ok()?;
      object
        .write(
          start.checked_add(AT_SLOTS)?,
          &u64::try_from(slots).ok()?.to_le_bytes(),
        )
        .ok()?;
    }
    Some(table)
  }

  /// Reads every slot in use into the index: the rest are free.
  fn read(&mut self, object: &SparseObject) -> Option<()> {
    for slot in 0..self.slots {
      let at = self.slot_at(slot)?;
      let flags = word(object, at.checked_add(AT_FLAGS)?)?;
      if flags & USED == 0 {
        self.freed.push(slot);
        continue;
      }
      let attachment = word(object, at)?;
      let inode = word(object, at.checked_add(AT_INODE)?)?;
      let range = word(object, at.checked_add(AT_RANGE)?)?;
      self
        .held
        .insert((attachment, inode, range), (slot, flags & WRITABLE != 0));
    }
    // Reused in ascending order.
    self.freed.reverse();
    self.next = self.slots;
    Some(())
  }

  /// Where slot `slot` begins in the content object.
  fn slot_at(&self, slot: usize) -> Option<usize> {
    slot
      .checked_mul(DAX_RANGE_SLOT_BYTES)?
      .checked_add(DAX_RANGE_HEADER_BYTES)?
      .checked_add(self.start)
  }

  /// Records that `attachment`'s backing of `inode` answered range `range`, writable or not: a slot is taken for a range
  /// not yet recorded, and a recorded range asked writable now is marked so (never unmarked: its stores may still be
  /// owed to the file). Refused `Full` with every slot in use, `Unwritten` when the object refuses the store.
  pub(crate) fn record(
    &mut self,
    object: &mut SparseObject,
    (attachment, inode): (u64, u64),
    range: u64,
    writable: bool,
  ) -> Result<(), Unrecorded> {
    let key = (attachment, inode, range);
    if let Some(&(slot, recorded)) = self.held.get(&key) {
      if writable && !recorded {
        let at = self.slot_at(slot).ok_or(Unrecorded::Unwritten)?;
        store(object, at.checked_add(AT_FLAGS), USED | WRITABLE)?;
        self.held.insert(key, (slot, true));
      }
      return Ok(());
    }
    let slot = self.take_slot().ok_or(Unrecorded::Full)?;
    let at = self.slot_at(slot).ok_or(Unrecorded::Unwritten)?;
    let flags = if writable { USED | WRITABLE } else { USED };
    let written = store(object, Some(at), attachment)
      .and(store(object, at.checked_add(AT_INODE), inode))
      .and(store(object, at.checked_add(AT_RANGE), range))
      .and(store(object, at.checked_add(AT_FLAGS), flags));
    if let Err(refused) = written {
      self.freed.push(slot);
      return Err(refused);
    }
    self.held.insert(key, (slot, writable));
    Ok(())
  }

  /// A free slot, the last given back first, then one never used.
  fn take_slot(&mut self) -> Option<usize> {
    if let Some(slot) = self.freed.pop() {
      return Some(slot);
    }
    let slot = self.next;
    if slot >= self.slots {
      return None;
    }
    self.next = slot.saturating_add(1);
    Some(slot)
  }

  /// Forgets every range of `attachment`'s backing of `inode`: their slots' flags cleared and the slots given back.
  /// Refused `Unwritten` when the object refuses a store; the ranges it could clear are forgotten.
  pub(crate) fn forget(
    &mut self,
    object: &mut SparseObject,
    (attachment, inode): (u64, u64),
  ) -> Result<(), Unrecorded> {
    let keys: Vec<(u64, u64, u64)> = self
      .held
      .range((attachment, inode, 0)..=(attachment, inode, u64::MAX))
      .map(|(key, _)| *key)
      .collect();
    self.clear(object, keys)
  }

  /// Forgets every range of every backing of `attachment` ([`RangeTable::forget`] for each).
  pub(crate) fn forget_attachment(
    &mut self,
    object: &mut SparseObject,
    attachment: u64,
  ) -> Result<(), Unrecorded> {
    let keys: Vec<(u64, u64, u64)> = self
      .held
      .range((attachment, 0, 0)..=(attachment, u64::MAX, u64::MAX))
      .map(|(key, _)| *key)
      .collect();
    self.clear(object, keys)
  }

  /// Clears the slots of `keys`, giving each back; the first refusal is returned.
  fn clear(
    &mut self,
    object: &mut SparseObject,
    keys: Vec<(u64, u64, u64)>,
  ) -> Result<(), Unrecorded> {
    let mut outcome = Ok(());
    for key in keys {
      let Some((slot, _)) = self.held.remove(&key) else {
        continue;
      };
      let cleared = self
        .slot_at(slot)
        .ok_or(Unrecorded::Unwritten)
        .and_then(|at| store(object, at.checked_add(AT_FLAGS), 0));
      match cleared {
        Ok(()) => self.freed.push(slot),
        Err(refused) => {
          // Kept out of reuse: its flags may still name the range, which a reader then takes, as it would the backing's.
          if outcome.is_ok() {
            outcome = Err(refused);
          }
        }
      }
    }
    outcome
  }

  /// The ranges recorded for `attachment`'s backing of `inode`.
  pub(crate) fn ranges_of(&self, (attachment, inode): (u64, u64)) -> Ranges {
    let mut ranges = Ranges::default();
    for ((_, _, range), (_, writable)) in self
      .held
      .range((attachment, inode, 0)..=(attachment, inode, u64::MAX))
    {
      ranges.filled.insert(*range);
      if *writable {
        ranges.writable.insert(*range);
      }
    }
    ranges
  }

  /// The backings with ranges recorded, each once: `(attachment, inode)`.
  pub(crate) fn backings(&self) -> BTreeSet<(u64, u64)> {
    self
      .held
      .keys()
      .map(|(attachment, inode, _)| (*attachment, *inode))
      .collect()
  }
}

/// The little-endian word at `at`.
fn word(object: &SparseObject, at: usize) -> Option<u64> {
  let mut bytes = [0u8; size_of::<u64>()];
  object.read(at, &mut bytes).ok()?;
  Some(u64::from_le_bytes(bytes))
}

/// Stores `value` at `at`, refused `Unwritten` for no offset or a store the object refuses.
fn store(object: &mut SparseObject, at: Option<usize>, value: u64) -> Result<(), Unrecorded> {
  let at = at.ok_or(Unrecorded::Unwritten)?;
  object
    .write(at, &value.to_le_bytes())
    .map_err(|_| Unrecorded::Unwritten)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the slots the tests' tables hold.
  const SLOTS: usize = 4;
  /// Shape: where the tests' tables sit in their objects.
  const AT: usize = 64;
  /// Shape: a content object of one page, the table in it.
  const OBJECT_BYTES: usize = 4096;

  fn table_bytes() -> usize {
    DAX_RANGE_HEADER_BYTES + SLOTS * DAX_RANGE_SLOT_BYTES
  }

  fn object(tag: &str) -> SparseObject {
    SparseObject::create(
      &format!("slates-dax-ranges-test-{tag}-{}", std::process::id()),
      OBJECT_BYTES,
      slates_mem::Words::new(),
    )
    .unwrap()
  }

  /// A-365. Do: open a table on fresh memory; record two ranges of one backing, one of them writable and later the
  /// other made writable too, and one range of another backing; reopen the table over the same memory, as a successor
  /// does. Expect each backing's ranges back as recorded, writability included; forgetting the first backing frees its
  /// slots for the next ranges; and the table, full, refuses one more range `Full`.
  #[test]
  fn a_tables_ranges_outlive_their_writer_and_its_slots_are_bounded() {
    let mut object = object("bounded");
    let mut table = RangeTable::open(&mut object, AT, table_bytes()).unwrap();
    table.record(&mut object, (7, 100), 0, false).unwrap();
    table.record(&mut object, (7, 100), 3, true).unwrap();
    table.record(&mut object, (7, 100), 0, true).unwrap();
    table.record(&mut object, (8, 200), 5, false).unwrap();
    let reopened = RangeTable::open(&mut object, AT, table_bytes()).unwrap();
    assert_eq!(
      reopened.ranges_of((7, 100)),
      Ranges {
        filled: BTreeSet::from([0, 3]),
        writable: BTreeSet::from([0, 3]),
      }
    );
    assert_eq!(
      reopened.ranges_of((8, 200)),
      Ranges {
        filled: BTreeSet::from([5]),
        writable: BTreeSet::new(),
      }
    );
    assert_eq!(reopened.backings(), BTreeSet::from([(7, 100), (8, 200)]));
    let mut table = reopened;
    table.forget(&mut object, (7, 100)).unwrap();
    assert_eq!(table.ranges_of((7, 100)), Ranges::default());
    for range in 10..13 {
      table.record(&mut object, (9, 300), range, false).unwrap();
    }
    assert_eq!(
      table.record(&mut object, (9, 300), 13, false),
      Err(Unrecorded::Full)
    );
    table.forget_attachment(&mut object, 9).unwrap();
    let again = RangeTable::open(&mut object, AT, table_bytes()).unwrap();
    assert_eq!(again.backings(), BTreeSet::from([(8, 200)]));
  }

  /// A-365. Do: fill a table's memory with bytes no daemon of this layout wrote (a slot's flags set, a foreign layout
  /// word), and open it; then open a table whose slot count differs from what its memory says. Expect each started
  /// again empty, nothing read as a range.
  #[test]
  fn a_table_of_another_layout_is_started_again_never_misread() {
    let mut object = object("foreign");
    object.write(AT, &u64::MAX.to_le_bytes()).unwrap();
    let flags_of_first = AT + DAX_RANGE_HEADER_BYTES + AT_FLAGS;
    object.write(flags_of_first, &USED.to_le_bytes()).unwrap();
    let table = RangeTable::open(&mut object, AT, table_bytes()).unwrap();
    assert!(table.backings().is_empty());
    let mut table = table;
    table.record(&mut object, (1, 2), 3, true).unwrap();
    let fewer = table_bytes() - DAX_RANGE_SLOT_BYTES;
    let other = RangeTable::open(&mut object, AT, fewer).unwrap();
    assert!(
      other.backings().is_empty(),
      "another slot count is another layout"
    );
    assert!(RangeTable::open(&mut object, AT, DAX_RANGE_HEADER_BYTES).is_none());
  }
}
