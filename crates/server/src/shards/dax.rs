//! The DAX half of the Shards route (§4.6; Shards part 3; `docs/wip/shards/dax-protocol.md`, agreed with the Shards owner
//! 2026-10-24). The daemon answers Shards' DESCRIPTOR ask with a descriptor of the asked file's DAX backing, one per
//! attachment and file, filled from the volume through the attachment's own bridge. The descriptor authorizes that one
//! file's bytes and nothing of any other file or volume.
//!
//! The backings, their fill, the ingest of a guest's stores and the refill of a backing the disk changed beneath are
//! the shared backed session (`slates_bridge_core::dax_backing`, whose module says what each does and why); slates' own
//! virtio-fs device drives the same session for its DAX window. This module is the Shards stream's half:
//! - **The ask** arrives as a DESCRIPTOR control frame. Its node, whether it maps to write, and its range from a device
//!   that names one, go to the session; the answer goes back on the stream, its descriptor by `SCM_RIGHTS`.
//! - **The ingest** is asked for by a request on the stream: an `FSYNC`, `FLUSH` or `RELEASE` of a backed file, a `READ`
//!   of it (so a read through the stream never contradicts the guest's own stores), a `FORGET` of it, and the stream's
//!   end. The request is held, unconsumed, while the session writes the guest's stores back. A write-back the volume
//!   refuses is answered: the held `FSYNC`, `FLUSH` or `READ` carries its errno, so the guest's fsync or close fails
//!   rather than claim bytes durable that are not.
//! - **The refill** runs before anything more reaches the guest: a reply that may reveal the disk's change waits for the
//!   map to show it too.
//! - **A slice a turn**, of the shard's copy slice ([`crate::config::DaemonConfig::copy_slice_bytes`]: half the live step
//!   quantum at the measured memcpy rate, in whole pages). The device waits for its answer with its stream held, so no
//!   other frame arrives meanwhile.
//!
//! **Ending.** A backing ends when its stream ends, or when the guest forgets its node: a Linux guest removes a DAX
//! inode's mappings, by waited `FUSE_REMOVEMAPPING` requests, before it queues the `FORGET` (`fs/fuse/inode.c`). Its
//! stores are ingested first, and its charge goes back.
//!
//! Refusals, each counted by kind:
//! - `shards.dax_malformed`, `EINVAL`: an ask of no length the protocol has, or a range past the object;
//! - `shards.dax_unreachable`, the bridge's errno: a node the attachment does not reach;
//! - `shards.dax_not_a_file`: `EISDIR` for a directory, `EINVAL` for anything else that is not a regular file;
//! - `shards.dax_read_only`, `EPERM`;
//! - `shards.dax_budget`, `ENOMEM`;
//! - `shards.dax_object`, `EIO`: the OS refused the object, a view or a fill;
//! - `shards.dax_witness`, the bridge's errno: a writable ask's file could not be copied up;
//! - `shards.dax_busy`, `EBUSY`: an in-process guest maps the file writable.
//!
//! An answered ask counts `shards.dax_answered`; an ingest's written runs `shards.dax_ingested`, and one the bridge
//! refused `shards.dax_ingest_refused`. A refilled backing counts `shards.dax_refilled`; a range the volume refused to
//! read for it `shards.dax_refill_refused` (the object keeps its bytes); a stale range asked writable, which is never
//! expected, `shards.dax_refill_writable`.

use slates_bridge_core::dax_backing::{
  Ask, Counts, DaxBackingSession, Failure, FillStep, Ingested, Refused,
};
use slates_bridge_core::{Bridge, DaxOffer, OpContext, VolumeBridge};
use slates_bridge_fuse::bridge::{dax_failure_errno, dax_refused_errno};
use slates_bridge_fuse::shards_fs::{descriptor_answer, descriptor_ask, descriptor_refusal};
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;

use super::{ShardsStream, owe};
use crate::state::ShardState;

/// Format: the refusal-ledger name of an answered descriptor ask.
const ANSWERED: &str = "shards.dax_answered";
/// Format: the refusal-ledger name of an ask of no length the protocol has, or a range past the object.
const MALFORMED: &str = "shards.dax_malformed";
/// Format: the refusal-ledger name of an ask for a node the attachment does not reach.
const UNREACHABLE: &str = "shards.dax_unreachable";
/// Format: the refusal-ledger name of an ask for a node that is not a regular file.
const NOT_A_FILE: &str = "shards.dax_not_a_file";
/// Format: the refusal-ledger name of a writable ask of an attachment that may not write.
const READ_ONLY: &str = "shards.dax_read_only";
/// Format: the refusal-ledger name of a fill past the shard's fill budget.
const BUDGET: &str = "shards.dax_budget";
/// Format: the refusal-ledger name of an object, a view or a fill the OS refused.
const OBJECT: &str = "shards.dax_object";
/// Format: the refusal-ledger name of a writable ask whose file the bridge refused to copy up first (the disk's file
/// unreadable, or no room for the copy).
const WITNESS: &str = "shards.dax_witness";
/// Format: the refusal-ledger name of an ask for a file an in-process guest maps writable.
const BUSY: &str = "shards.dax_busy";
/// Format: the refusal-ledger name of an ingest the bridge refused (the attachment revoked, its volume gone, no room,
/// or the file unreadable now): that file's stores are not all written, and the request that asked for them answers
/// the bridge's errno.
const INGEST_REFUSED: &str = "shards.dax_ingest_refused";
/// Format: the ledger name of each run of bytes an ingest wrote into the volume.
const INGESTED: &str = "shards.dax_ingested";
/// Format: the ledger name of each backing refilled because the disk changed its file beneath it.
const REFILLED: &str = "shards.dax_refilled";
/// Format: the refusal-ledger name of a range a refill could not read from the volume (the file unreadable now): its
/// object keeps the bytes it had.
const REFILL_REFUSED: &str = "shards.dax_refill_refused";
/// Format: the refusal-ledger name of a range asked writable in a backing marked stale, which a refill passes over
/// rather than lose the guest's stores: never expected, since a file is copied up before any writable ask of it.
const REFILL_WRITABLE: &str = "shards.dax_refill_writable";

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

