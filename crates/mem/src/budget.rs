//! Budget arithmetic: the shard reserve, bounded-volume reservations that commit at creation or
//! refuse whole, and the dynamic-growth formulas of §4.2 with each result as a `Derived` value.
//!
//! Bounded: a reservation moves bytes from the reserve to the volume's accounting in one step;
//! if the reserve cannot cover it the request is refused with `BudgetExceeded { available }` and
//! nothing changes (never a partial volume). Dynamic: an increment is sized so that at the
//! measured allocation rate it outlasts the measured time to prepare the next one (map,
//! pre-fault, lock), and growth is granted only while projected free memory after the grant stays
//! above the reserve derived from the measured peak burst.

use slates_machine::{Derived, derived};

use crate::error::MemError;

/// A shard's byte reserve and its accounting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBudget {
  reserve: u64,
  committed: u64,
  peak_burst: u64,
}

/// A reservation of bytes for one bounded volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
  /// Bytes reserved.
  pub bytes: u64,
}

impl ShardBudget {
  /// A budget over `reserve` pre-faulted, locked bytes, with `peak_burst` the measured peak
  /// burst across volumes that the free floor is derived from.
  pub const fn new(reserve: u64, peak_burst: u64) -> Self {
    Self {
      reserve,
      committed: 0,
      peak_burst,
    }
  }

  /// Bytes available to a new reservation.
  pub const fn available(&self) -> u64 {
    self.reserve.saturating_sub(self.committed)
  }

  /// Bytes committed to volumes.
  pub const fn committed(&self) -> u64 {
    self.committed
  }

  /// The free floor: the measured peak burst, kept free so a burst never meets exhaustion.
  pub fn floor(&self) -> Derived<u64> {
    derived!(
      self.peak_burst,
      "measured peak burst across volumes",
      ["mem.peak_burst"]
    )
  }

  /// Reserves `bytes` for a bounded volume, whole or not at all.
  pub fn reserve(&mut self, bytes: u64) -> Result<Reservation, MemError> {
    let available = self.available();
    if bytes > available {
      return Err(MemError::BudgetExceeded {
        requested: bytes,
        available,
      });
    }
    self.committed += bytes;
    Ok(Reservation { bytes })
  }

  /// Returns a reservation's bytes to the reserve.
  pub fn release(&mut self, reservation: Reservation) {
    self.committed = self.committed.saturating_sub(reservation.bytes);
  }

  /// Whether a dynamic volume may grow by `increment` now: only while the projected free bytes
  /// after the grant stay above the floor.
  pub fn may_grow(&self, increment: u64) -> bool {
    self.available().saturating_sub(increment) >= self.floor().get()
  }

  /// Grows a dynamic volume by `increment` under the floor rule.
  pub fn grow(&mut self, increment: u64) -> Result<Reservation, MemError> {
    if !self.may_grow(increment) {
      return Err(MemError::BudgetExceeded {
        requested: increment,
        available: self.available().saturating_sub(self.floor().get()),
      });
    }
    self.committed += increment;
    Ok(Reservation { bytes: increment })
  }
}

/// The dynamic-growth increment: at the measured p99 allocation rate (the p99 is the safety
/// margin over the median), the increment must outlast the measured time to prepare the next
/// one; never smaller than one slab.
pub fn growth_increment(
  rate_p99_bytes_per_ns: u64,
  prepare_ns: u64,
  slab_bytes: u64,
) -> Derived<u64> {
  derived!(
    rate_p99_bytes_per_ns
      .saturating_mul(prepare_ns)
      .max(slab_bytes),
    "max(p99 allocation rate × measured prepare time, one slab)",
    ["mem.alloc_rate_p99", "mem.prepare_ns", "mem.slab_bytes"]
  )
}

/// The slab size: the smallest page multiple that holds the measured p99 burst of allocations
/// per operation at the slot size.
pub fn slab_bytes(page: u64, burst_p99_slots: u64, slot_bytes: u64) -> Derived<u64> {
  let needed = burst_p99_slots.max(1).saturating_mul(slot_bytes.max(1));
  derived!(
    needed.div_ceil(page.max(1)).saturating_mul(page.max(1)),
    "smallest page multiple ≥ p99 allocation burst per operation × slot size",
    ["page.base", "mem.burst_p99", "slot_bytes"]
  )
}

/// The region size for a shard: its share of the lock capacity divided among the size classes.
pub fn region_bytes(lock_capacity: u64, shards: u64, classes: u64) -> Derived<u64> {
  derived!(
    lock_capacity / shards.max(1) / classes.max(1),
    "lock capacity / shards / classes",
    ["lock.bytes", "rt.shards", "mem.classes"]
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_bounded_reservation_commits_whole_or_refuses_with_what_is_available() {
    let mut b = ShardBudget::new(6 << 30, 1 << 20);
    let r = b.reserve(4 << 30).unwrap();
    assert_eq!(r.bytes, 4 << 30);
    assert_eq!(b.available(), 2 << 30);
    match b.reserve(3 << 30) {
      Err(MemError::BudgetExceeded {
        requested,
        available,
      }) => {
        assert_eq!(requested, 3 << 30);
        assert_eq!(available, 2 << 30);
      }
      other => panic!("{other:?}"),
    }
    assert_eq!(
      b.committed(),
      4 << 30,
      "a refused reservation changes nothing"
    );
    b.release(r);
    assert_eq!(b.available(), 6 << 30);
  }

  #[test]
  fn dynamic_growth_stops_at_the_floor_derived_from_the_peak_burst() {
    let mut b = ShardBudget::new(100, 30);
    assert_eq!(b.floor().get(), 30);
    assert!(b.may_grow(70));
    assert!(!b.may_grow(71));
    b.grow(70).unwrap();
    assert!(matches!(
      b.grow(1),
      Err(MemError::BudgetExceeded { available: 0, .. })
    ));
  }

  #[test]
  fn the_growth_formulas_carry_their_anchors() {
    let g = growth_increment(3, 1_000, 4096);
    assert_eq!(g.get(), 4096, "one slab is the floor");
    assert_eq!(growth_increment(10, 1_000, 4096).get(), 10_000);
    assert!(g.anchors.contains(&"mem.prepare_ns"));
    let s = slab_bytes(4096, 100, 48);
    assert_eq!(s.get(), 8192);
    assert_eq!(region_bytes(1 << 40, 5, 4).get(), (1 << 40) / 20);
  }
}
