//! The daemon's Linux FUSE transport (§4.6 "Linux (own /dev/fuse driver)"; AUD-29-64): a volume mounted at a
//! directory its caller owns, established through the OS's `fusermount3` (R10: no privilege of slates' own)
//! and served on the volume's owner shard by the shared operation layer, so ordinary Linux programs reach the
//! volume through the kernel.
//!
//! **Establishment.** `attach` with the FUSE form allocates the attachment, then defers its reply to a task
//! (the granted landing's discipline, `crate::landing`): the task spawns the helper and awaits its answer
//! through the runtime (`slates_bridge_fuse::mount::begin_mount`, polled on the socket's readiness and the
//! heartbeat's tick; the helper killed and reaped at the failover bound or if the task is dropped), so the
//! shard never blocks on it. The mount's `fsname` is `slates:<attachment>` — the volume-specific, live
//! authority the kernel's mount table carries for the OCI handoff to check (`crate::oci`). Only once the
//! device is held is the attachment recorded, in the same database transaction as the request's completion,
//! with the mount point as its chosen path: a failed mount leaves nothing recorded (its write lease is
//! released).
//!
//! **Serving.** One task per mount on the owner shard waits for the device's readiness through the runtime,
//! then takes one request per turn (`dispatch_ready`) with a transient `VolumeBridge` over the shard's slot —
//! the mount's open-handle map persists in the mount, since FUSE keeps file handles across requests — admitted
//! under the mount's registry attachment so a barrier over the volume sees it in flight. A request that
//! changed what survives a restart (`Dispatched::needs_barrier`: namespace and attribute changes, `fsync`,
//! and the `flush` every close sends) publishes the shard's recovery image before its reply (§4.8, D-18), as
//! NFS's do; a refused or uncaptured publication answers `EIO`, never a promise of survival. The task yields
//! between requests so one busy mount never holds its shard. While the owner may not serve the volume's latest
//! state — its configuration group not ready or its lease not holding, the NFS live tree's gate (§4.8;
//! AUD-29-83) — no request is read: the kernel's requests wait, asked again each heartbeat.
//!
//! **Teardown.** The kernel's unmount ends the device's connection; the turn sees it and the attachment is
//! ended as a recorded operation (`verbs::end_attachment`). A `detach`, the volume's destroy, or the daemon's
//! stop unmounts through `fusermount3 -u -z` (owned until reaped), and the kernel's disconnect then ends the
//! serve task the same way. A serve error unmounts and ends likewise; nothing is left mounted and unserved.
//!
//! **Across a restart (A-61; AC-3.4).** The anchor holds each mount's device (`crate::fuse_hold`): sent before the
//! record commits, with the session once `INIT` is answered, released at the end. A restarted daemon serves again
//! each kept mount whose kernel can resend ([`adopt_held`]): its durable owner's references back, the restored
//! serve state, one `FUSE_NOTIFY_RESEND`. A write's bytes are logged in the write log before its reply
//! (`crate::write_log`, A-63), so one the dead daemon acknowledged after its last publication is replayed by its
//! successor, never lost; only a write the log could not take reports `EIO` at the file's next `fsync`. A barrier's reply rides its publication, so a request the dead daemon applied and never answered is
//! answered from that record when the kernel resends it, never applied twice. One channel per mount, on its
//! volume's owner shard: per-shard channels would hand most requests to a shard that cannot serve the volume
//! (A-62, measured and rejected).

use std::collections::BTreeMap;

use slates_bridge_core::scoped::ScopedBridge;
use slates_bridge_core::volume_bridge::new_handle_store;
use slates_bridge_core::{AttachmentId, Bridge, LostWrites, Rights, View, VolumeBridge};
use slates_bridge_fuse::channel::{
  Dispatched, FuseChannel, Sent, ServeState, Turn, dispatch_ready, reclaim_dispatched, send_reply,
};
use slates_bridge_fuse::mount::{
  Awaiting, Mount, MountError, PendingExit, Progress, begin_mount, begin_unmount,
};
use slates_db::Op;
use slates_db::catalog::{AttachmentRecord, Principal, VolumeId};
use slates_ipc::protocol::{AttachmentCapability, Established, Refusal, ReplyBody};
use slates_mem::slab::Slab;
use slates_rt::futures;
use slates_vfs::clock::Clock;
use slates_vfs::host::HostFs;

use crate::state::{self, ShardState};

