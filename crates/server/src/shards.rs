//! The daemon's Shards fs transport (§4.6, the Shards route; A-161): a volume served to a Shards microVM's virtio-fs
//! device over an anonymous stream. Shards' VMM forwards each guest FUSE request to a backend over a Unix stream,
//! framed as a little-endian length then the request, and reads one reply frame back per request
//! (`slates_bridge_fuse::shards_fs`, which holds the framing and its evidence). Here the daemon is that backend.
//!
//! **Establishment.** An attach with the Shards form ([`attach`]) checks the principal's rights for its intent,
//! resolves the directory it presents if it names one, and takes the write lease for a write intent (D-16). It makes an
//! anonymous socket pair (`slates_ipc::stream`: no name, nothing on disk, R1), records the attachment
//! (`AttachForm::ShardsFs`, consumer `Guest`), admits it into the shard's registry with the record as its owner, and
//! spawns its serve task on the volume's owner shard, where it lives until it ends. The other end of the pair is the
//! attachment's capability: it goes to whoever attached, who hands it to the VMM, whose process is often sandboxed and
//! cannot open a socket of its own. Whoever holds it reaches this attachment and nothing else, because the daemon serves
//! nothing else on it. A shard serves at most its client seats of streams (`clients_per_shard`), as many as its guest
//! devices.
//!
//! **Serving.** The task waits on its end's readiness through the runtime and takes one frame per turn, each length
//! bounded by the largest frame before anything is allocated for it (`split_frame`). A request is answered through a
//! transient `VolumeBridge` over the shard's slot (a `ScopedBridge` for a directory), admitted under the attachment's
//! registry entry so a barrier over the volume sees it in flight, with the attachment's own open-handle map, kept here
//! across requests. A request that changed what survives a restart (`needs_barrier`, or an effect that waits on the
//! next publication, A-150 and A-154) publishes the shard's recovery image before its reply, as a virtio-fs guest's
//! does (AUD-29-82); a publication refused, or one that did not capture the volume, answers `EIO`, and what the reply
//! granted is given back. The guest's kernel has no path for an invalidation, so the attachment is revalidated (§4.6
//! cache posture: names and attributes checked on every use, pages dropped at each open). A reply the peer is slow to
//! read waits for the end's writability before the next request is read, so a peer that never reads holds one reply,
//! never a queue. While the owner may not serve the volume's latest state (its lease not holding, AUD-29-83), nothing is
//! read: the guest's requests wait in the stream, asked again each heartbeat.
//!
//! **The run's owner (A-281).** Shards' init names the run's owner on a share's root as it mounts it, with Shards' owner
//! word (`trusted.shards.owner`, `UID:GID`), so that a run of an unprivileged user writes the share it was given. The
//! stream takes the word before any dispatch, from the guest's root on the stream's root alone, and keeps it on no file:
//! from then on its guest is shown the owner the volume keeps for the root as the run's owner and the run's as the
//! root's, uid and gid each, and the ids a guest names (a creator, a chown) are kept the other way round. Every other
//! route sees the owners the volume keeps, so a host reads its own files as its own. The swap lives with the stream:
//! nothing survives a restart, and the next stream's guest names its owner again as it mounts.
//!
//! **The guest's attributes (A-290).** A stream is a guest kernel's route: besides `user.` its guest reads and writes
//! its own attribute namespaces, `trusted.` and `security.` (an overlayfs upper directory's `trusted.overlay.*`, a
//! binary's `security.capability`), kept in the volume as its data. No host's mount shows or sets them.
//!
//! **Control asks.** Shards' device sends asks of its own:
//! - its backend's tables for a snapshot of the VM, and their restore (Shards' D176 part 5; A-303). A SAVE, sent while
//!   the device is paused and so between requests, is held while the guest's stores through its maps reach the volume,
//!   then answered with the tables of the child module `tables`: a snapshot of the volume at the cut, taken for the
//!   stream's own principal and made durable first, the inodes the guest holds with their references and opens, its
//!   scope and its owner view. A RESTORE is taken only as a fresh stream's first frame, and only onto the volume saved,
//!   unchanged since the cut, or a clone of the cut's snapshot, presenting the same directory: the stream's attachment
//!   is then given what the saved one held, its owner view too. Anything else is refused in the form Shards reads, a
//!   control answer with its reason, and changes nothing.
//! - a file's descriptor for a DAX window's mapping (D189), answered by the child module `dax` (Shards part 3;
//!   `docs/wip/shards/dax-protocol.md`). The answer is a descriptor of the file's DAX object, one per attachment and
//!   file, holding only that file's bytes, filled a slice a turn. A descriptor of slates' content itself would hand the
//!   VMM the whole content object (A-135). A refusal is an errno its guest's mapping returns.
//!
//! **Teardown.** The peer closing its end, a frame refused as hostile, an error on the stream, a `detach`, a
//! revocation, the volume's destroy and the daemon's stop all end the attachment the same way ([`ended`]): its
//! references are swept through the bridge (a guest sends no forget per reference at its end), its registry entry
//! revoked and drained, its descriptor closed, and its record removed as a recorded operation. Anything but the stream
//! itself ends the stream first ([`end_stream`]), so the task wakes on its own readiness and ends the rest; the stream
//! never outlives its record. A daemon's death ends nothing by itself: recovery keeps a stream whose resync end the
//! anchor held (A-316; macOS too, A-339) and serves it again on a fresh stream ([`adopt_held`]), and removes any other's
//! record, as it removes every guest's.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(windows)]
use std::os::windows::io::AsHandle;

use slates_bridge_core::volume_bridge::new_handle_store;
use slates_bridge_core::{AttachmentId, ObjectId, OwnerView, Rights, View, VolumeBridge};
use slates_bridge_fuse::Opcode;
use slates_bridge_fuse::bridge::{
  EIO, KernelChange, kernel_changes, needs_barrier, reclaim_unreported,
};
use slates_bridge_fuse::shards_fs::{
  ASK_DESCRIPTOR, ASK_RESTORE, ASK_SAVE, Answered, FRAME_BYTES, Frame, LENGTH_BYTES, OwnerWord,
  Split, answer, control_answer, control_refusal, owner_word, owner_word_reply, refuse,
  split_frame_bounded,
};
use slates_db::Op;
use slates_db::catalog::{AttachForm, AttachmentRecord, Consumer, Principal, VolumeId};
use slates_ipc::protocol::{Intent, Refusal, ReplyBody};
use slates_mem::slab::Slab;
use slates_rt::futures;
use slates_vfs::clock::Clock;
use slates_vfs::host::HostFs;

use slates_rt::control::Control;
use slates_rt::registry;
use slates_rt::task::SpawnRequest;

use crate::daemon::Daemon;
use crate::error::ServerError;
use crate::state::{self, ShardState};

// The DAX half (Shards part 3): a descriptor ask is answered with a file's DAX object, by descriptor on Unix and by a
// section duplicated into the claimant's process on Windows (plan G-shards-windows step 5).
mod dax;
// The daemon's end of the stream as each platform serves it: one seam, paired arms.
mod end;
// A stream's tables for a VM's save and restore (A-303).
mod tables;
// A stream's resync channel, the daemon's half (A-316): Linux hands the device its end with the stream's on the control
// socket, macOS beside it on the socket route (A-339); the session's bytes on every platform.
mod resync;
// The resync channel's I/O on Windows (A-427): a message-mode pipe, held by the anchor through its exchange, the device's
// end handed by the handle route.
#[cfg(windows)]
mod resync_handle;
// A resyncable stream's DAX objects held by the anchor, and their adoption by a successor (A-365).
#[cfg(unix)]
pub(crate) mod dax_hold;

pub use end::HandedEnd;
use end::{DaemonEnd, Received, Sent};

/// What an ingest was for, on a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Then {
  /// The held request of `total` bytes at the head of the stream is served next.
  Serve {
    /// The request's bytes, its length word included.
    total: usize,
  },
  /// The stream ends next.
  End,
  /// The stream goes on: a resynced stream's adopted DAX objects were ingested before it served anything (A-365).
  Resume,
}

/// A stream's finished ingest.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Done {
  /// What asked for it.
  pub(super) then: Then,
  /// Whether anything was written: the change is then published before what asked for it goes on.
  pub(super) wrote: bool,
  /// The errno of the first write-back the bridge refused, which the request that asked for it answers.
  pub(super) refused: Option<i32>,
}

/// Format: the refusal-ledger name of an ending stream's ingested writes that the publication after them did not
/// capture: they reach the volume, and are made durable by the next publication.
const END_UNPUBLISHED: &str = "shards.dax_end_unpublished";
/// Format: the refusal-ledger name of a resynced stream's adopted DAX stores (A-365) that the publication after their
/// ingest did not capture: they reach the volume, and are made durable by the next publication.
const ADOPTED_UNPUBLISHED: &str = "shards.dax_adopted_unpublished";
/// Format: the refusal-ledger name of a frame the protocol refuses (a length past the largest frame, a control frame
/// with no ask): the stream that sent it is ended.
const FRAME_REFUSED: &str = "shards.frame_refused";
/// Format: the refusal-ledger name of a control ask refused (a snapshot's tables, their restore, a DAX descriptor).
const CONTROL_REFUSED: &str = "shards.control_refused";
/// Format: the refusal-ledger name of a barrier that did not capture its volume: the reply was answered `EIO`.
const BARRIER_REFUSED: &str = "shards.barrier_refused";
/// Counter: an `FSYNC` reply held until its live-shipped volume's log was durable where its barrier put it (A-200).
const LIVE_FSYNC_HELD: &str = "shards.live_fsync_held";
/// Counter: a held `FSYNC` answered `EIO`: its volume's log was not durable within the liveness budget, or never will
/// be here (the volume destroyed, its owner superseded).
const LIVE_FSYNC_REFUSED: &str = "shards.live_fsync_refused";
/// Format: the refusal-ledger name of the references and open handles given back for a reply refused after it was
/// made.
const REPLY_RECLAIMED: &str = "shards.reply_reclaimed";
/// Format: the refusal-ledger name of a stream that failed (a read or write error other than its peer's close, or a
/// readiness wait the runtime refused).
const STREAM_FAILED: &str = "shards.stream_failed";
/// Format: the refusal-ledger name of a stream whose peer closed it part-way through a frame (a truncated frame): the
/// part is dropped unanswered and the stream ends, as at a clean close, but the kind is kept apart from one.
const FRAME_TRUNCATED: &str = "shards.frame_truncated";
/// Format: the refusal-ledger name of a sweep of an ended attachment's references that the volume refused.
const SWEEP_REFUSED: &str = "shards.sweep_refused";
/// Format: the refusal-ledger name of an ended attachment whose record could not be removed.
const RECORD_END_REFUSED: &str = "shards.record_end_refused";
/// Format: the refusal-ledger name of a stream the daemon could not end ([`end_stream`]): its task ends at its next
/// turn regardless, as the stream is marked ending first.
const END_REFUSED: &str = "shards.end_refused";
/// Format: the refusal-ledger name of a stream end withdrawn unclaimed from the socket route because its attachment
/// ended first (macOS): an ended attachment leaves no end for a claim to take (A-173's rule for offers).
#[cfg(target_os = "macos")]
const OFFER_WITHDRAWN: &str = "shards.route_withdrawn";
/// Format: the refusal-ledger name of a stream end that could not be sent to its client (no control socket, the
/// client gone, a refused send): the end closes, so the stream ends and its attachment with it.
const DELIVERY_REFUSED: &str = "shards.delivery_refused";
/// Format: the refusal-ledger name of a hand-off whose holder could not be told to the stream's owner shard (Windows):
/// that stream's DAX asks are then refused.
#[cfg(windows)]
const HOLDER_UNTOLD: &str = "shards.holder_untold";
/// Format: the refusal-ledger name of a `ShardsHolder` whose named handle is not the stream's (Windows, A-181).
#[cfg(windows)]
const HOLDER_REFUSED: &str = "shards.holder_refused";
/// Format: the refusal-ledger name of a `ShardsHolder` that met a stream with no known holder yet (Windows, A-181):
/// the attach's own holder record had not reached the stream's shard, so it is refused `HolderPending` and resent.
#[cfg(windows)]
const HOLDER_PENDING: &str = "shards.holder_pending";
/// Format: the counter of processes `ShardsHolder` added as known holders of a stream (Windows, A-181): `slates
/// shards` adds the command it started, so each such run moves it once.
#[cfg(windows)]
const HOLDER_ADDED: &str = "shards.holder_added";
/// Format: the refusal-ledger name of a stream end dropped because its request's reply was not the attach that made
/// it (a rolled-back verb answered a refusal): the end closes, so the stream ends with it.
const HANDOFF_DROPPED: &str = "shards.handoff_dropped";
/// Format: the refusal-ledger name of a stream served with no resync channel (A-316, A-427): the secure random gave no token,
/// or the OS refused the pair or its first message. The stream ends at a daemon's death, as before.
const RESYNC_UNMADE: &str = "shards.resync_unmade";
/// Counter: a stream whose resync end the anchor does not hold (A-316): the daemon has no anchor (a test, a standalone
/// start), or the anchor refused the hold or its session (on Windows, A-427: or its answer did not come). The device
/// reads the channel's end; the stream ends at a daemon's death, as before.
const RESYNC_UNHELD: &str = "shards.resync_unheld";
/// Counter: a stream that gave its resync up while it lived (A-316): the anchor refused to hold a DAX object its guest
/// maps, or the range table to record its ranges (A-365), or the anchor refused the owner view's update; on Windows
/// (A-427), its guest's first DAX map, a refused session, or its device's end not handed to the client.
const RESYNC_GIVEN_UP: &str = "shards.resync_given_up";
/// Counter: a process named to hold a Windows stream (its attaching client, or one a `ShardsHolder` names) whose
/// creation time could not be read: the session keeps the holder it had (A-427).
#[cfg(windows)]
const RESYNC_HOLDER_UNNAMED: &str = "shards.resync_holder_unnamed";
/// Counter: a stream served again on a fresh stream after its daemon's death, its device handed the fresh end over the
/// resync channel the anchor held (A-316); the successor's non-vacuity counter.
const RESYNCED: &str = "shards.resynced";
/// Format: the refusal-ledger name of a kept stream the successor could not serve again (A-316): its hold's session
/// unreadable, the OS refusing the fresh pair, the registry or the task refused, or the device gone (the `Resync`
/// refused). The attachment is ended and its hold released: Shards' option (a).
const RESYNC_REFUSED: &str = "shards.resync_refused";
/// Counter: a resent request answered from the reply its dead daemon published with its effect and never wrote (A-316,
/// A-61's rule): never applied twice; the replay's non-vacuity counter.
const REPLAYED: &str = "shards.replayed";
/// Counter: a resynced stream's device showed it holds the fresh stream's end, by its first bytes on it, so this
/// daemon's copy of that end was let go (A-339); the kept copy's non-vacuity counter.
#[cfg(unix)]
const RESYNC_TAKEN: &str = "shards.resync_taken";
/// Counter: the resync channel of a resynced stream woke while this daemon still kept its copy of the fresh stream's end
/// (the device's end of the channel closed: the device went before it took the stream, A-339). The copy is let go, so
/// the stream sees its peer gone and ends its attachment.
#[cfg(unix)]
const RESYNC_ABANDONED: &str = "shards.resync_abandoned";
/// Counter: a stream resynced after a death whose daemon acknowledged writes its write log could not keep (the log
/// overflowed and the publication that would have emptied it was refused): its guest is told every file it had open
/// may have lost writes, as a taken-over FUSE mount's kernel is (`LostWrites`; A-393).
const RESYNC_WRITES_LOST: &str = "shards.resync_writes_lost";
/// Counter: a reply made and never written, under the test-support switch `shards_withhold_replies` (A-316).
const REPLY_WITHHELD: &str = "shards.reply_withheld";
/// Counter: a `close`'s `FLUSH` answered with no publication of its own, since the write log held every write the shard
/// acknowledged (A-399, A-63's flush rule): the fast path's non-vacuity count.
const FLUSH_LOGGED: &str = "shards.flush_logged";
/// Format: the refusal-ledger name of a resync end's release the anchor's channel refused (A-316): the anchor keeps the
/// end until the next daemon releases it as one no record of its kind claims, so the device reads the channel's end
/// only then.
const RESYNC_RELEASE_REFUSED: &str = "shards.resync_release_refused";
/// Counter: an owner word taken (A-281): a guest's root named its run's owner on the stream's root, which the stream
/// shows its guest as the owner of what the volume keeps as its root's owner's, and the reverse.
const OWNER_NAMED: &str = "shards.owner_named";
/// Format: the refusal-ledger name of an owner word refused (A-281): from a caller other than the guest's root
/// (`EPERM`), on a node other than the stream's root or with no `UID:GID` (`EINVAL`), or with its root unreadable.
const OWNER_REFUSED: &str = "shards.owner_refused";
/// Counter: a SAVE answered with the stream's tables (A-303).
const SAVED: &str = "shards.saved";
/// Format: the refusal-ledger name of a SAVE refused (A-303), with no snapshot left behind but one taken before a later
/// step failed.
const SAVE_REFUSED: &str = "shards.save_refused";
/// Counter: a RESTORE taken: the stream's attachment holds what the saved one did (A-303).
const RESTORED: &str = "shards.restored";
/// Format: the refusal-ledger name of a RESTORE refused (A-303), which changed nothing.
const RESTORE_REFUSED: &str = "shards.restore_refused";
/// Format: the reason a SAVE carries when it brings anything: Shards' device sends none.
const SAVE_MALFORMED: &str = "a SAVE brings nothing after its ask";
/// Format: the reason a SAVE carries when the guest's stores through its maps could not reach the volume first.
const SAVE_STORES_REFUSED: &str =
  "the guest's stores through its maps could not reach the volume before the cut";
