//! Recovery and the snapshot policy (§4.8 "Recovery": "the anchor segment replays the local
//! log ... into fresh indexes"; Phase 2 task 2: "snapshots of partition state into the segment
//! on a cadence derived from measured replay throughput and the recovery budget").
//!
//! [`Db`] is one partition's writer: `mutate` runs the guard, appends the record, applies it,
//! and once the log since the last snapshot has taken the recovery budget's worth of measured
//! replay work and is at least the last snapshot's size (A-243), publishes the partition into
//! the alternate snapshot slot and trims the log behind it. [`recover`] is the other direction:
//! the newest valid snapshot slot, then the records after its sequence, with the torn tail cut
//! off and the replay timed (the recovered tail's starting work).

use std::time::Instant;

use slates_anchor::{AnchorError, AnchorSegment, RegionKind};
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

/// A fixed snapshot cadence a caller pins ([`Db::set_policy`]): the model tests drive snapshots at a chosen rate, and
/// a bench that measures the whole log sets none. A partition no caller pinned follows [`snapshot_due`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotPolicy {
  /// Snapshot once this many log bytes were appended since the last one.
  pub bytes_between_snapshots: u64,
}

/// The log since the last snapshot, as the commits measured it (A-243).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tail {
  /// Its bytes.
  pub bytes: u64,
  /// The replay work its commits measured, nanoseconds: each record's append (encode, checksum, copy into the ring,
  /// the mirror of a replay's read, checksum and decode) and its apply. After a recovery it starts at that recovery's
  /// own time, the snapshot's decode included.
  pub replay_ns: u64,
}

/// Whether a partition snapshots now (A-243): once its tail took at least the recovery budget's replay work, and is at
/// least the last snapshot's size. The first condition bounds recovery by the budget with the tail's own work, measured
/// rather than extrapolated: a rate measured on one stretch of log extrapolated to the next misjudged it whenever the
/// records changed size (large green increments at 442 bytes a microsecond, small records after them at 12). The
/// second is the doubling rule, so a snapshot comes only once the log has absorbed what it costs and snapshot work
/// stays within about twice the log written, whatever the partition's size. Recovery is then at most the snapshot's
/// decode plus the larger of the budget and the replay of a snapshot's size of tail.
pub fn snapshot_due(tail: Tail, floor_bytes: u64, recovery_budget_ns: u64) -> bool {
  tail.replay_ns >= recovery_budget_ns && tail.bytes >= floor_bytes
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
  /// The log since the last snapshot and the replay work its commits measured.
  pub tail: Tail,
  /// The recovery budget the tail's work is held to, nanoseconds.
  pub recovery_budget_ns: u64,
  /// The floor: the last snapshot's size, which the tail must reach before the next one.
  pub snapshot_floor_bytes: u64,
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
  /// The cadence a caller pinned ([`Db::set_policy`]); with none, [`snapshot_due`] decides.
  pinned: Option<SnapshotPolicy>,
  since_snapshot_bytes: u64,
  next_slot: u8,
  snapshots_taken: u64,
  /// The running figures ([`DbStats`]).
  stats: DbStats,
  /// The time the commits since the last snapshot spent on what a replay repeats per record: the record's append
  /// (encode, checksum, copy into the ring), the mirror of the replay's read, checksum and decode, and its apply; never
  /// a wait outside them. Timing the apply alone made a replay look 16 to 21 times faster than it was (raft, A-243).
  /// After a recovery it starts at the recovery's own time, so it errs early.
  replay_work_ns_since_snapshot: u64,
  /// The recovery budget the tail's work is held to: [`RECOVERY_BUDGET_NS`] unless the operator set one
  /// ([`Db::set_recovery_budget`]; GAPS §5).
  recovery_budget_ns: u64,
  /// The last snapshot's size (the recovered one's after a boot, zero with none): the doubling rule's floor.
  snapshot_floor_bytes: u64,
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
      .field("pinned", &self.pinned)
      .finish()
  }
}

