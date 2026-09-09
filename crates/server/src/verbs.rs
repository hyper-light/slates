//! The lifecycle verbs of §4.4 as the shard serves them from a client's ring: one step each,
//! no awaits inside (§4.8 "Transactions"); every mutation is a database operation appended
//! before the reply (exactly-once by completion record, §4.9); every verb checks the
//! principal's right (§4.13) and, where the design says so, the lease and its epoch (D-16).

use std::sync::atomic::Ordering;

use slates_base::OsHost;
use slates_db::DurabilityScope;
use slates_db::Op;
use slates_db::catalog::{
  AccessEntry, AttachForm, AttachmentRecord, BaseRecord, CompletionRecord, Consumer, LeaseRecord,
  LineageEdge, NamePolicy as DbNamePolicy, PlacementState, PolicyRecord, Principal, Rights, Role,
  SizeClass as DbSizeClass, SnapshotId as DbSnapshotId, SnapshotRecord, VolumeId as DbVolumeId,
  VolumeRecord, VolumeState,
};
use slates_db::register::ObjectId;
use slates_ipc::protocol::{
  DaemonReport, Direction, HealthSignal, Intent, NamePolicy, PlacedState, Refusal, RefusalCount,
  ReplyBody, RequestBody, Scope, ShardReport, Signal, SizeClass, SnapshotId, StatusReport,
  VolumeId, VolumeSummary, WorkOp, pack, unpack,
};
use slates_ipc::slot::SlotKind;
use slates_ipc::{IpcError, Request};
use slates_machine::derived;
use slates_mem::Handle;
use slates_merge::increment::VolumeOp;
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::{Clock, HostClock};
use slates_vfs::host::HostFs;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{BudgetGrowth, Quota};
use slates_vfs::recover::{KeyedImage, ShardImage, VolumeImage};
use slates_vfs::volume::{DestroyProgress, Volume, VolumeConfig};
use slates_wire::Wire;
use slates_wire::request::{RequestId, Seen};

use crate::error::{refusal_of_db, refusal_of_vfs};
use crate::state::{ClientSlot, Deferred, PendingForward, ShardState, VolumeSlot};

/// Shape: the share of a volume's quota its journal may take, parts per thousand (ratified
/// GAPS §5: the op log of a bounded volume stays a small fraction of its bytes).
const JOURNAL_SHARE_PERMILLE: u64 = 10;
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the destroy slice's budget as a share of the shard's step budget, parts per
/// thousand: half, so a destroy never takes the whole step from the clients.
const DESTROY_SLICE_PERMILLE: u64 = 500;

pub(crate) fn to_db_volume(id: VolumeId) -> DbVolumeId {
  DbVolumeId { bytes: id.bytes }
}

fn to_wire_volume(id: DbVolumeId) -> VolumeId {
  VolumeId { bytes: id.bytes }
}

pub(crate) fn to_db_snapshot(id: SnapshotId) -> DbSnapshotId {
  DbSnapshotId { value: id.value }
}

/// The wire id of a volume-core snapshot: slot index in the high half, generation below.
fn wire_snapshot(id: slates_vfs::ids::SnapshotId) -> SnapshotId {
  SnapshotId {
    value: (u64::from(id.index) << u32::BITS) | u64::from(id.generation),
  }
}

fn core_snapshot(id: SnapshotId) -> slates_vfs::ids::SnapshotId {
  slates_vfs::ids::SnapshotId {
    index: u32::try_from(id.value >> u32::BITS).unwrap_or(u32::MAX),
    generation: u32::try_from(id.value & u64::from(u32::MAX)).unwrap_or(u32::MAX),
  }
}

/// Releases a clone's pin on its origin snapshot (§4.5): the shard owns both volumes, so when a
/// clone's destroy completes — or a partial clone is abandoned before it is published — the origin
/// snapshot's clone count is decremented so the origin can reclaim that snapshot. Best-effort: the
/// origin may already be gone, or the snapshot never pinned.
fn unpin_origin(state: &mut ShardState, origin: Handle<VolumeSlot>, snapshot: SnapshotId) {
  if let Ok(slot) = state.volumes.get_mut(origin) {
    let _ = slot.volume.unpin(core_snapshot(snapshot));
  }
}

/// A fresh volume id: the shard in the high bytes (the creator host's place in Phase 8), the
/// clock and a counter below, so ids never repeat on this host.
fn fresh_volume_id(state: &mut ShardState) -> DbVolumeId {
  let mut bytes = [0u8; 16];
  bytes[..2].copy_from_slice(&state.partition.to_be_bytes());
  bytes[2..10].copy_from_slice(&state.clock.monotonic_ns().to_be_bytes());
  let count = state.db.next_seq();
  bytes[10..].copy_from_slice(&count.to_be_bytes()[2..]);
  DbVolumeId { bytes }
}

/// The rights a principal holds on a volume (§4.13): the owner holds every right.
pub(crate) fn rights_of(record: &VolumeRecord, principal: &Principal) -> Rights {
  if &record.owner == principal {
    return Rights {
      read: true,
      write: true,
      admin: true,
    };
  }
  record
    .access
    .iter()
    .find(|e| &e.principal == principal)
    .map_or(Rights::default(), |e| e.rights)
}

pub(crate) fn forbidden(verb: &str) -> ReplyBody {
  ReplyBody::Refused {
    refusal: Refusal::Forbidden {
      verb: verb.to_owned(),
    },
  }
}

pub(crate) fn refused(refusal: Refusal) -> ReplyBody {
  ReplyBody::Refused { refusal }
}

/// What serving a request produced.
pub enum Served {
  /// A reply for the client now.
  Reply(ReplyBody),
  /// The request went to its volume's owner shard (or was scattered); the reply comes back
  /// through [`crate::state::deliver`].
  Forwarded,
}

/// The owner shard a volume id names (its first two bytes; §4.8 "Lookup": ids route to
/// owners, no index).
pub fn owner_of(volume: VolumeId) -> u16 {
  u16::from_be_bytes([volume.bytes[0], volume.bytes[1]])
}

/// Format: the FNV-1a 64-bit offset basis (Fowler, Noll, Vo; the reference constants).
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// Format: the FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// The partition that owns a name: a stable hash of the name over the daemon's partitions,
/// so every create of one name lands on one partition and the name's uniqueness is that
/// partition's to keep (§4.8 "Lookup": ids route to owners; no global index). The same
/// function on every restart, so a recovered partition still owns its names.
pub fn owner_of_name(name: &str, partitions: usize) -> u16 {
  let mut hash = FNV_OFFSET;
  for byte in name.bytes() {
    hash ^= u64::from(byte);
    hash = hash.wrapping_mul(FNV_PRIME);
  }
  let count = u64::try_from(partitions.max(1)).unwrap_or(u64::MAX);
  u16::try_from(hash % count).unwrap_or(u16::MAX)
}

/// The runtime shard a partition lives on in this process.
fn shard_of_partition(state: &ShardState, partition: u16) -> Option<u16> {
  state.shards.get(usize::from(partition)).copied()
}

/// Format: an attachment id carries its owner partition in the high 16 bits and a per-partition
/// counter below, so a detach routes to the partition that holds the record (§4.8 "Lookup":
/// ids route to owners, no global index).
const ATTACHMENT_PARTITION_SHIFT: u64 = 48;

/// The attachment id for a counter on `partition`.
fn attachment_id(partition: u16, counter: u64) -> u64 {
  (u64::from(partition) << ATTACHMENT_PARTITION_SHIFT) | counter
}

/// The partition that owns an attachment id.
pub fn owner_of_attachment(id: u64) -> u16 {
  u16::try_from(id >> ATTACHMENT_PARTITION_SHIFT).unwrap_or(0)
}

/// The volume a request is about, when it is about one.
fn volume_of(body: &RequestBody) -> Option<VolumeId> {
  match body {
    RequestBody::Snapshot { volume }
    | RequestBody::DestroySnapshot { volume, .. }
    | RequestBody::Versions { green: volume }
    | RequestBody::ChangedSince { green: volume, .. }
    | RequestBody::CreateWork { green: volume, .. }
    | RequestBody::Edit { work: volume, .. }
    | RequestBody::Declare { work: volume, .. }
    | RequestBody::Submit { work: volume }
    | RequestBody::Rebase { work: volume }
    | RequestBody::Clone { volume, .. }
    | RequestBody::Attach { volume, .. }
    | RequestBody::Resize { volume, .. }
    | RequestBody::Destroy { volume }
    | RequestBody::Status { volume }
    | RequestBody::ReadBase { volume, .. }
    | RequestBody::Rewitness { volume, .. }
    | RequestBody::Pin { volume, .. }
    | RequestBody::AwaitPlaced { volume, .. }
    | RequestBody::Land { volume, .. } => Some(*volume),
    RequestBody::Create { .. }
    | RequestBody::CreateGreen { .. }
    | RequestBody::Detach { .. }
    | RequestBody::List
    | RequestBody::DaemonStatus
    | RequestBody::Grants
    | RequestBody::Audit { .. }
    | RequestBody::Acknowledge { .. }
    | RequestBody::Grant { .. } => None,
  }
}

/// Serves one request for one client: the completion window first (a retry returns the
/// retained reply; a stale retry is refused), then the verb here, on its owner shard, or
/// across every shard; the completion is recorded when the reply is known.
pub fn serve(state: &mut ShardState, client: Handle<ClientSlot>, request: &Request) -> Served {
  let (client_id, principal) = match state.clients.get(client) {
    Ok(c) => (c.client_id, c.principal.clone()),
    Err(_) => return Served::Reply(refused(Refusal::NotFound)),
  };
  let id = RequestId::from_word(request.request);
  match state.db.partition().completion(id.client, id.sequence) {
    Seen::Completed(bytes) => {
      return Served::Reply(
        ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest)),
      );
    }
    Seen::Acknowledged => return Served::Reply(refused(Refusal::DuplicateRequest)),
    Seen::New => {}
  }
  let body: RequestBody = {
    let Ok(c) = state.clients.get(client) else {
      return Served::Reply(refused(Refusal::NotFound));
    };
    match unpack(c.end.region(), request.kind, &request.payload) {
      Ok(b) => b,
      Err(IpcError::BadSlot { reason }) => {
        return Served::Reply(record_completion(
          state,
          id,
          refused(Refusal::BadRequest {
            reason: reason.to_owned(),
          }),
        ));
      }
      Err(e) => {
        return Served::Reply(record_completion(
          state,
          id,
          refused(Refusal::BadRequest {
            reason: e.to_string(),
          }),
        ));
      }
    }
  };
  if let RequestBody::List = body {
    return scatter_list(state, client.index(), request.request, principal);
  }
  if let RequestBody::DaemonStatus = body {
    return scatter_status(state, client.index(), request.request);
  }
  if let RequestBody::Acknowledge { up_to } = body {
    return scatter_acknowledge(state, client.index(), request.request, client_id, up_to);
  }
  let owner = match &body {
    RequestBody::Create { name, .. } => Some(owner_of_name(name, state.shards.len())),
    RequestBody::Detach { attachment } => Some(owner_of_attachment(*attachment)),
    other => volume_of(other).map(owner_of),
  };
  if let Some(owner) = owner
    && owner != state.partition
  {
    let Some(shard) = shard_of_partition(state, owner) else {
      return Served::Reply(record_completion(state, id, refused(Refusal::NotFound)));
    };
    return forward(
      state,
      client.index(),
      request.request,
      client_id,
      principal,
      body,
      shard,
    );
  }
  Served::Reply(run_recorded(state, id, client_id, &principal, body))
}

/// Runs a verb on this shard with its effects and its completion record in one durable step
/// (`Db::begin` … `commit`: one log record, so a crash leaves both or neither, AC-2.3).
fn run_recorded(
  state: &mut ShardState,
  id: RequestId,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  state.db.begin();
  let reply = dispatch(state, client_id, principal, body);
  let reply = record_completion(state, id, reply);
  match state.db.commit(&mut state.segment) {
    Ok(_) => reply,
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// The owner's side of a forwarded verb: its own completion window first (a retry of a
/// verb this partition already ran answers from the record), then the verb and its record
/// in one step.
fn run_forwarded(
  state: &mut ShardState,
  id: RequestId,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  match state.db.partition().completion(id.client, id.sequence) {
    Seen::Completed(bytes) => {
      return ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest));
    }
    Seen::Acknowledged => return refused(Refusal::DuplicateRequest),
    Seen::New => {}
  }
  run_recorded(state, id, client_id, principal, body)
}

/// Records the completion (RIFL) and counts the refusal; the reply is then durable and may be
/// sent.
pub fn record_completion(state: &mut ShardState, id: RequestId, reply: ReplyBody) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  let record = Op::CompletionRecorded {
    record: CompletionRecord {
      client: id.client,
      sequence: id.sequence,
      result: reply.to_bytes(),
    },
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &record, now) {
    return refused(refusal_of_db(&e));
  }
  state.served += 1;
  if let ReplyBody::Refused { refusal } = &reply {
    *state.refusals.entry(refusal_name(refusal)).or_insert(0) += 1;
  }
  reply
}

