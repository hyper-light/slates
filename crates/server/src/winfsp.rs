//! The daemon's Windows WinFsp transport (§4.6 "Windows"): a volume mounted at a free drive letter and served on the
//! volume's owner shard by the shared operation layer, so ordinary Windows programs reach the volume through the
//! kernel — the Windows counterpart of the Linux FUSE transport (`crate::fuse`), built on the binding in
//! `slates-bridge-winfsp`.
//!
//! **Establishment.** `attach` with the WinFsp form allocates the attachment, then defers its reply to the mount's own
//! task (the granted landing's discipline, `crate::landing`). The task starts the mount's thread
//! (`slates_bridge_winfsp::host::MountThread`), which creates the WinFsp file system, defines the drive letter and starts
//! WinFsp's dispatcher, all off the shard; the task waits for its outcome without holding the shard. Only once the drive
//! is mounted is the attachment recorded, in the same database transaction as the request's completion, with the drive
//! as its path: a refused mount leaves nothing recorded (its write lease is released).
//!
//! **Serving.** The same task then serves the mount for its whole life. WinFsp's dispatcher threads hand it each
//! request over a bounded channel and wake it (§4.3 "Drivers": "WinFsp requests arrive on WinFsp's threads and are
//! routed to the owning shard by handle"); each turn serves one request whole with a transient `VolumeBridge` over the
//! shard's slot, admitted under the mount's registry attachment so a barrier over the volume sees it in flight. A request
//! that changed what survives a restart (a namespace or size change, a flush, the close of a handle that wrote)
//! publishes the shard's recovery image before its reply (§4.8, D-18), as the FUSE and NFS transports' do; a refused
//! publication answers `STATUS_UNEXPECTED_IO_ERROR`. The task yields between requests so a busy mount never holds its
//! shard. While the owner may not serve the volume's latest state (the live-tree fence, AUD-29-83) no request is
//! taken: WinFsp's dispatcher waits, asked again each heartbeat.
//!
//! **Coherence (§4.6 "Cache posture").** Windows keeps what it fetched for as long as the mount does not say otherwise
//! (`FileInfoTimeout` infinite), so the mount tells it of every change made through any other path — the SDK, another
//! attachment, a merge: before each request, and whenever the volume's change generation moves past what the mount
//! last told, the task gathers the seam's invalidations (`slates_bridge_core::delivery::Coherence`, the rounds the FUSE
//! mount runs) and hands Windows one round of `FspFileSystemNotify` records naming what it holds open
//! (`slates_bridge_winfsp::notify`). The mount thread hands the round over; one round is out at a time, and a round
//! WinFsp could not take (a rename held its lock, or the call failed) puts the cursor back, so it is gathered again.
//! The mount's own requests move the cursor past their own records, so Windows is never told of what it did itself.
//!
//! **Teardown.** A drive letter has nothing beneath it, so ending a mount unmounts it: there is no directory for a later
//! write by path to fall through to (the FUSE transport's reason to leave an ended mount in place, A-102, does not
//! arise). A `detach`, the volume's destroy, a revocation and the daemon's stop all end it: the request channel's
//! receiver is dropped first, so every dispatcher thread waiting on the shard returns at once, and then the mount thread
//! stops the dispatcher, removes the drive and deletes the file system, and is joined. The WinFsp file system is this
//! process's own, so a dead daemon's mount died with it and recovery ends its record.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

use slates_bridge_core::delivery::{Coherence, Delivered};
use slates_bridge_core::volume_bridge::new_handle_store;
use slates_bridge_core::{
  AttachmentId, Bridge, CachedBytes, Invalidation, InvalidationCursor, ObjectId, Rights, View,
  VolumeBridge,
};
use slates_bridge_winfsp::host::{MountThread, Notified, Outcome, REQUEST_BOUND, Request};
use slates_bridge_winfsp::notify::{NotifyRound, OpenNames};
use slates_bridge_winfsp::purge::{Purge, purge_range};
use slates_db::Op;
use slates_db::catalog::{AttachmentRecord, VolumeId};
use slates_ipc::protocol::{AttachmentCapability, Established, Refusal, ReplyBody};
use slates_mem::slab::Slab;
use slates_rt::futures;
use slates_vfs::clock::Clock;
use slates_vfs::host::HostFs;

