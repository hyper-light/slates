//! Recovery and the snapshot policy (§4.8 "Recovery": "the anchor segment replays the local
//! log ... into fresh indexes"; Phase 2 task 2: "snapshots of partition state into the segment
//! on a cadence derived from measured replay throughput and the recovery budget").
//!
//! [`Db`] is one partition's writer: `mutate` runs the guard, appends the record, applies it,
//! and when the bytes appended since the last snapshot would take longer to replay than the
//! recovery budget, publishes the partition into the alternate snapshot slot and trims the
//! log behind it. [`recover`] is the other direction: the newest valid snapshot slot, then the
//! records after its sequence, with the torn tail cut off and the replay throughput measured
//! (the policy's anchor).

use std::time::Instant;

use slates_anchor::{AnchorError, AnchorSegment, RegionKind};
use slates_machine::{Derived, derived};
use slates_wire::Wire;

use crate::error::DbError;
use crate::op::Op;
use crate::partition::{Partition, PartitionCaps, PartitionSnapshot};
use crate::record::{LogEntry, LogRing, Replayed};

/// Shape: the recovery budget, RAMCloud's target of about a second for a crashed node
/// [A: Ongaro et al., SOSP'11: 35 GB in 1.6 s]; an operator input in Phase 2's CLI, ratified
/// here as the default (GAPS §5).
pub const RECOVERY_BUDGET_NS: u64 = 1_000_000_000;
/// Format: nanoseconds per microsecond.
const NS_PER_US: u64 = 1_000;

/// When to snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotPolicy {
  /// Snapshot once this many log bytes were appended since the last one.
  pub bytes_between_snapshots: u64,
}

impl SnapshotPolicy {
  /// Derived: the bytes the measured replay throughput covers inside the recovery budget; a
  /// replay of the log tail then never exceeds the budget. Unknown throughput (no replay yet)
  /// means one byte per microsecond, so the first snapshot comes early and measures.
  pub fn derive(recovery_budget_ns: u64, replay_bytes_per_us: u64) -> Derived<SnapshotPolicy> {
    let budget_us = recovery_budget_ns / NS_PER_US;
    derived!(
      SnapshotPolicy {
        bytes_between_snapshots: budget_us.saturating_mul(replay_bytes_per_us.max(1)),
      },
      "bytes_between_snapshots = (recovery_budget_ns / 1000) * replay_bytes_per_us",
      ["recovery_budget_ns", "replay_bytes_per_us"]
    )
  }

  /// Derived at a snapshot of `snapshot_bytes` (A-243): the log bytes the measured apply throughput covers inside the
  /// recovery budget, and never fewer than the snapshot's own size. The first term is the boot policy's, measured on
  /// the apply every commit runs (the work a replay repeats) rather than only at a boot that may have had nothing to
  /// replay; the second is the doubling rule, so a snapshot is taken only once the log has absorbed what it costs and
  /// snapshot work stays within about twice the log written, whatever the partition's size. Recovery is then bounded by
  /// decoding the snapshot and replaying at most `max(budget bytes, snapshot_bytes)` of log: by the budget while the
  /// partition fits in it, by about twice the snapshot's own cost once it outgrows it.
  pub fn at_snapshot(
    recovery_budget_ns: u64,
    apply_bytes_per_us: u64,
    snapshot_bytes: u64,
  ) -> Derived<SnapshotPolicy> {
    let budget_us = recovery_budget_ns / NS_PER_US;
    derived!(
      SnapshotPolicy {
        bytes_between_snapshots: budget_us
          .saturating_mul(apply_bytes_per_us.max(1))
          .max(snapshot_bytes),
      },
      "bytes_between_snapshots = max((recovery_budget_ns / 1000) * apply_bytes_per_us, snapshot_bytes)",
      ["recovery_budget_ns", "apply_bytes_per_us", "snapshot_bytes"]
    )
  }
}