/// Format: `EIO`, the errno a refused barrier answers a FUSE request with (the effect is in the volume, not
/// stable; the caller is told rather than promised survival).
const EIO: i32 = 5;
/// Format: the refusal-ledger names of this transport's counted outcomes.
const MOUNT_REFUSED: &str = "fuse.mount_refused";
/// Format: see [`MOUNT_REFUSED`].
const SERVE_FAILED: &str = "fuse.serve_failed";
/// Format: see [`MOUNT_REFUSED`].
const BARRIER_REFUSED: &str = "fuse.barrier_refused";
/// Format: see [`MOUNT_REFUSED`].
const UNMOUNT_REFUSED: &str = "fuse.unmount_refused";
/// Format: see [`MOUNT_REFUSED`].
const REPLY_UNDELIVERED: &str = "fuse.reply_undelivered";
/// Format: see [`MOUNT_REFUSED`] — a reply the kernel answered `ENOENT` (its caller was interrupted).
const REPLY_UNMATCHED: &str = "fuse.reply_unmatched";
/// Format: see [`MOUNT_REFUSED`] — the references and handles given back for replies that never reached their
/// caller (AUD-29-85); the non-vacuity counter of the reclaim path.
const REPLY_RECLAIMED: &str = "fuse.reply_reclaimed";
/// Format: see [`MOUNT_REFUSED`] — a hold, session or release the anchor's channel refused (A-61): that mount's
/// device is not held across a restart.
const HOLD_REFUSED: &str = "fuse.hold_refused";
/// Format: see [`MOUNT_REFUSED`] — a kept mount recovery could not serve again (A-61), unmounted and ended.
const ADOPT_REFUSED: &str = "fuse.adopt_refused";
/// Format: see [`MOUNT_REFUSED`] — the mounts served again after a restart from a held device (A-61); the
/// takeover's non-vacuity counter.
const ADOPTED: &str = "fuse.adopted";
/// Format: see [`MOUNT_REFUSED`] — requests the kernel resent after a takeover's `FUSE_NOTIFY_RESEND` (A-61): ones
/// a dead daemon read and never answered, served here; the resend's non-vacuity counter.
const RESENT: &str = "fuse.resent";
/// Format: see [`MOUNT_REFUSED`] — resent requests answered from the reply their dead daemon published and never
/// delivered (A-61); the exact replay's non-vacuity counter.
const REPLAYED: &str = "fuse.replayed";

/// A mounted volume being served: its device, the serve loop's state, the open-handle map that persists
/// across requests, the registry attachment its requests are admitted under, and where it is mounted.
pub(crate) struct FuseMount {
  channel: FuseChannel,
  serve: ServeState,
  handles: Slab<u64>,
  registry: AttachmentId,
  volume: VolumeId,
  mount_point: String,
  /// The directory a scoped mount presents (AUD-29-76): its bridge answers nothing outside it.
  scope: Option<u64>,
  /// Whether the anchor has this connection's session (A-61): sent once, after the kernel's `INIT` is answered,
  /// or already held for a mount taken over from a previous daemon.
  session_held: bool,
  /// For a mount taken over from a daemon that died, the files whose acknowledged writes the death lost (A-61):
  /// reported `EIO` once to each handle opened before the takeover. Empty for a mount this daemon established.
  lost: LostWrites,
}

impl std::fmt::Debug for FuseMount {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("FuseMount")
      .field("mount_point", &self.mount_point)
      .finish()
  }
}

/// The shard's FUSE mounts, by attachment id.
pub(crate) type FuseMounts = BTreeMap<u64, FuseMount>;

/// What an attach with the FUSE form carries into its deferred establishment: the record to commit once the
/// device is held, and the reply's pieces.
pub(crate) struct PendingAttach {
  /// The attachment record, its form the chosen path (the mount point).
  pub(crate) record: AttachmentRecord,
  /// The rights the mount's requests run under.
  pub(crate) rights: Rights,
  /// The write lease's epoch, for a write intent.
  pub(crate) lease_epoch: Option<u64>,
  /// The transport's report for the reply.
  pub(crate) capability: AttachmentCapability,
  /// The capability token.
  pub(crate) token: [u8; 16],
}

/// The mount's `fsname`: `slates:` and the attachment id in hex, the authority the kernel's mount table
/// carries for this volume's mount (`crate::oci` reads it back).
pub(crate) fn fsname_of(attachment: u64) -> String {
  format!("slates:{attachment:016x}")
}