use crate::state::{self, ShardState};

/// Format: the refusal-ledger names of this transport's counted outcomes: a mount WinFsp refused.
const MOUNT_REFUSED: &str = "winfsp.mount_refused";
/// Format: see [`MOUNT_REFUSED`] — a request's publication refused, answered `STATUS_UNEXPECTED_IO_ERROR`.
const BARRIER_REFUSED: &str = "winfsp.barrier_refused";
/// Format: see [`MOUNT_REFUSED`] — a reply whose dispatcher thread was gone (the mount was being torn down).
const REPLY_UNDELIVERED: &str = "winfsp.reply_undelivered";
/// Format: see [`MOUNT_REFUSED`] — a mount's thread ended without being asked (its file system is gone).
const MOUNT_LOST: &str = "winfsp.mount_lost";
/// Format: see [`MOUNT_REFUSED`] — an attachment end the database refused to record.
const END_REFUSED: &str = "winfsp.end_refused";
/// Format: the notification rounds Windows took: the non-vacuity counter of the coherence path (§4.6), so a mount that
/// silently tells Windows nothing can never pass for one that does.
const ROUNDS_TAKEN: &str = "winfsp.rounds_taken";
/// Format: the notification records those rounds carried.
const RECORDS_TOLD: &str = "winfsp.records_told";
/// Format: rounds a rename's lock deferred (`STATUS_CANT_WAIT`), gathered again a tick later.
const ROUNDS_DEFERRED: &str = "winfsp.rounds_deferred";
/// Format: rounds WinFsp refused outright, gathered again at the next wake.
const ROUNDS_REFUSED: &str = "winfsp.rounds_refused";
/// Format: rounds that found the mount behind its volume journal's retention and told Windows of every name it holds
/// open (A-141's overrun). Zero is the healthy reading.
const JOURNAL_OVERRUNS: &str = "winfsp.journal_overruns";
/// Format: names left out of a round for being longer than any WinFsp path. Zero is the healthy reading.
const NAMES_TOO_LONG: &str = "winfsp.names_too_long";
/// Format: files whose changed ranges the mount thread purged from Windows' cache and every view mapped of them
/// (`slates_bridge_winfsp::purge`, A-190): the non-vacuity counter of the view purge.
const PURGES_DONE: &str = "winfsp.purges_done";
/// Format: purges whose open or write was refused (a file renamed or removed since its round).
const PURGES_REFUSED: &str = "winfsp.purges_refused";
/// Format: purges not made because WinFsp cannot name an open's process. Zero on a current WinFsp.
const PURGES_UNAVAILABLE: &str = "winfsp.purges_unavailable";
/// Format: purge writes the host answered without applying them: the count that shows purges reached the host.
const PURGE_ECHOES: &str = "winfsp.purge_echoes";

/// A mounted volume being served: the channel WinFsp's dispatcher threads send on, the mount's thread, the open-handle
/// map that persists across requests, the registry attachment its requests are admitted under, what Windows holds
/// open, and where its cache stands.
pub(crate) struct WinFspMount {
  /// The requests WinFsp's dispatcher threads hand the shard; dropped first at the end, so none waits on it.
  requests: Option<Receiver<Request>>,
  thread: MountThread,
  handles: Slab<u64>,
  registry: AttachmentId,
  volume: VolumeId,
  mount_point: String,
  /// What Windows holds open: the names a round tells it of.
  names: OpenNames,
  /// Where Windows' cache stands against the volume's journal.
  coherence: Coherence,
  /// The cursor before the round now out at the mount thread, when one is: put back if Windows does not take it.
  round_out: Option<Option<InvalidationCursor>>,
  /// A round is owed although the volume's generation has not moved since: the last one was put back.
  owed: bool,
  /// The volume's change generation as of the mount's last delivered round (`Volume::change_generation`).
  seen: u64,
  /// The mount is to end at its task's next turn: its attachment ended (`end_if_mounted`).
  ending: bool,
  /// The mount's task's waker, to wake it for an end decided elsewhere.
  waker: std::task::Waker,
  /// Records handed to the mount thread since the last turn counted them ([`RECORDS_TOLD`]).
  told: u64,
}

