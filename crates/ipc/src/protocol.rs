//! The ring protocol's bodies (§4.4's lifecycle verbs as the client sends them, §4.9's rules:
//! a schema hash in front of every body, a closed refusal taxonomy) and their framing into
//! slots: a body that fits the slot's payload travels inline; a larger one travels through the
//! client's bulk area, in the chunk that belongs to the slot's ring index, and the slot names
//! it by offset and length. Both ends share this module, so the daemon and the Rust client
//! (and, through it, the SDKs and the CLI) speak one thing.

use slates_wire::Wire;

use crate::error::IpcError;
use crate::region::ClientRegion;
use crate::slot::{PAYLOAD_BYTES, Slot, SlotKind};

/// A volume id on the wire.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default, Hash, PartialOrd, Ord)]
pub struct VolumeId {
  /// The bytes.
  pub bytes: [u8; 16],
}

/// A snapshot id on the wire.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SnapshotId {
  /// The value.
  pub value: u64,
}

/// Bounded or dynamic (§4.2).
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeClass {
  /// Reserved at creation.
  Bounded {
    /// The limit in bytes.
    limit: u64,
  },
  /// Grown from measured rates up to a maximum.
  Dynamic {
    /// The maximum in bytes.
    max: u64,
  },
}

/// The name-equivalence policy.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamePolicy {
  /// Byte-exact names.
  Exact,
  /// Folded names, as APFS.
  Fold,
}

/// The durability scope of `await placed` (§4.8 D-18): the owner's region, or the mirror.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
  /// The home region: `f + 1` of the owner's candidates (the local append at `f = 0`).
  Region,
  /// The mirror region (absent at `f = 0`, refused `Unsupported`).
  Mirror,
}

/// What a client attaches for.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intent {
  /// Reads only.
  Read,
  /// Reads and writes: takes the volume's lease.
  Write,
}

/// A request body.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum RequestBody {
  /// Create a volume.
  Create {
    /// The name.
    name: String,
    /// The size class.
    size: SizeClass,
    /// The name policy.
    names: NamePolicy,
    /// Whether unlocked memory refuses.
    require_locked: bool,
    /// A host directory to overlay, or nothing for a scratch volume.
    base: Option<String>,
  },
  /// Take a snapshot.
  Snapshot {
    /// The volume.
    volume: VolumeId,
  },
  /// Clone a snapshot into a new volume.
  Clone {
    /// The volume.
    volume: VolumeId,
    /// The snapshot.
    snapshot: SnapshotId,
    /// The clone's name.
    name: String,
  },
  /// Attach (the client form: the reply names the path a bridge will publish).
  Attach {
    /// The volume.
    volume: VolumeId,
    /// A snapshot, for a read-only attachment to it.
    snapshot: Option<SnapshotId>,
    /// The intent.
    intent: Intent,
  },
  /// Detach.
  Detach {
    /// The attachment.
    attachment: u64,
  },
  /// Resize.
  Resize {
    /// The volume.
    volume: VolumeId,
    /// The new size class.
    size: SizeClass,
  },
  /// Destroy.
  Destroy {
    /// The volume.
    volume: VolumeId,
  },
  /// Status.
  Status {
    /// The volume.
    volume: VolumeId,
  },
  /// List the caller's volumes.
  List,
  /// Acknowledge completions up to a sequence (releases their records).
  Acknowledge {
    /// Every sequence up to and including this one.
    up_to: u32,
  },
  /// The base entry as the disk holds it now (§4.4 `read_base`).
  ReadBase {
    /// The volume.
    volume: VolumeId,
    /// The path.
    path: String,
  },
  /// Re-witness drifted entries (§4.4 `rewitness`).
  Rewitness {
    /// The volume.
    volume: VolumeId,
    /// The paths, or every drifted entry.
    paths: Option<Vec<String>>,
  },
  /// Pin base subtrees (§4.4 `pin`).
  Pin {
    /// The volume.
    volume: VolumeId,
    /// The paths, or the whole base.
    paths: Option<Vec<String>>,
  },
  /// A grant: refused on this channel by kind (§4.13, AC-2.8); the kind exists so the refusal
  /// is typed and counted.
  Grant {
    /// The request the grant would cover.
    request: u64,
  },
  /// The daemon's own status (§4.14 `slates.status`: every shard's counters and health
  /// signals, and the anchor's view of the daemon as the segment holds it).
  DaemonStatus,
  /// Await a durability scope for a volume's head, or a snapshot (§4.8 D-18): returns when the
  /// scope is placed. At `f = 0` the region is the local append (already placed) and the
  /// mirror is refused `Unsupported`.
  AwaitPlaced {
    /// The volume.
    volume: VolumeId,
    /// A snapshot, or the head when none.
    snapshot: Option<SnapshotId>,
    /// The scope.
    scope: Scope,
  },
}

