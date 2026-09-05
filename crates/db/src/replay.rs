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

use slates_anchor::{AnchorSegment, RegionKind};
use slates_machine::{Derived, derived};
use slates_wire::Wire;

use crate::error::DbError;
use crate::op::Op;
use crate::partition::{Partition, PartitionCaps, PartitionSnapshot};
use crate::record::{LogEntry, LogRing};

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

impl Recovered {
  /// Measured: the replay throughput in bytes per microsecond (zero when nothing replayed).
  pub fn replay_bytes_per_us(&self) -> u64 {
    self.replayed_bytes.saturating_mul(NS_PER_US) / self.replay_ns.max(1)
  }
}

/// One partition's database: the partition, its log and its snapshot cadence.
pub struct Db {
  partition: Partition,
  index: u16,
  log: LogRing,
  next_seq: u64,
  policy: SnapshotPolicy,
  since_snapshot_bytes: u64,
  next_slot: u8,
  snapshots_taken: u64,
  /// Operations applied since `begin`, waiting to go into one record at `commit`.
  pending: Option<Vec<Op>>,
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
    records += 1;
  }
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
    since_snapshot_bytes: replayed.bytes,
    next_slot: snapshot.map_or(0, |(slot, _)| (slot + 1) % 2),
    snapshots_taken: 0,
    pending: None,
  };
  Ok((db, recovered))
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

  /// Sets the policy (the caller re-derives it after a measured replay).
  pub fn set_policy(&mut self, policy: SnapshotPolicy) {
    self.policy = policy;
  }

  /// Snapshots taken since recovery.
  pub fn snapshots_taken(&self) -> u64 {
    self.snapshots_taken
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
    if let Some(pending) = self.pending.as_mut() {
      // Inside a transaction: applied now (later operations see it), logged at the commit.
      self.partition.apply(op)?;
      pending.push(op.clone());
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
    self.partition.apply(op)?;
    self.next_seq = seq.saturating_add(1);
    self.since_snapshot_bytes = self.since_snapshot_bytes.saturating_add(bytes);
    if self.since_snapshot_bytes >= self.policy.bytes_between_snapshots {
      self.snapshot(segment)?;
    }
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
  pub fn commit(&mut self, segment: &mut AnchorSegment) -> Result<Option<u64>, DbError> {
    let Some(ops) = self.pending.take() else {
      return Ok(None);
    };
    if ops.is_empty() {
      return Ok(None);
    }
    let entry = LogEntry { ops };
    let seq = self.next_seq;
    let bytes = match self.log.append(segment, seq, &entry) {
      Ok(bytes) => bytes,
      Err(DbError::LogFull { .. }) => {
        // The snapshot carries the applied effects; the sequence moves past the record that
        // is not written, so a replay from the snapshot continues at the right place.
        self.next_seq = seq.saturating_add(1);
        self.snapshot(segment)?;
        return Ok(Some(seq));
      }
      Err(e) => return Err(e),
    };
    self.next_seq = seq.saturating_add(1);
    self.since_snapshot_bytes = self.since_snapshot_bytes.saturating_add(bytes);
    if self.since_snapshot_bytes >= self.policy.bytes_between_snapshots {
      self.snapshot(segment)?;
    }
    Ok(Some(seq))
  }

  /// Whether a transaction is open.
  pub fn in_transaction(&self) -> bool {
    self.pending.is_some()
  }

  /// Publishes the partition into the alternate snapshot slot and trims the log behind it.
  pub fn snapshot(&mut self, segment: &mut AnchorSegment) -> Result<(), DbError> {
    let seq = self.next_seq.saturating_sub(1);
    let snapshot = self.partition.to_snapshot(seq);
    segment.publish(
      RegionKind::Snapshot(self.index, self.next_slot),
      &snapshot.to_bytes(),
    )?;
    self.log.trim(segment, self.next_seq)?;
    self.next_slot = (self.next_slot + 1) % 2;
    self.since_snapshot_bytes = 0;
    self.snapshots_taken += 1;
    Ok(())
  }

  /// The log's bytes used, free and capacity.
  pub fn log_usage(&self, segment: &AnchorSegment) -> Result<(u64, u64, u64), DbError> {
    self.log.usage(segment)
  }
}
