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
use slates_db::{DbError, DurabilityScope, Partition};
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
use slates_vfs::recover::{KeyedImage, ShardImage, VolumeImage};
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
fn fresh_volume_id(state: &mut ShardState) -> DbVolumeId {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&state.fleet.host().0.to_be_bytes());
  bytes[8..10].copy_from_slice(&state.partition.to_be_bytes());
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
     bytes[capacity={} committed={} retained={} headroom={}] \
     versions[capacity={} committed={} retained={} headroom={}]",
    state.config.reserve_per_shard,
    state.config.runtime.shards,
    state.config.store.max_inodes,
    state.config.store.max_chunks,
    state.config.store.metadata_class_bytes,
    state.store.budget.capacity(),
    state.store.budget.committed(),
    state.store.budget.retained(),
    state.store.budget.headroom(),
    state.store.versions.capacity(),
    state.store.versions.committed(),
    state.store.versions.retained(),
    state.store.versions.headroom(),
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
pub(crate) fn attachment_id(partition: u16, counter: u64) -> u64 {
  (u64::from(partition) << ATTACHMENT_PARTITION_SHIFT) | counter
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
  match body {
    RequestBody::Snapshot { volume }
    | RequestBody::DestroySnapshot { volume, .. }
    | RequestBody::Versions { green: volume }
    | RequestBody::ChangedSince { green: volume, .. }
    | RequestBody::CreateWork { green: volume, .. }
    | RequestBody::Edit { work: volume, .. }
    | RequestBody::Declare { work: volume, .. }
    | RequestBody::Submit { work: volume, .. }
    | RequestBody::Rebase { work: volume }
    | RequestBody::Clone { volume, .. }
    | RequestBody::Attach { volume, .. }
    | RequestBody::Resize { volume, .. }
    | RequestBody::Destroy { volume }
    | RequestBody::Status { volume }
    | RequestBody::ReadBase { volume, .. }
    | RequestBody::Digest { volume, .. }
    | RequestBody::Rewitness { volume, .. }
    | RequestBody::Pin { volume, .. }
    | RequestBody::AwaitPlaced { volume, .. }
    | RequestBody::Read { volume, .. }
    | RequestBody::Land { volume, .. } => Some(*volume),
    // A green over a base is born on the base volume's owner shard, where the snapshot is walked.
    RequestBody::CreateGreen { base, .. } => base.map(|b| b.volume),
    RequestBody::Create { .. }
    | RequestBody::Advance { .. }
    | RequestBody::Detach { .. }
    | RequestBody::BindMount { .. }
    | RequestBody::List
    | RequestBody::DaemonStatus
    | RequestBody::DaemonStatusNext { .. }
    | RequestBody::Bootstrap { .. }
    | RequestBody::RecoveryPlan { .. }
    | RequestBody::Recover { .. }
    | RequestBody::PromoteRegion { .. }
    | RequestBody::Grants
    | RequestBody::Audit { .. }
    | RequestBody::Acknowledge { .. }
    | RequestBody::Telemetry { .. }
    | RequestBody::Grant { .. }
    | RequestBody::Enroll { .. }
    | RequestBody::Attest { .. }
    | RequestBody::Revoke { .. } => None,
    RequestBody::Share { volume, .. } => Some(*volume),
  }
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
        let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
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
    RequestBody::Versions { green } | RequestBody::ChangedSince { green, .. } => Some(*green),
    RequestBody::Status { volume } => Some(*volume),
    _ => None,
  }
}

/// Whether this node's authority over `object`'s latest state is **not** confirmed right now, and the
/// configuration version it would refuse with (§4.8 "Leases and reads"; AUD-08). Reads the fanned owner
/// lease against the object's candidate holders under the installed configuration and the host clock, so a
/// paused shard's lease has already lapsed by the clock when it resumes. `None` — confirmed, serve — when
/// `f` of the other candidates confirmed within the lease bound (or within the bounded startup allowance),
/// and on a laptop (`f = 0`, no other candidate needed). Read by the verb gate ([`dispatch`]) and the mount
/// bridge ([`crate::nfs`]).
pub(crate) fn lease_unconfirmed(state: &ShardState, object: ObjectId) -> Option<u64> {
  let config = state.fleet.configuration();
  let candidates = slates_db::register::candidates_for(
    config.owner,
    &config.neighbourhood,
    &config.domains,
    object,
    config.quorum,
  );
  let now = slates_machine::clock::monotonic_ns();
  let holds = state.lease.holds(
    now,
    config.owner,
    config.version,
    config.quorum,
    &candidates,
  );
  (!holds).then_some(config.version)
}

