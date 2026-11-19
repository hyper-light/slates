//! Recovery and the snapshot policy (§4.8 "Recovery": "the anchor segment replays the local
//! log ... into fresh indexes"; Phase 2 task 2: "snapshots of partition state into the segment
//! on a cadence derived from measured replay throughput and the recovery budget").
//!
//! [`Db`] is one partition's writer: `mutate` runs the guard, appends the record, applies it,
//! and once the log since the last snapshot has taken the recovery budget's worth of measured
//! replay work and is at least the last snapshot's size (A-243), publishes the partition into
//! the alternate snapshot slot and trims the log behind it. [`recover`] is the other direction:
//! the newest valid snapshot slot, then the records after its sequence, a record at a time
//! (A-420), with the torn tail cut off and the replay timed (the recovered tail's starting work).

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
  /// The replay work its commits measured, nanoseconds: each record's append (its encode, its decode as the replay
  /// will decode it, its checksum and its copy into the ring, which cover a replay's read, checksum and decode; A-420)
  /// and its apply. After a recovery it starts at that recovery's own time, the snapshot's decode included.
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
  /// The time the commits since the last snapshot spent on what a replay repeats per record: the record's append (its
  /// encode, its decode as the replay will decode it, its checksum and its copy into the ring, which cover the replay's
  /// read, checksum and decode) and its apply; never a wait outside them. Timing the apply alone made a replay look 16
  /// to 21 times faster than it was (raft, A-243); timing the encode in the decode's place made a record whose decode
  /// allocates what its encode copies replay in more than was measured (A-420). After a recovery it starts at the
  /// recovery's own time, so it errs early.
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
    replay_work_ns_since_snapshot: replay_ns,
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
  // A record at a time: decoded, applied and dropped before the next is read, the work each commit timed (A-420).
  let replayed = log.replay(segment, from_seq, |entry| {
    entry.ops().iter().try_for_each(|op| partition.apply(op))
  })?;
  let records = replayed.records;
  Ok(DerivedPartition {
    partition,
    snapshot,
    snapshot_bytes,
    from_seq,
    replayed,
    records,
  })
}

/// Runs `step`, adding its wall time to `work_ns`.
fn timed<T>(work_ns: &mut u64, step: impl FnOnce() -> T) -> T {
  let started = Instant::now();
  let done = step();
  let elapsed = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
  *work_ns = work_ns.saturating_add(elapsed);
  done
}