/// A partition database's running figures (A-243): what its snapshots cost against the log they absorb, and the
/// cadence they follow. Surfaced per shard in `slates status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DbStats {
  /// Snapshots taken since recovery.
  pub snapshots: u64,
  /// The bytes they wrote.
  pub snapshot_bytes_written: u64,
  /// The longest one took, encode and publish, nanoseconds: it runs on the committing shard's turn, so a late beat
  /// can be matched to it.
  pub snapshot_ns_max: u64,
  /// The log bytes appended since recovery.
  pub log_bytes_appended: u64,
  /// The cadence now: log bytes between snapshots.
  pub bytes_between_snapshots: u64,
  /// The apply throughput measured since the last snapshot, bytes a microsecond (zero until measured).
  pub apply_bytes_per_us: u64,
}

/// What recovery found.
#[derive(Debug, Clone)]
pub struct Recovered {
  /// The snapshot slot restored, and its sequence.
  pub snapshot: Option<(u8, u64)>,
  /// Records replayed after it.
  pub replayed_records: u64,
  /// Bytes replayed.
  pub replayed_bytes: u64,
  /// The replay's wall time.
  pub replay_ns: u64,
  /// Whether a torn tail was cut off.
  pub torn: bool,
  /// The next sequence.
  pub next_seq: u64,
}

/// The snapshot slot after `slot`: the two slots alternate, so a torn snapshot write never touches the last good one.
fn other_slot<T: Into<u64> + TryFrom<u64> + Default>(slot: T) -> T {
  T::try_from(slot.into().saturating_add(1).checked_rem(2).unwrap_or(0)).unwrap_or_default()
}

impl Recovered {
  /// Measured: the replay throughput in bytes per microsecond (zero when nothing replayed).
  pub fn replay_bytes_per_us(&self) -> u64 {
    self
      .replayed_bytes
      .saturating_mul(NS_PER_US)
      .checked_div(self.replay_ns)
      .unwrap_or(0)
  }
}

/// One partition's database: the partition, its log and its snapshot cadence.
pub struct Db {
  partition: Partition,
  index: u16,
  log: LogRing,
  next_seq: u64,
  policy: SnapshotPolicy,
  /// Whether `policy` was set by the caller ([`Db::set_policy`]) and stays, rather than re-derived at every snapshot
  /// ([`SnapshotPolicy::after_snapshot`]).
  policy_pinned: bool,
  since_snapshot_bytes: u64,
  next_slot: u8,
  snapshots_taken: u64,
  /// The running figures ([`DbStats`]).
  stats: DbStats,
  /// The time the partition's apply took since the last snapshot, over the log bytes since it
  /// (`since_snapshot_bytes`): the live apply throughput the cadence is derived from.
  apply_ns_since_snapshot: u64,
  /// The replay throughput recovery measured, bytes a microsecond (zero when it replayed nothing): the cadence's rate
  /// until a commit has measured its own.
  boot_bytes_per_us: u64,
  /// Operations applied since `begin`, waiting to go into one record at `commit`.
  pending: Option<Vec<Op>>,
  /// The clock the last mutation ran at: the time a partition re-derived on a rolled-back
  /// publication is restored at (its lease wheel starts there).
  last_now_ns: u64,
  /// Transactions rolled back because their record could not be made durable (AUD-06).
  rollbacks: u64,
  /// Maintenance snapshots that failed **after** a durable append and were deferred to the next
  /// commit — the record stood; only the log's trimming waited.
  maintenance_failures: u64,
  /// Test support (never reachable from the wire): the next publication fails as named, once.
  publication_fault: Option<PublicationFault>,
}

/// Test support: where the next publication is made to fail, once (`Db::inject_publication_fault`),
/// so the two failure classes AC-2.3 distinguishes can each be driven deterministically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublicationFault {
  /// The record's append is refused before anything is written: the transaction must roll back.
  BeforeAppend,
  /// The append succeeds; the maintenance snapshot the policy then asks for fails: the commit
  /// stands and the snapshot is deferred.
  AfterAppend,
}