/// Checks a FUSE mount point before any effect (§4.6 A-9's `UserOwnedExistingDirectory`): an absolute path
/// to an existing directory — not followed through a final symbolic link — owned by the daemon's user and,
/// for a Unix-user principal, by that user. `fusermount3` alone would mount over a regular file the user
/// owns, so the transport's stated constraint is the daemon's to hold. A read-only query of the path the
/// caller named; nothing is created.
pub(crate) fn check_mount_point(path: &str, principal: &Principal) -> Result<(), Refusal> {
  let refuse = |why: String| {
    Err(Refusal::TargetUnavailable {
      reason: format!("the FUSE mount point {path} {why}"),
    })
  };
  if !path.starts_with('/') {
    return refuse("is not an absolute path".to_owned());
  }
  let stat = match rustix::fs::statat(rustix::fs::CWD, path, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
  {
    Ok(stat) => stat,
    Err(e) => return refuse(format!("cannot be read (code {})", e.raw_os_error())),
  };
  if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::Directory {
    return refuse("is not a directory".to_owned());
  }
  let owner = stat.st_uid;
  if owner != rustix::process::geteuid().as_raw() {
    return refuse(format!("belongs to uid {owner}, not the daemon's user"));
  }
  if let Principal::Uid { uid } = principal
    && *uid != owner
  {
    return refuse(format!(
      "belongs to uid {owner}, not the requesting uid {uid}"
    ));
  }
  Ok(())
}

/// Defers an attach with the FUSE form to a task that mounts and then answers (see the module docs). The
/// placeholder reply the verb returns is never delivered.
pub(crate) fn defer_attach(state: &mut ShardState, pending: PendingAttach) -> ReplyBody {
  let Some(request) = state.current_request else {
    return crate::verbs::refused(Refusal::Unsupported {
      feature: "a FUSE mount outside a recorded request".to_owned(),
    });
  };
  let route = state.reply_route.take();
  let cause = state.current_span;
  let deadline_ns = state.config.failover_slo_ns;
  let shard = state.shard;
  let task = async move {
    let mount_point = pending
      .record
      .form
      .fuse_mount_point()
      .unwrap_or_default()
      .to_owned();
    // A mount shared with a container runtime's processes is `allow_other` (§4.6 A-9); the kernel still checks
    // each caller's permission bits (`default_permissions`).
    let shared = pending.record.form.shared_with_other_users();
    let mounted = mount_without_blocking(
      &mount_point,
      &fsname_of(pending.record.id),
      (deadline_ns, shared),
    )
    .await;
    let _ = state::with_state(move |s| {
      let attachment = pending.record.id;
      let reply = complete(s, request, cause, attachment, |s| match mounted {
        Ok(mount) => established(s, pending, mount),
        Err(MountError::AllowOtherNotGranted) => {
          *s.refusals.entry(MOUNT_REFUSED).or_insert(0) += 1;
          release_unmounted_lease(s, &pending.record);
          crate::verbs::refused(Refusal::AttachmentUnsupported {
            transport: slates_ipc::protocol::AttachTransport::Fuse,
            reason: slates_ipc::protocol::UnsupportedReason::AllowOtherNotGranted,
          })
        }
        Err(error) => {
          *s.refusals.entry(MOUNT_REFUSED).or_insert(0) += 1;
          release_unmounted_lease(s, &pending.record);
          crate::verbs::refused(Refusal::TargetUnavailable {
            reason: format!("the FUSE mount at {mount_point} was refused: {error}"),
          })
        }
      });
      deliver(s, reply, route);
    });
  };
  match futures::spawn(task).and_then(futures::detach) {
    Ok(()) => {
      state.acceptance_deferred = true;
      crate::landing::deferred_reply()
    }
    Err(_) => crate::verbs::refused(Refusal::Overloaded { shard }),
  }
}

/// Commits a deferred attach: its effect (`produce`) and its completion record as one database transaction
/// (`crate::landing`'s discipline). A commit refused leaves nothing durable, so a mount `produce` established
/// is unmounted and ended — no mount is served without its record.
fn complete(
  s: &mut ShardState,
  (origin, id): (u64, slates_wire::request::RequestId),
  cause: Option<slates_wire::observe::SpanContext>,
  attachment: u64,
  produce: impl FnOnce(&mut ShardState) -> ReplyBody,
) -> ReplyBody {
  let outer = std::mem::replace(&mut s.current_span, cause);
  s.db.begin();
  let reply = produce(s);
  let recorded = crate::verbs::record_completion(s, origin, id, reply);
  let reply = match s.db.commit(&mut s.segment) {
    Ok(_) => recorded,
    Err(e) => {
      let refusal = crate::error::refusal_of_db(&e);
      *s.refusals
        .entry(crate::verbs::refusal_name(&refusal))
        .or_insert(0) += 1;
      crate::verbs::reconcile_unpublished_effects(s);
      if let Some(mount) = s.fuse_mounts.get(&attachment) {
        unmount_owned(&mount.mount_point);
      }
      ended(s, attachment);
      crate::verbs::refused(refusal)
    }
  };
  s.current_span = outer;
  reply
}

/// Hands a deferred reply to its client's shard (`crate::landing`'s delivery); a reply that cannot be handed
/// over is counted, and the client's retry answers from the completion record.
fn deliver(s: &mut ShardState, reply: ReplyBody, route: Option<crate::merge_service::ReplyRoute>) {
  let Some(route) = route else {
    return;
  };
  let delivery = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      state::deliver(route.client_index, route.request, reply, true);
    }),
    None,
  );
  if slates_rt::registry::send_control(
    route.shard,
    slates_rt::control::Control::Spawn(Box::new(delivery)),
  )
  .is_err()
  {
    *s.refusals.entry(REPLY_UNDELIVERED).or_insert(0) += 1;
  }
}