/// Format: the reason a SAVE carries when the stream's principal may not snapshot its volume.
const SAVE_FORBIDDEN: &str =
  "the share's principal may not snapshot its volume, so its VM cannot be saved with it";
/// Format: the reason a SAVE carries when another attachment's request was in flight at the cut.
const SAVE_IN_FLIGHT: &str = "another attachment's request was in flight at the cut; save again";
/// Format: the reason a SAVE carries when the volume refused its snapshot otherwise.
const SAVE_UNSNAPSHOTTED: &str = "the volume refused the save's snapshot";
/// Format: the reason a SAVE carries when its snapshot could not be made durable before the answer.
const SAVE_UNDURABLE: &str = "the save's snapshot could not be made durable";
/// Format: the reason a SAVE carries when its tables are longer than a control frame carries.
const SAVE_TOO_LONG: &str = "the stream's tables are longer than a control frame carries";
/// Format: the reason a SAVE or a RESTORE carries when the stream can no longer be served (its volume or its
/// attachment gone).
const STREAM_GONE: &str = "the share's volume or attachment is gone";
/// Format: the reason a RESTORE carries when it is not the stream's first frame.
const RESTORE_NOT_FIRST: &str = "a restore must be a fresh stream's first frame";
/// Format: the reason a RESTORE carries when the stream serves neither the volume saved, unchanged since the cut, nor
/// a clone of the cut's snapshot.
const RESTORE_ELSEWHERE: &str = "the stream serves neither the saved volume unchanged since the save nor a clone of the save's snapshot";
/// Format: the reason a RESTORE carries when the stream presents another directory than the saved one did.
const RESTORE_ANOTHER_DIRECTORY: &str =
  "the stream presents another directory than the saved share";
/// Format: the reason a RESTORE carries when a file the saved guest held is not in the volume.
const RESTORE_FILE_GONE: &str = "a file the saved guest held is not in this volume";
/// Format: the reason a RESTORE carries when its tables name what no guest could hold.
const RESTORE_INVALID: &str = "the tables name references no guest could hold";
/// Format: the reason an unknown control ask's refusal carries to Shards.
const UNKNOWN_ASK: &str = "a control ask slates does not know";

/// One attachment served over a stream: its end of the pair, the open-handle map kept across requests, the registry
/// attachment its requests are admitted under, what it presents, and the frames in and out.
pub(crate) struct ShardsStream {
  /// The attachment's id, the key the shard keeps the stream under, and its owner in the volume's reference ledger.
  attachment: u64,
  /// The daemon's end of the pair: a non-blocking socket (Unix), or a pipe end its shard's driver serves (Windows).
  stream: DaemonEnd,
  /// The attachment's open file handles, kept across requests as FUSE keeps them.
  handles: Slab<u64>,
  /// The registry attachment its requests are admitted under.
  registry: AttachmentId,
  /// The volume it presents.
  volume: VolumeId,
  /// The directory it presents, if one (AUD-29-76): its bridge answers nothing outside it.
  scope: Option<u64>,
  /// Bytes read from the stream and not yet taken as frames: the first `filled`. Its length is its high-water mark, at
  /// most one largest frame, so a buffer is zeroed only when it grows.
  inbound: Vec<u8>,
  /// How many of `inbound`'s bytes were read.
  filled: usize,
  /// The reply frame being written, at the head: its length grows to the largest reply asked and falls back to one
  /// largest frame once a larger reply is out.
  outbound: Vec<u8>,
  /// The reply frame's bytes: zero when none is owed.
  owed: usize,
  /// How many of the owed bytes are out.
  sent: usize,
  /// Set when the attachment ends by anything but the stream (a detach, a revocation, a destroy): the task reads nothing
  /// more and ends it at its next turn.
  ending: bool,
  /// The backed DAX session of the files this attachment's guest maps (Shards part 3, [`dax`]): their backings, and the
  /// fill, ingest and refill under way, each a slice a turn.
  dax: dax::Session,
  /// The descriptor the owed reply carries by `SCM_RIGHTS` on its first byte: sent with it, then closed (Unix; none
  /// exists on Windows).
  owed_descriptor: Option<dax::Descriptor>,
  /// The processes known to hold this stream, each with the value of its handle there: the attaching client, once the
  /// attach's hand-off is recorded, and the last DAX claimant that passed (Windows; `dax::record_holder`).
  #[cfg(windows)]
  holders: Vec<(u32, u64)>,
  /// The process the DAX ask being filled is answered into, judged to hold the stream (Windows).
  #[cfg(windows)]
  claimant: Option<slates_ipc::handle_route::Claimant>,
  /// Whether the ingest a stream takes before it ends has been started: once, whatever ends it.
  ingested_at_end: bool,
  /// Where the volume's journal stood when the request held for its ingest was taken: the records the ingest writes
  /// are then that request's own (AC-3.3, A-196).
  told_mark: Option<crate::told::Mark>,
  /// What the owed reply waits for before it is sent: every FUSE mount of the volume told through the newest record its
  /// request made (AC-3.3, A-196). Shards' VMM asks one request at a time, so nothing else waits behind it.
  told: Option<crate::told::Owed>,
  /// The owed reply is an `FSYNC` whose barrier published a live-shipped volume not yet durable there (A-200): it is
  /// sent once the volume's log is durable at this position, and replaced by `EIO` (to request `unique`) if it is not
  /// within the liveness budget, since a guest's `fsync` must not claim what no candidate holds.
  durable: Option<DurableOwed>,
  /// How the guest is shown the volume's owners (A-281): the identity until its root names the run's owner with
  /// Shards' owner word, then a swap of the root's owner then and the run's. Kept for the stream's life; a restart ends
  /// the stream, and the next one's guest names it again as it mounts. A RESTORE brings the saved stream's (A-303).
  owners: OwnerView,
  /// Whether the stream has read no frame yet: only then may a RESTORE come, and only then may a control frame run to
  /// [`ShardsStream::restore_most`] (A-303).
  restorable: bool,
  /// Whether its guest's kernel sends the extended `fuse_setxattr_in` (A-378): its `INIT` reply kept
  /// `FUSE_SETXATTR_EXT`. Kept for the stream's life, through a resync (the held session) and a restore (the tables), so
  /// every request is parsed in the form the kernel sends it.
  setxattr_ext: bool,
  /// Derived: the longest tables this shard could make ([`tables::most_bytes`] of its inode slab's capacity). With its
  /// ask byte, the bound on a fresh stream's first control frame, past which it is refused before anything is allocated
  /// for it.
  restore_most: usize,
  /// The files whose acknowledged writes its dead daemon's death lost, for a stream served again after it (A-393): every
  /// file, where that daemon's write log overflowed; none otherwise, and none for a stream no daemon died under. Told
  /// to the guest as a taken-over FUSE mount's kernel is told (`LostWrites`): a handle from before the death answers
  /// its `flush` `EIO` until an `fsync` through it has, once.
  lost: slates_bridge_core::LostWrites,
  /// The stream's resync channel (A-316): its token, and the daemon's own copy of its end while the anchor holds one, so
  /// a successor can hand the device a fresh stream. None where the channel was not made.
  #[cfg(unix)]
  resync: Option<resync::Resync>,
  /// The stream's resync channel on Windows (A-427): its token, the daemon's end until the anchor answers its hold, the
  /// hold, the stream's holder and the session's version told. None where the channel was not made.
  #[cfg(windows)]
  resync: Option<resync_handle::HandleResync>,
  /// The serve task's waker while it waits for the stream (Windows, A-427): a holder named outside the task wakes it, so
  /// the anchor is told the session at once rather than at the guest's next request.
  #[cfg(windows)]
  waker: Option<std::task::Waker>,
  /// On a stream served again after its daemon's death (A-316): the reply the dead daemon published with its effect and
  /// never wrote, by the request's unique, answered when the device resends that request as its first, and dropped at
  /// any other first request (A-61's rule).
  replay: Option<(u64, Vec<u8>)>,
  /// On a stream served again after its daemon's death: this daemon's copy of the device's end of the fresh stream,
  /// kept until the device shows it holds its own, by its first bytes on the stream or by a wake of the resync channel
  /// (its end closing). XNU flushes a socket whose every copy rides a message in flight whenever its collector runs
  /// (`resync`'s module notes), and the `Resync` carrying the end waits in the device's channel until the device reads
  /// it, so the end is never left only there (A-339). The serve task watches the channel while a copy is kept
  /// ([`wait_for_work`]).
  #[cfg(unix)]
  fresh_copy: Option<HandedEnd>,
  /// The DAX objects of this stream's backings the anchor holds (A-365; [`dax_hold`]), by inode: each its copy id, so its
  /// backing's end releases it. Only a stream whose resync the anchor holds has any.
  #[cfg(unix)]
  held_dax: BTreeMap<u64, u64>,
}

impl ShardsStream {
  /// A stream for `attachment`, served on `stream` under `registry`, presenting `scope` of `volume`: nothing read or
  /// owed yet, the identity owner view, no resync channel, and a restore allowed as its first frame, whose tables may
  /// run to `restore_most`.
  fn new(
    attachment: u64,
    stream: DaemonEnd,
    registry: AttachmentId,
    volume: VolumeId,
    scope: Option<u64>,
    restore_most: usize,
  ) -> ShardsStream {
    ShardsStream {
      attachment,
      stream,
      handles: new_handle_store(),
      registry,
      volume,
      scope,
      inbound: Vec::new(),
      filled: 0,
      outbound: Vec::new(),
      owed: 0,
      sent: 0,
      ending: false,
      dax: dax::Session::new(),
      owed_descriptor: None,
      #[cfg(windows)]
      holders: Vec::new(),
      #[cfg(windows)]
      claimant: None,
      ingested_at_end: false,
      told_mark: None,
      told: None,
      durable: None,
      owners: OwnerView::IDENTITY,
      restorable: true,
      setxattr_ext: false,
      restore_most,
      lost: slates_bridge_core::LostWrites::default(),
      resync: None,
      #[cfg(windows)]
      waker: None,
      replay: None,
      #[cfg(unix)]
      fresh_copy: None,
      #[cfg(unix)]
      held_dax: BTreeMap::new(),
    }
  }
}

/// What an owed `FSYNC` reply waits for on a live-shipped volume (A-200): the log position its barrier published, and
/// the request it answers.
#[derive(Clone, Copy, Debug)]
struct DurableOwed {
  target: u64,
  unique: u64,
}

impl std::fmt::Debug for ShardsStream {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardsStream")
      .field("volume", &self.volume)
      .field("scope", &self.scope)
      .field("ending", &self.ending)
      .finish()
  }
}

/// The shard's Shards streams, by attachment id.
pub(crate) type ShardsStreams = BTreeMap<u64, ShardsStream>;

/// A socket end owed to the client whose attach made it: a Shards stream's (A-161) or a vhost-user device's (G-vhost,
/// `crate::virtiofs::attach_verb`). Kept on the client's shard under the attach's request word until the attach's reply
/// is written (`verbs::send_reply`), and sent over the client's control socket just before it, only with that reply.
/// Made on the volume's owner shard; a forwarded attach carries it back to the client's shard with the reply
/// (`verbs::send_forward`).
pub(crate) struct Handoff {
  /// The attachment the daemon serves on the stream's other end.
  pub(crate) attachment: u64,
  /// The shard that serves the stream, told who holds it once it is handed (Windows: `tell_holder`).
  #[cfg(windows)]
  pub(crate) owner: u16,
  /// The end to hand the client.
  pub(crate) stream: HandedEnd,
  /// The device's end of the stream's resync channel, handed right after the stream's (A-316; Linux), or duplicated into
  /// the client beside it (A-427; Windows).
  #[cfg(any(target_os = "linux", windows))]
  pub(crate) resync: Option<HandedEnd>,
}

/// The shard's stream ends owed to clients, by the request word of the attach that made each. Bounded by the
/// requests the shard's clients may have in flight ([`handoff_bound`]): an attach past it is refused `Overloaded`.
pub(crate) type Handoffs = BTreeMap<u64, Handoff>;

/// Derived: the socket ends a shard may owe at once: one per request its clients may have in flight
/// (`clients_per_shard × ring slots`), the bound `verbs::remember_forwarded_ring` keeps for forwarded requests.
pub(crate) fn handoff_bound(s: &ShardState) -> usize {
  s.config
    .clients_per_shard
    .saturating_mul(usize::try_from(s.config.region.slots).unwrap_or(1))
}

/// What an attach with the Shards form returns: its record's id, the write lease's epoch for a write intent, and the
/// stream end that is the attachment's capability.
#[derive(Debug)]
pub struct ShardsAttached {
  /// The attachment record's id (`detach` ends it).
  pub attachment: u64,
  /// The write lease's epoch, for a write intent.
  pub lease_epoch: Option<u64>,
  /// The other end of the stream the daemon serves the attachment on, for the VMM.
  pub stream: HandedEnd,
  /// The device's end of the stream's resync channel (A-316, A-339; Windows, A-427), for the VMM beside the stream's:
  /// none where the channel was not made.
  pub resync: Option<HandedEnd>,
}