impl std::fmt::Debug for Db {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Db")
      .field("index", &self.index)
      .field("next_seq", &self.next_seq)
      .field("policy", &self.policy)
      .finish()
  }
}

/// The newest valid snapshot slot of a partition.
fn newest_snapshot(
  segment: &AnchorSegment,
  index: u16,
) -> Result<Option<(u8, PartitionSnapshot)>, DbError> {
  let mut best: Option<(u8, PartitionSnapshot)> = None;
  for slot in 0..2u8 {
    let Ok(Some(bytes)) = segment.read_published(RegionKind::Snapshot(index, slot)) else {
      continue;
    };
    let Ok(snapshot) = PartitionSnapshot::from_bytes(&bytes) else {
      continue;
    };
    if best.as_ref().is_none_or(|(_, b)| snapshot.seq > b.seq) {
      best = Some((slot, snapshot));
    }
  }
  Ok(best)
}

/// Recovers partition `index` from the segment: the newest valid snapshot, then the records
/// after it; the torn tail is cut; the replay is timed.
pub fn recover(
  segment: &mut AnchorSegment,
  index: u16,
  caps: PartitionCaps,
  now_ns: u64,
) -> Result<(Db, Recovered), DbError> {
  let started = Instant::now();
  let log = LogRing::new(RegionKind::Log(index));
  let DerivedPartition {
    partition,
    snapshot,
    from_seq,
    replayed,
    records,
  } = derive_partition(segment, index, &log, caps, now_ns)?;
  if replayed.torn {
    log.truncate_to_verified(segment, replayed.verified_end)?;
  }
  let replay_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
  let recovered = Recovered {
    snapshot,
    replayed_records: records,
    replayed_bytes: replayed.bytes,
    replay_ns,
    torn: replayed.torn,
    next_seq: replayed.next_seq.max(from_seq),
  };
  let policy = SnapshotPolicy::derive(RECOVERY_BUDGET_NS, recovered.replay_bytes_per_us()).get();
  let db = Db {
    partition,
    index,
    log,
    next_seq: recovered.next_seq,
    policy,
    policy_pinned: false,
    since_snapshot_bytes: replayed.bytes,
    next_slot: snapshot.map_or(0, |(slot, _)| other_slot(slot)),
    snapshots_taken: 0,
    stats: DbStats::default(),
    apply_ns_since_snapshot: 0,
    boot_bytes_per_us: recovered.replay_bytes_per_us(),
    pending: None,
    last_now_ns: now_ns,
    rollbacks: 0,
    maintenance_failures: 0,
    publication_fault: None,
  };
  Ok((db, recovered))
}

/// A partition derived from the segment's durable state: the newest valid snapshot, then the
/// records after it, replayed in order — what [`recover`] builds at boot, and what a rolled-back
/// publication restores ([`Db::commit`]).
struct DerivedPartition {
  partition: Partition,
  /// The snapshot slot and sequence the derivation started from, if any.
  snapshot: Option<(u8, u64)>,
  /// The first sequence replayed from the log.
  from_seq: u64,
  replayed: Replayed,
  /// The records replayed.
  records: u64,
}

/// Derives partition `index` from the segment: the newest valid snapshot (or an empty partition),
/// then every record at or after it applied in order. Pure with respect to the segment (nothing is
/// written; a torn tail is reported in `replayed`, not cut — the caller decides).
fn derive_partition(
  segment: &AnchorSegment,
  index: u16,
  log: &LogRing,
  caps: PartitionCaps,
  now_ns: u64,
) -> Result<DerivedPartition, DbError> {
  let (mut partition, snapshot, from_seq) = match newest_snapshot(segment, index)? {
    Some((slot, snapshot)) => {
      let seq = snapshot.seq;
      (
        Partition::from_snapshot(&snapshot, caps, now_ns)?,
        Some((slot, seq)),
        seq.saturating_add(1),
      )
    }
    None => (Partition::new(caps, now_ns), None, 0),
  };
  let replayed = log.replay(segment, from_seq)?;
  let mut records = 0u64;
  for (_, entry) in &replayed.ops {
    for op in entry.ops() {
      partition.apply(op)?;
    }
    records = records.saturating_add(1);
  }
  Ok(DerivedPartition {
    partition,
    snapshot,
    from_seq,
    replayed,
    records,
  })
}