/// Mounts at `mount_point` through the OS helper without blocking the shard: the helper's answer awaited on
/// its socket's readiness, its exit on the heartbeat's tick, all within `deadline_ns`.
async fn mount_without_blocking(
  mount_point: &str,
  fsname: &str,
  (deadline_ns, shared): (u64, bool),
) -> Result<Mount, MountError> {
  let extra: &[&str] = if shared { &["allow_other"] } else { &[] };
  let mut pending = begin_mount(mount_point, fsname, extra)?;
  let began = futures::now_ns();
  let tick = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;
  loop {
    let left = deadline_ns.saturating_sub(futures::now_ns().saturating_sub(began));
    if left == 0 {
      return Err(pending.abort(std::time::Duration::from_nanos(deadline_ns)));
    }
    match pending.poll() {
      Progress::Done(device) => return device.and_then(|device| Mount::adopt(device, mount_point)),
      Progress::Waiting(Awaiting::Socket) => {
        let socket = std::os::fd::AsRawFd::as_raw_fd(&pending.socket());
        // A timed-out or refused wait is followed by the next poll, which aborts at the deadline.
        let _ = futures::within(left, slates_rt::readiness::readable(socket)).await;
      }
      Progress::Waiting(Awaiting::Exit) => {
        if futures::sleep(tick.min(left)).await.is_err() {
          return Err(pending.abort(std::time::Duration::from_nanos(deadline_ns)));
        }
      }
    }
  }
}