/// Whether a verb is a **read** safe to forward to a volume's owner without a completion record: a
/// volume-scoped query that mutates nothing, so re-serving a retried forward is idempotent (§4.8 "Lookup").
fn is_forwardable_read(body: &RequestBody) -> bool {
  matches!(
    body,
    RequestBody::Status { .. }
      | RequestBody::Versions { .. }
      | RequestBody::ChangedSince { .. }
      | RequestBody::Read { .. }
  )
}

/// Whether a verb is a **write** to an existing volume that is safe to forward to that volume's owner: a
/// volume-scoped mutation ([`volume_of`] names the volume, [`mutates_shard_image`] changes it). Forwarded
/// under a completion record keyed by the origin's request id, so a retried forward is exactly-once (§4.8
/// "Lookup"; the owner runs it through [`run_forwarded`]). A create (no existing volume, routed by name) is
/// not one of these — it is placed by the local partitioning, not a home redirect.
fn is_forwardable_write(body: &RequestBody) -> bool {
  volume_of(body).is_some() && mutates_shard_image(body)
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
    homed_elsewhere(state, volume)
      .or_else(|| superseded.then(|| state.node_regions.get(&local).map_or(0, |region| region.0)))
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
      move |s| dispatch(s, 0, &principal, body),
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
    slates_rt::futures::sleep(crate::daemon::HEARTBEAT_NS).await;
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
  let _ = state.db.mutate(
    &mut state.segment,
    &Op::CompletionsAcknowledged {
      origin,
      client,
      up_to,
    },
    now,
  );
}

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
  let cached = state
    .clients
    .get(client)
    .ok()
    .and_then(|slot| slot.owner_route);
  let origin = state.shard;
  let ack_up_to = state
    .db
    .partition()
    .acknowledged_up_to(state.origin_anchor.0, RequestId::from_word(request).client);
  let request_bytes = encode_body(&ForwardedRequest {
    principal,
    body,
    request,
    ack_up_to,
  });
  let task = SpawnRequest::new(
    Box::pin(async move {
      let resolved = crate::state::with_state(|state| {
        let query = crate::owner_location::Query::new(state, ObjectId(volume.bytes), region);
        (query, query.known_owner(state, cached))
      });
      let Some((query, known)) = resolved else {
        return;
      };
      let owner = match known {
        Some(owner) => {
          crate::state::with_state(|state| {
            *state
              .refusals
              .entry("fleet.owner_location.direct")
              .or_insert(0) += 1;
          });
          Some(owner)
        }
        None => crate::owner_location::locate(query).await.ok(),
      };
      let bytes = match owner {
        Some(owner) => {
          crate::fleet::forward_over_leader_session(
            owner,
            request_bytes,
            crate::daemon::LIVENESS_BUDGET_NS,
          )
          .await
        }
        None => None,
      };
      let reply = bytes
        .and_then(|bytes| decode_body::<ReplyBody>(&bytes).ok())
        .unwrap_or_else(|| refused(Refusal::HomedElsewhere { region }));
      let route = if matches!(&reply, ReplyBody::Refused { .. }) {
        None
      } else {
        owner.map(|owner| query.route(owner))
      };
      if origin == control {
        finish_owner_forward(client, request, reply, route);
      } else {
        let back = SpawnRequest::new(
          Box::pin(async move {
            finish_owner_forward(client, request, reply, route);
          }),
          None,
        );
        if let Err(error) =
          slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)))
        {
          crate::state::with_state(|state| {
            *state
              .refusals
              .entry("fleet.owner_location.delivery_refused")
              .or_insert(0) += 1;
          });
          eprintln!("slates-server: owner reply delivery refused: {error}");
        }
      }
    }),
    None,
  );
  if slates_rt::registry::send_control(control, Control::Spawn(Box::new(task))).is_err() {
    return Served::Reply(refused(Refusal::HomedElsewhere { region }));
  }
  Served::Forwarded
}