impl std::fmt::Debug for WinFspMount {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WinFspMount")
      .field("mount_point", &self.mount_point)
      .finish()
  }
}

/// The shard's WinFsp mounts, by attachment. Bounded by the partition's attachment cap: one entry per live WinFsp
/// attachment at most.
pub(crate) type WinFspMounts = BTreeMap<u64, WinFspMount>;

/// What an attach with the WinFsp form carries into its deferred establishment: the record to commit once the drive is
/// mounted, and the reply's pieces.
pub(crate) struct PendingAttach {
  /// The attachment record, its form the drive.
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

/// Checks a WinFsp mount point before any effect: a drive letter and its colon (`S:`). A directory is refused, as §4.6
/// decides (a directory mount is an NTFS reparse point, which leaves a directory beneath the mount).
pub(crate) fn check_mount_point(path: &str) -> Result<(), Refusal> {
  let mut chars = path.chars();
  match (chars.next(), chars.next(), chars.next()) {
    (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => Ok(()),
    _ => Err(Refusal::TargetUnavailable {
      reason: format!("the WinFsp mount point {path} is not a drive letter (`S:`)"),
    }),
  }
}

/// Defers an attach with the WinFsp form to the mount's task, which mounts, answers, and then serves (see the module
/// docs). The placeholder reply the verb returns is never delivered.
pub(crate) fn defer_attach(state: &mut ShardState, pending: PendingAttach) -> ReplyBody {
  let Some(request) = state.current_request else {
    return crate::verbs::refused(Refusal::Unsupported {
      feature: "a WinFsp mount outside a recorded request".to_owned(),
    });
  };
  let route = state.reply_route.take();
  let cause = state.current_span;
  let shard = state.shard;
  let task = async move {
    let waker = std::future::poll_fn(|cx| std::task::Poll::Ready(cx.waker().clone())).await;
    let attachment = pending.record.id;
    let mount_point = pending
      .record
      .form
      .mount_point()
      .unwrap_or_default()
      .to_owned();
    let (sender, requests) = sync_channel::<Request>(REQUEST_BOUND);
    let mounted =
      match MountThread::start(&mount_point, !pending.rights.write, sender, waker.clone()) {
        Ok(thread) => await_mounted(thread).await,
        Err(error) => Err((error.to_string(), None)),
      };
    let served = state::with_state_counted(|s| {
      let mut reaped = None;
      let reply = complete(s, request, cause, attachment, |s| match mounted {
        Ok(thread) => {
          let (reply, refused_thread) = established(s, pending, thread, requests, waker);
          reaped = refused_thread;
          reply
        }
        Err((why, thread)) => {
          s.count(MOUNT_REFUSED, 1);
          crate::verbs::release_unrecorded_lease(s, &pending.record);
          reaped = thread;
          crate::verbs::refused(Refusal::TargetUnavailable {
            reason: format!("the WinFsp mount at {mount_point} was refused: {why}"),
          })
        }
      });
      deliver(s, reply, route);
      reaped
    });
    match served {
      Some(Some(thread)) => reap(thread).await,
      Some(None) => serve(attachment).await,
      // The shard's state could not be borrowed (counted): the mount, if it was kept, is ended with the shard.
      None => {}
    }
  };
  match futures::spawn(task).and_then(futures::detach) {
    Ok(()) => {
      state.acceptance_deferred = true;
      crate::landing::deferred_reply()
    }
    Err(_) => crate::verbs::refused(Refusal::Overloaded { shard }),
  }
}

/// Waits, without holding the shard, for the mount thread's first outcome: the thread when the drive is mounted, or why
/// not with the thread to reap.
async fn await_mounted(
  mut thread: MountThread,
) -> Result<MountThread, (String, Option<MountThread>)> {
  loop {
    match thread.outcome() {
      Some(Outcome::Mounted(Ok(()))) => return Ok(thread),
      Some(Outcome::Mounted(Err(error))) => return Err((error.to_string(), Some(thread))),
      Some(Outcome::Stopped) => {
        return Err((
          "the mount thread ended before mounting".to_owned(),
          Some(thread),
        ));
      }
      // No round is sent before the mount is up, so none can come back.
      Some(Outcome::Notified(..)) | None => futures::idle().await,
    }
  }
}

/// Commits a deferred attach: its effect (`produce`) and its completion record as one database transaction
/// (`crate::landing`'s discipline). A commit refused leaves nothing durable, so a mount `produce` established is ended.
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
      s.count(crate::verbs::refusal_name(&refusal), 1);
      crate::verbs::reconcile_unpublished_effects(s);
      // The mount's task finds it ending at its first turn and tears it down.
      if let Some(mount) = s.winfsp_mounts.get_mut(&attachment) {
        mount.ending = true;
      }
      crate::verbs::refused(refusal)
    }
  };
  s.current_span = outer;
  reply
}

