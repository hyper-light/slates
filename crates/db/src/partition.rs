//! The partition: the records and their indexes, and the deterministic state transition every
//! operation is (§4.8 `Partition`). Two entry points: [`Partition::check`], the guard the live
//! path runs before appending (a stale lease, a duplicate name, a missing record), and
//! [`Partition::apply`], the unconditional transition that both the live path and replay run;
//! a recorded operation was checked once and applies the same way forever after, which is what
//! makes the partition in memory equal the partition replayed from the log.
//!
//! Indexes are adaptive radix trees over the records' keys (§4.8: `volumes_by_name: Art`,
//! leases by principal, completions by client); records sit in slabs named by generational
//! handles (D-8). The lease expiry wheel is the runtime's hierarchical timing wheel keyed by
//! the volume's slot, so expiry is a pop per tick, never a scan.

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::{Handle, Slab};
use slates_rt::timer::Wheel;
use slates_wire::Wire;
use slates_wire::request::{ClientWindow, Seen};

use crate::Art;
use crate::catalog::{
  AttachmentRecord, AuditRecord, CompletionRecord, Consumer, ConsumerRecord, GrantRecord,
  LandingLeaseRecord, LandingRecord, LeaseRecord, LineageEdge, NameAliasRecord, NfsCallRecord,
  NfsClientRecord, NfsDelegationRecord, NfsLockRecord, NfsOpenRecord, NfsPurgeRecord,
  NfsSessionRecord, Principal, SEAL_KEY_RECORD_BYTES, SEAL_RECIPIENT_RECORD_MAX, SealKeyOwner,
  SealKeyRecord, SnapshotId, SnapshotRecord, Tombstone, VolumeId, VolumeRecord,
};
use crate::error::DbError;
use crate::op::Op;

/// Derived: the table capacities a partition is created with (the daemon's derivation from
/// the profile: memory share per shard over record size, and the admission limit for the
/// windows).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionCaps {
  /// Volumes the partition may hold.
  pub volumes: usize,
  /// Snapshots the partition may hold.
  pub snapshots: usize,
  /// Attachments the partition may hold.
  pub attachments: usize,
  /// Slots per slab segment (the pre-faulted growth unit).
  pub segment_slots: usize,
  /// Lease timers the wheel may hold.
  pub timers: usize,
  /// The wheel's tick, nanoseconds.
  pub tick_ns: u64,
  /// The bytes all green merge chains together may hold in the partition (§4.16); a new increment
  /// past it is a typed capacity refusal (bounded growth until checkpointing folds old entries).
  pub green_chain_bytes: usize,
}

/// One client's completion state, as the snapshot carries it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ClientCompletions {
  /// The host id (`HostId.0`) whose client this is (this node for a local client; the authenticated origin
  /// for a forwarded one) — the high half of the globally-unique completion key.
  pub origin: u64,
  /// The client.
  pub client: u32,
  /// Every sequence up to and including this one is acknowledged, except `owed`.
  pub acknowledged_up_to: Option<u32>,
  /// The retained completions.
  pub records: Vec<CompletionRecord>,
  /// The ids at or below `acknowledged_up_to` the client still owes, kept open (§4.9).
  pub owed: Vec<u32>,
  /// The requests that ran and wait for a version to commit (A-252).
  pub awaiting: Vec<crate::catalog::AwaitingCompletion>,
}

/// One green volume's merge chain as a snapshot carries it: the green, its origin when it was
/// seeded from a complete base, and its increments in order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GreenChain {
  /// The green volume.
  pub green: VolumeId,
  /// The encoded increments, in commit order.
  pub increments: Vec<Vec<u8>>,
  /// The encoded origin (§4.16), or none for a green that started from scratch. Appended for
  /// append-only evolution.
  pub origin: Option<Vec<u8>>,
}

/// The whole partition state as one canonical value: what a snapshot slot holds.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct PartitionSnapshot {
  /// The sequence of the last operation folded in.
  pub seq: u64,
  /// The volumes, by id order.
  pub volumes: Vec<VolumeRecord>,
  /// The snapshots, by (volume, id) order.
  pub snapshots: Vec<SnapshotRecord>,
  /// The lineage edges, by child order.
  pub lineage: Vec<LineageEdge>,
  /// The attachments, by id order.
  pub attachments: Vec<AttachmentRecord>,
  /// The completion windows, by client order.
  pub completions: Vec<ClientCompletions>,
  /// The grants, by id order.
  pub grants: Vec<GrantRecord>,
  /// The landing leases, by target order.
  pub landing_leases: Vec<LandingLeaseRecord>,
  /// The landings, by id order.
  pub landings: Vec<LandingRecord>,
  /// The audit records retained (the ring holds the rest).
  pub audit: Vec<AuditRecord>,
  /// Each green volume's merge chain, by green order (§4.16; appended for append-only evolution).
  pub green_chains: Vec<GreenChain>,
  /// The enrolled consumers, by id order (§4.13; appended for append-only evolution).
  pub consumers: Vec<ConsumerRecord>,
  /// Highest client id reserved before handoff (§4.7); zero before the first admission.
  pub client_id_high_water: u32,
  /// The NFSv4 opens held at this partition's files, by `other` (§4.6 A-37; appended for append-only
  /// evolution, as are the two fields after it).
  pub nfs_opens: Vec<NfsOpenRecord>,
  /// The NFSv4 lock states held at this partition's files, by `other`.
  pub nfs_locks: Vec<NfsLockRecord>,
  /// The NFSv4 clients this partition's listener holds, by client id.
  pub nfs_clients: Vec<NfsClientRecord>,
  /// The partition's NFSv4 instance: the last one a daemon life advanced to (zero before any).
  pub nfs_instance: u32,
  /// The destroyed volumes whose tombstone is still owed to their candidate holders, by volume order
  /// (AUD-29-43; appended for append-only evolution).
  pub tombstones: Vec<Tombstone>,
  /// The NFSv4 delegations held at this partition's files, by `other` (§4.6 A-78; appended).
  pub nfs_delegations: Vec<NfsDelegationRecord>,
  /// The sealing keys recorded at this partition, wrapped by their parents, by owner (A-92; appended).
  pub seal_keys: Vec<SealKeyRecord>,
  /// The NFSv4.1 sessions this partition's listener holds, by session id (A-127; appended).
  pub nfs_sessions: Vec<NfsSessionRecord>,
  /// The NFSv4.1 call records this partition's file-state changes carried, by session slot (A-127; appended).
  pub nfs_calls: Vec<NfsCallRecord>,
  /// The NFSv4 purges this partition's shard owes its owners, not yet acknowledged by all (2026-10-10; appended).
  pub nfs_purges: Vec<NfsPurgeRecord>,
  /// Each work volume's log since its last restatement (§4.16, D-18; 2026-11-04; appended).
  pub work_logs: Vec<WorkLog>,
  /// Each green's origin held as a reference to its base snapshot (A-217; 2026-11-05; appended).
  pub green_origin_refs: Vec<GreenOriginRef>,
  /// The names this partition holds for volumes other partitions hold, by name (A-236; appended).
  pub name_aliases: Vec<NameAliasRecord>,
  /// The CLI attachments whose term lapsed, oldest first (A-230; appended).
  pub expired_attachments: Vec<u64>,
  /// Each green's checkpoint (W3; §4.16; 2026-11-06; appended).
  pub green_checkpoints: Vec<GreenCheckpoint>,
  /// Each live work's hold on a version of a green of this partition (A-247; appended).
  pub work_floors: Vec<crate::catalog::WorkFloorRecord>,
  /// Each work's submit awaiting its settle (A-247; appended).
  pub pending_submits: Vec<crate::catalog::PendingSubmitRecord>,
}

/// One green's checkpoint, as a snapshot carries it (W3; §4.16): the version it is the green's state at, and its
/// encoding (`slates_merge::checkpoint`). The green's chain holds only the increments after `version`.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct GreenCheckpoint {
  /// The green.
  pub green: VolumeId,
  /// The version it is the green's state at.
  pub version: u64,
  /// The encoded checkpoint.
  pub bytes: Vec<u8>,
}

/// A green's origin as a reference (A-217): the base volume and snapshot it was walked from, and the BLAKE3 of the
/// origin's canonical encoding, which a recovery's walk of the same snapshot must reproduce.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct OriginRef {
  /// The base volume.
  pub volume: VolumeId,
  /// The snapshot of it.
  pub snapshot: SnapshotId,
  /// The origin's identity.
  pub identity: [u8; 32],
}

/// One green's origin reference, as a snapshot carries it.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct GreenOriginRef {
  /// The green.
  pub green: VolumeId,
  /// Its origin.
  pub origin: OriginRef,
}

/// One work volume's log: what recovery replays onto the work, in order.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct WorkLog {
  /// The work volume.
  pub work: VolumeId,
  /// The encoded entries, oldest first: at most one restatement, first, then edits and declarations.
  pub entries: Vec<Vec<u8>>,
}

