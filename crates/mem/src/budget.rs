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
  /// A pressure hold (§4.2, admission.md §5.5): capacity the control shard withholds from new
  /// admission under real memory pressure, sampled from the host and set through [`ShardBudget::
  /// set_hold`]. It never reaches below `committed` — it only shrinks `admittable`, so a raised hold
  /// refuses a new reservation but touches no admitted claim (an admitted volume's within-entitlement
  /// writes use their own committed reservation). Released as the pressure sample recovers.
  hold: u64,
}

impl Ledger {
  const fn new(reserve: u64, headroom: u64) -> Self {
    Self {
      reserve,
      committed: 0,
      headroom,
      hold: 0,
    }
  }

  const fn admittable(&self) -> u64 {
    self
      .reserve
      .saturating_sub(self.committed)
      .saturating_sub(self.headroom)
      .saturating_sub(self.hold)
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

  /// Commits `amount` whole or refuses, allowed into the headroom (never into the pressure hold): for a holder the
  /// headroom is kept for, which every other admission leaves free.
  fn take_into_headroom(&mut self, amount: u64) -> Result<u64, MemError> {
    let available = self.admittable().saturating_add(
      self.headroom.min(
        self
          .reserve
          .saturating_sub(self.committed)
          .saturating_sub(self.hold),
      ),
    );
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
/// capacity — the usable, prepared arena; the committed bytes are the entitlement handed to volumes
/// plus the bytes their snapshots retain; and the headroom is the operation headroom kept free for
/// the bounded temporary coexistence of in-flight operations (a copy-up holds a source chunk and its
/// new extent at once). Every admission — a bounded reservation, a dynamic growth or a retention
/// charge alike — leaves the headroom free and takes only from capacity not already committed, so
/// growth and retention consume only unpromised space and a burst never meets exhaustion. This is
/// the one capacity owner for content bytes: control-path reservations, dynamic growth and the
/// retained bytes of snapshots all go through it, with no second, looser test against raw free
/// memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBudget {
  ledger: Ledger,
  /// The part of `committed` that is snapshot-retained content (§4.2 retention, the byte
  /// dimension): chunks the heads have let go of that their snapshots still pin, charged at their
  /// arena block length. Reported beside the reservations so the status distinguishes promised
  /// entitlement from bytes held on behalf of snapshots.
  retained: u64,
  /// The part of `committed` that is content this shard holds as a **candidate holder for other owners**
  /// (§4.2 "a remote holder makes the same admission against its own machine before acknowledging
  /// placement"; AUD-29-43): replicated chunks and manifests, charged at their arena block length, plus the
  /// transient scratch a put verifies in. Like `retained`, a running charge taken only from unpromised
  /// capacity, so another owner's content never spends an admitted volume's entitlement.
  replicated: u64,
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
      retained: 0,
      replicated: 0,
    }
  }

  /// The effective capacity — the usable, prepared arena the budget is over.
  pub const fn capacity(&self) -> u64 {
    self.ledger.reserve
  }

  /// Bytes committed: every volume's entitlement (reservations and admitted growth) plus the bytes
  /// their snapshots retain.
  pub const fn committed(&self) -> u64 {
    self.ledger.committed
  }

  /// The snapshot-retained part of the committed bytes (§4.2 retention).
  pub const fn retained(&self) -> u64 {
    self.retained
  }