/// Hands a deferred reply to its client's shard (`crate::landing`'s delivery); a reply that cannot be handed over is
/// counted, and the client's retry answers from the completion record.
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
    s.count(REPLY_UNDELIVERED, 1);
  }
}

/// The drive is mounted: the attachment recorded (inside the completion's transaction), its registry attachment
/// admitted, and the mount kept on the shard for its task to serve; the reply names the drive. A registry refusal
/// refuses the attach and gives the thread back to be reaped, its requests' receiver dropped.
fn established(
  s: &mut ShardState,
  pending: PendingAttach,
  thread: MountThread,
  requests: Receiver<Request>,
  waker: std::task::Waker,
) -> (ReplyBody, Option<MountThread>) {
  let attachment = pending.record.id;
  let volume = pending.record.volume;
  let mount_point = pending
    .record
    .form
    .mount_point()
    .unwrap_or_default()
    .to_owned();
  let now = s.clock.monotonic_ns();
  let op = Op::AttachmentAdded {
    record: pending.record.clone(),
  };
  let refused = |s: &mut ShardState, refusal: Refusal| {
    crate::verbs::release_unrecorded_lease(s, &pending.record);
    crate::verbs::refused(refusal)
  };
  let registry = match s.attachments.attach(
    volume,
    View::Current,
    pending.record.principal.clone(),
    pending.rights,
  ) {
    Ok(registry) => registry,
    Err(e) => {
      drop(requests);
      return (refused(s, crate::error::refusal_of_vfs(&e)), Some(thread));
    }
  };
  // The task delivers the volume's invalidations before each request and whenever the volume changes through any
  // other path (AUD-29-79; §4.6 cache posture).
  s.attachments
    .set_coherence(registry, slates_bridge_core::CacheCoherence::Invalidated);
  s.attachments.set_owner(registry, attachment);
  s.winfsp_mounts.insert(
    attachment,
    WinFspMount {
      requests: Some(requests),
      thread,
      handles: new_handle_store(),
      registry,
      volume,
      mount_point: mount_point.clone(),
      names: OpenNames::new(),
      coherence: Coherence::new(),
      round_out: None,
      owed: false,
      seen: 0,
      ending: false,
      waker,
      told: 0,
    },
  );
  if let Err(e) = s.db.mutate(&mut s.segment, &op, now) {
    if let Some(mount) = s.winfsp_mounts.get_mut(&attachment) {
      mount.ending = true;
    }
    return (refused(s, crate::error::refusal_of_db(&e)), None);
  }
  open_journal_reader(s, volume, registry);
  let reply = ReplyBody::Attached {
    attachment,
    lease_epoch: pending.lease_epoch,
    path: Some(mount_point),
    version: None,
    established: Established::Record,
    capability: pending.capability,
    token: Some(pending.token),
  };
  (reply, None)
}