/// The caller's callback for an attach with the Shards form: the attachment and its stream end, or the typed refusal.
pub type OnShardsAttached = Box<dyn FnOnce(Result<ShardsAttached, Refusal>) + Send>;

/// What an attach with the Shards form asks for.
#[derive(Clone, Debug)]
pub struct ShardsAsk {
  /// Who attaches: the rights checked and recorded are this principal's.
  pub principal: Principal,
  /// The volume.
  pub volume: slates_ipc::protocol::VolumeId,
  /// The directory presented, as a path from the volume's root: the guest's root, nothing outside it reached.
  pub subtree: Option<String>,
  /// Reads only, or reads and writes (which takes the volume's write lease).
  pub intent: Intent,
}

/// Attaches `ask`'s volume over a new stream and spawns its serve task on this shard (the volume's owner): the rights
/// first (§4.13), then the directory, the stream, the write lease, the registry, the record, the task, in the order
/// that takes nothing before it is allowed and gives back what it took when a later step is refused.
pub(crate) fn attach(s: &mut ShardState, ask: ShardsAsk) -> Result<ShardsAttached, Refusal> {
  let ShardsAsk {
    principal,
    volume,
    subtree,
    intent,
  } = ask;
  let volume = VolumeId {
    bytes: volume.bytes,
  };
  if !s.by_id.contains_key(&volume) {
    return Err(Refusal::NotFound);
  }
  let record = s
    .db
    .partition()
    .volume(volume)
    .cloned()
    .ok_or(Refusal::NotFound)?;
  let rights = crate::verbs::rights_of(&record, &principal);
  if !crate::verbs::permits(rights, intent) {
    return Err(Refusal::Forbidden {
      verb: "attach".to_owned(),
    });
  }
  let scope = subtree
    .as_deref()
    .map(|subtree| crate::verbs::resolve_scope(s, volume, subtree))
    .transpose()?;
  if s.shards_streams.len() >= s.config.clients_per_shard {
    return Err(Refusal::Overloaded { shard: s.shard });
  }
  let (daemon_end, handed) = end::pair().map_err(|_| Refusal::Overloaded { shard: s.shard })?;
  let now = s.clock.monotonic_ns();
  let lease_epoch = crate::verbs::lease_for(s, &record, &principal, intent, now)?;
  let granted = crate::verbs::granted_rights(rights, intent);
  let attachment = crate::verbs::next_attachment_id(s);
  let attachment_record = AttachmentRecord {
    id: attachment,
    volume,
    consumer: Consumer::Guest,
    snapshot: None,
    form: AttachForm::ShardsFs { scope },
    principal: principal.clone(),
    rights: granted,
    // A guest presents no mount capability: the zero token is refused by every edge that checks one.
    token: [0; 16],
  };
  let registry = match s.attachments.attach(
    volume,
    View::Current,
    principal,
    Rights {
      read: granted.read,
      write: granted.write,
    },
  ) {
    Ok(registry) => registry,
    Err(e) => {
      crate::verbs::release_unrecorded_lease(s, &attachment_record);
      return Err(crate::error::refusal_of_vfs(&e));
    }
  };
  s.attachments.set_owner(registry, attachment);
  // The resync channel before the record, so a committed record never names a channel the anchor was not given.
  #[cfg(unix)]
  let mut established = establish_resync(s, attachment);
  // Windows makes the channel here and asks the anchor to hold it from the serve task, before the stream serves anything
  // (A-427): its hold is an exchange that waits, which an attach's transaction cannot.
  #[cfg(windows)]
  let established = establish_handle_resync(s, attachment);
  let op = Op::AttachmentAdded {
    record: attachment_record.clone(),
  };
  if let Err(e) = s.db.mutate(&mut s.segment, &op, now) {
    #[cfg(unix)]
    if let Some(established) = established.as_mut()
      && resync::give_up(attachment, &mut established.resync) == resync::GivenUp::Refused
    {
      s.count(RESYNC_RELEASE_REFUSED, 1);
    }
    s.attachments.revoke(registry);
    s.attachments.drain(registry);
    crate::verbs::release_unrecorded_lease(s, &attachment_record);
    return Err(crate::error::refusal_of_db(&e));
  }
  let fresh = ShardsStream::new(
    attachment,
    daemon_end,
    registry,
    volume,
    scope,
    tables::most_bytes(s.config.store.max_inodes),
  );
  // The stream keeps its channel's token and its own copy of its end while the anchor holds one (A-316, A-339); the
  // device's end goes out with the stream's.
  #[cfg(unix)]
  let (fresh, resync) = match established {
    Some(resync::Established { end, resync }) => (
      ShardsStream {
        resync: Some(resync),
        ..fresh
      },
      Some(end),
    ),
    None => (fresh, None),
  };
  #[cfg(windows)]
  let (fresh, resync) = match established {
    Some(resync_handle::Established { device, resync }) => (
      ShardsStream {
        resync: Some(resync),
        ..fresh
      },
      Some(device),
    ),
    None => (fresh, None),
  };
  s.shards_streams.insert(attachment, fresh);
  if futures::spawn(serve(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    ended(s, attachment);
    return Err(Refusal::Overloaded { shard: s.shard });
  }
  Ok(ShardsAttached {
    attachment,
    lease_epoch,
    stream: handed,
    resync,
  })
}

/// Keeps `stream`'s resync through its guest's DAX map of `inode`, just answered (A-365; `dax::answer`): the anchor holds
/// the object and the range table records its ranges ([`dax_hold::keep`]), so a successor can take them back. Where
/// either is refused, or the answer names no file, the resync is given up instead ([`give_up_resync_for_dax`]). A stream
/// whose resync no anchor holds keeps nothing.
#[cfg(unix)]
fn keep_resync_through_dax(s: &mut ShardState, stream: &mut ShardsStream, inode: Option<u64>) {
  if !stream.resync.as_ref().is_some_and(resync::Resync::held) {
    return;
  }
  if inode.is_some_and(|inode| dax_hold::keep(s, stream, inode)) {
    return;
  }
  give_up_resync_for_dax(s, stream);
}

/// Gives `stream`'s resync up, counted, where its guest's DAX objects cannot be held for a successor (A-316, A-365): the
/// device reads its resync channel's end and knows its share ends at a daemon's death, and every object the anchor held
/// for it is let go.
#[cfg(unix)]
fn give_up_resync_for_dax(s: &mut ShardState, stream: &mut ShardsStream) {
  dax_hold::let_go_all(s, stream);
  let Some(resync) = stream.resync.as_mut() else {
    return;
  };
  match resync::give_up(stream.attachment, resync) {
    resync::GivenUp::Nothing => {}
    resync::GivenUp::Released => s.count(RESYNC_GIVEN_UP, 1),
    resync::GivenUp::Refused => {
      s.count(RESYNC_GIVEN_UP, 1);
      s.count(RESYNC_RELEASE_REFUSED, 1);
    }
  }
}

/// Gives `stream`'s resync up, counted, at its guest's first DAX map (Windows, A-427): the guest's stores through the
/// section live where only the device maps them once this daemon is gone, and no anchor holds a Windows DAX section
/// (A-365 holds DAX objects on Unix alone), so a successor could not take them back.
#[cfg(windows)]
fn give_up_resync_for_dax(s: &mut ShardState, stream: &mut ShardsStream) {
  if resync_handle::give_up(s, stream) {
    s.count(RESYNC_GIVEN_UP, 1);
  }
}

/// Makes `attachment`'s resync channel (A-316; [`resync::establish`]), counting a channel not made (the stream is then
/// served without one, ending at a daemon's death as before) and one the anchor was not given.
#[cfg(unix)]
fn establish_resync(s: &mut ShardState, attachment: u64) -> Option<resync::Established> {
  let fresh = resync::Kept {
    owners: OwnerView::IDENTITY,
    setxattr_ext: false,
  };
  match resync::establish(attachment, fresh) {
    Ok(established) => {
      if !established.resync.held() {
        s.count(RESYNC_UNHELD, 1);
      }
      Some(established)
    }
    Err(_) => {
      s.count(RESYNC_UNMADE, 1);
      None
    }
  }
}

/// Makes `attachment`'s resync channel on Windows (A-427; [`resync_handle::establish`]), counting a channel not made:
/// the stream is then served without one, ending at a daemon's death as before. Its serve task asks the anchor to hold
/// it before it reads anything on the stream.
#[cfg(windows)]
fn establish_handle_resync(
  s: &mut ShardState,
  attachment: u64,
) -> Option<resync_handle::Established> {
  match resync_handle::establish(attachment) {
    Ok(established) => Some(established),
    Err(_) => {
      s.count(RESYNC_UNMADE, 1);
      None
    }
  }
}

/// The attach verb's Shards form (§4.4, §4.6; A-161), on the volume's owner shard inside the verb's transaction: the
/// attachment made and served as [`attach`] makes it, and its end handed out by the platform's route ([`hand_out`]):
/// Linux keeps it for the client's control socket, sent just before the reply this returns; macOS offers it on the
/// socket route, the reply carrying the claim's secret. Where the transport report refuses the form (no route here,
/// or the route's listener unbound) it is refused by the report's reason before any effect. A snapshot is not
/// presented over a stream yet (refused `Unsupported`).
pub(crate) fn attach_verb(
  s: &mut ShardState,
  principal: &Principal,
  volume: slates_ipc::protocol::VolumeId,
  snapshot: Option<slates_ipc::protocol::SnapshotId>,
  (intent, subtree): (Intent, Option<String>),
) -> slates_ipc::protocol::ReplyBody {
  let refused = |refusal| slates_ipc::protocol::ReplyBody::Refused { refusal };
  let report = crate::transports::shards_fs(&crate::transports::situation(
    s,
    &slates_db::catalog::Rights::default(),
  ));
  if let Some(reason) = report.unsupported_reason {
    return refused(Refusal::AttachmentUnsupported {
      transport: slates_ipc::protocol::AttachTransport::ShardsFs,
      reason,
    });
  }
  if snapshot.is_some() {
    return refused(Refusal::Unsupported {
      feature: "a snapshot presented over a Shards stream".to_owned(),
    });
  }
  let Some((_, id)) = s.current_request else {
    return refused(Refusal::BadRequest {
      reason: "a Shards stream is attached by a recorded request".to_owned(),
    });
  };
  if pending_ends(s) >= handoff_bound(s) {
    return refused(Refusal::Overloaded { shard: s.shard });
  }
  let prepared = match prepare() {
    Ok(prepared) => prepared,
    Err(refusal) => return refused(refusal),
  };
  let attached = match attach(
    s,
    ShardsAsk {
      principal: principal.clone(),
      volume,
      subtree,
      intent,
    },
  ) {
    Ok(attached) => attached,
    Err(refusal) => return refused(refusal),
  };
  let rights = s
    .db
    .partition()
    .attachment(attached.attachment)
    .map(|record| record.rights)
    .unwrap_or_default();
  let mut capability = report;
  capability.read_write = if rights.write {
    slates_ipc::protocol::ReadWritePolicy::ReadWrite
  } else {
    slates_ipc::protocol::ReadWritePolicy::ReadOnly
  };
  hand_out(s, id.word(), attached, capability, prepared)
}

/// The socket ends this shard holds for clients: kept for a control socket, or offered on the socket route.
pub(crate) fn pending_ends(s: &ShardState) -> usize {
  #[cfg(target_os = "macos")]
  let offered = s.stream_offers.len();
  #[cfg(not(target_os = "macos"))]
  let offered = 0usize;
  s.stream_handoffs.len().saturating_add(offered)
}

/// What a hand-out needs made before the attach, so nothing it can refuse comes after one: on macOS the offer's secret,
/// drawn now (a refused random source refuses the attach before any effect).
#[cfg(target_os = "macos")]
type Prepared = [u8; slates_ipc::route::SECRET_BYTES];
/// See the macOS arm: Linux's hand-out needs nothing made first. A unit struct rather than `()`, so the one code path
/// binds and passes it on every platform without binding a unit value.
#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy, Debug)]
struct Prepared;

/// Draws the offer's secret (macOS).
#[cfg(target_os = "macos")]
fn prepare() -> Result<Prepared, Refusal> {
  slates_ipc::route::random::<{ slates_ipc::route::SECRET_BYTES }>().map_err(|_| {
    Refusal::BadRequest {
      reason: "secure random unavailable for the stream's secret".to_owned(),
    }
  })
}

/// See the macOS arm.
#[cfg(not(target_os = "macos"))]
fn prepare() -> Result<Prepared, Refusal> {
  Ok(Prepared)
}

/// Linux: the end is kept under the request's word for the client's control socket (`deliver_handoff`), and the reply
/// is the plain `Attached`.
#[cfg(target_os = "linux")]
fn hand_out(
  s: &mut ShardState,
  request: u64,
  attached: ShardsAttached,
  capability: slates_ipc::protocol::AttachmentCapability,
  _nothing: Prepared,
) -> slates_ipc::protocol::ReplyBody {
  let ShardsAttached {
    attachment,
    lease_epoch,
    stream,
    resync,
  } = attached;
  s.stream_handoffs.insert(
    request,
    Handoff {
      attachment,
      stream,
      resync,
    },
  );
  slates_ipc::protocol::ReplyBody::Attached {
    attachment,
    lease_epoch,
    path: None,
    version: None,
    established: slates_ipc::protocol::Established::Record,
    capability,
    // A stream presents no mount capability: the end is the capability.
    token: None,
  }
}

/// macOS: the end is offered on the socket route under `secret` (`crate::shards_route`), the device's end of its resync
/// channel beside it when one was made (A-339), and the reply carries the secret. An offer whose expiry could not be
/// armed is dropped at once, closing the ends, so the stream ends its attachment, and the attach is refused
/// `Overloaded`.
#[cfg(target_os = "macos")]
fn hand_out(
  s: &mut ShardState,
  _request: u64,
  attached: ShardsAttached,
  capability: slates_ipc::protocol::AttachmentCapability,
  secret: Prepared,
) -> slates_ipc::protocol::ReplyBody {
  let ShardsAttached {
    attachment,
    lease_epoch,
    stream,
    resync,
  } = attached;
  if !crate::shards_route::offer(
    s,
    (slates_ipc::route::OfferKind::Stream, attachment),
    secret,
    (stream, resync),
  ) {
    return slates_ipc::protocol::ReplyBody::Refused {
      refusal: Refusal::Overloaded { shard: s.shard },
    };
  }
  slates_ipc::protocol::ReplyBody::ShardsOffered {
    attachment,
    lease_epoch,
    capability,
    secret,
  }
}

/// Windows (A-179): the end is kept under the request's word, as on Linux, and duplicated into the client's process as
/// the reply goes to it (`deliver_handoff`), which writes the value into the reply; the device's end of its resync
/// channel beside it, when one was made (A-427). The reply recorded here names no handle (zero): a reply replayed from
/// the record after a restart must not name one, since the client's process outlives the daemon that duplicated it.
#[cfg(windows)]
fn hand_out(
  s: &mut ShardState,
  request: u64,
  attached: ShardsAttached,
  capability: slates_ipc::protocol::AttachmentCapability,
  _nothing: Prepared,
) -> slates_ipc::protocol::ReplyBody {
  let ShardsAttached {
    attachment,
    lease_epoch,
    stream,
    resync,
  } = attached;
  let owner = registry::current_shard().unwrap_or(0);
  s.stream_handoffs.insert(
    request,
    Handoff {
      attachment,
      owner,
      stream,
      resync,
    },
  );
  slates_ipc::protocol::ReplyBody::ShardsHanded {
    attachment,
    lease_epoch,
    capability,
    handle: 0,
    resync: 0,
  }
}

/// Any other Unix: no route to hand a client the end, which the transport report refuses before this is reached; the
/// end closes, so the stream ends its attachment.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn hand_out(
  _s: &mut ShardState,
  _request: u64,
  _attached: ShardsAttached,
  _capability: slates_ipc::protocol::AttachmentCapability,
  _nothing: Prepared,
) -> slates_ipc::protocol::ReplyBody {
  slates_ipc::protocol::ReplyBody::Refused {
    refusal: Refusal::AttachmentUnsupported {
      transport: slates_ipc::protocol::AttachTransport::ShardsFs,
      reason: slates_ipc::protocol::UnsupportedReason::HostPlatform,
    },
  }
}