/// Appends `entry` as record `seq`, adding to `work_ns` what a replay of the record repeats (A-420): its decode, and
/// the write's checksum and copy, which stand for the replay's own checksum and its copy out of the ring (an
/// allocation, a zeroing and a copy, which the encode's allocation and its write of every byte bound). The encode
/// itself is timed too, since it bounds the replay's read, so the measure covers the replay's work, erring early by the
/// encode and a preempted commit's stall. It timed the encode in the decode's place, and a record whose decode
/// allocates what its encode copies (a thousand one-byte keys) replayed in more than the commits measured, so the
/// trigger let a tail run past its budget. A body that does not decode is refused before anything is written, as a
/// replay would cut it.
fn append_record(
  log: &LogRing,
  segment: &mut AnchorSegment,
  seq: u64,
  entry: &LogEntry,
  work_ns: &mut u64,
) -> Result<u64, DbError> {
  timed(work_ns, || {
    let encoded = LogRing::encode(entry);
    encoded.decode_as_replayed(seq)?;
    log.write(segment, seq, &encoded)
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

  /// Adds `work_ns` to the tail's replay work (A-243). The time decides nothing inside an apply; the commit consults it
  /// after the append ([`maintain`](Self::maintain)).
  fn charge_replay_work(&mut self, work_ns: u64) {
    self.replay_work_ns_since_snapshot = self.replay_work_ns_since_snapshot.saturating_add(work_ns);
  }

  /// Applies `op` inside a transaction, timing it as replay work: its record is appended at the commit, so the apply
  /// and the append are timed apart.
  fn apply_timed(&mut self, op: &Op) -> Result<(), DbError> {
    let mut work_ns = 0;
    let applied = timed(&mut work_ns, || self.partition.apply(op));
    self.charge_replay_work(work_ns);
    applied
  }

  /// Appends a transaction's record at its commit ([`append_record`]), charging the work a replay repeats for it. A
  /// refused append's time counts too, so the measure errs early.
  fn append_timed(
    &mut self,
    segment: &mut AnchorSegment,
    seq: u64,
    entry: &LogEntry,
  ) -> Result<u64, DbError> {
    let mut work_ns = 0;
    let appended = append_record(&self.log, segment, seq, entry, &mut work_ns);
    self.charge_replay_work(work_ns);
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
    // The work a replay repeats for this record, timed step by step: the append's ([`append_record`]), then the apply.
    let mut work_ns = 0;
    let bytes = match append_record(&self.log, segment, seq, &entry, &mut work_ns) {
      Ok(bytes) => bytes,
      Err(DbError::LogFull { .. }) => {
        // A full ring: the snapshot releases everything before it, then the append retries; the snapshot's own time is
        // not replay work, and the refused append's, timed before it, counts in the tail it starts, erring early.
        self.snapshot(segment)?;
        append_record(&self.log, segment, seq, &entry, &mut work_ns)?
      }
      Err(e) => return Err(e),
    };
    let applied = timed(&mut work_ns, || self.partition.apply(op));
    self.charge_replay_work(work_ns);
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
mod tests {
  use super::*;
  use crate::HostId;
  use crate::catalog::{
    BaseRecord, NamePolicy, PolicyRecord, Principal, Role, SizeClass, SnapshotId, VolumeId,
    VolumeRecord, VolumeState,
  };
  use slates_anchor::Geometry;
  use slates_machine::facts::Identity;

  /// Shape: the segment page.
  const PAGE: u64 = 4096;
  /// Shape: a log ring with room for the whole tail (about 5 KiB a record), so no full ring forces a snapshot.
  const LOG_BYTES: u64 = 4 << 20;
  /// Shape: a snapshot slot: room for the one volume the test makes.
  const SNAPSHOT_BYTES: u64 = 1 << 20;
  /// Shape: the keys of each record, one byte each: a record whose decode allocates every key while its encode only
  /// copies each key's five bytes into one buffer, so a measure that times the encode in the decode's place falls
  /// short of the replay by most of the decode.
  const KEYS: usize = 1 << 10;
  /// Shape: the records whose measured work is compared with their replay: a tail whose replay is milliseconds in a
  /// debug build, far past the clocks' resolution.
  const RECORDS: usize = 64;
  /// Shape: the timed recoveries of a segment, the median judged and the inner quartiles' spread its noise: five, so
  /// the quartiles are the second and the fourth and the fastest and the slowest stand outside the spread.
  const RECOVERIES: usize = 5;

  fn segment() -> AnchorSegment {
    segment_named("slates-db-replay-unit")
  }

  /// A segment of its own for a test, named `prefix` and the process id, so tests running side by side never share one.
  fn segment_named(prefix: &str) -> AnchorSegment {
    AnchorSegment::create(
      &format!("{prefix}-{}", std::process::id()),
      &Identity {
        cpu: "test".into(),
        os: "test".into(),
        arch: "test".into(),
        cores: 4,
        memory: 1,
        page: PAGE,
      },
      Geometry {
        partitions: 1,
        page: PAGE,
        profile_bytes: PAGE,
        log_bytes: LOG_BYTES,
        snapshot_bytes: SNAPSHOT_BYTES,
        landing_slots: 1,
        landing_slot_bytes: PAGE,
      },
    )
    .unwrap()
  }

  fn caps() -> PartitionCaps {
    PartitionCaps {
      volumes: 1 << 4,
      snapshots: 1 << 4,
      attachments: 1 << 4,
      segment_slots: 4,
      timers: 1 << 4,
      tick_ns: 1_000,
      green_chain_bytes: 1 << 20,
      audit_records: 1 << 4,
    }
  }

  fn volume() -> VolumeRecord {
    let mut bytes = [0u8; 16];
    bytes[0] = 1;
    VolumeRecord {
      id: VolumeId { bytes },
      name: "signed".to_owned(),
      owner_shard: 0,
      policy: PolicyRecord {
        size: SizeClass::Bounded { limit: 1 << 30 },
        names: NamePolicy::Exact,
        require_locked: false,
        role: Role::Plain,
        live_shipped: false,
        live_holders: None,
        write: None,
        write_version: 0,
        signers: Vec::new(),
      },
      base: BaseRecord::Scratch,
      head: SnapshotId { value: 0 },
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: Principal::Uid {
        host: HostId(0),
        uid: 1000,
      },
      access: vec![],
      created_ns: 0,
      catalog_version: 0,
      taken_over: None,
    }
  }

  /// A recovery and its cost as the calling thread's CPU time: the work the replay did, which a loaded machine does not
  /// stretch (the cadence tests' measure, `crates/db/tests/snapshot_cadence.rs`).
  #[cfg(unix)]
  fn recovery_cost_ns(segment: &mut AnchorSegment) -> u64 {
    let before = slates_rt::registry::calling_thread_cpu().unwrap();
    let recovered = recover(segment, 0, caps(), 0).unwrap().1;
    let after = slates_rt::registry::calling_thread_cpu().unwrap();
    assert!(!recovered.torn, "the tail verifies");
    after.spent_ns.saturating_sub(before.spent_ns)
  }

  /// [`recovery_cost_ns`] on Windows, by wall time: the thread's CPU time is charged a scheduler tick at a time there
  /// (15.6 ms by default), too coarse for a span this short.
  #[cfg(not(unix))]
  fn recovery_cost_ns(segment: &mut AnchorSegment) -> u64 {
    let recovered = recover(segment, 0, caps(), 0).unwrap().1;
    assert!(!recovered.torn, "the tail verifies");
    recovered.replay_ns
  }

  /// `RECOVERIES` recoveries of the segment as it stands: their median cost and the spread of their inner quartiles.
  fn recoveries(segment: &mut AnchorSegment) -> (u64, u64) {
    let mut costs: Vec<u64> = (0..RECOVERIES).map(|_| recovery_cost_ns(segment)).collect();
    costs.sort_unstable();
    let quartile = RECOVERIES / 4;
    (
      costs[RECOVERIES / 2],
      costs[RECOVERIES - 1 - quartile] - costs[quartile],
    )
  }

  /// A-420 (A-243's open line), §4.8. Do: set a volume's signers again and again to a thousand one-byte keys, with no
  /// snapshot, and compare the replay work the commits measured for those records with what a recovery spends
  /// replaying them (the median of five recoveries past a baseline recovery of the segment before them, as the calling
  /// thread's CPU time). Expect: the measured work covers the replay's, within the recoveries' own noise, since the
  /// snapshot trigger fires on the measure and holds a recovery to its budget only if the measure is at least the
  /// replay. The commits timed each record's encode, checksum, copy and apply, and the replay's decode allocates every
  /// key where the encode copies its bytes into one buffer, so the measure fell short of the replay (A-243's open
  /// line: the commits measured 0.4x to 1.25x the replay on CI's macOS runner, 2026-11-18).
  #[test]
  fn a_tails_measured_replay_work_covers_its_replay_when_its_records_decode_dearer_than_they_encode()
   {
    let mut segment = segment();
    let mut db = recover(&mut segment, 0, caps(), 0).unwrap().0;
    db.set_policy(SnapshotPolicy {
      bytes_between_snapshots: u64::MAX,
    });
    let record = volume();
    let id = record.id;
    db.mutate(&mut segment, &Op::VolumeCreated { record }, 0)
      .unwrap();
    let keys: Vec<Vec<u8>> = (0..KEYS)
      .map(|n| vec![u8::try_from(n % 251).unwrap()])
      .collect();
    // The first set sizes the volume's key list, so every later apply reuses it at the commit and in the replay alike.
    db.mutate(
      &mut segment,
      &Op::VolumeSignersSet {
        id,
        keys: keys.clone(),
      },
      0,
    )
    .unwrap();
    let (baseline_ns, baseline_spread_ns) = recoveries(&mut segment);
    let before = db.stats().tail;
    for _ in 0..RECORDS {
      db.mutate(
        &mut segment,
        &Op::VolumeSignersSet {
          id,
          keys: keys.clone(),
        },
        0,
      )
      .unwrap();
    }
    let after = db.stats().tail;
    let measured_ns = after.replay_ns - before.replay_ns;
    let (tail_ns, tail_spread_ns) = recoveries(&mut segment);
    let replay_ns = tail_ns.saturating_sub(baseline_ns);
    let noise_ns = tail_spread_ns + baseline_spread_ns;
    println!(
      "{RECORDS} records of {KEYS} keys, {} bytes: the commits measured {measured_ns} ns of replay work; a recovery \
       replayed them in {replay_ns} ns past a {baseline_ns} ns baseline (noise {noise_ns} ns), {} per mille of the \
       measure",
      after.bytes - before.bytes,
      replay_ns * 1_000 / measured_ns.max(1)
    );
    assert!(
      replay_ns <= measured_ns + noise_ns,
      "a tail the commits measured at {measured_ns} ns of replay work replayed in {replay_ns} ns past the baseline \
       ({noise_ns} ns of noise): the trigger would let it run past its budget"
    );
  }

  /// A-503 (stage 2 of a captured clone's landing). Do: make a volume with two snapshots; record what a move carried of
  /// its capture with the first as its mark; ask the same naming a snapshot the partition does not hold; record the
  /// carried capture adopted again with the second as the mark; recover the database from its log, then from a snapshot;
  /// destroy the mark's snapshot; release the volume; recover once more. Expect: the carried record and its mark read
  /// back as recorded, the carry raising the catalog's sequence by one and the adoption leaving it; the unheld mark
  /// refused `NotFound` with nothing recorded; both recoveries agreeing; the mark gone with its snapshot and the record
  /// kept; and the record gone with its volume, the last recovery agreeing.
  #[test]
  fn a_carried_capture_is_kept_with_its_volume_and_its_mark_goes_with_its_snapshot() {
    use crate::catalog::CarriedRecord;
    let mut segment = segment_named("slates-db-replay-carried");
    let (mut db, id) = a_volume_with_snapshots_one_and_two(&mut segment);
    let marks = (SnapshotId { value: 1 }, SnapshotId { value: 2 });
    let carried = CarriedRecord {
      head: [1; 32],
      entries: vec![7; 5],
    };
    assert_carried_refused_unheld_then_adopted(&mut db, &mut segment, id, &carried, marks);
    drop(db);
    let db = assert_recovered_from_log_and_snapshot(&mut segment, id, &carried, marks.1);
    assert_mark_and_record_go_with_their_owners(db, &mut segment, id, &carried, marks.1);
  }

  /// A-503's setup: a volume with snapshots 1 and 2, each naming its own identity.
  fn a_volume_with_snapshots_one_and_two(segment: &mut AnchorSegment) -> (Db, VolumeId) {
    use crate::catalog::{PlacementState, SnapshotRecord};
    let mut db = recover(segment, 0, caps(), 0).unwrap().0;
    let record = volume();
    let id = record.id;
    db.mutate(segment, &Op::VolumeCreated { record }, 0)
      .unwrap();
    let taken = |value: u64| Op::SnapshotTaken {
      record: SnapshotRecord {
        id: SnapshotId { value },
        volume: id,
        epoch: value,
        identity: Some([u8::try_from(value).unwrap(); 32]),
        placed: PlacementState::Local,
        taken_ns: 0,
      },
    };
    db.mutate(segment, &taken(1), 0).unwrap();
    db.mutate(segment, &taken(2), 0).unwrap();
    (db, id)
  }

  /// A-503's carry: carried with the first mark (the catalog's sequence up by one), a mark the partition does not hold
  /// refused `NotFound` with nothing recorded, then adopted again with the second mark (the sequence kept).
  fn assert_carried_refused_unheld_then_adopted(
    db: &mut Db,
    segment: &mut AnchorSegment,
    id: VolumeId,
    carried: &crate::catalog::CarriedRecord,
    (first, second): (SnapshotId, SnapshotId),
  ) {
    let version = |db: &Db| db.partition().volume(id).unwrap().catalog_version;
    let before = version(db);
    let carry = |mark: SnapshotId| Op::CaptureCarried {
      volume: id,
      carried: carried.clone(),
      mark: Some(mark),
    };
    db.mutate(segment, &carry(first), 0).unwrap();
    assert_eq!(db.partition().carried(id), Some((carried, Some(first))));
    assert_eq!(
      version(db),
      before + 1,
      "the catalog ships what the move carries"
    );
    assert_eq!(
      db.mutate(segment, &carry(SnapshotId { value: 9 }), 0),
      Err(DbError::NotFound),
      "a mark names a snapshot the partition holds"
    );
    assert_eq!(db.partition().carried(id), Some((carried, Some(first))));
    let adopted = Op::CaptureAdopted {
      volume: id,
      carried: carried.clone(),
      mark: Some(second),
    };
    db.mutate(segment, &adopted, 0).unwrap();
    assert_eq!(db.partition().carried(id), Some((carried, Some(second))));
    assert_eq!(
      version(db),
      before + 1,
      "an adopted catalog's sequence stays"
    );
  }

  /// A-503's recovery: the carried record and its mark read back alike replayed from the log and restored from a
  /// snapshot. Returns the database the snapshot restored.
  fn assert_recovered_from_log_and_snapshot(
    segment: &mut AnchorSegment,
    id: VolumeId,
    carried: &crate::catalog::CarriedRecord,
    mark: SnapshotId,
  ) -> Db {
    let mut db = recover(segment, 0, caps(), 0).unwrap().0;
    assert_eq!(
      db.partition().carried(id),
      Some((carried, Some(mark))),
      "replayed from the log"
    );
    db.snapshot(segment).unwrap();
    drop(db);
    let db = recover(segment, 0, caps(), 0).unwrap().0;
    assert_eq!(
      db.partition().carried(id),
      Some((carried, Some(mark))),
      "restored from a snapshot"
    );
    db
  }

  /// A-503's ends: the mark goes with its snapshot and the record stays; the record goes with its volume, and a last
  /// recovery agrees.
  fn assert_mark_and_record_go_with_their_owners(
    mut db: Db,
    segment: &mut AnchorSegment,
    id: VolumeId,
    carried: &crate::catalog::CarriedRecord,
    mark: SnapshotId,
  ) {
    db.mutate(
      segment,
      &Op::SnapshotDestroyed {
        volume: id,
        id: mark,
      },
      0,
    )
    .unwrap();
    assert_eq!(
      db.partition().carried(id),
      Some((carried, None)),
      "the mark goes with its snapshot"
    );
    db.mutate(segment, &Op::VolumeReleased { id }, 0).unwrap();
    assert_eq!(
      db.partition().carried(id),
      None,
      "the record goes with its volume"
    );
    drop(db);
    let db = recover(segment, 0, caps(), 0).unwrap().0;
    assert_eq!(db.partition().carried(id), None);
  }

  /// A-540. Do: record a host mount's attachment and bind it; keep its mount's id with `AttachmentMountSeen`; ask the
  /// same for an attachment the partition does not hold; then recover the database from its log, and from a snapshot.
  /// Expect: no id before the mount was seen; the id kept once seen; the unheld attachment refused `NotFound` with
  /// nothing recorded; and both recoveries keeping the id, so a restarted daemon can judge the mount by it.
  #[test]
  fn an_attachments_seen_mount_is_kept_through_its_log_and_a_snapshot() {
    use crate::catalog::{AttachForm, AttachmentRecord, Consumer, Rights};
    /// Shape: the kernel's id of the attachment's mount.
    const MOUNT: u64 = (7 << 32) | 42;
    /// Shape: the attachment.
    const ATTACHMENT: u64 = 3;
    let mut segment = segment_named("slates-db-replay-mount-seen");
    let mut db = recover(&mut segment, 0, caps(), 0).unwrap().0;
    let record = volume();
    let (id, owner) = (record.id, record.owner.clone());
    db.mutate(&mut segment, &Op::VolumeCreated { record }, 0)
      .unwrap();
    let attachment = AttachmentRecord {
      id: ATTACHMENT,
      volume: id,
      consumer: Consumer::Bridge,
      snapshot: None,
      form: AttachForm::Root,
      principal: owner,
      rights: Rights {
        read: true,
        write: true,
        admin: false,
      },
      token: [1; 16],
      mount: None,
    };
    db.mutate(&mut segment, &Op::AttachmentAdded { record: attachment }, 0)
      .unwrap();
    db.mutate(
      &mut segment,
      &Op::AttachmentBound {
        id: ATTACHMENT,
        path: "/Volumes/seen".to_owned(),
      },
      0,
    )
    .unwrap();
    let seen = |db: &Db| {
      db.partition()
        .attachment(ATTACHMENT)
        .and_then(|record| record.mount)
    };
    assert_eq!(seen(&db), None, "a mount never seen leaves no id");
    db.mutate(
      &mut segment,
      &Op::AttachmentMountSeen {
        id: ATTACHMENT,
        mount: MOUNT,
      },
      0,
    )
    .unwrap();
    assert_eq!(
      seen(&db),
      Some(MOUNT),
      "the record keeps the seen mount's id"
    );
    assert_eq!(
      db.mutate(
        &mut segment,
        &Op::AttachmentMountSeen {
          id: ATTACHMENT + 1,
          mount: MOUNT,
        },
        0,
      ),
      Err(DbError::NotFound),
      "an attachment the partition does not hold keeps nothing"
    );
    drop(db);
    let mut db = recover(&mut segment, 0, caps(), 0).unwrap().0;
    assert_eq!(seen(&db), Some(MOUNT), "replayed from the log");
    db.snapshot(&mut segment).unwrap();
    drop(db);
    let db = recover(&mut segment, 0, caps(), 0).unwrap().0;
    assert_eq!(seen(&db), Some(MOUNT), "restored from a snapshot");
  }
}