/// The partition.
pub struct Partition {
  caps: PartitionCaps,
  volumes: Slab<VolumeRecord>,
  by_id: Art<Handle<VolumeRecord>>,
  by_name: Art<Handle<VolumeRecord>>,
  snapshots: Slab<SnapshotRecord>,
  snapshot_index: Art<Handle<SnapshotRecord>>,
  lineage: Art<LineageEdge>,
  leases_by_holder: Art<VolumeId>,
  lease_expiry: Wheel,
  lease_timers: BTreeMap<VolumeId, slates_rt::timer::TimerId>,
  attachments: Slab<AttachmentRecord>,
  attachment_index: Art<Handle<AttachmentRecord>>,
  completions: BTreeMap<(u64, u32), ClientWindow<Vec<u8>>>,
  /// A scalar reservation, retained even when a client ran no recorded verb.
  client_id_high_water: u32,
  grants: BTreeMap<u64, GrantRecord>,
  /// The enrolled consumers, by id (§4.13; bounded by the enrollments a human makes, one durable record
  /// each — never grown by a workload).
  consumers: BTreeMap<u64, ConsumerRecord>,
  landing_leases: Art<LandingLeaseRecord>,
  landings: BTreeMap<u64, LandingRecord>,
  audit: Vec<AuditRecord>,
  /// Each green volume's merge chain (§4.16): its accepted increments after its checkpoint (all of them while it holds
  /// none), encoded, in commit order, so recovery replays them to rebuild the green. Bounded by `green_chain_bytes`
  /// against the cap.
  green_chains: BTreeMap<VolumeId, Vec<Vec<u8>>>,
  /// Each checkpointed green's checkpoint (W3; §4.16): its version and its encoding, held in place of every increment
  /// up to that version. Counted in `green_chain_bytes` against the same cap. Removed with the green.
  green_checkpoints: BTreeMap<VolumeId, (u64, Vec<u8>)>,
  /// Each base-seeded green's origin (§4.16): its version-0 state, encoded, replayed before its
  /// chain on recovery. Counted in `green_chain_bytes` against the same cap.
  green_origins: BTreeMap<VolumeId, Vec<u8>>,
  /// The total bytes held across all green chains, origins and checkpoints, so a new increment can be refused at the
  /// cap before it is appended (bounded growth; a checkpoint folds the increments up to it, W3). The work logs count
  /// here too: one merge byte budget per partition.
  green_chain_bytes: usize,
  /// Each work volume's log since its last restatement (§4.16, D-18): what recovery replays onto the work so an
  /// acknowledged edit survives a daemon restart. Counted in `green_chain_bytes` against the same cap; a work's
  /// restatement (an accepted submit, a rebase) replaces its log, and its destroy reclaims it.
  work_logs: BTreeMap<VolumeId, Vec<Vec<u8>>>,
  /// Each green's origin held as a reference to its base snapshot (A-217): a few dozen bytes a green, whatever its
  /// tree, where `green_origins` held the whole tree's bytes. Removed with the green.
  green_origin_refs: BTreeMap<VolumeId, OriginRef>,
  /// The NFSv4 file state and clients (§4.6 A-37), each bounded before any operation is issued: the
  /// owner's file state bounds its opens and locks, the listener's session table its clients, and a
  /// replay reproduces only what they admitted.
  nfs_opens: BTreeMap<[u8; 12], NfsOpenRecord>,
  nfs_delegations: BTreeMap<[u8; 12], NfsDelegationRecord>,
  seal_keys: BTreeMap<SealKeyOwner, SealKeyRecord>,
  nfs_locks: BTreeMap<[u8; 12], NfsLockRecord>,
  nfs_clients: BTreeMap<u64, NfsClientRecord>,
  /// The listener's sessions (A-127), bounded by its session table: each client's sessions under its bound.
  nfs_sessions: BTreeMap<[u8; 16], NfsSessionRecord>,
  /// The call records file-state changes carried (A-127), one per session slot, bounded by the shard's records.
  nfs_calls: BTreeMap<[u8; 20], NfsCallRecord>,
  /// The purges this shard owes its owners (2026-10-10): one per client dropped or session destroyed here and not yet
  /// acknowledged by every owner, bounded by the drops a round could not settle.
  nfs_purges: BTreeSet<NfsPurgeRecord>,
  /// The names this partition holds for volumes other partitions hold (A-236): at most one a name, and at most
  /// `caps.volumes` of them, since each stands for a volume another partition holds under the same cap.
  name_aliases: BTreeMap<String, NameAliasRecord>,
  /// Each live work's base, by green and work (A-247), on the green's partition.
  work_floors: BTreeMap<(VolumeId, VolumeId), u64>,
  /// Each work's submit awaiting its settle, at most one a work (A-247).
  pending_submits: BTreeMap<VolumeId, crate::catalog::PendingSubmitRecord>,
  /// How many works hold each version of each green, so a green's reachable floor is one look-up per verdict rather
  /// than a walk of its works (A-247: a green with 10,000 works is judged once per accept).
  work_floor_counts: BTreeMap<(VolumeId, u64), u64>,
  /// The CLI attachments whose term lapsed, oldest first (A-230), so a later use is refused as expired rather than as
  /// unknown. At most `caps.attachments` of them: the oldest leaves when a lapse would pass it.
  expired_attachments: std::collections::VecDeque<u64>,
  nfs_instance: u32,
  /// The destroyed volumes whose tombstone this owner still owes its candidate holders (AUD-29-43), by volume,
  /// each with the register sequence it is written at. A tombstone keeps its volume's slot of the volume
  /// capacity until it is retired, so the table is bounded by the same derived cap.
  tombstones: BTreeMap<VolumeId, u64>,
}

impl std::fmt::Debug for Partition {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Partition")
      .field("volumes", &self.by_id.len())
      .field("snapshots", &self.snapshot_index.len())
      .field("attachments", &self.attachment_index.len())
      .field("grants", &self.grants.len())
      .field("landings", &self.landings.len())
      .finish()
  }
}

fn snapshot_key(volume: VolumeId, id: SnapshotId) -> Vec<u8> {
  let mut key = volume.bytes.to_vec();
  key.extend_from_slice(&id.value.to_be_bytes());
  key
}

impl Partition {
  /// An empty partition with `caps`, its wheel starting at `now_ns`.
  pub fn new(caps: PartitionCaps, now_ns: u64) -> Partition {
    Partition {
      caps,
      volumes: Slab::new(caps.segment_slots, caps.volumes),
      by_id: Art::new(),
      by_name: Art::new(),
      snapshots: Slab::new(caps.segment_slots, caps.snapshots),
      snapshot_index: Art::new(),
      lineage: Art::new(),
      leases_by_holder: Art::new(),
      lease_expiry: Wheel::new(caps.tick_ns, caps.timers, now_ns),
      lease_timers: BTreeMap::new(),
      attachments: Slab::new(caps.segment_slots, caps.attachments),
      attachment_index: Art::new(),
      completions: BTreeMap::new(),
      client_id_high_water: 0,
      grants: BTreeMap::new(),
      consumers: BTreeMap::new(),
      nfs_opens: BTreeMap::new(),
      nfs_delegations: BTreeMap::new(),
      seal_keys: BTreeMap::new(),
      nfs_locks: BTreeMap::new(),
      nfs_clients: BTreeMap::new(),
      nfs_sessions: BTreeMap::new(),
      nfs_calls: BTreeMap::new(),
      nfs_purges: BTreeSet::new(),
      name_aliases: BTreeMap::new(),
      work_floors: BTreeMap::new(),
      pending_submits: BTreeMap::new(),
      work_floor_counts: BTreeMap::new(),
      expired_attachments: std::collections::VecDeque::new(),
      nfs_instance: 0,
      landing_leases: Art::new(),
      landings: BTreeMap::new(),
      audit: Vec::new(),
      green_chains: BTreeMap::new(),
      green_origins: BTreeMap::new(),
      green_chain_bytes: 0,
      work_logs: BTreeMap::new(),
      green_origin_refs: BTreeMap::new(),
      green_checkpoints: BTreeMap::new(),
      tombstones: BTreeMap::new(),
    }
  }

  /// The register sequence a destroy of `id` would close its registers at (AUD-29-43): one past its head
  /// epoch and, for a green, past its newest version (version 0 is the origin, version `n` the `n`-th chain
  /// entry), so the tombstone is newer than every value the volume's registers ever held. `None` when the
  /// volume does not exist or the sequence is spent.
  pub fn tombstone_sequence(&self, id: VolumeId) -> Option<u64> {
    let record = self.volume(id)?;
    let newest_version = self
      .green_chains
      .get(&id)
      .map_or(0, |chain| u64::try_from(chain.len()).unwrap_or(u64::MAX));
    record.epoch.max(newest_version).checked_add(1)
  }

  /// The tombstone this owner still owes for the destroyed volume `id`, if any.
  pub fn tombstone(&self, id: VolumeId) -> Option<Tombstone> {
    self.tombstones.get(&id).map(|sequence| Tombstone {
      volume: id,
      sequence: *sequence,
    })
  }