  /// The part of the committed bytes held for other owners as their content's candidate holder
  /// (AUD-29-43).
  pub const fn replicated(&self) -> u64 {
    self.replicated
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

  /// The pressure hold now (§4.2): capacity withheld from new admission under memory pressure.
  pub const fn hold(&self) -> u64 {
    self.ledger.hold
  }

  /// Sets the pressure hold — capacity the control shard withholds from new admission while the host
  /// is under memory pressure (§4.2; admission.md §5.5). It shrinks `admittable` only, never revoking
  /// a committed claim: a within-entitlement write on an admitted volume uses its own reservation and
  /// is unaffected, while a new reservation is refused `BudgetExceeded` until the hold is released as
  /// the pressure sample recovers.
  pub fn set_hold(&mut self, bytes: u64) {
    self.ledger.hold = bytes;
  }

  /// The capacity a commitment of `bytes` more lacks: what the arena must grow by before it is admittable (A-98).
  /// Zero when it is admittable now. The pressure hold counts: it is an amount withheld from admission (the host's
  /// shortfall, §4.2), so a shard that claims lazily must hold it beyond its commitments for the admission ceiling to
  /// be what it was over a whole slice, held capacity less the hold. Claiming maps no RAM, and the hold keeps the
  /// claimed bytes from being admitted, so nothing is written into them.
  pub const fn deficit(&self, bytes: u64) -> u64 {
    self
      .ledger
      .committed
      .saturating_add(self.ledger.headroom)
      .saturating_add(self.ledger.hold)
      .saturating_add(bytes)
      .saturating_sub(self.ledger.reserve)
  }

  /// Raises the capacity by `bytes` the arena added (A-98: an extent claimed from the pool).
  pub fn extend(&mut self, bytes: u64) {
    self.ledger.reserve = self.ledger.reserve.saturating_add(bytes);
  }

  /// Lowers the capacity by `bytes` the arena is returning (A-98), if what remains still covers every commitment
  /// and the operation headroom: whether it did. A refusal changes nothing.
  pub fn retract(&mut self, bytes: u64) -> bool {
    let floor = self.ledger.committed.saturating_add(self.ledger.headroom);
    match self.ledger.reserve.checked_sub(bytes) {
      Some(left) if left >= floor => {
        self.ledger.reserve = left;
        true
      }
      _ => false,
    }
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

  /// Charges `bytes` of snapshot-retained content against the *unpromised* capacity — the arena less
  /// every reservation, every admitted growth and the operation headroom (§4.2: "a new retained
  /// snapshot ... cannot use up a writer's promised future space"). Refused whole, with nothing
  /// changed, if no unpromised capacity remains, so a volume's snapshots never spend a neighbour's
  /// entitlement. A running charge, not a held credit: the owner credits it back symmetrically as
  /// retained chunks are freed.
  pub fn charge_retention(&mut self, bytes: u64) -> Result<(), MemError> {
    let taken = self.ledger.take(bytes)?;
    self.retained = self.retained.saturating_add(taken);
    Ok(())
  }

  /// Returns `bytes` of retention charge as retained chunks are freed.
  pub fn credit_retention(&mut self, bytes: u64) {
    self.ledger.give(bytes);
    self.retained = self.retained.saturating_sub(bytes);
  }

  /// Charges `bytes` of content held for other owners (AUD-29-43) against the unpromised capacity, whole or
  /// refused with nothing changed — the same rule as [`charge_retention`](Self::charge_retention), so a
  /// holder's replicas never spend an admitted volume's promised space or the operation headroom.
  pub fn charge_replicated(&mut self, bytes: u64) -> Result<(), MemError> {
    let taken = self.ledger.take(bytes)?;
    self.replicated = self.replicated.saturating_add(taken);
    Ok(())
  }

  /// Returns `bytes` of replicated charge as held content is released.
  pub fn credit_replicated(&mut self, bytes: u64) {
    self.ledger.give(bytes);
    self.replicated = self.replicated.saturating_sub(bytes);
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
  /// The part of `committed` that is snapshot-retained inode versions (§4.2 retention, the inode
  /// dimension), reported beside the reserved allowances.
  retained: u64,
}

impl VersionBudget {
  /// A budget over `slots` inode-version slab slots (the version-slab capacity), keeping `headroom`
  /// slots free for the transient copy-up version. The caller derives `headroom` from the copy-up's
  /// atomicity (the structural constant one); the budget only enforces it.
  pub const fn new(slots: u64, headroom: u64) -> Self {
    Self {
      ledger: Ledger::new(slots, headroom),
      retained: 0,
    }
  }

  /// The version-slab capacity the budget is over.
  pub const fn capacity(&self) -> u64 {
    self.ledger.reserve
  }

  /// Version slots committed: every volume's reserved logical allowance plus the versions their
  /// snapshots retain.
  pub const fn committed(&self) -> u64 {
    self.ledger.committed
  }

  /// The snapshot-retained part of the committed slots (§4.2 retention).
  pub const fn retained(&self) -> u64 {
    self.retained
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
    let taken = self.ledger.take(slots)?;
    self.retained = self.retained.saturating_add(taken);
    Ok(())
  }

  /// Returns `slots` of retained-version charge to the slab as retained versions are freed.
  pub fn credit_retention(&mut self, slots: u64) {
    self.ledger.give(slots);
    self.retained = self.retained.saturating_sub(slots);
  }
}

/// A credit of metadata bytes reserved for one volume's records (§4.2 metadata dimension): its
/// journal's retention budget and its own record. Held in the volume's server slot and returned to
/// the [`MetadataBudget`] on teardown, exactly as a byte [`Reservation`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataCredit {
  /// Bytes reserved.
  pub bytes: u64,
}

/// The shard's metadata ledger (§4.2 resource vector, the metadata dimension): the part of the
/// shard's metadata class that its slabs cannot take — the class less every slab's maximum
/// footprint — from which each volume's records (its journal budget, its volume object, its
/// snapshot slab's first segment) are reserved at admission, whole or not at all. So the sum of
/// every volume's metadata is bounded by the class rather than by the heap: "an uncharged heap
/// allocation cannot sit outside the bound" (§4.2). Unbounded (`u64::MAX`) until the admitting
/// owner sets the class, so fixtures and non-admitting callers are unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataBudget {
  ledger: Ledger,
}

impl MetadataBudget {
  /// A ledger over `bytes` of metadata capacity, with no headroom: the slabs' own headroom is
  /// inside their maximum footprint, already subtracted from the class.
  pub const fn new(bytes: u64) -> Self {
    Self {
      ledger: Ledger::new(bytes, 0),
    }
  }

