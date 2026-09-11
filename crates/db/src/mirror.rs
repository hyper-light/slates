//! Asynchronous mirroring to a second region (§4.8 "Mirroring"; D-18). A volume's committed
//! records and their content reach a mirror region asynchronously, in epoch order, with a measured,
//! exposed lag; an operation that needs them there awaits the mirror scope (`await placed(mirror)`,
//! [`crate::register::DurabilityScope::Mirror`]). This module is that shipper, built entirely on the
//! fenced ledger register of [`crate::ledger`]: the mirror region is a second [`Cohort`] with its
//! own writer, and mirroring replays the home region's committed prefix onto it in position order —
//! which is epoch order, because the committed prefix commits position by position and its epochs
//! never decrease.
//!
//! It is pure and deterministic (R8): the mirror's holders are in memory and shipping is a direct
//! call, so the same code runs on one machine (the mirror region's own `f`) and across regions,
//! with no network and no clock. The lag is exposed as a count of records committed at home but not
//! yet at the mirror; `await placed(mirror)` for a record is true once the mirror has committed
//! through its position. A laptop has no mirror region ([`crate::register::Configuration`] refuses
//! the scope), so this type is instantiated only where one exists.
//!
//! The mirror is downstream, never a second source of truth: it only ever adopts the home region's
//! committed records, in order, so it can lag but never diverge — a partition of the mirror's own
//! holders stalls its lag without ever committing a record the home did not.

use crate::ledger::{Cohort, Owner, Reach};
use crate::register::{DomainId, HostId, ObjectId, Quorum};

/// A volume's mirror region: a cohort of the mirror's candidate holders and the writer that replays
/// the home region's committed records onto them in epoch order.
#[derive(Clone, Debug)]
pub struct Mirror {
  cohort: Cohort,
  writer: Owner,
}

impl Mirror {
  /// The mirror region for `object`: its own `2f + 1` candidate holders drawn from the mirror
  /// region's neighbourhood, with an empty log. The `quorum` is the mirror region's fault-domain
  /// tree, independent of the home region's.
  pub fn new(
    owner: HostId,
    neighbourhood: &[HostId],
    domains: &std::collections::BTreeMap<HostId, DomainId>,
    object: ObjectId,
    quorum: Quorum,
  ) -> Mirror {
    let cohort = Cohort::new(owner, neighbourhood, domains, object, quorum);
    let writer = Owner::bootstrap(&cohort);
    Mirror { cohort, writer }
  }

  /// The mirror region's candidate holders, owner first.
  pub fn candidates(&self) -> &[HostId] {
    self.cohort.candidates()
  }

  /// The number of records committed on the mirror.
  pub fn committed_len(&self) -> usize {
    self.cohort.committed_prefix().len()
  }

  /// The mirror lag against a home region whose committed prefix is `home_committed`: the records
  /// committed at home but not yet at the mirror. Zero when the mirror has caught up. The exposed
  /// lag of D-18 (a count here; a time or an epoch distance once the runtime carries clocks).
  pub fn lag(&self, home_committed: &[[u8; 32]]) -> usize {
    home_committed.len().saturating_sub(self.committed_len())
  }

  /// Whether the mirror has committed through `position` — `await placed(mirror)` for the record at
  /// that position. False while the record has not yet been shipped and committed on the mirror.
  pub fn placed_through(&self, position: usize) -> bool {
    self.committed_len() > position
  }

  /// Ships the home region's committed records that the mirror lacks, in order, to the reachable
  /// mirror holders, committing each at the mirror's own `f + 1`. Returns the number of records
  /// newly committed on the mirror. Replaying the home's committed prefix is epoch order, so the
  /// mirror commits a dense prefix and never a record before an earlier one; when the mirror's own
  /// holders are partitioned below their quorum nothing new commits (the lag holds), and a later
  /// ship after the partition heals catches the mirror up — the replay is idempotent and resumable.
  pub fn ship(&mut self, home_committed: &[[u8; 32]], reachable: &Reach) -> usize {
    let before = self.committed_len();
    let after = self
      .writer
      .replicate(&mut self.cohort, home_committed, reachable);
    after.saturating_sub(before)
  }
}
