//! The lifecycle verbs of §4.4 as the shard serves them from a client's ring: one step each,
//! no awaits inside (§4.8 "Transactions"); every mutation is a database operation appended
//! before the reply (exactly-once by completion record, §4.9); every verb checks the
//! principal's right (§4.13) and, where the design says so, the lease and its epoch (D-16).

use std::sync::atomic::Ordering;

use slates_base::OsHost;
use slates_db::Op;
use slates_db::catalog::{
  AccessEntry, AttachForm, AttachmentRecord, BaseRecord, CompletionRecord, Consumer, LeaseRecord,
  LineageEdge, NamePolicy as DbNamePolicy, PlacementState, PolicyRecord, Principal, Rights, Role,
  SizeClass as DbSizeClass, SnapshotId as DbSnapshotId, SnapshotRecord, VolumeId as DbVolumeId,
  VolumeRecord, VolumeState,
};
use slates_ipc::protocol::{
  Direction, Intent, NamePolicy, Refusal, ReplyBody, RequestBody, SizeClass, SnapshotId,
  StatusReport, VolumeId, VolumeSummary, pack, unpack,
};
use slates_ipc::slot::SlotKind;
use slates_ipc::{IpcError, Request};
use slates_machine::derived;
use slates_mem::Handle;
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
use slates_vfs::base::BaseConfig;
use slates_vfs::clock::{Clock, HostClock};
use slates_vfs::host::HostFs;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::{PressureSource, Quota};
use slates_vfs::volume::{DestroyProgress, Volume, VolumeConfig};
use slates_wire::Wire;
use slates_wire::request::{RequestId, Seen};

use crate::error::{refusal_of_db, refusal_of_vfs};
use crate::state::{ClientSlot, ShardState, VolumeSlot};

/// Shape: the operator's failover SLO, the lease term's ceiling (Gray & Cheriton: seconds;
/// §4.4 "Derived constants"): ten seconds until the CLI takes the operator's value.
const FAILOVER_SLO_NS: u64 = 10_000_000_000;
/// Shape: the share of a volume's quota its journal may take, parts per thousand (ratified
/// GAPS §5: the op log of a bounded volume stays a small fraction of its bytes).
const JOURNAL_SHARE_PERMILLE: u64 = 10;
/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the destroy slice's budget as a share of the shard's step budget, parts per
/// thousand: half, so a destroy never takes the whole step from the clients.
const DESTROY_SLICE_PERMILLE: u64 = 500;

/// The host's live memory as a dynamic quota's pressure source (§4.2: growth only while the
/// projected free memory after it stays above the reserve).
struct HostPressure {
  /// Derived: the free floor, the shard's measured peak burst (the budget's floor).
  floor: u64,
}

impl PressureSource for HostPressure {
  fn may_grow(&mut self, bytes: u64) -> bool {
    match slates_machine::facts::Facts::memory_available_now() {
      Some(available) => available.saturating_sub(bytes) > self.floor,
      None => false,
    }
  }
}

fn to_db_volume(id: VolumeId) -> DbVolumeId {
  DbVolumeId { bytes: id.bytes }
}

fn to_wire_volume(id: DbVolumeId) -> VolumeId {
  VolumeId { bytes: id.bytes }
}

