//! The daemon's Windows WinFsp transport (§4.6 "Windows"): a volume mounted at a free drive letter and served on the
//! volume's owner shard by the shared operation layer, so ordinary Windows programs reach the volume through the
//! kernel — the Windows counterpart of the Linux FUSE transport (`crate::fuse`), built on the binding in
//! `slates-bridge-winfsp`.
//!
//! **Establishment.** `attach` with the WinFsp form allocates the attachment, then defers its reply to the mount's own
//! task (the granted landing's discipline, `crate::landing`). The task starts the mount's thread
//! (`slates_bridge_winfsp::host::MountThread`), which creates the WinFsp file system, defines the drive letter and starts
//! its owned receive worker, all off the shard; the task waits for its outcome without holding the shard. Only once the drive
//! is mounted is the attachment recorded, in the same database transaction as the request's completion, with the drive
//! as its path: a refused mount leaves nothing recorded (its write lease is released).
//!
//! **Serving.** The same task then serves the mount for its whole life. The mount's single receive worker hands it each
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
//! After a round Windows took, the mount thread purges each changed file Windows holds open from its cache and every
//! view of it (A-190), and reads each purge back: a purge Windows did not take is owed again, alone, after a wait that
//! doubles from a tick, and sent as a purge-only command once due, until it takes or Windows has kept its page past the
//! volume's request timeout (`slates_bridge_winfsp::retry`). One command, a round or a purge, is out at a time.
//!
//! **Teardown.** A drive letter has nothing beneath it, so ending a mount unmounts it: there is no directory for a later
//! write by path to fall through to (the FUSE transport's reason to leave an ended mount in place, A-102, does not
//! arise). A `detach`, the volume's destroy, a revocation and the daemon's stop all end it: the request channel's
//! receiver is dropped first, so every dispatcher thread waiting on the shard returns at once, and then the mount thread
//! joins the receiver and closes its own drive and file-system handles, and is joined.
//!
//! **The file system is deleted only after the anchor released its hold.** The anchor's copies keep the volume and the
//! drive whoever else closes theirs, so a file system deleted first leaves a drive nobody serves. A `detach` therefore
//! asks the anchor while the mount goes on serving, repeating the request for [`crate::anchor_hold::RELEASE_BOUND_NS`],
//! and only an answer ends the attachment and removes the drive. Past the bound the detach is answered a typed refusal:
//! the drive stays served and held. The request stands, since it may already be waiting in the pipe for the anchor's
//! next turn, so it goes on being repeated, and the anchor's answer ends the mount then. Every other end (a destroy, a
//! revocation, a mount refused or lost) has already ended the attachment, so it stops serving and waits for the same
//! answer before it deletes the file system; a refused mount-time hold is answered first and then the same. The wait
//! holds one task and one message per mount, and ends at the answer or at the shard's stop. A daemon with no anchor
//! holds nothing, so nothing waits; its mount is refused at the hold.
//!
//! Across a killed daemon the anchor holds the control, drive-link and ledger
//! objects (A-198); recovery retains a matching catalog attachment, builds a successor dispatcher for that held volume,
//! and seeds its coherence names from the ledger's live opens. An old context resolves by stable file identity, with
//! the old process's bridge handle suppressed. The 20-kill CLI oracle also requires a counted uncached read through
//! the old handle, so cached bytes cannot stand in for a successor serving it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::mpsc::{Receiver, TryRecvError, sync_channel};