/// The counter `refused` is recorded under.
fn refused_counter(refused: &Refused) -> &'static str {
  match refused {
    Refused::Malformed => MALFORMED,
    Refused::Unreachable(_) => UNREACHABLE,
    Refused::NotAFile { .. } => NOT_A_FILE,
    Refused::ReadOnly => READ_ONLY,
    Refused::Budget => BUDGET,
    Refused::Object => OBJECT,
    Refused::Witness(_) => WITNESS,
    Refused::Busy => BUSY,
  }
}

/// The DAX offer a stream's requests are answered under (`slates_bridge_core::dax_backing::offer`).
pub(super) fn offer(s: &ShardState) -> DaxOffer {
  slates_bridge_core::dax_backing::offer(&s.store.shards_backings)
}

/// Derived: the bytes one fill, ingest or refill slice copies: the shard's copy slice, in whole pages, at least one
/// ([`crate::config::DaemonConfig::copy_slice_page_bytes`]).
fn slice_bytes(s: &ShardState) -> u64 {
  s.config.copy_slice_page_bytes()
}

/// Adds what `stream`'s session did since it was last asked to the shard's ledger.
fn forward_counts(s: &mut ShardState, stream: &mut ShardsStream) {
  let Counts {
    answered,
    ingested,
    ingest_refused,
    refilled,
    refill_refused,
    refill_writable,
  } = stream.dax.take_counts();
  for (name, count) in [
    (ANSWERED, answered),
    (INGESTED, ingested),
    (INGEST_REFUSED, ingest_refused),
    (REFILLED, refilled),
    (REFILL_REFUSED, refill_refused),
    (REFILL_WRITABLE, refill_writable),
  ] {
    if count > 0 {
      s.count(name, count);
    }
  }
}

/// Runs `f` on `stream`'s session with the attachment's bridge (its scope applied) and context. `None` when the volume
/// or the attachment is gone.
fn with_session<T>(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  f: impl FnOnce(&mut DaxBackingSession, &mut dyn Bridge, &OpContext) -> T,
) -> Option<T> {
  let &handle = s.by_id.get(&stream.volume)?;
  let cx = s.attachments.begin(stream.registry).ok()?;
  let result = {
    let ShardState { store, volumes, .. } = &mut *s;
    match volumes.get_mut(handle) {
      Ok(slot) => {
        let ShardsStream {
          volume,
          scope,
          handles,
          dax,
          ..
        } = stream;
        let mut bridge = VolumeBridge::attached(
          *volume,
          &mut slot.volume,
          store,
          handles,
          slot.host.as_mut().map(|host| host as &mut dyn HostFs),
        );
        let mut scoped = None;
        let served = crate::verbs::scoped_or_whole(&mut bridge, &mut scoped, *scope);
        Some(f(dax, served, &cx))
      }
      Err(_) => None,
    }
  };
  s.attachments.end(stream.registry);
  result
}

/// Takes the DESCRIPTOR ask whose body (after its ask byte) is `body` on `stream`: the session checks it, makes the
/// node's backing if it has none, and leaves its fill pending, run by [`step`]. A refusal is answered and counted at
/// once.
pub(super) fn begin(s: &mut ShardState, stream: &mut ShardsStream, body: &[u8]) {
  let Some(parsed) = descriptor_ask(body) else {
    refuse(s, stream, &Refused::Malformed);
    return;
  };
  let ask = Ask {
    node: parsed.node,
    writable: parsed.writable,
    range: parsed.range,
  };
  let taken = with_session(s, stream, |dax, bridge, cx| dax.ask(bridge, cx, ask))
    .unwrap_or(Err(Refused::Unreachable(VfsError::NotFound)));
  if let Err(refused) = taken {
    refuse(s, stream, &refused);
  }
}