/// Forwards a volume-bound request to its owner shard: a task there runs the verb on the
/// owner's state and hands the reply back to this shard's queue (sharing by move both ways).
fn forward(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  client_id: u32,
  principal: Principal,
  body: RequestBody,
  owner: u16,
) -> Served {
  match send_forward(
    state.shard,
    client_index,
    request,
    client_id,
    &principal,
    &body,
    owner,
  ) {
    Ok(()) => Served::Forwarded,
    Err(slates_rt::RtError::ControlFull { .. }) => {
      // Backpressure, never a drop: kept and retried next round; the clients' credit bounds
      // the queue, and past that bound the request is refused typed and not started.
      let bound = state
        .config
        .clients_per_shard
        .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
      if state.pending_forwards.len() >= bound {
        return Served::Reply(refused(Refusal::Overloaded { shard: owner }));
      }
      state.pending_forwards.push_back(PendingForward {
        client_index,
        request,
        client_id,
        principal,
        body,
        owner,
      });
      Served::Forwarded
    }
    Err(_) => Served::Reply(refused(Refusal::NotFound)),
  }
}

/// Spawns the owner's task for one forward; the reply comes back as a task on `origin`.
fn send_forward(
  origin: u16,
  client_index: u32,
  request: u64,
  client_id: u32,
  principal: &Principal,
  body: &RequestBody,
  owner: u16,
) -> Result<(), slates_rt::RtError> {
  let principal = principal.clone();
  let body = body.clone();
  let task = SpawnRequest::new(
    Box::pin(async move {
      let id = RequestId::from_word(request);
      let reply = crate::state::with_state(|s| {
        s.last_work_ns = s.clock.monotonic_ns();
        run_forwarded(s, id, client_id, &principal, body)
      })
      .unwrap_or_else(|| refused(Refusal::NotFound));
      let back = SpawnRequest::new(
        Box::pin(async move {
          crate::state::deliver(client_index, request, reply, true);
        }),
        None,
      );
      let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
    }),
    None,
  );
  slates_rt::registry::send_control(owner, Control::Spawn(Box::new(task)))
}

/// Retries the forwards a full control channel refused; whether any went out.
fn retry_forwards(state: &mut ShardState) -> bool {
  let mut any = false;
  while let Some(pending) = state.pending_forwards.pop_front() {
    match send_forward(
      state.shard,
      pending.client_index,
      pending.request,
      pending.client_id,
      &pending.principal,
      &pending.body,
      pending.owner,
    ) {
      Ok(()) => any = true,
      Err(slates_rt::RtError::ControlFull { .. }) => {
        state.pending_forwards.push_front(pending);
        break;
      }
      Err(_) => {
        state.deferred.push(Deferred {
          client_index: pending.client_index,
          request: pending.request,
          reply: refused(Refusal::NotFound),
          recorded: false,
        });
        any = true;
      }
    }
  }
  any
}

/// A listing is a scatter-gather over every shard: each answers with what it owns; the origin
/// merges when the last part arrives.
fn scatter_list(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  principal: Principal,
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = match list(state, &principal) {
    ReplyBody::Listed { volumes } => volumes,
    other => return Served::Reply(other),
  };
  if others.is_empty() {
    return Served::Reply(ReplyBody::Listed { volumes: mine });
  }
  state
    .scatters
    .insert(request, (client_index, others.len(), mine));
  let origin = state.shard;
  for shard in others {
    let principal = principal.clone();
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = match crate::state::with_state(|s| list(s, &principal)) {
          Some(ReplyBody::Listed { volumes }) => volumes,
          _ => Vec::new(),
        };
        let back = SpawnRequest::new(
          Box::pin(async move {
            gather(request, part);
          }),
          None,
        );
        let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
      }),
      None,
    );
    if slates_rt::registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
      gather(request, Vec::new());
    }
  }
  Served::Forwarded
}

/// The daemon's status is a scatter-gather like a listing: every shard reports its part and
/// the origin assembles the daemon's view (§4.14 `slates.status`).
fn scatter_status(state: &mut ShardState, client_index: u32, request: u64) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = shard_report(state);
  if others.is_empty() {
    return Served::Reply(daemon_report(state, vec![mine]));
  }
  state
    .status_scatters
    .insert(request, (client_index, others.len(), vec![mine]));
  let origin = state.shard;
  for shard in others {
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = crate::state::with_state(shard_report);
        let back = SpawnRequest::new(
          Box::pin(async move {
            gather_status(request, part);
          }),
          None,
        );
        let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
      }),
      None,
    );
    if slates_rt::registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
      gather_status(request, None);
    }
  }
  Served::Forwarded
}

/// An acknowledgement releases the client's records on every partition that may hold them
/// (a forwarded verb's record lives at its owner): a scatter, gathered as a count.
fn scatter_acknowledge(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  client_id: u32,
  up_to: u32,
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = acknowledge(state, client_id, up_to);
  if others.is_empty() {
    return Served::Reply(mine);
  }
  state
    .ack_scatters
    .insert(request, (client_index, others.len(), mine));
  let origin = state.shard;
  for shard in others {
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = crate::state::with_state(|s| acknowledge(s, client_id, up_to))
          .unwrap_or_else(|| refused(Refusal::NotFound));
        let back = SpawnRequest::new(
          Box::pin(async move {
            gather_acknowledge(request, part);
          }),
          None,
        );
        let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
      }),
      None,
    );
    if slates_rt::registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
      gather_acknowledge(request, refused(Refusal::NotFound));
    }
  }
  Served::Forwarded
}

/// Gathers one shard's acknowledgement; the last delivers the reply (a refusal anywhere
/// is the reply, so the client knows records may remain).
fn gather_acknowledge(request: u64, part: ReplyBody) {
  let done = crate::state::with_state(|s| {
    let entry = s.ack_scatters.get_mut(&request)?;
    if matches!(part, ReplyBody::Refused { .. }) {
      entry.2 = part;
    }
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      s.ack_scatters.remove(&request)
    } else {
      None
    }
  })
  .flatten();
  if let Some((client_index, _, reply)) = done {
    crate::state::deliver(client_index, request, reply, false);
  }
}

/// Gathers one shard's part of a status; the last part delivers the whole.
fn gather_status(request: u64, part: Option<ShardReport>) {
  let done = crate::state::with_state(|s| {
    let entry = s.status_scatters.get_mut(&request)?;
    entry.2.extend(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      s.status_scatters.remove(&request)
    } else {
      None
    }
  })
  .flatten();
  if let Some((client_index, _, mut shards)) = done {
    shards.sort_by_key(|r| r.partition);
    let reply = crate::state::with_state(|s| daemon_report(s, shards))
      .unwrap_or_else(|| refused(Refusal::NotFound));
    crate::state::deliver(client_index, request, reply, false);
  }
}

/// This shard's part of the status: its counters and its health signals (§4.14 catalog).
pub fn shard_report(state: &mut ShardState) -> ShardReport {
  let now = state.clock.monotonic_ns();
  let since_boot = now.saturating_sub(state.booted_ns);
  let ring_depth: u64 = state
    .clients
    .iter()
    .map(|(_, c)| {
      c.end
        .region()
        .cmd()
        .depth(c.end.region().object())
        .unwrap_or(0)
    })
    .sum();
  let term = state.config.failover_slo_ns;
  let expiring = state
    .db
    .partition()
    .volumes()
    .iter()
    .filter(|v| {
      v.lease
        .as_ref()
        .is_some_and(|l| l.expires_ns.saturating_sub(now) <= term)
    })
    .count();
  // Every value the registry can report, computed once. `measure` selects one by its `HealthSignal`,
  // and the report is built by mapping `HealthSignal::ALL` — so the report IS the closed registry, not
  // a hand-kept parallel list that could gain or lose a signal (GAP-A9-12). A new signal is a compile
  // error until it is both measured here and listed in `ALL`.
  let catalog_volumes = u64::try_from(state.by_id.len()).unwrap_or(u64::MAX);
  let log_replay_ns = state.recovered.replay_ns;
  let lease_expiring = u64::try_from(expiring).unwrap_or(u64::MAX);
  let shard_clients = u64::try_from(state.clients.iter().count()).unwrap_or(u64::MAX);
  let shard_deferred = u64::try_from(state.deferred.len()).unwrap_or(u64::MAX);
  let measure = |signal: HealthSignal| -> (u64, u64) {
    match signal {
      HealthSignal::CatalogVolumes => (catalog_volumes, 0),
      HealthSignal::LogReplayNs => (log_replay_ns, since_boot),
      HealthSignal::LeaseExpiring => (lease_expiring, 0),
      HealthSignal::RingDepth => (ring_depth, 0),
      HealthSignal::ShardClients => (shard_clients, 0),
      HealthSignal::ShardDeferred => (shard_deferred, 0),
    }
  };
  let signals = HealthSignal::ALL
    .iter()
    .map(|&signal| {
      let (value, freshness_ns) = measure(signal);
      Signal {
        name: signal.name().to_owned(),
        value,
        freshness_ns,
      }
    })
    .collect();
  ShardReport {
    partition: state.partition,
    clients: u32::try_from(state.clients.iter().count()).unwrap_or(u32::MAX),
    volumes: u64::try_from(state.by_id.len()).unwrap_or(u64::MAX),
    served: state.served,
    refusals: state
      .refusals
      .iter()
      .map(|(kind, count)| RefusalCount {
        kind: (*kind).to_owned(),
        count: *count,
      })
      .collect(),
    replayed_records: state.recovered.replayed_records,
    replay_ns: state.recovered.replay_ns,
    torn_tail: state.recovered.torn,
    reserve_bytes: state.store.budget.capacity(),
    committed_bytes: state.store.budget.committed(),
    version_slots: state.store.versions.capacity(),
    committed_versions: state.store.versions.committed(),
    signals,
  }
}

/// The daemon's view: the anchor's words in the segment and the process-wide counters, over
/// every shard's part.
fn daemon_report(state: &mut ShardState, shards: Vec<ShardReport>) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  let (generation, restarts, heartbeat_age_ns) = state
    .segment
    .supervision()
    .map(|s| {
      (
        s.generation(),
        s.restarts(),
        now.saturating_sub(s.heartbeat_ns()),
      )
    })
    .unwrap_or((0, 0, 0));
  ReplyBody::DaemonStatus {
    report: DaemonReport {
      pid: std::process::id(),
      generation,
      restarts,
      heartbeat_age_ns,
      clients_reaped: crate::daemon::CLIENTS_REAPED.load(Ordering::Acquire),
      clients_refused: crate::daemon::CLIENTS_REFUSED.load(Ordering::Acquire),
      shards,
    },
  }
}

/// One shard's part of a listing arrives at the origin; the last part completes the reply.
fn gather(request: u64, part: Vec<VolumeSummary>) {
  let done = crate::state::with_state(|s| {
    let entry = s.scatters.get_mut(&request)?;
    entry.2.extend(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      s.scatters.remove(&request)
    } else {
      None
    }
  })
  .flatten();
  if let Some((client_index, _, mut volumes)) = done {
    volumes.sort_by(|a, b| a.name.cmp(&b.name));
    crate::state::deliver(client_index, request, ReplyBody::Listed { volumes }, false);
  }
}

fn refusal_name(r: &Refusal) -> &'static str {
  match r {
    Refusal::NotFound => "not_found",
    Refusal::AlreadyExists { .. } => "already_exists",
    Refusal::StaleLease { .. } => "stale_lease",
    Refusal::LeaseHeld { .. } => "lease_held",
    Refusal::NoSpace => "no_space",
    Refusal::BudgetExceeded { .. } => "budget_exceeded",
    Refusal::Forbidden { .. } => "forbidden",
    Refusal::GrantChannelRefused { .. } => "grant_kind_refused",
    Refusal::Destroying => "destroying",
    Refusal::Archived => "archived",
    Refusal::InvalidName => "invalid_name",
    Refusal::PolicyMismatch => "policy_mismatch",
    Refusal::BaseUnavailable { .. } => "base_unavailable",
    Refusal::DuplicateRequest => "duplicate_request",
    Refusal::Unsupported { .. } => "unsupported",
    Refusal::TooManyClients => "too_many_clients",
    Refusal::Overloaded { .. } => "overloaded",
    Refusal::BadRequest { .. } => "bad_request",
    Refusal::TargetUnavailable { .. } => "target_unavailable",
    Refusal::LandingConflict { .. } => "landing_conflict",
    Refusal::LandingLeaseHeld { .. } => "landing_lease_held",
    Refusal::GrantMismatch => "grant_mismatch",
    Refusal::GrantInvalid => "grant_invalid",
  }
}

/// Whether a verb changes the set of volumes or a volume's roots, so the shard must republish its
/// recovery image (§4.8). Data-plane content writes go through the bridge, not here, and carry their
/// own barrier (owed with the mount path, docs/wip/recovery.md).
fn mutates_shard_image(body: &RequestBody) -> bool {
  matches!(
    body,
    RequestBody::Create { .. }
      | RequestBody::CreateGreen { .. }
      | RequestBody::CreateWork { .. }
      | RequestBody::Edit { .. }
      | RequestBody::Declare { .. }
      | RequestBody::Submit { .. }
      | RequestBody::Rebase { .. }
      | RequestBody::Clone { .. }
      | RequestBody::Resize { .. }
      | RequestBody::Destroy { .. }
      | RequestBody::Snapshot { .. }
      | RequestBody::DestroySnapshot { .. }
  )
}

fn dispatch(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  let republish = mutates_shard_image(&body);
  let reply = dispatch_inner(state, client_id, principal, body);
  // Publish the shard's recovery image after a successful volume-set or roots change, so a restart
  // recovers it from anchor-owned RAM (§4.8). A refusal changed nothing, so it needs no publish.
  if republish && !matches!(reply, ReplyBody::Refused { .. }) {
    publish_shard(state);
  }
  reply
}