/// Hands over the stream end owed for `request`, if the shard holds one, to the client at `client_index`, just before
/// `reply` is written into its ring: run by `verbs::send_reply` for every reply, so the end is taken out exactly once
/// whatever the reply's fate. Handed only with the attach that made it; any other reply for the request (the verb
/// rolled back and answered a refusal) drops it, counted. A client gone or retiring, or a hand-off the OS refused, drops
/// it too, counted: the end closes, so the stream sees its peer's close and ends its attachment. Linux sends it over the
/// client's control socket before the ring's reply, so the client finds it waiting; Windows duplicates it into the
/// client's process and writes the value into the reply ([`hand_to_client`]).
pub(crate) fn deliver_handoff(
  s: &mut ShardState,
  client_index: u32,
  request: u64,
  reply: &mut slates_ipc::protocol::ReplyBody,
) {
  let Some(handoff) = s.stream_handoffs.remove(&request) else {
    return;
  };
  if handed_attachment(reply) != Some(handoff.attachment) {
    s.count(HANDOFF_DROPPED, 1);
    return;
  }
  let client = slates_wire::request::RequestId::from_word(request).client;
  let slot = s
    .clients
    .generation_at(client_index)
    .and_then(|generation| {
      s.clients
        .get(slates_mem::Handle::from_raw(client_index, generation))
        .ok()
    })
    .filter(|slot| !slot.retiring && slot.client_id == client);
  #[cfg(windows)]
  let pid = slot.map_or(0, |slot| slot.pid);
  let handed = slot.is_some_and(|slot| hand_to_client(slot, request, &handoff, reply));
  if !handed {
    s.count(DELIVERY_REFUSED, 1);
  }
  #[cfg(windows)]
  {
    if handed
      && let slates_ipc::protocol::ReplyBody::ShardsHanded { handle, resync, .. } = reply
      && !tell_holder(
        &handoff,
        pid,
        *handle,
        *resync != 0 || handoff.resync.is_none(),
      )
    {
      s.count(HOLDER_UNTOLD, 1);
    }
  }
}

/// The attachment a reply hands a stream end for: the attach that made it, in the platform's form of its reply.
fn handed_attachment(reply: &slates_ipc::protocol::ReplyBody) -> Option<u64> {
  match reply {
    slates_ipc::protocol::ReplyBody::Attached { attachment, .. }
    | slates_ipc::protocol::ReplyBody::ShardsHanded { attachment, .. } => Some(*attachment),
    _ => None,
  }
}

/// Sends `handoff`'s end over `client`'s control socket (Unix; only Linux keeps ends for it, macOS offering them on its
/// socket route): whether it went.
#[cfg(unix)]
fn hand_to_client(
  client: &crate::state::ClientSlot,
  request: u64,
  handoff: &Handoff,
  _reply: &mut slates_ipc::protocol::ReplyBody,
) -> bool {
  #[cfg(target_os = "linux")]
  let resync = handoff.resync.as_ref();
  #[cfg(not(target_os = "linux"))]
  let resync: Option<&HandedEnd> = None;
  client.control.as_ref().is_some_and(|control| {
    control
      .send_stream(
        handoff.stream.as_fd(),
        request,
        handoff.attachment,
        resync.is_some(),
      )
      .is_ok()
      && resync.is_none_or(|resync| {
        control
          .send_resync(resync.as_fd(), request, handoff.attachment)
          .is_ok()
      })
  })
}

/// Duplicates `handoff`'s end into `client`'s process (Windows, A-175's route, A-179), once the process is judged to
/// run as the daemon's user, and writes the duplicate's value into `reply`: whether it went. The device's end of the
/// stream's resync channel goes beside it the same way (A-427); one the OS refuses is named zero, and the owner shard
/// gives that stream's resync up ([`tell_holder`]), since no device holds its end. The daemon's copies close when
/// `handoff` drops, so the client's are then the only ones.
#[cfg(windows)]
fn hand_to_client(
  client: &crate::state::ClientSlot,
  _request: u64,
  handoff: &Handoff,
  reply: &mut slates_ipc::protocol::ReplyBody,
) -> bool {
  // The reply's slots first, so a value is duplicated only where it will be written.
  let slates_ipc::protocol::ReplyBody::ShardsHanded { handle, resync, .. } = reply else {
    return false;
  };
  let Ok(value) = slates_ipc::handle_route::hand_stream(handoff.stream.as_handle(), client.pid)
  else {
    return false;
  };
  *handle = value;
  *resync = handoff
    .resync
    .as_ref()
    .and_then(|end| slates_ipc::handle_route::hand_stream(end.as_handle(), client.pid).ok())
    .unwrap_or(0);
  true
}

/// Tells the stream's owner shard that `pid` holds the stream as the handle `value` (Windows, plan G-shards-windows
/// step 5): the known holder a VMM's DAX claim is judged against (`dax::record_holder`), and the process a successor
/// hands a fresh stream (A-427, `resync_handle::name_holder`) until its client names another. Where its device's
/// resync end was not handed (`resync_handed` false), the stream's resync is given up, counted. Sent as a task to that
/// shard, where the stream lives, before the attach's reply is written, so it is queued there before the client can
/// name the value to anyone. Whether the runtime took it: if not, the stream has no known holder, and its DAX asks are
/// refused.
#[cfg(windows)]
fn tell_holder(handoff: &Handoff, pid: u32, value: u64, resync_handed: bool) -> bool {
  let attachment = handoff.attachment;
  let task = SpawnRequest::new(
    Box::pin(async move {
      state::with_state(|s| {
        dax::record_holder(s, attachment, pid, value);
        resync_handle::name_holder(s, attachment, pid);
        if !resync_handed {
          give_up_held_resync(s, attachment);
        }
      });
    }),
    None,
  );
  registry::send_control(handoff.owner, Control::Spawn(Box::new(task))).is_ok()
}

/// Gives up the resync of the stream of `attachment` this shard serves, counted (Windows, A-427): its device's end was
/// not handed to its client, so no device would ever read a successor's `Resync`.
#[cfg(windows)]
fn give_up_held_resync(s: &mut ShardState, attachment: u64) {
  let Some(mut stream) = s.shards_streams.remove(&attachment) else {
    return;
  };
  if resync_handle::give_up(s, &mut stream) {
    s.count(RESYNC_GIVEN_UP, 1);
  }
  s.shards_streams.insert(attachment, stream);
}

/// `ShardsHolder` (§4.6, A-181; Windows): adds `pid` as a known holder of the stream of `attachment`, its handle there
/// `handle`, once the claim is judged against a holder this shard already knows (`slates_ipc::handle_route::claim`):
/// `slates shards` names the command it started while its own copy still holds the stream, so the command stays a
/// known holder after that copy closes, and a VMM's DAX claim can be judged against it (A-180). A true claim adds no
/// authority, since the named process holds the stream already; a false one is refused `Forbidden` and counted.
/// `HolderPending` while the attach's own holder record has not arrived; `NotFound` for no stream of `attachment`
/// serving `volume` here.
#[cfg(windows)]
pub(crate) fn add_holder(
  s: &mut ShardState,
  volume: VolumeId,
  attachment: u64,
  (pid, handle): (u32, u64),
) -> slates_ipc::protocol::ReplyBody {
  let Some(stream) = s
    .shards_streams
    .get(&attachment)
    .filter(|stream| stream.volume == volume && !stream.ending)
  else {
    return crate::verbs::refused(Refusal::NotFound);
  };
  // The attach records its client as the first holder by a task it queues here as it replies, which a request sent at
  // once can overtake (measured on jupiter, 2026-10-26: 3 of 3 such requests met no holder). Judged against nothing,
  // every claim would fail, so the caller is told to send again; nothing is recorded meanwhile.
  if stream.holders.is_empty() {
    s.count(HOLDER_PENDING, 1);
    return crate::verbs::refused(Refusal::HolderPending);
  }
  if slates_ipc::handle_route::claim(pid, handle, &stream.holders).is_err() {
    s.count(HOLDER_REFUSED, 1);
    return crate::verbs::refused(Refusal::Forbidden {
      verb: "shards_holder".to_owned(),
    });
  }
  dax::record_holder(s, attachment, pid, handle);
  // The process the client names holds the stream for its VM: a successor hands it the fresh stream (A-427).
  resync_handle::name_holder(s, attachment, pid);
  s.count(HOLDER_ADDED, 1);
  slates_ipc::protocol::ReplyBody::ShardsHolderAdded
}

/// `ShardsHolder` on Unix (§4.6, A-181): refused by kind. A Unix stream's end is a descriptor the daemon never looks
/// for again, and DAX answers ride the stream itself.
#[cfg(unix)]
pub(crate) fn add_holder(
  _s: &mut ShardState,
  _volume: VolumeId,
  _attachment: u64,
  _holder: (u32, u64),
) -> slates_ipc::protocol::ReplyBody {
  crate::verbs::refused(Refusal::Unsupported {
    feature: "naming a Shards stream's holder (Windows only: a Unix end is a descriptor)"
      .to_owned(),
  })
}

/// Ends every stream serving `volume` (its destroy): each marked ending and ended both ways, so its task sweeps its
/// guest's references through the volume, still whole, and removes its record. The streams still serving, which the
/// destroy's teardown waits for, as it waits for the volume's guest devices.
pub(crate) fn end_volume_streams(s: &mut ShardState, volume: VolumeId) -> usize {
  let serving: Vec<u64> = s
    .shards_streams
    .iter()
    .filter(|(_, stream)| stream.volume == volume)
    .map(|(attachment, _)| *attachment)
    .collect();
  for attachment in &serving {
    end_stream(s, *attachment);
  }
  serving.len()
}

/// After a rolled-back verb (`verbs::reconcile_unpublished_effects`): every stream whose attachment record the
/// rollback removed is ended as any other end ends it ([`end_stream`]), and the end owed for it dropped, so a verb
/// that failed to commit leaves nothing serving and hands out nothing. Its task wakes on the stream's end and releases
/// the rest ([`ended`], which finds no record to remove).
pub(crate) fn reconcile_unrecorded(s: &mut ShardState) {
  let unrecorded: Vec<u64> = s
    .shards_streams
    .keys()
    .copied()
    .filter(|attachment| s.db.partition().attachment(*attachment).is_none())
    .collect();
  for attachment in unrecorded {
    end_stream(s, attachment);
    s.stream_handoffs
      .retain(|_, handoff| handoff.attachment != attachment);
  }
}

/// What one turn did.
enum Turned {
  /// A frame was answered (its reply out, or owed and waiting for the stream's writability).
  Served,
  /// No whole frame waits: the task waits for the stream's readability.
  Waiting,
  /// A reply is owed and the stream takes no more now: the task waits for its writability.
  Blocked,
  /// The owner may not serve the volume's latest state now (AUD-29-83): nothing was read.
  Fenced,
  /// The owed reply waits for every FUSE mount of the volume to have been told of its request's change (AC-3.3, A-196):
  /// the task awaits that, bounded, then sends it.
  Told(crate::told::Owed),
  /// The owed reply is an `FSYNC` that waits for its live-shipped volume's log to be durable at this position (A-200):
  /// the task awaits that within the liveness budget, then sends it, or `EIO` in its place.
  Durable(u64),
  /// The anchor must be told the stream's resync session before the owed reply goes out (Windows, A-427): the task tells
  /// it (`resync_handle::tell`), then sends the reply.
  #[cfg(windows)]
  Session,
  /// The attachment ended.
  Ended,
}

/// What woke a stream's serve task.
enum Woke {
  /// The stream's readiness, or the runtime's refusal of the wait.
  Stream(Result<(), slates_rt::RtError>),
  /// The resync channel, watched while this daemon keeps its copy of a fresh stream's end (A-339).
  #[cfg(unix)]
  Channel,
  /// A change of the resync session the anchor must be told, made outside the task: a holder named (Windows, A-427).
  #[cfg(windows)]
  Session,
}

/// Waits for `stream`'s readiness (its writability while `writing`), raced, while a resynced stream's daemon keeps its
/// copy of the fresh end, against the readability of the resync channel's end `watched`, which wakes only when the
/// device's end of the channel closes (the channel is one way, slates to the device). The stream is preferred when both
/// are ready, as its bytes say the device holds its end.
#[cfg(unix)]
async fn wait_for_work(_attachment: u64, stream: i32, writing: bool, watched: Option<i32>) -> Woke {
  let mut ready = std::pin::pin!(async move {
    if writing {
      slates_rt::readiness::writable(stream).await
    } else {
      slates_rt::readiness::readable(stream).await
    }
  });
  let Some(watched) = watched else {
    return Woke::Stream(ready.await);
  };
  let mut channel = std::pin::pin!(slates_rt::readiness::readable(watched));
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(ready) = ready.as_mut().poll(cx) {
      return std::task::Poll::Ready(Woke::Stream(ready));
    }
    if channel.as_mut().poll(cx).is_ready() {
      return std::task::Poll::Ready(Woke::Channel);
    }
    std::task::Poll::Pending
  })
  .await
}

/// Waits for `stream`'s readiness (its writability while `writing`), raced against a change of `attachment`'s resync
/// session the anchor must be told (Windows, A-427): a holder named outside this task wakes it through the waker kept
/// in the stream ([`resync_handle::owed_or_wake_me`]). The stream is preferred when both are ready.
#[cfg(windows)]
async fn wait_for_work(attachment: u64, stream: i32, writing: bool, _watched: Option<i32>) -> Woke {
  let mut ready = std::pin::pin!(async move {
    if writing {
      slates_rt::readiness::writable(stream).await
    } else {
      slates_rt::readiness::readable(stream).await
    }
  });
  std::future::poll_fn(|cx| {
    if let std::task::Poll::Ready(ready) = ready.as_mut().poll(cx) {
      return std::task::Poll::Ready(Woke::Stream(ready));
    }
    if resync_handle::owed_or_wake_me(attachment, cx.waker()) {
      return std::task::Poll::Ready(Woke::Session);
    }
    std::task::Poll::Pending
  })
  .await
}