/// The newest valid snapshot slot of a partition: of the slots that decode, the one with the highest sequence (the
/// lower slot on a tie). A snapshot's sequence is its encoding's first field, so the slots are ordered by it before
/// either is decoded, the newer is decoded first, and the older only when the newer does not decode. Until 2026-11-07
/// both were decoded whole and the older dropped: of a 1,001-volume partition's 184-218 ms snapshot read on this Mac,
/// one decode was waste, and the read is the longest step of a shard's recovery (§2.6 boot step 2).
fn newest_snapshot(
  segment: &AnchorSegment,
  index: u16,
) -> Result<Option<(u8, PartitionSnapshot, u64)>, DbError> {
  let mut slots: Vec<(u64, u8, Vec<u8>)> = Vec::new();
  for slot in 0..2u8 {
    let Ok(Some(bytes)) = segment.read_published(RegionKind::Snapshot(index, slot)) else {
      continue;
    };
    let Ok(seq) = u64::decode(&mut bytes.as_slice()) else {
      continue;
    };
    slots.push((seq, slot, bytes));
  }
  // Newest first; the sort is stable, so on a tie the lower slot stays first, as before.
  slots.sort_by_key(|(seq, _, _)| std::cmp::Reverse(*seq));
  for (_, slot, bytes) in slots {
    if let Ok(snapshot) = PartitionSnapshot::from_bytes(&bytes) {
      let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
      return Ok(Some((slot, snapshot, size)));
    }
  }
  Ok(None)
}

/// The clock a partition's replay work is charged by (A-243): the committing thread's CPU time
/// ([`slates_rt::calling_thread_cpu_ns`]), what a replay repeats of a commit (its append's encode, checksum and copy, and
/// its apply), never the time the thread waited for a core. Charged on the wall, a commit preempted under load was
/// charged its wait too, so the trigger came early exactly when the machine was busiest, adding snapshot work to the
/// load (`time_off_the_cpu_inside_timed_work_is_not_charged`; `snapshot_cadence.rs` failed 3 of 6 on a Mac at load
/// 69–112). Where no thread clock is read (Miri), nothing is charged, and the ring and the byte floor still force
/// snapshots.
#[derive(Clone, Copy, Debug)]
struct WorkClock {
  started: Option<u64>,
}

impl WorkClock {
  /// Starts the clock now.
  fn start() -> WorkClock {
    WorkClock {
      started: slates_rt::calling_thread_cpu_ns(),
    }
  }

