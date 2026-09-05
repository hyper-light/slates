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

use std::collections::BTreeMap;

use slates_mem::{Handle, Slab};
use slates_rt::timer::Wheel;
use slates_wire::Wire;
use slates_wire::request::{ClientWindow, Seen};

use crate::Art;
use crate::catalog::{
  AttachmentRecord, AuditRecord, CompletionRecord, GrantRecord, LandingLeaseRecord, LandingRecord,
  LeaseRecord, LineageEdge, Principal, SnapshotId, SnapshotRecord, VolumeId, VolumeRecord,
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
}

/// One client's completion state, as the snapshot carries it.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ClientCompletions {
  /// The client.
  pub client: u32,
  /// Every sequence up to and including this one is acknowledged.
  pub acknowledged_up_to: Option<u32>,
  /// The retained completions.
  pub records: Vec<CompletionRecord>,
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
  completions: BTreeMap<u32, ClientWindow<Vec<u8>>>,
  grants: BTreeMap<u64, GrantRecord>,
  landing_leases: Art<LandingLeaseRecord>,
  landings: BTreeMap<u64, LandingRecord>,
  audit: Vec<AuditRecord>,
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
      grants: BTreeMap::new(),
      landing_leases: Art::new(),
      landings: BTreeMap::new(),
      audit: Vec::new(),
    }
  }

  /// The caps.
  pub fn caps(&self) -> PartitionCaps {
    self.caps
  }

  // ------------------------------------------------------------------ reads

  /// A volume by id.
  pub fn volume(&self, id: VolumeId) -> Option<&VolumeRecord> {
    let h = *self.by_id.get(&id.bytes)?;
    self.volumes.get(h).ok()
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

  /// An attachment.
  pub fn attachment(&self, id: u64) -> Option<&AttachmentRecord> {
    let h = *self.attachment_index.get(&id.to_be_bytes())?;
    self.attachments.get(h).ok()
  }

  /// What a client's window knows about a sequence.
  pub fn completion(&self, client: u32, sequence: u32) -> Seen<Vec<u8>> {
    self
      .completions
      .get(&client)
      .map_or(Seen::New, |w| w.lookup(sequence))
  }

  /// A grant.
  pub fn grant(&self, id: u64) -> Option<&GrantRecord> {
    self.grants.get(&id)
  }

  /// The landing lease on a target.
  pub fn landing_lease(&self, target: &str) -> Option<&LandingLeaseRecord> {
    self.landing_leases.get(target.as_bytes())
  }

  /// A landing.
  pub fn landing(&self, id: u64) -> Option<&LandingRecord> {
    self.landings.get(&id)
  }

  /// The audit records retained in memory (the ring is the durable stream).
  pub fn audit(&self) -> &[AuditRecord] {
    &self.audit
  }

  // ------------------------------------------------------------------ the guard

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
        if self.by_id.len() >= self.caps.volumes {
          return Err(DbError::Capacity { table: "volumes" });
        }
        Ok(())
      }
      Op::VolumeStateChanged { id, .. }
      | Op::VolumeResized { id, .. }
      | Op::VolumeAccounted { id, .. }
      | Op::VolumeHeadAdvanced { id, .. }
      | Op::AccessChanged { id, .. }
      | Op::VolumeDestroyed { id } => self.volume(*id).map(|_| ()).ok_or(DbError::NotFound),
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
      Op::SnapshotPlaced { volume, id, .. } | Op::SnapshotDestroyed { volume, id } => self
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
      Op::CompletionRecorded { record } => match self.completion(record.client, record.sequence) {
        Seen::Acknowledged => Err(DbError::StaleCompletion {
          acknowledged_up_to: self
            .completions
            .get(&record.client)
            .and_then(ClientWindow::acknowledged_up_to)
            .unwrap_or(0),
        }),
        Seen::New | Seen::Completed(_) => Ok(()),
      },
      Op::CompletionsAcknowledged { .. } => Ok(()),
      Op::GrantIssued { record } => {
        if self.grants.contains_key(&record.id) {
          return Err(DbError::AlreadyExists {
            existing: record.volume.bytes,
          });
        }
        Ok(())
      }
      Op::GrantStateChanged { id, .. } => self.grant(*id).map(|_| ()).ok_or(DbError::NotFound),
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

  /// One holder per target: another session takes the landing lease only after expiry.
  fn check_landing_lease(&self, record: &LandingLeaseRecord, now_ns: u64) -> Result<(), DbError> {
    match self.landing_lease(&record.target) {
      Some(held) if held.holder != record.holder && held.expires_ns > now_ns => {
        Err(DbError::LeaseHeld {
          epoch: held.generation,
        })
      }
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
      Op::VolumeResized { id, size } => self.update_volume(*id, |v| v.policy.size = *size),
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
      Op::AccessChanged { id, access } => self.update_volume(*id, |v| v.access = access.clone()),
      Op::VolumeDestroyed { id } => self.remove_volume(*id),
      Op::SnapshotTaken { record } => self.insert_snapshot(record.clone()),
      Op::SnapshotPlaced { volume, id, placed } => {
        let h = *self
          .snapshot_index
          .get(&snapshot_key(*volume, *id))
          .ok_or(DbError::NotFound)?;
        self.snapshots.get_mut(h)?.placed = placed.clone();
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
      Op::AttachmentRemoved { id } => {
        let h = self
          .attachment_index
          .remove(&id.to_be_bytes())
          .ok_or(DbError::NotFound)?;
        self.attachments.remove(h)?;
        Ok(())
      }
      Op::CompletionRecorded { record } => {
        self
          .completions
          .entry(record.client)
          .or_default()
          .record(record.sequence, record.result.clone());
        Ok(())
      }
      Op::CompletionsAcknowledged { client, up_to } => {
        self
          .completions
          .entry(*client)
          .or_default()
          .acknowledge(*up_to);
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
    }
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
        .map(|(client, w)| ClientCompletions {
          client: *client,
          acknowledged_up_to: w.acknowledged_up_to(),
          records: w
            .retained_entries()
            .map(|(sequence, result)| CompletionRecord {
              client: *client,
              sequence,
              result: result.clone(),
            })
            .collect(),
        })
        .collect(),
      grants: self.grants.values().cloned().collect(),
      landing_leases: self.landing_leases.iter().map(|(_, l)| l.clone()).collect(),
      landings: self.landings.values().cloned().collect(),
      audit: self.audit.clone(),
    }
  }

  fn restore_completions(&mut self, completions: &[ClientCompletions]) {
    for c in completions {
      let w = self.completions.entry(c.client).or_default();
      for r in &c.records {
        w.record(r.sequence, r.result.clone());
      }
      if let Some(up_to) = c.acknowledged_up_to {
        w.acknowledge(up_to);
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
    Ok(p)
  }
}