/// The resync channel's end to watch beside `stream`: its own, while this daemon keeps its copy of a fresh stream's end
/// (A-339).
#[cfg(unix)]
fn watched(stream: &ShardsStream) -> Option<i32> {
  use std::os::fd::AsRawFd;
  stream
    .fresh_copy
    .as_ref()
    .and(stream.resync.as_ref())
    .and_then(|resync| resync.end.as_ref())
    .map(AsRawFd::as_raw_fd)
}

/// See the Unix arm: Windows resyncs no stream yet.
#[cfg(windows)]
fn watched(_stream: &ShardsStream) -> Option<i32> {
  None
}

/// Lets go of `attachment`'s copy of its fresh stream's end once its resync channel woke (A-339): the device's end of
/// the channel closed, so it went before it took the stream, which then sees its peer gone and ends, counted.
#[cfg(unix)]
fn abandoned(s: &mut ShardState, attachment: u64) {
  let let_go = s
    .shards_streams
    .get_mut(&attachment)
    .is_some_and(|stream| stream.fresh_copy.take().is_some());
  if let_go {
    s.count(RESYNC_ABANDONED, 1);
  }
}

/// The serve task of one stream: wait for the stream's readiness (its writability while a reply is owed), take frames
/// until none waits, yielding between them; end the attachment when the stream ends. While a resynced stream's daemon
/// keeps its copy of the fresh end, the resync channel is watched beside it ([`wait_for_work`]).
async fn serve(attachment: u64) {
  // A Windows stream's resync end is held by the anchor before anything is read on the stream (A-427).
  #[cfg(windows)]
  resync_handle::hold(attachment).await;
  loop {
    let Some(Some((stream, writing, watched, busy))) = state::with_state(|s| {
      s.shards_streams.get(&attachment).map(|stream| {
        (
          end::raw(&stream.stream),
          stream.sent < stream.owed,
          watched(stream),
          // An ingest under way turns at once: a resynced stream's adopted objects are ingested before anything is read
          // (A-365), whether or not its device has sent anything yet.
          stream.dax.ingesting(),
        )
      })
    }) else {
      return;
    };
    let ready = if busy {
      Ok(())
    } else {
      match wait_for_work(attachment, stream, writing, watched).await {
        Woke::Stream(ready) => ready,
        #[cfg(unix)]
        Woke::Channel => {
          let _ = state::with_state_counted(|s| abandoned(s, attachment));
          continue;
        }
        #[cfg(windows)]
        Woke::Session => {
          resync_handle::tell(attachment).await;
          continue;
        }
      }
    };
    if ready.is_err() {
      let _ = state::with_state_counted(|s| {
        s.count(STREAM_FAILED, 1);
        ended(s, attachment);
      });
      return;
    }
    loop {
      match state::with_state(|s| turn(s, attachment)) {
        Some(Turned::Served) => {
          // A served request is client activity, as a FUSE mount's is: the shard spins out its idle window after it,
          // so a burst's next request is read without a wake (§4.3, §4.7).
          slates_rt::registry::with_current(|ctx| ctx.note_activity());
          let _ = futures::yield_now().await;
        }
        Some(Turned::Waiting | Turned::Blocked) => break,
        Some(Turned::Fenced) => {
          // The requests wait in the stream, asked again each heartbeat: the cadence at which the confirmations that
          // restore the lease arrive, as a FUSE mount and a guest device wait.
          crate::daemon::pace(crate::daemon::HEARTBEAT_NS).await;
          break;
        }
        Some(Turned::Durable(target)) => {
          // The guest's `fsync` is answered once `f` candidates hold what its barrier published, or `EIO` past the
          // liveness budget (A-200): never a durability no candidate has. Shards' VMM sends nothing more meanwhile.
          let Some(Some(volume)) = state::with_state(|s| {
            s.shards_streams
              .get(&attachment)
              .map(|stream| stream.volume)
          }) else {
            return;
          };
          let durable = crate::live_ship::wait_durable(volume, target).await;
          let _ = state::with_state_counted(|s| settle_durable(s, attachment, durable));
        }
        #[cfg(windows)]
        Some(Turned::Session) => resync_handle::tell(attachment).await,
        Some(Turned::Told(owed)) => {
          // Every FUSE mount of the volume is told of the change, or the bound passes, before the reply goes out
          // (AC-3.3); the next turn sends it. Shards' VMM sends nothing more meanwhile, since it waits for this reply.
          let _waited = crate::told::wait(owed).await;
          let _ = state::with_state_counted(|s| {
            if let Some(stream) = s.shards_streams.get_mut(&attachment) {
              stream.told = None;
            }
          });
        }
        Some(Turned::Ended) | None => {
          let _ = state::with_state_counted(|s| ended(s, attachment));
          return;
        }
      }
    }
  }
}

/// One turn of stream `attachment`, taken out of the shard's map while it runs and always put back: a stream that
/// ended is torn down by [`ended`], which the serve task calls next, and nowhere else.
fn turn(s: &mut ShardState, attachment: u64) -> Turned {
  let Some(mut stream) = s.shards_streams.remove(&attachment) else {
    return Turned::Ended;
  };
  let turned = turn_with(s, &mut stream);
  s.shards_streams.insert(attachment, stream);
  turned
}

/// One turn of `stream`: the owed reply out first; then, while the owner may serve the volume, one frame taken and
/// answered, its reply out as far as the stream takes it.
fn turn_with(s: &mut ShardState, stream: &mut ShardsStream) -> Turned {
  // An ending stream first writes back its guest's stores through its maps (step 3), once, then ends.
  if stream.ending && !stream.dax.ingesting() && !begin_end_ingest(s, stream) {
    return Turned::Ended;
  }
  // A map the disk changed beneath is refilled before anything more reaches its guest (step 4): an owed reply that may
  // show the change waits for it.
  let refill_owed = !stream.ending && dax::refill_owed(s, stream);
  if !refill_owed {
    if let Some(target) = durable_owed(stream) {
      return Turned::Durable(target);
    }
    if let Some(owed) = told_owed(s, stream) {
      return Turned::Told(owed);
    }
    #[cfg(windows)]
    if resync_handle::owed(stream) {
      return Turned::Session;
    }
    let owed = stream.owed > 0;
    match flush(stream) {
      Flushed::Done => sent_whole(s, stream, owed),
      Flushed::Blocked => return Turned::Blocked,
      Flushed::Failed => {
        s.count(STREAM_FAILED, 1);
        return Turned::Ended;
      }
    }
  }
  // The volume is gone (destroyed): the attachment ends with it.
  if !s.by_id.contains_key(&stream.volume) {
    return Turned::Ended;
  }
  // The live-tree fence every transport applies before serving (§4.8 "Leases and reads"; AUD-29-83).
  if crate::verbs::live_tree_fenced(s, stream.volume) {
    return Turned::Fenced;
  }
  if refill_owed {
    dax::refill_step(s, stream);
    return Turned::Served;
  }
  // An ingest runs a slice a turn; when it is done, the request it held is served, or the stream ends.
  if stream.dax.ingesting() {
    return ingest_turn(s, stream);
  }
  // A descriptor ask's fill runs a slice a turn, its answer owed when the last is done; nothing more is read meanwhile,
  // and the device sends nothing more, since it waits for that answer with its stream held.
  if stream.dax.filling() {
    dax::step(s, stream);
    return flush_unless_refill_owed(s, stream);
  }
  let took = take_frame(stream);
  // Bytes on a resynced stream are the device's own end at work: this daemon's copy of it may go (A-339).
  #[cfg(unix)]
  if stream.filled > 0 && stream.fresh_copy.take().is_some() {
    s.count(RESYNC_TAKEN, 1);
  }
  let (control, total) = match took {
    Took::Frame { control, total } => (control, total),
    Took::Waiting => return Turned::Waiting,
    Took::Closed => {
      return if begin_end_ingest(s, stream) {
        Turned::Served
      } else {
        Turned::Ended
      };
    }
    Took::Truncated => {
      s.count(FRAME_TRUNCATED, 1);
      return if begin_end_ingest(s, stream) {
        Turned::Served
      } else {
        Turned::Ended
      };
    }
    Took::Refused => {
      s.count(FRAME_REFUSED, 1);
      return Turned::Ended;
    }
    Took::Failed => {
      s.count(STREAM_FAILED, 1);
      return Turned::Ended;
    }
  };
  answer_frame(s, stream, control, total)
}

/// Answers the frame of `total` bytes at the head of `stream`: a control ask (`control`: a SAVE or a RESTORE, A-303; a
/// descriptor ask; any other refused) or, with none, a guest's request. Then the frame is consumed, and unless serving
/// it ended the stream, its reply goes out as far as the stream takes it.
fn answer_frame(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  control: Option<u8>,
  total: usize,
) -> Turned {
  // Only a fresh stream's first frame may be a RESTORE (A-303); every frame after reads within the request's bound.
  let first = std::mem::replace(&mut stream.restorable, false);
  let served = match control {
    // Shards' device sends a SAVE with nothing after its ask byte; one carrying more is no SAVE of its.
    Some(ASK_SAVE) if total != LENGTH_BYTES.saturating_add(1) => {
      s.count(SAVE_REFUSED, 1);
      owe(stream, &control_refusal(SAVE_MALFORMED));
      true
    }
    Some(ASK_SAVE) => {
      // The guest's stores through its maps reach the volume before the cut: the SAVE waits for them, unconsumed.
      if dax::hold_save_for_ingest(s, stream, total) {
        return Turned::Served;
      }
      answer_save(s, stream, false);
      true
    }
    Some(ASK_RESTORE) => {
      answer_restore(s, stream, first, total);
      true
    }
    Some(ASK_DESCRIPTOR) => {
      // The ask's body follows its length word and its ask byte.
      let body_at = LENGTH_BYTES.saturating_add(1);
      let body = stream.inbound.get(body_at..total).unwrap_or(&[]).to_vec();
      dax::begin(s, stream, &body);
      true
    }
    Some(ask) => {
      answer_control(stream, ask);
      s.count(CONTROL_REFUSED, 1);
      true
    }
    None => {
      // A request that asks for the guest's stores to reach the volume first is held, unconsumed, for its ingest.
      let request = stream
        .inbound
        .get(LENGTH_BYTES..total)
        .unwrap_or(&[])
        .to_vec();
      // The request a dead daemon published and never answered, resent whole as a resynced stream's first (A-316).
      if let Some(reply) = replayed(stream, &request) {
        s.count(REPLAYED, 1);
        owe(stream, &reply);
        consume(stream, total);
        return flush_unless_refill_owed(s, stream);
      }
      // Shards' owner word is the stream's to take, never a file's attribute (A-281).
      if let Some(word) = owner_word(&request) {
        let served = take_owner_word(s, stream, word);
        consume(stream, total);
        if !served {
          return Turned::Ended;
        }
        return flush_unless_refill_owed(s, stream);
      }
      if dax::hold_for_ingest(s, stream, &request, total) {
        stream.told_mark = crate::told::mark(s, stream.volume);
        return Turned::Served;
      }
      serve_request(s, stream, total, Ingested::default())
    }
  };
  consume(stream, total);
  // A fresh stream's first frame may have grown the buffer to a restore's tables: the room goes back (A-303).
  if first && stream.inbound.len() > FRAME_BYTES && stream.filled <= FRAME_BYTES {
    stream.inbound.truncate(FRAME_BYTES);
    stream.inbound.shrink_to(FRAME_BYTES);
  }
  if !served {
    return Turned::Ended;
  }
  flush_unless_refill_owed(s, stream)
}

/// Sends what `stream` owes its guest, unless serving it marked one of the stream's backings stale: the reply may show
/// the disk's change (a size, a time), so it waits for the refill the next turn begins with (step 4).
fn flush_unless_refill_owed(s: &mut ShardState, stream: &mut ShardsStream) -> Turned {
  if !stream.ending && dax::refill_owed(s, stream) {
    return Turned::Served;
  }
  if let Some(target) = durable_owed(stream) {
    return Turned::Durable(target);
  }
  if let Some(owed) = told_owed(s, stream) {
    return Turned::Told(owed);
  }
  // A reply that rests on a change of the resync session waits until the anchor has it (Windows, A-427).
  #[cfg(windows)]
  if resync_handle::owed(stream) {
    return Turned::Session;
  }
  let owed = stream.owed > 0;
  match flush(stream) {
    Flushed::Done => {
      sent_whole(s, stream, owed);
      Turned::Served
    }
    Flushed::Blocked => Turned::Served,
    Flushed::Failed => {
      s.count(STREAM_FAILED, 1);
      Turned::Ended
    }
  }
}

/// What a frame `stream` owed (`owed`) lets go of once it is written whole: the barrier reply kept for its request's
/// resend (A-316), which the device now has; a reply on a stream's socket is the device's to read even if the daemon
/// dies the next instant.
fn sent_whole(s: &mut ShardState, stream: &ShardsStream, owed: bool) {
  if owed {
    s.pending_replies.remove(&stream.attachment);
  }
}

/// The log position the owed `FSYNC` reply of `stream` waits to be durable at (A-200); an ending stream, whose peer
/// reads no reply, waits for nothing.
fn durable_owed(stream: &mut ShardsStream) -> Option<u64> {
  if stream.ending {
    stream.durable = None;
  }
  stream.durable.map(|owed| owed.target)
}

/// Settles the owed `FSYNC` of stream `attachment` once its wait for durability is over: sent as it is when the log
/// became durable at its position, replaced by `EIO` when it did not (counted).
fn settle_durable(s: &mut ShardState, attachment: u64, durable: bool) {
  let Some(stream) = s.shards_streams.get_mut(&attachment) else {
    return;
  };
  let Some(owed) = stream.durable.take() else {
    return;
  };
  if !durable {
    stream.owed = refuse(owed.unique, EIO, &mut stream.outbound);
    stream.sent = 0;
    s.count(LIVE_FSYNC_REFUSED, 1);
  }
}

/// What the owed reply of `stream` still waits for: every FUSE mount of its volume told through its request's newest
/// record (AC-3.3, A-196). Cleared once they are, so the reply is sent; an ending stream waits for nothing.
fn told_owed(s: &ShardState, stream: &mut ShardsStream) -> Option<crate::told::Owed> {
  let owed = stream.told?;
  if stream.ending || crate::told::told(s, owed) {
    stream.told = None;
    return None;
  }
  Some(owed)
}

/// Starts the ingest a stream takes before it ends, once: whether one is under way now.
fn begin_end_ingest(s: &ShardState, stream: &mut ShardsStream) -> bool {
  if stream.ingested_at_end {
    return false;
  }
  stream.ingested_at_end = true;
  dax::ingest_before_end(s, stream)
}

