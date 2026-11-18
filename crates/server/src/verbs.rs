//! The lifecycle verbs of §4.4 as the shard serves them from a client's ring: one step each,
//! no awaits inside (§4.8 "Transactions"); every mutation is a database operation appended
//! before the reply (exactly-once by completion record, §4.9); every verb checks the
//! principal's right (§4.13) and, where the design says so, the lease and its epoch (D-16).

use std::sync::atomic::Ordering;

use slates_base::OsHost;
use slates_db::Op;
use slates_db::catalog::{
  AccessEntry, AttachForm, AttachmentRecord, BaseRecord, CompletionRecord, Consumer,
  ConsumerRecord, LeaseRecord, LineageEdge, NamePolicy as DbNamePolicy, PlacementState,
  PolicyRecord, Principal, Rights, Role, SizeClass as DbSizeClass, SnapshotId as DbSnapshotId,
  SnapshotRecord, VolumeId as DbVolumeId, VolumeRecord, VolumeState,
};
use slates_db::register::{HostId, ObjectId, RegionId, RootConfiguration};
use slates_db::{DbError, Partition};
use slates_ipc::protocol::{
  AttachRequest, AttachTransport, DaemonReport, Established, FleetReport, FreshnessBasis,
  GroupReport, HealthSignal, Intent, NamePolicy, PlacedState, ReadAt, ReadWritePolicy,
  RecoveryCause, Refusal, RefusalCount, ReplyBody, RequestBody, Scope, ShardReport, Signal,
  SizeClass, SnapshotId, StatusReport, UnsupportedReason, VolumeId, VolumeSummary, WorkOp,
  decode_body, encode_body, unpack,
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
use slates_vfs::recover::{ImageOut, ShardImage, VolumeImage};
use slates_vfs::volume::{DestroyProgress, Volume, VolumeConfig};
use slates_wire::Wire;
use slates_wire::observe::{Chokepoint, SpanContext};
use slates_wire::request::{RequestId, Seen};

use crate::error::{refusal_of_db, refusal_of_mem, refusal_of_vfs};
use crate::state::{ClientSlot, Deferred, PendingForward, ShardState, VolumeSlot};

/// Format: parts per thousand.
const PERMILLE: u64 = 1000;
/// Shape: the destroy slice's budget as a share of the shard's step budget, parts per
/// thousand: half, so a destroy never takes the whole step from the clients.
const DESTROY_SLICE_PERMILLE: u64 = 500;

pub(crate) fn to_db_volume(id: VolumeId) -> DbVolumeId {
  DbVolumeId { bytes: id.bytes }
}

pub(crate) fn to_wire_volume(id: DbVolumeId) -> VolumeId {
  VolumeId { bytes: id.bytes }
}

/// A wire principal (as `share` names one) as the catalog's, with the host it names (A-237).
fn to_db_principal(principal: &slates_ipc::protocol::Principal) -> Principal {
  match principal {
    slates_ipc::protocol::Principal::Uid { host, uid } => Principal::Uid {
      host: slates_db::HostId(*host),
      uid: *uid,
    },
    slates_ipc::protocol::Principal::Consumer {
      host,
      account,
      consumer,
    } => Principal::Consumer {
      host: slates_db::HostId(*host),
      account: *account,
      consumer: *consumer,
    },
  }
}

/// A catalog principal as the wire names it (A-237), host included; `None` for one a client never names (a SID, a
/// certificate).
fn to_wire_principal(principal: &Principal) -> Option<slates_ipc::protocol::Principal> {
  match principal {
    Principal::Uid { host, uid } => Some(slates_ipc::protocol::Principal::Uid {
      host: host.0,
      uid: *uid,
    }),
    Principal::Consumer {
      host,
      account,
      consumer,
    } => Some(slates_ipc::protocol::Principal::Consumer {
      host: host.0,
      account: *account,
      consumer: *consumer,
    }),
    Principal::Sid { .. } | Principal::Certificate { .. } => None,
  }
}

/// Wire rights as the catalog's.
fn to_db_rights(rights: slates_ipc::protocol::Rights) -> Rights {
  Rights {
    read: rights.read,
    write: rights.write,
    admin: rights.admin,
  }
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

pub(crate) fn core_snapshot(id: SnapshotId) -> slates_vfs::ids::SnapshotId {
  slates_vfs::ids::SnapshotId {
    index: u32::try_from(id.value >> u32::BITS).unwrap_or(u32::MAX),
    generation: u32::try_from(id.value & u64::from(u32::MAX)).unwrap_or(u32::MAX),
  }
}

/// Releases a clone's pin on its origin snapshot (§4.5): the shard owns both volumes, so when a
/// clone's destroy completes — or a partial clone is abandoned before it is published — the origin
/// snapshot's clone count is decremented so the origin can reclaim that snapshot. Best-effort: the
/// origin may already be gone, or the snapshot never pinned.
pub(crate) fn unpin_origin(
  state: &mut ShardState,
  origin: Handle<VolumeSlot>,
  snapshot: SnapshotId,
) {
  if let Ok(slot) = state.volumes.get_mut(origin) {
    let _ = slot.volume.unpin(core_snapshot(snapshot));
  }
}

/// A fresh volume id (§4.8 "Lookup"): the **creator host** in the high 8 bytes — the fleet member id, so the
/// id routes to its creator's node and region (`ObjectId::creator`), which the cross-region lookup depends on —
/// then the owner **partition** (bytes 8-9, what `owner_of` reads to route to the owner shard within the node)
/// and a per-host counter (the low 6 bytes), so ids never repeat on this host. (Before, the high bytes carried
/// the partition, a Phase-8 placeholder: the id then named no creator host, so a cross-region lookup could not
/// resolve the creator's region — the reason slice-1's guard misrouted a real volume.)
///
/// `None` once the counter would reach the register-class bit (`slates_db::register::MAX_OBJECT_COUNTER`): an id
/// past it would read as another register class of an older volume, so the partition mints no more ids
/// ([`ids_exhausted`]). Until 2026-09-30 the counter was truncated silently to its low 48 bits.
pub(crate) fn fresh_volume_id(state: &mut ShardState) -> Option<DbVolumeId> {
  let count = state.db.next_seq();
  if count > slates_db::register::MAX_OBJECT_COUNTER {
    return None;
  }
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&state.fleet.host().0.to_be_bytes());
  bytes[8..10].copy_from_slice(&state.partition.to_be_bytes());
  bytes[10..].copy_from_slice(&count.to_be_bytes()[2..]);
  Some(DbVolumeId { bytes })
}

/// The refusal of a verb that needs a fresh volume id on a partition that has minted its last one.
pub(crate) fn ids_exhausted() -> Refusal {
  Refusal::Unsupported {
    feature: format!(
      "a volume id past this partition's {} ids",
      slates_db::register::MAX_OBJECT_COUNTER
    ),
  }
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

/// The refusal feature (and status count) of a volume whose epoch has no successor (`u64::MAX`): no further
/// snapshot or head advance can fence the one before (AUD-29-26's sibling).
const VOLUME_EPOCH_EXHAUSTED: &str = "volume.epoch_exhausted";
/// The refusal feature of a write lease whose epoch has no successor: no new holder can fence the last.
const LEASE_EPOCH_EXHAUSTED: &str = "volume.lease_epoch_exhausted";

pub(crate) fn refused(refusal: Refusal) -> ReplyBody {
  ReplyBody::Refused { refusal }
}

/// §4.14 / D-12: a daemon that refuses provisioning must say why residency cannot be established.
/// A **refuse-all** refusal (`available == 0`, the pathological case, not an ordinary "volume too
/// big" refusal where `available > 0`) logs the store's whole breakdown once — both the byte budget
/// and the version (inode) budget, each `capacity/committed/retained/headroom` — so a
/// `BudgetExceeded { available: 0 }` names which dimension collapsed and whether it was the capacity
/// or the operation headroom. The `available > 0` guard is why an earlier once-gate saw nothing: an
/// intentional-too-big refusal consumed it. Evidence a boot profile does not carry, for the flaky
/// macOS-runner refusal (2026-09-16). Counted once (a genuinely-full running daemon must not spam).
fn report_first_budget_refusal(
  state: &crate::state::ShardState,
  dimension: &str,
  requested: u64,
  available: u64,
) {
  if available != 0 {
    return;
  }
  static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
  if LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
    return;
  }
  slates_machine::log_line!(
    "slates-server: {dimension} budget refuse-all (first occurrence): requested={requested} \
     config[reserve_per_shard={} shards={} max_inodes={} max_chunks={} metadata_class={}] \
     bytes[capacity={} committed={} retained={} headroom={} replicated={}] \
     versions[capacity={} committed={} retained={} headroom={}] held[stages={} charged={}]",
    state.config.reserve_per_shard,
    state.config.runtime.shards,
    state.config.store.max_inodes,
    state.config.store.max_chunks,
    state.config.store.metadata_class_bytes,
    state.store.budget.capacity(),
    state.store.budget.committed(),
    state.store.budget.retained(),
    state.store.budget.headroom(),
    state.store.budget.replicated(),
    state.store.versions.capacity(),
    state.store.versions.committed(),
    state.store.versions.retained(),
    state.store.versions.headroom(),
    state.held_content.stage_count(),
    state.held_content.charged_bytes(),
  );
}

/// What serving a request produced.
pub enum Served {
  /// A reply for the client now.
  Reply(ReplyBody),
  /// The request went to its volume's owner shard (or was scattered); the reply comes back
  /// through [`crate::state::deliver`].
  Forwarded,
}

/// The owner shard a volume id names (bytes 8-9, just below the creator host in the high 8 bytes; §4.8
/// "Lookup": ids route to owners, no index).
pub fn owner_of(volume: VolumeId) -> u16 {
  u16::from_be_bytes([volume.bytes[8], volume.bytes[9]])
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
  u16::try_from(hash.checked_rem(count).unwrap_or(0)).unwrap_or(u16::MAX)
}

/// The runtime shard a partition lives on in this process.
pub(crate) fn shard_of_partition(state: &ShardState, partition: u16) -> Option<u16> {
  state.shards.get(usize::from(partition)).copied()
}

/// Format: an attachment id carries its owner partition in the high 16 bits and a per-partition
/// counter below, so a detach routes to the partition that holds the record (§4.8 "Lookup":
/// ids route to owners, no global index).
const ATTACHMENT_PARTITION_SHIFT: u64 = 48;

/// The attachment id for a counter on `partition`.
pub(crate) fn attachment_id(partition: u16, counter: u64) -> u64 {
  (u64::from(partition) << ATTACHMENT_PARTITION_SHIFT) | counter
}

/// The next attachment id of this shard's partition, the counter advanced (saturating: a counter that reached
/// its top keeps naming one id, which the catalog then refuses as existing rather than wrapping onto another).
pub(crate) fn next_attachment_id(state: &mut ShardState) -> u64 {
  let id = attachment_id(state.partition, state.next_attachment);
  state.next_attachment = state.next_attachment.saturating_add(1);
  id
}

/// Format: the low 48 bits of an attachment id — its per-partition counter (the bits below
/// `ATTACHMENT_PARTITION_SHIFT`).
const ATTACHMENT_COUNTER_MASK: u64 = (1 << ATTACHMENT_PARTITION_SHIFT) - 1;

/// The attachment counter a shard boots with: one past the highest counter among the partition's
/// recovered attachments — a host mount's survives a restart (AUD-01) — or 1 for a partition holding
/// none. Without this a restarted daemon minted from 1 again and its next attach met a kept record's
/// id, refused `AlreadyExists` on every attach until the counter had passed it.
pub(crate) fn next_attachment_counter(partition: &Partition) -> u64 {
  partition
    .highest_attachment()
    .map_or(1, |id| (id & ATTACHMENT_COUNTER_MASK).saturating_add(1))
}

/// The partition that owns an attachment id.
pub fn owner_of_attachment(id: u64) -> u16 {
  u16::try_from(id >> ATTACHMENT_PARTITION_SHIFT).unwrap_or(0)
}

/// A landing id carries its owner partition in the high 16 bits and a per-partition counter below — the
/// same construction as an attachment id — so a `Grant` naming the landing routes to the partition that
/// holds its presented record (§4.8 "Lookup": ids route to owners, no global index; the CLAUDE.md gotcha
/// "a bare id needs the owner in its high bits"). A bare counter routed the grant to the client's own
/// shard, where the landing was never awaiting, and every grant refused `NotFound`.
/// Format: the partition's shift — the high 16 bits of the 64-bit id, as `ATTACHMENT_PARTITION_SHIFT`.
const LANDING_PARTITION_SHIFT: u64 = 48;

/// The landing id for a counter on `partition`.
pub(crate) fn landing_id(partition: u16, counter: u64) -> u64 {
  (u64::from(partition) << LANDING_PARTITION_SHIFT) | counter
}

/// The partition that owns a landing id.
pub fn owner_of_landing(id: u64) -> u16 {
  u16::try_from(id >> LANDING_PARTITION_SHIFT).unwrap_or(0)
}

/// Format: the low 48 bits of a landing id — its per-partition counter (the bits below
/// `LANDING_PARTITION_SHIFT`).
const LANDING_COUNTER_MASK: u64 = (1 << LANDING_PARTITION_SHIFT) - 1;

/// The landing counter a shard boots with: one past the highest counter among the partition's recovered
/// landing records (durable, §4.8; the guard refuses a duplicate id), or 1 for a partition holding none.
/// Without this a restarted daemon minted from 1 again, and the next `land` after the restart met a
/// recovered record's id and was refused `AlreadyExists` — once per recovered landing, since each
/// refusal still advanced the counter
/// (`docs/bugs/2026-09-19-landing-counter-restarts-at-one-after-a-restart.md`).
pub(crate) fn next_landing_counter(partition: &Partition) -> u64 {
  partition
    .highest_landing()
    .map_or(1, |id| (id & LANDING_COUNTER_MASK).saturating_add(1))
}

/// The volume a request is about, when it is about one.
fn volume_of(body: &RequestBody) -> Option<VolumeId> {
  body.volume()
}

/// The partition that owns a consumer id — the one its enrollment was recorded on (the id carries it
/// exactly as a landing id does, `landing_id`), so a revocation routes to the record and an attestation
/// reads it there (§4.8 "Lookup": ids route to owners).
pub fn owner_of_consumer(id: u64) -> u16 {
  owner_of_landing(id)
}

/// The region a `volume` is homed in, if it is **not** this node's own region — the cross-region redirect
/// signal (§4.8 "Lookup": a home move or region-loss promotion is a configuration exception; the answer comes
/// from the current owner). `None` when the volume is homed here (served locally as usual).
///
/// Fast path: a single-region fleet (the local and laptop default) homes every volume in the one region, so
/// this is a single length check and never a per-request map lookup — cross-region routing costs nothing where
/// there is one region. A volume's home is its explicitly moved home if any, else its creator region (the
/// creator host is the high half of the volume's object id), then any region-loss promotion of that region
/// ([`RootConfiguration::home_of`]). Reads this shard's committed root configuration (as current as the
/// placement reads on the same shard).
fn homed_elsewhere(state: &ShardState, volume: VolumeId) -> Option<u64> {
  home_redirect(
    state.root.configuration(),
    &state.node_regions,
    state.fleet.host(),
    volume,
  )
}

/// The pure cross-region redirect decision (see [`homed_elsewhere`]): the region a `volume` is homed in when
/// that is not `own_host`'s region, else `None`. A single-region fleet short-circuits before any map lookup.
fn home_redirect(
  root: &RootConfiguration,
  node_regions: &std::collections::BTreeMap<HostId, RegionId>,
  own_host: HostId,
  volume: VolumeId,
) -> Option<u64> {
  if root.regions.len() <= 1 {
    return None;
  }
  let region_of = |host: HostId| node_regions.get(&host).copied().unwrap_or(RegionId(0));
  let object = ObjectId(volume.bytes);
  let home = root.home_of(object, region_of(object.creator()));
  (home != region_of(own_host)).then_some(home.0)
}

/// Route bootstrap to the sole shard that drives consensus. The bounded call owns its reply
/// registration through cancellation; an unavailable shard cannot produce a successful bootstrap. Each call (the
/// operation on the control shard, then its publication on every other) is waited for in its target's own time
/// (`xshard::call_in_shard_time`): a starved shard's answer is heard, and a wedged one is refused `Overloaded` after a
/// window with no progress (R3 class A; before, a control shard given a fifth of a core for 1.5 s was refused at the
/// wall second while the bootstrap went on).
fn consensus_on_control(
  state: &ShardState,
  client_index: u32,
  request: u64,
  operation: impl FnOnce(&mut ShardState) -> ReplyBody + Send + 'static,
) -> Served {
  let Some(control) = state.shards.first().copied() else {
    return Served::Reply(refused(Refusal::ConsensusNotInitialized));
  };
  let origin = state.shard;
  let task = slates_rt::futures::spawn(async move {
    let outcome = crate::xshard::call_in_shard_time(
      origin,
      control,
      move |state| {
        let reply = operation(state);
        (
          reply,
          crate::consensus::Publication::capture(state),
          state.shards.clone(),
        )
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let reply = match outcome {
      Some((
        reply @ (ReplyBody::Acknowledged | ReplyBody::RecoveryStarted { .. }),
        publication,
        shards,
      )) => {
        let mut reply = reply;
        for shard in shards.into_iter().filter(|shard| *shard != control) {
          let publication = publication.clone();
          if crate::xshard::call_in_shard_time(
            origin,
            shard,
            move |state| publication.apply(state),
            crate::daemon::LIVENESS_BUDGET_NS,
          )
          .await
          .is_none()
          {
            reply = refused(Refusal::Overloaded { shard });
            break;
          }
        }
        reply
      }
      Some((reply, _, _)) => reply,
      None => refused(Refusal::Overloaded { shard: control }),
    };
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::Overloaded { shard: origin }));
  };
  // Freshly admitted on the current shard; no await can retire its slot before detach.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// Routes an operator's `PromoteRegion` to the **control shard**, where the root group is driven (§4.8, D-14 —
/// region-loss promotion at operator cadence), and delivers the reply back to the client's shard. The work runs
/// in a task on the control shard because it may forward over the transport (an `await`), which `serve` cannot:
/// this node either proposes the promotion (when it leads the root group), forwards it to the leader it knows
/// (so the operator may issue it on **any** node — [`promote_region_here_or_forward`]), or refuses
/// `NotRootLeader` when no leader is known.
fn promote_region_on_root(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  region: u64,
  principal: Principal,
) -> Served {
  let Some(control) = state.shards.first().copied() else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let origin = state.shard;
  let mut unstarted = crate::xshard::AnswerUnstarted::new(
    origin,
    control,
    client_index,
    request,
    refused(Refusal::Overloaded { shard: control }),
  );
  let task = SpawnRequest::new(
    Box::pin(async move {
      unstarted.started();
      let reply = promote_region_here_or_forward(region, principal).await;
      if origin == control {
        crate::state::deliver(client_index, request, reply, false);
      } else {
        crate::xshard::send_reply_home(origin, move || {
          crate::state::deliver(client_index, request, reply, false);
        });
      }
    }),
    None,
  );
  if crate::xshard::send_owing(control, task).is_err() {
    return Served::Reply(refused(Refusal::NotFound));
  }
  Served::Forwarded
}

/// On the control shard, either proposes the region-loss promotion here (when this node leads the root group)
/// or forwards it to the leader this node knows, so the operator may issue `promote-region` on **any** node,
/// not only the leader (§4.8, D-14). The leader is a redirection hint ([`slates_cluster::root_group::RootGroup::leader`]): a stale hint or
/// an unavailable leader session yields `NotRootLeader`, which the operator retries — never a wrong outcome
/// (the target proposes only if it is in fact the leader). `PromoteRegion` is idempotent, so a forward that is
/// retried after it already took is a no-op.
async fn promote_region_here_or_forward(region: u64, principal: Principal) -> ReplyBody {
  enum Route {
    Here,
    Forward(HostId),
    NoLeader,
  }
  let route = crate::state::with_state(|s| {
    if s.root.is_leader() {
      Route::Here
    } else {
      match s.root.leader() {
        Some(leader) => Route::Forward(leader),
        None => Route::NoLeader,
      }
    }
  })
  .unwrap_or(Route::NoLeader);
  match route {
    Route::Here => crate::state::with_state(|s| propose_region_promotion(s, region))
      .unwrap_or_else(|| refused(Refusal::NotFound)),
    Route::Forward(leader) => {
      let request = encode_body(&ForwardedRequest {
        principal,
        body: RequestBody::PromoteRegion { region },
        // PromoteRegion is proposed on the root group and is itself idempotent (a retry after it took is a
        // no-op), so it needs no completion record — the request id and watermark are unused for it.
        request: 0,
        ack_up_to: None,
        ack_owed: Vec::new(),
      });
      match crate::fleet::forward_over_leader_session(
        leader,
        request,
        crate::daemon::LIVENESS_BUDGET_NS,
      )
      .await
      {
        Some(reply) if !reply.is_empty() => {
          decode_body::<ReplyBody>(&reply).unwrap_or_else(|_| refused(Refusal::NotRootLeader))
        }
        // No live session to the leader, or the forward timed out: the operator retries (the promotion is
        // idempotent, so a retry after it took is a no-op).
        _ => refused(Refusal::NotRootLeader),
      }
    }
    Route::NoLeader => refused(Refusal::NotRootLeader),
  }
}

/// A request forwarded to another node over the fleet transport (§4.8 "Lookup"), carrying the requester's
/// `principal` alongside the `body`: the receiving node runs the verb under that principal (relayed over the
/// mutual-TLS session — a peer vouches for the principal it authenticated locally; §4.13 refines the
/// granularity). [`serve_forward`] serves it; [`encode_body`]/[`decode_body`] are its wire.
#[derive(Wire, Clone, Debug)]
pub(crate) struct ForwardedRequest {
  /// The requester's principal, as the origin node authenticated it.
  pub principal: Principal,
  /// The verb to run on the owner.
  pub body: RequestBody,
  /// The origin's request id (packed word): the owner keys the completion under `(origin host, client,
  /// sequence)`, so a forwarded **write** is idempotent — a retry re-forwarded with the same id answers from
  /// the record instead of re-executing. (A read carries it too but does not record.)
  pub request: u64,
  /// The client's acknowledged-sequence watermark, relayed so the owner prunes this client's forwarded
  /// completions the way a local client's are pruned by its acknowledgements (§4.9 RIFL; banned item 8).
  /// `None` before the client has acknowledged anything.
  pub ack_up_to: Option<u32>,
  /// The ids at or below `ack_up_to` the client still owes (its origin window's open ids), relayed so the owner keeps
  /// them open too: a forwarded submit held for its resend must still meet its record (§4.9).
  pub ack_owed: Vec<u32>,
}

/// The volume whose **latest state** a verb serves, so it requires a confirmed owner lease (§4.8 "Leases
/// and reads"; AUD-08): a read at the live head, the current version list, a status, or a since-a-version
/// query. A read explicitly pinned to an immutable point — a green's named version, an attachment's pinned
/// view — returns `None`: it keeps its separate contract (verified content and read rights, no latest-head
/// lease). Control queries, creations of new objects, and writes (gated by durability and the merge-commit
/// wait instead) return `None` too.
fn serves_latest_state(body: &RequestBody) -> Option<VolumeId> {
  match body {
    RequestBody::ReadRange {
      volume,
      at: ReadAt::Head,
      ..
    }
    | RequestBody::ReadWindow {
      volume,
      at: ReadAt::Head,
      ..
    }
    | RequestBody::ReadDir {
      volume,
      at: ReadAt::Head,
      ..
    } => Some(*volume),
    RequestBody::Versions { green } | RequestBody::ChangedSince { green, .. } => Some(*green),
    RequestBody::Status { volume } | RequestBody::Drifted { volume, .. } => Some(*volume),
    _ => None,
  }
}

/// The takeover state as this shard's council holds it (§4.8 "Neighbourhood changes", "Promotion and
/// takeover"): this node's settled and current neighbourhood versions, and every retirement kept — what a
/// status report shows so a stalled takeover says why.
fn takeover_report(state: &ShardState) -> slates_ipc::protocol::TakeoverReport {
  let regional = state.council.configuration();
  let local = state.fleet.host();
  let ids = |hosts: &[HostId]| hosts.iter().map(|host| host.0).collect::<Vec<u64>>();
  let mut retirements: Vec<slates_ipc::protocol::RetirementReport> = regional
    .retired
    .iter()
    .map(
      |(host, retirement)| slates_ipc::protocol::RetirementReport {
        host: host.0,
        version: retirement.version,
        survivors: ids(&retirement.survivors),
        confirmed: ids(&retirement.confirmed),
        unconfirmed: ids(&retirement.unconfirmed),
      },
    )
    .collect();
  retirements.sort_by_key(|retirement| retirement.version);
  slates_ipc::protocol::TakeoverReport {
    settled_generation: regional
      .settled
      .get(&local)
      .map_or(0, |settled| settled.generation),
    neighbourhood_generation: regional
      .neighbourhoods
      .get(&local)
      .map_or(0, |neighbourhood| neighbourhood.generation),
    retirements,
    members: u64::try_from(regional.members.len()).unwrap_or(u64::MAX),
  }
}

/// The scale of a detector allowance on the wire: thousandths, so an expected count well below one survives
/// as an integer.
/// Derived: a unit scale (one thousandth), not a tunable.
const ALLOWANCE_SCALE: f64 = 1_000.0;

/// An allowance in thousandths, rounded; zero for a value that is not a non-negative number, and saturated at
/// the integer's top.
fn thousandths(allowance: f64) -> u64 {
  let scaled = (allowance * ALLOWANCE_SCALE).round();
  if scaled.is_nan() || scaled <= 0.0 {
    return 0;
  }
  if scaled >= u64::MAX as f64 {
    return u64::MAX;
  }
  // The value is finite, positive and below the integer's top (both checked just above).
  #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
  let whole = scaled as u64;
  whole
}

/// What this shard's failure detector found of each peer by its own probes (§4.8 membership): live on the
/// control shard, which runs the one detector; empty on every other shard and on a laptop.
fn detector_report(state: &ShardState) -> Vec<slates_ipc::protocol::DetectorPeerReport> {
  let Some(plane) = state.plane.plane.as_ref() else {
    return Vec::new();
  };
  let detector = plane.detector();
  let local = state.fleet.host();
  detector
    .membership()
    .after(None)
    .filter(|(member, _)| member.0 != local.0)
    .filter_map(|(member, _)| {
      detector.report(member).map(|report| {
        let verdict = detector.verdict(member);
        let judged_by = match (report.configured, verdict) {
          (true, _) => "own",
          (false, None) => "measuring",
          // A provisional verdict promises no bound (hyper-swim's `misfit_verdict`: mistake 1).
          (false, Some(verdict)) if verdict.mistake >= 1.0 => "provisional",
          (false, Some(_)) => "pool",
        };
        slates_ipc::protocol::DetectorPeerReport {
          peer: member.0,
          configured: report.configured,
          suspicions: report.suspicions,
          suspicion_allowance_milli: thousandths(report.suspicion_allowance),
          condemnations: report.condemnations,
          condemnation_allowance_milli: thousandths(report.condemnation_allowance),
          judged_by: judged_by.to_owned(),
          expected_ns: verdict.map_or(0, |verdict| nanos_of(verdict.round_trip)),
          margin_ns: verdict.map_or(0, |verdict| nanos_of(verdict.margin)),
          samples: detector.round_trips_taken(member).unwrap_or(0),
          mistake_milli: verdict.map_or(0, |verdict| thousandths(verdict.mistake)),
          pool_misfit: report.pool_misfit,
          refused: report
            .refused
            .map(|refusal| format!("{refusal:?}"))
            .unwrap_or_default(),
        }
      })
    })
    .collect()
}

/// This shard's failure detector's pooled verdict (live on the control shard, empty elsewhere and on a laptop).
fn detector_pool_report(state: &ShardState) -> slates_ipc::protocol::DetectorPoolReport {
  let Some(plane) = state.plane.plane.as_ref() else {
    return slates_ipc::protocol::DetectorPoolReport::default();
  };
  let pool = plane.detector().pool();
  slates_ipc::protocol::DetectorPoolReport {
    configured: pool.verdict.is_some(),
    samples: pool.samples,
    refused: pool
      .refused
      .map(|refusal| format!("{refusal:?}"))
      .unwrap_or_default(),
    expected_ns: pool
      .verdict
      .map_or(0, |verdict| nanos_of(verdict.round_trip)),
    margin_ns: pool.verdict.map_or(0, |verdict| nanos_of(verdict.margin)),
  }
}

/// This shard's failure detector's relay stage, as it bounds it (live on the control shard, empty elsewhere and on a
/// laptop).
fn detector_relay_report(state: &ShardState) -> slates_ipc::protocol::DetectorRelayReport {
  let Some(plane) = state.plane.plane.as_ref() else {
    return slates_ipc::protocol::DetectorRelayReport::default();
  };
  let relays = plane.detector().relays();
  slates_ipc::protocol::DetectorRelayReport {
    due_ns: relays.due.map_or(0, nanos_of),
    stated: u64::try_from(relays.stated).unwrap_or(u64::MAX),
  }
}

/// A duration as whole nanoseconds, saturating.
fn nanos_of(duration: std::time::Duration) -> u64 {
  u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The member's own wake lateness as its detector measured it (granularity), nanoseconds; zero off the control shard
/// or before measured.
fn detector_granularity_ns(state: &ShardState) -> u64 {
  state
    .plane
    .plane
    .as_ref()
    .and_then(|plane| plane.detector().granularity())
    .map_or(0, nanos_of)
}

/// The objects this node's takeovers have learned and not yet adopted (§4.8 "Promotion and takeover"; the
/// takeover module): what a status report shows as pending, so a stalled takeover shows in any node's status.
fn takeovers_pending(state: &ShardState) -> u64 {
  let pending: usize = state
    .host_takeovers
    .values()
    .map(crate::takeover::HostTakeover::outstanding)
    .sum();
  u64::try_from(pending).unwrap_or(u64::MAX)
}

/// The owner lease's verdict on `object`'s latest state here (§4.8 "Leases and reads"; AUD-08). Reads the
/// fanned owner lease against the object's candidate holders under the installed configuration and the host
/// clock, so a paused shard's lease has already lapsed by the clock when it resumes. It holds when `f` of the
/// other candidates (every one of them when there are fewer) confirmed within the lease bound, within the
/// bounded startup allowance, and on a laptop (`f = 0`, no other candidate needed). Read by the verb gate
/// ([`dispatch`]), the mount bridge (`crate::nfs`, which is Unix's) and the daemon's test observation.
pub(crate) fn lease_verdict(state: &ShardState, object: ObjectId) -> crate::lease::LeaseVerdict {
  // An object this node lost to a successor is the successor's: its copy here is never served as the latest state,
  // however this host's lease stands (A-334, A-338).
  if lost_to_a_successor(state, DbVolumeId { bytes: object.0 }) {
    return crate::lease::LeaseVerdict::LostToSuccessor;
  }
  let config = state.fleet.configuration();
  // The cohorts a successor's promotion quorum can be drawn from: the settled one, and — while this owner's
  // neighbourhood change is in flight and moved the object's cohort — the current one the council may settle
  // at any moment (the joint lease, as the joint writes).
  let cohorts = config.lease_cohorts(object);
  let now = slates_machine::clock::monotonic_ns();
  state.lease.verdict(
    now,
    config.owner,
    config.standing(),
    config.quorum,
    &cohorts,
    state.fleet.members(),
  )
}

/// The configuration version a latest-state request on `object` is refused under while the owner lease does
/// not hold ([`lease_verdict`]), or `None` when it holds. Each refusal is counted by its reason
/// ([`crate::lease::LEASE_SUPERSEDED`], [`crate::lease::LEASE_UNCONFIRMED`]), so a status report tells a
/// supersession from missing confirmations.
pub(crate) fn lease_refusal(state: &mut ShardState, object: ObjectId) -> Option<u64> {
  let reason = lease_verdict(state, object).refusal_count_name()?;
  let count = state.refusals.entry(reason).or_insert(0);
  *count = count.saturating_add(1);
  Some(state.fleet.configuration().version)
}

/// Whether this owner must not serve `volume`'s latest state to a mounted transport now (§4.8 "Leases and
/// reads"; AUD-08, AUD-29-83): the configuration group is not ready, or the owner lease does not hold — the
/// gate the NFS live tree applies before every procedure, shared by the FUSE mount and the guest device, which
/// hold their caller's requests while it stands rather than answer from a stale view (and the WinFsp mount, on
/// Windows). A lease refusal is counted by its reason ([`lease_refusal`]).
pub(crate) fn live_tree_fenced(state: &mut ShardState, volume: DbVolumeId) -> bool {
  !state.consensus_ready || lease_refusal(state, ObjectId(volume.bytes)).is_some()
}

/// Whether a verb is a **read** safe to forward to a volume's owner without a completion record: a
/// volume-scoped query that mutates nothing, so re-serving a retried forward is idempotent (§4.8 "Lookup").
/// A staging verb forwards the same way: a retried put of bytes already written is answered unchanged, and a
/// retried begin only strands a charged buffer that expires within a lease (`crate::staging`).
fn is_forwardable_read(body: &RequestBody) -> bool {
  body.forwards_as_read()
}

/// Whether a verb is a **write** to an existing volume that is safe to forward to that volume's owner: a
/// volume-scoped mutation ([`volume_of`] names the volume, [`mutates_shard_image`] changes it). Forwarded
/// under a completion record keyed by the origin's request id, so a retried forward is exactly-once (§4.8
/// "Lookup"; the owner runs it through [`run_forwarded`]). A create (no existing volume, routed by name) is
/// not one of these — it is placed by the local partitioning, not a home redirect.
fn is_forwardable_write(body: &RequestBody) -> bool {
  body.forwards_as_write()
}

/// Serves a verb forwarded from another node over the fleet transport (§4.8 "Lookup"): decodes the
/// [`ForwardedRequest`], runs it on the volume's owner shard under the relayed principal, and returns the
/// encoded reply. `origin` is the **authenticated** forwarding peer (its mutual-TLS certificate, resolved by
/// the serve loop) — never a self-reported field, so a node cannot forge another's completion key. The
/// verb reaches its owner shard by a cross-shard call, so the reply is produced asynchronously (hence
/// [`slates_transport::endpoint::Endpoint::serve_once_async`]).
///
/// - `PromoteRegion` (operator region-loss) is proposed on the root group.
/// - A forwardable **read** ([`is_forwardable_read`]) runs with no completion record — reads are idempotent.
/// - A forwardable **write** ([`is_forwardable_write`]) runs through [`run_forwarded`], whose completion is
///   keyed by `(origin, client, sequence)`, so a retried forward is exactly-once; the relayed acknowledgement
///   watermark prunes this client's forwarded completions first (RIFL, banned item 8).
/// - Anything else is refused `Unsupported`.
///
/// `control` is the shard this serve loop runs on (the `xshard` origin). Each call to the owner shard is waited for in
/// that shard's own time (`xshard::call_in_shard_time`, R3 class A): before, a peer's read of a volume whose owner shard
/// was given a fifth of a core for 1.5 s was answered `NotFound` at the wall second.
pub(crate) async fn serve_forward(control: u16, origin: HostId, bytes: &[u8]) -> Vec<u8> {
  let Ok(ForwardedRequest {
    principal,
    body,
    request,
    ack_up_to,
    ack_owed,
  }) = decode_body::<ForwardedRequest>(bytes)
  else {
    return Vec::new();
  };
  // The requester is an account of the node this session authenticated, whatever host its request names (A-237): a
  // uid means something only on its host, so the owner judges it as (origin, account) and never by the origin's word.
  let principal = principal.rehosted(origin);
  let requester = principal.clone();
  if let RequestBody::PromoteRegion { region } = body {
    return encode_body(
      &crate::state::with_state(|s| propose_region_promotion(s, region))
        .unwrap_or_else(|| refused(Refusal::NotFound)),
    );
  }
  let Some(volume) = volume_of(&body) else {
    return encode_body(&refused(Refusal::Unsupported {
      feature: "forwarded verb".to_owned(),
    }));
  };
  // A remembered route is only a hint. If this node has learned a different owner or home,
  // refuse before dispatch (and before recording a forwarded completion), so the client can
  // refresh its hint and retry its original request id without a speculative second write.
  let redirect = forward_redirect(volume);
  if let Some(region) = redirect {
    return encode_body(&refused(Refusal::HomedElsewhere { region }));
  }
  let owner = owner_of(volume);
  let Some(shard) = crate::state::with_state(|s| shard_of_partition(s, owner)).flatten() else {
    return encode_body(&refused(Refusal::NotFound));
  };
  let origin_host = origin.0;
  let id = RequestId::from_word(request);
  let reply = if is_forwardable_read(&body) {
    crate::xshard::call_in_shard_time(
      control,
      shard,
      move |s| dispatch(s, FORWARDED_CLIENT, &principal, body),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
  } else if is_forwardable_write(&body) {
    let forwarded = crate::xshard::call_in_shard_time(
      control,
      shard,
      move |s| {
        if let Some(up_to) = ack_up_to {
          prune_forwarded(s, origin_host, id.client, up_to, &ack_owed);
        }
        // The fleet envelope carries no trace context yet: the verb's span declares its cause missing.
        let reply = run_forwarded(s, origin_host, id, 0, &principal, body, None);
        (reply, s.told_owed.take())
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    answer_forwarded_write(forwarded, (control, shard), origin_host, id).await
  } else {
    // A peer's reap of its dead client (A-222) is the one verb served here that is neither; any other is refused.
    Some(crate::remote_attach::serve_unforwardable(control, shard, origin_host, body).await)
  };
  encode_body(&named_if_unshared(reply, (control, shard), volume, requester).await)
}

/// The answer to a forwarded write once its verb has run on the owner `shard`: its reply after the mounts its change owes
/// are told, or, when the verb deferred its acceptance to a fleet commit, the completion that commit records.
async fn answer_forwarded_write(
  forwarded: Option<(Option<ReplyBody>, Option<crate::told::Owed>)>,
  (control, shard): (u16, u16),
  origin_host: u64,
  id: RequestId,
) -> Option<ReplyBody> {
  match forwarded {
    Some((Some(reply), owed)) => {
      // A change the verb recorded is told to every FUSE mount of its volume before the exchange answers (AC-3.3):
      // the wait runs on the owner shard, where the mounts are, and is bounded by its own deadline.
      if let Some(owed) = owed {
        let _ = crate::xshard::task_in_shard_time(
          control,
          shard,
          move || crate::told::wait(owed),
          crate::daemon::LIVENESS_BUDGET_NS,
        )
        .await;
      }
      Some(reply)
    }
    // The verb deferred its acceptance to a fleet commit (AUD-11): this exchange waits for the
    // completion record the commit writes, within the same liveness budget.
    Some((None, _)) => await_deferred_completion(control, shard, origin_host, id).await,
    None => None,
  }
}

/// The region a forward of `volume` should be redirected to, when this node has learned a different owner or home.
fn forward_redirect(volume: VolumeId) -> Option<u64> {
  crate::state::with_state(|state| {
    let object = ObjectId(volume.bytes);
    let local = state.fleet.host();
    let superseded = state
      .fleet
      .object_owner(object)
      .is_some_and(|owner| owner != local);
    let redirect = homed_elsewhere(state, volume)
      .map(|region| (region, "fleet.forward.redirect.homed_elsewhere"))
      .or_else(|| {
        superseded.then(|| {
          (
            state.node_regions.get(&local).map_or(0, |region| region.0),
            "fleet.forward.redirect.superseded",
          )
        })
      });
    if let Some((_, reason)) = redirect {
      state.count(reason, 1);
    }
    redirect.map(|(region, _)| region)
  })
  .flatten()
}

/// A forwarded `Forbidden` as [`not_shared_with`] when the requester has no rights on `volume` at all, judged on its
/// owner `shard`, where the record is ([`rights_of`]). A requester that holds rights was refused for something a share
/// cannot fix (a forward naming another host's attachment), so its `Forbidden` stands. Every other reply as it is; no
/// reply at all is `NotFound`.
async fn named_if_unshared(
  reply: Option<ReplyBody>,
  (control, shard): (u16, u16),
  volume: VolumeId,
  requester: Principal,
) -> ReplyBody {
  let reply = reply.unwrap_or_else(|| refused(Refusal::NotFound));
  if !matches!(
    reply,
    ReplyBody::Refused {
      refusal: Refusal::Forbidden { .. }
    }
  ) {
    return reply;
  }
  let judged = requester.clone();
  let unshared = crate::xshard::call_in_shard_time(
    control,
    shard,
    move |s| {
      let id = slates_db::catalog::VolumeId {
        bytes: volume.bytes,
      };
      s.db
        .partition()
        .volume(id)
        .is_some_and(|record| rights_of(record, &judged) == Rights::default())
    },
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await
  .unwrap_or(false);
  if unshared {
    not_shared_with(reply, &requester)
  } else {
    reply
  }
}

/// A forwarded request's `Forbidden` as the refusal that names its fix (A-237), for a requester holding no rights on the
/// volume ([`named_if_unshared`]): it is another host's account, which only a share naming (its host, its account) lets in, so the client is told exactly that principal.
/// Every other reply, and a requester with no wire form (a certificate), as it is.
fn not_shared_with(reply: ReplyBody, requester: &Principal) -> ReplyBody {
  match (reply, to_wire_principal(requester)) {
    (
      ReplyBody::Refused {
        refusal: Refusal::Forbidden { verb },
      },
      Some(principal),
    ) => refused(Refusal::NotShared { verb, principal }),
    (reply, _) => reply,
  }
}

/// A cross-node forwarded verb whose acceptance was deferred to a fleet commit (AUD-11): polls the
/// owner shard's completion window each period until the commit records the reply, within the
/// liveness budget the exchange runs under. Past the budget the request is refused **retryable**
/// (`Overloaded` names the owner shard): the acceptance still waits on the owner, and the origin's
/// retry meets the completion record once the version commits — never a success the quorum has
/// not committed.
async fn await_deferred_completion(
  control: u16,
  shard: u16,
  origin_host: u64,
  id: RequestId,
) -> Option<ReplyBody> {
  let deadline = slates_rt::futures::now_ns().saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
  loop {
    // Off a shard no poll can be timed: the reply is given up, as at its deadline.
    slates_rt::futures::sleep(crate::daemon::HEARTBEAT_NS)
      .await
      .ok()?;
    let recorded = crate::xshard::call_in_shard_time(
      control,
      shard,
      move |s| match s
        .db
        .partition()
        .completion(origin_host, id.client, id.sequence)
      {
        Seen::Completed(bytes) => Some(bytes),
        Seen::Acknowledged | Seen::New | Seen::Awaiting { .. } => None,
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match recorded {
      Some(Some(bytes)) => {
        return Some(
          ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest)),
        );
      }
      Some(None) if slates_rt::futures::now_ns() < deadline => {}
      Some(None) => return Some(refused(Refusal::Overloaded { shard })),
      None => return None,
    }
  }
}

/// Prunes a forwarded client's completions on this (owner) node up to the acknowledgement watermark the
/// origin relayed, except the ids it still owes, the same way a local client's are pruned by its acknowledgements — so
/// forwarded completions do not grow unbounded (§4.9 RIFL; banned item 8). A no-op when the acknowledgement would
/// release nothing new (the watermark has not advanced and every id still open is still owed), so a stream of
/// forwarded writes at a steady watermark writes no acknowledgement records.
fn prune_forwarded(state: &mut ShardState, origin: u64, client: u32, up_to: u32, owed: &[u32]) {
  let partition = state.db.partition();
  let advances = partition
    .acknowledged_up_to(origin, client)
    .is_none_or(|current| up_to > current)
    || partition
      .owed_completions(origin, client)
      .iter()
      .any(|open| *open <= up_to && !owed.contains(open));
  if !advances {
    return;
  }
  let owed = match acknowledgement_owing(state, up_to, owed) {
    Ok(owed) => owed,
    Err(_) => {
      state.count(ACK_MALFORMED, 1);
      return;
    }
  };
  let now = state.clock.monotonic_ns();
  // Retried by the next advance; a refusal is counted, never silent.
  let recorded = state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      origin,
      client,
      up_to,
      owed,
    },
    now,
  );
  count_secondary(state, recorded);
}

/// Format: the count of acknowledgements refused as malformed, a local client's (answered `BadRequest`) or one an
/// origin relayed with a forwarded write (dropped): an owed id above its own watermark, or more owed ids than the
/// client's completion bound. Nothing of one is released. Shown in `slates status`.
pub(crate) const ACK_MALFORMED: &str = "ack.malformed";

/// The owed ids of an acknowledgement, in order, or the refusal of a malformed one (§4.9): an owed id above its own
/// watermark, or more than the client's completion bound of them. A conforming client owes at most its ring's slots and
/// its hold, so only a hostile list exceeds the bound. It is refused whole, never cut: cutting would release ids the
/// client still owes, and their resends would then be refused `DuplicateRequest` (raft).
fn acknowledgement_owing(
  state: &ShardState,
  up_to: u32,
  owed: &[u32],
) -> Result<Vec<u32>, Refusal> {
  let mut named: Vec<u32> = owed.to_vec();
  named.sort_unstable();
  named.dedup();
  if named.last().is_some_and(|highest| *highest > up_to) {
    return Err(Refusal::BadRequest {
      reason: format!("an acknowledgement up to {up_to} names an owed id above it"),
    });
  }
  let bound = completion_bound(state);
  if u64::try_from(named.len()).unwrap_or(u64::MAX) > bound {
    return Err(Refusal::BadRequest {
      reason: format!(
        "an acknowledgement names {} owed ids, past the client's completion bound of {bound}",
        named.len()
      ),
    });
  }
  Ok(named)
}

/// Counts a secondary record the database refused (`DB_SECONDARY_REFUSED`): one a verb writes beside its own effect
/// and does not fail for (an acknowledgement watermark, a lease released with the last attachment, a lost head's
/// epoch), each retried or superseded by a later record, so a refusal is counted where it was dropped before
/// 2026-10-07.
pub(crate) fn count_secondary<T, E>(state: &mut ShardState, recorded: Result<T, E>) {
  if recorded.is_err() {
    state.count(DB_SECONDARY_REFUSED, 1);
  }
}

/// Format: the status counter of secondary records the database refused ([`count_secondary`]).
pub(crate) const DB_SECONDARY_REFUSED: &str = "db.secondary_refused";

/// Counts under `kind` a step whose refusal the caller cannot act on and must not drop: a rollback's discard of a
/// volume it built (`VOLUME_DISCARD_REFUSED`), an allowance a rebuilt or cloned volume could not take
/// (`VOLUME_ALLOWANCE_REFUSED`).
pub(crate) fn count_kept<T, E>(state: &mut ShardState, kind: &'static str, outcome: Result<T, E>) {
  if outcome.is_err() {
    state.count(kind, 1);
  }
}

/// Format: the status counter of rollbacks whose discard of a partly built volume was refused (its slots and blocks
/// stay held).
pub(crate) const VOLUME_DISCARD_REFUSED: &str = "volume.discard_refused";

/// Format: the status counter of inode or entry allowances a volume refused (it already holds more).
pub(crate) const VOLUME_ALLOWANCE_REFUSED: &str = "volume.allowance_refused";

/// Resolves a remotely homed volume by its creator, this client's cached route or a read-only
/// location exchange, then forwards the verb once (§4.8 Lookup, §4.9 RIFL). Discovery never
/// executes a write. An owner that does not answer is sent the forward again until it is confirmed dead or the takeover
/// bound passes, and then the client is refused `OwnerUnreachable` (retrying the original request id meets its
/// completion record).
fn forward_to_owner(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  request: u64,
  principal: Principal,
  region: u64,
  volume: VolumeId,
  body: RequestBody,
) -> Served {
  let Some(control) = state.shards.first().copied() else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  if let Some(page) = read_ahead_before_forward(state, client, volume, &body) {
    return Served::Reply(page);
  }
  let cached = state
    .clients
    .get(client)
    .ok()
    .and_then(|slot| slot.owner_route);
  let origin = state.shard;
  let (bodies, plan) = window_for(state, client, body);
  let charged = plan.as_ref().map_or(0, |plan| plan.charged);
  let requests: Vec<Vec<u8>> = bodies
    .into_iter()
    .map(|body| forwarded_request(state, request, principal.clone(), body))
    .collect();
  let mut unstarted = crate::xshard::AnswerUnstarted::new(
    origin,
    control,
    client.index(),
    request,
    refused(Refusal::Overloaded { shard: control }),
  );
  let task = SpawnRequest::new(
    Box::pin(async move {
      unstarted.started();
      crate::state::with_state_counted(|s| s.count(FORWARD_CLIENT_CONTROL_STARTED, 1));
      let (replies, route) = resolve_and_forward(volume, region, cached, requests).await;
      crate::state::with_state_counted(|s| s.count(FORWARD_CLIENT_RESOLVED, 1));
      deliver_owner_forward(origin, control, (client, request), replies, (route, plan));
    }),
    None,
  );
  if let Err(refusal) = crate::xshard::send_owing(control, task) {
    state.count(FORWARD_CLIENT_CONTROL_REFUSED, 1);
    if let Ok(slot) = state.clients.get_mut(client) {
      slot.read_ahead_pending = slot.read_ahead_pending.saturating_sub(charged);
      state.read_ahead.credit(charged);
    }
    return Served::Reply(refused(control_refusal(&refusal, control, region)));
  }
  state.count(FORWARD_CLIENT_TO_CONTROL, 1);
  Served::Forwarded
}

/// Counter: a client's verb for a volume another host owns, handed from the client's own shard to the control shard
/// to be forwarded (`forward_to_owner`). With the counters after it, each hop of the forward is counted, so a reply
/// lost in a stall is placed by the first count that falls behind the one before it (fixtures' f = 1 r3k stall,
/// 2026-11-07: requests in flight for 2,670 s with every daemon idle and nothing deferred or dropped).
const FORWARD_CLIENT_TO_CONTROL: &str = "fleet.forward.client.to_control";
/// Counter: such a verb the control shard's channel refused; it is answered `HomedElsewhere`.
const FORWARD_CLIENT_CONTROL_REFUSED: &str = "fleet.forward.client.control_refused";
/// Counter: the control shard's task for such a verb started.
const FORWARD_CLIENT_CONTROL_STARTED: &str = "fleet.forward.client.control_started";
/// Counter: its owner's reply (or the refusal standing for one) was resolved on the control shard.
const FORWARD_CLIENT_RESOLVED: &str = "fleet.forward.client.resolved";
/// Counter: its reply reached the client's shard, to be written into the client's ring ([`finish_owner_forward`]).
const FORWARD_CLIENT_DELIVERED_HOME: &str = "fleet.forward.client.delivered_home";

/// The read-ahead window of one client's forwarded read (`ClientSlot::read_ahead`): the file and view it reads, where
/// the window starts, and the owner's window with its stamp and the file's length. Read whole by the window's first
/// page and answered page by page from here, so a multi-page read takes one round trip to the owner per window, not
/// per page, and every page carries the stamp of one state of the file — the consistency `slates_client` already
/// checks across pages, the read linearizing at its window's fetch (read-ahead as Linux and NFS clients do it, with
/// a large `rsize`). A forwarded write by the same client to the volume, or a new read from its start, replaces it.
///
/// A read that keeps going where its window ends fetches the next as a **batch** of windows sent at once on the
/// owner's session ([`crate::fleet::forward_batch_over_leader_session`]), twice as many as the batch before — the
/// growth Linux's read-ahead gives a sequential reader (`get_next_ra_size`) — so a file of `n` windows costs about
/// `log2(n)` request round trips, not `n`, and a loss inside a batch is recovered by the acknowledgements of the
/// windows behind it rather than by a probe timeout per window. A batch is charged to the shard's read-ahead ledger
/// before it is fetched ([`ReadAheadLedger`], within `DaemonConfig::read_ahead_bytes`, the allowance the fleet's
/// receive share of the reserve holds for it), so the windows every client holds never exceed it; a batch is also
/// capped by what is left of the file and, at send, by what the path moves in one liveness budget.
#[derive(Clone, Debug)]
pub(crate) struct ReadAhead {
  volume: VolumeId,
  path: String,
  at: ReadAt,
  start: u64,
  max: u64,
  stamp: u64,
  total: u64,
  bytes: Vec<u8>,
  /// The bytes one window of this read carries: the read-ahead size this node asks for, until an owner's full
  /// window has answered shorter (its own bound), then that — so the next batch's windows abut.
  window: u64,
  /// The windows the fetch that filled this asked for (the next batch asks twice as many).
  batch: u64,
  /// The bytes charged to the shard's read-ahead ledger for this window ([`ReadAheadLedger`]); credited when it is
  /// replaced, dropped or its client goes ([`release_read_ahead`]).
  charged: u64,
}

/// The bytes a shard's clients hold or are fetching as read-ahead windows ([`ReadAhead`]), charged against
/// `DaemonConfig::read_ahead_bytes`: whole or refused with nothing changed, and never credited past what is held.
#[derive(Debug, Default)]
pub(crate) struct ReadAheadLedger {
  held: u64,
}

impl ReadAheadLedger {
  /// Charges `bytes` if the held windows stay within `bound`.
  fn charge(&mut self, bytes: u64, bound: u64) -> bool {
    let held = self.held.saturating_add(bytes);
    if held > bound {
      return false;
    }
    self.held = held;
    true
  }

  /// The bytes still free under `bound`.
  fn room(&self, bound: u64) -> u64 {
    bound.saturating_sub(self.held)
  }

  /// Returns `bytes` of charge, never more than is held.
  pub(crate) fn credit(&mut self, bytes: u64) {
    self.held = self.held.saturating_sub(bytes);
  }
}

/// Drops `slot`'s read-ahead window, crediting its charge to the shard's ledger.
pub(crate) fn release_read_ahead(ledger: &mut ReadAheadLedger, slot: &mut ClientSlot) {
  if let Some(ahead) = slot.read_ahead.take() {
    ledger.credit(ahead.charged);
  }
}

/// The status count of client pages answered from a read-ahead window. Format: a counter name in the status report.
const READ_AHEAD_HIT: &str = "fleet.read_ahead.hit";

/// The status count of windows joined to a read-ahead window from the rest of its batch (the batch path's
/// non-vacuity counter: zero means every fetch carried one window). Format: a counter name in the status report.
const READ_AHEAD_JOINED: &str = "fleet.read_ahead.joined";

/// A page answered from the client's read-ahead window, if the request is a later page of its read; otherwise
/// `None`, after dropping a window a write by this client to the volume makes stale.
fn read_ahead_before_forward(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  volume: VolumeId,
  body: &RequestBody,
) -> Option<ReplyBody> {
  let Ok(slot) = state.clients.get_mut(client) else {
    return None;
  };
  if is_forwardable_write(body) {
    if slot
      .read_ahead
      .as_ref()
      .is_some_and(|ahead| ahead.volume == volume)
    {
      release_read_ahead(&mut state.read_ahead, slot);
    }
    return None;
  }
  let RequestBody::ReadRange {
    path,
    at,
    offset,
    max,
    ..
  } = body
  else {
    return None;
  };
  let ahead = slot.read_ahead.as_ref()?;
  if *offset == 0 || ahead.volume != volume || ahead.path != *path || ahead.at != *at {
    return None;
  }
  let skip = usize::try_from(offset.checked_sub(ahead.start)?).ok()?;
  let rest = ahead.bytes.get(skip..)?;
  let end_of_file =
    offset.saturating_add(u64::try_from(rest.len()).unwrap_or(u64::MAX)) >= ahead.total;
  if rest.is_empty() && !end_of_file {
    return None;
  }
  let take = usize::try_from((*max).min(crate::merge_service::page_room())).unwrap_or(usize::MAX);
  let page = rest.get(..take.min(rest.len())).unwrap_or(rest).to_vec();
  let reply = ReplyBody::ReadPage {
    bytes: page,
    total: ahead.total,
    stamp: ahead.stamp,
  };
  state.count(READ_AHEAD_HIT, 1);
  Some(reply)
}

/// A forwarded `ReadRange` asked as a batch of windows of the owner (`RequestBody::ReadWindow`, consecutive from
/// its offset), with the plan to keep them; any other verb unchanged, alone. The client's previous window is
/// released first: this read missed it. A read continuing where that window ended asks twice the previous batch,
/// capped by the windows left in the file and by the room left in the shard's read-ahead ledger, which the batch is charged against; with no room a single window is fetched
/// uncharged — what one client could already make the shard hold (half its ring of pages, [`read_window_bytes`]).
fn window_for(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  body: RequestBody,
) -> (Vec<RequestBody>, Option<ReadAhead>) {
  let RequestBody::ReadRange {
    volume,
    path,
    at,
    offset,
    max,
  } = body
  else {
    return (vec![body], None);
  };
  let ShardState {
    clients,
    read_ahead,
    ..
  } = state;
  let previous = clients.get_mut(client).ok().and_then(|slot| {
    let continued = slot.read_ahead.as_ref().and_then(|ahead| {
      let end = ahead
        .start
        .saturating_add(u64::try_from(ahead.bytes.len()).unwrap_or(u64::MAX));
      (ahead.volume == volume && ahead.path == path && ahead.at == at && end == offset).then_some(
        Continued {
          window: ahead.window,
          batch: ahead.batch,
          total: ahead.total,
        },
      )
    });
    release_read_ahead(read_ahead, slot);
    continued
  });
  let (window, wanted) = next_batch(previous, offset, read_window_bytes(state));
  // A batch takes what is left of the shard's read-ahead allowance; the session's flow control paces it through
  // the receive window.
  let bound = state.config.read_ahead_bytes;
  let admitted = state
    .read_ahead
    .room(bound)
    .checked_div(window)
    .unwrap_or(0);
  let batch = wanted.min(admitted);
  let charged = window.saturating_mul(batch);
  let (batch, charged) = if batch > 0 && state.read_ahead.charge(charged, bound) {
    (batch, charged)
  } else {
    (1, 0)
  };
  if let Ok(slot) = state.clients.get_mut(client) {
    slot.read_ahead_pending = slot.read_ahead_pending.saturating_add(charged);
  }
  let bodies = (0..batch)
    .map(|index| RequestBody::ReadWindow {
      volume,
      path: path.clone(),
      at,
      offset: offset.saturating_add(window.saturating_mul(index)),
      max: window,
    })
    .collect();
  let plan = ReadAhead {
    volume,
    path,
    at,
    start: offset,
    max,
    stamp: 0,
    total: 0,
    bytes: Vec::new(),
    window,
    batch,
    charged,
  };
  (bodies, Some(plan))
}

/// The window a read ended its last fetch with, when its next request continues where that window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Continued {
  window: u64,
  batch: u64,
  total: u64,
}

/// The window size and the windows a read's next fetch at `offset` asks for: one window of `own_window` bytes for a
/// read that does not continue a window; for one that does, twice its last batch of that window's size, but no more
/// windows than are left in the file; at least one window. The shard's read-ahead ledger bounds the bytes in flight
/// (the caller), and a batch is given up only when it stalls. No measured rate caps it: every batch is app-limited by
/// construction (it asks for exactly its windows), and an app-limited delivery sample must not lower the estimate
/// it feeds (delivery-rate estimation as BBR uses it, draft-cheng-iccrg-delivery-rate-estimation §3.3). The cap it
/// replaced held each batch to the rate its own small batches measured, a loop that kept a cross-region read at one
/// window per forward: 97-131 s for 8 MiB on the two-network crossing, 28-52 s without it (2026-10-08).
fn next_batch(previous: Option<Continued>, offset: u64, own_window: u64) -> (u64, u64) {
  let Some(continued) = previous else {
    return (own_window.max(1), 1);
  };
  let window = continued.window.max(1);
  let left = continued
    .total
    .saturating_sub(offset)
    .div_ceil(window)
    .max(1);
  (window, continued.batch.saturating_mul(2).min(left))
}

/// Keeps the owner's windows answered for `plan` in the client's slot as one window and returns its first page as
/// the client's reply. The batch's replies are joined in order while each abuts the last and reads the same state
/// of the file (its stamp and length); the rest — a window past a write, unanswered or refused — is dropped, and the
/// read fetches from there when it gets there. A first reply that is not a page (a refusal) passes through, and the
/// slot keeps no window. Whatever of the charge the kept bytes do not use is credited back. Returns the reply and
/// the windows joined after the first.
fn keep_window(
  ledger: &mut ReadAheadLedger,
  slot: &mut ClientSlot,
  plan: ReadAhead,
  OwnerReplies {
    first,
    later: replies,
  }: OwnerReplies,
) -> (ReplyBody, u64) {
  slot.read_ahead_pending = slot.read_ahead_pending.saturating_sub(plan.charged);
  let ReplyBody::ReadPage {
    mut bytes,
    total,
    stamp,
  } = first
  else {
    ledger.credit(plan.charged);
    release_read_ahead(ledger, slot);
    return (first, 0);
  };
  let first_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  // A first window shorter than asked that does not end the file is the owner's own bound: the next batch's
  // windows are that long, so they abut.
  let window = if plan.start.saturating_add(first_len) < total {
    first_len.min(plan.window).max(1)
  } else {
    plan.window
  };
  let joined = join_batch(&mut bytes, plan.window, (stamp, total), replies);
  let kept = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  ledger.credit(plan.charged.saturating_sub(kept));
  let take = usize::try_from(plan.max.min(crate::merge_service::page_room())).unwrap_or(usize::MAX);
  let page = bytes
    .get(..take.min(bytes.len()))
    .unwrap_or(&bytes)
    .to_vec();
  release_read_ahead(ledger, slot);
  slot.read_ahead = Some(ReadAhead {
    stamp,
    total,
    bytes,
    window,
    charged: plan.charged.min(kept),
    ..plan
  });
  (
    ReplyBody::ReadPage {
      bytes: page,
      total,
      stamp,
    },
    joined,
  )
}

/// Joins to `bytes` — a batch's first window, asked as `window` bytes — the batch's later windows in order, while
/// each abuts the last (every window before it came back whole, `window` bytes) and reads the same state of the file
/// (`stamp`, `total`); stops at the first that does not, or that is not a page. Returns the windows joined.
fn join_batch(
  bytes: &mut Vec<u8>,
  window: u64,
  (stamp, total): (u64, u64),
  replies: Vec<ReplyBody>,
) -> u64 {
  let whole = |bytes: &[u8]| u64::try_from(bytes.len()).ok() == Some(window);
  let mut abuts = whole(bytes);
  let mut joined: u64 = 0;
  for reply in replies {
    let ReplyBody::ReadPage {
      bytes: more,
      total: more_total,
      stamp: more_stamp,
    } = reply
    else {
      break;
    };
    if !abuts || more_stamp != stamp || more_total != total {
      break;
    }
    abuts = whole(&more);
    bytes.extend_from_slice(&more);
    joined = joined.saturating_add(1);
  }
  joined
}

/// The bytes of `body` forwarded over the fleet under this client's request id, with the completion watermark
/// its client acknowledged (every partition holds it: acknowledgements are scattered to every shard).
pub(crate) fn forwarded_request(
  state: &ShardState,
  request: u64,
  principal: Principal,
  body: RequestBody,
) -> Vec<u8> {
  let client = RequestId::from_word(request).client;
  let partition = state.db.partition();
  let ack_up_to = partition.acknowledged_up_to(state.origin_anchor.0, client);
  let ack_owed = partition.owed_completions(state.origin_anchor.0, client);
  encode_body(&ForwardedRequest {
    principal,
    body,
    request,
    ack_up_to,
    ack_owed,
  })
}

/// On the control shard, where the fleet's sessions live: finds `volume`'s owner in `region` (the cached route,
/// the live creator, else the read-only location round) and forwards `requests` to it once — one verb, or a
/// read-ahead batch of windows, sent together. Returns the owner's reply to the first, or
/// `OwnerUnreachable` when no owner was found or none answered within the takeover bound, with the decoded replies to the rest that came back
/// (in order, up to the first that did not), and the route to cache when the owner served.
pub(crate) async fn resolve_and_forward(
  volume: VolumeId,
  region: u64,
  cached: Option<crate::owner_location::CachedRoute>,
  requests: Vec<Vec<u8>>,
) -> (OwnerReplies, Option<crate::owner_location::CachedRoute>) {
  if let Some(refusal) =
    crate::state::with_state(|state| owned_unmaterialized(state, volume)).flatten()
  {
    await_materialization(ObjectId(volume.bytes)).await;
    return (
      OwnerReplies {
        first: refused(refusal),
        later: Vec::new(),
      },
      None,
    );
  }
  let resolved = crate::state::with_state_counted(|state| {
    let query = crate::owner_location::Query::new(state, ObjectId(volume.bytes), region);
    if let Some(dropped) = cached.and_then(|route| query.reuses(state, route)) {
      state.count(dropped, 1);
    }
    (query, query.known_owner(state, cached))
  });
  let Some((query, known)) = resolved else {
    // The control shard's state was out of reach (a nested borrow is counted): the client is answered as for an
    // owner not found, never left waiting for a reply no task would send (before 2026-10-07 the task returned
    // and the client waited out its reply deadline).
    return (
      OwnerReplies {
        first: refused(Refusal::HomedElsewhere { region }),
        later: Vec::new(),
      },
      None,
    );
  };
  // A forward the owner was sent but did not answer within its budget, or one that could not be sent because the
  // session to the owner stayed busy, is sent again to the same owner while the
  // detector has not confirmed that owner dead and the takeover bound (the operator's failover SLO) allows. On a lossy path
  // the reply is most often still being recovered by the transport. A dead owner is declared within the membership
  // horizon, which ends the resends, and the client is refused as before, so a takeover's window is unchanged. Nothing
  // is re-located here: a successor that has not yet adopted the volume would answer `NotFound`, which tells a client
  // the volume does not exist. A resend is safe: a read is idempotent, and a forwarded write carries its request id,
  // which the owner's completion record answers exactly once (§4.9 RIFL); the client waits for a forwarded verb while
  // the daemon lives (`RequestBody::defers_reply`). Until 2026-10-09 one unanswered forward refused the client at once:
  // one cross-region read in nine was refused mid-file on a 200 ms path with 3% loss.
  // The failover bound, from the first send: the SLO, or the floor of the path to the owner where that is longer (A-352).
  let mut bound = crate::forward_lane::FailoverBound::from_now();
  let owner = match known {
    Some(owner) => {
      crate::state::with_state(|state| {
        state.count("fleet.owner_location.direct", 1);
      });
      Some(owner)
    }
    None => match locate_within_budget(query).await {
      Some(owner) => Some(owner),
      None => match locate_work_green(volume, region).await {
        WorkGreen::Remote(owner) => Some(owner),
        WorkGreen::Here => {
          // The work's green is this node's own, and the request missed its completion window on the way here (a
          // local request is answered from it first): no completion is recorded under its key, and the work is
          // gone with its owner, so nothing runs (§4.16's failure matrix: the work's loss window).
          crate::state::with_state(|state| state.count(WORK_GONE_HERE, 1));
          return (
            OwnerReplies {
              first: refused(Refusal::NotFound),
              later: Vec::new(),
            },
            None,
          );
        }
        WorkGreen::Unknown => None,
      },
    },
  };
  // Every resend keeps the place the first send had in the owner's lane (A-306), so forwards first queued after it
  // never go ahead of it however often it is sent again. Every send is judged against the same failover bound, from
  // the first: the lane's progress holds a forward still queued only until then (A-315), and the bound is held to the
  // longest floor the path to the owner has measured, so no resend is refused below it (A-352).
  let mut place = crate::forward_lane::Place::default();
  let (decoded, later) = loop {
    let (decoded, later) = forward_once(owner, &requests, &mut bound, &mut place).await;
    // Unanswered (sent, no reply in time) or unsent (the session to the owner stayed with another task the whole
    // budget): on a lossy path both are transient while the owner lives (measured 2026-10-09 at 10% loss: contested
    // reads refused after forwards that never left, `fleet.owner_location.forward_unsent`).
    let unanswered = matches!(decoded, Some(None)) || (decoded.is_none() && owner.is_some());
    // Alive or only suspected: a suspect is still a member until it is confirmed dead or refutes the suspicion, and
    // on a lossy path suspicion is common (the 2026-10-09 run that held to `alive()` alone still refused one read in
    // nine). A confirmed death ends the resends.
    let alive = owner.is_some_and(|owner| {
      crate::state::with_state(|state| {
        state
          .fleet
          .membership()
          .state(owner)
          .is_some_and(|member| member.liveness != slates_cluster::membership::Liveness::Dead)
      })
      .unwrap_or(false)
    });
    if !unanswered || !alive || slates_rt::futures::now_ns() >= bound.at_ns() {
      break (decoded, later);
    }
    crate::state::with_state_counted(|state| state.count(FORWARD_RETRIED, 1));
  };
  let reply = decoded
    .flatten()
    .unwrap_or_else(|| refused(Refusal::OwnerUnreachable { region }));
  let route = route_after(&reply, owner, query);
  (
    OwnerReplies {
      first: reply,
      later,
    },
    route,
  )
}

/// Counter: forwards sent again to an owner the detector has not confirmed dead because it did not answer the last within its
/// budget (`resolve_and_forward`).
const FORWARD_RETRIED: &str = "fleet.forward.retried";

/// One forward of `requests` to `owner`, within one liveness budget plus the measured round-trip tail of the path to
/// it, queued at `place` in the owner's lane (the first send's, for a resend) and judged against `bound_at`, the
/// failover bound on the wall from the first send (`crate::forward_lane`'s stall, A-315): the decoded reply to the
/// first (`None` when nothing was sent, `Some(None)` when it went unanswered) and the decoded replies to the rest that
/// came back, in order, up to the first that did not.
async fn forward_once(
  owner: Option<HostId>,
  requests: &[Vec<u8>],
  bound: &mut crate::forward_lane::FailoverBound,
  place: &mut crate::forward_lane::Place,
) -> (Option<Option<ReplyBody>>, Vec<ReplyBody>) {
  let Some(owner) = owner else {
    return (None, Vec::new());
  };
  // One liveness budget, plus the measured round-trip tail of the path to the owner: a WAN forward's reply is never
  // refused for crossing a path the local budget did not allow for.
  let tail_ns = crate::state::with_state(|state| {
    state
      .peer_paths
      .get(&owner)
      .and_then(slates_cluster::timing::PathRtt::tail_ns)
  })
  .flatten()
  .unwrap_or(0);
  let forwarded = crate::fleet::forward_batch_over_leader_session(
    owner,
    requests,
    crate::daemon::LIVENESS_BUDGET_NS.saturating_add(tail_ns),
    bound,
    place,
  )
  .await;
  if forwarded.is_none() {
    crate::state::with_state(|state| {
      state.count("fleet.owner_location.forward_unsent", 1);
    });
  }
  // A forward sent but unanswered within its budget comes back empty, and it is counted apart from one never sent: an
  // owner that was reached but did not answer in time is not one that could not be reached.
  let batch = forwarded.unwrap_or_default();
  let mut replies = batch.replies.into_iter();
  let decoded = replies
    .next()
    .map(|bytes| bytes.and_then(|bytes| decode_body::<ReplyBody>(&bytes).ok()));
  let later: Vec<ReplyBody> = replies
    .map_while(|bytes| bytes.and_then(|bytes| decode_body::<ReplyBody>(&bytes).ok()))
    .collect();
  if matches!(decoded, Some(None)) {
    crate::state::with_state_counted(|state| {
      state.count("fleet.owner_location.forward_unanswered", 1);
    });
  }
  (decoded, later)
}

/// The route a client keeps after its forward's `reply` from `owner`: the owner it reached, unless the reply refused.
/// The owner's refusal drops the route, so the client's next request looks the owner up again; that drop is counted
/// (`fleet.owner_location.route_dropped.refused`), so a retry that ran a round after a served request names the
/// refusal that sent it there.
fn route_after(
  reply: &ReplyBody,
  owner: Option<HostId>,
  query: crate::owner_location::Query,
) -> Option<crate::owner_location::CachedRoute> {
  if !matches!(reply, ReplyBody::Refused { .. }) {
    return owner.map(|owner| query.route(owner));
  }
  if owner.is_some() {
    crate::state::with_state(|state| {
      state.count("fleet.owner_location.route_dropped.refused", 1);
    });
  }
  None
}

/// Holds a verb on a volume this node owns but has not materialized while its materialization is pending, polled at
/// the heartbeat, for at most one liveness budget. The client waits for such a verb while its daemon lives
/// (`defers_reply`), and the refusal that follows meets the materialized volume on the client's retry. Answered at
/// once before 2026-10-07: a client that retried without a pause was refused 2.8 million times in 400 s while the
/// successor restored a volume. Ends early once the materialization is no longer pending, done or dropped.
async fn await_materialization(object: ObjectId) {
  let deadline = slates_rt::futures::now_ns().saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
  while crate::state::with_state(|state| state.pending_materializations.contains_key(&object))
    .unwrap_or(false)
    && slates_rt::futures::now_ns() < deadline
  {
    if slates_rt::futures::sleep(crate::daemon::HEARTBEAT_NS)
      .await
      .is_err()
    {
      return;
    }
  }
}

/// The status count of a request for a work no member owns, sent to its green's owner by the work's catalog record this
/// node holds (A-255).
const ROUTED_BY_WORK_GREEN: &str = "fleet.owner_location.routed_by_work_green";

/// The owner of the green that `volume` is a work of, for a request no member owns the work for (A-255): after its
/// owner's takeover a work has no owner (its head is never placed, so a successor never takes it over), but every holder
/// of its catalog record knows its green (`CatalogValue::work_of`), whose owner holds the work's submits' completions
/// (their merge records). This node's own held copy of that record is a hint and grants nothing (§4.8 "Lookup"): the
/// green's owner answers only a completion recorded under the request's own key, and refuses anything else typed
/// without running it. `None` when this node holds no catalog record naming a green for `volume`, or the green's owner is
/// not found within the location budget.
async fn locate_work_green(volume: VolumeId, region: u64) -> WorkGreen {
  // This node's own held copy of the work's catalog record first; else one bounded location round asks the peers that
  // hold one (`owner_location::locate_work_of`, the round's own bound and counters).
  let held = crate::state::with_state(|state| {
    crate::owner_location::held_work_of(state, ObjectId(volume.bytes))
  })
  .flatten();
  let green = match held {
    Some(green) => green,
    None => {
      let Some(query) = crate::state::with_state(|state| {
        crate::owner_location::Query::new(state, ObjectId(volume.bytes), region)
      }) else {
        return WorkGreen::Unknown;
      };
      match crate::owner_location::locate_work_of(query).await {
        Some(green) => green,
        None => return WorkGreen::Unknown,
      }
    }
  };
  let green = ObjectId(green);
  let found = crate::state::with_state(|state| {
    let here = state.fleet.object_owner(green) == Some(state.fleet.host());
    (
      here,
      crate::owner_location::Query::new(state, green, region),
    )
  });
  let Some((here, query)) = found else {
    return WorkGreen::Unknown;
  };
  if here {
    return WorkGreen::Here;
  }
  match locate_within_budget(query).await {
    Some(owner) => {
      crate::state::with_state(|state| state.count(ROUTED_BY_WORK_GREEN, 1));
      WorkGreen::Remote(owner)
    }
    None => WorkGreen::Unknown,
  }
}

/// Where the green of a work no member owns is ([`locate_work_green`]).
enum WorkGreen {
  /// Owned by this node.
  Here,
  /// Owned by this member.
  Remote(HostId),
  /// Not known: this node holds no catalog record naming one, or its owner was not found within the budget.
  Unknown,
}

/// The status count of a request for a work no member owns, whose green this node owns and whose completion it does not
/// hold: refused `NotFound`, run nowhere (A-255).
const WORK_GONE_HERE: &str = "fleet.owner_location.work_gone_here";

/// The owner of `query`'s object, resolved again for as long as the location budget allows: the direct route (the
/// volume's creator while eligible) and then a location round, asked again every poll interval. The requests are not
/// sent until an owner is found, so a retry can never apply a verb twice. The owner can be briefly unfindable while it
/// is live: the failure detector condemns a live member within its stated mistake allowance, the death is gossiped,
/// and every node drops its routes and its link to that member until it refutes and is linked again (2026-10-08: a
/// foreign node held a live successor dead that another node had condemned, 1 against an allowance of 4.9, and its
/// client was refused `HomedElsewhere`). A takeover's adoption leaves the same window. Measured
/// (`a_refuted_false_death_keeps_a_forward_out_for_less_than_its_location_budget`, 10 false deaths per arm): without the
/// retry every false death refused a call, and the next call 10 ms later was served after 22–217 ms; with location
/// rounds alone, paced a heartbeat, 1 s and still refused; with the direct route first, paced a heartbeat, none refused
/// and 111–224 ms; paced a tenth of a heartbeat, none refused and 21–218 ms. Each retry is counted
/// (`fleet.owner_location.retried`); `None` once the budget is spent with no owner found, counted by how it ended
/// ([`LOCATION_SPENT`], [`LOCATION_SLO_ENDED`]) and in all ([`LOCATION_GAVE_UP`]).
///
/// The budget (A-317) is the membership horizon (`lease::horizon_ns`: the longest a live member can go unanswered
/// before a detector declares it dead, the window in which a false death is gossiped and refuted; the supervisor's
/// derivation of 2026-11-07), on the wall from the first look, and it ends only with a look begun after the horizon,
/// which asks the direct route again once its round returns: a round waits on its peers while the shard runs its other
/// work, so a refutation the shard read meanwhile is seen before the location gives up. Before, one liveness budget of
/// wall time ran out on a control shard kept off its CPU while that gossip waited to be read (clients' runs, nice 19
/// beside two spinners a CPU: 1 or 2 calls refused in every false death), and a single round waiting on silent peers
/// spent it whole, so the location never looked again. Counted in polls (clients' `3c072f490`), a location whose
/// rounds each wait a liveness budget on a silent peer ran to the failover SLO, ten times main's second; ended by the
/// control shard's driver waits (A-292's rule), a busy shard, which rarely waits, held it to the SLO too (A-315 found
/// the same on the forward lane). The operator's failover SLO caps the whole on the wall.
async fn locate_within_budget(query: crate::owner_location::Query) -> Option<HostId> {
  let located = locate_within_horizon(query).await;
  if located.is_none() {
    // Counted here, so any way the location gives up is seen; its budget's own two ends are counted within it.
    crate::state::with_state(|state| state.count(LOCATION_GAVE_UP, 1));
  }
  located
}

/// [`locate_within_budget`]'s loop: a look every poll interval (the direct route, a round, and the direct route again,
/// with what the shard read while the round waited), until an owner is found, a look begun after the membership
/// horizon has found none, or the failover SLO has passed.
async fn locate_within_horizon(query: crate::owner_location::Query) -> Option<HostId> {
  let pace = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;
  let began = slates_rt::futures::now_ns();
  let horizon = crate::lease::horizon_ns();
  let ceiling = crate::state::with_state(|state| state.failover_ns).unwrap_or(horizon);
  let known = || crate::state::with_state(|state| query.known_owner(state, None)).flatten();
  loop {
    let look_began = slates_rt::futures::now_ns().saturating_sub(began);
    // The direct route first (the volume's creator, while eligible), as a fresh call would take it.
    if let Some(owner) = known() {
      return Some(owner);
    }
    if let Ok(owner) = crate::owner_location::locate(query).await {
      return Some(owner);
    }
    // The round waited on its peers while the shard ran its other work: a refutation read meanwhile is seen now.
    if let Some(owner) = known() {
      return Some(owner);
    }
    if slates_rt::futures::now_ns().saturating_sub(began) >= ceiling {
      crate::state::with_state(|state| state.count(LOCATION_SLO_ENDED, 1));
      return None;
    }
    if look_began >= horizon {
      crate::state::with_state(|state| state.count(LOCATION_SPENT, 1));
      return None;
    }
    if slates_rt::futures::sleep(pace).await.is_err() {
      return None;
    }
    crate::state::with_state(|state| state.count("fleet.owner_location.retried", 1));
  }
}

/// Counter: a forward's owner location ended by the operator's failover SLO, the owner still not found.
const LOCATION_SLO_ENDED: &str = "fleet.owner_location.slo_ended";
/// Counter: a forward's owner location spent its budget, a look begun past the membership horizon having found no
/// owner.
const LOCATION_SPENT: &str = "fleet.owner_location.budget_spent";
/// Counter: a forward's owner location gave up with no owner found, for any reason: at most the budget's two ends,
/// [`LOCATION_SLO_ENDED`] and [`LOCATION_SPENT`], when nothing gives up early (the shard stopping).
const LOCATION_GAVE_UP: &str = "fleet.owner_location.gave_up";

/// The refusal for a verb on a volume this node owns but has not materialized yet, asked on the control shard where
/// the routing lives: the routing names this node the owner (it adopted the head in a takeover), yet the verb is being
/// forwarded, so the owner shard's catalog does not hold the volume. A location round can never answer that, since it
/// asks the peers and never this node, and it refused `HomedElsewhere`, which names another region and is false
/// (three fleet tests, 2026-10-07: a successor's own client refused so for a whole wait). The truthful answer is the
/// owner shard's retryable `Overloaded`, the refusal a forwarded write already gives while its acceptance still waits
/// on its owner ([`await_deferred_completion`]), counted (`fleet.owner_location.owned_unmaterialized`) beside the
/// materialization's own refusals (`fleet.materialize.*`). `None` when another host owns the volume, or none is known.
pub(crate) fn owned_unmaterialized(state: &mut ShardState, volume: VolumeId) -> Option<Refusal> {
  let local = state.fleet.host();
  if state.fleet.object_owner(ObjectId(volume.bytes)) != Some(local) {
    return None;
  }
  let shard = state
    .shards
    .get(usize::from(owner_of(volume)))
    .copied()
    .unwrap_or(state.shard);
  state.count("fleet.owner_location.owned_unmaterialized", 1);
  Some(Refusal::Overloaded { shard })
}

/// What an owner answered a forward: the reply to its first request, and the decoded replies to the rest of a batch
/// (in order, up to the first that did not come back).
pub(crate) struct OwnerReplies {
  first: ReplyBody,
  later: Vec<ReplyBody>,
}

impl OwnerReplies {
  /// The owner's reply to the first request.
  pub(crate) fn first(&self) -> &ReplyBody {
    &self.first
  }
}

/// The region and volume of a verb this node must send to another host of its own region (§4.8 "Lookup": a lookup
/// by id routes to the volume's creator, or its successor after a takeover): a volume-scoped read or write that
/// forwards safely, whose volume this node lost to a successor ([`lost_to_a_successor`]), or whose volume this shard's
/// catalog does not hold and whose id names another creator. Asked on the volume's owner shard — the one shard whose
/// catalog would hold it — and only for a local client's verb, never one forwarded from another node, so a forward is
/// one hop. A volume this node created and holds no record of stays `NotFound`. A laptop never forwards: every volume it
/// can name, it created.
fn owned_by_another_host(state: &ShardState, body: &RequestBody) -> Option<(u64, VolumeId)> {
  if !(is_forwardable_read(body) || is_forwardable_write(body)) {
    return None;
  }
  let volume = volume_of(body)?;
  // A volume this shard records unrecovered is answered here, its refusal naming why: a boot recovery that could not
  // rebuild it, or a takeover that gave it up (A-304, and the materialization's bounds). Forwarded, it met this node's
  // own routing, which names this node its owner, and answered a retryable `Overloaded` for good on any fleet of more
  // than one node (a lone node never forwards, which is where the give-ups were first proven).
  if state.unrecovered.contains_key(&to_db_volume(volume)) {
    return None;
  }
  let host = state.fleet.host();
  let creator = ObjectId(volume.bytes).creator();
  // A volume this node lost to a successor is the successor's, whoever created it and whatever this catalog still holds
  // of it: its verbs go there, as any other node's do (`lost_to_a_successor`: A-338's fenced log, A-334's yield).
  if !lost_to_a_successor(state, to_db_volume(volume))
    && (creator == host || state.db.partition().volume(to_db_volume(volume)).is_some())
  {
    return None;
  }
  // Only another member can own it. A node alone (a laptop: the fleet of one) answers the verb here, `NotFound` for an
  // id it never created; before 2026-10-10 it went looking for another host, found none, and answered
  // `OwnerUnreachable` (`a_single_node_answers_an_id_it_never_created_not_found`).
  if !state.fleet.members().iter().any(|member| *member != host) {
    return None;
  }
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .copied()
    .unwrap_or(RegionId(0));
  Some((region.0, volume))
}

/// Whether this node lost volume `id` to a successor, so its verbs go to the successor and its copy here is never served
/// as the volume (A-334, A-338): its live log was fenced at a later epoch by a holder that took a successor's takeover
/// (`crate::live_ship::superseded`), or this shard yielded it when its host was admitted again under its id
/// (`ShardState::yielded`, `crate::readmission`).
pub(crate) fn lost_to_a_successor(state: &ShardState, id: DbVolumeId) -> bool {
  state.yielded.contains(&id) || crate::live_ship::superseded(state, id)
}

/// A late lookup belongs to the original client generation. It cannot populate a reused slot's
/// cache or deliver a predecessor's result to the client that now occupies that slot.
/// Carries a forwarded verb's reply from the control shard to the client's shard `origin` and finishes it there
/// ([`finish_owner_forward`]): at once on the same shard, else as a task sent home ([`crate::xshard::send_back`]).
fn deliver_owner_forward(
  origin: u16,
  control: u16,
  (client, request): (Handle<ClientSlot>, u64),
  reply: OwnerReplies,
  after: (
    Option<crate::owner_location::CachedRoute>,
    Option<ReadAhead>,
  ),
) {
  if origin == control {
    finish_owner_forward(client, request, reply, after);
    return;
  }
  crate::xshard::send_reply_home(origin, move || {
    finish_owner_forward(client, request, reply, after);
  });
}

fn finish_owner_forward(
  client: Handle<ClientSlot>,
  request: u64,
  reply: OwnerReplies,
  (route, plan): (
    Option<crate::owner_location::CachedRoute>,
    Option<ReadAhead>,
  ),
) {
  let delivered = crate::state::with_state(move |state| {
    state.count(FORWARD_CLIENT_DELIVERED_HOME, 1);
    let ShardState {
      clients,
      read_ahead,
      ..
    } = &mut *state;
    let Ok(slot) = clients.get_mut(client) else {
      // The client went: its reap credited the batch's charge with it.
      state.forwarded_rings.remove(&request);
      state.count("fleet.owner_location.client_gone", 1);
      return None;
    };
    slot.owner_route = route;
    // A window answered for a read: kept for the client's next pages, and its first page is the reply.
    Some(match plan {
      Some(plan) => {
        let (reply, joined) = keep_window(read_ahead, slot, plan, reply);
        state.count(READ_AHEAD_JOINED, joined);
        reply
      }
      None => reply.first,
    })
  })
  .flatten();
  if let Some(reply) = delivered {
    // The executing owner owns the completion. In particular, a transient routing refusal
    // must not become a permanent completion at the origin and prevent this id's retry.
    crate::state::deliver(client.index(), request, reply, true);
  }
}

/// Proposes a lost region's promotion to its declared mirror on this shard's root group (§4.8, D-14). Refuses
/// `Unsupported` when the region has no declared mirror (the operator declares mirrors in the manifest), and
/// `NotRootLeader` when this node's root group is not the leader, so cannot propose. Idempotent: a promotion
/// re-proposed after it has committed is a no-op (`RootConfiguration::home_of` follows the recorded promotion).
fn propose_region_promotion(state: &mut ShardState, region: u64) -> ReplyBody {
  let region = slates_db::register::RegionId(region);
  let Some(&mirror) = state.region_mirrors.get(&region) else {
    return refused(Refusal::Unsupported {
      feature: "region-loss promotion (the region has no declared mirror)".to_owned(),
    });
  };
  if state
    .root
    .propose(slates_cluster::root_group::RootCommand::PromoteRegion {
      lost: region,
      mirror,
    })
  {
    ReplyBody::Acknowledged
  } else {
    refused(Refusal::NotRootLeader)
  }
}

/// Sends authenticated human consensus operations to the control shard (§4.8).
fn serve_consensus_control(
  state: &mut ShardState,
  client: u32,
  request: u64,
  principal: Principal,
  body: RequestBody,
) -> Served {
  if !matches!(principal, Principal::Uid { .. } | Principal::Sid { .. }) {
    let verb = if matches!(body, RequestBody::Bootstrap { .. }) {
      "bootstrap"
    } else {
      "consensus recovery"
    };
    return Served::Reply(forbidden(verb));
  }
  consensus_on_control(state, client, request, move |state| match body {
    RequestBody::Bootstrap { root, member } => crate::consensus::bootstrap(state, root, member),
    RequestBody::RecoveryPlan { root, target } => {
      match crate::consensus_recovery::plan(state, root, target) {
        Ok(plan) => ReplyBody::RecoveryPlan { plan },
        Err(refusal) => refused(refusal),
      }
    }
    RequestBody::Recover {
      root,
      target,
      plan,
      proof,
    } => crate::consensus_recovery::recover(state, root, target, plan, proof),
    _ => refused(Refusal::ConsensusRecoveryUnavailable),
  })
}

/// The verbs a client's channel answers itself, before any routing: its status, a consumer's attestation and revocation
/// (§4.13), and a consumer's own write policy, set or read (A-285). `Break` with what was served; `Continue` with the
/// request's body when it is none of them, for [`serve`] to route.
fn serve_on_channel(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  request: &Request,
  (client_id, principal): (u32, &Principal),
  body: RequestBody,
) -> std::ops::ControlFlow<Served, RequestBody> {
  use std::ops::ControlFlow::{Break, Continue};
  if let Some(reply) = status_on_channel(state, client, request.request, &body) {
    return Break(reply);
  }
  if let RequestBody::Attest { consumer, proof } = body {
    return Break(attest_on_channel(
      state,
      client.index(),
      request.request,
      client_id,
      consumer,
      proof,
    ));
  }
  if let RequestBody::Revoke { consumer, proof } = body {
    return Break(revoke_on_channel(
      state,
      (client.index(), request.request),
      principal,
      consumer,
      proof,
    ));
  }
  if let RequestBody::SetPolicy {
    subject: slates_ipc::protocol::PolicySubject::Consumer { consumer },
    policy,
    version,
    proof,
  } = body
  {
    return Break(set_consumer_policy_on_channel(
      state,
      (client.index(), request.request),
      principal,
      (consumer, policy, version),
      proof,
    ));
  }
  if let RequestBody::Policy {
    subject: slates_ipc::protocol::PolicySubject::Consumer { consumer },
  } = body
  {
    return Break(consumer_policy_on_channel(
      state,
      (client.index(), request.request),
      consumer,
    ));
  }
  Continue(body)
}

/// Serves a client request: recover a previous completion or dispatch the operation to its owner
/// shard. Scatter operations record their completion after every shard has answered.
pub fn serve(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  request: &Request,
  now: u64,
) -> Served {
  let (client_id, principal) = match state.clients.get(client) {
    Ok(c) => (c.client_id, c.principal.clone()),
    Err(_) => return Served::Reply(refused(Refusal::NotFound)),
  };
  let id = RequestId::from_word(request.request);
  // A local client's completion key is this node's **stable cert-anchor** (the globally-unique key's high
  // half) — not the ephemeral member id, so a retry meets its completion record across a daemon restart (the
  // member id changes per boot; the anchor does not — task #22). A forwarded verb keys on its authenticated
  // origin's anchor instead (see `record_completion`, `serve_forward`).
  let origin = state.origin_anchor.0;
  // Where this request's reply is written — kept for a verb that must answer later (AUD-11).
  state.reply_route = Some(crate::merge_service::ReplyRoute {
    shard: state.shard,
    client_index: client.index(),
    request: request.request,
    through_work: false,
  });
  match state
    .db
    .partition()
    .completion(origin, id.client, id.sequence)
  {
    Seen::Completed(bytes) => {
      return Served::Reply(
        ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest)),
      );
    }
    Seen::Acknowledged => return Served::Reply(refused(Refusal::DuplicateRequest)),
    // It ran and waits for its version's commit (A-252): answered, or joined to the wait, never run again.
    Seen::Awaiting { object, version } => {
      return crate::merge_service::serve_awaiting(state, origin, id, (object, version))
        .map_or(Served::Forwarded, Served::Reply);
    }
    Seen::New => {
      // A retry of a submit whose acceptance still waits for its commit joins the wait: it is answered
      // with the committed result when the version places, never a success from memory. A retry of a
      // granted landing still running joins it the same way (AUD-29-03).
      if crate::merge_service::join_awaiting(state, origin, id)
        || crate::live_ship::join(state, origin, id)
        || crate::landing::join_in_flight(state, origin, id)
        || crate::image::join_in_flight(state, origin, id)
        || crate::direct_map::join_in_flight(state, origin, id)
        || crate::lease_wait::join(state, origin, id)
        || crate::work_home::join(state, origin, id)
      {
        return Served::Forwarded;
      }
    }
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
          origin,
          id,
          refused(Refusal::BadRequest {
            reason: reason.to_owned(),
          }),
        ));
      }
      Err(e) => {
        return Served::Reply(record_completion(
          state,
          origin,
          id,
          refused(Refusal::BadRequest {
            reason: e.to_string(),
          }),
        ));
      }
    }
  };
  judge_loop_request(state, &body, now);
  // A channel bound to a consumer the human has since revoked refuses every effect (§4.13), before any
  // lookup or mutation, whatever the verb — one local read (the revocation was fanned to this slot).
  if state.clients.get(client).is_ok_and(|slot| slot.revoked) {
    state.count("consumer_revoked", 1);
    return Served::Reply(record_completion(
      state,
      origin,
      id,
      refused(Refusal::ConsumerRevoked),
    ));
  }
  let body = match serve_on_channel(state, client, request, (client_id, &principal), body) {
    std::ops::ControlFlow::Continue(body) => body,
    std::ops::ControlFlow::Break(served) => return served,
  };
  // A consumer's own write policy (A-285) holds its merges and landings here, before they are routed to the volume's
  // owner, which then holds them to the volume's: the stricter of the two governs.
  let body = match consumer_gate(state, client, body) {
    Ok(body) => body,
    Err(refusal) => return Served::Reply(record_completion(state, origin, id, refused(refusal))),
  };
  if let RequestBody::List { after } = body {
    return scatter_list(state, client.index(), request.request, principal, after);
  }
  if let RequestBody::Grants { after } = body {
    return scatter_grants(state, client.index(), request.request, principal, after);
  }
  if matches!(
    &body,
    RequestBody::Bootstrap { .. } | RequestBody::RecoveryPlan { .. } | RequestBody::Recover { .. }
  ) {
    return serve_consensus_control(state, client.index(), request.request, principal, body);
  }
  if let RequestBody::PromoteRegion { region } = body {
    return promote_region_on_root(
      state,
      client.index(),
      request.request,
      region,
      principal.clone(),
    );
  }
  if let RequestBody::Acknowledge { up_to, owed } = body {
    return scatter_acknowledge(
      state,
      client.index(),
      request.request,
      client_id,
      (up_to, owed),
    );
  }
  // An attach of another host's volume is remembered with its client here, on the client's own shard, before it is
  // sent anywhere, so the client's reap reaches the owner (A-222).
  if let Some(refusal) = crate::remote_attach::foreign_attach(state, &body) {
    return Served::Reply(record_completion(state, origin, id, refused(refusal)));
  }
  if let Err(refusal) = crate::remote_attach::remember(state, client, &body) {
    return Served::Reply(record_completion(state, origin, id, refused(refusal)));
  }
  // A foreign-home verb resolves its owner from the creator, a cached successful route or a
  // read-only exchange with actual home-region holders. The write itself is forwarded once,
  // carrying the original completion key. A single-region request keeps the local fast path.
  if let Some(volume) = volume_of(&body)
    && let Some(region) = homed_elsewhere(state, volume)
  {
    if is_forwardable_read(&body) || is_forwardable_write(&body) {
      return forward_to_owner(
        state,
        client,
        request.request,
        principal,
        region,
        volume,
        body,
      );
    }
    return Served::Reply(record_completion(
      state,
      origin,
      id,
      refused(Refusal::HomedElsewhere { region }),
    ));
  }
  let owner = match &body {
    RequestBody::Create { name, .. } | RequestBody::CreateWork { name, .. } => {
      Some(owner_of_name(name, state.shards.len()))
    }
    RequestBody::Detach { attachment }
    | RequestBody::Advance { attachment, .. }
    | RequestBody::BindMount { attachment, .. }
    | RequestBody::Renew { attachment } => Some(owner_of_attachment(*attachment)),
    // A grant is served where its landing was presented: the volume's owner shard, which the landing id
    // names — not the client's shard, where nothing is awaiting. So is a presentation's entry list.
    RequestBody::Grant { landing, .. } | RequestBody::LandingEntries { landing, .. } => {
      Some(owner_of_landing(*landing))
    }
    // A telemetry drain names its shard: it runs there, on that shard's own ring (§4.14).
    RequestBody::Telemetry { partition } => Some(*partition),
    // The audit is one partition's, the control shard's (§4.15, A-265).
    RequestBody::Audit { .. } => Some(crate::landing::AUDIT_PARTITION),
    other => volume_of(other).map(owner_of),
  };
  if let Some(owner) = owner
    && owner != state.partition
  {
    let Some(shard) = shard_of_partition(state, owner) else {
      return Served::Reply(record_completion(
        state,
        origin,
        id,
        refused(Refusal::NotFound),
      ));
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
  serve_here(
    state,
    client,
    request.request,
    (origin, id, client_id),
    principal,
    body,
  )
}

/// Format: the client id a verb forwarded from another node is dispatched under (`serve_forward`): no client of this
/// node.
pub(crate) const FORWARDED_CLIENT: u32 = 0;

/// The most bytes one read-ahead window carries (`RequestBody::ReadWindow`): as many pages as a client may hold in
/// flight on its ring, half its slots (the half `slates_client` acknowledges at), so a window replaces the round
/// trips the client's own requests would have taken, and the origin's cache of it stays within what one client could
/// already make it hold.
pub(crate) fn read_window_bytes(state: &ShardState) -> u64 {
  crate::merge_service::page_room()
    .saturating_mul((u64::from(state.config.region.slots) / 2).max(1))
}

/// The completion records one client may hold unacknowledged on a shard (§4.9 "Exactly-once"; banned item 8). A
/// conforming client holds at most its ring's slots in flight, acknowledges every half ring of replies
/// (`slates_client` `ack_every`: slots / 2), and may lose one acknowledgement sent unawaited before the next is due:
/// `slots + slots / 2 + slots / 2`. Past it a new request is refused `AcknowledgementOwed`, never run or recorded,
/// so a client that never acknowledges cannot grow a shard's records, and every snapshot that encodes them, without
/// bound (RIFL, Lee et al., SOSP 2015, bounds its completion records the same way, by the client's acknowledgements).
pub(crate) fn completion_bound(state: &ShardState) -> u64 {
  u64::from(state.config.region.slots).saturating_mul(2)
}

/// The refusal, counted, a new request from `client` meets when its unacknowledged records here reached their bound
/// ([`completion_bound`]), or `None`. An acknowledgement is never refused: it is how a client gets under the bound.
/// Asked where the verb would record its completion, after its retry lookup, so a retry is always answered.
fn acknowledgement_owed(
  state: &mut ShardState,
  origin: u64,
  client: u32,
  body: &RequestBody,
) -> Option<ReplyBody> {
  if matches!(body, RequestBody::Acknowledge { .. }) {
    return None;
  }
  let retained =
    u64::try_from(state.db.partition().retained_completions(origin, client)).unwrap_or(u64::MAX);
  let bound = completion_bound(state);
  if retained < bound {
    return None;
  }
  let refusal = Refusal::AcknowledgementOwed { retained, bound };
  state.count(refusal_name(&refusal), 1);
  Some(refused(refusal))
}

/// Serves a verb whose owner shard is this one: sent to another host of this region when that host owns its volume
/// (§4.8 "Lookup", before any completion is recorded here), else run with its completion record.
fn serve_here(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  request: u64,
  (origin, id, client_id): (u64, RequestId, u32),
  principal: Principal,
  body: RequestBody,
) -> Served {
  // A mount of a volume this node lost to a successor is refused by its kind here, where the loss is known (A-334's
  // sibling): the client's shard took the volume for this node's own, since this node created it.
  if let Some(refusal) = crate::remote_attach::lost_attach(state, &body) {
    return Served::Reply(record_completion(state, origin, id, refused(refusal)));
  }
  // A volume another host of this region owns goes to it (§4.8 "Lookup"), before any completion is recorded here.
  if let Some((region, volume)) = owned_by_another_host(state, &body) {
    // An attach of a volume this node lost is remembered with its client before it is sent, so the client's reap
    // reaches the successor (A-222's rule, which the client's shard applies to another host's volume).
    if crate::remote_attach::lost_record_attach(state, &body).is_some()
      && let Err(refusal) = crate::remote_attach::remember_volume(state, client, volume)
    {
      return Served::Reply(record_completion(state, origin, id, refused(refusal)));
    }
    return forward_to_owner(state, client, request, principal, region, volume, body);
  }
  if let Some(owed) = acknowledgement_owed(state, origin, id.client, &body) {
    return Served::Reply(owed);
  }
  let reply = run_recorded(state, origin, id, client_id, &principal, body);
  // A change this verb was refused for a delegation asked for a recall; it is sent now, after the verb's own
  // transaction (A-79).
  crate::delegation::drain(state);
  match reply {
    Some(reply) => Served::Reply(reply),
    // The verb deferred its reply to a fleet commit (AUD-11), or its reply waits for its volume's FUSE mounts to be told
    // (AC-3.3): it comes back through `deliver`.
    None => Served::Forwarded,
  }
}

/// The reply [`run_recorded`] gives, with its completion recorded in the verb's own transaction unless it has none yet:
/// a `deferred` reply's completion follows the fleet commit or the live volume's durability it waits for, and a verb
/// that never started (answered `Overloaded`) records none, so a resend under its id runs the verb.
fn recorded_unless_deferred(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  deferred: bool,
  reply: ReplyBody,
) -> ReplyBody {
  if deferred {
    reply
  } else if not_started(&reply) {
    unrecorded_not_started(state, reply)
  } else {
    record_completion(state, origin, id, reply)
  }
}

/// Runs a verb on this shard with its effects and its completion record in one durable step
/// (`Db::begin` … `commit`: one log record, so a crash leaves both or neither, AC-2.3).
fn run_recorded(
  state: &mut ShardState,
  origin: u64,
  id: RequestId,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> Option<ReplyBody> {
  // A verb whose reply could be held for durability meets the held replies' bound before it runs, never after its
  // effects commit (A-200; `live_ship::refuse_over_bound`).
  if let Some(refusal) = crate::live_ship::refuse_over_bound(state, &body) {
    return Some(refusal);
  }
  // Two chokepoint spans measure one verb (§4.14): `shard.op` over the whole verb (one verb on its
  // owner shard, no awaits inside), and `log.append` over the durable `Db::commit` within it (one
  // op-log record appended and published). Each opens *within* the span that caused it — `shard.op`
  // within the `ring.request` span of the slot read (carried here from the origin shard when the verb
  // was forwarded), `log.append` within `shard.op` — so a request's spans are one trace with their
  // causes named; a verb whose cause crossed a boundary that carried none (a cross-node forward) opens
  // unlinked and says so. The `shard.op` label is content-free — a read (0) or a mutation (1), the
  // `{verb}` dimension at the coarsest honest granularity (a finer per-verb code is a follow-up); the
  // `log.append` label is the partition. Neither carries a path, a name or bytes.
  let start_ns = state.clock.monotonic_ns();
  let label = u32::from(mutates_shard_image(&body)); // before `dispatch` moves `body`
  let op = match state.current_span {
    Some(cause) => state
      .tracer
      .open_within(&cause, Chokepoint::ShardOp, start_ns),
    None => state
      .tracer
      .open_unlinked(id, Chokepoint::ShardOp, start_ns),
  };
  // The verb's own span is the cause of everything deeper in it (a `merge.verdict`, a `land.entry`).
  let outer = state.current_span.replace(op.context());
  // A mutation's room first, outside its transaction: the blocks freed since the last publication are released
  // when the arena is short of them (A-64).
  if label == 1 {
    relieve_deferred(state);
  }
  state.db.begin();
  state.current_request = Some((origin, id));
  // Where the verb's volume stands in its journal before it runs: its reply waits for the volume's FUSE mounts to be
  // told of whatever it records (AC-3.3; `crate::told`).
  state.told_owed = None;
  let mark = body
    .volume()
    .and_then(|volume| crate::told::mark(state, to_db_volume(volume)));
  let live_volume = body.volume().map(to_db_volume);
  let reply = dispatch(state, client_id, principal, body);
  state.current_request = None;
  let route = state.reply_route.take();
  // A verb that deferred its acceptance to a fleet commit (`submit` at `f > 0`; AUD-11) commits its
  // effects now but records no completion and gets no reply here: both follow the commit at the
  // quorum (`merge_service::resolve_accepted`), so a retry meanwhile joins the wait, never reads a
  // success the quorum has not committed.
  let mut deferred = std::mem::take(&mut state.acceptance_deferred);
  // A verb on a live-shipped volume answers once its publication is durable on `f` candidates (A-200): like a deferred
  // acceptance, its effects commit now and its completion and reply follow (`crate::live_ship::resolve`).
  let (live_target, destroy_target) =
    replicated_waits(state, live_volume.filter(|_| !deferred), label, &reply);
  deferred |= live_target.is_some() || destroy_target.is_some();
  let reply = recorded_unless_deferred(state, (origin, id), deferred, reply);
  let append = state.tracer.open_within(
    &op.context(),
    Chokepoint::LogAppend,
    state.clock.monotonic_ns(),
  );
  let reply = match state.db.commit(&mut state.segment) {
    Ok(_) => reply,
    Err(e) => {
      // Nothing of the verb is durable and the partition has been rolled back to its durable state —
      // the effects and the completion record gone together (`Db::commit`, AC-2.3; AUD-06). What the
      // verb built outside the partition for a record that no longer exists is released with it, so a
      // client retrying into a segment that cannot publish leaks nothing per attempt. The refusal is
      // counted here (it is deliberately *not* recorded as a completion: a retry must re-execute).
      let refusal = refusal_of_db(&e);
      state.count(refusal_name(&refusal), 1);
      reconcile_unpublished_effects(state);
      refused(refusal)
    }
  };
  // What the committed verb owed the audit goes to the control shard now, after its transaction (A-265).
  crate::landing::send_owed_audit(state);
  let end_ns = state.clock.monotonic_ns();
  state.current_span = outer;
  let partition_label = u32::from(state.partition);
  crate::telemetry::emit(state, op.end(label, end_ns));
  crate::telemetry::emit(state, append.end(partition_label, end_ns));
  if let Some(live_target) = live_target
    && !matches!(reply, ReplyBody::Refused { .. })
  {
    return crate::live_ship::defer(state, live_target, (origin, id), route, reply);
  }
  if let Some(destroy_target) = destroy_target
    && !matches!(reply, ReplyBody::Refused { .. })
  {
    crate::live_ship::defer_destroy(state, destroy_target, (origin, id), route, reply);
    return None;
  }
  if deferred && !matches!(reply, ReplyBody::Refused { .. }) {
    return None;
  }
  // A verb that recorded a change is answered once every FUSE mount of its volume has been told of it (AC-3.3): held
  // and sent by a task of this shard where it has a ring to go to, else awaited by the exchange that carried it here
  // from another node (`serve_forward`). A reply that cannot be held is answered now, counted.
  let Some(owed) = mark.and_then(|mark| crate::told::owed(state, mark)) else {
    return Some(reply);
  };
  match route {
    Some(route) => crate::told::hold_until_told(state, owed, route, reply).map(|(_, reply)| reply),
    None => {
      state.told_owed = Some(owed);
      Some(reply)
    }
  }
}

/// A volume and the position (a live log's) or sequence (a tombstone's) a verb's reply waits for.
type ReplicatedWait = (DbVolumeId, u64);

/// What a verb that just committed on `volume` must wait for before it answers, beyond its own partition: a mutation of
/// a live-shipped volume, its log durable where its publication put it (A-200; `live_ship::durable_target`); a destroy,
/// its tombstone placed at `f + 1` (`live_ship::tombstone_target`). A destroy never waits on the live log: it publishes
/// no content to make durable, and its teardown refuses whatever waits on the log `NotFound`, which had told a client
/// its successful destroy had not happened; and answered before its tombstone was placed, an owner lost at once left a
/// takeover that served the volume again (2026-11-06, `crates/cli/tests/live_chaos.rs`).
fn replicated_waits(
  state: &ShardState,
  volume: Option<DbVolumeId>,
  label: u32,
  reply: &ReplyBody,
) -> (Option<ReplicatedWait>, Option<ReplicatedWait>) {
  let Some(volume) = volume else {
    return (None, None);
  };
  if matches!(reply, ReplyBody::Destroyed) {
    let destroy =
      crate::live_ship::tombstone_target(state, volume).map(|sequence| (volume, sequence));
    return (None, destroy);
  }
  if label != 1 || matches!(reply, ReplyBody::Refused { .. }) {
    return (None, None);
  }
  let live = crate::live_ship::durable_target(state, volume).map(|target| (volume, target));
  (live, None)
}

/// The owner's side of a forwarded verb: its own completion window first (a retry of a
/// verb this partition already ran answers from the record), then the verb and its record
/// in one step. `None` when the verb deferred its reply to a fleet commit (AUD-11).
fn run_forwarded(
  state: &mut ShardState,
  origin: u64,
  id: RequestId,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
  cause: Option<SpanContext>,
) -> Option<ReplyBody> {
  // A client's verb run here opens this shard's idle window as a ring request does on the client's own
  // shard (§4.7): the owner of a burst of forwarded verbs catches the next one spinning, not parked.
  slates_rt::registry::with_current(|ctx| ctx.note_activity());
  match state
    .db
    .partition()
    .completion(origin, id.client, id.sequence)
  {
    Seen::Completed(bytes) => {
      return Some(
        ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest)),
      );
    }
    Seen::Acknowledged => return Some(refused(Refusal::DuplicateRequest)),
    Seen::Awaiting { object, version } => {
      return crate::merge_service::serve_awaiting(state, origin, id, (object, version));
    }
    Seen::New => {
      // A retry of a submit whose acceptance still waits for its commit joins the wait, as does a retry
      // of a granted landing still running (AUD-29-03).
      if crate::merge_service::join_awaiting(state, origin, id)
        || crate::live_ship::join(state, origin, id)
        || crate::landing::join_in_flight(state, origin, id)
        || crate::image::join_in_flight(state, origin, id)
        || crate::direct_map::join_in_flight(state, origin, id)
        || crate::lease_wait::join(state, origin, id)
        || crate::work_home::join(state, origin, id)
      {
        return None;
      }
    }
  }
  // The origin's `ring.request` span context, when the forward carried one (a same-node shard), is the
  // cause of this verb's `shard.op` (§4.14); a cross-node forward carries none yet, and the span says so.
  if let Some(owed) = acknowledgement_owed(state, origin, id.client, &body) {
    return Some(owed);
  }
  state.current_span = cause;
  let reply = run_recorded(state, origin, id, client_id, principal, body);
  state.current_span = None;
  crate::delegation::drain(state);
  reply
}

/// Applies the channel-local status cursor discipline before dispatching ordinary verbs.
fn status_on_channel(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  request: u64,
  body: &RequestBody,
) -> Option<Served> {
  let slot = state.clients.get_mut(client).ok()?;
  if let RequestBody::DaemonStatusNext { snapshot, offset } = body {
    let capacity = slates_ipc::status::page_capacity(slot.end.region());
    let now = state.clock.monotonic_ns();
    return Some(Served::Reply(
      slot
        .status_pages
        .page(*snapshot, *offset, capacity, now, &mut state.store.metadata)
        .unwrap_or_else(refused),
    ));
  }
  slot.status_pages.clear(&mut state.store.metadata);
  if matches!(body, RequestBody::DaemonStatus) {
    let expires = state
      .clock
      .monotonic_ns()
      .saturating_add(state.config.failover_slo_ns);
    slot
      .status_pages
      .begin(request, expires, &mut state.store.metadata);
    return Some(scatter_status(state, client.index(), request));
  }
  None
}

/// Format: the count of verbs answered `Overloaded` whose answer was left unrecorded, so a resend under the request's
/// id runs the verb ([`not_started`]). Shown in `slates status`.
const OVERLOADED_UNRECORDED: &str = "verb.overloaded_unrecorded";

/// Whether `reply` says its verb was not started: `Overloaded`, which the protocol defines as "the request was not
/// started and may be retried" under the same id (`slates_ipc::protocol::Refusal::Overloaded`; the client holds such an
/// id for its resend, `slates_client::Client::begin_again`). Such a reply is never a completion (§4.9 RIFL): recorded,
/// it was what every resend under the id met for good (GAPS, "The many-client load test from 10,000 to 100,000
/// clients"; `docs/bugs/2026-11-17-an-overloaded-verb-was-recorded-as-its-completion.md`).
fn not_started(reply: &ReplyBody) -> bool {
  matches!(
    reply,
    ReplyBody::Refused {
      refusal: Refusal::Overloaded { .. }
    }
  )
}

/// Answers a verb that was not started ([`not_started`]) without recording a completion, so the request's id stays
/// unanswered and a resend under it runs the verb; counted by the refusal's name and [`OVERLOADED_UNRECORDED`]. Its
/// transaction commits what the verb applied before refusing, which a resend meets as its own: nothing; the verb's
/// undoing of what it began (`crate::shards::attach` ends the attachment it recorded when its stream's task cannot be
/// spawned); or the write lease a FUSE or WinFsp mount took before its task, which the resend's attach renews as its
/// holder's own ([`take_write_lease`]).
fn unrecorded_not_started(state: &mut ShardState, reply: ReplyBody) -> ReplyBody {
  state.served = state.served.saturating_add(1);
  if let ReplyBody::Refused { refusal } = &reply {
    state.count(refusal_name(refusal), 1);
  }
  state.count(OVERLOADED_UNRECORDED, 1);
  reply
}

/// Records the completion (RIFL) and counts the refusal; the reply is then durable and may be
/// sent. `origin` is the host whose client issued the request — this node's own host for a local client,
/// the authenticated forwarding peer for a cross-node forwarded verb — so the completion key is globally
/// unique and a forwarded request never collides with a local client sharing its per-node id (§4.8).
pub fn record_completion(
  state: &mut ShardState,
  origin: u64,
  id: RequestId,
  reply: ReplyBody,
) -> ReplyBody {
  record_completion_routed(state, (origin, id), reply, None)
}

/// [`record_completion`], for a completion a green's successor adopted with `work`, the work whose submit it completes
/// (A-288): the partition keeps the completion counted under the work while it retains it, which keeps the work's route.
fn record_completion_routed(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  reply: ReplyBody,
  work: Option<DbVolumeId>,
) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  let record = Op::CompletionRecorded {
    record: CompletionRecord {
      origin,
      client: id.client,
      sequence: id.sequence,
      result: reply.to_bytes(),
      work,
    },
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &record, now) {
    return refused(refusal_of_db(&e));
  }
  state.served = state.served.saturating_add(1);
  if let ReplyBody::Refused { refusal } = &reply {
    state.count(refusal_name(refusal), 1);
    // Every refuse-all budget refusal, whatever path produced it (a write, a merge version, a takeover's
    // restore), logs the store's breakdown once — the create path alone did before, and a takeover's
    // `BudgetExceeded { available: 0 }` on CI (run 36828863866, 2026-10-01) carried no breakdown.
    if let Refusal::BudgetExceeded { available } = refusal {
      report_first_budget_refusal(state, "any verb", 0, *available);
    }
  }
  reply
}

/// Forwards a volume-bound request to its owner shard: a task there runs the verb on the
/// owner's state and hands the reply back to this shard's queue (sharing by move both ways).
/// Judges a request decoded at `now` against this shard's closed loop (§4.7; A-326). A request of the caller's own
/// continues or ends the loop ([`loop_hold`]), its forward's hold set and its round's loop I/O marked. A client's
/// acknowledgement, which it sends itself before a request every half ring of replies (§4.9), is the protocol's: it keeps
/// a loop's clock running when it comes within F of the loop's last I/O and ends a loop it comes after a longer pause,
/// but never lengthens a run or holds a shard. Skipped altogether, a debug build's acknowledgement ended the loop it
/// sat in, the next create read more than F after the loop's last I/O (64 to 75 of a loop's 400 forwards unheld,
/// against 20 to 31); counted as a request, it made a paced client's destroy the third request of a loop, which held
/// its owner (closed_loop.rs, three runs each, the Mac Studio, 2026-11-18).
fn judge_loop_request(state: &mut ShardState, body: &RequestBody, now: u64) {
  if !matches!(body, RequestBody::Acknowledge { .. }) {
    state.forward_hold_ns = loop_hold(state, now);
    state.loop_io = true;
  } else if now.saturating_sub(state.last_client_io_ns) <= crate::config::PROVISION_FLOOR_NS {
    state.loop_io = true;
  } else {
    state.loop_run = 0;
  }
}

/// Judges a ring request read at `now` (§4.7; A-326) and returns the idle window a forward made for it holds its owner
/// open for. The request continues this shard's closed loop when it came within F of the end of the shard's last round
/// of its clients' loop I/O (their requests read, the replies to them written, never an acknowledgement;
/// `ShardState::last_client_io_ns`): its client sent it within F of its last reply, R9's closed loop (A-223's window
/// rule). Its hold is the loop's revisit bound at
/// R9's percentile (`DaemonConfig::closed_loop_hold_ns`), but never more cycles of 2F than the loop had already run
/// before this request, so the spin a loop's end leaves on the shards it went round is at most what the loop itself
/// took (the ski-rental bound, counted in the loop's own cycles). A request after a longer pause ends the loop and holds
/// nothing, and neither does the first request after it: a paced client's create and its destroy hold no shard. The gap
/// is read against the shard's client I/O, one clock read a round, so a shard busy with several clients counts their
/// requests as one loop, and never against its background work, which kept going through a paced client's pause:
/// judged against the shard's last work, a local destroy's slices made 1 to 3 of 24 paced cycles look like a loop's
/// (`crates/server/tests/closed_loop.rs`, 2026-11-18). Stamped at a round's start instead of its client I/O's end, a
/// debug build's round counted toward every think time and left 69 to 83 of a loop's 400 forwards unheld, against 22.
/// A client's acknowledgement is judged apart ([`judge_loop_request`]).
pub(crate) fn loop_hold(state: &mut ShardState, now: u64) -> u64 {
  if now.saturating_sub(state.last_client_io_ns) > crate::config::PROVISION_FLOOR_NS {
    state.loop_run = 0;
    return 0;
  }
  let ran = state.loop_run;
  state.loop_run = ran.saturating_add(1);
  ran
    .saturating_mul(crate::config::CLOSED_LOOP_CYCLE_NS)
    .min(state.config.closed_loop_hold_ns)
}

fn forward(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  client_id: u32,
  principal: Principal,
  body: RequestBody,
  owner: u16,
) -> Served {
  // The `ring.request` span context of this slot read rides to the owner, so the verb's `shard.op` there
  // opens within it: one trace across the shard boundary, the cause named (§4.14).
  let cause = state.current_span;
  // The idle window the owner holds open for the closed loop this request continues, 0 for none (A-326).
  let hold_ns = state.forward_hold_ns;
  match send_forward(
    state.shard,
    client_index,
    request,
    client_id,
    &principal,
    &body,
    owner,
    (cause, hold_ns),
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
        cause,
        hold_ns,
      });
      Served::Forwarded
    }
    Err(_) => Served::Reply(refused(Refusal::NotFound)),
  }
}

/// Spawns the owner's task for one forward; the reply comes back as a task on `origin`. `cause` is the
/// origin's `ring.request` span context, the cause of the verb's `shard.op` on the owner (§4.14); `hold_ns` the idle
/// window the owner holds open for the client's closed loop, 0 for none (`ShardState::forward_hold_ns`; A-326).
#[allow(clippy::too_many_arguments)] // a forward's whole identity: where from, who, what, where to, and its cause
fn send_forward(
  origin: u16,
  client_index: u32,
  request: u64,
  client_id: u32,
  principal: &Principal,
  body: &RequestBody,
  owner: u16,
  (cause, hold_ns): (Option<SpanContext>, u64),
) -> Result<(), slates_rt::RtError> {
  let principal = principal.clone();
  let body = body.clone();
  let mut unstarted = crate::xshard::AnswerUnstarted::new(
    origin,
    owner,
    client_index,
    request,
    refused(Refusal::Overloaded { shard: owner }),
  );
  let task = SpawnRequest::new(
    Box::pin(async move {
      unstarted.started();
      // The closed loop this forward was made for comes back to this shard only after going round the others: the
      // owner holds its window for it (A-326).
      if hold_ns > 0 {
        slates_rt::registry::with_current(|ctx| ctx.hold_idle_window(hold_ns));
      }
      let id = RequestId::from_word(request);
      let mut elsewhere = None;
      let mut remember_first = None;
      let mut carried = None;
      let reply = crate::state::with_state(|s| {
        s.last_work_ns = s.clock.monotonic_ns();
        // A mount of a volume this node lost to a successor is refused by its kind here, where the loss is known
        // (A-334's sibling), its completion keyed as every same-node forward's is.
        if let Some(refusal) = crate::remote_attach::lost_attach(s, &body) {
          let anchor = s.origin_anchor.0;
          return Some(record_completion(s, anchor, id, refused(refusal)));
        }
        // A volume another host of this region owns goes to it (§4.8 "Lookup"), before any completion is
        // recorded here: from the control shard, which holds the fleet's sessions.
        if let Some((region, volume)) = owned_by_another_host(s, &body) {
          let control = s.shards.first().copied();
          remember_first = crate::remote_attach::lost_record_attach(s, &body);
          elsewhere = Some((
            region,
            volume,
            control,
            forwarded_request(s, request, principal.clone(), body.clone()),
          ));
          return None;
        }
        // Where the reply goes — the origin shard's client ring — kept for a verb that answers later.
        s.reply_route = Some(crate::merge_service::ReplyRoute {
          shard: origin,
          client_index,
          request,
          through_work: false,
        });
        // A same-node cross-shard forward serves a local client, so its completion keys on this node's stable
        // cert-anchor (the same id the local `serve` path uses — task #22), not the ephemeral member id.
        let origin = s.origin_anchor.0;
        let reply = run_forwarded(s, origin, id, client_id, &principal, body, cause);
        carried = Some(take_carried(s, request));
        reply
      })
      .unwrap_or_else(|| Some(refused(Refusal::NotFound)));
      if let Some((region, volume, control, request_bytes)) = elsewhere {
        // An attach of a volume this node lost is remembered with its client, on the client's shard where its slot is,
        // and sent from there once it is, so the client's reap reaches the successor (A-222's rule; A-334's sibling).
        if remember_first.is_some() {
          crate::xshard::send_reply_home(origin, move || {
            remember_then_forward(
              (origin, control),
              (client_index, client_id, request),
              (region, volume),
              request_bytes,
            );
          });
          return;
        }
        forward_for_local_client(
          origin,
          control,
          (client_index, request),
          region,
          volume,
          request_bytes,
        );
        return;
      }
      // A verb that deferred its reply to a fleet commit (AUD-11) answers through the merge plane, and one whose reply
      // waits for its volume's FUSE mounts to be told (AC-3.3) through the task holding it; neither carries anything
      // else home, since only an attach does, and an attach records nothing in its volume.
      let Some(reply) = reply else {
        return;
      };
      crate::xshard::send_reply_home(origin, move || {
        // What the verb owes its client besides the reply (a stream end, A-161) goes home with it, for `send_reply`
        // to hand over first. A shard whose state is out of reach drops it, counted, and the stream then ends.
        if let Some(carried) = carried {
          let _ = crate::state::with_state_counted(|s| keep_carried(s, request, carried));
        }
        crate::state::deliver(client_index, request, reply, true);
      });
    }),
    None,
  );
  crate::xshard::send_owing(owner, task)
}

/// On the client's shard `origin`: remembers that the client at `client_index` (still the client `client_id`) attaches
/// `volume`, a volume this node lost to a successor, at the successor (`remote_attach::remember_volume_at`), and then
/// sends the attach there ([`forward_for_local_client`]), so it is never sent unremembered and the client's reap reaches
/// it. Past the client's bound the attach is answered `RemoteAttachmentsFull`, recorded as its completion here, where
/// the client's shard records A-222's own refusal of it; a shard whose state is out of reach answers it `Overloaded`,
/// not started and not recorded.
fn remember_then_forward(
  (origin, control): (u16, Option<u16>),
  (client_index, client_id, request): (u32, u32, u64),
  (region, volume): (u64, VolumeId),
  request_bytes: Vec<u8>,
) {
  let remembered = crate::state::with_state(|s| {
    let refusal =
      crate::remote_attach::remember_volume_at(s, (client_index, client_id), volume).err()?;
    let anchor = s.origin_anchor.0;
    Some(record_completion(
      s,
      anchor,
      RequestId::from_word(request),
      refused(refusal),
    ))
  });
  match remembered {
    Some(None) => forward_for_local_client(
      origin,
      control,
      (client_index, request),
      region,
      volume,
      request_bytes,
    ),
    Some(Some(reply)) => crate::state::deliver(client_index, request, reply, true),
    None => crate::state::deliver(
      client_index,
      request,
      refused(Refusal::Overloaded { shard: origin }),
      true,
    ),
  }
}

/// Sends a local client's verb, run on its volume's owner shard here, to the volume's owner host over the fleet,
/// from the control shard, and delivers the reply to the client's ring on `origin`. A control channel that refuses
/// the task, or no control shard, answers `HomedElsewhere`, as an owner not found does. No route is cached: the
/// client's slot lives on `origin`, and a cached route is only a hint.
fn forward_for_local_client(
  origin: u16,
  control: Option<u16>,
  (client_index, request): (u32, u64),
  region: u64,
  volume: VolumeId,
  request_bytes: Vec<u8>,
) {
  let deliver_home = move |reply: ReplyBody| {
    move || {
      crate::state::with_state_counted(|s| s.count(FORWARD_LOCAL_DELIVERED_HOME, 1));
      crate::state::deliver(client_index, request, reply, true);
    }
  };
  let sent = control.map(|control| {
    let mut unstarted = crate::xshard::AnswerUnstarted::new(
      origin,
      control,
      client_index,
      request,
      refused(Refusal::Overloaded { shard: control }),
    );
    let task = SpawnRequest::new(
      Box::pin(async move {
        unstarted.started();
        crate::state::with_state_counted(|s| s.count(FORWARD_LOCAL_CONTROL_STARTED, 1));
        let (replies, _) = resolve_and_forward(volume, region, None, vec![request_bytes]).await;
        crate::state::with_state_counted(|s| s.count(FORWARD_LOCAL_RESOLVED, 1));
        let reply = replies.first;
        crate::xshard::send_reply_home(origin, deliver_home(reply));
      }),
      None,
    );
    crate::xshard::send_owing(control, task).map_err(|refusal| (refusal, control))
  });
  count_local_handoff(sent.as_ref().is_some_and(Result::is_ok));
  let refusal = match sent {
    Some(Ok(())) => return,
    Some(Err((refusal, control))) => control_refusal(&refusal, control, region),
    None => Refusal::HomedElsewhere { region },
  };
  crate::xshard::send_reply_home(origin, deliver_home(refused(refusal)));
}

/// Counts a local client's forward handed to the control shard (`handed`) or refused there.
fn count_local_handoff(handed: bool) {
  crate::state::with_state_counted(|s| {
    s.count(
      if handed {
        FORWARD_LOCAL_TO_CONTROL
      } else {
        FORWARD_LOCAL_CONTROL_REFUSED
      },
      1,
    );
  });
}

/// The refusal answering a client whose forward the control shard's channel refused: `Overloaded` while that channel is
/// full (retryable under the same id, since the verb never ran), `HomedElsewhere` when the control shard is gone, as an
/// owner not found is. Until 2026-11-07 a full channel was answered `HomedElsewhere`, which tells the client its volume
/// lives in another region: 10,198 to 10,298 reads of `a_silent_owner_leaves_no_forward_the_lane_admitted_unrun`.
fn control_refusal(refusal: &slates_rt::RtError, control: u16, region: u64) -> Refusal {
  match refusal {
    slates_rt::RtError::ControlFull { .. } => Refusal::Overloaded { shard: control },
    _ => Refusal::HomedElsewhere { region },
  }
}

/// Holds the forwarded verb `bytes` unanswered while a test's forward silence names its volume
/// (`Daemon::inject_forward_silence`), polled at the heartbeat. In production the fault is unset and the request is not
/// decoded: one field read. A sleep the runtime could not time ends the hold, counted.
pub(crate) async fn injected_forward_silence(bytes: &[u8]) {
  if !crate::state::with_state(|s| s.injected_forward_silence.is_some()).unwrap_or(false) {
    return;
  }
  let Some(volume) = decode_body::<ForwardedRequest>(bytes)
    .ok()
    .and_then(|forwarded| volume_of(&forwarded.body))
  else {
    return;
  };
  while crate::state::with_state(|s| s.injected_forward_silence == Some(volume)).unwrap_or(false) {
    if slates_rt::futures::sleep(crate::daemon::HEARTBEAT_NS)
      .await
      .is_err()
    {
      crate::state::with_state_counted(|s| s.count(FORWARD_SILENCE_UNTIMED, 1));
      return;
    }
  }
}

/// Counter: a test's forward silence the runtime could not time; the verb was served at once.
const FORWARD_SILENCE_UNTIMED: &str = "fleet.forward_silence_untimed";

/// Counter: a local client's verb for a volume another host owns, handed from the volume's shard here to the control
/// shard to be forwarded (`forward_for_local_client`). With the counters after it, each hop of the forward is counted,
/// so a reply lost in a stall is placed by the first count that falls behind the one before it (fixtures' f = 1 r3k
/// stall, 2026-11-07: requests in flight for 2,670 s with every daemon idle and nothing deferred or dropped).
const FORWARD_LOCAL_TO_CONTROL: &str = "fleet.forward.local.to_control";
/// Counter: such a verb the control shard's channel refused; it is answered `HomedElsewhere`.
const FORWARD_LOCAL_CONTROL_REFUSED: &str = "fleet.forward.local.control_refused";
/// Counter: the control shard's task for such a verb started.
const FORWARD_LOCAL_CONTROL_STARTED: &str = "fleet.forward.local.control_started";
/// Counter: its owner's reply (or the refusal standing for one) was resolved on the control shard.
const FORWARD_LOCAL_RESOLVED: &str = "fleet.forward.local.resolved";
/// Counter: its reply reached the client's shard, to be written into the client's ring.
const FORWARD_LOCAL_DELIVERED_HOME: &str = "fleet.forward.local.delivered_home";

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
      (pending.cause, pending.hold_ns),
    ) {
      Ok(()) => any = true,
      Err(slates_rt::RtError::ControlFull { .. }) => {
        state.pending_forwards.push_front(pending);
        break;
      }
      Err(_) => {
        refuse_unsent_forward(state, pending);
        any = true;
      }
    }
  }
  any
}

/// Refuses a kept forward that will never reach its owner: answered `NotFound` here, its `ring.request` span ending with
/// the refusal's write like any deferred reply's.
fn refuse_unsent_forward(state: &mut ShardState, pending: PendingForward) {
  let ring = state.forwarded_rings.remove(&pending.request);
  state.deferred.push(Deferred {
    client_index: pending.client_index,
    request: pending.request,
    reply: refused(Refusal::NotFound),
    recorded: false,
    ring,
  });
}

/// Counter: the serve loop could not ask a kept forward's owner for room because it was not running as a task on its
/// shard, which it always is. A bug signal: such a forward waits for the loop's next round instead of the owner's word.
const FORWARD_ROOM_UNASKED: &str = "forward.kept.room_unasked";

/// The asking half of the room wake (`slates_rt::room`) for the forwards a full control channel refused, run by the serve
/// loop as it is about to idle (§4.3 "admission limit": backpressure, never a drop). It asks the owner of the first kept
/// forward to tell this shard when its control channel next has room, then tries the kept forwards once more. The loop
/// idles only if that try is refused too, and then the owner's next drain that takes a message wakes it: either the try
/// finds the room a drain made meanwhile, or that drain finds the ask. So a kept forward waits for no timer and for no
/// other request. An owner that has left its loop will never drain again, so its kept forwards are refused typed now.
/// Whether the loop should serve a round instead of idling (a forward went out or was refused); false with none kept.
pub fn ask_room_for_kept_forwards(state: &mut ShardState) -> bool {
  let Some(owner) = state.pending_forwards.front().map(|pending| pending.owner) else {
    return false;
  };
  match slates_rt::futures::want_room(owner) {
    Ok(()) => retry_forwards(state),
    Err(slates_rt::RtError::ShardGone { .. }) => {
      let kept = std::mem::take(&mut state.pending_forwards);
      for pending in kept {
        if pending.owner == owner {
          refuse_unsent_forward(state, pending);
        } else {
          state.pending_forwards.push_back(pending);
        }
      }
      true
    }
    Err(_) => {
      state.count(FORWARD_ROOM_UNASKED, 1);
      retry_forwards(state)
    }
  }
}

/// A listing is a scatter-gather over every shard: each answers with what it owns; the origin
/// merges when the last part arrives.
fn scatter_list(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  principal: Principal,
  after: Option<VolumeId>,
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = list_part(state, &principal, after);
  if others.is_empty() {
    return Served::Reply(list_page(vec![mine]));
  }
  state
    .scatters
    .insert(request, (client_index, others.len(), vec![mine]));
  let origin = state.shard;
  for shard in others {
    let principal = principal.clone();
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part =
          crate::state::with_state(|s| list_part(s, &principal, after)).unwrap_or_default();
        crate::xshard::send_reply_home(origin, move || {
          gather(request, part);
        });
      }),
      None,
    );
    if let Some(reply) = send_part(state, shard, task, ScatterKind::List, request) {
      return Served::Reply(reply);
    }
  }
  Served::Forwarded
}

/// The caller's grants are a scatter-gather like a listing: a grant record is written on the shard that
/// presented its landing (the volume's owner), so every shard answers with the grants it holds for the
/// principal and the origin merges when the last part arrives.
fn scatter_grants(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  principal: Principal,
  after: Option<u64>,
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = crate::landing::grants_part(state, &principal, after);
  if others.is_empty() {
    return Served::Reply(crate::landing::grants_page(vec![mine]));
  }
  state
    .grant_scatters
    .insert(request, (client_index, others.len(), vec![mine]));
  let origin = state.shard;
  for shard in others {
    let principal = principal.clone();
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = crate::state::with_state(|s| crate::landing::grants_part(s, &principal, after))
          .unwrap_or_default();
        crate::xshard::send_reply_home(origin, move || {
          gather_grants(request, part);
        });
      }),
      None,
    );
    if let Some(reply) = send_part(state, shard, task, ScatterKind::Grants, request) {
      return Served::Reply(reply);
    }
  }
  Served::Forwarded
}

/// Folds one shard's part of a `grants` page into the scatter and, on the last part, delivers the merged page.
fn gather_grants(request: u64, part: PagePart<slates_ipc::protocol::GrantSummary>) {
  let done = crate::state::with_state(|s| {
    let entry = s.grant_scatters.get_mut(&request)?;
    entry.2.push(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      let refused_for = s.scatter_refusals.remove(&request);
      s.grant_scatters
        .remove(&request)
        .map(|entry| (entry, refused_for.map(|shard| part_unsent(s, shard))))
    } else {
      None
    }
  })
  .flatten();
  if let Some(((client_index, _, parts), unsent)) = done {
    let reply = unsent.unwrap_or_else(|| crate::landing::grants_page(parts));
    crate::state::deliver(client_index, request, reply, false);
  }
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
        crate::xshard::send_reply_home(origin, move || {
          gather_status(request, part);
        });
      }),
      None,
    );
    if let Some(reply) = send_part(state, shard, task, ScatterKind::Status, request) {
      return Served::Reply(reply);
    }
  }
  Served::Forwarded
}

/// Which scatter a part belongs to, so a part that cannot be sent is settled in its own table.
#[derive(Clone, Copy)]
enum ScatterKind {
  /// `scatter_list`, its entry in [`ShardState::scatters`].
  List,
  /// `scatter_grants`, in [`ShardState::grant_scatters`].
  Grants,
  /// `scatter_status`, in [`ShardState::status_scatters`].
  Status,
  /// `scatter_acknowledge`, in [`ShardState::ack_scatters`] (A-216).
  Acknowledge,
}

/// How long a scatter's part waits for room in a full control channel before it is refused (A-216's sibling sweep).
/// Derived: half the client's reply deadline (the liveness budget, which `slates_client::Deadlines` waits a reply for),
/// so the typed `Overloaded` reaches the client before it gives the reply up; a burst, such as the resends after a
/// daemon's restart, drains well within it.
const PART_ROOM_BUDGET_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS / 2;

/// Sends a scatter's part, `task`, to `shard`. A control channel full at once hands the part to a task of this shard
/// that waits for room for [`PART_ROOM_BUDGET_NS`] (`xshard::given_room_within`). Past that wait, or at once for a shard
/// gone or a waiting task that cannot be spawned, the part is settled as refused ([`settle_refused`]): the scatter then
/// answers `Overloaded { shard }`, typed, when its last part is in. Before 2026-11-05 a refused part was settled through
/// a nested state borrow that never ran, and the scatter was never answered
/// (`docs/bugs/2026-11-05-a-scatter-whose-part-could-not-be-sent-never-answered.md`). The reply, when this refusal
/// settled the scatter's last part now: the caller answers it.
fn send_part(
  state: &mut ShardState,
  shard: u16,
  task: SpawnRequest,
  kind: ScatterKind,
  request: u64,
) -> Option<ReplyBody> {
  let unsent =
    match slates_rt::registry::send_control_or_return(shard, Control::Spawn(Box::new(task))) {
      Ok(()) => return None,
      Err(slates_rt::registry::RefusedControl {
        error: slates_rt::RtError::ControlFull { .. },
        message: Some(unsent),
      }) => unsent,
      // A shard is gone only while its daemon stops: the scatter is left for the client's resend after it reconnects,
      // as every request in flight at a stop is (the scatter re-runs under its id on the next daemon), and an answer
      // from here would race the stop and end the call with no reason the next daemon would give.
      Err(slates_rt::registry::RefusedControl {
        error: slates_rt::RtError::ShardGone { .. },
        ..
      }) => {
        settle_gone(state, kind, request);
        return None;
      }
      Err(_) => return settle_refused(state, kind, request, shard).map(|(_, reply, _)| reply),
    };
  let waiting = slates_rt::futures::spawn(async move {
    if !crate::xshard::given_room_within(shard, unsent, PART_ROOM_BUDGET_NS).await {
      // Settled in its own borrow, and delivered after it: `deliver` borrows the state itself.
      if let Some(Some((client_index, reply, paged))) =
        crate::state::with_state_counted(|s| settle_refused(s, kind, request, shard))
      {
        crate::state::deliver(client_index, request, reply, paged);
      }
    }
  })
  .and_then(slates_rt::futures::detach);
  match waiting {
    Ok(()) => None,
    Err(_) => settle_refused(state, kind, request, shard).map(|(_, reply, _)| reply),
  }
}

/// Settles a part that could not be sent to `shard`: an acknowledgement's release is counted ([`release_part_back`]);
/// a listing's, grants' or status' part marks its scatter refused and counts down. When that was the scatter's last
/// part, the entry goes and the scatter's answer is returned, `Overloaded` for the first refused shard, counted, with
/// its client slot and whether it is paged.
fn settle_refused(
  state: &mut ShardState,
  kind: ScatterKind,
  request: u64,
  shard: u16,
) -> Option<(u32, ReplyBody, bool)> {
  let (done, paged) = match kind {
    ScatterKind::Acknowledge => {
      release_part_back(state, request, &refused(Refusal::NotFound));
      return None;
    }
    ScatterKind::List => (count_down(&mut state.scatters, request), false),
    ScatterKind::Grants => (count_down(&mut state.grant_scatters, request), false),
    ScatterKind::Status => (count_down(&mut state.status_scatters, request), true),
  };
  let first = *state.scatter_refusals.entry(request).or_insert(shard);
  let client_index = done?;
  state.scatter_refusals.remove(&request);
  Some((client_index, part_unsent(state, first), paged))
}

/// Format: the count of scatters left unanswered because a shard they needed was gone, its daemon stopping
/// ([`send_part`]): each is answered by the client's resend to the next daemon.
const SCATTER_SHARD_GONE: &str = "scatter.shard_gone";

/// Settles a scatter whose part's shard is gone, its daemon stopping: the entry goes, so nothing is left behind, and
/// it is counted ([`SCATTER_SHARD_GONE`]); the reply is the next daemon's, to the client's resend.
fn settle_gone(state: &mut ShardState, kind: ScatterKind, request: u64) {
  match kind {
    ScatterKind::Acknowledge => {
      state.ack_scatters.remove(&request);
    }
    ScatterKind::List => {
      state.scatters.remove(&request);
    }
    ScatterKind::Grants => {
      state.grant_scatters.remove(&request);
    }
    ScatterKind::Status => {
      state.status_scatters.remove(&request);
    }
  }
  state.scatter_refusals.remove(&request);
  state.count(SCATTER_SHARD_GONE, 1);
}

/// Counts one part of a scatter in `table` down; its client slot when that was the last part (the entry is gone).
fn count_down<T>(
  table: &mut std::collections::BTreeMap<u64, (u32, usize, T)>,
  request: u64,
) -> Option<u32> {
  let entry = table.get_mut(&request)?;
  entry.1 = entry.1.saturating_sub(1);
  if entry.1 == 0 {
    table
      .remove(&request)
      .map(|(client_index, _, _)| client_index)
  } else {
    None
  }
}

/// The answer a scatter gives when its part for `shard` could not be sent, the shard's control channel full past a
/// wait for room: the typed `Overloaded`, counted by its name.
fn part_unsent(state: &mut ShardState, shard: u16) -> ReplyBody {
  let refusal = Refusal::Overloaded { shard };
  state.count(refusal_name(&refusal), 1);
  refused(refusal)
}

/// Format: the count of an acknowledgement's releases on other shards that were refused or never sent (A-216); each
/// is taken over by the client's next acknowledgement, whose watermark covers it. Shown in `slates status`.
pub(crate) const ACK_RELEASE_REFUSED: &str = "ack.release_refused";

/// The acknowledgements a shard tracks at once (A-216): one ring of requests for each client it can serve, the most
/// that can be in flight from them. Derived: `clients_per_shard × slots`, the bound the forwarded spans keep
/// (`remember_forwarded_ring`).
pub(crate) fn ack_scatter_bound(state: &ShardState) -> usize {
  derived!(
    state
      .config
      .clients_per_shard
      .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1)),
    "clients_per_shard × slots",
    ["clients_per_shard", "region.slots"]
  )
  .get()
}

/// An acknowledgement releases the client's records on every partition that may hold them (a forwarded verb's
/// record lives at its owner), and is answered once this shard, the client's own, has released (A-216, §4.9): the
/// other shards' releases run as tasks tracked in [`ShardState::ack_scatters`] until each is back. A release refused
/// there, or never sent, is counted ([`ACK_RELEASE_REFUSED`]) and taken over by the next acknowledgement. This
/// shard's window advanced first, and every request passes it first, so a retry at or below `up_to` is answered here
/// whatever another shard still holds; and a later request to another shard queues behind its release on that
/// shard's control channel.
fn scatter_acknowledge(
  state: &mut ShardState,
  _client_index: u32,
  request: u64,
  client_id: u32,
  (up_to, owed): (u32, Vec<u32>),
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = acknowledge(state, client_id, up_to, &owed);
  // A refusal here is the reply, and nothing was released: the next acknowledgement asks again.
  if others.is_empty() || matches!(mine, ReplyBody::Refused { .. }) {
    return Served::Reply(mine);
  }
  if state.ack_scatters.len() >= ack_scatter_bound(state) {
    let unsent = u64::try_from(others.len()).unwrap_or(u64::MAX);
    state.count(ACK_RELEASE_REFUSED, unsent);
    return Served::Reply(mine);
  }
  state.ack_scatters.insert(request, others.len());
  let origin = state.shard;
  for shard in others {
    let owed = owed.clone();
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = crate::state::with_state(|s| acknowledge(s, client_id, up_to, &owed))
          .unwrap_or_else(|| refused(Refusal::NotFound));
        crate::xshard::send_reply_home(origin, move || {
          crate::state::with_state_counted(|s| release_part_back(s, request, &part));
        });
      }),
      None,
    );
    // The reply is this shard's (below); a part that cannot be sent is counted when it is settled.
    let _ = send_part(state, shard, task, ScatterKind::Acknowledge, request);
  }
  Served::Reply(mine)
}

/// One other shard's release of an acknowledgement is back (A-216): a refusal is counted, and the entry goes with its
/// last part.
fn release_part_back(state: &mut ShardState, request: u64, part: &ReplyBody) {
  if matches!(part, ReplyBody::Refused { .. }) {
    state.count(ACK_RELEASE_REFUSED, 1);
  }
  let done = match state.ack_scatters.get_mut(&request) {
    Some(left) => {
      *left = left.saturating_sub(1);
      *left == 0
    }
    None => false,
  };
  if done {
    state.ack_scatters.remove(&request);
  }
}

/// Gathers one shard's part of a status; the last part delivers the whole.
fn gather_status(request: u64, part: Option<ShardReport>) {
  let done = crate::state::with_state(|s| {
    let entry = s.status_scatters.get_mut(&request)?;
    entry.2.extend(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      let refused_for = s.scatter_refusals.remove(&request);
      s.status_scatters
        .remove(&request)
        .map(|entry| (entry, refused_for.map(|shard| part_unsent(s, shard))))
    } else {
      None
    }
  })
  .flatten();
  if let Some(((client_index, _, mut shards), unsent)) = done {
    shards.sort_by_key(|r| r.partition);
    let reply = match unsent {
      Some(unsent) => unsent,
      None => crate::state::with_state(|s| daemon_report(s, shards))
        .unwrap_or_else(|| refused(Refusal::NotFound)),
    };
    // A status capture is ephemeral; paging never appends the full report to the op log.
    crate::state::deliver(client_index, request, reply, true);
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
  // A signal's value is `Some` when measured (§4.14 A-9): the six shard signals are all computable on a
  // live shard, so each is `Some` — a real zero (no volumes, no expiring leases) is a measured zero, not
  // an absent one. The `None` case is reserved for a signal that genuinely cannot be measured in a state
  // (a mirror age at f = 0, a signal from a shard that is not reporting); the type keeps that
  // distinguishable from a numeric zero rather than conflated with it.
  let measure = |signal: HealthSignal| -> Option<u64> {
    match signal {
      HealthSignal::CatalogVolumes => Some(catalog_volumes),
      HealthSignal::LogReplayNs => Some(log_replay_ns),
      HealthSignal::LeaseExpiring => Some(lease_expiring),
      HealthSignal::RingDepth => Some(ring_depth),
      HealthSignal::ShardClients => Some(shard_clients),
      HealthSignal::ShardDeferred => Some(shard_deferred),
      HealthSignal::NfsLocalP50Ns => nfs_quantile(state, NfsTimes::Local, HALF_PPM),
      HealthSignal::NfsLocalP99Ns => nfs_quantile(state, NfsTimes::Local, P99_PPM),
      HealthSignal::NfsLocalOffCpuP99Ns => nfs_quantile(state, NfsTimes::LocalOffCpu, P99_PPM),
      HealthSignal::NfsForwardedP99Ns => nfs_quantile(state, NfsTimes::Forwarded, P99_PPM),
    }
  };
  // Each signal ages by the registry's stated basis (§4.14): a report-time value is age zero, a
  // boot-time fact is as old as the boot.
  let signals = HealthSignal::ALL
    .iter()
    .map(|&signal| Signal {
      name: signal.name().to_owned(),
      value: measure(signal),
      absence: signal.absence(),
      freshness_ns: match signal.freshness() {
        FreshnessBasis::AtReport => 0,
        FreshnessBasis::SinceBoot => since_boot,
      },
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
      .chain(
        [
          (
            CONTENT_POOL_REFUSED,
            state.store.content.arena().source_refusals(),
          ),
          (CONTENT_SEALED, state.store.content.sealed()),
          (CONTENT_SEAL_REFUSED, state.store.content.seal_refusals()),
          (CONTENT_TRIMMED, state.store.content.trimmed_bytes()),
          (
            CONTENT_TRIM_DECLINED,
            state.store.content.trim_declined_bytes(),
          ),
          (CONTENT_TRIM_REFUSED, state.store.content.trim_refusals()),
          (CONTENT_FREE_REFUSED, state.store.content.free_refusals()),
          (STORE_RELEASE_REFUSED, state.store.release_refusals),
          (STATE_BORROW_REFUSED, crate::state::lost_steps().borrowed),
          (
            STATE_RETENTION_REFUSED,
            crate::state::lost_steps().retention,
          ),
          (
            crate::xshard::RUN_REFUSED_COUNTER,
            crate::xshard::run_refused(),
          ),
          (
            crate::xshard::ROOM_WAITS_STARTED,
            crate::xshard::room_waits().started,
          ),
          (
            crate::xshard::ROOM_WAITS_ENDED,
            crate::xshard::room_waits().ended,
          ),
          (
            crate::xshard::ROOM_WAITS_PEAK,
            crate::xshard::room_waits().peak,
          ),
          (crate::xshard::KEEP_REFUSED, crate::xshard::keep_refused()),
          (
            crate::fleet::SERVE_RECEIVE_EARLIER_SEND,
            demux_sum(state, |c| c.receive_refused_earlier_send),
          ),
          (
            crate::fleet::SERVE_RECEIVE_RESOURCES,
            demux_sum(state, |c| c.receive_refused_resources),
          ),
          (
            crate::fleet::SERVE_RECEIVE_OTHER,
            demux_sum(state, |c| c.receive_refused_other),
          ),
          (crate::fleet::SERVE_OPENED, demux_sum(state, |c| c.opened)),
          (
            crate::fleet::SERVE_SETUP_REFUSED,
            demux_sum(state, |c| c.setup_refused),
          ),
          (
            crate::fleet::SERVE_PEER_SESSIONS_REFUSED,
            demux_sum(state, |c| c.peer_sessions_refused),
          ),
          (
            crate::fleet::SERVE_PEERS_REFUSED,
            demux_sum(state, |c| c.peers_refused),
          ),
          (
            crate::fleet::SERVE_HIGH_WATER,
            demux_sum(state, |c| c.high_water),
          ),
          (
            crate::fleet::DIAL_RECORD_DIALING,
            crate::fleet::record_dials_dialing(state),
          ),
          (
            crate::fleet::DIAL_RECORD_LONGEST_CALL_MS,
            crate::fleet::record_dials_longest_ms(state, |dial| dial.call_since_ns),
          ),
          (
            crate::fleet::DIAL_RECORD_LONGEST_ENROLL_MS,
            crate::fleet::record_dials_longest_ms(state, |dial| dial.enroll_since_ns),
          ),
          (
            crate::fleet::DIAL_RECORD_CALLS,
            crate::fleet::record_dials_calls(state),
          ),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(kind, count)| RefusalCount {
          kind: kind.to_owned(),
          count,
        }),
      )
      .collect(),
    replayed_records: state.recovered.replayed_records,
    replay_ns: state.recovered.replay_ns,
    torn_tail: state.recovered.torn,
    reserve_bytes: state.store.budget.capacity(),
    committed_bytes: state.store.budget.committed(),
    version_slots: state.store.versions.capacity(),
    committed_versions: state.store.versions.committed(),
    signals,
    spans_held: u64::try_from(state.telemetry.len()).unwrap_or(u64::MAX),
    spans_dropped: state.telemetry.dropped(),
    peers_probed: u32::try_from(state.formed_probe_peers.len()).unwrap_or(u32::MAX),
    retained_bytes: state.store.budget.retained(),
    retained_versions: state.store.versions.retained(),
    metadata_bytes: state.store.metadata.capacity(),
    committed_metadata: state.store.metadata.committed(),
    mapped_bytes: u64::try_from(state.store.content.mapped_bytes()).unwrap_or(u64::MAX),
    locked_bytes: u64::try_from(state.store.content.arena().locked_bytes()).unwrap_or(u64::MAX),
    control: is_control_shard(state),
    council: council_report(state),
    root: root_report(state),
    held_records: u64::try_from(state.holder_records.len()).unwrap_or(u64::MAX),
    takeovers_pending: takeovers_pending(state),
    configuration_version: state.fleet.configuration().version,
    takeover: takeover_report(state),
    detector: detector_report(state),
    sessions: state
      .record_sessions
      .iter()
      .map(|(peer, link)| link.report(*peer))
      .collect(),
    tasks_refused: slates_rt::registry::with_current(|ctx| ctx.counters().admission_refused)
      .unwrap_or(0),
    landings_awaiting: u64::try_from(state.landing.awaiting.len()).unwrap_or(u64::MAX),
    landings_awaiting_bound: u64::try_from(state.config.landings_awaiting_per_shard)
      .unwrap_or(u64::MAX),
    landings_in_flight: u64::try_from(state.landing.in_flight.len()).unwrap_or(u64::MAX),
    target_leases: u64::try_from(state.db.partition().landing_leases().count()).unwrap_or(u64::MAX),
    replicated_bytes: state.store.budget.replicated(),
    seal_recipient_id: state
      .seal_recipient
      .as_ref()
      .map(|recipient| recipient.public().id.to_vec())
      .unwrap_or_default(),
    nfs_calls: nfs_calls_of(state),
    detector_granularity_ns: detector_granularity_ns(state),
    served_sessions: state.served_sessions.values().cloned().collect(),
    detector_pool: detector_pool_report(state),
    detector_relays: detector_relay_report(state),
    registry_attachments: u64::try_from(state.attachments.live()).unwrap_or(u64::MAX),
    live: crate::live_ship::reports(state),
    snapshots: snapshot_report(state.db.stats()),
    peers_in_contact: if is_control_shard(state) {
      peers_in_contact(state).0
    } else {
      0
    },
    peers_unmeshed: if is_control_shard(state) {
      peers_in_contact(state).1
    } else {
      0
    },
    learner: if is_control_shard(state) {
      learner_report(state)
    } else {
      slates_ipc::protocol::LearnerReport::default()
    },
    failover: failover_report(state),
  }
}

/// This shard's failover estimate as the wire carries it (A-363): live on the control shard, which runs the
/// coordinator that refreshes it; every other shard's, and a laptop's, is the recovery prior alone.
fn failover_report(state: &ShardState) -> slates_ipc::protocol::FailoverReport {
  let estimate = state.failover.estimate();
  let recovery = state.failover.recovery();
  slates_ipc::protocol::FailoverReport {
    total_ns: estimate.total_ns(),
    detection_ns: estimate.detection.map_or(0, |term| term.ns),
    detection_samples: estimate.detection.map_or(0, |term| term.samples),
    confirmation_ns: estimate.confirmation.map_or(0, |term| term.ns),
    confirmation_samples: estimate.confirmation.map_or(0, |term| term.samples),
    recovery_ns: estimate.recovery.ns,
    takeovers_timed: recovery.takeovers().samples(),
    volumes_timed: recovery.volumes().samples(),
  }
}

/// This node's council configuration fetches as a learner, for status ([`slates_ipc::protocol::LearnerReport`]).
pub(crate) fn learner_report(state: &ShardState) -> slates_ipc::protocol::LearnerReport {
  let now = slates_rt::futures::now_ns();
  let log = state.learner_fetch;
  slates_ipc::protocol::LearnerReport {
    council_version: state.council.configuration().version,
    fetched_version: log.fetched_version,
    asked: log.asked,
    last_asked_ago_ns: log.asked_at_ns.map(|at| now.saturating_sub(at)),
  }
}

/// A partition's snapshot figures as the status report carries them (A-243).
fn snapshot_report(stats: slates_db::replay::DbStats) -> slates_ipc::protocol::DbSnapshotReport {
  slates_ipc::protocol::DbSnapshotReport {
    snapshots: stats.snapshots,
    bytes_written: stats.snapshot_bytes_written,
    longest_ns: stats.snapshot_ns_max,
    log_bytes_appended: stats.log_bytes_appended,
    tail_bytes: stats.tail.bytes,
    tail_replay_ns: stats.tail.replay_ns,
    budget_ns: stats.recovery_budget_ns,
    floor_bytes: stats.snapshot_floor_bytes,
  }
}

/// The NFS calls this shard served, by NFSv3 procedure (`nfs::ServiceTimes::calls`).
fn nfs_calls_of(state: &ShardState) -> Vec<u64> {
  state.nfs_service.calls.to_vec()
}

/// Whether `state` is the control shard's — the first of the daemon's shards, the one that runs the
/// membership loop and drives the consensus groups (`Daemon::council_leads` observes the same shard).
pub(crate) fn is_control_shard(state: &ShardState) -> bool {
  state.shards.first() == Some(&state.shard)
}

/// The regional council's status line ([`group_report`]).
fn council_report(state: &ShardState) -> GroupReport {
  group_report(
    state.council.is_leader(),
    state.council.voters(),
    state.council_timing,
    state.council.election_view(),
    state
      .council
      .election_rank(&crate::fleet::authenticated_alive(state)),
  )
}

/// The root group's status line ([`group_report`]).
fn root_report(state: &ShardState) -> GroupReport {
  group_report(
    state.root.is_leader(),
    state.root.voters(),
    state.root_timing,
    state.root.election_view(),
    state
      .root
      .election_rank(&crate::fleet::authenticated_alive(state)),
  )
}

/// A consensus group's status line: whether this node leads it, the election timing it derived, and its
/// election state — term, priority, lease and campaigns — with its `rank` among the voters it holds alive, as
/// its drive ranks it, and the ids of the voters in force.
fn group_report(
  leads: bool,
  voters: Vec<slates_db::HostId>,
  timing: slates_cluster::timing::ElectionTiming,
  view: slates_cluster::raft::ElectionView,
  rank: usize,
) -> GroupReport {
  GroupReport {
    leads,
    base_periods: timing.base_periods,
    span_periods: timing.span_periods,
    rtt_tail_ns: timing.broadcast_rtt_tail_ns,
    rtt_spread_ns: timing.broadcast_rtt_spread_ns,
    samples: timing.samples,
    term: view.term,
    priority_ns: view.priority.quorum_ns,
    priority_spread_ns: view.priority.spread_ns,
    rank: u32::try_from(rank).unwrap_or(u32::MAX),
    leader_lease: view.leader_lease,
    pre_elections: view.pre_elections,
    elections: view.elections,
    pre_votes_granted: view.pre_votes.granted,
    pre_votes_refused: view.pre_votes.refused,
    refused_role: view.pre_votes.refused_role,
    refused_leased: view.pre_votes.refused_leased,
    refused_term: view.pre_votes.refused_term,
    refused_log: view.pre_votes.refused_log,
    voters: view.seated,
    joint: view.joint,
    voter_ids: {
      let mut ids: Vec<u64> = voters.iter().map(|host| host.0).collect();
      ids.sort_unstable();
      ids.dedup();
      ids
    },
  }
}

/// The daemon's place in its fleet (§4.8), from this shard's placement authority — every shard's
/// `FleetNode` advances identically (the control shard hands it each peer state it folds) — with the
/// formed probe sessions summed over the shards' parts (only the control shard forms any) and the
/// consensus groups' state taken from the control shard's part (the only live one).
/// The members this node keeps direct contact with, this node left out (`fleet::keeps_direct_contact_with`), and how
/// many of them have no formed probe session yet: the probe mesh's peers and what keeps it down. Counted over the
/// members the membership holds alive (what `fleet_members` reports), not the configuration's members: from the
/// bootstrap until the others are admitted that list names the bootstrapping node alone, so a forming fleet read none
/// in contact while it probed every peer
/// (raft-chaos's and live-fixes' flows on CI, diag-live-fixes-32: `in_contact=0 probed=4` on a forming five-node fleet).
fn peers_in_contact(state: &ShardState) -> (u32, u32) {
  let mesh = crate::fleet::probe_mesh(state);
  (
    u32::try_from(mesh.in_contact.len()).unwrap_or(u32::MAX),
    u32::try_from(mesh.unmeshed.len()).unwrap_or(u32::MAX),
  )
}

fn fleet_report(state: &ShardState, shards: &[ShardReport]) -> FleetReport {
  let configuration = state.fleet.configuration();
  let control = shards.iter().find(|shard| shard.control);
  let council = control.map_or_else(|| council_report(state), |shard| shard.council.clone());
  let root = control.map_or_else(|| root_report(state), |shard| shard.root.clone());
  let takeover = control.map_or_else(|| takeover_report(state), |shard| shard.takeover.clone());
  let detector = control.map_or_else(|| detector_report(state), |shard| shard.detector.clone());
  let sessions = control.map_or_else(Vec::new, |shard| shard.sessions.clone());
  let served_sessions = control.map_or_else(Vec::new, |shard| shard.served_sessions.clone());
  let detector_granularity_ns = control.map_or_else(
    || detector_granularity_ns(state),
    |shard| shard.detector_granularity_ns,
  );
  let detector_pool = control.map_or_else(
    || detector_pool_report(state),
    |shard| shard.detector_pool.clone(),
  );
  let detector_relays = control.map_or_else(
    || detector_relay_report(state),
    |shard| shard.detector_relays.clone(),
  );
  let (held_records, takeovers_pending, configuration_version) = control.map_or_else(
    || {
      (
        u64::try_from(state.holder_records.len()).unwrap_or(u64::MAX),
        takeovers_pending(state),
        configuration.version,
      )
    },
    |shard| {
      (
        shard.held_records,
        shard.takeovers_pending,
        shard.configuration_version,
      )
    },
  );
  FleetReport {
    host: state.fleet.host().0,
    f: configuration.quorum.f,
    host_epoch: configuration.host_epoch.0,
    members: state
      .fleet
      .membership()
      .alive()
      .into_iter()
      .map(|host| host.0)
      .collect(),
    peers_probed: shards
      .iter()
      .fold(0u32, |sum, shard| sum.saturating_add(shard.peers_probed)),
    unknown_id: demux_sum(state, |c| c.unknown_id),
    inbox_full: demux_sum(state, |c| c.inbox_full),
    sessions_refused: demux_sum(state, |c| c.sessions_refused),
    replaced: demux_sum(state, |c| c.replaced),
    council,
    root,
    held_records,
    takeovers_pending,
    configuration_version,
    takeover,
    detector,
    sessions,
    detector_granularity_ns,
    served_sessions,
    detector_pool,
    detector_relays,
    peers_in_contact: shards.iter().fold(0u32, |sum, shard| {
      sum.saturating_add(shard.peers_in_contact)
    }),
    peers_unmeshed: shards
      .iter()
      .fold(0u32, |sum, shard| sum.saturating_add(shard.peers_unmeshed)),
    learner: control.map_or_else(|| learner_report(state), |shard| shard.learner),
    failover: control.map_or_else(|| failover_report(state), |shard| shard.failover),
  }
}

/// One counter summed over the serve-socket demultiplexers this shard runs (none on a laptop, or on a
/// shard other than the control shard).
fn demux_sum(
  state: &ShardState,
  counter: impl Fn(&slates_transport::demux::DemuxCounters) -> u64,
) -> u64 {
  // A handle that no longer resolves names a demultiplexer this shard no longer runs (its context ended
  // under a state that outlived it), which serves nothing and so counts nothing.
  state.demuxes.iter().fold(0u64, |sum, demux| {
    sum.saturating_add(demux.with(|demux| counter(&demux.counters())).unwrap_or(0))
  })
}

/// The daemon's view: the anchor's words in the segment and the process-wide counters, over
/// every shard's part.
fn daemon_report(state: &mut ShardState, shards: Vec<ShardReport>) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  let (
    generation,
    restarts,
    heartbeat_age_ns,
    anchor_hold_refusals,
    anchor_log_lines_refused,
    recovered,
  ) = state
    .segment
    .supervision()
    .map(|s| {
      (
        s.generation(),
        s.restarts(),
        now.saturating_sub(s.heartbeat_ns()),
        s.hold_refusals(),
        s.log_refusals(),
        s.recovered_since().is_some(),
      )
    })
    .unwrap_or((0, 0, 0, 0, 0, false));
  let fleet = fleet_report(state, &shards);
  // The node's recipient lives on the control shard: its part carries the id, whichever shard answers.
  let control_recipient = shards
    .iter()
    .find(|shard| shard.control)
    .map(|shard| shard.seal_recipient_id.clone())
    .unwrap_or_default();
  ReplyBody::DaemonStatus {
    report: Box::new(DaemonReport {
      pid: std::process::id(),
      generation,
      restarts,
      heartbeat_age_ns,
      clients_reaped: crate::daemon::CLIENTS_REAPED.load(Ordering::Acquire),
      clients_refused: crate::daemon::CLIENTS_REFUSED.load(Ordering::Acquire),
      shards,
      fleet,
      seal: seal_report(state, control_recipient),
      anchor_hold_refusals,
      log_lines_refused: slates_machine::log::log_lines_refused(),
      anchor_log_lines_refused,
      dumps_local_full: crate::local_dumps::full(),
      anchor: state.origin_anchor.0,
      recovered,
    }),
  }
}

/// The node's sealing at rest for the status report (A-92): how its root came and its id, and the key region.
fn seal_report(state: &ShardState, recipient_id: Vec<u8>) -> slates_ipc::protocol::SealReport {
  let root_id = state.seal_root.as_ref().map(|root| root.id().0);
  let (slots, held) = hyper_seal::keys_held().unwrap_or((0, 0));
  slates_ipc::protocol::SealReport {
    state: state.seal_state.name().to_owned(),
    root_id: root_id.map(|id| id.to_vec()).unwrap_or_default(),
    key_slots: u64::try_from(slots).unwrap_or(u64::MAX),
    keys_held: u64::try_from(held).unwrap_or(u64::MAX),
    recipient_id,
  }
}

/// One shard's part of a listing arrives at the origin; the last part completes the reply.
fn gather(request: u64, part: ListPart) {
  let done = crate::state::with_state(|s| {
    let entry = s.scatters.get_mut(&request)?;
    entry.2.push(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      let refused_for = s.scatter_refusals.remove(&request);
      s.scatters
        .remove(&request)
        .map(|entry| (entry, refused_for.map(|shard| part_unsent(s, shard))))
    } else {
      None
    }
  })
  .flatten();
  if let Some(((client_index, _, parts), unsent)) = done {
    let reply = unsent.unwrap_or_else(|| list_page(parts));
    crate::state::deliver(client_index, request, reply, false);
  }
}

pub(crate) fn refusal_name(r: &Refusal) -> &'static str {
  match r {
    Refusal::NotFound => "not_found",
    Refusal::AttachmentExpired { .. } => "attachment_expired",
    Refusal::SessionUnproven => "session_unproven",
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
    Refusal::BarrierIncomplete { .. } => "barrier_incomplete",
    Refusal::Unsupported { .. } => "unsupported",
    Refusal::TooManyClients => "too_many_clients",
    Refusal::LandingsAwaitingFull => "landings_awaiting_full",
    Refusal::LandingLeaseLost => "landing_lease_lost",
    Refusal::Overloaded { .. } => "overloaded",
    Refusal::BadRequest { .. } => "bad_request",
    Refusal::Unpublished { .. } => "unpublished",
    Refusal::TargetUnavailable { .. } => "target_unavailable",
    Refusal::LandingConflict { .. } => "landing_conflict",
    Refusal::LandingLeaseHeld { .. } => "landing_lease_held",
    Refusal::GrantMismatch => "grant_mismatch",
    Refusal::GrantInvalid => "grant_invalid",
    Refusal::HomedElsewhere { .. } => "homed_elsewhere",
    Refusal::OwnerUnreachable { .. } => "owner_unreachable",
    Refusal::MapUnavailable => "map_unavailable",
    Refusal::MapShareExhausted { .. } => "map_share_exhausted",
    Refusal::HolderPending => "holder_pending",
    Refusal::MapsHeld { .. } => "maps_held",
    Refusal::RemoteAttachmentsFull { .. } => "remote_attachments_full",
    Refusal::OwnMount { .. } => "own_mount",
    Refusal::TableAtCapacity { .. } => "table_at_capacity",
    Refusal::NotRecovered { .. } => "not_recovered",
    Refusal::NotShared { .. } => "not_shared",
    Refusal::PolicyForbids { .. } => "policy_forbids",
    Refusal::PolicyChanged { .. } => "policy_changed",
    Refusal::SignatureRequired { .. } => "signature_required",
    Refusal::Volume { .. } => "volume",
    Refusal::Database { .. } => "database",
    Refusal::Target { .. } => "target",
    Refusal::DurabilityPending { .. } => "durability_pending",
    Refusal::VerdictReplaced { .. } => "verdict_replaced",
    Refusal::ImageRefused { .. } => "image_refused",
    Refusal::NotRootLeader => "not_root_leader",
    Refusal::ConsensusNotInitialized => "consensus_not_initialized",
    Refusal::ConsensusAlreadyInitialized => "consensus_already_initialized",
    Refusal::ConsensusBootstrapStale => "consensus_bootstrap_stale",
    Refusal::ConsensusRecoveryStale => "consensus_recovery_stale",
    Refusal::ConsensusRecoveryUnavailable => "consensus_recovery_unavailable",
    Refusal::LeaseUnconfirmed { .. } => "lease_unconfirmed",
    Refusal::AcknowledgementOwed { .. } => "acknowledgement_owed",
    Refusal::DurabilityUnmet { .. } => "durability_unmet",
    Refusal::GrantIssuerUnverified => "grant_issuer_unverified",
    Refusal::ConsumerNotEnrolled => "consumer_not_enrolled",
    Refusal::ConsumerRevoked => "consumer_revoked",
    Refusal::DigestNotClean => "digest_not_clean",
    Refusal::DigestUnverified => "digest_unverified",
    Refusal::ReadOnlyVolume => "read_only_volume",
    Refusal::NotGreen => "not_green",
    Refusal::NotWork => "not_work",
    Refusal::UnknownBase { .. } => "unknown_base",
    Refusal::EvidenceRequired => "evidence_required",
    Refusal::ConsistentBaseUnavailable => "consistent_base_unavailable",
    Refusal::ContentUnavailable => "content_unavailable",
    Refusal::StaleEpoch { .. } => "stale_epoch",
    Refusal::AttachmentUnsupported { .. } => "attachment_unsupported",
    Refusal::ChosenPathUnavailable { .. } => "chosen_path_unavailable",
  }
}

/// Whether a verb's completion **commits a new head or seal the fleet replicates** — a creation head
/// (`Create`, `CreateGreen`, `CreateWork`, `Clone`), a sealed snapshot (`Snapshot`), or a green advance
/// (`Submit`) — and so claims the durability the committed configuration places it at (§4.8, D-18). These
/// are the writes the operator's durability policy gates ([`Refusal::DurabilityUnmet`]). Owner-local live
/// edits (`Edit`, `Declare`, `Rebase` — lost with the host by D-18's own statement), catalog changes
/// (`Resize`), destroys, attachments, grants, landings and every read claim no placement and continue.
fn claims_placed_durability(body: &RequestBody) -> bool {
  matches!(
    body,
    RequestBody::Create { .. }
      | RequestBody::CreateGreen { .. }
      | RequestBody::CreateWork { .. }
      | RequestBody::Clone { .. }
      | RequestBody::Snapshot { .. }
      | RequestBody::Submit { .. }
      | RequestBody::SubmitApproved { .. }
      | RequestBody::EditSubmit { .. }
      | RequestBody::EditSubmitHeld { .. }
      | RequestBody::SubmitHeld { .. }
      | RequestBody::SubmitSigned { .. }
      | RequestBody::ImageBuild { .. }
      | RequestBody::ImageMerge { .. }
  )
}

/// Whether a verb changes the set of volumes or a volume's roots, so the shard must republish its
/// recovery image (§4.8). Data-plane content writes go through the mount transport, not here, and
/// stand behind the same barrier there (`crate::nfs`, after every mutating procedure).
fn mutates_shard_image(body: &RequestBody) -> bool {
  body.mutates_shard_image()
}

/// What the owner-lease gate of [`dispatch`] decided.
enum LeaseGate {
  /// The verb may run now.
  Run(RequestBody),
  /// The verb's reply instead: the placeholder of a verb parked until its lease confirms
  /// (`crate::lease_wait`), or `LeaseUnconfirmed` when it cannot be parked.
  Answer(ReplyBody),
}

/// The owner-lease gate of [`dispatch`].
fn lease_gate(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> LeaseGate {
  let Some(volume) = serves_latest_state(&body) else {
    return LeaseGate::Run(body);
  };
  if state.db.partition().volume(to_db_volume(volume)).is_none() {
    return LeaseGate::Run(body);
  }
  let Some(version) = lease_refusal(state, ObjectId(volume.bytes)) else {
    return LeaseGate::Run(body);
  };
  if crate::lease_wait::park(state, client_id, principal, body, ObjectId(volume.bytes)) {
    return LeaseGate::Answer(crate::lease_wait::parked_reply());
  }
  LeaseGate::Answer(refused(Refusal::LeaseUnconfirmed { version }))
}

/// The gates a verb passes before it runs: the owner's lease, and for a work whose green lives on another shard its
/// owner for a base or a verdict (A-247): the verb is parked until the answer comes, and its later verbs park behind
/// it (`crate::work_home`).
fn gates(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> LeaseGate {
  let body = match lease_gate(state, client_id, principal, body) {
    LeaseGate::Run(body) => body,
    answered @ LeaseGate::Answer(_) => return answered,
  };
  match crate::work_home::gate(state, client_id, principal, body) {
    crate::work_home::Gate::Run(body) => LeaseGate::Run(body),
    crate::work_home::Gate::Answer(reply) => LeaseGate::Answer(reply),
  }
}

/// The gate a green's recovery puts on a verb that touches it: `Err` with the answer when the verb is not run, else `Ok`
/// with the fenced green an admin's `Destroy` of it releases. A fenced green (AUD-29-18) is never served: its recovered
/// history is not what was acknowledged. The one verb it takes is an admin's `Destroy` — the reviewed release; its
/// durable log is the evidence until then. A green a restarted successor holds by adoption is not served until the boot
/// has rebuilt its engine from this node's ledger (A-333): the request was not started and may be retried.
fn green_recovery_gate(
  state: &mut ShardState,
  touched: Option<DbVolumeId>,
  body: &RequestBody,
) -> Result<Option<DbVolumeId>, ReplyBody> {
  let fenced = touched.filter(|volume| state.fenced_greens.contains_key(volume));
  if fenced.is_some() && !matches!(body, RequestBody::Destroy { .. }) {
    state.count(GREEN_FENCED, 1);
    return Err(refused(Refusal::ContentUnavailable));
  }
  if touched.is_some_and(|volume| state.merge.awaiting_adoption.contains_key(&volume)) {
    state.count(GREEN_AWAITING_ADOPTION_REFUSED, 1);
    return Err(refused(Refusal::Overloaded { shard: state.shard }));
  }
  Ok(fenced)
}

pub(crate) fn dispatch(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  let republish = mutates_shard_image(&body);
  if let Some(refusal) = refused_before_dispatch(state, &body, republish) {
    return refused(refusal);
  }
  let touched = volume_of(&body).map(to_db_volume);
  // The owner-lease gate (§4.8 "Leases and reads"; AUD-08): a read of an owned object's **latest state** —
  // its live head, its version list, its status, a since-a-version query — is refused `LeaseUnconfirmed`
  // while this node's authority over that object is not currently confirmed (it is cut off, was paused past
  // the lease bound, or has learned of a newer configuration than it holds). Runs on the object's owner
  // shard (a forwardable read was routed here), so the fanned lease and configuration read here are the
  // owner's. An explicitly pinned immutable read (a green's named version, an attachment's pinned view)
  // is **not** gated (`serves_latest_state` returns `None`) — it keeps its separate contract. Placement
  // writes are separately gated by durability and, at `f > 0`, do not publish acceptance until the record
  // commits at the quorum (AUD-11), so they cannot return a stale success.
  //
  // Only an object this partition's catalog holds is gated: every latest-state verb answers from that
  // catalog record, so a volume with none is `NotFound` whatever the lease says, and this node has no
  // latest state of it to serve stale. A holder that keeps a peer's records but not its volume, or a node
  // that has not yet installed a configuration, answers `NotFound` for it, never `LeaseUnconfirmed`
  // (docs/bugs/2026-09-29-the-lease-gate-refused-volumes-the-node-did-not-hold.md).
  //
  // An unconfirmed lease is usually a confirmation in flight, so the verb is parked until it confirms or the
  // lease bound passes (`crate::lease_wait`), not refused at once; refused only when it cannot be parked.
  let body = match gates(state, client_id, principal, body) {
    LeaseGate::Run(body) => body,
    LeaseGate::Answer(reply) => return reply,
  };
  // The durability gate (§4.8 "Placement" — the operator's ε and coincident-failure size "gate a refusal"):
  // a write that would commit a new head or seal is refused, with the measured shortfall, while the
  // installed configuration cannot hold it to the declared policy. A field read: the shortfall was measured
  // when the configuration was installed. It sits after every completion-record lookup (`serve`,
  // `run_forwarded`), so a retry of a write that succeeded before the breach still meets its recorded reply.
  if claims_placed_durability(&body)
    && let Some(shortfall) = state.durability_shortfall
  {
    return refused(Refusal::DurabilityUnmet {
      coincident_loss: shortfall.coincident_loss,
      accepted_loss: shortfall.accepted_loss,
      coincident_failures: shortfall.coincident_failures,
    });
  }
  let fenced = match green_recovery_gate(state, touched, &body) {
    Ok(fenced) => fenced,
    Err(reply) => return reply,
  };
  let reply = dispatch_inner(state, client_id, principal, body);
  if let Some(volume) = fenced
    && matches!(reply, ReplyBody::Destroyed)
  {
    state.fenced_greens.remove(&volume);
  }
  // The content image must precede a successful completion (§4.8, AUD-05). A refusal can leave
  // an unacknowledged effect in memory, but neither this reply nor its retry may promise recovery.
  if republish && !matches!(reply, ReplyBody::Refused { .. }) {
    let touched = match &reply {
      ReplyBody::Created { id } | ReplyBody::Cloned { id } => Some(to_db_volume(*id)),
      _ => touched,
    };
    match publish_shard(state) {
      Ok(published)
        if touched.is_some_and(|volume| {
          state.by_id.contains_key(&volume)
            && !published.captured(volume)
            && !published.destroying(volume)
        }) =>
      {
        return refused(refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete));
      }
      Ok(_) => {}
      Err(error) => return refused(refusal_of_vfs(&error)),
    }
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
      base,
    } => create_green(state, principal, &name, require_evidence, base),
    RequestBody::Versions { green } => versions(state, principal, green),
    RequestBody::ChangedSince {
      green,
      version,
      from,
    } => changed_since(state, principal, green, version, from),
    RequestBody::CreateWork { green, name } => create_work(state, principal, green, &name),
    RequestBody::Edit {
      work,
      path,
      at,
      delete_len,
      bytes,
    } => edit(state, principal, work, &path, at, delete_len, &bytes),
    RequestBody::Declare { work, op } => declare(state, principal, work, op),
    RequestBody::Submit { work, evidence } => {
      submit(state, principal, work, (evidence, None), Approval::None)
    }
    RequestBody::SubmitSigned {
      work,
      evidence,
      signature,
    } => submit(
      state,
      principal,
      work,
      (evidence, Some(signature)),
      Approval::None,
    ),
    RequestBody::SubmitApproved {
      work,
      evidence,
      identity,
      proof,
      signature,
    } => submit_approved(
      state,
      principal,
      (work, evidence, signature),
      identity,
      proof,
    ),
    RequestBody::EditSubmit {
      work,
      path,
      at,
      delete_len,
      insert,
      evidence,
    } => edit_and_submit(
      state,
      principal,
      work,
      Splice {
        path: &path,
        at,
        delete_len,
        insert,
      },
      (evidence, false),
    ),
    RequestBody::EditSubmitHeld {
      work,
      path,
      at,
      delete_len,
      insert,
      evidence,
    } => edit_and_submit(
      state,
      principal,
      work,
      Splice {
        path: &path,
        at,
        delete_len,
        insert,
      },
      (evidence, true),
    ),
    RequestBody::SubmitHeld {
      work,
      evidence,
      signature,
    } => submit(
      state,
      principal,
      work,
      (evidence, signature),
      Approval::Held,
    ),
    RequestBody::SetSigners {
      volume,
      keys,
      version,
      proof,
    } => crate::landing::set_signers_verb(state, principal, (volume, keys, version), proof),
    RequestBody::MergeRecord { green, version } => {
      crate::merge_service::merge_record_verb(state, principal, green, version)
    }
    RequestBody::Rebase { work } => rebase(state, principal, work),
    RequestBody::Advance {
      attachment,
      version,
    } => crate::merge_service::advance(state, principal, attachment, version),
    RequestBody::MapRead { volume, path, at } => {
      crate::direct_map::map_read(state, client_id, principal, volume, &path, at)
    }
    RequestBody::MapWrite {
      volume,
      attachment,
      path,
      granule,
    } => crate::direct_map::map_write(
      state,
      client_id,
      principal,
      (volume, attachment),
      (&path, granule),
    ),
    RequestBody::MapSync { volume, mapping_id } => {
      crate::direct_map::map_sync(state, client_id, principal, (volume, mapping_id))
    }
    RequestBody::ReleaseMap { mapping_id, .. } => {
      crate::direct_map::release_map(state, client_id, principal, mapping_id)
    }
    RequestBody::ShardsHolder {
      volume,
      attachment,
      pid,
      handle,
    } => crate::shards::add_holder(state, to_db_volume(volume), attachment, (pid, handle)),
    // Retired (A-267): a whole file in one reply; a file is read a page at a time.
    RequestBody::Read { .. } => refused(Refusal::Unsupported {
      feature: RETIRED_WHOLE_READ.to_owned(),
    }),
    RequestBody::ReadRange {
      volume,
      path,
      at,
      offset,
      max,
    } => crate::merge_service::read_range(state, principal, (volume, &path, at), offset, max),
    // A window for another node's read-ahead; from a client of this node, an ordinary page.
    RequestBody::ReadWindow {
      volume,
      path,
      at,
      offset,
      max,
    } => {
      // A verb forwarded from another node is dispatched with no local client (client id 0, `serve_forward`); a
      // client of this node gets an ordinary page, which its reply chunk holds.
      let from_another_node = client_id == FORWARDED_CLIENT;
      let cap = if from_another_node {
        read_window_bytes(state)
      } else {
        0
      };
      crate::merge_service::read_window(state, principal, (volume, &path, at), offset, max, cap)
    }
    RequestBody::ReadDir {
      volume,
      path,
      at,
      cursor,
    } => crate::listing::read_dir(state, principal, (volume, &path, at), cursor),
    RequestBody::StageBegin { work, len } => stage_begin(state, principal, work, len),
    RequestBody::StagePut {
      work,
      token,
      offset,
      bytes,
    } => stage_put(state, principal, (work, token), offset, &bytes),
    RequestBody::EditStaged {
      work,
      path,
      at,
      delete_len,
      token,
    } => edit_staged(state, principal, (work, token), &path, at, delete_len),
    RequestBody::Clone {
      volume,
      snapshot,
      name,
    } => clone(state, principal, volume, snapshot, &name),
    RequestBody::ImageBuild {
      volume,
      snapshot,
      name,
      capacity,
    } => crate::image::build_verb(state, principal, volume, snapshot, name, capacity),
    RequestBody::ImageMerge { image, name } => {
      crate::image::merge_verb(state, principal, image, name)
    }
    RequestBody::Checkout {
      green,
      base,
      attachment,
    } => crate::checkout::checkout_verb(state, principal, green, base, attachment),
    RequestBody::Attach {
      volume,
      snapshot,
      intent,
      form,
    } => attach(state, client_id, principal, volume, snapshot, intent, form),
    RequestBody::Detach { attachment } => detach(state, principal, attachment),
    RequestBody::Renew { attachment } => renew(state, principal, attachment),
    RequestBody::DetachOf { volume, attachment } => {
      detach_of(state, principal, Some(volume), attachment)
    }
    RequestBody::LandingEntries { landing, from } => {
      crate::landing_entries::landing_entries_verb(state, principal, landing, from)
    }
    // A peer's reap is served in `serve_forward`, under the peer's authenticated id; no client sends one.
    RequestBody::RemoteReap { .. } => refused(Refusal::BadRequest {
      reason: "a remote reap is a fleet peer's, never a client's".to_owned(),
    }),
    RequestBody::BindMount { attachment, path } => bind_mount(state, principal, attachment, &path),
    RequestBody::Resize { volume, size } => resize(state, principal, volume, size),
    RequestBody::Destroy { volume } => destroy(state, principal, volume),
    RequestBody::Status { volume } => status(state, principal, volume),
    RequestBody::Drifted { volume, after } => drifted(state, principal, volume, after),
    RequestBody::Conflicts {
      work,
      verdict,
      from,
    } => conflicts(state, principal, work, verdict, from),
    RequestBody::List { after } => list_page(vec![list_part(state, principal, after)]),
    RequestBody::DaemonStatus => {
      let mine = shard_report(state);
      daemon_report(state, vec![mine])
    }
    RequestBody::DaemonStatusNext { .. } => refused(Refusal::NotFound),
    RequestBody::Telemetry { partition } => crate::telemetry::drain_verb(state, partition),
    // `serve` routes this to the control shard (`promote_region_on_root`) before dispatch; this defensive arm
    // proposes on whatever shard reached it — correct on the control shard, refused `NotRootLeader` otherwise.
    RequestBody::PromoteRegion { region } => propose_region_promotion(state, region),
    RequestBody::Bootstrap { .. }
    | RequestBody::RecoveryPlan { .. }
    | RequestBody::Recover { .. } => refused(Refusal::ConsensusNotInitialized),
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
      signature,
    } => crate::landing::land_verb(
      state,
      client_id,
      principal,
      crate::landing::LandCall {
        volume,
        snapshot,
        target: &target,
        filter: &filter,
        grant,
        present_only: false,
        signature: signature.as_deref(),
      },
    ),
    RequestBody::Present {
      volume,
      snapshot,
      target,
      filter,
      signature,
    } => crate::landing::land_verb(
      state,
      client_id,
      principal,
      crate::landing::LandCall {
        volume,
        snapshot,
        target: &target,
        filter: &filter,
        grant: None,
        present_only: true,
        signature: signature.as_deref(),
      },
    ),
    RequestBody::FsWrite {
      volume,
      attachment,
      path,
      bytes,
      mode,
    } => crate::fs_verbs::serve(
      state,
      principal,
      (volume, attachment),
      crate::fs_verbs::FsOp::Write {
        path: &path,
        bytes: &bytes,
        mode,
      },
    ),
    RequestBody::FsWriteStaged {
      volume,
      attachment,
      path,
      token,
      mode,
    } => fs_write_staged(state, principal, (volume, attachment), &path, token, mode),
    RequestBody::FsRemove {
      volume,
      attachment,
      path,
    } => crate::fs_verbs::serve(
      state,
      principal,
      (volume, attachment),
      crate::fs_verbs::FsOp::Remove { path: &path },
    ),
    RequestBody::FsRename {
      volume,
      attachment,
      from,
      to,
    } => crate::fs_verbs::serve(
      state,
      principal,
      (volume, attachment),
      crate::fs_verbs::FsOp::Rename {
        from: &from,
        to: &to,
      },
    ),
    RequestBody::FsMkdir {
      volume,
      attachment,
      path,
      mode,
    } => crate::fs_verbs::serve(
      state,
      principal,
      (volume, attachment),
      crate::fs_verbs::FsOp::Mkdir { path: &path, mode },
    ),
    RequestBody::Grants { after } => {
      crate::landing::grants_page(vec![crate::landing::grants_part(state, principal, after)])
    }
    RequestBody::Audit { since } => crate::landing::audit_verb(state, since),
    RequestBody::Acknowledge { up_to, owed } => acknowledge(state, client_id, up_to, &owed),
    RequestBody::ReadBase {
      volume,
      path,
      offset,
    } => read_base(state, principal, volume, &path, offset),
    RequestBody::Digest { volume, path } => digest(state, principal, volume, &path),
    RequestBody::Rewitness { volume, paths } => {
      rewitness(state, principal, volume, paths.as_deref())
    }
    RequestBody::Pin { volume, paths } => pin(state, principal, volume, paths.as_deref()),
    RequestBody::Grant {
      landing,
      manifest,
      scope,
      term_ns,
      proof,
    } => crate::landing::grant_verb(state, principal, landing, manifest, scope, term_ns, proof),
    RequestBody::Enroll { account, proof } => enroll(state, principal, account, proof),
    RequestBody::SetPolicy {
      subject,
      policy,
      version,
      proof,
    } => crate::landing::set_policy_verb(state, principal, (subject, policy, version), proof),
    RequestBody::Policy { subject } => crate::landing::policy_verb(state, subject),
    RequestBody::Share {
      volume,
      principal: subject,
      rights,
    } => share(state, principal, volume, &subject, rights),
    RequestBody::LiveShip {
      volume,
      live_shipped,
    } => live_ship(state, principal, volume, live_shipped),
    // Served in `serve` as tasks that cross shards; neither reaches the dispatch.
    RequestBody::Attest { .. } | RequestBody::Revoke { .. } => refused(Refusal::BadRequest {
      reason: "attest and revoke are served on the channel".to_owned(),
    }),
  }
}

/// Enrolls a consumer under `account` (§4.13 "Principals"): the human surface's proof of issuer authority
/// verifies, a consumer id and a secret capability are minted, the hash of the capability is recorded
/// durably (never the capability), and the capability is returned once for the human to deliver to the
/// workload through the trusted harness. Refused `GrantIssuerUnverified` — counted — when the proof does
/// not verify: an agent cannot enroll itself. Refused `Forbidden` — counted `enroll_by_consumer` — on a channel bound
/// to a consumer, before anything else and whatever its proof (A-285): the launcher's authority is the instance's
/// issuer, which an agent's consumer channel never holds, so a leaked proof cannot mint the agent a second consumer
/// outside its own write policy.
fn enroll(
  state: &mut ShardState,
  principal: &Principal,
  account: u32,
  proof: Capability,
) -> ReplyBody {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("enroll_by_consumer", 1);
    return forbidden("enroll");
  }
  if !crate::landing::proven_by_launcher(
    state,
    |secret| crate::landing::enroll_proof(secret, account),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return refused(Refusal::GrantIssuerUnverified);
  }
  let mut secret = [0u8; 32];
  if slates_transport::handshake::secure_random(&mut secret).is_err() {
    return refused(Refusal::Unsupported {
      feature: "the crypto provider's secure random".to_owned(),
    });
  }
  // The consumer id: this partition's next sequence, owner-tagged like a landing id, so two shards never
  // mint the same id and the human's revocation names one consumer.
  let consumer = landing_id(state.partition, state.db.next_seq());
  let record = ConsumerRecord {
    consumer,
    account,
    secret,
    revoked: false,
    write: None,
    write_version: 0,
  };
  let now = state.clock.monotonic_ns();
  match state
    .db
    .mutate(&mut state.segment, &Op::ConsumerEnrolled { record }, now)
  {
    Ok(_) => ReplyBody::Enrolled { consumer, secret },
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Holds a consumer's channel to its own write policy (A-285): a landing with no grant is refused under a forbidding
/// land mode and answered with its presentation under an asking one (`Present`); a merge is refused under a forbidding
/// merge mode and held for approval under an asking one (`SubmitHeld`, `EditSubmitHeld`); a bypass mode passes the
/// request as it came, so the volume's own policy decides. Any other request, and an account's channel, pass.
fn consumer_gate(
  state: &mut ShardState,
  client: Handle<ClientSlot>,
  body: RequestBody,
) -> Result<RequestBody, Refusal> {
  use slates_db::catalog::WriteMode;
  use slates_ipc::protocol::{PolicySubject, WriteAct};
  let Some((consumer, policy)) =
    state
      .clients
      .get(client)
      .ok()
      .and_then(|slot| match (&slot.principal, slot.consumer_policy) {
        (Principal::Consumer { consumer, .. }, Some(policy)) => Some((*consumer, policy)),
        _ => None,
      })
  else {
    return Ok(body);
  };
  let forbids = |act| Refusal::PolicyForbids {
    act,
    by: PolicySubject::Consumer { consumer },
  };
  match body {
    RequestBody::Land {
      volume,
      snapshot,
      target,
      filter,
      grant: None,
      signature,
    } => match policy.land {
      WriteMode::Forbid => {
        state.count("policy_forbids.land", 1);
        Err(forbids(WriteAct::Land))
      }
      // A signed landing is presented as an unsigned one is, its signature judged for the approval request (A-321).
      WriteMode::Ask => Ok(RequestBody::Present {
        volume,
        snapshot,
        target,
        filter,
        signature,
      }),
      WriteMode::Bypass => Ok(RequestBody::Land {
        volume,
        snapshot,
        target,
        filter,
        grant: None,
        signature,
      }),
    },
    RequestBody::Submit { .. }
    | RequestBody::SubmitSigned { .. }
    | RequestBody::EditSubmit { .. }
      if policy.merge == WriteMode::Forbid =>
    {
      state.count("policy_forbids.merge", 1);
      Err(forbids(WriteAct::Merge))
    }
    RequestBody::Submit { work, evidence } if policy.merge == WriteMode::Ask => {
      Ok(RequestBody::SubmitHeld {
        work,
        evidence,
        signature: None,
      })
    }
    // A signed merge is held as an unsigned one is, its signature kept for the approval request (A-321).
    RequestBody::SubmitSigned {
      work,
      evidence,
      signature,
    } if policy.merge == WriteMode::Ask => Ok(RequestBody::SubmitHeld {
      work,
      evidence,
      signature: Some(signature),
    }),
    RequestBody::EditSubmit {
      work,
      path,
      at,
      delete_len,
      insert,
      evidence,
    } if policy.merge == WriteMode::Ask => Ok(RequestBody::EditSubmitHeld {
      work,
      path,
      at,
      delete_len,
      insert,
      evidence,
    }),
    other => Ok(other),
  }
}

/// Sets a consumer's write policy (A-285), served on the channel as a revocation is: refused `Forbidden` for a consumer
/// principal whatever its proof, `GrantIssuerUnverified` without the launcher's proof; then, as a task, recorded on the
/// partition the consumer id names (refused `PolicyChanged` from a version the policy has moved past), and set on every
/// shard's slots bound to the consumer before it answers, so the agent's very next request is held to it.
fn set_consumer_policy_on_channel(
  state: &mut ShardState,
  (client_index, request): (u32, u64),
  principal: &Principal,
  (consumer, policy, version): (u64, slates_ipc::protocol::WritePolicy, u64),
  proof: [u8; 32],
) -> Served {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("policy_set_by_consumer", 1);
    return Served::Reply(forbidden("set_policy"));
  }
  let subject = slates_ipc::protocol::PolicySubject::Consumer { consumer };
  if !crate::landing::proven_by_launcher(
    state,
    |secret| crate::landing::policy_proof(secret, &subject, &policy, version),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return Served::Reply(refused(Refusal::GrantIssuerUnverified));
  }
  // Signed merges are a volume's requirement (A-321): what a green accepts, or what a work's merges must carry, judged
  // where the merge's verdict is, which holds both volumes' records and neither session's. A launcher holds an agent to
  // it by setting it on the agent's work; a consumer's policy is refused it, rather than keep a rule nothing reads.
  if policy.require_signed_merges {
    return Served::Reply(refused(Refusal::BadRequest {
      reason: "signed merges are required by a volume's policy, not a consumer's".to_owned(),
    }));
  }
  let origin = state.shard;
  let Some(owner) = shard_of_partition(state, owner_of_consumer(consumer)) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let shards = state.shards.clone();
  let recorded = crate::landing::db_policy(policy);
  let task = slates_rt::futures::spawn(async move {
    let reply =
      set_consumer_policy_everywhere(origin, owner, &shards, (consumer, recorded, version)).await;
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  // Freshly admitted on this shard, with no await before detaching: the slot is still live.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// The consumer policy's two bounded steps, as a revocation's: the durable record on `owner`, then every shard's bound
/// slots. Each call is waited for in its target's own time, up to the liveness budget of its CPU; one that does not
/// answer refuses `NotFound`, and the record, if written, stands for the retry.
async fn set_consumer_policy_everywhere(
  origin: u16,
  owner: u16,
  shards: &[u16],
  (consumer, policy, version): (u64, slates_db::catalog::WritePolicy, u64),
) -> ReplyBody {
  let recorded = crate::xshard::call_in_shard_time(
    origin,
    owner,
    move |s| {
      let current = s
        .db
        .partition()
        .consumer(consumer)
        .ok_or(Refusal::NotFound)?
        .write_version;
      if current != version {
        s.count("policy_changed", 1);
        return Err(Refusal::PolicyChanged { current });
      }
      let now = s.clock.monotonic_ns();
      s.db
        .mutate(
          &mut s.segment,
          &Op::ConsumerWritePolicySet { consumer, policy },
          now,
        )
        .map(|_| ())
        .map_err(|e| refusal_of_db(&e))
    },
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  match recorded {
    None => return refused(Refusal::NotFound),
    Some(Err(refusal)) => return refused(refusal),
    Some(Ok(())) => {}
  }
  for shard in shards.iter().copied() {
    let marked = crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |s| {
        let bound: Vec<Handle<ClientSlot>> = s
          .clients
          .iter()
          .filter(|(_, slot)| matches!(slot.principal, Principal::Consumer { consumer: c, .. } if c == consumer))
          .map(|(h, _)| h)
          .collect();
        for handle in bound {
          if let Ok(slot) = s.clients.get_mut(handle) {
            slot.consumer_policy = Some(policy);
          }
        }
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    if marked.is_none() {
      return refused(Refusal::NotFound);
    }
  }
  ReplyBody::PolicySet
}

/// Reads a consumer's write policy and its version (A-285), served on the channel as a task: the record lives on the
/// partition the consumer id names.
fn consumer_policy_on_channel(
  state: &mut ShardState,
  (client_index, request): (u32, u64),
  consumer: u64,
) -> Served {
  let origin = state.shard;
  let Some(owner) = shard_of_partition(state, owner_of_consumer(consumer)) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let task = slates_rt::futures::spawn(async move {
    let read = crate::xshard::call_in_shard_time(
      origin,
      owner,
      move |s| {
        s.db
          .partition()
          .consumer(consumer)
          .map(|r| (r.write, r.write_version))
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let reply = match read {
      Some(Some((policy, version))) => ReplyBody::Policy {
        policy: policy.map(crate::landing::wire_policy),
        version,
        signers: Vec::new(),
      },
      _ => refused(Refusal::NotFound),
    };
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// Revokes a consumer's enrollment (§4.13 "every later effect from a channel bound to it refuses
/// `ConsumerRevoked`"). The proof of issuer authority is checked first, here, on the channel's own shard
/// (every shard holds the issuer secret): a forged proof refuses `GrantIssuerUnverified`, counted, with no
/// cross-shard work. A verified revocation runs as a task: it records the revocation durably on the
/// partition the consumer id names, then marks every shard's slots bound to the consumer — each step a
/// bounded [`crate::xshard::call_within`] — and **only then** delivers `Revoked`. "Later" means later than
/// the acknowledged revocation: a `run_on` fan-out is a sent message the workload's next request can
/// overtake (measured: the very next `Status` after `Revoked` was served), so the acknowledgement waits for
/// every mark. A shard that does not answer within the liveness budget makes the revocation refuse
/// `NotFound` rather than acknowledge a revocation not yet in force everywhere; the durable record stands,
/// so the human retries and the retry is a no-op mark. Refused `Forbidden` — counted `revoke_by_consumer` — on a channel
/// bound to a consumer, before anything else and whatever its proof (A-285): revocation is the launcher's, so an agent
/// holding a leaked proof cannot revoke another agent's consumer.
fn revoke_on_channel(
  state: &mut ShardState,
  (client_index, request): (u32, u64),
  principal: &Principal,
  consumer: u64,
  proof: Capability,
) -> Served {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("revoke_by_consumer", 1);
    return Served::Reply(forbidden("revoke"));
  }
  if !crate::landing::proven_by_launcher(
    state,
    |secret| crate::landing::revoke_proof(secret, consumer),
    &proof,
  ) {
    state.count("grant_issuer_unverified", 1);
    return Served::Reply(refused(Refusal::GrantIssuerUnverified));
  }
  let origin = state.shard;
  let Some(owner) = shard_of_partition(state, owner_of_consumer(consumer)) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let shards = state.shards.clone();
  let task = slates_rt::futures::spawn(async move {
    let reply = revoke_everywhere(origin, owner, &shards, consumer).await;
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  // Freshly admitted on this shard, with no await before detaching: the slot is still live.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// The revocation's two bounded steps: the durable record on `owner`, then the mark on every shard.
/// Returns the reply to deliver. Each call is waited for in its target's own time, up to the liveness budget of its CPU
/// (`xshard::call_in_shard_time`); one whose target made no progress for a window, or ran the budget without answering,
/// refuses `NotFound` (the record, if written, stands for the retry). R3 class A: before, a shard given a fifth of a
/// core for 1.5 s was refused at the wall second.
async fn revoke_everywhere(origin: u16, owner: u16, shards: &[u16], consumer: u64) -> ReplyBody {
  let recorded = crate::xshard::call_in_shard_time(
    origin,
    owner,
    move |s| {
      let now = s.clock.monotonic_ns();
      s.db
        .mutate(&mut s.segment, &Op::ConsumerRevoked { consumer }, now)
        .map(|_| ())
        .map_err(|e| refusal_of_db(&e))
    },
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  match recorded {
    None => return refused(Refusal::NotFound),
    Some(Err(refusal)) => return refused(refusal),
    Some(Ok(())) => {}
  }
  let mut fenced = Vec::new();
  for shard in shards.iter().copied() {
    let marked = crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |s| mark_revoked(s, consumer),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match marked {
      None => return refused(Refusal::NotFound),
      Some(Err(refusal)) => return refused(refusal),
      Some(Ok(mounts)) if mounts.is_empty() => {}
      Some(Ok(mounts)) => fenced.push((shard, mounts)),
    }
  }
  revoked_mounts_ended(origin, fenced).await;
  ReplyBody::Revoked
}

/// Waits until every FUSE mount the revocation fenced (each shard's, by attachment and volume) has had its kernel drop
/// what it cached and ended (AUD-29-84; `crate::fuse::fence_if_mounted`), so no process reads, after the revocation is
/// acknowledged, a page the consumer's kernel held. Each shard is waited on for at most [`crate::told::TOLD_WAIT_NS`],
/// the bound a writer waits for the same kernels to be told of a change; one past it is counted and the revocation
/// acknowledged anyway, since every request of the mount is already refused.
#[cfg(target_os = "linux")]
async fn revoked_mounts_ended(origin: u16, fenced: Vec<(u16, Vec<(u64, DbVolumeId)>)>) {
  for (shard, mounts) in fenced {
    let ended = crate::xshard::task_within(
      origin,
      shard,
      move || crate::fuse::revoked_mounts_ended(mounts),
      crate::told::TOLD_WAIT_NS,
    )
    .await;
    if ended.is_none() {
      let _ = crate::state::with_state_counted(|s| s.count(REVOKED_MOUNTS_OVERDUE, 1));
    }
  }
}

/// Elsewhere no mount is fenced this way: nothing to wait for.
#[cfg(not(target_os = "linux"))]
async fn revoked_mounts_ended(_origin: u16, _fenced: Vec<(u16, Vec<(u64, DbVolumeId)>)>) {}

/// Marks every slot on this shard bound to `consumer` revoked, so its next verb's gate — one local read
/// in `serve` — refuses before any effect (banned item 10: no cross-shard call on a write path).
/// Returns the FUSE mounts the ending fenced, each by attachment and volume: they end once their kernels have dropped
/// what they cached, which the revocation awaits (`revoked_mounts_ended`).
fn mark_revoked(s: &mut ShardState, consumer: u64) -> Result<Vec<(u64, DbVolumeId)>, Refusal> {
  let bound: Vec<Handle<ClientSlot>> = s
    .clients
    .iter()
    .filter(|(_, slot)| matches!(slot.principal, Principal::Consumer { consumer: c, .. } if c == consumer))
    .map(|(h, _)| h)
    .collect();
  for handle in bound {
    if let Ok(slot) = s.clients.get_mut(handle) {
      slot.revoked = true;
    }
  }
  // The consumer's guest devices on this shard stop at their next pass boundary (AUD-29-73).
  #[cfg(unix)]
  crate::virtiofs::revoke_consumer_devices(s, consumer);
  // The consumer's attachments on this shard's partition end with its channels (AUD-29-84): each is a
  // capability that outlives the channel — a host mount's token, a FUSE mount, the record a container
  // binding is held to — so each is ended as a recorded operation, its mount capability then reaching nothing
  // and its mount unmounted. An attachment that cannot be ended refuses the revocation, never acknowledges one
  // not in force; the retry ends what is left. A host account's own attachments are not the consumer's.
  let held: Vec<AttachmentRecord> = s
    .db
    .partition()
    .attachments_held_by_consumer(consumer)
    .into_iter()
    .cloned()
    .collect();
  for record in &held {
    end_attachment(s, record, Ending::Otherwise).map_err(|e| refusal_of_db(&e))?;
  }
  Ok(fenced_mounts(s, &held))
}

/// The FUSE mounts of `ended` attachments still served on this shard: fenced, and ending once their kernels have dropped
/// what they cached (`crate::fuse::fence_if_mounted`).
#[cfg(target_os = "linux")]
fn fenced_mounts(s: &ShardState, ended: &[AttachmentRecord]) -> Vec<(u64, DbVolumeId)> {
  ended
    .iter()
    .filter(|record| s.fuse_mounts.contains_key(&record.id))
    .map(|record| (record.id, record.volume))
    .collect()
}

/// Elsewhere there is no FUSE mount.
#[cfg(not(target_os = "linux"))]
fn fenced_mounts(_s: &ShardState, _ended: &[AttachmentRecord]) -> Vec<(u64, DbVolumeId)> {
  Vec::new()
}

/// Sets `subject`'s rights on `volume` (§4.13 "Access lists": `admin` covers changing the list; the owner
/// holds every right). All-false rights remove the entry; the owner's own rights are not an entry and
/// cannot be reduced here.
fn share(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  subject: &slates_ipc::protocol::Principal,
  rights: slates_ipc::protocol::Rights,
) -> ReplyBody {
  // The record, not the volume's slot: a green has no slot (its engine is `state.greens`), and its access list is
  // what `merge_service::require_green` authorizes by, so a green is shared like any volume (§4.13, §4.16).
  let record = match crate::merge_service::find_record(state, volume) {
    Ok(record) => record,
    Err(Refusal::NotFound) => return refused(absent(state, volume)),
    Err(refusal) => return refused(refusal),
  };
  if !rights_of(&record, principal).admin {
    return forbidden("share");
  }
  let subject = to_db_principal(subject);
  let rights = to_db_rights(rights);
  let mut access: Vec<AccessEntry> = record
    .access
    .iter()
    .filter(|entry| entry.principal != subject)
    .cloned()
    .collect();
  if rights.read || rights.write || rights.admin {
    access.push(AccessEntry {
      principal: subject,
      rights,
    });
  }
  let now = state.clock.monotonic_ns();
  match state.db.mutate(
    &mut state.segment,
    &Op::AccessChanged {
      id: to_db_volume(volume),
      access,
    },
    now,
  ) {
    Ok(_) => ReplyBody::Shared,
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Turns a volume's live shipping on or off (§4.10, A-200), as one recorded operation on its owner partition; `admin`
/// on the volume is required, as for its access list. The flag rides the volume's catalog register, so a successor
/// keeps shipping.
fn live_ship(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  live_shipped: bool,
) -> ReplyBody {
  let (_, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).admin {
    return forbidden("live-ship");
  }
  let now = state.clock.monotonic_ns();
  match state.db.mutate(
    &mut state.segment,
    &Op::LiveShippingChanged {
      id: to_db_volume(volume),
      live_shipped,
    },
    now,
  ) {
    Ok(_) => ReplyBody::LiveShippingSet { live_shipped },
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Format: a consumer's secret capability is a BLAKE3 key, the same width as the issuer secret
/// ([`slates_anchor::layout::ISSUER_SECRET_BYTES`]) — one width for every keyed proof in §4.13.
type Capability = [u8; slates_anchor::layout::ISSUER_SECRET_BYTES];

/// What an attestation is checked against, read from the consumer's owner partition (§4.13): the
/// account the consumer was enrolled under, the secret capability its proofs are keyed with, and whether
/// a human has revoked it. `None` when no such consumer was ever enrolled there.
type ConsumerFacts = Option<(u32, Capability, bool)>;

/// The pure decision of an attestation (§4.13 "a consumer channel is bound at rendezvous using a
/// capability delivered and retained outside other agents' reach"): the channel's principal must be the
/// account the consumer was enrolled under (a consumer is a workload *within* an account, never a way
/// across accounts), the consumer must not be revoked, and the proof — the capability keyed over this
/// channel's client id — must verify, so a proof captured from another session does not bind this one.
/// Cfg-free and side-effect-free, so it is tested on every host without a daemon.
pub fn verify_attestation(
  channel_principal: &Principal,
  client_id: u32,
  facts: ConsumerFacts,
  proof: &Capability,
) -> Result<u32, Refusal> {
  let Some((account, secret, revoked)) = facts else {
    return Err(Refusal::ConsumerNotEnrolled);
  };
  if revoked {
    return Err(Refusal::ConsumerRevoked);
  }
  let same_account = matches!(channel_principal, Principal::Uid { uid, .. } if *uid == account);
  let expected = crate::landing::attest_proof(&secret, client_id);
  if !same_account || !crate::landing::constant_time_eq(&expected, proof) {
    return Err(Refusal::ConsumerNotEnrolled);
  }
  Ok(account)
}

/// The counter name a refusal of an attestation is counted under (§4.13 "refusals are counted, never
/// logged with content").
fn attest_refusal_counter(refusal: &Refusal) -> &'static str {
  match refusal {
    Refusal::ConsumerRevoked => "consumer_revoked",
    _ => "consumer_not_enrolled",
  }
}

/// Binds the channel at `client_index` to the enrolled consumer it attests (§4.13). The consumer's record
/// lives on the partition its id names ([`owner_of_consumer`] — ids route to owners, D-14), so the check
/// runs as a task on this shard that reads the record there ([`crate::xshard::call_within`], bounded by
/// the liveness budget), decides ([`verify_attestation`]), binds the slot here, and delivers the reply —
/// the shape of [`promote_region_on_root`]. No cross-shard call is on any later verb's path: after the
/// bind, the channel's principal *is* the consumer and every right is checked locally against it.
/// Refused `ConsumerNotEnrolled` or `ConsumerRevoked`, each counted where it was decided; an owner
/// partition that does not answer within the budget refuses `NotFound` rather than binding blind.
fn attest_on_channel(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  client_id: u32,
  consumer: u64,
  proof: Capability,
) -> Served {
  let origin = state.shard;
  // The record's partition, mapped to the runtime shard that holds it in this process: `call_within`
  // addresses shards, the id names a partition (the two differ — a shard id is the runtime's).
  let Some(owner) = shard_of_partition(state, owner_of_consumer(consumer)) else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let Some(channel_principal) = state
    .clients
    .iter()
    .find(|(h, _)| h.index() == client_index)
    .map(|(_, slot)| slot.principal.clone())
  else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  let task = slates_rt::futures::spawn(async move {
    // Waited for in the record's shard's own time (R3 class A): a starved shard's record is read, not taken for absent.
    let fetched = crate::xshard::call_in_shard_time(
      origin,
      owner,
      move |s| {
        s.db
          .partition()
          .consumer(consumer)
          .map(|r| ((r.account, r.secret, r.revoked), r.write))
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    let (facts, policy): (Option<ConsumerFacts>, _) = match fetched {
      None => (None, None),
      Some(None) => (Some(None), None),
      Some(Some((facts, policy))) => (Some(Some(facts)), policy),
    };
    let reply = match facts {
      None => refused(Refusal::NotFound),
      Some(facts) => match verify_attestation(&channel_principal, client_id, facts, &proof) {
        Ok(account) => {
          // Bind the slot on the origin shard, by index: the channel's principal becomes the consumer.
          let bound = crate::xshard::run_on(origin, origin, move |s| {
            let handle = s
              .clients
              .iter()
              .find(|(h, _)| h.index() == client_index)
              .map(|(h, _)| h);
            if let Some(slot) = handle.and_then(|h| s.clients.get_mut(h).ok()) {
              // The consumer of this host's account: the slot and its channel are this node's (A-237).
              slot.principal = Principal::Consumer {
                host: s.origin_anchor,
                account,
                consumer,
              };
              slot.consumer_policy = policy;
            }
          })
          .is_ok();
          if bound {
            ReplyBody::Attested
          } else {
            refused(Refusal::NotFound)
          }
        }
        Err(refusal) => {
          let counter = attest_refusal_counter(&refusal);
          crate::xshard::run_on_counted(origin, origin, move |s| {
            s.count(counter, 1);
          });
          refused(refusal)
        }
      },
    };
    crate::state::deliver(client_index, request, reply, false);
  });
  let Ok(task) = task else {
    return Served::Reply(refused(Refusal::NotFound));
  };
  // Freshly admitted on this shard, with no await before detaching: the slot is still live.
  let _ = slates_rt::futures::detach(task);
  Served::Forwarded
}

/// The refusal for a volume this shard serves no slot of. An object this node holds a copy of (a recovery
/// candidate) but has not yet materialized is NOT absent: it answers a retryable `Overloaded`, never `NotFound`, so
/// a read in the takeover settle window — before this node has processed the retirement and recognized its
/// ownership (`resolve_held_objects`) — is retried, not told the volume is gone (Bug 1,
/// docs/bugs/2026-10-11-taken-over-scratch-volumes-never-become-servable.md).
fn absent(state: &ShardState, volume: VolumeId) -> Refusal {
  if state.holder_records.contains_key(&ObjectId(volume.bytes)) {
    let shard = state
      .shards
      .get(usize::from(owner_of(volume)))
      .copied()
      .unwrap_or(state.shard);
    Refusal::Overloaded { shard }
  } else {
    Refusal::NotFound
  }
}

/// The volume slot and its record, or the refusal.
pub(crate) fn find(
  state: &ShardState,
  volume: VolumeId,
) -> Result<(Handle<VolumeSlot>, VolumeRecord), Box<ReplyBody>> {
  let id = to_db_volume(volume);
  let handle = *state.by_id.get(&id).ok_or_else(|| {
    // A volume recovery could not rebuild is listed, so it answers why, never `NotFound`.
    Box::new(refused(match state.unrecovered.get(&id) {
      Some(cause) => Refusal::NotRecovered {
        cause: cause.clone(),
      },
      None => absent(state, volume),
    }))
  })?;
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

/// The journal's floor (§4.5, A-157): one page, reserved with the volume's records at admission. Above it the
/// journal grows from the shard's metadata ledger, as far as a live reader's backlog needs and no further, so its
/// budget is the formula's product measured at the moment of need: the mutation rate times a reader's lag times the
/// record size. Until 2026-10-23 every volume reserved a fixed 10‰ of its quota here, whatever it needed. Shared by
/// creation and recovery: a rebuilt journal starts empty, at the floor.
fn journal_floor_bytes(state: &ShardState) -> usize {
  derived!(
    usize::try_from(state.config.geometry.page)
      .unwrap_or(1)
      .max(1),
    "one page: the journal's floor, which it grows from as a live reader needs",
    ["page"]
  )
  .get()
}

/// A volume's inode allowance (§4.2 resource vector): its quota's share of the shard's version slab, the quota over
/// the bytes per inode the shard's layout gives (its arena over its slab, never less than an inode's size), clamped to
/// the slab. It scales with the policy the caller asked for, and bounded volumes whose quotas fill the arena are
/// allowed the slab between them, so the bytes, not the slots, bound how many a shard holds. A fixed disjoint
/// per-volume reservation is the fuller §4.2 refinement (docs/wip/resource-vector.md).
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
  // The bytes of quota one inode is allowed per (ext4's bytes-per-inode ratio, `mke2fs -i`): the shard's arena over
  // the slab it can back, so the allowances of bounded volumes that fill the arena fill the slab and no more. One
  // inode per `size_of::<Inode>()` let a quarter of a shard's bytes take its whole slab, and the next volume was
  // refused with the rest of the bytes uncommitted (2026-10-06: 256 MiB took 1,140,937 of 1,140,938 slots).
  let per_inode = state
    .config
    .reserve_per_shard
    .div_ceil(cap)
    .max(inode_bytes);
  derived!(
    limit.checked_div(per_inode).unwrap_or(0).min(cap).max(1),
    "min(quota / max(reserve_per_shard / slab, size_of::<Inode>), store.max_inodes − copy-up headroom), at least one",
    ["quota", "reserve_per_shard", "store.max_inodes", "vfs.copy_up_version_headroom"]
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

pub(crate) fn entry_allowance(size: SizeClass) -> u64 {
  let limit = match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  };
  let entry_bytes = u64::try_from(size_of::<slates_vfs::dir::Child>())
    .unwrap_or(1)
    .max(1);
  derived!(
    limit.checked_div(entry_bytes).unwrap_or(0).max(1),
    "quota / size_of::<Child>() (minimum entry footprint)",
    ["quota"]
  )
  .get()
}

pub(crate) fn volume_config(
  state: &mut ShardState,
  names: NamePolicy,
  quota: Quota,
) -> VolumeConfig {
  let prefix = state.next_prefix;
  state.next_prefix = state.next_prefix.wrapping_add(1).max(1);
  let journal_bytes = journal_floor_bytes(state);
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

pub(crate) fn quota_for(size: SizeClass) -> Quota {
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

pub(crate) fn wire_names(names: DbNamePolicy) -> NamePolicy {
  match names {
    DbNamePolicy::Exact => NamePolicy::Exact,
    DbNamePolicy::Fold => NamePolicy::Fold,
  }
}

fn wire_size(size: DbSizeClass) -> SizeClass {
  match size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  }
}

pub(crate) fn db_size(size: SizeClass) -> DbSizeClass {
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
  let Admitted {
    volume,
    host,
    credits,
  } = match admit(state, principal, size, names, require_locked, base) {
    Ok(admitted) => admitted,
    Err(reply) => return *reply,
  };
  let Credits {
    reservation,
    version: version_credit,
    metadata: metadata_credit,
    lock: lock_credit,
  } = credits;
  let Some(id) = fresh_volume_id(state) else {
    let hold = volume.version_hold();
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    release_version_hold(&mut state.store, hold);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      ids_exhausted(),
    );
  };
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
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
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
    catalog_version: 0,
    taken_over: None,
  };
  publish_created_volume(
    state,
    id,
    volume,
    host,
    (reservation, version_credit, metadata_credit, lock_credit),
    record,
  )
}

/// What a volume holds against the shard's ledgers from its admission to its teardown (§4.2): a bounded volume's byte
/// reservation, its inode allowance against the version slab, its records against the metadata ledger, and a strict
/// volume's locked entitlement.
pub(crate) struct Credits {
  pub(crate) reservation: Option<slates_mem::budget::Reservation>,
  pub(crate) version: Option<slates_mem::budget::VersionCredit>,
  pub(crate) metadata: Option<slates_mem::budget::MetadataCredit>,
  pub(crate) lock: Option<crate::lock_ledger::LockCredit>,
}

/// A volume admitted and made, not yet recorded or in the registry: the volume, an overlay's read-only host, and its
/// credits.
pub(crate) struct Admitted {
  pub(crate) volume: Volume,
  pub(crate) host: Option<OsHost>,
  pub(crate) credits: Credits,
}

/// Admits a new volume of `size` for `principal` (§4.2: every credit reserved, or none, before anything is published)
/// and makes it, over `base` for an overlay, with its root owned by the principal and its dimensions admitted. Not
/// recorded: [`create`] records it at once, and an image build once its image is written (A-160). A refusal gives
/// back what was reserved.
pub(crate) fn admit(
  state: &mut ShardState,
  principal: &Principal,
  size: SizeClass,
  names: NamePolicy,
  require_locked: bool,
  base: Option<&str>,
) -> Result<Admitted, Box<ReplyBody>> {
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        report_first_budget_refusal(state, "bytes", limit, available);
        return Err(Box::new(refused(Refusal::BudgetExceeded { available })));
      }
      Err(e) => {
        return Err(Box::new(refused(refusal_of_mem(&e))));
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  // Reserve a bounded claim's inode allowance against the shard's version slab *before* creating the volume (§4.2:
  // "admission reserves all required credits or none before publishing"). So the advertised allowance is backed by
  // real slab capacity — two claims cannot each be promised the same slots — and a refusal here allocates nothing to
  // leak (no root inode, trie or dir), giving back only the byte reservation. A dynamic volume reserves nothing: its
  // allowance is a ceiling, and it holds slots for its live inodes from the unpromised capacity as it makes them
  // ([`Volume::hold_versions`] below; A-342), as its bytes grow — one big maximum promised the whole slab to a volume
  // holding 54 bytes (docs/bugs/2026-11-18-a-dynamic-volume-reserved-the-whole-version-slab.md).
  let inode_alloc = inode_allowance(state, size);
  // A dynamic volume's first slots are its root's, taken here, before the volume is made, as a claim's reservation
  // is, so a refusal leaves no root inode, trie or dir allocated; they become its hold once it is made (below).
  let (wanted, taken) = match size {
    SizeClass::Bounded { .. } => (inode_alloc, state.store.versions.reserve(inode_alloc)),
    SizeClass::Dynamic { .. } => (
      slates_vfs::volume::NEW_VOLUME_INODES,
      state
        .store
        .versions
        .grow(slates_vfs::volume::NEW_VOLUME_INODES),
    ),
  };
  let version_credit = match taken {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      report_first_budget_refusal(state, "versions", wanted, available);
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BudgetExceeded { available },
      )));
    }
    Err(e) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        refusal_of_mem(&e),
      )));
    }
  };
  // The volume's records — its journal budget, its object, its snapshot slab's first segment — are
  // reserved against the shard's metadata ledger before anything is allocated (§4.2 metadata
  // dimension: "an uncharged heap allocation cannot sit outside the bound").
  let quota = quota_for(size);
  let journal_bytes = journal_floor_bytes(state);
  let metadata_credit = match reserve_metadata(state, journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        None,
        refusal,
      )));
    }
  };
  // A strict volume's content lives in locked RAM (§4.2 D-12, BUG-1): its whole entitlement is reserved against the
  // process's lock capacity now, refused `BudgetExceeded` if it does not fit, so a strict guarantee never silently
  // becomes swappable service; its blocks are then locked as it allocates them, and only its blocks.
  let lock_credit = match require_locked
    .then(|| reserve_locked(state, size))
    .transpose()
  {
    Ok(credit) => credit,
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal,
      )));
    }
  };
  let config = volume_config(state, names, quota);
  let (mut volume, host) = match base {
    None => match Volume::create(&mut state.store, config) {
      Ok(v) => (v, None),
      Err(e) => {
        return Err(Box::new(give_back(
          state,
          reservation,
          version_credit,
          metadata_credit,
          refusal_of_vfs(&e),
        )));
      }
    },
    Some(path) => match open_base(state, path, config) {
      Ok(pair) => pair,
      Err(reply) => {
        return Err(Box::new(give_back(
          state,
          reservation,
          version_credit,
          metadata_credit,
          reply_refusal(*reply),
        )));
      }
    },
  };
  volume.set_locked(require_locked);
  // The volume root is owned by its provisioning user (§4.13 "runs as the mounting user"; the
  // root:wheel sibling, docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md): the volume core
  // births it uid 0, gid 0, which a mount showed as root:wheel — and which the NFS export's POSIX
  // permission checks would now refuse the mounting user its own volume's root.
  if let Err(e) = stamp_root_owner(&mut state.store, &mut volume, principal) {
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  // Admit the volume's inode dimension (§4.2 resource vector): set the per-volume cap that
  // `next_no` enforces, to the same allowance already reserved against the version slab above.
  if let Err(e) = admit_dimensions(state, &mut volume, size) {
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  // A dynamic volume's root slot becomes its hold (A-342), which grows as it makes inodes and goes back with it at
  // teardown, never a reserved credit.
  let version_credit = match version_credit {
    Some(credit) if matches!(size, SizeClass::Dynamic { .. }) => {
      volume.adopt_version_hold(credit.slots);
      None
    }
    credit => credit,
  };
  Ok(Admitted {
    volume,
    host,
    credits: Credits {
      reservation,
      version: version_credit,
      metadata: metadata_credit,
      lock: lock_credit,
    },
  })
}

/// Returns a dynamic volume's version hold (A-342) to the shard's version budget: what it took for its inodes, released
/// whole when the volume is torn down or discarded, as its byte growth is.
fn release_version_hold(store: &mut slates_vfs::volume::Store, hold: u64) {
  if hold > 0 {
    store
      .versions
      .release(slates_mem::budget::VersionCredit { slots: hold });
  }
}

/// Discards an admitted volume that will not be recorded: its blocks back to the store, its credits back to their
/// ledgers (a strict volume's locked entitlement with its credit's drop).
pub(crate) fn discard_admitted(state: &mut ShardState, admitted: Admitted) {
  let Admitted {
    volume, credits, ..
  } = admitted;
  let hold = volume.version_hold();
  let discarded = volume.discard_partial(&mut state.store);
  release_version_hold(&mut state.store, hold);
  count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
  release_credits(
    state,
    credits.reservation,
    credits.version,
    credits.metadata,
  );
}

/// Records a freshly-created volume and moves it into the shard's registry, or gives its credits back
/// and discards the volume (returning its slab slots) if the record cannot be written or the registry
/// has no room. The registry room is checked before the insert, since a full registry's `insert`
/// consumes and drops the slot.
pub(crate) fn publish_created_volume(
  state: &mut ShardState,
  id: slates_db::catalog::VolumeId,
  volume: Volume,
  host: Option<OsHost>,
  credits: (
    Option<slates_mem::budget::Reservation>,
    Option<slates_mem::budget::VersionCredit>,
    Option<slates_mem::budget::MetadataCredit>,
    Option<crate::lock_ledger::LockCredit>,
  ),
  record: VolumeRecord,
) -> ReplyBody {
  let (reservation, version_credit, metadata_credit, lock_credit) = credits;
  // Capture the mount name before the record is moved into the log op below; the slot lists the
  // volume under it in the host root (a client mounts `/<name>` or reaches it by `cd <name>`).
  let name = record.name.clone();
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    let hold = volume.version_hold();
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    release_version_hold(&mut state.store, hold);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_db(&e),
    );
  }
  if !state.volumes.has_room() {
    let full = state.volumes.max_slots();
    let hold = volume.version_hold();
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    release_version_hold(&mut state.store, hold);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_mem(&slates_mem::MemError::SlabFull { capacity: full }),
    );
  }
  let mut volume = volume;
  crate::content_cipher::key_volume(&mut state.store, &mut volume, id.bytes);
  let hold = volume.version_hold();
  let slot = VolumeSlot {
    id,
    name,
    volume,
    host,
    reservation,
    version_credit,
    metadata_credit,
    lock_credit,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      ReplyBody::Created {
        id: to_wire_volume(id),
      }
    }
    // Unreachable given the room check above; the slot was consumed, so only the credits and the hold return.
    Err(e) => {
      release_version_hold(&mut state.store, hold);
      give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal_of_mem(&e),
      )
    }
  }
}

pub(crate) fn reply_refusal(reply: ReplyBody) -> Refusal {
  match reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::BadRequest {
      reason: "not a refusal".to_owned(),
    },
  }
}

/// The bytes a strict volume of `size` may come to hold: its bound, or its dynamic maximum.
fn locked_entitlement(size: SizeClass) -> u64 {
  match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  }
}

/// Reserves a strict volume's entitlement against the daemon's lock capacity (§4.2 D-12, `lock_ledger`), or the
/// lock-capacity refusal with what is still available.
fn reserve_locked(
  state: &ShardState,
  size: SizeClass,
) -> Result<crate::lock_ledger::LockCredit, Refusal> {
  let control = state.shards.first().copied().unwrap_or(state.shard);
  crate::lock_ledger::LockCredit::reserve(
    control,
    locked_entitlement(size),
    state.config.lock_capacity_bytes,
  )
  .map_err(|available| Refusal::BudgetExceeded { available })
}

fn give_back(
  state: &mut ShardState,
  reservation: Option<slates_mem::budget::Reservation>,
  version: Option<slates_mem::budget::VersionCredit>,
  metadata: Option<slates_mem::budget::MetadataCredit>,
  refusal: Refusal,
) -> ReplyBody {
  release_credits(state, reservation, version, metadata);
  refused(refusal)
}

/// Returns a volume's byte reservation, inode allowance and metadata reservation to the shard's ledgers.
fn release_credits(
  state: &mut ShardState,
  reservation: Option<slates_mem::budget::Reservation>,
  version: Option<slates_mem::budget::VersionCredit>,
  metadata: Option<slates_mem::budget::MetadataCredit>,
) {
  if let Some(r) = reservation {
    state.store.budget.release(r);
  }
  if let Some(c) = version {
    state.store.versions.release(c);
  }
  if let Some(m) = metadata {
    state.store.metadata.release(m);
  }
}

/// Reserves a volume's records against the shard's metadata ledger (§4.2 metadata dimension) before
/// the volume exists: its journal's whole retention budget, its object and its snapshot slab's first
/// segment, whole or not at all, so the sum of every volume's metadata stays inside the class.
pub(crate) fn reserve_metadata(
  state: &mut ShardState,
  journal_bytes: usize,
) -> Result<slates_mem::budget::MetadataCredit, Refusal> {
  let page = usize::try_from(state.config.geometry.page).unwrap_or(1);
  match state
    .store
    .metadata
    .reserve(Volume::metadata_footprint(journal_bytes, page))
  {
    Ok(credit) => Ok(credit),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      Err(Refusal::BudgetExceeded { available })
    }
    Err(e) => Err(refusal_of_mem(&e)),
  }
}

/// Why the base directory at `path` could not be opened, as a create (and a clone of an overlay) is refused for it.
fn base_open_refusal(path: &str, error: slates_vfs::host::HostError) -> Refusal {
  match error {
    // A-227: a base on one of this daemon's own mounts, refused before anything opened it.
    slates_vfs::host::HostError::OwnMount(_) => Refusal::OwnMount {
      path: path.to_owned(),
      used_as: slates_ipc::protocol::OwnMountUse::Base,
    },
    slates_vfs::host::HostError::Unavailable(code) => Refusal::BaseUnavailable {
      path: path.to_owned(),
      errno: code,
    },
    _ => Refusal::BaseUnavailable {
      path: path.to_owned(),
      errno: 0,
    },
  }
}

/// The host a clone of `record`'s overlay (in slot `origin`) reads its base through (§4.4 clone, AC-1.16): a duplicate of
/// every directory handle the origin's host keeps, under the same handles (`OsHost::try_clone_dirs`), so each base
/// directory the clone's plane names is the very directory the origin's names, whatever an outsider has since put at
/// its path. `None` for a volume with no base directory (scratch, image). Before 2026-11-18 every clone was given no
/// host, so an overlay's clone could not image its base and the clone verb was refused `ContentUnavailable` (GAPS
/// 2026-11-05, "The daemon cannot clone an overlay volume").
fn clone_base_host(
  state: &ShardState,
  origin: Handle<VolumeSlot>,
  record: &VolumeRecord,
) -> Result<Option<OsHost>, Refusal> {
  let BaseRecord::Path { path } = &record.base else {
    return Ok(None);
  };
  let slot = state.volumes.get(origin).map_err(|_| Refusal::NotFound)?;
  let Some(host) = slot.host.as_ref() else {
    return Err(Refusal::BaseUnavailable {
      path: path.clone(),
      errno: 0,
    });
  };
  host
    .try_clone_dirs()
    .map(Some)
    .map_err(|error| base_open_refusal(path, error))
}

/// Opens a base directory (read-only, `O_NOFOLLOW`) and creates the overlay over it.
fn open_base(
  state: &mut ShardState,
  path: &str,
  config: VolumeConfig,
) -> Result<(Volume, Option<OsHost>), Box<ReplyBody>> {
  let (mut host, root) = OsHost::open_root(std::path::Path::new(path))
    .map_err(|e| Box::new(refused(base_open_refusal(path, e))))?;
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

/// A snapshot of `volume` (§4.4 `snapshot`). A guest's DAX windows need nothing of it: each maps its file's backing, an
/// object the write boundary keeps equal to the head, never the shared content a snapshot freezes (A-168, A-195).
fn snapshot(state: &mut ShardState, principal: &Principal, volume: VolumeId) -> ReplyBody {
  match take_snapshot(state, principal, volume) {
    Ok((id, coverage)) => ReplyBody::Snapshotted { id, coverage },
    Err(refusal) => *refusal,
  }
}

/// Takes the snapshot [`snapshot`] answers with, for `principal`, who must be allowed to write `volume`: its id and its
/// coverage, or the refusal the verb answers. A Shards stream's SAVE takes one at its cut the same way (A-303), for
/// the stream's own principal, so a save can do nothing its principal could not ask itself.
pub(crate) fn take_snapshot(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Result<(SnapshotId, slates_ipc::protocol::SnapshotCoverage), Box<ReplyBody>> {
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = find_store_backed(state, volume, StoreVerb::Snapshot)?;
  if !rights_of(&record, principal).write {
    return Err(Box::new(forbidden("snapshot")));
  }
  // The volume's next epoch, checked before any effect: an epoch with no successor refuses the snapshot
  // rather than repeat one (AUD-29-26's sibling; reachable only through a maximal recorded epoch).
  let Some(next_epoch) = record.epoch.checked_add(1) else {
    return Err(Box::new(refused(Refusal::Unsupported {
      feature: VOLUME_EPOCH_EXHAUSTED.to_owned(),
    })));
  };
  // The barrier (§4.6 "Writeback and snapshot barrier"; GAP-A9-4): every live attachment of the
  // volume in the shard's registry has its generation closed before the root is frozen, so every
  // request admitted before belongs to this snapshot and every request after to the next; a request
  // still in flight — a consumer lost mid-request — is the typed incomplete barrier, never a clean
  // snapshot over it.
  let coverage = match state.attachments.barrier(record.id) {
    Ok(barrier) => snapshot_coverage(state, &barrier),
    Err(slates_vfs::VfsError::BarrierIncomplete {
      attachment,
      generation,
    }) => {
      return Err(Box::new(refused(Refusal::BarrierIncomplete {
        attachment: catalog_attachment_of(state, attachment),
        generation,
      })));
    }
    Err(e) => return Err(Box::new(refused(refusal_of_vfs(&e)))),
  };
  let taken = match state.volumes.get_mut(handle) {
    Ok(slot) => slot.volume.snapshot(&mut state.store),
    Err(_) => return Err(Box::new(refused(Refusal::NotFound))),
  };
  let id = match taken {
    Ok(id) => wire_snapshot(id),
    Err(e) => return Err(Box::new(refused(refusal_of_vfs(&e)))),
  };
  let now = state.clock.monotonic_ns();
  let ops = [
    Op::SnapshotTaken {
      record: SnapshotRecord {
        id: to_db_snapshot(id),
        volume: record.id,
        epoch: next_epoch,
        identity: None,
        placed: placement_of(state, record.id),
        taken_ns: now,
      },
    },
    Op::VolumeHeadAdvanced {
      id: record.id,
      head: to_db_snapshot(id),
      epoch: next_epoch,
    },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return Err(Box::new(refused(refusal_of_db(&e))));
    }
  }
  Ok((id, coverage))
}

/// What a closed barrier lets the snapshot claim (§4.6): the attachments closed, and the narrowest
/// boundary among them — a mount whose kernel client may still hold acknowledged writes (an NFS
/// client before its `COMMIT`) makes the snapshot server-visible; ring clients and a barrier over
/// no mount leave it complete.
fn snapshot_coverage(
  state: &ShardState,
  barrier: &slates_bridge_core::Barrier,
) -> slates_ipc::protocol::SnapshotCoverage {
  use slates_ipc::protocol::SnapshotBoundary;
  let server_visible = barrier.closed.iter().any(|(closed, _)| {
    state
      .mount_attachments
      .values()
      .any(|mount| mount.registry == *closed && mount.boundary == SnapshotBoundary::ServerVisible)
  });
  slates_ipc::protocol::SnapshotCoverage {
    boundary: if server_visible {
      SnapshotBoundary::ServerVisible
    } else {
      SnapshotBoundary::Complete
    },
    attachments_closed: u32::try_from(barrier.closed.len()).unwrap_or(u32::MAX),
  }
}

/// The catalog attachment id a registry attachment (by its key) rides for (a mount's), or the registry
/// key itself for one no catalog record maps (a guest device's).
fn catalog_attachment_of(state: &ShardState, registry_key: u64) -> u64 {
  state
    .mount_attachments
    .iter()
    .find(|(_, mount)| mount.registry.key() == registry_key)
    .map_or(registry_key, |(catalog, _)| *catalog)
}

fn destroy_snapshot_verb(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
) -> ReplyBody {
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Base) {
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
///
/// A green over a base (§4.16 "Green creation"; A-9's complete immutable base; A-217) is checked at once and then
/// built in a task of the base's owner shard a slice of the shard's step quantum at a time (§4.3 "Bounded work
/// everywhere"), the reply deferred to it as a pin's and a checkout's are: the snapshot walked into the origin
/// (`merge_service::OriginWalk`), the origin's identity hashed as its bytes are written
/// (`slates_merge::origin::IdentityCursor`), and the engine seeded from it (`slates_merge::engine::OriginLoad`); the
/// green is recorded with the request's completion as one atom ([`found_green`]). It was one synchronous call: a
/// green over the whole Linux kernel (97,229 files) held its shard for one poll of 69,028 ms (2026-11-06), and 4,000
/// files of 16 KiB for 549 ms on CI's x86_64 runner, 3.8 s on its arm64 one, every verb on the shard waiting for it
/// (`crates/server/tests/green_fairness.rs`). A scratch green has nothing to walk and is created at once.
fn create_green(
  state: &mut ShardState,
  principal: &Principal,
  name: &str,
  require_evidence: bool,
  base: Option<slates_ipc::protocol::GreenBase>,
) -> ReplyBody {
  if let Some(existing) = state.db.partition().volume_by_name(name) {
    return refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    });
  }
  // The chain starts from scratch or from a complete immutable base (§4.16; A-9): the base is checked and walked
  // into the origin before anything is recorded, so a refusal (an incomplete snapshot, a torn read) changes nothing.
  let Some(base) = base else {
    return found_green(state, principal, name, require_evidence, None);
  };
  let handle = match crate::merge_service::check_origin_base(state, principal, base) {
    Ok(handle) => handle,
    Err(refusal) => return refused(refusal),
  };
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a green over a base outside a recorded request".to_owned(),
    });
  };
  let job = GreenJob {
    request,
    principal: principal.clone(),
    name: name.to_owned(),
    require_evidence,
    base,
    handle,
    stage: GreenStage::Walk(crate::merge_service::OriginWalk::new(core_snapshot(
      base.snapshot,
    ))),
  };
  crate::image::defer(state, run_green(job))
}

/// What a green over a base was built from, for its record: the base it holds, the origin's identity, and the engine
/// seeded from the origin.
struct Seeded {
  base: slates_ipc::protocol::GreenBase,
  identity: [u8; 32],
  engine: slates_merge::engine::Green,
}

/// Records green `name` and installs its engine (scratch when `seeded` is `None`), the reply. A name taken meanwhile
/// (a green built over many slices, another created under its name before it ended) is refused `AlreadyExists`, and
/// so is nothing recorded. A base-seeded green holds its base snapshot for as long as it lives (A-217): pinned in the
/// base volume first, so nothing the green names can be freed under it, and given back if a later step refuses.
fn found_green(
  state: &mut ShardState,
  principal: &Principal,
  name: &str,
  require_evidence: bool,
  seeded: Option<Seeded>,
) -> ReplyBody {
  if let Some(existing) = state.db.partition().volume_by_name(name) {
    return refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    });
  }
  let Some(id) = fresh_volume_id(state) else {
    return refused(ids_exhausted());
  };
  let held = match &seeded {
    Some(seeded) => match hold_origin_snapshot(state, seeded.base) {
      Ok(()) => Some(slates_db::partition::OriginRef {
        volume: to_db_volume(seeded.base.volume),
        snapshot: to_db_snapshot(seeded.base.snapshot),
        identity: seeded.identity,
      }),
      Err(refusal) => return refused(refusal),
    },
    None => None,
  };
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
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
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
    catalog_version: 0,
    taken_over: None,
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    release_origin_snapshot(state, held.as_ref());
    return refused(refusal_of_db(&e));
  }
  // A base-seeded green records its origin as the reference it holds (A-217: the snapshot and the origin's
  // identity, never the tree's bytes), before the engine is installed, so a refused record leaves no engine ahead of
  // its log; recovery walks the held snapshot again before the chain (`rebuild_green`).
  let mut engine = match (seeded, held) {
    (Some(seeded), Some(held)) => {
      let record = Op::GreenOriginHeld {
        green: id,
        origin: held,
      };
      if let Err(e) = state.db.mutate(&mut state.segment, &record, now) {
        release_origin_snapshot(state, Some(&held));
        return refused(refusal_of_db(&e));
      }
      seeded.engine
    }
    _ => slates_merge::engine::Green::new(),
  };
  engine.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
  state.greens.insert(id, engine);
  // Version 0's merge record — the origin, placed before the record names it (§4.16 "Commit").
  crate::merge_service::enqueue_record(state, id, 0, [0u8; 32], 0, Vec::new(), (None, None));
  ReplyBody::GreenCreated {
    id: to_wire_volume(id),
  }
}

/// A green over a base between its slices ([`create_green`]).
struct GreenJob {
  request: (u64, RequestId),
  principal: Principal,
  name: String,
  require_evidence: bool,
  base: slates_ipc::protocol::GreenBase,
  handle: Handle<crate::state::VolumeSlot>,
  stage: GreenStage,
}

/// Where a green over a base has reached.
enum GreenStage {
  /// The base snapshot being walked into the origin.
  Walk(crate::merge_service::OriginWalk),
  /// The origin's identity being hashed.
  Identity {
    origin: slates_merge::origin::Origin,
    /// Boxed: a BLAKE3 hasher's state is about two kilobytes, far past the other stages.
    cursor: Box<slates_merge::origin::IdentityCursor>,
  },
  /// The engine being seeded from the origin.
  Load {
    identity: [u8; 32],
    /// Boxed: it holds the engine being seeded, far past the other stages.
    load: Box<slates_merge::engine::OriginLoad>,
  },
  /// Ended: the engine was handed to the record.
  Ended,
}

impl GreenJob {
  /// One unit: a directory listed or a file read, a share of the identity hashed, or a share of the engine seeded; the
  /// identity and the engine once the last is done. The base volume destroyed meanwhile is a stale handle, refused
  /// `NotFound`; its snapshot gone, the volume core's refusal.
  fn step(
    &mut self,
    state: &mut ShardState,
  ) -> Result<Option<([u8; 32], slates_merge::engine::Green)>, Refusal> {
    match std::mem::replace(&mut self.stage, GreenStage::Ended) {
      GreenStage::Walk(mut walk) => {
        let slot = state
          .volumes
          .get(self.handle)
          .map_err(|_| Refusal::NotFound)?;
        self.stage = match walk
          .unit(&slot.volume, &state.store)
          .map_err(|e| refusal_of_vfs(&e))?
        {
          Some(origin) => {
            // The walk canonicalizes what it gives, so a cursor is always had here.
            let Some(cursor) = slates_merge::origin::IdentityCursor::new(&origin) else {
              return Err(Refusal::ContentUnavailable);
            };
            GreenStage::Identity {
              origin,
              cursor: Box::new(cursor),
            }
          }
          None => GreenStage::Walk(walk),
        };
        Ok(None)
      }
      GreenStage::Identity { origin, mut cursor } => {
        let share = usize::try_from(state.config.archive_slice_bytes()).unwrap_or(usize::MAX);
        self.stage = match cursor.step(&origin, share) {
          Some(identity) => GreenStage::Load {
            identity,
            load: Box::new(slates_merge::engine::OriginLoad::new(origin)),
          },
          None => GreenStage::Identity { origin, cursor },
        };
        Ok(None)
      }
      GreenStage::Load { identity, mut load } => {
        // A file's seeding is bound by hashing its bytes into the head identity, so its share is the hash rate's.
        let share = usize::try_from(state.config.archive_slice_bytes()).unwrap_or(usize::MAX);
        match load.step(share) {
          Some(engine) => Ok(Some((identity, engine))),
          None => {
            self.stage = GreenStage::Load { identity, load };
            Ok(None)
          }
        }
      }
      GreenStage::Ended => Err(Refusal::Unsupported {
        feature: "a green's creation stepped after its end".to_owned(),
      }),
    }
  }
}

/// Builds the green a slice at a time, then records it with the request's completion as one atom and delivers the
/// reply. With the shard's state unreachable (its shutdown) no completion is recorded, so the client's retry creates
/// the green again on the shard that succeeds it.
async fn run_green(mut job: GreenJob) {
  let built = loop {
    match crate::image::with(|s| crate::image::sliced(s, |s| job.step(s))) {
      Ok(Some(built)) => break Ok(built),
      Ok(None) => slates_rt::futures::yield_now().await,
      Err(refusal) => break Err(refusal),
    }
  };
  let GreenJob {
    request,
    principal,
    name,
    require_evidence,
    base,
    ..
  } = job;
  let _ = crate::state::with_state(|s| {
    let (reply, route) = crate::image::complete(s, request, |s| match built {
      Ok((identity, engine)) => found_green(
        s,
        &principal,
        &name,
        require_evidence,
        Some(Seeded {
          base,
          identity,
          engine,
        }),
      ),
      Err(refusal) => refused(refusal),
    });
    crate::image::deliver(s, reply, route);
  });
}

/// Pins `base`'s snapshot in its volume for a green that names it (A-217), as a clone pins its origin: its destroy is
/// then refused until the green is gone.
fn hold_origin_snapshot(
  state: &mut ShardState,
  base: slates_ipc::protocol::GreenBase,
) -> Result<(), Refusal> {
  let (handle, _) = find(state, base.volume).map_err(|reply| match *reply {
    ReplyBody::Refused { refusal } => refusal,
    _ => Refusal::NotFound,
  })?;
  let slot = state
    .volumes
    .get_mut(handle)
    .map_err(|_| Refusal::NotFound)?;
  slot
    .volume
    .pin(core_snapshot(base.snapshot))
    .map_err(|e| refusal_of_vfs(&e))
}

/// Gives back the pin a green held on its origin's snapshot (A-217): at the green's destroy, or when its creation was
/// refused after the pin.
pub(crate) fn release_origin_snapshot(
  state: &mut ShardState,
  held: Option<&slates_db::partition::OriginRef>,
) {
  let Some(held) = held else {
    return;
  };
  if let Some(handle) = state.by_id.get(&held.volume).copied() {
    unpin_origin(
      state,
      handle,
      SnapshotId {
        value: held.snapshot.value,
      },
    );
  }
}

/// A green's version chain (§4.16): its head version. The read side of the chain — `changed_since`
/// and the per-version records follow. Answered by the green's owner shard, which holds the engine.
fn versions(state: &ShardState, principal: &Principal, green: VolumeId) -> ReplyBody {
  let record = match crate::merge_service::require_green(state, principal, green, "versions") {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  let Some(engine) = state.greens.get(&record.id) else {
    return refused(Refusal::NotFound);
  };
  ReplyBody::Versions {
    head: engine.head(),
  }
}

/// One page of the paths a green changed strictly after `version` (§4.16, A-264): the read a lagging work uses to know
/// what the green moved under it before it rebases. Read from the green's change index from `from`, in version order,
/// answering each path at the last version it changed at; the page ends when the next path would not fit one reply chunk
/// or when the page has spent its slice of the shard's step ([`CHANGED_SLICE_PERMILLE`]), and its cursor names the first
/// entry it did not pass. So a page costs the entries it passes, never the green's tree, and never more than its slice:
/// before 2026-11-09 a page walked every path of every dimension, about 1 ms a page on a kernel tree whatever the span,
/// and ten times that on ten times the tree. A version past the head is refused `UnknownBase`; one below the fold floor
/// is answered, as the index keeps each path's last change there.
fn changed_since(
  state: &ShardState,
  principal: &Principal,
  green: VolumeId,
  version: u64,
  from: Option<slates_ipc::protocol::ChangedCursor>,
) -> ReplyBody {
  let record = match crate::merge_service::require_green(state, principal, green, "changed_since") {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  let Some(engine) = state.greens.get(&record.id) else {
    return refused(Refusal::NotFound);
  };
  if version > engine.head() {
    return refused(Refusal::UnknownBase { green, version });
  }
  let slice = state
    .config
    .step_quantum_ns()
    .saturating_mul(CHANGED_SLICE_PERMILLE)
    / PERMILLE;
  let started = slates_rt::futures::now_ns();
  let room = changed_page_room();
  let mut used = 0usize;
  let mut paths = Vec::new();
  let mut next = None;
  let cursor = from.map(|at| (at.version, usize::try_from(at.passed).unwrap_or(usize::MAX)));
  for (passed, change) in engine.changes_after(version, cursor).enumerate() {
    // Where a page that stops before this entry continues: this version, this many of its entries passed (a version
    // holds far fewer than 2^32 paths; the green's memory is bounded long before).
    let here = slates_ipc::protocol::ChangedCursor {
      version: change.version,
      passed: u32::try_from(change.index).unwrap_or(u32::MAX),
    };
    if passed > 0
      && passed % CHANGED_CLOCK_EVERY == 0
      && slates_rt::futures::now_ns().saturating_sub(started) >= slice
    {
      next = Some(here);
      break;
    }
    if !change.last {
      continue;
    }
    let path = change.path.to_owned();
    let bytes = encoded_len(&path);
    if !paths.is_empty() && used.saturating_add(bytes) > room {
      next = Some(here);
      break;
    }
    used = used.saturating_add(bytes);
    paths.push(path);
  }
  ReplyBody::ChangedSince { paths, next }
}

/// Shape: a `changed_since` page's slice of the shard's step budget, parts per thousand: half, as a destroy slice's, so
/// one page never takes the whole step from the shard's other clients (§4.3).
const CHANGED_SLICE_PERMILLE: u64 = 500;
/// Shape: the change-index entries a page passes between two looks at the clock: a look costs about what passing a
/// few entries does, so one each 64 keeps the looks a small share and a page's overrun of its slice within 64 entries.
const CHANGED_CLOCK_EVERY: usize = 64;

/// The bytes a `changed_since` page's paths may take: one reply chunk less the page's own framing, its cursor included.
fn changed_page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::ChangedSince {
    paths: Vec::new(),
    next: Some(slates_ipc::protocol::ChangedCursor {
      version: u64::MAX,
      passed: u32::MAX,
    }),
  };
  chunk.saturating_sub(slates_ipc::protocol::framed_len(&empty))
}

/// The encoded bytes `value` adds to a reply.
pub(crate) fn encoded_len<T: Wire>(value: &T) -> usize {
  let mut encoded = Vec::new();
  value.encode(&mut encoded);
  encoded.len()
}

/// Creates a work volume over a green (§4.16): an agent's private place to declare operations,
/// based on the green's current head. The green must be owned by this shard (it holds the engine).
fn create_work(
  state: &mut ShardState,
  principal: &Principal,
  green: VolumeId,
  name: &str,
) -> ReplyBody {
  // The green's owner admitted the work (A-247), having recorded its floor, or the green is on this shard and its
  // floor is recorded below, in this transaction.
  let (green_id, base, minted) = match state.work_home.admitted.take() {
    Some(Ok(admitted)) => (to_db_volume(green), admitted.base, Some(admitted.id)),
    Some(Err(refusal)) => {
      // The owner refused it, so the name and the id it held go back.
      if let Err(e) = settle_creation(state, name) {
        return refused(refusal_of_db(&e));
      }
      return refused(refusal);
    }
    None if owner_of(green) != state.partition => {
      // A green on another shard is admitted through its owner (`crate::work_home`); a creation that reaches here
      // did not ask, because its name is held.
      return refused(match name_held(state, name) {
        Some(existing) => Refusal::AlreadyExists { existing },
        None => Refusal::NotFound,
      });
    }
    None => {
      let green_id =
        match crate::merge_service::require_green(state, principal, green, "create_work") {
          Ok(record) => record.id,
          Err(refusal) => return refused(refusal),
        };
      let Some(engine) = state.greens.get(&green_id) else {
        return refused(Refusal::NotFound);
      };
      // The work is the green at its head: it holds nothing of its own until it changes a file, and reads every
      // other file from the green there (`crate::work_files`), so an edit to a base file splices what the file holds.
      (green_id, engine.head(), None)
    }
  };
  // An admitted work holds its own name while it waits (A-247): that is its own hold, released here.
  if let Some(existing) = name_held(state, name)
    && Some(to_db_volume(existing)) != minted
  {
    if let Some(id) = minted {
      crate::work_home::tell_floor(state, green_id, id, None);
    }
    return refused(Refusal::AlreadyExists { existing });
  }
  if minted.is_some()
    && let Err(e) = settle_creation(state, name)
  {
    return refused(refusal_of_db(&e));
  }
  let Some(id) = minted.or_else(|| fresh_volume_id(state)) else {
    return refused(ids_exhausted());
  };
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
      live_shipped: false,
      live_holders: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
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
    catalog_version: 0,
    taken_over: None,
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    if minted.is_some() {
      crate::work_home::tell_floor(state, green_id, id, None);
    }
    return refused(refusal_of_db(&e));
  }
  // The green keeps this base while the work holds it (A-247); a green elsewhere recorded it when it admitted the work.
  if minted.is_none()
    && let Err(e) = state.db.mutate(
      &mut state.segment,
      &Op::WorkFloorSet {
        green: green_id,
        work: id,
        base,
      },
      now,
    )
  {
    return refused(refusal_of_db(&e));
  }
  // A new work holds nothing of its own, so it starts charged for nothing; each change charges what it adds.
  let charged = 0;
  state.works.insert(
    id,
    crate::state::WorkState {
      green: green_id,
      base_version: base,
      journal: Vec::new(),
      files: crate::work_files::WorkFiles::default(),
      revision: 0,
      charged,
      entries: std::collections::BTreeMap::new(),
      verdict: None,
    },
  );
  // Its catalog register, which names its green, is the route a resend of its submit takes after a takeover, and its
  // version's merge record waits for it (A-255): shipped by the record plane now, not at the coordinator's period.
  state.merge.works_owing_catalog.insert(id);
  crate::merge_service::nudge_record_plane(state);
  ReplyBody::WorkCreated {
    id: to_wire_volume(id),
    base,
  }
}

/// Declares an edit on a work volume (§4.16): a splice at `path` — remove `delete_len` bytes at `at`,
/// insert `bytes`. Maintains the work's content and appends the declared operations, composed into an
/// increment on submit. A new path is created first.
/// `StageBegin` (`crate::staging`): a buffer of `len` bytes for an edit of `work` too large for one request, after
/// the same write check an edit takes.
/// The volume a staging buffer is for: a work the caller may edit, or a plain volume it may write (its file verbs'
/// staged content, `crate::fs_verbs`); a green is read-only.
fn staging_volume(
  state: &ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Result<DbVolumeId, Refusal> {
  // The catalog record, not a store slot: a work is a merge volume with no slot of its own.
  let record = state
    .db
    .partition()
    .volume(to_db_volume(volume))
    .cloned()
    .ok_or(Refusal::NotFound)?;
  if record.policy.role == Role::Plain {
    if !rights_of(&record, principal).write {
      return Err(Refusal::Forbidden {
        verb: "stage".to_owned(),
      });
    }
    return Ok(record.id);
  }
  crate::merge_service::require_work(state, principal, volume, true, "edit")
    .map(|(record, _)| record.id)
}

fn stage_begin(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  len: u64,
) -> ReplyBody {
  let work_id = match staging_volume(state, principal, work) {
    Ok(id) => id,
    Err(refusal) => return refused(refusal),
  };
  let now = state.clock.monotonic_ns();
  state.staging.expire(now, &mut state.store.metadata);
  let expires = now.saturating_add(state.config.failover_slo_ns);
  match state.staging.begin(
    (work_id, principal),
    len,
    (state.partition, expires),
    &mut state.store.metadata,
  ) {
    Ok(token) => ReplyBody::Staged { token },
    Err(refusal) => refused(refusal),
  }
}

/// `StagePut` (`crate::staging`): the next bytes of a staging buffer.
fn stage_put(
  state: &mut ShardState,
  principal: &Principal,
  (work, token): (VolumeId, u64),
  offset: u64,
  bytes: &[u8],
) -> ReplyBody {
  let work_id = match staging_volume(state, principal, work) {
    Ok(id) => id,
    Err(refusal) => return refused(refusal),
  };
  let now = state.clock.monotonic_ns();
  state.staging.expire(now, &mut state.store.metadata);
  let expires = now.saturating_add(state.config.failover_slo_ns);
  match state
    .staging
    .put((work_id, principal, token), offset, bytes, expires)
  {
    Ok(filled) => ReplyBody::StagePutDone { filled },
    Err(refusal) => refused(refusal),
  }
}

/// `EditStaged`: [`edit`] inserting a full staging buffer's bytes, which it releases.
/// `FsWriteStaged` (`crate::fs_verbs`): an `FsWrite` whose content is a full staging buffer.
fn fs_write_staged(
  state: &mut ShardState,
  principal: &Principal,
  (volume, attachment): (VolumeId, u64),
  path: &str,
  token: u64,
  mode: u32,
) -> ReplyBody {
  let bytes = match state.staging.take(
    (to_db_volume(volume), principal, token),
    &mut state.store.metadata,
  ) {
    Ok(bytes) => bytes,
    Err(refusal) => return refused(refusal),
  };
  crate::fs_verbs::serve(
    state,
    principal,
    (volume, attachment),
    crate::fs_verbs::FsOp::Write {
      path,
      bytes: &bytes,
      mode,
    },
  )
}

/// The splice an [`edit_and_submit`] makes before it submits: `edit`'s arguments, with the bytes inline or staged.
struct Splice<'a> {
  /// The file.
  path: &'a str,
  /// The offset.
  at: u64,
  /// Bytes removed at `at`.
  delete_len: u64,
  /// Bytes inserted at `at`.
  insert: slates_ipc::protocol::EditBytes,
}

/// `edit` and then `submit`, as one request (§4.16 "Edit and submit in one request", A-225): the edit exactly as
/// [`edit`] (or [`edit_staged`]) makes it and, if it is accepted, the submit exactly as [`submit`] makes it. Both run in
/// the request's one durable step under its one completion record (`run_recorded`), so a restart replays both or
/// neither. The edit's refusal is the answer when the edit is refused, and nothing is submitted, as a client that saw
/// `edit` refused would not submit; otherwise the answer is the submit's, verdict, windows and refusals alike.
fn edit_and_submit(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  splice: Splice<'_>,
  (evidence, held): (Vec<[u8; 32]>, bool),
) -> ReplyBody {
  // The green's owner already judged what the edit would leave (A-247; `crate::work_home`). The edit was never applied
  // to the work: an accept moves past it and a conflict applies it now, so the work holds it as a client's two verbs
  // would have left it.
  if let Some(reply) = state.work_home.verdict.take() {
    if let Err(e) = settle_pending(state, to_db_volume(work)) {
      return refused(refusal_of_db(&e));
    }
    return match crate::merge_service::require_work(state, principal, work, true, "edit") {
      Ok((record, green)) => {
        settle_edit_submit(state, principal, (record.id, green, work), splice, reply)
      }
      Err(refusal) => refused(refusal),
    };
  }
  let edited = apply_splice(state, principal, work, splice);
  match edited {
    ReplyBody::Edited => submit(
      state,
      principal,
      work,
      (evidence, None),
      if held { Approval::Held } else { Approval::None },
    ),
    refused_edit => refused_edit,
  }
}

/// An edit-and-submit's edit, of its bytes inline or from a staging buffer.
fn apply_splice(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  splice: Splice<'_>,
) -> ReplyBody {
  match splice.insert {
    slates_ipc::protocol::EditBytes::Inline { bytes } => edit(
      state,
      principal,
      work,
      splice.path,
      splice.at,
      splice.delete_len,
      &bytes,
    ),
    slates_ipc::protocol::EditBytes::Staged { token } => edit_staged(
      state,
      principal,
      (work, token),
      splice.path,
      splice.at,
      splice.delete_len,
    ),
  }
}

/// The owner's answer to an edit-and-submit on the work's shard (A-247): an accept resets the work; any other answer
/// applies the edit, as the verb on one shard would have before its verdict, and answers as the submit did, a conflict
/// kept with the work ([`kept_conflict`], A-267).
fn settle_edit_submit(
  state: &mut ShardState,
  principal: &Principal,
  (work_id, green_id, work): (DbVolumeId, DbVolumeId, VolumeId),
  splice: Splice<'_>,
  reply: ReplyBody,
) -> ReplyBody {
  if accepted_version(&reply).is_some() {
    // The edit went to the owner inside the increment, so a buffer it came from is done with.
    if let slates_ipc::protocol::EditBytes::Staged { token } = splice.insert {
      let _ = state
        .staging
        .take((work_id, principal, token), &mut state.store.metadata);
    }
    return settle_submit(state, work_id, green_id, reply);
  }
  let edited = apply_splice(state, principal, work, splice);
  if matches!(edited, ReplyBody::Edited) {
    kept_conflict(state, work_id, reply)
  } else {
    edited
  }
}

fn edit_staged(
  state: &mut ShardState,
  principal: &Principal,
  (work, token): (VolumeId, u64),
  path: &str,
  at: u64,
  delete_len: u64,
) -> ReplyBody {
  let work_id = match crate::merge_service::require_work(state, principal, work, true, "edit") {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  let bytes = match state
    .staging
    .take((work_id, principal, token), &mut state.store.metadata)
  {
    Ok(bytes) => bytes,
    Err(refusal) => return refused(refusal),
  };
  edit(state, principal, work, path, at, delete_len, &bytes)
}

fn edit(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  path: &str,
  at: u64,
  delete_len: u64,
  bytes: &[u8],
) -> ReplyBody {
  // A declared write: refused `ReadOnlyVolume` on a green, `NotWork` on a plain volume (§4.16, D-27).
  let work_id = match crate::merge_service::require_work(state, principal, work, true, "edit") {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  let path = crate::merge_service::canonical_path(path);
  let Some(w) = state.works.get(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let base = crate::work_files::Base {
    green: state.greens.get(&w.green),
    fetched: state.work_home.fetched.as_ref(),
    version: w.base_version,
  };
  // A file the work has not changed is the green's at its base; with the green gone it cannot be read (§4.16
  // failure matrix: the work's submit answers the same).
  let Ok(old) = w.files.get(base, path) else {
    return refused(Refusal::UnknownBase {
      green: to_wire_volume(w.green),
      version: w.base_version,
    });
  };
  // The journal operations this edit records, built before anything changes so its charge is exact, by the one
  // builder the work log's replay uses too (`crate::work_log`, A-207).
  let inserted = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  let ops = crate::work_log::edit_ops(path, old.as_deref(), (at, delete_len, inserted));
  // The work is charged for what the edit adds before it changes anything (`crate::work_charge`): the file it now
  // holds (copied from the base when it had not changed it), less what the splice deletes, and the journal's ops.
  let (spliced, removed) =
    w.files
      .splice_delta(path, old.as_deref(), (at, delete_len, bytes.len()));
  let added = crate::work_charge::journal_footprint(&ops).saturating_add(spliced);
  let entry = crate::work_log::WorkLogEntry::Edit {
    path: path.to_owned(),
    at,
    delete_len,
    bytes: bytes.to_vec(),
  };
  if let Err(refusal) = record_work_change(state, work_id, added, entry) {
    return refused(refusal);
  }
  let (works, greens, home) = (&mut state.works, &state.greens, &state.work_home);
  let Some(w) = works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let base = crate::work_files::Base {
    green: greens.get(&w.green),
    fetched: home.fetched.as_ref(),
    version: w.base_version,
  };
  // The base was read above on this single-threaded shard, so it still is; a refusal here gives the charge back.
  if crate::work_log::apply_edit(
    (&mut w.files, &mut w.journal),
    base,
    (path, at, delete_len, bytes),
    ops,
  )
  .is_err()
  {
    let green = to_wire_volume(w.green);
    let version = w.base_version;
    crate::work_charge::shrink(state, work_id, added);
    return refused(Refusal::UnknownBase { green, version });
  }
  w.revision = w.revision.wrapping_add(1);
  crate::work_charge::shrink(state, work_id, removed);
  ReplyBody::Edited
}

/// Records a change to `work` in its log, in the verb's transaction (A-207; D-18: an acknowledged change survives a
/// daemon restart; `crate::work_log`), after charging the `added` bytes it brings: refused, with nothing changed, when
/// the partition's merge budget cannot hold the entry or the budget cannot cover the bytes.
fn record_work_change(
  state: &mut ShardState,
  work: DbVolumeId,
  added: u64,
  entry: crate::work_log::WorkLogEntry,
) -> Result<(), Refusal> {
  let op = Op::WorkLogged {
    work,
    entry: slates_wire::Wire::to_bytes(&entry),
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .partition()
    .check(&op, now)
    .map_err(|e| refusal_of_db(&e))?;
  crate::work_charge::grow(state, work, added)
    .map_err(|available| Refusal::BudgetExceeded { available })?;
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    crate::work_charge::shrink(state, work, added);
    return Err(refusal_of_db(&e));
  }
  Ok(())
}

/// The `WorkRestated` record of `work` restated as `entry` (A-207): what an accepted submit or a rebase records.
fn restatement(work: DbVolumeId, entry: &crate::work_log::WorkLogEntry) -> Op {
  Op::WorkRestated {
    work,
    entry: slates_wire::Wire::to_bytes(entry),
  }
}

/// The restatement of a work an accepted submit resets to `version`: based there, holding nothing of its own, with an
/// empty journal. Its length does not depend on the version (the wire codec writes a `u64` at its fixed width), so the
/// submit checks it before the verdict.
fn accepted_restatement(version: u64) -> crate::work_log::WorkLogEntry {
  crate::work_log::WorkLogEntry::Restated {
    base: version,
    files: Vec::new(),
    journal: Vec::new(),
  }
}

/// Declares a namespace or metadata operation on a work volume (§4.16): appends the corresponding
/// declared operation to the work's journal, the counterpart to [`edit`]'s content splice. An unlink
/// or rename also keeps the work's content map consistent, so a later edit and the post-state seal
/// see the right files; a symlink's target travels in the ops document's path table and an xattr's
/// value in the journal, so neither needs a work-side store.
fn declare(state: &mut ShardState, principal: &Principal, work: VolumeId, op: WorkOp) -> ReplyBody {
  let work_id = match crate::merge_service::require_work(state, principal, work, true, "declare") {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  // A guest's own attribute namespaces (`trusted.`, `security.`) are its kernel's to set (A-290): a guest's kernel
  // acts on them (a binary's capability, an overlay's opaque directory), and inside the guest only its privileged
  // processes may set them. A declaration is a host's channel, so it sets and removes none, as no host's mount does.
  if let WorkOp::SetXattr { name, .. } | WorkOp::RemoveXattr { name, .. } = &op
    && slates_bridge_core::guest_only(name.as_bytes())
  {
    return refused(Refusal::Unsupported {
      feature: format!("a guest's own attribute ({name}) from a host's channel"),
    });
  }
  let Some(w) = state.works.get(&work_id) else {
    return refused(Refusal::NotFound);
  };
  // Every path the operation names is keyed canonically (no leading slash), as `edit` keys its
  // path and the origin walk its entries; a symlink's target is a link string, kept as given.
  let key = |path: String| crate::merge_service::canonical_path(&path).to_owned();
  let volume_op = match op {
    WorkOp::Mknod { path, node } => {
      use slates_ipc::protocol::IpcNodeKind;
      use slates_merge::special::{SpecialKind, SpecialNode};
      let path = key(path);
      VolumeOp::Mknod {
        path,
        mode: node.mode,
        node: SpecialNode {
          kind: match node.kind {
            IpcNodeKind::Fifo => SpecialKind::Fifo,
            IpcNodeKind::Socket => SpecialKind::Socket,
            // A declared device node is metadata (A-294): the merge keeps its number and nothing opens it.
            IpcNodeKind::BlockDevice { major, minor } => SpecialKind::BlockDevice { major, minor },
            IpcNodeKind::CharDevice { major, minor } => SpecialKind::CharDevice { major, minor },
          },
          uid: node.uid,
          gid: node.gid,
          atime: node.atime,
          mtime: node.mtime,
          ctime: node.ctime,
          btime: node.btime,
        },
      }
    }
    WorkOp::Unlink { path } => VolumeOp::Unlink { path: key(path) },
    WorkOp::Rename { from, to } => VolumeOp::Rename {
      from: key(from),
      to: key(to),
    },
    WorkOp::Mkdir { path } => VolumeOp::Mkdir { path: key(path) },
    WorkOp::Rmdir { path } => VolumeOp::Rmdir { path: key(path) },
    WorkOp::SetMode { path, mode } => VolumeOp::SetMode {
      path: key(path),
      mode,
    },
    WorkOp::Symlink { path, target } => VolumeOp::Symlink {
      path: key(path),
      target,
    },
    WorkOp::Link { path, target } => VolumeOp::Link {
      path: key(path),
      target: key(target),
    },
    WorkOp::SetXattr { path, name, value } => VolumeOp::SetXattr {
      path: key(path),
      name,
      value,
    },
    WorkOp::RemoveXattr { path, name } => VolumeOp::RemoveXattr {
      path: key(path),
      name,
    },
  };
  // The work is charged for what the declaration adds before it changes anything (`crate::work_charge`): a moved
  // file it now holds, a removal it records, the operation itself.
  let base = crate::work_files::Base {
    green: state.greens.get(&w.green),
    fetched: state.work_home.fetched.as_ref(),
    version: w.base_version,
  };
  let Ok((moved, removed)) = w.files.declare_delta(base, &volume_op) else {
    return refused(Refusal::UnknownBase {
      green: to_wire_volume(w.green),
      version: w.base_version,
    });
  };
  let added = volume_op.footprint().saturating_add(moved);
  let entry = crate::work_log::WorkLogEntry::Declared {
    op: crate::work_log::LoggedOp::of(&volume_op),
  };
  if let Err(refusal) = record_work_change(state, work_id, added, entry) {
    return refused(refusal);
  }
  let (works, greens, home) = (&mut state.works, &state.greens, &state.work_home);
  let Some(w) = works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let base = crate::work_files::Base {
    green: greens.get(&w.green),
    fetched: home.fetched.as_ref(),
    version: w.base_version,
  };
  if crate::work_log::apply_declare((&mut w.files, &mut w.journal), base, volume_op).is_err() {
    let green = to_wire_volume(w.green);
    let version = w.base_version;
    crate::work_charge::shrink(state, work_id, added);
    return refused(Refusal::UnknownBase { green, version });
  }
  w.revision = w.revision.wrapping_add(1);
  crate::work_charge::shrink(state, work_id, removed);
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

/// Assembles an increment's post-state from a work's content and journal (§4.16): the deriver lays the runs its
/// content ops add out compactly, then the extended-attribute values and the special nodes, and each such op names
/// its slice (`slates_merge::increment::compose_volume_planned`). A content op copies `len` bytes of its file from
/// where the work's file holds them, the plan's offset for its `src`; a `SetXattr` op copies the attribute's whole
/// value — both into `[op.src, op.src + op.len)`. Until 2026-11-07 a content op's bytes were read at `op.at`, its
/// offset in base coordinates, so a run after an earlier length change in the same file was read from the wrong
/// place: a delete of a file's first two bytes then an overwrite of its last two merged two zeros into the green
/// (`crates/server/tests/green_post_state.rs`).
/// The xattr value round-trips through the post-state, which is how the green stores it and how the
/// identity check compares two agents' values, so it must be laid in, not left zero.
fn assemble_post_state(
  (doc, plan): (
    &slates_merge::ops_doc::OpsDoc,
    &slates_merge::increment::PostStatePlan,
  ),
  (files, base): (&crate::work_files::WorkFiles, crate::work_files::Base<'_>),
  journal: &[VolumeOp],
) -> Vec<u8> {
  use slates_merge::ops_doc::OpKind;
  let is_file_content =
    |kind: OpKind| matches!(kind, OpKind::Overwrite | OpKind::Insert | OpKind::Extend);
  let mut size = 0u64;
  for op in &doc.ops {
    if op.src != u64::MAX
      && (is_file_content(op.kind) || matches!(op.kind, OpKind::SetXattr | OpKind::Mknod))
    {
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
    let encoded_node;
    let file_bytes;
    // The bytes this op contributes, and the offset into them: a file's slice, or an xattr's value.
    let (source, source_at): (&[u8], u64) = if is_file_content(op.kind) {
      // The submit read the base already (`build_increment` refuses a work whose green is gone).
      let Some(file) = files.get(base, path).ok().flatten() else {
        continue;
      };
      let Some(at) = plan.get(&op.src) else {
        continue;
      };
      file_bytes = file;
      (&file_bytes, *at)
    } else if op.kind == OpKind::Mknod {
      let Some(node) = journal.iter().rev().find_map(|entry| match entry {
        VolumeOp::Mknod {
          path: declared,
          node,
          ..
        } if declared == path => Some(node),
        _ => None,
      }) else {
        continue;
      };
      encoded_node = node.encode();
      (&encoded_node, 0)
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
    let span = (
      post.get_mut(dst..dst.saturating_add(len)),
      source.get(from..from.saturating_add(len)),
    );
    if let (Some(into), Some(from)) = span {
      into.copy_from_slice(from);
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
  sealed: &crate::work_home::SealedWork,
  evidence: Vec<[u8; 32]>,
) -> Result<(DbVolumeId, slates_merge::engine::Increment), Refusal> {
  let green_id = sealed.green;
  let base_version = sealed.base_version;
  // The green is gone (destroyed), or the work's base is past the head it holds: the base names a
  // version this green does not have (§4.16 failure matrix, `UnknownBase`).
  let engine = state
    .greens
    .get(&green_id)
    .filter(|engine| base_version <= engine.head())
    .ok_or(Refusal::UnknownBase {
      green: to_wire_volume(green_id),
      version: base_version,
    })?;
  // A work off this shard composed its own (A-247): the owner only judges it.
  if let Some(composed) = &sealed.composed {
    return Ok((green_id, composed.clone()));
  }
  // Only the base entries the journal names (`Green::base_for`): the deriver reads nothing else, and the
  // whole base cloned every path's name on every submit.
  let base = engine.base_for(base_version, &sealed.journal);
  let files_base = crate::work_files::Base {
    green: Some(engine),
    fetched: None,
    version: base_version,
  };
  let inc = compose_increment(
    (sealed.work, base_version),
    &base,
    (&sealed.files, files_base),
    &sealed.journal,
    evidence,
  )?;
  Ok((green_id, inc))
}

/// Composes a work's increment over `base`, the restricted base of its journal at `base_version` (§4.16): the ops
/// document, the post-state assembled from the work's files over `files_base`, and the identity. The owner's shard and
/// a work's own shard (A-247, from the entries it fetched, `Base::for_names`) compose by this one function, so the
/// increment is the same byte for byte wherever it is composed.
pub(crate) fn compose_increment(
  (work_id, base_version): (DbVolumeId, u64),
  base: &slates_merge::increment::Base,
  (files, files_base): (&crate::work_files::WorkFiles, crate::work_files::Base<'_>),
  journal: &[VolumeOp],
  evidence: Vec<[u8; 32]>,
) -> Result<slates_merge::engine::Increment, Refusal> {
  let (doc, plan) = match slates_merge::increment::compose_volume_planned(base, journal) {
    Ok(planned) => planned,
    Err(e) => {
      return Err(Refusal::BadRequest {
        reason: format!("increment does not compose: {e:?}"),
      });
    }
  };
  let post_state = assemble_post_state((&doc, &plan), (files, files_base), journal);
  // The identity names the work and its base before the post-state and the document (§4.16 data model:
  // `blake3(work_volume, base, post_state, ops_doc, filter)`), so the engine's retry cache answers only a
  // retry of this work's increment. Without them, a second work's identical edit from the same base was
  // answered with the first work's version even when the green had since changed that range, a conflict
  // reported as an accept (`docs/bugs/2026-11-04-the-same-edit-from-two-works-was-one-increment.md`).
  // Both are fixed widths, so the concatenation is unambiguous. Every work volume of a green is under one
  // exclusion filter today (none), so the filter adds nothing yet.
  let mut hasher = blake3::Hasher::new();
  hasher.update(&work_id.bytes);
  hasher.update(&base_version.to_le_bytes());
  hasher.update(&doc.encode());
  hasher.update(&post_state);
  let id = *hasher.finalize().as_bytes();
  Ok(slates_merge::engine::Increment {
    id,
    base: base_version,
    doc,
    post_state,
    evidence,
  })
}

/// Keeps a conflicting verdict with its work (§4.16, A-267): every window, under a fresh id, until the work's next
/// submit or rebase. Returns what the verdict's reply carries in `room` bytes — the first windows, the count of the
/// rest, which `Conflicts` reads, and the id it reads them by. The id is the shard's monotonic clock, kept past the
/// work's last, so a verdict replaced (or a daemon restarted, which keeps none) never answers for another.
fn keep_verdict(
  state: &mut ShardState,
  work_id: DbVolumeId,
  windows: Vec<slates_ipc::protocol::MergeWindow>,
  room: usize,
) -> (Vec<slates_ipc::protocol::MergeWindow>, u32, u64) {
  let now = state.clock.monotonic_ns();
  let Some(work) = state.works.get_mut(&work_id) else {
    // No work to keep it with (it cannot happen inside the verb that judged it): carry what fits, read nothing more.
    let (carried, elided) = carried(windows, room);
    return (carried, elided, 0);
  };
  let id = now.max(
    work
      .verdict
      .as_ref()
      .map_or(1, |kept| kept.id.saturating_add(1)),
  );
  let (carried, elided) = carried(windows.clone(), room);
  work.verdict = Some(crate::state::KeptVerdict { id, windows });
  (carried, elided, id)
}

/// Keeps a conflicting verdict the green's half answered with the work it judged (§4.16, A-267; A-247). The verdict is
/// the green's, judged on its owner's shard, which holds no work on another shard; the work and its `Conflicts` reads
/// are on this one. So the green's half answers every window, and this shard keeps them and answers what one reply
/// carries, the rest counted under the kept verdict's id. An answer already carried (a count or an id in it) and any
/// answer but a conflict are the reply as they stand.
fn kept_conflict(state: &mut ShardState, work_id: DbVolumeId, reply: ReplyBody) -> ReplyBody {
  match reply {
    ReplyBody::Submitted {
      version: None,
      conflicts,
      conflicts_elided: 0,
      verdict: 0,
    } => {
      let room = reply_room(&ReplyBody::Submitted {
        version: None,
        conflicts: Vec::new(),
        conflicts_elided: 0,
        verdict: u64::MAX,
      });
      let (conflicts, conflicts_elided, verdict) = keep_verdict(state, work_id, conflicts, room);
      ReplyBody::Submitted {
        version: None,
        conflicts,
        conflicts_elided,
        verdict,
      }
    }
    ReplyBody::SubmittedSigned {
      version: None,
      conflicts,
      verification,
      conflicts_elided: 0,
      verdict: 0,
    } => {
      let room = reply_room(&ReplyBody::SubmittedSigned {
        version: None,
        conflicts: Vec::new(),
        verification,
        conflicts_elided: 0,
        verdict: u64::MAX,
      });
      let (conflicts, conflicts_elided, verdict) = keep_verdict(state, work_id, conflicts, room);
      ReplyBody::SubmittedSigned {
        version: None,
        conflicts,
        verification,
        conflicts_elided,
        verdict,
      }
    }
    ReplyBody::Rebased {
      version: None,
      conflicts,
      conflicts_elided: 0,
      verdict: 0,
    } => {
      let room = reply_room(&ReplyBody::Rebased {
        version: None,
        conflicts: Vec::new(),
        conflicts_elided: 0,
        verdict: u64::MAX,
      });
      let (conflicts, conflicts_elided, verdict) = keep_verdict(state, work_id, conflicts, room);
      ReplyBody::Rebased {
        version: None,
        conflicts,
        conflicts_elided,
        verdict,
      }
    }
    other => other,
  }
}

/// `Conflicts`: one page of a work's kept verdict from position `from` (§4.16, A-267), as many windows as one reply
/// holds, in the verdict's order. A verdict the work no longer keeps — replaced by a later submit or rebase, cleared by
/// one that succeeded, or forgotten by a restart — is refused `VerdictReplaced`. A window always fits a page alone: a
/// work's path arrives in a request, whose framing is larger than a page's (as A-264's changed paths).
fn conflicts(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  verdict: u64,
  from: u32,
) -> ReplyBody {
  let work_id = match crate::merge_service::require_work(state, principal, work, false, "conflicts")
  {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  let Some(kept) = state
    .works
    .get(&work_id)
    .and_then(|w| w.verdict.as_ref())
    .filter(|kept| verdict != 0 && kept.id == verdict)
  else {
    return refused(Refusal::VerdictReplaced { work });
  };
  let room = reply_room(&ReplyBody::Conflicts {
    windows: Vec::new(),
    next: Some(u32::MAX),
  });
  let start = usize::try_from(from).unwrap_or(usize::MAX);
  let rest = kept.windows.get(start..).unwrap_or_default();
  let (windows, more) = fitting(rest.iter().cloned(), room);
  let next = if more {
    u32::try_from(start.saturating_add(windows.len())).ok()
  } else {
    None
  };
  ReplyBody::Conflicts { windows, next }
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

/// Takes a work's journal and changed files out, for a judge on this shard to read; [`put_back_sealed`] returns them.
/// A move rather than a copy: a verdict on the green's own shard is one step, and nothing else can see the work.
fn take_sealed(state: &mut ShardState, work: DbVolumeId) -> Option<crate::work_home::SealedWork> {
  let w = state.works.get_mut(&work)?;
  Some(crate::work_home::SealedWork {
    work,
    green: w.green,
    base_version: w.base_version,
    journal: std::mem::take(&mut w.journal),
    files: std::mem::take(&mut w.files),
    composed: None,
  })
}

/// Returns what [`take_sealed`] took.
fn put_back_sealed(state: &mut ShardState, sealed: crate::work_home::SealedWork) {
  if let Some(w) = state.works.get_mut(&sealed.work) {
    w.journal = sealed.journal;
    w.files = sealed.files;
  }
}

/// Whether the restatement a submit's accept records would fit the work's partition now (A-207).
pub(crate) fn reset_would_fit(
  state: &ShardState,
  work: DbVolumeId,
  now: u64,
) -> Result<(), Refusal> {
  state
    .db
    .partition()
    .check(&restatement(work, &accepted_restatement(0)), now)
    .map_err(|e| refusal_of_db(&e))
}

/// Serves `SubmitApproved` (A-285): the launcher's approval of exactly one increment, proven under the issuer secret,
/// then the submit with that approval. Refused `Forbidden` for a consumer principal, whatever its proof (an agent never
/// approves its own merge), and `GrantIssuerUnverified` when the proof does not verify.
fn submit_approved(
  state: &mut ShardState,
  principal: &Principal,
  (work, evidence, signature): (VolumeId, Vec<[u8; 32]>, Option<Vec<u8>>),
  identity: [u8; 32],
  proof: [u8; 32],
) -> ReplyBody {
  if matches!(principal, Principal::Consumer { .. }) {
    state.count("merge_approved_by_consumer", 1);
    return forbidden("submit_approved");
  }
  if !approval_verifies(state, principal, work, (&identity, &proof)) {
    state.count("grant_issuer_unverified", 1);
    return refused(Refusal::GrantIssuerUnverified);
  }
  submit(
    state,
    principal,
    work,
    (evidence, signature),
    Approval::Of(identity),
  )
}

/// What a submit carries of the write policy's approval (A-285).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Approval {
  /// None: the volumes' merge modes decide.
  None,
  /// Held by the caller's own ask policy (`SubmitHeld`): asked whatever the volumes' modes, unless they forbid.
  Held,
  /// The launcher's approval of exactly this increment (`SubmitApproved`).
  Of([u8; 32]),
}

/// Whether a `SubmitApproved` is the launcher's approval of `identity` for `work` (A-285): never from a consumer, and
/// its proof the keyed hash under the launcher's secret or the issuer's (`landing::proven_by_launcher`). The work's
/// shard holds a submit whose green lives elsewhere to this before it sends the approval to the verdict
/// (`crate::work_home`), since the verdict trusts what it is sent.
pub(crate) fn approval_verifies(
  state: &ShardState,
  principal: &Principal,
  work: VolumeId,
  (identity, proof): (&[u8; 32], &[u8; 32]),
) -> bool {
  !matches!(principal, Principal::Consumer { .. })
    && crate::landing::proven_by_launcher(
      state,
      |secret| crate::landing::merge_approval_proof(secret, work, identity),
      proof,
    )
}

/// A volume's merge mode under its write policy (A-285): `Bypass` when it has none set, as every merge was before write
/// policies. Read where the volume's record is: a work's on the work's shard, a green's on its owner's (A-247), so a
/// submit carries its work's mode to the verdict ([`SubmitPolicy`]).
pub(crate) fn merge_mode_of(state: &ShardState, id: DbVolumeId) -> slates_db::catalog::WriteMode {
  state
    .db
    .partition()
    .volume(id)
    .and_then(|record| record.policy.write)
    .map_or(slates_db::catalog::WriteMode::Bypass, |policy| policy.merge)
}

/// The merge mode a submit follows (A-285): the stricter of its work's and its green's, and the volume whose mode it is;
/// a tie is the work's.
fn stricter_merge_mode(
  work: (slates_db::catalog::WriteMode, DbVolumeId),
  green: (slates_db::catalog::WriteMode, DbVolumeId),
) -> (slates_db::catalog::WriteMode, DbVolumeId) {
  use slates_db::catalog::WriteMode;
  let rank = |mode: WriteMode| match mode {
    WriteMode::Forbid => 0u8,
    WriteMode::Ask => 1,
    WriteMode::Bypass => 2,
  };
  if rank(green.0) < rank(work.0) {
    green
  } else {
    work
  }
}

/// The refusals a submit into `green_id` meets before its seal, so nothing is composed for a submit that cannot be
/// accepted: a forbidding write policy (A-285), `mode` the one `by` set; then a green that requires evidence, for an
/// increment without any (§4.16 `EvidenceRequired`).
fn refused_before_the_seal(
  state: &mut ShardState,
  (mode, by): (slates_db::catalog::WriteMode, DbVolumeId),
  green_id: DbVolumeId,
  evidence: &[[u8; 32]],
) -> Option<ReplyBody> {
  if mode == slates_db::catalog::WriteMode::Forbid {
    state.count("policy_forbids.merge", 1);
    return Some(refused(Refusal::PolicyForbids {
      act: slates_ipc::protocol::WriteAct::Merge,
      by: slates_ipc::protocol::PolicySubject::Volume {
        volume: to_wire_volume(by),
      },
    }));
  }
  if evidence.is_empty()
    && matches!(
      crate::merge_service::merge_role(state, green_id),
      Some(crate::merge_service::MergeRole::Green {
        require_evidence: true
      })
    )
  {
    return Some(refused(Refusal::EvidenceRequired));
  }
  None
}

/// What a submit's write policy answers once the increment `inc` of `work` is built (A-285). The volumes' asking `mode`
/// asks, and so does the caller's own ask policy (`Approval::Held`) whatever their modes, short of their forbidding it
/// (refused before the seal). An ask merges only the increment an approval names: with none yet it answers what to
/// approve, and an approval of another increment (the work changed since) is refused. A mode that does not ask, or an
/// approval of this increment, answers nothing and the merge goes on. Building the increment changed nothing, so an
/// answer leaves nothing to undo.
fn approval_answer(
  mode: slates_db::catalog::WriteMode,
  approval: Approval,
  (work, green, inc): (DbVolumeId, DbVolumeId, &slates_merge::engine::Increment),
  signed: &JudgedSignature,
) -> Option<ReplyBody> {
  let asks = mode == slates_db::catalog::WriteMode::Ask
    || (approval == Approval::Held && mode == slates_db::catalog::WriteMode::Bypass);
  if !asks {
    return None;
  }
  match approval {
    Approval::Of(identity) if identity == inc.id => None,
    Approval::Of(_) => Some(refused(Refusal::GrantMismatch)),
    Approval::None | Approval::Held => Some(ReplyBody::MergeApprovalRequired {
      work: to_wire_volume(work),
      identity: inc.id,
      paths: u64::try_from(inc.doc.paths.paths().len()).unwrap_or(u64::MAX),
      statement: crate::signed_work::merge_statement(&work.bytes, &green.bytes, inc.base, &inc.id),
      verification: signed.verification,
    }),
  }
}

/// A volume's signed-work rules (A-321): its trusted signers and whether its merges must be signed. Read where the
/// volume's record is: a work's on the work's shard, a green's on its owner's (A-247), so a submit carries its work's
/// to the verdict ([`SubmitPolicy`]) as it carries its work's merge mode.
#[derive(Clone, Debug, Default)]
pub(crate) struct SignedRules {
  /// The trusted signers, in SSH wire form: read only for a submit that carries a signature, the only one they judge.
  pub(crate) signers: Vec<Vec<u8>>,
  /// Whether merges from (a work) or into (a green) the volume must be signed.
  pub(crate) required: bool,
}

/// Whether `id`'s policy, as its record on this shard holds it, requires signed merges (A-321): never for a volume this
/// shard does not hold.
pub(crate) fn requires_signed_merges(state: &ShardState, id: DbVolumeId) -> bool {
  state
    .db
    .partition()
    .volume(id)
    .and_then(|record| record.policy.write)
    .is_some_and(|policy| policy.require_signed_merges)
}

/// `id`'s trusted signers as its record on this shard holds them (A-321): none, for a volume this shard does not hold.
pub(crate) fn signers_of(state: &ShardState, id: DbVolumeId) -> &[Vec<u8>] {
  state
    .db
    .partition()
    .volume(id)
    .map_or(&[], |record| record.policy.signers.as_slice())
}

/// A submit's signature as its verdict judged it (A-321): over the merge's statement, under the work's and the green's
/// trusted signers together.
struct JudgedSignature {
  /// Whether the submit carried a signature: its reply is then `SubmittedSigned`.
  carried: bool,
  /// How it was judged.
  verification: slates_ipc::protocol::Verification,
  /// The volume whose policy requires signed merges, when either does: the work's first, as a tie of merge modes is.
  required_by: Option<DbVolumeId>,
}

/// Judges the signature `policy` carries for `inc`, the increment of `work` into `green` (A-321), on the green's owner,
/// where the green's record is; the work's rules came with the submit. An unsigned submit costs two reads of the
/// records' flags: the statement is built and the signers parsed only for a signature. Counted per verdict.
fn judge_signature(
  state: &mut ShardState,
  (work, green): (DbVolumeId, DbVolumeId),
  inc: &slates_merge::engine::Increment,
  policy: &SubmitPolicy,
) -> JudgedSignature {
  let verification = match policy.signature.as_deref() {
    None => slates_ipc::protocol::Verification::Unsigned,
    Some(signature) => {
      let statement =
        crate::signed_work::merge_statement(&work.bytes, &green.bytes, inc.base, &inc.id);
      let trusted = crate::signed_work::trusted_of(
        policy
          .work_signed
          .signers
          .iter()
          .chain(signers_of(state, green)),
      );
      crate::signed_work::judge(Some(signature), &statement, &trusted)
    }
  };
  if let Some(counter) = crate::signed_work::counter(&verification) {
    state.count(counter, 1);
  }
  let required_by = if policy.work_signed.required {
    Some(work)
  } else {
    requires_signed_merges(state, green).then_some(green)
  };
  JudgedSignature {
    carried: policy.signature.is_some(),
    verification,
    required_by,
  }
}

/// The refusal a volume's signed-merge requirement makes of `signed` (A-321): `SignatureRequired`, naming the volume
/// and how the signature was judged, unless it verified. Met in every write mode when the merge would happen: a bypass
/// merge at once, an asked one at its approval.
fn refused_unsigned(state: &mut ShardState, signed: &JudgedSignature) -> Option<ReplyBody> {
  let by = signed.required_by?;
  if matches!(
    signed.verification,
    slates_ipc::protocol::Verification::Verified { .. }
  ) {
    return None;
  }
  state.count("signature_required.merge", 1);
  Some(refused(Refusal::SignatureRequired {
    by: slates_ipc::protocol::PolicySubject::Volume {
      volume: to_wire_volume(by),
    },
    verification: signed.verification,
  }))
}

/// A conflict's reply (§4.16): `SubmittedSigned` with how the signature was judged when the submit carried one (A-321),
/// else `Submitted`. Nothing is recorded for a conflict.
fn conflicted_reply(
  windows: &[slates_merge::engine::ConflictWindow],
  signed: &JudgedSignature,
) -> ReplyBody {
  let conflicts = merge_windows(windows);
  if signed.carried {
    ReplyBody::SubmittedSigned {
      version: None,
      conflicts,
      verification: signed.verification,
      conflicts_elided: 0,
      verdict: 0,
    }
  } else {
    ReplyBody::Submitted {
      version: None,
      conflicts,
      conflicts_elided: 0,
      verdict: 0,
    }
  }
}

/// The version a submit's reply names as accepted, signed or not (A-321): what a work settles on.
pub(crate) fn accepted_version(reply: &ReplyBody) -> Option<u64> {
  match reply {
    ReplyBody::Submitted {
      version: Some(version),
      ..
    }
    | ReplyBody::SubmittedSigned {
      version: Some(version),
      ..
    } => Some(*version),
    _ => None,
  }
}

/// What a submit brings its verdict from its write policy (A-285): its work's own merge mode, read on the work's shard
/// where the work's record is, and what it carries of an approval: none, its caller's own ask (a consumer's session
/// held it, `SubmitHeld`), or the launcher's approval of one increment, verified before it got here
/// ([`approval_verifies`]). And its signed work (A-321): the signature it carries, if any, and its work's signed-work
/// rules, read with the mode. The green's mode and rules are read where the verdict runs.
#[derive(Clone, Debug)]
pub(crate) struct SubmitPolicy {
  /// The work's merge mode ([`merge_mode_of`]).
  pub(crate) work_mode: slates_db::catalog::WriteMode,
  /// What the submit carries of an approval.
  pub(crate) approval: Approval,
  /// The signature the submit carries over its merge's statement (A-321).
  pub(crate) signature: Option<Vec<u8>>,
  /// The work's signed-work rules ([`requires_signed_merges`], [`signers_of`]).
  pub(crate) work_signed: SignedRules,
}

impl SubmitPolicy {
  /// The policy a submit of `work` carrying `approval` and `signature` brings its verdict, read on the work's shard.
  pub(crate) fn of_work(
    state: &ShardState,
    work: DbVolumeId,
    (approval, signature): (Approval, Option<Vec<u8>>),
  ) -> SubmitPolicy {
    SubmitPolicy {
      work_mode: merge_mode_of(state, work),
      approval,
      work_signed: SignedRules {
        signers: if signature.is_some() {
          signers_of(state, work).to_vec()
        } else {
          Vec::new()
        },
        required: requires_signed_merges(state, work),
      },
      signature,
    }
  }
}

/// Submits a work volume's declared operations to its green as an increment (§4.16): compose the
/// declared operations into the canonical document, seal the post-state, hash the identity, and run
/// the green's merge verdict — accepted with the new version, or the conflict windows to rebase.
/// The verdict is the green's (A-247): here, in this step, when the green is on this shard; otherwise it was asked of
/// the green's owner by `crate::work_home`, and this settles its answer on the work. The write policy is judged with
/// the verdict ([`judge_submission`]), where both volumes' modes are known (A-285).
fn submit(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  (evidence, signature): (Vec<[u8; 32]>, Option<Vec<u8>>),
  approval: Approval,
) -> ReplyBody {
  let (work_id, green_id) =
    match crate::merge_service::require_work(state, principal, work, false, "submit") {
      Ok((record, green)) => (record.id, green),
      Err(refusal) => return refused(refusal),
    };
  let reply = match state.work_home.verdict.take() {
    Some(reply) => {
      // The owner's verdict on a submit the work recorded as pending (A-247): settled here, in this transaction.
      if let Err(e) = settle_pending(state, work_id) {
        return refused(refusal_of_db(&e));
      }
      reply
    }
    None => {
      // The restatement an acceptance records must fit before the verdict (A-207), so a submit is refused rather than
      // accepted with the work's log unable to record it.
      let now = state.clock.monotonic_ns();
      if let Err(refusal) = reset_would_fit(state, work_id, now) {
        return refused(refusal);
      }
      let Some(sealed) = take_sealed(state, work_id) else {
        return refused(Refusal::NotFound);
      };
      let policy = SubmitPolicy::of_work(state, work_id, (approval, signature));
      let reply = judge_submission(state, &sealed, evidence, policy);
      put_back_sealed(state, sealed);
      reply
    }
  };
  settle_submit(state, work_id, green_id, reply)
}

/// Ends the work creation pending under `name` (A-247), in the running verb's transaction.
pub(crate) fn settle_creation(
  state: &mut ShardState,
  name: &str,
) -> Result<(), slates_db::DbError> {
  if state.db.partition().pending_creation(name).is_none() {
    return Ok(());
  }
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(
      &mut state.segment,
      &Op::WorkCreationSettled {
        name: name.to_owned(),
      },
      now,
    )
    .map(|_| ())
}

/// Clears `work`'s pending submit (A-247), in the transaction that settles its verdict.
fn settle_pending(state: &mut ShardState, work: DbVolumeId) -> Result<(), slates_db::DbError> {
  if state.db.partition().pending_submit(work).is_none() {
    return Ok(());
  }
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &Op::WorkSubmitSettled { work }, now)
    .map(|_| ())
}

/// Settles a verdict on the work, in the verb's transaction: an accept moves the work's base to the new version and
/// clears what it held, and tells the green's owner the floor moved; a conflict is kept with the work
/// ([`kept_conflict`]). Anything else leaves the work as it was.
fn settle_submit(
  state: &mut ShardState,
  work_id: DbVolumeId,
  green_id: DbVolumeId,
  reply: ReplyBody,
) -> ReplyBody {
  let Some(version) = accepted_version(&reply) else {
    return kept_conflict(state, work_id, reply);
  };
  // A retry's answer for a work already past that version has nothing left to settle. At that very version it resets
  // again, which an identical increment's accept needs (its version is the head the work is based on).
  if state
    .works
    .get(&work_id)
    .is_none_or(|w| w.base_version > version)
  {
    return reply;
  }
  let now = state.clock.monotonic_ns();
  if let Err(e) = state.db.mutate(
    &mut state.segment,
    &restatement(work_id, &accepted_restatement(version)),
    now,
  ) {
    return refused(refusal_of_db(&e));
  }
  reset_accepted_work(state, work_id, version);
  crate::work_home::tell_floor(state, green_id, work_id, Some(version));
  reply
}

/// The owner's half of a submit (§4.16, A-247): judges a work's sealed increment against the green and commits it, as
/// the single serialized step of the green. Runs in the transaction of the verb that asked, whether on this shard or
/// through [`owner_judge`]. The work's own reset is the caller's to settle.
fn judge_submission(
  state: &mut ShardState,
  sealed: &crate::work_home::SealedWork,
  evidence: Vec<[u8; 32]>,
  policy: SubmitPolicy,
) -> ReplyBody {
  let green_id = sealed.green;
  // The write policy (A-285): the stricter of the work's merge mode, read on the work's shard and carried here, and the
  // green's, read here. A forbidding mode refuses before the increment is built; an asking one, or the caller's own
  // ask (`Approval::Held`), is answered below, once the increment it would approve is known. Read here alone, a work on
  // another shard than its green would merge under the green's mode only, since this partition holds no record of it
  // (A-247).
  let (mode, by) = stricter_merge_mode(
    (policy.work_mode, sealed.work),
    (merge_mode_of(state, green_id), green_id),
  );
  if let Some(refusal) = refused_before_the_seal(state, (mode, by), green_id, &evidence) {
    return refusal;
  }
  // The submission barrier (§4.16 "Submission": "seal the work volume"): the seal is taken here, in
  // this verb, on the single-threaded owner shard — every declared operation before it is in the
  // increment, and a write that arrives after it is a later verb that lands in the next increment,
  // never this one.
  let (green_id, inc) = match build_increment(state, sealed, evidence) {
    Ok(built) => built,
    Err(refusal) => return refused(refusal),
  };
  // Signed work (A-321): the signature is judged over the merge's statement, once the increment it names is built. It
  // permits nothing — the modes above did — and is recorded with the version as its provenance. An asked merge answers
  // with the verdict, so the approver sees it and a signer learns the statement from a held submit, whatever the
  // requirement; a volume that requires signed merges refuses one that did not verify when it would merge, here.
  let signed = judge_signature(state, (sealed.work, green_id), &inc, &policy);
  if let Some(answer) = approval_answer(
    mode,
    policy.approval,
    (sealed.work, green_id, &inc),
    &signed,
  ) {
    return answer;
  }
  if let Some(refusal) = refused_unsigned(state, &signed) {
    return refusal;
  }
  // The increment is recorded durably so a restart replays it (§4.8, §4.16). Guard-then-apply: the
  // chain must be able to hold it *before* the verdict commits, so an accepted increment is always
  // recorded — otherwise a full-chain refusal after an in-memory commit would strand the green ahead
  // of its log, hidden by the engine's idempotent `seen` cache on retry. A full chain refuses every
  // submit (it cannot advance regardless of the verdict).
  // What the chain will hold is the increment and what its verdict resolves it to (A-257), known only after the verdict,
  // so the admission charges the most an entry for this increment can be.
  let increment_bytes = inc.encode();
  let bound = slates_merge::chain_entry::entry_bound(increment_bytes.len(), &inc);
  let admission = Op::GreenAdvanced {
    green: green_id,
    increment: vec![0; bound],
  };
  let now = state.clock.monotonic_ns();
  if let Err(refusal) = chain_admits(state, green_id, &admission, now) {
    return refused(refusal);
  }
  // Charge superseded values before the verdict. Incoming bytes do not bound the retained
  // history: unlink carries none and a short overwrite retains the complete previous file.
  let Some(engine) = state.greens.get(&green_id) else {
    return refused(Refusal::NotFound);
  };
  // The change index grows with the version too (A-264), charged with the history it indexes.
  let secured = u64::try_from(
    engine
      .history_reservation(&inc)
      .saturating_add(engine.changes_reservation(&inc)),
  )
  .unwrap_or(u64::MAX);
  // An accepted work is reset to its green at the new version, where it holds nothing of its own
  // (`crate::work_files`): the reset only releases charge, so nothing is secured for it here.
  // Make room first: fold whatever the retention budget allows, leaving room for what is secured (never a live
  // reader's version).
  let room = usize::try_from(secured).unwrap_or(usize::MAX);
  if let Err(available) =
    crate::merge_service::settle_green_retention_leaving(state, green_id, room)
  {
    return refused(Refusal::BudgetExceeded { available });
  }
  if let Err(available) = crate::merge_service::secure_green_retention(state, green_id, secured) {
    report_first_budget_refusal(state, "retention", secured, available);
    return refused(Refusal::BudgetExceeded { available });
  }
  let verdict_start = state.clock.monotonic_ns();
  let (outcome, applied) = {
    let Some(engine) = state.greens.get_mut(&green_id) else {
      return refused(Refusal::NotFound);
    };
    engine.submit_resolved(&inc)
  };
  // The `merge.verdict` chokepoint span (§4.14): one increment judged (accept, identical, or conflict),
  // opened within the `shard.op` span of the submit that caused it (set in `run_recorded`). The label
  // is content-free — accepted (1) or conflict (0). It never carries a path, a name or bytes.
  let verdict_end = state.clock.monotonic_ns();
  let accepted = u32::from(matches!(
    &outcome,
    slates_merge::engine::Outcome::Accepted { .. }
  ));
  let verdict = match state.current_span {
    Some(cause) => state
      .tracer
      .open_within(&cause, Chokepoint::MergeVerdict, verdict_start),
    // A submit always runs inside a recorded verb; without its span the cause is declared missing.
    None => state.tracer.open_unlinked(
      RequestId::default(),
      Chokepoint::MergeVerdict,
      verdict_start,
    ),
  };
  crate::telemetry::emit(state, verdict.end(accepted, verdict_end));
  match outcome {
    slates_merge::engine::Outcome::Accepted { version } => {
      // The wait goes first, in this transaction, so a version is never durable without it (A-252).
      if let Err(e) = crate::merge_service::stage_awaiting(state, green_id, version) {
        return refused(refusal_of_db(&e));
      }
      // The pre-check passed and the shard is single-threaded, so this append fits the budget; a
      // segment-full failure refuses like any other verb. A resubmit the engine answered from its
      // idempotent accept names a version the chain already holds: nothing is appended twice.
      // The durable head is the checkpoint's version and the increments after it (W3).
      if let Err(refusal) = record_accepted(
        state,
        (green_id, sealed.work),
        version,
        (&increment_bytes, &inc),
        (applied.as_deref(), &signed.verification),
      ) {
        return refused(refusal);
      }
      // The version's merge record, issued only once its inputs — the increment just appended — are
      // placed (§4.16 "Commit"; at `f = 0` the append is the placement).
      let completion = crate::merge_service::SubmitCompletion::of_current(state, sealed.work);
      crate::merge_service::enqueue_record(
        state,
        green_id,
        version,
        inc.id,
        inc.base,
        inc.evidence,
        (completion, Some(sealed.work)),
      );
      // True the retention charge up to what the commit retained (the secured surplus credited). A settle after an
      // accept only credits: nothing was retained beyond what was secured. The work's floor moves when it has settled
      // (`crate::work_home::tell_floor`), which folds the histories and may checkpoint the chain (W3).
      let _ = crate::merge_service::settle_green_retention(state, green_id);
      // The acceptance is published only once the version's merge record is **committed at the
      // quorum** (§4.16 "Commit": "committed at f+1 acknowledgements … issued only when every identity
      // the version references is placed"; AUD-11). At `f = 0` the append was the commit and the reply
      // goes now; at `f > 0` the reply and the completion record wait for the record plane
      // (`merge_service::resolve_accepted`), so a client is never told a version a surviving quorum
      // may not hold.
      let object = ObjectId(green_id.bytes);
      if crate::merge_service::placed_version(state, object).is_none_or(|placed| placed < version) {
        crate::merge_service::defer_acceptance(state, green_id, version);
      }
      // The version's own record answers (A-321): the signature recorded with it, which for an increment the engine
      // had accepted before is that first submit's.
      crate::merge_service::accepted_reply(state, green_id, version)
    }
    slates_merge::engine::Outcome::Conflict { windows } => {
      // The work is not reset: the bound secured for a reset is released.
      settle_after_conflict(state, green_id, &inc.id);
      // Every window, for the work's shard to keep with the work and carry as far as one reply holds
      // ([`kept_conflict`], A-267): this may be the green's owner, which holds no work on another shard.
      conflicted_reply(&windows, &signed)
    }
    // A base below the green's checkpoint (W3) is no live reader's, so it is unknown: never judged, and nothing
    // retained, so the bytes secured are credited back.
    slates_merge::engine::Outcome::BaseUnavailable { base, .. } => {
      let _ = crate::merge_service::settle_green_retention(state, green_id);
      refused(Refusal::UnknownBase {
        green: to_wire_volume(green_id),
        version: base,
      })
    }
  }
}

/// Whether green `green`'s chain can hold `record`, an increment about to be judged: checked guard-then-apply. A chain
/// at its cap is checkpointed at its floor first, the doubling rule waived (A-238): the floor may have risen since the
/// last accept (a work destroyed or rebased), and a full chain refuses every submit whose accept would take the
/// checkpoint, so without it the green stayed full for good. What is still full is refused `TableAtCapacity`.
/// Appends accepted `version`'s entry to `green`'s durable chain, unless an idempotent accept already holds it (A-257): the
/// increment as submitted and what its verdict resolved it to, with the completion key of the submit from `work` that made
/// it (A-255, A-288) and how its signature was judged (A-321: the version's provenance). At a version whose record goes to
/// holders the head's identity is read anyway, and goes beside it for a replay to compare; at `f = 0` the entries after
/// the last checkpoint are checked by their digest alone.
fn record_accepted(
  state: &mut ShardState,
  (green, work): (DbVolumeId, DbVolumeId),
  version: u64,
  (increment_bytes, increment): (&[u8], &slates_merge::engine::Increment),
  (applied, verification): (
    Option<&[slates_merge::chain_entry::Applied]>,
    &slates_ipc::protocol::Verification,
  ),
) -> Result<(), Refusal> {
  if state.db.partition().green_head(green) >= version {
    return Ok(());
  }
  let identity = crate::merge_service::record_goes_to_holders(state, green)
    .then(|| {
      state
        .greens
        .get_mut(&green)
        .map(slates_merge::engine::Green::head_identity)
    })
    .flatten();
  let record = Op::GreenAdvanced {
    green,
    increment: slates_merge::chain_entry::encode(
      increment_bytes,
      increment,
      applied.unwrap_or_default(),
      crate::merge_service::SubmitCompletion::of_current(state, work)
        .map(crate::merge_service::SubmitCompletion::to_entry),
      identity,
      crate::signed_work::to_entry(verification),
    ),
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &record, now)
    .map_err(|e| refusal_of_db(&e))?;
  // The version's provenance (A-321), once its entry is durable: what the version's answers and its merge record read,
  // and what a checkpoint keeps when it folds the entry.
  if let (Some(signature), Some(engine)) = (
    crate::signed_work::to_entry(verification),
    state.greens.get_mut(&green),
  ) {
    engine.record_signature(version, signature);
  }
  Ok(())
}

fn chain_admits(
  state: &mut ShardState,
  green: DbVolumeId,
  record: &Op,
  now: u64,
) -> Result<(), Refusal> {
  if let Err(slates_db::DbError::Capacity { .. }) = state.db.partition().check(record, now) {
    crate::merge_service::checkpoint_green_when(state, green, crate::merge_service::Doubling::Now);
  }
  state
    .db
    .partition()
    .check(record, now)
    .map_err(|e| refusal_of_db(&e))
}

/// An accepted work now equals its green at `version` (§4.16 "Submission"): its journal is consumed (the increment
/// holds it) and its base moves to the version, where every file it had changed now is, so a later edit declares
/// against what the green holds (a work with `stream` submits at every auto-seal on exactly this footing; without
/// the reset a second submit would re-declare the already-merged operations against the old base). It then holds
/// nothing of its own (`crate::work_files`), so the reset is O(1) whatever the green holds — it copied every file of
/// the green into the work before 2026-11-04 — and its charge falls to nothing (`crate::work_charge`).
fn reset_accepted_work(state: &mut ShardState, work_id: DbVolumeId, version: u64) {
  let Some(w) = state.works.get_mut(&work_id) else {
    return;
  };
  w.base_version = version;
  w.journal.clear();
  w.files.clear();
  // The entries were the old base's (A-247).
  w.entries.clear();
  w.revision = w.revision.wrapping_add(1);
  // Accepted: no conflicting verdict stands, so a read of an earlier one is refused (A-267).
  w.verdict = None;
  // The recount of what it now keeps: nothing, so the charge only falls and cannot be refused.
  let kept = crate::work_charge::footprint(&w.files, &w.journal);
  let _ = crate::work_charge::set_to(state, work_id, kept);
}

/// The retention settle after a refused submit: nothing was retained but the rejected result, so the
/// bytes secured before the verdict are credited back and the cache's growth charged in their place;
/// a budget that cannot cover even that drops the result from the cache (the verdict stands; a retry
/// is judged again) and counts it as an eviction, then settles to what remains.
fn settle_after_conflict(state: &mut ShardState, green_id: DbVolumeId, increment: &[u8; 32]) {
  if crate::merge_service::settle_green_retention(state, green_id).is_err() {
    if let Some(engine) = state.greens.get_mut(&green_id) {
      engine.forget_rejected(increment);
    }
    let _ = crate::merge_service::settle_green_retention(state, green_id);
  }
}

/// Rebases a work volume onto its green's head (§4.16 "Rebase, the only corrective path"): compose the
/// same increment `submit` would, run the verdict without committing to the green, and — when every
/// operation maps cleanly — move the work onto the head, restating its base, its content and its
/// journal in head coordinates so a later submit composes with no further mapping. A conflict returns
/// the windows and changes nothing. The green is never changed by a rebase.
fn rebase(state: &mut ShardState, principal: &Principal, work: VolumeId) -> ReplyBody {
  let (work_id, green_id) =
    match crate::merge_service::require_work(state, principal, work, false, "rebase") {
      Ok((record, green)) => (record.id, green),
      Err(refusal) => return refused(refusal),
    };
  let answer = match state.work_home.rebased.take() {
    Some(answer) => answer,
    None => {
      let Some(sealed) = take_sealed(state, work_id) else {
        return refused(Refusal::NotFound);
      };
      let answer = map_rebase(state, &sealed);
      put_back_sealed(state, sealed);
      answer
    }
  };
  settle_rebase(state, work_id, green_id, answer)
}

/// The owner's half of a rebase (A-247): maps a work's sealed increment through the green's deltas. The green is never
/// changed by a rebase.
fn map_rebase(
  state: &mut ShardState,
  sealed: &crate::work_home::SealedWork,
) -> crate::work_home::RebaseAnswer {
  use crate::work_home::RebaseAnswer;
  let (green_id, inc) = match build_increment(state, sealed, Vec::new()) {
    Ok(built) => built,
    Err(refusal) => return RebaseAnswer::Reply(refused(refusal)),
  };
  let Some(engine) = state.greens.get_mut(&green_id) else {
    return RebaseAnswer::Reply(refused(Refusal::NotFound));
  };
  match engine.rebase(&inc) {
    slates_merge::engine::Rebased::Rebased {
      version,
      changed,
      journal,
    } => {
      // A work that composed its own increment composes its next one too: it is given the entries it will need.
      let entries = if sealed.composed.is_some() {
        journal
          .iter()
          .flat_map(VolumeOp::names)
          .flatten()
          .map(|path| (path.to_owned(), engine.path_base(version, path)))
          .collect()
      } else {
        std::collections::BTreeMap::new()
      };
      RebaseAnswer::Rebased {
        version,
        changed,
        journal,
        entries,
      }
    }
    slates_merge::engine::Rebased::Conflict { windows } => {
      // Every window, for the work's shard to keep with the work and carry as far as one reply holds
      // ([`kept_conflict`], A-267): this may be the green's owner, which holds no work on another shard.
      RebaseAnswer::Reply(ReplyBody::Rebased {
        version: None,
        conflicts: merge_windows(&windows),
        conflicts_elided: 0,
        verdict: 0,
      })
    }
    // A base below the green's checkpoint (W3): unknown, as the submit answers it.
    slates_merge::engine::Rebased::BaseUnavailable { base, .. } => {
      RebaseAnswer::Reply(refused(Refusal::UnknownBase {
        green: to_wire_volume(green_id),
        version: base,
      }))
    }
  }
}

/// Settles a rebase on the work: the work moves onto the head; the engine returned only what the work's mapped edits
/// leave different from the head (`Rebased::changed`), which becomes the work's delta over its new base
/// (`crate::work_files`): O(what the work changed). The restatement's log entry is checked first (A-207), then its new
/// footprint is charged: refused either way, the work stays where it was.
fn settle_rebase(
  state: &mut ShardState,
  work_id: DbVolumeId,
  green_id: DbVolumeId,
  answer: crate::work_home::RebaseAnswer,
) -> ReplyBody {
  let (version, changed, journal, entries) = match answer {
    crate::work_home::RebaseAnswer::Rebased {
      version,
      changed,
      journal,
      entries,
    } => (version, changed, journal, entries),
    crate::work_home::RebaseAnswer::Reply(reply) => return kept_conflict(state, work_id, reply),
  };
  let changed = crate::work_files::WorkFiles::from_changed(changed);
  let wanted = crate::work_charge::footprint(&changed, &journal).saturating_add(
    entries
      .iter()
      .map(|(path, base)| crate::work_charge::base_footprint(path, base))
      .fold(0, u64::saturating_add),
  );
  let op = restatement(
    work_id,
    &crate::work_log::WorkLogEntry::restated(version, &changed, &journal),
  );
  let now = state.clock.monotonic_ns();
  if let Err(e) = state.db.partition().check(&op, now) {
    return refused(refusal_of_db(&e));
  }
  let charged_before = state.works.get(&work_id).map_or(0, |w| w.charged);
  if let Err(available) = crate::work_charge::set_to(state, work_id, wanted) {
    return refused(Refusal::BudgetExceeded { available });
  }
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    // Not recorded, so the work stays where it was and so does its charge (a return to what it held cannot be
    // refused).
    let _ = crate::work_charge::set_to(state, work_id, charged_before);
    return refused(refusal_of_db(&e));
  }
  if let Some(w) = state.works.get_mut(&work_id) {
    w.files = changed;
    w.base_version = version;
    w.journal = journal;
    w.entries = entries;
    w.revision = w.revision.wrapping_add(1);
    // Rebased: no conflicting verdict stands (A-267).
    w.verdict = None;
  }
  // The work's base rose, so the reachable floor may have: the owner's floor moves, folds the green's histories to it
  // and credits the retention released.
  crate::work_home::tell_floor(state, green_id, work_id, Some(version));
  ReplyBody::Rebased {
    version: Some(version),
    conflicts: Vec::new(),
    conflicts_elided: 0,
    verdict: 0,
  }
}

/// A transaction of one op on this partition, for what the green's owner records on a work's behalf (A-247).
fn owner_record(state: &mut ShardState, op: &Op) -> Result<(), slates_db::DbError> {
  let now = state.clock.monotonic_ns();
  state.db.begin();
  let applied = state.db.mutate(&mut state.segment, op, now);
  match (applied, state.db.commit(&mut state.segment)) {
    (Ok(_), Ok(_)) => Ok(()),
    (Err(error), _) | (_, Err(error)) => {
      reconcile_unpublished_effects(state);
      Err(error)
    }
  }
}

/// Admits a work over `green` for its own partition (A-247): the rights a creation needs and the green's head, the
/// work's floor recorded with it.
pub(crate) fn owner_admit(
  state: &mut ShardState,
  principal: &Principal,
  green: DbVolumeId,
  work: DbVolumeId,
) -> Result<crate::work_home::Admitted, Refusal> {
  let record =
    crate::merge_service::require_green(state, principal, to_wire_volume(green), "create_work")?;
  let engine = state.greens.get(&record.id).ok_or(Refusal::NotFound)?;
  let base = engine.head();
  owner_record(
    state,
    &Op::WorkFloorSet {
      green: record.id,
      work,
      base,
    },
  )
  .map_err(|error| refusal_of_db(&error))?;
  Ok(crate::work_home::Admitted { id: work, base })
}

/// The base's bytes of `paths` for a verb on a work's shard (A-247).
pub(crate) fn owner_files(
  state: &ShardState,
  green: DbVolumeId,
  version: u64,
  paths: &[String],
) -> crate::work_files::FetchedBase {
  let mut fetched = crate::work_files::FetchedBase::default();
  let Some(engine) = state.greens.get(&green) else {
    fetched.gone = true;
    return fetched;
  };
  for path in paths {
    let file = engine
      .content_ref_at(path, version)
      .map(std::borrow::Cow::into_owned);
    fetched.files.insert(path.clone(), file);
    // With the path's base entries, so the work composes its increment itself (A-247).
    fetched
      .entries
      .insert(path.clone(), engine.path_base(version, path));
  }
  fetched
}

/// A page of the base's `path` for a paged read on a work's shard (A-247).
pub(crate) fn owner_page(
  state: &ShardState,
  green: DbVolumeId,
  version: u64,
  path: String,
  (offset, want): (u64, u64),
) -> crate::work_files::FetchedBase {
  let mut fetched = crate::work_files::FetchedBase::default();
  let Some(engine) = state.greens.get(&green) else {
    fetched.gone = true;
    return fetched;
  };
  let page = engine.content_page_at(&path, version, (offset, want));
  fetched.page = Some(crate::work_files::FetchedPage {
    path,
    offset,
    answer: page,
  });
  fetched
}

/// Every file of the base with its length, for a listing or a status on a work's shard (A-247).
pub(crate) fn owner_sizes(
  state: &ShardState,
  green: DbVolumeId,
  version: u64,
) -> crate::work_files::FetchedBase {
  let mut fetched = crate::work_files::FetchedBase::default();
  let Some(engine) = state.greens.get(&green) else {
    fetched.gone = true;
    return fetched;
  };
  fetched.sizes = Some(
    engine
      .file_lens_at(version)
      .into_iter()
      .map(|(path, len)| (path.to_owned(), u64::try_from(len).unwrap_or(u64::MAX)))
      .collect(),
  );
  fetched
}

/// The owner's step of a submit asked from a work's shard (A-247): the request's completion is the owner's to keep, so
/// a retry meets it whatever the work now holds. Returns the reply, or `None` when the acceptance waits for its record
/// to commit at the quorum (AUD-11) or joined one that does; `back` then carries it to the work's shard.
pub(crate) fn owner_judge(
  state: &mut ShardState,
  (origin, id): (u64, RequestId),
  (sealed, evidence, policy): (crate::work_home::SealedWork, Vec<[u8; 32]>, SubmitPolicy),
  back: crate::merge_service::ReplyRoute,
) -> Option<ReplyBody> {
  match state
    .db
    .partition()
    .completion(origin, id.client, id.sequence)
  {
    Seen::Completed(bytes) => {
      return Some(
        ReplyBody::from_bytes(&bytes).unwrap_or_else(|_| refused(Refusal::DuplicateRequest)),
      );
    }
    Seen::Acknowledged => return Some(refused(Refusal::DuplicateRequest)),
    // It ran and waits for its version's commit (A-252): answered, or joined to the wait with this route, never run
    // again, whatever the work sends now.
    Seen::Awaiting { object, version } => {
      state.reply_route = Some(back);
      let answered = crate::merge_service::serve_awaiting(state, origin, id, (object, version));
      state.reply_route = None;
      return answered;
    }
    Seen::New => {
      state.reply_route = Some(back);
      if crate::merge_service::join_awaiting(state, origin, id) {
        state.reply_route = None;
        return None;
      }
      state.reply_route = None;
    }
  }
  let verb = RequestBody::Submit {
    work: to_wire_volume(sealed.work),
    evidence: Vec::new(),
  };
  if let Some(owed) = acknowledgement_owed(state, origin, id.client, &verb) {
    return Some(owed);
  }
  state.db.begin();
  state.current_request = Some((origin, id));
  state.reply_route = Some(back);
  let reply = judge_submission(state, &sealed, evidence, policy);
  state.current_request = None;
  state.reply_route = None;
  let deferred = std::mem::take(&mut state.acceptance_deferred);
  let reply = if deferred {
    reply
  } else {
    record_completion(state, origin, id, reply)
  };
  match state.db.commit(&mut state.segment) {
    Ok(_) => (!deferred).then_some(reply),
    Err(error) => {
      reconcile_unpublished_effects(state);
      Some(refused(refusal_of_db(&error)))
    }
  }
}

/// The owner's step of a rebase asked from a work's shard (A-247): nothing of the green changes, so it records nothing
/// and a retry runs it again.
pub(crate) fn owner_rebase(
  state: &mut ShardState,
  sealed: crate::work_home::SealedWork,
) -> crate::work_home::RebaseAnswer {
  map_rebase(state, &sealed)
}

/// Moves or releases a work's floor on its green's partition, and folds what that frees (A-247).
pub(crate) fn owner_floor(
  state: &mut ShardState,
  green: DbVolumeId,
  work: DbVolumeId,
  base: Option<u64>,
) {
  let op = match base {
    Some(base) => Op::WorkFloorSet { green, work, base },
    None => Op::WorkFloorDropped { green, work },
  };
  if owner_record(state, &op).is_err() {
    return;
  }
  let _ = crate::merge_service::settle_green_retention(state, green);
  crate::merge_service::checkpoint_green(state, green);
}

fn clone(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: SnapshotId,
  name: &str,
) -> ReplyBody {
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Clone) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("clone");
  }
  if let Some(existing) = name_held(state, name) {
    return refused(Refusal::AlreadyExists { existing });
  }
  // A name that hashes to another partition is held there by an alias of the clone, which lives here beside its origin
  // for copy-on-write (A-236, §4.8 "Lookup"): the clone is made across the two partitions by the alias protocol.
  let name_partition = owner_of_name(name, state.shards.len());
  if name_partition != state.partition {
    return clone_named_elsewhere(state, principal, (volume, snapshot, name), name_partition);
  }
  let admitted = match admit_clone(state, handle, &record, snapshot) {
    Ok(admitted) => admitted,
    Err(reply) => return *reply,
  };
  match publish_clone(
    state,
    principal,
    handle,
    &record,
    (snapshot, name),
    admitted,
  ) {
    Ok(id) => ReplyBody::Cloned {
      id: to_wire_volume(id),
    },
    Err(refusal) => refused(refusal),
  }
}

/// The volume holding `name` on this partition: one of its own volumes, or one another partition holds under an alias
/// here (A-236), confirmed or reserved.
pub(crate) fn name_held(state: &ShardState, name: &str) -> Option<VolumeId> {
  let partition = state.db.partition();
  partition
    .volume_by_name(name)
    .map(|volume| to_wire_volume(volume.id))
    .or_else(|| {
      partition
        .name_alias(name)
        .map(|alias| to_wire_volume(alias.volume))
    })
    .or_else(|| {
      // A work being admitted by its green's owner holds its name meanwhile (A-247).
      partition
        .pending_creation(name)
        .map(|pending| to_wire_volume(pending.work))
    })
}

/// Records one step of a name alias on this, the name's, partition (A-236): its own durable record.
pub(crate) fn alias_step(state: &mut ShardState, op: &Op) -> Result<(), Refusal> {
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, op, now)
    .map(drop)
    .map_err(|e| refusal_of_db(&e))
}

/// Format: the status counter of alias steps whose name's shard did not take them (A-236): a reserved alias left for
/// the next recovery to confirm or release, so the name resolves nothing until then.
pub(crate) const ALIAS_STEP_UNTAKEN: &str = "names.alias_step_untaken";

/// Makes a clone whose name hashes to `name_partition`, another partition (A-236, `slates_db::alias::CLONE_STEPS`):
/// the alias reserved at the name's partition, the clone made here beside its origin with its completion record, and
/// the alias confirmed (or released, when the clone could not be made) before the reply. The reply is deferred to a
/// task of this shard, so a retry meanwhile joins it.
fn clone_named_elsewhere(
  state: &mut ShardState,
  principal: &Principal,
  (volume, snapshot, name): (VolumeId, SnapshotId, &str),
  name_partition: u16,
) -> ReplyBody {
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a clone outside a recorded request".to_owned(),
    });
  };
  let Some(name_shard) = shard_of_partition(state, name_partition) else {
    return refused(Refusal::NotFound);
  };
  let Some(id) = fresh_volume_id(state) else {
    return refused(ids_exhausted());
  };
  let this = state.shard;
  let principal = principal.clone();
  let name = name.to_owned();
  crate::image::defer(state, async move {
    let reservation = Op::NameAliasReserved {
      record: slates_db::catalog::NameAliasRecord {
        name: name.clone(),
        volume: id,
        confirmed: false,
      },
    };
    let reserved = crate::xshard::call_within(
      this,
      name_shard,
      move |s| alias_step(s, &reservation),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
    .unwrap_or_else(|| {
      Err(Refusal::Unpublished {
        reason: "the name's shard did not answer its reservation".to_owned(),
      })
    });
    let made = crate::state::with_state(|s| {
      crate::image::complete(s, request, |s| match &reserved {
        Err(refusal) => refused(refusal.clone()),
        Ok(()) => clone_as(s, &principal, (volume, snapshot, &name), id),
      })
    });
    let Some((reply, route)) = made else {
      return;
    };
    if reserved.is_ok() {
      let settle = if matches!(reply, ReplyBody::Cloned { .. }) {
        Op::NameAliasConfirmed { name }
      } else {
        Op::NameAliasReleased { name }
      };
      let settled = crate::xshard::call_within(
        this,
        name_shard,
        move |s| alias_step(s, &settle),
        crate::daemon::LIVENESS_BUDGET_NS,
      )
      .await;
      if !matches!(settled, Some(Ok(()))) {
        let _ = crate::state::with_state(|s| s.count(ALIAS_STEP_UNTAKEN, 1));
      }
    }
    let _ = crate::state::with_state(|s| crate::image::deliver(s, reply, route));
  })
}

/// The clone step of [`clone_named_elsewhere`], on the origin's partition with the name reserved: the origin found and
/// the caller's right checked again (the volume could have gone while the reservation was asked for), the clone made
/// under the id the alias names, and published before its completion; the reply.
fn clone_as(
  state: &mut ShardState,
  principal: &Principal,
  (volume, snapshot, name): (VolumeId, SnapshotId, &str),
  id: DbVolumeId,
) -> ReplyBody {
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Clone) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("clone");
  }
  if let Some(existing) = state.db.partition().volume_by_name(name) {
    return refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    });
  }
  let admitted = match admit_clone(state, handle, &record, snapshot) {
    Ok(admitted) => admitted,
    Err(reply) => return *reply,
  };
  match publish_clone_as(
    state,
    principal,
    handle,
    &record,
    (snapshot, name),
    admitted,
    id,
  )
  .and_then(|id| crate::image::publish_new_volume(state, id).map(|()| id))
  {
    Ok(id) => ReplyBody::Cloned {
      id: to_wire_volume(id),
    },
    Err(refusal) => refused(refusal),
  }
}

/// Releases the alias the name's partition holds for `id`, a volume of this partition destroyed under `name` (A-236,
/// `slates_db::alias::DESTROY_STEPS`). Nothing when the name hashes here, which holds no alias for it. The release is a
/// message to the name's shard, which takes it only while its alias still names `id`; one not taken (refused,
/// counted) is the next recovery's to release.
pub(crate) fn release_name_alias(state: &ShardState, id: DbVolumeId, name: &str) {
  let name_partition = owner_of_name(name, state.shards.len());
  if name_partition == state.partition {
    return;
  }
  let Some(name_shard) = shard_of_partition(state, name_partition) else {
    return;
  };
  let name = name.to_owned();
  crate::xshard::run_on_counted(state.shard, name_shard, move |s| {
    if s
      .db
      .partition()
      .name_alias(&name)
      .is_some_and(|alias| alias.volume == id)
      && alias_step(s, &Op::NameAliasReleased { name }).is_err()
    {
      s.count(ALIAS_STEP_UNTAKEN, 1);
    }
  });
}

/// Admits a clone of `record`'s volume (in slot `handle`) at `snapshot` (§4.2: every credit reserved, or none) and
/// makes it, pinning the snapshot. Not recorded: [`clone`] records it at once, and an image merge once the clone holds
/// the image's tree (A-160). A refusal gives back what was reserved.
pub(crate) fn admit_clone(
  state: &mut ShardState,
  handle: Handle<VolumeSlot>,
  record: &VolumeRecord,
  snapshot: SnapshotId,
) -> Result<Admitted, Box<ReplyBody>> {
  let size = match record.policy.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  };
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return Err(Box::new(refused(Refusal::BudgetExceeded { available })));
      }
      Err(e) => {
        return Err(Box::new(refused(refusal_of_mem(&e))));
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  // Take the clone's version slots *before* cloning (§4.2: reserve all credits or none before publishing), so a refusal
  // allocates nothing and does not leave the origin's `clone_refs` bumped. A clone shares its origin's versions until
  // it diverges. A bounded clone reserves its whole allowance — a create's fair share, but never below the count it
  // inherits from the origin (equal to the origin's live count at clone time) — as the sacred claim that backs full
  // divergence of the inherited inodes. A dynamic clone holds slots for the inodes it inherits only, from the
  // unpromised capacity, and grows its hold as it makes more (A-342). If the slab cannot back them, the clone is refused.
  let inherited = match state.volumes.get(handle) {
    Ok(slot) => slot.volume.inode_usage().0,
    Err(_) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        Refusal::NotFound,
      )));
    }
  };
  let clone_allowance = inode_allowance(state, size).max(inherited);
  let dynamic = matches!(size, SizeClass::Dynamic { .. });
  let taken = if dynamic {
    state.store.versions.grow(inherited)
  } else {
    state.store.versions.reserve(clone_allowance)
  };
  let version_credit = match taken {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BudgetExceeded { available },
      )));
    }
    Err(e) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        refusal_of_mem(&e),
      )));
    }
  };
  let names = wire_names(record.policy.names);
  let quota = quota_for(size);
  let config = volume_config(state, names, quota);
  // The clone's records are reserved against the metadata ledger before it exists (§4.2).
  let metadata_credit = match reserve_metadata(state, config.journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        None,
        refusal,
      )));
    }
  };
  // A clone of a strict volume is strict (its record is the source's): its own entitlement is reserved (§4.2 D-12).
  let locked = record.policy.require_locked;
  let lock_credit = match locked.then(|| reserve_locked(state, size)).transpose() {
    Ok(credit) => credit,
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal,
      )));
    }
  };
  // An overlay's clone reads the same base directories through a host of its own, made before the clone pins the
  // snapshot so a refusal here has nothing to unpin.
  let host = match clone_base_host(state, handle, record) {
    Ok(host) => host,
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal,
      )));
    }
  };
  let cloned = match state.volumes.get_mut(handle) {
    Ok(slot) => Volume::clone_of(
      &state.store,
      &mut slot.volume,
      core_snapshot(snapshot),
      config,
    ),
    Err(_) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        Refusal::NotFound,
      )));
    }
  };
  let mut volume_core = match cloned {
    Ok(v) => v,
    Err(e) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal_of_vfs(&e),
      )));
    }
  };
  volume_core.set_locked(locked);
  // Cap the clone's inode dimension at the same allowance already reserved above, so its per-volume
  // cap (`next_no`) and its version-slab reservation agree. A clone previously carried no inode cap.
  let allowed = volume_core.set_inode_allowance(clone_allowance);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  // A dynamic clone's slots become its version hold, returned with it at teardown, never a reserved credit (A-342).
  let version_credit = match version_credit {
    Some(credit) if dynamic => {
      volume_core.adopt_version_hold(credit.slots);
      None
    }
    credit => credit,
  };
  Ok(Admitted {
    volume: volume_core,
    host,
    credits: Credits {
      reservation,
      version: version_credit,
      metadata: metadata_credit,
      lock: lock_credit,
    },
  })
}

/// Discards an admitted clone that will not be recorded: its pin on the origin's snapshot released, its credits back.
pub(crate) fn discard_clone(
  state: &mut ShardState,
  origin: Handle<VolumeSlot>,
  snapshot: SnapshotId,
  admitted: Admitted,
) {
  unpin_origin(state, origin, snapshot);
  discard_admitted(state, admitted);
}

/// Records an admitted clone named `name` for `principal` — its record, and its lineage to `record`'s volume (in slot
/// `origin`) at `snapshot` — and moves it into the registry. A refusal discards it, its pin on the snapshot released
/// and its credits given back. Before 2026-10-24 a clone refused for want of a fresh id kept its pin on the snapshot.
pub(crate) fn publish_clone(
  state: &mut ShardState,
  principal: &Principal,
  origin: Handle<VolumeSlot>,
  record: &VolumeRecord,
  (snapshot, name): (SnapshotId, &str),
  admitted: Admitted,
) -> Result<DbVolumeId, Refusal> {
  let Some(id) = fresh_volume_id(state) else {
    discard_clone(state, origin, snapshot, admitted);
    return Err(ids_exhausted());
  };
  publish_clone_as(
    state,
    principal,
    origin,
    record,
    (snapshot, name),
    admitted,
    id,
  )
}

/// [`publish_clone`] under an id chosen before (A-236: the id the name's alias already names).
#[allow(clippy::too_many_arguments)] // publish_clone's arguments and the id it would choose
pub(crate) fn publish_clone_as(
  state: &mut ShardState,
  principal: &Principal,
  origin: Handle<VolumeSlot>,
  record: &VolumeRecord,
  (snapshot, name): (SnapshotId, &str),
  admitted: Admitted,
  id: DbVolumeId,
) -> Result<DbVolumeId, Refusal> {
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
      discard_clone(state, origin, snapshot, admitted);
      return Err(refusal_of_db(&e));
    }
  }
  // As in create: ensure the registry has room before moving the clone into a slot, discarding it
  // (which frees only what the clone made — nothing, since it shares its origin's versions) otherwise.
  // A partial clone also releases the pin `clone_of` put on the origin snapshot.
  if !state.volumes.has_room() {
    let full = state.volumes.max_slots();
    discard_clone(state, origin, snapshot, admitted);
    return Err(refusal_of_mem(&slates_mem::MemError::SlabFull {
      capacity: full,
    }));
  }
  let Admitted {
    volume: mut volume_core,
    host,
    credits,
  } = admitted;
  crate::content_cipher::key_volume(&mut state.store, &mut volume_core, id.bytes);
  let slot = VolumeSlot {
    id,
    name: name.to_owned(),
    volume: volume_core,
    // An overlay's clone keeps the host its admission duplicated from its origin's (AC-1.16).
    host,
    reservation: credits.reservation,
    version_credit: credits.version,
    metadata_credit: credits.metadata,
    lock_credit: credits.lock,
  };
  match state.volumes.insert(slot) {
    Ok(h) => {
      state.by_id.insert(id, h);
      Ok(id)
    }
    // The slot did not land: give both credits back (they are `Copy`), never leaking on this path.
    Err(e) => {
      release_credits(
        state,
        credits.reservation,
        credits.version,
        credits.metadata,
      );
      Err(refusal_of_mem(&e))
    }
  }
}

/// Attaches in the requested form (§4.4 `attach(volume|snapshot, consumer, transport, chosen_path?)`;
/// §4.6 A-9), in the order that takes nothing before it is allowed: the rights for the intent first
/// (§4.13: checked before any lookup or effect), then the form's establishment (a container bind is
/// verified against the kernel's mount table before any effect, so "Refusal rolls back owned
/// resources" holds by having taken none), then the write lease, then the record. The reply carries
/// what was established and the transport's report for this attachment, narrowed to the intent (a
/// reader on a volume it could write is still read-only through this attachment).
fn attach(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  intent: Intent,
  form: AttachRequest,
) -> ReplyBody {
  // A merge volume has no store slot: its record alone admits it. A green pins a version and
  // refuses a write intent; a work attaches like a plain volume (its lease and epoch).
  let record = match crate::merge_service::attachable_record(state, volume) {
    Ok(record) => record,
    Err(r) => return *r,
  };
  if let Some(reply) = shards_form(state, principal, &record, (volume, snapshot, intent), &form) {
    return reply;
  }
  if let Some(reply) = vhost_user_form(state, principal, &record, (volume, snapshot, intent), &form)
  {
    return reply;
  }
  if matches!(record.policy.role, Role::Green { .. }) {
    return crate::merge_service::attach_green(state, client_id, principal, &record, intent, &form);
  }
  let rights = rights_of(&record, principal);
  if !permits(rights, intent) {
    return forbidden("attach");
  }
  let situation = crate::transports::situation(state, &rights);
  let mut binding = match establish_form(&record, &situation, snapshot, (intent, principal), &form)
  {
    Ok(binding) => binding,
    Err(refusal) => return refused(refusal),
  };
  let expires_ns = termed_expiry(state);
  let resolved = binding.as_ref().map_or_else(
    // Another host's client's attachment names its host and client (A-222); a local client's is its own, a CLI one's
    // lapsing at `expires_ns` (A-230).
    || {
      crate::remote_attach::consumer(state, &form, client_id, expires_ns)
        .map(|consumer| (consumer, false))
    },
    |binding| {
      binding.consumer(
        state.db.partition(),
        record.id,
        principal,
        snapshot.map(to_db_snapshot),
      )
    },
  );
  let (consumer, shared_source) = match resolved {
    Ok(resolved) => resolved,
    Err(refusal) => return refused(refusal),
  };
  // Whether the source is shared with other users is the parent attachment's record, which only `consumer`
  // above has resolved — so the evidence is stamped here rather than in `oci::bind`, which sees the mount
  // table and not the attachment. A rootful runtime needs it true; a rootless one does not (§4.6 A-9, and
  // `HostMountEvidence::shared_with_other_users`).
  if let Some(binding) = binding.as_mut() {
    binding.entry.evidence.shared_with_other_users = shared_source;
  }
  let scope = match scope_of(state, volume, &form) {
    Ok(scope) => scope,
    Err(refusal) => return refused(refusal),
  };
  let borrows_mount = binding.is_some();
  let now = state.clock.monotonic_ns();
  let lease_epoch = match lease_for(state, &record, principal, intent, now) {
    Ok(epoch) => epoch,
    Err(refusal) => return refused(refusal),
  };
  let (db_form, established, mut capability) = match binding {
    None => (
      AttachForm::Root,
      Established::Record,
      crate::transports::root(&situation),
    ),
    Some(binding) => (
      AttachForm::Oci {
        source: binding.entry.source.clone(),
        destination: binding.entry.destination.clone(),
        read_only: binding.entry.read_only,
      },
      Established::OciBind {
        binding: Box::new(binding.entry),
      },
      crate::transports::oci(&situation),
    ),
  };
  let attachment = next_attachment_id(state);
  let termed = matches!(consumer, Consumer::Termed { .. });
  let token = match capability_token(borrows_mount) {
    Ok(token) => token,
    Err(refusal) => return refused(refusal),
  };
  let attachment_record = AttachmentRecord {
    id: attachment,
    volume: record.id,
    consumer,
    snapshot: snapshot.map(to_db_snapshot),
    form: recorded_form(&form, db_form, scope),
    principal: principal.clone(),
    rights: granted_rights(rights, intent),
    token,
  };
  if let Some(defer) = deferred_mount(&form) {
    return defer(
      state,
      attachment_record,
      situation,
      intent,
      lease_epoch,
      token,
    );
  }
  if let Err(refusal) = commit_attachment(state, &record, &form, attachment_record, now) {
    return refused(refusal);
  }
  if termed {
    // Its making is its holder's first use: the gaps its term follows are measured from here (A-384).
    state
      .attachment_paces
      .insert(attachment, crate::state::AttachmentPace::begun(now));
  }
  capability.read_write = read_write_of(intent);
  ReplyBody::Attached {
    attachment,
    lease_epoch,
    path: None,
    version: None,
    established,
    capability,
    token: (!borrows_mount).then_some(token),
  }
}

/// The attach verb's answer when `form` is the Shards form (A-161), which its own module makes and serves, its end handed
/// to the client with the reply; `None` for every other form. A green is not presented over a stream (it pins a
/// version, which a stream's view does not hold yet).
fn shards_form(
  state: &mut ShardState,
  principal: &Principal,
  record: &VolumeRecord,
  (volume, snapshot, intent): (VolumeId, Option<SnapshotId>, Intent),
  form: &AttachRequest,
) -> Option<ReplyBody> {
  let AttachRequest::ShardsFs { subtree } = form else {
    return None;
  };
  if matches!(record.policy.role, Role::Green { .. }) {
    return Some(refused(Refusal::Unsupported {
      feature: "a green presented over a Shards stream".to_owned(),
    }));
  }
  Some(shards_attach(
    state,
    principal,
    volume,
    snapshot,
    (intent, subtree.clone()),
  ))
}

/// The attach verb's answer when `form` is the vhost-user form (G-vhost): slates' own virtio-fs device served on a socket
/// pair whose other end the client hands its VMM; `None` for every other form. A green is not presented to a device.
fn vhost_user_form(
  state: &mut ShardState,
  principal: &Principal,
  record: &VolumeRecord,
  (volume, snapshot, intent): (VolumeId, Option<SnapshotId>, Intent),
  form: &AttachRequest,
) -> Option<ReplyBody> {
  let AttachRequest::VhostUserFs {
    tag,
    subtree,
    dax_window,
  } = form
  else {
    return None;
  };
  if matches!(record.policy.role, Role::Green { .. }) {
    return Some(refused(Refusal::Unsupported {
      feature: "a green presented to a vhost-user device".to_owned(),
    }));
  }
  Some(vhost_user_attach(
    state,
    principal,
    (volume, snapshot),
    (intent, tag.clone(), subtree.clone(), *dax_window),
  ))
}

/// The attach verb's vhost-user form, served on Linux by `crate::virtiofs`.
#[cfg(target_os = "linux")]
fn vhost_user_attach(
  state: &mut ShardState,
  principal: &Principal,
  (volume, snapshot): (VolumeId, Option<SnapshotId>),
  (intent, tag, subtree, dax_window): (Intent, String, Option<String>, Option<u64>),
) -> ReplyBody {
  crate::virtiofs::attach_verb(
    state,
    principal,
    volume,
    snapshot,
    crate::virtiofs::VhostUserAsk {
      intent,
      tag,
      subtree,
      dax_window,
    },
  )
}

/// vhost-user is Linux's (its eventfds and the KVM VMMs that speak it): the form is refused here before any effect,
/// with the device's own reason.
#[cfg(not(target_os = "linux"))]
fn vhost_user_attach(
  _state: &mut ShardState,
  _principal: &Principal,
  _volume: (VolumeId, Option<SnapshotId>),
  _ask: (Intent, String, Option<String>, Option<u64>),
) -> ReplyBody {
  refused(Refusal::AttachmentUnsupported {
    transport: AttachTransport::VirtioFsInheritedDescriptor,
    reason: UnsupportedReason::BindingNotBuilt,
  })
}

/// The attach verb's Shards form (A-161): served by `crate::shards` on every platform, which refuses it where the
/// platform has no route to hand the client its end (A-179 built Windows' handle route).
fn shards_attach(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  snapshot: Option<SnapshotId>,
  ask: (Intent, Option<String>),
) -> ReplyBody {
  crate::shards::attach_verb(state, principal, volume, snapshot, ask)
}

/// What a verb run on its volume's owner shard carries back to its client's shard besides its reply: the Shards stream
/// end owed for it, if it made one (A-161; Windows too since A-179).
type Carried = Option<crate::shards::Handoff>;

/// Takes what request `request`'s verb, just run here, owes its client besides its reply.
fn take_carried(state: &mut ShardState, request: u64) -> Carried {
  state.stream_handoffs.remove(&request)
}

/// Keeps on the client's shard what request `request`'s verb carried back, for `send_reply` to hand over with the
/// reply.
fn keep_carried(state: &mut ShardState, request: u64, carried: Carried) {
  if let Some(handoff) = carried {
    state.stream_handoffs.insert(request, handoff);
  }
}

/// Commits an attachment record. A host mount of a snapshot opens its read-only view first (AUD-29-76), so a
/// refusal changes nothing, and closes it again if the record does not commit.
fn commit_attachment(
  state: &mut ShardState,
  volume: &VolumeRecord,
  form: &AttachRequest,
  attachment: AttachmentRecord,
  now: u64,
) -> Result<(), Refusal> {
  let view = match (form, attachment.snapshot) {
    (AttachRequest::HostMount, Some(snapshot)) => Some(crate::snapshot_view::open(
      state,
      volume.id,
      core_snapshot(SnapshotId {
        value: snapshot.value,
      }),
      wire_names(volume.policy.names),
    )?),
    _ => None,
  };
  let id = attachment.id;
  let op = Op::AttachmentAdded { record: attachment };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    if let Some(view) = view {
      crate::snapshot_view::close(state, view);
    }
    return Err(refusal_of_db(&e));
  }
  if let Some(view) = view {
    state.snapshot_views.insert(id, view);
  }
  Ok(())
}

/// The bridge a transport's requests are served through: held to `scope` when the attachment presents one directory
/// (AUD-29-76), else the volume's whole bridge. `scoped` is the caller's slot the scoped bridge lives in. Its callers
/// are the FUSE mount and the Shards stream (on every platform since A-179).
pub(crate) fn scoped_or_whole<'a>(
  bridge: &'a mut slates_bridge_core::VolumeBridge<'_>,
  scoped: &'a mut Option<slates_bridge_core::scoped::ScopedBridge<'a>>,
  scope: Option<u64>,
) -> &'a mut dyn slates_bridge_core::Bridge {
  match scope {
    Some(scope) => scoped.insert(slates_bridge_core::scoped::ScopedBridge::new(bridge, scope)),
    None => bridge,
  }
}

/// Releases the write lease an attach took for `record` before the attachment it was for was recorded, when no other
/// attachment of the holder came of it: the attach failed after its lease (a FUSE mount refused, a Shards stream that
/// could not be served). A refused release is counted, never raised: the lease's own term ends it. Unix only, as its
/// callers are (the FUSE mount, the Shards stream, the WinFsp mount).
pub(crate) fn release_unrecorded_lease(state: &mut ShardState, record: &AttachmentRecord) {
  let holds_another = state
    .db
    .partition()
    .attachments_of(record.volume)
    .iter()
    .any(|a| a.principal == record.principal);
  let lease_is_ours = state
    .db
    .partition()
    .volume(record.volume)
    .and_then(|v| v.lease.as_ref())
    .is_some_and(|l| l.holder == record.principal);
  if !holds_another && lease_is_ours {
    let now = state.clock.monotonic_ns();
    let released = state.db.mutate(
      &mut state.segment,
      &Op::LeaseReleased {
        volume: record.volume,
      },
      now,
    );
    count_secondary(state, released);
  }
}

/// Whether `rights` permit an attach with `intent`.
pub(crate) fn permits(rights: Rights, intent: Intent) -> bool {
  match intent {
    Intent::Read => rights.read,
    Intent::Write => rights.write,
  }
}

/// The write lease an attach with `intent` takes (D-16): none for a read, the volume's lease for a write
/// (its epoch), or the refusal when another principal holds it unexpired.
pub(crate) fn lease_for(
  state: &mut ShardState,
  record: &VolumeRecord,
  principal: &Principal,
  intent: Intent,
  now: u64,
) -> Result<Option<u64>, Refusal> {
  match intent {
    Intent::Read => Ok(None),
    Intent::Write => take_write_lease(state, record, principal, now).map(Some),
  }
}

/// A FUSE mount point's checks, before any effect (`crate::fuse::check_mount_point`).
#[cfg(target_os = "linux")]
fn check_fuse_mount_point(form: &AttachRequest, principal: &Principal) -> Result<(), Refusal> {
  match form.fuse_mount_point() {
    Some(mount_point) => crate::fuse::check_mount_point(mount_point, principal),
    None => Ok(()),
  }
}

/// FUSE is Linux's; the transport report refused it before this elsewhere.
#[cfg(not(target_os = "linux"))]
fn check_fuse_mount_point(_form: &AttachRequest, _principal: &Principal) -> Result<(), Refusal> {
  Ok(())
}

/// The mount capability token (§4.6, §4.13; AUD-01): a random secret bound to the attachment, returned to the
/// authorized consumer and presented at the mount so the edge authorizes the connection as this consumer
/// with the granted rights. Refused (never a weak token) if the platform's secure random is unavailable, the
/// same discipline the daemon applies to its issuer secret. A bind borrows the parent's authority
/// (`borrows_mount`), so it mints no independent capability.
fn capability_token(borrows_mount: bool) -> Result<[u8; 16], Refusal> {
  if borrows_mount {
    return Ok([0; 16]);
  }
  mint_mount_token().ok_or_else(|| Refusal::BadRequest {
    reason: "secure random unavailable for the mount capability token".to_owned(),
  })
}

/// The form an attachment is recorded under: a FUSE mount's is its chosen path (the mount point); every other
/// form's is the one its establishment named.
fn recorded_form(form: &AttachRequest, established: AttachForm, scope: Option<u64>) -> AttachForm {
  match (form, scope) {
    (AttachRequest::FuseMount { mount_point }, _) => AttachForm::FuseMount {
      path: mount_point.clone(),
    },
    (AttachRequest::ScopedHostMount { .. }, Some(scope)) => AttachForm::ScopedMount {
      scope,
      mount_point: None,
    },
    (AttachRequest::ScopedFuseMount { mount_point, .. }, Some(scope)) => {
      AttachForm::ScopedFuseMount {
        path: mount_point.clone(),
        scope,
      }
    }
    (AttachRequest::SharedFuseMount { mount_point, .. }, scope) => AttachForm::SharedFuseMount {
      path: mount_point.clone(),
      scope,
    },
    (AttachRequest::WinFspMount { mount_point }, _) => AttachForm::WinFspMount {
      path: mount_point.clone(),
    },
    (AttachRequest::BoundHostMount { uid, .. }, scope) => AttachForm::BoundMount {
      uid: *uid,
      scope,
      mount_point: None,
    },
    _ => established,
  }
}

/// The directory inode a scoped host mount presents: `subtree`, resolved from the volume's root in its head
/// (§4.6 scoped exports; AUD-29-76). Refused typed, before any effect, when the path names nothing or no
/// directory; `None` for every other form.
fn scope_of(
  state: &ShardState,
  volume: VolumeId,
  form: &AttachRequest,
) -> Result<Option<u64>, Refusal> {
  let Some(subtree) = form.subtree() else {
    return Ok(None);
  };
  resolve_scope(state, to_db_volume(volume), subtree).map(Some)
}

/// The inode of directory `subtree` of `volume` in its head: the scope a scoped mount or a scoped guest device
/// presents (AUD-29-76). Refused typed when the path names nothing or no directory.
pub(crate) fn resolve_scope(
  state: &ShardState,
  volume: DbVolumeId,
  subtree: &str,
) -> Result<u64, Refusal> {
  let handle = *state.by_id.get(&volume).ok_or(Refusal::NotFound)?;
  let slot = state.volumes.get(handle).map_err(|_| Refusal::NotFound)?;
  let located = slot
    .volume
    .resolve(&state.store, subtree)
    .map_err(|e| refusal_of_vfs(&e))?;
  match located.child {
    slates_vfs::dir::Child::Dir(_) => Ok(located.inode.0),
    _ => Err(refusal_of_vfs(&slates_vfs::VfsError::NotDirectory)),
  }
}

/// The read/write policy an attachment's reply reports for its intent.
fn read_write_of(intent: Intent) -> ReadWritePolicy {
  match intent {
    Intent::Read => ReadWritePolicy::ReadOnly,
    Intent::Write => ReadWritePolicy::ReadWrite,
  }
}

/// The refusal for a form `establish_form` does not establish: a guest device over the ring (its seam is handed in
/// in-process), and a Shards stream, which `attach` hands to `crate::shards` before this is reached, so a caller that
/// gets here has no stream to establish. Either is refused before any effect.
fn not_established_here(situation: &crate::transports::Situation, form: &AttachRequest) -> Refusal {
  match form {
    AttachRequest::Guest { transport } => guest_over_the_ring(situation, *transport),
    _ => Refusal::BadRequest {
      reason: "a Shards stream is attached by its own path".to_owned(),
    },
  }
}

/// Hands a FUSE attach to its deferred establishment (`crate::fuse`): the mount is made without blocking the
/// shard, and the attachment recorded with the reply once the device is held (§4.6 "Linux"; AUD-29-64).
#[cfg(target_os = "linux")]
fn defer_fuse_attach(
  state: &mut ShardState,
  record: AttachmentRecord,
  situation: crate::transports::Situation,
  intent: Intent,
  lease_epoch: Option<u64>,
  token: [u8; 16],
) -> ReplyBody {
  let mut capability = crate::transports::fuse(&situation);
  capability.read_write = read_write_of(intent);
  let rights = slates_bridge_core::Rights {
    read: record.rights.read,
    write: record.rights.write,
  };
  crate::fuse::defer_attach(
    state,
    crate::fuse::PendingAttach {
      record,
      rights,
      lease_epoch,
      capability,
      token,
    },
  )
}

/// A deferred mount's establishment: the shard, the record to commit, the situation, the intent, the lease's epoch and
/// the capability token, to the placeholder reply.
type DeferMount = fn(
  &mut ShardState,
  AttachmentRecord,
  crate::transports::Situation,
  Intent,
  Option<u64>,
  [u8; 16],
) -> ReplyBody;

/// The establishment a mount form the daemon itself mounts defers its reply to — a FUSE mount's or a WinFsp volume's —
/// or `None` for a form recorded at once.
fn deferred_mount(form: &AttachRequest) -> Option<DeferMount> {
  if matches!(form, AttachRequest::WinFspMount { .. }) {
    Some(defer_winfsp_attach)
  } else if form.fuse_mount_point().is_some() {
    Some(defer_fuse_attach)
  } else {
    None
  }
}

/// A FUSE mount's establishment, before any effect: offered where the host has FUSE, presenting the live head (never a
/// snapshot), at a mount point that passes `check_fuse_mount_point`.
fn establish_fuse(
  situation: &crate::transports::Situation,
  snapshot: Option<SnapshotId>,
  form: &AttachRequest,
  principal: &Principal,
) -> Result<(), Refusal> {
  if let Some(reason) = crate::transports::fuse(situation).unsupported_reason {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Fuse,
      reason,
    });
  }
  if snapshot.is_some() {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Fuse,
      reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
    });
  }
  check_fuse_mount_point(form, principal)
}

/// A WinFsp volume's establishment, before any effect: offered where WinFsp is installed, presenting the live head
/// (never a snapshot), at a drive letter.
fn establish_winfsp(
  situation: &crate::transports::Situation,
  snapshot: Option<SnapshotId>,
  mount_point: &str,
) -> Result<(), Refusal> {
  if let Some(reason) = crate::transports::winfsp(situation).unsupported_reason {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::WinFsp,
      reason,
    });
  }
  if snapshot.is_some() {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::WinFsp,
      reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
    });
  }
  check_winfsp_mount_point(mount_point)
}

/// Hands a WinFsp attach to its deferred establishment (`crate::winfsp`): the drive is mounted off the shard, and the
/// attachment recorded with the reply once it is (§4.6 "Windows").
#[cfg(windows)]
fn defer_winfsp_attach(
  state: &mut ShardState,
  record: AttachmentRecord,
  situation: crate::transports::Situation,
  intent: Intent,
  lease_epoch: Option<u64>,
  token: [u8; 16],
) -> ReplyBody {
  let mut capability = crate::transports::winfsp(&situation);
  capability.read_write = read_write_of(intent);
  let rights = slates_bridge_core::Rights {
    read: record.rights.read,
    write: record.rights.write,
  };
  crate::winfsp::defer_attach(
    state,
    crate::winfsp::PendingAttach {
      record,
      rights,
      lease_epoch,
      capability,
      token,
    },
  )
}

/// WinFsp mounts are Windows'; `establish_form` has refused the form before any effect elsewhere.
#[cfg(not(windows))]
fn defer_winfsp_attach(
  _state: &mut ShardState,
  _record: AttachmentRecord,
  _situation: crate::transports::Situation,
  _intent: Intent,
  _lease_epoch: Option<u64>,
  _token: [u8; 16],
) -> ReplyBody {
  refused(Refusal::AttachmentUnsupported {
    transport: AttachTransport::WinFsp,
    reason: UnsupportedReason::HostPlatform,
  })
}

/// A WinFsp mount point's check, before any effect (`crate::winfsp::check_mount_point`).
#[cfg(windows)]
fn check_winfsp_mount_point(mount_point: &str) -> Result<(), Refusal> {
  crate::winfsp::check_mount_point(mount_point)
}

/// WinFsp is Windows'; the transport report refused it before this elsewhere.
#[cfg(not(windows))]
fn check_winfsp_mount_point(_mount_point: &str) -> Result<(), Refusal> {
  Ok(())
}

/// FUSE mounts are Linux's; `establish_form` has refused the form before any effect elsewhere.
#[cfg(not(target_os = "linux"))]
fn defer_fuse_attach(
  _state: &mut ShardState,
  _record: AttachmentRecord,
  _situation: crate::transports::Situation,
  _intent: Intent,
  _lease_epoch: Option<u64>,
  _token: [u8; 16],
) -> ReplyBody {
  refused(Refusal::AttachmentUnsupported {
    transport: AttachTransport::Fuse,
    reason: UnsupportedReason::HostPlatform,
  })
}

/// Mints a fresh 16-byte mount capability token from the platform's secure random (§4.13; AUD-01) — the
/// same source the daemon's grant-issuer secret uses, so a token is unpredictable and cannot be guessed
/// from an attachment id (which is a routable counter). `None` if the provider cannot mint one, so the
/// attach refuses rather than issue a guessable token.
pub(crate) fn mint_mount_token() -> Option<[u8; 16]> {
  let mut token = [0u8; 16];
  slates_transport::handshake::secure_random(&mut token).ok()?;
  Some(token)
}

/// The consumer an attachment in `form` belongs to (§4.6, §4.13; AUD-01), which is its lifetime: a
/// host kernel mount's attachment is the OS filesystem bridge's (`Consumer::Bridge`) — it outlives the
/// ring client that requested it (`slates mount` exits after `mount_nfs`) and the daemon (the anchor
/// keeps the listener across a restart), ending with the kernel's `UMNT`, a `detach`, or the volume's
/// destroy. A root record is the ring client's. OCI requires a verified source dependency,
/// constructed separately by `oci::Binding::consumer`; a guest requires its owned device seam.
pub(crate) fn consumer_of(
  form: &AttachRequest,
  client_id: u32,
  expires_ns: u64,
) -> Result<Consumer, Refusal> {
  match form {
    AttachRequest::HostMount
    | AttachRequest::FuseMount { .. }
    | AttachRequest::ScopedHostMount { .. }
    | AttachRequest::ScopedFuseMount { .. }
    | AttachRequest::SharedFuseMount { .. }
    | AttachRequest::BoundHostMount { .. }
    | AttachRequest::WinFspMount { .. } => Ok(Consumer::Bridge),
    AttachRequest::Root => Ok(Consumer::Sdk { client: client_id }),
    // A CLI attachment outlives its client for a term (A-230), lapsing at `expires_ns`.
    AttachRequest::Termed => Ok(Consumer::Termed { expires_ns }),
    AttachRequest::Oci { .. } => Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::HostMountRequired,
    }),
    AttachRequest::Guest { transport } => Err(Refusal::AttachmentUnsupported {
      transport: *transport,
      reason: UnsupportedReason::SeamNotOnWire,
    }),
    // A Shards stream is a guest's: it ends with the stream, and recovery ends a dead daemon's (A-161). So is a
    // vhost-user device the verb hands out (G-vhost).
    AttachRequest::ShardsFs { .. } | AttachRequest::VhostUserFs { .. } => Ok(Consumer::Guest),
  }
}

/// A CLI attachment's term (A-230, A-384, §4.7): the longest it may go unused before it lapses with the write lease it
/// holds, its holder's own. At most this node's failover delay (`ShardState::failover_ns`, A-363): an abandoned attachment
/// holds the volume's write lease no longer than a failover would leave the volume unserved, past which any other
/// writer's failover delay would be broken anyway (A-230's bound). Within it, its holder's pace: the tail of the gaps
/// between its uses (RFC 6298's estimator, `smoothed + 4·deviation`, as the failover estimate's terms) plus a liveness
/// window, the lateness by which a live party may fall behind before it is judged (A-228) — an accrual term per holder
/// (Hayashibara et al. 2004, judged at the estimator's tail). So a holder that renews at a steady pace (`slates attach
/// --hold`, the library's and the SDKs' attachment objects, which renew at the cadence the `Renewed` reply names,
/// [`renewal_cadence`]) is judged by its own pace, and one with no pace measured yet (just made, or recovered by a restart, or used by verbs
/// alone a term apart) has the failover delay.
pub(crate) fn attachment_term(state: &ShardState, attachment: u64) -> u64 {
  let ceiling = state.failover_ns.max(1);
  state
    .attachment_paces
    .get(&attachment)
    .and_then(|pace| pace.gaps.tail_ns())
    .map_or(ceiling, |tail| {
      tail
        .saturating_add(crate::daemon::LIVENESS_BUDGET_NS)
        .min(ceiling)
    })
}

/// When a CLI attachment made now lapses (A-230, A-384): one term from the shard's clock, its holder's pace not yet
/// measured, so the failover delay ([`attachment_term`]).
pub(crate) fn termed_expiry(state: &mut ShardState) -> u64 {
  let term = state.failover_ns.max(1);
  state.clock.monotonic_ns().saturating_add(term)
}

/// The `Renew` verb (A-384): a holder's renewal of its CLI attachment's term, and nothing else. Renewed by
/// [`use_attachment`] as any verb naming it is, and answered the term now in force and the cadence the holder renews at
/// ([`renewal_cadence`]). Refused as any use is (`AttachmentExpired` past its term, `NotFound` for no such attachment);
/// another principal's is `Forbidden`, its use having renewed nothing; and an attachment with no term (a mount's,
/// which renews through its own activity) is a bad request.
fn renew(state: &mut ShardState, principal: &Principal, attachment: u64) -> ReplyBody {
  if let Err(refusal) = use_attachment(state, principal, attachment) {
    return refused(refusal);
  }
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("renew");
  }
  let Consumer::Termed {
    expires_ns: current,
  } = record.consumer
  else {
    return refused(Refusal::BadRequest {
      reason:
        "only a CLI attachment has a term to renew; a mount's renews through its own activity"
          .to_owned(),
    });
  };
  // A renewal asked for is made: its holder's next is a third of the term away, so the expiry is a whole term from now,
  // whatever an ordinary use's half-term rule left it at (`use_attachment`).
  let term = attachment_term(state, attachment);
  let now = state.clock.monotonic_ns();
  let expires_ns = now.saturating_add(term);
  if current < expires_ns {
    let renewal = Op::AttachmentRenewed {
      id: attachment,
      expires_ns,
    };
    if let Err(e) = state.db.mutate(&mut state.segment, &renewal, now) {
      return refused(refusal_of_db(&e));
    }
  }
  ReplyBody::Renewed {
    term_ns: term,
    renew_every_ns: renewal_cadence(term),
  }
}

/// Derived: how soon a holder renews its attachment again after a renewal that named `term_ns` (A-384): a third of the
/// shorter of the term and the liveness window `W`, so two renewals lost or late are survived within the term. Not a
/// third of the term alone: the holder's gaps shape its term (its tail plus `W`), so a cadence that followed the term fed
/// back into it, and the term grew to the failover delay (1.0 s to 4.0 s in ten renewals on the Mac Studio, 2026-11-18).
/// With the cadence a third of `W`, the gaps are steady and the term settles near `4W / 3`.
pub(crate) fn renewal_cadence(term_ns: u64) -> u64 {
  /// Format: the renewals a holder makes within the term it must renew inside: the one due, and two that may be lost or
  /// late.
  const RENEWALS_PER_TERM: u64 = 3;
  term_ns
    .min(crate::daemon::LIVENESS_BUDGET_NS)
    .checked_div(RENEWALS_PER_TERM)
    .unwrap_or(0)
    .max(1)
}

/// Gives every CLI attachment of this partition a fresh term from now, at the shard's start (A-384): a holder could not
/// renew while this daemon was not serving, and its own downtime is never counted against a holder (Lifeguard's rule,
/// Dadgar et al. 2018: a member does not condemn others for its own unavailability). Each pace starts again with the
/// failover delay. The attachments renewed; one whose renewal could not be recorded keeps its old expiry.
pub(crate) fn renew_termed_at_start(state: &mut ShardState) -> usize {
  let now = state.clock.monotonic_ns();
  let expires_ns = now.saturating_add(state.failover_ns.max(1));
  let termed: Vec<u64> = state
    .db
    .partition()
    .attachments()
    .filter(|record| matches!(record.consumer, Consumer::Termed { .. }))
    .map(|record| record.id)
    .collect();
  let mut renewed = 0usize;
  for id in termed {
    let renewal = Op::AttachmentRenewed { id, expires_ns };
    if state.db.mutate(&mut state.segment, &renewal, now).is_ok() {
      state
        .attachment_paces
        .insert(id, crate::state::AttachmentPace::begun(now));
      renewed = renewed.saturating_add(1);
    }
  }
  renewed
}

/// The rights an attachment records (§4.13 "Access lists"; AUD-01): the principal's rights on the volume
/// bounded by the attachment's intent — an attach-for-read records no write right, so its mount
/// capability presents a read-only view whatever the principal could otherwise do (the NFS edge maps
/// the record's rights, never the principal's). `admin` is never an attachment's: the admin verbs act on
/// the volume by principal, not through an attachment.
pub(crate) fn granted_rights(rights: Rights, intent: Intent) -> Rights {
  Rights {
    read: rights.read,
    write: rights.write && intent == Intent::Write,
    admin: false,
  }
}

/// The form's establishment, before any effect: nothing for the record form; for a container bind,
/// the transport must be offered on this host (the report's own reason otherwise), the view must be
/// the live head the host mount presents (never a snapshot), and the host path must verify as this
/// volume's mount point (`crate::oci::bind`).
fn establish_form(
  record: &VolumeRecord,
  situation: &crate::transports::Situation,
  snapshot: Option<SnapshotId>,
  (intent, principal): (Intent, &Principal),
  form: &AttachRequest,
) -> Result<Option<crate::oci::Binding>, Refusal> {
  let (source, destination) = match form {
    // The record forms: nothing to establish — the SDK's record, a CLI's (A-230), and the host mount the
    // requesting process establishes itself with the capability the reply carries (`mount_nfs`, R10: no
    // privilege and nothing of the daemon's touches the mount table).
    AttachRequest::Root | AttachRequest::Termed => return Ok(None),
    // A scoped host mount presents the live head beneath one directory; a snapshot of a subtree is not
    // presented (its scope and its version would both have to hold through one view). A bound host mount
    // presents the live head too (§4.6 A-115).
    AttachRequest::ScopedHostMount { .. } | AttachRequest::BoundHostMount { .. } => {
      if snapshot.is_some() {
        return Err(Refusal::AttachmentUnsupported {
          transport: AttachTransport::NfsLoopback,
          reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
        });
      }
      return Ok(None);
    }
    // A host mount of a snapshot presents the attachment's own read-only view of it (`crate::snapshot_view`),
    // never the head (AUD-29-76): only a read may attach one, since a snapshot is immutable.
    AttachRequest::HostMount => {
      if snapshot.is_some() && matches!(intent, Intent::Write) {
        return Err(Refusal::AttachmentUnsupported {
          transport: AttachTransport::NfsLoopback,
          reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
        });
      }
      return Ok(None);
    }
    // A FUSE mount (§4.6 "Linux"; AUD-29-64): offered where the host has FUSE, presenting the live head.
    AttachRequest::FuseMount { .. }
    | AttachRequest::ScopedFuseMount { .. }
    | AttachRequest::SharedFuseMount { .. } => {
      return establish_fuse(situation, snapshot, form, principal).map(|()| None);
    }
    // A WinFsp volume (§4.6 "Windows"): offered where WinFsp is installed, at a drive letter, presenting the live head.
    AttachRequest::WinFspMount { mount_point } => {
      return establish_winfsp(situation, snapshot, mount_point).map(|()| None);
    }
    AttachRequest::Guest { .. }
    | AttachRequest::ShardsFs { .. }
    | AttachRequest::VhostUserFs { .. } => {
      return Err(not_established_here(situation, form));
    }
    AttachRequest::Oci {
      source,
      destination,
    } => (source, destination),
  };
  if let Some(reason) = crate::transports::oci(situation).unsupported_reason {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason,
    });
  }
  // A bind of a snapshot borrows a host mount presenting that snapshot (`oci::Binding::consumer`), and a
  // snapshot is immutable: only a read binds one (AUD-29-76).
  if snapshot.is_some() && matches!(intent, Intent::Write) {
    return Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::SnapshotNotPresentedByHostMount,
    });
  }
  let read_only = matches!(intent, Intent::Read);
  crate::oci::bind(&record.name, source, destination, read_only).map(Some)
}

/// Why a guest form asked for over the ring is refused (§4.6 A-9 "Requesting an unsupported form
/// returns `AttachmentUnsupported{transport, reason}`"): a transport that is not a guest's is a bad
/// request; a guest transport the device refuses carries the device's own reason; one it serves still
/// cannot be established here, because the VMM seam is handed to the daemon in-process by the harness
/// (`Daemon::attach_guest_device`) and no seam accompanies a ring request.
fn guest_over_the_ring(
  situation: &crate::transports::Situation,
  transport: AttachTransport,
) -> Refusal {
  match crate::transports::guest(situation, transport) {
    None => Refusal::BadRequest {
      reason: format!("{transport:?} is not a guest transport"),
    },
    Some(entry) => Refusal::AttachmentUnsupported {
      transport,
      reason: entry
        .unsupported_reason
        .unwrap_or(UnsupportedReason::SeamNotOnWire),
    },
  }
}

/// Takes (or renews) the volume's write lease for `principal` (D-16): the holder renews at its
/// epoch, an expired lease passes to the next epoch, and the term is the operator's failover SLO.
fn take_write_lease(
  state: &mut ShardState,
  record: &VolumeRecord,
  principal: &Principal,
  now: u64,
) -> Result<u64, Refusal> {
  let epoch = match &record.lease {
    Some(current) if &current.holder == principal => current.epoch,
    Some(current) if current.expires_ns <= now => {
      current
        .epoch
        .checked_add(1)
        .ok_or_else(|| Refusal::Unsupported {
          feature: LEASE_EPOCH_EXHAUSTED.to_owned(),
        })?
    }
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
  state
    .db
    .mutate(
      &mut state.segment,
      &Op::LeaseTaken {
        volume: record.id,
        lease,
      },
      now,
    )
    .map_err(|e| refusal_of_db(&e))?;
  Ok(epoch)
}

fn detach(state: &mut ShardState, principal: &Principal, attachment: u64) -> ReplyBody {
  detach_of(state, principal, None, attachment)
}

/// `detach`, and `DetachOf` (A-222), which names the attachment's volume so another host's attachment routes to its
/// owner: refused `NotFound` for an attachment of another volume, and `Forbidden` for one the requester's host does
/// not hold (`remote_attach::may_use`), so a forward cannot end another host's attachment or this node's.
fn detach_of(
  state: &mut ShardState,
  principal: &Principal,
  volume: Option<VolumeId>,
  attachment: u64,
) -> ReplyBody {
  // A CLI attachment past its term is refused as expired, never as unknown (A-230).
  if let Err(refusal) = use_attachment(state, principal, attachment) {
    return refused(refusal);
  }
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if volume.is_some_and(|volume| to_db_volume(volume) != record.volume) {
    return refused(Refusal::NotFound);
  }
  if &record.principal != principal || !crate::remote_attach::may_use(state, &record) {
    return forbidden("detach");
  }
  // A writable direct map's stores reach its file only through the attachment it was made under (§4.6, A-186): the
  // attachment stays until each is released, as a file system with files open refuses to unmount.
  let maps = crate::direct_map::writable_maps_of(state, attachment);
  if maps > 0 {
    return refused(Refusal::MapsHeld { maps });
  }
  // A WinFsp mount serves on until the anchor has released its hold: its file system is deleted only after that.
  #[cfg(windows)]
  if let Some(deferred) = crate::winfsp::defer_detach(state, &record) {
    return deferred;
  }
  match end_attachment(state, &record, Ending::Detached) {
    Ok(()) => ReplyBody::Detached,
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Format: the longest mount point a bind may name (the OS path limit, Linux `PATH_MAX`; a longer one
/// cannot be a mount point).
const MOUNT_PATH_MAX: usize = 4096;

/// Binds a host mount's attachment to the path the mounting process established it at (§4.4
/// `Binding → Bound`; GAP-A9-4): the record's form becomes `ChosenPath { path }`, which `status` then
/// reports. Only the attachment's principal may bind it, and only a host mount's attachment
/// (`Consumer::Bridge`) has a mount point; a path that is empty, holds a NUL, or exceeds the OS limit
/// is a bad request.
fn bind_mount(
  state: &mut ShardState,
  principal: &Principal,
  attachment: u64,
  path: &str,
) -> ReplyBody {
  // A CLI attachment is renewed by the verb, or refused once its term lapsed (A-230).
  if let Err(refusal) = use_attachment(state, principal, attachment) {
    return refused(refusal);
  }
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("bind_mount");
  }
  if !matches!(record.consumer, Consumer::Bridge) {
    return refused(Refusal::BadRequest {
      reason: "only a host mount's attachment binds a mount point".to_owned(),
    });
  }
  if path.is_empty() || path.contains('\0') || path.len() > MOUNT_PATH_MAX {
    return refused(Refusal::BadRequest {
      reason: "a mount point is a non-empty path within the OS limit".to_owned(),
    });
  }
  // The mount the CLI just made is this daemon's own from here (A-227): the kernel's table names it at exactly the
  // bound path, the one the kernel records (a point holding no mount of the volume records nothing).
  #[cfg(target_os = "macos")]
  let own = match crate::own_nfs::bound(state, &record, path) {
    Ok(own) => own,
    Err(refusal) => return refused(refusal),
  };
  let now = state.clock.monotonic_ns();
  match state.db.mutate(
    &mut state.segment,
    &Op::AttachmentBound {
      id: attachment,
      path: path.to_owned(),
    },
    now,
  ) {
    Ok(_) => {
      #[cfg(target_os = "macos")]
      if let Some(own) = own {
        state.own_nfs_mounts.insert(attachment, own);
      }
      ReplyBody::MountBound
    }
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Derived: the most bytes of mounts one status report carries — a quarter of the reply's bulk chunk
/// (`BULK_CHUNK_BYTES` / 4), so the mounts never crowd the report's other fields out of the slot; the
/// rest are counted (`mounts_elided`). Each mount is counted at its whole encoding (its attachment, its path's length
/// prefix and its path), not its path alone, and the mounts take no more than the room the rest of the report leaves
/// (A-267, 2026-11-18): a long name can leave less than this.
const MOUNTS_REPORT_BYTES: usize = 1024;

/// Format: what the retired whole-file `Read` is refused as (A-267): the verb, and the paged read that replaced it.
const RETIRED_WHOLE_READ: &str =
  "Read, retired (A-267): a file is read a page at a time with ReadRange";

/// Format: the watcher text of a status of a volume with no base (a scratch volume, a clone of one).
const WATCHER_SCRATCH: &str = "scratch";
/// Format: the watcher text of a status whose base could not be read now.
const WATCHER_UNAVAILABLE: &str = "unavailable";
/// Format: the watcher text of a green's or a work volume's status (`crate::merge_service`).
pub(crate) const WATCHER_MERGE: &str = "merge";

/// The volume's bound host mounts for its status (§4.4 `Bound`; GAP-A9-4): each `Consumer::Bridge`
/// attachment whose form is a chosen path, as many as `budget` bytes carry (in id order), and the
/// count of those it did not.
fn mounts_of(
  state: &ShardState,
  volume: DbVolumeId,
  budget: usize,
) -> (Vec<slates_ipc::protocol::MountReport>, u32) {
  let mut mounts = Vec::new();
  let mut elided = 0u32;
  let mut carried = 0usize;
  for record in state.db.partition().attachments_of(volume) {
    let Some(path) = record.form.mount_point() else {
      continue;
    };
    if !matches!(record.consumer, Consumer::Bridge) {
      continue;
    }
    let mount = slates_ipc::protocol::MountReport {
      attachment: record.id,
      path: path.to_owned(),
    };
    let bytes = encoded_len(&mount);
    if carried.saturating_add(bytes) > budget {
      elided = elided.saturating_add(1);
      continue;
    }
    carried = carried.saturating_add(bytes);
    mounts.push(mount);
  }
  (mounts, elided)
}

/// Why an attachment ends, which decides whether its FUSE mount is unmounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
  /// Its user detached it (the `detach` verb): the mount goes with it, as the user asked.
  Detached,
  /// Anything else (a revocation, the mount's own end, a protocol's unmount): the mount, if any, stays until its
  /// user unmounts it.
  Otherwise,
  /// A CLI attachment's term lapsed (A-230): recorded as expired, so a later use is refused `AttachmentExpired`.
  Expired,
}

/// `principal`'s use of attachment `attachment` against its term (A-230, §4.7). A CLI attachment within its term is
/// renewed when less than half of the term is left, so a user at work keeps it with one durable record per half term,
/// never one a verb; one past its term is ended now, as its lapse would end it, and refused. An id the partition
/// recorded as expired is refused `AttachmentExpired`, never `NotFound`. Only the attachment's own principal's use
/// renews or ends it: another's passes untouched, for the verb's own check to refuse `Forbidden` (before 2026-11-09 a
/// use renewed the attachment ahead of that check, so any channel that named the id kept another account's attachment
/// alive; fixtures' sweep after A-255). Any other attachment, or none, passes, for the verb's own lookup to answer.
pub(crate) fn use_attachment(
  state: &mut ShardState,
  principal: &Principal,
  attachment: u64,
) -> Result<(), Refusal> {
  let expired = Refusal::AttachmentExpired { attachment };
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return if state.db.partition().attachment_expired(attachment) {
      Err(expired)
    } else {
      Ok(())
    };
  };
  if &record.principal != principal {
    return Ok(());
  }
  let Consumer::Termed { expires_ns } = record.consumer else {
    return Ok(());
  };
  let now = state.clock.monotonic_ns();
  if expires_ns <= now {
    end_attachment(state, &record, Ending::Expired).map_err(|e| refusal_of_db(&e))?;
    return Err(expired);
  }
  // The use is a sample of its holder's pace, which its term follows (A-384).
  state
    .attachment_paces
    .entry(attachment)
    .and_modify(|pace| pace.used(now))
    .or_insert_with(|| crate::state::AttachmentPace::begun(now));
  let term = attachment_term(state, attachment);
  if expires_ns.saturating_sub(now) < term / 2 {
    let renewal = Op::AttachmentRenewed {
      id: attachment,
      expires_ns: now.saturating_add(term),
    };
    state
      .db
      .mutate(&mut state.segment, &renewal, now)
      .map_err(|e| refusal_of_db(&e))?;
  }
  Ok(())
}

/// Ends every CLI attachment of this partition whose term has lapsed (A-230): as a detach would, releasing the write
/// lease it held, recorded as expired. Run at the reaper's cadence, so an abandoned one ends within a term and a sweep.
/// The attachments ended.
pub(crate) fn expire_lapsed_attachments(state: &mut ShardState) -> usize {
  let now = state.clock.monotonic_ns();
  let lapsed: Vec<AttachmentRecord> = state
    .db
    .partition()
    .attachments()
    .filter(
      |record| matches!(record.consumer, Consumer::Termed { expires_ns } if expires_ns <= now),
    )
    .cloned()
    .collect();
  let mut ended = 0usize;
  for record in lapsed {
    if end_attachment(state, &record, Ending::Expired).is_ok() {
      ended = ended.saturating_add(1);
    }
  }
  ended
}

/// Ends an attachment on its owner shard — the `detach` verb's effect, and the kernel's `UMNT` of a host
/// mount's (§4.6, §4.13; AUD-01): the record removed as a recorded operation, a green pin dropped, and,
/// when it was the holder's last attachment of the volume and the holder holds the write lease, the
/// lease released (D-16). The caller has authorized the end — the verb by the principal, the `UMNT` by
/// the mount capability. The typed database refusal when the removal could not be recorded.
pub(crate) fn end_attachment(
  state: &mut ShardState,
  record: &AttachmentRecord,
  ending: Ending,
) -> Result<(), DbError> {
  let now = state.clock.monotonic_ns();
  let removal = match ending {
    Ending::Expired => Op::AttachmentExpired { id: record.id },
    Ending::Detached | Ending::Otherwise => Op::AttachmentRemoved { id: record.id },
  };
  state.db.mutate(&mut state.segment, &removal, now)?;
  // Its pace goes with it (A-384).
  state.attachment_paces.remove(&record.id);
  crate::merge_service::forget_attachment(state, record.id);
  // A mount that survived a takeover holds its volume's kept files until it ends, however it ends (U10).
  if let Some(&handle) = state.by_id.get(&record.volume)
    && let Ok(slot) = state.volumes.get_mut(handle)
    && slot
      .volume
      .end_takeover_holder(&mut state.store, record.id)
      .is_err()
  {
    state.count(TAKEOVER_KEPT_UNRELEASED, 1);
  }
  // A guest device's record ends its device: the loop is asked to revoke and runs its terminal step, which
  // sweeps the device's references through its view and only then closes it (AUD-29-68).
  #[cfg(unix)]
  let a_device_closes_the_view = crate::virtiofs::revoke_attachment_device(state, record.id);
  #[cfg(not(unix))]
  let a_device_closes_the_view = false;
  // A snapshot attachment with no live device releases its view now, unpinning the snapshot (AUD-29-76).
  if !a_device_closes_the_view {
    crate::snapshot_view::end(state, record.id);
  }
  // A Shards stream's record ends its stream: its task wakes on the stream's own end and sweeps what its guest held
  // (`crate::shards`). Every end, not only a detach: nothing beneath the stream is exposed by ending it.
  crate::shards::end_stream(state, record.id);
  // A FUSE mount of the attachment is unmounted only when its user detached it; the kernel's disconnect then ends
  // its serve task. Any other end leaves the mount in place, its requests refused under the revoked registry
  // attachment below until its user unmounts it: an unmount nobody asked for lets the next write by path land on
  // the disk beneath the mount point (conditions 3 and 4; `docs/bugs/2026-10-06-an-ended-mount-let-writes-reach-the-disk-beneath.md`).
  if ending == Ending::Detached {
    #[cfg(target_os = "linux")]
    crate::fuse::unmount_if_mounted(state, record.id);
  }
  // However it ends, a FUSE mount of the attachment serves nothing more: its registry attachment is revoked, as a host
  // mount's is below, so a revoked consumer's mount reaches nothing (AUD-29-84).
  #[cfg(target_os = "linux")]
  crate::fuse::fence_if_mounted(state, record.id);
  // A WinFsp mount ends with its attachment, however it ends: a drive letter has no directory beneath it for a later
  // write by path to reach, so nothing is exposed by removing it (§4.6 "Windows").
  #[cfg(windows)]
  crate::winfsp::end_if_mounted(state, record.id);
  // A macOS NFS mount is no longer this daemon's once its attachment ends: its requests are refused under the revoked
  // registry attachment below, and a path on it is a stranger's (A-227).
  #[cfg(target_os = "macos")]
  state.own_nfs_mounts.remove(&record.id);
  // The registry attachment a mount's requests rode ends with the record: revoked so no later request
  // is admitted under it, drained so its slot is reused (GAP-A9-4).
  if let Some(mount) = state.mount_attachments.remove(&record.id) {
    state.attachments.revoke(mount.registry);
    state.attachments.drain(mount.registry);
  }
  // What NFSv4 clients held open through it is held no longer: no request is served under it again (A-260).
  crate::nfs_state::release_attachment(state, record.volume, record.id);
  // The last write attachment of the holder releases the lease.
  let holds_another = state
    .db
    .partition()
    .attachments_of(record.volume)
    .iter()
    .any(|a| a.principal == record.principal);
  let lease_is_ours = state
    .db
    .partition()
    .volume(record.volume)
    .and_then(|v| v.lease.as_ref())
    .is_some_and(|l| l.holder == record.principal);
  if !holds_another && lease_is_ours {
    let released = state.db.mutate(
      &mut state.segment,
      &Op::LeaseReleased {
        volume: record.volume,
      },
      now,
    );
    count_secondary(state, released);
  }
  Ok(())
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
  match versions.reserve(new.saturating_sub(old)) {
    Ok(c) => Ok(Some(c)),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      Err(Refusal::BudgetExceeded { available })
    }
    Err(e) => Err(refusal_of_mem(&e)),
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
    versions.release(slates_mem::budget::VersionCredit {
      slots: old.saturating_sub(new),
    });
  }
  slates_mem::budget::VersionCredit { slots: new }
}

/// The version slots a resized volume's reservation moves to: a bounded claim's new allowance; a dynamic volume's
/// unchanged (A-342), since it reserves none of its allowance and holds slots for its inodes instead.
fn resized_reservation(volume: &Volume, old: u64, new_allowance: u64) -> u64 {
  if volume.holds_versions() {
    old
  } else {
    new_allowance
  }
}

/// The version credit a resized volume's slot holds once the core accepted the resize: a bounded claim's reservation
/// settled from its `old` slots to its new allowance ([`settle_version_reservation`]); a dynamic volume's unchanged
/// (A-342), as it reserves none.
fn settle_resized_credit(
  versions: &mut slates_mem::budget::VersionBudget,
  volume: &Volume,
  credit: Option<slates_mem::budget::VersionCredit>,
  (old, new_allowance): (u64, u64),
) -> Option<slates_mem::budget::VersionCredit> {
  if volume.holds_versions() {
    return credit;
  }
  Some(settle_version_reservation(versions, old, new_allowance))
}

fn resize(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  size: SizeClass,
) -> ReplyBody {
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Resize) {
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
  let reserved = resized_reservation(&slot.volume, old_version, new_allowance);
  let version_grown =
    match grow_version_reservation(&mut state.store.versions, old_version, reserved) {
      Ok(v) => v,
      Err(refusal) => return refused(refusal),
    };
  // A bounded volume's byte reservation moves with the limit: grow first (refused whole if the
  // reserve cannot cover it), shrink after the core accepted the new limit. A failure here rolls
  // back the version growth taken above, so a refused resize changes neither budget.
  let old = slot.reservation;
  let grow = old.map_or(0, |r| limit.saturating_sub(r.bytes));
  let grown = if grow > 0 {
    match state.store.reserve(grow) {
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
        return refused(refusal_of_mem(&e));
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
  // Never refused: `new_allowance` is at least the live count, the setter's one refusal. Counted all the same.
  let allowed = slot.volume.set_inode_allowance(new_allowance);
  slot.version_credit = settle_resized_credit(
    &mut state.store.versions,
    &slot.volume,
    slot.version_credit,
    (old_version, new_allowance),
  );
  if let Some(old) = old {
    let combined = old.bytes.saturating_add(grown.map_or(0, |g| g.bytes));
    state
      .store
      .budget
      .release(slates_mem::budget::Reservation { bytes: combined });
    match state.store.reserve(limit) {
      Ok(r) => slot.reservation = Some(r),
      Err(_) => slot.reservation = None,
    }
  }
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
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
  if let Some(reply) = crate::merge_service::destroy_merge_volume(state, principal, volume) {
    return reply;
  }
  if let Some(reply) = destroy_unrecovered(state, principal, volume) {
    return reply;
  }
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).admin {
    return forbidden("destroy");
  }
  // A volume whose snapshot a green names as its origin is held with it (A-217): the green's recovery walks that
  // snapshot, so the volume is refused as the pinned snapshot's own destroy is, until the green is gone.
  if state
    .db
    .partition()
    .green_origin_refs()
    .any(|(_, held)| held.volume == record.id)
  {
    return refused(refusal_of_vfs(&slates_vfs::VfsError::Pinned));
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
  // The volume's guest devices are revoked first (AUD-29-68): each ends at its next pass boundary, its terminal
  // step sweeping its references through its volume or view while they still exist. The teardown waits for
  // them (`step_destroys` starts it once none serves the volume); with none, it starts now.
  // Its Shards streams end the same way, each sweeping its guest's references before the teardown (A-161).
  #[cfg(unix)]
  let devices_serving = crate::virtiofs::revoke_volume_devices(state, record.id)
    .saturating_add(crate::shards::end_volume_streams(state, record.id));
  #[cfg(not(unix))]
  let devices_serving = crate::shards::end_volume_streams(state, record.id);
  if devices_serving == 0
    && let Err(e) = start_teardown(state, handle, record.id)
  {
    return refused(refusal_of_vfs(&e));
  }
  // The destroy is stepped in this shard's serve rounds (`step_destroys`), which otherwise run only for
  // this shard's own clients: a destroy forwarded here from another shard's client would wait, its
  // reservation held, until one of this shard's clients spoke
  // (docs/bugs/2026-09-29-a-destroy-on-a-shard-without-a-client-never-completed.md). The round, woken, keeps
  // running while any destroy is unfinished.
  if let Some(task) = state.server_task {
    slates_rt::registry::wake(task.0);
  }
  ReplyBody::Destroyed
}

/// After a rolled-back publication (`Db::commit` re-derived the partition from durable state;
/// AUD-06): every in-memory object a verb created for a catalog record that no longer exists is
/// released — a volume whose `VolumeCreated` was never published (its slot removed, its content
/// returned to the store, its credits returned to the shard's ledgers exactly as a completed destroy
/// returns them), and a green or work state keyed by a volume the catalog does not hold. The
/// volume is at most one verb old — a fresh create is empty and a fresh clone has not diverged —
/// so its destroy completes within the destroy slice budget the cooperative path uses; the loop is
/// bounded by that volume's own extent. Returns how many volumes were released.
pub(crate) fn reconcile_unpublished_effects(state: &mut ShardState) -> usize {
  // The audit records the refused atom owed the control partition went with it (A-265).
  state.landing.audit_owed.clear();
  let orphans: Vec<(DbVolumeId, Handle<VolumeSlot>)> = state
    .by_id
    .iter()
    .filter(|(id, _)| state.db.partition().volume(**id).is_none())
    .map(|(id, h)| (*id, *h))
    .collect();
  let budget = state
    .config
    .step_quantum_ns()
    .saturating_mul(DESTROY_SLICE_PERMILLE)
    / PERMILLE;
  for (id, handle) in &orphans {
    if let Ok(mut slot) = state.volumes.remove(*handle) {
      if slot.volume.destroy(&mut state.store).is_ok() {
        while !matches!(
          slot.volume.destroy_step(&mut state.store, budget.max(1)),
          Ok(DestroyProgress::Done) | Err(_)
        ) {}
      }
      release_slot_credits(state, &slot);
    }
    state.by_id.remove(id);
    crate::live_ship::forget(state, *id);
  }
  let partition = state.db.partition();
  state.greens.retain(|id, _| partition.volume(*id).is_some());
  let mut released = 0u64;
  state.works.retain(|id, work| {
    let keep = partition.volume(*id).is_some();
    if !keep {
      released = released.saturating_add(work.charged);
    }
    keep
  });
  crate::work_charge::release(state, released);
  // A rolled-back clone's pin on its origin's snapshot: the record that named the origin went with the rollback, so the
  // pins are recounted against the lineage the catalog still records, as a recovery recounts them. Before 2026-11-18
  // the teardown kept the pin, and the snapshot's destroy was refused `Pinned` until a restart.
  if !orphans.is_empty() {
    reconcile_clone_pins(state);
  }
  // A Shards stream whose attachment the rollback removed serves nothing, and its end is handed to no one (A-161).
  crate::shards::reconcile_unrecorded(state);
  // Nor does a vhost-user device the verb handed out (G-vhost).
  #[cfg(target_os = "linux")]
  crate::virtiofs::reconcile_unrecorded(state);
  orphans.len()
}

/// Begins a destroying volume's teardown: its snapshot views close first, unpinning the snapshots the destroy
/// frees (AUD-29-76), then its objects are queued for the cooperative destroy.
fn start_teardown(
  state: &mut ShardState,
  handle: Handle<VolumeSlot>,
  volume: DbVolumeId,
) -> Result<(), slates_vfs::VfsError> {
  crate::snapshot_view::end_all_of(state, volume);
  match state.volumes.get_mut(handle) {
    Ok(slot) => slot.volume.destroy(&mut state.store),
    Err(_) => Ok(()),
  }
}

/// Whether a destroying volume's teardown may run now: begun already, or begun here because no guest device
/// serves the volume any more. A volume a device still serves waits (its device ends at its next pass).
fn teardown_started(
  state: &mut ShardState,
  handle: Handle<VolumeSlot>,
  volume: DbVolumeId,
) -> bool {
  if state
    .volumes
    .get(handle)
    .is_ok_and(|slot| slot.volume.is_destroying())
  {
    return true;
  }
  #[cfg(unix)]
  if crate::virtiofs::revoke_volume_devices(state, volume) > 0 {
    return false;
  }
  if crate::shards::end_volume_streams(state, volume) > 0 {
    return false;
  }
  if let Err(e) = start_teardown(state, handle, volume) {
    state.count(refusal_name(&refusal_of_vfs(&e)), 1);
  }
  true
}

/// One cooperative destroy slice per destroying volume and per stale copy relinquished to a re-adoption (A-362); a
/// finished one leaves the tables.
pub fn step_destroys(state: &mut ShardState) -> bool {
  let budget = state
    .config
    .step_quantum_ns()
    .saturating_mul(DESTROY_SLICE_PERMILLE)
    / PERMILLE;
  let mut any = step_relinquished(state, budget);
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
  for (id, handle) in destroying {
    // A volume whose guest devices still serve it waits for their ends (the round keeps running meanwhile).
    if !teardown_started(state, handle, id) {
      any = true;
      continue;
    }
    let done = match state.volumes.get_mut(handle) {
      Ok(slot) => matches!(
        slot.volume.destroy_step(&mut state.store, budget.max(1)),
        Ok(DestroyProgress::Done) | Err(_)
      ),
      Err(_) => true,
    };
    // A slice ran; the round goes on while it has more (the serve loop keeps stepping).
    any |= !done;
    if done {
      // If this volume is a clone, capture its pin on the origin snapshot before the record goes, so
      // the pin can be released once its destroy completes (§4.5): the origin can then reclaim that
      // snapshot. Best-effort — the origin may be gone or on another shard.
      let origin_pin = origin_pin_of(state, id);
      let now = state.clock.monotonic_ns();
      let name = state
        .db
        .partition()
        .volume(id)
        .map(|record| record.name.clone());
      if let Err(e) = state
        .db
        .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now)
      {
        // Not recorded: the volume keeps its slot and credits, counted by the refusal's name, and is
        // recorded again at the reaper's next cadence (`reap_loop` steps destroys), not in a busy round
        // — before 2026-09-29 the refusal was discarded and the tables let go of a volume the catalog
        // still held.
        state.count(refusal_name(&refusal_of_db(&e)), 1);
        continue;
      }
      any = true;
      // A clone whose name hashes elsewhere gives its alias back there (A-236).
      if let Some(name) = name {
        release_name_alias(state, id, &name);
      }
      release_origin_pin(state, origin_pin);
      if let Ok(slot) = state.volumes.remove(handle) {
        release_slot_credits(state, &slot);
      }
      state.by_id.remove(&id);
      crate::live_ship::forget(state, id);
      retire_local_tombstones(state);
    }
  }
  any
}

/// One destroy slice of each stale copy relinquished to a re-adoption ([`relinquish_superseded`], A-362), within `budget`
/// each; a finished one gives its credits back and leaves. Whether any has more to tear down.
fn step_relinquished(state: &mut ShardState, budget: u64) -> bool {
  let mut any = false;
  for mut slot in std::mem::take(&mut state.relinquished) {
    let done = matches!(
      slot.volume.destroy_step(&mut state.store, budget.max(1)),
      Ok(DestroyProgress::Done) | Err(_)
    );
    if done {
      release_slot_credits(state, &slot);
      state.count(TAKEOVER_RELINQUISHED_FREED, 1);
    } else {
      any = true;
      state.relinquished.push(slot);
    }
  }
  any
}

/// The origin snapshot volume `id` pins, captured before its record goes so the pin can be given back once its destroy
/// is recorded (§4.5): a clone's origin, or the snapshot an image volume was built from (A-160).
fn origin_pin_of(state: &ShardState, id: DbVolumeId) -> Option<(DbVolumeId, DbSnapshotId)> {
  state
    .db
    .partition()
    .lineage(id)
    .map(|e| (e.origin_volume, e.origin_snapshot))
    .or_else(
      || match state.db.partition().volume(id).map(|record| &record.base) {
        Some(BaseRecord::Image { volume, snapshot }) => Some((*volume, *snapshot)),
        _ => None,
      },
    )
}

/// Gives back a destroyed volume's pin on its origin's snapshot ([`origin_pin_of`]), when the origin is served here.
/// Best-effort: the origin may be gone or on another shard.
fn release_origin_pin(state: &mut ShardState, origin_pin: Option<(DbVolumeId, DbSnapshotId)>) {
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
}

/// Destroys a volume recovery could not rebuild (§4.8; GAPS, 2026-11-08), or `None` for any other volume. It has no
/// slot and holds nothing in this process, so, as a merge volume's destroy is, it is recorded in one step: its
/// attachments' records end, then `Destroying`, the lease's release and `VolumeDestroyed`, which takes its snapshots'
/// records, its lineage and its keys with it and leaves the tombstone a fleet's holders release its copies by. Its name
/// alias and its pin on an origin's snapshot are given back. Nothing is materialized. Before, every verb on such a
/// volume answered `NotRecovered`, `destroy` too, so its record stayed for good (banned item 8).
fn destroy_unrecovered(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
) -> Option<ReplyBody> {
  let id = to_db_volume(volume);
  if !state.unrecovered.contains_key(&id) {
    return None;
  }
  let Some(record) = state.db.partition().volume(id).cloned() else {
    state.unrecovered.remove(&id);
    return Some(refused(Refusal::NotFound));
  };
  if !rights_of(&record, principal).admin {
    return Some(forbidden("destroy"));
  }
  // A volume whose snapshot a green names as its origin is held with it (A-217), as a rebuilt one is.
  if state
    .db
    .partition()
    .green_origin_refs()
    .any(|(_, held)| held.volume == id)
  {
    return Some(refused(refusal_of_vfs(&slates_vfs::VfsError::Pinned)));
  }
  Some(match record_unrecovered_destroy(state, &record) {
    Ok(()) => ReplyBody::Destroyed,
    Err(e) => refused(refusal_of_db(&e)),
  })
}

/// Records [`destroy_unrecovered`]'s one step for `record`, then gives back what the volume held outside the catalog.
/// A refusal part-way leaves what was recorded: a `Destroying` volume is completed at the next start
/// (`complete_recovered_destroys`), and one still unrecovered is destroyed again.
fn record_unrecovered_destroy(
  state: &mut ShardState,
  record: &VolumeRecord,
) -> Result<(), DbError> {
  let attachments: Vec<AttachmentRecord> = state
    .db
    .partition()
    .attachments_of(record.id)
    .into_iter()
    .cloned()
    .collect();
  for attachment in &attachments {
    end_attachment(state, attachment, Ending::Otherwise)?;
  }
  let origin_pin = origin_pin_of(state, record.id);
  let now = state.clock.monotonic_ns();
  for op in [
    Op::VolumeStateChanged {
      id: record.id,
      state: VolumeState::Destroying,
    },
    Op::LeaseReleased { volume: record.id },
    Op::VolumeDestroyed { id: record.id },
  ] {
    state.db.mutate(&mut state.segment, &op, now)?;
  }
  state.unrecovered.remove(&record.id);
  release_name_alias(state, record.id, &record.name);
  release_origin_pin(state, origin_pin);
  crate::live_ship::forget(state, record.id);
  retire_local_tombstones(state);
  Ok(())
}

/// Returns every credit a torn-down volume held to the shard's ledgers (§4.2 accounting through
/// teardown): its byte reservation, a bounded claim's inode allowance or a dynamic volume's version
/// hold (so the version slab is never over-offered; A-342), its records' metadata reservation, and the
/// growth a dynamic volume acquired from the shard budget as it wrote.
fn release_slot_credits(state: &mut ShardState, slot: &VolumeSlot) {
  // The volume's hold on its seal key goes with it (A-208); its sealed chunks hold the key while any of them lives.
  if !crate::content_cipher::release_seal_key(&mut state.store, slot.volume.seal_key()) {
    state.count(crate::content_cipher::KEY_RELEASE_REFUSED, 1);
  }
  // The copies of its files the direct map handed out stop following them (§4.6, A-171), so a prefix a new volume is
  // given once the count wraps never writes into a map of a file it does not hold. Each copy stays held, and charged,
  // until its client releases it or is reaped: the client's own map keeps the object's pages, which the daemon wrote.
  let _detached = state
    .store
    .shards_backings
    .detach_volume_peers(slot.volume.prefix());
  if let Some(r) = slot.reservation {
    state.store.budget.release(r);
  }
  if let Some(c) = slot.version_credit {
    state.store.versions.release(c);
  }
  // A dynamic volume's slots for its inodes, taken from the unpromised capacity as it made them (A-342).
  release_version_hold(&mut state.store, slot.volume.version_hold());
  if let Some(m) = slot.metadata_credit {
    state.store.metadata.release(m);
  }
  let held = slot.volume.budget_hold();
  if held > 0 {
    state
      .store
      .budget
      .release(slates_mem::budget::Reservation { bytes: held });
  }
  // The journal's growth beyond its floor, granted from the metadata ledger as a live reader needed it (A-157).
  let grown = slot.volume.journal_grown();
  if grown > 0 {
    state
      .store
      .metadata
      .release(slates_mem::budget::MetadataCredit { bytes: grown });
  }
}

fn status(state: &mut ShardState, principal: &Principal, volume: VolumeId) -> ReplyBody {
  if let Some(reply) = crate::merge_service::status_merge_volume(state, principal, volume) {
    return reply;
  }
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  let rights = rights_of(&record, principal);
  if !rights.read {
    return forbidden("status");
  }
  let transports = crate::transports::report(&crate::transports::situation(state, &rights));
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let accounting = slot.volume.accounting();
  let journal = journal_report(&slot.volume);
  let takeover_kept = u32::try_from(slot.volume.takeover_kept()).unwrap_or(u32::MAX);
  let unchanged_since_head = record.epoch > 0
    && slot
      .volume
      .unchanged_since(core_snapshot(SnapshotId {
        value: record.head.value,
      }))
      .unwrap_or(false);
  // An overlay's base, read once: text for people (`drifted`, `watcher`) and typed for consumers (`base`, A-299), the
  // same entries in both, filled as far as one reply holds (A-267).
  let read = drift_entries(slot, &mut state.store);
  let attachments = state.db.partition().attachments_of(record.id).len();
  let mut reply = ReplyBody::Status {
    report: Box::new(StatusReport {
      id: volume,
      name: record.name.clone(),
      referenced_bytes: accounting.referenced_bytes,
      unique_bytes: accounting.unique_bytes,
      lease_epoch: record.lease.as_ref().map(|l| l.epoch),
      attachments: u32::try_from(attachments).unwrap_or(u32::MAX),
      head: SnapshotId {
        value: record.head.value,
      },
      drifted: Vec::new(),
      watcher: read.watcher.clone(),
      snapshots: u32::try_from(slot.volume.snapshot_count()).unwrap_or(u32::MAX),
      placed: placed_state(state, &record),
      // The daemon's NFS port (§4.6), 0 until the listener binds; report it so a client can mount.
      nfs_port: u16::try_from(crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire))
        .ok()
        .filter(|port| *port != 0),
      transports: Box::new(transports),
      mounts: Vec::new(),
      mounts_elided: 0,
      journal,
      takeover_kept,
      base: read.base.clone(),
      loss: loss_report(state, &record, unchanged_since_head),
      drifted_elided: 0,
    }),
  };
  // The mounts, then the drift, each in the room the report leaves (A-267): the mounts up to their budget.
  let room = reply_room(&reply);
  if let ReplyBody::Status { report } = &mut reply {
    (report.mounts, report.mounts_elided) =
      mounts_of(state, record.id, room.min(MOUNTS_REPORT_BYTES));
  }
  with_drift(reply, read)
}

/// A status reply's drift, filled in last (A-267): the status stays one reply, so it carries the first entries that fit
/// beside its other fields — the same entries as text for people (`drifted`) and typed for consumers (`base`, A-299) —
/// and counts the rest, which `Drifted` reads typed. An unreadable base's one text entry says why.
fn with_drift(reply: ReplyBody, read: DriftRead) -> ReplyBody {
  let ReplyBody::Status { mut report } = reply else {
    return reply;
  };
  let DriftRead { items, refused, .. } = read;
  let texts: Vec<String> = match refused {
    Some(refusal) => vec![format!("status refused: {refusal:?}")],
    None => items.iter().map(DriftItem::text).collect(),
  };
  let total = texts.len();
  let room = reply_room(&ReplyBody::Status {
    report: report.clone(),
  });
  let mut used = 0usize;
  for (index, text) in texts.into_iter().enumerate() {
    let typed = items.get(index).map_or(0, DriftItem::typed_len);
    let bytes = encoded_len(&text).saturating_add(typed);
    if used.saturating_add(bytes) > room {
      break;
    }
    used = used.saturating_add(bytes);
    report.drifted.push(text);
    if let Some(item) = items.get(index) {
      item.push_into(&mut report.base);
    }
  }
  report.drifted_elided =
    u32::try_from(total.saturating_sub(report.drifted.len())).unwrap_or(u32::MAX);
  ReplyBody::Status { report }
}

/// One entry of an overlay's drift as a status reads it (§4.15 "Drift"): changed on the disk, or its re-check refused.
enum DriftItem {
  /// The disk changed it.
  Drifted(slates_ipc::protocol::DriftEntry),
  /// The host refused its re-check now; the refusal as people read it.
  Unverified {
    /// The entry.
    path: String,
    /// The host's refusal.
    refusal: String,
  },
}

impl DriftItem {
  /// An entry of a page the volume core read ([`slates_vfs::base::DriftPageEntry`]).
  fn of_page(entry: slates_vfs::base::DriftPageEntry) -> DriftItem {
    match entry {
      slates_vfs::base::DriftPageEntry::Drifted { path, kind } => {
        DriftItem::Drifted(slates_ipc::protocol::DriftEntry {
          path,
          kind: wire_drift_kind(kind),
        })
      }
      slates_vfs::base::DriftPageEntry::Unverified { path, refusal } => DriftItem::Unverified {
        path,
        refusal: format!("{refusal}"),
      },
    }
  }

  /// The entry's path.
  fn path(&self) -> &str {
    match self {
      DriftItem::Drifted(entry) => &entry.path,
      DriftItem::Unverified { path, .. } => path,
    }
  }

  /// The entry as text for people: `PATH (KIND)`, or `PATH (unverified: REFUSAL)`.
  fn text(&self) -> String {
    match self {
      DriftItem::Drifted(entry) => format!("{} ({:?})", entry.path, entry.kind),
      DriftItem::Unverified { path, refusal } => format!("{path} (unverified: {refusal})"),
    }
  }

  /// The bytes the entry takes in its typed list.
  fn typed_len(&self) -> usize {
    match self {
      DriftItem::Drifted(entry) => encoded_len(entry),
      DriftItem::Unverified { path, .. } => encoded_len(path),
    }
  }

  /// Puts the entry in a read base's typed lists.
  fn push_into(&self, base: &mut Option<slates_ipc::protocol::BaseReport>) {
    if let Some(slates_ipc::protocol::BaseReport::Read {
      drifted,
      unverified,
      ..
    }) = base
    {
      match self {
        DriftItem::Drifted(entry) => drifted.push(entry.clone()),
        DriftItem::Unverified { path, .. } => unverified.push(path.clone()),
      }
    }
  }
}

/// An overlay's base as one status reads it: its drifted and unverified entries in path order, the watcher as text,
/// and the typed report with its lists empty (filled as far as a reply holds); none for a volume with no base. An
/// unreadable base keeps the volume core's refusal.
struct DriftRead {
  items: Vec<DriftItem>,
  watcher: String,
  base: Option<slates_ipc::protocol::BaseReport>,
  refused: Option<slates_vfs::VfsError>,
}

/// A drift's kind as the wire carries it.
fn wire_drift_kind(kind: slates_vfs::base::DriftKind) -> slates_ipc::protocol::DriftKind {
  use slates_ipc::protocol::DriftKind;
  match kind {
    slates_vfs::base::DriftKind::Modified => DriftKind::Modified,
    slates_vfs::base::DriftKind::Deleted => DriftKind::Deleted,
    slates_vfs::base::DriftKind::Replaced => DriftKind::Replaced,
    slates_vfs::base::DriftKind::TypeChanged => DriftKind::TypeChanged,
  }
}

/// Reads an overlay volume's base once, re-checking every witnessed entry ([`DriftRead`]).
fn drift_entries(slot: &mut VolumeSlot, store: &mut slates_vfs::volume::Store) -> DriftRead {
  use slates_ipc::protocol::{BaseReport, DriftEntry, WatcherState};
  let Some(host) = slot.host.as_mut() else {
    return DriftRead {
      items: Vec::new(),
      watcher: WATCHER_SCRATCH.to_owned(),
      base: None,
      refused: None,
    };
  };
  match slot.volume.with_host(host).status(store) {
    Ok(status) => {
      let mut items: Vec<DriftItem> = status
        .drift
        .iter()
        .map(|(path, kind)| {
          DriftItem::Drifted(DriftEntry {
            path: path.clone(),
            kind: wire_drift_kind(*kind),
          })
        })
        .chain(
          status
            .unverified
            .iter()
            .map(|(path, refusal)| DriftItem::Unverified {
              path: path.clone(),
              refusal: format!("{refusal}"),
            }),
        )
        .collect();
      items.sort_by(|a, b| a.path().cmp(b.path()));
      DriftRead {
        items,
        watcher: format!("{:?}", status.watcher),
        base: Some(BaseReport::Read {
          drifted: Vec::new(),
          unverified: Vec::new(),
          watcher: match status.watcher {
            slates_vfs::host::WatchState::Live => WatcherState::Live,
            slates_vfs::host::WatchState::Overflowed => WatcherState::Overflowed,
            slates_vfs::host::WatchState::Unavailable => WatcherState::Unavailable,
          },
        }),
        refused: None,
      }
    }
    Err(refusal) => DriftRead {
      items: Vec::new(),
      watcher: WATCHER_UNAVAILABLE.to_owned(),
      base: Some(BaseReport::Unreadable),
      refused: Some(refusal),
    },
  }
}

/// `Drifted`: one page of the volume's drift past inode `after` (A-267), typed as a status's `base` types it: the volume
/// core visits, in inode order, the inodes a status would list, re-checking the witnessed ones now
/// ([`slates_vfs::base::Overlay::drift_page`]), until a reply chunk's worth is found or the page has spent its slice of
/// the shard's step ([`DRIFT_SLICE_PERMILLE`]). So a walk re-checks each witnessed entry once and a page never holds the
/// shard longer than its slice; reading the whole status a page re-checked every entry once a page. An inode's entries
/// go on one page together; an entry too long for any page alone is passed over and counted. A base that cannot be
/// read now is refused as the volume core refused it.
fn drifted(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  after: Option<u64>,
) -> ReplyBody {
  let (handle, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).read {
    return forbidden("drifted");
  }
  let room = reply_room(&ReplyBody::Drifted {
    drifted: Vec::new(),
    unverified: Vec::new(),
    next: Some(u64::MAX),
    oversized: u32::MAX,
  });
  let slice = state
    .config
    .step_quantum_ns()
    .saturating_mul(DRIFT_SLICE_PERMILLE)
    / PERMILLE;
  let ShardState { volumes, store, .. } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let Some(host) = slot.host.as_mut() else {
    // A volume with no base has no drift.
    return ReplyBody::Drifted {
      drifted: Vec::new(),
      unverified: Vec::new(),
      next: None,
      oversized: 0,
    };
  };
  let started = slates_rt::futures::now_ns();
  let mut spent = || slates_rt::futures::now_ns().saturating_sub(started) >= slice;
  let read = match slot.volume.with_host(host).drift_page(
    store,
    after.map(slates_vfs::ids::InodeNo),
    drift_batch(room),
    &mut spent,
  ) {
    Ok(read) => read,
    Err(e) => return refused(refusal_of_vfs(&e)),
  };
  drift_page_reply(read, after, room)
}

/// A page of drift as one reply carries it: whole inodes in order while they fit `room`, an entry too long for any page
/// alone passed over and counted, and the inode the next page starts after. A page always takes its first inode (as
/// much of it as fits), so a walk moves.
fn drift_page_reply(
  read: slates_vfs::base::DriftPageRead,
  after: Option<u64>,
  room: usize,
) -> ReplyBody {
  let mut base = Some(slates_ipc::protocol::BaseReport::Read {
    drifted: Vec::new(),
    unverified: Vec::new(),
    watcher: slates_ipc::protocol::WatcherState::Unavailable,
  });
  let mut used = 0usize;
  let mut oversized = 0u32;
  let mut cursor = after;
  let mut taken_any = false;
  let mut stopped_short = false;
  for (no, found) in read.entries {
    let items: Vec<DriftItem> = found.into_iter().map(DriftItem::of_page).collect();
    let (sendable, too_long): (Vec<DriftItem>, Vec<DriftItem>) =
      items.into_iter().partition(|item| item.typed_len() <= room);
    let bytes: usize = sendable.iter().map(DriftItem::typed_len).sum();
    if taken_any && used.saturating_add(bytes) > room {
      stopped_short = true;
      break;
    }
    oversized = oversized.saturating_add(u32::try_from(too_long.len()).unwrap_or(u32::MAX));
    for item in sendable {
      let len = item.typed_len();
      if used.saturating_add(len) > room {
        // Only the page's first inode can come here (two long entries of one inode): the rest of it is counted.
        oversized = oversized.saturating_add(1);
        continue;
      }
      used = used.saturating_add(len);
      item.push_into(&mut base);
    }
    taken_any = true;
    cursor = Some(no.0);
  }
  let next = if stopped_short {
    cursor
  } else {
    read.next.map(|no| no.0)
  };
  let (drifted, unverified) = match base {
    Some(slates_ipc::protocol::BaseReport::Read {
      drifted,
      unverified,
      ..
    }) => (drifted, unverified),
    _ => (Vec::new(), Vec::new()),
  };
  ReplyBody::Drifted {
    drifted,
    unverified,
    next,
    oversized,
  }
}

/// Shape: a `Drifted` page's slice of the shard's step budget, parts per thousand: half, as a `changed_since` page's
/// and a destroy slice's, so one page never takes the whole step from the shard's other clients (§4.3). A re-check is a
/// host's open, `fstat` and close, tens of microseconds, so the page looks at the clock after each.
const DRIFT_SLICE_PERMILLE: u64 = 500;

/// Derived: the inodes one drift page asks the volume core for: as many drifted entries as a page could hold at the
/// smallest an entry encodes to (an empty path), so one visit fills a page.
fn drift_batch(room: usize) -> usize {
  let smallest = encoded_len(&slates_ipc::protocol::DriftEntry {
    path: String::new(),
    kind: slates_ipc::protocol::DriftKind::Modified,
  });
  room.checked_div(smallest).unwrap_or(1).max(1)
}

/// A volume's loss window for its status (§4.10 "Auto-seal", AC-8.3, D-18): what an owner loss would lose now, and the
/// takeover that made this node its owner. `unchanged_since_head` is whether the volume is exactly its head snapshot.
fn loss_report(
  state: &ShardState,
  record: &VolumeRecord,
  unchanged_since_head: bool,
) -> slates_ipc::protocol::LossReport {
  let placed = newest_placed_seal(state, record);
  slates_ipc::protocol::LossReport {
    at_risk_ns: at_risk_ns(state, record, unchanged_since_head, placed),
    placed_sequence: placed.map(|snapshot| snapshot.epoch),
    live_shipped: record.policy.live_shipped,
    taken_over: record
      .taken_over
      .map(|taken| slates_ipc::protocol::VolumeTakeover {
        epoch: taken.epoch,
        from_live_log: taken.from_live_log,
        head_sequence: taken.head_sequence,
        past_tolerance: taken.past_tolerance.map(wire_past_tolerance),
      }),
  }
}

/// A takeover's record of going past the volume's tolerance (A-309, A-332), as the loss report carries it.
fn wire_past_tolerance(
  past: slates_db::catalog::TakeoverPastTolerance,
) -> slates_ipc::protocol::PastTolerance {
  let shortfall = |short: slates_db::catalog::RecoveryShortfall| slates_ipc::protocol::Shortfall {
    needed: short.needed,
    remaining: short.remaining,
  };
  slates_ipc::protocol::PastTolerance {
    registers: past.registers.map(shortfall),
    live_log: past.live_log.map(shortfall),
  }
}

/// The newest seal of `record`'s volume placed on `f + 1` hosts, by head sequence: the one a takeover would serve from
/// now (§4.8: a successor adopts the newest committed head, and a seal is recorded placed only once its head is). None
/// on a laptop (`f = 0`), where a seal is placed on the owner alone.
fn newest_placed_seal<'a>(
  state: &'a ShardState,
  record: &VolumeRecord,
) -> Option<&'a SnapshotRecord> {
  if state.fleet.configuration().quorum.f == 0 {
    return None;
  }
  state
    .db
    .partition()
    .snapshots_of(record.id)
    .into_iter()
    .filter(|snapshot| matches!(snapshot.placed, PlacementState::Placed { .. }))
    .max_by_key(|snapshot| snapshot.epoch)
}

/// The window an owner loss would lose of `record`'s volume now (AC-8.3), on this host's monotonic clock:
/// - zero for a live-shipped volume in a fleet, whose writes are durable on its holders before they are answered
///   (A-200);
/// - zero for a volume that is exactly its head snapshot when that snapshot is `placed`, the newest seal on `f + 1`
///   hosts;
/// - otherwise the age of `placed`, the edits after which live on the owner alone;
/// - and with none placed, the age of the volume itself: on a laptop (`f = 0`) nothing is on another host, a
///   live-shipped volume's log included, so the volume's whole life is at risk to the loss of its one host.
///
/// `None` when that time is from another boot of the clock: an unknown window is never reported as zero, as
/// [`mirror_age_ns`] reports the mirror's lag.
fn at_risk_ns(
  state: &ShardState,
  record: &VolumeRecord,
  unchanged_since_head: bool,
  placed: Option<&SnapshotRecord>,
) -> Option<u64> {
  if record.policy.live_shipped && state.fleet.configuration().quorum.f > 0 {
    return Some(0);
  }
  // The shard's clock is the host's monotonic clock (`HostClock`), read here without borrowing it mutably.
  let now = slates_machine::clock::monotonic_ns();
  match placed {
    Some(snapshot) if snapshot.id == record.head && unchanged_since_head => Some(0),
    Some(snapshot) => now.checked_sub(snapshot.taken_ns),
    None => now.checked_sub(record.created_ns),
  }
}

/// A volume's journal for its status (§4.5): its budget and retention, and what its consuming rounds found waiting.
fn journal_report(volume: &Volume) -> slates_ipc::protocol::JournalReport {
  let log = volume.op_log();
  let measure = log.measure();
  let wide = |count: usize| u64::try_from(count).unwrap_or(u64::MAX);
  slates_ipc::protocol::JournalReport {
    budget_bytes: wide(log.budget_bytes()),
    retained_bytes: wide(log.retained_bytes()),
    retained_records: wide(log.len()),
    mean_record_bytes: log.mean_record_bytes(),
    appended: log.head_seq(),
    dropped: log.dropped(),
    rounds: measure.rounds,
    overruns: measure.overruns,
    largest_records: measure.largest.records,
    largest_bytes: measure.largest.bytes,
    largest_lag_ns: measure.largest.lag_ns,
    longest_lag_ns: measure.longest_lag_ns,
    readers: wide(log.readers()),
    slowest_reader_behind: log
      .slowest_reader()
      .map_or(0, |point| log.head_seq().saturating_sub(point)),
    grown_bytes: log.grown(),
    growth_refused: log.growth_refused(),
  }
}

/// The placement of a volume's snapshot as the register configuration computes it: at `f = 0` the
/// owner alone, committed on the local append (§4.8 "Laptop degenerate"). A snapshot places on its
/// volume's candidate holders, so the placement object is the volume's 128-bit id (whose high half
/// names the creator host), not the volume-unique snapshot id.
fn placement_of(state: &ShardState, volume: DbVolumeId) -> PlacementState {
  let placement = state.fleet.configuration().place(ObjectId(volume.bytes));
  if state.fleet.configuration().region_placed(&placement) {
    PlacementState::Placed {
      region: placement.acked.iter().map(|h| h.0).collect(),
      mirror: None,
    }
  } else {
    PlacementState::Local
  }
}

/// Whether `object`'s head at `sequence` is region-placed as the fleet has **actually committed** it (§4.8 "the
/// acknowledging set is recorded in the object's head record"): the acknowledgements the control-shard record plane
/// recorded when it replicated that head (`ShardState::placed_heads`), judged under the configuration's placement now
/// (`head::PlacedHead::placed_under`); at `f = 0` placed on the local append (R8: the laptop is the empty-map
/// degenerate). Reading the computed placement alone reported a fleet's head *never* region-placed, however many
/// holders held it; reading the recorded round's own cohorts kept a head unplaced once its neighbourhood settled.
fn committed_placed(state: &ShardState, object: ObjectId, sequence: u64) -> bool {
  state
    .placed_heads
    .get(&object)
    .filter(|head| head.sequence == sequence)
    .is_some_and(|head| head.placed_under(state.fleet.configuration(), object))
    || state.fleet.configuration().quorum.f == 0
}

/// A volume's head placement for a status reply (§4.8, D-18): whether the head is placed in
/// the region, the mirror's lag (none at `f = 0`), and the owner's host epoch. "No snapshot yet" is
/// the volume's **epoch** being zero, never the head id being the default: the first snapshot a volume
/// takes lands in slab slot 0 at generation 0, whose wire id is exactly the default — so a check on the id
/// mistook every volume's first snapshot for none and reported the creation head's placement instead.
pub(crate) fn placed_state(state: &ShardState, record: &VolumeRecord) -> PlacedState {
  let region = if record.epoch == 0 {
    // No snapshot yet: the catalog register itself is locally committed, so at `f = 0` the
    // head is placed (nothing to replicate until a seal). The placement object is the volume's full
    // 128-bit id (its high half names the creator host); the old code truncated it to that high half
    // alone, so every volume of one creator collided to one placement object — fixed by ObjectId.
    let object = ObjectId(record.id.bytes);
    committed_placed(state, object, CREATION_HEAD_SEQUENCE)
  } else {
    match state.db.partition().snapshot(record.id, record.head) {
      Some(snapshot) => matches!(snapshot.placed, PlacementState::Placed { .. }),
      None => false,
    }
  };
  PlacedState {
    region,
    mirror_age_ns: mirror_age_ns(state, record),
    host_epoch: state.fleet.configuration().host_epoch.0,
  }
}

/// The mirror's lag for a volume (§4.8 "Mirroring"; `docs/wip/mirroring.md` M3): zero once its head snapshot is
/// recorded in the mirror (or it has none), else the time since that snapshot was taken, on this host's monotonic
/// clock, an upper bound on the lag from its home placement. `None` where the region has no mirror, and where the
/// snapshot's time is from another boot of the clock: an unknown lag is never reported as zero.
fn mirror_age_ns(state: &ShardState, record: &VolumeRecord) -> Option<u64> {
  if !state.fleet.configuration().has_mirror {
    return None;
  }
  if record.epoch == 0 {
    return Some(0);
  }
  let snapshot = state.db.partition().snapshot(record.id, record.head)?;
  if matches!(
    snapshot.placed,
    PlacementState::Placed {
      mirror: Some(_),
      ..
    }
  ) {
    return Some(0);
  }
  // The shard's clock is the host's monotonic clock (`HostClock`), read here without borrowing it mutably.
  slates_machine::clock::monotonic_ns().checked_sub(snapshot.taken_ns)
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
  // A green's placement is its merge records' (§4.16 "Commit"), never a store snapshot's.
  if let Some(reply) = crate::merge_service::await_placed_green(state, principal, volume, scope) {
    return reply;
  }
  let (_, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).read {
    return forbidden("await_placed");
  }
  // The snapshot places on its volume's candidate holders — the placement object is the volume id.
  let object = ObjectId(volume.bytes);
  let config = state.fleet.configuration();
  // The region: a snapshot is placed once the content plane recorded it so (§4.10 — its content held by
  // `f + 1` candidates and the head naming it committed, the durable `SnapshotPlaced`); with no snapshot
  // yet (the volume's epoch is zero — never "the head id is the default", which the first snapshot's id
  // also is), the creation head's recorded acknowledgements (sequence 0). `await placed` reports which
  // coverage was placed, never silently upgrading it (§4.4).
  let target = match snapshot {
    Some(snapshot) => Some(to_db_snapshot(snapshot)),
    None if record.epoch == 0 => None,
    None => Some(record.head),
  };
  let region = match target {
    None => committed_placed(state, object, CREATION_HEAD_SEQUENCE),
    Some(target) => match state.db.partition().snapshot(record.id, target) {
      Some(snapshot) => matches!(snapshot.placed, PlacementState::Placed { .. }),
      None => return refused(Refusal::NotFound),
    },
  };
  match scope {
    Scope::Region => ReplyBody::Placed {
      placed: region,
      mirror_age_ns: None,
    },
    // The mirror (§4.10 "Mirroring across regions"; `docs/wip/mirroring.md`): a snapshot is placed there once `f + 1`
    // of its mirror cohort acknowledged its content and the owner recorded it (`SnapshotPlaced { mirror }`). A volume
    // with no snapshot yet has nothing mirrored. Refused `Unsupported` where the region declares no mirror.
    Scope::Mirror if !config.has_mirror => refused(Refusal::Unsupported {
      feature: "mirror".to_owned(),
    }),
    Scope::Mirror => {
      let placed = target
        .and_then(|target| state.db.partition().snapshot(record.id, target))
        .is_some_and(|snapshot| crate::mirror::mirror_placed(&snapshot.placed, config.quorum));
      ReplyBody::Placed {
        placed,
        mirror_age_ns: None,
      }
    }
  }
}

/// Format: the head register's sequence at a volume's creation — the "epoch-one head record" that names
/// no content yet (§4.4 create); each snapshot's placement then writes the next sequence.
pub(crate) const CREATION_HEAD_SEQUENCE: u64 = 0;

/// Adopts a taken-over **green** on the shard its id routes to (§4.16 owner-loss recovery; AUD-14; A-333): records the
/// green by its adoption — its catalog record under the adopted record's name, evidence policy and owner, with the
/// access list of its adopted catalog register at that register's sequence, and the adopted version, its identity and
/// version 0's inputs manifest (`Op::GreenAdopted`), one operation checked as a creation is — and installs the engine the
/// control shard handed it, this node's replica at the adopted version, whose versions up to it are this node's holder
/// ledger. Nothing is recorded or replayed beyond that: the origin's bytes and the adopted versions' increments stay in
/// the ledger (`crate::green_adopt`). Then the completions the ledger's records carried are recorded (A-255) and the
/// adopted head is placed (its records committed at the quorum under the departed owner, the adoption under this node's
/// epoch), so `await placed`, `versions`, reads at any version and new submits serve. Idempotent: a green this shard
/// already serves is left alone, and one recorded by the same adoption is installed. A name taken here, or a volume of
/// another role or another adoption under the id, is refused typed and recorded nothing; the adoption is handed back so
/// its engine waits on the control shard for the next attempt.
pub(crate) fn adopt_taken_over_green(
  state: &mut ShardState,
  id: DbVolumeId,
  adopted: crate::green_adopt::AdoptedGreen,
) -> Result<(), Box<crate::green_adopt::AdoptedGreen>> {
  if state.greens.contains_key(&id) {
    return Ok(());
  }
  if let Err(refusal) = record_adopted_green(state, id, &adopted) {
    state.count(GREEN_TAKEOVER_ADOPTION_REFUSED, 1);
    slates_machine::log_line!(
      "slates-server: partition {}: taken-over green {} not adopted: {refusal:?}",
      state.partition,
      adopted.name
    );
    return Err(Box::new(adopted));
  }
  let Some(record) = state.db.partition().volume(id).cloned() else {
    return Err(Box::new(adopted));
  };
  let crate::green_adopt::AdoptedGreen {
    mut engine,
    completions,
    head,
    ..
  } = adopted;
  engine.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
  state.greens.insert(id, engine);
  // The adopted replica holds every history in full; it is folded oldest-first to the budget and its retention charged,
  // as a recovered green's is (`install_green`). A retention the budget cannot hold fences the green.
  if let Err(short) = crate::merge_service::settle_green_retention(state, id) {
    fence_green(
      state,
      &record,
      crate::merge_service::GreenFence::Retention { short },
    );
    return Ok(());
  }
  record_taken_over_completions(state, &completions);
  let object = ObjectId(id.bytes);
  let placed = state.merge.placed.entry(object).or_insert(head);
  *placed = (*placed).max(head);
  // A round that gave the green up waiting for its cohorts' promises, then adopted it under a later configuration, left
  // its record here (`takeover::give_up_unpromised`); the green is served now.
  state.unrecovered.remove(&id);
  state.count(GREEN_TAKEOVER_ADOPTED, 1);
  Ok(())
}

/// Records green `id` by its adoption (A-333), or finds it recorded by this same adoption: `Err` with the refusal when
/// the id or the name is another volume's here, or the partition refuses the record.
fn record_adopted_green(
  state: &mut ShardState,
  id: DbVolumeId,
  adopted: &crate::green_adopt::AdoptedGreen,
) -> Result<(), Refusal> {
  if let Some(existing) = state.db.partition().volume(id) {
    // Recorded already: by this adoption (an attempt whose answer did not come within its period), or it is not this
    // green, and the adoption would serve one object as another.
    let this_adoption = matches!(existing.policy.role, Role::Green { .. })
      && state
        .db
        .partition()
        .green_adoption(id)
        .is_some_and(|recorded| {
          recorded.version == adopted.head && recorded.identity == adopted.identity
        });
    return if this_adoption {
      Ok(())
    } else {
      Err(Refusal::AlreadyExists {
        existing: to_wire_volume(id),
      })
    };
  }
  let record = VolumeRecord {
    id,
    name: adopted.name.clone(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: db_size(SizeClass::Dynamic { max: 0 }),
      names: DbNamePolicy::Exact,
      require_locked: false,
      role: Role::Green {
        require_evidence: adopted.require_evidence,
        head_version: adopted.head,
      },
      live_shipped: false,
      live_holders: None,
      // The green's write policy and trusted signers, from its adopted catalog register (A-285, A-321): a successor
      // enforces and reports them as the departed owner did.
      write: adopted.write,
      write_version: adopted.write_version,
      signers: adopted.signers.clone(),
    },
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    epoch: 0,
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: adopted.owner.clone(),
    access: adopted.access.clone(),
    created_ns: state.clock.monotonic_ns(),
    catalog_version: adopted.catalog_sequence,
    // The takeover that made this node the green's owner (AC-8.3, A-325): its promotion epoch, which the holders fenced
    // the green at, is what this node writes the green's records at after a restart forgets `placed_heads`
    // (`fleet::promotion_epoch`). Before A-333 a successor recorded none, so a restarted one wrote them at its own
    // host's epoch, every one refused, and the green's next version never committed.
    taken_over: Some(slates_db::catalog::TakeoverRecord {
      epoch: adopted.epoch,
      head_sequence: adopted.head,
      from_live_log: false,
      // A green's takeover past its tolerance is not carried here yet: the round keeps the shortfall of a volume's head
      // register only (A-332; `takeover::record_adoption`), GAPS "The many-client load test", A-333's open items.
      past_tolerance: None,
    }),
  };
  let op = Op::GreenAdopted {
    record,
    version: adopted.head,
    identity: adopted.identity,
    origin: adopted.origin,
  };
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &op, now)
    .map(|_| ())
    .map_err(|e| match refusal_of_db(&e) {
      Refusal::AlreadyExists { .. } => match state.db.partition().volume_by_name(&adopted.name) {
        Some(existing) => Refusal::AlreadyExists {
          existing: to_wire_volume(existing.id),
        },
        None => Refusal::AlreadyExists {
          existing: to_wire_volume(id),
        },
      },
      refusal => refusal,
    })
}

/// The status count of taken-over greens adopted from this node's holder ledger (A-333): the non-vacuity counter of the
/// adoption.
const GREEN_TAKEOVER_ADOPTED: &str = "merge.takeover.adopted";
/// The status count of adoptions the owner shard refused (a name taken here, another volume under the id, the partition
/// refusing the record); the control shard keeps the engine and attempts again.
const GREEN_TAKEOVER_ADOPTION_REFUSED: &str = "merge.takeover.adoption_refused";

/// The status count of a taken-over submit's completion recorded on its green's successor (A-255).
const TAKEN_OVER_COMPLETION_RECORDED: &str = "merge.takeover.completion_recorded";
/// The status count of a taken-over submit's completion left out because its client's newer completions filled the
/// client's bound ([`completion_bound`]): its resend is refused typed and runs nothing.
const TAKEN_OVER_COMPLETION_PAST_BOUND: &str = "merge.takeover.completion_past_bound";
/// The status count of a taken-over submit's completion the partition could not make durable.
const TAKEN_OVER_COMPLETION_UNRECORDED: &str = "merge.takeover.completion_unrecorded";

/// Records, on a taken-over green's partition, the completions its merge records carried (A-255; RIFL §3, "If the
/// object migrates, then the completion record must move with it"): each submit's resend after the takeover is then
/// answered its version here, the request's key the one it was accepted under, and never run again. Each a resend can
/// reach only through its work's route (a client of another node than the work's owner) is recorded under that work
/// (A-288), so the partition counts what the route still leads to, and the route's adopter retires it once nothing is
/// left to answer.
///
/// Bounded per client as any client's records are ([`completion_bound`]): a client holds at most that many unacknowledged
/// at once, so its newest that many submits are the ones it may still resend. Older ones it had acknowledged (the
/// acknowledgements stayed with the departed owner), and their resend, which a conforming client never sends, is refused
/// typed and runs nothing (the work is gone with its owner). Counted.
fn record_taken_over_completions(
  state: &mut ShardState,
  completions: &[(u64, crate::merge_service::SubmitCompletion)],
) {
  let bound = usize::try_from(completion_bound(state)).unwrap_or(usize::MAX);
  // Each client's submits: a request's sequence, the version it was answered and the work it came from.
  let mut by_client: std::collections::BTreeMap<(u64, u32), Vec<_>> =
    std::collections::BTreeMap::new();
  for (version, key) in completions {
    by_client
      .entry((key.origin, key.client))
      .or_default()
      .push((key.sequence, *version, key.work));
  }
  for ((origin, client), mut submits) in by_client {
    submits.sort_unstable();
    let past = submits.len().saturating_sub(bound);
    state.count(
      TAKEN_OVER_COMPLETION_PAST_BOUND,
      u64::try_from(past).unwrap_or(u64::MAX),
    );
    for (sequence, version, work) in submits.into_iter().skip(past) {
      let reply = record_completion_routed(
        state,
        (origin, RequestId { client, sequence }),
        ReplyBody::Submitted {
          version: Some(version),
          conflicts: Vec::new(),
          conflicts_elided: 0,
          verdict: 0,
        },
        work,
      );
      let counter = if matches!(reply, ReplyBody::Refused { .. }) {
        TAKEN_OVER_COMPLETION_UNRECORDED
      } else {
        TAKEN_OVER_COMPLETION_RECORDED
      };
      state.count(counter, 1);
    }
  }
}

/// What a takeover adopted for one volume: its head (the content), its catalog (what the volume is served
/// as) and each register's adopted sequence.
pub(crate) struct TakenOver<'a> {
  /// The adopted head register value.
  pub head: &'a crate::head::HeadValue,
  /// The adopted catalog register value (AUD-29-17).
  pub catalog: &'a crate::catalog::CatalogValue,
  /// The catalog register's adopted sequence: the successor's record continues the register from it.
  pub catalog_sequence: u64,
  /// The head register's adopted sequence.
  pub sequence: u64,
  /// The takeover itself, kept in the volume's record (AC-8.3, A-325): the epoch the head was placed under, `sequence`,
  /// and whether the volume is rebuilt from its live log.
  pub takeover: slates_db::catalog::TakeoverRecord,
}

/// Counter: a stale copy this node relinquished to a takeover that named it again (A-362, [`relinquish_superseded`]).
/// Format: a counter name in the daemon's status report.
const TAKEOVER_RELINQUISHED: &str = "fleet.takeover.relinquished";
/// Counter: a relinquished copy's slot torn down by the reaper's slices and its credits given back (A-362).
/// Format: a counter name in the daemon's status report.
const TAKEOVER_RELINQUISHED_FREED: &str = "fleet.takeover.relinquished_freed";
/// Counter: a relinquish whose shard image could not be published without the stale copy (A-362): the materialization
/// is tried again next period, and finds the volume gone, so it makes it from the adopted state.
/// Format: a counter name in the daemon's status report.
const TAKEOVER_RELINQUISH_UNPUBLISHED: &str = "fleet.takeover.relinquish_unpublished";

/// Relinquishes this node's stale copy of volume `id` to a takeover that names this node again (A-362; §4.8 "Promotion
/// and takeover"). The copy was lost to a successor ([`lost_to_a_successor`]: its log fenced at a later epoch, A-338, or
/// yielded at this host's re-admission, A-334), so the state the takeover adopted, which the successor's acknowledged
/// writes passed, replaces it. Its attachments end; its record goes with no tombstone and no key erased
/// (`Op::VolumeRelinquished`); its guest devices, streams and snapshot views end; its slot leaves the tables for the
/// reaper to tear down a slice at a time ([`ShardState::relinquished`], [`step_destroys`]); its shipper and the fence it
/// kept are forgotten, and it is yielded no more; and the shard's image is published without it before the adopted state
/// is made under its id. So a restart in between rebuilds nothing stale: the old image with no record is torn down as an
/// orphan, and the new record with no image is answered not recovered, typed. Counted [`TAKEOVER_RELINQUISHED`]. Until
/// 2026-11-18 the takeover found the id present and kept the stale copy, served nowhere at the new epoch (A-334's owed
/// re-adoption; the chaos nemesis, seed 6876545201569229162, after A-349).
fn relinquish_superseded(state: &mut ShardState, id: DbVolumeId) -> Result<(), Box<ReplyBody>> {
  let attachments: Vec<AttachmentRecord> = state
    .db
    .partition()
    .attachments_of(id)
    .into_iter()
    .cloned()
    .collect();
  for attachment in &attachments {
    end_attachment(state, attachment, Ending::Otherwise)
      .map_err(|e| Box::new(refused(refusal_of_db(&e))))?;
  }
  let origin_pin = origin_pin_of(state, id);
  let now = state.clock.monotonic_ns();
  state
    .db
    .mutate(&mut state.segment, &Op::VolumeRelinquished { id }, now)
    .map_err(|e| Box::new(refused(refusal_of_db(&e))))?;
  release_origin_pin(state, origin_pin);
  #[cfg(unix)]
  let _devices = crate::virtiofs::revoke_volume_devices(state, id);
  let _streams = crate::shards::end_volume_streams(state, id);
  crate::snapshot_view::end_all_of(state, id);
  if let Some(handle) = state.by_id.remove(&id)
    && let Ok(mut slot) = state.volumes.remove(handle)
  {
    if let Err(e) = slot.volume.destroy(&mut state.store) {
      state.count(refusal_name(&refusal_of_vfs(&e)), 1);
    }
    state.relinquished.push(slot);
  }
  crate::live_ship::forget(state, id);
  state.yielded.remove(&id);
  state.unrecovered.remove(&id);
  state.count(TAKEOVER_RELINQUISHED, 1);
  match publish_shard(state) {
    Ok(_) => Ok(()),
    Err(e) => {
      state.count(TAKEOVER_RELINQUISH_UNPUBLISHED, 1);
      Err(Box::new(refused(refusal_of_vfs(&e))))
    }
  }
}

/// Materializes a taken-over volume on this node (§4.8 "Promotion and takeover": the new owner "adopts
/// the newest records, and serves"; §4.10 clone-from-archive): from the adopted head's catalog essentials
/// and the archive of the content it names, creates the volume **under its original id and name**, restores
/// every directory, file (bytes, mode, times) and symlink from the archive, and seals the tree as the
/// volume's head snapshot at the adopted `sequence` — with the manifest identity the head names and the
/// content holders (`region`) the head recorded, so `status` and `await placed(region)` answer for it as
/// they did on the dead owner. Idempotent: a volume already present under the id is served as it is. The
/// admission (byte reservation, inode allowance) is the same a `create` makes, so a successor short of
/// capacity refuses `BudgetExceeded` rather than over-committing; a mount name already taken locally is
/// `AlreadyExists` (names are per host, §4.4); a malformed archive is a `BadRequest` naming the reader's
/// refusal. Any refusal leaves nothing behind (the credits go back, the partial volume is discarded).
pub(crate) fn materialize_taken_over(
  state: &mut ShardState,
  id: DbVolumeId,
  taken: &TakenOver<'_>,
  region: Vec<u64>,
  archive: Option<&slates_archive::Archive>,
) -> Result<(), Box<ReplyBody>> {
  if made_already(state, id, taken.catalog)? {
    return Ok(());
  }
  let restored = restore_taken_over(state, taken.catalog, archive)?;
  make_restored(state, id, taken, region, restored.as_ref())
}

/// [`materialize_taken_over`] from a restore made in an earlier turn of this shard ([`restore_taken_over`]): a live
/// takeover's materialization in turns (g251, `fleet::make_live_in_turns`), whose restore and whose making are turns
/// of their own.
pub(crate) fn materialize_taken_over_restored(
  state: &mut ShardState,
  id: DbVolumeId,
  taken: &TakenOver<'_>,
  region: Vec<u64>,
  restored: Option<&slates_archive::Restored>,
) -> Result<(), Box<ReplyBody>> {
  if made_already(state, id, taken.catalog)? {
    return Ok(());
  }
  make_restored(state, id, taken, region, restored)
}

/// Whether the taken-over volume `id` is made here already, refused when its name is taken here by another: a copy this
/// takeover made already (a retry after its publication was refused) is kept; this node's own copy lost to a successor
/// whose takeover now names this node again is relinquished, to be replaced by the state the takeover adopted (A-362).
pub(crate) fn made_already(
  state: &mut ShardState,
  id: DbVolumeId,
  catalog: &crate::catalog::CatalogValue,
) -> Result<bool, Box<ReplyBody>> {
  if state.by_id.contains_key(&id) {
    if !lost_to_a_successor(state, id) {
      return Ok(true);
    }
    relinquish_superseded(state, id)?;
  }
  if let Some(existing) = state.db.partition().volume_by_name(&catalog.name) {
    return Err(Box::new(refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    })));
  }
  Ok(false)
}

/// The size class a taken-over volume's catalog names.
fn taken_over_size(catalog: &crate::catalog::CatalogValue) -> SizeClass {
  match catalog.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  }
}

/// A taken-over volume's archive restored for its making ([`make_restored`]), admitted before it allocates (AUD-29-13):
/// no more than the volume may hold — its bound or its dynamic maximum — and no more than this shard could admit, since
/// every restored byte is written into the volume next. An archive needing more is refused typed before a byte is
/// reconstructed; a malformed one is a `BadRequest` naming the reader's refusal. A contentless volume (a scratch volume
/// never sealed, taken over) has no archive: it restores nothing and serves its empty tree from its adopted head and
/// catalog registers.
pub(crate) fn restore_taken_over(
  state: &mut ShardState,
  catalog: &crate::catalog::CatalogValue,
  archive: Option<&slates_archive::Archive>,
) -> Result<Option<slates_archive::Restored>, Box<ReplyBody>> {
  let volume_cap = match taken_over_size(catalog) {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  };
  let Some(archive) = archive else {
    return Ok(None);
  };
  // A shard that claims its arena lazily makes room for what the restore needs first (A-98); an archive that does not
  // plan is refused by the restore below, as before.
  if let Ok(needed) = slates_archive::restore_needed(archive) {
    state.store.make_room(needed.min(volume_cap));
  }
  let restore_budget = volume_cap.min(state.store.budget.admittable());
  // A refusal names what the shard could admit now, the pool's free extents included: the arena claims nothing
  // for a restore the pool cannot cover (A-98), so the budget alone names only what the shard already holds.
  let room = volume_cap.min(state.store.admittable());
  slates_archive::restore(archive, restore_budget)
    .map(Some)
    .map_err(|e| {
      Box::new(match e {
        slates_archive::ArchiveError::OverBudget { .. } => {
          refused(Refusal::BudgetExceeded { available: room })
        }
        other => refused(Refusal::BadRequest {
          reason: format!("taken-over content archive: {other}"),
        }),
      })
    })
}

/// A taken-over volume made from its restored tree ([`restore_taken_over`]): the admission (byte reservation, inode
/// allowance) a `create` makes, the volume created under its original id and name and populated, and the tree sealed as
/// its head snapshot at the adopted sequence ([`materialize_taken_over`]'s doc).
fn make_restored(
  state: &mut ShardState,
  id: DbVolumeId,
  taken: &TakenOver<'_>,
  region: Vec<u64>,
  restored: Option<&slates_archive::Restored>,
) -> Result<(), Box<ReplyBody>> {
  let (head, catalog, sequence) = (taken.head, taken.catalog, taken.sequence);
  let size = taken_over_size(catalog);
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return Err(Box::new(refused(Refusal::BudgetExceeded { available })));
      }
      Err(e) => {
        return Err(Box::new(refused(refusal_of_mem(&e))));
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  // A bounded claim reserves its whole inode allowance; a dynamic volume holds slots for the inodes it is restored with
  // (taken below, once they are in) and grows its hold as it makes more (A-342).
  let reserved = match size {
    SizeClass::Bounded { .. } => Some(state.store.versions.reserve(inode_allowance(state, size))),
    SizeClass::Dynamic { .. } => None,
  };
  let version_credit = match reserved {
    None => None,
    Some(Ok(c)) => Some(c),
    Some(Err(slates_mem::MemError::BudgetExceeded { available, .. })) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BudgetExceeded { available },
      )));
    }
    Some(Err(e)) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        None,
        None,
        refusal_of_mem(&e),
      )));
    }
  };
  let names = wire_names(catalog.names);
  let config = volume_config(state, names, quota_for(size));
  // The taken-over volume's records are reserved against the metadata ledger before it exists (§4.2).
  let metadata_credit = match reserve_metadata(state, config.journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        None,
        refusal,
      )));
    }
  };
  // A volume that must live in locked RAM keeps that guarantee on its successor (§4.2, AUD-29-17): its entitlement
  // is reserved against this process's lock capacity as a strict create reserves it, and a successor that cannot
  // promise it refuses the takeover typed rather than serve the volume swappable.
  let lock_credit = match catalog
    .require_locked
    .then(|| reserve_locked(state, size))
    .transpose()
  {
    Ok(credit) => credit,
    Err(refusal) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal,
      )));
    }
  };
  let mut volume = match Volume::create(&mut state.store, config) {
    Ok(v) => v,
    Err(e) => {
      return Err(Box::new(give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal_of_vfs(&e),
      )));
    }
  };
  volume.set_locked(catalog.require_locked);
  if let Err(e) = admit_dimensions(state, &mut volume, size) {
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  if let Some(restored) = restored
    && let Err(e) = populate_restored(&mut state.store, &mut volume, restored)
  {
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  // A dynamic volume's hold for the inodes it was restored with (A-342), whole or refused with nothing taken.
  if matches!(size, SizeClass::Dynamic { .. })
    && let Err(e) = volume.hold_versions(&mut state.store)
  {
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  let now = state.clock.monotonic_ns();
  let record = VolumeRecord {
    id,
    name: catalog.name.clone(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: catalog.size,
      names: catalog.names,
      require_locked: catalog.require_locked,
      role: Role::Plain,
      // A live-shipped volume stays live-shipped under its successor (A-200): its catalog carries the flag.
      live_shipped: catalog.live_shipped,
      // The holder set the dead owner's shipper recorded (A-240), kept until this node's own shipper records its own:
      // a catalog shipped meanwhile still names the holders that hold the log.
      live_holders: catalog.live_holders.clone(),
      // The write policy and trusted signers the launcher set on the departed owner (A-285, A-321): a successor
      // enforces and reports them as the owner did. Until 2026-11-18 the catalog carried neither and a volume taken
      // over merged and landed as an unset one does, its signed-merge requirement dropped.
      write: catalog.write,
      write_version: catalog.write_version,
      signers: catalog.signers.clone(),
    },
    // Placed content is always whole in RAM (an overlay's base-backed bodies are never sealed), so the
    // successor serves it with no base beneath.
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    // The seal below advances the epoch to the adopted sequence, so the head register continues from it.
    epoch: sequence.saturating_sub(1),
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    // A lease was a client's of the dead owner; clients take new ones from the successor.
    lease: None,
    owner: catalog.owner.clone(),
    access: catalog.access.clone(),
    created_ns: now,
    catalog_version: taken.catalog_sequence,
    // The takeover that made this node the owner, kept with the volume so its loss report survives a restart (A-325).
    taken_over: Some(taken.takeover),
  };
  let published = publish_created_volume(
    state,
    id,
    volume,
    None,
    (reservation, version_credit, metadata_credit, lock_credit),
    record,
  );
  if !matches!(published, ReplyBody::Created { .. }) {
    return Err(Box::new(published));
  }
  // Seal the restored tree as the taken-over head: the snapshot at the adopted sequence, carrying the
  // manifest identity the head names and the placement the head recorded, so the successor answers for it.
  let Some(&handle) = state.by_id.get(&id) else {
    return Err(Box::new(refused(Refusal::NotFound)));
  };
  let taken = match state.volumes.get_mut(handle) {
    Ok(slot) => slot.volume.snapshot(&mut state.store),
    Err(_) => return Err(Box::new(refused(Refusal::NotFound))),
  };
  let snapshot = match taken {
    Ok(snapshot) => wire_snapshot(snapshot),
    Err(e) => return Err(Box::new(refused(refusal_of_vfs(&e)))),
  };
  let ops = [
    Op::SnapshotTaken {
      record: SnapshotRecord {
        id: to_db_snapshot(snapshot),
        volume: id,
        epoch: sequence,
        identity: head.manifest,
        placed: PlacementState::Placed {
          region,
          mirror: None,
        },
        taken_ns: now,
      },
    },
    Op::VolumeHeadAdvanced {
      id,
      head: to_db_snapshot(snapshot),
      epoch: sequence,
    },
  ];
  for op in &ops {
    if let Err(e) = state.db.mutate(&mut state.segment, op, now) {
      return Err(Box::new(refused(refusal_of_db(&e))));
    }
  }
  // A round that gave the volume up waiting for its cohorts' promises, then adopted it under a later configuration, left
  // its record here (`takeover::give_up_unpromised`); the volume is served now.
  state.unrecovered.remove(&id);
  Ok(())
}

/// Recreates a restored archive's tree in a fresh volume: directories parents-first (the restore's paths
/// sort so), then each file's bytes under its mode and times, a symlink (a file under the link type bits,
/// its bytes the target) as a link. The archive's own metadata carries the permission bits, the owner, all
/// four times and the extended attributes (format minor 3, AUD-29-56); the inode numbers are this
/// volume's; the source inode number groups hard links so a takeover keeps every name of one file or IPC
/// endpoint attached to the same new inode (A-26), and its attributes are set once.
fn populate_restored(
  store: &mut slates_vfs::volume::Store,
  volume: &mut Volume,
  restored: &slates_archive::Restored,
) -> Result<(), slates_vfs::VfsError> {
  use slates_vfs::export::{kind_of_mode, permissions_of_mode};
  use slates_vfs::inode::Kind;
  validate_restored_nodes(restored)?;
  let mut inodes = std::collections::BTreeMap::new();
  let mut directories: std::collections::BTreeMap<String, Handle<slates_vfs::dir::DirNode>> =
    std::collections::BTreeMap::new();
  directories.insert(String::new(), volume.root());
  let parent_of = |directories: &std::collections::BTreeMap<_, _>, path: &str| {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    directories
      .get(parent)
      .copied()
      .map(|handle| (handle, name.to_owned()))
      .ok_or(slates_vfs::VfsError::NotFound)
  };
  for path in &restored.directories {
    let (parent, name) = parent_of(&directories, path)?;
    let mode = restored
      .metadata
      .get(path)
      .map_or(DEFAULT_DIRECTORY_MODE, |meta| {
        permissions_of_mode(meta.mode)
      });
    let handle = volume.mkdir(store, parent, &name, mode)?;
    directories.insert(path.clone(), handle);
  }
  for (path, file) in &restored.files {
    let (parent, name) = parent_of(&directories, path)?;
    let meta = restored
      .metadata
      .get(path)
      .ok_or(slates_vfs::VfsError::RecoveryIncomplete)?;
    if let Some(&no) = inodes.get(&meta.ino) {
      volume.link(store, parent, &name, no)?;
      continue;
    }
    let no = match kind_of_mode(meta.mode) {
      // A name with no content: a FIFO or socket name (A-26), or a device node with the number its metadata carries
      // (A-294).
      Some(kind @ (Kind::Fifo | Kind::Socket | Kind::BlockDevice | Kind::CharDevice)) => {
        if file.len != 0 {
          return Err(slates_vfs::error::VfsError::RecoveryIncomplete);
        }
        let node = slates_vfs::inode::SpecialNode::of(
          kind,
          slates_vfs::inode::DeviceNumber::new(meta.device_major, meta.device_minor),
        )
        .ok_or(slates_vfs::VfsError::RecoveryIncomplete)?;
        let parent_no = store.dirs.get(parent)?.inode;
        volume.mknod_no(
          store,
          parent_no,
          &name,
          permissions_of_mode(meta.mode),
          node,
        )?
      }
      Some(Kind::Symlink) => {
        let target = file
          .dense()
          .ok_or(slates_vfs::VfsError::RecoveryIncomplete)?;
        let target =
          std::str::from_utf8(&target).map_err(|_| slates_vfs::VfsError::RecoveryIncomplete)?;
        volume.symlink(store, parent, &name, target)?
      }
      Some(Kind::File) => {
        let no = volume.create_file(store, parent, &name, permissions_of_mode(meta.mode))?;
        write_sparse(store, volume, no, file)?;
        no
      }
      Some(Kind::Dir) | None => return Err(slates_vfs::VfsError::RecoveryIncomplete),
    };
    inodes.insert(meta.ino, no);
  }
  // Linking changes ctime. Restore attributes only after the complete namespace exists. A hard link's
  // names share one inode, so its attributes are set once, at its first name.
  let mut restored_inodes = std::collections::BTreeSet::new();
  for (path, meta) in &restored.metadata {
    if restored.files.contains_key(path)
      && let Some(&no) = inodes.get(&meta.ino)
      && restored_inodes.insert(no)
    {
      restore_node(store, volume, no, meta, restored.xattrs.get(path))?;
    }
  }
  // The directories' owners and times last: populating a directory moves its times, and a directory
  // is only whole once its entries are in. Deepest first, so a parent's stamp follows its children's.
  for path in restored.directories.iter().rev() {
    if let (Some(handle), Some(meta)) = (directories.get(path), restored.metadata.get(path)) {
      let no = store.dirs.get(*handle)?.inode;
      restore_node(store, volume, no, meta, restored.xattrs.get(path))?;
    }
  }
  // The root has no entry naming it; its own metadata rides the archive's head (format minor 2), so
  // the rebuilt volume's root carries the mode, owner, times and attributes the origin's did — which,
  // under the export's POSIX access control, is what lets the owner into their taken-over volume at all.
  let root = volume.root_inode(store)?;
  volume.chmod(store, root, permissions_of_mode(restored.root.mode))?;
  restore_node(store, volume, root, &restored.root, restored.xattrs.get(""))?;
  Ok(())
}

/// Writes a restored file's data pieces at their offsets and nothing else, then gives it its length — so a
/// hole stays a hole: unwritten, uncharged, and found by `SEEK_HOLE` as on the origin (AUD-29-57). Until
/// 2026-10-01 the file was written as one dense buffer of its logical length.
fn write_sparse(
  store: &mut slates_vfs::volume::Store,
  volume: &mut Volume,
  no: slates_vfs::ids::InodeNo,
  file: &slates_archive::RestoredFile,
) -> Result<(), slates_vfs::VfsError> {
  for (offset, bytes) in &file.pieces {
    volume.write(store, no, *offset, bytes)?;
  }
  let written = file.pieces.last().map_or(0, |(offset, bytes)| {
    offset.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
  });
  if file.len > written {
    volume.truncate(store, no, file.len)?;
  }
  Ok(())
}

/// Validates archived inode groups before touching the replacement volume. A repeated inode is a
/// hard link only when kind, attributes and bytes agree; a hostile archive cannot alias unlike nodes.
fn validate_restored_nodes(
  restored: &slates_archive::Restored,
) -> Result<(), slates_vfs::VfsError> {
  use slates_vfs::{VfsError, export::kind_of_mode, inode::Kind};
  let mut groups: std::collections::BTreeMap<
    u64,
    (
      &slates_archive::NodeMeta,
      &slates_archive::RestoredFile,
      u32,
    ),
  > = std::collections::BTreeMap::new();
  for (path, file) in &restored.files {
    let meta = restored
      .metadata
      .get(path)
      .ok_or(VfsError::RecoveryIncomplete)?;
    match kind_of_mode(meta.mode) {
      Some(Kind::Fifo | Kind::Socket | Kind::BlockDevice | Kind::CharDevice)
        if meta.size == 0 && file.len == 0 => {}
      Some(Kind::File | Kind::Symlink) if meta.size == file.len => {}
      _ => return Err(VfsError::RecoveryIncomplete),
    }
    let group = groups.entry(meta.ino).or_insert((meta, file, 0));
    if group.0 != meta || group.1 != file {
      return Err(VfsError::RecoveryIncomplete);
    }
    group.2 = group.2.checked_add(1).ok_or(VfsError::RecoveryIncomplete)?;
  }
  if groups.values().any(|(meta, _, count)| meta.nlink != *count) {
    return Err(VfsError::RecoveryIncomplete);
  }
  Ok(())
}

/// Gives a restored node the extended attributes, the owner and the times its archive metadata carries:
/// the attributes and the owner first, since each marks the change time, and all four times last so the
/// archived stamps win. Until 2026-09-30 a successor dropped every attribute, set the access time to the
/// modification time, kept its own birth time and clamped pre-epoch times to zero (AUD-29-56).
fn restore_node(
  store: &mut slates_vfs::volume::Store,
  volume: &mut Volume,
  no: slates_vfs::ids::InodeNo,
  meta: &slates_archive::NodeMeta,
  xattrs: Option<&std::collections::BTreeMap<Vec<u8>, Vec<u8>>>,
) -> Result<(), slates_vfs::VfsError> {
  for (name, value) in xattrs.into_iter().flatten() {
    volume.xattr_set(store, no, name, value, slates_vfs::xattr::XattrSet::Create)?;
  }
  volume.chown(store, no, meta.uid, meta.gid)?;
  volume.set_times(
    store,
    no,
    Some(meta.atime_ns),
    Some(meta.mtime_ns),
    Some(meta.ctime_ns),
    Some(meta.btime_ns),
  )
}

/// Format: POSIX `0755`, the mode a restored directory takes when the archive carries none for it.
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;

/// What a verb is refused before it runs: a mutation before the consensus group exists, a landing whose target no
/// grants page could hold (`landing::longest_grantable_target`), and a new volume whose name no listing page could hold
/// ([`longest_listable_name`]).
fn refused_before_dispatch(
  state: &ShardState,
  body: &RequestBody,
  republish: bool,
) -> Option<Refusal> {
  if republish && !state.consensus_ready {
    return Some(Refusal::ConsensusNotInitialized);
  }
  if let RequestBody::Land { target, .. } = body
    && target.len() > crate::landing::longest_grantable_target()
  {
    // Typed (A-299's `Refusal::Target`): the length a grant's listing holds, which the caller compares its own with.
    return Some(Refusal::Target {
      refusal: slates_ipc::protocol::TargetRefusal::TooLong {
        longest: u64::try_from(crate::landing::longest_grantable_target()).unwrap_or(u64::MAX),
      },
    });
  }
  if new_volume_name(body).is_some_and(|name| name.len() > longest_name()) {
    return Some(Refusal::InvalidName);
  }
  match body {
    RequestBody::Create {
      base: Some(path), ..
    } if path.len() > longest_recordable_base() => Some(Refusal::BaseUnavailable {
      path: path.clone(),
      errno: PATH_TOO_LONG_ERRNO,
    }),
    _ => None,
  }
}

/// Format: the longest path the platform resolves, in bytes: `PATH_MAX` less its terminating NUL on macOS (1,024,
/// `<sys/syslimits.h>`) and on Linux and the other Unixes (4,096, `<linux/limits.h>`), and on Windows the long-path
/// limit, 32,767 UTF-16 units of at most three UTF-8 bytes each.
#[cfg(target_os = "macos")]
const PLATFORM_PATH_BYTES: usize = 1_023;
/// Format: see the macOS arm.
#[cfg(all(unix, not(target_os = "macos")))]
const PLATFORM_PATH_BYTES: usize = 4_095;
/// Format: see the macOS arm.
#[cfg(windows)]
const PLATFORM_PATH_BYTES: usize = 32_767 * 3;

/// Format: the errno a base path past [`longest_recordable_base`] is refused with: `ENAMETOOLONG`, the host's own answer
/// to a path past its maximum (the MSVC runtime's value on Windows, `<errno.h>`).
#[cfg(unix)]
const PATH_TOO_LONG_ERRNO: i32 = rustix::io::Errno::NAMETOOLONG.raw_os_error();
/// Format: see the Unix arm.
#[cfg(windows)]
const PATH_TOO_LONG_ERRNO: i32 = 38;

/// Format: the longest reason a recovery cause records ([`RecoveryCause`]'s `reason`), in bytes. A reason is the
/// daemon's own text, never a caller's: a fixed phrase and a typed error's display (a host refusal and its errno, a
/// byte count, a table's name), a short phrase beside one refusal's display. A reason built longer is cut here as it is
/// recorded ([`recorded_reason`]), so the cause's bound does not rest on every display staying short.
const RECOVERY_REASON_BYTES: usize = 256;

/// Derived: the longest overlay base path a volume records, in bytes (§4.4 `create`, §4.8): the platform's path
/// maximum, and no longer than one reply holds whole in the refusal every verb on the volume answers when recovery
/// cannot reacquire the base (`NotRecovered { BaseUnavailable { path, reason } }` beside the longest reason), so
/// `status` names the cause whole. A longer base is refused at `create`, `BaseUnavailable` with `ENAMETOOLONG`, before
/// anything is made. A listing page shows the cause cut to fit beside the volume's name ([`listed`]); the record and
/// the refusal keep it whole.
pub fn longest_recordable_base() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let pathless = ReplyBody::Refused {
    refusal: Refusal::NotRecovered {
      cause: slates_ipc::protocol::RecoveryCause::BaseUnavailable {
        path: String::new(),
        reason: "r".repeat(RECOVERY_REASON_BYTES),
      },
    },
  };
  chunk
    .saturating_sub(slates_ipc::protocol::framed_len(&pathless))
    .min(PLATFORM_PATH_BYTES)
}

/// `reason` as a recovery cause records it: whole, as every reason the daemon builds is, or cut at a character boundary
/// to [`RECOVERY_REASON_BYTES`].
fn recorded_reason(mut reason: String) -> String {
  let mut keep = reason.len().min(RECOVERY_REASON_BYTES);
  while !reason.is_char_boundary(keep) {
    keep = keep.saturating_sub(1);
  }
  reason.truncate(keep);
  reason
}

/// The name of the volume a request would create, if it creates one.
fn new_volume_name(body: &RequestBody) -> Option<&str> {
  match body {
    RequestBody::Create { name, .. }
    | RequestBody::CreateGreen { name, .. }
    | RequestBody::CreateWork { name, .. }
    | RequestBody::Clone { name, .. }
    | RequestBody::ImageBuild { name, .. }
    | RequestBody::ImageMerge { name, .. } => Some(name),
    _ => None,
  }
}

/// Derived: the longest volume name, in bytes, whose summary one listing page holds alone (§4.4 `list`): the page's
/// room less a summary's bytes with an empty name. Every volume a client can create is one it can list; a longer name
/// is refused `InvalidName` before anything is made. A `create` request carried names up to 4,062 bytes in a reply
/// chunk of 4,096 (2026-11-08), whose summary no page held.
fn longest_listable_name() -> usize {
  let unnamed = bare_causes()
    .into_iter()
    .map(|cause| VolumeSummary {
      id: VolumeId::default(),
      name: String::new(),
      referenced_bytes: u64::MAX,
      unique_bytes: u64::MAX,
      overlay: false,
      live_shipped: false,
      not_recovered: Some(cause),
    })
    .map(|summary| encoded_len(&summary))
    .max()
    .unwrap_or(0);
  list_page_room().saturating_sub(unnamed)
}

/// Derived: the longest volume name, in bytes, every reply that names the volume holds: the lesser of what a listing
/// page leaves a summary's name ([`longest_listable_name`], A-263) and what a status leaves its name beside its other
/// fields at their widest ([`longest_statused_name`], A-267). A create refuses a longer name `InvalidName` before
/// anything is made. Read once: it is a function of the wire's shapes and the reply chunk alone.
fn longest_name() -> usize {
  static LONGEST: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
  *LONGEST.get_or_init(|| longest_listable_name().min(longest_statused_name()))
}

/// Derived: the longest name a status holds beside its other fields at their widest on any host (A-267): a reply
/// chunk less the widest status with an empty name ([`widest_unnamed_status`]). The status's two lists, its mounts and
/// its drift, are carried in the room it leaves and counted past it, so they take none here. Bounded by the listing
/// alone, a volume was created and listed under a name of 4,012 bytes whose status was refused "reply too large for
/// the bulk area" (`crates/server/tests/reply_bounds.rs`, 2026-11-18).
fn longest_statused_name() -> usize {
  reply_room(&ReplyBody::Status {
    report: Box::new(widest_unnamed_status()),
  })
}

/// Format: a status at its widest but for its name and the two lists it carries as far as its room allows (its mounts,
/// its drift): every optional field present, every number at its largest, the watcher's text at the longest a status
/// writes, the transports at their widest on any host (`crate::transports::widest_report`), the base read, and a
/// takeover reported. The literal names every field, so a field appended to the report does not compile until it is
/// given its widest here.
fn widest_unnamed_status() -> StatusReport {
  use slates_ipc::protocol::{
    BaseReport, JournalReport, LossReport, PlacedState, VolumeTakeover, WatcherState,
  };
  StatusReport {
    id: VolumeId::default(),
    name: String::new(),
    referenced_bytes: u64::MAX,
    unique_bytes: u64::MAX,
    lease_epoch: Some(u64::MAX),
    attachments: u32::MAX,
    head: SnapshotId { value: u64::MAX },
    drifted: Vec::new(),
    watcher: longest_watcher_text(),
    snapshots: u32::MAX,
    placed: PlacedState {
      region: true,
      mirror_age_ns: Some(u64::MAX),
      host_epoch: u64::MAX,
    },
    nfs_port: Some(u16::MAX),
    transports: Box::new(crate::transports::widest_report()),
    mounts: Vec::new(),
    mounts_elided: u32::MAX,
    journal: JournalReport {
      budget_bytes: u64::MAX,
      retained_bytes: u64::MAX,
      retained_records: u64::MAX,
      mean_record_bytes: u64::MAX,
      appended: u64::MAX,
      dropped: u64::MAX,
      rounds: u64::MAX,
      overruns: u64::MAX,
      largest_records: u64::MAX,
      largest_bytes: u64::MAX,
      largest_lag_ns: u64::MAX,
      longest_lag_ns: u64::MAX,
      readers: u64::MAX,
      slowest_reader_behind: u64::MAX,
      grown_bytes: u64::MAX,
      growth_refused: u64::MAX,
    },
    takeover_kept: u32::MAX,
    base: Some(BaseReport::Read {
      drifted: Vec::new(),
      unverified: Vec::new(),
      watcher: WatcherState::Unavailable,
    }),
    loss: LossReport {
      at_risk_ns: Some(u64::MAX),
      placed_sequence: Some(u64::MAX),
      live_shipped: true,
      taken_over: Some(VolumeTakeover {
        epoch: u64::MAX,
        from_live_log: true,
        head_sequence: u64::MAX,
        past_tolerance: Some(slates_ipc::protocol::PastTolerance {
          registers: Some(slates_ipc::protocol::Shortfall {
            needed: u64::MAX,
            remaining: u64::MAX,
          }),
          live_log: Some(slates_ipc::protocol::Shortfall {
            needed: u64::MAX,
            remaining: u64::MAX,
          }),
        }),
      }),
    },
    drifted_elided: u32::MAX,
  }
}

/// Format: the longest watcher text a status writes: a scratch volume's, an unreadable base's, a merge volume's, or a
/// base watcher's state by name (`drift_entries`).
fn longest_watcher_text() -> String {
  use slates_vfs::host::WatchState;
  [WATCHER_SCRATCH, WATCHER_UNAVAILABLE, WATCHER_MERGE]
    .map(str::to_owned)
    .into_iter()
    .chain(
      [
        WatchState::Live,
        WatchState::Overflowed,
        WatchState::Unavailable,
      ]
      .map(|state| format!("{state:?}")),
    )
    .max_by_key(String::len)
    .unwrap_or_default()
}

/// Format: every recovery cause with its text empty: the fixed part of the cause a listed volume may carry, which the
/// longest listable name leaves room for. A cause appended to the wire is added here too: [`shorten_cause`]'s match is
/// exhaustive, so it does not compile until the new cause's text is given its place.
fn bare_causes() -> [slates_ipc::protocol::RecoveryCause; 9] {
  use slates_ipc::protocol::RecoveryCause;
  [
    RecoveryCause::NoImage,
    RecoveryCause::BlocksUnclaimed,
    RecoveryCause::BaseMissing {
      path: String::new(),
    },
    RecoveryCause::BaseOnOwnMount {
      path: String::new(),
    },
    RecoveryCause::BaseUnavailable {
      path: String::new(),
      reason: String::new(),
    },
    RecoveryCause::OriginNotRecovered,
    RecoveryCause::ImageRefused {
      reason: String::new(),
    },
    RecoveryCause::ResourcesUnavailable {
      reason: String::new(),
    },
    RecoveryCause::TakeoverIncomplete {
      waited_on: String::new(),
    },
  ]
}

/// `summary` as a listing page carries it (§4.4 `list`): whole when it fits a page alone, else with its recovery
/// cause's text (the overlay base's path, a refusal's reason) shortened until it does. Its name, at most
/// [`longest_listable_name`], always fits beside a cause's fixed part, so only the cause's text is ever cut; the cause
/// names an unrecovered volume's trouble, and a path or a reason of any length cannot refuse the listing it is in.
fn listed(mut summary: VolumeSummary) -> VolumeSummary {
  let over = encoded_len(&summary).saturating_sub(list_page_room());
  if over > 0
    && let Some(cause) = summary.not_recovered.as_mut()
  {
    shorten_cause(cause, over);
  }
  summary
}

/// Cuts at least `over` bytes from `cause`'s text, its reason before its path, each at a character boundary.
fn shorten_cause(cause: &mut slates_ipc::protocol::RecoveryCause, mut over: usize) {
  use slates_ipc::protocol::RecoveryCause;
  let texts: Vec<&mut String> = match cause {
    RecoveryCause::BaseMissing { path } | RecoveryCause::BaseOnOwnMount { path } => vec![path],
    RecoveryCause::BaseUnavailable { path, reason } => vec![reason, path],
    RecoveryCause::ImageRefused { reason } | RecoveryCause::ResourcesUnavailable { reason } => {
      vec![reason]
    }
    RecoveryCause::TakeoverIncomplete { waited_on } => vec![waited_on],
    RecoveryCause::NoImage | RecoveryCause::BlocksUnclaimed | RecoveryCause::OriginNotRecovered => {
      Vec::new()
    }
  };
  for text in texts {
    if over == 0 {
      break;
    }
    let mut keep = text.len().saturating_sub(over);
    while !text.is_char_boundary(keep) {
      keep = keep.saturating_sub(1);
    }
    over = over.saturating_sub(text.len().saturating_sub(keep));
    text.truncate(keep);
  }
}

/// One shard's part of a page gathered from every shard (§4.3; a listing, §4.4 `list`, or a principal's grants): the
/// items it holds past the page's cursor in key order, as many as one page can hold, and whether it holds more past
/// them. No page takes more than one page's worth from any shard, so a shard's part never needs more, and a gather
/// holds at most a page a shard.
#[derive(Debug)]
pub(crate) struct PagePart<T> {
  pub(crate) items: Vec<T>,
  pub(crate) more: bool,
}

impl<T> Default for PagePart<T> {
  fn default() -> Self {
    PagePart {
      items: Vec::new(),
      more: false,
    }
  }
}

/// A shard's part of a listing page.
pub(crate) type ListPart = PagePart<VolumeSummary>;

/// The parts of a listing page gathered so far, one a shard.
pub(crate) type ListParts = Vec<ListPart>;

/// The page the shards' `parts` make, merged in `key` order: as many items as `room` holds, and whether any part, or
/// the merge, left more.
pub(crate) fn merged_page<T: Wire, K: Ord>(
  parts: Vec<PagePart<T>>,
  room: usize,
  key: impl Fn(&T) -> K,
) -> (Vec<T>, bool) {
  let more_elsewhere = parts.iter().any(|part| part.more);
  let mut items: Vec<T> = parts.into_iter().flat_map(|part| part.items).collect();
  items.sort_by_key(|item| key(item));
  let (items, more_here) = fitting(items.into_iter(), room);
  (items, more_here || more_elsewhere)
}

/// The bytes a listing page's volumes may take: one reply chunk less the page's own framing, its cursor included.
fn list_page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::Listed {
    volumes: Vec::new(),
    next: Some(VolumeId {
      bytes: [u8::MAX; 16],
    }),
  };
  chunk.saturating_sub(slates_ipc::protocol::framed_len(&empty))
}

/// The longest prefix of `items` a reply carries in `room` bytes beside the rest of it (§4.3: a reply is one bulk
/// chunk; A-267), and how many it leaves out. A list that rides in a verb's own reply is carried this way — an advance's
/// invalidated paths, a rewitness's paths, a presentation's conflicts, a refused landing's conflicts — so the reply
/// always fits, and the count says what it could not hold. Unlike a page ([`fitting`]) no item is forced in: one path
/// longer than the room is counted, never sent as a reply no chunk holds.
pub(crate) fn carried<T: Wire>(items: Vec<T>, room: usize) -> (Vec<T>, u32) {
  let total = items.len();
  let mut used = 0usize;
  let mut kept = Vec::new();
  for item in items {
    let bytes = encoded_len(&item);
    if used.saturating_add(bytes) > room {
      break;
    }
    used = used.saturating_add(bytes);
    kept.push(item);
  }
  let elided = u32::try_from(total.saturating_sub(kept.len())).unwrap_or(u32::MAX);
  (kept, elided)
}

/// The bytes a reply's carried list may take: one reply chunk less `empty`'s framing, `empty` being the reply with its
/// list empty (§4.3). A sequence's length prefix and a count are fixed-width on the wire, so what the list adds is
/// exactly its items' encodings.
pub(crate) fn reply_room(empty: &ReplyBody) -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  chunk.saturating_sub(slates_ipc::protocol::framed_len(empty))
}

/// An advance's reply (§4.16 `advance`, §4.4): the version pinned and as many of the invalidated paths as one reply
/// holds, the rest counted (A-267). The advance has taken effect, so its reply must always fit.
pub(crate) fn advanced_reply(version: u64, invalidated: Vec<String>) -> ReplyBody {
  let room = reply_room(&ReplyBody::Advanced {
    version,
    invalidated: Vec::new(),
    invalidated_elided: 0,
  });
  let (invalidated, invalidated_elided) = carried(invalidated, room);
  ReplyBody::Advanced {
    version,
    invalidated,
    invalidated_elided,
  }
}

/// The first of `items` (in the page's order) that fit `room`, and whether any are left: at least one, so a walk
/// always progresses.
pub(crate) fn fitting<T: Wire>(items: impl Iterator<Item = T>, room: usize) -> (Vec<T>, bool) {
  let mut used = 0usize;
  let mut page = Vec::new();
  let mut items = items.peekable();
  while let Some(item) = items.peek() {
    let bytes = encoded_len(item);
    if !page.is_empty() && used.saturating_add(bytes) > room {
      break;
    }
    used = used.saturating_add(bytes);
    if let Some(item) = items.next() {
      page.push(item);
    }
  }
  (page, items.peek().is_some())
}

/// This shard's part of the listing page after `after` ([`ListPart`]): its volumes the caller may read, in id order past
/// the cursor, as many as one page holds. Read from the id index a batch at a time (`Partition::volumes_after`), and a
/// summary is built only for a volume the page takes, so a page costs the volumes it passes, never the shard's table;
/// collecting, summarizing and sorting every volume made a walk of the listing quadratic in it (A-267).
fn list_part(state: &ShardState, principal: &Principal, after: Option<VolumeId>) -> ListPart {
  let room = list_page_room();
  let batch = list_batch();
  let mut items = Vec::new();
  let mut used = 0usize;
  let mut cursor = after.map(to_db_volume);
  loop {
    let records = state.db.partition().volumes_after(cursor, batch);
    let exhausted = records.len() < batch;
    for v in records {
      cursor = Some(v.id);
      if !rights_of(v, principal).read || v.state == VolumeState::Destroyed {
        continue;
      }
      let summary = listed_summary(state, v);
      let bytes = encoded_len(&summary);
      // A summary always fits a page alone (a name past `longest_listable_name` is refused at create, and a cause is
      // cut to fit), so the first is always taken.
      if !items.is_empty() && used.saturating_add(bytes) > room {
        return ListPart { items, more: true };
      }
      used = used.saturating_add(bytes);
      items.push(summary);
    }
    if exhausted {
      return ListPart { items, more: false };
    }
  }
}

/// A volume as a listing page carries it ([`listed`]): its live accounting where this shard serves it.
fn listed_summary(state: &ShardState, v: &slates_db::catalog::VolumeRecord) -> VolumeSummary {
  let live = state
    .by_id
    .get(&v.id)
    .and_then(|h| state.volumes.get(*h).ok())
    .map(|s| s.volume.accounting());
  listed(VolumeSummary {
    id: to_wire_volume(v.id),
    name: v.name.clone(),
    referenced_bytes: live.map_or(v.referenced_bytes, |a| a.referenced_bytes),
    unique_bytes: live.map_or(v.unique_bytes, |a| a.unique_bytes),
    overlay: matches!(v.base, BaseRecord::Path { .. }),
    live_shipped: v.policy.live_shipped,
    not_recovered: state.unrecovered.get(&v.id).cloned(),
  })
}

/// Derived: the volumes one read of the id index takes for a listing page: as many summaries as a page could hold at the
/// smallest a summary encodes to (an empty name, no cause), so one read fills a page of readable volumes and a page
/// that passes volumes it cannot read reads again.
fn list_batch() -> usize {
  let smallest = encoded_len(&VolumeSummary {
    id: VolumeId::default(),
    name: String::new(),
    referenced_bytes: 0,
    unique_bytes: 0,
    overlay: false,
    live_shipped: false,
    not_recovered: None,
  });
  list_page_room().checked_div(smallest).unwrap_or(1).max(1)
}

/// The listing page the shards' `parts` make: their volumes in id order, as many as one reply chunk holds, and the
/// cursor past the last when any part, or the merge, left more.
fn list_page(parts: ListParts) -> ReplyBody {
  let (volumes, more) = merged_page(parts, list_page_room(), |summary| summary.id);
  let next = more
    .then(|| volumes.last().map(|summary| summary.id))
    .flatten();
  ReplyBody::Listed { volumes, next }
}

fn acknowledge(state: &mut ShardState, client_id: u32, up_to: u32, owed: &[u32]) -> ReplyBody {
  let now = state.clock.monotonic_ns();
  // A local client acknowledges its own completions, keyed under this node's **stable cert-anchor** — the
  // same key `serve` records and looks them up under (task #22 two-id model), never the ephemeral member
  // id: pruning under a key nothing is recorded under released no record (the windows grew unbounded,
  // banned item 8) and a retry after acknowledgement met its record again instead of `DuplicateRequest`
  // (`docs/bugs/2026-09-13-acknowledge-prunes-under-the-ephemeral-id.md`; found independently five
  // times in two days — also `…-acknowledge-keyed-on-ephemeral-member-id.md`,
  // `2026-09-14-ack-keyed-under-ephemeral-member-id.md` and `2026-09-14-ack-keyed-on-ephemeral-member-id.md`).
  let origin = state.origin_anchor.0;
  let owed = match acknowledgement_owing(state, up_to, owed) {
    Ok(owed) => owed,
    Err(refusal) => {
      state.count(ACK_MALFORMED, 1);
      return refused(refusal);
    }
  };
  match state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      origin,
      client: client_id,
      up_to,
      owed,
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
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = find_store_backed(state, volume, StoreVerb::Base)?;
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

/// `read_base` (§4.4, A-267): one page of the base entry as the disk holds it now, from `offset`, as many bytes as one
/// reply holds, with the file's size and its fingerprint's stamp. The client walks the pages and refuses a file whose
/// stamp moved between them; neither a reply nor this shard holds more than a page of a base file however large.
fn read_base(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  path: &str,
  offset: u64,
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
  let room = reply_room(&ReplyBody::BaseBytes {
    bytes: Vec::new(),
    total: u64::MAX,
    stamp: u64::MAX,
  });
  match slot
    .volume
    .with_host(host)
    .read_base_range(path, offset, room)
  {
    Ok(range) => ReplyBody::BaseBytes {
      bytes: range.bytes,
      total: range.total,
      stamp: range.stamp,
    },
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// `digest` (§4.15): a clean base file's verified content digest — a read, so the read right
/// suffices; the volume core's refusals cross typed (`DigestNotClean`, `DigestUnverified`).
fn digest(
  state: &mut ShardState,
  principal: &Principal,
  volume: VolumeId,
  path: &str,
) -> ReplyBody {
  let handle = match base_of(state, principal, volume, "digest", false) {
    Ok(h) => h,
    Err(r) => return *r,
  };
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let Some(host) = slot.host.as_mut() else {
    return refused(Refusal::NotFound);
  };
  match slot.volume.with_host(host).digest(&mut state.store, path) {
    Ok(digest) => ReplyBody::Digest {
      identity: digest.identity,
      size: digest.size,
    },
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
    Ok(paths) => {
      // The rewitness has taken effect, so its reply must always fit: the paths one reply holds, the rest counted.
      let room = reply_room(&ReplyBody::Rewitnessed {
        paths: Vec::new(),
        paths_elided: 0,
      });
      let (paths, paths_elided) = carried(paths, room);
      ReplyBody::Rewitnessed {
        paths,
        paths_elided,
      }
    }
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// `Pin` (§4.4): checks the overlay and the caller's write right at once, then reads the named subtrees (the whole
/// base when none are named) into the store in a task of the volume's owner shard, a slice of the shard's step quantum
/// at a time (§4.3 "Bounded work everywhere"), and defers the reply to it, as a green's checkout does. It was one
/// synchronous call: the kernel's `Documentation/` (11,608 files) held its shard for one poll of 9,194 ms, past the
/// anchor's 1,000 ms liveness budget, and every verb on the shard waited for the whole pin
/// (`a_pin_of_a_large_base_never_holds_its_shard`). A retry while it runs joins it.
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
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a pin outside a recorded request".to_owned(),
    });
  };
  let job = PinJob {
    request,
    handle,
    volume: to_db_volume(volume),
    walk: slates_vfs::base::PinWalk::new(paths),
  };
  crate::image::defer(state, run_pin(job))
}

/// A pin between its slices.
struct PinJob {
  request: (u64, RequestId),
  handle: Handle<crate::state::VolumeSlot>,
  volume: DbVolumeId,
  walk: slates_vfs::base::PinWalk,
}

impl PinJob {
  /// One unit of the walk on the overlay's slot; `Some(pinned)` once it is whole. A volume destroyed meanwhile is a
  /// stale handle, refused `NotFound`.
  fn step(&mut self, state: &mut ShardState) -> Result<Option<usize>, Refusal> {
    let Ok(slot) = state.volumes.get_mut(self.handle) else {
      return Err(Refusal::NotFound);
    };
    let Some(host) = slot.host.as_mut() else {
      return Err(Refusal::NotFound);
    };
    slot
      .volume
      .with_host(host)
      .pin_unit(&mut state.store, &mut self.walk)
      .map_err(|e| refusal_of_vfs(&e))
  }

  /// Publishes the volume so its pinned state precedes the completion (a client told it is pinned never meets a
  /// restart that lost the pin); the reply.
  fn finish(&self, state: &mut ShardState, pinned: usize) -> Result<ReplyBody, Refusal> {
    match publish_shard(state) {
      Ok(published) if published.captured(self.volume) => Ok(ReplyBody::Pinned {
        entries: u64::try_from(pinned).unwrap_or(u64::MAX),
      }),
      Ok(_) => Err(refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete)),
      Err(error) => Err(refusal_of_vfs(&error)),
    }
  }
}

/// Walks the pin a slice at a time, then commits the completion and delivers the reply. With the shard's state
/// unreachable (its shutdown) no completion is recorded, so the client's retry pins again on the shard that succeeds
/// it, from the volume as it stands (a pin is idempotent: what is pinned stays pinned).
async fn run_pin(mut job: PinJob) {
  let outcome = loop {
    match crate::image::with(|s| crate::image::sliced(s, |s| job.step(s))) {
      Ok(Some(pinned)) => break Ok(pinned),
      Ok(None) => slates_rt::futures::yield_now().await,
      Err(refusal) => break Err(refusal),
    }
  };
  let request = job.request;
  let _ = crate::state::with_state(|s| {
    let (reply, route) = crate::image::complete(s, request, |s| {
      match outcome.and_then(|pinned| job.finish(s, pinned)) {
        Ok(reply) => reply,
        Err(refusal) => refused(refusal),
      }
    });
    crate::image::deliver(s, reply, route);
  });
}

/// Serves every ready request of every client, retries deferred replies, steps destroys and
/// expires leases; returns whether anything was done.
pub fn serve_round(state: &mut ShardState) -> bool {
  let mut any = retry_forwards(state);
  any |= retry_deferred(state);
  let handles: Vec<(u32, Handle<ClientSlot>)> = state
    .clients
    .iter()
    .filter(|(_, client)| !client.retiring)
    .map(|(handle, _)| (handle.index(), handle))
    .collect();
  // One clock read per round marks every client served in it (the reap's silence clock).
  let now = state.clock.monotonic_ns();
  for (index, handle) in handles {
    any |= serve_client(state, index, handle, now);
  }
  // The closed loop's clock (`loop_hold`; A-326): the end of a round's client I/O, the requests of its clients' loops
  // read and the replies to them written, before the round's background work, so a client's think time is read from its
  // reply, not from the round's start, and a destroy's slices, a lease's expiry or an acknowledgement never stand in for
  // a client's loop.
  if std::mem::take(&mut state.loop_io) {
    state.last_client_io_ns = state.clock.monotonic_ns();
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

/// Deferred replies first: a reply back from another shard (recorded as a completion here,
/// the client's shard) or one whose completion ring was full and may have room now. A reply written is a client's loop
/// I/O, which `serve_round` stamps for the closed loop (`ShardState::loop_io`, A-326).
fn retry_deferred(state: &mut ShardState) -> bool {
  let mut any = false;
  let deferred = std::mem::take(&mut state.deferred);
  for entry in deferred {
    let Deferred {
      client_index,
      request,
      reply,
      recorded,
      ring,
    } = entry;
    let id = RequestId::from_word(request);
    // A deferred reply is the local client's own (its shard is here), so its completion keys on this node's
    // **stable cert-anchor** — the key `serve` records and looks up under (task #22 two-id model), never the
    // ephemeral member id, or a retry of a deferred request finds no record and runs again; a cross-node
    // forward records on the owner under its authenticated origin instead.
    let origin = state.origin_anchor.0;
    let mut reply = if recorded {
      reply
    } else {
      match state
        .db
        .partition()
        .completion(origin, id.client, id.sequence)
      {
        Seen::New => record_completion(state, origin, id, reply),
        _ => reply,
      }
    };
    if send_reply(state, client_index, request, &mut reply) {
      any = true;
      state.loop_io = true;
      // The `ring.request` span (§4.14) for a reply that was deferred (its ring was full, or it came
      // back from another shard) and is now written: from the slot read to the reply written.
      if let Some(ring) = ring {
        let written_ns = state.clock.monotonic_ns();
        let label = u32::from(state.partition);
        crate::telemetry::emit(state, ring.end(label, written_ns));
      }
    } else {
      state.deferred.push(Deferred {
        client_index,
        request,
        reply,
        recorded: true,
        ring,
      });
    }
  }
  any
}

/// Whether a request passes the gates before it is served, neither of which records a completion:
/// - a request names its client in its id, and a completion is keyed by that id (§4.9): one naming another client than
///   this slot's would run under that client's id and record its reply there, where that client's own request of the
///   same id would meet it, so it is refused before any effect (`refuse_foreign_request`);
/// - a client serves nothing until it has proven its id (`crate::session_key`): a stranger that names a dead client's
///   id is refused `SessionUnproven`, and its slot is ended without retiring anything of the id's.
fn gate(state: &mut ShardState, index: u32, handle: Handle<ClientSlot>, request: u64) -> bool {
  if refuse_foreign_request(state, handle, request) {
    return false;
  }
  if session_proven(state, handle) {
    return true;
  }
  let mut reply = refused(Refusal::SessionUnproven);
  // Not recorded: the id's completion records are its owner's. A refusal the full ring cannot take is lost with the
  // slot, which ends next either way.
  let _answered = send_reply(state, index, request, &mut reply);
  crate::reap::end_unproven(state, handle);
  false
}

/// Refuses, `BadRequest`, a request whose id names another client than the slot's (`serve_client`): answered on this
/// slot's ring under the id it carried, so its sender learns why, and recorded nowhere. `true` when refused. Counted.
fn refuse_foreign_request(
  state: &mut ShardState,
  handle: Handle<ClientSlot>,
  request: u64,
) -> bool {
  let Ok(client) = state.clients.get_mut(handle) else {
    return false;
  };
  if client.client_id == RequestId::from_word(request).client {
    return false;
  }
  let refusal = refused(Refusal::BadRequest {
    reason: "a request names another client's id".to_owned(),
  });
  // A full ring loses the refusal; the request is refused either way, and the sender's wait ends at its deadline.
  let _answered = client.end.reply_message(request, &refusal);
  state.count("client.foreign_request", 1);
  true
}

/// Whether the client in `handle` has proven its id: once, the first time it is asked, by the session key its region
/// holds (`crate::session_key::proves`); every later call reads the slot's flag. A refusal is counted.
fn session_proven(state: &mut ShardState, handle: Handle<ClientSlot>) -> bool {
  let secret = state.session_secret;
  let Ok(client) = state.clients.get_mut(handle) else {
    return false;
  };
  if client.proven {
    return true;
  }
  let proven = client
    .end
    .region()
    .session_key()
    .is_ok_and(|key| crate::session_key::proves(&secret, client.client_id, &key));
  client.proven = proven;
  if !proven {
    state.count("client.session_unproven", 1);
  }
  proven
}

/// What a taken slot asks before it is served: the client is seen; a cancel clears its status pages; a heartbeat or a
/// cancel is done with; any other request is served if it passes the gates (`gate`).
fn to_serve(
  state: &mut ShardState,
  index: u32,
  handle: Handle<ClientSlot>,
  request: &Request,
  now: u64,
) -> bool {
  if let Ok(client) = state.clients.get_mut(handle) {
    client.last_seen_ns = now;
    if request.kind == SlotKind::Cancel {
      client.status_pages.clear(&mut state.store.metadata);
    }
  }
  if matches!(request.kind, SlotKind::Heartbeat | SlotKind::Cancel) {
    return false;
  }
  gate(state, index, handle, request.request)
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
    // A slot the gate ended is gone, and the next take finds it so.
    if !to_serve(state, index, handle, &request, now) {
      continue;
    }
    // The `ring.request` span (§4.14) opens the request's trace as a root at the slot read: the client
    // is the entry point, its request id the replay identity. `now` (this round's single clock read,
    // §4.7) stands for the read time — a small, conservative over-estimate within one batch, not a
    // per-request clock read on the hot path. Everything the request causes on this shard opens within
    // this context; a forward carries it to the owner.
    let ring = state.tracer.open_root(
      RequestId::from_word(request.request),
      Chokepoint::RingRequest,
      now,
    );
    state.current_span = Some(ring.context());
    let served = serve(state, handle, &request, now);
    state.current_span = None;
    state.forward_hold_ns = 0;
    match served {
      Served::Reply(mut reply) => {
        if send_reply(state, index, request.request, &mut reply) {
          // Written synchronously: the span ends at the reply written.
          let written_ns = state.clock.monotonic_ns();
          let label = u32::from(state.partition);
          crate::telemetry::emit(state, ring.end(label, written_ns));
        } else {
          // The ring was full: the open span rides the deferred reply and ends when it is written.
          state.deferred.push(Deferred {
            client_index: index,
            request: request.request,
            reply,
            recorded: true,
            ring: Some(ring),
          });
        }
      }
      // A forwarded (or scattered) request: its reply returns through `deliver`, which takes the open
      // span back out and ends it at the write.
      Served::Forwarded => remember_forwarded_ring(state, request.request, ring),
    }
  }
  any
}

/// Keeps the open `ring.request` span of a request that left this shard, until its reply comes back
/// through `deliver` (§4.14). Bounded by the clients' credit — the forwards that can be in flight — and
/// past that bound the span is shed and counted, never held unbounded (ban 8).
fn remember_forwarded_ring(
  state: &mut ShardState,
  request: u64,
  ring: slates_wire::observe::OpenSpan,
) {
  let bound = state
    .config
    .clients_per_shard
    .saturating_mul(usize::try_from(state.config.region.slots).unwrap_or(1));
  if state.forwarded_rings.len() >= bound {
    state.telemetry.record_dropped(1);
    return;
  }
  state.forwarded_rings.insert(request, ring);
}

/// Writes a reply into the client's completion ring; false when the ring is full (the reply
/// is kept and retried; never dropped).
fn send_reply(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  reply: &mut ReplyBody,
) -> bool {
  // A stream end owed for this request goes first, over the client's control socket, so the client finds it waiting
  // when it reads the reply (A-161); on Windows it is duplicated into the client's process and its value written into
  // the reply (A-179). It is taken out here whatever becomes of the reply.
  crate::shards::deliver_handoff(state, client_index, request, reply);
  let Some(generation) = state.clients.generation_at(client_index) else {
    return true;
  };
  let handle = Handle::from_raw(client_index, generation);
  let Ok(client) = state.clients.get_mut(handle) else {
    return true;
  };
  if client.retiring || client.client_id != RequestId::from_word(request).client {
    return true;
  }
  if matches!(reply, ReplyBody::DaemonStatus { .. }) {
    let capacity = slates_ipc::status::snapshot_capacity(client.end.region());
    let page = slates_ipc::status::page_capacity(client.end.region());
    let now = state.clock.monotonic_ns();
    // Replace the deferred reply itself: a full ring retries these exact page bytes,
    // including the final page whose retained snapshot has already been released.
    *reply = client
      .status_pages
      .capture(request, reply, capacity, &mut state.store.metadata)
      .and_then(|()| {
        client
          .status_pages
          .page(request, 0, page, now, &mut state.store.metadata)
      })
      .unwrap_or_else(refused);
  }
  // Room first, then the body (`DaemonEnd::reply_message`): a reply packed before a full ring refused it was written
  // over the bulk body of the oldest reply the client had not taken.
  match client.end.reply_message(request, reply) {
    Ok(()) => true,
    Err(IpcError::RingFull) => false,
    Err(_) => {
      let fallback = ReplyBody::Refused {
        refusal: Refusal::BadRequest {
          reason: "reply too large for the bulk area".to_owned(),
        },
      };
      !matches!(
        client.end.reply_message(request, &fallback),
        Err(IpcError::RingFull)
      )
    }
  }
}

/// The wire volume id of a record (for tests and the CLI).
pub fn wire_id(id: DbVolumeId) -> VolumeId {
  to_wire_volume(id)
}

/// The rights the owner holds, for the status of access lists (§4.13).
pub fn owner_rights() -> AccessEntry {
  AccessEntry {
    // A placeholder for the owner: host 0 is no node's anchor.
    principal: Principal::Uid {
      host: slates_db::HostId(0),
      uid: 0,
    },
    rights: Rights {
      read: true,
      write: true,
      admin: true,
    },
  }
}

/// The status refusal count of a client region whose idle announcement could not be written (§4.14:
/// counted, never silent). The shard then keeps polling rather than idling, since that client would never
/// ring it. A region's announcement word lies inside the mapping its creation checked, so this is a
/// health signal that should stay at zero.
const IDLE_ANNOUNCE_REFUSED: &str = "ipc.idle_announce";

/// The shard's half of the doorbell protocol (§4.7 "Wake strategy", `slates_ipc::doorbell`): announces
/// to every client that the shard is going idle, then — after the protocol's fence — re-checks the client
/// rings. `true` when the shard must keep serving instead of idling: a request (or deferred work) is
/// pending, or an announcement could not be written. A request the re-check misses was published after
/// the fence, so its client reads the announcement and rings the doorbell; before 2026-09-28 the shard's
/// last look came before any fence and such a request could wait a whole reap period for a timer
/// (`docs/bugs/2026-09-28-a-client-request-waited-for-a-timer-after-a-lost-doorbell.md`).
pub fn announce_idle(state: &mut ShardState) -> bool {
  let refused = state
    .clients
    .iter()
    .filter(|(_, c)| c.end.set_parked(true).is_err())
    .count();
  if refused > 0 {
    let count = state.refusals.entry(IDLE_ANNOUNCE_REFUSED).or_insert(0);
    *count = count.saturating_add(u64::try_from(refused).unwrap_or(u64::MAX));
  }
  let pending =
    slates_ipc::doorbell::after_announcing_idle_pending(|| crate::state::ring_ready_in(state));
  pending || refused > 0
}

/// Withdraws the idle announcement once the shard is serving again, so clients stop ringing. A region
/// that refuses the write is counted as the announcement is; its client only rings when it need not.
pub fn announce_polling(state: &mut ShardState) {
  let refused = state
    .clients
    .iter()
    .filter(|(_, c)| c.end.set_parked(false).is_err())
    .count();
  if refused > 0 {
    let count = state.refusals.entry(IDLE_ANNOUNCE_REFUSED).or_insert(0);
    *count = count.saturating_add(u64::try_from(refused).unwrap_or(u64::MAX));
  }
}

/// What recovery rebuilt on this shard (§2.6 step 2; §4.8 "replay on start").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rebuilt {
  /// Volumes given a live tree again.
  pub volumes: usize,
  /// Volumes the catalog holds that could not be rebuilt (logged with the reason; a health
  /// signal, `RECOVERY_SKIPPED`): refused, never presented empty (§4.8).
  pub skipped: usize,
  /// Local snapshots the catalog recorded that the recovery image did not carry (an image that could
  /// not be published before the crash), reconciled out of the catalog.
  pub snapshots_dropped: usize,
  /// Attachments reconciled out of the catalog (their clients attach again).
  pub attachments_dropped: usize,
  /// Snapshots the recovery image carried that the catalog never recorded — a crash between a
  /// verb's publish and its completion record — dropped from the rebuilt volume, so no
  /// unacknowledged effect is partially present (AC-2.3).
  pub snapshots_trimmed: usize,
  /// Volumes the catalog held as `Destroying` whose destroy this recovery completed: the old
  /// process's cooperative slices never ran to their `VolumeDestroyed` record, and a fresh shard has
  /// nothing of theirs to free, so the record is written here and their origins unpinned.
  pub destroys_completed: usize,
  /// Snapshot pins corrected to the catalog's live clones (the image carries the pins the old
  /// process held; a destroy completing after the last publish, or a clone's record never
  /// committing, leaves them ahead of the catalog).
  pub pins_reconciled: usize,
  /// Unlinked-but-open files whose every holder died with the process, reclaimed as their volume's references
  /// were settled (A-61).
  pub orphans_reclaimed: usize,
  /// Writable direct maps' copies the anchor held, taken back (A-197).
  pub copies_adopted: usize,
  /// FUSE writes acknowledged after the last publication, replayed from the write log (A-63).
  pub writes_replayed: usize,
  /// Claimed blocks no rebuilt volume reaches, freed by the recovery sweep (A-64).
  pub blocks_swept: usize,
  /// Host mounts of snapshots whose read-only views were rebuilt (AUD-29-76).
  pub snapshot_views: usize,
  /// Held NFSv3 connections' byte states taken back (A-231), one per connection and volume.
  pub nfs_states_restored: usize,
  /// Merge volumes rebuilt (§4.16): greens with their persisted chain replayed, works with their log replayed over
  /// their green (A-207: their acknowledged edits survive).
  pub merge_volumes: usize,
  /// Manifests held for other owners that recovery held again from the image (AUD-29-59).
  pub replicas: usize,
  /// Whether the image's held replicas could not be recovered (logged with the reason; a health
  /// signal): the hold starts empty and the healer refills it, but those acknowledgements were not kept.
  pub replicas_refused: bool,
  /// Register records this shard held for other owners, rebuilt with their fences (§4.8 persistence before reply):
  /// a warm restart keeps the member id, so every record it acknowledged must be held again.
  pub records: usize,
}

/// Rebuilds the recovered catalog's volumes into live state after a daemon start over a segment
/// with history (§2.6 boot step 2, §4.8 "Recovery", A-9): each volume the catalog holds that is not
/// destroyed is rebuilt from its recovery image in anchor-owned RAM — its identity and policy from
/// the catalog record, its content, tree, snapshots and inode-number prefix from the image
/// ([`rebuild_volume`]) — with its reservations taken again; an overlay re-opens its base from the
/// recorded path (its diverged state in an image is the owed base gate). A scratch volume whose
/// image is missing refuses `RecoveryIncomplete` (never presented empty, §4.8). Then what the
/// image did not carry is reconciled in the log so the catalog stays true ([`reconcile_lost`]):
/// a local snapshot the image lacks is destroyed and the head reset, attachments are removed. The
/// catalog is the authority on what was acknowledged, so the image is trimmed back to it too: a
/// snapshot the image carries that the catalog never recorded (a crash between a verb's publish
/// and its completion record) is dropped from the rebuilt volume, so no unacknowledged effect is
/// partially present (AC-2.3). Leases keep their terms (the wheel was rebuilt by recovery) and
/// expire on their own. The counts name what happened, never more.
///
/// It runs in `slices` (`crate::recovery_slices`): each record rebuilt is a unit, a green's held origin a directory
/// a unit and its chain an entry a unit, and the slice may end after any unit, so the shard steps through a recovery
/// of any length. The order is the one recovery always took; only where a poll ends moves.
pub(crate) async fn rebuild_recovered(
  state: &mut ShardState,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Rebuilt {
  let records: Vec<VolumeRecord> = state
    .db
    .partition()
    .volumes()
    .into_iter()
    .filter(|v| !matches!(v.state, VolumeState::Destroying | VolumeState::Destroyed))
    .cloned()
    .collect();
  // The shard's recovery images from anchor-owned RAM (§4.8), by volume id, and the replicas it held for
  // other owners (AUD-29-59). Empty when there is no content object (a degraded build), or a fresh one
  // with nothing published yet.
  let RecoveredImages {
    volumes: images,
    held,
    replies,
    calls,
    nfs_replies,
    nfs_states,
  } = recover_images(state);
  restore_nfs_answers(state, calls, nfs_replies);
  // The barrier replies the dead daemon published and never delivered, for the mounts (Linux) and streams (A-316,
  // A-339) the anchor held (A-61).
  #[cfg(unix)]
  {
    state.recovered_replies = replies
      .into_iter()
      .map(|reply| (reply.attachment, (reply.unique, reply.reply)))
      .collect();
  }
  #[cfg(windows)]
  drop(replies);
  let mut rebuilt = Rebuilt::default();
  // Every block the images name is claimed before anything else allocates in the arena (A-64): the held
  // replicas and the merge volumes below allocate fresh blocks, which must never land on one an image names.
  let claims = claim_images(state, &images);
  restore_held_image(state, &held, &mut rebuilt);
  // Plain volumes first, origins before their clones (A-64: a clone is rebuilt over its recovered origin's snapshot);
  // then greens, whose held origin is walked from a plain volume's snapshot (A-217); then works, each replayed over its
  // rebuilt green (§4.16, A-207). Every record is then reconciled the same way.
  let max_prefix = rebuild_records(
    state,
    records,
    &images,
    claims.as_ref(),
    slices,
    &mut rebuilt,
  )
  .await;
  rebuilt.blocks_swept = sweep_claims(state, claims);
  slices.unit_done().await;
  // The objects the anchor held are taken back first, so every write the replay applies is written into them by the
  // volume's write boundary: the writable direct maps' copies (A-197, Linux) and the Shards streams' DAX objects
  // (A-365), in one ascending order of their backings' handles.
  #[cfg(unix)]
  {
    rebuilt.copies_adopted = crate::shards::dax_hold::adopt_inherited(state);
    slices.unit_done().await;
  }
  // The writes acknowledged after the last publication, a FUSE mount's (Linux) and a Shards stream's (Linux, macOS;
  // A-316, A-339), back on top of the rebuilt volumes (A-63).
  #[cfg(unix)]
  {
    rebuilt.writes_replayed = replay_writes(state, slices).await;
  }
  // Hand out prefixes past every recovered one, so a new volume never collides with a recovered
  // volume's inode numbers (the prefixes came from the images, not from `next_prefix`).
  state.next_prefix = max_prefix;
  rebuilt.destroys_completed = complete_recovered_destroys(state);
  slices.unit_done().await;
  rebuilt.pins_reconciled = reconcile_clone_pins(state);
  slices.unit_done().await;
  // The recorded snapshot mounts' views, once the volumes and their pins are back (AUD-29-76).
  rebuilt.snapshot_views = crate::snapshot_view::rebuild(state);
  // The held connections' byte states, once the volumes are back and every write since the publication replayed onto
  // them (A-231): each change the successor's journal holds is one the states have not seen.
  rebuilt.nfs_states_restored = crate::nfs_writeback::restore(state, nfs_states);
  rebuilt
}

/// The held replicas' content and registers from the image (AUD-29-59; §4.8 persistence before reply), what
/// [`rebuild_recovered`] takes back before the volumes.
fn restore_held_image(
  state: &mut ShardState,
  held: &[slates_vfs::recover::HeldPiece],
  rebuilt: &mut Rebuilt,
) {
  // The held replicas' blocks too (A-64), then every claim is committed: the recovered image names them all, so none
  // is reused before this daemon's first publication commits.
  // The held image's pieces carry the content hold, the held registers and the live copies (§4.8 persistence before
  // reply; A-270): split, the content claimed and rebuilt below, the registers rebuilt with their fences once the hold
  // is.
  let held = match crate::content_holder::split_held(held) {
    Ok(held) => Some(held),
    Err(e) => {
      slates_machine::log_line!(
        "slates-server: partition {}: held image refused: {e}",
        state.partition
      );
      rebuilt.replicas_refused = true;
      None
    }
  };
  let held_content = held.as_ref().map_or(&[][..], |held| &held.content[..]);
  let held_claims = slates_cluster::content::ContentHold::claim_image(
    state.store.content.arena_mut(),
    held_content,
  );
  state.store.content.arena_mut().commit_live();
  let recovered_hold = held_claims.and_then(|claimed| {
    slates_cluster::content::ContentHold::from_claimed(
      &mut crate::content_holder::hold_space(&mut state.store),
      claimed,
    )
  });
  match recovered_hold {
    Ok(mut hold) => {
      rebuilt.replicas = hold.manifest_count();
      // The image it was rebuilt from carries every piece of it, so no change is owed to the next publication (A-376).
      hold.clear_changes();
      state.held_content = hold;
    }
    Err(e) => {
      slates_machine::log_line!(
        "slates-server: partition {}: held replicas not recovered: {e:?}",
        state.partition
      );
      rebuilt.replicas_refused = true;
    }
  }
  if let Some(held) = held {
    let local = state.fleet.host();
    rebuilt.records = crate::content_holder::restore_held_registers(state, local, held);
  }
}

/// Every recorded volume rebuilt in the order recovery takes them, a unit each, and reconciled; the prefix a new volume
/// must start from.
async fn rebuild_records(
  state: &mut ShardState,
  mut records: Vec<VolumeRecord>,
  images: &std::collections::BTreeMap<[u8; 16], VolumeImage>,
  claims: Option<&slates_vfs::recover::Claims>,
  slices: &mut crate::recovery_slices::RecoverySlices,
  rebuilt: &mut Rebuilt,
) -> u16 {
  let mut max_prefix = state.next_prefix;
  // Each record's key is computed once: computed at every comparison, with a lineage walk that listed the whole catalog
  // each time, the sort alone took 132-141 ms of a 1,001-volume recovery's CPU (2026-11-07, step 1 of the slicing plan).
  records.sort_by_cached_key(|record| {
    let pass = match record.policy.role {
      Role::Plain => 0u8,
      Role::Green { .. } => 1,
      Role::Work { .. } => 2,
    };
    (pass, lineage_depth(state, record.id))
  });
  slices.unit_done().await;
  for record in &records {
    if rebuild_record(
      state,
      record,
      images,
      claims,
      slices,
      rebuilt,
      &mut max_prefix,
    )
    .await
    {
      let (snapshots, attachments) = reconcile_lost(state, record);
      rebuilt.snapshots_dropped = rebuilt.snapshots_dropped.saturating_add(snapshots);
      rebuilt.attachments_dropped = rebuilt.attachments_dropped.saturating_add(attachments);
      // The NFSv4 opens and delegations the partition journaled hold their files again before the settle reclaims the
      // files no one holds (A-260): the image carries none of these holds.
      crate::nfs_state::hold_recovered(state, record.id);
      rebuilt.orphans_reclaimed = rebuilt
        .orphans_reclaimed
        .saturating_add(settle_references(state, record));
    }
    slices.unit_done().await;
  }
  max_prefix
}

/// Rebuilds one recorded volume by its role; false when a plain volume could not be (refused and logged, never
/// presented empty), which leaves it out of the reconciliation.
async fn rebuild_record(
  state: &mut ShardState,
  record: &VolumeRecord,
  images: &std::collections::BTreeMap<[u8; 16], VolumeImage>,
  claims: Option<&slates_vfs::recover::Claims>,
  slices: &mut crate::recovery_slices::RecoverySlices,
  rebuilt: &mut Rebuilt,
  max_prefix: &mut u16,
) -> bool {
  match record.policy.role {
    // A green its takeover adopted from this node's holder ledger (A-333) is rebuilt from that ledger at boot, which
    // no replay of this partition can do: it waits for it.
    Role::Green { .. } if await_adoption(state, record) => {
      rebuilt.merge_volumes = rebuilt.merge_volumes.saturating_add(1);
    }
    Role::Green { .. } => {
      // A restarted owner owes its holders every merge record its chain holds that they may not: the pending records
      // lived in memory, so a version appended before the restart and not yet committed at `f + 1` was never shipped
      // again (the bug record of 2026-11-07). Re-enqueued, each is placed from the current configuration; a holder that
      // already holds a version acknowledges it again at no cost.
      for version in rebuild_green_sliced(state, record, slices).await {
        crate::merge_service::enqueue_recovered_record(state, record.id, version);
        slices.unit_done().await;
      }
      rebuilt.merge_volumes = rebuilt.merge_volumes.saturating_add(1);
    }
    Role::Work { green, .. } => {
      rebuild_work(state, record, green);
      rebuilt.merge_volumes = rebuilt.merge_volumes.saturating_add(1);
    }
    Role::Plain => match rebuild_volume(state, record, images.get(&record.id.bytes), claims) {
      Ok(prefix) => {
        rebuilt.volumes = rebuilt.volumes.saturating_add(1);
        *max_prefix = (*max_prefix).max(prefix.wrapping_add(1).max(1));
        rebuilt.snapshots_trimmed = rebuilt
          .snapshots_trimmed
          .saturating_add(trim_unrecorded_snapshots(state, record.id));
      }
      Err(cause) => {
        rebuilt.skipped = rebuilt.skipped.saturating_add(1);
        slates_machine::log_line!(
          "slates-server: partition {}: volume {} not rebuilt: {cause:?}",
          state.partition,
          record.name
        );
        state.unrecovered.insert(record.id, cause);
        return false;
      }
    },
  }
  true
}

/// How many clone edges lead from `volume` back to a volume that is no clone (A-64: the order recovery rebuilds in).
/// Bounded by the catalog's volume count, so a cycle in a corrupt catalog ends.
fn lineage_depth(state: &ShardState, volume: DbVolumeId) -> usize {
  let partition = state.db.partition();
  let bound = partition.volume_count();
  let mut depth = 0;
  let mut at = volume;
  while depth < bound {
    let Some(edge) = partition.lineage(at) else {
      break;
    };
    at = edge.origin_volume;
    depth = depth.saturating_add(1);
  }
  depth
}

/// Claims every block the shard's recovery images name, before anything else allocates in the arena (A-64); `None`,
/// logged, when the images name blocks no store could hold, and then no volume is rebuilt from them.
fn claim_images(
  state: &mut ShardState,
  images: &std::collections::BTreeMap<[u8; 16], VolumeImage>,
) -> Option<slates_vfs::recover::Claims> {
  // A strict volume's blocks are locked as they are claimed (§4.2 D-12): the policy is the catalog's.
  let strict: std::collections::BTreeSet<[u8; 16]> = state
    .db
    .partition()
    .volumes()
    .iter()
    .filter(|record| record.policy.require_locked)
    .map(|record| record.id.bytes)
    .collect();
  let marked = images
    .iter()
    .map(|(key, image)| (*key, image, strict.contains(key)));
  match slates_vfs::recover::Claims::prepare_with(&mut state.store, marked) {
    Ok(claims) => Some(claims),
    Err(e) => {
      slates_machine::log_line!(
        "slates-server: partition {}: the recovery images' blocks were not claimed, no volume is rebuilt: {e}",
        state.partition
      );
      None
    }
  }
}

/// Gives back every claimed block no rebuilt volume reaches (A-64): a refused volume's, an image the catalog does not
/// hold. Deferred, since the recovered image names them, until this daemon's first publication commits. Returns the
/// blocks freed.
fn sweep_claims(state: &mut ShardState, claims: Option<slates_vfs::recover::Claims>) -> usize {
  let Some(claims) = claims else {
    return 0;
  };
  let volumes = state.volumes.iter().map(|(_, slot)| &slot.volume);
  match claims.sweep(&mut state.store, volumes) {
    Ok(freed) => freed,
    Err(e) => {
      slates_machine::log_line!(
        "slates-server: partition {}: the recovery sweep stopped: {e}",
        state.partition
      );
      0
    }
  }
}

/// Completes every destroy the catalog holds in flight (`Destroying`, §4.8): the old process marked
/// the volume and was to free its tree in cooperative slices ending in a `VolumeDestroyed` record;
/// the slices never ran, and a fresh shard holds nothing of the volume to free (it is not rebuilt),
/// so the record is written now — each a recorded operation, so replay agrees — and the volume's
/// lineage edge goes with it, which is what lets [`reconcile_clone_pins`] release its origin's pin.
/// Returns how many were completed.
fn complete_recovered_destroys(state: &mut ShardState) -> usize {
  let destroying: Vec<DbVolumeId> = state
    .db
    .partition()
    .volumes()
    .into_iter()
    .filter(|v| v.state == VolumeState::Destroying)
    .map(|v| v.id)
    .collect();
  let now = state.clock.monotonic_ns();
  let mut completed: usize = 0;
  for id in destroying {
    let name = state
      .db
      .partition()
      .volume(id)
      .map(|record| record.name.clone());
    if state
      .db
      .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now)
      .is_ok()
    {
      completed = completed.saturating_add(1);
      // A clone whose name hashes elsewhere gives its alias back there (A-236).
      if let Some(name) = name {
        release_name_alias(state, id, &name);
      }
    }
  }
  retire_local_tombstones(state);
  completed
}

/// Retires the tombstones a destroy left that no remote candidate is owed (AUD-29-43): on a laptop every one,
/// with its destroy; in a fleet none until the record plane has shipped both stages — the same rule, which
/// the record period applies again each period ([`crate::fleet::retire_done_tombstones`]).
pub(crate) fn retire_local_tombstones(state: &mut ShardState) {
  let local = state.fleet.host();
  let _ = crate::fleet::retire_done_tombstones(state, local);
}

/// Sets every rebuilt snapshot's clone pins to the catalog's live clones of it (§4.8, the catalog
/// is the authority): the image carries the pins the old process held, which are ahead of the catalog
/// when a clone's record never committed (a crash between its publish and its record) or when a
/// clone's destroy completed after the last publish (the unpin is never published), and behind it
/// when a clone's image predates... never — a clone is recorded only after its publish. A pin left
/// ahead would refuse the snapshot's destroy forever; one left behind would free a clone's shared
/// tree. An image volume pins the snapshot it was built from as a clone does (A-160), and is counted with the clones.
/// Returns how many pins were changed.
/// The pins the catalog records per (origin volume, origin snapshot): its live clones, its image volumes (A-160) and
/// its greens' held origins (A-217). What recovery brings each rebuilt snapshot's pins up to.
fn recorded_pins(state: &ShardState) -> std::collections::BTreeMap<([u8; 16], u64), u32> {
  let mut recorded: std::collections::BTreeMap<([u8; 16], u64), u32> =
    std::collections::BTreeMap::new();
  for record in state.db.partition().volumes() {
    if matches!(
      record.state,
      VolumeState::Destroying | VolumeState::Destroyed
    ) {
      continue;
    }
    if let Some(edge) = state.db.partition().lineage(record.id) {
      let count = recorded
        .entry((edge.origin_volume.bytes, edge.origin_snapshot.value))
        .or_insert(0);
      *count = count.saturating_add(1);
    }
    // An image volume pins the snapshot it was built from, as a clone does (A-160).
    if let BaseRecord::Image { volume, snapshot } = &record.base {
      let count = recorded.entry((volume.bytes, snapshot.value)).or_insert(0);
      *count = count.saturating_add(1);
    }
    // So does a green its origin's snapshot (A-217).
    if let Some(held) = state.db.partition().green_origin_ref(record.id) {
      let count = recorded
        .entry((held.volume.bytes, held.snapshot.value))
        .or_insert(0);
      *count = count.saturating_add(1);
    }
  }
  recorded
}

fn reconcile_clone_pins(state: &mut ShardState) -> usize {
  let recorded = recorded_pins(state);
  let rebuilt: Vec<(DbVolumeId, Handle<VolumeSlot>)> =
    state.by_id.iter().map(|(id, h)| (*id, *h)).collect();
  let mut changed: usize = 0;
  for (id, handle) in rebuilt {
    let Ok(slot) = state.volumes.get_mut(handle) else {
      continue;
    };
    let snapshots: Vec<slates_vfs::ids::SnapshotId> = slot.volume.snapshot_ids().collect();
    for snapshot in snapshots {
      let wanted = recorded
        .get(&(id.bytes, db_snapshot_value(snapshot)))
        .copied()
        .unwrap_or(0);
      let Ok(mut held) = slot.volume.clone_pins(snapshot) else {
        continue;
      };
      while held < wanted && slot.volume.pin(snapshot).is_ok() {
        held = held.saturating_add(1);
        changed = changed.saturating_add(1);
      }
      while held > wanted && slot.volume.unpin(snapshot).is_ok() {
        held = held.saturating_sub(1);
        changed = changed.saturating_add(1);
      }
    }
  }
  changed
}

/// Rebuilds a recovered green volume (§4.16, §4.8): a fresh merge engine with its persisted chain
/// replayed in order, so its content, versions, last-changed index and dedup set return exactly as
/// before the restart. A corrupt chain entry stops the replay there — the green recovers to its last
/// good version, logged, rather than presenting a wrong later state. The tests' form of
/// [`rebuild_green_sliced`], in one call: a recovery runs the sliced one, and since A-333 a takeover replays nothing.
#[cfg(test)]
fn rebuild_green(
  state: &mut ShardState,
  record: &VolumeRecord,
) -> Vec<crate::merge_service::ReplayedVersion> {
  let replayed = replay_green(state, record.id);
  install_green(state, record, replayed)
}

/// Rebuilds a recovered green volume in a recovery's slices (§2.6 boot step 2; §4.16, §4.8): a fresh merge engine with
/// its persisted origin and chain replayed in order, a directory of the held origin or a chain entry a unit, so its
/// content, versions, last-changed index and dedup set return exactly as before the restart; or the green fenced with
/// why its replay was refused (AUD-29-18).
async fn rebuild_green_sliced(
  state: &mut ShardState,
  record: &VolumeRecord,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> Vec<crate::merge_service::ReplayedVersion> {
  let (replayed, load_steps) = replay_green_sliced(state, record.id, slices).await;
  if load_steps > 0 {
    state.count(GREEN_ORIGIN_LOAD_STEPS, load_steps);
  }
  install_green(state, record, replayed)
}

/// Counted at a recovery for each share of a green's origin seeded into its rebuilt engine (`GreenReplay`'s load): more
/// than one for a green shows its origin was seeded in shares, a recovery's unit each, not in one poll.
/// Format: a counter name in the daemon's status report.
const GREEN_ORIGIN_LOAD_STEPS: &str = "merge.recovery.origin_load_steps";

/// Installs a replayed green and returns the versions its replay rebuilt, each owed to its holders again; or fences it
/// with why its replay was refused (AUD-29-18), owing nothing.
fn install_green(
  state: &mut ShardState,
  record: &VolumeRecord,
  replayed: Result<ReplayedGreen, crate::merge_service::GreenFence>,
) -> Vec<crate::merge_service::ReplayedVersion> {
  let (green, replayed) = match replayed {
    Ok(rebuilt) => rebuilt,
    Err(fence) => {
      fence_green(state, record, fence);
      return Vec::new();
    }
  };
  state.greens.insert(record.id, green);
  // The replay rebuilt every history in full; fold it oldest-first back to the budget and re-take the retention the
  // remaining histories hold, ahead of new claims (§4.2 recovery order). The works are rebuilt after their greens,
  // at the bases their logs restore (A-207), so no work's base is in the floor here; the fold still never passes one.
  // Before the restart a claim past the green's budget was refused (`secure_green_retention`), so the history the
  // live engine held, folded to where it had folded, was within the budget; folding oldest-first therefore stops at
  // or below that point, which no live work's base was under. A retention the budget cannot hold fences the green
  // rather than serve it over budget.
  if let Err(short) = crate::merge_service::settle_green_retention(state, record.id) {
    fence_green(
      state,
      record,
      crate::merge_service::GreenFence::Retention { short },
    );
    return Vec::new();
  }
  replayed
}

/// Replays green `id`'s durable origin and chain into a fresh engine (§4.16): every chain entry was an
/// acknowledged version, so each must decode and be accepted at exactly its own version — version `n` for
/// the `n`-th entry after the checkpoint (W3) or over the origin's version 0 — or the replay is refused with the fence
/// naming why (AUD-29-18). Until 2026-09-30 a corrupt origin installed an empty green, a corrupt entry stopped the
/// replay and installed the shorter one, and a submit's outcome was not checked at all. [`rebuild_green`]'s replay,
/// in one call, for the tests; a recovery runs [`replay_green_sliced`].
#[cfg(test)]
fn replay_green(
  state: &ShardState,
  id: DbVolumeId,
) -> Result<ReplayedGreen, crate::merge_service::GreenFence> {
  let mut replay = GreenReplay::begin(state, id)?;
  while !replay.step(state)? {}
  replay.finish(state)
}

/// Replays green `id`'s durable origin and chain into a fresh engine in a recovery's slices ([`GreenReplay`]): the slice
/// may end after any unit. With it, the shares its origin was seeded in.
async fn replay_green_sliced(
  state: &ShardState,
  id: DbVolumeId,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> (Result<ReplayedGreen, crate::merge_service::GreenFence>, u64) {
  let mut replay = match GreenReplay::begin(state, id) {
    Ok(replay) => replay,
    Err(fence) => return (Err(fence), 0),
  };
  loop {
    match replay.step(state) {
      Ok(true) => break,
      Ok(false) => slices.unit_done().await,
      Err(fence) => return (Err(fence), replay.load_steps),
    }
  }
  let load_steps = replay.load_steps;
  (replay.finish(state), load_steps)
}

/// A green's engine as its replay rebuilt it, and each version the replay rebuilt, with what its merge record needs.
type ReplayedGreen = (
  slates_merge::engine::Green,
  Vec<crate::merge_service::ReplayedVersion>,
);

/// A green's replay a unit at a time (§4.16; every chain entry must decode and be accepted at exactly its own version,
/// or the replay is refused with the fence naming why, AUD-29-18): its held origin walked a directory a step (A-217), or
/// its recorded origin bytes decoded a share a step; then its engine seeded from the origin a share a step and restored
/// at its checkpoint (W3); then its chain an entry a step. Holds no borrow of the state between steps. Until 2026-11-18 the engine
/// was built from the whole origin in one step (`Green::with_origin`, or `Green::restore` over it), which hashes and
/// copies every file: seconds of one poll over the Linux kernel's 1.67 GB, past the watchdog's verdict on a shard that
/// runs a window of CPU in one step, so a restarted owner of a kernel-sized green was ended again inside its recovery.
pub(crate) struct GreenReplay {
  /// The green replayed.
  id: DbVolumeId,
  /// Where the replay is.
  phase: GreenReplayPhase,
  /// The shares its origin was seeded in: more than one shows the seeding was sliced.
  load_steps: u64,
}

/// Where a [`GreenReplay`] is.
enum GreenReplayPhase {
  /// Walking the held origin's base snapshot.
  Origin(Box<crate::merge_service::HeldWalk>),
  /// Decoding the origin the green records as bytes (a taken-over green's), a share a step: where the decoding is.
  Decode(Box<slates_merge::origin::DecodeCursor>),
  /// Seeding the engine from the origin a share a step. Boxed: it holds the engine being seeded.
  Load(Box<slates_merge::engine::OriginLoad>),
  /// Replaying the chain: the engine so far, the version its checkpoint holds (0 with none), the index of the next
  /// entry, the versions rebuilt so far, and the head and identity the checkpoint recorded (A-257).
  Chain {
    green: Box<slates_merge::engine::Green>,
    held: u64,
    next: usize,
    replayed: Vec<crate::merge_service::ReplayedVersion>,
    head_check: Option<(u64, [u8; 32])>,
  },
}

impl GreenReplay {
  /// The replay of green `id`: its held origin's walk begun, its recorded origin's decoding begun, or, for a green with
  /// no origin, its engine built.
  fn begin(
    state: &ShardState,
    id: DbVolumeId,
  ) -> Result<GreenReplay, crate::merge_service::GreenFence> {
    let partition = state.db.partition();
    let phase = match (partition.green_origin_ref(id), partition.green_origin(id)) {
      // A held origin (A-217) is walked again from its base snapshot, rebuilt before any green, and must give the
      // origin the green was created from.
      (Some(held), _) => GreenReplayPhase::Origin(Box::new(
        crate::merge_service::HeldWalk::begin(state, held).map_err(held_origin_fence)?,
      )),
      (None, Some(_)) => {
        GreenReplayPhase::Decode(Box::new(slates_merge::origin::DecodeCursor::new()))
      }
      (None, None) => chain_phase(state, id, None)?,
    };
    Ok(GreenReplay {
      id,
      phase,
      load_steps: 0,
    })
  }

  /// One unit: a piece of the held origin's walk, a share of the recorded origin decoded, a share of the origin seeded,
  /// or one chain entry submitted at its own version. True once the chain is replayed.
  fn step(&mut self, state: &ShardState) -> Result<bool, crate::merge_service::GreenFence> {
    use crate::merge_service::GreenFence;
    let id = self.id;
    let seeded = match &mut self.phase {
      GreenReplayPhase::Origin(walk) => {
        if !walk.step(state).map_err(held_origin_fence)? {
          return Ok(false);
        }
        None
      }
      GreenReplayPhase::Decode(cursor) => {
        let bytes = state
          .db
          .partition()
          .green_origin(id)
          .ok_or(GreenFence::OriginUnavailable)?;
        let decoded = cursor
          .step(bytes, crate::merge_origin::copy_share(state))
          .map_err(|_| GreenFence::OriginCorrupt)?;
        if let Some(origin) = decoded {
          self.phase =
            GreenReplayPhase::Load(Box::new(slates_merge::engine::OriginLoad::new(origin)));
        }
        return Ok(false);
      }
      GreenReplayPhase::Load(load) => {
        self.load_steps = self.load_steps.saturating_add(1);
        match load.step(crate::merge_origin::hash_share(state)) {
          Some(seeded) => Some(seeded),
          None => return Ok(false),
        }
      }
      GreenReplayPhase::Chain {
        green,
        held,
        next,
        replayed,
        head_check,
      } => return replay_chain_entry(state, id, green, (*held, *head_check), next, replayed),
    };
    match seeded {
      // The origin is seeded: the engine is restored at its checkpoint and the chain's replay begins.
      Some(seeded) => self.phase = chain_phase(state, id, Some(seeded))?,
      // The held walk is whole: it is taken out and finished into the origin the engine is seeded from.
      None => {
        let walked = std::mem::replace(
          &mut self.phase,
          GreenReplayPhase::Decode(Box::new(slates_merge::origin::DecodeCursor::new())),
        );
        if let GreenReplayPhase::Origin(walk) = walked {
          let origin = walk.finish().map_err(held_origin_fence)?;
          self.phase =
            GreenReplayPhase::Load(Box::new(slates_merge::engine::OriginLoad::new(origin)));
        }
      }
    }
    Ok(false)
  }

  /// The replayed engine, with the shard's rejected-cache budget, and the versions it rebuilt; `OriginUnavailable` if
  /// called before the chain's replay began (a caller that stopped stepping early).
  fn finish(self, state: &ShardState) -> Result<ReplayedGreen, crate::merge_service::GreenFence> {
    match self.phase {
      GreenReplayPhase::Chain {
        mut green,
        replayed,
        held,
        ..
      } => {
        if held > 0 {
          // What the replay crossed (A-257): a restore from a checkpoint, and the entries above it based below it, each
          // of which a replay that judged entries again would have refused.
          let below = replayed
            .iter()
            .filter(|version| version.base < held)
            .count();
          // The floor is a checkpoint's, or with none the version the green's takeover adopted it at (A-333).
          let floor = if state.db.partition().green_checkpoint(self.id).is_some() {
            "checkpoint"
          } else {
            "adoption"
          };
          slates_machine::log_line!(
            "slates-server: partition {}: green {} restored at its {floor} at version {held}, applied {} chain entries above it, {below} of them based below it",
            state.partition,
            hex_of(&self.id.bytes),
            replayed.len()
          );
        }
        green.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
        Ok((*green, replayed))
      }
      GreenReplayPhase::Origin(_) | GreenReplayPhase::Decode(_) | GreenReplayPhase::Load(_) => {
        Err(crate::merge_service::GreenFence::OriginUnavailable)
      }
    }
  }
}

/// Applies green `id`'s next chain entry as its own version (A-257), keeping the version's merge record as its owner enqueued
/// it, with the head identity the green has at that version; true once no entry is left.
fn replay_chain_entry(
  state: &ShardState,
  id: DbVolumeId,
  green: &mut slates_merge::engine::Green,
  (held, head_check): (u64, Option<(u64, [u8; 32])>),
  next: &mut usize,
  replayed: &mut Vec<crate::merge_service::ReplayedVersion>,
) -> Result<bool, crate::merge_service::GreenFence> {
  use crate::merge_service::GreenFence;
  let Some(bytes) = state.db.partition().green_chain(id).get(*next) else {
    return Ok(true);
  };
  let version = u64::try_from(*next)
    .unwrap_or(u64::MAX)
    .saturating_add(held)
    .saturating_add(1);
  // Applied as the verdict resolved it (A-257): a checkpoint's position cannot refuse it. A replay that does not reach an
  // identity recorded for this version, by the entry or by the checkpoint at its head, is fenced.
  let entry =
    slates_merge::chain_entry::decode(bytes).map_err(|_| GreenFence::EntryCorrupt { version })?;
  let accepted = green.apply_recorded(&entry.increment, &entry.applied);
  if accepted != version {
    return Err(GreenFence::WrongVersion { version, accepted });
  }
  // The version's signature verdict, as its submit's was judged (A-321): provenance read back, never judged again.
  if let Some(signature) = entry.signature {
    green.record_signature(version, signature);
  }
  let identity = green.head_identity();
  let at_checkpoint = head_check.and_then(|(at, recorded)| (at == version).then_some(recorded));
  if entry
    .identity
    .or(at_checkpoint)
    .is_some_and(|recorded| recorded != identity)
  {
    return Err(GreenFence::IdentityDiverged { version });
  }
  replayed.push(crate::merge_service::ReplayedVersion {
    version,
    increment: entry.increment.id,
    base: entry.increment.base,
    evidence: entry.increment.evidence,
    identity,
    completion: entry
      .completion
      .map(crate::merge_service::SubmitCompletion::of_entry),
  });
  *next = next.saturating_add(1);
  Ok(false)
}

/// Green `id`'s engine before its chain, from the origin recovery found (`None` for a green that started from
/// scratch): a checkpointed green (W3) restored at its checkpoint, so only the increments after it are replayed;
/// any other built over the origin.
fn chain_phase(
  state: &ShardState,
  id: DbVolumeId,
  seeded: Option<slates_merge::engine::Green>,
) -> Result<GreenReplayPhase, crate::merge_service::GreenFence> {
  use crate::merge_service::GreenFence;
  // The engine seeded from the origin (the empty engine for a green with no origin), restored at its checkpoint if it
  // has one, as `Green::restore` does in one call.
  let seeded = seeded.unwrap_or_default();
  let partition = state.db.partition();
  let adopted = partition.green_adoption(id).is_some();
  let (mut green, held, head_check) = match partition.green_checkpoint(id) {
    Some((held, bytes)) => {
      let checkpoint = slates_merge::checkpoint::Checkpoint::decode(bytes)
        .ok()
        .filter(|checkpoint| checkpoint.version == held)
        .ok_or(GreenFence::CheckpointCorrupt)?;
      (
        seeded.with_checkpoint(&checkpoint),
        held,
        checkpoint.head_check,
      )
    }
    // With no checkpoint the chain begins after the green's floor: 0, or the version its takeover adopted it at, whose
    // engine the boot rebuilt from this node's ledger (A-333).
    None => (seeded, partition.green_floor(id), None),
  };
  // The identity the head had when the checkpoint was taken (A-257): compared once the replay reaches that version.
  if head_check
    .is_some_and(|(version, identity)| version == held && identity != green.head_identity())
  {
    return Err(GreenFence::IdentityDiverged { version: held });
  }
  // Version 0's merge record, as its owner enqueued it, when no checkpoint stands for it; `replay_chain_entry` keeps
  // the rest. An adopted green's version 0 is its departed owner's, placed before its takeover (A-333).
  let mut replayed = Vec::new();
  if held == 0 && !adopted {
    replayed.push(crate::merge_service::ReplayedVersion {
      version: 0,
      increment: [0u8; 32],
      base: 0,
      evidence: Vec::new(),
      identity: green.head_identity(),
      completion: None,
    });
  }
  Ok(GreenReplayPhase::Chain {
    green: Box::new(green),
    held,
    next: 0,
    replayed,
    head_check,
  })
}

/// A volume id's bytes in hexadecimal, as the CLI prints a volume id.
pub(crate) fn hex_of(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The fence a held origin's refused walk puts on its green: `OriginCorrupt` when the walk no longer gives the origin
/// the green was created from, `OriginUnavailable` when the base is not here to walk.
fn held_origin_fence(refusal: Refusal) -> crate::merge_service::GreenFence {
  match refusal {
    Refusal::ContentUnavailable => crate::merge_service::GreenFence::OriginCorrupt,
    _ => crate::merge_service::GreenFence::OriginUnavailable,
  }
}

/// Fences green `record` (AUD-29-18): no engine and no retention stay installed, every verb naming it is
/// refused typed (`dispatch`), and its durable origin and chain are left as they are — the evidence a
/// reviewed recovery works from. Counted and logged once per fencing.
fn fence_green(
  state: &mut ShardState,
  record: &VolumeRecord,
  fence: crate::merge_service::GreenFence,
) {
  state.greens.remove(&record.id);
  crate::merge_service::release_green_retention(state, record.id);
  state.fenced_greens.insert(record.id, fence);
  state.count(GREEN_FENCED, 1);
  slates_machine::log_line!(
    "slates-server: partition {}: green {} fenced, its recovered history is not what was acknowledged: {fence:?}",
    state.partition,
    record.name
  );
}

/// The status count of greens fenced at recovery (AUD-29-18). Format: a refusal name in the daemon's status
/// report, alongside the verbs' refusal kinds.
const GREEN_FENCED: &str = "merge.green_fenced";

/// The status count of verbs on a green a restarted successor holds by adoption, answered `Overloaded` while the boot
/// rebuilds its engine from this node's ledger (A-333).
const GREEN_AWAITING_ADOPTION_REFUSED: &str = "merge.awaiting_adoption_refused";
/// The status count of greens a recovery found held by adoption, left for the boot to rebuild from this node's ledger
/// (A-333).
const GREEN_AWAITING_ADOPTION: &str = "merge.recovery.awaiting_adoption";

/// Leaves recovered green `record` for the boot to re-adopt, when its takeover adopted it from this node's holder ledger
/// (A-333): its partition records the versions up to the adoption by reference, so no replay of the partition rebuilds
/// it. Its engine is rebuilt from the ledger through the adopted version, or through version 0 alone when a checkpoint
/// past the adoption stands (the checkpoint is applied onto the origin), then handed here
/// (`crate::green_adopt::readopt_at_boot`). Whether the green waits so.
fn await_adoption(state: &mut ShardState, record: &VolumeRecord) -> bool {
  let partition = state.db.partition();
  let Some(adoption) = partition.green_adoption(record.id).copied() else {
    return false;
  };
  let awaiting = if partition.green_checkpoint(record.id).is_some() {
    crate::green_adopt::AwaitingAdoption {
      through: 0,
      identity: None,
      replay: None,
    }
  } else {
    crate::green_adopt::AwaitingAdoption {
      through: adoption.version,
      identity: Some(adoption.identity),
      replay: None,
    }
  };
  state.merge.awaiting_adoption.insert(record.id, awaiting);
  state.count(GREEN_AWAITING_ADOPTION, 1);
  true
}

/// Takes the engine the boot rebuilt from this node's ledger for green `id`, awaiting its re-adoption here (A-333), and
/// begins the replay of this partition's own chain onto it: its checkpoint applied when one stands, then each chain
/// entry after the green's floor, a slice a call ([`step_readoption`]). False, and the engine dropped, when the engine
/// is not at the version and identity the adoption names, or the checkpoint does not decode: the boot fences the green.
/// A green no longer awaiting (destroyed meanwhile) takes nothing, and answers true.
pub(crate) fn begin_readoption(
  state: &mut ShardState,
  id: DbVolumeId,
  mut engine: slates_merge::engine::Green,
) -> bool {
  let Some(awaiting) = state.merge.awaiting_adoption.get(&id) else {
    return true;
  };
  let (through, identity) = (awaiting.through, awaiting.identity);
  if engine.head() != through || identity.is_some_and(|identity| engine.head_identity() != identity)
  {
    return false;
  }
  let Ok(phase) = chain_phase(state, id, Some(engine)) else {
    return false;
  };
  if let Some(awaiting) = state.merge.awaiting_adoption.get_mut(&id) {
    awaiting.replay = Some(Box::new(GreenReplay {
      id,
      phase,
      load_steps: 0,
    }));
  }
  true
}

/// What one call of a green's re-adoption replay did ([`step_readoption`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadoptionStep {
  /// A slice of the chain was applied; more remains.
  More,
  /// The chain is applied: the green is installed and served, each version after its floor owed to its holders again.
  Installed,
  /// The replay was refused (an entry that does not decode or does not reach its recorded identity), or none began:
  /// the green is fenced.
  Fenced,
}

/// Applies a slice of green `id`'s own chain onto the engine its re-adoption was handed ([`begin_readoption`], A-333);
/// once all of it is applied, installs the green as a recovery does (`install_green`: the rejected-cache budget, the
/// retention settled) and owes its holders every version after the green's floor again (A-246). A green no longer
/// awaiting (destroyed meanwhile) is `Installed`: there is nothing left to do for it.
pub(crate) fn step_readoption(state: &mut ShardState, id: DbVolumeId) -> ReadoptionStep {
  let Some(awaiting) = state.merge.awaiting_adoption.get_mut(&id) else {
    return ReadoptionStep::Installed;
  };
  let Some(mut replay) = awaiting.replay.take() else {
    fence_awaiting_adoption(state, id);
    return ReadoptionStep::Fenced;
  };
  let budget = state.config.copy_slice_ns().max(1);
  let began = state.clock.monotonic_ns();
  loop {
    match replay.step(state) {
      Ok(true) => break,
      Ok(false) if state.clock.monotonic_ns().saturating_sub(began) < budget => {}
      Ok(false) => {
        if let Some(awaiting) = state.merge.awaiting_adoption.get_mut(&id) {
          awaiting.replay = Some(replay);
        }
        return ReadoptionStep::More;
      }
      Err(fence) => {
        state.merge.awaiting_adoption.remove(&id);
        if let Some(record) = state.db.partition().volume(id).cloned() {
          fence_green(state, &record, fence);
        }
        return ReadoptionStep::Fenced;
      }
    }
  }
  state.merge.awaiting_adoption.remove(&id);
  let Some(record) = state.db.partition().volume(id).cloned() else {
    return ReadoptionStep::Installed;
  };
  let replayed = replay.finish(state);
  for version in install_green(state, &record, replayed) {
    crate::merge_service::enqueue_recovered_record(state, id, version);
  }
  // The adopted version was placed at the quorum before the restart (the takeover re-committed it); only the versions
  // after it are owed to the holders again.
  if let Some(adoption) = state.db.partition().green_adoption(id).copied()
    && state.greens.contains_key(&id)
  {
    let placed = state
      .merge
      .placed
      .entry(ObjectId(id.bytes))
      .or_insert(adoption.version);
    *placed = (*placed).max(adoption.version);
  }
  ReadoptionStep::Installed
}

/// Fences green `id`, awaiting its re-adoption here, whose boot could not re-adopt it from this node's ledger (A-333):
/// every verb on it refused typed from here, its records left as the evidence (AUD-29-18).
pub(crate) fn fence_awaiting_adoption(state: &mut ShardState, id: DbVolumeId) {
  if state.merge.awaiting_adoption.remove(&id).is_none() {
    return;
  }
  if let Some(record) = state.db.partition().volume(id).cloned() {
    fence_green(
      state,
      &record,
      crate::merge_service::GreenFence::LedgerUnavailable,
    );
  }
}

/// Rebuilds a recovered work volume from its log (§4.16, A-207; D-18): its base, what it holds of its own and its
/// journal, replayed over its rebuilt green, so every change it acknowledged before the restart is there after it.
/// Skipped when its green is gone (the work has nothing to be over). Charged for what it keeps, like any work; a log
/// that does not replay, or a budget that cannot hold the work, leaves it unbuilt and counted
/// (`WORK_REBUILD_REFUSED`), never presented reset as if its edits were kept.
fn rebuild_work(state: &mut ShardState, record: &VolumeRecord, green: DbVolumeId) {
  // A green on another partition has not recovered yet, and its engine is not this shard's: the work is replayed
  // through its owner once every partition has recovered, before any client is admitted (A-247). So is a work over a
  // green this shard holds by adoption, whose engine the boot rebuilds from the ledger first (A-333).
  if owner_of(to_wire_volume(green)) != state.partition
    || state.merge.awaiting_adoption.contains_key(&green)
  {
    let created_at = match record.policy.role {
      Role::Work { base_version, .. } => base_version,
      _ => 0,
    };
    state.work_home.unbuilt.push(crate::work_home::Unbuilt {
      work: record.id,
      green,
      created_at,
    });
    return;
  }
  let Some(engine) = state.greens.get(&green) else {
    return;
  };
  // A work comes back from its log (A-207; D-18): its base, what it holds of its own and its journal, replayed over its
  // green through the verbs' own change path. It was rebuilt as a fresh clone of the head, so its acknowledged edits
  // read back as the green's bytes and its next submit was accepted without them.
  let created_at = match record.policy.role {
    Role::Work { base_version, .. } => base_version,
    _ => engine.head(),
  };
  let entries = state.db.partition().work_log(record.id);
  let Ok((base_version, files, journal)) =
    crate::work_log::replay(Some(engine), created_at, entries)
  else {
    state.count(WORK_REBUILD_REFUSED, 1);
    return;
  };
  install_rebuilt_work(state, record.id, green, (base_version, files, journal));
}

/// Installs a recovered work (A-207): charged like any work for what it keeps; a budget that cannot hold it leaves the
/// work unbuilt and counted.
fn install_rebuilt_work(
  state: &mut ShardState,
  work: DbVolumeId,
  green: DbVolumeId,
  (base_version, files, journal): (u64, crate::work_files::WorkFiles, Vec<VolumeOp>),
) {
  let charged = crate::work_charge::footprint(&files, &journal);
  if state.store.grow(charged).is_err() {
    state.count(WORK_REBUILD_REFUSED, 1);
    return;
  }
  state.works.insert(
    work,
    crate::state::WorkState {
      green,
      base_version,
      journal,
      files,
      revision: 0,
      charged,
      entries: std::collections::BTreeMap::new(),
      verdict: None,
    },
  );
}

/// The replay of a work's log for a shard that does not hold its green (A-247): run here, over the green.
pub(crate) fn owner_replay(
  state: &ShardState,
  green: DbVolumeId,
  created_at: u64,
  entries: &[Vec<u8>],
) -> Option<(u64, crate::work_files::WorkFiles, Vec<VolumeOp>)> {
  let engine = state.greens.get(&green)?;
  crate::work_log::replay(Some(engine), created_at, entries).ok()
}

/// A work the green's owner replayed, installed on its own shard; a log that did not replay is counted (A-207).
pub(crate) fn install_replayed_work(
  state: &mut ShardState,
  unbuilt: &crate::work_home::Unbuilt,
  replayed: Option<(u64, crate::work_files::WorkFiles, Vec<VolumeOp>)>,
) {
  match replayed {
    Some(replayed) => install_rebuilt_work(state, unbuilt.work, unbuilt.green, replayed),
    None => state.count(WORK_REBUILD_REFUSED, 1),
  }
}

/// The status count of recovered works left unbuilt because their log did not replay or the shard's budget could not
/// hold them (A-207). Format: a refusal name in the daemon's status report.
const WORK_REBUILD_REFUSED: &str = "merge.work_rebuild_refused";

/// A shard's slice of the anchor content object, read by copies (§4.8): the object is sparse (backed
/// only where it is touched) and hands out no reference to its bytes (AUD-29-09), so the image is found
/// and copied out without ever viewing the whole slice.
pub(crate) struct ContentView<'a> {
  pub(crate) object: &'a slates_mem::SparseObject,
  pub(crate) start: usize,
  pub(crate) len: usize,
}

impl slates_vfs::recover::ImageRead for ContentView<'_> {
  fn image_len(&self) -> usize {
    self.len
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::VfsError> {
    content_range(self.start, self.len, offset, into.len())
      .and_then(|at| self.object.read(at, into).ok())
      .ok_or(slates_vfs::VfsError::RecoveryIncomplete)
  }
}

/// A shard's slice of the anchor content object, published by ranges (§4.8); only the frame written
/// is backed.
struct ContentSlots<'a> {
  object: &'a mut slates_mem::SparseObject,
  start: usize,
  len: usize,
}

impl slates_vfs::recover::ImageRead for ContentSlots<'_> {
  fn image_len(&self) -> usize {
    self.len
  }

  fn image_read(&self, offset: usize, into: &mut [u8]) -> Result<(), slates_vfs::VfsError> {
    content_range(self.start, self.len, offset, into.len())
      .and_then(|at| self.object.read(at, into).ok())
      .ok_or(slates_vfs::VfsError::RecoveryIncomplete)
  }
}

impl slates_vfs::recover::ImageWrite for ContentSlots<'_> {
  fn image_write(&mut self, offset: usize, from: &[u8]) -> Result<(), slates_vfs::VfsError> {
    // A span past the slice, or memory the OS would not back (a Windows commit refused), is no space.
    let at = content_range(self.start, self.len, offset, from.len())
      .ok_or(slates_vfs::VfsError::NoSpace)?;
    self
      .object
      .write(at, from)
      .map_err(|_| slates_vfs::VfsError::NoSpace)
  }
}

/// The object offset of `[offset, offset + len)` within a slice `[start, start + slice_len)`, or `None`
/// when the span leaves the slice.
fn content_range(start: usize, slice_len: usize, offset: usize, len: usize) -> Option<usize> {
  let end = offset.checked_add(len)?;
  (end <= slice_len)
    .then(|| start.checked_add(offset))
    .flatten()
}

/// The shard's journal as a restart reads it from its slice of the anchor content object (§4.8, A-68): the committed
/// checkpoint with every valid delta after it applied, and the journal positioned after them; `None` without a content
/// object or a slice in it.
fn read_journal(
  state: &ShardState,
) -> Option<Result<(Option<ShardImage>, slates_vfs::checkpoint_log::Journal), slates_vfs::VfsError>>
{
  let (start, end) = state.content_range;
  let (delta_start, delta_end) = state.delta_range;
  let object = state.content.as_ref()?;
  if end <= start || end > object.len() {
    return None;
  }
  let checkpoints = ContentView {
    object,
    start,
    len: end.saturating_sub(start),
  };
  // The delta log beside the checkpoints (A-68); an absent one (an object laid out without it) is empty.
  let log = ContentView {
    object,
    start: delta_start,
    len: if delta_end > delta_start && delta_end <= object.len() {
      delta_end.saturating_sub(delta_start)
    } else {
      0
    },
  };
  Some(slates_vfs::checkpoint_log::Journal::recover(
    &checkpoints,
    &log,
  ))
}

/// Test support (A-270's oracle): whether the held pieces a restart would read from this shard's committed journal are
/// exactly the ones it holds now ([`crate::content_holder::held_image`]), each delta's pieces folded over its
/// checkpoint. Read after a publication commits, a missed change mark is a difference here before it is a lost record.
pub(crate) fn committed_held_agrees(state: &ShardState) -> bool {
  let committed = match read_journal(state) {
    Some(Ok((Some(shard), _))) => shard.held,
    Some(Ok((None, _))) | None => Vec::new(),
    Some(Err(_)) => return false,
  };
  let mut held = crate::content_holder::held_image(state);
  held.sort_by_key(|piece| piece.key);
  committed == held
}

/// The shard's recovery images from its slice of the anchor content object (§4.8), by volume id, and the
/// held replicas' image the same shard image carries (AUD-29-59). A torn or malformed image logs and yields
/// nothing for that shard (each volume then refuses as unrecoverable rather than presenting empty),
/// matching §4.8's "never an empty success".
fn recover_images(state: &mut ShardState) -> RecoveredImages {
  let Some(recovered) = read_journal(state) else {
    return RecoveredImages::default();
  };
  match recovered {
    Ok((Some(shard), journal)) => {
      state.journal = journal;
      state.published_keys = shard.volumes.iter().map(|keyed| keyed.key).collect();
      state.published_held = crate::content_holder::published_from(&shard.held);
      RecoveredImages {
        volumes: shard
          .volumes
          .into_iter()
          .map(|keyed| (keyed.key, keyed.image))
          .collect(),
        held: shard.held,
        replies: shard.replies,
        calls: shard.calls,
        nfs_replies: shard.nfs_replies,
        nfs_states: shard.nfs_states,
      }
    }
    Ok((None, _)) => RecoveredImages::default(),
    Err(e) => {
      slates_machine::log_line!(
        "slates-server: partition {}: shard image unreadable: {e}",
        state.partition
      );
      RecoveredImages::default()
    }
  }
}

/// What a shard's last committed image held: its volumes by id, the replicas it held for other owners
/// (AUD-29-59), the barrier replies it had not delivered (A-61), and its call records (A-127).
#[derive(Default)]
struct RecoveredImages {
  volumes: std::collections::BTreeMap<[u8; 16], VolumeImage>,
  held: Vec<slates_vfs::recover::HeldPiece>,
  replies: Vec<slates_vfs::recover::HeldReply>,
  calls: Vec<slates_vfs::recover::HeldCall>,
  nfs_replies: Vec<slates_vfs::recover::HeldNfsReply>,
  nfs_states: Vec<slates_vfs::recover::HeldNfsStates>,
}

/// What a shard publish committed (§4.8): the volumes the new image carries, and the ones it could
/// not image. A mutation of an omitted volume must refuse its stability guarantee.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Published {
  /// Volumes whose content the committed image actually carries. An absent volume is never
  /// inferred captured from an empty omission list (AUD-05).
  pub volumes: Vec<DbVolumeId>,
  /// Volumes skipped because they could not be imaged (an overlay with base-backed inodes, whose base
  /// recovery is its own gate); every other volume of the shard is in the committed image or being
  /// destroyed.
  pub skipped: Vec<DbVolumeId>,
  /// Volumes left out because they are being destroyed: their catalog record says so and recovery
  /// completes a recorded destroy without an image (`complete_recovered_destroys`), so none is owed.
  pub destroying: Vec<DbVolumeId>,
  /// The committed frame's bytes (the image plus its slot header), what the slot now holds.
  pub frame_bytes: usize,
}

impl Published {
  /// Whether the committed image carries `volume`: the barrier's question for the volume a
  /// data-plane mutation touched.
  pub fn captured(&self, volume: DbVolumeId) -> bool {
    self.volumes.contains(&volume)
  }

  /// Whether `volume` was left out because it is being destroyed (nothing of it is owed to recovery).
  pub fn destroying(&self, volume: DbVolumeId) -> bool {
    self.destroying.contains(&volume)
  }
}

/// Publishes the shard's volumes as one recovery image into its slice of the anchor content object
/// (§4.8), so a restart recovers their content from anchor-owned RAM. This is the **barrier** every
/// acknowledgement of a mutation stands behind (D-18): a control verb publishes before its completion
/// record commits ([`dispatch`]), and a data-plane mutation through the mount transport publishes
/// before its reply claims stability (`crate::nfs`). `Ok` names what the committed image carries;
/// `Err` is a refused publish — the image did not fit its slot (`NoSpace`) or the slot could not be
/// written — after which nothing changed since the last committed image survives a restart, so the
/// caller must not acknowledge stability. Missing recovery storage refuses `RecoveryIncomplete`.
/// Efficiency gate: it re-images
/// every volume on each call; an incremental publish is the owed refinement (docs/wip/recovery.md).
pub fn publish_shard(state: &mut ShardState) -> Result<Published, slates_vfs::VfsError> {
  let (start, end) = state.content_range;
  if state.content.is_none() || end <= start {
    return Err(slates_vfs::VfsError::RecoveryIncomplete);
  }
  // The blocks this publication may name, recorded before any is imaged: until it commits or is abandoned, a free
  // of one is deferred, so the committed image never names a reused block (A-64). Settled here on every outcome.
  state.store.content.arena_mut().capture();
  let outcome = publish_captured(state);
  match outcome {
    Ok(_) => {
      // Every volume the publication recorded has its ceiling raised (`Volume::mark_published`, A-150); a volume still
      // short sets the flag again at its next number.
      state.store.counters_low = false;
      state.store.content.arena_mut().commit_capture();
      crate::live_ship::ship_captured(state);
      // The committed image names no block of a wholly free extent, and no deferred free holds one: it goes back to
      // the pool for any shard to claim (A-98).
      state.store.release_idle();
    }
    Err(_) => {
      state.store.content.arena_mut().abandon_capture();
      // Nothing of a refused publication ships; the volumes stay dirty and publish again.
      state.live.captured.clear();
    }
  }
  outcome
}

/// The body of [`publish_shard`] once its capture is taken: a delta of what changed since the last publication,
/// appended to the shard's journal (A-68), or — at the first publication, once the deltas since the last checkpoint
/// reach its size, or when the delta does not fit the log — a checkpoint of every volume. A barrier's cost is its
/// changes, not the shard: re-imaging every inode of every volume at each barrier made a workload's total grow with
/// its square (2026-10-04: 2,000 creates took 4.9 s into an empty volume and 50.2 s into one of 12,000 files).
fn publish_captured(state: &mut ShardState) -> Result<Published, slates_vfs::VfsError> {
  if !state.journal.wants_checkpoint() {
    match publish_delta(state) {
      Ok(published) => return Ok(published),
      // The log has no room for this delta: a checkpoint takes its place (and restarts the log).
      Err(slates_vfs::VfsError::NoSpace) => {}
      Err(e) => return Err(publish_refused(state, e)),
    }
  }
  publish_checkpoint(state)
}

/// Counts and logs a refused publication, returning its refusal.
fn publish_refused(state: &ShardState, e: slates_vfs::VfsError) -> slates_vfs::VfsError {
  crate::daemon::PUBLISH_REFUSED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
  slates_machine::log_line!(
    "slates-server: partition {}: shard image not published: {e}",
    state.partition
  );
  e
}

/// Counts a volume a publication could not record, logging the first on this shard.
fn count_skipped(state: &mut ShardState, id: DbVolumeId, e: &slates_vfs::VfsError) {
  // Counted every time on this shard and logged once per shard, naming the volume: the publish runs inside every
  // mutating verb, so a line per volume per publish was an unbounded log on a verb's latency path.
  let skipped = state
    .refusals
    .entry(crate::daemon::PUBLISH_VOLUME_SKIPPED)
    .or_insert(0);
  *skipped = skipped.saturating_add(1);
  if *skipped == 1 {
    let volume: String = id.bytes.iter().map(|b| format!("{b:02x}")).collect();
    slates_machine::log_line!(
      "slates-server: partition {}: volume {volume} was not imaged, skipped: {e} (first on this shard; later \
       ones are counted as {})",
      state.partition,
      crate::daemon::PUBLISH_VOLUME_SKIPPED
    );
  }
}

/// Whether a volume's slot is being destroyed: it carries nothing to recover, and its destroy slices release its
/// tree as they go — imaging it walked released nodes (ESTALE), counted a false skip and printed a line on every
/// publish while a destroy ran, 31,378 in one provisioning histogram
/// (docs/bugs/2026-09-29-a-destroy-on-a-shard-without-a-client-never-completed.md).
fn being_destroyed(volume: &slates_vfs::volume::Volume) -> bool {
  matches!(
    volume.state(),
    slates_vfs::volume::VolumeState::Destroying | slates_vfs::volume::VolumeState::Destroyed
  )
}

/// Takes back what answers the NFS requests a dead daemon applied and left unanswered: the call records it published
/// and committed, which answer its NFSv4.1 clients' retries (A-127), and the NFSv3 replies it published for its held
/// connections and never sent, which answer their requests read again (A-128). Counted.
fn restore_nfs_answers(
  state: &mut ShardState,
  calls: Vec<slates_vfs::recover::HeldCall>,
  nfs_replies: Vec<slates_vfs::recover::HeldNfsReply>,
) {
  crate::nfs_replay::restore(state, calls);
  state.recovered_nfs_replies = nfs_replies
    .into_iter()
    .map(|reply| {
      (
        (reply.connection, reply.xid),
        (reply.accepted, reply.results),
      )
    })
    .collect();
  let restored = state.recovered_nfs_replies.len();
  if restored > 0 {
    state.count(
      crate::nfs::HELD_REPLY_RESTORED,
      u64::try_from(restored).unwrap_or(u64::MAX),
    );
  }
}

/// The call records a delta carries (A-127): the durable ones changed since the last publication, and the slots purged
/// since.
fn call_records_delta(
  state: &ShardState,
) -> (
  Vec<slates_vfs::recover::HeldCall>,
  Vec<[u8; slates_vfs::recover::HELD_CALL_KEY_BYTES]>,
) {
  crate::nfs_replay::delta(state)
}

/// The call records a checkpoint carries: every durable one (A-127).
fn call_records_checkpoint(state: &ShardState) -> Vec<slates_vfs::recover::HeldCall> {
  crate::nfs_replay::checkpoint(state)
}

/// A publication carrying the call records and the held NFSv3 replies committed: nothing of them is owed to the next
/// one (A-127, A-128).
fn call_records_published(state: &mut ShardState) {
  state.nfs_calls.published();
  state.held_nfs_replies_changed = false;
}

/// The held NFSv3 replies a publication carries (A-128): every reply held for a held connection's turn in progress.
fn held_nfs_replies(state: &ShardState) -> Vec<slates_vfs::recover::HeldNfsReply> {
  state
    .held_nfs_replies
    .iter()
    .map(
      |(&(connection, xid), (accepted, results))| slates_vfs::recover::HeldNfsReply {
        connection,
        xid,
        accepted: *accepted,
        results: results.clone(),
      },
    )
    .collect()
}

/// Whether the held NFSv3 replies changed since the last publication, which a delta then carries whole (A-128).
fn held_nfs_replies_changed(state: &ShardState) -> bool {
  state.held_nfs_replies_changed
}

/// The barrier replies not yet delivered, which ride with the effects they answer (A-61).
fn pending_replies(state: &ShardState) -> Vec<slates_vfs::recover::HeldReply> {
  #[cfg(unix)]
  return state.pending_replies.values().cloned().collect();
  #[cfg(windows)]
  {
    let _ = state;
    Vec::new()
  }
}

/// A checkpoint of every volume into the free slot (§4.8): the whole shard, as every publication was before A-68.
fn publish_checkpoint(state: &mut ShardState) -> Result<Published, slates_vfs::VfsError> {
  // In key order, the order a shard image holds its volumes in (`ShardImage::new` sorts by key).
  let mut keyed: Vec<_> = state
    .volumes
    .iter()
    .map(|(handle, slot)| (slot.id.bytes, handle))
    .collect();
  keyed.sort_by_key(|(key, _)| *key);
  let handles: Vec<_> = keyed.iter().map(|(_, handle)| *handle).collect();
  capture_live_changes(state, &handles)?;
  let (_, end) = state.content_range;
  match state.content.as_ref() {
    None => return Err(slates_vfs::VfsError::RecoveryIncomplete),
    Some(object) if end > object.len() => return Err(slates_vfs::VfsError::NoSpace),
    Some(_) => {}
  }
  // The replicas this shard holds for other owners ride the same image (AUD-29-59): a holder acknowledges a
  // content put only once a publish carrying it commits.
  let mut held = crate::content_holder::held_image(state);
  let whole = crate::content_holder::whole_of(&held);
  let owned = crate::content_holder::owned_of(&held);
  let mut replies = pending_replies(state);
  let mut calls = call_records_checkpoint(state);
  let mut nfs_replies = held_nfs_replies(state);
  // The held connections' byte states (A-231), each tracker consumed through now first.
  let mut nfs_states = crate::nfs_writeback::carry(state);
  // A volume that cannot be captured restarts the checkpoint without it: the image's bytes before it may already be
  // in the slot, and the slot commits only when its header is written last, so the abandoned stream commits nothing.
  // Each restart leaves out one more volume, so there are at most as many restarts as volumes.
  let mut skipped: Vec<(DbVolumeId, slates_vfs::VfsError)> = Vec::new();
  let outcome = loop {
    match stream_checkpoint(
      state,
      &handles,
      &skipped,
      &mut held,
      (&mut replies, &mut calls, &mut nfs_replies, &mut nfs_states),
    ) {
      Ok(Streamed::Uncaptured(id, e)) => skipped.push((id, e)),
      Ok(Streamed::Committed(published)) => break Ok(published),
      Ok(Streamed::Refused(e)) => break Err(publish_refused(state, e)),
      Err(e) => break Err(e),
    }
  };
  // Publish unaffected volumes even when another cannot be captured. Callers must check the touched volume against
  // the returned coverage; an omitted volume never receives a stable acknowledgement, and recovery refuses it instead
  // of rebuilding empty (§4.8, AUD-05).
  for (id, e) in &skipped {
    count_skipped(state, *id, e);
  }
  let mut published = outcome?;
  published.skipped = skipped.into_iter().map(|(id, _)| id).collect();
  // Membership by set, not a scan: this sweep marks every hosted volume published or not, and a linear `contains`
  // over the committed list per volume made it O(V²) in the volumes a shard holds. Measured 2026-10-12
  // (`checkpoint_cost_scales_with_the_hosted_volume_count`, BENCHMARKS.md §4.8): an 8,192-volume shard's checkpoint
  // spent about 9 of its 12 ms in this scan, a term that grows with the square of the volume count. Built once as an
  // ordered set over the volumes' own key type, the sweep is O(V log V).
  let committed: std::collections::BTreeSet<DbVolumeId> =
    published.volumes.iter().copied().collect();
  for handle in handles {
    if let Ok(slot) = state.volumes.get_mut(handle) {
      if committed.contains(&slot.id) {
        slot.volume.mark_published(&state.store);
      } else {
        slot.volume.mark_unpublished();
      }
    }
  }
  state.published_keys = published.volumes.iter().map(|id| id.bytes).collect();
  crate::content_holder::held_committed(state, whole, true);
  crate::live_ship::owned_committed(state, owned, true);
  call_records_published(state);
  // Every volume captured: this image holds every logged write, so the write log empties, stamped with this
  // generation (A-63). A volume skipped keeps the log, so its writes stay replayable.
  #[cfg(unix)]
  clear_write_log(
    state,
    published.skipped.is_empty(),
    state.journal.generation(),
  );
  Ok(published)
}

/// How one streamed checkpoint attempt ended.
enum Streamed {
  /// The checkpoint committed, carrying these volumes.
  Committed(Published),
  /// This volume could not be captured; nothing committed.
  Uncaptured(DbVolumeId, slates_vfs::VfsError),
  /// The slot refused the image (it does not fit); nothing committed.
  Refused(slates_vfs::VfsError),
}

/// One attempt at [`publish_checkpoint`]: the shard image streamed straight into the content object's free slot, each
/// volume encoded as it is captured, leaving out the volumes being destroyed and those in `skipped`. The checkpoint was
/// first held whole (every one left its own size in the daemon's footprint: 270 MB after 50,000 macOS files whose
/// volume needs 47, 2026-10-06 `sizes_probe`), then its encoding was (a buffer the size of the image kept per shard,
/// 399 bytes a file, `create_heap`); now only the stage, [`slates_vfs::recover::STREAM_STAGE_BYTES`] and one inode.
fn stream_checkpoint(
  state: &mut ShardState,
  handles: &[Handle<VolumeSlot>],
  skipped: &[(DbVolumeId, slates_vfs::VfsError)],
  held: &mut Vec<slates_vfs::recover::HeldPiece>,
  (replies, calls, nfs_replies, nfs_states): (
    &mut Vec<slates_vfs::recover::HeldReply>,
    &mut Vec<slates_vfs::recover::HeldCall>,
    &mut Vec<slates_vfs::recover::HeldNfsReply>,
    &mut Vec<slates_vfs::recover::HeldNfsStates>,
  ),
) -> Result<Streamed, slates_vfs::VfsError> {
  let left_out = |slot: &VolumeSlot| {
    being_destroyed(&slot.volume) || skipped.iter().any(|(id, _)| *id == slot.id)
  };
  let mut count = 0usize;
  for handle in handles {
    if !left_out(state.volumes.get(*handle)?) {
      count = count.saturating_add(1);
    }
  }
  let (start, end) = state.content_range;
  let Some(object) = state.content.as_mut() else {
    return Err(slates_vfs::VfsError::RecoveryIncomplete);
  };
  let mut slots = ContentSlots {
    object,
    start,
    len: end.saturating_sub(start),
  };
  let mut stream = match state
    .journal
    .begin_checkpoint(&mut slots, &mut state.checkpoint_buffer)
  {
    Ok(stream) => stream,
    Err(e) => return Ok(Streamed::Refused(e)),
  };
  ShardImage::encode_start(stream.buf(), count);
  let mut published = Published::default();
  for handle in handles {
    let slot = state.volumes.get_mut(*handle)?;
    if being_destroyed(&slot.volume) {
      published.destroying.push(slot.id);
      continue;
    }
    if left_out(slot) {
      continue;
    }
    let id = slot.id;
    stream.buf().extend_from_slice(&id.bytes);
    if let Err(e) = slot.volume.encode_image_into(
      &state.store,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      &mut stream,
    ) {
      return Ok(match stream.refused() {
        Some(refused) => Streamed::Refused(refused.clone()),
        None => Streamed::Uncaptured(id, e),
      });
    }
    published.volumes.push(id);
  }
  ShardImage::encode_finish(
    stream.buf(),
    held,
    (replies, calls, nfs_replies, nfs_states),
  );
  match state.journal.finish_checkpoint(stream) {
    Ok(frame_bytes) => published.frame_bytes = frame_bytes,
    Err(e) => return Ok(Streamed::Refused(e)),
  }
  // What a live volume changed ships once the checkpoint commits, for the volumes it carried (A-200,
  // `crate::live_ship`).
  crate::live_ship::note_checkpoint(state, &published.volumes);
  Ok(Streamed::Committed(published))
}

/// Takes each changed live-shipped volume's record before a checkpoint, as a delta publication takes it: a checkpoint is
/// this shard's recovery journal restarting, which says nothing about what a live log needs, and it is due when the
/// deltas reach the size of the shard's metadata, while a whole record carries every byte of the volume's content. Mapped
/// to whole records, one write in three copied the volume (68 of 201 records, 2026-11-05, `live_fleet`). An unchanged
/// live volume ships nothing. A record that cannot be taken is `None`: the volume ships whole.
fn capture_live_changes(
  state: &mut ShardState,
  handles: &[Handle<VolumeSlot>],
) -> Result<(), slates_vfs::VfsError> {
  for handle in handles {
    let slot = state.volumes.get_mut(*handle)?;
    let id = slot.id;
    let live = state
      .db
      .partition()
      .volume(id)
      .is_some_and(|volume| volume.policy.live_shipped);
    if !live || being_destroyed(&slot.volume) || slot.volume.is_clean(&state.store) {
      continue;
    }
    let mut record = Vec::new();
    let encoded = slot.volume.encode_publication(
      &state.store,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      &mut record,
    );
    state
      .live
      .captured
      .push((id, encoded.ok().map(|()| record)));
  }
  Ok(())
}

/// A delta of what changed since the last publication, appended to the shard's delta log (A-68): each changed
/// volume's record (a delta, or its full image when it has none), the volumes gone, the held replicas when they
/// changed, and the undelivered replies. Refuses `NoSpace` when the log cannot take it, changing nothing.
fn publish_delta(state: &mut ShardState) -> Result<Published, slates_vfs::VfsError> {
  // The delta is streamed into the shard's publish buffer, idle between publications: each changed volume's record is
  // encoded from the store as it is taken, never built as a value (an image of every changed inode and a name per
  // changed entry: nine allocations a create, 2026-10-06 `create_heap`).
  let mut encoded = std::mem::take(&mut state.checkpoint_buffer);
  encoded.clear();
  let outcome = publish_delta_into(state, &mut encoded);
  state.checkpoint_buffer = encoded;
  outcome
}

/// [`publish_delta`] into `encoded`.
fn publish_delta_into(
  state: &mut ShardState,
  encoded: &mut Vec<u8>,
) -> Result<Published, slates_vfs::VfsError> {
  let mut recorded = Vec::new();
  let mut present = std::collections::BTreeSet::new();
  let mut published = Published::default();
  // In key order, the order a shard delta holds its volumes in (`ShardDelta::new` sorts by key).
  let mut keyed: Vec<_> = state
    .volumes
    .iter()
    .map(|(handle, slot)| (slot.id.bytes, handle))
    .collect();
  keyed.sort_by_key(|(key, _)| *key);
  let count_at = slates_vfs::checkpoint_log::ShardDelta::encode_start(encoded);
  let mut count = 0usize;
  for (_, handle) in &keyed {
    let slot = state.volumes.get_mut(*handle)?;
    let id = slot.id;
    if being_destroyed(&slot.volume) {
      published.destroying.push(id);
      continue;
    }
    if slot.volume.is_clean(&state.store) {
      present.insert(id.bytes);
      published.volumes.push(id);
      continue;
    }
    let mark = encoded.len();
    encoded.extend_from_slice(&id.bytes);
    match slot.volume.encode_publication(
      &state.store,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      encoded,
    ) {
      Ok(()) => {
        present.insert(id.bytes);
        published.volumes.push(id);
        recorded.push(*handle);
        count = count.saturating_add(1);
        // A live-shipped volume's record ships once the publication commits (A-200, `crate::live_ship`).
        if state
          .db
          .partition()
          .volume(id)
          .is_some_and(|volume| volume.policy.live_shipped)
        {
          let record = encoded
            .get(mark.saturating_add(id.bytes.len())..)
            .map(<[u8]>::to_vec);
          state.live.captured.push((id, record));
        }
      }
      Err(e) => {
        encoded.truncate(mark);
        count_skipped(state, id, &e);
        published.skipped.push(id);
        // A skipped volume keeps its committed record, so it stays present.
        if state.published_keys.contains(&id.bytes) {
          present.insert(id.bytes);
        }
      }
    }
  }
  let mut removed: Vec<[u8; 16]> = state
    .published_keys
    .iter()
    .filter(|key| !present.contains(*key))
    .copied()
    .collect();
  let mut held = crate::content_holder::held_changes(state);
  let (mut calls, mut calls_removed) = call_records_delta(state);
  let mut nfs_replies = held_nfs_replies_changed(state).then(|| held_nfs_replies(state));
  // The held connections' byte states (A-231), whole in every delta: a READ changes them without a publication of its
  // own, so a delta carries them as of its own moment.
  let mut nfs_states = crate::nfs_writeback::carry(state);
  slates_vfs::checkpoint_log::ShardDelta::encode_finish(
    encoded,
    count_at,
    count,
    &mut removed,
    (&mut held.put, &mut held.removed),
    Some(&pending_replies(state)),
    (&mut calls, &mut calls_removed),
    (nfs_replies.as_deref_mut(), Some(&mut nfs_states[..])),
  )?;
  let (start, end) = state.delta_range;
  let Some(object) = state.content.as_mut() else {
    return Err(slates_vfs::VfsError::RecoveryIncomplete);
  };
  if end <= start || end > object.len() {
    return Err(slates_vfs::VfsError::NoSpace);
  }
  let mut log = ContentSlots {
    object,
    start,
    len: end.saturating_sub(start),
  };
  published.frame_bytes = state.journal.append_encoded(&mut log, encoded)?;
  for handle in recorded {
    if let Ok(slot) = state.volumes.get_mut(handle) {
      slot.volume.mark_published(&state.store);
    }
  }
  state.published_keys = present;
  let carried = held
    .put
    .iter()
    .map(|piece| piece.bytes.len())
    .fold(0usize, usize::saturating_add);
  if carried > 0 {
    state.count(DELTA_HELD_BYTES, u64::try_from(carried).unwrap_or(u64::MAX));
  }
  crate::content_holder::held_committed(state, held.whole, false);
  crate::live_ship::owned_committed(state, held.owned, false);
  call_records_published(state);
  #[cfg(unix)]
  clear_write_log(
    state,
    published.skipped.is_empty(),
    state.journal.generation(),
  );
  Ok(published)
}

/// Format: the status counter of the held-image bytes the shard's committed deltas carried (§4.8 persistence before
/// reply): what a holder's acknowledgement costs its publication, so a test can hold it to the change made.
pub(crate) const DELTA_HELD_BYTES: &str = "publish.delta_held_bytes";

/// Format: the status counter of publications run to release blocks whose frees waited on one (A-64).
pub(crate) const DEFERRED_RELIEVED: &str = "arena.deferred_relieved";

/// Format: the status counter of content-pool claims refused (A-98): an extent whose owner word could not be reached,
/// or whose range the OS would not map, or one that could not be given back. Counted by the pool, never lost.
pub(crate) const CONTENT_POOL_REFUSED: &str = "content.pool_refused";

/// Format: the status counter of chunks sealed in the arena (A-99): idle content encrypted at rest.
pub(crate) const CONTENT_SEALED: &str = "content.sealed";

/// Format: the status counter of chunk seals that fell back to the clear because the cipher refused (A-99).
pub(crate) const CONTENT_SEAL_REFUSED: &str = "content.seal_refused";

/// Format: the status counter of bytes seals gave back by trimming chunks' blocks to their lengths (A-118).
pub(crate) const CONTENT_TRIMMED: &str = "content.trimmed_bytes";

/// Format: the status counter of bytes trims would have given back but the arena declined (an imaged or locked block).
pub(crate) const CONTENT_TRIM_DECLINED: &str = "content.trim_declined_bytes";

/// Format: the status counter of trims the arena refused after the chunk was recorded (A-118).
pub(crate) const CONTENT_TRIM_REFUSED: &str = "content.trim_refused";

/// Format: the status counter of frees refused on a seal's paths (a block or a tag run not given back, A-99).
pub(crate) const CONTENT_FREE_REFUSED: &str = "content.free_refused";

/// Format: the status counter of retired objects whose release the store refused (listed twice, or a free refused).
pub(crate) const STORE_RELEASE_REFUSED: &str = "store.release_refused";

/// Format: the status counter of nested state borrows on this shard (a step that never ran, `state::with_state_counted`).
pub(crate) const STATE_BORROW_REFUSED: &str = "state.borrow_refused";

/// Format: the status counter of retention checks refused after a counted borrow ran (`state::with_state_counted`).
pub(crate) const STATE_RETENTION_REFUSED: &str = "state.retention_refused";

/// Publishes when the shard's arena is short of room only because freed blocks wait on a publication (A-64): a
/// block the committed recovery image may name is not reused until a newer image commits. Run before each unit of
/// work — a transport request on a volume, a verb — so an operation within the operation headroom (the derived
/// bound on in-flight copy-ups the budget already keeps free, §4.2) never meets `PublishNeeded`. Costs two loads
/// when nothing is deferred or the arena has room. A refused publication is counted where it is refused.
pub(crate) fn relieve_deferred(state: &mut ShardState) {
  // A volume's reserve of inode numbers ran low (A-150): the publication tops it up before this unit of work hands
  // one out, so the unit's replies need no barrier of their own. It relieves the arena's deferred frees too.
  if state.store.counters_low {
    if publish_shard(state).is_ok() {
      state.count(COUNTERS_TOPPED_UP, 1);
    }
    return;
  }
  let arena = state.store.content.arena();
  if arena.deferred_bytes() == 0 {
    return;
  }
  let free = u64::try_from(arena.free_bytes()).unwrap_or(u64::MAX);
  if free >= state.store.budget.headroom() {
    return;
  }
  if publish_shard(state).is_ok() {
    let relieved = state.refusals.entry(DEFERRED_RELIEVED).or_insert(0);
    *relieved = relieved.saturating_add(1);
  }
}

/// Format: the status counter of publications run to top up a volume's reserve of inode numbers (A-150), before a unit
/// of work that might hand one out.
pub(crate) const COUNTERS_TOPPED_UP: &str = "publish.counters_topped_up";

/// Whether an effect a client keeps across a restart waits for volume `volume`'s next publication
/// (`Volume::awaits_publication`: an unreserved inode number, A-150; a release the committed publication still counts
/// as an open, A-154): the reply must wait for the barrier (D-18). `false` for a volume this shard does not hold. Asked
/// by the FUSE and NFS replies and by the Shards stream's (on every platform since A-179).
pub(crate) fn awaits_publication(state: &ShardState, volume: DbVolumeId) -> bool {
  state
    .by_id
    .get(&volume)
    .and_then(|handle| state.volumes.get(*handle).ok())
    .is_some_and(|slot| slot.volume.awaits_publication())
}

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

/// Re-acquires a recovered volume's version-slab credit (§4.2 accounting through recovery): its
/// logical inode allowance, admitted before the restart. (Its retention — retained versions and
/// bytes — is re-established by the rebuild itself, `Volume::from_image`.) If the slab shrank below
/// what the recovered state needs, recovery cannot represent it and fails, returning the byte
/// reservation and re-grown hold so a refused recovery leaks neither.
fn recover_version_reservations(
  store: &mut slates_vfs::volume::Store,
  allowance: u64,
  reservation: Option<slates_mem::budget::Reservation>,
  held: u64,
) -> Result<Option<slates_mem::budget::VersionCredit>, String> {
  match store.versions.reserve(allowance) {
    Ok(c) => Ok(Some(c)),
    Err(e) => {
      release_recovered_bytes(store, reservation, held);
      Err(format!(
        "recovered inode allowance exceeds the version slab: {e}"
      ))
    }
  }
}

/// Rebuilds one recovered volume into the shard's store, returning its inode-number prefix (so the
/// caller advances `next_prefix` past it). A scratch volume is rebuilt from its recovery image when
/// one is present (its content, tree, snapshots and prefix restored, §4.8); with a content object but
/// no image the volume's content was lost, which refuses (`RecoveryIncomplete`) rather than
/// presenting empty. An overlay also refuses: the image format does not retain its base witnesses
/// or host handles, and reopening its path would absorb outsider changes and lose private edits. The volume's reservations — bytes, re-grown dynamic hold, inode and entry
/// allowances, version credits — are taken again (§4.2 accounting through recovery), and every one
/// is given back if a later step refuses.
fn rebuild_volume(
  state: &mut ShardState,
  record: &VolumeRecord,
  image: Option<&VolumeImage>,
  claims: Option<&slates_vfs::recover::Claims>,
) -> Result<u16, RecoveryCause> {
  let size = wire_size(record.policy.size);
  let reservation = match size {
    SizeClass::Bounded { limit } => Some(
      state
        .store
        .budget
        .reserve(limit)
        .map_err(|e| resources(format!("the shard's byte budget: {e}")))?,
    ),
    SizeClass::Dynamic { .. } => None,
  };
  // A recovered strict volume's entitlement is reserved again in this process (§4.2 D-12); its claimed blocks were
  // locked as they were claimed (`claim_images`). One the OS would not let lock is refused, never served swappable.
  if record.policy.require_locked
    && claims.is_some_and(|claims| claims.unlockable(&record.id.bytes))
  {
    if let Some(r) = reservation {
      state.store.budget.release(r);
    }
    return Err(resources(
      "a strict volume's content could not be locked in this process".to_owned(),
    ));
  }
  let lock_credit = match record
    .policy
    .require_locked
    .then(|| reserve_locked(state, size))
    .transpose()
  {
    Ok(credit) => credit,
    Err(refusal) => {
      if let Some(r) = reservation {
        state.store.budget.release(r);
      }
      return Err(resources(format!(
        "a strict volume's entitlement exceeds this process's lock capacity: {}",
        refusal_name(&refusal)
      )));
    }
  };
  let built = build_recovered_volume(state, record, image, claims, size);
  let (mut volume, host, prefix) = match built {
    Ok(triple) => triple,
    Err(cause) => {
      if let Some(r) = reservation {
        state.store.budget.release(r);
      }
      return Err(cause);
    }
  };
  // A recovered dynamic volume's growth was committed to the shard budget before the crash; re-acquire
  // it now so the budget reflects it and later admissions account for it (§4.2 accounting through
  // recovery). It was admitted before the restart, so it fits unless the capacity itself shrank; on
  // teardown it is released through `budget_hold`, the same as a running volume's.
  let held = volume.budget_hold();
  if held > 0 && state.store.grow(held).is_err() {
    // The rebuilt volume returns its slots, blocks and retention rather than leaking them.
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    if let Some(r) = reservation {
      state.store.budget.release(r);
    }
    return Err(resources(
      "recovered dynamic growth exceeds the shard budget".to_owned(),
    ));
  }
  // Re-admit the inode dimension (§4.2): the fair share, but never below what the recovered volume
  // already holds — recovery does not refuse inodes that were admitted before the restart.
  let allowance = inode_allowance(state, size).max(volume.inode_usage().0);
  let allowed = volume.set_inode_allowance(allowance);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  let entries = entry_allowance(size).max(volume.entry_usage().0);
  let allowed = volume.set_entry_allowance(entries);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  // Re-acquire a bounded claim's version reservation (§4.2 accounting through recovery), or a dynamic volume's hold for
  // the live inodes it was rebuilt with (A-342), giving back the byte reservation and re-grown hold — and the rebuilt
  // volume's slots, blocks and retention — if the slab shrank.
  let version_credit = match size {
    SizeClass::Bounded { .. } => {
      match recover_version_reservations(&mut state.store, allowance, reservation, held) {
        Ok(credit) => credit,
        Err(reason) => {
          let discarded = volume.discard_partial(&mut state.store);
          count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
          return Err(resources(reason));
        }
      }
    }
    SizeClass::Dynamic { .. } => {
      if let Err(e) = volume.hold_versions(&mut state.store) {
        let discarded = volume.discard_partial(&mut state.store);
        count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
        release_recovered_bytes(&mut state.store, reservation, held);
        return Err(resources(format!(
          "recovered dynamic volume's inodes exceed the version slab: {e}"
        )));
      }
      None
    }
  };
  // Re-acquire the records' metadata reservation (§4.2 accounting through recovery), giving every
  // other credit and the rebuilt volume back if the ledger cannot back it.
  let journal_bytes = journal_floor_bytes(state);
  let metadata_credit = match reserve_metadata(state, journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      let hold = volume.version_hold();
      let discarded = volume.discard_partial(&mut state.store);
      count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
      if let Some(c) = version_credit {
        state.store.versions.release(c);
      }
      release_version_hold(&mut state.store, hold);
      release_recovered_bytes(&mut state.store, reservation, held);
      return Err(resources(format!(
        "recovered volume records exceed the metadata ledger: {}",
        refusal_name(&refusal)
      )));
    }
  };
  let mut volume = volume;
  volume.set_locked(record.policy.require_locked);
  crate::content_cipher::key_volume(&mut state.store, &mut volume, record.id.bytes);
  let slot = VolumeSlot {
    id: record.id,
    name: record.name.clone(),
    volume,
    host,
    reservation,
    version_credit,
    metadata_credit,
    lock_credit,
  };
  let handle = state
    .volumes
    .insert(slot)
    .map_err(|e| resources(format!("the volume table: {e}")))?;
  state.by_id.insert(record.id, handle);
  Ok(prefix)
}

/// Rebuilds an image against its catalog base (§4.8). An overlay reacquires each source directory
/// without following links and verifies the retained fingerprint before serving any source bytes.
fn build_recovered_volume(
  state: &mut ShardState,
  record: &VolumeRecord,
  image: Option<&VolumeImage>,
  claims: Option<&slates_vfs::recover::Claims>,
  size: SizeClass,
) -> Result<(Volume, Option<OsHost>, u16), RecoveryCause> {
  let image = image.ok_or(RecoveryCause::NoImage)?;
  let quota = quota_for(size);
  let journal = journal_floor_bytes(state);
  let mut opened = match &record.base {
    // An image volume holds its content itself, as a scratch one does; its base names only the snapshot it pins.
    BaseRecord::Scratch | BaseRecord::Image { .. } => None,
    BaseRecord::Path { path } => {
      Some(OsHost::open_root(std::path::Path::new(path)).map_err(|error| base_cause(path, error))?)
    }
  };
  let source = opened
    .as_mut()
    .map(|(host, root)| (host as &mut dyn HostFs, *root));
  let claims = claims.ok_or(RecoveryCause::BlocksUnclaimed)?;
  // A clone shares its origin snapshot's records (A-64), so it is rebuilt beside its recovered origin, which
  // `rebuild_recovered` rebuilds first.
  let lineage = state.db.partition().lineage(record.id).cloned();
  let rebuilt = match lineage {
    None => Volume::from_image(
      &mut state.store,
      image,
      claims,
      Box::new(HostClock::new()),
      journal,
      source,
    ),
    Some(edge) => {
      let origin = state
        .by_id
        .get(&edge.origin_volume)
        .copied()
        .ok_or(RecoveryCause::OriginNotRecovered)?;
      let ShardState { store, volumes, .. } = &mut *state;
      let origin = volumes
        .get(origin)
        .map_err(|_| RecoveryCause::OriginNotRecovered)?;
      Volume::clone_from_image(
        store,
        image,
        claims,
        &origin.volume,
        core_snapshot(SnapshotId {
          value: edge.origin_snapshot.value,
        }),
        Box::new(HostClock::new()),
        journal,
        source,
      )
    }
  };
  let mut volume = rebuilt.map_err(|error| RecoveryCause::ImageRefused {
    reason: recorded_reason(error.to_string()),
  })?;
  // The catalog is the authority on the acknowledged size policy (§4.8, AC-2.3): a resize
  // publishes its image before its record commits. Refuse content exceeding that policy.
  let acknowledged = quota.limit();
  if volume.capacity_bytes() != acknowledged
    && let Err(error) = volume.resize(acknowledged)
  {
    // The half-built volume gives back its sources, records and blocks, as `from_image` does on its own refusals.
    let host = opened.as_mut().map(|(host, _)| host as &mut dyn HostFs);
    let discarded = volume.discard_partial_releasing(&mut state.store, host);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return Err(RecoveryCause::ImageRefused {
      reason: recorded_reason(format!(
        "the image's quota exceeds the acknowledged size policy: {error}"
      )),
    });
  }
  Ok((volume, opened.map(|(host, _)| host), image.prefix))
}

/// A recovered volume's resources that do not fit this process, named by `reason`.
fn resources(reason: String) -> RecoveryCause {
  RecoveryCause::ResourcesUnavailable {
    reason: recorded_reason(reason),
  }
}

/// Why the overlay's base at `path` could not be reacquired, by the host's refusal.
fn base_cause(path: &str, error: slates_vfs::host::HostError) -> RecoveryCause {
  let path = path.to_owned();
  match error {
    slates_vfs::host::HostError::NotFound => RecoveryCause::BaseMissing { path },
    slates_vfs::host::HostError::OwnMount(_) => RecoveryCause::BaseOnOwnMount { path },
    other => RecoveryCause::BaseUnavailable {
      path,
      reason: recorded_reason(other.to_string()),
    },
  }
}

/// Trims the rebuilt volume back to the catalog (§4.8, AC-2.3): a verb publishes its effect before
/// its completion record commits ([`dispatch`] inside `run_recorded`), so a crash between the two
/// leaves the image carrying a snapshot the catalog never acknowledged. The catalog is the authority
/// on what was acknowledged; such a snapshot is destroyed in the volume — its tree returned to the
/// store — so the unacknowledged effect is not partially present and a retry of the verb makes a
/// fresh one. Returns how many were trimmed. A snapshot the catalog does record is untouched.
fn trim_unrecorded_snapshots(state: &mut ShardState, volume: DbVolumeId) -> usize {
  let Some(handle) = state.by_id.get(&volume).copied() else {
    return 0;
  };
  let recorded: std::collections::BTreeSet<u64> = state
    .db
    .partition()
    .snapshots_of(volume)
    .iter()
    .map(|s| s.id.value)
    .collect();
  let ShardState { store, volumes, .. } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return 0;
  };
  let unrecorded: Vec<slates_vfs::ids::SnapshotId> = slot
    .volume
    .snapshot_ids()
    .filter(|id| !recorded.contains(&db_snapshot_value(*id)))
    .collect();
  let mut trimmed: usize = 0;
  for id in unrecorded {
    if slot.volume.destroy_snapshot(store, id).is_ok() {
      trimmed = trimmed.saturating_add(1);
    }
  }
  trimmed
}

/// The catalog's snapshot id value for a vfs snapshot id: the slot and generation packed as
/// `(index << 32) | generation` (the inverse of the unpacking in [`recovered_snapshot`]).
fn db_snapshot_value(id: slates_vfs::ids::SnapshotId) -> u64 {
  (u64::from(id.index) << u32::BITS) | u64::from(id.generation)
}

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

/// Settles a rebuilt volume's references once its attachments are reconciled (A-61): a recorded attachment whose
/// record survived keeps the references its kernel holds (a FUSE mount the anchor held), every other holder's are
/// released, and an unlinked-but-open file left with no holder is reclaimed. Returns the files reclaimed. A volume
/// that cannot be settled is counted and keeps what it has: its references are released at each attachment's
/// teardown instead.
fn settle_references(state: &mut ShardState, record: &VolumeRecord) -> usize {
  let Some(handle) = state.by_id.get(&record.id).copied() else {
    return 0;
  };
  let partition = state.db.partition();
  // The FUSE mounts of this volume whose records survived: the anchor held their devices, so their kernels may hold
  // handles the dead daemon issued after its last publication (U10).
  let held_mounts: Vec<u64> = partition
    .attachments_of(record.id)
    .into_iter()
    .filter(|attachment| attachment.form.fuse_mount_point().is_some())
    .map(|attachment| attachment.id)
    .collect();
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return 0;
  };
  match slot.volume.settle_recovered_references(
    &mut state.store,
    |id| partition.attachment(id).is_some(),
    &held_mounts,
  ) {
    Ok((_, reclaimed)) => reclaimed,
    Err(_) => {
      state.count(REFERENCES_UNSETTLED, 1);
      0
    }
  }
}

/// Format: mounts that ended with the files kept for them (U10) refused reclamation: they stay charged to the volume until
/// its next restart. Zero is the healthy reading.
const TAKEOVER_KEPT_UNRELEASED: &str = "recovery.takeover_kept_unreleased";
/// Format: revocations acknowledged while a FUSE mount they fenced was still dropping its kernel's cache, past
/// `told::TOLD_WAIT_NS` (AUD-29-84): its requests were refused, but a page its kernel held may have been read meanwhile.
/// Zero is the healthy reading.
#[cfg(target_os = "linux")]
const REVOKED_MOUNTS_OVERDUE: &str = "fuse.revoked_overdue";

/// Format: the refusal counter of a write-log span the content object refused (A-63): that write is not logged.
#[cfg(unix)]
pub(crate) const WRITE_LOG_UNWRITTEN: &str = "recovery.write_log_unwritten";

/// Empties the shard's FUSE write log after a publication that captured every volume (A-63), stamped with that
/// publication's generation: every write before it is in that image now. A volume skipped keeps the log, so its
/// writes stay owed to the next daemon.
#[cfg(unix)]
fn clear_write_log(state: &mut ShardState, captured_every_volume: bool, generation: u64) {
  if !captured_every_volume {
    return;
  }
  let Some(object) = state.content.as_mut() else {
    return;
  };
  if let Some(log) = state.write_log.as_mut()
    && log.clear(object, generation).is_err()
  {
    state.count(WRITE_LOG_UNWRITTEN, 1);
  }
}

/// Replays the FUSE writes the previous daemon acknowledged after its last publication (A-63) into the volumes
/// recovery rebuilt, in the order they were acknowledged: each to the volume it was written through, which its record
/// names (an inode number's prefix names a clone's origin for every file the clone inherited, 2026-11-18). A write
/// that cannot be replayed is counted and the run carries on; the file then lacks it, as the record says. A write
/// refused `PublishNeeded` (the arena's freed blocks wait on a publication, A-64) publishes, puts every record not
/// yet replayed back in the log under the new image's stamp — the publication cleared the log, and a crash before
/// the replay ends must still find them — and is tried once more. A logged `FORGET` gives its lookups back in its
/// place among the writes (GAPS, A-154's sibling): the kernel never resends one, so without it the references a dead
/// daemon was given back after its last publication stayed counted, and an unlinked file the kernel had let go was
/// never reclaimed.
#[cfg(unix)]
async fn replay_writes(
  state: &mut ShardState,
  slices: &mut crate::recovery_slices::RecoverySlices,
) -> usize {
  let records = std::mem::take(&mut state.replay);
  let mut replayed: usize = 0;
  for (index, logged) in records.iter().enumerate() {
    if index > 0 {
      slices.unit_done().await;
    }
    let record = match logged {
      crate::write_log::Logged::Write(record) => record,
      crate::write_log::Logged::Forget(forgotten) => {
        if replay_forget(state, *forgotten).is_none_or(|given| given.is_err()) {
          state.count(FORGET_REPLAY_REFUSED, 1);
        }
        continue;
      }
    };
    let mut written = replay_one(state, record);
    if matches!(written, Some(Err(slates_vfs::VfsError::PublishNeeded))) {
      let _ = publish_shard(state);
      relog_remaining(state, records.get(index..).unwrap_or_default());
      written = replay_one(state, record);
    }
    match written {
      Some(Ok(_)) => replayed = replayed.saturating_add(1),
      _ => state.count(WRITE_REPLAY_REFUSED, 1),
    }
  }
  replayed
}

/// The slot of the volume a record names, when recovery rebuilt it.
#[cfg(unix)]
fn slot_of_volume(
  state: &ShardState,
  volume: [u8; 16],
) -> Option<slates_mem::Handle<crate::state::VolumeSlot>> {
  state.by_id.get(&DbVolumeId { bytes: volume }).copied()
}

/// Gives back one logged `FORGET`'s lookups, as its attachment's own, in the volume its record names; `None` when
/// recovery did not rebuild that volume.
#[cfg(unix)]
fn replay_forget(
  state: &mut ShardState,
  forgotten: crate::write_log::Forgotten,
) -> Option<Result<(), slates_vfs::VfsError>> {
  let handle = slot_of_volume(state, forgotten.volume)?;
  let slot = state.volumes.get_mut(handle).ok()?;
  Some(slot.volume.forget_for(
    &mut state.store,
    slates_vfs::ids::InodeNo(forgotten.inode),
    slates_vfs::ids::RefOwner::Attachment(forgotten.attachment),
    forgotten.count,
  ))
}

/// Writes one logged record into the volume it was written through, which the record names; `None` when recovery did
/// not rebuild that volume.
#[cfg(unix)]
fn replay_one(
  state: &mut ShardState,
  record: &crate::write_log::Record,
) -> Option<Result<usize, slates_vfs::VfsError>> {
  let handle = slot_of_volume(state, record.volume)?;
  let slot = state.volumes.get_mut(handle).ok()?;
  let inode = slates_vfs::ids::InodeNo(record.inode);
  let written = slot
    .volume
    .write(&mut state.store, inode, record.offset, &record.bytes);
  // A mount taken over by this daemon may hold pages of this inode that differ from the volume with no reference of it
  // recorded, so its takeover round names it (A-158): older ones when the write landed, and the refused write's own
  // bytes, which its writer's kernel cached, when it did not.
  slot.volume.mark_replayed(inode);
  Some(written)
}

/// Empties the write log under the committed image's generation and appends `remaining` to it, so the records a
/// replay has not yet applied survive a crash before it ends. A record the log cannot take marks it overflowed
/// (A-63: the next daemon reports every taken-over file as having lost writes).
#[cfg(unix)]
fn relog_remaining(state: &mut ShardState, remaining: &[crate::write_log::Logged]) {
  let generation = state.journal.generation();
  let (Some(object), Some(log)) = (state.content.as_mut(), state.write_log.as_mut()) else {
    return;
  };
  let mut kept = log.clear(object, generation).is_ok();
  for logged in remaining {
    let appended = match logged {
      crate::write_log::Logged::Write(record) => log.append(
        object,
        (record.volume, record.inode),
        record.offset,
        &record.bytes,
      ),
      crate::write_log::Logged::Forget(forgotten) => log.append_forget(object, *forgotten),
    };
    if kept && appended.is_err() {
      kept = false;
    }
  }
  if !kept {
    let _ = log.overflow(object);
    state.count(WRITE_LOG_UNWRITTEN, 1);
  }
}

/// Format: the refusal counter of a logged write recovery could not replay (A-63).
#[cfg(unix)]
const WRITE_REPLAY_REFUSED: &str = "recovery.write_replay_refused";

/// Format: the refusal counter of a logged `FORGET` recovery could not give back (no rebuilt volume holds its inode,
/// or the volume refused it).
#[cfg(unix)]
const FORGET_REPLAY_REFUSED: &str = "recovery.forget_replay_refused";

/// Format: the refusal counter of a volume whose recovered references could not be settled.
const REFERENCES_UNSETTLED: &str = "recovery.references_unsettled";

/// Reconciles the catalog with what recovery could honour, each change a recorded operation so
/// replay agrees (§4.8): a **local** (unplaced) snapshot the volume's recovery image did not carry
/// is destroyed and, if it was the head, the head is reset — its content did not reach anchor-owned
/// RAM before the crash (an image that could not be published), so the catalog must not claim it;
/// one the image *did* rebuild is kept, its content and the head that points at it surviving the
/// restart. A placed snapshot (durably held elsewhere) is never dropped here. A ring client's
/// attachments are removed (the client's ring did not survive the process; it attaches again), while a
/// host mount's — the bridge's, `Consumer::Bridge` — is kept: the kernel mount outlives the daemon (the
/// anchor keeps the listener, §4.6) and its handles carry the attachment's capability, which nothing
/// can re-mint for the kernel (AUD-01). Returns the (snapshots, attachments) reconciled out, so the
/// caller reports exactly that.
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
  let mut snapshots: usize = 0;
  for id in &lost {
    let op = Op::SnapshotDestroyed {
      volume: record.id,
      id: *id,
    };
    if state.db.mutate(&mut state.segment, &op, now).is_ok() {
      snapshots = snapshots.saturating_add(1);
    }
  }
  if lost.contains(&record.head) {
    match record.epoch.checked_add(1) {
      Some(epoch) => {
        let op = Op::VolumeHeadAdvanced {
          id: record.id,
          head: DbSnapshotId::default(),
          epoch,
        };
        let advanced = state.db.mutate(&mut state.segment, &op, now);
        count_secondary(state, advanced);
      }
      // No epoch can supersede the lost head's: counted, and the head is left as recorded.
      None => state.count(VOLUME_EPOCH_EXHAUSTED, 1),
    }
  }
  // An SDK's record, a FUSE mount and a guest device all die with the process: the SDK's client attaches again,
  // a FUSE mount's device was the killed process's own (AUD-29-64), so its dead mount is unmounted once this
  // shard runs (`fuse::unmount_stale`), the kernel's table confirming the mount is the one the record names;
  // and a guest device's loop and seam were the process's own (AUD-29-68), so its harness attaches again.
  let attached: Vec<u64> = state
    .db
    .partition()
    .attachments_of(record.id)
    .iter()
    .filter(|a| {
      if let Some(path) = a.form.fuse_mount_point() {
        // A mount whose device the anchor held, on a kernel that can resend what the dead daemon read, is kept
        // and served again (A-61); any other dies with the process, as before.
        #[cfg(target_os = "linux")]
        if let Some(at) = state
          .inherited_fuse
          .iter()
          .position(|held| held.attachment == a.id && held.adoptable())
        {
          let held = state.inherited_fuse.swap_remove(at);
          state.adopt_fuse.push(((**a).clone(), held));
          return false;
        }
        state.stale_fuse_mounts.push((a.id, path.to_owned()));
        return true;
      }
      // An SDK client's write attachment that writes back a copy the anchor held is kept, its copy taken back (A-197).
      #[cfg(target_os = "linux")]
      if crate::held_copy::keeps(&state.inherited_copies, a.id) {
        return false;
      }
      // A Shards stream whose resync end the anchor held is kept, and served again on a fresh stream its device is handed
      // over that end (A-316; macOS too, A-339); any other dies with the process, as before.
      #[cfg(unix)]
      if matches!(a.form, AttachForm::ShardsFs { .. })
        && let Some(at) = state
          .inherited_fuse
          .iter()
          .position(|held| held.attachment == a.id)
      {
        let held = state.inherited_fuse.swap_remove(at);
        state.adopt_shards.push(((**a).clone(), held));
        return false;
      }
      #[cfg(windows)]
      if matches!(a.form, AttachForm::WinFspMount { .. })
        && let Some(at) = state
          .inherited_winfsp
          .iter()
          .position(|held| held.attachment == a.id && held.matches(a))
      {
        let held = state.inherited_winfsp.swap_remove(at);
        state.adopt_winfsp.push(((**a).clone(), held));
        return false;
      }
      // A WinFsp volume's file system was the dead process's own (§4.6 "Windows"): its drive went with it.
      matches!(a.consumer, Consumer::Sdk { .. } | Consumer::Guest)
        || matches!(a.form, AttachForm::WinFspMount { .. })
    })
    .map(|a| a.id)
    .collect();
  let mut attachments: usize = 0;
  for id in attached {
    if state
      .db
      .mutate(&mut state.segment, &Op::AttachmentRemoved { id }, now)
      .is_ok()
    {
      attachments = attachments.saturating_add(1);
    }
  }
  (snapshots, attachments)
}

/// Stamps a freshly provisioned `volume`'s root directory with its provisioning user's identity:
/// `principal`'s uid — a Unix user; another kind of principal (a Windows SID) leaves the root as born,
/// ownership being security descriptors there — and this process's effective group
/// ([`creator_gid`]). Without this the root keeps the volume core's born `uid 0, gid 0`
/// (`Inode::new`) and lists as root:wheel through a mount, so tools that check their working
/// directory's owner (git's `safe.directory`) refuse it and the NFS export's POSIX permission checks
/// refuse the mounting user the root of its own volume.
fn stamp_root_owner(
  store: &mut slates_vfs::volume::Store,
  volume: &mut Volume,
  principal: &Principal,
) -> Result<(), slates_vfs::error::VfsError> {
  let Principal::Uid { uid, .. } = principal else {
    return Ok(());
  };
  let root = volume.root_inode(store)?;
  volume.chown(store, root, *uid, creator_gid())
}

/// The group a provisioned volume's root takes: this process's effective gid. The rendezvous admits
/// only the daemon's own uid (`crates/ipc/src/rendezvous.rs`, "refuses another uid"), so the
/// provisioning client and the daemon are one user, and this is that user's primary group — the same
/// group an object the user creates through the mount takes from its `AUTH_SYS` credential.
#[cfg(unix)]
fn creator_gid() -> u32 {
  rustix::process::getegid().as_raw()
}

/// Windows: ownership is a security descriptor, not a gid; the root keeps the volume core's default
/// group, which the WinFsp bridge never reports as a POSIX id.
#[cfg(not(unix))]
fn creator_gid() -> u32 {
  0
}

#[cfg(test)]
mod tests {
  use super::{Continued, next_batch};
  use super::{
    HostId, ObjectId, Principal, ReadAheadLedger, Refusal, RegionId, ReplyBody, VolumeId,
    home_redirect, join_batch, verify_attestation,
  };
  use slates_db::register::RootConfiguration;

  /// §4.8, §4.4 `status` (A-263 on B9). Do: frame the refusal a verb on an unrecovered overlay answers, its cause the
  /// longest one a volume records: a base path of [`super::longest_recordable_base`] bytes and a reason of
  /// [`super::RECOVERY_REASON_BYTES`]; and record a reason past that bound, multi-byte characters at the cut. Expect: the
  /// refusal fits one reply chunk whole, the base is no longer than the platform's path maximum, and the long reason
  /// is cut at a character boundary to the bound.
  #[test]
  fn the_longest_recorded_cause_fits_the_refusal_whole() {
    use slates_ipc::protocol::RecoveryCause;
    let longest = super::longest_recordable_base();
    let refusal = ReplyBody::Refused {
      refusal: Refusal::NotRecovered {
        cause: RecoveryCause::BaseUnavailable {
          path: "p".repeat(longest),
          reason: "r".repeat(super::RECOVERY_REASON_BYTES),
        },
      },
    };
    let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap();
    let framed = slates_ipc::protocol::framed_len(&refusal);
    eprintln!(
      "the longest recorded base here: {longest} bytes; the refusal's own room: {} bytes",
      chunk - (framed - longest)
    );
    assert!(framed <= chunk);
    assert!(longest <= super::PLATFORM_PATH_BYTES);
    let reason = super::recorded_reason("é".repeat(super::RECOVERY_REASON_BYTES));
    assert!(reason.len() <= super::RECOVERY_REASON_BYTES && reason.chars().all(|c| c == 'é'));
  }

  /// §4.4 `list` (A-263 with B8's `not_recovered`). Do: list a summary with the longest listable name and an unrecovered
  /// overlay's cause whose base path and reason run 10,000 bytes each, multi-byte characters among them; and one whose
  /// cause is short. Expect: the first fits a page alone, its name whole and its cause the same variant shortened at
  /// character boundaries; the second is listed unchanged. A cause's text comes from the host and the recorded base, so
  /// without the cut one long path made the listing it was in "reply too large for the bulk area".
  #[test]
  fn a_listed_volume_fits_a_page_whatever_its_recovery_cause_says() {
    use slates_ipc::protocol::{RecoveryCause, VolumeSummary};
    let summary = |cause: RecoveryCause| VolumeSummary {
      id: VolumeId::default(),
      name: "n".repeat(super::longest_listable_name()),
      referenced_bytes: u64::MAX,
      unique_bytes: u64::MAX,
      overlay: true,
      live_shipped: false,
      not_recovered: Some(cause),
    };
    let long = "é/".repeat(3_334);
    let whole = summary(RecoveryCause::BaseUnavailable {
      path: long.clone(),
      reason: long,
    });
    assert!(
      super::encoded_len(&whole) > super::list_page_room(),
      "uncut, it fits no page"
    );
    let listed = super::listed(whole);
    assert!(super::encoded_len(&listed) <= super::list_page_room());
    assert_eq!(listed.name.len(), super::longest_listable_name());
    assert!(matches!(
      listed.not_recovered,
      Some(RecoveryCause::BaseUnavailable { .. })
    ));
    let short = summary(RecoveryCause::BaseMissing {
      path: String::new(),
    });
    assert_eq!(super::listed(short.clone()), short);
  }

  /// A volume owned by `owner`, sharing it with each of `access`.
  fn volume_owned_by(
    owner: Principal,
    access: Vec<slates_db::catalog::AccessEntry>,
  ) -> slates_db::catalog::VolumeRecord {
    use slates_db::catalog::{
      BaseRecord, NamePolicy, PolicyRecord, Role, SizeClass, SnapshotId, VolumeRecord, VolumeState,
    };
    VolumeRecord {
      id: slates_db::catalog::VolumeId { bytes: [7; 16] },
      name: "owned".to_owned(),
      owner_shard: 0,
      policy: PolicyRecord {
        size: SizeClass::Dynamic { max: 0 },
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
      head: SnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner,
      access,
      created_ns: 0,
      catalog_version: 0,
      taken_over: None,
    }
  }

  /// A-237 (§4.13): Windows→Unix-root by rule. Every Windows channel is recorded as uid 0, Unix root's number. Do:
  /// judge a volume owned by root on host A for root on A, for uid 0 forwarded from host B (a Windows user there, or
  /// B's root) as `serve_forward` rescopes it by B's session, and for that principal again once A shares the volume
  /// read-only with (B, uid 0). Expect: A's root owns it; B's uid 0 has no right at all; the share grants B's uid 0
  /// read alone.
  #[test]
  fn uid_zero_on_another_host_is_not_this_hosts_root() {
    use slates_db::catalog::{AccessEntry, Rights};
    let host_a = slates_db::HostId(0xA);
    let host_b = slates_db::HostId(0xB);
    let root_on_a = Principal::Uid {
      host: host_a,
      uid: 0,
    };
    // The origin's request names its own host; the session's anchor replaces it (`Principal::rehosted`).
    let zero_from_b = Principal::Uid {
      host: host_a,
      uid: 0,
    }
    .rehosted(host_b);
    let read_only = Rights {
      read: true,
      write: false,
      admin: false,
    };
    let all = Rights {
      read: true,
      write: true,
      admin: true,
    };
    let unshared = volume_owned_by(root_on_a.clone(), Vec::new());
    let shared = volume_owned_by(
      root_on_a.clone(),
      vec![AccessEntry {
        principal: Principal::Uid {
          host: host_b,
          uid: 0,
        },
        rights: read_only,
      }],
    );
    assert_eq!(
      (
        super::rights_of(&unshared, &root_on_a),
        super::rights_of(&unshared, &zero_from_b),
        super::rights_of(&shared, &zero_from_b),
      ),
      (all, Rights::default(), read_only)
    );
  }

  /// A page reply of `len` bytes of `fill`, at `stamp`, of a file `total` long.
  fn page(fill: u8, len: usize, (stamp, total): (u64, u64)) -> ReplyBody {
    ReplyBody::ReadPage {
      bytes: vec![fill; len],
      total,
      stamp,
    }
  }

  /// §4.8 Lookup read-ahead (`join_batch`). Do: join batches of four-byte windows: all whole and at one stamp; one
  /// whose third window was read after a write (another stamp); one whose second window is short (the end of the
  /// file) with a third behind it; one whose second reply is a refusal; one whose first window is short. Expect: the
  /// whole batch joined in order; the windows before the changed stamp only; the short window joined and nothing
  /// past it; nothing past the refusal; nothing past a short first window.
  #[test]
  fn a_batch_joins_in_order_only_while_each_window_abuts_and_reads_one_state() {
    let at = (7, 100);
    let mut bytes = vec![0; 4];
    let joined = join_batch(&mut bytes, 4, at, vec![page(1, 4, at), page(2, 4, at)]);
    assert_eq!((joined, bytes), (2, [[0; 4], [1; 4], [2; 4]].concat()));

    let mut bytes = vec![0; 4];
    let joined = join_batch(
      &mut bytes,
      4,
      at,
      vec![page(1, 4, at), page(2, 4, (8, 100))],
    );
    assert_eq!((joined, bytes), (1, [[0; 4], [1; 4]].concat()));

    let mut bytes = vec![0; 4];
    let joined = join_batch(&mut bytes, 4, at, vec![page(1, 2, at), page(2, 4, at)]);
    assert_eq!((joined, bytes), (1, [vec![0; 4], vec![1; 2]].concat()));

    let refusal = ReplyBody::Refused {
      refusal: Refusal::NotFound,
    };
    let mut bytes = vec![0; 4];
    let joined = join_batch(&mut bytes, 4, at, vec![refusal, page(2, 4, at)]);
    assert_eq!((joined, bytes), (0, vec![0; 4]));

    let mut bytes = vec![0; 3];
    let joined = join_batch(&mut bytes, 4, at, vec![page(1, 4, at)]);
    assert_eq!((joined, bytes), (0, vec![0; 3]));
  }

  /// §4.8 Lookup read-ahead (`next_batch`). Do: plan the fetch of a read that does not continue a window; of one
  /// that continues a batch of 4 with 100 windows left; with 3 windows left; with none left; and of a batch of 512
  /// in a file of 4 GiB. Expect: one window of the node's own size; 8; 3; 1, never zero; and 1,024. No measured
  /// rate caps a batch (an app-limited sample must not lower the estimate it feeds, `next_batch`): the cap this
  /// replaced held the two-network crossing to one window per forward.
  #[test]
  fn a_continuing_read_doubles_its_batch_within_the_file() {
    let window = 64 << 10;
    let continued = |batch, total| {
      Some(Continued {
        window,
        batch,
        total,
      })
    };
    assert_eq!(next_batch(None, 0, 5_000), (5_000, 1));
    assert_eq!(
      next_batch(continued(4, 200 * window), 100 * window, 1),
      (window, 8)
    );
    assert_eq!(
      next_batch(continued(4, 103 * window), 100 * window, 1),
      (window, 3)
    );
    assert_eq!(
      next_batch(continued(4, 100 * window), 100 * window, 1),
      (window, 1)
    );
    assert_eq!(next_batch(continued(512, 4 << 30), 0, 1), (window, 1_024));
  }

  /// §4.8 Lookup read-ahead (`ReadAheadLedger`). Do: charge windows up to the bound, one byte past it, then credit
  /// more than is held. Expect: the charges within the bound are taken, the one past it refused with nothing
  /// changed, and the over-credit leaves nothing held (never a negative or wrapped charge).
  #[test]
  fn the_read_ahead_ledger_refuses_past_its_bound_and_never_credits_below_zero() {
    let mut ledger = ReadAheadLedger::default();
    assert!(ledger.charge(60, 100));
    assert!(ledger.charge(40, 100));
    assert!(!ledger.charge(1, 100));
    assert_eq!(ledger.room(100), 0);
    ledger.credit(1_000);
    assert_eq!(ledger.room(100), 100);
  }

  /// Shape: the reduced reserve the relief test's shard runs with, so filling its arena takes a few chunks, not the
  /// machine's measured production reserve.
  const RELIEF_RESERVE: u64 = 32 << 20;

  /// A-63, A-166's sibling sweep (from reading, proven here, 2026-11-18): a FUSE write a dead daemon logged to a clone's
  /// inherited file is replayed into the clone, never its origin. A clone keeps its origin's inode numbers, the origin's
  /// prefix in their top bits, for every file it inherits, and the replay found its volume by that prefix. Do: on a
  /// shard, a volume whose file "f" holds "origin", snapshotted and cloned; replay a logged write of "clone!" at offset 0
  /// of the clone's "f", through the clone, as the successor of a daemon that served it would. Expect: the clone's "f"
  /// reads "clone!" and the origin's still "origin", the inode carrying the origin's prefix (non-vacuous). Before, the
  /// write went to the origin, the clone kept "origin" and lost its write. Every Unix: macOS replays a Shards stream's
  /// logged writes too (A-339).
  #[cfg(unix)]
  #[test]
  fn a_replayed_write_to_a_clones_inherited_file_lands_in_the_clone() {
    let (origin_reads, clone_reads, inherited_prefix, origin_prefix) =
      crate::daemon::audit_on_shard(|state| {
        let principal = Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        };
        let super::ReplyBody::Created { id: origin } = super::dispatch(
          state,
          1,
          &principal,
          super::RequestBody::Create {
            name: "replay-origin".to_owned(),
            size: super::SizeClass::Dynamic { max: 1 << 20 },
            names: super::NamePolicy::Exact,
            require_locked: false,
            base: None,
          },
        ) else {
          panic!("the origin was created")
        };
        let origin_handle = *state.by_id.get(&super::to_db_volume(origin)).unwrap();
        let super::ShardState { store, volumes, .. } = &mut *state;
        let slot = volumes.get_mut(origin_handle).unwrap();
        let root = slot.volume.root_inode(store).unwrap();
        let file = slot.volume.create_file_no(store, root, "f", 0o644).unwrap();
        slot.volume.write(store, file, 0, b"origin").unwrap();
        let origin_prefix = slot.volume.prefix();
        let super::ReplyBody::Snapshotted { id: snapshot, .. } = super::dispatch(
          state,
          1,
          &principal,
          super::RequestBody::Snapshot { volume: origin },
        ) else {
          panic!("the origin was snapshotted")
        };
        let reply = super::dispatch(
          state,
          1,
          &principal,
          super::RequestBody::Clone {
            volume: origin,
            snapshot,
            name: "replay-clone".to_owned(),
          },
        );
        let super::ReplyBody::Cloned { id: clone } = reply else {
          panic!("the clone was made: {reply:?}")
        };
        let clone_handle = *state.by_id.get(&super::to_db_volume(clone)).unwrap();
        let super::ShardState { store, volumes, .. } = &mut *state;
        let clone_slot = volumes.get(clone_handle).unwrap();
        let clone_root = clone_slot.volume.root_inode(store).unwrap();
        let inherited = clone_slot
          .volume
          .lookup_no(store, clone_root, "f")
          .unwrap()
          .inode;
        let record = crate::write_log::Record {
          volume: clone.bytes,
          inode: inherited.0,
          offset: 0,
          bytes: b"clone!".to_vec(),
        };
        assert!(
          matches!(super::replay_one(state, &record), Some(Ok(_))),
          "the logged write was replayed"
        );
        let read = |state: &mut super::ShardState, handle| {
          let super::ShardState { store, volumes, .. } = &mut *state;
          let slot = volumes.get(handle).unwrap();
          let root = slot.volume.root_inode(store).unwrap();
          let no = slot.volume.lookup_no(store, root, "f").unwrap().inode;
          let mut bytes = [0u8; 6];
          let read = slot.volume.read(store, no, 0, &mut bytes).unwrap();
          bytes.get(..read).unwrap_or_default().to_vec()
        };
        (
          read(state, origin_handle),
          read(state, clone_handle),
          inherited.prefix(),
          origin_prefix,
        )
      });
    assert_eq!(
      inherited_prefix, origin_prefix,
      "the clone's inherited file carries its origin's prefix"
    );
    assert_eq!(clone_reads, b"clone!", "the clone has its replayed write");
    assert_eq!(origin_reads, b"origin", "the origin is untouched");
  }

  /// A-64 (the relief). Do: on a shard with a reduced reserve, write a file large enough that rewriting it leaves the
  /// arena below the operation headroom; publish (the image names its blocks); rewrite it whole, so every old block's
  /// free is deferred; then relieve. Expect: before, deferred bytes and free bytes below the headroom; after, a
  /// publication ran (`arena.deferred_relieved` moved), nothing is deferred, and the arena has the headroom free.
  #[test]
  fn a_shard_short_of_room_only_by_deferred_frees_publishes_to_release_them() {
    crate::daemon::audit_on_shard_configured(
      |state| {
        let principal = Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        };
        let chunk = state.store.content.chunk_bytes();
        // The shard claims its arena lazily (A-98); one claim takes its own slice, a single extent at this
        // power-of-two reserve, and the volume below fits in it, so nothing else is claimed.
        state.store.make_room(1);
        let capacity = state.store.content.arena().capacity();
        let headroom = usize::try_from(state.store.budget.headroom()).unwrap();
        let file_bytes = (capacity - headroom) / 2 + 2 * chunk;
        let limit = u64::try_from(file_bytes + chunk).unwrap();
        let reply = super::dispatch(
          state,
          1,
          &principal,
          super::RequestBody::Create {
            name: "relief".to_owned(),
            size: super::SizeClass::Bounded { limit },
            names: super::NamePolicy::Exact,
            require_locked: false,
            base: None,
          },
        );
        let super::ReplyBody::Created { id } = reply else {
          panic!("{reply:?}")
        };
        let handle = *state.by_id.get(&super::to_db_volume(id)).unwrap();
        let super::ShardState { store, volumes, .. } = &mut *state;
        let slot = volumes.get_mut(handle).unwrap();
        let root = slot.volume.root_inode(store).unwrap();
        let file = slot.volume.create_file_no(store, root, "f", 0o644).unwrap();
        slot
          .volume
          .write(store, file, 0, &vec![b'a'; file_bytes])
          .unwrap();
        super::publish_shard(state).expect("the shard publishes");
        let super::ShardState { store, volumes, .. } = &mut *state;
        let slot = volumes.get_mut(handle).unwrap();
        slot
          .volume
          .write(store, file, 0, &vec![b'b'; file_bytes])
          .unwrap();
        let arena = state.store.content.arena();
        assert!(arena.deferred_bytes() > 0, "the rewrite's old blocks wait");
        assert!(
          arena.free_bytes() < headroom,
          "the arena is short of room only by them"
        );
        super::relieve_deferred(state);
        let arena = state.store.content.arena();
        assert_eq!(arena.deferred_bytes(), 0, "the publication released them");
        assert!(arena.free_bytes() >= headroom, "the headroom is free again");
        assert_eq!(state.refusals.get(super::DEFERRED_RELIEVED), Some(&1));
      },
      |config| config.reserve_per_shard = RELIEF_RESERVE,
    );
  }

  /// Measurement (not a gate): the per-checkpoint CPU cost as the number of volumes one shard hosts grows. A barrier's
  /// common path is `publish_delta` (its cost is its changes, A-68); `publish_checkpoint` re-images the whole shard,
  /// periodically and amortized, when the journal `wants_checkpoint`. This drives one checkpoint over a shard holding
  /// `volumes` tiny volumes and reports the best-of-[`ROUNDS`] wall time, so a cost that grows faster than the volume
  /// count — a scan nested in the per-volume mark-published sweep — shows as super-linear growth. Wall-clock, best-of-N
  /// (BENCHMARKS discipline; quiesce the machine for an honest reading). Run:
  /// `cargo test --release -p slates-server checkpoint_cost_scales -- --ignored --nocapture`.
  #[test]
  #[ignore = "measurement, not a gate; prints the per-checkpoint cost vs the hosted-volume count"]
  #[allow(clippy::cast_precision_loss)]
  fn checkpoint_cost_scales_with_the_hosted_volume_count() {
    /// Shape: the best of this many back-to-back checkpoints is reported.
    const ROUNDS: usize = 5;
    /// Shape: each measured volume's quota, a small bounded one above the inode allowance (A-109); the volumes are
    /// empty, so the checkpoint's streaming is metadata only and the sweep's cost stands out.
    const VOLUME_LIMIT: u64 = 1 << 13;
    for volumes in [512usize, 1024, 2048, 4096, 8192] {
      let (hosted, published, best_ns) = crate::daemon::audit_on_shard_configured(
        move |state| {
          let principal = Principal::Uid {
            host: slates_db::HostId(0),
            uid: 1234,
          };
          for n in 0..volumes {
            let reply = super::dispatch(
              state,
              1,
              &principal,
              super::RequestBody::Create {
                name: format!("v{n:07}"),
                size: super::SizeClass::Bounded {
                  limit: VOLUME_LIMIT,
                },
                names: super::NamePolicy::Exact,
                require_locked: false,
                base: None,
              },
            );
            assert!(
              matches!(reply, super::ReplyBody::Created { .. }),
              "volume {n} of {volumes}: {reply:?}"
            );
          }
          let mut best = u64::MAX;
          let mut published = 0usize;
          for _ in 0..ROUNDS {
            let t0 = slates_machine::clock::monotonic_ns();
            match super::publish_checkpoint(state) {
              Ok(out) => {
                let t1 = slates_machine::clock::monotonic_ns();
                best = best.min(t1.saturating_sub(t0));
                published = out.volumes.len();
              }
              // A repeat may find no free content slot; the rounds that ran still give a best.
              Err(_) => break,
            }
          }
          assert!(best < u64::MAX, "at least one checkpoint ran");
          (state.volumes.len(), published, best)
        },
        move |config| {
          let cap = volumes.saturating_add(volumes / 4).saturating_add(64);
          config.store.max_inodes = config.store.max_inodes.max(cap);
          config.store.max_dirs = config.store.max_dirs.max(cap);
          config.store.max_dir_blocks = config.store.max_dir_blocks.max(cap);
          config.store.max_chunks = config.store.max_chunks.max(cap);
          let cap64 = u64::try_from(cap).unwrap_or(u64::MAX);
          config.store.metadata_class_bytes = config
            .store
            .metadata_class_bytes
            .max(cap64.saturating_mul(1 << 12));
          config.reserve_per_shard = config
            .reserve_per_shard
            .max(cap64.saturating_mul(VOLUME_LIMIT).saturating_add(128 << 20));
        },
      );
      let per = best_ns as f64 / hosted as f64;
      eprintln!(
        "checkpoint: {hosted} volumes hosted, {published} published | best {best_ns} ns ({per:.1} ns/volume)"
      );
    }
  }

  /// Measurement (not a gate): what a mutating verb's barrier costs as the one volume a shard hosts grows (§4.3, §4.8).
  /// The barrier is usually a delta (A-68), whose cost is its changes. Once the deltas since the last checkpoint reach
  /// the checkpoint's size, or the log has no room, the publish is instead a checkpoint of every volume, in the one
  /// call. For each shape (`files` files of `bytes` bytes in one volume), this prints:
  /// - a forced `publish_checkpoint`, best of [`ROUNDS`];
  /// - the publishes of one 64-byte overwrite each, until one is a checkpoint: their count, their median and worst
  ///   delta, and the checkpoint publish's own cost.
  ///
  /// A publish was a checkpoint when the log's tail is back at its start after it. Wall-clock, best-of-N (BENCHMARKS
  /// discipline). Run: `cargo test --release -p slates-server --lib a_barriers_cost_against_the_shards_size -- --ignored
  /// --nocapture`.
  #[test]
  #[ignore = "measurement, not a gate; prints a barrier's delta and checkpoint cost vs the shard's files and bytes"]
  #[allow(clippy::cast_precision_loss)]
  fn a_barriers_cost_against_the_shards_size() {
    /// Shape: the best of this many forced checkpoints is reported.
    const ROUNDS: usize = 3;
    /// Shape: the overwrite each measured barrier publishes, small against every file.
    const OVERWRITE: usize = 64;
    /// Shape: the most overwrites tried before a checkpoint comes; past it, the count is reported as reached.
    const MOST_WRITES: usize = 400_000;
    for (files, bytes) in [
      (1_000usize, 64usize),
      (1_000, 16_384),
      (4_000, 64),
      (4_000, 16_384),
      (16_000, 64),
      (16_000, 16_384),
    ] {
      let line = crate::daemon::audit_on_shard_configured(
        move |state| {
          let principal = Principal::Uid {
            host: slates_db::HostId(0),
            uid: 1234,
          };
          let limit = u64::try_from(files.saturating_mul(bytes.max(4096)).saturating_mul(2))
            .unwrap_or(u64::MAX)
            .saturating_add(64 << 20);
          let reply = super::dispatch(
            state,
            1,
            &principal,
            super::RequestBody::Create {
              name: "sized".to_owned(),
              size: super::SizeClass::Bounded { limit },
              names: super::NamePolicy::Exact,
              require_locked: false,
              base: None,
            },
          );
          let super::ReplyBody::Created { id } = reply else {
            panic!("{reply:?}")
          };
          let handle = *state.by_id.get(&super::to_db_volume(id)).unwrap();
          let content = vec![b'a'; bytes];
          let first = {
            let super::ShardState { store, volumes, .. } = &mut *state;
            let slot = volumes.get_mut(handle).unwrap();
            let root = slot.volume.root_inode(store).unwrap();
            let mut first = None;
            for n in 0..files {
              let file = slot
                .volume
                .create_file_no(store, root, &format!("f{n:06}"), 0o644)
                .unwrap();
              slot.volume.write(store, file, 0, &content).unwrap();
              first.get_or_insert(file);
            }
            first.unwrap()
          };
          super::publish_shard(state).expect("the shard publishes");
          let mut checkpoint = u64::MAX;
          for _ in 0..ROUNDS {
            let t0 = slates_machine::clock::monotonic_ns();
            super::publish_checkpoint(state).expect("a checkpoint publishes");
            checkpoint = checkpoint.min(slates_machine::clock::monotonic_ns().saturating_sub(t0));
          }
          let tail = |state: &super::ShardState| {
            state
              .journal
              .dead_ranges(usize::MAX, usize::MAX)
              .map_or(0, |dead| dead.log_past_tail.0)
          };
          let mut deltas: Vec<u64> = Vec::new();
          let mut checkpointed = None;
          for n in 0..MOST_WRITES {
            {
              let super::ShardState { store, volumes, .. } = &mut *state;
              let slot = volumes.get_mut(handle).unwrap();
              let fill = [u8::try_from(n % 251).unwrap(); OVERWRITE];
              slot.volume.write(store, first, 0, &fill).unwrap();
            }
            let t0 = slates_machine::clock::monotonic_ns();
            super::publish_shard(state).expect("the shard publishes");
            let took = slates_machine::clock::monotonic_ns().saturating_sub(t0);
            if tail(state) == 0 {
              checkpointed = Some(took);
              break;
            }
            deltas.push(took);
          }
          deltas.sort_unstable();
          let median = deltas.get(deltas.len() / 2).copied().unwrap_or(0);
          let worst = deltas.last().copied().unwrap_or(0);
          format!(
            "barrier: {files} files of {bytes} B | forced checkpoint best {checkpoint} ns ({:.0} ns/file) | {} deltas \
             before a checkpoint, median {median} ns, worst {worst} ns | the checkpoint publish {}",
            checkpoint as f64 / files as f64,
            deltas.len(),
            checkpointed.map_or_else(
              || format!("not reached in {MOST_WRITES}"),
              |ns| format!("{ns} ns")
            ),
          )
        },
        move |config| {
          let cap = files.saturating_add(files / 4).saturating_add(64);
          config.store.max_inodes = config.store.max_inodes.max(cap);
          config.store.max_dirs = config.store.max_dirs.max(cap);
          config.store.max_dir_blocks = config.store.max_dir_blocks.max(cap);
          config.store.max_chunks = config.store.max_chunks.max(
            cap
              .saturating_mul(bytes.div_ceil(4096).max(1))
              .saturating_mul(2),
          );
          let cap64 = u64::try_from(cap).unwrap_or(u64::MAX);
          config.store.metadata_class_bytes = config
            .store
            .metadata_class_bytes
            .max(cap64.saturating_mul(1 << 12));
          config.reserve_per_shard = config.reserve_per_shard.max(
            cap64
              .saturating_mul(u64::try_from(bytes.max(4096)).unwrap_or(u64::MAX))
              .saturating_mul(4)
              .saturating_add(256 << 20),
          );
        },
      );
      eprintln!("{line}");
    }
  }

  /// §4.2 (the resource vector's inode dimension; GAPS 2026-10-06). Do: on one shard, create four bounded volumes
  /// whose quotas together take the bytes the shard can admit. Expect: all four are admitted, so the inode allowances
  /// of volumes that fill the arena fit the version slab. Before, each volume asked quota ÷ `size_of::<Inode>()`
  /// slots, the first took the whole slab, and the second was refused `BudgetExceeded` with the bytes uncommitted.
  #[test]
  fn bounded_volumes_that_fill_a_shards_bytes_fit_its_version_slab() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let page = u64::try_from(state.store.content.granule()).unwrap();
      let share = state.store.admittable() / 4 / page * page;
      for n in 0..4 {
        let reply = super::dispatch(
          state,
          1,
          &principal,
          super::RequestBody::Create {
            name: format!("share-{n}"),
            size: super::SizeClass::Bounded { limit: share },
            names: super::NamePolicy::Exact,
            require_locked: false,
            base: None,
          },
        );
        assert!(
          matches!(reply, super::ReplyBody::Created { .. }),
          "volume {n} of four, {share} bytes each: {reply:?}"
        );
      }
    });
  }

  /// §4.8 (the recovery image) and §4.4 (destroy): a volume being destroyed has nothing to recover —
  /// recovery completes a recorded destroy from the catalog — so a shard publish leaves it out rather
  /// than image a tree its destroy slices have released. Before 2026-09-29 every publish during a destroy
  /// walked the released tree, failed `ESTALE`, counted the volume skipped and printed a line: 31,378
  /// lines in one provisioning histogram, each inside a verb's latency
  /// (docs/bugs/2026-09-29-a-destroy-on-a-shard-without-a-client-never-completed.md). Do: create a volume,
  /// destroy it through the verb (whose own barrier must still accept), run one destroy slice so its tree
  /// is released while it is still being destroyed, then publish. Expect: nothing skipped, and the volume
  /// neither captured nor refused.
  #[test]
  fn a_publish_leaves_out_a_volume_being_destroyed() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let reply = super::dispatch(
        state,
        1,
        &principal,
        super::RequestBody::Create {
          name: "tearing-down".to_owned(),
          size: super::SizeClass::Bounded { limit: 1 << 20 },
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id } = reply else {
        panic!("{reply:?}")
      };
      let reply = super::dispatch(
        state,
        1,
        &principal,
        super::RequestBody::Destroy { volume: id },
      );
      assert!(
        matches!(reply, super::ReplyBody::Destroyed),
        "the destroy's own barrier accepts: {reply:?}"
      );
      assert!(
        super::step_destroys(state),
        "one slice ran and the destroy is not yet recorded"
      );
      let published = super::publish_shard(state).expect("the shard publishes");
      assert!(published.skipped.is_empty(), "{published:?}");
      assert!(!published.captured(super::to_db_volume(id)));
    });
  }

  /// §4.8 (a streamed checkpoint, 2026-10-06): a checkpoint is streamed straight into its slot, so when a volume cannot
  /// be captured the bytes of the volumes before it may already be there; the checkpoint restarts without it. Do:
  /// create a volume of 2,000 files (its image several stream stages long), then an overlay whose base host is taken
  /// away (as a landing holds it), so it sorts after the first and cannot be imaged; publish a checkpoint; read the
  /// committed image back; then give the host back and publish again. Expect: the first publish captures the plain
  /// volume and skips the overlay, and the committed image holds exactly the plain volume's image; the second captures
  /// both. Non-vacuity: the plain volume sorts first and its image is longer than a stage, so the abandoned attempt had
  /// written into the slot before the overlay refused.
  #[test]
  fn a_streamed_checkpoint_restarts_without_a_volume_it_cannot_capture() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let base = concat!(env!("CARGO_MANIFEST_DIR"), "/src").to_owned();
      let (plain, plain_handle) = dynamic_volume(state, &principal, "streamed-plain", None);
      let (overlay, overlay_handle) =
        dynamic_volume(state, &principal, "streamed-overlay", Some(base));
      assert!(plain.bytes < overlay.bytes, "the plain volume sorts first");
      let plain_image = filled(state, plain_handle, 2_000);
      assert!(
        plain_image.to_content().len() > slates_vfs::recover::STREAM_STAGE_BYTES,
        "the plain volume's image spans a stage"
      );
      let host = state.volumes.get_mut(overlay_handle).unwrap().host.take();
      assert!(host.is_some(), "the overlay has a base host");

      let published = super::publish_checkpoint(state).expect("the shard publishes");
      assert_eq!(published.volumes, vec![plain], "{published:?}");
      assert_eq!(published.skipped, vec![overlay], "{published:?}");
      let committed: Vec<_> = committed_image(state)
        .volumes
        .into_iter()
        .map(|keyed| (keyed.key, keyed.image))
        .collect();
      assert_eq!(
        committed,
        vec![(plain.bytes, plain_image)],
        "the committed image holds the plain volume's image, whole, and nothing else"
      );

      state.volumes.get_mut(overlay_handle).unwrap().host = host;
      let published = super::publish_checkpoint(state).expect("the shard publishes");
      assert_eq!(published.volumes, vec![plain, overlay], "{published:?}");
      assert!(published.skipped.is_empty(), "{published:?}");
    });
  }

  /// A dynamic volume named `name` (over `base` when given), created through the verb: its id and its slot.
  fn dynamic_volume(
    state: &mut crate::state::ShardState,
    principal: &Principal,
    name: &str,
    base: Option<String>,
  ) -> (
    super::DbVolumeId,
    slates_mem::Handle<crate::state::VolumeSlot>,
  ) {
    let reply = super::dispatch(
      state,
      1,
      principal,
      super::RequestBody::Create {
        name: name.to_owned(),
        size: super::SizeClass::Dynamic { max: 1 << 26 },
        names: super::NamePolicy::Exact,
        require_locked: false,
        base,
      },
    );
    let super::ReplyBody::Created { id } = reply else {
      panic!("{name}: {reply:?}")
    };
    let id = super::to_db_volume(id);
    let handle = state
      .volumes
      .iter()
      .find(|(_, slot)| slot.id == id)
      .map(|(handle, _)| handle)
      .unwrap();
    (id, handle)
  }

  /// Fills the volume in `handle` with `files` small files in its root, through the volume core: its image after.
  fn filled(
    state: &mut crate::state::ShardState,
    handle: slates_mem::Handle<crate::state::VolumeSlot>,
    files: usize,
  ) -> slates_vfs::recover::VolumeImage {
    let slot = state.volumes.get_mut(handle).unwrap();
    let root = slot.volume.root_inode(&state.store).unwrap();
    for file in 0..files {
      let no = slot
        .volume
        .create_file_no(&mut state.store, root, &format!("file-{file:05}"), 0o644)
        .unwrap();
      slot
        .volume
        .write(&mut state.store, no, 0, b"streamed")
        .unwrap();
    }
    slot.volume.to_image(&state.store, None).unwrap()
  }

  /// The shard image committed in the shard's checkpoint slots.
  fn committed_image(state: &mut crate::state::ShardState) -> super::ShardImage {
    let (start, end) = state.content_range;
    let slots = super::ContentSlots {
      object: state.content.as_mut().unwrap(),
      start,
      len: end - start,
    };
    super::ShardImage::read_from(&slots).unwrap().unwrap()
  }

  /// A step lost to a borrow is counted, never silent (2026-10-07). Do: on a shard, while the shard's state is borrowed
  /// (an observation runs inside the borrow), ask for a counted borrow. Expect: it refused (`None`), and this shard's
  /// nested-borrow count moved by one. Before, 57 call sites discarded such a refusal and the step was lost unseen.
  #[test]
  fn a_nested_borrow_is_counted() {
    let (before, refused, after) = crate::daemon::audit_on_shard(|_state| {
      let before = crate::state::lost_steps().borrowed;
      let refused = crate::state::with_state_counted(|_| ()).is_none();
      (before, refused, crate::state::lost_steps().borrowed)
    });
    assert!(refused, "a nested borrow is refused");
    assert_eq!(after, before + 1, "and counted");
  }

  /// AUD-29-43, the laptop degenerate (R8): a destroy's tombstone is owed only to remote candidate holders, and
  /// a laptop has none, so the tombstone retires with its destroy and gives the volume's slot back. Do: create
  /// a volume, destroy it through the verb and run its slices to the end. Expect: no tombstone left in the
  /// partition, and a volume of the same name created again under a fresh id.
  #[test]
  fn a_laptop_destroy_retires_its_tombstone_with_the_destroy() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let id = created(state, &principal, "tombstoned");
      let reply = super::dispatch(
        state,
        1,
        &principal,
        super::RequestBody::Destroy { volume: id },
      );
      assert!(matches!(reply, super::ReplyBody::Destroyed), "{reply:?}");
      while super::step_destroys(state) {}
      assert_eq!(
        state.db.partition().tombstone(super::to_db_volume(id)),
        None
      );
      assert_eq!(state.db.partition().tombstones().count(), 0);
      let again = created(state, &principal, "tombstoned");
      assert_ne!(again, id, "a destroyed id is never reissued");
    });
  }

  /// A fresh bounded volume named `name`, created through the verb.
  fn created(
    state: &mut crate::state::ShardState,
    principal: &Principal,
    name: &str,
  ) -> super::VolumeId {
    let reply = super::dispatch(
      state,
      1,
      principal,
      super::RequestBody::Create {
        name: name.to_owned(),
        size: super::SizeClass::Bounded { limit: 1 << 20 },
        names: super::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let super::ReplyBody::Created { id } = reply else {
      panic!("{reply:?}")
    };
    id
  }

  /// §4.8 "Recovery", §4.3 "Bounded work everywhere" (A-217's owed item; 2026-11-18, goal g232): a recovering green's
  /// origin is decoded and seeded into its engine a share at a time, never in one step. The whole origin seeded in one
  /// step hashed and copied every file: over the Linux kernel's 1.67 GB, seconds of one poll, past the watchdog's verdict
  /// on a shard that runs a window of CPU in one step. Do: create a green with no origin, record an origin of four files
  /// of a share each as a taken-over green records one (`GreenOriginated`), and replay it a unit at a time. Expect: the
  /// origin decoded over more than one unit and seeded over more than one, and the replayed engine at version 0 with the
  /// identity the whole origin seeds.
  #[test]
  fn a_recovering_green_decodes_and_seeds_its_origin_a_share_at_a_time() {
    crate::daemon::audit_on_shard(|state| {
      let owner = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let super::ReplyBody::GreenCreated { id } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateGreen {
          name: "seeded".to_owned(),
          require_evidence: false,
          base: None,
        },
      ) else {
        panic!("create green")
      };
      let green = super::to_db_volume(id);
      let share =
        crate::merge_origin::hash_share(state).max(crate::merge_origin::copy_share(state));
      let mut origin = slates_merge::origin::Origin::default();
      for file in 0..4u8 {
        origin
          .files
          .push((format!("file-{file}"), vec![file; share]));
      }
      origin.canonicalize();
      let expected = slates_merge::engine::Green::with_origin(origin.clone()).head_identity();
      let recorded = super::Op::GreenOriginated {
        green,
        origin: origin.encode(),
      };
      state.db.mutate(&mut state.segment, &recorded, 0).unwrap();
      let mut replay = super::GreenReplay::begin(state, green).unwrap();
      let (mut decoded_in, mut seeded_in) = (0u32, 0u32);
      loop {
        match &replay.phase {
          super::GreenReplayPhase::Decode(_) => decoded_in += 1,
          super::GreenReplayPhase::Load(_) => seeded_in += 1,
          _ => {}
        }
        if replay.step(state).unwrap() {
          break;
        }
      }
      let (mut engine, _) = replay.finish(state).unwrap();
      assert!(decoded_in > 1, "the origin decoded over {decoded_in} units");
      assert!(seeded_in > 1, "the origin seeded over {seeded_in} units");
      assert_eq!(
        (engine.head(), engine.head_identity()),
        (0, expected),
        "the replayed engine is the whole origin's version 0"
      );
    });
  }

  /// AUD-29-18 (§4.16, D-27: a recovered green is its acknowledged history or nothing): do: create a green,
  /// append a chain entry no increment decodes to its durable log, rebuild it as a restart does, then ask
  /// its versions; expect the green fenced — no engine installed, the verb refused `ContentUnavailable` —
  /// never served as the shorter history the replay could reach. Until 2026-09-30 the replay stopped at the
  /// corrupt entry, logged, and installed the shortened green.
  #[test]
  fn a_green_whose_recovered_chain_is_corrupt_is_fenced_not_served_shorter() {
    crate::daemon::audit_on_shard(|state| {
      let owner = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let reply = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateGreen {
          name: "fenced".to_owned(),
          require_evidence: false,
          base: None,
        },
      );
      let super::ReplyBody::GreenCreated { id } = reply else {
        panic!("{reply:?}")
      };
      let green = super::to_db_volume(id);
      let corrupt = super::Op::GreenAdvanced {
        green,
        increment: vec![0xff; 16],
      };
      state.db.mutate(&mut state.segment, &corrupt, 0).unwrap();
      let record = state.db.partition().volume(green).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert!(
        !state.greens.contains_key(&green),
        "no engine was installed"
      );
      let reply = super::dispatch(state, 1, &owner, super::RequestBody::Versions { green: id });
      assert!(
        matches!(
          reply,
          super::ReplyBody::Refused {
            refusal: Refusal::ContentUnavailable
          }
        ),
        "{reply:?}"
      );
      // The reviewed release: its admin destroys the fenced green, and the fence goes with it.
      let reply = super::dispatch(state, 1, &owner, super::RequestBody::Destroy { volume: id });
      assert!(matches!(reply, super::ReplyBody::Destroyed), "{reply:?}");
      assert!(
        state.fenced_greens.is_empty(),
        "the fence is released with the green"
      );
    });
  }

  /// AUD-29-18: do: create a green, submit one increment through a work so its chain holds one acknowledged
  /// version, and rebuild it — expect the replay to reproduce exactly that head (version 1, the same identity);
  /// then append that same entry again, as a log replaying a duplicate would, and rebuild — expect the green
  /// fenced, since the replay accepts the duplicate as version 1, not the acknowledged version 2; and a green
  /// whose origin does not decode fenced too. A healthy replay matches; anything else is never served.
  #[test]
  fn a_healthy_green_replays_exactly_and_a_duplicate_or_corrupt_origin_is_fenced() {
    crate::daemon::audit_on_shard(|state| {
      let owner = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let super::ReplyBody::GreenCreated { id } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateGreen {
          name: "replayed".to_owned(),
          require_evidence: false,
          base: None,
        },
      ) else {
        panic!("create green")
      };
      let super::ReplyBody::WorkCreated { id: work, .. } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateWork {
          green: id,
          name: "replayed-work".to_owned(),
        },
      ) else {
        panic!("create work")
      };
      // A second work held at version 0 keeps the reachable floor there, so the version stays a chain entry rather
      // than folding into a checkpoint (W3): the replay of an entry is what this test holds.
      let super::ReplyBody::WorkCreated { .. } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateWork {
          green: id,
          name: "floor-holder".to_owned(),
        },
      ) else {
        panic!("create the floor's holder")
      };
      let edited = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::Edit {
          work,
          path: "/f".to_owned(),
          at: 0,
          delete_len: 0,
          bytes: b"hello".to_vec(),
        },
      );
      assert!(matches!(edited, super::ReplyBody::Edited), "{edited:?}");
      let submitted = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::Submit {
          work,
          evidence: Vec::new(),
        },
      );
      assert!(
        matches!(
          submitted,
          super::ReplyBody::Submitted {
            version: Some(1),
            ..
          }
        ),
        "{submitted:?}"
      );
      let green = super::to_db_volume(id);
      let before = state
        .greens
        .get_mut(&green)
        .map(|g| (g.head(), g.head_identity()));
      let record = state.db.partition().volume(green).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert_eq!(
        state
          .greens
          .get_mut(&green)
          .map(|g| (g.head(), g.head_identity())),
        before,
        "the healthy replay reproduces the acknowledged head exactly"
      );
      let entry = state.db.partition().green_chain(green)[0].clone();
      let duplicate = super::Op::GreenAdvanced {
        green,
        increment: entry,
      };
      state.db.mutate(&mut state.segment, &duplicate, 0).unwrap();
      super::rebuild_green(state, &record);
      assert!(!state.greens.contains_key(&green));
      assert_eq!(
        state.fenced_greens.get(&green),
        Some(&crate::merge_service::GreenFence::WrongVersion {
          version: 2,
          accepted: 1
        })
      );

      let super::ReplyBody::GreenCreated { id: other } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateGreen {
          name: "origin".to_owned(),
          require_evidence: false,
          base: None,
        },
      ) else {
        panic!("create green")
      };
      let other = super::to_db_volume(other);
      let corrupt = super::Op::GreenOriginated {
        green: other,
        origin: vec![0xff; 16],
      };
      state.db.mutate(&mut state.segment, &corrupt, 0).unwrap();
      let record = state.db.partition().volume(other).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert_eq!(
        state.fenced_greens.get(&other),
        Some(&crate::merge_service::GreenFence::OriginCorrupt)
      );
    });
  }

  /// Test support: creates a green named `name` with a work, and — when `hold_floor` — a second work left at version
  /// 0, which keeps the reachable floor there so nothing is checkpointed; then edits `/f` in the first work and submits
  /// it, expecting version 1. Returns the green.
  fn green_at_version_one(
    state: &mut crate::state::ShardState,
    name: &str,
    hold_floor: bool,
  ) -> super::DbVolumeId {
    let owner = Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1234,
    };
    let super::ReplyBody::GreenCreated { id } = super::dispatch(
      state,
      1,
      &owner,
      super::RequestBody::CreateGreen {
        name: name.to_owned(),
        require_evidence: false,
        base: None,
      },
    ) else {
      panic!("create green")
    };
    let works: &[&str] = if hold_floor {
      &["work", "floor"]
    } else {
      &["work"]
    };
    let mut created = Vec::new();
    for work in works {
      let super::ReplyBody::WorkCreated { id: work, .. } = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::CreateWork {
          green: id,
          name: format!("{name}-{work}"),
        },
      ) else {
        panic!("create work")
      };
      created.push(work);
    }
    let work = created[0];
    let edited = super::dispatch(
      state,
      1,
      &owner,
      super::RequestBody::Edit {
        work,
        path: "/f".to_owned(),
        at: 0,
        delete_len: 0,
        bytes: b"hello".to_vec(),
      },
    );
    assert!(matches!(edited, super::ReplyBody::Edited), "{edited:?}");
    let submitted = super::dispatch(
      state,
      1,
      &owner,
      super::RequestBody::Submit {
        work,
        evidence: Vec::new(),
      },
    );
    assert!(
      matches!(
        submitted,
        super::ReplyBody::Submitted {
          version: Some(1),
          ..
        }
      ),
      "{submitted:?}"
    );
    super::to_db_volume(id)
  }

  /// W3, AUD-29-18: do: submit one version of a green whose only work is reset to it (so the service checkpoints
  /// it) and rebuild the green; then, on a second green whose floor a work holds at version 0, record a checkpoint at
  /// version 1 whose bytes do not decode, and rebuild it. Expect: the first green's chain is empty, its checkpoint at
  /// version 1, and the rebuild restores the acknowledged head exactly (version 1, the same identity); the second is
  /// fenced `CheckpointCorrupt` rather than served from bytes the replay cannot read.
  #[test]
  fn a_green_replays_from_its_checkpoint_and_a_corrupt_one_is_fenced() {
    crate::daemon::audit_on_shard(|state| {
      let green = green_at_version_one(state, "checkpointed", false);
      assert_eq!(
        state
          .db
          .partition()
          .green_checkpoint(green)
          .map(|(held, _)| held),
        Some(1)
      );
      assert!(state.db.partition().green_chain(green).is_empty());
      let before = state
        .greens
        .get_mut(&green)
        .map(|g| (g.head(), g.head_identity()));
      let record = state.db.partition().volume(green).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert_eq!(
        state
          .greens
          .get_mut(&green)
          .map(|g| (g.head(), g.head_identity())),
        before,
        "the replay from the checkpoint reproduces the acknowledged head exactly"
      );

      let held = green_at_version_one(state, "corrupted", true);
      assert_eq!(
        state.db.partition().green_checkpoint(held),
        None,
        "the floor held at 0 folds nothing"
      );
      let corrupt = super::Op::GreenCheckpointed {
        green: held,
        version: 1,
        checkpoint: vec![0xff; 16],
      };
      state.db.mutate(&mut state.segment, &corrupt, 0).unwrap();
      let record = state.db.partition().volume(held).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert!(!state.greens.contains_key(&held));
      assert_eq!(
        state.fenced_greens.get(&held),
        Some(&crate::merge_service::GreenFence::CheckpointCorrupt)
      );
    });
  }

  /// A takeover catalog for `name` bounded at `limit` bytes, owned by uid 501, with no locked policy and no
  /// grants.
  fn takeover_catalog(name: &str, limit: u64) -> crate::catalog::CatalogValue {
    crate::catalog::CatalogValue {
      name: name.to_owned(),
      size: slates_db::catalog::SizeClass::Bounded { limit },
      names: slates_db::catalog::NamePolicy::Exact,
      owner: Principal::Uid {
        host: slates_db::HostId(0),
        uid: 501,
      },
      require_locked: false,
      access: Vec::new(),
      live_shipped: false,
      live_holders: None,
      work_of: None,
      write: None,
      write_version: 0,
      signers: Vec::new(),
    }
  }

  /// Takes over volume `id` as `catalog` describes it, from `archive`, at head sequence 1 and catalog
  /// sequence 0 (the head names no holders: the content is handed over whole).
  fn take_over(
    state: &mut crate::state::ShardState,
    id: slates_db::catalog::VolumeId,
    catalog: &crate::catalog::CatalogValue,
    archive: &slates_archive::Archive,
  ) -> Result<(), Box<super::ReplyBody>> {
    take_over_past(state, id, catalog, archive, None)
  }

  /// [`take_over`] recording `past_tolerance` as the takeover's (A-332): `None` within tolerance.
  fn take_over_past(
    state: &mut crate::state::ShardState,
    id: slates_db::catalog::VolumeId,
    catalog: &crate::catalog::CatalogValue,
    archive: &slates_archive::Archive,
    past_tolerance: Option<slates_db::catalog::TakeoverPastTolerance>,
  ) -> Result<(), Box<super::ReplyBody>> {
    take_over_at(
      state,
      id,
      catalog,
      archive,
      slates_db::catalog::TakeoverRecord {
        epoch: 1,
        head_sequence: 1,
        from_live_log: false,
        past_tolerance,
      },
    )
  }

  /// [`take_over`] as the takeover `takeover` records it: its epoch and head sequence (the adopted head's), served from a
  /// seal or a live log, past tolerance or within it.
  fn take_over_at(
    state: &mut crate::state::ShardState,
    id: slates_db::catalog::VolumeId,
    catalog: &crate::catalog::CatalogValue,
    archive: &slates_archive::Archive,
    takeover: slates_db::catalog::TakeoverRecord,
  ) -> Result<(), Box<super::ReplyBody>> {
    let head = crate::head::HeadValue {
      manifest: None,
      content_holders: Vec::new(),
      sealing: None,
    };
    let taken = super::TakenOver {
      head: &head,
      catalog,
      catalog_sequence: 0,
      sequence: takeover.head_sequence,
      takeover,
    };
    super::materialize_taken_over(state, id, &taken, Vec::new(), Some(archive))
  }

  /// The bytes of the file `f` a [`takeover_archive`] names, read from volume `id` on this shard.
  fn read_f(state: &crate::state::ShardState, id: slates_db::catalog::VolumeId) -> Vec<u8> {
    let slot = state.volumes.get(*state.by_id.get(&id).unwrap()).unwrap();
    let inode = slot.volume.resolve(&state.store, "f").unwrap().inode;
    let size = slot.volume.stat(&state.store, inode).unwrap().size;
    let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
    let read = slot
      .volume
      .read(&state.store, inode, 0, &mut bytes)
      .unwrap();
    bytes.truncate(read);
    bytes
  }

  /// A-362 (§4.8 "Promotion and takeover", A-334; the chaos nemesis, seed 6876545201569229162): a takeover that names a
  /// node holding its own superseded copy of the volume serves the state it adopted, not that copy. Do: take a volume over
  /// from an archive holding `OLD!` (this node's copy, as it held it when it owned the volume); mark it yielded, as this
  /// host's re-admission under its id marks every volume it catalogued (A-334); then take it over again from an archive
  /// holding `NEW!` at a later epoch and head (the state its successor's acknowledged writes left), as a takeover that
  /// names this node again does; then let the reaper step its destroys. Expect the file to read `NEW!`, the volume no
  /// longer lost to a successor (its verbs run here), its record naming the later takeover, the stale copy counted
  /// relinquished, and its slot torn down and counted freed. Before, the second takeover found the id present and kept the
  /// stale copy: the file read `OLD!`, and every verb on the volume was still sent away from its new owner.
  #[test]
  fn a_takeover_that_names_this_node_again_replaces_its_superseded_copy() {
    crate::daemon::audit_on_shard(|state| {
      let archive_of = |bytes: &[u8]| {
        let chunk = slates_archive::Archive::raw_chunk(bytes.to_vec());
        takeover_archive(
          vec![slates_archive::Extent {
            offset: 0,
            len: 4,
            chunk: chunk.identity,
            chunk_offset: 0,
          }],
          vec![chunk],
        )
      };
      let id = slates_db::catalog::VolumeId { bytes: [0x81; 16] };
      let catalog = takeover_catalog("readopted", 1 << 20);
      take_over(state, id, &catalog, &archive_of(b"OLD!")).unwrap();
      assert_eq!(read_f(state, id), b"OLD!", "this node's own copy");
      state.yielded.insert(id);
      assert!(
        super::lost_to_a_successor(state, id),
        "the copy is lost to a successor"
      );
      let later = slates_db::catalog::TakeoverRecord {
        epoch: 3,
        head_sequence: 9,
        from_live_log: false,
        past_tolerance: None,
      };
      take_over_at(state, id, &catalog, &archive_of(b"NEW!"), later).unwrap();
      let count = |state: &crate::state::ShardState, name: &str| {
        state.refusals.get(name).copied().unwrap_or(0)
      };
      // Shape: destroy slices enough for a four-byte volume's teardown, many times over.
      const STEPS: usize = 64;
      for _ in 0..STEPS {
        if !super::step_destroys(state) {
          break;
        }
      }
      assert_eq!(read_f(state, id), b"NEW!", "the adopted state is served");
      assert!(
        !super::lost_to_a_successor(state, id),
        "its verbs run here, its new owner"
      );
      assert_eq!(
        state
          .db
          .partition()
          .volume(id)
          .and_then(|record| record.taken_over),
        Some(later),
        "its record names the later takeover"
      );
      assert_eq!(
        (
          count(state, "fleet.takeover.relinquished"),
          count(state, "fleet.takeover.relinquished_freed")
        ),
        (1, 1),
        "the stale copy relinquished once and its slot torn down"
      );
    });
  }

  /// A-332 (A-309, AC-8.3; §4.10 "Auto-seal"): a takeover past the volume's tolerance shows in the volume's loss report.
  /// Do: take over one volume recorded past tolerance (its head register adopted from one member of a cohort whose
  /// recovery quorum is two, its live log within tolerance) and another within tolerance, then ask each one's status as
  /// its owner. Expect the first's loss report to name the takeover and that shortfall, registers short and the live log
  /// not, and the second's to name its takeover with no shortfall. Before, the report could not tell a takeover that
  /// lost acknowledged writes to the lost hosts from one that kept every one.
  #[test]
  fn a_takeover_past_its_tolerance_shows_in_the_volumes_loss_report() {
    crate::daemon::audit_on_shard(|state| {
      let data = slates_archive::Archive::raw_chunk(b"DATA".to_vec());
      let archive = takeover_archive(
        vec![slates_archive::Extent {
          offset: 0,
          len: 4,
          chunk: data.identity,
          chunk_offset: 0,
        }],
        vec![data.clone()],
      );
      // Shape: the store-destroyed takeover's cohort at `f = 1`, a recovery quorum of two with one member left.
      let short = slates_db::catalog::RecoveryShortfall {
        needed: 2,
        remaining: 1,
      };
      let past = slates_db::catalog::VolumeId { bytes: [0x71; 16] };
      take_over_past(
        state,
        past,
        &takeover_catalog("past-tolerance", 1 << 20),
        &archive,
        Some(slates_db::catalog::TakeoverPastTolerance {
          registers: Some(short),
          live_log: None,
        }),
      )
      .unwrap();
      let within = slates_db::catalog::VolumeId { bytes: [0x72; 16] };
      take_over(
        state,
        within,
        &takeover_catalog("within-tolerance", 1 << 20),
        &archive,
      )
      .unwrap();
      let owner = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 501,
      };
      let taken = |state: &mut crate::state::ShardState, id: slates_db::catalog::VolumeId| {
        match super::status(state, &owner, VolumeId { bytes: id.bytes }) {
          ReplyBody::Status { report } => report.loss.taken_over,
          other => panic!("the volume's status was answered: {other:?}"),
        }
      };
      let past_taken = taken(state, past).expect("the takeover past tolerance is reported");
      assert_eq!(
        past_taken.past_tolerance,
        Some(slates_ipc::protocol::PastTolerance {
          registers: Some(slates_ipc::protocol::Shortfall {
            needed: 2,
            remaining: 1
          }),
          live_log: None,
        }),
        "its registers recovered from one member of a cohort that needed two: {past_taken:?}"
      );
      let within_taken = taken(state, within).expect("the takeover within tolerance is reported");
      assert_eq!(
        within_taken.past_tolerance, None,
        "a takeover within tolerance names no shortfall: {within_taken:?}"
      );
    });
  }

  /// AUD-29-17: do: take over a volume whose catalog carries a consumer's grant, and one whose catalog
  /// requires locked RAM; expect the successor's record to keep the grant (the consumer keeps its access), and the
  /// locked volume either kept locked or refused `BudgetExceeded` with nothing published — never served as
  /// a weaker, swappable volume. Until 2026-09-30 a takeover reset both.
  #[test]
  fn a_takeover_keeps_the_volumes_grants_and_its_locked_policy() {
    crate::daemon::audit_on_shard(|state| {
      let data = slates_archive::Archive::raw_chunk(b"DATA".to_vec());
      let archive = || {
        takeover_archive(
          vec![slates_archive::Extent {
            offset: 0,
            len: 4,
            chunk: data.identity,
            chunk_offset: 0,
          }],
          vec![data.clone()],
        )
      };
      let grant = slates_db::catalog::AccessEntry {
        principal: Principal::Uid {
          host: slates_db::HostId(0),
          uid: 777,
        },
        rights: slates_db::catalog::Rights {
          read: true,
          write: true,
          admin: false,
        },
      };
      let granted = slates_db::catalog::VolumeId { bytes: [0x61; 16] };
      let catalog = crate::catalog::CatalogValue {
        access: vec![grant.clone()],
        ..takeover_catalog("granted", 1 << 20)
      };
      take_over(state, granted, &catalog, &archive()).unwrap();
      let record = state.db.partition().volume(granted).unwrap();
      assert_eq!(record.access, vec![grant], "the consumer keeps its access");
      assert!(!record.policy.require_locked);

      let locked = slates_db::catalog::VolumeId { bytes: [0x62; 16] };
      let catalog = crate::catalog::CatalogValue {
        require_locked: true,
        ..takeover_catalog("locked", 1 << 20)
      };
      match take_over(state, locked, &catalog, &archive()) {
        Ok(()) => assert!(
          state
            .db
            .partition()
            .volume(locked)
            .unwrap()
            .policy
            .require_locked,
          "served, it is served locked"
        ),
        Err(refusal) => {
          assert!(
            matches!(
              *refusal,
              super::ReplyBody::Refused {
                refusal: Refusal::BudgetExceeded { .. }
              }
            ),
            "{refusal:?}"
          );
          assert!(!state.by_id.contains_key(&locked), "nothing was published");
        }
      }
    });
  }

  /// An archive of one file `f` laid out by `extents` over `chunks`, its recorded size their tiled length.
  fn takeover_archive(
    extents: Vec<slates_archive::Extent>,
    chunks: Vec<slates_archive::Chunk>,
  ) -> slates_archive::Archive {
    let size = slates_archive::manifest::tiled_length(&extents).unwrap();
    slates_archive::Archive {
      base_page_size: 4096,
      chunk_min: 4096,
      chunk_max: 65_536,
      created_unix: 0,
      volume_id: 0,
      snapshot_id: 0,
      name_policy_id: 0,
      unicode_version: 0,
      root_meta: slates_archive::NodeMeta {
        mode: 0o040755,
        ..Default::default()
      },
      manifest: slates_archive::Node::Directory(vec![slates_archive::Entry {
        name: "f".to_owned(),
        meta: slates_archive::NodeMeta {
          mode: 0o100644,
          size,
          nlink: 1,
          ..Default::default()
        },
        node: slates_archive::Node::File(extents),
      }]),
      chunks,
    }
  }

  /// AUD-29-13, AUD-29-14, AUD-29-57 (through the successor's own materialization path): do: take over a
  /// volume whose archived file is a 4 KiB hole then 4 bytes of data; one whose file is a gibibyte hole under
  /// a 1 MiB bound; and one whose file holds two mebibytes of data under the same bound. Expect the first
  /// served with the data at its file offset (zeros before it), the second served at its
  /// gibibyte length with nothing charged — a hole costs what it cost the origin, nothing (until 2026-10-01 a
  /// dense restore refused it) — and the third refused `BudgetExceeded` before any reconstruction, with no
  /// volume published under its name.
  #[test]
  fn a_taken_over_archive_lands_at_its_offsets_and_an_oversized_one_is_refused() {
    crate::daemon::audit_on_shard(|state| {
      let data = slates_archive::Archive::raw_chunk(b"DATA".to_vec());
      let sparse = takeover_archive(
        vec![
          slates_archive::Extent {
            offset: 0,
            len: 4096,
            chunk: [0u8; 32],
            chunk_offset: 0,
          },
          slates_archive::Extent {
            offset: 4096,
            len: 4,
            chunk: data.identity,
            chunk_offset: 0,
          },
        ],
        vec![data],
      );
      let id = slates_db::catalog::VolumeId { bytes: [0x51; 16] };
      take_over(state, id, &takeover_catalog("successor", 1 << 20), &sparse).unwrap();
      let handle = *state.by_id.get(&id).unwrap();
      let slot = state.volumes.get(handle).unwrap();
      let inode = slot.volume.resolve(&state.store, "f").unwrap().inode;
      let mut bytes = vec![0xffu8; 4100];
      let read = slot
        .volume
        .read(&state.store, inode, 0, &mut bytes)
        .unwrap();
      assert_eq!(read, 4100);
      assert!(
        bytes[..4096].iter().all(|byte| *byte == 0),
        "the hole reads as zeros"
      );
      assert_eq!(&bytes[4096..], b"DATA", "the data sits at its file offset");

      let gibibyte = 1u64 << 30;
      let hole = takeover_archive(
        vec![slates_archive::Extent {
          offset: 0,
          len: gibibyte,
          chunk: [0u8; 32],
          chunk_offset: 0,
        }],
        Vec::new(),
      );
      let holed = slates_db::catalog::VolumeId { bytes: [0x53; 16] };
      take_over(state, holed, &takeover_catalog("holed", 1 << 20), &hole).unwrap();
      let slot = state
        .volumes
        .get(*state.by_id.get(&holed).unwrap())
        .unwrap();
      let inode = slot.volume.resolve(&state.store, "f").unwrap().inode;
      assert_eq!(
        slot.volume.stat(&state.store, inode).unwrap().size,
        gibibyte
      );
      assert_eq!(
        slot.volume.accounting().referenced_bytes,
        0,
        "a hole is free"
      );

      let mebibyte = slates_archive::Archive::raw_chunk(vec![
        0x5a;
        usize::try_from(
          slates_archive::format::MAX_CHUNK_BYTES
        )
        .unwrap()
      ]);
      let twice = (0..2)
        .map(|at| slates_archive::Extent {
          offset: at * mebibyte.raw_len,
          len: mebibyte.raw_len,
          chunk: mebibyte.identity,
          chunk_offset: 0,
        })
        .collect();
      let oversized = takeover_archive(twice, vec![mebibyte]);
      let other = slates_db::catalog::VolumeId { bytes: [0x52; 16] };
      let refusal = take_over(
        state,
        other,
        &takeover_catalog("too-big", 1 << 20),
        &oversized,
      )
      .unwrap_err();
      assert!(
        matches!(
          *refusal,
          super::ReplyBody::Refused {
            refusal: Refusal::BudgetExceeded { .. }
          }
        ),
        "{refusal:?}"
      );
      assert!(!state.by_id.contains_key(&other), "nothing was published");
    });
  }

  /// AC-5.2 / A-26: takeover reconstructs linked IPC names as one inode, including archived owners.
  #[test]
  fn archive_restore_preserves_ipc_hardlinks() {
    crate::daemon::audit_on_shard(|state| {
      let reply = super::dispatch(
        state,
        1,
        &Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        },
        super::RequestBody::Create {
          name: "ipc-restore".to_owned(),
          size: super::SizeClass::Bounded { limit: 1 << 20 },
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id } = reply else {
        panic!("{reply:?}")
      };
      let handle = *state.by_id.get(&super::to_db_volume(id)).unwrap();
      let slot = state.volumes.get_mut(handle).unwrap();
      let meta = slates_archive::NodeMeta {
        ino: 17,
        mode: 0o010640,
        nlink: 2,
        uid: 1234,
        gid: 456,
        mtime_ns: 789,
        ctime_ns: 890,
        ..Default::default()
      };
      let restored = slates_archive::Restored {
        files: [
          ("pipe".to_owned(), slates_archive::RestoredFile::default()),
          (
            "pipe-link".to_owned(),
            slates_archive::RestoredFile::default(),
          ),
        ]
        .into(),
        metadata: [
          ("pipe".to_owned(), meta.clone()),
          ("pipe-link".to_owned(), meta.clone()),
        ]
        .into(),
        directories: Default::default(),
        root: slates_archive::NodeMeta {
          mode: 0o040755,
          ..Default::default()
        },
        xattrs: Default::default(),
        chunks_decoded: 0,
      };
      super::populate_restored(&mut state.store, &mut slot.volume, &restored).unwrap();
      let first = slot.volume.resolve(&state.store, "pipe").unwrap().inode;
      let linked = slot
        .volume
        .resolve(&state.store, "pipe-link")
        .unwrap()
        .inode;
      assert_eq!(
        first, linked,
        "takeover retains the IPC endpoint's hard-link identity"
      );
      let attrs = slot.volume.stat(&state.store, first).unwrap();
      assert_eq!(
        (
          attrs.mode,
          attrs.uid,
          attrs.gid,
          attrs.nlink,
          attrs.mtime,
          attrs.ctime
        ),
        (0o640, 1234, 456, 2, 789, 890)
      );
      slot.volume.chmod(&mut state.store, linked, 0o600).unwrap();
      assert_eq!(slot.volume.stat(&state.store, first).unwrap().mode, 0o600);
      let before = slot
        .volume
        .to_image(&state.store, None)
        .unwrap()
        .to_content();
      let mut malformed = restored.clone();
      malformed.metadata.get_mut("pipe-link").unwrap().gid += 1;
      assert_eq!(
        super::populate_restored(&mut state.store, &mut slot.volume, &malformed),
        Err(slates_vfs::VfsError::RecoveryIncomplete)
      );
      let mut malformed = restored;
      *malformed.files.get_mut("pipe").unwrap() = slates_archive::RestoredFile {
        len: 1,
        pieces: vec![(0, vec![1])],
      };
      assert_eq!(
        super::populate_restored(&mut state.store, &mut slot.volume, &malformed),
        Err(slates_vfs::VfsError::RecoveryIncomplete)
      );
      assert_eq!(
        slot
          .volume
          .to_image(&state.store, None)
          .unwrap()
          .to_content(),
        before
      );
    });
  }

  /// Creates a bounded volume named `name` as uid 1234 on the test shard and returns its slot handle.
  fn created_volume(
    state: &mut crate::state::ShardState,
    name: &str,
  ) -> super::Handle<super::VolumeSlot> {
    let reply = super::dispatch(
      state,
      1,
      &Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      },
      super::RequestBody::Create {
        name: name.to_owned(),
        size: super::SizeClass::Bounded { limit: 1 << 20 },
        names: super::NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    let super::ReplyBody::Created { id } = reply else {
      panic!("{reply:?}")
    };
    *state.by_id.get(&super::to_db_volume(id)).unwrap()
  }

  /// The adoption a test hands an owner shard (A-333): a green with no origin, at version 0, named `name`.
  fn adoption(name: &str, owner: &Principal) -> crate::green_adopt::AdoptedGreen {
    let mut engine = slates_merge::engine::Green::new();
    let identity = engine.head_identity();
    crate::green_adopt::AdoptedGreen {
      name: name.to_owned(),
      require_evidence: false,
      owner: owner.clone(),
      access: Vec::new(),
      catalog_sequence: 1,
      write: None,
      write_version: 0,
      signers: Vec::new(),
      head: 0,
      identity,
      origin: None,
      epoch: 0,
      engine,
      completions: Vec::new(),
    }
  }

  /// §4.16 owner-loss recovery, CLAUDE.md §2 item 6 (no panics in non-test code, ever), A-333: a taken-over green whose
  /// id names a volume of another role on its owner shard is refused, and nothing is adopted over it. Do: create a plain
  /// volume, then adopt a taken-over green under its id and another name. Expect the adoption handed back with its
  /// engine, counted refused, no green engine, and the plain volume still plain and served. Before (2026-11-18), a
  /// `debug_assert!` on the volume's role panicked the shard in a debug build, and a release build went on to replay the
  /// green's chain against the plain volume's record.
  #[test]
  fn a_taken_over_green_whose_id_names_a_plain_volume_is_refused_typed() {
    let (handed_back, refused, engine, role, served) = crate::daemon::audit_on_shard(|state| {
      let owner = Principal::Uid {
        host: slates_db::HostId(0),
        uid: 1234,
      };
      let reply = super::dispatch(
        state,
        1,
        &owner,
        super::RequestBody::Create {
          name: "plain".to_owned(),
          size: super::SizeClass::Bounded { limit: 1 << 20 },
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id: wire } = reply else {
        panic!("{reply:?}")
      };
      let id = super::to_db_volume(wire);
      let answer = super::adopt_taken_over_green(state, id, adoption("green", &owner));
      (
        answer.err().map(|back| back.engine.head()),
        state
          .refusals
          .get(super::GREEN_TAKEOVER_ADOPTION_REFUSED)
          .copied(),
        state.greens.contains_key(&id),
        crate::merge_service::merge_role(state, id).is_none(),
        state.by_id.contains_key(&id),
      )
    });
    assert_eq!(
      handed_back,
      Some(0),
      "the adoption is handed back with its engine"
    );
    assert_eq!(refused, Some(1), "counted refused once");
    assert!(!engine, "no green engine over the plain volume");
    assert!(role, "the volume under the id is still plain");
    assert!(served, "the plain volume is still served");
  }

  /// §4.16 owner-loss recovery, A-333: a taken-over green is recorded on its owner shard by its adoption and served by
  /// the engine handed to it. Do: adopt a green at version 0 under a free id and ask its versions; adopt it again; drop its
  /// engine (an install that did not happen) and adopt it again; drop it again and adopt under the same id an adoption
  /// naming another identity; adopt another green under the same name. Expect the first adoption served (its versions at
  /// head 0), recorded with its adoption and no origin bytes, counted adopted once; the second a no-op; the same adoption
  /// installed again over its own record, counted; the other adoption and the other green refused and handed back.
  #[test]
  fn a_taken_over_green_is_recorded_by_its_adoption_and_served() {
    let (versions, recorded, origin_bytes, answers, adopted) =
      crate::daemon::audit_on_shard(|state| {
        let owner = Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        };
        let id = super::DbVolumeId { bytes: [0x42; 16] };
        let first = super::adopt_taken_over_green(state, id, adoption("adopted", &owner)).is_ok();
        let versions = super::dispatch(
          state,
          1,
          &owner,
          super::RequestBody::Versions {
            green: super::to_wire_volume(id),
          },
        );
        let recorded = state.db.partition().green_adoption(id).copied();
        let origin_bytes = state.db.partition().green_origin(id).is_some();
        let again = super::adopt_taken_over_green(state, id, adoption("adopted", &owner)).is_ok();
        state.greens.remove(&id);
        let reinstalled = super::adopt_taken_over_green(state, id, adoption("adopted", &owner))
          .is_ok()
          && state.greens.contains_key(&id);
        state.greens.remove(&id);
        let mut another = adoption("adopted", &owner);
        another.identity = [9; 32];
        let another = super::adopt_taken_over_green(state, id, another).is_err();
        let other = super::DbVolumeId { bytes: [0x43; 16] };
        let other =
          super::adopt_taken_over_green(state, other, adoption("adopted", &owner)).is_err();
        let adopted = state.refusals.get(super::GREEN_TAKEOVER_ADOPTED).copied();
        (
          versions,
          recorded,
          origin_bytes,
          [first, again, reinstalled, another, other],
          adopted,
        )
      });
    assert!(
      matches!(versions, ReplyBody::Versions { head: 0 }),
      "{versions:?}"
    );
    assert_eq!(
      recorded.map(|adoption| (adoption.version, adoption.origin)),
      Some((0, None)),
      "recorded by its adoption"
    );
    assert!(!origin_bytes, "no origin bytes recorded");
    assert_eq!(
      answers, [true; 5],
      "installed; a no-op again; installed over its own record; another adoption and another green refused"
    );
    assert_eq!(adopted, Some(2), "counted adopted at each install");
  }

  /// §4.16 owner-loss recovery and its failure matrix (a green owner's restart is masked), A-333: a restarted successor's
  /// green, held by adoption, waits for the engine its ledger rebuilds and serves once it has it. Do: adopt a green at
  /// version 0 and another whose adoption names another identity; leave both as a restart leaves them (recorded, no engine,
  /// nothing placed) and await their re-adoption; ask the first's versions; hand the second an engine at version 0, and the
  /// first one, and step the first's replay; ask its versions again. Expect the first ask answered `Overloaded`, counted;
  /// the engine off the second's identity not taken; the first's replay installed in one step, its versions at head 0 and
  /// version 0 placed again.
  #[test]
  fn a_green_awaiting_its_readoption_answers_overloaded_then_serves_the_engine_handed_to_it() {
    let (waiting, taken, step, versions, placed, counted) =
      crate::daemon::audit_on_shard(|state| {
        let owner = Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        };
        let (id, off) = (
          super::DbVolumeId { bytes: [0x44; 16] },
          super::DbVolumeId { bytes: [0x45; 16] },
        );
        let mut other = adoption("off", &owner);
        other.identity = [9; 32];
        let adopted = super::adopt_taken_over_green(state, id, adoption("readopted", &owner))
          .is_ok()
          && super::adopt_taken_over_green(state, off, other).is_ok();
        // What a restart leaves of each: its record and adoption, no engine and nothing placed.
        for green in [id, off] {
          state.greens.remove(&green);
          state.merge.placed.remove(&ObjectId(green.bytes));
          let record = state.db.partition().volume(green).cloned();
          assert!(
            adopted && record.is_some_and(|record| super::await_adoption(state, &record)),
            "an adopted green waits for its ledger"
          );
        }
        let wire = super::to_wire_volume(id);
        let waiting = super::dispatch(
          state,
          1,
          &owner,
          super::RequestBody::Versions { green: wire },
        );
        let taken = [
          super::begin_readoption(state, off, slates_merge::engine::Green::new()),
          super::begin_readoption(state, id, slates_merge::engine::Green::new()),
        ];
        let step = super::step_readoption(state, id);
        let versions = super::dispatch(
          state,
          1,
          &owner,
          super::RequestBody::Versions { green: wire },
        );
        let placed = crate::merge_service::placed_version(state, ObjectId(id.bytes));
        let counted = state
          .refusals
          .get(super::GREEN_AWAITING_ADOPTION_REFUSED)
          .copied();
        (waiting, taken, step, versions, placed, counted)
      });
    assert!(
      matches!(
        waiting,
        ReplyBody::Refused {
          refusal: Refusal::Overloaded { .. }
        }
      ),
      "a verb on a green awaiting its re-adoption is not started: {waiting:?}"
    );
    assert_eq!(
      taken,
      [false, true],
      "an engine off the adoption's identity is not taken; the right one is"
    );
    assert_eq!(step, super::ReadoptionStep::Installed);
    assert!(
      matches!(versions, ReplyBody::Versions { head: 0 }),
      "{versions:?}"
    );
    assert_eq!(placed, Some(0), "the adopted version is placed again");
    assert_eq!(counted, Some(1), "the refused verb counted");
  }

  /// What a client observes of the node at `path`: its kind and permission bits, owner, link count, four
  /// times and every attribute value — everything but the inode number, which is the volume's own.
  type Observed = (u32, (u32, u32, u32), [i64; 4], Vec<(Box<[u8]>, Vec<u8>)>);

  fn observe(
    store: &slates_vfs::volume::Store,
    volume: &super::Volume,
    path: &str,
  ) -> (slates_vfs::ids::InodeNo, Observed) {
    let no = volume.resolve(store, path).unwrap().inode;
    let attrs = volume.stat(store, no).unwrap();
    let values = volume
      .xattr_names(store, no)
      .unwrap()
      .into_iter()
      .map(|name| {
        let len = volume.xattr_len(store, no, &name).unwrap();
        let mut value = vec![0u8; usize::try_from(len).unwrap()];
        volume.xattr_read(store, no, &name, 0, &mut value).unwrap();
        (name, value)
      })
      .collect();
    (
      no,
      (
        attrs.mode,
        (attrs.uid, attrs.gid, attrs.nlink),
        [attrs.atime, attrs.mtime, attrs.ctime, attrs.btime],
        values,
      ),
    )
  }

  /// AUD-29-56 (§4.10 takeover, R8 one code path): do: on an origin volume give the root, a directory and a
  /// hard-linked file attributes (one empty) and the file pre-epoch access, modification and birth times;
  /// snapshot, export, encode, decode, restore and rebuild the tree in a second volume as a takeover does;
  /// expect every node — the root, the directory and both names of the file — to show the origin's mode,
  /// owner, link count, all four times and every attribute value, and both names to stay one inode.
  #[test]
  fn a_rebuilt_volume_shows_the_origins_attributes_times_and_links() {
    crate::daemon::audit_on_shard(|state| {
      let origin = created_volume(state, "origin");
      let successor = created_volume(state, "successor");
      let paths = ["", "dir", "dir/file", "link"];
      let (archive, expected) = {
        let store = &mut state.store;
        let volume = &mut state.volumes.get_mut(origin).unwrap().volume;
        let root = volume.root();
        let dir = volume.mkdir(store, root, "dir", 0o750).unwrap();
        let file = volume.create_file(store, dir, "file", 0o640).unwrap();
        volume.write(store, file, 0, b"body").unwrap();
        volume.link(store, root, "link", file).unwrap();
        let dir_no = store.dirs.get(dir).unwrap().inode;
        let root_no = volume.root_inode(store).unwrap();
        let set = slates_vfs::xattr::XattrSet::Create;
        for (no, name, value) in [
          (root_no, b"user.root".as_slice(), b"r".as_slice()),
          (dir_no, b"user.dir".as_slice(), b"d".as_slice()),
          (file, b"user.file".as_slice(), b"f".as_slice()),
          (file, b"user.empty".as_slice(), b"".as_slice()),
        ] {
          volume.xattr_set(store, no, name, value, set).unwrap();
        }
        // Pre-epoch and distinct: the nearest negative instants to zero, stepping down.
        volume
          .set_times(store, file, Some(-1), Some(-2), None, Some(-3))
          .unwrap();
        let snapshot = volume.snapshot(store).unwrap();
        let mut archiver = slates_vfs::export::SnapshotArchiver::new(
          volume,
          store,
          snapshot,
          0,
          0,
          slates_archive::CodecPolicy::raw_only(),
        )
        .unwrap();
        let archive = loop {
          if let slates_vfs::export::Progress::Done(archive) =
            archiver.advance(volume, store, u64::MAX).unwrap()
          {
            break archive;
          }
        };
        let expected: Vec<Observed> = paths
          .iter()
          .map(|path| observe(store, volume, path).1)
          .collect();
        (archive, expected)
      };
      let decoded = slates_archive::Archive::decode(&archive.encode()).unwrap();
      let restored = slates_archive::restore(&decoded, u64::MAX).unwrap();
      let store = &mut state.store;
      let volume = &mut state.volumes.get_mut(successor).unwrap().volume;
      super::populate_restored(store, volume, &restored).unwrap();
      let rebuilt: Vec<(slates_vfs::ids::InodeNo, Observed)> = paths
        .iter()
        .map(|path| observe(store, volume, path))
        .collect();
      for ((path, (_, got)), want) in paths.iter().zip(&rebuilt).zip(&expected) {
        assert_eq!(got, want, "{path:?}");
      }
      assert_eq!(rebuilt[2].0, rebuilt[3].0, "both names stay one inode");
      assert_eq!(expected[2].2, [-1, -2, expected[2].2[2], -3], "non-vacuous");
    });
  }

  /// A file's data map as a client finds it: `(data, hole)` pairs from walking `SEEK_DATA` and `SEEK_HOLE`
  /// from offset zero to its size.
  fn seek_map(
    store: &slates_vfs::volume::Store,
    volume: &super::Volume,
    no: slates_vfs::ids::InodeNo,
  ) -> Vec<(u64, u64)> {
    use slates_vfs::volume::Seek;
    let size = volume.stat(store, no).unwrap().size;
    let mut map = Vec::new();
    let mut at = 0;
    while let Some(data) = volume.seek(store, no, at, Seek::Data).unwrap() {
      let hole = volume
        .seek(store, no, data, Seek::Hole)
        .unwrap()
        .unwrap_or(size);
      map.push((data, hole));
      at = hole;
    }
    map
  }

  /// Shape: the sparse file's length in chunk windows — room for islands well apart and a trailing hole.
  const SPARSE_WINDOWS: u64 = 32;

  /// AUD-29-57 (§4.5, §4.11 sparse files; R8): do: on an origin volume truncate a file to
  /// [`SPARSE_WINDOWS`] chunk windows and write three islands — one window-aligned, one small write inside a
  /// window (holes on both sides within it), one straddling a window boundary — leaving a trailing hole;
  /// snapshot, export, encode, decode, restore and rebuild in a second volume as a takeover does. Expect the
  /// rebuilt file's bytes, length and `SEEK_DATA`/`SEEK_HOLE` map equal to the origin's, its physical charge
  /// equal too, the export to have hashed only the windows holding data (non-vacuous: a small fraction of
  /// the logical length), and the restore to carry exactly the origin's data map as pieces, no hole.
  #[test]
  fn a_sparse_file_keeps_its_holes_through_export_restore_and_rebuild() {
    crate::daemon::audit_on_shard(|state| {
      let origin = created_volume(state, "sparse-origin");
      let successor = created_volume(state, "sparse-successor");
      let chunk = u64::try_from(state.store.content.chunk_bytes()).unwrap();
      let size = SPARSE_WINDOWS * chunk;
      // Islands: window 3 whole; a few bytes in the middle of window 10; across the window 20/21 boundary.
      let islands: [(u64, u64); 3] = [
        (3 * chunk, chunk),
        (10 * chunk + chunk / 2, chunk / 8),
        (21 * chunk - chunk / 4, chunk / 2),
      ];
      let data_windows = 1 + 1 + 2;
      let (archive, expected, hashed) = {
        let store = &mut state.store;
        let volume = &mut state.volumes.get_mut(origin).unwrap().volume;
        let root = volume.root();
        let file = volume.create_file(store, root, "sparse", 0o644).unwrap();
        volume.truncate(store, file, size).unwrap();
        for (at, (offset, len)) in islands.iter().enumerate() {
          let fill = u8::try_from(at + 1).unwrap();
          let bytes = vec![fill; usize::try_from(*len).unwrap()];
          volume.write(store, file, *offset, &bytes).unwrap();
        }
        let snapshot = volume.snapshot(store).unwrap();
        let mut archiver = slates_vfs::export::SnapshotArchiver::new(
          volume,
          store,
          snapshot,
          0,
          0,
          slates_archive::CodecPolicy::raw_only(),
        )
        .unwrap();
        let archive = loop {
          if let slates_vfs::export::Progress::Done(archive) =
            archiver.advance(volume, store, u64::MAX).unwrap()
          {
            break archive;
          }
        };
        let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
        volume.read(store, file, 0, &mut bytes).unwrap();
        let expected = (
          bytes,
          seek_map(store, volume, file),
          volume.accounting().referenced_bytes,
        );
        (archive, expected, archiver.bytes_hashed())
      };
      let decoded = slates_archive::Archive::decode(&archive.encode()).unwrap();
      let restored = slates_archive::restore(&decoded, u64::MAX).unwrap();
      let restored_file = restored.files["sparse"].clone();
      let store = &mut state.store;
      let volume = &mut state.volumes.get_mut(successor).unwrap().volume;
      super::populate_restored(store, volume, &restored).unwrap();
      let file = volume.resolve(store, "sparse").unwrap().inode;
      let mut bytes = vec![0u8; usize::try_from(size).unwrap()];
      volume.read(store, file, 0, &mut bytes).unwrap();
      let rebuilt = (
        bytes,
        seek_map(store, volume, file),
        volume.accounting().referenced_bytes,
      );
      assert_eq!(rebuilt.1, expected.1, "the data map");
      assert!(rebuilt.0 == expected.0, "the bytes");
      assert_eq!(rebuilt.2, expected.2, "the physical charge");
      assert_eq!(volume.stat(store, file).unwrap().size, size, "the length");
      assert_eq!(
        hashed,
        data_windows * chunk,
        "the export hashed only the windows holding data"
      );
      assert_eq!(restored_file.len, size);
      assert_eq!(
        restored_file.data_bytes(),
        expected
          .1
          .iter()
          .map(|(data, hole)| hole - data)
          .sum::<u64>(),
        "the restore carries exactly the origin's data map, no hole"
      );
    });
  }

  /// §4.13 (the root:wheel sibling, docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md): a
  /// volume's root directory is owned by the user who provisioned it — the uid of the principal, the
  /// gid this process's effective group — not the volume core's born root:wheel. Do: create a volume
  /// as uid 1234. Expect: the root inode's uid is 1234 and its gid the process's own.
  #[test]
  fn a_created_volumes_root_is_owned_by_its_provisioning_user() {
    let (uid, gid) = crate::daemon::audit_on_shard(|state| {
      let reply = super::dispatch(
        state,
        1,
        &Principal::Uid {
          host: slates_db::HostId(0),
          uid: 1234,
        },
        super::RequestBody::Create {
          name: "owned".to_owned(),
          size: super::SizeClass::Bounded { limit: 1 << 20 },
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id } = reply else {
        panic!("{reply:?}");
      };
      let handle = *state.by_id.get(&super::to_db_volume(id)).unwrap();
      let slot = state.volumes.get(handle).unwrap();
      let root = slot.volume.root_inode(&state.store).unwrap();
      let attrs = slot.volume.stat(&state.store, root).unwrap();
      (attrs.uid, attrs.gid)
    });
    assert_eq!(uid, 1234, "the root's owner is the provisioning principal");
    assert_eq!(
      gid,
      super::creator_gid(),
      "the root's group is the provisioning user's own"
    );
  }

  /// AC-2.12 / T-2.14, AUD-05: recovery has a catalog entry but no image or retained base
  /// witnesses. Expect `RecoveryIncomplete`, never an empty scratch or a reopened live directory.
  #[test]
  fn recovery_without_content_or_base_witnesses_never_presents_empty() {
    let (scratch, overlay) = crate::daemon::audit_on_shard(|state| {
      let size = super::SizeClass::Bounded { limit: 1 << 20 };
      let reply = super::dispatch(
        state,
        1,
        &Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        super::RequestBody::Create {
          name: "recovery-proof".to_owned(),
          size,
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id } = reply else {
        panic!("{reply:?}");
      };
      let mut record = state
        .db
        .partition()
        .volume(super::to_db_volume(id))
        .unwrap()
        .clone();
      state.content = None;
      let scratch = super::build_recovered_volume(state, &record, None, None, size).err();
      record.base = super::BaseRecord::Path {
        path: ".".to_owned(),
      };
      let overlay = super::build_recovered_volume(state, &record, None, None, size).err();
      (scratch, overlay)
    });
    assert_eq!(scratch, Some(slates_ipc::protocol::RecoveryCause::NoImage));
    assert_eq!(overlay, Some(slates_ipc::protocol::RecoveryCause::NoImage));
  }

  /// AC-2.3, §4.8 (the catalog is the authority on the size policy): rebuild an image whose content exceeds the
  /// catalog's acknowledged size. Do: create a volume, write a 100-byte file into it, image it, and rebuild that image
  /// against a record whose size policy is 64 bytes. Expect: refused for the size policy, and the store's inode and
  /// directory records exactly as many as before — the half-built volume returned them
  /// (`docs/bugs/2026-10-03-a-write-or-truncate-refused-partway-dropped-the-files-body.md`, the sibling sweep).
  #[test]
  fn a_recovered_volume_over_its_acknowledged_size_is_refused_without_leaking_its_records() {
    let (refusal, before, after) = crate::daemon::audit_on_shard(|state| {
      let size = super::SizeClass::Bounded { limit: 1 << 20 };
      let reply = super::dispatch(
        state,
        1,
        &Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        super::RequestBody::Create {
          name: "over-policy".to_owned(),
          size,
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        },
      );
      let super::ReplyBody::Created { id } = reply else {
        panic!("{reply:?}");
      };
      let handle = *state.by_id.get(&super::to_db_volume(id)).unwrap();
      let super::ShardState { store, volumes, .. } = &mut *state;
      let slot = volumes.get_mut(handle).unwrap();
      let root = slot.volume.root_inode(store).unwrap();
      let file = slot.volume.create_file_no(store, root, "f", 0o644).unwrap();
      slot.volume.write(store, file, 0, &[b'x'; 100]).unwrap();
      let image = slot.volume.to_image(store, None).unwrap();
      let record = state
        .db
        .partition()
        .volume(super::to_db_volume(id))
        .unwrap()
        .clone();
      let claims = slates_vfs::recover::Claims::prepare(&mut state.store, [&image]).unwrap();
      let before = (state.store.inodes.len(), state.store.dirs.len());
      let small = super::SizeClass::Bounded { limit: 64 };
      let refusal =
        super::build_recovered_volume(state, &record, Some(&image), Some(&claims), small).err();
      let after = (state.store.inodes.len(), state.store.dirs.len());
      (refusal, before, after)
    });
    assert!(refusal.is_some_and(|cause| matches!(
      cause,
      slates_ipc::protocol::RecoveryCause::ImageRefused { reason } if reason.contains("size policy")
    )));
    assert_eq!(after, before, "the refused rebuild returned its records");
  }

  /// AC-2.12 / T-2.14, AUD-05: remove or exhaust the recovery image storage before a create.
  /// Expect a refused completion, including the retry, rather than a success lost on restart.
  #[test]
  fn a_control_verb_cannot_acknowledge_an_unpublished_image() {
    for absent in [false, true] {
      let (first, retry) = crate::daemon::audit_on_shard(move |state| {
        if absent {
          state.content = None;
        } else {
          state.content_range = (0, 1);
        }
        let id = slates_wire::request::RequestId {
          client: 1,
          sequence: 1,
        };
        let request = super::RequestBody::Create {
          name: "unpublished".to_owned(),
          size: super::SizeClass::Bounded { limit: 1 << 20 },
          names: super::NamePolicy::Exact,
          require_locked: false,
          base: None,
        };
        let first = super::run_recorded(
          state,
          1,
          id,
          1,
          &Principal::Uid {
            host: slates_db::HostId(0),
            uid: 0,
          },
          request.clone(),
        );
        let retry = super::run_forwarded(
          state,
          1,
          id,
          1,
          &Principal::Uid {
            host: slates_db::HostId(0),
            uid: 0,
          },
          request,
          None,
        );
        (first, retry)
      });
      assert!(
        matches!(first, Some(super::ReplyBody::Refused { .. })),
        "{first:?}"
      );
      assert_eq!(
        retry, first,
        "a retry must not turn a failed publish into success"
      );
    }
  }

  /// §4.9 (RIFL), the protocol's `Overloaded` ("the request was not started and may be retried" under its id), GAPS
  /// "The many-client load test from 10,000 to 100,000 clients"
  /// (`docs/bugs/2026-11-17-an-overloaded-verb-was-recorded-as-its-completion.md`): a verb answered `Overloaded` ran
  /// nothing, so its id stays unanswered and a resend under it runs the verb. It was recorded as the completion, so
  /// every resend under the id met it for good. Do: snapshot a volume this node holds a copy of and has not
  /// materialized (the takeover's settle window, answered `Overloaded`); then, the copy gone, send the same request
  /// again under its id. Expect: `Overloaded` first, counted unrecorded once; the resend runs the verb and answers what
  /// it answers now, `NotFound`, not the first answer again.
  #[test]
  fn a_resend_under_the_id_of_an_overloaded_verb_runs_the_verb() {
    let (first, resent, unrecorded) = crate::daemon::audit_on_shard(|state| {
      let owner = HostId(7);
      let volume = VolumeId {
        bytes: ObjectId::new(owner, 3).0,
      };
      let object = ObjectId(volume.bytes);
      state.holder_records.insert(
        object,
        slates_db::register::Acceptor::new(
          owner,
          slates_db::register::Authority {
            generation: 1,
            owner,
          },
        ),
      );
      let id = slates_wire::request::RequestId {
        client: 1,
        sequence: 1,
      };
      let principal = Principal::Uid {
        host: HostId(0),
        uid: 0,
      };
      let request = super::RequestBody::Snapshot { volume };
      let first = super::run_forwarded(state, 1, id, 1, &principal, request.clone(), None);
      state.holder_records.remove(&object);
      let resent = super::run_forwarded(state, 1, id, 1, &principal, request, None);
      let unrecorded = state
        .refusals
        .get(super::OVERLOADED_UNRECORDED)
        .copied()
        .unwrap_or(0);
      (first, resent, unrecorded)
    });
    assert!(
      matches!(
        first,
        Some(ReplyBody::Refused {
          refusal: Refusal::Overloaded { .. }
        })
      ),
      "a held copy not yet materialized answers Overloaded: {first:?}"
    );
    assert_eq!(
      resent,
      Some(ReplyBody::Refused {
        refusal: Refusal::NotFound
      }),
      "the resend under the same id ran the verb"
    );
    assert_eq!(
      unrecorded, 1,
      "the Overloaded answer was left unrecorded once"
    );
  }

  /// AC (§4.8 "Lookup"): a volume homed in another region is redirected there (naming that region); a volume
  /// homed in this node's own region is served locally (`None`); and a single-region fleet always serves
  /// locally — the fast path, taken before any map lookup.
  #[test]
  fn a_cross_region_volume_is_redirected_to_its_home_region() {
    let (r0, r1) = (RegionId(0), RegionId(1));
    let own_in_r0 = HostId(1);
    let creator_in_r1 = HostId(2);
    let node_regions = std::collections::BTreeMap::from([(own_in_r0, r0), (creator_in_r1, r1)]);

    // A volume created by a host in region 1, seen at a node in region 0, redirects to region 1.
    let remote = VolumeId {
      bytes: ObjectId::new(creator_in_r1, 7).0,
    };
    let two_regions = RootConfiguration::formed(vec![r0, r1]);
    assert_eq!(
      home_redirect(&two_regions, &node_regions, own_in_r0, remote),
      Some(1),
      "a volume created in another region redirects to that region"
    );

    // A volume created in this node's own region is served locally.
    let local = VolumeId {
      bytes: ObjectId::new(own_in_r0, 7).0,
    };
    assert_eq!(
      home_redirect(&two_regions, &node_regions, own_in_r0, local),
      None,
      "a locally-homed volume is served here"
    );

    // A single-region fleet always serves locally — the fast path, whatever the creator.
    let one_region = RootConfiguration::formed(vec![r0]);
    assert_eq!(
      home_redirect(&one_region, &node_regions, own_in_r0, remote),
      None,
      "a single-region fleet never redirects"
    );
  }

  /// A region-loss promotion re-homes the lost region's volumes: a volume created in a promoted-away region is
  /// redirected to its mirror, following `home_of`'s promotion chain.
  #[test]
  fn a_promoted_regions_volume_redirects_to_its_mirror() {
    let (r0, r1, r2) = (RegionId(0), RegionId(1), RegionId(2));
    let own_in_r0 = HostId(1);
    let creator_in_r2 = HostId(3);
    let node_regions = std::collections::BTreeMap::from([(own_in_r0, r0), (creator_in_r2, r2)]);
    let volume = VolumeId {
      bytes: ObjectId::new(creator_in_r2, 7).0,
    };

    let mut root = RootConfiguration::formed(vec![r0, r1, r2]);
    assert!(root.promote_region(r2, r1)); // region 2 lost, promoted to region 1
    assert_eq!(
      home_redirect(&root, &node_regions, own_in_r0, volume),
      Some(1),
      "a volume created in the promoted-away region redirects to its mirror"
    );
  }

  /// §4.13 "Principals": an attestation binds a channel only when the consumer is enrolled, not revoked,
  /// under the channel's own account, and the proof is the capability keyed over *this* channel's client
  /// id — each of the four ways it can fail is its own typed refusal, and a proof from another session
  /// (a different client id) is not this one's.
  #[test]
  fn an_attestation_verifies_only_the_enrolled_unrevoked_consumer_of_the_channels_own_account() {
    const ACCOUNT: u32 = 501;
    const OTHER_ACCOUNT: u32 = 502;
    const CLIENT: u32 = 7;
    const OTHER_CLIENT: u32 = 8;
    let secret = [0x5Au8; 32];
    let channel = Principal::Uid {
      host: slates_db::HostId(0),
      uid: ACCOUNT,
    };
    let proof = crate::landing::attest_proof(&secret, CLIENT);

    assert_eq!(
      verify_attestation(&channel, CLIENT, Some((ACCOUNT, secret, false)), &proof),
      Ok(ACCOUNT),
      "enrolled, unrevoked, same account, this channel's proof: bound"
    );
    assert_eq!(
      verify_attestation(&channel, CLIENT, None, &proof),
      Err(Refusal::ConsumerNotEnrolled),
      "no such consumer"
    );
    assert_eq!(
      verify_attestation(&channel, CLIENT, Some((ACCOUNT, secret, true)), &proof),
      Err(Refusal::ConsumerRevoked),
      "revoked by a human"
    );
    assert_eq!(
      verify_attestation(
        &channel,
        CLIENT,
        Some((OTHER_ACCOUNT, secret, false)),
        &proof
      ),
      Err(Refusal::ConsumerNotEnrolled),
      "a consumer of another account is not a way across accounts"
    );
    let captured = crate::landing::attest_proof(&secret, OTHER_CLIENT);
    assert_eq!(
      verify_attestation(&channel, CLIENT, Some((ACCOUNT, secret, false)), &captured),
      Err(Refusal::ConsumerNotEnrolled),
      "a proof captured from another session does not bind this one"
    );
    let forged = crate::landing::attest_proof(&[0u8; 32], CLIENT);
    assert_eq!(
      verify_attestation(&channel, CLIENT, Some((ACCOUNT, secret, false)), &forged),
      Err(Refusal::ConsumerNotEnrolled),
      "a proof under a guessed capability does not verify"
    );
  }
}

/// Format: the median, in parts per million (the histogram's quantile unit).
const HALF_PPM: u64 = 500_000;
/// Format: the 99th percentile, in parts per million.
const P99_PPM: u64 = 990_000;

/// Which of a shard's NFS service-time histograms a signal reads (§4.14).
#[derive(Clone, Copy)]
enum NfsTimes {
  Local,
  LocalOffCpu,
  Forwarded,
}

/// The quantile `ppm` of one of this shard's NFS service-time histograms; `None` until a call was recorded there (an
/// unmeasured time is unknown, never zero; on Windows the off-core share is never recorded, `nfs::ServiceTimes`).
fn nfs_quantile(state: &ShardState, times: NfsTimes, ppm: u64) -> Option<u64> {
  let histogram = match times {
    NfsTimes::Local => &state.nfs_service.local,
    NfsTimes::LocalOffCpu => &state.nfs_service.local_off_cpu,
    NfsTimes::Forwarded => &state.nfs_service.forwarded,
  };
  (histogram.count() > 0).then(|| histogram.quantile(ppm))
}