/// What one turn did.
enum Turned {
  /// Nothing waited: the task waits for a request, a change, or the mount thread.
  Idle,
  /// A request was served.
  Served,
  /// A rename held WinFsp's lock, so the owed round waits a tick.
  RoundDeferred,
  /// The owner may not serve the volume's latest state now (AUD-29-83): no request was taken.
  Fenced,
  /// The mount ends.
  Ended,
}

/// The serve task of one mount, once it is recorded: turn until the mount ends, then tear it down and reap its thread.
async fn serve(attachment: u64) {
  loop {
    match state::with_state(|s| turn(s, attachment)) {
      Some(Turned::Served) => {
        // A served request is client activity, as a FUSE request is (§4.3, §4.7): the shard spins out its idle window
        // after it, so a burst's next request is taken without a wake.
        slates_rt::registry::with_current(|ctx| ctx.note_activity());
        let _ = futures::yield_now().await;
      }
      Some(Turned::Idle) => futures::idle().await,
      Some(Turned::RoundDeferred) => {
        let tick = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;
        if futures::sleep(tick).await.is_err() {
          futures::idle().await;
        }
      }
      Some(Turned::Fenced) => crate::daemon::pace(crate::daemon::HEARTBEAT_NS).await,
      Some(Turned::Ended) | None => {
        if let Some(Some(thread)) = state::with_state_counted(|s| ended(s, attachment)) {
          reap(thread).await;
        }
        return;
      }
    }
  }
}

/// Waits for a stopped mount thread's last outcome and joins it, without holding the shard: the thread has only to
/// stop WinFsp's dispatcher (whose threads return at once, the request channel's receiver being dropped) and remove the
/// drive.
async fn reap(mut thread: MountThread) {
  thread.stop();
  loop {
    match thread.outcome() {
      Some(Outcome::Stopped) => {
        thread.join();
        return;
      }
      Some(Outcome::Mounted(_) | Outcome::Notified(..)) => {}
      None => futures::idle().await,
    }
  }
}

/// One turn of mount `attachment`: the mount thread's outcomes, the owed round, then one request. The mount is taken
/// out of the shard's map for the turn and always put back; a mount that ends is torn down by [`ended`], which the
/// task calls next, and nowhere else.
fn turn(s: &mut ShardState, attachment: u64) -> Turned {
  let Some(mut mount) = s.winfsp_mounts.remove(&attachment) else {
    return Turned::Ended;
  };
  let turned = turn_with(s, &mut mount);
  s.winfsp_mounts.insert(attachment, mount);
  turned
}

