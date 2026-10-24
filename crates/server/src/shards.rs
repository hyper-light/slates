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
//! **Control asks.** Shards' device sends asks of its own: its backend's tables for a snapshot of the VM, and their
//! restore (D176), and a file's descriptor for a DAX window's mapping (D189). slates keeps no tables a VM's snapshot can
//! carry, and a file's descriptor would hand the VMM the volume's whole content object (A-135), so each is refused in
//! the form Shards reads: a control answer with its reason, and for a descriptor an errno its guest's mapping returns. A
//! share slates serves is therefore launched without a DAX window until the per-attachment backing exists.
//!
//! **Teardown.** The peer closing its end, a frame refused as hostile, an error on the stream, a `detach`, a
//! revocation, the volume's destroy and the daemon's stop all end the attachment the same way ([`ended`]): its
//! references are swept through the bridge (a guest sends no forget per reference at its end), its registry entry
//! revoked and drained, its descriptor closed, and its record removed as a recorded operation. Anything but the stream
//! itself ends the stream first ([`end_stream`]), so the task wakes on its own readiness and ends the rest; the stream
//! never outlives its record. Nothing survives a restart: recovery removes the record, as it removes every guest's.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use slates_bridge_core::volume_bridge::new_handle_store;
use slates_bridge_core::{AttachmentId, Rights, View, VolumeBridge};
use slates_bridge_fuse::bridge::{EIO, needs_barrier, reclaim_unreported};
use slates_bridge_fuse::shards_fs::{
  ASK_DESCRIPTOR, ASK_RESTORE, ASK_SAVE, Answered, FRAME_BYTES, Frame, LENGTH_BYTES, Split, answer,
  control_refusal, descriptor_refusal, refuse, split_frame,
};
use slates_db::Op;
use slates_db::catalog::{AttachForm, AttachmentRecord, Consumer, Principal, VolumeId};
use slates_ipc::protocol::{Intent, Refusal};
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

/// Format: the refusal-ledger name of a frame the protocol refuses (a length past the largest frame, a control frame
/// with no ask): the stream that sent it is ended.
const FRAME_REFUSED: &str = "shards.frame_refused";
/// Format: the refusal-ledger name of a control ask refused (a snapshot's tables, their restore, a DAX descriptor).
const CONTROL_REFUSED: &str = "shards.control_refused";
/// Format: the refusal-ledger name of a barrier that did not capture its volume: the reply was answered `EIO`.
const BARRIER_REFUSED: &str = "shards.barrier_refused";
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
/// Format: the refusal-ledger name of a stream end that could not be sent to its client (no control socket, the
/// client gone, a refused send): the end closes, so the stream ends and its attachment with it.
const DELIVERY_REFUSED: &str = "shards.delivery_refused";
/// Format: the refusal-ledger name of a stream end dropped because its request's reply was not the attach that made
/// it (a rolled-back verb answered a refusal): the end closes, so the stream ends with it.
const HANDOFF_DROPPED: &str = "shards.handoff_dropped";
/// Format: Linux `EOPNOTSUPP`, the errno a refused descriptor ask carries to the guest's mapping; the guest is a Linux
/// kernel whatever the host, so the number is Linux's.
const EOPNOTSUPP: i32 = 95;
/// Format: the reason a refused snapshot ask carries to Shards, which reports it.
const NO_TABLES: &str =
  "slates keeps no share tables a VM snapshot can carry; the share ends with the VM";
/// Format: the reason a refused restore ask carries to Shards, which reports it.
const NO_RESTORE: &str =
  "slates restores no share tables; attach the volume again to the restored VM";
/// Format: the reason an unknown control ask's refusal carries to Shards.
const UNKNOWN_ASK: &str = "a control ask slates does not know";

/// One attachment served over a stream: its end of the pair, the open-handle map kept across requests, the registry
/// attachment its requests are admitted under, what it presents, and the frames in and out.
pub(crate) struct ShardsStream {
  /// The daemon's end of the pair, non-blocking.
  stream: OwnedFd,
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

/// A stream end owed to the client whose attach made it (A-161): kept on the client's shard under the attach's
/// request word until the attach's reply is written (`verbs::send_reply`), and sent over the client's control socket
/// just before it, only with that reply. Made on the volume's owner shard; a forwarded attach carries it back to the
/// client's shard with the reply (`verbs::send_forward`).
pub(crate) struct Handoff {
  /// The attachment the daemon serves on the stream's other end.
  pub(crate) attachment: u64,
  /// The end to hand the client.
  pub(crate) stream: OwnedFd,
}

/// The shard's stream ends owed to clients, by the request word of the attach that made each. Bounded by the
/// requests the shard's clients may have in flight ([`handoff_bound`]): an attach past it is refused `Overloaded`.
pub(crate) type Handoffs = BTreeMap<u64, Handoff>;

/// Derived: the stream ends a shard may owe at once: one per request its clients may have in flight
/// (`clients_per_shard × ring slots`), the bound `verbs::remember_forwarded_ring` keeps for forwarded requests.
fn handoff_bound(s: &ShardState) -> usize {
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
  pub stream: OwnedFd,
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
  let (daemon_end, handed) =
    slates_ipc::stream::pair().map_err(|_| Refusal::Overloaded { shard: s.shard })?;
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
  let op = Op::AttachmentAdded {
    record: attachment_record.clone(),
  };
  if let Err(e) = s.db.mutate(&mut s.segment, &op, now) {
    s.attachments.revoke(registry);
    s.attachments.drain(registry);
    crate::verbs::release_unrecorded_lease(s, &attachment_record);
    return Err(crate::error::refusal_of_db(&e));
  }
  s.shards_streams.insert(
    attachment,
    ShardsStream {
      stream: daemon_end,
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
    },
  );
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
  })
}