  /// The metadata bytes the ledger is over.
  pub const fn capacity(&self) -> u64 {
    self.ledger.reserve
  }

  /// Bytes reserved for volumes' records.
  pub const fn committed(&self) -> u64 {
    self.ledger.committed
  }

  /// Bytes a further reservation may still take.
  pub const fn admittable(&self) -> u64 {
    self.ledger.admittable()
  }

  /// Keeps `bytes` of the ledger as observation room (§4.14): room no volume's records may take, so the status report
  /// that explains a full ledger can always be held — one capture's bound, a client ring's status snapshot capacity.
  /// Refused when the room would exceed the ledger, or is set after records were reserved.
  pub fn set_observation_room(&mut self, bytes: u64) -> Result<(), MemError> {
    if self.ledger.committed != 0 || bytes > self.ledger.reserve {
      return Err(MemError::BudgetExceeded {
        requested: bytes,
        available: self.ledger.reserve.saturating_sub(self.ledger.committed),
      });
    }
    self.ledger.headroom = bytes;
    Ok(())
  }

  /// Bytes a status capture may still take: the admittable room plus what is left of the observation room.
  pub fn observation_admittable(&self) -> u64 {
    let ledger = &self.ledger;
    ledger.admittable().saturating_add(
      ledger.headroom.min(
        ledger
          .reserve
          .saturating_sub(ledger.committed)
          .saturating_sub(ledger.hold),
      ),
    )
  }

  /// Reserves `bytes` for a status capture (§4.14), whole or not at all, from the admittable room and then the
  /// observation room no volume may take ([`MetadataBudget::set_observation_room`]).
  pub fn reserve_observation(&mut self, bytes: u64) -> Result<MetadataCredit, MemError> {
    self
      .ledger
      .take_into_headroom(bytes)
      .map(|bytes| MetadataCredit { bytes })
  }

  /// Reserves `bytes` for one volume's records, whole or not at all.
  pub fn reserve(&mut self, bytes: u64) -> Result<MetadataCredit, MemError> {
    self
      .ledger
      .take(bytes)
      .map(|bytes| MetadataCredit { bytes })
  }