/// The mount is held: the attachment recorded (inside the completion's transaction), its registry
/// attachment admitted, the mount kept on the shard and its serve task started; the reply names the mount
/// point. A recording or a spawn refused unmounts and refuses.
fn established(s: &mut ShardState, pending: PendingAttach, mount: Mount) -> ReplyBody {
  let (channel, mount_point) = mount.into_parts();
  let attachment = pending.record.id;
  let volume = pending.record.volume;
  let refuse = |s: &mut ShardState, mount_point: &str, refusal: Refusal| {
    unmount_owned(mount_point);
    release_unmounted_lease(s, &pending.record);
    crate::verbs::refused(refusal)
  };
  // The anchor holds the device before the record commits (A-61), so a recorded mount's device outlives this
  // process. A hold refused leaves a mount that ends with the process, counted; it is still served.
  if matches!(
    crate::fuse_hold::hold(attachment, std::os::fd::AsFd::as_fd(&channel.device())),
    Some(Err(_))
  ) {
    *s.refusals.entry(HOLD_REFUSED).or_insert(0) += 1;
  }
  let now = s.clock.monotonic_ns();
  let op = Op::AttachmentAdded {
    record: pending.record.clone(),
  };
  if let Err(e) = s.db.mutate(&mut s.segment, &op, now) {
    let _ = crate::fuse_hold::release(attachment);
    return refuse(s, &mount_point, crate::error::refusal_of_db(&e));
  }
  let registry = match s.attachments.attach(
    volume,
    View::Current,
    pending.record.principal.clone(),
    pending.rights,
  ) {
    Ok(registry) => registry,
    Err(e) => return refuse(s, &mount_point, crate::error::refusal_of_vfs(&e)),
  };
  // The serve turn delivers the volume's invalidations before each request (AUD-29-79); the kernel's references
  // are the record's, so they are carried across a restart (A-61).
  s.attachments
    .set_coherence(registry, slates_bridge_core::CacheCoherence::Invalidated);
  s.attachments.set_owner(registry, attachment);
  s.fuse_mounts.insert(
    attachment,
    FuseMount {
      channel,
      serve: ServeState::new(),
      handles: new_handle_store(),
      registry,
      volume,
      mount_point: mount_point.clone(),
      scope: pending.record.form.scope(),
      session_held: false,
      lost: LostWrites::default(),
    },
  );
  if futures::spawn(serve(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    s.fuse_mounts.remove(&attachment);
    s.attachments.revoke(registry);
    s.attachments.drain(registry);
    return refuse(s, &mount_point, Refusal::Overloaded { shard: s.shard });
  }
  ReplyBody::Attached {
    attachment,
    lease_epoch: pending.lease_epoch,
    path: Some(mount_point),
    version: None,
    established: Established::Record,
    capability: pending.capability,
    token: Some(pending.token),
  }
}

/// A write intent's lease, taken before the mount, released when no attachment of the holder came of it.
fn release_unmounted_lease(s: &mut ShardState, record: &AttachmentRecord) {
  let holds_another = s
    .db
    .partition()
    .attachments_of(record.volume)
    .iter()
    .any(|a| a.principal == record.principal);
  let lease_is_ours = s
    .db
    .partition()
    .volume(record.volume)
    .and_then(|v| v.lease.as_ref())
    .is_some_and(|l| l.holder == record.principal);
  if !holds_another && lease_is_ours {
    let now = s.clock.monotonic_ns();
    let _ = s.db.mutate(
      &mut s.segment,
      &Op::LeaseReleased {
        volume: record.volume,
      },
      now,
    );
  }
}

/// What one turn did.
enum Turned {
  /// No request was waiting.
  Idle,
  /// A request was served (or a malformed one dropped).
  Served,
  /// The owner may not serve the volume's latest state now (AUD-29-83): nothing was read from the device, so
  /// the kernel's requests wait for the lease.
  Fenced,
  /// The mount ended.
  Ended,
}

/// The serve task of one mount: wait for the device's readiness, take requests until none waits, yield
/// between them; end the attachment when the mount ends.
async fn serve(attachment: u64) {
  loop {
    let Some(Some(raw)) = state::with_state(|s| {
      s.fuse_mounts
        .get(&attachment)
        .map(|m| m.channel.raw_device())
    }) else {
      return;
    };
    if slates_rt::readiness::readable(raw).await.is_err() {
      let _ = state::with_state(|s| fail(s, attachment));
      return;
    }
    loop {
      match state::with_state(|s| turn(s, attachment)) {
        Some(Turned::Idle) => break,
        Some(Turned::Served) => {
          let _ = futures::yield_now().await;
        }
        Some(Turned::Fenced) => {
          // The requests wait in the kernel, asked again each heartbeat — the cadence at which the
          // confirmations that restore the lease arrive — as the guest device and a hard NFS mount wait.
          crate::daemon::pace(crate::daemon::HEARTBEAT_NS).await;
          break;
        }
        Some(Turned::Ended) | None => {
          let _ = state::with_state(|s| ended(s, attachment));
          return;
        }
      }
    }
  }
}

/// One turn: one request dispatched over a transient bridge on the volume's slot, its barrier run when it
/// changed what survives a restart, its reply (or `EIO`) written.
fn turn(s: &mut ShardState, attachment: u64) -> Turned {
  let Some(mut mount) = s.fuse_mounts.remove(&attachment) else {
    return Turned::Ended;
  };
  let Some(&handle) = s.by_id.get(&mount.volume) else {
    unmount_owned(&mount.mount_point);
    return Turned::Ended;
  };
  // The live-tree fence the NFS mount applies before every procedure (§4.8 "Leases and reads"; AUD-29-83):
  // while the owner's lease does not hold, no request is read, so none is answered from a stale view.
  if crate::verbs::live_tree_fenced(s, mount.volume) {
    s.fuse_mounts.insert(attachment, mount);
    return Turned::Fenced;
  }
  let dispatched = {
    let ShardState {
      store,
      volumes,
      attachments,
      ..
    } = &mut *s;
    let Ok(slot) = volumes.get_mut(handle) else {
      unmount_owned(&mount.mount_point);
      return Turned::Ended;
    };
    let mut bridge = VolumeBridge::attached(
      mount.volume,
      &mut slot.volume,
      store,
      &mut mount.handles,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
    )
    .with_lost_writes(&mut mount.lost);
    let mut scoped = None;
    dispatch_ready(
      &mut mount.channel,
      scoped_or_whole(&mut bridge, &mut scoped, mount.scope),
      attachments,
      mount.registry,
      &mut mount.serve,
    )
  };
  hand_over_session(s, attachment, &mut mount);
  let turned = match dispatched {
    Ok(Turn::Idle) => Turned::Idle,
    Ok(Turn::Dropped) => Turned::Served,
    Ok(Turn::Replayed) => {
      let count = s.refusals.entry(REPLAYED).or_insert(0);
      *count = count.saturating_add(1);
      Turned::Served
    }
    Ok(Turn::Ended) => return Turned::Ended,
    Ok(Turn::Dispatched(dispatched)) => {
      log_write(s, &dispatched);
      if dispatched.resent() {
        let count = s.refusals.entry(RESENT).or_insert(0);
        *count = count.saturating_add(1);
      }
      reply(s, attachment, &mut mount, &dispatched)
    }
    Err(_) => {
      *s.refusals.entry(SERVE_FAILED).or_insert(0) += 1;
      unmount_owned(&mount.mount_point);
      return Turned::Ended;
    }
  };
  s.fuse_mounts.insert(attachment, mount);
  turned
}

/// The bridge a mount's requests are served through: held to `scope` when the mount presents one directory
/// (AUD-29-76), else the volume's whole bridge. `scoped` is the caller's slot the scoped bridge lives in.
fn scoped_or_whole<'a>(
  bridge: &'a mut VolumeBridge<'_>,
  scoped: &'a mut Option<ScopedBridge<'a>>,
  scope: Option<u64>,
) -> &'a mut dyn Bridge {
  match scope {
    Some(scope) => scoped.insert(ScopedBridge::new(bridge, scope)),
    None => bridge,
  }
}