/// The attach verb's Shards form (§4.4, §4.6; A-161), on the volume's owner shard inside the verb's transaction: the
/// attachment made and served as [`attach`] makes it, and its stream end kept for the client under the request's word,
/// sent just before the reply this returns. Linux only: the end reaches a client over its control socket, which the
/// other platforms do not have (refused `DescriptorRouteNotBuilt`, before any effect). A snapshot is not presented
/// over a stream yet (refused `Unsupported`).
pub(crate) fn attach_verb(
  s: &mut ShardState,
  principal: &Principal,
  volume: slates_ipc::protocol::VolumeId,
  snapshot: Option<slates_ipc::protocol::SnapshotId>,
  (intent, subtree): (Intent, Option<String>),
) -> slates_ipc::protocol::ReplyBody {
  let refused = |refusal| slates_ipc::protocol::ReplyBody::Refused { refusal };
  if !cfg!(target_os = "linux") {
    return refused(Refusal::AttachmentUnsupported {
      transport: slates_ipc::protocol::AttachTransport::ShardsFs,
      reason: slates_ipc::protocol::UnsupportedReason::DescriptorRouteNotBuilt,
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
  if s.stream_handoffs.len() >= handoff_bound(s) {
    return refused(Refusal::Overloaded { shard: s.shard });
  }
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
  let ShardsAttached {
    attachment,
    lease_epoch,
    stream,
  } = attached;
  s.stream_handoffs
    .insert(id.word(), Handoff { attachment, stream });
  let rights = s
    .db
    .partition()
    .attachment(attachment)
    .map(|record| record.rights)
    .unwrap_or_default();
  let mut capability = crate::transports::shards_fs(&crate::transports::situation(s, &rights));
  capability.read_write = if rights.write {
    slates_ipc::protocol::ReadWritePolicy::ReadWrite
  } else {
    slates_ipc::protocol::ReadWritePolicy::ReadOnly
  };
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

/// Sends the stream end owed for `request`, if the shard holds one, over the control socket of the client at
/// `client_index`, just before `reply` is written into its ring: run by `verbs::send_reply` for every reply, so the
/// end is taken out exactly once whatever the reply's fate. Sent only with the attach that made it; any other reply
/// for the request (the verb rolled back and answered a refusal) drops it, counted. A client gone, retiring, or
/// without a control socket, or a refused send, drops it too, counted: the end closes, so the stream sees its peer's
/// close and ends its attachment. The end is sent before the ring's reply, so the client finds it waiting.
pub(crate) fn deliver_handoff(
  s: &mut ShardState,
  client_index: u32,
  request: u64,
  reply: &slates_ipc::protocol::ReplyBody,
) {
  let Some(handoff) = s.stream_handoffs.remove(&request) else {
    return;
  };
  let ours = matches!(
    reply,
    slates_ipc::protocol::ReplyBody::Attached { attachment, .. } if *attachment == handoff.attachment
  );
  if !ours {
    s.count(HANDOFF_DROPPED, 1);
    return;
  }
  let client = slates_wire::request::RequestId::from_word(request).client;
  let sent = s
    .clients
    .generation_at(client_index)
    .and_then(|generation| {
      s.clients
        .get(slates_mem::Handle::from_raw(client_index, generation))
        .ok()
    })
    .filter(|slot| !slot.retiring && slot.client_id == client)
    .and_then(|slot| slot.control.as_ref())
    .is_some_and(|control| {
      control
        .send_stream(handoff.stream.as_fd(), request, handoff.attachment)
        .is_ok()
    });
  if !sent {
    s.count(DELIVERY_REFUSED, 1);
  }
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
  /// The attachment ended.
  Ended,
}

/// The serve task of one stream: wait for the stream's readiness (its writability while a reply is owed), take frames
/// until none waits, yielding between them; end the attachment when the stream ends.
async fn serve(attachment: u64) {
  loop {
    let Some(Some((stream, writing))) = state::with_state(|s| {
      s.shards_streams
        .get(&attachment)
        .map(|stream| (stream.stream.as_raw_fd(), stream.sent < stream.owed))
    }) else {
      return;
    };
    let ready = if writing {
      slates_rt::readiness::writable(stream).await
    } else {
      slates_rt::readiness::readable(stream).await
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
  if stream.ending {
    return Turned::Ended;
  }
  match flush(stream) {
    Flushed::Done => {}
    Flushed::Blocked => return Turned::Blocked,
    Flushed::Failed => {
      s.count(STREAM_FAILED, 1);
      return Turned::Ended;
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
  let (control, total) = match take_frame(stream) {
    Took::Frame { control, total } => (control, total),
    Took::Waiting => return Turned::Waiting,
    Took::Closed => return Turned::Ended,
    Took::Truncated => {
      s.count(FRAME_TRUNCATED, 1);
      return Turned::Ended;
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
  let served = match control {
    Some(ask) => {
      answer_control(stream, ask);
      s.count(CONTROL_REFUSED, 1);
      true
    }
    None => serve_request(s, stream, total),
  };
  consume(stream, total);
  if !served {
    return Turned::Ended;
  }
  match flush(stream) {
    Flushed::Done | Flushed::Blocked => Turned::Served,
    Flushed::Failed => {
      s.count(STREAM_FAILED, 1);
      Turned::Ended
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
  loop {
    let head = stream.inbound.get(..stream.filled).unwrap_or(&[]);
    let need = match split_frame(head, FRAME_BYTES) {
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
    match slates_ipc::stream::recv(stream.stream.as_fd(), room) {
      Ok(0) if stream.filled > 0 => return Took::Truncated,
      Ok(0) => return Took::Closed,
      Ok(read) => stream.filled = stream.filled.saturating_add(read),
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Took::Waiting,
      Err(_) => return Took::Failed,
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

/// Answers a control ask with its refusal, owed as the stream's reply (see the module doc).
fn answer_control(stream: &mut ShardsStream, ask: u8) {
  let refusal = match ask {
    ASK_DESCRIPTOR => descriptor_refusal(EOPNOTSUPP),
    ASK_SAVE => control_refusal(NO_TABLES),
    ASK_RESTORE => control_refusal(NO_RESTORE),
    _ => control_refusal(UNKNOWN_ASK),
  };
  owe(stream, &refusal);
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

/// Answers the request at the head of `stream` (`total` bytes, its length word included): dispatched under the
/// attachment's registry entry through a transient bridge, then the barrier its change waits for, refused `EIO` with
/// what it granted given back when the barrier does not capture the volume. `false` when the attachment can no longer
/// be served (its registry entry or its volume gone): the stream ends.
fn serve_request(s: &mut ShardState, stream: &mut ShardsStream, total: usize) -> bool {
  crate::verbs::relieve_deferred(s);
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
    );
    let mut scoped = None;
    let served = crate::verbs::scoped_or_whole(&mut bridge, &mut scoped, stream.scope);
    let request = stream.inbound.get(LENGTH_BYTES..total).unwrap_or(&[]);
    answer(request, served, &cx, &mut stream.outbound)
  };
  s.attachments.end(stream.registry);
  stream.owed = answered.frame;
  stream.sent = 0;
  // A reply after which the guest keeps an effect across a restart waits for the publication that makes it durable
  // (D-18): a change the volume's next restart must hold (`needs_barrier`), an inode number no committed publication
  // reserves (A-150), or a release of an open the committed publication counts (A-154).
  let awaits_publication = crate::verbs::awaits_publication(s, stream.volume);
  if needs_barrier(answered.opcode, answered.error) || awaits_publication {
    let captured =
      crate::verbs::publish_shard(s).is_ok_and(|published| published.captured(stream.volume));
    if !captured {
      s.count(BARRIER_REFUSED, 1);
      reclaim(s, stream, &answered);
      stream.owed = refuse(answered.unique, EIO, &mut stream.outbound);
    }
  }
  true
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
    match slates_ipc::stream::send(stream.stream.as_fd(), rest) {
      Ok(0) | Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => return Flushed::Blocked,
      Ok(sent) => stream.sent = stream.sent.saturating_add(sent),
      Err(_) => return Flushed::Failed,
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
  if let Some(mut stream) = s.shards_streams.remove(&attachment) {
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
/// more, and ended both ways, so the task wakes on its own readiness and ends the rest ([`ended`]).
pub(crate) fn end_stream(s: &mut ShardState, attachment: u64) {
  let Some(stream) = s.shards_streams.get_mut(&attachment) else {
    return;
  };
  stream.ending = true;
  if slates_ipc::stream::end(stream.stream.as_fd()).is_err() {
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
/// at the next start's recovery, as every guest's does.
pub(crate) fn drop_all(s: &mut ShardState) {
  s.shards_streams.clear();
}

impl Daemon {
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