  /// Every tombstone this owner still owes, by volume order.
  pub fn tombstones(&self) -> impl Iterator<Item = Tombstone> + '_ {
    self.tombstones.iter().map(|(volume, sequence)| Tombstone {
      volume: *volume,
      sequence: *sequence,
    })
  }

  /// Volumes and owed tombstones together: what the volume capacity bounds.
  fn volume_slots_used(&self) -> usize {
    self.by_id.len().saturating_add(self.tombstones.len())
  }

  /// The caps.
  pub fn caps(&self) -> PartitionCaps {
    self.caps
  }

  /// A green volume's merge chain (§4.16): its accepted increments after its checkpoint
  /// ([`Partition::green_checkpoint`]; all of them while it holds none), encoded, in commit order, so the chain's
  /// entry `n` is version `checkpoint + n + 1`. Empty for a green with no version past its checkpoint and for any
  /// non-green volume.
  pub fn green_chain(&self, green: VolumeId) -> &[Vec<u8>] {
    self.green_chains.get(&green).map_or(&[], Vec::as_slice)
  }

  /// A green's checkpoint (W3; §4.16): the version it is the green's state at and its encoding, or `None` while the
  /// green holds none (and for any non-green volume).
  pub fn green_checkpoint(&self, green: VolumeId) -> Option<(u64, &[u8])> {
    self
      .green_checkpoints
      .get(&green)
      .map(|(version, bytes)| (*version, bytes.as_slice()))
  }

  /// A green's head as its durable records hold it: its checkpoint's version and the increments after it.
  pub fn green_head(&self, green: VolumeId) -> u64 {
    let held = self
      .green_checkpoints
      .get(&green)
      .map_or(0, |(version, _)| *version);
    let chained = u64::try_from(self.green_chain(green).len()).unwrap_or(u64::MAX);
    held.saturating_add(chained)
  }

  /// What a checkpoint at `version` folds of `green`'s records: the increments it drops and the bytes it releases (the
  /// increments and the checkpoint it replaces), or the refusal when `version` is not past the checkpoint held or is
  /// past the head (W3).
  fn checkpoint_folds(&self, green: VolumeId, version: u64) -> Result<(usize, usize), DbError> {
    let (held, held_bytes) = self
      .green_checkpoints
      .get(&green)
      .map_or((0, 0), |(held, bytes)| (*held, bytes.len()));
    let chain = self.green_chain(green);
    let out_of_range = DbError::CheckpointOutOfRange {
      version,
      held,
      head: self.green_head(green),
    };
    let folded = version
      .checked_sub(held)
      .filter(|folded| *folded > 0)
      .and_then(|folded| usize::try_from(folded).ok())
      .filter(|folded| *folded <= chain.len())
      .ok_or(out_of_range)?;
    let dropped: usize = chain.iter().take(folded).map(Vec::len).sum();
    Ok((folded, dropped.saturating_add(held_bytes)))
  }

  /// The check of a green's checkpoint (W3): the green exists, the version is in range, and the bytes fit the merge
  /// budget. A checkpoint no larger than what it releases is admitted at any fill, so a partition at its cap can
  /// always fold back under it.
  fn check_green_checkpoint(
    &self,
    green: VolumeId,
    version: u64,
    bytes: usize,
  ) -> Result<(), DbError> {
    self.volume(green).ok_or(DbError::NotFound)?;
    let (_, released) = self.checkpoint_folds(green, version)?;
    if bytes <= released {
      return Ok(());
    }
    self.merge_bytes_fit(released, bytes)
  }

  /// Applies a green's checkpoint (W3): drops the increments up to its version and the checkpoint it replaces, and
  /// holds it. Unconditional, as every apply is: a version the check would refuse folds nothing it does not hold.
  fn apply_green_checkpoint(&mut self, green: VolumeId, version: u64, bytes: &[u8]) {
    let (folded, released) = self.checkpoint_folds(green, version).unwrap_or((0, 0));
    if let Some(chain) = self.green_chains.get_mut(&green) {
      chain.drain(..folded.min(chain.len()));
      if chain.is_empty() {
        self.green_chains.remove(&green);
      }
    }
    self.green_chain_bytes = self
      .green_chain_bytes
      .saturating_sub(released)
      .saturating_add(bytes.len());
    self
      .green_checkpoints
      .insert(green, (version, bytes.to_vec()));
  }

  /// A green volume's encoded origin (§4.16), or `None` for a green that started from scratch
  /// (and for any non-green volume).
  pub fn green_origin(&self, green: VolumeId) -> Option<&[u8]> {
    self.green_origins.get(&green).map(Vec::as_slice)
  }

  // ------------------------------------------------------------------ reads

  /// A volume by id.
  pub fn volume(&self, id: VolumeId) -> Option<&VolumeRecord> {
    let h = *self.by_id.get(&id.bytes)?;
    self.volumes.get(h).ok()
  }

  /// Whether attachment `id` was a CLI attachment whose term lapsed (A-230), among the most recent the partition keeps.
  pub fn attachment_expired(&self, id: u64) -> bool {
    self.expired_attachments.contains(&id)
  }

  /// The oldest version any live work of `green` is based on (A-247), or `None` when it has none.
  pub fn oldest_work_floor(&self, green: VolumeId) -> Option<u64> {
    self
      .work_floor_counts
      .range((green, 0)..=(green, u64::MAX))
      .next()
      .map(|((_, base), _)| *base)
  }

  /// The work's submit awaiting its settle, when it has one (A-247).
  pub fn pending_submit(&self, work: VolumeId) -> Option<&crate::catalog::PendingSubmitRecord> {
    self.pending_submits.get(&work)
  }

  /// Every work's hold on a green of this partition (A-247), for the orphan check at recovery.
  pub fn work_floors(&self) -> impl Iterator<Item = crate::catalog::WorkFloorRecord> + '_ {
    self
      .work_floors
      .iter()
      .map(|((green, work), base)| crate::catalog::WorkFloorRecord {
        green: *green,
        work: *work,
        base: *base,
      })
  }

  /// The work's floor on its green, when it holds one (A-247).
  pub fn work_floor(&self, green: VolumeId, work: VolumeId) -> Option<u64> {
    self.work_floors.get(&(green, work)).copied()
  }

  /// Records `work`'s hold of version `base` of `green`, in place of any it had (A-247), keeping the counts over them.
  fn set_work_floor(&mut self, green: VolumeId, work: VolumeId, base: u64) {
    if let Some(previous) = self.work_floors.insert((green, work), base) {
      self.release_floor_count(green, previous);
    }
    let held = self.work_floor_counts.entry((green, base)).or_insert(0);
    *held = held.saturating_add(1);
  }

  /// The guard of a work's records on a partition (A-247). A floor is one per live work, so it is bounded as the volumes
  /// are; a drop of none is a no-op (a destroy's message may meet the recovery's orphan check). A pending submit is one a
  /// work: a second while one waits is refused, so the first is never lost.
  fn check_work_record(&self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::WorkFloorSet { green, work, .. } => {
        if self.work_floors.contains_key(&(*green, *work))
          || self.work_floors.len() < self.caps.volumes
        {
          Ok(())
        } else {
          Err(DbError::Capacity {
            table: "work floors",
          })
        }
      }
      Op::WorkSubmitPending { record } => match self.pending_submits.get(&record.work) {
        Some(pending) if pending != record => Err(DbError::AlreadyExists {
          existing: record.work.bytes,
        }),
        _ => Ok(()),
      },
      _ => Ok(()),
    }
  }

  /// Applies a work's floor or pending-submit record (A-247).
  fn apply_work_floor(&mut self, op: &Op) {
    match op {
      Op::WorkFloorSet { green, work, base } => self.set_work_floor(*green, *work, *base),
      Op::WorkFloorDropped { green, work } => {
        if let Some(base) = self.work_floors.remove(&(*green, *work)) {
          self.release_floor_count(*green, base);
        }
      }
      Op::WorkSubmitPending { record } => {
        self.pending_submits.insert(record.work, record.clone());
      }
      Op::WorkSubmitSettled { work } => {
        self.pending_submits.remove(work);
      }
      _ => {}
    }
  }

  /// Releases one work's hold of `base`.
  fn release_floor_count(&mut self, green: VolumeId, base: u64) {
    if let Some(held) = self.work_floor_counts.get_mut(&(green, base)) {
      *held = held.saturating_sub(1);
      if *held == 0 {
        self.work_floor_counts.remove(&(green, base));
      }
    }
  }

  /// The alias this partition holds for `name`, if any (A-236): a name another partition's volume holds.
  pub fn name_alias(&self, name: &str) -> Option<&NameAliasRecord> {
    self.name_aliases.get(name)
  }

  /// Every alias this partition holds, by name (A-236): what a recovery reconciles.
  pub fn name_aliases(&self) -> impl Iterator<Item = &NameAliasRecord> {
    self.name_aliases.values()
  }

  /// A volume by name.
  pub fn volume_by_name(&self, name: &str) -> Option<&VolumeRecord> {
    let h = *self.by_name.get(name.as_bytes())?;
    self.volumes.get(h).ok()
  }

  /// Every volume, by id order.
  pub fn volumes(&self) -> Vec<&VolumeRecord> {
    self
      .by_id
      .iter()
      .filter_map(|(_, h)| self.volumes.get(*h).ok())
      .collect()
  }

  /// Volumes held.
  pub fn volume_count(&self) -> usize {
    self.by_id.len()
  }

  /// A snapshot.
  pub fn snapshot(&self, volume: VolumeId, id: SnapshotId) -> Option<&SnapshotRecord> {
    let h = *self.snapshot_index.get(&snapshot_key(volume, id))?;
    self.snapshots.get(h).ok()
  }

  /// A volume's snapshots, by id order.
  pub fn snapshots_of(&self, volume: VolumeId) -> Vec<&SnapshotRecord> {
    self
      .snapshot_index
      .scan_prefix(&volume.bytes)
      .into_iter()
      .filter_map(|(_, h)| self.snapshots.get(*h).ok())
      .collect()
  }

  /// A clone's lineage edge.
  pub fn lineage(&self, child: VolumeId) -> Option<&LineageEdge> {
    self.lineage.get(&child.bytes)
  }

  /// The volume a principal holds a lease on.
  pub fn lease_of(&self, holder: &Principal) -> Option<VolumeId> {
    self.leases_by_holder.get(&holder.key()).copied()
  }

  /// The attachment ids held by one SDK client (a walk of the table, bounded by its cap; the
  /// reclaim of a dead client asks, never a hot path).
  pub fn attachments_of_client(&self, client: u32) -> Vec<u64> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| matches!(a.consumer, Consumer::Sdk { client: c } if c == client))
      .map(|(_, a)| a.id)
      .collect()
  }

  /// The attachments an enrolled consumer holds on this partition, whatever their volume or form (§4.13: a
  /// consumer's revocation ends every one of them, AUD-29-84). A walk of the table, bounded by its cap; a
  /// revocation asks, never a hot path.
  pub fn attachments_held_by_consumer(&self, consumer: u64) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(
        |(_, a)| matches!(a.principal, Principal::Consumer { consumer: c, .. } if c == consumer),
      )
      .map(|(_, a)| a)
      .collect()
  }

  /// Every attachment this partition holds, in slab order: what a sweep over all of them walks (A-230's lapse).
  pub fn attachments(&self) -> impl Iterator<Item = &AttachmentRecord> {
    self.attachments.iter().map(|(_, record)| record)
  }

  /// The attachments other hosts' clients hold here (§4.10, A-222: those of a host that left the configuration end).
  /// A walk of the table, bounded by its cap; a configuration's install asks, never a hot path.
  pub fn remote_attachments(&self) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| matches!(a.consumer, Consumer::Remote { .. }))
      .map(|(_, a)| a)
      .collect()
  }

  /// The attachments of a volume (a walk of the table, bounded by its cap; a detach and a
  /// recovery ask, never a hot path).
  pub fn attachments_of(&self, volume: VolumeId) -> Vec<&AttachmentRecord> {
    self
      .attachments
      .iter()
      .filter(|(_, a)| a.volume == volume)
      .map(|(_, a)| a)
      .collect()
  }

  /// A binding borrows one live bridge mount of the same volume and principal, within its
  /// rights (§4.6). This guard also prevents dependency chains and cycles before the append.
  fn check_mount_dependency(&self, record: &AttachmentRecord) -> Result<(), DbError> {
    let Consumer::Mount { attachment } = record.consumer else {
      return Ok(());
    };
    let parent = self.attachment(attachment).ok_or(DbError::NotFound)?;
    if !matches!(parent.consumer, Consumer::Bridge)
      || parent.volume != record.volume
      || parent.principal != record.principal
      || !matches!(record.form, crate::catalog::AttachForm::Oci { .. })
      || record.snapshot.is_some()
      || (record.rights.read && !parent.rights.read)
      || (record.rights.write && !parent.rights.write)
      || record.rights.admin
    {
      return Err(DbError::NotFound);
    }
    Ok(())
  }

  /// Removes one already-checked attachment while applying its recorded transition.
  fn remove_attachment(&mut self, id: u64) -> Result<(), DbError> {
    let handle = self
      .attachment_index
      .remove(&id.to_be_bytes())
      .ok_or(DbError::NotFound)?;
    self.attachments.remove(handle)?;
    Ok(())
  }

  /// The highest attachment id the partition holds, or `None` when it holds none (a walk of the table,
  /// bounded by its cap; asked once at boot, so a fresh counter never re-mints a recovered id).
  pub fn highest_attachment(&self) -> Option<u64> {
    self.attachments.iter().map(|(_, a)| a.id).max()
  }

  /// An attachment.
  pub fn attachment(&self, id: u64) -> Option<&AttachmentRecord> {
    let h = *self.attachment_index.get(&id.to_be_bytes())?;
    self.attachments.get(h).ok()
  }

  /// What a client's window knows about a sequence. The key is `(origin, client)`: `origin` is this node's
  /// own host for a local client and the authenticated forwarding peer for a cross-node forwarded verb, so a
  /// forwarded request never collides with a local client sharing its per-node id (§4.8 "Lookup").
  pub fn completion(&self, origin: u64, client: u32, sequence: u32) -> Seen<Vec<u8>> {
    self
      .completions
      .get(&(origin, client))
      .map_or(Seen::New, |w| w.lookup(sequence))
  }

  /// How many completion records a client's window retains unacknowledged here (zero for a client with none).
  pub fn retained_completions(&self, origin: u64, client: u32) -> usize {
    self
      .completions
      .get(&(origin, client))
      .map_or(0, ClientWindow::retained)
  }

  /// The ids at or below a client's watermark it still owes (its window's open ids), in order: what an origin relays
  /// beside the watermark with a forwarded write, so the owner keeps them open too (§4.9).
  pub fn owed_completions(&self, origin: u64, client: u32) -> Vec<u32> {
    self
      .completions
      .get(&(origin, client))
      .map_or_else(Vec::new, |w| w.owed().collect())
  }

  /// The highest sequence a client has acknowledged (its completion window's watermark), or `None` if it has
  /// acknowledged nothing. The origin reads its local client's watermark (`origin` = its own host) to relay
  /// it with a forwarded write, so the owner prunes the forwarded client's completions the same way a local
  /// client's are pruned — by acknowledgement, not left to grow (§4.9 RIFL; banned item 8).
  pub fn acknowledged_up_to(&self, origin: u64, client: u32) -> Option<u32> {
    self
      .completions
      .get(&(origin, client))
      .and_then(ClientWindow::acknowledged_up_to)
  }

  /// The allocation floor after recovery: issued ids and this host's retained completion
  /// identities. Foreign hosts' client numbers never advance the local allocator.
  pub fn client_id_high_water(&self, origin: u64) -> u32 {
    self
      .completions
      .range((origin, 0)..=(origin, u32::MAX))
      .next_back()
      .map_or(self.client_id_high_water, |((_, client), _)| {
        self.client_id_high_water.max(*client)
      })
  }

  /// The grants made for a principal (a walk of the table; a `slates grants` read, never a
  /// hot path).
  pub fn grants_of(&self, principal: &Principal) -> Vec<&GrantRecord> {
    self
      .grants
      .values()
      .filter(|g| &g.principal == principal)
      .collect()
  }

  /// A grant.
  pub fn grant(&self, id: u64) -> Option<&GrantRecord> {
    self.grants.get(&id)
  }

  /// Every grant, by id order: what a restarted shard rebuilds its runtime grants from (AUD-29-06).
  pub fn grants(&self) -> impl Iterator<Item = &GrantRecord> {
    self.grants.values()
  }

  /// An enrolled consumer's record (§4.13), or `None` if no such consumer was ever enrolled.
  pub fn consumer(&self, id: u64) -> Option<&ConsumerRecord> {
    self.consumers.get(&id)
  }

  /// The NFSv4 opens held at this partition's files (§4.6 A-37).
  pub fn nfs_opens(&self) -> impl Iterator<Item = &NfsOpenRecord> {
    self.nfs_opens.values()
  }

  /// The NFSv4 delegations recorded at this partition's files (§4.6 A-78).
  pub fn nfs_delegations(&self) -> impl Iterator<Item = &NfsDelegationRecord> {
    self.nfs_delegations.values()
  }

  /// The NFSv4 lock states held at this partition's files.
  pub fn nfs_locks(&self) -> impl Iterator<Item = &NfsLockRecord> {
    self.nfs_locks.values()
  }

  /// The NFSv4 clients this partition's listener holds.
  pub fn nfs_clients(&self) -> impl Iterator<Item = &NfsClientRecord> {
    self.nfs_clients.values()
  }

  /// The NFSv4.1 sessions this partition's listener holds (A-127).
  pub fn nfs_sessions(&self) -> impl Iterator<Item = &NfsSessionRecord> {
    self.nfs_sessions.values()
  }

  /// The NFSv4.1 call records this partition's file-state changes carried (A-127).
  pub fn nfs_calls(&self) -> impl Iterator<Item = &NfsCallRecord> {
    self.nfs_calls.values()
  }

  /// The NFSv4 purges this partition's shard owes its owners (2026-10-10).
  pub fn nfs_purges(&self) -> impl Iterator<Item = &NfsPurgeRecord> {
    self.nfs_purges.iter()
  }

  /// The partition's NFSv4 instance: the last one a daemon life advanced to.
  pub fn nfs_instance(&self) -> u32 {
    self.nfs_instance
  }

  /// The sealing key recorded for `owner` at this partition, wrapped by its parent (A-92).
  pub fn seal_key(&self, owner: &SealKeyOwner) -> Option<&SealKeyRecord> {
    self.seal_keys.get(owner)
  }

  /// Every sealing key recorded at this partition, by owner.
  pub fn seal_keys(&self) -> impl Iterator<Item = &SealKeyRecord> {
    self.seal_keys.values()
  }

  /// Applies one NFSv4 record operation (A-37): an upsert, a clear, or a client's purge.
  fn apply_nfs(&mut self, op: &Op) {
    match op {
      Op::NfsOpenSet { record } => {
        self.nfs_opens.insert(record.other, record.clone());
      }
      Op::NfsOpenCleared { other } => {
        self.nfs_opens.remove(other);
      }
      Op::NfsDelegationSet { record } => {
        self.nfs_delegations.insert(record.other, record.clone());
      }
      Op::NfsDelegationCleared { other } => {
        self.nfs_delegations.remove(other);
      }
      Op::NfsLockSet { record } => {
        self.nfs_locks.insert(record.other, record.clone());
      }
      Op::NfsLockCleared { other } => {
        self.nfs_locks.remove(other);
      }
      Op::NfsClientStateCleared { clientid } => {
        self
          .nfs_opens
          .retain(|_, record| record.clientid != *clientid);
        self
          .nfs_locks
          .retain(|_, record| record.clientid != *clientid);
        self
          .nfs_delegations
          .retain(|_, record| record.clientid != *clientid);
        // Its call records go with its state (A-127).
        self
          .nfs_calls
          .retain(|_, record| record.clientid != *clientid);
      }
      Op::NfsClientSet { record } => {
        self.nfs_clients.insert(record.clientid, record.clone());
      }
      Op::NfsClientCleared { clientid } => {
        self.nfs_clients.remove(clientid);
        // A dropped client's sessions go with it (A-127).
        self
          .nfs_sessions
          .retain(|_, record| record.clientid != *clientid);
      }
      Op::NfsSessionSet { record } => {
        self.nfs_sessions.insert(record.sessionid, record.clone());
      }
      Op::NfsSessionCleared { sessionid } => {
        self.nfs_sessions.remove(sessionid);
      }
      Op::NfsCallSet { record } => {
        self.nfs_calls.insert(record.key, record.clone());
      }
      Op::NfsCallsCleared { sessionid } => {
        self
          .nfs_calls
          .retain(|key, _| key.get(..sessionid.len()) != Some(&sessionid[..]));
      }
      Op::NfsPurgeOwed { record } => {
        self.nfs_purges.insert(*record);
      }
      Op::NfsPurgeSettled { record } => {
        self.nfs_purges.remove(record);
      }
      Op::NfsInstanceAdvanced { instance } => {
        self.nfs_instance = self.nfs_instance.max(*instance);
      }
      _ => {}
    }
  }
}

