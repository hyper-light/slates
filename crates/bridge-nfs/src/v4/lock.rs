//! NFSv4 byte-range locks (RFC 8881 §9, §18.10–18.12; A-35): the range algebra one lock-owner's locks
//! on one file follow, with POSIX semantics (a lock over the owner's own ranges replaces them there, an
//! unlock splits them, adjacent ranges of one type merge), and the conflict rule between owners (two
//! ranges conflict when they overlap and either is a write lock).
//!
//! The server's lock table ([`crate::v4::compound`]) keeps one [`OwnerRanges`] per (file, client,
//! lock-owner); this module is pure, so its rules are tested against a byte-level model on every
//! generated history.

use std::collections::BTreeMap;

use super::Nfsstat4;
use super::types::{OTHER_SIZE, Stateid};
use crate::nfs::Nfsfh3;
use crate::xdr::XdrWriter;

/// A lock's type: shared (`READ_LT`, `READW_LT`) or exclusive (`WRITE_LT`, `WRITEW_LT`). A blocking
/// request is answered as a non-blocking one: without callbacks the client polls (RFC 8881 §9.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
  /// A shared lock.
  Read,
  /// An exclusive lock.
  Write,
}

/// Format: `nfs_lock_type4` (RFC 7863): `READ_LT`, `WRITE_LT`, `READW_LT`, `WRITEW_LT`.
pub mod locktype {
  /// Format: `READ_LT`.
  pub const READ: u32 = 1;
  /// Format: `WRITE_LT`.
  pub const WRITE: u32 = 2;
  /// Format: `READW_LT`.
  pub const READ_WAIT: u32 = 3;
  /// Format: `WRITEW_LT`.
  pub const WRITE_WAIT: u32 = 4;
}

impl LockKind {
  /// The kind a wire lock type names, or `None` for a value `nfs_lock_type4` does not define.
  pub fn from_wire(value: u32) -> Option<LockKind> {
    match value {
      locktype::READ | locktype::READ_WAIT => Some(LockKind::Read),
      locktype::WRITE | locktype::WRITE_WAIT => Some(LockKind::Write),
      _ => None,
    }
  }

  /// The non-blocking wire type of this kind (what a `LOCK4denied` reports).
  pub fn wire(self) -> u32 {
    match self {
      LockKind::Read => locktype::READ,
      LockKind::Write => locktype::WRITE,
    }
  }
}

/// A half-open byte range `[start, end)`; `end == u64::MAX` reaches the end of any file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
  /// The first byte.
  pub start: u64,
  /// One past the last byte, or `u64::MAX` for "to the end of the file".
  pub end: u64,
}

impl Range {
  /// The range an `offset4`/`length4` pair names: `length == u64::MAX` is "to the end of the file";
  /// a zero length, or one whose end would pass `u64::MAX`, is `None` (`NFS4ERR_INVAL`, §18.10.3).
  pub fn of(offset: u64, length: u64) -> Option<Range> {
    if length == 0 {
      return None;
    }
    let end = if length == u64::MAX {
      u64::MAX
    } else {
      offset.checked_add(length)?
    };
    Some(Range { start: offset, end })
  }

  /// The `length4` this range is reported with.
  pub fn length(self) -> u64 {
    if self.end == u64::MAX {
      u64::MAX
    } else {
      self.end - self.start
    }
  }

  /// Whether the two ranges share a byte.
  pub fn overlaps(self, other: Range) -> bool {
    self.start < other.end && other.start < self.end
  }
}

/// One lock-owner's locks on one file: disjoint ranges in order, adjacent ranges of one kind merged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerRanges {
  ranges: Vec<(Range, LockKind)>,
}

impl OwnerRanges {
  /// The owner's ranges, in order.
  pub fn ranges(&self) -> &[(Range, LockKind)] {
    &self.ranges
  }

  /// Whether the owner holds no lock.
  pub fn is_empty(&self) -> bool {
    self.ranges.is_empty()
  }

  /// The first of the owner's ranges that conflicts with `kind` over `range`: an overlap where either
  /// is a write lock.
  pub fn conflict(&self, range: Range, kind: LockKind) -> Option<(Range, LockKind)> {
    self
      .ranges
      .iter()
      .find(|(held, held_kind)| {
        held.overlaps(range) && (kind == LockKind::Write || *held_kind == LockKind::Write)
      })
      .copied()
  }

