//! Reconfiguration of a register's holder set (§4.8 "Configuration, by consensus"; D-14; the
//! `Reconfig` model, GAPS §10). The configuration group changes a register's candidate set — the
//! placement neighbourhood — while the owner keeps writing, a fresh holder replacing one that
//! restarted and left. This is Raft's joint consensus / Vertical Paxos I made concrete: the change
//! runs through three phases, and the transition never loses a committed record nor lets a read
//! return a stale one.
//!
//! - **Old**: a record commits at a majority of the old set.
//! - **Joint** (after `announce`): every write must reach a majority of the old set *and* a majority
//!   of the new set to commit, so the two configurations overlap and no majority of either can act
//!   without the other. A holder that left the old set never accepts again (the restart-identity
//!   rule: a restarted holder is a new member, starting empty); a fresh holder in the new set
//!   accepts from the announcement.
//! - **New** (after `retire`): a majority of the new set suffices and old-only holders are ignored.
//!   The old set is retired only after the owner has acknowledged the new configuration and the
//!   newest committed record is held by a majority of the new set (state transfer first), so the new
//!   set carries the latest state.
//!
//! This is a pure, deterministic port of the model (R8): the holders are in-memory state and each
//! action is a method, so the simulation is the protocol's proof. Its tests check exactly the two
//! properties the model verified — ReadSafety (every read majority, of whichever configuration a
//! reader knows, already holds a record at least as new as every committed record) and NoLoss
//! (every record committed under the joint rule at retirement stays committed under the new rule).
//! The register here is a single newest-wins value (a head, a chain version, a lease), the shape the
//! `Reconfig` model checks, distinct from the growing log of [`crate::ledger`].

use std::collections::{BTreeMap, BTreeSet};

use crate::register::HostId;

/// The phase of a reconfiguration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
  /// Before the change: commit at a majority of the old set.
  Old,
  /// The transition: commit at a majority of the old set and a majority of the new set.
  Joint,
  /// After the change: commit at a majority of the new set.
  New,
}

/// A register value: its sequence number (the newest wins) and the payload's identity. Sequence
/// zero is the absence of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rec {
  /// The sequence number; higher is newer.
  pub seq: u64,
  /// The payload's identity.
  pub identity: [u8; 32],
}

/// Why a retirement was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetireError {
  /// Retirement was attempted outside the joint phase.
  NotInJointPhase,
  /// The owner has not yet acknowledged the new configuration.
  OwnerNotAcked,
  /// The newest committed record is not yet held by a majority of the new set (state transfer must
  /// complete first).
  NewMajorityMissing,
}

/// A register undergoing a holder-set change from the old configuration to the new one.
#[derive(Clone, Debug)]
pub struct Reconfiguration {
  old: BTreeSet<HostId>,
  new: BTreeSet<HostId>,
  phase: Phase,
  stored: BTreeMap<HostId, Option<Rec>>,
  next_seq: u64,
  issued: BTreeMap<u64, [u8; 32]>,
  acks: BTreeMap<u64, BTreeSet<HostId>>,
  owner_acked: bool,
  retired_committed: BTreeSet<u64>,
}

/// Whether the acknowledging set holds a majority of `set` (`2 * |acked ∩ set| > |set|`).
fn majority_of(acked: &BTreeSet<HostId>, set: &BTreeSet<HostId>) -> bool {
  acked.intersection(set).count().saturating_mul(2) > set.len()
}

impl Reconfiguration {
  /// A register in the old phase whose holders are `old`, changing to `new`. Every acceptor in
  /// either set starts empty.
  pub fn new(old: &[HostId], new: &[HostId]) -> Reconfiguration {
    let old: BTreeSet<HostId> = old.iter().copied().collect();
    let new: BTreeSet<HostId> = new.iter().copied().collect();
    let mut stored = BTreeMap::new();
    for host in old.union(&new) {
      stored.insert(*host, None);
    }
    Reconfiguration {
      old,
      new,
      phase: Phase::Old,
      stored,
      next_seq: 1,
      issued: BTreeMap::new(),
      acks: BTreeMap::new(),
      owner_acked: false,
      retired_committed: BTreeSet::new(),
    }
  }

  /// The current phase.
  pub fn phase(&self) -> Phase {
    self.phase
  }

  /// The old set.
  pub fn old_set(&self) -> &BTreeSet<HostId> {
    &self.old
  }

  /// The new set.
  pub fn new_set(&self) -> &BTreeSet<HostId> {
    &self.new
  }

  /// The sequence of the newest record host `host` holds, or zero when it holds none. Lets a caller
  /// compute the value a read majority would return.
  pub fn stored_seq(&self, host: HostId) -> u64 {
    self
      .stored
      .get(&host)
      .and_then(|slot| slot.map(|rec| rec.seq))
      .unwrap_or(0)
  }