/// Runs a dispatched request's barrier when it needs one, then writes its reply or `EIO`.
fn reply(
  s: &mut ShardState,
  attachment: u64,
  mount: &mut FuseMount,
  dispatched: &Dispatched,
) -> Turned {
  let refuse = if dispatched.needs_barrier() {
    // The reply rides the publication with its effect (A-61), so a daemon that dies before writing it leaves its
    // successor the answer for the request the kernel resends, never a second application.
    s.pending_replies.insert(
      attachment,
      slates_vfs::recover::HeldReply {
        attachment,
        unique: dispatched.unique(),
        reply: dispatched.reply(&mount.serve).to_vec(),
      },
    );
    let captured =
      crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(mount.volume));
    if captured {
      None
    } else {
      *s.refusals.entry(BARRIER_REFUSED).or_insert(0) += 1;
      Some(EIO)
    }
  } else {
    None
  };
  // A refusal overwrites the reply, so what it granted is given back first (AUD-29-85).
  if refuse.is_some() {
    reclaim(s, mount, dispatched);
  }
  let sent = send_reply(&mount.channel, &mut mount.serve, dispatched, refuse);
  // Written (or refused, or failed): nothing of this request remains to answer from the record.
  s.pending_replies.remove(&attachment);
  match sent {
    Ok(Sent::Delivered) => Turned::Served,
    Ok(Sent::Unmatched) => {
      *s.refusals.entry(REPLY_UNMATCHED).or_insert(0) += 1;
      if refuse.is_none() {
        reclaim(s, mount, dispatched);
      }
      Turned::Served
    }
    Err(_) => {
      *s.refusals.entry(SERVE_FAILED).or_insert(0) += 1;
      unmount_owned(&mount.mount_point);
      Turned::Ended
    }
  }
}

/// Gives back what `dispatched`'s success reply granted — its lookup references and open handle — over a
/// transient bridge on the volume's slot, since the reply will not reach its caller (AUD-29-85).
fn reclaim(s: &mut ShardState, mount: &mut FuseMount, dispatched: &Dispatched) {
  let Some(&handle) = s.by_id.get(&mount.volume) else {
    return;
  };
  let ShardState {
    store,
    volumes,
    attachments,
    refusals,
    ..
  } = &mut *s;
  let Ok(slot) = volumes.get_mut(handle) else {
    return;
  };
  let mut bridge = VolumeBridge::attached(
    mount.volume,
    &mut slot.volume,
    store,
    &mut mount.handles,
    slot.host.as_mut().map(|host| host as &mut dyn HostFs),
  );
  let mut scoped = None;
  let reclaimed = reclaim_dispatched(
    &mount.serve,
    dispatched,
    scoped_or_whole(&mut bridge, &mut scoped, mount.scope),
    attachments,
    mount.registry,
  );
  let given_back = reclaimed.references.saturating_add(reclaimed.handles);
  if given_back > 0 {
    let count = refusals.entry(REPLY_RECLAIMED).or_insert(0);
    *count = count.saturating_add(given_back);
  }
}

/// A serve task that cannot wait on its device: the mount is unmounted and its attachment ended.
fn fail(s: &mut ShardState, attachment: u64) {
  *s.refusals.entry(SERVE_FAILED).or_insert(0) += 1;
  if let Some(mount) = s.fuse_mounts.get(&attachment) {
    unmount_owned(&mount.mount_point);
  }
  ended(s, attachment);
}

/// Logs a write's bytes in the shard's write log before its reply (A-63, `crate::write_log`): the write is
/// acknowledged before the next publication, so a daemon that dies first must leave its bytes for its successor to
/// replay. Logged before the reply, so a write applied and never answered is resent, not replayed twice over a
/// different later write (the kernel resends it with the same bytes at the same offset). A log with no room forces a
/// publication, which carries this write (already applied) and empties the log; only a refused publication leaves the
/// write unlogged, marked as an overflow so the next daemon reports the loss rather than replaying around it.
fn log_write(s: &mut ShardState, dispatched: &Dispatched) {
  if dispatched.opcode != Some(slates_bridge_fuse::Opcode::Write) || dispatched.error != 0 {
    return;
  }
  let Some((offset, data)) = dispatched.written() else {
    return;
  };
  let appended = match (s.write_log.as_mut(), s.content.as_mut()) {
    (Some(log), Some(object)) => log.append(object, dispatched.nodeid(), offset, data),
    _ => return,
  };
  let logged = match appended {
    Ok(()) => true,
    Err(crate::write_log::Refused::Full) => {
      // The publication carries this write and, capturing every volume, empties the log.
      crate::verbs::publish_shard(s).is_ok_and(|published| published.skipped.is_empty())
    }
    Err(crate::write_log::Refused::Unwritten) => false,
  };
  if !logged {
    *s.refusals
      .entry(crate::verbs::WRITE_LOG_UNWRITTEN)
      .or_insert(0) += 1;
    if let (Some(log), Some(object)) = (s.write_log.as_mut(), s.content.as_mut()) {
      let _ = log.overflow(object);
    }
  }
}