impl Db {
  /// The partition.
  pub fn partition(&self) -> &Partition {
    &self.partition
  }

  /// The partition, mutably (for the lease wheel).
  pub fn partition_mut(&mut self) -> &mut Partition {
    &mut self.partition
  }

  /// The next sequence.
  pub fn next_seq(&self) -> u64 {
    self.next_seq
  }

  /// The policy.
  pub fn policy(&self) -> SnapshotPolicy {
    self.policy
  }

  /// Sets the policy and pins it: the snapshots after it keep this cadence rather than re-derive one (a test that
  /// drives snapshots at a chosen rate; a caller that measured its own replay).
  pub fn set_policy(&mut self, policy: SnapshotPolicy) {
    self.policy = policy;
    self.policy_pinned = true;
  }

  /// Snapshots taken since recovery.
  pub fn snapshots_taken(&self) -> u64 {
    self.snapshots_taken
  }

  /// The running figures: snapshots and their bytes and longest duration, the log appended, the cadence and the
  /// measured apply throughput.
  pub fn stats(&self) -> DbStats {
    DbStats {
      bytes_between_snapshots: self.policy.bytes_between_snapshots,
      apply_bytes_per_us: self.apply_bytes_per_us(),
      ..self.stats
    }
  }

  /// The apply throughput since the last snapshot, bytes a microsecond: zero until a commit's apply was timed.
  fn apply_bytes_per_us(&self) -> u64 {
    self
      .since_snapshot_bytes
      .saturating_mul(NS_PER_US)
      .checked_div(self.apply_ns_since_snapshot)
      .unwrap_or(0)
  }

  /// Applies `op` to the partition, timing it: the work a replay repeats, measured where every commit already does
  /// it (A-243). The time decides nothing inside the apply; the cadence is derived from it at the next snapshot.
  fn apply_timed(&mut self, op: &Op) -> Result<(), DbError> {
    let started = Instant::now();
    let applied = self.partition.apply(op);
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    self.apply_ns_since_snapshot = self.apply_ns_since_snapshot.saturating_add(elapsed);
    applied
  }

  /// Guard, append, apply: the mutation is durable in the segment when this returns; a reply
  /// may follow. Returns the record's sequence.
  pub fn mutate(
    &mut self,
    segment: &mut AnchorSegment,
    op: &Op,
    now_ns: u64,
  ) -> Result<u64, DbError> {
    self.partition.check(op, now_ns)?;
    self.last_now_ns = now_ns;
    if self.pending.is_some() {
      // Inside a transaction: applied now (later operations see it), logged at the commit.
      self.apply_timed(op)?;
      if let Some(pending) = self.pending.as_mut() {
        pending.push(op.clone());
      }
      return Ok(self.next_seq);
    }
    let seq = self.next_seq;
    let entry = LogEntry {
      ops: vec![op.clone()],
    };
    let bytes = match self.log.append(segment, seq, &entry) {
      Ok(bytes) => bytes,
      Err(DbError::LogFull { .. }) => {
        // A full ring: the snapshot releases everything before it, then the append retries.
        self.snapshot(segment)?;
        self.log.append(segment, seq, &entry)?
      }
      Err(e) => return Err(e),
    };
    self.apply_timed(op)?;
    self.next_seq = seq.saturating_add(1);
    self.since_snapshot_bytes = self.since_snapshot_bytes.saturating_add(bytes);
    self.stats.log_bytes_appended = self.stats.log_bytes_appended.saturating_add(bytes);
    self.maintain(segment);
    Ok(seq)
  }