fn dispatch_inner(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  match body {
    RequestBody::Create {
      name,
      size,
      names,
      require_locked,
      base,
    } => create(
      state,
      principal,
      &name,
      size,
      names,
      require_locked,
      base.as_deref(),
    ),
    RequestBody::Snapshot { volume } => snapshot(state, principal, volume),
    RequestBody::DestroySnapshot { volume, snapshot } => {
      destroy_snapshot_verb(state, principal, volume, snapshot)
    }
    RequestBody::CreateGreen {
      name,
      require_evidence,
    } => create_green(state, principal, &name, require_evidence),
    RequestBody::Versions { green } => versions(state, principal, green),
    RequestBody::ChangedSince { green, version } => changed_since(state, principal, green, version),
    RequestBody::CreateWork { green, name } => create_work(state, principal, green, &name),
    RequestBody::Edit {
      work,
      path,
      at,
      delete_len,
      bytes,
    } => edit(state, work, &path, at, delete_len, &bytes),
    RequestBody::Declare { work, op } => declare(state, work, op),
    RequestBody::Submit { work } => submit(state, work),
    RequestBody::Rebase { work } => rebase(state, work),
    RequestBody::Clone {
      volume,
      snapshot,
      name,
    } => clone(state, principal, volume, snapshot, &name),
    RequestBody::Attach {
      volume,
      snapshot,
      intent,
    } => attach(state, client_id, principal, volume, snapshot, intent),
    RequestBody::Detach { attachment } => detach(state, principal, attachment),
    RequestBody::Resize { volume, size } => resize(state, principal, volume, size),
    RequestBody::Destroy { volume } => destroy(state, principal, volume),
    RequestBody::Status { volume } => status(state, principal, volume),
    RequestBody::List => list(state, principal),
    RequestBody::DaemonStatus => {
      let mine = shard_report(state);
      daemon_report(state, vec![mine])
    }
    RequestBody::AwaitPlaced {
      volume,
      snapshot,
      scope,
    } => await_placed(state, principal, volume, snapshot, scope),
    RequestBody::Land {
      volume,
      snapshot,
      target,
      filter,
      grant,
    } => crate::landing::land_verb(state, principal, volume, snapshot, &target, &filter, grant),
    RequestBody::Grants => crate::landing::grants_verb(state, principal),
    RequestBody::Audit { since } => crate::landing::audit_verb(state, since),
    RequestBody::Acknowledge { up_to } => acknowledge(state, client_id, up_to),
    RequestBody::ReadBase { volume, path } => read_base(state, principal, volume, &path),
    RequestBody::Rewitness { volume, paths } => {
      rewitness(state, principal, volume, paths.as_deref())
    }
    RequestBody::Pin { volume, paths } => pin(state, principal, volume, paths.as_deref()),
    RequestBody::Grant { .. } => {
      *state.refusals.entry("grant_kind_refused").or_insert(0) += 1;
      refused(Refusal::GrantChannelRefused {
        channel: "ring".to_owned(),
      })
    }
  }
}

/// The volume slot and its record, or the refusal.
pub(crate) fn find(
  state: &ShardState,
  volume: VolumeId,
) -> Result<(Handle<VolumeSlot>, VolumeRecord), Box<ReplyBody>> {
  let id = to_db_volume(volume);
  let handle = *state
    .by_id
    .get(&id)
    .ok_or_else(|| Box::new(refused(Refusal::NotFound)))?;
  let record = state
    .db
    .partition()
    .volume(id)
    .cloned()
    .ok_or_else(|| Box::new(refused(Refusal::NotFound)))?;
  if record.state == VolumeState::Destroying || record.state == VolumeState::Destroyed {
    return Err(Box::new(refused(Refusal::Destroying)));
  }
  Ok((handle, record))
}

/// The journal retention budget for a volume of the given quota: a per-mille share of the quota,
/// at least one page (§4.16 journal). Shared by fresh creation and recovery so a rebuilt volume's
/// journal is sized as its original was.
fn journal_bytes_for(state: &ShardState, quota: &Quota) -> usize {
  derived!(
    usize::try_from(quota.limit().saturating_mul(JOURNAL_SHARE_PERMILLE) / PERMILLE)
      .unwrap_or(usize::MAX)
      .max(usize::try_from(state.config.geometry.page).unwrap_or(1)),
    "quota × JOURNAL_SHARE_PERMILLE / 1000, at least one page",
    ["quota", "GAPS §5 journal share"]
  )
  .get()
}

/// A volume's inode allowance (§4.2 resource vector): the inodes whose metadata fits in the volume's
/// reserved byte footprint (its quota over an inode's size), clamped to the shard's inode slab. It
/// scales with the policy the caller asked for — a larger quota admits more inodes — and no single
/// volume exhausts the slab. A fixed disjoint per-volume reservation is the fuller §4.2 refinement
/// (docs/wip/resource-vector.md).
fn inode_allowance(state: &ShardState, size: SizeClass) -> u64 {
  let limit = match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  };
  let inode_bytes = u64::try_from(size_of::<slates_vfs::inode::Inode>())
    .unwrap_or(1)
    .max(1);
  // Cap at the most the version slab can back a single volume — the slab less the copy-up headroom
  // it always keeps free — not the raw slab, so the largest derivable allowance is still reservable
  // (a big-quota volume is not refused for wanting the one slot the transient copy-up needs).
  let cap = state
    .store
    .versions
    .capacity()
    .saturating_sub(state.store.versions.headroom())
    .max(1);
  derived!(
    (limit / inode_bytes).min(cap).max(1),
    "min(quota / size_of::<Inode>, store.max_inodes − copy-up headroom), at least one",
    ["quota", "store.max_inodes", "vfs.copy_up_version_headroom"]
  )
  .get()
}

/// A volume's namespace (entry) allowance (§4.2 resource vector): the entries whose minimum
/// footprint (a child pointer) fits in the volume's reserved byte quota. Bounds hard-link fan-out
/// and name churn that the inode allowance does not; the directory-block slab is the ultimate cap.
/// Admits a fresh volume's resource-vector dimensions (§4.2): its inode and namespace allowances,
/// derived from the requested policy. A fresh volume is under both, so this refuses only on a
/// pathological policy; the caller gives back the byte reservation on failure.
fn admit_dimensions(
  state: &ShardState,
  volume: &mut Volume,
  size: SizeClass,
) -> Result<(), slates_vfs::VfsError> {
  volume.set_inode_allowance(inode_allowance(state, size))?;
  volume.set_entry_allowance(entry_allowance(size))?;
  Ok(())
}

fn entry_allowance(size: SizeClass) -> u64 {
  let limit = match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  };
  let entry_bytes = u64::try_from(size_of::<slates_vfs::dir::Child>())
    .unwrap_or(1)
    .max(1);
  derived!(
    (limit / entry_bytes).max(1),
    "quota / size_of::<Child>() (minimum entry footprint)",
    ["quota"]
  )
  .get()
}

fn volume_config(state: &mut ShardState, names: NamePolicy, quota: Quota) -> VolumeConfig {
  let prefix = state.next_prefix;
  state.next_prefix = state.next_prefix.wrapping_add(1).max(1);
  let journal_bytes = journal_bytes_for(state, &quota);
  VolumeConfig {
    prefix,
    names: match names {
      NamePolicy::Exact => NameEquivalence::Exact,
      NamePolicy::Fold => NameEquivalence::Fold,
    },
    quota,
    journal_bytes,
    clock: Box::new(HostClock::new()),
  }
}

fn quota_for(size: SizeClass) -> Quota {
  match size {
    SizeClass::Bounded { limit } => Quota::Bounded { limit },
    SizeClass::Dynamic { max } => Quota::Dynamic {
      max,
      // Dynamic growth is admitted against, and debited from, the shard budget — the one capacity
      // owner — on each increment as the write path takes it (§4.2), never against raw host memory
      // ("raw free RAM ... do not qualify") and never from a private per-volume ceiling. So two
      // dynamic volumes cannot receive the same capacity, and a growth reduces what bounded volumes
      // are later offered. The `max` is the volume's own ceiling; the budget is the shard's.
      source: Box::new(BudgetGrowth),
      granted: 0,
      denied: 0,
    },
  }
}

fn wire_size(size: DbSizeClass) -> SizeClass {
  match size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  }
}

fn db_size(size: SizeClass) -> DbSizeClass {
  match size {
    SizeClass::Bounded { limit } => DbSizeClass::Bounded { limit },
    SizeClass::Dynamic { max } => DbSizeClass::Dynamic { max },
  }
}

#[allow(clippy::too_many_arguments)]
fn create(
  state: &mut ShardState,
  principal: &Principal,
  name: &str,
  size: SizeClass,
  names: NamePolicy,
  require_locked: bool,
  base: Option<&str>,
) -> ReplyBody {
  if state.db.partition().volume_by_name(name).is_some() {
    let existing = state
      .db
      .partition()
      .volume_by_name(name)
      .map(|v| to_wire_volume(v.id))
      .unwrap_or_default();
    return refused(Refusal::AlreadyExists { existing });
  }
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.budget.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return refused(Refusal::BudgetExceeded { available });
      }
      Err(e) => {
        return refused(Refusal::BadRequest {
          reason: e.to_string(),
        });
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  // Reserve the inode allowance against the shard's version slab *before* creating the volume (§4.2:
  // "admission reserves all required credits or none before publishing"). So the advertised allowance
  // is backed by real slab capacity — two volumes cannot each be promised the same slots — and a
  // refusal here allocates nothing to leak (no root inode, trie or dir), giving back only the byte
  // reservation. Bounded and dynamic both reserve their whole logical allowance: the sacred claim
  // that backs divergence.
  let version_credit = match state.store.versions.reserve(inode_allowance(state, size)) {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      return give_back(
        state,
        reservation,
        None,
        Refusal::BudgetExceeded { available },
      );
    }
    Err(e) => {
      return give_back(
        state,
        reservation,
        None,
        Refusal::BadRequest {
          reason: e.to_string(),
        },
      );
    }
  };
  // A strict volume backs its content in locked RAM (§4.2, BUG-1): lock the shard's arena so its
  // content never swaps, refusing (as BudgetExceeded, the §4.2 lock-capacity refusal) if the OS
  // will not — a strict guarantee never silently becomes swappable service. Whole-arena locking is
  // a coarse first cut; locking only a strict volume's own chunks is the refinement (GAP-A9-1).
  if require_locked && let Err(e) = state.store.content.arena_mut().lock() {
    let available = match e {
      slates_mem::MemError::LockRefused { locked, .. } => u64::try_from(locked).unwrap_or(u64::MAX),
      _ => 0,
    };
    return give_back(
      state,
      reservation,
      version_credit,
      Refusal::BudgetExceeded { available },
    );
  }
  let quota = quota_for(size);
  let config = volume_config(state, names, quota);
  let (mut volume, host) = match base {
    None => match Volume::create(&mut state.store, config) {
      Ok(v) => (v, None),
      Err(e) => return give_back(state, reservation, version_credit, refusal_of_vfs(&e)),
    },
    Some(path) => match open_base(state, path, config) {
      Ok(pair) => pair,
      Err(reply) => return give_back(state, reservation, version_credit, reply_refusal(*reply)),
    },
  };
  // Admit the volume's inode dimension (§4.2 resource vector): set the per-volume cap that
  // `next_no` enforces, to the same allowance already reserved against the version slab above.
  if let Err(e) = admit_dimensions(state, &mut volume, size) {
    return give_back(state, reservation, version_credit, refusal_of_vfs(&e));
  }
  let id = fresh_volume_id(state);
  let record = VolumeRecord {
    id,
    name: name.to_owned(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: db_size(size),
      names: match names {
        NamePolicy::Exact => DbNamePolicy::Exact,
        NamePolicy::Fold => DbNamePolicy::Fold,
      },
      require_locked,
      role: Role::Plain,
    },
    base: match base {
      None => BaseRecord::Scratch,
      Some(path) => BaseRecord::Path {
        path: path.to_owned(),
      },
    },
    head: DbSnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: principal.clone(),
    access: Vec::new(),
    created_ns: state.clock.monotonic_ns(),
  };
  publish_created_volume(state, id, volume, host, reservation, version_credit, record)
}

/// Records a freshly-created volume and moves it into the shard's registry, or gives its credits back
/// and discards the volume (returning its slab slots) if the record cannot be written or the registry
/// has no room. The registry room is checked before the insert, since a full registry's `insert`
/// consumes and drops the slot.
#[allow(clippy::too_many_arguments)]
fn publish_created_volume(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  volume: Volume,
  host: Option<OsHost>,
  reservation: Option<slates_mem::budget::Reservation>,
  version_credit: Option<slates_mem::budget::VersionCredit>,
  record: VolumeRecord,
) -> ReplyBody {
  // Capture the mount name before the record is moved into the log op below; the slot lists the
  // volume under it in the host root (a client mounts `/<name>` or reaches it by `cd <name>`).
  let name = record.name.clone();
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    let _ = volume.discard_partial(&mut state.store);
    return give_back(state, reservation, version_credit, refusal_of_db(&e));
  }
  if !state.volumes.has_room() {
    let full = state.volumes.max_slots();
    let _ = volume.discard_partial(&mut state.store);
    return give_back(
      state,
      reservation,
      version_credit,
      Refusal::BadRequest {
        reason: format!("volume registry full at {full}"),
      },
    );
  }
  let slot = VolumeSlot {
    id,
    name,
    volume,
    host,
    reservation,
    version_credit,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      ReplyBody::Created {
        id: to_wire_volume(id),
      }
    }
    // Unreachable given the room check above; the slot was consumed, so only the credits return.
    Err(e) => give_back(
      state,
      reservation,
      version_credit,
      Refusal::BadRequest {
        reason: e.to_string(),
      },
    ),
  }
}