/// A late lookup belongs to the original client generation. It cannot populate a reused slot's
/// cache or deliver a predecessor's result to the client that now occupies that slot.
fn finish_owner_forward(
  client: Handle<ClientSlot>,
  request: u64,
  reply: ReplyBody,
  route: Option<crate::owner_location::CachedRoute>,
) {
  let present = crate::state::with_state(|state| {
    let Ok(slot) = state.clients.get_mut(client) else {
      state.forwarded_rings.remove(&request);
      *state
        .refusals
        .entry("fleet.owner_location.client_gone")
        .or_insert(0) += 1;
      return false;
    };
    slot.owner_route = route;
    true
  })
  .unwrap_or(false);
  if present {
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
      // with the committed result when the version places, never a success from memory.
      if crate::merge_service::join_awaiting(state, origin, id) {
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
    *state.refusals.entry("consumer_revoked").or_insert(0) += 1;
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
  match run_recorded(state, origin, id, client_id, &principal, body) {
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
      *state.refusals.entry(refusal_name(&refusal)).or_insert(0) += 1;
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
      // A retry of a submit whose acceptance still waits for its commit joins the wait.
      if crate::merge_service::join_awaiting(state, origin, id) {
        return None;
      }
    }
  }
  // The origin's `ring.request` span context, when the forward carried one (a same-node shard), is the
  // cause of this verb's `shard.op` (§4.14); a cross-node forward carries none yet, and the span says so.
  state.current_span = cause;
  let reply = run_recorded(state, origin, id, client_id, principal, body);
  state.current_span = None;
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
      let reply = crate::state::with_state(|s| {
        s.last_work_ns = s.clock.monotonic_ns();
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
        let _ = slates_rt::registry::send_control(origin, Control::Spawn(Box::new(back)));
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
    control: is_control_shard(state),
    council: group_report(state.council.is_leader(), state.council_timing),
    root: group_report(state.root.is_leader(), state.root_timing),
    tasks_refused: slates_rt::registry::with_current(|ctx| ctx.counters().admission_refused)
      .unwrap_or(0),
  }
}

/// Whether `state` is the control shard's — the first of the daemon's shards, the one that runs the
/// membership loop and drives the consensus groups (`Daemon::council_leads` observes the same shard).
fn is_control_shard(state: &ShardState) -> bool {
  state.shards.first() == Some(&state.shard)
}

/// A consensus group's status line: whether this node leads it, and the election timing it derived.
fn group_report(leads: bool, timing: slates_cluster::timing::ElectionTiming) -> GroupReport {
  GroupReport {
    leads,
    base_periods: timing.base_periods,
    span_periods: timing.span_periods,
    rtt_tail_ns: timing.broadcast_rtt_tail_ns,
    rtt_spread_ns: timing.broadcast_rtt_spread_ns,
    samples: timing.samples,
  }
}

/// The daemon's place in its fleet (§4.8), from this shard's placement authority — every shard's
/// `FleetNode` advances identically (the control shard hands it each peer state it folds) — with the
/// formed probe sessions summed over the shards' parts (only the control shard forms any) and the
/// consensus groups' state taken from the control shard's part (the only live one).
fn fleet_report(state: &ShardState, shards: &[ShardReport]) -> FleetReport {
  let configuration = state.fleet.configuration();
  let control = shards.iter().find(|shard| shard.control);
  let council = control.map_or_else(
    || group_report(state.council.is_leader(), state.council_timing),
    |shard| shard.council.clone(),
  );
  let root = control.map_or_else(
    || group_report(state.root.is_leader(), state.root_timing),
    |shard| shard.root.clone(),
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
  }
}

/// One counter summed over the serve-socket demultiplexers this shard runs (none on a laptop, or on a
/// shard other than the control shard).
fn demux_sum(
  state: &ShardState,
  counter: impl Fn(&slates_transport::demux::DemuxCounters) -> u64,
) -> u64 {
  state.demuxes.iter().fold(0u64, |sum, demux| {
    sum.saturating_add(counter(&demux.counters()))
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
    }),
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
      | RequestBody::Share { .. }
      | RequestBody::Enroll { .. }
      | RequestBody::Revoke { .. }
  )
}

fn dispatch(
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
  if let Some(volume) = serves_latest_state(&body)
    && let Some(version) = lease_unconfirmed(state, ObjectId(volume.bytes))
  {
    return refused(Refusal::LeaseUnconfirmed { version });
  }
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
  let reply = dispatch_inner(state, client_id, principal, body);
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
          state.by_id.contains_key(&volume) && !published.captured(volume)
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
    } => crate::landing::land_verb(state, principal, volume, snapshot, &target, &filter, grant),
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
    *state.refusals.entry("grant_issuer_unverified").or_insert(0) += 1;
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
    *state.refusals.entry("grant_issuer_unverified").or_insert(0) += 1;
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
    if marked.is_none() {
      return refused(Refusal::NotFound);
    }
  }
  ReplyBody::Revoked
}