  /// Returns a credit's bytes to the ledger.
  pub fn release(&mut self, credit: MetadataCredit) {
    self.ledger.give(credit.bytes);
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

  /// A-98: do ask a budget with no capacity for the deficit of a reservation, extend it by that, reserve, then retract
  /// below and above what the commitments need; expect the deficit to cover the request, the headroom and the hold, the
  /// reservation to fit exactly after the extension, a retraction that would uncover a commitment refused with nothing
  /// changed, and one that leaves the commitments covered accepted.
  #[test]
  fn a_budget_extends_by_its_deficit_and_retracts_only_above_its_commitments() {
    let mut b = ShardBudget::new(0, 10);
    b.set_hold(5);
    assert_eq!(
      b.deficit(100),
      115,
      "the request, the headroom and the hold"
    );
    b.extend(115);
    assert_eq!(b.deficit(100), 0);
    let r = b.reserve(100).unwrap();
    b.set_hold(0);
    assert!(
      !b.retract(6),
      "110 would remain, under 100 committed + 10 headroom"
    );
    assert_eq!(b.capacity(), 115);
    assert!(b.retract(5));
    assert_eq!(b.capacity(), 110);
    b.release(r);
    assert!(b.retract(100));
    assert_eq!(b.capacity(), 10);
  }

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

  /// The pressure hold withholds capacity from new admission without touching an admitted claim
  /// (§4.2; admission.md §5.5): a raised hold shrinks `admittable` and refuses a new reservation, an
  /// already-committed reservation is untouched (its bytes stay committed), and lowering the hold
  /// releases the capacity for admission again.
  #[test]
  fn replicated_content_takes_only_unpromised_capacity_and_is_refused_whole_past_it() {
    // AUD-29-43: do: reserve an admitted volume's claim, charge replicated content up to the unpromised
    // capacity, then one byte more, then credit it back; expect the charge to stop exactly at the
    // unpromised capacity, the refusal to change nothing, the volume's claim untouched throughout, and the
    // credit to restore admission.
    let mut b = ShardBudget::new(100, 20);
    let claim = b.reserve(50).unwrap();
    assert_eq!(b.admittable(), 30);
    b.charge_replicated(30).unwrap();
    assert_eq!((b.replicated(), b.committed(), b.admittable()), (30, 80, 0));
    assert!(matches!(
      b.charge_replicated(1),
      Err(MemError::BudgetExceeded {
        requested: 1,
        available: 0
      })
    ));
    assert_eq!(
      (b.replicated(), b.committed()),
      (30, 80),
      "a refusal changes nothing"
    );
    assert_eq!(claim.bytes, 50);
    b.credit_replicated(30);
    assert_eq!((b.replicated(), b.committed(), b.admittable()), (0, 50, 30));
  }

  #[test]
  fn a_pressure_hold_withholds_admission_without_touching_a_committed_claim() {
    let mut b = ShardBudget::new(100, 20);
    let sacred = b.reserve(50).unwrap(); // an admitted volume's committed 50
    assert_eq!(b.admittable(), 30, "100 − 50 committed − 20 headroom");

    // Under pressure the control shard holds 25 of the remaining admittable.
    b.set_hold(25);
    assert_eq!(b.hold(), 25);
    assert_eq!(b.admittable(), 5, "the hold shrinks admittable, 30 − 25");
    assert_eq!(b.committed(), 50, "the hold touches no committed claim");
    assert!(
      matches!(
        b.reserve(10),
        Err(MemError::BudgetExceeded { available: 5, .. })
      ),
      "a new reservation past the held admittable is refused"
    );
    // The admitted volume keeps its reservation; a growth within its committed bytes is its own,
    // not a new admission, so it is unaffected (the reservation stands).
    assert_eq!(sacred.bytes, 50);

    // As the sample recovers the hold releases and admission resumes.
    b.set_hold(0);
    assert_eq!(b.admittable(), 30, "the released hold restores admittable");
    assert_eq!(b.reserve(10).unwrap().bytes, 10, "admission resumes");
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

  /// The byte dimension's retention charge draws only from unpromised capacity — never from a
  /// reservation or the headroom — is refused whole at the boundary, and is credited back as the
  /// retained chunks go, with the retained sub-account tracking exactly the retention part of the
  /// committed bytes (§4.2 "a new retained snapshot ... cannot use up a writer's promised future
  /// space").
  #[test]
  fn retained_bytes_are_charged_from_unpromised_capacity_and_credited_back() {
    let mut b = ShardBudget::new(100, 10);
    let sacred = b.reserve(60).unwrap(); // a bounded volume's promise
    assert_eq!(b.admittable(), 30, "100 − 60 reserved − 10 headroom");
    b.charge_retention(30).unwrap();
    assert_eq!(b.retained(), 30);
    assert_eq!(
      b.committed(),
      90,
      "the reservation and the retention are both committed"
    );
    // The promise and the headroom are not for retention: refused whole, offering nothing.
    assert!(matches!(
      b.charge_retention(1),
      Err(MemError::BudgetExceeded {
        requested: 1,
        available: 0
      })
    ));
    assert_eq!(b.retained(), 30, "a refused charge changes nothing");
    b.credit_retention(30);
    assert_eq!(b.retained(), 0);
    assert_eq!(b.committed(), 60, "only the reservation remains");
    b.release(sacred);
    assert_eq!(b.committed(), 0);
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

#[cfg(test)]
mod observation_tests {
  use super::MetadataBudget;

  /// §4.14: do fill a metadata ledger with records, then hold a status capture as large as the observation room;
  /// expect the records refused at the room's edge, the capture admitted inside it, a second capture refused, and the
  /// room back once the capture is released.
  #[test]
  fn the_observation_room_holds_a_status_capture_no_record_can_take() {
    let mut budget = MetadataBudget::new(1000);
    budget.set_observation_room(100).unwrap();
    let records = budget.reserve(900).unwrap();
    assert!(budget.reserve(1).is_err(), "records never take the room");
    let capture = budget.reserve_observation(100).unwrap();
    assert!(budget.reserve_observation(1).is_err(), "one capture's room");
    budget.release(capture);
    assert_eq!(budget.observation_admittable(), 100);
    budget.release(records);
    assert!(
      budget.set_observation_room(2000).is_err(),
      "larger than the ledger"
    );
  }
}