fn reply_refusal(reply: ReplyBody) -> Refusal {
  match reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::BadRequest {
      reason: "not a refusal".to_owned(),
    },
  }
}

fn give_back(
  state: &mut ShardState,
  reservation: Option<slates_mem::budget::Reservation>,
  version: Option<slates_mem::budget::VersionCredit>,
  refusal: Refusal,
) -> ReplyBody {
  if let Some(r) = reservation {
    state.store.budget.release(r);
  }
  if let Some(c) = version {
    state.store.versions.release(c);
  }
  refused(refusal)
}

/// Opens a base directory (read-only, `O_NOFOLLOW`) and creates the overlay over it.
fn open_base(
  state: &mut ShardState,
  path: &str,
  config: VolumeConfig,
) -> Result<(Volume, Option<OsHost>), Box<ReplyBody>> {
  let (mut host, root) = OsHost::open_root(std::path::Path::new(path)).map_err(|e| {
    Box::new(refused(Refusal::BaseUnavailable {
      path: path.to_owned(),
      errno: match e {
        slates_vfs::host::HostError::Unavailable(code) => code,
        _ => 0,
      },
    }))
  })?;
  let facts = host.facts(root).map_err(|_| {
    Box::new(refused(Refusal::BaseUnavailable {
      path: path.to_owned(),
      errno: 0,
    }))
  })?;
  let volume = Volume::create_overlay(
    &mut state.store,
    config,
    BaseConfig {
      root,
      facts,
      large_class_bytes: state.config.large_class_bytes,
    },
  )
  .map_err(|e| Box::new(refused(refusal_of_vfs(&e))))?;
  Ok((volume, Some(host)))
}

fn snapshot(state: &mut ShardState, principal: &Principal, volume: VolumeId) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("snapshot");
  }
  let taken = match state.volumes.get_mut(handle) {
    Ok(slot) => slot.volume.snapshot(&mut state.store),
    Err(_) => return refused(Refusal::NotFound),
  };
  let id = match taken {
    Ok(id) => wire_snapshot(id),
    Err(e) => return refused(refusal_of_vfs(&e)),
  };
  let now = state.clock.monotonic_ns();
  let ops = [
    Op::SnapshotTaken {
      record: SnapshotRecord {
        id: to_db_snapshot(id),
        volume: record.id,
        epoch: record.epoch.saturating_add(1),
        identity: None,
        placed: placement_of(state, record.id),
        taken_ns: now,
      },
    },
    Op::VolumeHeadAdvanced {
      id: record.id,
      head: to_db_snapshot(id),
      epoch: record.epoch.saturating_add(1),
    },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(refusal_of_db(&e));
    }
  }
  ReplyBody::Snapshotted { id }
}

fn destroy_snapshot_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("destroy_snapshot");
  }
  // Remove it from the volume core first: this refuses `Pinned` if a clone still references it and
  // returns the snapshot's retained inode versions to the shard's version budget (§4.2). Only then
  // record the removal, so a refused destroy leaves the catalog untouched.
  match state.volumes.get_mut(handle) {
    Ok(slot) => {
      if let Err(e) = slot
        .volume
        .destroy_snapshot(&mut state.store, core_snapshot(snapshot))
      {
        return refused(refusal_of_vfs(&e));
      }
    }
    Err(_) => return refused(Refusal::NotFound),
  }
  let now = state.clock.monotonic_ns();
  let op = Op::SnapshotDestroyed {
    volume: record.id,
    id: to_db_snapshot(snapshot),
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    return refused(refusal_of_db(&e));
  }
  ReplyBody::SnapshotDestroyed
}

/// Creates a green volume (§4.16): a shared merge target. It is not a store-backed VFS tree — its
/// merged content lives in the in-memory merge engine kept in `state.greens`, keyed by the id — so it
/// takes no byte or version reservation. The catalog records the `Green` role and its head version.
fn create_green(
  state: &mut ShardState,
  principal: &Principal,
  name: &str,
  require_evidence: bool,
) -> ReplyBody {
  if let Some(existing) = state.db.partition().volume_by_name(name) {
    return refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    });
  }
  let id = fresh_volume_id(state);
  let record = VolumeRecord {
    id,
    name: name.to_owned(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: db_size(SizeClass::Dynamic { max: 0 }),
      names: DbNamePolicy::Exact,
      require_locked: false,
      role: Role::Green {
        require_evidence,
        head_version: 0,
      },
    },
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: principal.clone(),
    access: Vec::new(),
    created_ns: state.clock.monotonic_ns(),
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    return refused(refusal_of_db(&e));
  }
  state.greens.insert(id, slates_merge::engine::Green::new());
  ReplyBody::GreenCreated {
    id: to_wire_volume(id),
  }
}

/// A green's version chain (§4.16): its head version. The read side of the chain — `changed_since`
/// and the per-version records follow. Answered by the green's owner shard, which holds the engine.
fn versions(state: &ShardState, principal: &Principal, green: VolumeId) -> ReplyBody {
  let id = to_db_volume(green);
  let Some(record) = state.db.partition().volume(id) else {
    return refused(Refusal::NotFound);
  };
  if !rights_of(record, principal).read {
    return forbidden("versions");
  }
  let Some(engine) = state.greens.get(&id) else {
    return refused(Refusal::NotFound);
  };
  ReplyBody::Versions {
    head: engine.head(),
  }
}

/// The files a green changed strictly after `version` (§4.16): the read a lagging work uses to know
/// what green moved under it before it rebases.
fn changed_since(
  state: &ShardState,
  principal: &Principal,
  green: VolumeId,
  version: u64,
) -> ReplyBody {
  let id = to_db_volume(green);
  let Some(record) = state.db.partition().volume(id) else {
    return refused(Refusal::NotFound);
  };
  if !rights_of(record, principal).read {
    return forbidden("changed_since");
  }
  let Some(engine) = state.greens.get(&id) else {
    return refused(Refusal::NotFound);
  };
  ReplyBody::ChangedSince {
    paths: engine.changed_since(version),
  }
}

/// Creates a work volume over a green (§4.16): an agent's private place to declare operations,
/// based on the green's current head. The green must be owned by this shard (it holds the engine).
fn create_work(
  state: &mut ShardState,
  principal: &Principal,
  green: VolumeId,
  name: &str,
) -> ReplyBody {
  let green_id = to_db_volume(green);
  let Some(engine) = state.greens.get(&green_id) else {
    return refused(Refusal::NotFound);
  };
  let base = engine.head();
  // Seed the work with the green's current content, so an edit to a base file splices what the file
  // holds rather than looking like a fresh create.
  let seeded: std::collections::BTreeMap<String, Vec<u8>> = engine
    .files()
    .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
    .collect();
  if let Some(existing) = state.db.partition().volume_by_name(name) {
    return refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    });
  }
  let id = fresh_volume_id(state);
  let record = VolumeRecord {
    id,
    name: name.to_owned(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: db_size(SizeClass::Dynamic { max: 0 }),
      names: DbNamePolicy::Exact,
      require_locked: false,
      role: Role::Work {
        green: green_id,
        base_version: base,
        stream: false,
      },
    },
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: principal.clone(),
    access: Vec::new(),
    created_ns: state.clock.monotonic_ns(),
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    return refused(refusal_of_db(&e));
  }
  state.works.insert(
    id,
    crate::state::WorkState {
      green: green_id,
      base_version: base,
      journal: Vec::new(),
      content: seeded,
    },
  );
  ReplyBody::WorkCreated {
    id: to_wire_volume(id),
    base,
  }
}

/// Declares an edit on a work volume (§4.16): a splice at `path` — remove `delete_len` bytes at `at`,
/// insert `bytes`. Maintains the work's content and appends the declared operations, composed into an
/// increment on submit. A new path is created first.
fn edit(
  state: &mut ShardState,
  work: VolumeId,
  path: &str,
  at: u64,
  delete_len: u64,
  bytes: &[u8],
) -> ReplyBody {
  let work_id = to_db_volume(work);
  let Some(w) = state.works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let is_new = !w.content.contains_key(path);
  let old_len = w.content.get(path).map_or(0, |c| c.len() as u64);
  {
    let content = w.content.entry(path.to_owned()).or_default();
    let start = usize::try_from(at).unwrap_or(usize::MAX).min(content.len());
    let del = usize::try_from(delete_len)
      .unwrap_or(usize::MAX)
      .min(content.len() - start);
    content.splice(start..start + del, bytes.iter().copied());
  }
  if is_new {
    w.journal.push(VolumeOp::Create {
      path: path.to_owned(),
    });
  }
  if delete_len > 0 {
    w.journal.push(VolumeOp::Delete {
      path: path.to_owned(),
      at,
      len: delete_len,
    });
  }
  if !bytes.is_empty() {
    let len = bytes.len() as u64;
    w.journal.push(if at >= old_len {
      VolumeOp::Extend {
        path: path.to_owned(),
        at,
        len,
      }
    } else {
      VolumeOp::Insert {
        path: path.to_owned(),
        at,
        len,
      }
    });
  }
  ReplyBody::Edited
}

/// Declares a namespace or metadata operation on a work volume (§4.16): appends the corresponding
/// declared operation to the work's journal, the counterpart to [`edit`]'s content splice. An unlink
/// or rename also keeps the work's content map consistent, so a later edit and the post-state seal
/// see the right files; a symlink's target travels in the ops document's path table and an xattr's
/// value in the journal, so neither needs a work-side store.
fn declare(state: &mut ShardState, work: VolumeId, op: WorkOp) -> ReplyBody {
  let work_id = to_db_volume(work);
  let Some(w) = state.works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let volume_op = match op {
    WorkOp::Unlink { path } => {
      w.content.remove(&path);
      VolumeOp::Unlink { path }
    }
    WorkOp::Rename { from, to } => {
      if let Some(bytes) = w.content.remove(&from) {
        w.content.insert(to.clone(), bytes);
      }
      VolumeOp::Rename { from, to }
    }
    WorkOp::Mkdir { path } => VolumeOp::Mkdir { path },
    WorkOp::Rmdir { path } => VolumeOp::Rmdir { path },
    WorkOp::SetMode { path, mode } => VolumeOp::SetMode { path, mode },
    WorkOp::Symlink { path, target } => VolumeOp::Symlink { path, target },
    WorkOp::Link { path, target } => VolumeOp::Link { path, target },
    WorkOp::SetXattr { path, name, value } => VolumeOp::SetXattr { path, name, value },
    WorkOp::RemoveXattr { path, name } => VolumeOp::RemoveXattr { path, name },
  };
  w.journal.push(volume_op);
  ReplyBody::Declared
}

/// The composed value of the extended attribute `(path, name)` in a work's journal (§4.16): the last
/// `SetXattr` for that key, which is the deriver's final value. `None` when the work set no such
/// attribute (a stale op referencing one, so the region is left zero and the guard below skips it).
fn declared_xattr<'a>(journal: &'a [VolumeOp], path: &str, name: &str) -> Option<&'a [u8]> {
  journal.iter().rev().find_map(|op| match op {
    VolumeOp::SetXattr {
      path: p,
      name: n,
      value,
    } if p == path && n == name => Some(value.as_slice()),
    _ => None,
  })
}

/// Assembles an increment's post-state from a work's content and journal (§4.16): the seal lays each
/// file's final content out by region and then the extended-attribute values, and each content or
/// `SetXattr` op names a slice of that region. A content op copies `[op.at, op.at + op.len)` of its
/// file; a `SetXattr` op copies the attribute's whole value — both into `[op.src, op.src + op.len)`.
/// The xattr value round-trips through the post-state, which is how the green stores it and how the
/// identity check compares two agents' values, so it must be laid in, not left zero.
fn assemble_post_state(
  doc: &slates_merge::ops_doc::OpsDoc,
  content: &std::collections::BTreeMap<String, Vec<u8>>,
  journal: &[VolumeOp],
) -> Vec<u8> {
  use slates_merge::ops_doc::OpKind;
  let is_file_content =
    |kind: OpKind| matches!(kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend);
  let mut size = 0u64;
  for op in &doc.ops {
    if op.src != u64::MAX && (is_file_content(op.kind) || op.kind == OpKind::SetXattr) {
      size = size.max(op.src.saturating_add(op.len));
    }
  }
  let mut post = vec![0u8; usize::try_from(size).unwrap_or(0)];
  for op in &doc.ops {
    if op.src == u64::MAX {
      continue;
    }
    let Some(path) = doc.paths.path(op.path) else {
      continue;
    };
    // The bytes this op contributes, and the offset into them: a file's slice, or an xattr's value.
    let (source, source_at): (&[u8], u64) = if is_file_content(op.kind) {
      let Some(file) = content.get(path) else {
        continue;
      };
      (file.as_slice(), op.at)
    } else if op.kind == OpKind::SetXattr {
      let Ok(name_index) = u16::try_from(op.at) else {
        continue;
      };
      let Some(name) = doc.paths.path(name_index) else {
        continue;
      };
      let Some(value) = declared_xattr(journal, path, name) else {
        continue;
      };
      (value, 0)
    } else {
      continue;
    };
    let (Ok(from), Ok(dst), Ok(len)) = (
      usize::try_from(source_at),
      usize::try_from(op.src),
      usize::try_from(op.len),
    ) else {
      continue;
    };
    let to = from.saturating_add(len);
    if to <= source.len() && dst.saturating_add(len) <= post.len() {
      post[dst..dst + len].copy_from_slice(&source[from..to]);
    }
  }
  post
}