/// Sends the anchor this mount's session once its `INIT` has been answered (A-61), so a restarted daemon can
/// serve the device without the negotiation the kernel will not repeat.
fn hand_over_session(s: &mut ShardState, attachment: u64, mount: &mut FuseMount) {
  if mount.session_held {
    return;
  }
  let Some(session) = mount.serve.session() else {
    return;
  };
  mount.session_held = true;
  if matches!(crate::fuse_hold::session(attachment, session), Some(Err(_))) {
    *s.refusals.entry(HOLD_REFUSED).or_insert(0) += 1;
  }
}

/// Serves again every FUSE mount recovery kept because the anchor held its device (A-61), and closes every held
/// device recovery found no record for. Each kept mount gets a registry attachment from its record (its rights,
/// its durable owner, so the references recovery gave back are its own), the serve state its session restored,
/// and one `FUSE_NOTIFY_RESEND`, so the requests the dead daemon read and never answered come back; then its
/// serve task. A mount that cannot be served again is unmounted and ended, counted.
pub(crate) fn adopt_held() {
  let (kept, orphaned) = state::with_state(|s| {
    (
      std::mem::take(&mut s.adopt_fuse),
      std::mem::take(&mut s.inherited_fuse),
    )
  })
  .unwrap_or_default();
  for held in orphaned {
    let _ = crate::fuse_hold::release(held.attachment);
  }
  for (record, held) in kept {
    let _ = state::with_state(move |s| adopt_one(s, record, held));
  }
}