  /// The sequence numbers committed under the current phase's rule.
  pub fn committed_seqs(&self) -> BTreeSet<u64> {
    self
      .issued
      .keys()
      .copied()
      .filter(|seq| self.is_committed(*seq))
      .collect()
  }

  /// The records committed under the joint rule at the moment of retirement (empty until retired).
  pub fn retired_committed(&self) -> &BTreeSet<u64> {
    &self.retired_committed
  }

  /// Whether a holder may accept in the current phase: old-set holders in the old phase, both sets
  /// in the joint phase, new-set holders in the new phase. A holder that left never accepts again.
  fn active(&self, host: HostId) -> bool {
    match self.phase {
      Phase::Old => self.old.contains(&host),
      Phase::Joint => self.old.contains(&host) || self.new.contains(&host),
      Phase::New => self.new.contains(&host),
    }
  }

  /// The acknowledging set for `seq` (empty when none).
  fn acked(&self, seq: u64) -> BTreeSet<HostId> {
    self.acks.get(&seq).cloned().unwrap_or_default()
  }

  /// Whether `seq` is committed under the current phase's majority rule.
  fn is_committed(&self, seq: u64) -> bool {
    let acked = self.acked(seq);
    match self.phase {
      Phase::Old => majority_of(&acked, &self.old),
      Phase::Joint => majority_of(&acked, &self.old) && majority_of(&acked, &self.new),
      Phase::New => majority_of(&acked, &self.new),
    }
  }

  /// The newest committed record, or `None` when nothing is committed.
  fn newest_committed(&self) -> Option<Rec> {
    self
      .committed_seqs()
      .into_iter()
      .next_back()
      .and_then(|seq| {
        self.issued.get(&seq).map(|identity| Rec {
          seq,
          identity: *identity,
        })
      })
  }

  /// The owner issues a record carrying `identity` at the next sequence, and returns that sequence.
  pub fn issue(&mut self, identity: [u8; 32]) -> u64 {
    let seq = self.next_seq;
    self.issued.insert(seq, identity);
    self.acks.entry(seq).or_default();
    self.next_seq = self.next_seq.saturating_add(1);
    seq
  }

  /// A holder accepts the record at `seq`: it stores it when newer than what it holds and records
  /// its acknowledgement. Ignored when the record was never issued, the holder is not active, or it
  /// already acknowledged. Returns whether it accepted.
  pub fn accept(&mut self, seq: u64, host: HostId) -> bool {
    let Some(identity) = self.issued.get(&seq).copied() else {
      return false;
    };
    if !self.active(host) || self.acked(seq).contains(&host) {
      return false;
    }
    let slot = self.stored.entry(host).or_insert(None);
    let newer = slot.map(|rec| seq > rec.seq).unwrap_or(true);
    if newer {
      *slot = Some(Rec { seq, identity });
    }
    self.acks.entry(seq).or_default().insert(host);
    true
  }

  /// The configuration master announces the change; the owner enters the joint phase. Returns
  /// whether it moved (only from the old phase).
  pub fn announce(&mut self) -> bool {
    if self.phase == Phase::Old {
      self.phase = Phase::Joint;
      true
    } else {
      false
    }
  }

  /// The owner acknowledges the new configuration (a precondition of retirement). Returns whether
  /// it moved.
  pub fn owner_ack(&mut self) -> bool {
    if self.phase == Phase::Joint && !self.owner_acked {
      self.owner_acked = true;
      true
    } else {
      false
    }
  }

  /// State transfer: the owner copies the newest committed record into a new-set holder that lags
  /// it, so the new set carries the latest state before retirement. Returns whether it moved.
  pub fn transfer(&mut self, host: HostId) -> bool {
    if self.phase != Phase::Joint || !self.new.contains(&host) {
      return false;
    }
    let Some(newest) = self.newest_committed() else {
      return false;
    };
    let slot = self.stored.entry(host).or_insert(None);
    if slot.map(|rec| newest.seq > rec.seq).unwrap_or(true) {
      *slot = Some(newest);
      self.acks.entry(newest.seq).or_default().insert(host);
      true
    } else {
      false
    }
  }

  /// The master retires the old set, moving to the new phase. Refused outside the joint phase,
  /// before the owner acknowledged the change, or before the newest committed record is held by a
  /// majority of the new set. Snapshots the joint-committed records as `retired_committed`.
  pub fn retire(&mut self) -> Result<(), RetireError> {
    if self.phase != Phase::Joint {
      return Err(RetireError::NotInJointPhase);
    }
    if !self.owner_acked {
      return Err(RetireError::OwnerNotAcked);
    }
    if let Some(newest) = self.newest_committed()
      && !majority_of(&self.acked(newest.seq), &self.new)
    {
      return Err(RetireError::NewMajorityMissing);
    }
    self.retired_committed = self.committed_seqs();
    self.phase = Phase::New;
    Ok(())
  }
}