  /// The ranges the owner would hold after locking `range` as `kind` (POSIX: the new lock replaces the
  /// owner's own locks over `range`; ranges of one kind that touch merge).
  pub fn locked(&self, range: Range, kind: LockKind) -> OwnerRanges {
    let mut next = self.unlocked(range);
    next.ranges.push((range, kind));
    next.ranges.sort_by_key(|(held, _)| held.start);
    next.merge();
    next
  }

  /// The ranges the owner would hold after unlocking `range` (the parts of its ranges outside it).
  pub fn unlocked(&self, range: Range) -> OwnerRanges {
    let mut ranges = Vec::with_capacity(self.ranges.len() + 1);
    for &(held, kind) in &self.ranges {
      if !held.overlaps(range) {
        ranges.push((held, kind));
        continue;
      }
      if held.start < range.start {
        ranges.push((
          Range {
            start: held.start,
            end: range.start,
          },
          kind,
        ));
      }
      if range.end < held.end {
        ranges.push((
          Range {
            start: range.end,
            end: held.end,
          },
          kind,
        ));
      }
    }
    OwnerRanges { ranges }
  }

  /// Joins neighbouring ranges of one kind that touch or overlap.
  fn merge(&mut self) {
    let mut merged: Vec<(Range, LockKind)> = Vec::with_capacity(self.ranges.len());
    for &(range, kind) in &self.ranges {
      match merged.last_mut() {
        Some((last, last_kind)) if *last_kind == kind && range.start <= last.end => {
          last.end = last.end.max(range.end);
        }
        _ => merged.push((range, kind)),
      }
    }
    self.ranges = merged;
  }
}

/// The stateid `other` bytes (RFC 8881 §8.2.2).
pub type Other = [u8; OTHER_SIZE];

/// One lock-owner's lock state on one file (a lock state id): the client and owner, the file, the open
/// it was created from, its seqid, and its ranges.
#[derive(Clone, Debug)]
pub struct LockState {
  /// The client that holds it.
  pub clientid: u64,
  /// The lock-owner.
  pub owner: Vec<u8>,
  /// The file.
  pub fh: Nfsfh3,
  /// The open state id it was created from.
  pub open: Other,
  /// The state id's current seqid.
  pub seqid: u32,
  /// The ranges it holds.
  pub ranges: OwnerRanges,
}

/// A conflicting lock, as a `LOCK4denied` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
  /// The conflicting range.
  pub range: Range,
  /// Its kind.
  pub kind: LockKind,
  /// The client that holds it.
  pub clientid: u64,
  /// Its lock-owner.
  pub owner: Vec<u8>,
}

impl Denied {
  /// The `LOCK4denied` body.
  pub fn encode(&self) -> Vec<u8> {
    let mut body = XdrWriter::new();
    body.u64(self.range.start);
    body.u64(self.range.length());
    body.u32(self.kind.wire());
    body.u64(self.clientid);
    body.opaque(&self.owner);
    body.into_bytes()
  }
}

/// The key a lock state is found by from its file: the file first, so every lock on one file is one
/// range of the index.
type LockKey = (Vec<u8>, u64, Vec<u8>);

/// The server's lock states, bounded by the ranges they hold together (each state counts as one while
/// it holds none).
pub struct LockTable {
  max_ranges: usize,
  next: u64,
  boot: u32,
  table: BTreeMap<Other, LockState>,
  by_owner: BTreeMap<LockKey, Other>,
}

/// Format: the bit that marks a lock state id's counter, so lock and open state ids never collide.
const LOCK_ID_BIT: u64 = 1 << 63;

impl LockTable {
  /// An empty table for server instance `boot`, holding at most `max_ranges` ranges and states.
  pub fn new(boot: u32, max_ranges: usize) -> LockTable {
    LockTable {
      max_ranges,
      next: 1,
      boot,
      table: BTreeMap::new(),
      by_owner: BTreeMap::new(),
    }
  }

  /// The table's charge: every range held, and one for each state holding none.
  fn charge(&self) -> usize {
    self
      .table
      .values()
      .map(|state| state.ranges.ranges().len().max(1))
      .sum()
  }

  /// The first lock another owner holds on `fh` that conflicts with `kind` over `range`.
  pub fn conflict(
    &self,
    fh: &Nfsfh3,
    clientid: u64,
    owner: &[u8],
    range: Range,
    kind: LockKind,
  ) -> Option<Denied> {
    self
      .by_owner
      .range((fh.0.clone(), 0, Vec::new())..)
      .take_while(|((file, _, _), _)| *file == fh.0)
      .filter(|((_, holder, holder_owner), _)| {
        (*holder, holder_owner.as_slice()) != (clientid, owner)
      })
      .filter_map(|(_, other)| self.table.get(other))
      .find_map(|state| {
        state
          .ranges
          .conflict(range, kind)
          .map(|(held, held_kind)| Denied {
            range: held,
            kind: held_kind,
            clientid: state.clientid,
            owner: state.owner.clone(),
          })
      })
  }

