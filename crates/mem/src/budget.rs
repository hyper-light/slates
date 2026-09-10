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

/// The reservation arithmetic every §4.2 capacity dimension shares: a `reserve` (the effective
/// capacity the dimension is over), the `committed` entitlement already handed out, and the
/// `headroom` kept free for the bounded temporary coexistence of in-flight operations. `admittable`
/// is what a further admission may still take — capacity, less committed, less the headroom every
/// admission leaves free. The unit is the dimension's — bytes for content, version slots for the
/// inode slab — and is named by the wrapper that owns the ledger; because the arithmetic is
/// identical across dimensions it lives once, here, rather than duplicated per wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ledger {
  reserve: u64,
  committed: u64,
  headroom: u64,
}

impl Ledger {
  const fn new(reserve: u64, headroom: u64) -> Self {
    Self {
      reserve,
      committed: 0,
      headroom,
    }
  }

  const fn admittable(&self) -> u64 {
    self
      .reserve
      .saturating_sub(self.committed)
      .saturating_sub(self.headroom)
  }

  /// Commits `amount` whole or refuses, keeping the headroom free; returns the amount committed so
  /// the caller can wrap it in its unit's credit type.
  fn take(&mut self, amount: u64) -> Result<u64, MemError> {
    let available = self.admittable();
    if amount > available {
      return Err(MemError::BudgetExceeded {
        requested: amount,
        available,
      });
    }
    self.committed += amount;
    Ok(amount)
  }

  /// Returns `amount` to the reserve.
  fn give(&mut self, amount: u64) {
    self.committed = self.committed.saturating_sub(amount);
  }
}

/// A shard's byte reserve and its accounting (§4.2 atomic admission). The capacity is the effective
/// capacity — the usable, prepared arena; the committed bytes are the entitlement handed to volumes;
/// and the headroom is the operation headroom kept free for the bounded temporary coexistence of
/// in-flight operations (a copy-up holds a source chunk and its new extent at once). Every
/// admission — a bounded reservation or a dynamic growth alike — leaves the headroom free and takes
/// only from capacity not already committed, so growth consumes only unpromised space and a burst
/// never meets exhaustion. This is the one capacity owner for content bytes: control-path
/// reservations and dynamic growth both go through it, with no second, looser test against raw free
/// memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBudget {
  ledger: Ledger,
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
      ledger: Ledger::new(reserve, headroom),
    }
  }

  /// The effective capacity — the usable, prepared arena the budget is over.
  pub const fn capacity(&self) -> u64 {
    self.ledger.reserve
  }

  /// Bytes committed to volumes (their entitlement).
  pub const fn committed(&self) -> u64 {
    self.ledger.committed
  }

  /// The operation headroom kept free of every admission (§4.2).
  pub const fn headroom(&self) -> u64 {
    self.ledger.headroom
  }

  /// Bytes an admission — a reservation or a growth — may still take: the effective capacity, less
  /// what is committed, less the operation headroom every admission leaves free. So both a bounded
  /// reservation and a dynamic growth take only unpromised capacity and never eat the headroom.
  pub const fn admittable(&self) -> u64 {
    self.ledger.admittable()
  }

  /// Reserves `bytes` for a bounded volume, whole or not at all, keeping the operation headroom free.
  pub fn reserve(&mut self, bytes: u64) -> Result<Reservation, MemError> {
    self.ledger.take(bytes).map(|bytes| Reservation { bytes })
  }

  /// Returns a reservation's bytes to the reserve.
  pub fn release(&mut self, reservation: Reservation) {
    self.ledger.give(reservation.bytes);
  }

  /// Whether a dynamic volume may grow by `increment` now: only from capacity that is neither
  /// committed to another volume nor the operation headroom — the same rule a reservation obeys, so
  /// growth is sacred-claim-safe by construction and never needs a separate free-memory test.
  pub const fn may_grow(&self, increment: u64) -> bool {
    increment <= self.ledger.admittable()
  }

  /// Grows a dynamic volume by `increment`, consuming only unpromised capacity above the headroom.
  pub fn grow(&mut self, increment: u64) -> Result<Reservation, MemError> {
    self
      .ledger
      .take(increment)
      .map(|bytes| Reservation { bytes })
  }
}

/// A credit of inode-version slots reserved for one volume's logical inode allowance (§4.2). Held
/// in the volume's server slot and returned to the [`VersionBudget`] on teardown, so the reserved
/// slab capacity is accounted through create failure, destroy and resize exactly as a byte
/// [`Reservation`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionCredit {
  /// Version slots reserved.
  pub slots: u64,
}