/// One turn of `mount`, lent by [`turn`].
fn turn_with(s: &mut ShardState, mount: &mut WinFspMount) -> Turned {
  if mount.ending {
    return Turned::Ended;
  }
  let deferred = match take_outcomes(s, mount) {
    Ok(deferred) => deferred,
    Err(()) => return Turned::Ended,
  };
  let Some(&handle) = s.by_id.get(&mount.volume) else {
    return Turned::Ended;
  };
  if crate::verbs::live_tree_fenced(s, mount.volume) {
    return Turned::Fenced;
  }
  crate::verbs::relieve_deferred(s);
  let changed = s
    .volumes
    .get_mut(handle)
    .is_ok_and(|slot| slot.volume.wait_for_change(mount.seen, &mount.waker));
  let request = match mount.requests.as_ref().map(Receiver::try_recv) {
    Some(Ok(request)) => Some(request),
    Some(Err(TryRecvError::Empty)) => None,
    Some(Err(TryRecvError::Disconnected)) | None => return Turned::Ended,
  };
  if request.is_none() && !changed && !mount.owed {
    return if deferred {
      Turned::RoundDeferred
    } else {
      Turned::Idle
    };
  }
  let Ok(cx) = s.attachments.begin(mount.registry) else {
    return Turned::Ended;
  };
  let turned = {
    let ShardState { store, volumes, .. } = &mut *s;
    volumes.get_mut(handle).ok().map(|slot| {
      let WinFspMount {
        handles,
        names,
        coherence,
        round_out,
        owed,
        thread,
        told,
        volume,
        ..
      } = &mut *mount;
      let mut bridge = VolumeBridge::attached(
        *volume,
        &mut slot.volume,
        store,
        handles,
        slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      );
      let delivered = deliver_owed(
        &mut bridge,
        &cx,
        Rounds {
          coherence,
          names,
          round_out,
          owed,
          thread,
          told,
        },
      );
      let served = request.map(|request| {
        let served = request.serve(&mut bridge, &cx, names);
        coherence.served_own_request(&mut bridge, &cx, delivered.delivered);
        served
      });
      (served, delivered)
    })
  };
  s.attachments.end(mount.registry);
  let Some((served, delivered)) = turned else {
    return Turned::Ended;
  };
  s.count(JOURNAL_OVERRUNS, delivered.overruns);
  s.count(RECORDS_TOLD, std::mem::take(&mut mount.told));
  s.count(NAMES_TOO_LONG, delivered.too_long);
  // What this turn delivered is behind it, so the task waits past the generation as it now stands; another path's
  // change after this point moves it again and wakes the task.
  if !delivered.delivered.gather_refused {
    mount.seen = s
      .volumes
      .get(handle)
      .map_or(mount.seen, |slot| slot.volume.change_generation());
  }
  match served {
    Some(served) => {
      answer(s, mount, served);
      Turned::Served
    }
    None if deferred => Turned::RoundDeferred,
    None => Turned::Idle,
  }
}

/// Reads what the mount thread reported since the last turn: a round taken, deferred or refused (the latter two put the
/// cursor back, so the round is gathered again). `Ok(true)` when a rename deferred it; `Err` when the mount is gone.
fn take_outcomes(s: &mut ShardState, mount: &mut WinFspMount) -> Result<bool, ()> {
  let mut deferred = false;
  while let Some(outcome) = mount.thread.outcome() {
    match outcome {
      Outcome::Notified(told, purged) => {
        s.count(PURGES_DONE, purged.done);
        s.count(PURGES_REFUSED, purged.refused);
        s.count(PURGES_UNAVAILABLE, purged.unavailable);
        let before = mount.round_out.take().flatten();
        match told {
          Ok(Notified::Taken) => s.count(ROUNDS_TAKEN, 1),
          Ok(Notified::RenameInProgress) => {
            s.count(ROUNDS_DEFERRED, 1);
            mount.coherence.rewind(before);
            mount.owed = true;
            deferred = true;
          }
          Err(_) => {
            s.count(ROUNDS_REFUSED, 1);
            mount.coherence.rewind(before);
            mount.owed = true;
          }
        }
      }
      Outcome::Mounted(_) => {}
      Outcome::Stopped => {
        s.count(MOUNT_LOST, 1);
        return Err(());
      }
    }
  }
  Ok(deferred)
}

/// What [`deliver_owed`] did.
struct Round {
  delivered: Delivered,
  overruns: u64,
  too_long: u64,
}

/// The parts of a mount a delivery round works with, lent apart from its open-handle map, which the round's bridge holds.
struct Rounds<'m> {
  coherence: &'m mut Coherence,
  names: &'m OpenNames,
  round_out: &'m mut Option<Option<InvalidationCursor>>,
  owed: &'m mut bool,
  thread: &'m MountThread,
  told: &'m mut u64,
}