  /// The work since the start, nanoseconds: the thread's CPU time since, zero where either read failed.
  fn elapsed_ns(self) -> u64 {
    match (self.started, slates_rt::calling_thread_cpu_ns()) {
      (Some(started), Some(now)) => now.saturating_sub(started),
      _ => 0,
    }
  }
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
  let work = WorkClock::start();
  let log = LogRing::new(RegionKind::Log(index));
  let DerivedPartition {
    partition,
    snapshot,
    snapshot_bytes,
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
  let db = Db {
    partition,
    index,
    log,
    next_seq: recovered.next_seq,
    pinned: None,
    since_snapshot_bytes: replayed.bytes,
    next_slot: snapshot.map_or(0, |(slot, _)| other_slot(slot)),
    snapshots_taken: 0,
    stats: DbStats::default(),
    replay_work_ns_since_snapshot: work.elapsed_ns(),
    recovery_budget_ns: RECOVERY_BUDGET_NS,
    snapshot_floor_bytes: snapshot_bytes,
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
  /// That snapshot's encoded size (zero with none).
  snapshot_bytes: u64,
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
  let (mut partition, snapshot, snapshot_bytes, from_seq) = match newest_snapshot(segment, index)? {
    Some((slot, snapshot, size)) => {
      let seq = snapshot.seq;
      (
        Partition::from_snapshot(&snapshot, caps, now_ns)?,
        Some((slot, seq)),
        size,
        seq.saturating_add(1),
      )
    }
    None => (Partition::new(caps, now_ns), None, 0, 0),
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
    snapshot_bytes,
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

  /// Pins a fixed cadence: the snapshots after it come every `policy.bytes_between_snapshots` of log, whatever the
  /// measured work (a model test that drives snapshots at a chosen rate; a bench that wants none).
  pub fn set_policy(&mut self, policy: SnapshotPolicy) {
    self.pinned = Some(policy);
  }

  /// Sets the recovery budget the tail's measured replay work is held to (an operator input, GAPS §5).
  pub fn set_recovery_budget(&mut self, recovery_budget_ns: u64) {
    self.recovery_budget_ns = recovery_budget_ns;
  }

  /// Snapshots taken since recovery.
  pub fn snapshots_taken(&self) -> u64 {
    self.snapshots_taken
  }

  /// The running figures: snapshots and their bytes and longest duration, the log appended, the tail and its measured
  /// replay work, the budget and the floor.
  pub fn stats(&self) -> DbStats {
    DbStats {
      tail: self.tail(),
      recovery_budget_ns: self.recovery_budget_ns,
      snapshot_floor_bytes: self.snapshot_floor_bytes,
      ..self.stats
    }
  }

  /// The log since the last snapshot and its measured replay work.
  fn tail(&self) -> Tail {
    Tail {
      bytes: self.since_snapshot_bytes,
      replay_ns: self.replay_work_ns_since_snapshot,
    }
  }

  /// Adds the work since `started` to the tail's replay work (A-243). The work decides nothing inside an apply; the
  /// commit consults it after the append ([`maintain`](Self::maintain)).
  fn charge_replay_work(&mut self, started: WorkClock) {
    self.replay_work_ns_since_snapshot = self
      .replay_work_ns_since_snapshot
      .saturating_add(started.elapsed_ns());
  }

  /// Applies `op` inside a transaction, timing it as replay work: its record is appended at the commit, so the apply
  /// and the append are timed apart.
  fn apply_timed(&mut self, op: &Op) -> Result<(), DbError> {
    let started = WorkClock::start();
    let applied = self.partition.apply(op);
    self.charge_replay_work(started);
    applied
  }

  /// Appends a transaction's record at its commit, timing it as replay work: the encode, checksum and copy a replay
  /// mirrors with its read, checksum and decode (A-243). A refused append's time counts too, so the measure errs early.
  fn append_timed(
    &mut self,
    segment: &mut AnchorSegment,
    seq: u64,
    entry: &LogEntry,
  ) -> Result<u64, DbError> {
    let started = WorkClock::start();
    let appended = self.log.append(segment, seq, entry);
    self.charge_replay_work(started);
    appended
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
    // One clock pair over the append and the apply, adjacent here: the work a replay repeats for this record (A-243).
    let mut started = WorkClock::start();
    let bytes = match self.log.append(segment, seq, &entry) {
      Ok(bytes) => bytes,
      Err(DbError::LogFull { .. }) => {
        // A full ring: the snapshot releases everything before it, then the append retries; the snapshot's own time is
        // not replay work.
        self.snapshot(segment)?;
        started = WorkClock::start();
        self.log.append(segment, seq, &entry)?
      }
      Err(e) => return Err(e),
    };
    let applied = self.partition.apply(op);
    self.charge_replay_work(started);
    applied?;
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
      self.append_timed(segment, seq, &entry)
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
    let due = match self.pinned {
      Some(policy) => self.since_snapshot_bytes >= policy.bytes_between_snapshots,
      None => snapshot_due(
        self.tail(),
        self.snapshot_floor_bytes,
        self.recovery_budget_ns,
      ),
    };
    if !due {
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

  /// Publishes the partition into the alternate snapshot slot and releases the log behind it.
  pub fn snapshot(&mut self, segment: &mut AnchorSegment) -> Result<(), DbError> {
    let seq = self.next_seq.saturating_sub(1);
    let started = Instant::now();
    let snapshot = self.partition.to_snapshot(seq);
    let bytes = snapshot.to_bytes();
    segment.publish(RegionKind::Snapshot(self.index, self.next_slot), &bytes)?;
    let snapshot_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    // The next snapshot's floor (A-243): the tail must reach this one's size first.
    self.snapshot_floor_bytes = snapshot_bytes;
    self.log.release_all(segment, self.next_seq)?;
    self.next_slot = other_slot(self.next_slot);
    self.since_snapshot_bytes = 0;
    self.replay_work_ns_since_snapshot = 0;
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

#[cfg(test)]
mod work_clock_tests {
  use super::WorkClock;

  /// Shape: how long the test keeps its thread off the CPU inside timed work.
  const OFF_CPU: std::time::Duration = std::time::Duration::from_millis(50);

  /// A-243, the trigger's measure: do start the clock a commit's replay work is charged by, keep the thread off the CPU
  /// for 50 ms (asleep), then read the work. Expect under half of it: a replay repeats a commit's work, not the time its
  /// thread waited, so time off the CPU must not be charged (Windows charges a thread's time a scheduler tick at a time,
  /// 15.6 ms by default, so a sleeping thread shows at most one). The wall clock charged the whole sleep, and under
  /// load, where a committing thread waits for a core, the trigger came early (`snapshot_cadence.rs`, 3 of 6 at load
  /// 69–112 on a Mac).
  #[test]
  fn time_off_the_cpu_inside_timed_work_is_not_charged() {
    let clock = WorkClock::start();
    #[allow(clippy::disallowed_methods)]
    // a test keeping its own thread off the CPU, which is what it measures
    std::thread::sleep(OFF_CPU);
    let charged = clock.elapsed_ns();
    let slept = u64::try_from(OFF_CPU.as_nanos()).unwrap();
    assert!(
      charged < slept / 2,
      "time off the CPU was charged as replay work: {charged} ns of a {slept} ns sleep"
    );
  }
}