/// One slice of `stream`'s pending fill. When the fill is done the answer is owed, its descriptor with it; a refusal
/// mid-fill is answered and counted. Nothing pending, nothing done.
pub(super) fn step(s: &mut ShardState, stream: &mut ShardsStream) {
  let slice = slice_bytes(s);
  let stepped = with_session(s, stream, |dax, bridge, cx| {
    dax.fill_step(bridge, cx, slice)
  });
  match stepped {
    None => {
      stream.dax.abandon_fill(&mut s.store.shards_backings);
      refuse(s, stream, &Refused::Unreachable(VfsError::NotFound));
    }
    Some(FillStep::Idle | FillStep::Filling) => {}
    Some(FillStep::Done(descriptor)) => {
      owe(stream, &descriptor_answer());
      stream.owed_descriptor = Some(descriptor);
    }
    Some(FillStep::Refused(refused)) => refuse(s, stream, &refused),
  }
  forward_counts(s, stream);
}

/// Answers the ask being taken or filled with `refused`, and counts it.
fn refuse(s: &mut ShardState, stream: &mut ShardsStream, refused: &Refused) {
  owe(stream, &descriptor_refusal(dax_refused_errno(refused)));
  s.count(refused_counter(refused), 1);
}

/// Lets go of `inode`'s backing on `stream`, if it has one, and gives its charge back: the guest forgot the node.
pub(super) fn release(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) {
  stream.dax.release(&mut s.store.shards_backings, inode);
}

/// Lets go of every backing on `stream`, and of its pending fill and ingest: the stream ended.
pub(super) fn release_all(s: &mut ShardState, stream: &mut ShardsStream) {
  stream.dax.release_all(&mut s.store.shards_backings);
}

/// Holds the request of `total` bytes at the head of `stream` for an ingest, where it is one that asks for the guest's
/// stores to reach the volume first: a `FSYNC`, `FLUSH`, `RELEASE` or `READ` of a node with a writable backing, or a
/// `FORGET` or `BATCH_FORGET` of such nodes. Whether it was held.
pub(super) fn hold_for_ingest(
  s: &ShardState,
  stream: &mut ShardsStream,
  request: &[u8],
  total: usize,
) -> bool {
  use slates_bridge_fuse::Opcode;
  use slates_bridge_fuse::bridge::{KernelChange, kernel_changes};
  if stream.dax.is_empty() {
    return false;
  }
  let Ok(header) = slates_bridge_fuse::InHeader::parse(request) else {
    return false;
  };
  let nodes: Vec<u64> = match Opcode::from_wire(header.opcode) {
    Some(Opcode::FSync | Opcode::Flush | Opcode::Release | Opcode::Read) => vec![header.nodeid],
    Some(Opcode::Forget | Opcode::BatchForget) => {
      let mut forgotten = Vec::new();
      kernel_changes(request, &[], &mut |change| {
        if let KernelChange::Forgot { node, .. } = change {
          forgotten.push(node);
        }
      });
      forgotten
    }
    _ => return false,
  };
  let inodes = stream.dax.worth_ingesting(&s.store.shards_backings, nodes);
  stream.dax.begin_ingest(
    inodes,
    slates_bridge_core::dax_backing::Then::Serve(u64::try_from(total).unwrap_or(u64::MAX)),
  )
}

/// Starts the ingest every backing with writable ranges takes before the stream ends. Whether there is one.
pub(super) fn ingest_before_end(s: &ShardState, stream: &mut ShardsStream) -> bool {
  stream.dax.ingest_before_end(&s.store.shards_backings)
}

/// One slice of `stream`'s ingest: the finished ingest when it is done, for the caller to go on with. An inode the
/// bridge refuses (the attachment revoked, its volume gone, or one that may not write) is passed over, counted, its
/// stores unwritten.
pub(super) fn ingest_step(s: &mut ShardState, stream: &mut ShardsStream) -> Option<Done> {
  let slice = slice_bytes(s);
  let done = match with_session(s, stream, |dax, bridge, cx| {
    dax.ingest_step(bridge, cx, slice)
  }) {
    Some(done) => done,
    None => stream.dax.abort_ingest(Failure::Volume(VfsError::NotFound)),
  };
  forward_counts(s, stream);
  let Ingested {
    wrote,
    refused,
    then,
  } = done?;
  Some(Done {
    then: match then {
      slates_bridge_core::dax_backing::Then::Serve(total) => Then::Serve {
        total: usize::try_from(total).unwrap_or(usize::MAX),
      },
      slates_bridge_core::dax_backing::Then::End => Then::End,
    },
    wrote,
    refused: refused.as_ref().map(dax_failure_errno),
  })
}

/// Whether `stream` owes its guest a refill before anything more (the session's `refill_owed`).
pub(super) fn refill_owed(s: &ShardState, stream: &mut ShardsStream) -> bool {
  stream.dax.refill_owed(&s.store.shards_backings)
}

/// One slice of `stream`'s refill. A range the volume refuses to read now keeps the bytes it had, counted: the map
/// cannot answer an error, and the guest learns of the refusal from its next request of the file, which the volume
/// refuses too.
pub(super) fn refill_step(s: &mut ShardState, stream: &mut ShardsStream) {
  let slice = slice_bytes(s);
  if with_session(s, stream, |dax, bridge, cx| {
    dax.refill_step(bridge, cx, slice);
  })
  .is_none()
  {
    stream.dax.abort_refill();
  }
  forward_counts(s, stream);
}