/// Composes a work volume's declared operations into an increment against its green's base version
/// (§4.16), shared by [`submit`] and [`rebase`] so the two never derive an increment differently.
/// The base is the green as it was at the work's base version — empty at 0, the current state at the
/// head, replayed from the deltas for an intervening version a lagging work is based on — what the
/// work was seeded with, so the composition is exact. Returns the green's id and the increment, or
/// the refusal to reply with.
fn build_increment(
  state: &ShardState,
  work_id: DbVolumeId,
) -> Result<(DbVolumeId, slates_merge::engine::Increment), Refusal> {
  let Some(w) = state.works.get(&work_id) else {
    return Err(Refusal::NotFound);
  };
  let green_id = w.green;
  let base_version = w.base_version;
  let Some(engine) = state.greens.get(&green_id) else {
    return Err(Refusal::NotFound);
  };
  let base = engine.base_at(base_version);
  let doc = match slates_merge::increment::compose_volume(&base, &w.journal) {
    Ok(doc) => doc,
    Err(e) => {
      return Err(Refusal::BadRequest {
        reason: format!("increment does not compose: {e:?}"),
      });
    }
  };
  let post_state = assemble_post_state(&doc, &w.content, &w.journal);
  let mut hasher = blake3::Hasher::new();
  hasher.update(&doc.encode());
  hasher.update(&post_state);
  let id = *hasher.finalize().as_bytes();
  Ok((
    green_id,
    slates_merge::engine::Increment {
      id,
      base: base_version,
      doc,
      post_state,
    },
  ))
}

/// The conflict windows of a merge outcome, as the wire's [`MergeWindow`](slates_ipc::protocol::MergeWindow)s.
fn merge_windows(
  windows: &[slates_merge::engine::ConflictWindow],
) -> Vec<slates_ipc::protocol::MergeWindow> {
  windows
    .iter()
    .map(|w| slates_ipc::protocol::MergeWindow {
      path: w.path.clone(),
      at: w.range.start,
      len: w.range.len,
      class: w.class as u8,
    })
    .collect()
}

/// Submits a work volume's declared operations to its green as an increment (§4.16): compose the
/// declared operations into the canonical document, seal the post-state, hash the identity, and run
/// the green's merge verdict — accepted with the new version, or the conflict windows to rebase.
fn submit(state: &mut ShardState, work: VolumeId) -> ReplyBody {
  let work_id = to_db_volume(work);
  let (green_id, inc) = match build_increment(state, work_id) {
    Ok(built) => built,
    Err(refusal) => return refused(refusal),
  };
  // The increment is recorded durably so a restart replays it (§4.8, §4.16). Guard-then-apply: the
  // chain must be able to hold it *before* the verdict commits, so an accepted increment is always
  // recorded — otherwise a full-chain refusal after an in-memory commit would strand the green ahead
  // of its log, hidden by the engine's idempotent `seen` cache on retry. A full chain refuses every
  // submit (it cannot advance regardless of the verdict).
  let record = Op::GreenAdvanced {
    green: green_id,
    increment: inc.encode(),
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state.db.partition().check(&record, now) {
    return refused(refusal_of_db(&e));
  }
  let outcome = {
    let Some(engine) = state.greens.get_mut(&green_id) else {
      return refused(Refusal::NotFound);
    };
    engine.submit(&inc)
  };
  match outcome {
    slates_merge::engine::Outcome::Accepted { version } => {
      // The pre-check passed and the shard is single-threaded, so this append fits the budget; a
      // segment-full failure refuses like any other verb and a resubmit records the (idempotent) accept.
      if let Err(e) = state.db.mutate(&mut state.segment, &record, now) {
        return refused(refusal_of_db(&e));
      }
      ReplyBody::Submitted {
        version: Some(version),
        conflicts: Vec::new(),
      }
    }
    slates_merge::engine::Outcome::Conflict { windows } => ReplyBody::Submitted {
      version: None,
      conflicts: merge_windows(&windows),
    },
  }
}

/// Rebases a work volume onto its green's head (§4.16 "Rebase, the only corrective path"): compose the
/// same increment `submit` would, run the verdict without committing to the green, and — when every
/// operation maps cleanly — move the work onto the head, restating its base, its content and its
/// journal in head coordinates so a later submit composes with no further mapping. A conflict returns
/// the windows and changes nothing. The green is never changed by a rebase.
fn rebase(state: &mut ShardState, work: VolumeId) -> ReplyBody {
  let work_id = to_db_volume(work);
  let (green_id, inc) = match build_increment(state, work_id) {
    Ok(built) => built,
    Err(refusal) => return refused(refusal),
  };
  let Some(engine) = state.greens.get_mut(&green_id) else {
    return refused(Refusal::NotFound);
  };
  match engine.rebase(&inc) {
    slates_merge::engine::Rebased::Rebased {
      version,
      files,
      journal,
    } => {
      // The work moves onto the head; the green is untouched. `build_increment` proved the work
      // exists, so this lookup finds it.
      if let Some(w) = state.works.get_mut(&work_id) {
        w.base_version = version;
        w.content = files;
        w.journal = journal;
      }
      ReplyBody::Rebased {
        version: Some(version),
        conflicts: Vec::new(),
      }
    }
    slates_merge::engine::Rebased::Conflict { windows } => ReplyBody::Rebased {
      version: None,
      conflicts: merge_windows(&windows),
    },
  }
}

fn clone(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
  name: &str,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("clone");
  }
  if state.db.partition().volume_by_name(name).is_some() {
    let existing = state
      .db
      .partition()
      .volume_by_name(name)
      .map(|v| to_wire_volume(v.id))
      .unwrap_or_default();
    return refused(Refusal::AlreadyExists { existing });
  }
  let size = match record.policy.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  };
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.budget.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return refused(Refusal::BudgetExceeded { available });
      }
      Err(e) => {
        return refused(Refusal::BadRequest {
          reason: e.to_string(),
        });
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  // Reserve the clone's inode allowance against the version slab *before* cloning (§4.2: reserve all
  // credits or none before publishing), so a refusal allocates nothing and does not leave the
  // origin's `clone_refs` bumped. A clone shares its origin's versions until it diverges, so it
  // reserves the whole allowance — a create's fair share, but never below the count it inherits from
  // the origin (equal to the origin's live count at clone time) — as the sacred claim that backs full
  // divergence of the inherited inodes. If the slab cannot back it, the clone is refused.
  let inherited = match state.volumes.get(handle) {
    Ok(slot) => slot.volume.inode_usage().0,
    Err(_) => return give_back(state, reservation, None, Refusal::NotFound),
  };
  let clone_allowance = inode_allowance(state, size).max(inherited);
  let version_credit = match state.store.versions.reserve(clone_allowance) {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      return give_back(
        state,
        reservation,
        None,
        Refusal::BudgetExceeded { available },
      );
    }
    Err(e) => {
      return give_back(
        state,
        reservation,
        None,
        Refusal::BadRequest {
          reason: e.to_string(),
        },
      );
    }
  };
  let names = match record.policy.names {
    DbNamePolicy::Exact => NamePolicy::Exact,
    DbNamePolicy::Fold => NamePolicy::Fold,
  };
  let quota = quota_for(size);
  let config = volume_config(state, names, quota);
  let cloned = match state.volumes.get_mut(handle) {
    Ok(slot) => Volume::clone_of(
      &state.store,
      &mut slot.volume,
      core_snapshot(snapshot),
      config,
    ),
    Err(_) => return give_back(state, reservation, version_credit, Refusal::NotFound),
  };
  let mut volume_core = match cloned {
    Ok(v) => v,
    Err(e) => return give_back(state, reservation, version_credit, refusal_of_vfs(&e)),
  };
  // Cap the clone's inode dimension at the same allowance already reserved above, so its per-volume
  // cap (`next_no`) and its version-slab reservation agree. A clone previously carried no inode cap.
  let _ = volume_core.set_inode_allowance(clone_allowance);
  let id = fresh_volume_id(state);
  let now = state.clock.monotonic_ns();
  let mut new_record = record.clone();
  new_record.id = id;
  new_record.name = name.to_owned();
  new_record.owner = principal.clone();
  new_record.access = Vec::new();
  new_record.lease = None;
  new_record.created_ns = now;
  let ops = [
    Op::VolumeCreated { record: new_record },
    Op::LineageAdded {
      edge: LineageEdge {
        child: id,
        origin_volume: record.id,
        origin_snapshot: to_db_snapshot(snapshot),
      },
    },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      unpin_origin(state, handle, snapshot);
      let _ = volume_core.discard_partial(&mut state.store);
      return give_back(state, reservation, version_credit, refusal_of_db(&e));
    }
  }
  // As in create: ensure the registry has room before moving the clone into a slot, discarding it
  // (which frees only what the clone made — nothing, since it shares its origin's versions) otherwise.
  // A partial clone also releases the pin `clone_of` put on the origin snapshot.
  if !state.volumes.has_room() {
    let full = state.volumes.max_slots();
    unpin_origin(state, handle, snapshot);
    let _ = volume_core.discard_partial(&mut state.store);
    return give_back(
      state,
      reservation,
      version_credit,
      Refusal::BadRequest {
        reason: format!("volume registry full at {full}"),
      },
    );
  }
  let slot = VolumeSlot {
    id,
    name: name.to_owned(),
    volume: volume_core,
    host: None,
    reservation,
    version_credit,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      ReplyBody::Cloned {
        id: to_wire_volume(id),
      }
    }
    // The slot did not land: give both credits back (they are `Copy`), never leaking on this path.
    Err(e) => give_back(
      state,
      reservation,
      version_credit,
      Refusal::BadRequest {
        reason: e.to_string(),
      },
    ),
  }
}

fn attach(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  intent: Intent,
) -> ReplyBody {
  let (_, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  let rights = rights_of(&record, principal);
  let now = state.clock.monotonic_ns();
  let lease_epoch = match intent {
    Intent::Read => {
      if !rights.read {
        return forbidden("attach");
      }
      None
    }
    Intent::Write => {
      if !rights.write {
        return forbidden("attach");
      }
      let epoch = match &record.lease {
        Some(current) if &current.holder == principal => current.epoch,
        Some(current) if current.expires_ns <= now => current.epoch.saturating_add(1),
        Some(_) => 1,
        None => 1,
      };
      let term = derived!(
        state.config.failover_slo_ns,
        "the operator's failover SLO (the renewal round trip is microseconds, so one term is the SLO)",
        ["failover_slo_ns"]
      );
      let lease = LeaseRecord {
        holder: principal.clone(),
        epoch,
        expires_ns: now.saturating_add(term.get()),
      };
      if let Err(e) = state.db.mutate(
        &mut state.segment,
        &Op::LeaseTaken {
          volume: record.id,
          lease,
        },
        now,
      ) {
        return refused(refusal_of_db(&e));
      }
      Some(epoch)
    }
  };
  let attachment = attachment_id(state.partition, state.next_attachment);
  state.next_attachment += 1;
  let op = Op::AttachmentAdded {
    record: AttachmentRecord {
      id: attachment,
      volume: record.id,
      consumer: Consumer::Sdk { client: client_id },
      snapshot: snapshot.map(to_db_snapshot),
      form: AttachForm::Root,
      principal: principal.clone(),
    },
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    return refused(refusal_of_db(&e));
  }
  ReplyBody::Attached {
    attachment,
    lease_epoch,
    path: None,
  }
}

fn detach(state: &mut ShardState, principal: &Principal, attachment: u64) -> ReplyBody {
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("detach");
  }
  let now = state.clock.monotonic_ns();
  if let Err(e) = state.db.mutate(
    &mut state.segment,
    &Op::AttachmentRemoved { id: attachment },
    now,
  ) {
    return refused(refusal_of_db(&e));
  }
  // The last write attachment of the holder releases the lease.
  let holds_another = state
    .db
    .partition()
    .attachments_of(record.volume)
    .iter()
    .any(|a| &a.principal == principal);
  let lease_is_ours = state
    .db
    .partition()
    .volume(record.volume)
    .and_then(|v| v.lease.as_ref())
    .is_some_and(|l| &l.holder == principal);
  if !holds_another && lease_is_ours {
    let _ = state.db.mutate(
      &mut state.segment,
      &Op::LeaseReleased {
        volume: record.volume,
      },
      now,
    );
  }
  ReplyBody::Detached
}