use slates_bridge_core::delivery::{Coherence, Delivered};
use slates_bridge_core::volume_bridge::{MAX_OPEN_HANDLES, new_handle_store};
use slates_bridge_core::{
  AttachmentId, Bridge, CachedBytes, Invalidation, InvalidationCursor, ObjectId, Rights, View,
  VolumeBridge,
};
use slates_bridge_winfsp::held::{Record as HeldRecord, windows::Inherited};
use slates_bridge_winfsp::host::Ready;
use slates_bridge_winfsp::host::{MountThread, Notified, Outcome, Purged, REQUEST_BOUND, Request};
use slates_bridge_winfsp::notify::{NotifyRound, OpenNames};
use slates_bridge_winfsp::opens::{Ledger, Open};
use slates_bridge_winfsp::purge::{Purge, purge_range};
use slates_bridge_winfsp::retry::{PurgeRetries, REQUEST_TIMEOUT_NS, Recorded};
use slates_db::Op;
use slates_db::catalog::{AttachmentRecord, VolumeId};
use slates_ipc::protocol::{AttachmentCapability, Established, Refusal, ReplyBody};
use slates_mem::SparseObject;
use slates_mem::slab::Slab;
use slates_rt::futures;
use slates_vfs::clock::Clock;
use slates_vfs::host::HostFs;
use slates_vfs::ids::{InodeNo, RefOwner};

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
/// Format: purges whose open, write or read back was refused (a file renamed or removed since its round).
const PURGES_REFUSED: &str = "winfsp.purges_refused";
/// Format: historical purge unavailability counter. The owned receiver reads the checked Create PID and token;
/// private-context setup failures now end the mount with a typed failure, so this counter stays zero.
const PURGES_UNAVAILABLE: &str = "winfsp.purges_unavailable";
/// Format: the counter of purge attempts Windows did not take: written whole, but read back through its cache as bytes
/// the host no longer holds, so the purge is owed again (`slates_bridge_winfsp::retry`; Step 8 of the VFS and DAX plan,
/// docs/bugs/2026-11-01-a-winfsp-purge-windows-did-not-take-left-a-mapped-view-stale.md).
const PURGES_UNTAKEN: &str = "winfsp.purges_untaken";
/// Format: purges given up after Windows kept their page until the volume's request timeout (or with the retry list
/// full): a view of the range may stay stale until a later change purges it again. Zero is the healthy reading.
const PURGES_KEPT: &str = "winfsp.purges_kept";
/// Format: purge-only commands sent to the mount thread, each the purges owed again that had come due.
const PURGE_RETRIES: &str = "winfsp.purge_retries";
/// Format: purge writes the host answered without applying them: the count that shows purges reached the host.
const PURGE_ECHOES: &str = "winfsp.purge_echoes";
/// Format: successful reads through an open the predecessor recorded. The takeover oracle requires it to move.
const RESTORED_READS: &str = "winfsp.restored_reads";
/// Format: kernel requests settled from the retained receive journal before taking a new batch (A-198).
const CAPTURED_SETTLED: &str = "winfsp.captured_requests_settled";
/// Format: release attempts the anchor refused or did not answer, each repeated: the non-vacuity counter of the unmount
/// oracle. A healthy anchor keeps it at zero.
const HOLD_RELEASE_REFUSED: &str = "winfsp.hold_release_refused";
/// Format: detaches answered a refusal because the anchor had not released within the bound.
const DETACH_REFUSED: &str = "winfsp.detach_refused";

/// A mounted volume being served: the channel WinFsp's dispatcher threads send on, the mount's thread, the open-handle
/// map that persists across requests, the registry attachment its requests are admitted under, what Windows holds
/// open, and where its cache stands.
pub(crate) struct WinFspMount {
  /// The requests WinFsp's dispatcher threads hand the shard; dropped first at the end, so none waits on it.
  requests: Option<Receiver<Request>>,
  ledger: Ledger<SparseObject>,
  seeds: VecDeque<(u64, Open, String)>,
  takeover: bool,
  owner: RefOwner,
  reconcile: Option<RestoredOpenCounts>,
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
  /// The purges Windows did not take, each owed another attempt after its wait (`slates_bridge_winfsp::retry`).
  retries: PurgeRetries,
  /// A purge-only command is out at the mount thread: no round or other purge is sent until its outcome is read, so one
  /// command is out at a time.
  purge_out: bool,
  /// A detach has asked the anchor to release this mount and the mount serves on until it answers.
  detaching: bool,
  /// The anchor answered the release, so the mount's end asks nothing more of it.
  released: bool,
  /// This volume's place in the daemon's own mounts (A-227), so no daemon thread opens a path on its drive; forgotten
  /// when the mount is dropped. Held for its drop alone.
  _own: OwnVolumeRecord,
}

/// A WinFsp volume's id in the process's set of its own mounts (`slates_base::own_mounts`, A-227): its kernel name's,
/// the same name its drive letter's DOS device names, so a base path on the drive is refused before any open.
struct OwnVolumeRecord(u64);

impl OwnVolumeRecord {
  fn of(volume: &[u16]) -> Result<Self, Refusal> {
    let id = slates_base::own_mounts::volume_id(volume);
    slates_base::own_mounts::record(id).map_err(|full| Refusal::TargetUnavailable {
      reason: format!(
        "the WinFsp volume is past the {} mounts this daemon records as its own",
        full.capacity
      ),
    })?;
    Ok(Self(id))
  }
}

impl Drop for OwnVolumeRecord {
  fn drop(&mut self) {
    slates_base::own_mounts::forget(self.0);
  }
}

