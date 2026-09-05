//! The RAM-only locking sequence: lock regions in priority order (metadata slabs first, then
//! rings, then chunk regions) within the capacity the profile measured, and report exactly what
//! was locked and what was not (§4.2, "RAM-only"; D-12 "honest degradation").
//!
//! The first refusal stops the sequence: a lower-priority region would meet the same limit, and
//! trying it would only waste the syscall. Everything after the refusal stays mapped and usable,
//! unlocked, and counted.

use crate::error::MemError;
use crate::region::Region;

/// The order regions are locked in; lower locks first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
  /// Metadata slabs: the tables every operation walks.
  Metadata,
  /// Rings: what clients and shards write into.
  Rings,
  /// Chunk regions: file bytes.
  Chunks,
}

/// What the sequence achieved.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LockReport {
  /// Bytes asked to be locked.
  pub requested: usize,
  /// Bytes locked.
  pub locked: usize,
  /// Bytes left unlocked (usable, unlocked, counted).
  pub unlocked: usize,
  /// The refusal that stopped the sequence, if one did.
  pub refusal: Option<MemError>,
  /// Regions that stayed unlocked because the capacity would have been exceeded before the OS
  /// was even asked.
  pub over_capacity: usize,
}

/// Locks `regions` (each with its priority) within `capacity_bytes`.
pub fn lock_in_order(regions: &mut [(Priority, &mut Region)], capacity_bytes: usize) -> LockReport {
  regions.sort_by_key(|(priority, _)| *priority);
  let mut report = LockReport::default();
  let mut stopped = false;
  for (_, region) in regions.iter_mut() {
    report.requested += region.len();
    if stopped {
      report.unlocked += region.len();
      continue;
    }
    if report.locked.saturating_add(region.len()) > capacity_bytes {
      report.over_capacity += 1;
      report.unlocked += region.len();
      stopped = true;
      continue;
    }
    match region.lock() {
      Ok(()) => report.locked += region.len(),
      Err(e) => {
        report.refusal = Some(e);
        report.unlocked += region.len();
        stopped = true;
      }
    }
  }
  report
}

#[cfg(test)]
mod tests {
  use super::*;

  fn page() -> usize {
    usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap()
  }

  #[test]
  fn regions_lock_in_priority_order_within_capacity_and_report_the_rest() {
    let p = page();
    let mut chunks = Region::map(p * 8, p, false).unwrap();
    let mut meta = Region::map(p * 2, p, false).unwrap();
    let mut rings = Region::map(p * 4, p, false).unwrap();
    let mut list = [
      (Priority::Chunks, &mut chunks),
      (Priority::Metadata, &mut meta),
      (Priority::Rings, &mut rings),
    ];
    let report = lock_in_order(&mut list, p * 6);
    assert_eq!(report.requested, p * 14);
    assert_eq!(
      report.locked,
      p * 6,
      "metadata and rings fit; chunks would exceed capacity"
    );
    assert_eq!(report.unlocked, p * 8);
    assert_eq!(report.over_capacity, 1);
    assert!(report.refusal.is_none());
    assert!(meta.locked() && rings.locked() && !chunks.locked());
  }

  #[test]
  fn zero_capacity_locks_nothing_and_crashes_nothing() {
    let p = page();
    let mut r = Region::map(p, p, false).unwrap();
    let mut list = [(Priority::Metadata, &mut r)];
    let report = lock_in_order(&mut list, 0);
    assert_eq!(report.locked, 0);
    assert_eq!(report.unlocked, p);
    assert!(!r.locked());
  }
}