/// Grows a volume's version reservation for a resize by `new − old` slots, whole or not at all (a
/// resize-up the version slab cannot back is refused before anything else changes). Returns the
/// growth credit — held so a later resize step can roll it back — or the refusal reply. A resize
/// that does not grow the allowance returns `Ok(None)`; the shrink is applied later by
/// [`settle_version_reservation`], after the core accepts the new limit.
fn grow_version_reservation(
  versions: &mut slates_mem::budget::VersionBudget,
  old: u64,
  new: u64,
) -> Result<Option<slates_mem::budget::VersionCredit>, Refusal> {
  if new <= old {
    return Ok(None);
  }
  match versions.reserve(new - old) {
    Ok(c) => Ok(Some(c)),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      Err(Refusal::BudgetExceeded { available })
    }
    Err(e) => Err(Refusal::BadRequest {
      reason: e.to_string(),
    }),
  }
}

/// Settles a volume's version reservation to `new` slots once a resize is accepted: the growth was
/// already taken by [`grow_version_reservation`], so only a shrink acts here, returning `old − new`
/// slots to the slab. Returns the credit the volume's slot now holds.
fn settle_version_reservation(
  versions: &mut slates_mem::budget::VersionBudget,
  old: u64,
  new: u64,
) -> slates_mem::budget::VersionCredit {
  if new < old {
    versions.release(slates_mem::budget::VersionCredit { slots: old - new });
  }
  slates_mem::budget::VersionCredit { slots: new }
}

fn resize(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  size: SizeClass,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).admin {
    return forbidden("resize");
  }
  let limit = match size {
    SizeClass::Bounded { limit } | SizeClass::Dynamic { max: limit } => limit,
  };
  // The inode allowance moves with the policy too (§4.2: allowances are derived from the requested
  // policy, and the policy — the quota — is what resize changes). Re-derive it and grow its version
  // reservation *first*, so a resize-up the version slab cannot back is refused whole before anything
  // changes. Derived before the slot is borrowed (it reads the store's version budget). Never below
  // the volume's live count, so a resize-down cannot strand inodes the volume already holds.
  let derived_inode_allowance = inode_allowance(state, size);
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let new_allowance = derived_inode_allowance.max(slot.volume.inode_usage().0);
  let old_version = slot.version_credit.map_or(0, |c| c.slots);
  let version_grown =
    match grow_version_reservation(&mut state.store.versions, old_version, new_allowance) {
      Ok(v) => v,
      Err(refusal) => return refused(refusal),
    };
  // A bounded volume's byte reservation moves with the limit: grow first (refused whole if the
  // reserve cannot cover it), shrink after the core accepted the new limit. A failure here rolls
  // back the version growth taken above, so a refused resize changes neither budget.
  let old = slot.reservation;
  let grow = old.map_or(0, |r| limit.saturating_sub(r.bytes));
  let grown = if grow > 0 {
    match state.store.budget.reserve(grow) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        if let Some(v) = version_grown {
          state.store.versions.release(v);
        }
        return refused(Refusal::BudgetExceeded { available });
      }
      Err(e) => {
        if let Some(v) = version_grown {
          state.store.versions.release(v);
        }
        return refused(Refusal::BadRequest {
          reason: e.to_string(),
        });
      }
    }
  } else {
    None
  };
  if let Err(e) = slot.volume.resize(limit) {
    if let Some(g) = grown {
      state.store.budget.release(g);
    }
    if let Some(v) = version_grown {
      state.store.versions.release(v);
    }
    return refused(refusal_of_vfs(&e));
  }
  // The core accepted the new limit: apply the new inode cap and settle the version reservation to
  // the new allowance (the growth is already taken above; a shrink returns the difference now), then
  // settle the byte reservation the same way.
  let _ = slot.volume.set_inode_allowance(new_allowance);
  slot.version_credit = Some(settle_version_reservation(
    &mut state.store.versions,
    old_version,
    new_allowance,
  ));
  if let Some(old) = old {
    let combined = old.bytes.saturating_add(grown.map_or(0, |g| g.bytes));
    state
      .store
      .budget
      .release(slates_mem::budget::Reservation { bytes: combined });
    match state.store.budget.reserve(limit) {
      Ok(r) => slot.reservation = Some(r),
      Err(_) => slot.reservation = None,
    }
  }
  let now = state.clock.monotonic_ns();
  if let Err(e) = state.db.mutate(
    &mut state.segment,
    &Op::VolumeResized {
      id: record.id,
      size: db_size(size),
    },
    now,
  ) {
    return refused(refusal_of_db(&e));
  }
  ReplyBody::Resized
}

fn destroy(state: &mut ShardState, principal: &Principal, volume: VolumeId) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).admin {
    return forbidden("destroy");
  }
  let now = state.clock.monotonic_ns();
  let ops = [
    Op::VolumeStateChanged {
      id: record.id,
      state: VolumeState::Destroying,
    },
    Op::LeaseReleased { volume: record.id },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return refused(refusal_of_db(&e));
    }
  }
  if let Ok(slot) = state.volumes.get_mut(handle)
    && let Err(e) = slot.volume.destroy(&mut state.store)
  {
    return refused(refusal_of_vfs(&e));
  }
  ReplyBody::Destroyed
}

/// One cooperative destroy slice per destroying volume; a finished one leaves the tables.
pub fn step_destroys(state: &mut ShardState) -> bool {
  let budget = state
    .config
    .runtime
    .step_budget_ns
    .saturating_mul(DESTROY_SLICE_PERMILLE)
    / PERMILLE;
  let destroying: Vec<(DbVolumeId, Handle<VolumeSlot>)> = state
    .by_id
    .iter()
    .filter(|(id, _)| {
      state
        .db
        .partition()
        .volume(**id)
        .is_some_and(|v| v.state == VolumeState::Destroying)
    })
    .map(|(id, h)| (*id, *h))
    .collect();
  let mut any = false;
  for (id, handle) in destroying {
    any = true;
    let done = match state.volumes.get_mut(handle) {
      Ok(slot) => matches!(
        slot.volume.destroy_step(&mut state.store, budget.max(1)),
        Ok(DestroyProgress::Done) | Err(_)
      ),
      Err(_) => true,
    };
    if done {
      // If this volume is a clone, capture its pin on the origin snapshot before the record goes, so
      // the pin can be released once its destroy completes (§4.5): the origin can then reclaim that
      // snapshot. Best-effort — the origin may be gone or on another shard.
      let origin_pin = state
        .db
        .partition()
        .lineage(id)
        .map(|e| (e.origin_volume, e.origin_snapshot));
      let now = state.clock.monotonic_ns();
      let _ = state
        .db
        .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now);
      if let Some((origin_volume, origin_snapshot)) = origin_pin
        && let Some(origin_handle) = state.by_id.get(&origin_volume).copied()
      {
        unpin_origin(
          state,
          origin_handle,
          SnapshotId {
            value: origin_snapshot.value,
          },
        );
      }
      if let Ok(slot) = state.volumes.remove(handle) {
        if let Some(r) = slot.reservation {
          state.store.budget.release(r);
        }
        // The volume's inode allowance was reserved against the version slab at create; give those
        // slots back on teardown so the slab is never over-offered (§4.2 accounting through teardown).
        if let Some(c) = slot.version_credit {
          state.store.versions.release(c);
        }
        // A dynamic volume's growth was acquired from the shard budget as it wrote; give it back on
        // teardown so the capacity returns to the one owner (§4.2 accounting through teardown).
        let held = slot.volume.budget_hold();
        if held > 0 {
          state
            .store
            .budget
            .release(slates_mem::budget::Reservation { bytes: held });
        }
      }
      state.by_id.remove(&id);
    }
  }
  any
}

fn status(state: &mut ShardState, principal: &Principal, volume: VolumeId) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).read {
    return forbidden("status");
  }
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let accounting = slot.volume.accounting();
  let (drifted, watcher) = match slot.host.as_mut() {
    Some(host) => match slot.volume.with_host(host).status(&mut state.store) {
      Ok(s) => (
        s.drift
          .iter()
          .map(|(p, k)| format!("{p} ({k:?})"))
          .collect(),
        format!("{:?}", s.watcher),
      ),
      Err(e) => (
        vec![format!("status refused: {e:?}")],
        "unavailable".to_owned(),
      ),
    },
    None => (Vec::new(), "scratch".to_owned()),
  };
  let attachments = state.db.partition().attachments_of(record.id).len();
  ReplyBody::Status {
    report: StatusReport {
      id: volume,
      name: record.name.clone(),
      referenced_bytes: accounting.referenced_bytes,
      unique_bytes: accounting.unique_bytes,
      lease_epoch: record.lease.as_ref().map(|l| l.epoch),
      attachments: u32::try_from(attachments).unwrap_or(u32::MAX),
      head: SnapshotId {
        value: record.head.value,
      },
      drifted,
      watcher,
      snapshots: u32::try_from(slot.volume.snapshot_count()).unwrap_or(u32::MAX),
      placed: placed_state(state, record.id, record.head),
      // The daemon's NFS port (§4.6), 0 until the listener binds; report it so a client can mount.
      nfs_port: u16::try_from(crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire))
        .ok()
        .filter(|port| *port != 0),
    },
  }
}

/// The placement of a volume's snapshot as the register configuration computes it: at `f = 0` the
/// owner alone, committed on the local append (§4.8 "Laptop degenerate"). A snapshot places on its
/// volume's candidate holders, so the placement object is the volume's 128-bit id (whose high half
/// names the creator host), not the volume-unique snapshot id.
fn placement_of(state: &ShardState, volume: DbVolumeId) -> PlacementState {
  let placement = state.config_register.place(ObjectId(volume.bytes));
  if state.config_register.region_placed(&placement) {
    PlacementState::Placed {
      region: placement.acked.iter().map(|h| h.0).collect(),
      mirror: None,
    }
  } else {
    PlacementState::Local
  }
}

/// A volume's head placement for a status reply (§4.8, D-18): whether the head is placed in
/// the region, the mirror's lag (none at `f = 0`), and the owner's host epoch.
fn placed_state(state: &ShardState, volume: DbVolumeId, head: DbSnapshotId) -> PlacedState {
  let region = if head == DbSnapshotId::default() {
    // No snapshot yet: the catalog register itself is locally committed, so at `f = 0` the
    // head is placed (nothing to replicate until a seal). The placement object is the volume's full
    // 128-bit id (its high half names the creator host); the old code truncated it to that high half
    // alone, so every volume of one creator collided to one placement object — fixed by ObjectId.
    let object = ObjectId(volume.bytes);
    state
      .config_register
      .region_placed(&state.config_register.place(object))
  } else {
    match state.db.partition().snapshot(volume, head) {
      Some(record) => matches!(record.placed, PlacementState::Placed { .. }),
      None => false,
    }
  };
  PlacedState {
    region,
    mirror_age_ns: None,
    host_epoch: state.config_register.host_epoch.0,
  }
}

/// Awaits a durability scope for a volume's head (or a snapshot): at `f = 0` the region is the
/// local append (already placed) and the mirror is refused `Unsupported` (§4.8 D-18).
fn await_placed(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  scope: Scope,
) -> ReplyBody {
  let (_, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).read {
    return forbidden("await_placed");
  }
  let target = snapshot.map_or(record.head, to_db_snapshot);
  // The snapshot places on its volume's candidate holders — the placement object is the volume id.
  let _ = target;
  let placement = state.config_register.place(ObjectId(volume.bytes));
  let db_scope = match scope {
    Scope::Region => DurabilityScope::Region,
    Scope::Mirror => DurabilityScope::Mirror,
  };
  match state.config_register.await_placed(db_scope, &placement) {
    Ok(placed) => ReplyBody::Placed {
      placed,
      mirror_age_ns: None,
    },
    Err(slates_db::register::RegisterError::Unsupported { .. }) => refused(Refusal::Unsupported {
      feature: "mirror".to_owned(),
    }),
    Err(_) => refused(Refusal::NotFound),
  }
}

fn list(state: &mut ShardState, principal: &Principal) -> ReplyBody {
  let volumes = state
    .db
    .partition()
    .volumes()
    .into_iter()
    .filter(|v| rights_of(v, principal).read && v.state != VolumeState::Destroyed)
    .map(|v| VolumeSummary {
      id: to_wire_volume(v.id),
      name: v.name.clone(),
      referenced_bytes: state
        .by_id
        .get(&v.id)
        .and_then(|h| state.volumes.get(*h).ok())
        .map_or(v.referenced_bytes, |s| {
          s.volume.accounting().referenced_bytes
        }),
      unique_bytes: state
        .by_id
        .get(&v.id)
        .and_then(|h| state.volumes.get(*h).ok())
        .map_or(v.unique_bytes, |s| s.volume.accounting().unique_bytes),
      overlay: matches!(v.base, BaseRecord::Path { .. }),
    })
    .collect();
  ReplyBody::Listed { volumes }
}

fn acknowledge(state: &mut ShardState, client_id: u32, up_to: u32) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  match state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      client: client_id,
      up_to,
    },
    now,
  ) {
    Ok(_) => ReplyBody::Acknowledged,
    Err(e) => refused(refusal_of_db(&e)),
  }
}

fn base_of(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  verb: &str,
  write: bool,
) -> Result<Handle<VolumeSlot>, Box<ReplyBody>> {
  let (handle, record) = find(state, volume)?;
  let rights = rights_of(&record, principal);
  if (write && !rights.write) || (!write && !rights.read) {
    return Err(Box::new(forbidden(verb)));
  }
  if !matches!(record.base, BaseRecord::Path { .. }) {
    return Err(Box::new(refused(Refusal::Unsupported {
      feature: format!("{verb} on a scratch volume"),
    })));
  }
  Ok(handle)
}