/// A consumer id in the 16-byte `existing` shape `AlreadyExists` carries (the id in the low eight
/// bytes, little-endian), so a duplicate enrollment names the consumer it collided with.
fn consumer_id_bytes(consumer: u64) -> [u8; 16] {
  let mut out = [0u8; 16];
  out[..8].copy_from_slice(&consumer.to_le_bytes());
  out
}

impl Partition {
  /// The landing lease on a target.
  pub fn landing_lease(&self, target: &str) -> Option<&LandingLeaseRecord> {
    self.landing_leases.get(target.as_bytes())
  }

  /// Every landing lease record (the control partition's, §4.15; AUD-29-03).
  pub fn landing_leases(&self) -> impl Iterator<Item = &LandingLeaseRecord> {
    self.landing_leases.iter().map(|(_, lease)| lease)
  }

  /// A landing.
  pub fn landing(&self, id: u64) -> Option<&LandingRecord> {
    self.landings.get(&id)
  }

  /// The highest landing id the partition holds, or `None` when it holds none — asked once at boot, so
  /// a fresh counter never re-mints a recovered landing's id (the records are durable and the guard
  /// refuses a duplicate).
  pub fn highest_landing(&self) -> Option<u64> {
    self.landings.keys().next_back().copied()
  }

  /// The audit records retained in memory (the ring is the durable stream).
  pub fn audit(&self) -> &[AuditRecord] {
    &self.audit
  }

  // ------------------------------------------------------------------ the guard