/// Marks every slot on this shard bound to `consumer` revoked, so its next verb's gate — one local read
/// in `serve` — refuses before any effect (banned item 10: no cross-shard call on a write path).
fn mark_revoked(s: &mut ShardState, consumer: u64) {
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
          let _ = crate::xshard::run_on(origin, origin, move |s| {
            *s.refusals.entry(counter).or_insert(0) += 1;
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

fn wire_names(names: DbNamePolicy) -> NamePolicy {
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
    SizeClass::Bounded { limit } => match state.store.budget.reserve(limit) {
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
      metadata_credit,
      Refusal::BudgetExceeded { available },
    );
  }
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
  publish_created_volume(
    state,
    id,
    volume,
    host,
    (reservation, version_credit, metadata_credit),
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
  ),
  record: VolumeRecord,
) -> ReplyBody {
  let (reservation, version_credit, metadata_credit) = credits;
  // Capture the mount name before the record is moved into the log op below; the slot lists the
  // volume under it in the host root (a client mounts `/<name>` or reaches it by `cd <name>`).
  let name = record.name.clone();
  let now = state.clock.monotonic_ns();
  if let Err(e) = state
    .db
    .mutate(&mut state.segment, &Op::VolumeCreated { record }, now)
  {
    let _ = volume.discard_partial(&mut state.store);
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
    let _ = volume.discard_partial(&mut state.store);
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
  let slot = VolumeSlot {
    id,
    name,
    volume,
    host,
    reservation,
    version_credit,
    metadata_credit,
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
fn reserve_metadata(
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
  let Some(w) = state.works.get_mut(&work_id) else {
    return refused(Refusal::NotFound);
  };
  let path = crate::merge_service::canonical_path(path);
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
fn declare(state: &mut ShardState, principal: &Principal, work: VolumeId, op: WorkOp) -> ReplyBody {
  let work_id = match crate::merge_service::require_work(state, principal, work, true, "declare") {
    Ok((record, _)) => record.id,
    Err(refusal) => return refused(refusal),
  };
  let Some(w) = state.works.get_mut(&work_id) else {
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
    WorkOp::Unlink { path } => {
      let path = key(path);
      w.content.remove(&path);
      VolumeOp::Unlink { path }
    }
    WorkOp::Rename { from, to } => {
      let (from, to) = (key(from), key(to));
      if let Some(bytes) = w.content.remove(&from) {
        w.content.insert(to.clone(), bytes);
      }
      VolumeOp::Rename { from, to }
    }
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
  // Make room first: fold whatever the retention budget allows (never a live reader's version).
  if let Err(available) = crate::merge_service::settle_green_retention(state, green_id) {
    return refused(Refusal::BudgetExceeded { available });
  }
  if let Err(available) = crate::merge_service::secure_green_retention(state, green_id, secured) {
    report_first_budget_refusal(state, "retention", secured, available);
    return refused(Refusal::BudgetExceeded { available });
  }
  let verdict_start = state.clock.monotonic_ns();
  let outcome = {
    let Some(engine) = state.greens.get_mut(&green_id) else {
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
        return refused(refusal_of_db(&e));
      }
      // The accepted work now equals the green at the new version: its journal is consumed (the
      // increment holds it), its base moves to the version, and its content is the green's — so a
      // later edit declares against what the green holds (§4.16 "Submission"; a work with `stream`
      // submits at every auto-seal on exactly this footing). Without this a second submit would
      // re-declare the already-merged operations against the old base.
      if let (Some(engine), Some(w)) = (state.greens.get(&green_id), state.works.get_mut(&work_id))
      {
        w.base_version = version;
        w.journal.clear();
        w.content = engine
          .files()
          .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
          .collect();
      }
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
      settle_after_conflict(state, green_id, &inc.id);
      ReplyBody::Submitted {
        version: None,
        conflicts: merge_windows(&windows),
      }
    }
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
      // The work moves onto the head; the green is untouched. `build_increment` proved the work
      // exists, so this lookup finds it.
      if let Some(w) = state.works.get_mut(&work_id) {
        w.base_version = version;
        w.content = files;
        w.journal = journal;
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
    let _ = volume_core.discard_partial(&mut state.store);
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
  let slot = VolumeSlot {
    id,
    name: name.to_owned(),
    volume: volume_core,
    host: None,
    reservation,
    version_credit,
    metadata_credit,
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
  let allowed = match intent {
    Intent::Read => rights.read,
    Intent::Write => rights.write,
  };
  if !allowed {
    return forbidden("attach");
  }
  let situation = crate::transports::situation(state, &rights);
  let binding = match establish_form(&record, &situation, snapshot, intent, &form) {
    Ok(binding) => binding,
    Err(refusal) => return refused(refusal),
  };
  let consumer = binding.as_ref().map_or_else(
    || consumer_of(&form, client_id),
    |binding| binding.consumer(state.db.partition(), record.id, principal),
  );
  let consumer = match consumer {
    Ok(consumer) => consumer,
    Err(refusal) => return refused(refusal),
  };
  let borrows_mount = binding.is_some();
  let now = state.clock.monotonic_ns();
  let lease_epoch = match intent {
    Intent::Read => None,
    Intent::Write => match take_write_lease(state, &record, principal, now) {
      Ok(epoch) => Some(epoch),
      Err(refusal) => return refused(refusal),
    },
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
  let attachment = attachment_id(state.partition, state.next_attachment);
  state.next_attachment += 1;
  // The mount capability token (§4.6, §4.13; AUD-01): a random secret bound to this attachment, returned
  // to the authorized consumer and presented at the NFS mount so the loopback edge authorizes the
  // connection as this consumer with the granted `rights`. Refused (never a weak token) if the platform's
  // secure random is unavailable, the same discipline the daemon applies to its issuer secret.
  // A bind borrows the parent's authority; it must not mint an independent mount capability.
  let Some(token) = (if borrows_mount {
    Some([0; 16])
  } else {
    mint_mount_token()
  }) else {
    return refused(Refusal::BadRequest {
      reason: "secure random unavailable for the mount capability token".to_owned(),
    });
  };
  let op = Op::AttachmentAdded {
    record: AttachmentRecord {
      id: attachment,
      volume: record.id,
      consumer,
      snapshot: snapshot.map(to_db_snapshot),
      form: db_form,
      principal: principal.clone(),
      rights: granted_rights(rights, intent),
      token,
    },
  };
  if let Err(e) = state.db.mutate(&mut state.segment, &op, now) {
    return refused(refusal_of_db(&e));
  }
  capability.read_write = match intent {
    Intent::Read => ReadWritePolicy::ReadOnly,
    Intent::Write => ReadWritePolicy::ReadWrite,
  };
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
    AttachRequest::HostMount => Ok(Consumer::Bridge),
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
  intent: Intent,
  form: &AttachRequest,
) -> Result<Option<crate::oci::Binding>, Refusal> {
  let (source, destination) = match form {
    // The record forms: nothing to establish — the SDK's record, and the host mount the requesting
    // process establishes itself with the capability the reply carries (`mount_nfs`, R10: no privilege
    // and nothing of the daemon's touches the mount table).
    AttachRequest::Root | AttachRequest::HostMount => return Ok(None),
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
  if snapshot.is_some() {
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
  match end_attachment(state, &record) {
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
    let AttachForm::ChosenPath { path } = &record.form else {
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
      path: path.clone(),
    });
  }
  (mounts, elided)
}

/// Ends an attachment on its owner shard — the `detach` verb's effect, and the kernel's `UMNT` of a host
/// mount's (§4.6, §4.13; AUD-01): the record removed as a recorded operation, a green pin dropped, and,
/// when it was the holder's last attachment of the volume and the holder holds the write lease, the
/// lease released (D-16). The caller has authorized the end — the verb by the principal, the `UMNT` by
/// the mount capability. The typed database refusal when the removal could not be recorded.
pub(crate) fn end_attachment(
  state: &mut ShardState,
  record: &AttachmentRecord,
) -> Result<(), DbError> {
  let now = state.clock.monotonic_ns();
  state.db.mutate(
    &mut state.segment,
    &Op::AttachmentRemoved { id: record.id },
    now,
  )?;
  crate::merge_service::forget_attachment(state, record.id);
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
    let _ = state.db.mutate(
      &mut state.segment,
      &Op::LeaseReleased {
        volume: record.volume,
      },
      now,
    );
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
  if let Ok(slot) = state.volumes.get_mut(handle)
    && let Err(e) = slot.volume.destroy(&mut state.store)
  {
    return refused(refusal_of_vfs(&e));
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
fn reconcile_unpublished_effects(state: &mut ShardState) -> usize {
  let orphans: Vec<(DbVolumeId, Handle<VolumeSlot>)> = state
    .by_id
    .iter()
    .filter(|(id, _)| state.db.partition().volume(**id).is_none())
    .map(|(id, h)| (*id, *h))
    .collect();
  let budget = state
    .config
    .runtime
    .step_budget_ns
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
  state.works.retain(|id, _| partition.volume(*id).is_some());
  orphans.len()
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
        release_slot_credits(state, &slot);
      }
      state.by_id.remove(&id);
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
    mirror_age_ns: None,
    host_epoch: state.fleet.configuration().host_epoch.0,
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
    Scope::Mirror => match config.await_placed(
      DurabilityScope::Mirror,
      &committed_placement(state, object, record.epoch),
    ) {
      Ok(placed) => ReplyBody::Placed {
        placed,
        mirror_age_ns: None,
      },
      Err(slates_db::register::RegisterError::Unsupported { .. }) => {
        refused(Refusal::Unsupported {
          feature: "mirror".to_owned(),
        })
      }
      Err(_) => refused(Refusal::NotFound),
    },
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
    *state.refusals.entry(GREEN_TAKEOVER_MISMATCH).or_insert(0) += 1;
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

pub(crate) fn materialize_taken_over(
  state: &mut ShardState,
  id: DbVolumeId,
  head: &crate::head::HeadValue,
  sequence: u64,
  region: Vec<u64>,
  archive: &slates_archive::Archive,
) -> Result<(), Box<ReplyBody>> {
  if state.by_id.contains_key(&id) {
    return Ok(());
  }
  if let Some(existing) = state.db.partition().volume_by_name(&head.name) {
    return Err(Box::new(refused(Refusal::AlreadyExists {
      existing: to_wire_volume(existing.id),
    })));
  }
  let restored = slates_archive::restore(archive).map_err(|e| {
    refused(Refusal::BadRequest {
      reason: format!("taken-over content archive: {e}"),
    })
  })?;
  let size = match head.size {
    DbSizeClass::Bounded { limit } => SizeClass::Bounded { limit },
    DbSizeClass::Dynamic { max } => SizeClass::Dynamic { max },
  };
  let reservation = match size {
    SizeClass::Bounded { limit } => match state.store.budget.reserve(limit) {
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
  let names = wire_names(head.names);
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
  if let Err(e) = admit_dimensions(state, &mut volume, size) {
    let _ = volume.discard_partial(&mut state.store);
    return Err(Box::new(give_back(
      state,
      reservation,
      version_credit,
      metadata_credit,
      refusal_of_vfs(&e),
    )));
  }
  if let Err(e) = populate_restored(&mut state.store, &mut volume, &restored) {
    let _ = volume.discard_partial(&mut state.store);
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
    name: head.name.clone(),
    owner_shard: state.partition,
    policy: PolicyRecord {
      size: head.size,
      names: head.names,
      require_locked: false,
      role: Role::Plain,
    },
    base: BaseRecord::Scratch,
    head: DbSnapshotId::default(),
    // The seal below advances the epoch to the adopted sequence, so the head register continues from it.
    epoch: sequence.saturating_sub(1),
    referenced_bytes: 0,
    unique_bytes: 0,
    state: VolumeState::Live,
    lease: None,
    owner: head.owner.clone(),
    access: Vec::new(),
    created_ns: now,
  };
  let published = publish_created_volume(
    state,
    id,
    volume,
    None,
    (reservation, version_credit, metadata_credit),
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
/// its bytes the target) as a link. The archive's own metadata carries the permission bits and times; the
/// inode numbers are this volume's; the source inode number groups hard links so a takeover keeps
/// every name of one file or IPC endpoint attached to the same new inode (A-26).
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
  for (path, bytes) in &restored.files {
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
        if !bytes.is_empty() {
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
        let target =
          std::str::from_utf8(bytes).map_err(|_| slates_vfs::VfsError::RecoveryIncomplete)?;
        volume.symlink(store, parent, &name, target)?
      }
      Some(Kind::File) => {
        let no = volume.create_file(store, parent, &name, permissions_of_mode(meta.mode))?;
        if !bytes.is_empty() {
          volume.write(store, no, 0, bytes)?;
        }
        no
      }
      Some(Kind::Dir) | None => return Err(slates_vfs::VfsError::RecoveryIncomplete),
    };
    inodes.insert(meta.ino, no);
  }
  // Linking changes ctime. Restore attributes only after the complete namespace exists.
  for (path, meta) in &restored.metadata {
    if restored.files.contains_key(path)
      && let Some(&no) = inodes.get(&meta.ino)
    {
      restore_owner_and_times(store, volume, no, meta)?;
    }
  }
  // The directories' owners and times last: populating a directory moves its times, and a directory
  // is only whole once its entries are in. Deepest first, so a parent's stamp follows its children's.
  for path in restored.directories.iter().rev() {
    if let (Some(handle), Some(meta)) = (directories.get(path), restored.metadata.get(path)) {
      let no = store.dirs.get(*handle)?.inode;
      restore_owner_and_times(store, volume, no, meta)?;
    }
  }
  // The root has no entry naming it; its own metadata rides the archive's head (format minor 2), so
  // the rebuilt volume's root carries the mode, owner and times the origin's did — which, under the
  // export's POSIX access control, is what lets the owner into their taken-over volume at all.
  let root = volume.root_inode(store)?;
  volume.chmod(store, root, permissions_of_mode(restored.root.mode))?;
  restore_owner_and_times(store, volume, root, &restored.root)?;
  Ok(())
}

/// Validates archived inode groups before touching the replacement volume. A repeated inode is a
/// hard link only when kind, attributes and bytes agree; a hostile archive cannot alias unlike nodes.
fn validate_restored_nodes(
  restored: &slates_archive::Restored,
) -> Result<(), slates_vfs::VfsError> {
  use slates_vfs::{VfsError, export::kind_of_mode, inode::Kind};
  let mut groups: std::collections::BTreeMap<u64, (&slates_archive::NodeMeta, &[u8], u32)> =
    std::collections::BTreeMap::new();
  for (path, bytes) in &restored.files {
    let meta = restored
      .metadata
      .get(path)
      .ok_or(VfsError::RecoveryIncomplete)?;
    match kind_of_mode(meta.mode) {
      Some(Kind::Fifo | Kind::Socket) if meta.size == 0 && bytes.is_empty() => {}
      Some(Kind::File | Kind::Symlink) if meta.size == bytes.len() as u64 => {}
      _ => return Err(VfsError::RecoveryIncomplete),
    }
    let group = groups.entry(meta.ino).or_insert((meta, bytes, 0));
    if group.0 != meta || group.1 != bytes {
      return Err(VfsError::RecoveryIncomplete);
    }
    group.2 = group.2.checked_add(1).ok_or(VfsError::RecoveryIncomplete)?;
  }
  if groups.values().any(|(meta, _, count)| meta.nlink != *count) {
    return Err(VfsError::RecoveryIncomplete);
  }
  Ok(())
}

/// Gives a restored node the owner and the times its archive metadata carries — the owner first,
/// since a `chown` marks the change time, and the times last so the archived stamps win.
fn restore_owner_and_times(
  store: &mut slates_vfs::volume::Store,
  volume: &mut Volume,
  no: slates_vfs::ids::InodeNo,
  meta: &slates_archive::NodeMeta,
) -> Result<(), slates_vfs::VfsError> {
  volume.chown(store, no, meta.uid, meta.gid)?;
  let mtime = i64::try_from(meta.mtime_ns).unwrap_or(i64::MAX);
  let ctime = i64::try_from(meta.ctime_ns).unwrap_or(i64::MAX);
  volume.set_times(store, no, Some(mtime), Some(mtime), Some(ctime))
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
  /// Merge volumes rebuilt (§4.16): greens with their persisted chain replayed, works reset to a
  /// fresh clone of their green's head (their scratch edits did not survive).
  pub merge_volumes: usize,
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
          rebuilt.snapshots_trimmed += trim_unrecorded_snapshots(state, record.id);
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
  rebuilt.destroys_completed = complete_recovered_destroys(state);
  rebuilt.pins_reconciled = reconcile_clone_pins(state);
  rebuilt
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
  let mut completed = 0;
  for id in destroying {
    if state
      .db
      .mutate(&mut state.segment, &Op::VolumeDestroyed { id }, now)
      .is_ok()
    {
      completed += 1;
    }
  }
  completed
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
      *recorded
        .entry((edge.origin_volume.bytes, edge.origin_snapshot.value))
        .or_insert(0) += 1;
    }
  }
  let rebuilt: Vec<(DbVolumeId, Handle<VolumeSlot>)> =
    state.by_id.iter().map(|(id, h)| (*id, *h)).collect();
  let mut changed = 0;
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
        held += 1;
        changed += 1;
      }
      while held > wanted && slot.volume.unpin(snapshot).is_ok() {
        held -= 1;
        changed += 1;
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
  let chain: Vec<Vec<u8>> = state.db.partition().green_chain(record.id).to_vec();
  // A base-seeded green re-seeds its origin (version 0) before the chain replays over it; a corrupt
  // origin stops the rebuild there, logged, rather than replaying the chain over an empty version 0.
  let mut green = match state.db.partition().green_origin(record.id) {
    None => slates_merge::engine::Green::new(),
    Some(bytes) => match slates_merge::origin::Origin::decode(bytes) {
      Ok(origin) => slates_merge::engine::Green::with_origin(&origin),
      Err(e) => {
        eprintln!(
          "slates-server: partition {}: green {} origin is corrupt, replay stops: {e}",
          state.partition, record.name
        );
        state
          .greens
          .insert(record.id, slates_merge::engine::Green::new());
        return;
      }
    },
  };
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
  green.set_rejected_budget(crate::merge_service::rejected_cache_budget(state));
  state.greens.insert(record.id, green);
  // The replay rebuilt every history in full; the recovered works are reset to the head and a green's
  // attachments are reconciled out at boot, so nothing reachable lies below the head: fold to it and
  // re-take the retention the remaining histories hold, ahead of new claims (§4.2 recovery order).
  let _ = crate::merge_service::settle_green_retention(state, record.id);
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

/// What a shard publish committed (§4.8): the volumes the new image carries, and the ones it could
/// not image. A mutation of an omitted volume must refuse its stability guarantee.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Published {
  /// Volumes whose content the committed image actually carries. An absent volume is never
  /// inferred captured from an empty omission list (AUD-05).
  pub volumes: Vec<DbVolumeId>,
  /// Volumes skipped because they could not be imaged (an overlay with base-backed inodes, whose base
  /// recovery is its own gate); every other volume of the shard is in the committed image.
  pub skipped: Vec<DbVolumeId>,
  /// The committed frame's bytes (the image plus its slot header), what the slot now holds.
  pub frame_bytes: usize,
}

impl Published {
  /// Whether the committed image carries `volume`: the barrier's question for the volume a
  /// data-plane mutation touched.
  pub fn captured(&self, volume: DbVolumeId) -> bool {
    self.volumes.contains(&volume)
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
  let mut keyed = Vec::new();
  let mut published = Published::default();
  let handles: Vec<_> = state.volumes.iter().map(|(handle, _)| handle).collect();
  for handle in handles {
    let slot = state.volumes.get_mut(handle)?;
    match slot.volume.to_image(
      &state.store,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
    ) {
      Ok(image) => {
        published.volumes.push(slot.id);
        keyed.push(KeyedImage {
          key: slot.id.bytes,
          image,
        });
      }
      // Publish unaffected volumes even when another cannot be captured. Callers must check the
      // touched volume against the returned coverage; an omitted volume never receives a stable
      // acknowledgement, and recovery refuses it instead of rebuilding empty (§4.8, AUD-05).
      Err(e) => {
        crate::daemon::PUBLISH_SKIPPED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        eprintln!(
          "slates-server: partition {}: a volume was not imaged, skipped: {e}",
          state.partition
        );
        published.skipped.push(slot.id);
      }
    }
  }
  let shard = ShardImage::new(keyed);
  let Some(object) = state.content.as_mut() else {
    return Err(slates_vfs::VfsError::RecoveryIncomplete);
  };
  let Some(slice) = object.bytes_mut().get_mut(start..end) else {
    return Err(slates_vfs::VfsError::NoSpace);
  };
  match shard.write_to(slice) {
    Ok(frame_bytes) => {
      published.frame_bytes = frame_bytes;
      Ok(published)
    }
    Err(e) => {
      crate::daemon::PUBLISH_REFUSED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
      eprintln!(
        "slates-server: partition {}: shard image not published: {e}",
        state.partition
      );
      Err(e)
    }
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
    // The rebuilt volume returns its slots, blocks and retention rather than leaking them.
    let _ = volume.discard_partial(&mut state.store);
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
  // Re-acquire the version reservation (§4.2 accounting through recovery), giving back the byte
  // reservation and re-grown hold — and the rebuilt volume's slots, blocks and retention — if the
  // slab shrank.
  let version_credit =
    match recover_version_reservations(&mut state.store, allowance, reservation, held) {
      Ok(credit) => credit,
      Err(reason) => {
        let _ = volume.discard_partial(&mut state.store);
        return Err(reason);
      }
    };
  // Re-acquire the records' metadata reservation (§4.2 accounting through recovery), giving every
  // other credit and the rebuilt volume back if the ledger cannot back it.
  let journal_bytes = journal_bytes_for(state, &quota_for(size));
  let metadata_credit = match reserve_metadata(state, journal_bytes) {
    Ok(credit) => Some(credit),
    Err(refusal) => {
      let _ = volume.discard_partial(&mut state.store);
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
  let slot = VolumeSlot {
    id: record.id,
    name: record.name.clone(),
    volume,
    host,
    reservation,
    version_credit,
    metadata_credit,
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
  let mut volume = Volume::from_image(
    &mut state.store,
    image,
    Box::new(HostClock::new()),
    journal,
    source,
  )
  .map_err(|error| error.to_string())?;
  // The catalog is the authority on the acknowledged size policy (§4.8, AC-2.3): a resize
  // publishes its image before its record commits. Refuse content exceeding that policy.
  let acknowledged = quota.limit();
  if volume.capacity_bytes() != acknowledged {
    volume.resize(acknowledged).map_err(|error| {
      format!("RecoveryIncomplete: the image's quota exceeds the acknowledged size policy: {error}")
    })?;
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
  let mut trimmed = 0;
  for id in unrecorded {
    if slot.volume.destroy_snapshot(store, id).is_ok() {
      trimmed += 1;
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
    .filter(|a| matches!(a.consumer, Consumer::Sdk { .. }))
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
  use super::{
    HostId, ObjectId, Principal, Refusal, RegionId, VolumeId, home_redirect, verify_attestation,
  };
  use slates_db::register::RootConfiguration;

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
          ("pipe".to_owned(), Vec::new()),
          ("pipe-link".to_owned(), Vec::new()),
        ]
        .into(),
        metadata: [("pipe".to_owned(), meta), ("pipe-link".to_owned(), meta)].into(),
        directories: Default::default(),
        root: slates_archive::NodeMeta {
          mode: 0o040755,
          ..Default::default()
        },
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
      malformed.files.get_mut("pipe").unwrap().push(1);
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
      let scratch = super::build_recovered_volume(state, &record, None, size).err();
      record.base = super::BaseRecord::Path {
        path: ".".to_owned(),
      };
      let overlay = super::build_recovered_volume(state, &record, None, size).err();
      (scratch, overlay)
    });
    assert!(scratch.is_some_and(|reason| reason.contains("RecoveryIncomplete")));
    assert!(overlay.is_some_and(|reason| reason.contains("RecoveryIncomplete")));
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