fn read_base(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  path: &str,
) -> ReplyBody {
  let handle = match base_of(state, principal, volume, "read_base", false) {
    Ok(h) => h,
    Err(r) => return *r,
  };
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let Some(host) = slot.host.as_mut() else {
    return refused(Refusal::NotFound);
  };
  match slot.volume.with_host(host).read_base(path) {
    Ok(bytes) => ReplyBody::BaseBytes { bytes },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

fn rewitness(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  paths: Option<&[String]>,
) -> ReplyBody {
  let handle = match base_of(state, principal, volume, "rewitness", true) {
    Ok(h) => h,
    Err(r) => return *r,
  };
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let Some(host) = slot.host.as_mut() else {
    return refused(Refusal::NotFound);
  };
  match slot
    .volume
    .with_host(host)
    .rewitness(&mut state.store, paths)
  {
    Ok(paths) => ReplyBody::Rewitnessed { paths },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

fn pin(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  paths: Option<&[String]>,
) -> ReplyBody {
  let handle = match base_of(state, principal, volume, "pin", true) {
    Ok(h) => h,
    Err(r) => return *r,
  };
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let Some(host) = slot.host.as_mut() else {
    return refused(Refusal::NotFound);
  };
  match slot.volume.with_host(host).pin(&mut state.store, paths) {
    Ok(entries) => ReplyBody::Pinned {
      entries: u64::try_from(entries).unwrap_or(u64::MAX),
    },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// Serves every ready request of every client, retries deferred replies, steps destroys and
/// expires leases; returns whether anything was done.
pub fn serve_round(state: &mut ShardState) -> bool {
  let mut any = retry_forwards(state);
  any |= retry_deferred(state);
  let handles: Vec<(u32, Handle<ClientSlot>)> =
    state.clients.iter().map(|(h, _)| (h.index(), h)).collect();
  // One clock read per round marks every client served in it (the reap's silence clock).
  let now = state.clock.monotonic_ns();
  for (index, handle) in handles {
    any |= serve_client(state, index, handle, now);
  }
  any |= step_destroys(state);
  any |= expire_leases(state) > 0;
  any
}

/// Releases every lease whose term passed (the wheel pops; nothing scans); the count.
pub fn expire_leases(state: &mut ShardState) -> usize {
  let now = state.clock.monotonic_ns();
  let expired = state.db.partition_mut().expired_leases(now);
  let count = expired.len();
  for volume in expired {
    let _ = state
      .db
      .mutate(&mut state.segment, &Op::LeaseReleased { volume }, now);
  }
  count
}

/// What one sweep for dead clients did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reaped {
  /// Clients found gone and reclaimed.
  pub clients: usize,
  /// Their attachments removed.
  pub attachments: usize,
}

/// Asks about every client silent past `silence_ns` and reclaims the gone ones (§4.7 "Failure
/// matrix"): its attachments leave the catalog as recorded operations, its region and control
/// channel are dropped, its id is returned to the control shard's live set, and its leases
/// keep their terms (a paused client is not a dead one; the term is the fence, D-16). Other
/// clients are untouched.
pub fn reap_dead_clients(state: &mut ShardState, silence_ns: u64) -> Reaped {
  let now = state.clock.monotonic_ns();
  let silent: Vec<(Handle<ClientSlot>, u32)> = state
    .clients
    .iter()
    .filter(|(_, c)| now.saturating_sub(c.last_seen_ns) >= silence_ns)
    .map(|(h, c)| (h, c.client_id))
    .collect();
  let mut reaped = Reaped::default();
  for (handle, client_id) in silent {
    let gone = state
      .clients
      .get(handle)
      .is_ok_and(|c| crate::peer::peer_gone(c.control.as_ref(), c.pid));
    if !gone {
      continue;
    }
    reaped.attachments += reap_client(state, handle, client_id);
    reaped.clients += 1;
  }
  reaped
}

/// Reclaims one client; its attachments removed, counted.
fn reap_client(state: &mut ShardState, handle: Handle<ClientSlot>, client_id: u32) -> usize {
  let now = state.clock.monotonic_ns();
  let mut removed = 0;
  for id in state.db.partition().attachments_of_client(client_id) {
    if state
      .db
      .mutate(&mut state.segment, &Op::AttachmentRemoved { id }, now)
      .is_ok()
    {
      removed += 1;
    }
  }
  let index = handle.index();
  state.deferred.retain(|d| d.client_index != index);
  // The slot goes last: the region's mapping and the control channel close with it.
  let _ = state.clients.remove(handle);
  // The id returns to the control shard's live set as a task there (sharing by move).
  if let Some(control) = state.shards.first().copied() {
    let forget = Box::new(SpawnRequest::new(
      Box::pin(async move {
        crate::state::with_handed(|handed| {
          handed.remove(&client_id);
        });
      }),
      None,
    ));
    let _ = slates_rt::registry::send_control(control, Control::Spawn(forget));
  }
  removed
}

/// Deferred replies first: a reply back from another shard (recorded as a completion here,
/// the client's shard) or one whose completion ring was full and may have room now.
fn retry_deferred(state: &mut ShardState) -> bool {
  let mut any = false;
  let deferred = std::mem::take(&mut state.deferred);
  for entry in deferred {
    let Deferred {
      client_index,
      request,
      reply,
      recorded,
    } = entry;
    let id = RequestId::from_word(request);
    let reply = if recorded {
      reply
    } else {
      match state.db.partition().completion(id.client, id.sequence) {
        Seen::New => record_completion(state, id, reply),
        _ => reply,
      }
    };
    if send_reply(state, client_index, request, &reply) {
      any = true;
    } else {
      state.deferred.push(Deferred {
        client_index,
        request,
        reply,
        recorded: true,
      });
    }
  }
  any
}

/// Up to a batch of one client's requests.
fn serve_client(state: &mut ShardState, index: u32, handle: Handle<ClientSlot>, now: u64) -> bool {
  let mut any = false;
  let batch = state.config.runtime.batch.max(1);
  for _ in 0..batch {
    let taken = match state.clients.get_mut(handle) {
      Ok(c) => c.end.try_take(),
      Err(_) => break,
    };
    let request = match taken {
      Ok(Some(r)) => r,
      Ok(None) => break,
      Err(_) => {
        any = true;
        continue;
      }
    };
    any = true;
    if let Ok(c) = state.clients.get_mut(handle) {
      c.last_seen_ns = now;
    }
    if matches!(request.kind, SlotKind::Heartbeat | SlotKind::Cancel) {
      continue;
    }
    match serve(state, handle, &request) {
      Served::Reply(reply) => {
        if !send_reply(state, index, request.request, &reply) {
          state.deferred.push(Deferred {
            client_index: index,
            request: request.request,
            reply,
            recorded: true,
          });
        }
      }
      Served::Forwarded => {}
    }
  }
  any
}

/// Writes a reply into the client's completion ring; false when the ring is full (the reply
/// is kept and retried; never dropped).
fn send_reply(state: &mut ShardState, client_index: u32, request: u64, reply: &ReplyBody) -> bool {
  let Some(generation) = state.clients.generation_at(client_index) else {
    return true;
  };
  let handle = Handle::from_raw(client_index, generation);
  let Ok(client) = state.clients.get_mut(handle) else {
    return true;
  };
  let index = client.end.next_reply_index();
  let slot = match pack(
    client.end.region_mut(),
    Direction::Reply,
    index,
    request,
    reply,
  ) {
    Ok(s) => s,
    Err(_) => {
      let fallback = ReplyBody::Refused {
        refusal: Refusal::BadRequest {
          reason: "reply too large for the bulk area".to_owned(),
        },
      };
      match pack(
        client.end.region_mut(),
        Direction::Reply,
        index,
        request,
        &fallback,
      ) {
        Ok(s) => s,
        Err(_) => return true,
      }
    }
  };
  match client.end.reply(&slot) {
    Ok(()) => true,
    Err(IpcError::RingFull) => false,
    Err(_) => true,
  }
}

/// The wire volume id of a record (for tests and the CLI).
pub fn wire_id(id: DbVolumeId) -> VolumeId {
  to_wire_volume(id)
}

/// The rights the owner holds, for the status of access lists (§4.13).
pub fn owner_rights() -> AccessEntry {
  AccessEntry {
    principal: Principal::Uid { uid: 0 },
    rights: Rights {
      read: true,
      write: true,
      admin: true,
    },
  }
}

/// Whether the daemon's parked flag should be set on a client's region: exported for the
/// server task.
pub fn mark_parked(state: &ShardState, parked: bool) {
  for (_, c) in state.clients.iter() {
    let _ = c.end.set_parked(parked);
  }
  let _ = Ordering::Relaxed;
}

/// What recovery rebuilt on this shard (§2.6 step 2; §4.8 "replay on start").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rebuilt {
  /// Volumes given a live tree again.
  pub volumes: usize,
  /// Volumes the catalog holds that could not be rebuilt (logged with the reason; a health
  /// signal, `RECOVERY_SKIPPED`).
  pub skipped: usize,
  /// Snapshots the daemon's memory alone held, reconciled out of the catalog.
  pub snapshots_dropped: usize,
  /// Attachments reconciled out of the catalog (their clients attach again).
  pub attachments_dropped: usize,
  /// Merge volumes rebuilt (§4.16): greens with their persisted chain replayed, works reset to a
  /// fresh clone of their green's head (their scratch edits did not survive).
  pub merge_volumes: usize,
}

/// Rebuilds the recovered catalog's volumes into live state after a daemon start over a
/// segment with history: each volume that is not destroyed gets a live tree again (an
/// overlay's base re-opened from the recorded path, a scratch empty; RAM only, so what the
/// old process held is gone: R1, D-26), its reservation taken again, and what only that
/// process's memory held is reconciled in the log so the catalog stays true: snapshots
/// placed nowhere but locally are destroyed and the head reset, attachments removed.
/// Leases keep their terms (the wheel was rebuilt by recovery) and expire on their own.
/// Rebuilds the shard's volumes from the recovered catalog after a restart. This restores each
/// volume's **identity and policy** (name, size, name-equivalence) from the anchor-persisted
/// records — *not* its content: a rebuilt volume's scratch bytes are recreated **empty** and its
/// unplaced local snapshots are dropped ([`reconcile_lost`]), because volume content (the CoW
/// dirtree, the arena, the inode table) is not yet anchor-backed. Restoring content across a
/// restart is BUG-11 / GAP-A9-6 (§4.8, D-18): the owed re-architecture that persists content roots,
/// bytes and witnesses in anchor-owned RAM with an atomic recovery boundary. The counts this
/// returns name the loss honestly (`snapshots_dropped`, `attachments_dropped`), never dress it as
/// content survival.
pub fn rebuild_recovered(state: &mut ShardState) -> Rebuilt {
  let records: Vec<VolumeRecord> = state
    .db
    .partition()
    .volumes()
    .into_iter()
    .filter(|v| !matches!(v.state, VolumeState::Destroying | VolumeState::Destroyed))
    .cloned()
    .collect();
  // The shard's recovery images from anchor-owned RAM (§4.8), by volume id. Empty when there is no
  // content object (a degraded build), or a fresh one with nothing published yet.
  let images = recover_images(state);
  let mut rebuilt = Rebuilt::default();
  let mut max_prefix = state.next_prefix;
  // Greens first, so a work can seed from a rebuilt green (§4.16): a green's merge chain is replayed
  // into a fresh engine, restoring its content and versions.
  for record in &records {
    if matches!(record.policy.role, Role::Green { .. }) {
      rebuild_green(state, record);
      rebuilt.merge_volumes += 1;
    }
  }
  for record in &records {
    match record.policy.role {
      // Rebuilt in the pass above; here only its content-less attachments are reconciled out.
      Role::Green { .. } => {}
      Role::Work { green, .. } => {
        rebuild_work(state, record, green);
        rebuilt.merge_volumes += 1;
      }
      Role::Plain => match rebuild_volume(state, record, images.get(&record.id.bytes)) {
        Ok(prefix) => {
          rebuilt.volumes += 1;
          max_prefix = max_prefix.max(prefix.wrapping_add(1).max(1));
        }
        Err(reason) => {
          rebuilt.skipped += 1;
          eprintln!(
            "slates-server: partition {}: volume {} not rebuilt: {reason}",
            state.partition, record.name
          );
          continue;
        }
      },
    }
    let (snapshots, attachments) = reconcile_lost(state, record);
    rebuilt.snapshots_dropped += snapshots;
    rebuilt.attachments_dropped += attachments;
  }
  // Hand out prefixes past every recovered one, so a new volume never collides with a recovered
  // volume's inode numbers (the prefixes came from the images, not from `next_prefix`).
  state.next_prefix = max_prefix;
  rebuilt
}

/// Rebuilds a recovered green volume (§4.16, §4.8): a fresh merge engine with its persisted chain
/// replayed in order, so its content, versions, last-changed index and dedup set return exactly as
/// before the restart. A corrupt chain entry stops the replay there — the green recovers to its last
/// good version, logged, rather than presenting a wrong later state.
fn rebuild_green(state: &mut ShardState, record: &VolumeRecord) {
  let chain: Vec<Vec<u8>> = state.db.partition().green_chain(record.id).to_vec();
  let mut green = slates_merge::engine::Green::new();
  for (version, bytes) in chain.iter().enumerate() {
    match slates_merge::engine::Increment::decode(bytes) {
      Ok(inc) => {
        let _ = green.submit(&inc);
      }
      Err(e) => {
        eprintln!(
          "slates-server: partition {}: green {} chain entry {version} is corrupt, replay stops: {e}",
          state.partition, record.name
        );
        break;
      }
    }
  }
  state.greens.insert(record.id, green);
}

/// Rebuilds a recovered work volume as a fresh clone of its green's current head (§4.16): a work's
/// declared edits are scratch and do not survive a restart (BUG-11 class), so the work is reset —
/// seeded with the green's content and based on its head — never presenting lost edits as if kept.
/// Skipped when its green is gone (the work has nothing to be over).
fn rebuild_work(state: &mut ShardState, record: &VolumeRecord, green: DbVolumeId) {
  let Some(engine) = state.greens.get(&green) else {
    return;
  };
  let base_version = engine.head();
  let content = engine
    .files()
    .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
    .collect();
  state.works.insert(
    record.id,
    crate::state::WorkState {
      green,
      base_version,
      journal: Vec::new(),
      content,
    },
  );
}

/// The shard's recovery images from its slice of the anchor content object (§4.8), by volume id.
/// A torn or malformed image logs and yields nothing for that shard (each volume then refuses as
/// unrecoverable rather than presenting empty), matching §4.8's "never an empty success".
fn recover_images(state: &ShardState) -> std::collections::BTreeMap<[u8; 16], VolumeImage> {
  let (start, end) = state.content_range;
  let Some(object) = &state.content else {
    return std::collections::BTreeMap::new();
  };
  if end <= start || end > object.len() {
    return std::collections::BTreeMap::new();
  }
  match ShardImage::read_from(&object.bytes()[start..end]) {
    Ok(Some(shard)) => shard
      .volumes
      .into_iter()
      .map(|keyed| (keyed.key, keyed.image))
      .collect(),
    Ok(None) => std::collections::BTreeMap::new(),
    Err(e) => {
      eprintln!(
        "slates-server: partition {}: shard image unreadable: {e}",
        state.partition
      );
      std::collections::BTreeMap::new()
    }
  }
}

/// Publishes the shard's volumes as one recovery image into its slice of the anchor content object
/// (§4.8), so a restart recovers their content from anchor-owned RAM. A no-op without a content
/// object. Efficiency gate: it re-images every volume on each call; an incremental or
/// barrier-batched publish is owed (docs/wip/recovery.md).
pub fn publish_shard(state: &mut ShardState) {
  let (start, end) = state.content_range;
  if state.content.is_none() || end <= start {
    return;
  }
  let mut keyed = Vec::new();
  for (_, slot) in state.volumes.iter() {
    match slot.volume.to_image(&state.store) {
      Ok(image) => keyed.push(KeyedImage {
        key: slot.id.bytes,
        image,
      }),
      // A volume the image cannot yet hold (an overlay with base-backed inodes, whose base recovery
      // is its own gate) is *skipped*, not a barrier — publishing the rest of the shard, so one such
      // volume never blocks every other volume's recovery. The skipped volume recovers by its own
      // path (an overlay reopens its base); a scratch volume that could not be imaged refuses on
      // recovery rather than presenting empty, which is contained to that volume.
      Err(e) => {
        crate::daemon::PUBLISH_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        eprintln!(
          "slates-server: partition {}: a volume was not imaged, skipped: {e}",
          state.partition
        );
      }
    }
  }
  let shard = ShardImage::new(keyed);
  if let Some(object) = state.content.as_mut()
    && let Some(slice) = object.bytes_mut().get_mut(start..end)
    && let Err(e) = shard.write_to(slice)
  {
    eprintln!(
      "slates-server: partition {}: shard image not published: {e}",
      state.partition
    );
  }
}

/// One recovered volume's live tree, reservation and slot.
/// Recreates one volume from its catalog record: a fresh, **empty** scratch volume with the
/// record's identity, size policy and name-equivalence. It does not restore content — the bytes an
/// agent had written before the restart are gone until content is anchor-backed (BUG-11 /
/// GAP-A9-6). An overlay volume's base is still on disk, so its untouched base entries are served
/// again; only the in-memory overlay (the diverged, copied-up state) is lost.
/// Rebuilds one recovered volume into the shard's store, returning its inode-number prefix (so the
/// caller advances `next_prefix` past it). A scratch volume is rebuilt from its recovery image when
/// one is present (its content, tree and prefix restored, §4.8); with a content object but no image
/// the volume's content was lost, which refuses (`RecoveryIncomplete`) rather than presenting empty;
/// without any content object (a degraded build) it is recreated empty as before (BUG-11).
/// Returns a recovered volume's byte reservation and re-grown dynamic hold to the shard budget, for
/// a recovery that must be refused after they were taken.
fn release_recovered_bytes(
  store: &mut slates_vfs::volume::Store,
  reservation: Option<slates_mem::budget::Reservation>,
  held: u64,
) {
  if let Some(r) = reservation {
    store.budget.release(r);
  }
  if held > 0 {
    store
      .budget
      .release(slates_mem::budget::Reservation { bytes: held });
  }
}

/// Re-acquires a recovered volume's version-slab credits (§4.2 accounting through recovery): its
/// logical inode allowance and its retained-version charge, both admitted before the restart. If the
/// slab shrank below what the recovered state needs, recovery cannot represent it and fails, returning
/// the byte reservation and re-grown hold so a refused recovery leaks neither.
fn recover_version_reservations(
  store: &mut slates_vfs::volume::Store,
  volume: &Volume,
  allowance: u64,
  reservation: Option<slates_mem::budget::Reservation>,
  held: u64,
) -> Result<Option<slates_mem::budget::VersionCredit>, String> {
  let version_credit = match store.versions.reserve(allowance) {
    Ok(c) => Some(c),
    Err(e) => {
      release_recovered_bytes(store, reservation, held);
      return Err(format!(
        "recovered inode allowance exceeds the version slab: {e}"
      ));
    }
  };
  let retained = volume.retained_versions();
  if store.versions.charge_retention(retained).is_err() {
    if let Some(c) = version_credit {
      store.versions.release(c);
    }
    release_recovered_bytes(store, reservation, held);
    return Err(format!(
      "recovered retained versions ({retained}) exceed the version slab"
    ));
  }
  Ok(version_credit)
}

fn rebuild_volume(
  state: &mut ShardState,
  record: &VolumeRecord,
  image: Option<&VolumeImage>,
) -> Result<u16, String> {
  let size = wire_size(record.policy.size);
  let reservation = match size {
    SizeClass::Bounded { limit } => Some(
      state
        .store
        .budget
        .reserve(limit)
        .map_err(|e| e.to_string())?,
    ),
    SizeClass::Dynamic { .. } => None,
  };
  let built = build_recovered_volume(state, record, image, size);
  let (mut volume, host, prefix) = match built {
    Ok(triple) => triple,
    Err(reason) => {
      if let Some(r) = reservation {
        state.store.budget.release(r);
      }
      return Err(reason);
    }
  };
  // A recovered dynamic volume's growth was committed to the shard budget before the crash; re-acquire
  // it now so the budget reflects it and later admissions account for it (§4.2 accounting through
  // recovery). It was admitted before the restart, so it fits unless the capacity itself shrank; on
  // teardown it is released through `budget_hold`, the same as a running volume's.
  let held = volume.budget_hold();
  if held > 0 && state.store.budget.grow(held).is_err() {
    if let Some(r) = reservation {
      state.store.budget.release(r);
    }
    return Err("recovered dynamic growth exceeds the shard budget".to_string());
  }
  // Re-admit the inode dimension (§4.2): the fair share, but never below what the recovered volume
  // already holds — recovery does not refuse inodes that were admitted before the restart.
  let allowance = inode_allowance(state, size).max(volume.inode_usage().0);
  let _ = volume.set_inode_allowance(allowance);
  let entries = entry_allowance(size).max(volume.entry_usage().0);
  let _ = volume.set_entry_allowance(entries);
  // Re-acquire the version reservation and re-establish the retained-version charge (§4.2 accounting
  // through recovery), giving back the byte reservation and re-grown hold if the slab shrank.
  let version_credit =
    recover_version_reservations(&mut state.store, &volume, allowance, reservation, held)?;
  let slot = VolumeSlot {
    id: record.id,
    name: record.name.clone(),
    volume,
    host,
    reservation,
    version_credit,
  };
  let handle = state.volumes.insert(slot).map_err(|e| e.to_string())?;
  state.by_id.insert(record.id, handle);
  Ok(prefix)
}

/// Builds the recovered volume and reports its inode-number prefix. See [`rebuild_volume`] for the
/// scratch-volume cases; a base-backed volume re-opens its base (its content is on the base, not in
/// the image; base recovery through retained handles is its own gate, §4.8).
fn build_recovered_volume(
  state: &mut ShardState,
  record: &VolumeRecord,
  image: Option<&VolumeImage>,
  size: SizeClass,
) -> Result<(Volume, Option<OsHost>, u16), String> {
  let names = match record.policy.names {
    DbNamePolicy::Exact => NamePolicy::Exact,
    DbNamePolicy::Fold => NamePolicy::Fold,
  };
  match (&record.base, image) {
    (BaseRecord::Scratch, Some(image)) => {
      let quota = quota_for(size);
      let journal = journal_bytes_for(state, &quota);
      let volume = Volume::from_image(&mut state.store, image, Box::new(HostClock::new()), journal)
        .map_err(|e| e.to_string())?;
      Ok((volume, None, image.prefix))
    }
    (BaseRecord::Scratch, None) if state.content.is_some() => {
      Err("RecoveryIncomplete: no content image for the volume".to_owned())
    }
    (BaseRecord::Scratch, None) => {
      let config = volume_config(state, names, quota_for(size));
      let prefix = config.prefix;
      Volume::create(&mut state.store, config)
        .map(|v| (v, None, prefix))
        .map_err(|e| e.to_string())
    }
    (BaseRecord::Path { path }, _) => {
      let config = volume_config(state, names, quota_for(size));
      let prefix = config.prefix;
      open_base(state, path, config)
        .map(|(v, h)| (v, h, prefix))
        .map_err(|reply| match *reply {
          ReplyBody::Refused { refusal } => refusal_name(&refusal).to_owned(),
          _ => "refused".to_owned(),
        })
    }
  }
}

/// Reconciles what the old process's memory alone held: local-only snapshots (and the head
/// they may have been) and attachments; each a recorded operation, so replay agrees.
/// Drops the state a rebuilt volume cannot honor: its **local** (unplaced) snapshots and its
/// attachments, whose in-memory content did not survive the restart. Returns the (snapshots,
/// attachments) dropped, so the caller reports the loss rather than implying it was recovered. A
/// placed snapshot (one durably held elsewhere) is not dropped here; only local, content-less
/// snapshots are. This is the honest reconciliation until content is anchor-backed (BUG-11).
/// Whether the volume's recovery image rebuilt this snapshot into the vfs volume (§4.8), so it is
/// kept rather than reconciled out of the catalog. The db snapshot id packs the vfs snapshot's slot
/// and generation as `(index << 32) | generation`.
fn recovered_snapshot(
  state: &ShardState,
  handle: Option<Handle<VolumeSlot>>,
  id: DbSnapshotId,
) -> bool {
  let Some(handle) = handle else {
    return false;
  };
  let Ok(slot) = state.volumes.get(handle) else {
    return false;
  };
  let vfs_id = slates_vfs::ids::SnapshotId {
    index: u32::try_from(id.value >> u32::BITS).unwrap_or(u32::MAX),
    generation: u32::try_from(id.value & u64::from(u32::MAX)).unwrap_or(u32::MAX),
  };
  slot.volume.snapshot_info(vfs_id).is_ok()
}

fn reconcile_lost(state: &mut ShardState, record: &VolumeRecord) -> (usize, usize) {
  let now = state.clock.monotonic_ns();
  // Local-only snapshots that the volume's recovery image did not bring back are genuinely lost and
  // reconciled out of the catalog; one the image *did* rebuild (§4.8) is kept, so its content and
  // the head that points at it survive the restart.
  let candidates: Vec<DbSnapshotId> = state
    .db
    .partition()
    .snapshots_of(record.id)
    .iter()
    .filter(|s| matches!(s.placed, PlacementState::Local))
    .map(|s| s.id)
    .collect();
  let handle = state.by_id.get(&record.id).copied();
  let lost: Vec<DbSnapshotId> = candidates
    .into_iter()
    .filter(|id| !recovered_snapshot(state, handle, *id))
    .collect();
  let mut snapshots = 0;
  for id in &lost {
    let op = Op::SnapshotDestroyed {
      volume: record.id,
      id: *id,
    };
    if state.db.mutate(&mut state.segment, &op, now).is_ok() {
      snapshots += 1;
    }
  }
  if lost.contains(&record.head) {
    let op = Op::VolumeHeadAdvanced {
      id: record.id,
      head: DbSnapshotId::default(),
      epoch: record.epoch.saturating_add(1),
    };
    let _ = state.db.mutate(&mut state.segment, &op, now);
  }
  let attached: Vec<u64> = state
    .db
    .partition()
    .attachments_of(record.id)
    .iter()
    .map(|a| a.id)
    .collect();
  let mut attachments = 0;
  for id in attached {
    if state
      .db
      .mutate(&mut state.segment, &Op::AttachmentRemoved { id }, now)
      .is_ok()
    {
      attachments += 1;
    }
  }
  (snapshots, attachments)
}