  /// The lock state of `owner` of `clientid` on `fh`, created from `open` if it has none;
  /// `NFS4ERR_DELAY` at the table's bound (the only exhaustion status LOCK allows, RFC 8881 §15.2).
  pub fn state_for(
    &mut self,
    clientid: u64,
    owner: Vec<u8>,
    fh: &Nfsfh3,
    open: Other,
  ) -> Result<Other, Nfsstat4> {
    let key: LockKey = (fh.0.clone(), clientid, owner);
    if let Some(other) = self.by_owner.get(&key) {
      return Ok(*other);
    }
    if self.charge() >= self.max_ranges {
      return Err(Nfsstat4::Delay);
    }
    let mut other = [0u8; OTHER_SIZE];
    other[..8].copy_from_slice(&(LOCK_ID_BIT | self.next).to_be_bytes());
    other[8..].copy_from_slice(&self.boot.to_be_bytes());
    self.next = self.next.saturating_add(1);
    self.table.insert(
      other,
      LockState {
        clientid,
        owner: key.2.clone(),
        fh: fh.clone(),
        open,
        seqid: 0,
        ranges: OwnerRanges::default(),
      },
    );
    self.by_owner.insert(key, other);
    Ok(other)
  }

  /// The lock state `stateid` names for `clientid` on `fh`, at a current seqid (0 is "the current
  /// one"; an earlier one is `NFS4ERR_OLD_STATEID`, §8.2.2); anything else is `NFS4ERR_BAD_STATEID`.
  pub fn get(
    &self,
    stateid: &Stateid,
    fh: &Nfsfh3,
    clientid: Option<u64>,
  ) -> Result<&LockState, Nfsstat4> {
    let state = self.table.get(&stateid.other).ok_or(Nfsstat4::BadStateid)?;
    if state.fh != *fh || Some(state.clientid) != clientid {
      return Err(Nfsstat4::BadStateid);
    }
    match stateid.seqid {
      0 => Ok(state),
      seqid if seqid == state.seqid => Ok(state),
      seqid if seqid < state.seqid => Err(Nfsstat4::OldStateid),
      _ => Err(Nfsstat4::BadStateid),
    }
  }

  /// Sets the ranges of the state `other` to `ranges` and advances its state id; `NFS4ERR_DELAY` if
  /// that would pass the table's bound.
  pub fn set_ranges(&mut self, other: &Other, ranges: OwnerRanges) -> Result<Stateid, Nfsstat4> {
    let before = self
      .table
      .get(other)
      .map(|state| state.ranges.ranges().len().max(1))
      .ok_or(Nfsstat4::BadStateid)?;
    let charge = self.charge() - before + ranges.ranges().len().max(1);
    if charge > self.max_ranges {
      return Err(Nfsstat4::Delay);
    }
    let state = self.table.get_mut(other).ok_or(Nfsstat4::BadStateid)?;
    state.ranges = ranges;
    state.seqid = state.seqid.checked_add(1).unwrap_or(1);
    Ok(Stateid {
      seqid: state.seqid,
      other: *other,
    })
  }

  /// The lock state `other` names.
  pub fn state(&self, other: &Other) -> Option<&LockState> {
    self.table.get(other)
  }

  /// How many lock states the table holds.
  pub fn state_count(&self) -> usize {
    self.table.len()
  }

  /// Whether `other` names a lock state.
  pub fn contains(&self, other: &Other) -> bool {
    self.table.contains_key(other)
  }

  /// Whether any lock state created from the open `open` holds a lock.
  pub fn held_under(&self, open: &Other) -> bool {
    self
      .table
      .values()
      .any(|state| state.open == *open && !state.ranges.is_empty())
  }

  /// Drops the lock states created from the open `open` (none of which holds a lock).
  pub fn drop_under(&mut self, open: &Other) {
    let gone: Vec<Other> = self
      .table
      .iter()
      .filter(|(_, state)| state.open == *open)
      .map(|(other, _)| *other)
      .collect();
    for other in &gone {
      self.drop_state(other);
    }
  }

  /// `FREE_STATEID` of a lock state: `NFS4ERR_LOCKS_HELD` while it holds a lock (§18.38.3).
  pub fn free(&mut self, other: &Other) -> Result<(), Nfsstat4> {
    let state = self.table.get(other).ok_or(Nfsstat4::BadStateid)?;
    if !state.ranges.is_empty() {
      return Err(Nfsstat4::LocksHeld);
    }
    self.drop_state(other);
    Ok(())
  }

