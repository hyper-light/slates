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
  AttachRequest, AttachTransport, DaemonReport, Direction, Established, FleetReport,
  FreshnessBasis, GroupReport, HealthSignal, Intent, NamePolicy, PlacedState, ReadAt,
  ReadWritePolicy, Refusal, RefusalCount, ReplyBody, RequestBody, Scope, ShardReport, Signal,
  SizeClass, SnapshotId, StatusReport, UnsupportedReason, VolumeId, VolumeSummary, WorkOp,
  decode_body, encode_body, pack, unpack,
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

/// A wire principal (as `share` names one) as the catalog's.
fn to_db_principal(principal: &slates_ipc::protocol::Principal) -> Principal {
  match principal {
    slates_ipc::protocol::Principal::Uid { uid } => Principal::Uid { uid: *uid },
    slates_ipc::protocol::Principal::Consumer { account, consumer } => Principal::Consumer {
      account: *account,
      consumer: *consumer,
    },
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
fn unpin_origin(state: &mut ShardState, origin: Handle<VolumeSlot>, snapshot: SnapshotId) {
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
fn fresh_volume_id(state: &mut ShardState) -> Option<DbVolumeId> {
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
fn ids_exhausted() -> Refusal {
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
  eprintln!(
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
fn shard_of_partition(state: &ShardState, partition: u16) -> Option<u16> {
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
/// registration through cancellation; an unavailable shard cannot produce a successful bootstrap.
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
    let outcome = crate::xshard::call_within(
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
          if crate::xshard::call_within(
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
  let task = SpawnRequest::new(
    Box::pin(async move {
      let reply = promote_region_here_or_forward(region, principal).await;
      if origin == control {
        crate::state::deliver(client_index, request, reply, false);
      } else {
        let back = SpawnRequest::new(
          Box::pin(async move {
            crate::state::deliver(client_index, request, reply, false);
          }),
          None,
        );
        crate::xshard::send_back(origin, back).await;
      }
    }),
    None,
  );
  if slates_rt::registry::send_control(control, Control::Spawn(Box::new(task))).is_err() {
    return Served::Reply(refused(Refusal::NotFound));
  }
  Served::Forwarded
}

/// On the control shard, either proposes the region-loss promotion here (when this node leads the root group)
/// or forwards it to the leader this node knows, so the operator may issue `promote-region` on **any** node,
/// not only the leader (§4.8, D-14). The leader is a redirection hint ([`RootGroup::leader`]): a stale hint or
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
}

/// The volume whose **latest state** a verb serves, so it requires a confirmed owner lease (§4.8 "Leases
/// and reads"; AUD-08): a read at the live head, the current version list, a status, or a since-a-version
/// query. A read explicitly pinned to an immutable point — a green's named version, an attachment's pinned
/// view — returns `None`: it keeps its separate contract (verified content and read rights, no latest-head
/// lease). Control queries, creations of new objects, and writes (gated by durability and the merge-commit
/// wait instead) return `None` too.
fn serves_latest_state(body: &RequestBody) -> Option<VolumeId> {
  match body {
    RequestBody::Read {
      volume,
      at: ReadAt::Head,
      ..
    } => Some(*volume),
    RequestBody::Read {
      at: ReadAt::Version { .. } | ReadAt::Attachment { .. },
      ..
    } => None,
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
    RequestBody::Status { volume } => Some(*volume),
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
        }
      })
    })
    .collect()
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
/// ([`dispatch`]), the mount bridge ([`crate::nfs`]) and the daemon's test observation.
pub(crate) fn lease_verdict(state: &ShardState, object: ObjectId) -> crate::lease::LeaseVerdict {
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
/// hold their caller's requests while it stands rather than answer from a stale view. A lease refusal is
/// counted by its reason ([`lease_refusal`]).
#[cfg(unix)]
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
/// `control` is the shard this serve loop runs on (the `xshard` origin).
pub(crate) async fn serve_forward(control: u16, origin: HostId, bytes: &[u8]) -> Vec<u8> {
  let Ok(ForwardedRequest {
    principal,
    body,
    request,
    ack_up_to,
  }) = decode_body::<ForwardedRequest>(bytes)
  else {
    return Vec::new();
  };
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
  let redirect = crate::state::with_state(|state| {
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
  .flatten();
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
    crate::xshard::call_within(
      control,
      shard,
      move |s| dispatch(s, FORWARDED_CLIENT, &principal, body),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await
  } else if is_forwardable_write(&body) {
    let forwarded = crate::xshard::call_within(
      control,
      shard,
      move |s| {
        if let Some(up_to) = ack_up_to {
          prune_forwarded(s, origin_host, id.client, up_to);
        }
        // The fleet envelope carries no trace context yet: the verb's span declares its cause missing.
        run_forwarded(s, origin_host, id, 0, &principal, body, None)
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match forwarded {
      Some(Some(reply)) => Some(reply),
      // The verb deferred its acceptance to a fleet commit (AUD-11): this exchange waits for the
      // completion record the commit writes, within the same liveness budget.
      Some(None) => await_deferred_completion(control, shard, origin_host, id).await,
      None => None,
    }
  } else {
    Some(refused(Refusal::Unsupported {
      feature: "forwarded verb".to_owned(),
    }))
  };
  encode_body(&reply.unwrap_or_else(|| refused(Refusal::NotFound)))
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
    let recorded = crate::xshard::call_within(
      control,
      shard,
      move |s| match s
        .db
        .partition()
        .completion(origin_host, id.client, id.sequence)
      {
        Seen::Completed(bytes) => Some(bytes),
        Seen::Acknowledged | Seen::New => None,
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
/// origin relayed, the same way a local client's are pruned by its acknowledgements — so forwarded
/// completions do not grow unbounded (§4.9 RIFL; banned item 8). A no-op when the watermark has not advanced,
/// so a stream of forwarded writes at a steady watermark writes no acknowledgement records.
fn prune_forwarded(state: &mut ShardState, origin: u64, client: u32, up_to: u32) {
  let advances = state
    .db
    .partition()
    .acknowledged_up_to(origin, client)
    .is_none_or(|current| up_to > current);
  if !advances {
    return;
  }
  let now = state.clock.monotonic_ns();
  // Retried by the next advance; a refusal is counted, never silent.
  let recorded = state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      origin,
      client,
      up_to,
    },
    now,
  );
  count_secondary(state, recorded);
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
/// executes a write. A transport failure invalidates the hint and returns HomedElsewhere;
/// retrying the original request id remains the caller's choice and meets its completion record.
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
  let task = SpawnRequest::new(
    Box::pin(async move {
      let (replies, route) = resolve_and_forward(volume, region, cached, requests).await;
      deliver_owner_forward(origin, control, (client, request), replies, (route, plan)).await;
    }),
    None,
  );
  if slates_rt::registry::send_control(control, Control::Spawn(Box::new(task))).is_err() {
    if let Ok(slot) = state.clients.get_mut(client) {
      slot.read_ahead_pending = slot.read_ahead_pending.saturating_sub(charged);
      state.read_ahead.credit(charged);
    }
    return Served::Reply(refused(Refusal::HomedElsewhere { region }));
  }
  Served::Forwarded
}

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
  /// The delivery rate, bytes per second, the fetch that filled this was received at ([`delivery_rate`]); zero
  /// before one is measured. The next batch carries no more than this rate moves in one liveness budget.
  rate: u64,
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
          rate: ahead.rate,
        },
      )
    });
    release_read_ahead(read_ahead, slot);
    continued
  });
  let rate = previous.as_ref().map_or(0, |continued| continued.rate);
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
    rate,
    charged,
  };
  (bodies, Some(plan))
}

/// Format: nanoseconds per second, for rates in bytes per second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The window a read ended its last fetch with, when its next request continues where that window ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Continued {
  window: u64,
  batch: u64,
  total: u64,
  rate: u64,
}

/// The window size and the windows a read's next fetch at `offset` asks for: one window of `own_window` bytes for a
/// read that does not continue a window; for one that does, twice its last batch of that window's size, but no more
/// windows than are left in the file, and — once a delivery rate is measured — no more than that rate moves in one
/// liveness budget, so a batch holds the owner's session no longer than one forward is already allowed to; at least
/// one window.
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
  let in_budget = if continued.rate > 0 {
    u128::from(continued.rate)
      .saturating_mul(u128::from(crate::daemon::LIVENESS_BUDGET_NS))
      .checked_div(u128::from(NANOS_PER_SECOND).saturating_mul(u128::from(window)))
      .and_then(|windows| u64::try_from(windows).ok())
      .unwrap_or(u64::MAX)
      .max(1)
  } else {
    u64::MAX
  };
  (
    window,
    continued.batch.saturating_mul(2).min(left).min(in_budget),
  )
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
    rate,
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
    rate,
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
fn forwarded_request(
  state: &ShardState,
  request: u64,
  principal: Principal,
  body: RequestBody,
) -> Vec<u8> {
  let ack_up_to = state
    .db
    .partition()
    .acknowledged_up_to(state.origin_anchor.0, RequestId::from_word(request).client);
  encode_body(&ForwardedRequest {
    principal,
    body,
    request,
    ack_up_to,
  })
}

/// On the control shard, where the fleet's sessions live: finds `volume`'s owner in `region` (the cached route,
/// the live creator, else the read-only location round) and forwards `requests` to it once — one verb, or a
/// read-ahead batch of windows, sent together. Returns the owner's reply to the first, or
/// `HomedElsewhere` when no owner was found or none answered, with the decoded replies to the rest that came back
/// (in order, up to the first that did not), and the route to cache when the owner served.
async fn resolve_and_forward(
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
        rate: 0,
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
        rate: 0,
      },
      None,
    );
  };
  let owner = match known {
    Some(owner) => {
      crate::state::with_state(|state| {
        state.count("fleet.owner_location.direct", 1);
      });
      Some(owner)
    }
    None => crate::owner_location::locate(query).await.ok(),
  };
  let bytes = match owner {
    Some(owner) => {
      // One liveness budget, plus the measured round-trip tail of the path to the owner: a WAN forward's reply is
      // never refused for crossing a path the local budget did not allow for.
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
      )
      .await;
      if forwarded.is_none() {
        crate::state::with_state(|state| {
          state.count("fleet.owner_location.forward_unsent", 1);
        });
      }
      forwarded
    }
    None => None,
  };
  // A forward sent but unanswered within its budget comes back empty, and it is counted apart from one never
  // sent: an owner that was reached but did not answer in time is not one that could not be reached.
  let batch = bytes.unwrap_or_default();
  let rate = delivery_rate(batch.streamed_bytes, batch.streamed_ns);
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
  let reply = decoded
    .flatten()
    .unwrap_or_else(|| refused(Refusal::HomedElsewhere { region }));
  let route = route_after(&reply, owner, query);
  (
    OwnerReplies {
      first: reply,
      later,
      rate,
    },
    route,
  )
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

/// What an owner answered a forward: the reply to its first request, the decoded replies to the rest of a batch
/// (in order, up to the first that did not come back), and the delivery rate the batch was received at
/// ([`delivery_rate`]).
pub(crate) struct OwnerReplies {
  first: ReplyBody,
  later: Vec<ReplyBody>,
  rate: u64,
}

/// The rate, bytes per second, a batch streamed at: the bytes the session consumed over the interval they were
/// arriving in ([`slates_cluster::BatchReplies`]) — delivery timed between deliveries, as BBR times it between
/// acknowledgements (Cardwell et al., ACM Queue 2016), so neither the request's round trip nor the wait for the
/// first byte, the fixed cost a batch exists to amortize, reads as a slow path, however the owner interleaves the
/// windows' streams. Zero — no rate measured — when nothing streamed over a measurable interval: the next batch is
/// then capped by nothing but the file and the ledger, and measures.
fn delivery_rate(streamed_bytes: u64, streamed_ns: u64) -> u64 {
  u128::from(streamed_bytes)
    .saturating_mul(u128::from(NANOS_PER_SECOND))
    .checked_div(u128::from(streamed_ns))
    .and_then(|rate| u64::try_from(rate).ok())
    .unwrap_or(0)
}

/// The region and volume of a verb this node must send to another host of its own region (§4.8 "Lookup": a lookup
/// by id routes to the volume's creator, or its successor after a takeover): a volume-scoped read or write that
/// forwards safely, whose volume this shard's catalog does not hold, and whose id names another creator. Asked on
/// the volume's owner shard — the one shard whose catalog would hold it — and only for a local client's verb, never
/// one forwarded from another node, so a forward is one hop. A volume this node created and no longer holds stays
/// `NotFound` (its successors after a same-id re-admission are GAP-A9-7's ledger transfer). A laptop never forwards:
/// every volume it can name, it created.
fn owned_by_another_host(state: &ShardState, body: &RequestBody) -> Option<(u64, VolumeId)> {
  if !(is_forwardable_read(body) || is_forwardable_write(body)) {
    return None;
  }
  let volume = volume_of(body)?;
  let creator = ObjectId(volume.bytes).creator();
  if creator == state.fleet.host() || state.db.partition().volume(to_db_volume(volume)).is_some() {
    return None;
  }
  let region = state
    .node_regions
    .get(&state.fleet.host())
    .copied()
    .unwrap_or(RegionId(0));
  Some((region.0, volume))
}