/// Delivers the owed round when none is out: every invalidation since the cursor, as records naming what Windows holds
/// open, handed to the mount thread. While a round is out nothing new is gathered (the gather consumes) and the round is
/// reported refused, so the cursor does not move past what Windows has not been told — the FUSE mount's rule.
fn deliver_owed(
  bridge: &mut VolumeBridge<'_>,
  cx: &slates_bridge_core::OpContext,
  mount: Rounds<'_>,
) -> Round {
  let refused = Round {
    delivered: Delivered {
      written: 0,
      gather_refused: true,
    },
    overruns: 0,
    too_long: 0,
  };
  if mount.round_out.is_some() {
    return refused;
  }
  let before = mount.coherence.cursor();
  let mut round = NotifyRound::new();
  let mut overruns = 0u64;
  let mut changed: Vec<(u64, CachedBytes)> = Vec::new();
  let names = mount.names;
  let Ok(delivered) = mount
    .coherence
    .deliver(bridge, cx, &mut |invalidation: &Invalidation| {
      match invalidation {
        Invalidation::Everything => overruns = overruns.saturating_add(1),
        Invalidation::Inode { ino, data } => changed.push((*ino, *data)),
        Invalidation::Entry { .. } => {}
      }
      round.add(invalidation, names);
      Ok::<(), std::convert::Infallible>(())
    });
  if delivered.gather_refused {
    return refused;
  }
  *mount.owed = false;
  let too_long = round.too_long();
  if !round.is_empty() {
    let purges = purges_owed(bridge, cx, names, (&changed, overruns > 0));
    if mount.thread.notify(round.bytes().to_vec(), purges).is_ok() {
      *mount.round_out = Some(before);
      *mount.told = mount
        .told
        .saturating_add(u64::try_from(round.records()).unwrap_or(u64::MAX));
    } else {
      // The thread is gone or busy: the round is owed again, from where it began.
      mount.coherence.rewind(before);
      *mount.owed = true;
      return refused;
    }
  }
  Round {
    delivered,
    overruns,
    too_long,
  }
}

/// The purges a round owes (`slates_bridge_winfsp::purge`, A-190): each changed inode Windows holds open, under every
/// name it is open by, over the hull of what changed of it, widened to whole sectors and clipped to the file as it now
/// stands. A round that found the mount behind the journal owes every open file whole. Bounded by what Windows holds
/// open.
fn purges_owed(
  bridge: &mut VolumeBridge<'_>,
  cx: &slates_bridge_core::OpContext,
  names: &OpenNames,
  (changed, everything): (&[(u64, CachedBytes)], bool),
) -> Vec<Purge> {
  let sector = slates_bridge_winfsp::host::sector_bytes();
  let owed: Vec<(u64, CachedBytes)> = if everything {
    names
      .open_inodes()
      .into_iter()
      .map(|ino| (ino, CachedBytes::ALL))
      .collect()
  } else {
    changed.to_vec()
  };
  let mut hulls: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
  for (ino, data) in owed {
    if names.open_names(ino).is_empty() {
      continue;
    }
    let Ok(attr) = bridge.getattr(ObjectId::new(ino, 0), cx) else {
      continue;
    };
    let Some((offset, len)) = purge_range(&data, attr.size, sector) else {
      continue;
    };
    let end = offset.saturating_add(len);
    hulls
      .entry(ino)
      .and_modify(|(first, last)| {
        *first = (*first).min(offset);
        *last = (*last).max(end);
      })
      .or_insert((offset, end));
  }
  let mut purges = Vec::new();
  for (ino, (offset, end)) in hulls {
    for name in names.open_names(ino) {
      purges.push(Purge {
        path: name.to_owned(),
        offset,
        len: end.saturating_sub(offset),
      });
    }
  }
  purges
}

/// Publishes when the served request needs it, then sends its reply.
fn answer(s: &mut ShardState, mount: &WinFspMount, mut served: slates_bridge_winfsp::host::Served) {
  if served.is_purge_echo() {
    s.count(PURGE_ECHOES, 1);
  }
  if served.needs_barrier() {
    let captured =
      crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(mount.volume));
    if !captured {
      s.count(BARRIER_REFUSED, 1);
      served.refuse_unpublished();
    }
  }
  if !served.send() {
    s.count(REPLY_UNDELIVERED, 1);
  }
}