/// Serves one kept mount again, or ends it.
fn adopt_one(s: &mut ShardState, record: AttachmentRecord, held: crate::fuse_hold::HeldDevice) {
  let attachment = record.id;
  let mount_point = record
    .form
    .fuse_mount_point()
    .unwrap_or_default()
    .to_owned();
  let end = |s: &mut ShardState| {
    *s.refusals.entry(ADOPT_REFUSED).or_insert(0) += 1;
    unmount_owned(&mount_point);
    let _ = crate::fuse_hold::release(attachment);
    if crate::verbs::end_attachment(s, &record).is_err() {
      *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1;
    }
  };
  let Some(session) = held.session else {
    return end(s);
  };
  let Ok(channel) = FuseChannel::nonblocking(held.device) else {
    return end(s);
  };
  let rights = Rights {
    read: record.rights.read,
    write: record.rights.write,
  };
  let Ok(registry) = s.attachments.attach(
    record.volume,
    View::Current,
    record.principal.clone(),
    rights,
  ) else {
    return end(s);
  };
  s.attachments
    .set_coherence(registry, slates_bridge_core::CacheCoherence::Invalidated);
  s.attachments.set_owner(registry, attachment);
  let mut resend = [0u8; slates_bridge_fuse::abi::OUT_HEADER_LEN];
  let resent = slates_bridge_fuse::notify::resend(&mut resend)
    .ok()
    .and_then(|len| resend.get(..len))
    .is_some_and(|message| channel.write_reply(message).is_ok());
  if !resent {
    s.attachments.revoke(registry);
    s.attachments.drain(registry);
    return end(s);
  }
  s.fuse_mounts.insert(
    attachment,
    FuseMount {
      channel,
      serve: ServeState::restored(session, s.recovered_replies.remove(&attachment)),
      handles: new_handle_store(),
      registry,
      volume: record.volume,
      mount_point: mount_point.clone(),
      scope: record.form.scope(),
      session_held: true,
      lost: LostWrites::new(std::collections::BTreeSet::new(), s.writes_lost),
    },
  );
  if futures::spawn(serve(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    s.fuse_mounts.remove(&attachment);
    s.attachments.revoke(registry);
    s.attachments.drain(registry);
    return end(s);
  }
  let count = s.refusals.entry(ADOPTED).or_insert(0);
  *count = count.saturating_add(1);
}

/// The mount has ended: its device dropped, its registry attachment revoked and drained, and its catalog
/// attachment ended as a recorded operation (unless a `detach` or destroy already ended it).
fn ended(s: &mut ShardState, attachment: u64) {
  if let Some(mount) = s.fuse_mounts.remove(&attachment) {
    s.attachments.revoke(mount.registry);
    s.attachments.drain(mount.registry);
  }
  // The anchor closes its copy of the device (A-61), so an ended mount is never handed to the next daemon.
  if matches!(crate::fuse_hold::release(attachment), Some(Err(_))) {
    *s.refusals.entry(HOLD_REFUSED).or_insert(0) += 1;
  }
  if let Some(record) = s.db.partition().attachment(attachment).cloned()
    && crate::verbs::end_attachment(s, &record).is_err()
  {
    *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1;
  }
}

/// Unmounts the FUSE mount of `attachment` if the shard serves one (a `detach`, the volume's destroy): the
/// kernel's disconnect then ends the serve task, which ends what remains.
pub(crate) fn unmount_if_mounted(s: &ShardState, attachment: u64) {
  if let Some(mount) = s.fuse_mounts.get(&attachment) {
    unmount_owned(&mount.mount_point);
  }
}

/// Unmounts every FUSE mount the shard serves (the daemon's stop), waiting for each helper within the
/// failover bound. Run by the end of the shard's serve loop (`daemon::EndMounts`): on a stop nothing else
/// runs on the shard, and an unmount left to a task the stop is about to end would leave a dead mount behind.
pub(crate) fn unmount_all(s: &mut ShardState) {
  unmount_points(s);
  let attachments: Vec<u64> = s.fuse_mounts.keys().copied().collect();
  for attachment in attachments {
    ended(s, attachment);
  }
}

/// Unmounts every FUSE mount point the shard serves, waiting for each helper within the failover bound, and
/// writes no record: [`unmount_all`]'s first half, and all a fenced shard may do at its stop.
pub(crate) fn unmount_points(s: &mut ShardState) {
  let deadline = std::time::Duration::from_nanos(s.config.failover_slo_ns);
  let points: Vec<String> = s
    .fuse_mounts
    .values()
    .map(|m| m.mount_point.clone())
    .collect();
  for point in points {
    if let Ok(mut pending) = begin_unmount(&point) {
      let began = std::time::Instant::now();
      while pending.poll().is_none() && began.elapsed() < deadline {
        std::thread::yield_now();
      }
    }
  }
}

/// Unmounts the dead FUSE mounts recovery ended (AUD-29-64): the mounts a killed daemon served, whose device
/// died with it, so each answers `ENOTCONN` until unmounted. Run once the shard runs; each is unmounted only if
/// the kernel's table shows exactly that mount there — `fuse.slates` with the record's attachment as its source
/// — so a mount someone made at the path since is never touched. A table that cannot be read is counted.
pub(crate) fn unmount_stale() {
  let stale = state::with_state(|s| std::mem::take(&mut s.stale_fuse_mounts)).unwrap_or_default();
  if stale.is_empty() {
    return;
  }
  let Ok(table) = slates_bridge_oci::mount_table::mount_table() else {
    let _ = state::with_state(|s| *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1);
    return;
  };
  for (attachment, point) in stale {
    let source = fsname_of(attachment);
    if table
      .iter()
      .any(|m| m.mount_point == point && m.fstype == FUSE_TYPE && m.source == source)
    {
      unmount_owned(&point);
    }
  }
}

/// Format: the filesystem type the kernel lists for a slates FUSE mount (the `slates` subtype).
const FUSE_TYPE: &str = "fuse.slates";

/// Starts `fusermount3 -u -z` at `mount_point` and owns the helper until it is reaped, on a detached task
/// polling at the heartbeat's tick (bounded by the failover bound; the helper is killed and reaped past it).
fn unmount_owned(mount_point: &str) {
  let Ok(pending) = begin_unmount(mount_point) else {
    let _ = state::with_state(|s| *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1);
    return;
  };
  if futures::spawn(reap(pending))
    .and_then(futures::detach)
    .is_err()
  {
    let _ = state::with_state(|s| *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1);
  }
}

/// Polls an unmount helper until it is reaped; dropped at the bound, which kills and reaps it.
async fn reap(mut pending: PendingExit) {
  let tick = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;
  let bound =
    state::with_state(|s| s.config.failover_slo_ns).unwrap_or(crate::daemon::HEARTBEAT_NS);
  let began = futures::now_ns();
  loop {
    if let Some(done) = pending.poll() {
      if done.is_err() {
        let _ = state::with_state(|s| *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1);
      }
      return;
    }
    if futures::now_ns().saturating_sub(began) >= bound || futures::sleep(tick).await.is_err() {
      let _ = state::with_state(|s| *s.refusals.entry(UNMOUNT_REFUSED).or_insert(0) += 1);
      return;
    }
  }
}