/// A health signal (§4.14): a value and how old it is.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct Signal {
  /// The name, dotted (`catalog.volumes`).
  pub name: String,
  /// The value.
  pub value: u64,
  /// How old the value is, in nanoseconds (zero for one computed now).
  pub freshness_ns: u64,
}

/// A refusal kind's count.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct RefusalCount {
  /// The kind's name.
  pub kind: String,
  /// Refusals of it.
  pub count: u64,
}

/// One shard's part of the daemon's status.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct ShardReport {
  /// The partition.
  pub partition: u16,
  /// Live clients.
  pub clients: u32,
  /// Volumes owned.
  pub volumes: u64,
  /// Requests served (completions recorded).
  pub served: u64,
  /// Refusals by kind.
  pub refusals: Vec<RefusalCount>,
  /// Records replayed at the last start.
  pub replayed_records: u64,
  /// The last replay's duration.
  pub replay_ns: u64,
  /// Whether the last replay cut a torn tail.
  pub torn_tail: bool,
  /// The shard's reserve.
  pub reserve_bytes: u64,
  /// Bytes committed to bounded volumes.
  pub committed_bytes: u64,
  /// The health signals.
  pub signals: Vec<Signal>,
}

/// The daemon's status.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct DaemonReport {
  /// The daemon's process id.
  pub pid: u32,
  /// The segment's generation (daemon starts over it).
  pub generation: u64,
  /// Restarts the anchor made.
  pub restarts: u64,
  /// The age of the daemon's last heartbeat in the segment.
  pub heartbeat_age_ns: u64,
  /// Clients found dead and reclaimed.
  pub clients_reaped: u64,
  /// Connects refused at the client bound.
  pub clients_refused: u64,
  /// Every shard's part, by partition.
  pub shards: Vec<ShardReport>,
}

/// A volume's summary in a listing.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct VolumeSummary {
  /// The id.
  pub id: VolumeId,
  /// The name.
  pub name: String,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes unique.
  pub unique_bytes: u64,
  /// Whether it is an overlay.
  pub overlay: bool,
}

/// A volume's placement (§4.8, D-18): every reply carries it from the first version, so the
/// fleet parts of Phase 8 change no interface. At `f = 0` a volume's head is placed the moment
/// the owner holds it and the mirror does not exist.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlacedState {
  /// Whether the head is placed in the region (`f + 1` regional acknowledgements).
  pub region: bool,
  /// The age of the newest record the mirror acknowledged, or nothing where no mirror exists.
  pub mirror_age_ns: Option<u64>,
  /// The owner's host epoch (its authority; 1 on a laptop).
  pub host_epoch: u64,
}

/// A volume's status (§4.4 `status`).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub struct StatusReport {
  /// The id.
  pub id: VolumeId,
  /// The name.
  pub name: String,
  /// Bytes referenced.
  pub referenced_bytes: u64,
  /// Bytes unique.
  pub unique_bytes: u64,
  /// The lease epoch, when held.
  pub lease_epoch: Option<u64>,
  /// Attachments.
  pub attachments: u32,
  /// The head snapshot.
  pub head: SnapshotId,
  /// Drifted entries, for an overlay.
  pub drifted: Vec<String>,
  /// The watcher state name, for an overlay.
  pub watcher: String,
  /// Snapshots held.
  pub snapshots: u32,
  /// The placement of the head (§4.8, D-18).
  pub placed: PlacedState,
}