/// One turn of an ingest under way. When it is done, the request it held is served (published first where the ingest
/// wrote) and consumed, or the stream ends (its writes published first).
fn ingest_turn(s: &mut ShardState, stream: &mut ShardsStream) -> Turned {
  let Some(done) = dax::ingest_step(s, stream) else {
    return Turned::Served;
  };
  match done.then {
    // A SAVE held for the guest's stores is answered with the tables once they are in (A-303).
    Then::Serve { total } if held_save(stream, total) => {
      answer_save(s, stream, done.refused.is_some());
      consume(stream, total);
      flush_unless_refill_owed(s, stream)
    }
    Then::Serve { total } => {
      let served = serve_request(
        s,
        stream,
        total,
        Ingested {
          wrote: done.wrote,
          refused: done.refused,
        },
      );
      consume(stream, total);
      if !served {
        return Turned::Ended;
      }
      flush_unless_refill_owed(s, stream)
    }
    Then::End => {
      if done.wrote
        && !crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(stream.volume))
      {
        s.count(END_UNPUBLISHED, 1);
      }
      Turned::Ended
    }
    // A resynced stream's adopted objects are in the file (A-365): published, so they outlive a second death, before
    // the stream reads anything.
    Then::Resume => {
      if done.wrote
        && !crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(stream.volume))
      {
        s.count(ADOPTED_UNPUBLISHED, 1);
      }
      Turned::Served
    }
  }
}

/// What the head of a stream's inbound bytes held.
enum Took {
  /// A whole frame of `total` bytes at the head: a control ask (its ask byte) or a request (`None`).
  Frame {
    /// The ask of a control frame; `None` for a guest's request.
    control: Option<u8>,
    /// The frame's bytes, its length word included.
    total: usize,
  },
  /// No whole frame, and nothing more to read now.
  Waiting,
  /// The peer closed its end, or this end was ended, between frames.
  Closed,
  /// The peer closed its end part-way through a frame.
  Truncated,
  /// A frame the protocol refuses.
  Refused,
  /// The stream failed.
  Failed,
}

/// Takes the first whole frame from what `stream` has read, reading more as it needs: each read fills the buffer
/// towards the frame's own length (bounded by the largest frame before the buffer grows for it), so the loop ends with
/// a whole frame, nothing more to read, the peer's close, or a refusal.
fn take_frame(stream: &mut ShardsStream) -> Took {
  // A fresh stream's first frame may be a RESTORE, whose tables can run past any request (A-303).
  let control_most = if stream.restorable {
    stream.restore_most.saturating_add(1).max(FRAME_BYTES)
  } else {
    FRAME_BYTES
  };
  loop {
    let head = stream.inbound.get(..stream.filled).unwrap_or(&[]);
    let need = match split_frame_bounded(head, FRAME_BYTES, control_most) {
      Ok(Split::Whole(Frame::Request(_), total)) => {
        return Took::Frame {
          control: None,
          total,
        };
      }
      Ok(Split::Whole(Frame::Control { ask, .. }, total)) => {
        return Took::Frame {
          control: Some(ask),
          total,
        };
      }
      Ok(Split::Partial(need)) => need,
      Err(_) => return Took::Refused,
    };
    if stream.inbound.len() < need {
      stream.inbound.resize(need, 0);
    }
    let Some(room) = stream.inbound.get_mut(stream.filled..) else {
      return Took::Failed;
    };
    match end::receive(&stream.stream, room) {
      Received::Closed if stream.filled > 0 => return Took::Truncated,
      Received::Closed => return Took::Closed,
      Received::Bytes(read) => stream.filled = stream.filled.saturating_add(read),
      Received::Waiting => return Took::Waiting,
      Received::Failed => return Took::Failed,
    }
  }
}

/// Drops the frame of `total` bytes at the head of `stream`'s inbound bytes, keeping what follows it (nothing, from a
/// peer that waits for each reply before its next request, so nothing is moved).
fn consume(stream: &mut ShardsStream, total: usize) {
  let filled = stream.filled.min(stream.inbound.len());
  let total = total.min(filled);
  if total < filled
    && let Some(window) = stream.inbound.get_mut(..filled)
  {
    window.copy_within(total.., 0);
  }
  stream.filled = filled.saturating_sub(total);
}

/// Answers a control ask slates does not know with its refusal, owed as the stream's reply (see the module doc).
fn answer_control(stream: &mut ShardsStream, _ask: u8) {
  owe(stream, &control_refusal(UNKNOWN_ASK));
}

/// Whether the frame of `total` bytes held at the head of `stream` is a SAVE (A-303).
fn held_save(stream: &ShardsStream, total: usize) -> bool {
  let head = stream.inbound.get(..total).unwrap_or(&[]);
  matches!(
    split_frame_bounded(head, FRAME_BYTES, FRAME_BYTES),
    Ok(Split::Whole(Frame::Control { ask: ASK_SAVE, .. }, _))
  )
}

/// Answers a SAVE (A-303; Shards' D176 part 5) with the stream's tables, owed as its reply, or a refusal with its
/// reason. The guest's stores through its maps reached the volume first, unless `stores_refused`: then the cut would
/// lack them, and the SAVE is refused.
fn answer_save(s: &mut ShardState, stream: &mut ShardsStream, stores_refused: bool) {
  let saved = if stores_refused {
    Err(SAVE_STORES_REFUSED)
  } else {
    save(s, stream).and_then(|tables| control_answer(&tables).ok_or(SAVE_TOO_LONG))
  };
  match saved {
    Ok(answer) => {
      s.count(SAVED, 1);
      owe(stream, &answer);
    }
    Err(reason) => {
      s.count(SAVE_REFUSED, 1);
      owe(stream, &control_refusal(reason));
    }
  }
}

/// The tables of `stream` at a cut made now: a snapshot of its volume taken for the attachment's own principal and made
/// durable, then what the attachment holds of the volume, its scope, its owner view and its guest's `SETXATTR` form
/// ([`tables::StreamTables`]).
fn save(s: &mut ShardState, stream: &ShardsStream) -> Result<Vec<u8>, &'static str> {
  let principal = s
    .db
    .partition()
    .attachment(stream.attachment)
    .map(|record| record.principal.clone())
    .ok_or(STREAM_GONE)?;
  let volume = slates_ipc::protocol::VolumeId {
    bytes: stream.volume.bytes,
  };
  let (snapshot, _) =
    crate::verbs::take_snapshot(s, &principal, volume).map_err(|refused| match *refused {
      ReplyBody::Refused {
        refusal: Refusal::Forbidden { .. },
      } => SAVE_FORBIDDEN,
      ReplyBody::Refused {
        refusal: Refusal::BarrierIncomplete { .. },
      } => SAVE_IN_FLIGHT,
      _ => SAVE_UNSNAPSHOTTED,
    })?;
  if !crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(stream.volume)) {
    return Err(SAVE_UNDURABLE);
  }
  let &handle = s.by_id.get(&stream.volume).ok_or(STREAM_GONE)?;
  let held = s
    .volumes
    .get(handle)
    .map(|slot| slot.volume.held_by(stream.attachment))
    .map_err(|_| STREAM_GONE)?;
  let owners = stream
    .owners
    .swapped()
    .map(
      |((kept_uid, kept_gid), (run_uid, run_gid))| tables::SwappedOwners {
        kept_uid,
        kept_gid,
        run_uid,
        run_gid,
      },
    );
  Ok(tables::encode(&tables::StreamTables {
    volume: stream.volume,
    snapshot: crate::verbs::to_db_snapshot(snapshot),
    scope: stream.scope,
    owners,
    setxattr_ext: stream.setxattr_ext,
    held,
  }))
}

/// Answers a RESTORE (A-303), owed as the stream's reply: the stream's attachment takes what the saved stream held and
/// its guest's owner view, or a refusal with its reason, which changed nothing. `first` is whether it is the stream's
/// first frame; the tables follow its ask byte in the frame of `total` bytes at the head of the stream.
fn answer_restore(s: &mut ShardState, stream: &mut ShardsStream, first: bool, total: usize) {
  let restored = if first {
    // The tables are read where they lie; the buffer is lent out of the stream meanwhile, not copied.
    let inbound = std::mem::take(&mut stream.inbound);
    let body_at = LENGTH_BYTES.saturating_add(1);
    let restored = restore(s, stream, inbound.get(body_at..total).unwrap_or(&[]));
    stream.inbound = inbound;
    restored
  } else {
    Err(RESTORE_NOT_FIRST)
  };
  match restored {
    Ok(()) => {
      s.count(RESTORED, 1);
      owe(stream, &control_answer(&[]).unwrap_or_default());
    }
    Err(reason) => {
      s.count(RESTORE_REFUSED, 1);
      owe(stream, &control_refusal(reason));
    }
  }
}

/// Gives `stream`'s attachment what the tables `bytes` say the saved stream held, after every check: the tables
/// decode; the stream presents the directory the saved one did; and it serves the volume saved, unchanged since the
/// cut, or a clone of the cut's snapshot, whose numbers are its origin's. The volume then checks each reference
/// itself, all before any is given ([`slates_vfs::volume::Volume::adopt_held`]).
fn restore(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  bytes: &[u8],
) -> Result<(), &'static str> {
  let tables = tables::decode(bytes, stream.restore_most).map_err(tables::TablesRefused::reason)?;
  if tables.scope != stream.scope {
    return Err(RESTORE_ANOTHER_DIRECTORY);
  }
  let &handle = s.by_id.get(&stream.volume).ok_or(STREAM_GONE)?;
  let from_the_cut = if tables.volume == stream.volume {
    let snapshot = crate::verbs::core_snapshot(slates_ipc::protocol::SnapshotId {
      value: tables.snapshot.value,
    });
    s.volumes
      .get(handle)
      .ok()
      .and_then(|slot| slot.volume.unchanged_since(snapshot).ok())
      .unwrap_or(false)
  } else {
    s.db.partition().lineage(stream.volume).is_some_and(|edge| {
      edge.origin_volume == tables.volume && edge.origin_snapshot == tables.snapshot
    })
  };
  if !from_the_cut {
    return Err(RESTORE_ELSEWHERE);
  }
  {
    let ShardState { store, volumes, .. } = &mut *s;
    let slot = volumes.get_mut(handle).map_err(|_| STREAM_GONE)?;
    slot
      .volume
      .adopt_held(store, stream.attachment, &tables.held)
      .map_err(|e| match e {
        slates_vfs::VfsError::NotFound | slates_vfs::VfsError::StaleHandle => RESTORE_FILE_GONE,
        _ => RESTORE_INVALID,
      })?;
  }
  stream.owners = tables.owners.map_or(OwnerView::IDENTITY, |owners| {
    OwnerView::swapping(
      (owners.kept_uid, owners.kept_gid),
      (owners.run_uid, owners.run_gid),
    )
  });
  stream.setxattr_ext = tables.setxattr_ext;
  // What a successor must keep is the restored guest's: its owner view and its kernel's form (A-378; before, a restored
  // stream's held session kept the fresh stream's identity view).
  #[cfg(unix)]
  tell_session(s, stream);
  #[cfg(windows)]
  resync_handle::changed(stream);
  Ok(())
}

/// Records the form of `SETXATTR` `stream`'s guest's kernel sends from now on, where `answered` is its `INIT` answered:
/// as the reply it reads kept it (A-378), and tells a held resync session so a successor parses as this daemon does.
fn kept_at_init(s: &mut ShardState, stream: &mut ShardsStream, answered: &Answered) {
  if answered.opcode != Some(Opcode::Init) || answered.error != 0 {
    return;
  }
  stream.setxattr_ext = answered
    .reply(&stream.outbound)
    .get(slates_bridge_fuse::abi::OUT_HEADER_LEN..)
    .and_then(slates_bridge_fuse::init::kept_flags)
    .is_some_and(|kept| kept & slates_bridge_fuse::abi::flags::SETXATTR_EXT != 0);
  #[cfg(unix)]
  tell_session(s, stream);
  #[cfg(windows)]
  {
    resync_handle::changed(stream);
    let _ = s;
  }
}

/// Tells the anchor what `stream`'s successor must keep (its owner view and its kernel's form; A-281, A-378), where
/// the anchor holds its resync end: a refused update gives the resync up, counted.
#[cfg(unix)]
fn tell_session(s: &mut ShardState, stream: &mut ShardsStream) {
  let kept = resync::Kept {
    owners: stream.owners,
    setxattr_ext: stream.setxattr_ext,
  };
  if let Some(resync) = stream.resync.as_mut()
    && resync.held()
    && !resync::tell_session(stream.attachment, resync, kept)
  {
    s.count(RESYNC_GIVEN_UP, 1);
  }
}

/// Takes a guest's owner word (A-281): a word Shards' rules refuse is answered with its errno and changes nothing; one
/// named by the guest's root on the stream's root makes the stream show the root's owner as it is now, uid and gid, as
/// the run's owner the word names, and the run's as the root's, until the stream ends. The word is kept on no file:
/// the reply is owed with nothing written to the volume, so no barrier waits on it. `false` when the attachment can no
/// longer be served (its registry entry or its volume gone): the stream ends.
fn take_owner_word(s: &mut ShardState, stream: &mut ShardsStream, word: OwnerWord) -> bool {
  let answered = match word {
    OwnerWord::Named { unique, uid, gid } => match root_owner(s, stream) {
      Some(Ok(kept)) => {
        stream.owners = OwnerView::swapping(kept, (uid, gid));
        s.count(OWNER_NAMED, 1);
        #[cfg(unix)]
        tell_session(s, stream);
        #[cfg(windows)]
        resync_handle::changed(stream);
        word
      }
      Some(Err(errno)) => OwnerWord::Refused { unique, errno },
      None => return false,
    },
    OwnerWord::Refused { .. } => word,
  };
  if answered.errno() != 0 {
    s.count(OWNER_REFUSED, 1);
  }
  stream.owed = owner_word_reply(&answered, &mut stream.outbound);
  stream.sent = 0;
  true
}

/// The owner (uid, gid) the volume keeps for the stream's root, its scope's directory or the volume's own: the errno
/// its read was refused with, or `None` when the attachment can no longer be served.
fn root_owner(s: &mut ShardState, stream: &mut ShardsStream) -> Option<Result<(u32, u32), i32>> {
  let &handle = s.by_id.get(&stream.volume)?;
  let cx = s.attachments.begin(stream.registry).ok()?;
  let owner = {
    let ShardState { store, volumes, .. } = &mut *s;
    match volumes.get_mut(handle) {
      Ok(slot) => {
        let mut bridge = VolumeBridge::attached(
          stream.volume,
          &mut slot.volume,
          store,
          &mut stream.handles,
          slot.host.as_mut().map(|host| host as &mut dyn HostFs),
        );
        let mut scoped = None;
        let served = crate::verbs::scoped_or_whole(&mut bridge, &mut scoped, stream.scope);
        let attrs = served
          .root(&cx)
          .and_then(|root| served.getattr(ObjectId::new(root, 0), &cx));
        Some(
          attrs
            .map(|attrs| (attrs.uid, attrs.gid))
            .map_err(slates_bridge_fuse::bridge::errno),
        )
      }
      Err(_) => None,
    }
  };
  s.attachments.end(stream.registry);
  owner
}

/// Makes `bytes` the reply owed on `stream`.
fn owe(stream: &mut ShardsStream, bytes: &[u8]) {
  if stream.outbound.len() < bytes.len() {
    stream.outbound.resize(bytes.len(), 0);
  }
  if let Some(head) = stream.outbound.get_mut(..bytes.len()) {
    head.copy_from_slice(bytes);
  }
  stream.owed = bytes.len();
  stream.sent = 0;
}

/// What the ingest a request was held for did (step 3).
#[derive(Clone, Copy, Debug, Default)]
struct Ingested {
  /// Whether it wrote the guest's stores into the volume: the change is published before the reply.
  wrote: bool,
  /// The errno of a write-back the volume refused, which the request answers if it is one that reports one.
  refused: Option<i32>,
}