  /// Opens a transaction: every `mutate` until `commit` is checked and applied at once but
  /// logged together as one `Op::Batch` record, so replay sees all of them or none (a verb's
  /// effects and its completion record are one durable step, §4.9). A transaction that is
  /// never committed logs nothing; its applied effects die with the process, as replay would
  /// have it.
  pub fn begin(&mut self) {
    if self.pending.is_none() {
      self.pending = Some(Vec::new());
    }
  }

  /// Commits the transaction: one record for every operation applied since `begin` (none
  /// when nothing was applied). A full log takes a snapshot instead, which already holds the
  /// applied effects, so the record is not needed and the sequence moves past it.
  ///
  /// **Two failure classes, kept apart (AC-2.3; AUD-06).** A failure **before** anything is
  /// durable — the append refused for a reason a snapshot cannot cure, or the log full and the
  /// snapshot that would carry the effects not published — **rolls the transaction back**: the
  /// partition is re-derived from the segment's durable state ([`derive_partition`], what a
  /// restart would build), so the applied effects and the completion record are gone together,
  /// and the typed [`DbError::Unpublished`] names the cause. Before this, the effects and the
  /// completion stayed applied in memory behind a refusal, a retry was answered the success from
  /// that memory, and the next restart lost it. A failure **after** a durable append — the
  /// maintenance snapshot the policy asked for — never fails the commit: the record stands, the
  /// snapshot is deferred to the next commit ([`maintain`](Self::maintain)) and counted.
  pub fn commit(&mut self, segment: &mut AnchorSegment) -> Result<Option<u64>, DbError> {
    let Some(ops) = self.pending.take() else {
      return Ok(None);
    };
    if ops.is_empty() {
      return Ok(None);
    }
    let entry = LogEntry { ops };
    let seq = self.next_seq;
    let appended = if self
      .publication_fault
      .take_if(|f| *f == PublicationFault::BeforeAppend)
      .is_some()
    {
      Err(DbError::Anchor(AnchorError::PublicationInProgress))
    } else {
      self.log.append(segment, seq, &entry)
    };
    let bytes = match appended {
      Ok(bytes) => bytes,
      Err(DbError::LogFull { .. }) => {
        // The snapshot carries the applied effects, so the record is not needed; the sequence moves
        // past it only once the snapshot is published, so a replay from that snapshot continues at
        // the right place and a failed snapshot leaves the sequence where the durable state is.
        return match self.snapshot_or_fault(segment) {
          Ok(()) => {
            self.next_seq = seq.saturating_add(1);
            Ok(Some(seq))
          }
          Err(cause) => Err(self.rollback(segment, seq, cause)),
        };
      }
      Err(cause) => return Err(self.rollback(segment, seq, cause)),
    };
    self.next_seq = seq.saturating_add(1);
    self.since_snapshot_bytes = self.since_snapshot_bytes.saturating_add(bytes);
    self.stats.log_bytes_appended = self.stats.log_bytes_appended.saturating_add(bytes);
    self.maintain(segment);
    Ok(Some(seq))
  }

  /// Rolls a transaction back after its record could not be made durable (nothing was written):
  /// the partition is re-derived from the segment — the newest published snapshot and the records
  /// after it, the state a restart would build — so the transaction's effects and its completion
  /// record are gone together, and the sequence stays where the durable state is. Counted
  /// (`rollbacks`). Returns the typed refusal to hand the caller. Should the derivation itself
  /// fail (the segment unreadable — a state in which nothing can be made durable at all), the
  /// derivation's refusal is the cause named and the partition is left as it was.
  fn rollback(&mut self, segment: &AnchorSegment, seq: u64, cause: DbError) -> DbError {
    self.rollbacks = self.rollbacks.saturating_add(1);
    match derive_partition(
      segment,
      self.index,
      &self.log,
      self.partition.caps(),
      self.last_now_ns,
    ) {
      Ok(derived) => {
        self.partition = derived.partition;
        DbError::Unpublished {
          seq,
          cause: Box::new(cause),
        }
      }
      Err(derivation) => DbError::Unpublished {
        seq,
        cause: Box::new(derivation),
      },
    }
  }