/// The closed refusal taxonomy on the wire (§4.4, §4.13).
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// Not found.
  NotFound,
  /// The name exists; the original's id.
  AlreadyExists {
    /// The existing volume.
    existing: VolumeId,
  },
  /// A stale lease epoch.
  StaleLease {
    /// The current epoch.
    current: u64,
  },
  /// Another holder has the lease.
  LeaseHeld {
    /// The holder's epoch.
    epoch: u64,
  },
  /// No space in the quota.
  NoSpace,
  /// The shard's reserve cannot cover the reservation.
  BudgetExceeded {
    /// Bytes available.
    available: u64,
  },
  /// The principal lacks the right.
  Forbidden {
    /// The verb.
    verb: String,
  },
  /// A grant on a channel that cannot carry one.
  GrantChannelRefused {
    /// The channel.
    channel: String,
  },
  /// The volume is being destroyed.
  Destroying,
  /// The volume is archived.
  Archived,
  /// An invalid name.
  InvalidName,
  /// A policy the base does not allow.
  PolicyMismatch,
  /// The base directory is unavailable.
  BaseUnavailable {
    /// The path.
    path: String,
    /// The errno.
    errno: i32,
  },
  /// A stale retry of an acknowledged request.
  DuplicateRequest,
  /// Not supported here.
  Unsupported {
    /// The feature.
    feature: String,
  },
  /// The shard's client or task capacity is reached.
  TooManyClients,
  /// A shard's cross-shard queue is at its bound; the request was not started and may be
  /// retried (the client's credit bounds what it can have in flight).
  Overloaded {
    /// The shard whose queue is full.
    shard: u16,
  },
  /// A malformed request.
  BadRequest {
    /// What was wrong.
    reason: String,
  },
}

/// A reply body.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub enum ReplyBody {
  /// Created.
  Created {
    /// The id.
    id: VolumeId,
  },
  /// Snapshotted.
  Snapshotted {
    /// The id.
    id: SnapshotId,
  },
  /// Cloned.
  Cloned {
    /// The clone's id.
    id: VolumeId,
  },
  /// Attached.
  Attached {
    /// The attachment.
    attachment: u64,
    /// The lease epoch, for a write attachment.
    lease_epoch: Option<u64>,
    /// The path a bridge publishes (none until a bridge exists).
    path: Option<String>,
  },
  /// Detached.
  Detached,
  /// Resized.
  Resized,
  /// Destroyed.
  Destroyed,
  /// The status.
  Status {
    /// The report.
    report: StatusReport,
  },
  /// The listing.
  Listed {
    /// The volumes.
    volumes: Vec<VolumeSummary>,
  },
  /// Acknowledged.
  Acknowledged,
  /// The base bytes.
  BaseBytes {
    /// The bytes.
    bytes: Vec<u8>,
  },
  /// Re-witnessed.
  Rewitnessed {
    /// The paths re-witnessed.
    paths: Vec<String>,
  },
  /// Pinned.
  Pinned {
    /// Entries pinned.
    entries: u64,
  },
  /// Refused.
  Refused {
    /// The refusal.
    refusal: Refusal,
  },
  /// The daemon's status.
  DaemonStatus {
    /// The report.
    report: DaemonReport,
  },
  /// The awaited scope's placement.
  Placed {
    /// Whether the scope is placed.
    placed: bool,
    /// The mirror's lag, for a mirror scope.
    mirror_age_ns: Option<u64>,
  },
}

/// A message body on the ring: the schema hash then the canonical encoding.
fn frame<M: Wire>(message: &M) -> Vec<u8> {
  let mut out = Vec::with_capacity(size_of::<u64>());
  M::SCHEMA_HASH.encode(&mut out);
  message.encode(&mut out);
  out
}