/// Answers the request at the head of `stream` (`total` bytes, its length word included): dispatched under the
/// attachment's registry entry through a transient bridge, then the barrier its change waits for, refused `EIO` with
/// what it granted given back when the barrier does not capture the volume. A request whose ingest the volume refused
/// answers that errno where its asker reads one (an `FSYNC`, a `FLUSH` and a `READ`), so a guest's fsync or close learns
/// its stores did not all reach the volume. A `RELEASE`'s answer the kernel ignores, and a `FORGET` has none. `false`
/// when the attachment can no longer be served (its registry entry or its volume gone): the stream ends.
fn serve_request(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  total: usize,
  ingested: Ingested,
) -> bool {
  crate::verbs::relieve_deferred(s);
  let dax_offer = dax::offer(s);
  // Where the journal stood before the request (or the ingest held for it) ran: its reply waits for the volume's FUSE
  // mounts to have been told of the records since (AC-3.3, A-196).
  let told_mark = stream
    .told_mark
    .take()
    .or_else(|| crate::told::mark(s, stream.volume));
  let Some(&handle) = s.by_id.get(&stream.volume) else {
    return false;
  };
  let Ok(cx) = s.attachments.begin(stream.registry) else {
    return false;
  };
  let answered = {
    let ShardState {
      store,
      volumes,
      attachments,
      ..
    } = &mut *s;
    let Ok(slot) = volumes.get_mut(handle) else {
      attachments.end(stream.registry);
      return false;
    };
    let mut bridge = VolumeBridge::attached(
      stream.volume,
      &mut slot.volume,
      store,
      &mut stream.handles,
      slot.host.as_mut().map(|host| host as &mut dyn HostFs),
    )
    .offering_dax(dax_offer)
    .viewing_owners(stream.owners)
    .serving_a_guest()
    .with_lost_writes(&mut stream.lost);
    // The guest's kernel sends `SETXATTR` in the form its `INIT` reply kept, or the saved guest's did (A-378).
    slates_bridge_core::Bridge::set_setxattr_ext(&mut bridge, stream.setxattr_ext);
    let mut scoped = None;
    let served = crate::verbs::scoped_or_whole(&mut bridge, &mut scoped, stream.scope);
    let request = stream.inbound.get(LENGTH_BYTES..total).unwrap_or(&[]);
    answer(request, served, &cx, &mut stream.outbound)
  };
  s.attachments.end(stream.registry);
  kept_at_init(s, stream, &answered);
  release_forgotten(s, stream, total, &answered);
  // A write the guest is told succeeded outlives this daemon in the shard's write log, as a FUSE mount's does (A-63):
  // it is acknowledged before the next publication, and a death first would otherwise take it from the volume (A-316;
  // every platform, A-402).
  log_write(s, stream, total, &answered);
  log_forgets(s, stream, total, &answered);
  stream.owed = answered.frame;
  stream.sent = 0;
  // A reply after which the guest keeps an effect across a restart waits for the publication that makes it durable
  // (D-18): a change the volume's next restart must hold (`needs_barrier`), an inode number no committed publication
  // reserves (A-150), or a release of an open the committed publication counts (A-154).
  let awaits_publication = crate::verbs::awaits_publication(s, stream.volume);
  // A `close`'s `FLUSH` owes no publication of its own while the write log holds every write the shard acknowledged
  // (A-399, A-63's flush rule): a successor replays them all, kept stream or not, and a `FLUSH` the device resends after
  // a death is answered again, changing nothing. Published anyway when the log overflowed, as a mount's is.
  let flush_logged = answered.opcode == Some(Opcode::Flush)
    && s
      .write_log
      .as_ref()
      .is_some_and(crate::write_log::WriteLog::holds_every_write);
  if flush_logged {
    s.count(FLUSH_LOGGED, 1);
  }
  // An ingest that wrote the guest's stores before this request is published before its reply too (step 3).
  let mut refused = false;
  if (needs_barrier(answered.opcode, answered.error) && !flush_logged)
    || awaits_publication
    || ingested.wrote
  {
    // The reply rides the publication with its effect (A-61's rule), so a daemon that dies before writing it leaves a
    // successor that resyncs the stream the answer to the request the device resends, never a second application.
    keep_pending_reply(s, stream, &answered);
    let captured =
      crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(stream.volume));
    if !captured {
      s.count(BARRIER_REFUSED, 1);
      reclaim(s, stream, &answered);
      stream.owed = refuse(answered.unique, EIO, &mut stream.outbound);
      refused = true;
    }
  }
  if let Some(errno) = ingested.refused
    && matches!(
      answered.opcode,
      Some(Opcode::FSync | Opcode::Flush | Opcode::Read)
    )
  {
    stream.owed = refuse(answered.unique, errno, &mut stream.outbound);
    refused = true;
  }
  // A reply refused after it was made is not the one a resent request may be answered with.
  if refused {
    s.pending_replies.remove(&stream.attachment);
  }
  // A guest's `fsync` of a live-shipped volume is answered once its log is durable on `f` candidates where this barrier
  // put it (A-200; AC-8.3), as a FUSE mount's is (`crate::fuse`): held, the task waits for it ([`Turned::Durable`]).
  stream.durable = None;
  if !refused
    && answered.error == 0
    && matches!(answered.opcode, Some(Opcode::FSync | Opcode::FSyncDir))
    && let Some(target) = crate::live_ship::durable_target(s, stream.volume)
  {
    stream.durable = Some(DurableOwed {
      target,
      unique: answered.unique,
    });
    s.count(LIVE_FSYNC_HELD, 1);
    // A held `fsync` claims nothing until its log is durable: resent, it is served again, as FUSE's is.
    s.pending_replies.remove(&stream.attachment);
  }
  stream.told = told_mark.and_then(|mark| crate::told::owed(s, mark));
  if s.shards_withhold_replies && stream.owed > 0 {
    // Test support: the reply is made, and published with its effect where it waited for a barrier, and never written.
    s.count(REPLY_WITHHELD, 1);
    stream.owed = 0;
    stream.sent = 0;
  }
  true
}

/// Whether the anchor holds `stream`'s resync end, so a successor may serve it again (A-316, A-339).
#[cfg(unix)]
fn resync_held(stream: &ShardsStream) -> bool {
  stream.resync.as_ref().is_some_and(resync::Resync::held)
}

/// See the Unix arm: the anchor's answer to the hold says (A-427).
#[cfg(windows)]
fn resync_held(stream: &ShardsStream) -> bool {
  resync_handle::held(stream)
}

/// Keeps the barrier reply of the request `answered` names, the reply `stream` owes, in the shard's pending replies,
/// so the publication about to run carries it with its effect (A-61's rule; `fuse`'s `reply` does the same for a
/// mount): only for a stream a successor may resync (A-316; Windows, A-433), whose device would resend the request.
fn keep_pending_reply(s: &mut ShardState, stream: &ShardsStream, answered: &Answered) {
  if !resync_held(stream) {
    return;
  }
  s.pending_replies.insert(
    stream.attachment,
    slates_vfs::recover::HeldReply {
      attachment: stream.attachment,
      unique: answered.unique,
      reply: answered.reply(&stream.outbound).to_vec(),
    },
  );
}

/// Logs the `WRITE` at the head of `stream` (`total` bytes), when `answered` says it succeeded, in the shard's write log
/// before its reply, under the stream's volume, where a replay puts it (A-63, A-316; an inode number alone names the
/// origin of a clone's inherited file): as `fuse`'s `log_write` does for a mount, a log with no room forces a publication
/// that carries the write, and only a refused publication leaves it unlogged, marked as an overflow so the next daemon
/// reports the loss rather than replaying around it.
fn log_write(s: &mut ShardState, stream: &ShardsStream, total: usize, answered: &Answered) {
  if answered.opcode != Some(Opcode::Write) || answered.error != 0 {
    return;
  }
  let request = stream.inbound.get(LENGTH_BYTES..total).unwrap_or(&[]);
  let Some((nodeid, offset, data)) = written(request) else {
    return;
  };
  let appended = match (s.write_log.as_mut(), s.content.as_mut()) {
    (Some(log), Some(object)) => log.append(object, (stream.volume.bytes, nodeid), offset, data),
    _ => return,
  };
  let logged = match appended {
    Ok(()) => true,
    Err(crate::write_log::Refused::Full) => {
      crate::verbs::publish_shard(s).is_ok_and(|published| published.skipped.is_empty())
    }
    Err(crate::write_log::Refused::Unwritten) => false,
  };
  if !logged {
    s.count(crate::verbs::WRITE_LOG_UNWRITTEN, 1);
    if let (Some(log), Some(object)) = (s.write_log.as_mut(), s.content.as_mut())
      && log.overflow(object).is_err()
    {
      s.count(crate::verbs::WRITE_LOG_UNWRITTEN, 1);
    }
  }
}

/// Logs the lookups the `FORGET` or `BATCH_FORGET` at the head of `stream` (`total` bytes) gave back, where the anchor
/// holds the stream's resync end (A-390): a successor that keeps the stream gives them back again, as a FUSE mount's
/// does (A-154's sibling), since the guest never resends a `FORGET`. Before A-390 a resynced stream kept the references
/// a lost one gave back until it ended, and an unlinked file its guest had let go stayed referenced. A stream no
/// successor keeps ends at its daemon's death and every reference it held goes with it, so nothing is logged for it.
fn log_forgets(s: &mut ShardState, stream: &ShardsStream, total: usize, answered: &Answered) {
  if !matches!(answered.opcode, Some(Opcode::Forget | Opcode::BatchForget)) || !resync_held(stream)
  {
    return;
  }
  let mut forgotten = Vec::new();
  let request = stream.inbound.get(LENGTH_BYTES..total).unwrap_or(&[]);
  kernel_changes(request, answered.reply(&stream.outbound), &mut |change| {
    if let KernelChange::Forgot { node, count } = change {
      forgotten.push((node, count));
    }
  });
  crate::write_log::log_forgets(s, (stream.volume, stream.attachment), forgotten);
}

/// The node, offset and bytes of the `WRITE` request `request` is; `None` for any other.
fn written(request: &[u8]) -> Option<(u64, u64, &[u8])> {
  let parsed = slates_bridge_fuse::request::Request::parse(request).ok()?;
  if parsed.opcode != Some(Opcode::Write) {
    return None;
  }
  let write =
    slates_bridge_fuse::request::WriteIn::parse(Opcode::Write.to_wire(), parsed.body).ok()?;
  Some((parsed.header.nodeid, write.offset, write.data))
}

/// The framed reply a resent `request` is answered with on a stream served again after its daemon's death (A-316): the
/// one the dead daemon published with its effect and never wrote, when `request` is the request it answers (its unique
/// unchanged, as Shards' device resends it). Taken at the stream's first request either way: a first request that is
/// another means the device was answered before the death.
fn replayed(stream: &mut ShardsStream, request: &[u8]) -> Option<Vec<u8>> {
  let (recorded, reply) = stream.replay.take()?;
  let unique = slates_bridge_fuse::request::InHeader::parse(request)
    .ok()?
    .unique;
  if unique != recorded {
    return None;
  }
  let mut frame = u32::try_from(reply.len()).ok()?.to_le_bytes().to_vec();
  frame.extend_from_slice(&reply);
  Some(frame)
}

/// Lets go of the backing of every node the request at the head of `stream` (`total` bytes) gave back: a guest's
/// `FORGET` or `BATCH_FORGET`. A Linux guest has removed a DAX inode's mappings, by waited `FUSE_REMOVEMAPPING` requests,
/// before it forgets the inode (`fs/fuse/inode.c`), so the device maps none of the object by then.
fn release_forgotten(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  total: usize,
  answered: &Answered,
) {
  if stream.dax.is_empty() {
    return;
  }
  let mut forgotten = Vec::new();
  let request = stream.inbound.get(LENGTH_BYTES..total).unwrap_or(&[]);
  kernel_changes(request, answered.reply(&stream.outbound), &mut |change| {
    if let KernelChange::Forgot { node, .. } = change {
      forgotten.push(node);
    }
  });
  for node in forgotten {
    dax::release(s, stream, node);
  }
}

/// Gives back what `answered`'s reply granted (its lookup references, its open handle), over a transient bridge, since
/// the reply is refused and the guest never learns of them (AUD-29-85).
fn reclaim(s: &mut ShardState, stream: &mut ShardsStream, answered: &Answered) {
  let Some(&handle) = s.by_id.get(&stream.volume) else {
    return;
  };
  let Ok(cx) = s.attachments.begin(stream.registry) else {
    return;
  };
  let given_back = {
    let ShardState { store, volumes, .. } = &mut *s;
    volumes.get_mut(handle).ok().map(|slot| {
      let mut bridge = VolumeBridge::attached(
        stream.volume,
        &mut slot.volume,
        store,
        &mut stream.handles,
        slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      );
      let mut scoped = None;
      let served = crate::verbs::scoped_or_whole(&mut bridge, &mut scoped, stream.scope);
      let reply = answered.reply(&stream.outbound);
      let reclaimed = reclaim_unreported(answered.opcode, answered.nodeid, reply, served, &cx);
      reclaimed.references.saturating_add(reclaimed.handles)
    })
  };
  s.attachments.end(stream.registry);
  s.count(REPLY_RECLAIMED, given_back.unwrap_or(0));
}

/// Where writing the owed reply got to.
enum Flushed {
  /// Nothing is owed (any owed reply is all out).
  Done,
  /// The stream takes no more now; the rest waits for its writability.
  Blocked,
  /// The stream failed (its peer gone, or an error).
  Failed,
}

/// Writes as much of `stream`'s owed reply as the stream takes now. Each send takes at least a byte or answers, so the
/// loop ends; a buffer grown past one largest frame for a large read falls back once its reply is out, so a stream holds
/// that much only while it serves one.
fn flush(stream: &mut ShardsStream) -> Flushed {
  while stream.sent < stream.owed {
    let Some(rest) = stream.outbound.get(stream.sent..stream.owed) else {
      return Flushed::Failed;
    };
    // A descriptor the reply carries rides its first byte; once any byte is out, the descriptor went with it.
    match end::send(&stream.stream, rest, stream.owed_descriptor.as_ref()) {
      Sent::Blocked => return Flushed::Blocked,
      Sent::Taken(sent) => {
        stream.owed_descriptor = None;
        stream.sent = stream.sent.saturating_add(sent);
      }
      Sent::Failed => return Flushed::Failed,
    }
  }
  stream.owed = 0;
  stream.sent = 0;
  let resting = LENGTH_BYTES.saturating_add(FRAME_BYTES);
  if stream.outbound.len() > resting {
    stream.outbound.truncate(resting);
    stream.outbound.shrink_to(resting);
  }
  Flushed::Done
}