/// Tears mount `attachment` down: its requests' receiver dropped (every waiting dispatcher thread returns), its journal
/// reader closed, its registry attachment revoked and drained, and its attachment ended as a recorded operation when it
/// still stands. Returns its thread, asked to stop, for the task to reap off the shard.
fn ended(s: &mut ShardState, attachment: u64) -> Option<MountThread> {
  let mut mount = s.winfsp_mounts.remove(&attachment)?;
  mount.requests = None;
  close_journal_reader(s, &mount);
  s.attachments.revoke(mount.registry);
  s.attachments.drain(mount.registry);
  if let Some(record) = s.db.partition().attachment(attachment).cloned()
    && crate::verbs::end_attachment(s, &record, crate::verbs::Ending::Otherwise).is_err()
  {
    s.count(END_REFUSED, 1);
  }
  mount.thread.stop();
  Some(mount.thread)
}

/// Ends the WinFsp mount of `attachment` if the shard serves one (its attachment ended: a `detach`, the volume's
/// destroy, a revocation): its task tears it down at its next turn, woken now.
pub(crate) fn end_if_mounted(s: &mut ShardState, attachment: u64) {
  if let Some(mount) = s.winfsp_mounts.get_mut(&attachment) {
    mount.ending = true;
    mount.waker.wake_by_ref();
  }
}

/// Ends every WinFsp mount the shard serves (the daemon's stop): each torn down and its attachment ended, its thread
/// stopped and joined here — at the stop nothing else runs on the shard, and each dispatcher thread returns at once,
/// its request channel's receiver dropped first.
pub(crate) fn end_all(s: &mut ShardState) {
  let attachments: Vec<u64> = s.winfsp_mounts.keys().copied().collect();
  let mut threads: Vec<MountThread> = attachments
    .into_iter()
    .filter_map(|attachment| ended(s, attachment))
    .collect();
  for thread in &mut threads {
    thread.join();
  }
}

/// Ends every WinFsp mount and writes no record: all a fenced shard may do at its stop. The records end at the next
/// start's recovery.
pub(crate) fn drop_all(s: &mut ShardState) {
  let mounts = std::mem::take(&mut s.winfsp_mounts);
  for (_, mut mount) in mounts {
    mount.requests = None;
    mount.thread.join();
  }
}

/// Opens the journal reader of a mount that now serves `volume` under registry attachment `registry` (§4.5): from here
/// the volume keeps the records after the head for it, and each of its rounds moves its point.
fn open_journal_reader(s: &mut ShardState, volume: VolumeId, registry: AttachmentId) {
  if let Some(&handle) = s.by_id.get(&volume)
    && let Ok(slot) = s.volumes.get_mut(handle)
  {
    slot
      .volume
      .open_journal_reader(registry.key(), crate::daemon::LIVENESS_BUDGET_NS);
  }
}

/// Closes the journal reader of `mount`, removed for good. A volume already gone took its readers with it.
fn close_journal_reader(s: &mut ShardState, mount: &WinFspMount) {
  if let Some(&handle) = s.by_id.get(&mount.volume)
    && let Ok(slot) = s.volumes.get_mut(handle)
  {
    slot.volume.close_journal_reader(mount.registry.key());
  }
}

#[cfg(test)]
mod tests {
  use super::check_mount_point;

  /// §4.6 "Windows": a drive letter is the mount point; a directory, a UNC path or a bare letter is refused before any
  /// effect.
  #[test]
  fn only_a_drive_letter_is_a_winfsp_mount_point() {
    for good in ["S:", "z:"] {
      assert!(check_mount_point(good).is_ok(), "{good}");
    }
    for bad in ["S", "S:\\", "C:\\mnt", "\\\\server\\share", "1:", ""] {
      assert!(check_mount_point(bad).is_err(), "{bad}");
    }
  }
}