/// The bounded held-ledger/image merge, one inode per owner turn. The held map has at most MAX_OPEN_HANDLES entries.
struct RestoredOpenCounts {
  held: BTreeMap<u64, (u64, u32)>,
  after: Option<u64>,
  changed: bool,
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
    let mounted = match MountThread::start(
      &mount_point,
      !pending.rights.write,
      sender,
      waker.clone(),
      MAX_OPEN_HANDLES,
      None,
    ) {
      Ok(thread) => await_mounted(thread).await,
      Err(error) => Err((error.to_string(), None)),
    };
    // A hold the anchor may yet read names this process's objects, so a refused one keeps them until it is released.
    let mut _unheld = None;
    let mounted = match mounted {
      Ok((thread, ready)) => {
        let held = match (ready.handle_values(), ready.offer.record.encode()) {
          (Ok(values), Ok(record)) => crate::anchor_hold::hold_mount(attachment, values, record)
            .await
            .map_err(|error| format!("the anchor's hold: {error}")),
          (Err(error), _) => Err(error.to_string()),
          (_, Err(error)) => Err(format!("held mount record: {error:?}")),
        };
        match held {
          Ok(()) => Ok((thread, ready)),
          Err(error) => {
            _unheld = Some(ready);
            Err((error, Some(thread)))
          }
        }
      }
      Err(error) => Err(error),
    };
    let served = state::with_state_counted(|s| {
      let mut reaped = None;
      let reply = complete(s, request, cause, attachment, |s| match mounted {
        Ok((thread, ready)) => {
          let (reply, refused_thread) = established(s, pending, thread, requests, waker, ready);
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
      Some(Some(thread)) => reap(thread, attachment, false).await,
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

/// Defers a `detach` of a served mount until the anchor has released its hold (see the module docs), or answers why it
/// cannot start: `None` when the attachment has no served mount here, so the verb ends it as any other.
pub(crate) fn defer_detach(state: &mut ShardState, record: &AttachmentRecord) -> Option<ReplyBody> {
  let attachment = record.id;
  let request = state.current_request?;
  let mount = state.winfsp_mounts.get_mut(&attachment)?;
  if mount.detaching {
    return Some(crate::verbs::refused(Refusal::TargetUnavailable {
      reason: format!(
        "the drive {} waits for the anchor to release its hold; a detach already asked for this",
        mount.mount_point
      ),
    }));
  }
  mount.detaching = true;
  let route = state.reply_route.take();
  let cause = state.current_span;
  let shard = state.shard;
  let record = record.clone();
  let task = async move {
    let answered = release_within(attachment, crate::anchor_hold::RELEASE_BOUND_NS).await;
    let refused = answered.is_err();
    let _ = state::with_state_counted(|s| {
      let reply = complete(s, request, cause, attachment, |s| match &answered {
        Ok(()) => end_released(s, &record),
        Err(error) => {
          s.count(DETACH_REFUSED, 1);
          crate::verbs::refused(Refusal::TargetUnavailable {
            reason: format!(
              "the anchor has not released the drive it holds, so the file system still serves it: {error}"
            ),
          })
        }
      });
      deliver(s, reply, route);
    });
    if refused {
      release_hold(attachment).await;
      let _ = state::with_state_counted(|s| {
        if s.winfsp_mounts.contains_key(&attachment) {
          let _ = end_released(s, &record);
        }
      });
    }
  };
  match futures::spawn(task).and_then(futures::detach) {
    Ok(()) => {
      state.acceptance_deferred = true;
      Some(crate::landing::deferred_reply())
    }
    Err(_) => {
      if let Some(mount) = state.winfsp_mounts.get_mut(&attachment) {
        mount.detaching = false;
      }
      Some(crate::verbs::refused(Refusal::Overloaded { shard }))
    }
  }
}

/// The anchor has released the mount, so a killed daemon would lose it anyway: its attachment ends and the mount ends,
/// whether or not the end could be recorded here (the mount's own end records it again, and a restart ends a record no
/// hold matches).
fn end_released(s: &mut ShardState, record: &AttachmentRecord) -> ReplyBody {
  let ended = crate::verbs::end_attachment(s, record, crate::verbs::Ending::Detached);
  if let Some(mount) = s.winfsp_mounts.get_mut(&record.id) {
    mount.released = true;
    mount.ending = true;
    mount.waker.wake_by_ref();
  }
  match ended {
    Ok(()) => ReplyBody::Detached,
    Err(error) => crate::verbs::refused(crate::error::refusal_of_db(&error)),
  }
}

/// Waits, without holding the shard, for the mount thread's first outcome: the thread when the drive is mounted, or why
/// not with the thread to reap.
async fn await_mounted(
  mut thread: MountThread,
) -> Result<(MountThread, Ready), (String, Option<MountThread>)> {
  loop {
    match thread.outcome() {
      Some(Outcome::Mounted(Ok(ready))) => return Ok((thread, *ready)),
      Some(Outcome::Mounted(Err(error)) | Outcome::Failed(error)) => {
        return Err((error.to_string(), Some(thread)));
      }
      Some(Outcome::Stopped) => {
        return Err((
          "the mount thread ended before mounting".to_owned(),
          Some(thread),
        ));
      }
      // No command (a round or a purge) is sent before the mount is up, so none can come back.
      Some(Outcome::Notified(..) | Outcome::Purged(_) | Outcome::Recovered(_)) | None => {
        futures::idle().await
      }
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
  ready: Ready,
) -> (ReplyBody, Option<MountThread>) {
  let attachment = pending.record.id;
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
  if let Err((refusal, thread)) = keep(s, &pending.record, thread, requests, waker, ready, false) {
    return (refused(s, refusal), Some(thread));
  }
  if let Err(e) = s.db.mutate(&mut s.segment, &op, now) {
    if let Some(mount) = s.winfsp_mounts.get_mut(&attachment) {
      mount.ending = true;
    }
    return (refused(s, crate::error::refusal_of_db(&e)), None);
  }
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

/// The same live mount state for an establishment and an adoption: only the record publication differs.
fn keep(
  s: &mut ShardState,
  record: &AttachmentRecord,
  thread: MountThread,
  requests: Receiver<Request>,
  waker: std::task::Waker,
  ready: Ready,
  takeover: bool,
) -> Result<(), (Refusal, MountThread)> {
  let registry = match s.attachments.attach(
    record.volume,
    View::Current,
    record.principal.clone(),
    Rights {
      read: record.rights.read,
      write: record.rights.write,
    },
  ) {
    Ok(registry) => registry,
    Err(error) => {
      drop(requests);
      return Err((crate::error::refusal_of_vfs(&error), thread));
    }
  };
  s.attachments
    .set_coherence(registry, slates_bridge_core::CacheCoherence::Invalidated);
  s.attachments.set_owner(registry, record.id);
  let seeds = if takeover {
    ready.opens
  } else {
    VecDeque::new()
  };
  s.count(
    "winfsp.ledger_quarantined",
    u64::try_from(ready.ledger.quarantined()).unwrap_or(u64::MAX),
  );
  let own = match OwnVolumeRecord::of(&ready.offer.record.volume) {
    Ok(own) => own,
    Err(refusal) => {
      drop(requests);
      s.attachments.revoke(registry);
      s.attachments.drain(registry);
      return Err((refusal, thread));
    }
  };
  drop(ready.offer);
  s.winfsp_mounts.insert(
    record.id,
    WinFspMount {
      requests: Some(requests),
      ledger: ready.ledger,
      seeds,
      takeover,
      owner: RefOwner::Attachment(record.id),
      reconcile: takeover.then_some(RestoredOpenCounts {
        held: ready.open_counts,
        after: None,
        changed: false,
      }),
      thread,
      handles: new_handle_store(),
      registry,
      volume: record.volume,
      mount_point: record.form.mount_point().unwrap_or_default().to_owned(),
      names: OpenNames::new(),
      coherence: Coherence::new(),
      round_out: None,
      owed: takeover,
      seen: 0,
      ending: false,
      waker,
      told: 0,
      retries: PurgeRetries::new(MAX_OPEN_HANDLES, tick_ns(), REQUEST_TIMEOUT_NS),
      purge_out: false,
      detaching: false,
      released: false,
      _own: own,
    },
  );
  open_journal_reader(s, record.volume, registry);
  Ok(())
}

/// A checked held mount, routed by the attachment's durable partition rather than a process-local shard id.
pub(crate) struct InheritedMount {
  pub(crate) attachment: u64,
  held: Option<Inherited>,
}

impl InheritedMount {
  pub(crate) fn matches(&self, record: &AttachmentRecord) -> bool {
    self.held.as_ref().is_some_and(|held| {
      held.record.drive_text().ok().as_deref() == record.form.mount_point()
        && held.record.read_only == !record.rights.write
    })
  }
}

/// Splits all inherited objects by their durable owner. Unmatched or invalid records are released by the control
/// shard's adoption task, never kept after their catalog attachment ended.
pub(crate) fn split_held(
  handed: Vec<crate::anchor_hold::HandedMount>,
  partitions: usize,
) -> Vec<Vec<InheritedMount>> {
  let mut split: Vec<Vec<InheritedMount>> = (0..partitions).map(|_| Vec::new()).collect();
  for mount in handed {
    let attachment = mount.attachment;
    let held = InheritedMount {
      attachment,
      held: checked_inherited(mount),
    };
    let partition = usize::from(crate::verbs::owner_of_attachment(attachment));
    let target = if partition < partitions { partition } else { 0 };
    if let Some(slot) = split.get_mut(target) {
      slot.push(held);
    } else {
      slates_machine::log_line!(
        "slates-server: no shard can adopt held mount {}",
        attachment
      );
    }
  }
  split
}

/// A rejected mount retains its id for release, while every partial set of inherited handles closes here.
fn checked_inherited(mount: crate::anchor_hold::HandedMount) -> Option<Inherited> {
  if !mount.adoptable {
    return None;
  }
  let record = match HeldRecord::decode(&mount.record) {
    Ok(record) => record,
    Err(error) => {
      slates_machine::log_line!(
        "slates-server: held mount {} record refused: {error:?}",
        mount.attachment
      );
      return None;
    }
  };
  let mut handles = mount.handles.into_iter();
  let (Some(volume), Some(link), Some(ledger)) = (handles.next(), handles.next(), handles.next())
  else {
    slates_machine::log_line!(
      "slates-server: held mount {} has fewer than three handles",
      mount.attachment
    );
    return None;
  };
  Some(Inherited {
    volume,
    link,
    ledger,
    record,
  })
}

/// Starts one owned adoption task per matched mount. An acknowledged but uncommitted hold is released, and an
/// adoption refusal ends its catalog record before the anchor's objects and the mount thread are released.
pub(crate) fn adopt_held() {
  let (kept, orphaned) = state::with_state(|s| {
    (
      std::mem::take(&mut s.adopt_winfsp),
      std::mem::take(&mut s.inherited_winfsp),
    )
  })
  .unwrap_or_default();
  if !orphaned.is_empty()
    && let Err(error) = futures::spawn(async move {
      for orphan in orphaned {
        release_hold(orphan.attachment).await;
      }
    })
    .and_then(futures::detach)
  {
    slates_machine::log_line!("slates-server: orphaned mount release task refused: {error}");
  }
  for (record, held) in kept {
    if let Err(error) = futures::spawn(adopt_mount(record, held)).and_then(futures::detach) {
      slates_machine::log_line!("slates-server: mount adoption task refused: {error}");
    }
  }
}

async fn adopt_mount(record: AttachmentRecord, held: InheritedMount) {
  let attachment = record.id;
  let Some(held) = held.held else {
    let _ = state::with_state_counted(|s| {
      refuse_adoption(s, &record, "the held mount record is unusable")
    });
    release_hold(attachment).await;
    return;
  };
  let waker = std::future::poll_fn(|cx| std::task::Poll::Ready(cx.waker().clone())).await;
  let (sender, requests) = sync_channel(REQUEST_BOUND);
  let mounted = match MountThread::start(
    record.form.mount_point().unwrap_or_default(),
    !record.rights.write,
    sender,
    waker.clone(),
    MAX_OPEN_HANDLES,
    Some(held),
  ) {
    Ok(thread) => await_mounted(thread).await,
    Err(error) => Err((error.to_string(), None)),
  };
  let adopted = state::with_state_counted(|s| match mounted {
    Ok((thread, ready)) => match keep(s, &record, thread, requests, waker, ready, true) {
      Ok(()) => {
        s.count("winfsp.adopted", 1);
        Ok(())
      }
      Err((error, thread)) => {
        refuse_adoption(s, &record, &format!("{error:?}"));
        Err(Some(thread))
      }
    },
    Err((why, thread)) => {
      refuse_adoption(s, &record, &why);
      Err(thread)
    }
  });
  match adopted {
    Some(Ok(())) => serve(attachment).await,
    Some(Err(Some(thread))) => reap(thread, attachment, false).await,
    Some(Err(None)) | None => release_hold(attachment).await,
  }
}

fn refuse_adoption(s: &mut ShardState, record: &AttachmentRecord, why: &str) {
  slates_machine::log_line!("slates-server: mount {} adoption refused: {why}", record.id);
  s.count("winfsp.adopt_refused", 1);
  if let Err(error) = crate::verbs::end_attachment(s, record, crate::verbs::Ending::Otherwise) {
    slates_machine::log_line!("slates-server: refused mount end failed: {error:?}");
    s.count(END_REFUSED, 1);
  }
}

/// One attempt at a release: `Ok` once the anchor no longer holds the mount, which a daemon with no anchor never asked
/// it to. A refused attempt is counted.
async fn release_attempt(attachment: u64) -> Result<(), crate::anchor_hold::HoldRefusal> {
  use crate::anchor_hold::HoldRefusal;
  match crate::anchor_hold::release_mount(attachment).await {
    Ok(()) | Err(HoldRefusal::NoChannel) => Ok(()),
    Err(error) => {
      let _ = state::with_state_counted(|s| s.count(HOLD_RELEASE_REFUSED, 1));
      Err(error)
    }
  }
}

/// Repeats the release until the anchor answers or `bound_ns` has passed, then answers the last refusal.
async fn release_within(
  attachment: u64,
  bound_ns: u64,
) -> Result<(), crate::anchor_hold::HoldRefusal> {
  let began = futures::now_ns();
  loop {
    let refusal = match release_attempt(attachment).await {
      Ok(()) => return Ok(()),
      Err(refusal) => refusal,
    };
    if futures::now_ns().saturating_sub(began) >= bound_ns {
      return Err(refusal);
    }
    // An attempt refused at once (not one that waited out its budget) is not repeated in a tight loop.
    crate::daemon::pace(tick_ns()).await;
  }
}

/// Repeats the release until the anchor answers: what the deletion of a file system waits for (see the module docs). The
/// release is idempotent and each attempt is bounded, so this holds one task and at most a message in the pipe; it ends
/// at the answer or at the shard's stop. Only the first refusal is printed, since the rest repeat it.
async fn release_hold(attachment: u64) {
  let mut told = false;
  while let Err(error) = release_attempt(attachment).await {
    if !told {
      slates_machine::log_line!(
        "slates-server: mount {attachment} hold release refused: {error}; repeating it"
      );
      told = true;
    }
    crate::daemon::pace(tick_ns()).await;
  }
}

/// Derived: one tick of the mount's pacing, the daemon's heartbeat over the fleet's polls a period: a deferred round's
/// wait, and the first wait of a purge owed again.
fn tick_ns() -> u64 {
  crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD
}

/// Waits `ns`, or until the task is woken for anything else (a request, a change, the mount thread's outcome), whichever
/// comes first: a purge's wait that never holds up the mount's other work. Where no timer can be armed (off a shard) it
/// waits for a wake alone, as a deferred round does.
async fn idle_or_sleep(ns: u64) {
  let mut timer = std::pin::pin!(futures::sleep(ns));
  let mut armed = false;
  std::future::poll_fn(|cx| {
    if armed {
      return std::task::Poll::Ready(());
    }
    armed = true;
    match timer.as_mut().poll(cx) {
      std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(()),
      std::task::Poll::Ready(Err(_)) | std::task::Poll::Pending => std::task::Poll::Pending,
    }
  })
  .await;
}

/// What one turn did.
enum Turned {
  /// Nothing waited: the task waits for a request, a change, or the mount thread.
  Idle,
  /// A request was served.
  Served,
  /// A rename held WinFsp's lock, so the owed round waits a tick.
  RoundDeferred,
  /// Nothing waited but a purge owed again, due in this many nanoseconds: the task waits that long or until it is woken.
  RetryIn(u64),
  /// The owner may not serve the volume's latest state now (AUD-29-83): no request was taken.
  Fenced,
  /// The mount ends.
  Ended,
}

/// The serve task of one mount, once it is recorded: turn until the mount ends, then tear it down and reap its thread.
async fn serve(attachment: u64) {
  loop {
    match state::with_state(|s| turn(s, attachment)) {
      Some(Turned::Ended) | None => {
        if let Some(Some((thread, released))) = state::with_state_counted(|s| ended(s, attachment))
        {
          reap(thread, attachment, released).await;
        }
        return;
      }
      Some(turned) => wait_after(turned).await,
    }
  }
}

/// What the task waits for after a turn that did not end the mount.
async fn wait_after(turned: Turned) {
  match turned {
    Turned::Served => {
      // A served request is client activity, as a FUSE request is (§4.3, §4.7): the shard spins out its idle window
      // after it, so a burst's next request is taken without a wake.
      slates_rt::registry::with_current(|ctx| ctx.note_activity());
      let _ = futures::yield_now().await;
    }
    // `Ended` never reaches here: the loop ends the mount before it waits.
    Turned::Idle | Turned::Ended => futures::idle().await,
    Turned::RoundDeferred => {
      if futures::sleep(tick_ns()).await.is_err() {
        futures::idle().await;
      }
    }
    Turned::RetryIn(ns) => idle_or_sleep(ns).await,
    Turned::Fenced => crate::daemon::pace(crate::daemon::HEARTBEAT_NS).await,
  }
}

/// Waits for the anchor's release unless it was `released` already, then stops the mount thread and joins it at its last
/// outcome, without holding the shard: the thread has only to stop WinFsp's dispatcher (whose threads return at once, the
/// request channel's receiver being dropped) and remove the drive.
async fn reap(mut thread: MountThread, attachment: u64, released: bool) {
  if !released {
    release_hold(attachment).await;
  }
  thread.stop();
  loop {
    match thread.outcome() {
      Some(Outcome::Stopped) => {
        thread.join();
        return;
      }
      Some(
        Outcome::Mounted(_) | Outcome::Notified(..) | Outcome::Purged(_) | Outcome::Recovered(_),
      ) => {}
      Some(Outcome::Failed(error)) => {
        slates_machine::log_line!("slates-server: ending WinFsp receive worker: {error}")
      }
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
  if let Some(turned) = reconcile_opens(s, mount) {
    return turned;
  }
  if let Some((context, open, name)) = mount.seeds.pop_front() {
    mount.names.opened(context, open.object.inode, &name);
    return Turned::Served;
  }
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
    let retry_in = send_due_retries(s, mount);
    return if deferred {
      Turned::RoundDeferred
    } else {
      retry_in.map_or(Turned::Idle, Turned::RetryIn)
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
        ledger,
        takeover,
        coherence,
        round_out,
        purge_out,
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
          purge_out: *purge_out,
          owed,
          thread,
          told,
          takeover,
        },
      );
      let served = request.map(|request| {
        let served = request.serve(&mut bridge, &cx, names, ledger);
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
    let Ok(slot) = s.volumes.get_mut(handle) else {
      return Turned::Ended;
    };
    mount.seen = slot.volume.change_generation();
    // The earlier wait answered ready without registering. An empty round produces no command outcome to wake us,
    // so rearm at the delivered generation before parking (A-198's SDK-rename mapped-view regression).
    slot.volume.wait_for_change(mount.seen, &mount.waker);
  }
  let retry_in = send_due_retries(s, mount);
  match served {
    Some(served) => {
      answer(s, mount, served);
      Turned::Served
    }
    None if deferred => Turned::RoundDeferred,
    None => retry_in.map_or(Turned::Idle, Turned::RetryIn),
  }
}

/// Settles the image's open-reference counts before taking a captured cleanup/close or any fresh operation. A missing
/// unhanded context is retired in the ledger off-shard first, so this never consumes another context's restored open.
fn reconcile_opens(s: &mut ShardState, mount: &mut WinFspMount) -> Option<Turned> {
  let mut counts = mount.reconcile.take()?;
  let result = s
    .by_id
    .get(&mount.volume)
    .copied()
    .and_then(|handle| s.volumes.get_mut(handle).ok())
    .map_or(Err(slates_vfs::VfsError::StaleHandle), |slot| {
      reconcile_one(&mut slot.volume, &mut s.store, mount.owner, &mut counts)
    });
  match result {
    Ok(false) => {
      mount.reconcile = Some(counts);
      Some(Turned::Served)
    }
    Ok(true) => {
      if counts.changed
        && !crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(mount.volume))
      {
        s.count(BARRIER_REFUSED, 1);
        mount.ending = true;
        return Some(Turned::Ended);
      }
      Some(Turned::Served)
    }
    Err(error) => {
      slates_machine::log_line!("slates-server: WinFsp open-reference recovery refused: {error}");
      mount.ending = true;
      Some(Turned::Ended)
    }
  }
}

fn reconcile_one(
  volume: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  owner: RefOwner,
  counts: &mut RestoredOpenCounts,
) -> Result<bool, slates_vfs::VfsError> {
  let restored = volume
    .next_restored_open(owner, counts.after)
    .map(|(inode, _)| inode);
  let held = counts.held.first_key_value().map(|(inode, _)| *inode);
  let inode = match (restored, held) {
    (None, None) => return Ok(true),
    (Some(inode), None) | (None, Some(inode)) => inode,
    (Some(restored), Some(held)) => restored.min(held),
  };
  let wanted = counts.held.remove(&inode);
  counts.changed |= volume.reconcile_restored_opens(store, InodeNo(inode), owner, wanted)?;
  counts.after = Some(inode);
  Ok(false)
}

/// Reads what the mount thread reported since the last turn: a round taken, deferred or refused (the latter two put the
/// cursor back, so the round is gathered again). `Ok(true)` when a rename deferred it; `Err` when the mount is gone.
fn take_outcomes(s: &mut ShardState, mount: &mut WinFspMount) -> Result<bool, ()> {
  let mut deferred = false;
  while let Some(outcome) = mount.thread.outcome() {
    match outcome {
      Outcome::Notified(told, purged) => {
        if matches!(&told, Ok(Notified::Taken)) {
          mount.takeover = false;
        }
        record_purges(s, &mut mount.retries, &purged);
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
      Outcome::Purged(purged) => {
        mount.purge_out = false;
        record_purges(s, &mut mount.retries, &purged);
      }
      Outcome::Mounted(_) => {}
      Outcome::Recovered(count) => s.count(CAPTURED_SETTLED, count),
      Outcome::Failed(error) => {
        slates_machine::log_line!("slates-server: WinFsp receive worker failed: {error}");
        mount.ending = true;
        s.count(MOUNT_LOST, 1);
      }
      Outcome::Stopped => {
        s.count(MOUNT_LOST, 1);
        return Err(());
      }
    }
  }
  Ok(deferred)
}

/// Counts what a command's purges came to and records each in the mount's retries: a purge Windows did not take is owed
/// again after its wait, and given up, counted kept, once Windows has kept its page past the volume's request timeout
/// (`slates_bridge_winfsp::retry`).
fn record_purges(s: &mut ShardState, retries: &mut PurgeRetries, purged: &Purged) {
  s.count(PURGES_UNAVAILABLE, purged.unavailable);
  let now = futures::now_ns();
  for (purge, outcome) in &purged.outcomes {
    match retries.record(purge, *outcome, now) {
      Recorded::Taken => s.count(PURGES_DONE, 1),
      Recorded::Refused => s.count(PURGES_REFUSED, 1),
      Recorded::Retrying => s.count(PURGES_UNTAKEN, 1),
      Recorded::Kept => {
        s.count(PURGES_UNTAKEN, 1);
        s.count(PURGES_KEPT, 1);
      }
    }
  }
}

/// Sends the mount thread the purges owed again that have come due, when no command is out there, so one command is out
/// at a time. Answers how long the task may wait before the next falls due: `None` when nothing is owed, or when a
/// command is out (its outcome wakes the task).
fn send_due_retries(s: &mut ShardState, mount: &mut WinFspMount) -> Option<u64> {
  if mount.purge_out || mount.round_out.is_some() {
    return None;
  }
  let now = futures::now_ns();
  let due = mount.retries.due(now);
  if due.is_empty() {
    return mount.retries.next_due_in(now);
  }
  if mount.thread.purge(due).is_ok() {
    mount.purge_out = true;
    s.count(PURGE_RETRIES, 1);
    return None;
  }
  // The thread is gone (the next turn reads its stop and ends the mount) or busy: the purges stay owed, sent a tick on.
  Some(tick_ns())
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
  /// A purge-only command is out, so no round is sent until its outcome is read.
  purge_out: bool,
  owed: &'m mut bool,
  thread: &'m MountThread,
  told: &'m mut u64,
  takeover: &'m mut bool,
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
  if mount.round_out.is_some() || mount.purge_out {
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
  if *mount.takeover {
    round.add(&Invalidation::Everything, names);
  }
  *mount.owed = false;
  let too_long = round.too_long();
  if !round.is_empty() {
    let purges = purges_owed(
      bridge,
      cx,
      names,
      (&changed, overruns > 0 || *mount.takeover),
    );
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
        object: ObjectId::new(ino, 0),
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
  if served.is_restored_read() {
    s.count(RESTORED_READS, 1);
  }
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
/// still stands. Returns its thread, for the task to reap off the shard, and whether the anchor has released it.
fn ended(s: &mut ShardState, attachment: u64) -> Option<(MountThread, bool)> {
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
  Some((mount.thread, mount.released))
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
    .filter_map(|attachment| ended(s, attachment).map(|(thread, _)| thread))
    .collect();
  // Start every cancellation before joining any receiver, so the finite control waits overlap across mounts.
  for thread in &mut threads {
    thread.stop();
  }
  for thread in &mut threads {
    thread.join();
  }
}

/// Ends every WinFsp mount and writes no record: all a fenced shard may do at its stop. The records end at the next
/// start's recovery.
pub(crate) fn drop_all(s: &mut ShardState) {
  let mounts = std::mem::take(&mut s.winfsp_mounts);
  let mut threads = Vec::with_capacity(mounts.len());
  for (_, mut mount) in mounts {
    mount.requests = None;
    mount.thread.stop();
    threads.push(mount.thread);
  }
  for thread in &mut threads {
    thread.join();
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