/// Decodes a framed body, refusing another schema.
fn unframe<M: Wire>(bytes: &[u8]) -> Result<M, IpcError> {
  let mut input = bytes;
  let schema = u64::decode(&mut input).map_err(|_| IpcError::BadSlot {
    reason: "no schema word",
  })?;
  if schema != M::SCHEMA_HASH {
    return Err(IpcError::BadSlot {
      reason: "schema mismatch",
    });
  }
  M::from_bytes(input).map_err(|_| IpcError::BadSlot {
    reason: "body does not decode",
  })
}

/// Format: a bulk reference in a slot's payload: offset (8), length (8).
const BULK_REF_BYTES: usize = 16;

/// Which half of the bulk area a message uses: requests (the client writes), replies (the
/// daemon writes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
  /// Client to daemon.
  Request,
  /// Daemon to client.
  Reply,
}

/// The bulk chunk for ring index `index` in `direction`: the area is split in two halves,
/// each divided evenly among the ring's slots, so a message's chunk is fixed by its slot and
/// no allocator sits on the path.
fn chunk(region: &ClientRegion, direction: Direction, index: u64) -> (usize, usize) {
  let slots = region.cmd().slots().max(1);
  let half = region.bulk().len() / 2;
  let chunk = half / slots;
  let position = usize::try_from(index % u64::try_from(slots).unwrap_or(1)).unwrap_or(0);
  let base = match direction {
    Direction::Request => 0,
    Direction::Reply => half,
  };
  (base + position * chunk, chunk)
}

/// Frames `message` into the slot for ring index `index`, inline or through the bulk chunk.
pub fn pack<M: Wire>(
  region: &mut ClientRegion,
  direction: Direction,
  index: u64,
  request: u64,
  message: &M,
) -> Result<Slot, IpcError> {
  let body = frame(message);
  if body.len() <= PAYLOAD_BYTES {
    return Slot::inline(request, &body);
  }
  let (offset, capacity) = chunk(region, direction, index);
  if body.len() > capacity {
    return Err(IpcError::PayloadTooLarge {
      offered: body.len(),
      capacity,
    });
  }
  region.bulk_mut()[offset..offset + body.len()].copy_from_slice(&body);
  let mut payload = [0u8; BULK_REF_BYTES];
  payload[..8].copy_from_slice(&u64::try_from(offset).unwrap_or(u64::MAX).to_le_bytes());
  payload[8..].copy_from_slice(&u64::try_from(body.len()).unwrap_or(u64::MAX).to_le_bytes());
  Ok(Slot {
    kind: SlotKind::Bulk,
    request,
    payload: payload.to_vec(),
  })
}

/// Decodes the message a slot carries, inline or from the bulk chunk it names; a reference
/// outside the bulk area is a typed refusal.
pub fn unpack<M: Wire>(
  region: &ClientRegion,
  kind: SlotKind,
  payload: &[u8],
) -> Result<M, IpcError> {
  match kind {
    SlotKind::Inline => unframe(payload),
    SlotKind::Bulk => {
      if payload.len() != BULK_REF_BYTES {
        return Err(IpcError::BadSlot {
          reason: "bulk reference of the wrong size",
        });
      }
      let offset = usize::try_from(u64::from_le_bytes(
        payload[..8].try_into().unwrap_or([0; 8]),
      ))
      .unwrap_or(usize::MAX);
      let len = usize::try_from(u64::from_le_bytes(
        payload[8..].try_into().unwrap_or([0; 8]),
      ))
      .unwrap_or(usize::MAX);
      let bulk = region.bulk();
      let end = offset.checked_add(len).ok_or(IpcError::BadSlot {
        reason: "bulk reference overflows",
      })?;
      if end > bulk.len() {
        return Err(IpcError::BadSlot {
          reason: "bulk reference outside the area",
        });
      }
      unframe(&bulk[offset..end])
    }
    SlotKind::Cancel | SlotKind::Heartbeat => Err(IpcError::BadSlot {
      reason: "not a message",
    }),
  }
}