  /// The maintenance after a durable append: the snapshot the policy asks for once enough log
  /// bytes accumulated. A snapshot that fails here is **deferred**, not fatal — the record is
  /// already durable, so the commit stands; `since_snapshot_bytes` stays past the policy so the
  /// next commit asks again, and the failure is counted (`maintenance_failures`). A log that fills
  /// before a deferred snapshot succeeds takes the full-log path on the next append, whose own
  /// snapshot failure is a rollback — durability degrades into typed refusals, never into a
  /// record the segment does not hold.
  fn maintain(&mut self, segment: &mut AnchorSegment) {
    if self.since_snapshot_bytes < self.policy.bytes_between_snapshots {
      return;
    }
    if self.snapshot_or_fault(segment).is_err() {
      self.maintenance_failures = self.maintenance_failures.saturating_add(1);
    }
  }

  /// [`snapshot`](Self::snapshot), or the injected after-append fault consumed as its failure.
  fn snapshot_or_fault(&mut self, segment: &mut AnchorSegment) -> Result<(), DbError> {
    if self
      .publication_fault
      .take_if(|f| *f == PublicationFault::AfterAppend)
      .is_some()
    {
      return Err(DbError::Anchor(AnchorError::PublicationInProgress));
    }
    self.snapshot(segment)
  }

  /// Test support (never reachable from the wire): the next publication fails as `fault` names,
  /// once — before the append (the transaction rolls back) or after it (the maintenance snapshot
  /// is deferred). `None` clears a pending fault.
  pub fn inject_publication_fault(&mut self, fault: Option<PublicationFault>) {
    self.publication_fault = fault;
  }

  /// Transactions rolled back because their record could not be made durable (AUD-06).
  pub fn rollbacks(&self) -> u64 {
    self.rollbacks
  }

  /// Maintenance snapshots that failed after a durable append and were deferred.
  pub fn maintenance_failures(&self) -> u64 {
    self.maintenance_failures
  }

  /// Whether a transaction is open.
  pub fn in_transaction(&self) -> bool {
    self.pending.is_some()
  }

  /// Publishes the partition into the alternate snapshot slot and trims the log behind it.
  pub fn snapshot(&mut self, segment: &mut AnchorSegment) -> Result<(), DbError> {
    let seq = self.next_seq.saturating_sub(1);
    let started = Instant::now();
    let snapshot = self.partition.to_snapshot(seq);
    let bytes = snapshot.to_bytes();
    segment.publish(RegionKind::Snapshot(self.index, self.next_slot), &bytes)?;
    let snapshot_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    // The cadence after it (A-243), decided here at the commit, never inside an apply: the apply throughput measured
    // since the last snapshot (the boot replay's until one was), and never less than this snapshot's size.
    if !self.policy_pinned {
      let measured = self.apply_bytes_per_us();
      let rate = if measured > 0 {
        measured
      } else {
        self.boot_bytes_per_us
      };
      self.policy = SnapshotPolicy::at_snapshot(RECOVERY_BUDGET_NS, rate, snapshot_bytes).get();
    }
    self.log.trim(segment, self.next_seq)?;
    self.next_slot = other_slot(self.next_slot);
    self.since_snapshot_bytes = 0;
    self.apply_ns_since_snapshot = 0;
    self.snapshots_taken = self.snapshots_taken.saturating_add(1);
    self.stats.snapshots = self.snapshots_taken;
    self.stats.snapshot_ns_max = self.stats.snapshot_ns_max.max(elapsed);
    self.stats.snapshot_bytes_written = self
      .stats
      .snapshot_bytes_written
      .saturating_add(snapshot_bytes);
    Ok(())
  }

  /// The log's bytes used, free and capacity.
  pub fn log_usage(&self, segment: &AnchorSegment) -> Result<(u64, u64, u64), DbError> {
    self.log.usage(segment)
  }
}