fn to_db_snapshot(id: SnapshotId) -> DbSnapshotId {
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

/// A fresh volume id: the shard in the high bytes (the creator host's place in Phase 8), the
/// clock and a counter below, so ids never repeat on this host.
fn fresh_volume_id(state: &mut ShardState) -> DbVolumeId {
  let mut bytes = [0u8; 16];
  bytes[..2].copy_from_slice(&state.shard.to_be_bytes());
  bytes[2..10].copy_from_slice(&state.clock.monotonic_ns().to_be_bytes());
  let count = state.db.next_seq();
  bytes[10..].copy_from_slice(&count.to_be_bytes()[2..]);
  DbVolumeId { bytes }
}

/// The rights a principal holds on a volume (§4.13): the owner holds every right.
fn rights_of(record: &VolumeRecord, principal: &Principal) -> Rights {
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

fn forbidden(verb: &str) -> ReplyBody {
  ReplyBody::Refused {
    refusal: Refusal::Forbidden {
      verb: verb.to_owned(),
    },
  }
}

fn refused(refusal: Refusal) -> ReplyBody {
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

/// The volume a request is about, when it is about one.
fn volume_of(body: &RequestBody) -> Option<VolumeId> {
  match body {
    RequestBody::Snapshot { volume }
    | RequestBody::Clone { volume, .. }
    | RequestBody::Attach { volume, .. }
    | RequestBody::Resize { volume, .. }
    | RequestBody::Destroy { volume }
    | RequestBody::Status { volume }
    | RequestBody::ReadBase { volume, .. }
    | RequestBody::Rewitness { volume, .. }
    | RequestBody::Pin { volume, .. } => Some(*volume),
    RequestBody::Create { .. }
    | RequestBody::Detach { .. }
    | RequestBody::List
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
  if let Some(volume) = volume_of(&body)
    && owner_of(volume) != state.shard
  {
    return forward(
      state,
      client.index(),
      request.request,
      client_id,
      principal,
      body,
      owner_of(volume),
    );
  }
  let reply = dispatch(state, client_id, &principal, body);
  Served::Reply(record_completion(state, id, reply))
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
  let origin = state.shard;
  let task = SpawnRequest::new(
    Box::pin(async move {
      let reply = crate::state::with_state(|s| dispatch(s, client_id, &principal, body))
        .unwrap_or_else(|| refused(Refusal::NotFound));
      let back = SpawnRequest::new(
        Box::pin(async move {
          crate::state::deliver(client_index, request, reply);
        }),
        None,
      );
      let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
    }),
    None,
  );
  match slates_rt::registry::send_control(owner, Control::Spawn(Box::new(task))) {
    Ok(()) => Served::Forwarded,
    Err(_) => Served::Reply(refused(Refusal::NotFound)),
  }
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
    crate::state::deliver(client_index, request, ReplyBody::Listed { volumes });
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
    Refusal::BadRequest { .. } => "bad_request",
  }
}

fn dispatch(
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
fn find(
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

fn volume_config(state: &mut ShardState, names: NamePolicy, quota: Quota) -> VolumeConfig {
  let prefix = state.next_prefix;
  state.next_prefix = state.next_prefix.wrapping_add(1).max(1);
  let journal_bytes = derived!(
    usize::try_from(quota.limit().saturating_mul(JOURNAL_SHARE_PERMILLE) / PERMILLE)
      .unwrap_or(usize::MAX)
      .max(usize::try_from(state.config.geometry.page).unwrap_or(1)),
    "quota × JOURNAL_SHARE_PERMILLE / 1000, at least one page",
    ["quota", "GAPS §5 journal share"]
  );
  VolumeConfig {
    prefix,
    names: match names {
      NamePolicy::Exact => NameEquivalence::Exact,
      NamePolicy::Fold => NameEquivalence::Fold,
    },
    quota,
    journal_bytes: journal_bytes.get(),
    clock: Box::new(HostClock::new()),
  }
}

fn quota_for(state: &ShardState, size: SizeClass) -> Quota {
  match size {
    SizeClass::Bounded { limit } => Quota::Bounded { limit },
    SizeClass::Dynamic { max } => Quota::Dynamic {
      max,
      source: Box::new(HostPressure {
        floor: state.budget.floor().get(),
      }),
      granted: 0,
      denied: 0,
    },
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
    SizeClass::Bounded { limit } => match state.budget.reserve(limit) {
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
  let quota = quota_for(state, size);
  let config = volume_config(state, names, quota);
  let (volume, host) = match base {
    None => match Volume::create(&mut state.store, config) {
      Ok(v) => (v, None),
      Err(e) => return give_back(state, reservation, refusal_of_vfs(&e)),
    },
    Some(path) => match open_base(state, path, config) {
      Ok(pair) => pair,
      Err(reply) => return give_back(state, reservation, reply_refusal(*reply)),
    },
  };
  let id = fresh_volume_id(state);
  let record = VolumeRecord {
    id,
    name: name.to_owned(),
    owner_shard: state.shard,
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
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    return give_back(state, reservation, refusal_of_db(&e));
  }
  let slot = VolumeSlot {
    id,
    volume,
    host,
    reservation,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      ReplyBody::Created {
        id: to_wire_volume(id),
      }
    }
    Err(e) => refused(Refusal::BadRequest {
      reason: e.to_string(),
    }),
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
  refusal: Refusal,
) -> ReplyBody {
  if let Some(r) = reservation {
    state.budget.release(r);
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
        placed: PlacementState::Local,
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
    SizeClass::Bounded { limit } => match state.budget.reserve(limit) {
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
  let names = match record.policy.names {
    DbNamePolicy::Exact => NamePolicy::Exact,
    DbNamePolicy::Fold => NamePolicy::Fold,
  };
  let quota = quota_for(state, size);
  let config = volume_config(state, names, quota);
  let cloned = match state.volumes.get_mut(handle) {
    Ok(slot) => Volume::clone_of(
      &state.store,
      &mut slot.volume,
      core_snapshot(snapshot),
      config,
    ),
    Err(_) => return give_back(state, reservation, Refusal::NotFound),
  };
  let volume_core = match cloned {
    Ok(v) => v,
    Err(e) => return give_back(state, reservation, refusal_of_vfs(&e)),
  };
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
      return give_back(state, reservation, refusal_of_db(&e));
    }
  }
  let slot = VolumeSlot {
    id,
    volume: volume_core,
    host: None,
    reservation,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      ReplyBody::Cloned {
        id: to_wire_volume(id),
      }
    }
    Err(e) => refused(Refusal::BadRequest {
      reason: e.to_string(),
    }),
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
        FAILOVER_SLO_NS,
        "the operator's failover SLO (the renewal round trip is microseconds, so one term is the SLO)",
        ["FAILOVER_SLO_NS"]
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
  let attachment = state.next_attachment;
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
    .to_snapshot(0)
    .attachments
    .iter()
    .any(|a| a.volume == record.volume && &a.principal == principal);
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
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  // A bounded volume's reservation moves with the limit: grow first (refused whole if the
  // reserve cannot cover it), shrink after the core accepted the new limit.
  let old = slot.reservation;
  let grow = old.map_or(0, |r| limit.saturating_sub(r.bytes));
  let grown = if grow > 0 {
    match state.budget.reserve(grow) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return refused(Refusal::BudgetExceeded { available });
      }
      Err(e) => {
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
      state.budget.release(g);
    }
    return refused(refusal_of_vfs(&e));
  }
  if let Some(old) = old {
    let combined = old.bytes.saturating_add(grown.map_or(0, |g| g.bytes));
    state
      .budget
      .release(slates_mem::budget::Reservation { bytes: combined });
    match state.budget.reserve(limit) {
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
      let now = state.clock.monotonic_ns();
      let _ = state
        .db
        .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now);
      if let Ok(slot) = state.volumes.remove(handle)
        && let Some(r) = slot.reservation
      {
        state.budget.release(r);
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
  let attachments = state
    .db
    .partition()
    .to_snapshot(0)
    .attachments
    .iter()
    .filter(|a| a.volume == record.id)
    .count();
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
    },
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
  let mut any = retry_deferred(state);
  let handles: Vec<(u32, Handle<ClientSlot>)> =
    state.clients.iter().map(|(h, _)| (h.index(), h)).collect();
  for (index, handle) in handles {
    any |= serve_client(state, index, handle);
  }
  any |= step_destroys(state);
  let now = state.clock.monotonic_ns();
  for expired in state.db.partition_mut().expired_leases(now) {
    let _ = state.db.mutate(
      &mut state.segment,
      &Op::LeaseReleased { volume: expired },
      now,
    );
    any = true;
  }
  any
}

/// Deferred replies first: a reply back from another shard (recorded as a completion here,
/// the client's shard) or one whose completion ring was full and may have room now.
fn retry_deferred(state: &mut ShardState) -> bool {
  let mut any = false;
  let deferred = std::mem::take(&mut state.deferred);
  for (client_index, request, reply) in deferred {
    let id = RequestId::from_word(request);
    let reply = match state.db.partition().completion(id.client, id.sequence) {
      Seen::New => record_completion(state, id, reply),
      _ => reply,
    };
    if send_reply(state, client_index, request, &reply) {
      any = true;
    } else {
      state.deferred.push((client_index, request, reply));
    }
  }
  any
}

/// Up to a batch of one client's requests.
fn serve_client(state: &mut ShardState, index: u32, handle: Handle<ClientSlot>) -> bool {
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
    if matches!(request.kind, SlotKind::Heartbeat | SlotKind::Cancel) {
      continue;
    }
    match serve(state, handle, &request) {
      Served::Reply(reply) => {
        if !send_reply(state, index, request.request, &reply) {
          state.deferred.push((index, request.request, reply));
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