  /// [`Partition::check`] for a name alias (A-236) and a CLI attachment's term (A-230).
  fn check_alias_or_term(&self, op: &Op) -> Result<(), DbError> {
    match op {
      // A name is held once on its partition: by a volume there, or by one alias for another partition's volume.
      Op::NameAliasReserved { record } => {
        if let Some(existing) = self.volume_by_name(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: existing.id.bytes,
          });
        }
        if let Some(alias) = self.name_alias(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: alias.volume.bytes,
          });
        }
        if self.name_aliases.len() >= self.caps.volumes {
          return Err(DbError::Capacity {
            table: "name aliases",
          });
        }
        Ok(())
      }
      Op::NameAliasConfirmed { name } | Op::NameAliasReleased { name } => {
        self.name_alias(name).map(|_| ()).ok_or(DbError::NotFound)
      }
      // Only a CLI attachment holds a term.
      Op::AttachmentRenewed { id, .. } | Op::AttachmentExpired { id } => match self.attachment(*id)
      {
        Some(record) if matches!(record.consumer, Consumer::Termed { .. }) => Ok(()),
        _ => Err(DbError::NotFound),
      },
      _ => Ok(()),
    }
  }

  /// The guard the live path runs before appending: refuses what the log must never record.
  pub fn check(&self, op: &Op, now_ns: u64) -> Result<(), DbError> {
    match op {
      Op::VolumeCreated { record } => {
        if let Some(existing) = self.volume(record.id) {
          return Err(DbError::AlreadyExists {
            existing: existing.id.bytes,
          });
        }
        if let Some(existing) = self.volume_by_name(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: existing.id.bytes,
          });
        }
        // A name this partition holds for another partition's volume is that volume's (A-236), confirmed or not.
        if let Some(alias) = self.name_alias(&record.name) {
          return Err(DbError::AlreadyExists {
            existing: alias.volume.bytes,
          });
        }
        if self.tombstones.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.id.bytes,
          });
        }
        if self.volume_slots_used() >= self.caps.volumes {
          return Err(DbError::Capacity { table: "volumes" });
        }
        Ok(())
      }
      Op::VolumeDestroyed { id } => {
        self.volume(*id).ok_or(DbError::NotFound)?;
        self
          .tombstone_sequence(*id)
          .map(|_| ())
          .ok_or(DbError::Capacity {
            table: "tombstone sequence",
          })
      }
      Op::TombstoneAdopted { tombstone } => {
        if self.volume(tombstone.volume).is_some()
          || self.tombstones.contains_key(&tombstone.volume)
        {
          return Err(DbError::AlreadyExists {
            existing: tombstone.volume.bytes,
          });
        }
        if self.volume_slots_used() >= self.caps.volumes {
          return Err(DbError::Capacity { table: "volumes" });
        }
        Ok(())
      }
      Op::TombstoneRetired { id } => self.tombstones.get(id).map(|_| ()).ok_or(DbError::NotFound),
      Op::VolumeStateChanged { id, .. }
      | Op::VolumeResized { id, .. }
      | Op::VolumeAccounted { id, .. }
      | Op::VolumeHeadAdvanced { id, .. }
      | Op::AccessChanged { id, .. }
      | Op::LiveShippingChanged { id, .. }
      | Op::LiveHoldersCommitted { id, .. }
      | Op::VolumeRebased { id, .. } => self.volume(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::SnapshotTaken { record } => {
        self.volume(record.volume).ok_or(DbError::NotFound)?;
        if self.snapshot(record.volume, record.id).is_some() {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        if self.snapshot_index.len() >= self.caps.snapshots {
          return Err(DbError::Capacity { table: "snapshots" });
        }
        Ok(())
      }
      Op::SnapshotPlaced { volume, id, .. }
      | Op::SnapshotIdentified { volume, id, .. }
      | Op::SnapshotDestroyed { volume, id } => self
        .snapshot(*volume, *id)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::LineageAdded { edge } => {
        self.volume(edge.child).ok_or(DbError::NotFound)?;
        Ok(())
      }
      Op::LeaseTaken { volume, lease } => self.check_lease(*volume, lease, now_ns),
      Op::LeaseReleased { volume } => self.volume(*volume).map(|_| ()).ok_or(DbError::NotFound),
      Op::AttachmentAdded { record } => {
        self.volume(record.volume).ok_or(DbError::NotFound)?;
        self.check_mount_dependency(record)?;
        if self.attachment(record.id).is_some() {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        if self.attachment_index.len() >= self.caps.attachments {
          return Err(DbError::Capacity {
            table: "attachments",
          });
        }
        Ok(())
      }
      Op::AttachmentRemoved { id } => self.attachment(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::AttachmentBound { id, .. } => self.attachment(*id).map(|_| ()).ok_or(DbError::NotFound),
      // Only an attachment presenting a snapshot is re-pinned; the head is not a version to move from.
      Op::AttachmentRepinned { id, .. } => self
        .attachment(*id)
        .filter(|record| record.snapshot.is_some())
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::CompletionRecorded { record } => {
        match self.completion(record.origin, record.client, record.sequence) {
          Seen::Acknowledged => Err(DbError::StaleCompletion {
            acknowledged_up_to: self
              .completions
              .get(&(record.origin, record.client))
              .and_then(ClientWindow::acknowledged_up_to)
              .unwrap_or(0),
          }),
          Seen::New | Seen::Completed(_) | Seen::Awaiting { .. } => Ok(()),
        }
      }
      Op::CompletionAwaiting {
        origin,
        client,
        sequence,
        ..
      } => match self.completion(*origin, *client, *sequence) {
        Seen::Acknowledged => Err(DbError::StaleCompletion {
          acknowledged_up_to: self
            .completions
            .get(&(*origin, *client))
            .and_then(ClientWindow::acknowledged_up_to)
            .unwrap_or(0),
        }),
        Seen::New | Seen::Completed(_) | Seen::Awaiting { .. } => Ok(()),
      },
      Op::CompletionsAcknowledged { .. } | Op::ClientIdReserved { .. } => Ok(()),
      // The NFSv4 records mirror state their owner already bounded and admitted (A-37).
      Op::NfsOpenSet { .. }
      | Op::NfsOpenCleared { .. }
      | Op::NfsDelegationSet { .. }
      | Op::NfsDelegationCleared { .. }
      | Op::NfsLockSet { .. }
      | Op::NfsLockCleared { .. }
      | Op::NfsClientStateCleared { .. }
      | Op::NfsClientSet { .. }
      | Op::NfsClientCleared { .. }
      | Op::NfsSessionSet { .. }
      | Op::NfsSessionCleared { .. }
      | Op::NfsCallSet { .. }
      | Op::NfsCallsCleared { .. }
      | Op::NfsPurgeOwed { .. }
      | Op::NfsPurgeSettled { .. } => Ok(()),
      Op::WorkFloorSet { .. }
      | Op::WorkFloorDropped { .. }
      | Op::WorkSubmitPending { .. }
      | Op::WorkSubmitSettled { .. } => self.check_work_record(op),
      Op::NameAliasReserved { .. }
      | Op::NameAliasConfirmed { .. }
      | Op::NameAliasReleased { .. }
      | Op::AttachmentRenewed { .. }
      | Op::AttachmentExpired { .. } => self.check_alias_or_term(op),
      // A key is made once: a second record for its owner would leave two keys sealing one owner's content. A record
      // is hyper-seal's fixed length, so a malformed one never enters the log.
      Op::SealKeySet { record } => {
        let fits = match record.owner {
          SealKeyOwner::Recipient => (1..=SEAL_RECIPIENT_RECORD_MAX).contains(&record.record.len()),
          _ => record.record.len() == SEAL_KEY_RECORD_BYTES,
        };
        if !fits {
          Err(DbError::SealKeyMalformed {
            len: record.record.len(),
          })
        } else if self.seal_keys.contains_key(&record.owner) {
          Err(DbError::AlreadyExists {
            existing: record.id,
          })
        } else {
          Ok(())
        }
      }
      Op::SealKeyDestroyed { owner } => {
        if self.seal_keys.contains_key(owner) {
          Ok(())
        } else {
          Err(DbError::NotFound)
        }
      }
      // An instance only moves forward: an earlier one would let a new id repeat an old one.
      Op::NfsInstanceAdvanced { instance } => {
        if *instance > self.nfs_instance {
          Ok(())
        } else {
          Err(DbError::NfsInstanceRegressed {
            current: self.nfs_instance,
          })
        }
      }
      Op::GrantIssued { record } => {
        if self.grants.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        Ok(())
      }
      Op::GrantStateChanged { id, .. } => self.grant(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::ConsumerEnrolled { record } => {
        if self.consumers.contains_key(&record.consumer) {
          return Err(DbError::AlreadyExists {
            existing: consumer_id_bytes(record.consumer),
          });
        }
        Ok(())
      }
      Op::ConsumerRevoked { consumer } => self
        .consumers
        .get(consumer)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::LandingLeaseTaken { record } => self.check_landing_lease(record, now_ns),
      Op::LandingLeaseReleased { target } => self
        .landing_lease(target)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::LandingRecorded { record } => {
        if self.landings.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        Ok(())
      }
      Op::LandingStateChanged { id, .. } => self.landing(*id).map(|_| ()).ok_or(DbError::NotFound),
      Op::AuditAppended { .. } => Ok(()),
      Op::GreenAdvanced { green, increment } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        // Bounded growth (§4.16; CLAUDE.md): the chain grows per accepted version, so a new increment
        // is refused once the partition's green chains would exceed their byte budget — the typed
        // refusal at the bound; a checkpoint (W3) folds the increments below the reachable floor.
        if self.green_chain_bytes.saturating_add(increment.len()) > self.caps.green_chain_bytes {
          return Err(DbError::Capacity {
            table: "green_chains",
          });
        }
        Ok(())
      }
      Op::WorkLogged { work, entry } => {
        self.volume(*work).ok_or(DbError::NotFound)?;
        self.merge_bytes_fit(0, entry.len())
      }
      Op::GreenOriginHeld { green, origin } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        self
          .snapshot(origin.volume, origin.snapshot)
          .ok_or(DbError::NotFound)?;
        // Version 0, recorded once and only before any increment, whatever form the origin takes.
        if self.green_origins.contains_key(green)
          || self.green_origin_refs.contains_key(green)
          || self.green_head(*green) > 0
        {
          return Err(DbError::AlreadyExists {
            existing: green.bytes,
          });
        }
        Ok(())
      }
      Op::WorkRestated { work, entry } => {
        self.volume(*work).ok_or(DbError::NotFound)?;
        self.merge_bytes_fit(self.work_log_bytes(*work), entry.len())
      }
      Op::GreenCheckpointed {
        green,
        version,
        checkpoint,
      } => self.check_green_checkpoint(*green, *version, checkpoint.len()),
      Op::GreenOriginated { green, origin } => {
        self.volume(*green).ok_or(DbError::NotFound)?;
        // An origin is version 0: recorded once, and only before any increment — a chain that has
        // advanced was based on whatever origin it had, so a later origin would rewrite history.
        if self.green_origins.contains_key(green) || self.green_head(*green) > 0 {
          return Err(DbError::AlreadyExists {
            existing: green.bytes,
          });
        }
        if self.green_chain_bytes.saturating_add(origin.len()) > self.caps.green_chain_bytes {
          return Err(DbError::Capacity {
            table: "green_chains",
          });
        }
        Ok(())
      }
    }
  }

  /// A lease is taken by a new holder only after the current one expired or was released,
  /// with the epoch one above the current; the holder renews with the same epoch (D-16).
  fn check_lease(&self, volume: VolumeId, lease: &LeaseRecord, now_ns: u64) -> Result<(), DbError> {
    let record = self.volume(volume).ok_or(DbError::NotFound)?;
    match &record.lease {
      None => Ok(()),
      Some(current) if current.holder == lease.holder => {
        if lease.epoch < current.epoch {
          Err(DbError::StaleLease {
            current: current.epoch,
          })
        } else {
          Ok(())
        }
      }
      Some(current) if current.expires_ns <= now_ns => {
        if lease.epoch <= current.epoch {
          Err(DbError::StaleLease {
            current: current.epoch,
          })
        } else {
          Ok(())
        }
      }
      Some(current) => Err(DbError::LeaseHeld {
        epoch: current.epoch,
      }),
    }
  }

  /// One holder per target (§4.15 step 4; AUD-29-03): a take by another attempt is refused while the current
  /// lease is unexpired, and a lease's fencing generation never goes backwards. The holder is one landing attempt,
  /// so its own take of a live lease is that attempt's renewal (the keepalive, AUD-29-25). Before 2026-09-29 the
  /// holder was the caller's session, so two landings of one principal took each other's lease; from then until
  /// 2026-11-07 the attempt's own renewal was refused while its lease held, so it only renewed once lapsed.
  fn check_landing_lease(&self, record: &LandingLeaseRecord, now_ns: u64) -> Result<(), DbError> {
    match self.landing_lease(&record.target) {
      // A live lease is its holder's: another attempt is refused, and the holder itself takes it again to renew it
      // (§4.15 "The lease keepalive", AUD-29-25) under the newer generation the next arm requires. Before 2026-11-07
      // the holder was refused too, so a running landing's renewal only succeeded once its lease had lapsed.
      Some(held) if held.expires_ns > now_ns && held.holder != record.holder => {
        Err(DbError::LeaseHeld {
          epoch: held.generation,
        })
      }
      Some(held) if record.generation <= held.generation => Err(DbError::StaleLease {
        current: held.generation,
      }),
      _ => Ok(()),
    }
  }

  // ------------------------------------------------------------------ the transition

  /// Applies a recorded operation; unconditional, so replay is deterministic. A record the
  /// operation names that does not exist (a corrupted history) is a typed refusal.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::VolumeCreated { record } => self.insert_volume(record.clone()),
      Op::VolumeStateChanged { id, state } => self.update_volume(*id, |v| v.state = *state),
      Op::VolumeResized { id, size } => self.update_volume(*id, |v| {
        v.policy.size = *size;
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeRebased { id, base } => self.update_volume(*id, |v| {
        v.base = base.clone();
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeAccounted {
        id,
        referenced_bytes,
        unique_bytes,
      } => self.update_volume(*id, |v| {
        v.referenced_bytes = *referenced_bytes;
        v.unique_bytes = *unique_bytes;
      }),
      Op::VolumeHeadAdvanced { id, head, epoch } => self.update_volume(*id, |v| {
        v.head = *head;
        v.epoch = *epoch;
      }),
      Op::AccessChanged { id, access } => self.update_volume(*id, |v| {
        v.access = access.clone();
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The catalog register carries the flag (a successor keeps shipping), so its version moves as an access change's.
      Op::LiveShippingChanged { id, live_shipped } => self.update_volume(*id, |v| {
        v.policy.live_shipped = *live_shipped;
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      // The catalog register carries the holder set (A-240), so its version moves as the flag's does.
      Op::LiveHoldersCommitted { id, holders } => self.update_volume(*id, |v| {
        v.policy.live_holders = Some(holders.clone());
        v.catalog_version = v.catalog_version.saturating_add(1);
      }),
      Op::VolumeDestroyed { id } => {
        // The tombstone takes the volume's slot: its sequence is read before the record goes.
        let sequence = self.tombstone_sequence(*id);
        let tenant = self.volume(*id).map(|volume| volume.owner.tenant());
        self.remove_volume(*id)?;
        if let Some(sequence) = sequence {
          self.tombstones.insert(*id, sequence);
        }
        // The volume's lineage key goes with it, in the same record (A-92; seal.md §3.1, cryptographic erase): its
        // one wrapped copy was here, so every chunk sealed under it, on any holder, opens for no one after this.
        self
          .seal_keys
          .remove(&SealKeyOwner::Lineage { volume: *id });
        self.seal_keys.remove(&SealKeyOwner::Naming { volume: *id });
        // A tenant key wraps only its volumes' lineage keys, so with the tenant's last volume on this partition it is
        // erased too, in the same record. The scan is bounded by the partition's volume cap, once per destroy.
        if let Some(account) = tenant
          && !self
            .volumes()
            .iter()
            .any(|volume| volume.owner.tenant() == account)
        {
          self.seal_keys.remove(&SealKeyOwner::Tenant { account });
        }
        Ok(())
      }
      Op::TombstoneAdopted { tombstone } => {
        self.tombstones.insert(tombstone.volume, tombstone.sequence);
        Ok(())
      }
      Op::TombstoneRetired { id } => {
        self.tombstones.remove(id);
        Ok(())
      }
      Op::SnapshotTaken { record } => self.insert_snapshot(record.clone()),
      Op::SnapshotPlaced { volume, id, placed } => {
        let h = *self
          .snapshot_index
          .get(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.get_mut(h)?.placed = placed.clone();
        Ok(())
      }
      Op::SnapshotIdentified {
        volume,
        id,
        identity,
      } => {
        let h = *self
          .snapshot_index
          .get(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.get_mut(h)?.identity = Some(*identity);
        Ok(())
      }
      Op::SnapshotDestroyed { volume, id } => {
        let h = self
          .snapshot_index
          .remove(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.remove(h)?;
        Ok(())
      }
      Op::LineageAdded { edge } => {
        self.lineage.insert(&edge.child.bytes, edge.clone());
        Ok(())
      }
      Op::LeaseTaken { volume, lease } => self.set_lease(*volume, Some(lease.clone())),
      Op::LeaseReleased { volume } => self.set_lease(*volume, None),
      Op::AttachmentAdded { record } => {
        let h = self.attachments.insert(record.clone())?;
        self.attachment_index.insert(&record.id.to_be_bytes(), h);
        Ok(())
      }
      Op::ClientIdReserved { client } => {
        self.client_id_high_water = self.client_id_high_water.max(*client);
        Ok(())
      }
      Op::NfsOpenSet { .. }
      | Op::NfsOpenCleared { .. }
      | Op::NfsDelegationSet { .. }
      | Op::NfsDelegationCleared { .. }
      | Op::NfsLockSet { .. }
      | Op::NfsLockCleared { .. }
      | Op::NfsClientStateCleared { .. }
      | Op::NfsClientSet { .. }
      | Op::NfsClientCleared { .. }
      | Op::NfsSessionSet { .. }
      | Op::NfsSessionCleared { .. }
      | Op::NfsCallSet { .. }
      | Op::NfsCallsCleared { .. }
      | Op::NfsPurgeOwed { .. }
      | Op::NfsPurgeSettled { .. }
      | Op::NfsInstanceAdvanced { .. } => {
        self.apply_nfs(op);
        Ok(())
      }
      Op::AttachmentRenewed { id, expires_ns } => {
        let h = *self
          .attachment_index
          .get(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
        record.consumer = Consumer::Termed {
          expires_ns: *expires_ns,
        };
        Ok(())
      }
      Op::AttachmentExpired { id } => {
        self.remove_attachment(*id)?;
        if self.expired_attachments.len() >= self.caps.attachments.max(1) {
          self.expired_attachments.pop_front();
        }
        self.expired_attachments.push_back(*id);
        Ok(())
      }
      Op::WorkFloorSet { .. }
      | Op::WorkFloorDropped { .. }
      | Op::WorkSubmitPending { .. }
      | Op::WorkSubmitSettled { .. } => {
        self.apply_work_floor(op);
        Ok(())
      }
      Op::NameAliasReserved { record } => {
        self
          .name_aliases
          .insert(record.name.clone(), record.clone());
        Ok(())
      }
      Op::NameAliasConfirmed { name } => {
        let alias = self.name_aliases.get_mut(name).ok_or(DbError::NotFound)?;
        alias.confirmed = true;
        Ok(())
      }
      Op::NameAliasReleased { name } => self
        .name_aliases
        .remove(name)
        .map(|_| ())
        .ok_or(DbError::NotFound),
      Op::SealKeySet { record } => {
        self.seal_keys.insert(record.owner, record.clone());
        Ok(())
      }
      Op::SealKeyDestroyed { owner } => {
        self.seal_keys.remove(owner);
        Ok(())
      }
      Op::AttachmentBound { id, path } => {
        let h = *self
          .attachment_index
          .get(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
        // A FUSE form or a WinFsp volume already names its mount point (the daemon mounted it); a bound mount keeps
        // its identity and scope, and a scoped mount its scope, when it is bound; any other host mount is bound at a
        // chosen path.
        if record.form.daemon_mount_point().is_none() {
          record.form = match (record.form.bound_uid(), record.form.scope()) {
            (Some(uid), scope) => crate::catalog::AttachForm::BoundMount {
              uid,
              scope,
              mount_point: Some(path.clone()),
            },
            (None, Some(scope)) => crate::catalog::AttachForm::ScopedMount {
              scope,
              mount_point: Some(path.clone()),
            },
            (None, None) => crate::catalog::AttachForm::ChosenPath { path: path.clone() },
          };
        }
        Ok(())
      }
      Op::AttachmentRepinned { id, snapshot } => {
        // The mount and its borrowers move together, so a bind never claims a version its mount no longer
        // presents (`oci::Binding::consumer` matches them).
        let moved: Vec<u64> = self
          .attachments
          .iter()
          .filter(|(_, record)| {
            record.id == *id
              || matches!(record.consumer, crate::catalog::Consumer::Mount { attachment } if attachment == *id)
          })
          .map(|(_, record)| record.id)
          .collect();
        for moved in moved {
          let h = *self
            .attachment_index
            .get(&moved.to_be_bytes())
            .ok_or(DbError::NotFound)?;
          let record = self.attachments.get_mut(h).map_err(|_| DbError::NotFound)?;
          record.snapshot = Some(*snapshot);
        }
        Ok(())
      }
      Op::AttachmentRemoved { id } => {
        // One recorded transition removes the mount and its borrowers. A dependency is
        // one level deep by admission, so no recursive walk or intermediate orphan exists.
        let dependent: Vec<u64> = if self
          .attachment(*id)
          .is_some_and(|record| matches!(record.consumer, Consumer::Bridge))
        {
          self.attachments.iter()
            .filter(|(_, record)| matches!(record.consumer, Consumer::Mount { attachment } if attachment == *id))
            .map(|(_, record)| record.id)
            .collect()
        } else {
          Vec::new()
        };
        for dependent in dependent {
          self.remove_attachment(dependent)?;
        }
        self.remove_attachment(*id)?;
        Ok(())
      }
      Op::CompletionRecorded { record } => {
        self
          .completions
          .entry((record.origin, record.client))
          .or_default()
          .record(record.sequence, record.result.clone());
        Ok(())
      }
      Op::CompletionAwaiting {
        origin,
        client,
        sequence,
        green,
        version,
      } => {
        self
          .completions
          .entry((*origin, *client))
          .or_default()
          .await_version(*sequence, green.bytes, *version);
        Ok(())
      }
      Op::CompletionsAcknowledged {
        origin,
        client,
        up_to,
        owed,
      } => {
        self
          .completions
          .entry((*origin, *client))
          .or_default()
          .acknowledge(*up_to, owed);
        Ok(())
      }
      Op::GrantIssued { record } => {
        self.grants.insert(record.id, record.clone());
        Ok(())
      }
      Op::GrantStateChanged { id, state } => {
        self.grants.get_mut(id).ok_or(DbError::NotFound)?.state = *state;
        Ok(())
      }
      Op::ConsumerEnrolled { record } => {
        self.consumers.insert(record.consumer, record.clone());
        Ok(())
      }
      Op::ConsumerRevoked { consumer } => {
        self
          .consumers
          .get_mut(consumer)
          .ok_or(DbError::NotFound)?
          .revoked = true;
        Ok(())
      }
      Op::LandingLeaseTaken { record } => {
        self
          .landing_leases
          .insert(record.target.as_bytes(), record.clone());
        Ok(())
      }
      Op::LandingLeaseReleased { target } => {
        self
          .landing_leases
          .remove(target.as_bytes())
          .ok_or(DbError::NotFound)?;
        Ok(())
      }
      Op::LandingRecorded { record } => {
        self.landings.insert(record.id, record.clone());
        Ok(())
      }
      Op::LandingStateChanged {
        id,
        state,
        written,
        conflicts,
      } => {
        let landing = self.landings.get_mut(id).ok_or(DbError::NotFound)?;
        landing.state = *state;
        landing.written = *written;
        landing.conflicts = *conflicts;
        Ok(())
      }
      Op::AuditAppended { record } => {
        self.audit.push(record.clone());
        Ok(())
      }
      Op::GreenAdvanced { green, increment } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(increment.len());
        self
          .green_chains
          .entry(*green)
          .or_default()
          .push(increment.clone());
        Ok(())
      }
      Op::GreenOriginated { green, origin } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(origin.len());
        self.green_origins.insert(*green, origin.clone());
        Ok(())
      }
      Op::GreenOriginHeld { green, origin } => {
        self.green_origin_refs.insert(*green, *origin);
        Ok(())
      }
      Op::WorkLogged { work, entry } => {
        self.green_chain_bytes = self.green_chain_bytes.saturating_add(entry.len());
        self.work_logs.entry(*work).or_default().push(entry.clone());
        Ok(())
      }
      Op::WorkRestated { work, entry } => {
        let released = self.work_log_bytes(*work);
        self.green_chain_bytes = self
          .green_chain_bytes
          .saturating_sub(released)
          .saturating_add(entry.len());
        self.work_logs.insert(*work, vec![entry.clone()]);
        Ok(())
      }
      Op::GreenCheckpointed {
        green,
        version,
        checkpoint,
      } => {
        self.apply_green_checkpoint(*green, *version, checkpoint);
        Ok(())
      }
    }
  }

  /// The bytes a work's log holds now.
  fn work_log_bytes(&self, work: VolumeId) -> usize {
    self
      .work_logs
      .get(&work)
      .map_or(0, |log| log.iter().map(Vec::len).sum())
  }

  /// Whether replacing `released` bytes of the merge budget with `added` keeps it within the partition's cap: the
  /// typed refusal at the bound otherwise (bounded growth, CLAUDE.md banned item 8).
  fn merge_bytes_fit(&self, released: usize, added: usize) -> Result<(), DbError> {
    if self
      .green_chain_bytes
      .saturating_sub(released)
      .saturating_add(added)
      > self.caps.green_chain_bytes
    {
      return Err(DbError::Capacity {
        table: "green_chains",
      });
    }
    Ok(())
  }

  /// A green's origin reference (A-217), if its origin is held as one.
  pub fn green_origin_ref(&self, green: VolumeId) -> Option<&OriginRef> {
    self.green_origin_refs.get(&green)
  }

  /// Every green origin reference, by green (A-217): what recovery counts as pins, and what a volume's destroy checks.
  pub fn green_origin_refs(&self) -> impl Iterator<Item = (&VolumeId, &OriginRef)> {
    self.green_origin_refs.iter()
  }

  /// A work volume's log since its last restatement, oldest first (§4.16, D-18): what recovery replays onto it.
  pub fn work_log(&self, work: VolumeId) -> &[Vec<u8>] {
    self.work_logs.get(&work).map_or(&[], Vec::as_slice)
  }

  fn insert_volume(&mut self, record: VolumeRecord) -> Result<(), DbError> {
    let id = record.id;
    let name = record.name.clone();
    let lease = record.lease.clone();
    let h = self.volumes.insert(record)?;
    self.by_id.insert(&id.bytes, h);
    self.by_name.insert(name.as_bytes(), h);
    if let Some(lease) = lease {
      self.index_lease(id, &lease)?;
    }
    Ok(())
  }

  fn update_volume(
    &mut self,
    id: VolumeId,
    f: impl FnOnce(&mut VolumeRecord),
  ) -> Result<(), DbError> {
    let h = *self.by_id.get(&id.bytes).ok_or(DbError::NotFound)?;
    f(self.volumes.get_mut(h)?);
    Ok(())
  }

  fn remove_volume(&mut self, id: VolumeId) -> Result<(), DbError> {
    let h = self.by_id.remove(&id.bytes).ok_or(DbError::NotFound)?;
    let record = self.volumes.remove(h)?;
    self.by_name.remove(record.name.as_bytes());
    if let Some(lease) = &record.lease {
      self.leases_by_holder.remove(&lease.holder.key());
    }
    if let Some(t) = self.lease_timers.remove(&id) {
      let _ = self.lease_expiry.cancel(t);
    }
    let keys: Vec<Vec<u8>> = self
      .snapshot_index
      .scan_prefix(&id.bytes)
      .into_iter()
      .map(|(key, _)| key)
      .collect();
    for key in keys {
      if let Some(h) = self.snapshot_index.remove(&key) {
        let _ = self.snapshots.remove(h);
      }
    }
    self.lineage.remove(&id.bytes);
    // A destroyed green's merge chain is reclaimed with it (its increments can never be replayed for
    // a volume that no longer exists), returning its bytes to the chain budget.
    if let Some(chain) = self.green_chains.remove(&id) {
      let bytes: usize = chain.iter().map(Vec::len).sum();
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(bytes);
    }
    if let Some(origin) = self.green_origins.remove(&id) {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(origin.len());
    }
    self.green_origin_refs.remove(&id);
    if let Some((_, checkpoint)) = self.green_checkpoints.remove(&id) {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(checkpoint.len());
    }
    // A destroyed work's log can never be replayed either.
    let log = self.work_log_bytes(id);
    if self.work_logs.remove(&id).is_some() {
      self.green_chain_bytes = self.green_chain_bytes.saturating_sub(log);
    }
    Ok(())
  }

  fn insert_snapshot(&mut self, record: SnapshotRecord) -> Result<(), DbError> {
    let key = snapshot_key(record.volume, record.id);
    let h = self.snapshots.insert(record)?;
    self.snapshot_index.insert(&key, h);
    Ok(())
  }

  fn set_lease(&mut self, volume: VolumeId, lease: Option<LeaseRecord>) -> Result<(), DbError> {
    let h = *self.by_id.get(&volume.bytes).ok_or(DbError::NotFound)?;
    if let Some(old) = self.volumes.get(h)?.lease.clone() {
      self.leases_by_holder.remove(&old.holder.key());
    }
    if let Some(t) = self.lease_timers.remove(&volume) {
      let _ = self.lease_expiry.cancel(t);
    }
    self.volumes.get_mut(h)?.lease = lease.clone();
    if let Some(lease) = lease {
      self.index_lease(volume, &lease)?;
    }
    Ok(())
  }

  fn index_lease(&mut self, volume: VolumeId, lease: &LeaseRecord) -> Result<(), DbError> {
    self.leases_by_holder.insert(&lease.holder.key(), volume);
    let h = *self.by_id.get(&volume.bytes).ok_or(DbError::NotFound)?;
    let word = u64::from(h.index());
    let timer = self
      .lease_expiry
      .insert(lease.expires_ns, word)
      .map_err(|_| DbError::Capacity {
        table: "lease timers",
      })?;
    self.lease_timers.insert(volume, timer);
    Ok(())
  }

  /// The volumes whose leases expired by `now_ns` (the caller records `LeaseReleased` for
  /// each; the wheel pops, it never scans).
  pub fn expired_leases(&mut self, now_ns: u64) -> Vec<VolumeId> {
    let mut fired = Vec::new();
    self.lease_expiry.advance(now_ns, &mut fired);
    let mut out = Vec::new();
    for word in fired {
      let index = u32::try_from(word).unwrap_or(u32::MAX);
      let Some(generation) = self.volumes.generation_at(index) else {
        continue;
      };
      let h = Handle::from_raw(index, generation);
      if let Ok(v) = self.volumes.get(h)
        && v.lease.as_ref().is_some_and(|l| l.expires_ns <= now_ns)
      {
        out.push(v.id);
      }
    }
    out
  }

  // ------------------------------------------------------------------ snapshots

  /// The whole state as one value, labelled with `seq`.
  pub fn to_snapshot(&self, seq: u64) -> PartitionSnapshot {
    PartitionSnapshot {
      client_id_high_water: self.client_id_high_water,
      seq,
      volumes: self.volumes().into_iter().cloned().collect(),
      snapshots: self
        .snapshot_index
        .iter()
        .filter_map(|(_, h)| self.snapshots.get(*h).ok().cloned())
        .collect(),
      lineage: self.lineage.iter().map(|(_, e)| e.clone()).collect(),
      attachments: self
        .attachment_index
        .iter()
        .filter_map(|(_, h)| self.attachments.get(*h).ok().cloned())
        .collect(),
      completions: self
        .completions
        .iter()
        .map(|((origin, client), w)| ClientCompletions {
          origin: *origin,
          client: *client,
          acknowledged_up_to: w.acknowledged_up_to(),
          owed: w.owed().collect(),
          records: w
            .retained_entries()
            .map(|(sequence, result)| CompletionRecord {
              origin: *origin,
              client: *client,
              sequence,
              result: result.clone(),
            })
            .collect(),
          awaiting: w
            .awaiting_entries()
            .map(
              |(sequence, object, version)| crate::catalog::AwaitingCompletion {
                sequence,
                green: VolumeId { bytes: object },
                version,
              },
            )
            .collect(),
        })
        .collect(),
      grants: self.grants.values().cloned().collect(),
      landing_leases: self.landing_leases.iter().map(|(_, l)| l.clone()).collect(),
      landings: self.landings.values().cloned().collect(),
      audit: self.audit.clone(),
      consumers: self.consumers.values().cloned().collect(),
      nfs_opens: self.nfs_opens.values().cloned().collect(),
      nfs_delegations: self.nfs_delegations.values().cloned().collect(),
      seal_keys: self.seal_keys.values().cloned().collect(),
      nfs_locks: self.nfs_locks.values().cloned().collect(),
      nfs_clients: self.nfs_clients.values().cloned().collect(),
      nfs_sessions: self.nfs_sessions.values().cloned().collect(),
      nfs_calls: self.nfs_calls.values().cloned().collect(),
      nfs_purges: self.nfs_purges.iter().copied().collect(),
      green_origin_refs: self
        .green_origin_refs
        .iter()
        .map(|(green, origin)| GreenOriginRef {
          green: *green,
          origin: *origin,
        })
        .collect(),
      green_checkpoints: self
        .green_checkpoints
        .iter()
        .map(|(green, (version, bytes))| GreenCheckpoint {
          green: *green,
          version: *version,
          bytes: bytes.clone(),
        })
        .collect(),
      work_logs: self
        .work_logs
        .iter()
        .map(|(work, entries)| WorkLog {
          work: *work,
          entries: entries.clone(),
        })
        .collect(),
      name_aliases: self.name_aliases.values().cloned().collect(),
      work_floors: self.work_floors().collect(),
      pending_submits: self.pending_submits.values().cloned().collect(),
      expired_attachments: self.expired_attachments.iter().copied().collect(),
      nfs_instance: self.nfs_instance,
      tombstones: self.tombstones().collect(),
      green_chains: {
        // Every green with a chain or an origin, in id order, so a base-seeded green with no
        // increment yet is carried too.
        let greens: std::collections::BTreeSet<VolumeId> = self
          .green_chains
          .keys()
          .chain(self.green_origins.keys())
          .copied()
          .collect();
        greens
          .into_iter()
          .map(|green| GreenChain {
            green,
            increments: self.green_chains.get(&green).cloned().unwrap_or_default(),
            origin: self.green_origins.get(&green).cloned(),
          })
          .collect()
      },
    }
  }

  /// Restores the enrolled consumers (§4.13) and the owed tombstones (AUD-29-43) a snapshot carries.
  fn restore_consumers_and_tombstones(&mut self, snapshot: &PartitionSnapshot) {
    for c in &snapshot.consumers {
      self.consumers.insert(c.consumer, c.clone());
    }
    for tombstone in &snapshot.tombstones {
      self.tombstones.insert(tombstone.volume, tombstone.sequence);
    }
  }

  /// Restores the expired CLI attachments a snapshot carries (A-230).
  fn restore_expired_attachments(&mut self, snapshot: &PartitionSnapshot) {
    self.expired_attachments = snapshot.expired_attachments.iter().copied().collect();
  }

  /// Restores the name aliases a snapshot carries (A-236).
  fn restore_name_aliases(&mut self, snapshot: &PartitionSnapshot) {
    for record in &snapshot.name_aliases {
      self
        .name_aliases
        .insert(record.name.clone(), record.clone());
    }
  }

  /// Restores the NFSv4 records and instance a snapshot carries (§4.6 A-37).
  fn restore_nfs(&mut self, snapshot: &PartitionSnapshot) {
    for record in &snapshot.nfs_opens {
      self.nfs_opens.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_delegations {
      self.nfs_delegations.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_locks {
      self.nfs_locks.insert(record.other, record.clone());
    }
    for record in &snapshot.nfs_clients {
      self.nfs_clients.insert(record.clientid, record.clone());
    }
    for record in &snapshot.nfs_sessions {
      self.nfs_sessions.insert(record.sessionid, record.clone());
    }
    for record in &snapshot.nfs_calls {
      self.nfs_calls.insert(record.key, record.clone());
    }
    for record in &snapshot.nfs_purges {
      self.nfs_purges.insert(*record);
    }
    self.nfs_instance = snapshot.nfs_instance;
    for record in &snapshot.seal_keys {
      self.seal_keys.insert(record.owner, record.clone());
    }
  }

  fn restore_completions(&mut self, completions: &[ClientCompletions]) {
    for c in completions {
      let w = self.completions.entry((c.origin, c.client)).or_default();
      for r in &c.records {
        w.record(r.sequence, r.result.clone());
      }
      for a in &c.awaiting {
        w.await_version(a.sequence, a.green.bytes, a.version);
      }
      if let Some(up_to) = c.acknowledged_up_to {
        // A fresh window: the snapshot's open ids are restored exactly (each at or below its watermark, as every
        // acknowledgement applied had them).
        w.acknowledge(up_to, &c.owed);
      }
    }
  }

  /// A partition rebuilt from a snapshot: every index and the wheel rebuilt.
  pub fn from_snapshot(
    snapshot: &PartitionSnapshot,
    caps: PartitionCaps,
    now_ns: u64,
  ) -> Result<Partition, DbError> {
    let mut p = Partition::new(caps, now_ns);
    p.client_id_high_water = snapshot.client_id_high_water;
    for v in &snapshot.volumes {
      p.insert_volume(v.clone())?;
    }
    for s in &snapshot.snapshots {
      p.insert_snapshot(s.clone())?;
    }
    for e in &snapshot.lineage {
      p.lineage.insert(&e.child.bytes, e.clone());
    }
    for a in &snapshot.attachments {
      let h = p.attachments.insert(a.clone())?;
      p.attachment_index.insert(&a.id.to_be_bytes(), h);
    }
    p.restore_completions(&snapshot.completions);
    for g in &snapshot.grants {
      p.grants.insert(g.id, g.clone());
    }
    for l in &snapshot.landing_leases {
      p.landing_leases.insert(l.target.as_bytes(), l.clone());
    }
    for l in &snapshot.landings {
      p.landings.insert(l.id, l.clone());
    }
    p.audit = snapshot.audit.clone();
    p.restore_merge(snapshot);
    p.restore_nfs(snapshot);
    p.restore_name_aliases(snapshot);
    for pending in &snapshot.pending_submits {
      p.pending_submits.insert(pending.work, pending.clone());
    }
    for floor in &snapshot.work_floors {
      p.set_work_floor(floor.green, floor.work, floor.base);
    }
    p.restore_expired_attachments(snapshot);
    p.restore_consumers_and_tombstones(snapshot);
    Ok(p)
  }

  /// Restores the merge plane a snapshot carries (§4.16): every green's chain and origin, every held origin reference
  /// (A-217) and every work's log (A-207), each charged to the merge byte budget as when it was recorded.
  fn restore_merge(&mut self, snapshot: &PartitionSnapshot) {
    for chain in &snapshot.green_chains {
      self.restore_green_chain(chain);
    }
    for held in &snapshot.green_origin_refs {
      self.green_origin_refs.insert(held.green, held.origin);
    }
    for checkpoint in &snapshot.green_checkpoints {
      self.green_chain_bytes = self
        .green_chain_bytes
        .saturating_add(checkpoint.bytes.len());
      self.green_checkpoints.insert(
        checkpoint.green,
        (checkpoint.version, checkpoint.bytes.clone()),
      );
    }
    for log in &snapshot.work_logs {
      self.green_chain_bytes = self
        .green_chain_bytes
        .saturating_add(log.entries.iter().map(Vec::len).sum());
      self.work_logs.insert(log.work, log.entries.clone());
    }
  }

  /// Restores one green's chain and origin from a snapshot, charging both to the chain budget; a
  /// green carried with no increment (a seeded green before its first merge) gets no empty chain
  /// entry, so the restored tables equal the live ones.
  fn restore_green_chain(&mut self, chain: &GreenChain) {
    let bytes: usize = chain.increments.iter().map(Vec::len).sum();
    self.green_chain_bytes = self.green_chain_bytes.saturating_add(bytes);
    if !chain.increments.is_empty() {
      self
        .green_chains
        .insert(chain.green, chain.increments.clone());
    }
    if let Some(origin) = &chain.origin {
      self.green_chain_bytes = self.green_chain_bytes.saturating_add(origin.len());
      self.green_origins.insert(chain.green, origin.clone());
    }
  }
}