/// The attachment has ended: its references swept through the bridge, its registry entry revoked and drained, its
/// descriptor closed, and its record removed as a recorded operation (unless a `detach`, a revocation or a destroy
/// already removed it). Idempotent.
fn ended(s: &mut ShardState, attachment: u64) {
  withdraw_offer(s, attachment);
  if let Some(mut stream) = s.shards_streams.remove(&attachment) {
    // The device reads its resync channel's end: no daemon will resync an attachment that ended (A-316).
    #[cfg(unix)]
    if let Some(resync) = stream.resync.as_mut()
      && resync::give_up(attachment, resync) == resync::GivenUp::Refused
    {
      s.count(RESYNC_RELEASE_REFUSED, 1);
    }
    #[cfg(windows)]
    let _given_up = resync_handle::give_up(s, &mut stream);
    dax::release_all(s, &mut stream);
    sweep(s, &mut stream);
    s.attachments.revoke(stream.registry);
    s.attachments.drain(stream.registry);
  }
  if let Some(record) = s.db.partition().attachment(attachment).cloned()
    && crate::verbs::end_attachment(s, &record, crate::verbs::Ending::Otherwise).is_err()
  {
    s.count(RECORD_END_REFUSED, 1);
  }
}

/// Withdraws `attachment`'s stream end still offered on the socket route, counted: an attachment that ended leaves no
/// end for a claim to take, as a direct map's copy let go leaves no offer (A-173). Before, a claim after the end took an
/// end whose other side was already closed.
#[cfg(target_os = "macos")]
fn withdraw_offer(s: &mut ShardState, attachment: u64) {
  if crate::shards_route::withdraw(s, (slates_ipc::route::OfferKind::Stream, attachment)) {
    s.count(OFFER_WITHDRAWN, 1);
  }
}

/// Nothing is offered here: Linux sends a stream end on the control socket before the attach's reply, and Windows
/// refuses the form.
#[cfg(not(target_os = "macos"))]
fn withdraw_offer(_s: &mut ShardState, _attachment: u64) {}

/// Releases every reference `stream`'s guest still holds (§3 the teardown sweep): a guest's end sends no forget per
/// reference. A volume already gone took them with it.
fn sweep(s: &mut ShardState, stream: &mut ShardsStream) {
  let Some(&handle) = s.by_id.get(&stream.volume) else {
    return;
  };
  let Ok(cx) = s.attachments.begin(stream.registry) else {
    return;
  };
  let swept = {
    let ShardState { store, volumes, .. } = &mut *s;
    volumes.get_mut(handle).ok().map(|slot| {
      let mut bridge = VolumeBridge::attached(
        stream.volume,
        &mut slot.volume,
        store,
        &mut stream.handles,
        slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      );
      slates_bridge_core::Bridge::sweep_attachment(&mut bridge, &cx)
    })
  };
  s.attachments.end(stream.registry);
  if !swept.is_some_and(|swept| swept.is_ok()) {
    s.count(SWEEP_REFUSED, 1);
  }
}

/// Ends the stream of `attachment`, if the shard serves one: run by `verbs::end_attachment` for every end but the
/// stream's own (a detach, a revocation, the volume's destroy). The stream is marked ending, so its task reads nothing
/// more, and ended both ways, so the task wakes on its own readiness and ends the rest ([`ended`]). A stream already
/// ending is left as it is: it ends at its task's next turn.
pub(crate) fn end_stream(s: &mut ShardState, attachment: u64) {
  // Withdrawn here, before the ending's reply, so no claim made after it can take the end.
  withdraw_offer(s, attachment);
  // Ended once: a destroy's teardown asks again each pass while it waits for the stream (`verbs::teardown_started`),
  // and a second shutdown of a socket is refused (`ENOTCONN` on macOS), which was counted as a failed end
  // (2026-10-25, found by `a_destroy_ends_the_stream_and_removes_its_record`).
  let Some(stream) = s
    .shards_streams
    .get_mut(&attachment)
    .filter(|stream| !stream.ending)
  else {
    return;
  };
  stream.ending = true;
  if !end::end(&stream.stream) {
    s.count(END_REFUSED, 1);
  }
}

/// Ends every stream the shard serves (the daemon's stop): each attachment's references swept, its registry entry
/// released and its record removed. Run by the end of the shard's serve loop (`daemon::EndStreams`): on a stop
/// nothing else runs on the shard.
pub(crate) fn end_all(s: &mut ShardState) {
  let attachments: Vec<u64> = s.shards_streams.keys().copied().collect();
  for attachment in attachments {
    ended(s, attachment);
  }
}

/// Closes every stream the shard serves and writes no record: all a fenced shard may do at its stop. The records end
/// at the next start's recovery, as every guest's does, but for those whose resync ends the anchor still holds, which
/// that recovery keeps and serves again (A-316): nothing here releases a hold, as a death releases none.
pub(crate) fn drop_all(s: &mut ShardState) {
  s.shards_streams.clear();
}

/// Serves again every Shards stream recovery kept because the anchor held its resync end (A-316, A-339): each on a fresh
/// stream ([`adopt_one`]), its device handed the fresh end over the held channel. Every held device no record of its
/// kind claimed (a FUSE device or a resync end whose attachment recovery ended) is released first, so the anchor closes
/// it: a FUSE mount's kernel then sees its device gone, and a device reads its resync channel's end.
#[cfg(unix)]
pub(crate) fn adopt_held() {
  let (kept, orphaned) = state::with_state(|s| {
    (
      std::mem::take(&mut s.adopt_shards),
      std::mem::take(&mut s.inherited_fuse),
    )
  })
  .unwrap_or_default();
  for held in orphaned {
    let _ = crate::fuse_hold::release(held.attachment);
  }
  for (record, held) in kept {
    let _ = state::with_state_counted(move |s| adopt_one(s, record, held));
  }
  // Every DAX object recovery kept for a stream no adoption took is let go (A-365).
  let _ = state::with_state_counted(dax_hold::let_go_every_unclaimed);
}

/// Serves `record`, a Shards attachment its dead daemon served, again on a fresh stream, then hands its device the fresh
/// end over `held`, the resync end the anchor kept (A-316): the same attachment, its owner view and token from the
/// hold's session, its directory from its record, the references and opens recovery gave back to its record; no `INIT`
/// (the guest's kernel negotiated with the dead daemon) and no restore; and the reply the dead daemon published and never
/// wrote answers the request the device resends. Anything refused ends the attachment and releases its hold, counted:
/// the device reads the channel's end (Shards' option (a)).
#[cfg(unix)]
fn adopt_one(s: &mut ShardState, record: AttachmentRecord, held: crate::fuse_hold::HeldDevice) {
  let attachment = record.id;
  let kept = held.raw_session.as_deref().and_then(resync::from_session);
  let (Some((token, kept)), AttachForm::ShardsFs { scope }) = (kept, record.form.clone()) else {
    return refuse_adoption(s, &record);
  };
  let Ok((daemon_end, device_end)) = end::pair() else {
    return refuse_adoption(s, &record);
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
    return refuse_adoption(s, &record);
  };
  s.attachments.set_owner(registry, attachment);
  let mut stream = ShardsStream::new(
    attachment,
    daemon_end,
    registry,
    record.volume,
    scope,
    tables::most_bytes(s.config.store.max_inodes),
  );
  stream.owners = kept.owners;
  stream.setxattr_ext = kept.setxattr_ext;
  // What the dead daemon's death lost of the writes it acknowledged: every file, where its log overflowed (A-393).
  stream.lost =
    slates_bridge_core::LostWrites::new(std::collections::BTreeSet::new(), s.writes_lost);
  stream.restorable = false;
  // The successor's copy of the held end is kept as the attach's daemon kept its own (`resync`'s module notes).
  stream.resync = Some(resync::Resync {
    token,
    end: Some(held.device),
  });
  stream.replay = s.recovered_replies.remove(&attachment);
  // Its DAX objects recovery kept, taken into its session and ingested before it serves anything (A-365).
  let _ingesting = dax_hold::claim(s, &mut stream);
  s.shards_streams.insert(attachment, stream);
  if futures::spawn(serve(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    s.count(RESYNC_REFUSED, 1);
    ended(s, attachment);
    return;
  }
  let sent = s
    .shards_streams
    .get(&attachment)
    .and_then(|stream| stream.resync.as_ref())
    .and_then(|resync| resync.end.as_ref())
    .is_some_and(|held| resync::send_resync(held, attachment, token, &device_end));
  if !sent {
    s.count(RESYNC_REFUSED, 1);
    end_stream(s, attachment);
    return;
  }
  // The device's end of the fresh stream stays open here until the device shows it holds its own (A-339).
  if let Some(stream) = s.shards_streams.get_mut(&attachment) {
    stream.fresh_copy = Some(device_end);
  }
  s.count(RESYNCED, 1);
  if s.writes_lost {
    s.count(RESYNC_WRITES_LOST, 1);
  }
}

/// Ends `record`'s attachment, which its successor could not serve again, and releases its held resync end, counted
/// (A-316): the device reads the channel's end (Shards' option (a)).
#[cfg(unix)]
fn refuse_adoption(s: &mut ShardState, record: &AttachmentRecord) {
  s.count(RESYNC_REFUSED, 1);
  if !matches!(crate::fuse_hold::release(record.id), Some(Ok(()))) {
    s.count(RESYNC_RELEASE_REFUSED, 1);
  }
  if crate::verbs::end_attachment(s, record, crate::verbs::Ending::Otherwise).is_err() {
    s.count(RECORD_END_REFUSED, 1);
  }
}

/// Splits the resync ends the anchor handed back (Windows, A-433; `crate::anchor_hold::adopt`) by the partition that
/// owns their attachment (an attachment id carries its owner in its high bits); one no partition owns goes to the
/// first, where no record claims it and it is let go.
#[cfg(windows)]
pub(crate) fn split_held_resync(
  handed: Vec<crate::anchor_hold::HandedResync>,
  partitions: usize,
) -> Vec<Vec<crate::anchor_hold::HandedResync>> {
  let mut split: Vec<Vec<crate::anchor_hold::HandedResync>> =
    (0..partitions).map(|_| Vec::new()).collect();
  for held in handed {
    let partition = usize::from(crate::verbs::owner_of_attachment(held.attachment));
    let target = if partition < partitions { partition } else { 0 };
    if let Some(list) = split.get_mut(target) {
      list.push(held);
    }
  }
  split
}

/// Serves again every Windows Shards stream recovery kept because the anchor held its resync end (A-433), each on a
/// fresh stream ([`adopt_one`]); every held end no kept record claims is let go first, by a task, since the anchor's
/// release is an exchange: its device reads the channel's end.
#[cfg(windows)]
pub(crate) fn adopt_held() {
  let (kept, orphaned) = state::with_state(|s| {
    (
      std::mem::take(&mut s.adopt_shards),
      std::mem::take(&mut s.inherited_resync),
    )
  })
  .unwrap_or_default();
  for orphan in orphaned {
    let attachment = orphan.attachment;
    drop(orphan);
    let _ = state::with_state_counted(|s| resync_handle::release_orphan(s, attachment));
  }
  for (record, held) in kept {
    let _ = state::with_state_counted(move |s| adopt_one(s, record, held));
  }
}

/// Serves `record`, a Shards attachment its dead daemon served on Windows, again on a fresh stream, then hands its
/// device the fresh end (A-433): the same attachment, its owner view, form, token and holder from the record the anchor
/// kept; no `INIT` and no restore; the reply the dead daemon published and never wrote answers the request the device
/// resends. The fresh end is duplicated into the holder, checked by its creation time, and a `Resync` naming it is
/// written on the held channel without waiting ([`resync_handle::send_resync`]). Anything refused ends the attachment,
/// whose end releases the hold, counted: the device reads the channel's end (Shards' option (a)).
#[cfg(windows)]
fn adopt_one(s: &mut ShardState, record: AttachmentRecord, held: crate::anchor_hold::HandedResync) {
  let attachment = record.id;
  let session = resync::from_handle_session(&held.record);
  let (Some((token, kept, holder)), AttachForm::ShardsFs { scope }) =
    (session, record.form.clone())
  else {
    return refuse_adoption(s, &record);
  };
  let Ok((daemon_end, device_end)) = end::pair() else {
    return refuse_adoption(s, &record);
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
    return refuse_adoption(s, &record);
  };
  s.attachments.set_owner(registry, attachment);
  let mut stream = ShardsStream::new(
    attachment,
    daemon_end,
    registry,
    record.volume,
    scope,
    tables::most_bytes(s.config.store.max_inodes),
  );
  stream.owners = kept.owners;
  stream.setxattr_ext = kept.setxattr_ext;
  // What the dead daemon's death lost of the writes it acknowledged: every file, where its log overflowed (A-393).
  stream.lost =
    slates_bridge_core::LostWrites::new(std::collections::BTreeSet::new(), s.writes_lost);
  stream.restorable = false;
  stream.resync = Some(resync_handle::adopted(token, holder));
  stream.replay = s.recovered_replies.remove(&attachment);
  s.shards_streams.insert(attachment, stream);
  if futures::spawn(serve(attachment))
    .and_then(futures::detach)
    .is_err()
  {
    s.count(RESYNC_REFUSED, 1);
    ended(s, attachment);
    return;
  }
  if !resync_handle::send_resync(&held.end, (attachment, token), holder, device_end) {
    s.count(RESYNC_REFUSED, 1);
    end_stream(s, attachment);
    return;
  }
  s.count(RESYNCED, 1);
  if s.writes_lost {
    s.count(RESYNC_WRITES_LOST, 1);
  }
}

/// Ends `record`'s attachment, which its successor could not serve again, and lets its held resync end go, counted
/// (Windows, A-433): the device reads the channel's end (Shards' option (a)).
#[cfg(windows)]
fn refuse_adoption(s: &mut ShardState, record: &AttachmentRecord) {
  s.count(RESYNC_REFUSED, 1);
  resync_handle::release_orphan(s, record.id);
  if crate::verbs::end_attachment(s, record, crate::verbs::Ending::Otherwise).is_err() {
    s.count(RECORD_END_REFUSED, 1);
  }
}

impl Daemon {
  /// The runtime shard that owns `volume`, or the typed refusal when the partition has no shard: where a Shards stream
  /// (here) or a guest device (`crate::virtiofs`, Unix) is served.
  pub(crate) fn owner_shard(
    &self,
    volume: slates_ipc::protocol::VolumeId,
  ) -> Result<u16, ServerError> {
    let partition = crate::verbs::owner_of(volume);
    self
      .shards()
      .get(usize::from(partition))
      .map(|shard| shard.0)
      .ok_or(ServerError::Runtime(slates_rt::RtError::ShardGone {
        shard: partition,
      }))
  }

  /// Attaches a volume to a Shards microVM's fs device over a new anonymous stream (§4.6, the Shards route; A-161),
  /// served on the volume's owning shard for its whole life: `ask` names the volume, who attaches, the directory it
  /// presents if one, and its intent. `on_attached` receives the attachment and the stream end to hand the VMM, or the
  /// typed refusal (the volume unknown, the rights short of the intent, the directory not one, the write lease held by
  /// another, the shard's streams at their bound). Refuses typed only when the owner shard cannot be reached.
  pub fn attach_shards_stream(
    &self,
    ask: ShardsAsk,
    on_attached: OnShardsAttached,
  ) -> Result<(), ServerError> {
    let shard = self.owner_shard(ask.volume)?;
    let task = SpawnRequest::new(
      Box::pin(async move {
        let attached =
          state::with_state(|s| attach(s, ask)).unwrap_or(Err(Refusal::Overloaded { shard }));
        on_attached(attached);
      }),
      None,
    );
    registry::send_control(shard, Control::Spawn(Box::new(task))).map_err(ServerError::Runtime)
  }
}