/// The shard's inode-version slab reservation (§4.2 resource vector, inode dimension) — the counted
/// parallel of [`ShardBudget`]. Its capacity is the version slab (`store.max_inodes`); the committed
/// slots are the sum of every live volume's reserved logical inode allowance; the headroom is the
/// bounded copy-up coexistence — the transient inode version `Volume::make_current_inode` holds
/// after inserting the new version and before retiring the old.
///
/// That copy-up is one synchronous, exclusively-borrowed shard call (no `await` between the insert
/// and the retire), so at most one such transient exists at a time regardless of how many clients a
/// shard serves; the headroom is therefore the structural constant one, not a write-rate
/// measurement. This is the design's `operation_headroom` — "bounded temporary coexistence during
/// copy-up", with "measured *or structural* anchors" (§4.2). A create reserves its whole logical
/// allowance or is refused, and destroy releases it, so the sum of advertised inode allowances is
/// physically backed by the slab rather than merely capped against `SlabFull`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionBudget {
  ledger: Ledger,
}

impl VersionBudget {
  /// A budget over `slots` inode-version slab slots (the version-slab capacity), keeping `headroom`
  /// slots free for the transient copy-up version. The caller derives `headroom` from the copy-up's
  /// atomicity (the structural constant one); the budget only enforces it.
  pub const fn new(slots: u64, headroom: u64) -> Self {
    Self {
      ledger: Ledger::new(slots, headroom),
    }
  }

  /// The version-slab capacity the budget is over.
  pub const fn capacity(&self) -> u64 {
    self.ledger.reserve
  }

  /// Version slots committed to volumes (their reserved logical allowances).
  pub const fn committed(&self) -> u64 {
    self.ledger.committed
  }

  /// The copy-up headroom kept free of every reservation (§4.2).
  pub const fn headroom(&self) -> u64 {
    self.ledger.headroom
  }

  /// Version slots a further reservation may still take: capacity, less committed, less headroom.
  pub const fn admittable(&self) -> u64 {
    self.ledger.admittable()
  }

  /// Reserves `slots` for a volume's logical inode allowance, whole or not at all, keeping the
  /// copy-up headroom free — so the sum of reserved allowances can never exceed the backed slab.
  pub fn reserve(&mut self, slots: u64) -> Result<VersionCredit, MemError> {
    self.ledger.take(slots).map(|slots| VersionCredit { slots })
  }

  /// Returns a credit's slots to the version slab.
  pub fn release(&mut self, credit: VersionCredit) {
    self.ledger.give(credit.slots);
  }

  /// Charges `slots` of snapshot-retained inode versions against the *unpromised* capacity — the
  /// slab less what is reserved for volumes' logical allowances and the copy-up headroom (§4.2: "a
  /// new retained snapshot ... cannot use up a writer's promised future space"). Refused whole if no
  /// unpromised capacity remains, so a snapshot-and-diverge draws only from genuinely free slots and
  /// a bounded writer's reservation is never spent on another volume's retention. Unlike a
  /// reservation this is a running charge, not a held credit — the caller credits it back symmetrically
  /// as retained versions are freed.
  pub fn charge_retention(&mut self, slots: u64) -> Result<(), MemError> {
    self.ledger.take(slots).map(drop)
  }

  /// Returns `slots` of retained-version charge to the slab as retained versions are freed.
  pub fn credit_retention(&mut self, slots: u64) {
    self.ledger.give(slots);
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

/// The region size for a shard: its share of a memory `capacity` divided among the size classes. The
/// caller decides what the capacity is — the daemon's default reserve derives it from usable memory
/// (§4.2 D-12 honest degradation), a locked reserve from the OS lock capacity.
pub fn region_bytes(capacity: u64, shards: u64, classes: u64) -> Derived<u64> {
  derived!(
    capacity / shards.max(1) / classes.max(1),
    "memory capacity / shards / classes",
    ["mem.capacity", "rt.shards", "mem.classes"]
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

  /// The counted parallel of the byte reservation: the sum of reserved inode allowances can never
  /// exceed the backed slab, and a released credit returns its slots (§4.2 inode dimension).
  #[test]
  fn version_reservations_sum_to_at_most_the_backed_slab_and_release_returns_slots() {
    let mut v = VersionBudget::new(100, 1); // 100-slot version slab, one slot of copy-up headroom
    assert_eq!(
      v.admittable(),
      99,
      "one slot kept free for the transient copy-up"
    );
    let a = v.reserve(60).unwrap();
    assert_eq!(a.slots, 60);
    assert_eq!(
      v.admittable(),
      39,
      "60 reserved, the copy-up slot still free"
    );
    // A second volume cannot reserve more than the slab (less the reservation and the headroom) —
    // the disjoint reservation the bare per-volume cap does not give.
    match v.reserve(40) {
      Err(MemError::BudgetExceeded {
        requested,
        available,
      }) => {
        assert_eq!(requested, 40);
        assert_eq!(available, 39);
      }
      other => panic!("{other:?}"),
    }
    assert_eq!(v.committed(), 60, "a refused reservation changes nothing");
    v.release(a);
    assert_eq!(v.admittable(), 99, "release returns the credit's slots");
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
