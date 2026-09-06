//! Budget arithmetic: the shard reserve, bounded-volume reservations that commit at creation or
//! refuse whole, and the dynamic-growth formulas of §4.2 with each result as a `Derived` value.
//!
//! Bounded: a reservation moves bytes from the reserve to the volume's accounting in one step;
//! if the reserve cannot cover it — keeping the operation headroom free — the request is refused
//! with `BudgetExceeded { available }` and nothing changes (never a partial volume). Dynamic: an
//! increment is sized so that at the measured allocation rate it outlasts the measured time to
//! prepare the next one (map, pre-fault, lock), and growth is admitted through this same budget,
//! taking only capacity that is neither committed to another volume nor the operation headroom —
//! so a dynamic volume never eats a bounded volume's sacred claim or the room in-flight operations
//! need to coexist (§4.2).

use slates_machine::{Derived, derived};

use crate::error::MemError;

/// A shard's byte reserve and its accounting (§4.2 atomic admission). The `reserve` is the effective
/// capacity — the usable, prepared arena; `committed` is the entitlement handed to volumes; and
/// `headroom` is the operation headroom kept free for the bounded temporary coexistence of in-flight
/// operations (a copy-up holds a source chunk and its new extent at once). Every admission — a
/// bounded reservation or a dynamic growth alike — leaves the headroom free and takes only from
/// capacity not already committed, so growth consumes only unpromised space and a burst never meets
/// exhaustion. This is the one capacity owner: control-path reservations and dynamic growth both go
/// through it, with no second, looser test against raw free memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBudget {
  reserve: u64,
  committed: u64,
  headroom: u64,
}

/// A reservation of bytes for one bounded volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
  /// Bytes reserved.
  pub bytes: u64,
}

impl ShardBudget {
  /// A budget over `reserve` pre-faulted, locked bytes (the effective capacity), keeping `headroom`
  /// bytes free for in-flight operation coexistence (§4.2). The caller derives `headroom` from
  /// structural anchors (concurrent copy-ups × the copy-up window); the budget only enforces it.
  pub const fn new(reserve: u64, headroom: u64) -> Self {
    Self {
      reserve,
      committed: 0,
      headroom,
    }
  }

  /// The effective capacity — the usable, prepared arena the budget is over.
  pub const fn capacity(&self) -> u64 {
    self.reserve
  }

  /// Bytes committed to volumes (their entitlement).
  pub const fn committed(&self) -> u64 {
    self.committed
  }

  /// The operation headroom kept free of every admission (§4.2).
  pub const fn headroom(&self) -> u64 {
    self.headroom
  }

  /// Bytes an admission — a reservation or a growth — may still take: the effective capacity, less
  /// what is committed, less the operation headroom every admission leaves free. So both a bounded
  /// reservation and a dynamic growth take only unpromised capacity and never eat the headroom.
  pub const fn admittable(&self) -> u64 {
    self
      .reserve
      .saturating_sub(self.committed)
      .saturating_sub(self.headroom)
  }

  /// Reserves `bytes` for a bounded volume, whole or not at all, keeping the operation headroom free.
  pub fn reserve(&mut self, bytes: u64) -> Result<Reservation, MemError> {
    let available = self.admittable();
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

  /// Whether a dynamic volume may grow by `increment` now: only from capacity that is neither
  /// committed to another volume nor the operation headroom — the same rule a reservation obeys, so
  /// growth is sacred-claim-safe by construction and never needs a separate free-memory test.
  pub const fn may_grow(&self, increment: u64) -> bool {
    increment <= self.admittable()
  }

  /// Grows a dynamic volume by `increment`, consuming only unpromised capacity above the headroom.
  pub fn grow(&mut self, increment: u64) -> Result<Reservation, MemError> {
    let available = self.admittable();
    if increment > available {
      return Err(MemError::BudgetExceeded {
        requested: increment,
        available,
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
  fn a_bounded_reservation_commits_whole_and_keeps_the_operation_headroom_free() {
    let mut b = ShardBudget::new(6 << 30, 1 << 30); // 6 GiB capacity, 1 GiB operation headroom
    assert_eq!(
      b.admittable(),
      5 << 30,
      "only capacity above the headroom is admittable"
    );
    let r = b.reserve(4 << 30).unwrap();
    assert_eq!(r.bytes, 4 << 30);
    assert_eq!(
      b.admittable(),
      1 << 30,
      "4 GiB committed, the 1 GiB headroom still kept free"
    );
    // A reservation that would dip into the headroom refuses, whole, offering only the space above it.
    match b.reserve(2 << 30) {
      Err(MemError::BudgetExceeded {
        requested,
        available,
      }) => {
        assert_eq!(requested, 2 << 30);
        assert_eq!(available, 1 << 30);
      }
      other => panic!("{other:?}"),
    }
    assert_eq!(
      b.committed(),
      4 << 30,
      "a refused reservation changes nothing"
    );
    b.release(r);
    assert_eq!(b.admittable(), 5 << 30);
  }

  #[test]
  fn dynamic_growth_takes_only_unpromised_capacity_above_the_headroom() {
    let mut b = ShardBudget::new(100, 30);
    assert_eq!(b.headroom(), 30);
    assert_eq!(b.admittable(), 70, "capacity above the operation headroom");
    assert!(b.may_grow(70));
    assert!(!b.may_grow(71), "growth may not dip into the headroom");
    b.grow(70).unwrap();
    assert!(matches!(
      b.grow(1),
      Err(MemError::BudgetExceeded { available: 0, .. })
    ));
  }

  /// The one capacity owner: a dynamic volume's growth cannot eat a bounded volume's reservation
  /// (a sacred claim) or the operation headroom — the same admittable rule bounds both, so growth
  /// takes only genuinely unpromised capacity (§4.2).
  #[test]
  fn dynamic_growth_cannot_eat_a_bounded_reservation_or_the_headroom() {
    let mut b = ShardBudget::new(100, 20);
    b.reserve(50).unwrap(); // a bounded volume's sacred 50
    assert_eq!(
      b.admittable(),
      30,
      "100 capacity − 50 committed − 20 headroom"
    );
    assert!(b.may_grow(30));
    assert!(
      !b.may_grow(31),
      "growth cannot dip into the bounded reservation or the headroom"
    );
    b.grow(30).unwrap();
    assert!(matches!(b.grow(1), Err(MemError::BudgetExceeded { .. })));
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