  /// Drops every lock state whose client `live` no longer holds.
  pub fn purge(&mut self, live: impl Fn(u64) -> bool) {
    let gone: Vec<Other> = self
      .table
      .iter()
      .filter(|(_, state)| !live(state.clientid))
      .map(|(other, _)| *other)
      .collect();
    for other in &gone {
      self.drop_state(other);
    }
  }

  fn drop_state(&mut self, other: &Other) {
    if let Some(state) = self.table.remove(other) {
      self
        .by_owner
        .remove(&(state.fh.0, state.clientid, state.owner));
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// Shape: the bytes the byte-level model covers; ranges are drawn inside it, with "to the end".
  const DOMAIN: u64 = 48;

  /// The byte-level oracle: each byte's lock kind, or none.
  fn model_apply(model: &mut [Option<LockKind>], range: Range, kind: Option<LockKind>) {
    let end = range.end.min(DOMAIN);
    for byte in range.start..end {
      model[usize::try_from(byte).unwrap()] = kind;
    }
  }

  /// The ranges' byte image over the model's domain.
  fn image(ranges: &OwnerRanges) -> Vec<Option<LockKind>> {
    let mut bytes = vec![None; usize::try_from(DOMAIN).unwrap()];
    for &(range, kind) in ranges.ranges() {
      model_apply(&mut bytes, range, Some(kind));
    }
    bytes
  }

  fn range_strategy() -> impl Strategy<Value = Range> {
    (0..DOMAIN, 1..=DOMAIN, any::<bool>()).prop_map(|(start, length, to_end)| Range {
      start,
      end: if to_end { u64::MAX } else { start + length },
    })
  }

  proptest! {
    /// A-35 (the lock algebra, RFC 8881 §18.10.4 POSIX semantics): on every generated history of locks
    /// and unlocks, the owner's ranges equal the byte-level model, stay disjoint and ordered, and never
    /// leave two touching ranges of one kind unmerged.
    #[test]
    fn locks_and_unlocks_match_the_byte_model(
      steps in proptest::collection::vec((range_strategy(), prop::option::of(any::<bool>())), 1..40)
    ) {
      let mut owner = OwnerRanges::default();
      let mut model = vec![None; usize::try_from(DOMAIN).unwrap()];
      for (range, action) in steps {
        let kind = action.map(|write| if write { LockKind::Write } else { LockKind::Read });
        owner = match kind {
          Some(kind) => owner.locked(range, kind),
          None => owner.unlocked(range),
        };
        model_apply(&mut model, range, kind);
        prop_assert_eq!(image(&owner), model.clone());
        for pair in owner.ranges().windows(2) {
          prop_assert!(pair[0].0.end <= pair[1].0.start, "disjoint and ordered");
          prop_assert!(
            pair[0].0.end < pair[1].0.start || pair[0].1 != pair[1].1,
            "touching ranges of one kind are merged"
          );
        }
      }
    }
  }

  /// A-35: two owners' ranges conflict only where they overlap and either is a write lock.
  #[test]
  fn a_conflict_needs_an_overlap_and_a_writer() {
    let held = OwnerRanges::default().locked(Range { start: 10, end: 20 }, LockKind::Read);
    assert_eq!(
      held.conflict(Range { start: 15, end: 30 }, LockKind::Read),
      None
    );
    assert!(
      held
        .conflict(Range { start: 15, end: 30 }, LockKind::Write)
        .is_some()
    );
    assert_eq!(
      held.conflict(Range { start: 20, end: 30 }, LockKind::Write),
      None
    );
    let writer = OwnerRanges::default().locked(
      Range {
        start: 0,
        end: u64::MAX,
      },
      LockKind::Write,
    );
    assert!(
      writer
        .conflict(
          Range {
            start: u64::MAX - 1,
            end: u64::MAX
          },
          LockKind::Read
        )
        .is_some()
    );
  }

  /// A-35 (§18.10.3): a zero length and an end past `u64::MAX` are invalid; `u64::MAX` is "to the end".
  #[test]
  fn a_range_is_refused_when_empty_or_past_the_largest_offset() {
    assert_eq!(Range::of(5, 0), None);
    assert_eq!(Range::of(u64::MAX - 1, 2), None);
    assert_eq!(Range::of(7, u64::MAX).map(Range::length), Some(u64::MAX));
    assert_eq!(Range::of(7, 3), Some(Range { start: 7, end: 10 }));
  }
}