/// A late lookup belongs to the original client generation. It cannot populate a reused slot's
/// cache or deliver a predecessor's result to the client that now occupies that slot.
/// Carries a forwarded verb's reply from the control shard to the client's shard `origin` and finishes it there
/// ([`finish_owner_forward`]): at once on the same shard, else as a task sent home ([`crate::xshard::send_back`]).
async fn deliver_owner_forward(
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
  let back = SpawnRequest::new(
    Box::pin(async move {
      finish_owner_forward(client, request, reply, after);
    }),
    None,
  );
  crate::xshard::send_back(origin, back).await;
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

/// Serves a client request: recover a previous completion or dispatch the operation to its owner
/// shard. Scatter operations record their completion after every shard has answered.
pub fn serve(state: &mut ShardState, client: Handle<ClientSlot>, request: &Request) -> Served {
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
    Seen::New => {
      // A retry of a submit whose acceptance still waits for its commit joins the wait: it is answered
      // with the committed result when the version places, never a success from memory. A retry of a
      // granted landing still running joins it the same way (AUD-29-03).
      if crate::merge_service::join_awaiting(state, origin, id)
        || crate::landing::join_in_flight(state, origin, id)
        || crate::lease_wait::join(state, origin, id)
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
  if let Some(reply) = status_on_channel(state, client, request.request, &body) {
    return reply;
  }
  if let RequestBody::Attest { consumer, proof } = body {
    return attest_on_channel(
      state,
      client.index(),
      request.request,
      client_id,
      consumer,
      proof,
    );
  }
  if let RequestBody::Revoke { consumer, proof } = body {
    return revoke_on_channel(state, client.index(), request.request, consumer, proof);
  }
  if let RequestBody::List = body {
    return scatter_list(state, client.index(), request.request, principal);
  }
  if let RequestBody::Grants = body {
    return scatter_grants(state, client.index(), request.request, principal);
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
  if let RequestBody::Acknowledge { up_to } = body {
    return scatter_acknowledge(state, client.index(), request.request, client_id, up_to);
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
    RequestBody::Create { name, .. } => Some(owner_of_name(name, state.shards.len())),
    RequestBody::Detach { attachment }
    | RequestBody::Advance { attachment, .. }
    | RequestBody::BindMount { attachment, .. } => Some(owner_of_attachment(*attachment)),
    // A grant is served where its landing was presented: the volume's owner shard, which the landing id
    // names — not the client's shard, where nothing is awaiting.
    RequestBody::Grant { landing, .. } => Some(owner_of_landing(*landing)),
    // A telemetry drain names its shard: it runs there, on that shard's own ring (§4.14).
    RequestBody::Telemetry { partition } => Some(*partition),
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
const FORWARDED_CLIENT: u32 = 0;

/// The most bytes one read-ahead window carries (`RequestBody::ReadWindow`): as many pages as a client may hold in
/// flight on its ring, half its slots (the half `slates_client` acknowledges at), so a window replaces the round
/// trips the client's own requests would have taken, and the origin's cache of it stays within what one client could
/// already make it hold.
fn read_window_bytes(state: &ShardState) -> u64 {
  crate::merge_service::page_room()
    .saturating_mul((u64::from(state.config.region.slots) / 2).max(1))
}

/// The completion records one client may hold unacknowledged on a shard (§4.9 "Exactly-once"; banned item 8). A
/// conforming client holds at most its ring's slots in flight, acknowledges every half ring of replies
/// (`slates_client` `ack_every`: slots / 2), and may lose one acknowledgement sent unawaited before the next is due:
/// `slots + slots / 2 + slots / 2`. Past it a new request is refused `AcknowledgementOwed`, never run or recorded,
/// so a client that never acknowledges cannot grow a shard's records, and every snapshot that encodes them, without
/// bound (RIFL, Lee et al., SOSP 2015, bounds its completion records the same way, by the client's acknowledgements).
fn completion_bound(state: &ShardState) -> u64 {
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
  // A volume another host of this region owns goes to it (§4.8 "Lookup"), before any completion is recorded here.
  if let Some((region, volume)) = owned_by_another_host(state, &body) {
    return forward_to_owner(state, client, request, principal, region, volume, body);
  }
  if let Some(owed) = acknowledgement_owed(state, origin, id.client, &body) {
    return Served::Reply(owed);
  }
  let reply = run_recorded(state, origin, id, client_id, &principal, body);
  // A change this verb was refused for a delegation asked for a recall; it is sent now, after the verb's own
  // transaction (A-79).
  #[cfg(unix)]
  crate::delegation::drain(state);
  match reply {
    Some(reply) => Served::Reply(reply),
    // The verb deferred its reply to a fleet commit (AUD-11): it comes back through `deliver`.
    None => Served::Forwarded,
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
  let reply = dispatch(state, client_id, principal, body);
  state.current_request = None;
  state.reply_route = None;
  // A verb that deferred its acceptance to a fleet commit (`submit` at `f > 0`; AUD-11) commits its
  // effects now but records no completion and gets no reply here: both follow the commit at the
  // quorum (`merge_service::resolve_accepted`), so a retry meanwhile joins the wait, never reads a
  // success the quorum has not committed.
  let deferred = std::mem::take(&mut state.acceptance_deferred);
  let reply = if deferred {
    reply
  } else {
    record_completion(state, origin, id, reply)
  };
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
  let end_ns = state.clock.monotonic_ns();
  state.current_span = outer;
  let partition_label = u32::from(state.partition);
  crate::telemetry::emit(state, op.end(label, end_ns));
  crate::telemetry::emit(state, append.end(partition_label, end_ns));
  if deferred && !matches!(reply, ReplyBody::Refused { .. }) {
    None
  } else {
    Some(reply)
  }
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
    Seen::New => {
      // A retry of a submit whose acceptance still waits for its commit joins the wait, as does a retry
      // of a granted landing still running (AUD-29-03).
      if crate::merge_service::join_awaiting(state, origin, id)
        || crate::landing::join_in_flight(state, origin, id)
        || crate::lease_wait::join(state, origin, id)
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
  #[cfg(unix)]
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
  let now = state.clock.monotonic_ns();
  let record = Op::CompletionRecorded {
    record: CompletionRecord {
      origin,
      client: id.client,
      sequence: id.sequence,
      result: reply.to_bytes(),
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
  match send_forward(
    state.shard,
    client_index,
    request,
    client_id,
    &principal,
    &body,
    owner,
    cause,
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
      });
      Served::Forwarded
    }
    Err(_) => Served::Reply(refused(Refusal::NotFound)),
  }
}

/// Spawns the owner's task for one forward; the reply comes back as a task on `origin`. `cause` is the
/// origin's `ring.request` span context, the cause of the verb's `shard.op` on the owner (§4.14).
#[allow(clippy::too_many_arguments)] // a forward's whole identity: where from, who, what, where to, and its cause
fn send_forward(
  origin: u16,
  client_index: u32,
  request: u64,
  client_id: u32,
  principal: &Principal,
  body: &RequestBody,
  owner: u16,
  cause: Option<SpanContext>,
) -> Result<(), slates_rt::RtError> {
  let principal = principal.clone();
  let body = body.clone();
  let task = SpawnRequest::new(
    Box::pin(async move {
      let id = RequestId::from_word(request);
      let mut elsewhere = None;
      let reply = crate::state::with_state(|s| {
        s.last_work_ns = s.clock.monotonic_ns();
        // A volume another host of this region owns goes to it (§4.8 "Lookup"), before any completion is
        // recorded here: from the control shard, which holds the fleet's sessions.
        if let Some((region, volume)) = owned_by_another_host(s, &body) {
          let control = s.shards.first().copied();
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
        });
        // A same-node cross-shard forward serves a local client, so its completion keys on this node's stable
        // cert-anchor (the same id the local `serve` path uses — task #22), not the ephemeral member id.
        let origin = s.origin_anchor.0;
        run_forwarded(s, origin, id, client_id, &principal, body, cause)
      })
      .unwrap_or_else(|| Some(refused(Refusal::NotFound)));
      if let Some((region, volume, control, request_bytes)) = elsewhere {
        forward_for_local_client(
          origin,
          control,
          (client_index, request),
          region,
          volume,
          request_bytes,
        )
        .await;
        return;
      }
      // A verb that deferred its reply to a fleet commit (AUD-11) answers through the merge plane.
      let Some(reply) = reply else {
        return;
      };
      let back = SpawnRequest::new(
        Box::pin(async move {
          crate::state::deliver(client_index, request, reply, true);
        }),
        None,
      );
      crate::xshard::send_back(origin, back).await;
    }),
    None,
  );
  slates_rt::registry::send_control(owner, Control::Spawn(Box::new(task)))
}

/// Sends a local client's verb, run on its volume's owner shard here, to the volume's owner host over the fleet,
/// from the control shard, and delivers the reply to the client's ring on `origin`. A control channel that refuses
/// the task, or no control shard, answers `HomedElsewhere`, as an owner not found does. No route is cached: the
/// client's slot lives on `origin`, and a cached route is only a hint.
async fn forward_for_local_client(
  origin: u16,
  control: Option<u16>,
  (client_index, request): (u32, u64),
  region: u64,
  volume: VolumeId,
  request_bytes: Vec<u8>,
) {
  let deliver_home = move |reply: ReplyBody| {
    SpawnRequest::new(
      Box::pin(async move {
        crate::state::deliver(client_index, request, reply, true);
      }),
      None,
    )
  };
  let sent = control.is_some_and(|control| {
    let task = SpawnRequest::new(
      Box::pin(async move {
        let (replies, _) = resolve_and_forward(volume, region, None, vec![request_bytes]).await;
        let reply = replies.first;
        crate::xshard::send_back(origin, deliver_home(reply)).await;
      }),
      None,
    );
    slates_rt::registry::send_control(control, Control::Spawn(Box::new(task))).is_ok()
  });
  if !sent {
    crate::xshard::send_back(
      origin,
      deliver_home(refused(Refusal::HomedElsewhere { region })),
    )
    .await;
  }
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
      pending.cause,
    ) {
      Ok(()) => any = true,
      Err(slates_rt::RtError::ControlFull { .. }) => {
        state.pending_forwards.push_front(pending);
        break;
      }
      Err(_) => {
        // A forward that never reached its owner: refused here, its `ring.request` span ending with the
        // refusal's write like any deferred reply's.
        let ring = state.forwarded_rings.remove(&pending.request);
        state.deferred.push(Deferred {
          client_index: pending.client_index,
          request: pending.request,
          reply: refused(Refusal::NotFound),
          recorded: false,
          ring,
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
        crate::xshard::send_back(origin, back).await;
      }),
      None,
    );
    if slates_rt::registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
      gather(request, Vec::new());
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
) -> Served {
  let others: Vec<u16> = state
    .shards
    .iter()
    .copied()
    .filter(|s| *s != state.shard)
    .collect();
  let mine = match crate::landing::grants_verb(state, &principal) {
    ReplyBody::Grants { grants } => grants,
    other => return Served::Reply(other),
  };
  if others.is_empty() {
    return Served::Reply(ReplyBody::Grants { grants: mine });
  }
  state
    .grant_scatters
    .insert(request, (client_index, others.len(), mine));
  let origin = state.shard;
  for shard in others {
    let principal = principal.clone();
    let task = SpawnRequest::new(
      Box::pin(async move {
        let part = match crate::state::with_state(|s| crate::landing::grants_verb(s, &principal)) {
          Some(ReplyBody::Grants { grants }) => grants,
          _ => Vec::new(),
        };
        let back = SpawnRequest::new(
          Box::pin(async move {
            gather_grants(request, part);
          }),
          None,
        );
        crate::xshard::send_back(origin, back).await;
      }),
      None,
    );
    if slates_rt::registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
      gather_grants(request, Vec::new());
    }
  }
  Served::Forwarded
}

/// Folds one shard's part of a `grants` read into the scatter and, on the last part, delivers the merged
/// grants in id order.
fn gather_grants(request: u64, part: Vec<slates_ipc::protocol::GrantSummary>) {
  let done = crate::state::with_state(|s| {
    let entry = s.grant_scatters.get_mut(&request)?;
    entry.2.extend(part);
    entry.1 = entry.1.saturating_sub(1);
    if entry.1 == 0 {
      s.grant_scatters.remove(&request)
    } else {
      None
    }
  })
  .flatten();
  if let Some((client_index, _, mut grants)) = done {
    grants.sort_by_key(|g| g.id);
    crate::state::deliver(client_index, request, ReplyBody::Grants { grants }, false);
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
        let back = SpawnRequest::new(
          Box::pin(async move {
            gather_status(request, part);
          }),
          None,
        );
        crate::xshard::send_back(origin, back).await;
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
        crate::xshard::send_back(origin, back).await;
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
  }
}

/// The NFS calls this shard served, by NFSv3 procedure (`nfs::ServiceTimes::calls`); empty off Unix, where no NFS
/// bridge runs.
fn nfs_calls_of(state: &ShardState) -> Vec<u64> {
  #[cfg(unix)]
  {
    state.nfs_service.calls.to_vec()
  }
  #[cfg(not(unix))]
  {
    let _ = state;
    Vec::new()
  }
}

/// Whether `state` is the control shard's — the first of the daemon's shards, the one that runs the
/// membership loop and drives the consensus groups (`Daemon::council_leads` observes the same shard).
fn is_control_shard(state: &ShardState) -> bool {
  state.shards.first() == Some(&state.shard)
}

/// The regional council's status line ([`group_report`]).
fn council_report(state: &ShardState) -> GroupReport {
  group_report(
    state.council.is_leader(),
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
    state.root_timing,
    state.root.election_view(),
    state
      .root
      .election_rank(&crate::fleet::authenticated_alive(state)),
  )
}

/// A consensus group's status line: whether this node leads it, the election timing it derived, and its
/// election state — term, priority, lease and campaigns — with its `rank` among the voters it holds alive, as
/// its drive ranks it.
fn group_report(
  leads: bool,
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
  }
}

/// The daemon's place in its fleet (§4.8), from this shard's placement authority — every shard's
/// `FleetNode` advances identically (the control shard hands it each peer state it folds) — with the
/// formed probe sessions summed over the shards' parts (only the control shard forms any) and the
/// consensus groups' state taken from the control shard's part (the only live one).
fn fleet_report(state: &ShardState, shards: &[ShardReport]) -> FleetReport {
  let configuration = state.fleet.configuration();
  let control = shards.iter().find(|shard| shard.control);
  let council = control.map_or_else(|| council_report(state), |shard| shard.council.clone());
  let root = control.map_or_else(|| root_report(state), |shard| shard.root.clone());
  let takeover = control.map_or_else(|| takeover_report(state), |shard| shard.takeover.clone());
  let detector = control.map_or_else(|| detector_report(state), |shard| shard.detector.clone());
  let sessions = control.map_or_else(Vec::new, |shard| shard.sessions.clone());
  let detector_granularity_ns = control.map_or_else(
    || detector_granularity_ns(state),
    |shard| shard.detector_granularity_ns,
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

pub(crate) fn refusal_name(r: &Refusal) -> &'static str {
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

pub(crate) fn dispatch(
  state: &mut ShardState,
  client_id: u32,
  principal: &Principal,
  body: RequestBody,
) -> ReplyBody {
  let republish = mutates_shard_image(&body);
  if republish && !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
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
  let body = match lease_gate(state, client_id, principal, body) {
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
  // A fenced green (AUD-29-18) is never served: its recovered history is not what was acknowledged. The one
  // verb it takes is an admin's `Destroy` — the reviewed release; its durable log is the evidence until then.
  let fenced = touched.filter(|volume| state.fenced_greens.contains_key(volume));
  let destroying = matches!(body, RequestBody::Destroy { .. });
  if fenced.is_some() && !destroying {
    state.count(GREEN_FENCED, 1);
    return refused(Refusal::ContentUnavailable);
  }
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
    RequestBody::ChangedSince { green, version } => changed_since(state, principal, green, version),
    RequestBody::CreateWork { green, name } => create_work(state, principal, green, &name),
    RequestBody::Edit {
      work,
      path,
      at,
      delete_len,
      bytes,
    } => edit(state, principal, work, &path, at, delete_len, &bytes),
    RequestBody::Declare { work, op } => declare(state, principal, work, op),
    RequestBody::Submit { work, evidence } => submit(state, principal, work, evidence),
    RequestBody::Rebase { work } => rebase(state, principal, work),
    RequestBody::Advance {
      attachment,
      version,
    } => crate::merge_service::advance(state, principal, attachment, version),
    RequestBody::Read { volume, path, at } => {
      crate::merge_service::read(state, principal, volume, &path, at)
    }
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
    RequestBody::Attach {
      volume,
      snapshot,
      intent,
      form,
    } => attach(state, client_id, principal, volume, snapshot, intent, form),
    RequestBody::Detach { attachment } => detach(state, principal, attachment),
    RequestBody::BindMount { attachment, path } => bind_mount(state, principal, attachment, &path),
    RequestBody::Resize { volume, size } => resize(state, principal, volume, size),
    RequestBody::Destroy { volume } => destroy(state, principal, volume),
    RequestBody::Status { volume } => status(state, principal, volume),
    RequestBody::List => list(state, principal),
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
    RequestBody::Grants => crate::landing::grants_verb(state, principal),
    RequestBody::Audit { since } => crate::landing::audit_verb(state, since),
    RequestBody::Acknowledge { up_to } => acknowledge(state, client_id, up_to),
    RequestBody::ReadBase { volume, path } => read_base(state, principal, volume, &path),
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
    RequestBody::Enroll { account, proof } => enroll(state, account, proof),
    RequestBody::Share {
      volume,
      principal: subject,
      rights,
    } => share(state, principal, volume, &subject, rights),
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
/// not verify: an agent cannot enroll itself.
fn enroll(state: &mut ShardState, account: u32, proof: Capability) -> ReplyBody {
  let expected = crate::landing::enroll_proof(&state.issuer_secret, account);
  if !crate::landing::constant_time_eq(&expected, &proof) {
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
/// so the human retries and the retry is a no-op mark.
fn revoke_on_channel(
  state: &mut ShardState,
  client_index: u32,
  request: u64,
  consumer: u64,
  proof: Capability,
) -> Served {
  let expected = crate::landing::revoke_proof(&state.issuer_secret, consumer);
  if !crate::landing::constant_time_eq(&expected, &proof) {
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
/// Returns the reply to deliver. Each call is bounded by the liveness budget; one that does not answer
/// refuses `NotFound` (the record, if written, stands for the retry).
async fn revoke_everywhere(origin: u16, owner: u16, shards: &[u16], consumer: u64) -> ReplyBody {
  let recorded = crate::xshard::call_within(
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
  for shard in shards.iter().copied() {
    let marked = crate::xshard::call_within(
      origin,
      shard,
      move |s| mark_revoked(s, consumer),
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
    match marked {
      None => return refused(Refusal::NotFound),
      Some(Err(refusal)) => return refused(refusal),
      Some(Ok(())) => {}
    }
  }
  ReplyBody::Revoked
}

/// Marks every slot on this shard bound to `consumer` revoked, so its next verb's gate — one local read
/// in `serve` — refuses before any effect (banned item 10: no cross-shard call on a write path).
fn mark_revoked(s: &mut ShardState, consumer: u64) -> Result<(), Refusal> {
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
  for record in held {
    end_attachment(s, &record, Ending::Otherwise).map_err(|e| refusal_of_db(&e))?;
  }
  Ok(())
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
  let (_, record) = match find(state, volume) {
    Ok(x) => x,
    Err(r) => return *r,
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
  let same_account = matches!(channel_principal, Principal::Uid { uid } if *uid == account);
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
    let facts: Option<ConsumerFacts> = crate::xshard::call_within(
      origin,
      owner,
      move |s| {
        s.db
          .partition()
          .consumer(consumer)
          .map(|r| (r.account, r.secret, r.revoked))
      },
      crate::daemon::LIVENESS_BUDGET_NS,
    )
    .await;
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
              slot.principal = Principal::Consumer { account, consumer };
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

fn entry_allowance(size: SizeClass) -> u64 {
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
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        report_first_budget_refusal(state, "bytes", limit, available);
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
  let inode_alloc = inode_allowance(state, size);
  let version_credit = match state.store.versions.reserve(inode_alloc) {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      report_first_budget_refusal(state, "versions", inode_alloc, available);
      return give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BudgetExceeded { available },
      );
    }
    Err(e) => {
      return give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BadRequest {
          reason: e.to_string(),
        },
      );
    }
  };
  // The volume's records — its journal budget, its object, its snapshot slab's first segment — are
  // reserved against the shard's metadata ledger before anything is allocated (§4.2 metadata
  // dimension: "an uncharged heap allocation cannot sit outside the bound").
  let quota = quota_for(size);
  let journal_bytes = journal_bytes_for(state, &quota);
  let metadata_credit = match reserve_metadata(state, journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => return give_back(state, reservation, version_credit, None, refusal),
  };
  // A strict volume's content lives in locked RAM (§4.2 D-12, BUG-1): its whole entitlement is reserved against the
  // process's lock capacity now, refused `BudgetExceeded` if it does not fit, so a strict guarantee never silently
  // becomes swappable service; its blocks are then locked as it allocates them, and only its blocks.
  let lock_credit = match require_locked
    .then(|| reserve_locked(state, size))
    .transpose()
  {
    Ok(credit) => credit,
    Err(refusal) => return give_back(state, reservation, version_credit, metadata_credit, refusal),
  };
  let config = volume_config(state, names, quota);
  let (mut volume, host) = match base {
    None => match Volume::create(&mut state.store, config) {
      Ok(v) => (v, None),
      Err(e) => {
        return give_back(
          state,
          reservation,
          version_credit,
          metadata_credit,
          refusal_of_vfs(&e),
        );
      }
    },
    Some(path) => match open_base(state, path, config) {
      Ok(pair) => pair,
      Err(reply) => {
        return give_back(
          state,
          reservation,
          version_credit,
          metadata_credit,
          reply_refusal(*reply),
        );
      }
    },
  };
  volume.set_locked(require_locked);
  // The volume root is owned by its provisioning user (§4.13 "runs as the mounting user"; the
  // root:wheel sibling, docs/bugs/2026-09-14-volume-root-owned-by-root-wheel.md): the volume core
  // births it uid 0, gid 0, which a mount showed as root:wheel — and which the NFS export's POSIX
  // permission checks would now refuse the mounting user its own volume's root.
  if let Err(e) = stamp_root_owner(&mut state.store, &mut volume, principal) {
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    );
  }
  // Admit the volume's inode dimension (§4.2 resource vector): set the per-volume cap that
  // `next_no` enforces, to the same allowance already reserved against the version slab above.
  if let Err(e) = admit_dimensions(state, &mut volume, size) {
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    );
  }
  let Some(id) = fresh_volume_id(state) else {
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
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

/// Records a freshly-created volume and moves it into the shard's registry, or gives its credits back
/// and discards the volume (returning its slab slots) if the record cannot be written or the registry
/// has no room. The registry room is checked before the insert, since a full registry's `insert`
/// consumes and drops the slot.
fn publish_created_volume(
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
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
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
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      Refusal::BadRequest {
        reason: format!("volume registry full at {full}"),
      },
    );
  }
  let mut volume = volume;
  crate::content_cipher::key_volume(&mut state.store, &mut volume, id.bytes);
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
    // Unreachable given the room check above; the slot was consumed, so only the credits return.
    Err(e) => give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
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
  if let Some(r) = reservation {
    state.store.budget.release(r);
  }
  if let Some(c) = version {
    state.store.versions.release(c);
  }
  if let Some(m) = metadata {
    state.store.metadata.release(m);
  }
  refused(refusal)
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
    Err(e) => Err(Refusal::BadRequest {
      reason: e.to_string(),
    }),
  }
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
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Snapshot) {
    Ok(x) => x,
    Err(r) => return *r,
  };
  if !rights_of(&record, principal).write {
    return forbidden("snapshot");
  }
  // The volume's next epoch, checked before any effect: an epoch with no successor refuses the snapshot
  // rather than repeat one (AUD-29-26's sibling; reachable only through a maximal recorded epoch).
  let Some(next_epoch) = record.epoch.checked_add(1) else {
    return refused(Refusal::Unsupported {
      feature: VOLUME_EPOCH_EXHAUSTED.to_owned(),
    });
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
      return refused(Refusal::BarrierIncomplete {
        attachment: catalog_attachment_of(state, attachment),
        generation,
      });
    }
    Err(e) => return refused(refusal_of_vfs(&e)),
  };
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
      return refused(refusal_of_db(&e));
    }
  }
  ReplyBody::Snapshotted { id, coverage }
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
  // The chain starts from scratch or from a complete immutable base (§4.16; A-9): the base is
  // walked into the origin before anything is recorded, so a refusal (an incomplete snapshot, a
  // torn read) changes nothing.
  let origin = match base {
    None => None,
    Some(base) => match crate::merge_service::seed_origin(state, principal, base) {
      Ok(origin) => Some(origin),
      Err(refusal) => return refused(refusal),
    },
  };
  let Some(id) = fresh_volume_id(state) else {
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
    catalog_version: 0,
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    return refused(refusal_of_db(&e));
  }
  // A base-seeded green records its origin durably — guarded (the chain budget) before the engine
  // is seeded, so a refused origin leaves no engine ahead of its log — and replays it before the
  // chain on recovery (`rebuild_green`).
  let engine = match origin {
    None => slates_merge::engine::Green::new(),
    Some(origin) => {
      let record = Op::GreenOriginated {
        green: id,
        origin: origin.encode(),
      };
      if let Err(e) = state.db.mutate(&mut state.segment, &record, now) {
        return refused(refusal_of_db(&e));
      }
      slates_merge::engine::Green::with_origin(&origin)
    }
  };
  let mut engine = engine;
  engine.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
  state.greens.insert(id, engine);
  // Version 0's merge record — the origin, placed before the record names it (§4.16 "Commit").
  crate::merge_service::enqueue_record(state, id, 0, [0u8; 32], 0, Vec::new());
  ReplyBody::GreenCreated {
    id: to_wire_volume(id),
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

/// The files a green changed strictly after `version` (§4.16): the read a lagging work uses to know
/// what green moved under it before it rebases.
fn changed_since(
  state: &ShardState,
  principal: &Principal,
  green: VolumeId,
  version: u64,
) -> ReplyBody {
  let record = match crate::merge_service::require_green(state, principal, green, "changed_since") {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  let Some(engine) = state.greens.get(&record.id) else {
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
  let green_id = match crate::merge_service::require_green(state, principal, green, "create_work") {
    Ok(record) => record.id,
    Err(refusal) => return refused(refusal),
  };
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
  let Some(id) = fresh_volume_id(state) else {
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
  };
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    return refused(refusal_of_db(&e));
  }
  // The seeded copy of the green is charged before the work exists; refused, nothing is created.
  let charged = crate::work_charge::footprint(&seeded, &[]);
  if state.store.grow(charged).is_err() {
    return refused(Refusal::BudgetExceeded {
      available: state.store.admittable(),
    });
  }
  state.works.insert(
    id,
    crate::state::WorkState {
      green: green_id,
      base_version: base,
      journal: Vec::new(),
      content: seeded,
      revision: 0,
      charged,
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
  let old = w.content.get(path);
  let old_len = old.map_or(0, |c| c.len() as u64);
  // The journal operations this edit records, built before anything changes so its charge is exact.
  let mut ops = Vec::new();
  if old.is_none() {
    ops.push(VolumeOp::Create {
      path: path.to_owned(),
    });
  }
  if delete_len > 0 {
    ops.push(VolumeOp::Delete {
      path: path.to_owned(),
      at,
      len: delete_len,
    });
  }
  if !bytes.is_empty() {
    let len = bytes.len() as u64;
    ops.push(if at >= old_len {
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
  // The work is charged for what the edit adds before it changes anything (`crate::work_charge`).
  let (added, removed) = crate::work_charge::edit_delta(
    old.map(Vec::as_slice),
    (path, at, delete_len, bytes.len()),
    &ops,
  );
  if let Err(available) = crate::work_charge::grow(state, work_id, added) {
    return refused(Refusal::BudgetExceeded { available });
  }
  let Some(w) = state.works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  {
    let content = w.content.entry(path.to_owned()).or_default();
    let start = usize::try_from(at).unwrap_or(usize::MAX).min(content.len());
    let del = usize::try_from(delete_len)
      .unwrap_or(usize::MAX)
      .min(content.len().saturating_sub(start));
    content.splice(start..start.saturating_add(del), bytes.iter().copied());
  }
  w.revision = w.revision.wrapping_add(1);
  w.journal.extend(ops);
  crate::work_charge::shrink(state, work_id, removed);
  ReplyBody::Edited
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
  // The work is charged for what the declaration adds before it changes anything (`crate::work_charge`).
  let (added, removed) = crate::work_charge::declare_delta(&w.content, &volume_op);
  if let Err(available) = crate::work_charge::grow(state, work_id, added) {
    return refused(Refusal::BudgetExceeded { available });
  }
  let Some(w) = state.works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  match &volume_op {
    VolumeOp::Unlink { path } => {
      w.content.remove(path);
    }
    VolumeOp::Rename { from, to } => {
      if let Some(bytes) = w.content.remove(from) {
        w.content.insert(to.clone(), bytes);
      }
    }
    _ => {}
  }
  w.journal.push(volume_op);
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
    // The bytes this op contributes, and the offset into them: a file's slice, or an xattr's value.
    let (source, source_at): (&[u8], u64) = if is_file_content(op.kind) {
      let Some(file) = content.get(path) else {
        continue;
      };
      (file.as_slice(), op.at)
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
  work_id: DbVolumeId,
  evidence: Vec<[u8; 32]>,
) -> Result<(DbVolumeId, slates_merge::engine::Increment), Refusal> {
  let Some(w) = state.works.get(&work_id) else {
    return Err(Refusal::NotFound);
  };
  let green_id = w.green;
  let base_version = w.base_version;
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
      evidence,
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
fn submit(
  state: &mut ShardState,
  principal: &Principal,
  work: VolumeId,
  evidence: Vec<[u8; 32]>,
) -> ReplyBody {
  let (work_id, green_id) =
    match crate::merge_service::require_work(state, principal, work, false, "submit") {
      Ok((record, green)) => (record.id, green),
      Err(refusal) => return refused(refusal),
    };
  // A green that requires evidence refuses an increment without any (§4.16 `EvidenceRequired`),
  // before the seal: nothing is composed for a submit that cannot be accepted.
  if evidence.is_empty()
    && matches!(
      crate::merge_service::merge_role(state, green_id),
      Some(crate::merge_service::MergeRole::Green {
        require_evidence: true
      })
    )
  {
    return refused(Refusal::EvidenceRequired);
  }
  // The submission barrier (§4.16 "Submission": "seal the work volume"): the seal is taken here, in
  // this verb, on the single-threaded owner shard — every declared operation before it is in the
  // increment, and a write that arrives after it is a later verb that lands in the next increment,
  // never this one.
  let (green_id, inc) = match build_increment(state, work_id, evidence) {
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
  // Charge superseded values before the verdict. Incoming bytes do not bound the retained
  // history: unlink carries none and a short overwrite retains the complete previous file.
  let Some(engine) = state.greens.get(&green_id) else {
    return refused(Refusal::NotFound);
  };
  let secured = u64::try_from(engine.history_reservation(&inc)).unwrap_or(u64::MAX);
  // An accepted work is reset to the green's content at the new version, at most the green's files now and the
  // work's own: that bound is charged to the work first, so a refusal changes nothing (`crate::work_charge`), and
  // the settle after the verdict trues it up.
  let reset_bound = engine
    .files()
    .map(|(path, bytes)| crate::work_charge::file_footprint(path, bytes.len()))
    .fold(0u64, u64::saturating_add);
  if let Err(available) = crate::work_charge::grow(state, work_id, reset_bound) {
    report_first_budget_refusal(state, "work", reset_bound, available);
    return refused(Refusal::BudgetExceeded { available });
  }
  // Make room first: fold whatever the retention budget allows (never a live reader's version).
  if let Err(available) = crate::merge_service::settle_green_retention(state, green_id) {
    crate::work_charge::shrink(state, work_id, reset_bound);
    return refused(Refusal::BudgetExceeded { available });
  }
  if let Err(available) = crate::merge_service::secure_green_retention(state, green_id, secured) {
    crate::work_charge::shrink(state, work_id, reset_bound);
    report_first_budget_refusal(state, "retention", secured, available);
    return refused(Refusal::BudgetExceeded { available });
  }
  let verdict_start = state.clock.monotonic_ns();
  let outcome = {
    let Some(engine) = state.greens.get_mut(&green_id) else {
      crate::work_charge::shrink(state, work_id, reset_bound);
      return refused(Refusal::NotFound);
    };
    engine.submit(&inc)
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
      // The pre-check passed and the shard is single-threaded, so this append fits the budget; a
      // segment-full failure refuses like any other verb. A resubmit the engine answered from its
      // idempotent accept names a version the chain already holds: nothing is appended twice.
      let chain_len =
        u64::try_from(state.db.partition().green_chain(green_id).len()).unwrap_or(u64::MAX);
      if chain_len < version
        && let Err(e) = state.db.mutate(&mut state.segment, &record, now)
      {
        crate::work_charge::shrink(state, work_id, reset_bound);
        return refused(refusal_of_db(&e));
      }
      reset_accepted_work(state, (green_id, work_id), version);
      // The version's merge record, issued only once its inputs — the increment just appended — are
      // placed (§4.16 "Commit"; at `f = 0` the append is the placement).
      crate::merge_service::enqueue_record(
        state,
        green_id,
        version,
        inc.id,
        inc.base,
        inc.evidence,
      );
      // The work's base moved to the version, so the reachable floor may have risen: fold the histories
      // to it and true the retention charge up to what the commit retained (the secured surplus credited).
      // A settle after an accept only credits: nothing was retained beyond what was secured.
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
      ReplyBody::Submitted {
        version: Some(version),
        conflicts: Vec::new(),
      }
    }
    slates_merge::engine::Outcome::Conflict { windows } => {
      // The work is not reset: the bound secured for a reset is released.
      crate::work_charge::shrink(state, work_id, reset_bound);
      settle_after_conflict(state, green_id, &inc.id);
      ReplyBody::Submitted {
        version: None,
        conflicts: merge_windows(&windows),
      }
    }
  }
}

/// An accepted work now equals its green at `version` (§4.16 "Submission"): its journal is consumed (the increment
/// holds it), its base moves to the version and its content is the green's, so a later edit declares against what
/// the green holds (a work with `stream` submits at every auto-seal on exactly this footing; without the reset a
/// second submit would re-declare the already-merged operations against the old base). Its charge is then trued up
/// to what it keeps, within the bound the submit secured before the verdict (`crate::work_charge`).
fn reset_accepted_work(
  state: &mut ShardState,
  (green_id, work_id): (DbVolumeId, DbVolumeId),
  version: u64,
) {
  if let (Some(engine), Some(w)) = (state.greens.get(&green_id), state.works.get_mut(&work_id)) {
    w.base_version = version;
    w.journal.clear();
    w.content = engine
      .files()
      .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
      .collect();
    w.revision = w.revision.wrapping_add(1);
  }
  if let Some(w) = state.works.get(&work_id) {
    let wanted = crate::work_charge::footprint(&w.content, &w.journal);
    let _ = crate::work_charge::set_to(state, work_id, wanted);
  }
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
  let work_id = match crate::merge_service::require_work(state, principal, work, false, "rebase") {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  let (green_id, inc) = match build_increment(state, work_id, Vec::new()) {
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
      // The work moves onto the head; the green is untouched. Its new footprint is charged first: refused, the
      // work stays where it was. `build_increment` proved the work exists, so this lookup finds it.
      let wanted = crate::work_charge::footprint(&files, &journal);
      if let Err(available) = crate::work_charge::set_to(state, work_id, wanted) {
        return refused(Refusal::BudgetExceeded { available });
      }
      if let Some(w) = state.works.get_mut(&work_id) {
        w.base_version = version;
        w.content = files;
        w.journal = journal;
        w.revision = w.revision.wrapping_add(1);
      }
      // The work's base rose, so the reachable floor may have: fold the green's histories to it and
      // credit the retention released (a settle after a rise only credits).
      let _ = crate::merge_service::settle_green_retention(state, green_id);
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
  use crate::merge_service::{StoreVerb, find_store_backed};
  let (handle, record) = match find_store_backed(state, volume, StoreVerb::Clone) {
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
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
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
    Err(_) => return give_back(state, reservation, None, None, Refusal::NotFound),
  };
  let clone_allowance = inode_allowance(state, size).max(inherited);
  let version_credit = match state.store.versions.reserve(clone_allowance) {
    Ok(c) => Some(c),
    Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
      return give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BudgetExceeded { available },
      );
    }
    Err(e) => {
      return give_back(
        state,
        reservation,
        None,
        None,
        Refusal::BadRequest {
          reason: e.to_string(),
        },
      );
    }
  };
  let names = wire_names(record.policy.names);
  let quota = quota_for(size);
  let config = volume_config(state, names, quota);
  // The clone's records are reserved against the metadata ledger before it exists (§4.2).
  let metadata_credit = match reserve_metadata(state, config.journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => return give_back(state, reservation, version_credit, None, refusal),
  };
  // A clone of a strict volume is strict (its record is the source's): its own entitlement is reserved (§4.2 D-12).
  let locked = record.policy.require_locked;
  let lock_credit = match locked.then(|| reserve_locked(state, size)).transpose() {
    Ok(credit) => credit,
    Err(refusal) => return give_back(state, reservation, version_credit, metadata_credit, refusal),
  };
  let cloned = match state.volumes.get_mut(handle) {
    Ok(slot) => Volume::clone_of(
      &state.store,
      &mut slot.volume,
      core_snapshot(snapshot),
      config,
    ),
    Err(_) => {
      return give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        Refusal::NotFound,
      );
    }
  };
  let mut volume_core = match cloned {
    Ok(v) => v,
    Err(e) => {
      return give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal_of_vfs(&e),
      );
    }
  };
  volume_core.set_locked(locked);
  // Cap the clone's inode dimension at the same allowance already reserved above, so its per-volume
  // cap (`next_no`) and its version-slab reservation agree. A clone previously carried no inode cap.
  let allowed = volume_core.set_inode_allowance(clone_allowance);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  let Some(id) = fresh_volume_id(state) else {
    let discarded = volume_core.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      ids_exhausted(),
    );
  };
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
      let discarded = volume_core.discard_partial(&mut state.store);
      count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
      return give_back(
        state,
        reservation,
        version_credit,
        metadata_credit,
        refusal_of_db(&e),
      );
    }
  }
  // As in create: ensure the registry has room before moving the clone into a slot, discarding it
  // (which frees only what the clone made — nothing, since it shares its origin's versions) otherwise.
  // A partial clone also releases the pin `clone_of` put on the origin snapshot.
  if !state.volumes.has_room() {
    let full = state.volumes.max_slots();
    unpin_origin(state, handle, snapshot);
    let discarded = volume_core.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    return give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      Refusal::BadRequest {
        reason: format!("volume registry full at {full}"),
      },
    );
  }
  let mut volume_core = volume_core;
  crate::content_cipher::key_volume(&mut state.store, &mut volume_core, id.bytes);
  let slot = VolumeSlot {
    id,
    name: name.to_owned(),
    volume: volume_core,
    host: None,
    reservation,
    version_credit,
    metadata_credit,
    lock_credit,
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
      metadata_credit,
      Refusal::BadRequest {
        reason: e.to_string(),
      },
    ),
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
  if matches!(record.policy.role, Role::Green { .. }) {
    return crate::merge_service::attach_green(state, client_id, principal, &record, intent, &form);
  }
  let rights = rights_of(&record, principal);
  if !permits(rights, intent) {
    return forbidden("attach");
  }
  let situation = crate::transports::situation(state, &rights);
  let binding = match establish_form(&record, &situation, snapshot, (intent, principal), &form) {
    Ok(binding) => binding,
    Err(refusal) => return refused(refusal),
  };
  let consumer = binding.as_ref().map_or_else(
    || consumer_of(&form, client_id),
    |binding| {
      binding.consumer(
        state.db.partition(),
        record.id,
        principal,
        snapshot.map(to_db_snapshot),
      )
    },
  );
  let consumer = match consumer {
    Ok(consumer) => consumer,
    Err(refusal) => return refused(refusal),
  };
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
  if form.fuse_mount_point().is_some() {
    return defer_fuse_attach(
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

/// Whether `rights` permit an attach with `intent`.
fn permits(rights: Rights, intent: Intent) -> bool {
  match intent {
    Intent::Read => rights.read,
    Intent::Write => rights.write,
  }
}

/// The write lease an attach with `intent` takes (D-16): none for a read, the volume's lease for a write
/// (its epoch), or the refusal when another principal holds it unexpired.
fn lease_for(
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
pub(crate) fn consumer_of(form: &AttachRequest, client_id: u32) -> Result<Consumer, Refusal> {
  match form {
    AttachRequest::HostMount
    | AttachRequest::FuseMount { .. }
    | AttachRequest::ScopedHostMount { .. }
    | AttachRequest::ScopedFuseMount { .. }
    | AttachRequest::SharedFuseMount { .. } => Ok(Consumer::Bridge),
    AttachRequest::Root => Ok(Consumer::Sdk { client: client_id }),
    AttachRequest::Oci { .. } => Err(Refusal::AttachmentUnsupported {
      transport: AttachTransport::Oci,
      reason: UnsupportedReason::HostMountRequired,
    }),
    AttachRequest::Guest { transport } => Err(Refusal::AttachmentUnsupported {
      transport: *transport,
      reason: UnsupportedReason::SeamNotOnWire,
    }),
  }
}

/// The rights an attachment records (§4.13 "Access lists"; AUD-01): the principal's rights on the volume
/// bounded by the attachment's intent — an attach-for-read records no write right, so its mount
/// capability presents a read-only view whatever the principal could otherwise do (the NFS edge maps
/// the record's rights, never the principal's). `admin` is never an attachment's: the admin verbs act on
/// the volume by principal, not through an attachment.
fn granted_rights(rights: Rights, intent: Intent) -> Rights {
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
    // The record forms: nothing to establish — the SDK's record, and the host mount the requesting
    // process establishes itself with the capability the reply carries (`mount_nfs`, R10: no privilege
    // and nothing of the daemon's touches the mount table).
    AttachRequest::Root => return Ok(None),
    // A scoped host mount presents the live head beneath one directory; a snapshot of a subtree is not
    // presented (its scope and its version would both have to hold through one view).
    AttachRequest::ScopedHostMount { .. } => {
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
      check_fuse_mount_point(form, principal)?;
      return Ok(None);
    }
    AttachRequest::Guest { transport } => return Err(guest_over_the_ring(situation, *transport)),
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
  let Some(record) = state.db.partition().attachment(attachment).cloned() else {
    return refused(Refusal::NotFound);
  };
  if &record.principal != principal {
    return forbidden("detach");
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
  let now = state.clock.monotonic_ns();
  match state.db.mutate(
    &mut state.segment,
    &Op::AttachmentBound {
      id: attachment,
      path: path.to_owned(),
    },
    now,
  ) {
    Ok(_) => ReplyBody::MountBound,
    Err(e) => refused(refusal_of_db(&e)),
  }
}

/// Derived: the bytes of mount points one status report carries — a quarter of the reply's bulk chunk
/// (`BULK_CHUNK_BYTES` / 4), so the mounts never crowd the report's other fields out of the slot; the
/// rest are counted (`mounts_elided`).
const MOUNTS_REPORT_BYTES: usize = 1024;

/// The volume's bound host mounts for its status (§4.4 `Bound`; GAP-A9-4): each `Consumer::Bridge`
/// attachment whose form is a chosen path, as many as the mount budget carries (in id order), and the
/// count of those it did not.
fn mounts_of(
  state: &ShardState,
  volume: DbVolumeId,
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
    if carried.saturating_add(path.len()) > MOUNTS_REPORT_BYTES {
      elided = elided.saturating_add(1);
      continue;
    }
    carried = carried.saturating_add(path.len());
    mounts.push(slates_ipc::protocol::MountReport {
      attachment: record.id,
      path: path.to_owned(),
    });
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
  state.db.mutate(
    &mut state.segment,
    &Op::AttachmentRemoved { id: record.id },
    now,
  )?;
  crate::merge_service::forget_attachment(state, record.id);
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
  // A FUSE mount of the attachment is unmounted only when its user detached it; the kernel's disconnect then ends
  // its serve task. Any other end leaves the mount in place, its requests refused under the revoked registry
  // attachment below until its user unmounts it: an unmount nobody asked for lets the next write by path land on
  // the disk beneath the mount point (conditions 3 and 4; `docs/bugs/2026-10-06-an-ended-mount-let-writes-reach-the-disk-beneath.md`).
  if ending == Ending::Detached {
    #[cfg(target_os = "linux")]
    crate::fuse::unmount_if_mounted(state, record.id);
  }
  // The registry attachment a mount's requests rode ends with the record: revoked so no later request
  // is admitted under it, drained so its slot is reused (GAP-A9-4).
  if let Some(mount) = state.mount_attachments.remove(&record.id) {
    state.attachments.revoke(mount.registry);
    state.attachments.drain(mount.registry);
  }
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
    versions.release(slates_mem::budget::VersionCredit {
      slots: old.saturating_sub(new),
    });
  }
  slates_mem::budget::VersionCredit { slots: new }
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
  // Never refused: `new_allowance` is at least the live count, the setter's one refusal. Counted all the same.
  let allowed = slot.volume.set_inode_allowance(new_allowance);
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
  // The volume's guest devices are revoked first (AUD-29-68): each ends at its next pass boundary, its terminal
  // step sweeping its references through its volume or view while they still exist. The teardown waits for
  // them (`step_destroys` starts it once none serves the volume); with none, it starts now.
  #[cfg(unix)]
  let devices_serving = crate::virtiofs::revoke_volume_devices(state, record.id);
  #[cfg(not(unix))]
  let devices_serving = 0;
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
  if let Err(e) = start_teardown(state, handle, volume) {
    state.count(refusal_name(&refusal_of_vfs(&e)), 1);
  }
  true
}

/// One cooperative destroy slice per destroying volume; a finished one leaves the tables.
pub fn step_destroys(state: &mut ShardState) -> bool {
  let budget = state
    .config
    .step_quantum_ns()
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
      let origin_pin = state
        .db
        .partition()
        .lineage(id)
        .map(|e| (e.origin_volume, e.origin_snapshot));
      let now = state.clock.monotonic_ns();
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
        release_slot_credits(state, &slot);
      }
      state.by_id.remove(&id);
      retire_local_tombstones(state);
    }
  }
  any
}

/// Returns every credit a torn-down volume held to the shard's ledgers (§4.2 accounting through
/// teardown): its byte reservation, its inode allowance (so the version slab is never
/// over-offered), its records' metadata reservation, and the growth a dynamic volume acquired from
/// the shard budget as it wrote.
fn release_slot_credits(state: &mut ShardState, slot: &VolumeSlot) {
  if let Some(r) = slot.reservation {
    state.store.budget.release(r);
  }
  if let Some(c) = slot.version_credit {
    state.store.versions.release(c);
  }
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
  let (mounts, mounts_elided) = mounts_of(state, record.id);
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
          .chain(
            s.unverified
              .iter()
              .map(|(p, refusal)| format!("{p} (unverified: {refusal})")),
          )
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
      drifted,
      watcher,
      snapshots: u32::try_from(slot.volume.snapshot_count()).unwrap_or(u32::MAX),
      placed: placed_state(state, &record),
      // The daemon's NFS port (§4.6), 0 until the listener binds; report it so a client can mount.
      nfs_port: u16::try_from(crate::daemon::NFS_PORT.load(std::sync::atomic::Ordering::Acquire))
        .ok()
        .filter(|port| *port != 0),
      transports: Box::new(transports),
      mounts,
      mounts_elided,
    }),
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

/// The region placement of `object`'s head at `sequence` as the fleet has **actually committed** it (§4.8
/// "the acknowledging set is recorded in the object's head record"): the acknowledging candidates the
/// control-shard record plane recorded when it replicated that head to its holders
/// (`ShardState::placed_heads`), or — before any holder has acknowledged, for an older or newer sequence,
/// or on a laptop where no fleet loop runs — the owner's local placement the configuration computes
/// (`Configuration::place`: the owner alone, which is placed at `f = 0` and not yet at `f > 0`). One code
/// path serves both (R8): the laptop is the empty-map degenerate. Reading the computed placement alone
/// reported a fleet's head *never* region-placed, however many holders held it.
fn committed_placement(
  state: &ShardState,
  object: ObjectId,
  sequence: u64,
) -> slates_db::register::Placement {
  state
    .placed_heads
    .get(&object)
    .filter(|head| head.sequence == sequence)
    .map(|head| head.placement.clone())
    .unwrap_or_else(|| state.fleet.configuration().place(object))
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
    let config = state.fleet.configuration();
    config.region_placed(&committed_placement(state, object, CREATION_HEAD_SEQUENCE))
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
    None => config.region_placed(&committed_placement(state, object, CREATION_HEAD_SEQUENCE)),
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
const CREATION_HEAD_SEQUENCE: u64 = 0;

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
/// Materializes a taken-over **green** on the shard its id routes to (§4.16 owner-loss recovery;
/// AUD-14): records its catalog entry under the adopted record's name, evidence policy and owner
/// (guard-then-apply, refused typed when the name is taken or the chain budget cannot hold it),
/// re-records its origin and every increment of the recovered chain durably — so a restart of this
/// node rebuilds the same green — replays them into a fresh engine (`rebuild_green`: the same
/// derivation a restart runs, with the rejected-cache budget and retention settled), and verifies the
/// rebuilt head identity against the adopted record's: a mismatch is fatal-and-loud for the green
/// here (the catalog record stays but the engine is not installed, counted). On success the adopted
/// head is placed (its records committed at the quorum under the departed owner, the adoption under
/// this node's epoch), so `await placed`, `versions`, reads at any version and new submits serve.
/// Idempotent: a green this shard already holds is left alone.
pub(crate) fn materialize_taken_over_green(
  state: &mut ShardState,
  id: DbVolumeId,
  recovery: crate::merge_service::GreenRecovery,
) -> Result<(), Box<ReplyBody>> {
  use crate::merge_service::MergeRole;
  if state.greens.contains_key(&id) {
    return Ok(());
  }
  if state.db.partition().volume(id).is_none() {
    if let Some(existing) = state.db.partition().volume_by_name(&recovery.name) {
      return Err(Box::new(refused(Refusal::AlreadyExists {
        existing: to_wire_volume(existing.id),
      })));
    }
    let record = VolumeRecord {
      id,
      name: recovery.name.clone(),
      owner_shard: state.partition,
      policy: PolicyRecord {
        size: db_size(SizeClass::Dynamic { max: 0 }),
        names: DbNamePolicy::Exact,
        require_locked: false,
        role: Role::Green {
          require_evidence: recovery.require_evidence,
          head_version: recovery.head,
        },
      },
      base: BaseRecord::Scratch,
      head: DbSnapshotId::default(),
      epoch: 0,
      referenced_bytes: 0,
      unique_bytes: 0,
      state: VolumeState::Live,
      lease: None,
      owner: recovery.owner.clone(),
      access: Vec::new(),
      created_ns: state.clock.monotonic_ns(),
      catalog_version: 0,
    };
    let now = state.clock.monotonic_ns();
    if let Err(e) = state
      .db
      .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
    {
      return Err(Box::new(refused(refusal_of_db(&e))));
    }
  }
  debug_assert!(matches!(
    crate::merge_service::merge_role(state, id),
    Some(MergeRole::Green { .. })
  ));
  // The chain, durably, in order — each entry guarded before it is applied.
  let now = state.clock.monotonic_ns();
  let already = state.db.partition().green_chain(id).len();
  if already == 0
    && state.db.partition().green_origin(id).is_none()
    && let Some(origin) = &recovery.origin
  {
    let op = Op::GreenOriginated {
      green: id,
      origin: origin.clone(),
    };
    if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
      return Err(Box::new(refused(refusal_of_db(&e))));
    }
  }
  for increment in recovery.chain.iter().skip(already) {
    let op = Op::GreenAdvanced {
      green: id,
      increment: increment.clone(),
    };
    if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
      return Err(Box::new(refused(refusal_of_db(&e))));
    }
  }
  let Some(record) = state.db.partition().volume(id).cloned() else {
    return Err(Box::new(refused(Refusal::NotFound)));
  };
  rebuild_green(state, &record);
  let rebuilt = state
    .greens
    .get(&id)
    .map(|engine| (engine.head(), engine.head_identity()));
  if rebuilt != Some((recovery.head, recovery.identity)) {
    // The chain this node held does not reproduce the adopted head: refuse the green here, loudly,
    // rather than serve versions the quorum did not commit.
    state.greens.remove(&id);
    crate::merge_service::release_green_retention(state, id);
    state.count(GREEN_TAKEOVER_MISMATCH, 1);
    eprintln!(
      "slates-server: partition {}: taken-over green {} rebuilt to {:?}, the adopted record names version {} with another identity; refused",
      state.partition,
      record.name,
      rebuilt.map(|(head, _)| head),
      recovery.head
    );
    return Err(Box::new(refused(Refusal::BadRequest {
      reason: "taken-over green does not reproduce its adopted head".to_owned(),
    })));
  }
  let object = ObjectId(id.bytes);
  let placed = state.merge.placed.entry(object).or_insert(recovery.head);
  *placed = (*placed).max(recovery.head);
  Ok(())
}

/// A taken-over green whose recovered chain did not reproduce the adopted head's identity: refused on
/// this node, fatal-and-loud (§4.16 D-27).
const GREEN_TAKEOVER_MISMATCH: &str = "merge.takeover_mismatch";

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
}

pub(crate) fn materialize_taken_over(
  state: &mut ShardState,
  id: DbVolumeId,
  taken: &TakenOver<'_>,
  region: Vec<u64>,
  archive: &slates_archive::Archive,
) -> Result<(), Box<ReplyBody>> {
  let (head, catalog, sequence) = (taken.head, taken.catalog, taken.sequence);
  if state.by_id.contains_key(&id) {
    return Ok(());
  }
  if let Some(existing) = state.db.partition().volume_by_name(&catalog.name) {
    return Err(Box::new(refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    })));
  }
  let size = match catalog.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  };
  // The restore is admitted before it allocates (AUD-29-13): no more than the volume may hold — its bound
  // or its dynamic maximum — and no more than this shard could admit, since every restored byte is written
  // into the volume next. An archive needing more is refused typed before a byte is reconstructed.
  let volume_cap = match size {
    SizeClass::Bounded { limit } => limit,
    SizeClass::Dynamic { max } => max,
  };
  // A shard that claims its arena lazily makes room for what the restore needs first (A-98); an archive that does not
  // plan is refused by the restore below, as before.
  if let Ok(needed) = slates_archive::restore_needed(archive) {
    state.store.make_room(needed.min(volume_cap));
  }
  let restore_budget = volume_cap.min(state.store.budget.admittable());
  let restored = slates_archive::restore(archive, restore_budget).map_err(|e| match e {
    slates_archive::ArchiveError::OverBudget { .. } => refused(Refusal::BudgetExceeded {
      available: restore_budget,
    }),
    other => refused(Refusal::BadRequest {
      reason: format!("taken-over content archive: {other}"),
    }),
  })?;
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.reserve(limit) {
      Ok(r) => Some(r),
      Err(slates_mem::MemError::BudgetExceeded { available, .. }) => {
        return Err(Box::new(refused(Refusal::BudgetExceeded { available })));
      }
      Err(e) => {
        return Err(Box::new(refused(Refusal::BadRequest {
          reason: e.to_string(),
        })));
      }
    },
    SizeClass::Dynamic { .. } => None,
  };
  let version_credit = match state.store.versions.reserve(inode_allowance(state, size)) {
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
        Refusal::BadRequest {
          reason: e.to_string(),
        },
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
  if let Err(e) = populate_restored(&mut state.store, &mut volume, &restored) {
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
      Some(kind @ (Kind::Fifo | Kind::Socket)) => {
        if file.len != 0 {
          return Err(slates_vfs::error::VfsError::RecoveryIncomplete);
        }
        let parent_no = store.dirs.get(parent)?.inode;
        volume.mknod_no(
          store,
          parent_no,
          &name,
          permissions_of_mode(meta.mode),
          kind,
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
      Some(Kind::Fifo | Kind::Socket) if meta.size == 0 && file.len == 0 => {}
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
  // A local client acknowledges its own completions, keyed under this node's **stable cert-anchor** — the
  // same key `serve` records and looks them up under (task #22 two-id model), never the ephemeral member
  // id: pruning under a key nothing is recorded under released no record (the windows grew unbounded,
  // banned item 8) and a retry after acknowledgement met its record again instead of `DuplicateRequest`
  // (`docs/bugs/2026-09-13-acknowledge-prunes-under-the-ephemeral-id.md`; found independently five
  // times in two days — also `…-acknowledge-keyed-on-ephemeral-member-id.md`,
  // `2026-09-14-ack-keyed-under-ephemeral-member-id.md` and `2026-09-14-ack-keyed-on-ephemeral-member-id.md`).
  let origin = state.origin_anchor.0;
  match state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      origin,
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
    if request.kind == SlotKind::Cancel
      && let Ok(client) = state.clients.get_mut(handle)
    {
      client.status_pages.clear(&mut state.store.metadata);
    }
    if matches!(request.kind, SlotKind::Heartbeat | SlotKind::Cancel) {
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
    let served = serve(state, handle, &request);
    state.current_span = None;
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
  /// FUSE writes acknowledged after the last publication, replayed from the write log (A-63).
  pub writes_replayed: usize,
  /// Claimed blocks no rebuilt volume reaches, freed by the recovery sweep (A-64).
  pub blocks_swept: usize,
  /// Host mounts of snapshots whose read-only views were rebuilt (AUD-29-76).
  pub snapshot_views: usize,
  /// Merge volumes rebuilt (§4.16): greens with their persisted chain replayed, works reset to a
  /// fresh clone of their green's head (their scratch edits did not survive).
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
pub fn rebuild_recovered(state: &mut ShardState) -> Rebuilt {
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
  } = recover_images(state);
  // The barrier replies the dead daemon published and never delivered, for the mounts the anchor held (A-61).
  #[cfg(target_os = "linux")]
  {
    state.recovered_replies = replies
      .into_iter()
      .map(|reply| (reply.attachment, (reply.unique, reply.reply)))
      .collect();
  }
  #[cfg(not(target_os = "linux"))]
  drop(replies);
  let mut rebuilt = Rebuilt::default();
  // Every block the images name is claimed before anything else allocates in the arena (A-64): the held
  // replicas and the merge volumes below allocate fresh blocks, which must never land on one an image names.
  let claims = claim_images(state, &images);
  // The held replicas' blocks too (A-64), then every claim is committed: the recovered image names them all, so none
  // is reused before this daemon's first publication commits.
  // The held image carries the content hold and the held registers (§4.8 persistence before reply): split, the
  // content claimed and rebuilt below, the registers rebuilt with their fences once the hold is.
  let held = match crate::content_holder::split_held(&held) {
    Ok(held) => Some(held),
    Err(e) => {
      eprintln!(
        "slates-server: partition {}: held image refused: {e:?}",
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
    Ok(hold) => {
      rebuilt.replicas = hold.manifest_count();
      state.held_content = hold;
    }
    Err(e) => {
      eprintln!(
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
  let mut max_prefix = state.next_prefix;
  // Greens first, so a work can seed from a rebuilt green (§4.16): a green's merge chain is replayed
  // into a fresh engine, restoring its content and versions.
  for record in &records {
    if matches!(record.policy.role, Role::Green { .. }) {
      rebuild_green(state, record);
      rebuilt.merge_volumes = rebuilt.merge_volumes.saturating_add(1);
    }
  }
  // Origins before their clones (A-64): a clone is rebuilt over its recovered origin's snapshot.
  let mut records = records;
  records.sort_by_key(|record| lineage_depth(state, record.id));
  for record in &records {
    match record.policy.role {
      // Rebuilt in the pass above; here only its content-less attachments are reconciled out.
      Role::Green { .. } => {}
      Role::Work { green, .. } => {
        rebuild_work(state, record, green);
        rebuilt.merge_volumes = rebuilt.merge_volumes.saturating_add(1);
      }
      Role::Plain => {
        match rebuild_volume(state, record, images.get(&record.id.bytes), claims.as_ref()) {
          Ok(prefix) => {
            rebuilt.volumes = rebuilt.volumes.saturating_add(1);
            max_prefix = max_prefix.max(prefix.wrapping_add(1).max(1));
            rebuilt.snapshots_trimmed = rebuilt
              .snapshots_trimmed
              .saturating_add(trim_unrecorded_snapshots(state, record.id));
          }
          Err(reason) => {
            rebuilt.skipped = rebuilt.skipped.saturating_add(1);
            eprintln!(
              "slates-server: partition {}: volume {} not rebuilt: {reason}",
              state.partition, record.name
            );
            continue;
          }
        }
      }
    }
    let (snapshots, attachments) = reconcile_lost(state, record);
    rebuilt.snapshots_dropped = rebuilt.snapshots_dropped.saturating_add(snapshots);
    rebuilt.attachments_dropped = rebuilt.attachments_dropped.saturating_add(attachments);
    rebuilt.orphans_reclaimed = rebuilt
      .orphans_reclaimed
      .saturating_add(settle_references(state, record));
  }
  rebuilt.blocks_swept = sweep_claims(state, claims);
  // The FUSE writes acknowledged after the last publication, back on top of the rebuilt volumes (A-63).
  #[cfg(target_os = "linux")]
  {
    rebuilt.writes_replayed = replay_writes(state);
  }
  // Hand out prefixes past every recovered one, so a new volume never collides with a recovered
  // volume's inode numbers (the prefixes came from the images, not from `next_prefix`).
  state.next_prefix = max_prefix;
  rebuilt.destroys_completed = complete_recovered_destroys(state);
  rebuilt.pins_reconciled = reconcile_clone_pins(state);
  // The recorded snapshot mounts' views, once the volumes and their pins are back (AUD-29-76).
  rebuilt.snapshot_views = crate::snapshot_view::rebuild(state);
  rebuilt
}

/// How many clone edges lead from `volume` back to a volume that is no clone (A-64: the order recovery rebuilds in).
/// Bounded by the catalog's volume count, so a cycle in a corrupt catalog ends.
fn lineage_depth(state: &ShardState, volume: DbVolumeId) -> usize {
  let partition = state.db.partition();
  let bound = partition.volumes().len();
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
      eprintln!(
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
      eprintln!(
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
    if state
      .db
      .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now)
      .is_ok()
    {
      completed = completed.saturating_add(1);
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
/// tree. Returns how many pins were changed.
fn reconcile_clone_pins(state: &mut ShardState) -> usize {
  // The catalog's live clones per (origin volume, origin snapshot).
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
  }
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
/// good version, logged, rather than presenting a wrong later state.
fn rebuild_green(state: &mut ShardState, record: &VolumeRecord) {
  let green = match replay_green(state, record.id) {
    Ok(green) => green,
    Err(fence) => return fence_green(state, record, fence),
  };
  state.greens.insert(record.id, green);
  // The replay rebuilt every history in full; the recovered works are reset to the head and a green's
  // attachments are reconciled out at boot, so nothing reachable lies below the head: fold to it and
  // re-take the retention the remaining histories hold, ahead of new claims (§4.2 recovery order). A
  // retention the budget cannot hold fences the green rather than serve it over budget.
  if let Err(short) = crate::merge_service::settle_green_retention(state, record.id) {
    fence_green(
      state,
      record,
      crate::merge_service::GreenFence::Retention { short },
    );
  }
}

/// Replays green `id`'s durable origin and chain into a fresh engine (§4.16): every chain entry was an
/// acknowledged version, so each must decode and be accepted at exactly its own version — version `n` for
/// the `n`-th entry, over the origin's version 0 — or the replay is refused with the fence naming why
/// (AUD-29-18). Until 2026-09-30 a corrupt origin installed an empty green, a corrupt entry stopped the replay
/// and installed the shorter one, and a submit's outcome was not checked at all.
fn replay_green(
  state: &ShardState,
  id: DbVolumeId,
) -> Result<slates_merge::engine::Green, crate::merge_service::GreenFence> {
  use crate::merge_service::GreenFence;
  let mut green = match state.db.partition().green_origin(id) {
    None => slates_merge::engine::Green::new(),
    Some(bytes) => slates_merge::origin::Origin::decode(bytes)
      .map(|origin| slates_merge::engine::Green::with_origin(&origin))
      .map_err(|_| GreenFence::OriginCorrupt)?,
  };
  for (index, bytes) in state.db.partition().green_chain(id).iter().enumerate() {
    let version = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
    let increment = slates_merge::engine::Increment::decode(bytes)
      .map_err(|_| GreenFence::EntryCorrupt { version })?;
    match green.submit(&increment) {
      slates_merge::engine::Outcome::Accepted { version: accepted } if accepted == version => {}
      slates_merge::engine::Outcome::Accepted { version: accepted } => {
        return Err(GreenFence::WrongVersion { version, accepted });
      }
      slates_merge::engine::Outcome::Conflict { .. } => {
        return Err(GreenFence::NotAccepted { version });
      }
    }
  }
  green.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
  Ok(green)
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
  eprintln!(
    "slates-server: partition {}: green {} fenced, its recovered history is not what was acknowledged: {fence:?}",
    state.partition, record.name
  );
}

/// The status count of greens fenced at recovery (AUD-29-18). Format: a refusal name in the daemon's status
/// report, alongside the verbs' refusal kinds.
const GREEN_FENCED: &str = "merge.green_fenced";

/// Rebuilds a recovered work volume as a fresh clone of its green's current head (§4.16): a work's
/// declared edits are scratch and do not survive a restart (BUG-11 class), so the work is reset —
/// seeded with the green's content and based on its head — never presenting lost edits as if kept.
/// Skipped when its green is gone (the work has nothing to be over).
fn rebuild_work(state: &mut ShardState, record: &VolumeRecord, green: DbVolumeId) {
  let Some(engine) = state.greens.get(&green) else {
    return;
  };
  let base_version = engine.head();
  let content: std::collections::BTreeMap<String, Vec<u8>> = engine
    .files()
    .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
    .collect();
  // Charged like a fresh work; a budget that cannot hold it leaves the work unbuilt (its verbs answer NotFound)
  // and counted, never kept uncharged.
  let charged = crate::work_charge::footprint(&content, &[]);
  if state.store.grow(charged).is_err() {
    state.count(WORK_REBUILD_REFUSED, 1);
    return;
  }
  state.works.insert(
    record.id,
    crate::state::WorkState {
      green,
      base_version,
      journal: Vec::new(),
      content,
      revision: 0,
      charged,
    },
  );
}

/// The status count of recovered works left unbuilt because the shard's budget could not hold their content.
/// Format: a refusal name in the daemon's status report.
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

/// The shard's recovery images from its slice of the anchor content object (§4.8), by volume id, and the
/// held replicas' image the same shard image carries (AUD-29-59). A torn or malformed image logs and yields
/// nothing for that shard (each volume then refuses as unrecoverable rather than presenting empty),
/// matching §4.8's "never an empty success".
fn recover_images(state: &mut ShardState) -> RecoveredImages {
  let (start, end) = state.content_range;
  let (delta_start, delta_end) = state.delta_range;
  let Some(object) = &state.content else {
    return RecoveredImages::default();
  };
  if end <= start || end > object.len() {
    return RecoveredImages::default();
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
  match slates_vfs::checkpoint_log::Journal::recover(&checkpoints, &log) {
    Ok((Some(shard), journal)) => {
      state.journal = journal;
      state.published_keys = shard.volumes.iter().map(|keyed| keyed.key).collect();
      state.published_held.clone_from(&shard.held);
      RecoveredImages {
        volumes: shard
          .volumes
          .into_iter()
          .map(|keyed| (keyed.key, keyed.image))
          .collect(),
        held: shard.held,
        replies: shard.replies,
      }
    }
    Ok((None, _)) => RecoveredImages::default(),
    Err(e) => {
      eprintln!(
        "slates-server: partition {}: shard image unreadable: {e}",
        state.partition
      );
      RecoveredImages::default()
    }
  }
}

/// What a shard's last committed image held: its volumes by id, the replicas it held for other owners
/// (AUD-29-59), and the barrier replies it had not delivered (A-61).
#[derive(Default)]
struct RecoveredImages {
  volumes: std::collections::BTreeMap<[u8; 16], VolumeImage>,
  held: Vec<u8>,
  replies: Vec<slates_vfs::recover::HeldReply>,
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
      state.store.content.arena_mut().commit_capture();
      // The committed image names no block of a wholly free extent, and no deferred free holds one: it goes back to
      // the pool for any shard to claim (A-98).
      state.store.release_idle();
    }
    Err(_) => state.store.content.arena_mut().abandon_capture(),
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
  eprintln!(
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
    eprintln!(
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

/// The barrier replies not yet delivered, which ride with the effects they answer (A-61).
fn pending_replies(state: &ShardState) -> Vec<slates_vfs::recover::HeldReply> {
  #[cfg(target_os = "linux")]
  return state.pending_replies.values().cloned().collect();
  #[cfg(not(target_os = "linux"))]
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
  let (_, end) = state.content_range;
  match state.content.as_ref() {
    None => return Err(slates_vfs::VfsError::RecoveryIncomplete),
    Some(object) if end > object.len() => return Err(slates_vfs::VfsError::NoSpace),
    Some(_) => {}
  }
  // The replicas this shard holds for other owners ride the same image (AUD-29-59): a holder acknowledges a
  // content put only once a publish carrying it commits.
  let held = crate::content_holder::held_image(state);
  let mut replies = pending_replies(state);
  // A volume that cannot be captured restarts the checkpoint without it: the image's bytes before it may already be
  // in the slot, and the slot commits only when its header is written last, so the abandoned stream commits nothing.
  // Each restart leaves out one more volume, so there are at most as many restarts as volumes.
  let mut skipped: Vec<(DbVolumeId, slates_vfs::VfsError)> = Vec::new();
  let outcome = loop {
    match stream_checkpoint(state, &handles, &skipped, &held, &mut replies) {
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
  for handle in handles {
    if let Ok(slot) = state.volumes.get_mut(handle) {
      if published.volumes.contains(&slot.id) {
        slot.volume.mark_published(&state.store);
      } else {
        slot.volume.mark_unpublished();
      }
    }
  }
  state.published_keys = published.volumes.iter().map(|id| id.bytes).collect();
  state.published_held = held;
  // Every volume captured: this image holds every logged write, so the write log empties, stamped with this
  // generation (A-63). A volume skipped keeps the log, so its writes stay replayable.
  #[cfg(target_os = "linux")]
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
  held: &[u8],
  replies: &mut Vec<slates_vfs::recover::HeldReply>,
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
  ShardImage::encode_finish(stream.buf(), held, replies);
  match state.journal.finish_checkpoint(stream) {
    Ok(frame_bytes) => published.frame_bytes = frame_bytes,
    Err(e) => return Ok(Streamed::Refused(e)),
  }
  Ok(Streamed::Committed(published))
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
  let held = crate::content_holder::held_image(state);
  let held_changed = held != state.published_held;
  slates_vfs::checkpoint_log::ShardDelta::encode_finish(
    encoded,
    count_at,
    count,
    &mut removed,
    held_changed.then_some(&held[..]),
    Some(&pending_replies(state)),
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
  if held_changed {
    state.published_held = held;
  }
  #[cfg(target_os = "linux")]
  clear_write_log(
    state,
    published.skipped.is_empty(),
    state.journal.generation(),
  );
  Ok(published)
}

/// Format: the status counter of publications run to release blocks whose frees waited on one (A-64).
pub(crate) const DEFERRED_RELIEVED: &str = "arena.deferred_relieved";

/// Format: the status counter of content-pool claims refused (A-98): an extent whose owner word could not be reached,
/// or whose range the OS would not map, or one that could not be given back. Counted by the pool, never lost.
pub(crate) const CONTENT_POOL_REFUSED: &str = "content.pool_refused";

/// Format: the status counter of chunks sealed in the arena (A-99): idle content encrypted at rest.
pub(crate) const CONTENT_SEALED: &str = "content.sealed";

/// Format: the status counter of chunk seals that fell back to the clear because the cipher refused (A-99).
pub(crate) const CONTENT_SEAL_REFUSED: &str = "content.seal_refused";

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
  // A recovered strict volume's entitlement is reserved again in this process (§4.2 D-12); its claimed blocks were
  // locked as they were claimed (`claim_images`). One the OS would not let lock is refused, never served swappable.
  if record.policy.require_locked
    && claims.is_some_and(|claims| claims.unlockable(&record.id.bytes))
  {
    if let Some(r) = reservation {
      state.store.budget.release(r);
    }
    return Err("a strict volume's content could not be locked in this process".to_owned());
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
      return Err(format!(
        "a strict volume's entitlement exceeds this process's lock capacity: {}",
        refusal_name(&refusal)
      ));
    }
  };
  let built = build_recovered_volume(state, record, image, claims, size);
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
  if held > 0 && state.store.grow(held).is_err() {
    // The rebuilt volume returns its slots, blocks and retention rather than leaking them.
    let discarded = volume.discard_partial(&mut state.store);
    count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
    if let Some(r) = reservation {
      state.store.budget.release(r);
    }
    return Err("recovered dynamic growth exceeds the shard budget".to_string());
  }
  // Re-admit the inode dimension (§4.2): the fair share, but never below what the recovered volume
  // already holds — recovery does not refuse inodes that were admitted before the restart.
  let allowance = inode_allowance(state, size).max(volume.inode_usage().0);
  let allowed = volume.set_inode_allowance(allowance);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  let entries = entry_allowance(size).max(volume.entry_usage().0);
  let allowed = volume.set_entry_allowance(entries);
  count_kept(state, VOLUME_ALLOWANCE_REFUSED, allowed);
  // Re-acquire the version reservation (§4.2 accounting through recovery), giving back the byte
  // reservation and re-grown hold — and the rebuilt volume's slots, blocks and retention — if the
  // slab shrank.
  let version_credit =
    match recover_version_reservations(&mut state.store, allowance, reservation, held) {
      Ok(credit) => credit,
      Err(reason) => {
        let discarded = volume.discard_partial(&mut state.store);
        count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
        return Err(reason);
      }
    };
  // Re-acquire the records' metadata reservation (§4.2 accounting through recovery), giving every
  // other credit and the rebuilt volume back if the ledger cannot back it.
  let journal_bytes = journal_bytes_for(state, &quota_for(size));
  let metadata_credit = match reserve_metadata(state, journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      let discarded = volume.discard_partial(&mut state.store);
      count_kept(state, VOLUME_DISCARD_REFUSED, discarded);
      if let Some(c) = version_credit {
        state.store.versions.release(c);
      }
      release_recovered_bytes(&mut state.store, reservation, held);
      return Err(format!(
        "recovered volume records exceed the metadata ledger: {}",
        refusal_name(&refusal)
      ));
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
  let handle = state.volumes.insert(slot).map_err(|e| e.to_string())?;
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
) -> Result<(Volume, Option<OsHost>, u16), String> {
  let image =
    image.ok_or_else(|| "RecoveryIncomplete: no content image for the volume".to_owned())?;
  let quota = quota_for(size);
  let journal = journal_bytes_for(state, &quota);
  let mut opened = match &record.base {
    BaseRecord::Scratch => None,
    BaseRecord::Path { path } => Some(
      OsHost::open_root(std::path::Path::new(path))
        .map_err(|error| format!("RecoveryIncomplete: cannot reacquire the base: {error}"))?,
    ),
  };
  let source = opened
    .as_mut()
    .map(|(host, root)| (host as &mut dyn HostFs, *root));
  let claims = claims.ok_or_else(|| {
    "RecoveryIncomplete: the shard's recovery images name blocks that could not be claimed"
      .to_owned()
  })?;
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
        .ok_or_else(|| {
          "RecoveryIncomplete: the clone's origin volume was not recovered".to_owned()
        })?;
      let ShardState { store, volumes, .. } = &mut *state;
      let origin = volumes.get(origin).map_err(|_| {
        "RecoveryIncomplete: the clone's origin volume was not recovered".to_owned()
      })?;
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
  let mut volume = rebuilt.map_err(|error| error.to_string())?;
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
    return Err(format!(
      "RecoveryIncomplete: the image's quota exceeds the acknowledged size policy: {error}"
    ));
  }
  Ok((volume, opened.map(|(host, _)| host), image.prefix))
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
  let Ok(slot) = state.volumes.get_mut(handle) else {
    return 0;
  };
  match slot
    .volume
    .settle_recovered_references(&mut state.store, |id| partition.attachment(id).is_some())
  {
    Ok((_, reclaimed)) => reclaimed,
    Err(_) => {
      state.count(REFERENCES_UNSETTLED, 1);
      0
    }
  }
}

/// Format: the refusal counter of a write-log span the content object refused (A-63): that write is not logged.
#[cfg(target_os = "linux")]
pub(crate) const WRITE_LOG_UNWRITTEN: &str = "recovery.write_log_unwritten";

/// Empties the shard's FUSE write log after a publication that captured every volume (A-63), stamped with that
/// publication's generation: every write before it is in that image now. A volume skipped keeps the log, so its
/// writes stay owed to the next daemon.
#[cfg(target_os = "linux")]
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
/// recovery rebuilt, in the order they were acknowledged: each to the volume whose prefix its inode carries. A write
/// that cannot be replayed is counted and the run carries on; the file then lacks it, as the record says. A write
/// refused `PublishNeeded` (the arena's freed blocks wait on a publication, A-64) publishes, puts every record not
/// yet replayed back in the log under the new image's stamp — the publication cleared the log, and a crash before
/// the replay ends must still find them — and is tried once more.
#[cfg(target_os = "linux")]
fn replay_writes(state: &mut ShardState) -> usize {
  let records = std::mem::take(&mut state.replay);
  let mut replayed: usize = 0;
  for (index, record) in records.iter().enumerate() {
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

/// Writes one logged record into the volume whose prefix its inode carries; `None` when no rebuilt volume has it.
#[cfg(target_os = "linux")]
fn replay_one(
  state: &mut ShardState,
  record: &crate::write_log::Record,
) -> Option<Result<usize, slates_vfs::VfsError>> {
  let prefix =
    u16::try_from(record.inode >> slates_vfs::ids::InodeNo::COUNTER_BITS).unwrap_or(u16::MAX);
  let handle = state
    .volumes
    .iter()
    .find(|(_, slot)| slot.volume.prefix() == prefix)
    .map(|(handle, _)| handle)?;
  let slot = state.volumes.get_mut(handle).ok()?;
  Some(slot.volume.write(
    &mut state.store,
    slates_vfs::ids::InodeNo(record.inode),
    record.offset,
    &record.bytes,
  ))
}

/// Empties the write log under the committed image's generation and appends `remaining` to it, so the records a
/// replay has not yet applied survive a crash before it ends. A record the log cannot take marks it overflowed
/// (A-63: the next daemon reports every taken-over file as having lost writes).
#[cfg(target_os = "linux")]
fn relog_remaining(state: &mut ShardState, remaining: &[crate::write_log::Record]) {
  let generation = state.journal.generation();
  let (Some(object), Some(log)) = (state.content.as_mut(), state.write_log.as_mut()) else {
    return;
  };
  let mut kept = log.clear(object, generation).is_ok();
  for record in remaining {
    if kept
      && log
        .append(object, record.inode, record.offset, &record.bytes)
        .is_err()
    {
      kept = false;
    }
  }
  if !kept {
    let _ = log.overflow(object);
    state.count(WRITE_LOG_UNWRITTEN, 1);
  }
}

/// Format: the refusal counter of a logged write recovery could not replay (A-63).
#[cfg(target_os = "linux")]
const WRITE_REPLAY_REFUSED: &str = "recovery.write_replay_refused";

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
      matches!(a.consumer, Consumer::Sdk { .. } | Consumer::Guest)
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
  let Principal::Uid { uid } = principal else {
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
  use super::{Continued, delivery_rate, next_batch};
  use super::{
    HostId, ObjectId, Principal, ReadAheadLedger, Refusal, RegionId, ReplyBody, VolumeId,
    home_redirect, join_batch, verify_attestation,
  };
  use slates_db::register::RootConfiguration;

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
  /// that continues a batch of 4 with 100 windows left and no rate measured; with 3 windows left; at 640 KiB/s
  /// delivered (10 windows of 64 KiB in the 1 s liveness budget) after a batch of 8; and at a rate too slow for one
  /// window in the budget. Expect: one window of the node's own size; 8; 3; 10; and 1 — never zero.
  #[test]
  fn a_continuing_read_doubles_its_batch_within_the_file_and_the_measured_rate() {
    let window = 64 << 10;
    let continued = |batch, total, rate| {
      Some(Continued {
        window,
        batch,
        total,
        rate,
      })
    };
    assert_eq!(next_batch(None, 0, 5_000), (5_000, 1));
    assert_eq!(
      next_batch(continued(4, 200 * window, 0), 100 * window, 1),
      (window, 8)
    );
    assert_eq!(
      next_batch(continued(4, 103 * window, 0), 100 * window, 1),
      (window, 3)
    );
    assert_eq!(
      next_batch(continued(8, 1 << 30, 640 << 10), 0, 1),
      (window, 10)
    );
    assert_eq!(next_batch(continued(8, 1 << 30, 1_000), 0, 1), (window, 1));
  }

  /// §4.8 Lookup read-ahead (`delivery_rate`). Do: rate 1 MiB streamed over half a second, nothing over a second,
  /// and bytes over no measurable interval. Expect: 2 MiB/s, and zero — no rate measured — for both degenerate
  /// cases, never a division fault.
  #[test]
  fn a_batch_delivery_rate_is_its_streamed_bytes_over_their_interval() {
    assert_eq!(delivery_rate(1 << 20, 500_000_000), 2 << 20);
    assert_eq!(delivery_rate(0, 1_000_000_000), 0);
    assert_eq!(delivery_rate(1 << 20, 0), 0);
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

  /// A-64 (the relief). Do: on a shard with a reduced reserve, write a file large enough that rewriting it leaves the
  /// arena below the operation headroom; publish (the image names its blocks); rewrite it whole, so every old block's
  /// free is deferred; then relieve. Expect: before, deferred bytes and free bytes below the headroom; after, a
  /// publication ran (`arena.deferred_relieved` moved), nothing is deferred, and the arena has the headroom free.
  #[test]
  fn a_shard_short_of_room_only_by_deferred_frees_publishes_to_release_them() {
    crate::daemon::audit_on_shard_configured(
      |state| {
        let principal = Principal::Uid { uid: 1234 };
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

  /// §4.2 (the resource vector's inode dimension; GAPS 2026-10-06). Do: on one shard, create four bounded volumes
  /// whose quotas together take the bytes the shard can admit. Expect: all four are admitted, so the inode allowances
  /// of volumes that fill the arena fit the version slab. Before, each volume asked quota ÷ `size_of::<Inode>()`
  /// slots, the first took the whole slab, and the second was refused `BudgetExceeded` with the bytes uncommitted.
  #[test]
  fn bounded_volumes_that_fill_a_shards_bytes_fit_its_version_slab() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid { uid: 1234 };
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
      let principal = Principal::Uid { uid: 1234 };
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
      let principal = Principal::Uid { uid: 1234 };
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
      let principal = Principal::Uid { uid: 1234 };
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

  /// A landing of `volume` into `target` by client 1 with no grant: a presentation, or its refusal.
  fn present_landing(
    state: &mut crate::state::ShardState,
    principal: &Principal,
    volume: super::VolumeId,
    target: &str,
  ) -> super::ReplyBody {
    crate::landing::land_verb(
      state,
      1,
      principal,
      crate::landing::LandCall {
        volume,
        snapshot: None,
        target,
        filter: &slates_ipc::protocol::Filter::default(),
        grant: None,
      },
    )
  }

  /// The landings the shard's status report counts awaiting a grant.
  fn awaiting_count(state: &mut crate::state::ShardState) -> u64 {
    super::shard_report(state).landings_awaiting
  }

  /// The `landing_planned` records the audit verb lists.
  fn planned_records(state: &mut crate::state::ShardState, principal: &Principal) -> usize {
    let reply = super::dispatch(state, 1, principal, super::RequestBody::Audit { since: 0 });
    let super::ReplyBody::Audit { records } = reply else {
      panic!("{reply:?}")
    };
    records
      .iter()
      .filter(|r| r.kind == "landing_planned")
      .count()
  }

  /// §4.15 step 3 (AUD-29-07): the landings a shard holds awaiting a grant are bounded, and a presentation
  /// past the bound is refused before it allocates anything, while a client re-presenting the same volume
  /// and target replaces its own and needs no room. Do: with the bound at one, present volume A, then B,
  /// then A again (the target is this crate's own directory, opened read-only: nothing is granted, so
  /// nothing is written). Expect: B is refused `LandingsAwaitingFull` and changes nothing — the status report
  /// still counts one presentation, the audit log still one `landing_planned`, and A's re-presentation takes
  /// the very next id, so none was spent on B; A's second presentation replaces its first (still one), and
  /// the first's id is no longer grantable. Before 2026-09-29 nothing bounded the presentations.
  #[test]
  fn a_presentation_past_the_bound_is_refused_and_changes_nothing() {
    crate::daemon::audit_on_shard(|state| {
      state.config.landings_awaiting_per_shard = 1;
      let principal = Principal::Uid { uid: 1234 };
      let target = env!("CARGO_MANIFEST_DIR");
      let a = created(state, &principal, "presented-a");
      let b = created(state, &principal, "presented-b");
      let first = present_landing(state, &principal, a, target);
      let super::ReplyBody::GrantRequired { landing: first, .. } = first else {
        panic!("{first:?}")
      };
      assert_eq!(awaiting_count(state), 1);
      let planned = planned_records(state, &principal);
      let refusal = present_landing(state, &principal, b, target);
      assert!(
        matches!(
          refusal,
          super::ReplyBody::Refused {
            refusal: Refusal::LandingsAwaitingFull
          }
        ),
        "{refusal:?}"
      );
      assert_eq!(awaiting_count(state), 1, "the refusal presented nothing");
      assert_eq!(
        planned_records(state, &principal),
        planned,
        "nor recorded anything"
      );
      let again = present_landing(state, &principal, a, target);
      let super::ReplyBody::GrantRequired { landing: again, .. } = again else {
        panic!("{again:?}")
      };
      assert_eq!(again, first + 1, "no id was spent on the refusal");
      assert_eq!(
        awaiting_count(state),
        1,
        "the re-presentation replaced the first"
      );
      assert_eq!(
        crate::landing::issue_grant(
          state,
          &principal,
          first,
          slates_ipc::protocol::GrantScope::Once,
          1
        ),
        Err(Refusal::NotFound),
        "the replaced presentation is no longer grantable"
      );
    });
  }

  /// §4.15 (AUD-29-02, A-49): a landing of a named snapshot presents that snapshot's state, whatever the head
  /// has become since, and a snapshot the volume does not hold is `NotFound` before any host access. Do:
  /// snapshot a volume holding "v1" and present the snapshot's landing (into this crate's directory, opened
  /// read-only: nothing is granted, so nothing is written); write "v2" at the head and present the snapshot
  /// again, then the head; present a snapshot the volume never had. Expect: the snapshot's second
  /// presentation carries its first's manifest and the head's another; the unknown one is `NotFound`. Before
  /// 2026-09-30 the changed head was refused `Unsupported`; before 2026-09-29 the head was landed under the
  /// snapshot's name.
  #[test]
  fn a_landing_of_a_named_snapshot_presents_that_snapshot_whatever_the_head_became() {
    crate::daemon::audit_on_shard(|state| {
      let principal = Principal::Uid { uid: 1234 };
      let reply = super::dispatch(
        state,
        1,
        &principal,
        super::RequestBody::Create {
          name: "landed".to_owned(),
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
      let file = {
        let slot = state.volumes.get_mut(handle).unwrap();
        let root = slot.volume.root();
        let file = slot
          .volume
          .create_file(&mut state.store, root, "f", 0o644)
          .unwrap();
        slot.volume.write(&mut state.store, file, 0, b"v1").unwrap();
        file
      };
      let reply = super::dispatch(
        state,
        1,
        &principal,
        super::RequestBody::Snapshot { volume: id },
      );
      let super::ReplyBody::Snapshotted { id: snapshot, .. } = reply else {
        panic!("{reply:?}")
      };
      let present = |state: &mut crate::state::ShardState, snapshot| {
        crate::landing::land_verb(
          state,
          1,
          &principal,
          crate::landing::LandCall {
            volume: id,
            snapshot,
            target: env!("CARGO_MANIFEST_DIR"),
            filter: &slates_ipc::protocol::Filter::default(),
            grant: None,
          },
        )
      };
      let manifest_of = |reply: super::ReplyBody| match reply {
        super::ReplyBody::GrantRequired { manifest, .. } => manifest,
        other => panic!("not presented: {other:?}"),
      };
      let before = manifest_of(present(state, Some(snapshot)));
      {
        let slot = state.volumes.get_mut(handle).unwrap();
        slot.volume.write(&mut state.store, file, 0, b"v2").unwrap();
      }
      assert_eq!(
        manifest_of(present(state, Some(snapshot))),
        before,
        "the snapshot presents what it froze"
      );
      assert_ne!(
        manifest_of(present(state, None)),
        before,
        "the head presents what it became"
      );
      let never = super::SnapshotId {
        value: snapshot.value.wrapping_add(1 << u32::BITS),
      };
      let unknown = present(state, Some(never));
      assert!(
        matches!(
          &unknown,
          super::ReplyBody::Refused {
            refusal: Refusal::NotFound
          }
        ),
        "a snapshot the volume does not hold: {unknown:?}"
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
      let owner = Principal::Uid { uid: 1234 };
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
      let owner = Principal::Uid { uid: 1234 };
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
        .get(&green)
        .map(|g| (g.head(), g.head_identity()));
      let record = state.db.partition().volume(green).cloned().unwrap();
      super::rebuild_green(state, &record);
      assert_eq!(
        state
          .greens
          .get(&green)
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

  /// A takeover catalog for `name` bounded at `limit` bytes, owned by uid 501, with no locked policy and no
  /// grants.
  fn takeover_catalog(name: &str, limit: u64) -> crate::catalog::CatalogValue {
    crate::catalog::CatalogValue {
      name: name.to_owned(),
      size: slates_db::catalog::SizeClass::Bounded { limit },
      names: slates_db::catalog::NamePolicy::Exact,
      owner: Principal::Uid { uid: 501 },
      require_locked: false,
      access: Vec::new(),
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
    let head = crate::head::HeadValue {
      manifest: None,
      content_holders: Vec::new(),
      sealing: None,
    };
    let taken = super::TakenOver {
      head: &head,
      catalog,
      catalog_sequence: 0,
      sequence: 1,
    };
    super::materialize_taken_over(state, id, &taken, Vec::new(), archive)
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
        principal: Principal::Uid { uid: 777 },
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
        &Principal::Uid { uid: 1234 },
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
      &Principal::Uid { uid: 1234 },
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
        &Principal::Uid { uid: 1234 },
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
        &Principal::Uid { uid: 0 },
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
    assert!(scratch.is_some_and(|reason| reason.contains("RecoveryIncomplete")));
    assert!(overlay.is_some_and(|reason| reason.contains("RecoveryIncomplete")));
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
        &Principal::Uid { uid: 0 },
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
    assert!(refusal.is_some_and(|reason| reason.contains("size policy")));
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
        let first =
          super::run_recorded(state, 1, id, 1, &Principal::Uid { uid: 0 }, request.clone());
        let retry =
          super::run_forwarded(state, 1, id, 1, &Principal::Uid { uid: 0 }, request, None);
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
    let channel = Principal::Uid { uid: ACCOUNT };
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

/// The quantile `ppm` of one of this shard's NFS service-time histograms; `None` until a call was recorded there
/// (an unmeasured time is unknown, never zero), and always `None` where the daemon serves no NFS.
#[cfg(unix)]
fn nfs_quantile(state: &ShardState, times: NfsTimes, ppm: u64) -> Option<u64> {
  let histogram = match times {
    NfsTimes::Local => &state.nfs_service.local,
    NfsTimes::LocalOffCpu => &state.nfs_service.local_off_cpu,
    NfsTimes::Forwarded => &state.nfs_service.forwarded,
  };
  (histogram.count() > 0).then(|| histogram.quantile(ppm))
}

/// Windows serves no NFS (it mounts through WinFsp): every NFS time is unknown.
#[cfg(not(unix))]
fn nfs_quantile(_state: &ShardState, _times: NfsTimes, _ppm: u64) -> Option<u64> {
  None
}
