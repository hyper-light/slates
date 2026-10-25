//! The DAX half of the Shards route (§4.6; Shards part 3; `docs/wip/shards/dax-protocol.md`, agreed with the Shards owner
//! 2026-10-24). The daemon answers Shards' DESCRIPTOR ask with a descriptor of the asked file's DAX object
//! (`slates_mem::DaxObject`), one per attachment and file, filled from the volume through the attachment's own bridge.
//! The descriptor authorizes that one file's bytes and nothing of any other file or volume. A descriptor of slates'
//! content itself would carry the whole content arena, every volume's (A-135). The objects are kept by the shard's
//! store (`slates_vfs::shards_backing`), so every change of a backed file reaches them at the volume's write boundary
//! (step 4); a stream holds their handles.
//!
//! **Answering an ask** (step 2). The ask names a node, whether the mapping is to be written, and, from a device that
//! names it, the mapping's range.
//! - The node is resolved and its attributes read through the attachment's bridge, so its scope and rights apply as
//!   to any request. A node it does not reach, or one that is not a regular file, is refused.
//! - A writable ask of an attachment that may not write is refused `EPERM`, as its writes are. A mapping the guest
//!   wrote would otherwise take stores that never reach the volume.
//! - A file an in-process guest maps writable is refused `EBUSY`: those stores go to the file's block past the write
//!   boundary, where no object could follow them (`Volume::dax_map_write` refuses the other way round).
//! - A writable ask's file is copied up first (§4.6 DAX rights), as its first write would be. Beneath an overlay an
//!   untouched file follows the disk (D-25), and an ingest comparing the guest's map against it would take each
//!   outsider's edit for the guest's store. Copied up, the volume serves its witness or refuses `BaseDrift`.
//! - The node's object, made on its first ask, has the asked ranges filled from the volume through the same bridge, a
//!   slice at a time (below). Holes are skipped (SEEK_DATA), and a page the volume holds as zeros is not copied, since
//!   an untouched page already reads zero, so a hole commits no memory.
//! - A range is marked filled when its fill starts, so a change written through meanwhile lands on it and the rest of
//!   the fill reads the volume as changed.
//! - Then a new descriptor of the object goes back with the answer, by `SCM_RIGHTS`. Every ask of the node is answered
//!   with the same object. That keeps every range of the file coherent in Shards' window, which maps each range from
//!   its own ask's descriptor.
//!
//! **Ingest** (step 3). The guest's stores through its map reach the volume when it asks for them to:
//! - an `FSYNC`, `FLUSH` or `RELEASE` of the file;
//! - a `READ` of it, so a read through the stream never contradicts the guest's own stores;
//! - a `FORGET` of it;
//! - and the stream's end.
//!
//! The request is held, unconsumed, while the object is compared with the volume over the ranges asked writable, up to
//! the file's end. Each run that differs is written through the attachment's bridge. The backing is suppressed in the
//! store while its own stores are written, so they are not written back into it over a newer store. The change is
//! published before the held request's reply. A write-back the volume refuses (no room, a drifted file) is answered:
//! the held `FSYNC`, `FLUSH` or `READ` carries its errno, so the guest's fsync or close fails rather than claim bytes
//! durable that are not. A read-only attachment is never ingested.
//!
//! **Refill** (step 4). Every change through slates reaches the objects at the write boundary. An untouched file of an
//! overlay also changes when the disk does, and the base plane marks its backings stale where it takes such a change.
//! Before anything more reaches the guest, the stream refills the stale objects' filled ranges from the volume, a copy
//! slice a turn: a reply that may reveal the change (a new size) waits for the map to show it too. Ranges asked writable
//! are never refilled, since their files are copied up before such an ask and so never follow the disk.
//!
//! **A slice a turn.** A fill or an ingest copies user-scaled bytes, so it runs in cooperative slices of the shard's
//! copy slice ([`crate::config::DaemonConfig::copy_slice_bytes`]: half the live step quantum at the measured memcpy rate),
//! one per turn of the stream's task. The device waits for its answer with its stream held, so no other frame arrives
//! meanwhile.
//!
//! **Bounds, derived (R3).** A shard's backings count against its fill budget: its content reserve
//! ([`crate::config::DaemonConfig::shard_content_layout`]), one copy of everything the shard can hold, the cost the
//! design measured for a copy (slope 1.0 over mapped bytes). A backing counts a whole range for each range it has
//! filled, and at least one: what write-through can touch, so it never commits memory the budget has not counted. A
//! fill past the budget is refused `ENOMEM`, counted, and files stop being offered DAX before that ([`offer`]).
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

use std::collections::{BTreeMap, VecDeque};

use slates_bridge_core::{Bridge, DaxOffer, ObjectId, OpContext, VolumeBridge};
use slates_bridge_fuse::bridge::errno;
use slates_bridge_fuse::shards_fs::{
  DescriptorAsk, descriptor_answer, descriptor_ask, descriptor_refusal,
};
use slates_mem::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES, DaxObject, Handle};
use slates_vfs::host::HostFs;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::Kind;
use slates_vfs::shards_backing::ShardsBacking;

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

/// Format: the FUSE node id that names the attachment's root (`FUSE_ROOT_ID`).
const ROOT_NODE: u64 = 1;
/// Format: Linux's `EPERM`, which the device returns to the guest's mapping.
const EPERM: i32 = 1;
/// Format: Linux's `ENOENT`.
const ENOENT: i32 = 2;
/// Format: Linux's `EIO`.
const EIO: i32 = 5;
/// Format: Linux's `ENOMEM`.
const ENOMEM: i32 = 12;
/// Format: Linux's `EBUSY`.
const EBUSY: i32 = 16;
/// Format: Linux's `EISDIR`.
const EISDIR: i32 = 21;
/// Format: Linux's `EINVAL`.
const EINVAL: i32 = 22;

/// An attachment's backings, by inode: handles into the shard store's registry.
pub(crate) type Backings = BTreeMap<u64, Handle<ShardsBacking>>;

/// A descriptor ask being answered: its fill, a slice a turn.
pub(crate) struct Pending {
  /// The ask.
  ask: DescriptorAsk,
  /// The inode it resolved to.
  inode: u64,
  /// The file's size when the ask was taken: the fill stops at it.
  size: u64,
  /// The range being filled, by index, and the last the fill covers.
  next: u64,
  last: u64,
  /// How far into the range being filled the fill has got, in bytes.
  progress: u64,
  /// What the range being filled was charged when its fill began, given back if the fill fails.
  range_charge: u64,
}

/// Why an ask was refused: the errno the device passes to the guest, and the counter it is recorded under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
  /// An ask of no length the protocol has, or a range past the object.
  Malformed,
  /// A node the attachment does not reach: the bridge's errno.
  Unreachable(i32),
  /// A node that is not a regular file: `EISDIR` or `EINVAL`.
  NotAFile(i32),
  /// A writable ask of an attachment that may not write.
  ReadOnly,
  /// A fill past the shard's fill budget.
  Budget,
  /// An object, a view or a fill the OS refused.
  Object,
  /// A writable ask whose file the bridge refused to copy up first: the bridge's errno.
  Witness(i32),
  /// A file an in-process guest maps writable.
  Busy,
}

impl Refused {
  /// The errno the device returns to the guest's mapping.
  fn errno(self) -> i32 {
    match self {
      Refused::Malformed => EINVAL,
      Refused::Unreachable(errno) | Refused::NotAFile(errno) | Refused::Witness(errno) => errno,
      Refused::ReadOnly => EPERM,
      Refused::Budget => ENOMEM,
      Refused::Object => EIO,
      Refused::Busy => EBUSY,
    }
  }

  /// The counter it is recorded under.
  fn counter(self) -> &'static str {
    match self {
      Refused::Malformed => MALFORMED,
      Refused::Unreachable(_) => UNREACHABLE,
      Refused::NotAFile(_) => NOT_A_FILE,
      Refused::ReadOnly => READ_ONLY,
      Refused::Budget => BUDGET,
      Refused::Object => OBJECT,
      Refused::Witness(_) => WITNESS,
      Refused::Busy => BUSY,
    }
  }
}

/// Derived: the bytes a shard's DAX backings may count: its content reserve (`shard_content_layout().arena`), one copy
/// of everything the shard can hold.
fn fill_budget(s: &ShardState) -> u64 {
  u64::try_from(s.config.shard_content_layout().arena.1).unwrap_or(u64::MAX)
}

/// Derived: the views of its object a backing may hold at once: the fill budget in ranges, the most it can fill.
fn most_views(s: &ShardState) -> usize {
  usize::try_from(fill_budget(s).checked_div(DAX_RANGE_BYTES).unwrap_or(0)).unwrap_or(usize::MAX)
}

/// What a backing counts against the fill budget: a range for each range it has filled, at least one.
fn charge_of(backing: &ShardsBacking) -> u64 {
  u64::try_from(backing.filled().len().max(1))
    .unwrap_or(u64::MAX)
    .saturating_mul(DAX_RANGE_BYTES)
}

/// The DAX offer a stream's requests are answered under (`slates_bridge_core::DaxOffer`): per-inode DAX for regular files
/// of a range or more, marked while the shard's fill budget has room for another backing (the one range each counts at
/// least), so files stop being offered DAX before a fill would be refused.
pub(super) fn offer(s: &ShardState) -> DaxOffer {
  DaxOffer::PerInode {
    smallest: DAX_RANGE_BYTES,
    new_files: s.dax_filled.saturating_add(DAX_RANGE_BYTES) <= fill_budget(s),
  }
}

/// Counts `bytes` more against the shard's fill budget, or refuses with nothing counted.
fn charge(s: &mut ShardState, bytes: u64) -> Result<(), Refused> {
  let budget = fill_budget(s);
  let after = s
    .dax_filled
    .checked_add(bytes)
    .filter(|after| *after <= budget)
    .ok_or(Refused::Budget)?;
  s.dax_filled = after;
  Ok(())
}

/// Gives `bytes` back to the shard's fill budget.
fn uncharge(s: &mut ShardState, bytes: u64) {
  s.dax_filled = s.dax_filled.saturating_sub(bytes);
}

/// The store's backing behind `stream`'s handle for `inode`, if both are live.
fn backing<'s>(
  s: &'s mut ShardState,
  stream: &ShardsStream,
  inode: u64,
) -> Option<&'s mut ShardsBacking> {
  let handle = *stream.backings.get(&inode)?;
  s.store.shards_backings.get_mut(handle)
}

/// Takes the DESCRIPTOR ask whose body (after its ask byte) is `body` on `stream`: checks it, makes the node's object if
/// it has none, and leaves its fill pending, run by [`step`]. A refusal is answered and counted at once.
pub(super) fn begin(s: &mut ShardState, stream: &mut ShardsStream, body: &[u8]) {
  if let Err(refused) = taken(s, stream, body) {
    refuse(s, stream, refused);
  }
}

/// [`begin`]'s work: the pending fill set, or the refusal.
fn taken(s: &mut ShardState, stream: &mut ShardsStream, body: &[u8]) -> Result<(), Refused> {
  let ask = descriptor_ask(body).ok_or(Refused::Malformed)?;
  if let Some((offset, len)) = ask.range
    && offset.saturating_add(len) > DAX_OBJECT_BYTES
  {
    return Err(Refused::Malformed);
  }
  let (inode, size) = checked(s, stream, ask)?;
  // An in-process guest storing into the file's own block bypasses the write boundary, so no object of the file could
  // be kept equal to the volume: the two routes exclude each other, refused before any effect (`Volume::dax_map_write`
  // refuses the other way).
  if s.store.dax.has_writable_of(InodeNo(inode)) {
    return Err(Refused::Busy);
  }
  // A writable map's file is copied up first (§4.6 DAX rights), as its first write would be: what the volume serves
  // for it then changes only through the write boundary, so a write-back never takes an outsider's change for the
  // guest's. Its size is the witnessed file's.
  let size = if ask.writable {
    witnessed(s, stream, inode)?
  } else {
    size
  };
  if let std::collections::btree_map::Entry::Vacant(vacant) = stream.backings.entry(inode) {
    // The floor every backing counts: one range, so the objects a device can make are bounded.
    charge(s, DAX_RANGE_BYTES)?;
    let made = DaxObject::create(most_views(s))
      .map_err(|_| Refused::Object)
      .and_then(|object| {
        s.store
          .shards_backings
          .insert(ShardsBacking::new(object, InodeNo(inode)))
          .map_err(|_| Refused::Object)
      });
    match made {
      Ok(handle) => {
        vacant.insert(handle);
      }
      Err(refused) => {
        uncharge(s, DAX_RANGE_BYTES);
        return Err(refused);
      }
    }
  }
  let (next, last) = covered(ask.range, size);
  stream.pending = Some(Pending {
    ask,
    inode,
    size,
    next,
    last,
    progress: 0,
    range_charge: 0,
  });
  Ok(())
}

/// The ranges a fill covers, by index, first and last: the asked range's, or the whole file's, within the file. A first
/// past the last covers nothing.
fn covered(range: Option<(u64, u64)>, size: u64) -> (u64, u64) {
  let (from, to) = match range {
    Some((offset, len)) => (offset, offset.saturating_add(len).min(size)),
    None => (0, size),
  };
  if to <= from {
    return (1, 0);
  }
  let first = from.checked_div(DAX_RANGE_BYTES).unwrap_or(0);
  let last = to
    .saturating_sub(1)
    .checked_div(DAX_RANGE_BYTES)
    .unwrap_or(0);
  (first, last)
}

/// Resolves the ask's node through the attachment's bridge and checks it may be backed: its inode and size.
fn checked(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  ask: DescriptorAsk,
) -> Result<(u64, u64), Refused> {
  let (inode, size, may_write) = with_bridge(s, stream, |bridge, cx| {
    let inode = if ask.node == ROOT_NODE {
      bridge
        .root(cx)
        .map_err(|e| Refused::Unreachable(errno(e)))?
    } else {
      ask.node
    };
    let attr = bridge
      .getattr(ObjectId::new(inode, 0), cx)
      .map_err(|e| Refused::Unreachable(errno(e)))?;
    match attr.kind {
      Kind::File => {}
      Kind::Dir => return Err(Refused::NotAFile(EISDIR)),
      _ => return Err(Refused::NotAFile(EINVAL)),
    }
    Ok((inode, attr.size, cx.rights.write))
  })?;
  if ask.writable && !may_write {
    return Err(Refused::ReadOnly);
  }
  Ok((inode, size))
}

/// Copies file `inode` up through the attachment's bridge before a writable map of it is filled: its size afterwards, the
/// witnessed file's.
fn witnessed(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) -> Result<u64, Refused> {
  with_bridge(s, stream, |bridge, cx| {
    let object = ObjectId::new(inode, 0);
    bridge
      .witness_for_writable_map(object, cx)
      .map_err(|e| Refused::Witness(errno(e)))?;
    bridge
      .getattr(object, cx)
      .map(|attr| attr.size)
      .map_err(|e| Refused::Unreachable(errno(e)))
  })
}

/// Runs `f` with the attachment's bridge (its scope applied) and context: the volume or the attachment gone is the
/// node unreachable.
fn with_bridge<T>(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  f: impl FnOnce(&mut dyn Bridge, &OpContext) -> Result<T, Refused>,
) -> Result<T, Refused> {
  let Some(&handle) = s.by_id.get(&stream.volume) else {
    return Err(Refused::Unreachable(ENOENT));
  };
  let Ok(cx) = s.attachments.begin(stream.registry) else {
    return Err(Refused::Unreachable(ENOENT));
  };
  let result = {
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
        f(served, &cx)
      }
      Err(_) => Err(Refused::Unreachable(ENOENT)),
    }
  };
  s.attachments.end(stream.registry);
  result
}

/// One slice of `stream`'s pending fill. When the fill is done the answer is owed, its descriptor with it; a refusal
/// mid-fill is answered and counted. Nothing pending, nothing done.
pub(super) fn step(s: &mut ShardState, stream: &mut ShardsStream) {
  match stepped(s, stream) {
    Ok(false) => {}
    Ok(true) => {
      if let Err(refused) = answered(s, stream) {
        refuse(s, stream, refused);
      }
    }
    Err(refused) => refuse(s, stream, refused),
  }
}

/// [`step`]'s work: whether the fill is done.
fn stepped(s: &mut ShardState, stream: &mut ShardsStream) -> Result<bool, Refused> {
  let Some(mut pending) = stream.pending.take() else {
    return Ok(false);
  };
  // Ranges filled already (by an earlier ask, the floor's first range or another) are passed over.
  while pending.progress == 0
    && pending.next <= pending.last
    && backing(s, stream, pending.inode).is_some_and(|b| b.filled().contains(&pending.next))
  {
    pending.next = pending.next.saturating_add(1);
  }
  if pending.next > pending.last {
    stream.pending = Some(pending);
    return Ok(true);
  }
  if pending.progress == 0 {
    // The range's fill begins: it is counted, then marked filled, so write-through reaches it from now on.
    let target = backing(s, stream, pending.inode).ok_or(Refused::Object)?;
    let before = charge_of(target);
    target.mark_filled(pending.next);
    let after = charge_of(target);
    let delta = after.saturating_sub(before);
    if let Err(refused) = charge(s, delta) {
      if let Some(target) = backing(s, stream, pending.inode) {
        target.unmark_filled(pending.next);
      }
      return Err(refused);
    }
    pending.range_charge = delta;
  }
  match fill_slice(s, stream, &mut pending) {
    Ok(()) => {
      stream.pending = Some(pending);
      Ok(false)
    }
    Err(refused) => {
      // A fill that failed part-way leaves the range unfilled and uncounted; the next ask fills it again.
      if let Some(target) = backing(s, stream, pending.inode) {
        target.unmark_filled(pending.next);
      }
      uncharge(s, pending.range_charge);
      Err(refused)
    }
  }
}

/// One slice of the range the pending fill is on: read from the volume (holes skipped), stored into the object (pages
/// of zeros skipped), and the range finished when it is done.
fn fill_slice(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  pending: &mut Pending,
) -> Result<(), Refused> {
  let range_start = pending.next.saturating_mul(DAX_RANGE_BYTES);
  let range_bytes = pending
    .size
    .saturating_sub(range_start)
    .min(DAX_RANGE_BYTES);
  let at = range_start.saturating_add(pending.progress);
  let want = range_bytes
    .saturating_sub(pending.progress)
    .min(slice_bytes(s));
  let inode = pending.inode;
  let read = with_bridge(s, stream, |bridge, cx| {
    let object = ObjectId::new(inode, 0);
    // A hole needs no fill: skip to the range's next data, if any is before its end.
    let data = bridge
      .seek(object, cx, at, true)
      .map_err(|_| Refused::Object)?;
    let range_end = range_start.saturating_add(range_bytes);
    match data {
      Some(data) if data < range_end => {
        let mut bytes = Vec::new();
        let size = u32::try_from(want.min(range_end.saturating_sub(data))).unwrap_or(u32::MAX);
        bridge
          .read(object, cx, data, size, &mut bytes)
          .map_err(|_| Refused::Object)?;
        Ok(Some((data, bytes)))
      }
      _ => Ok(None),
    }
  })?;
  match read {
    Some((data, bytes)) if !bytes.is_empty() => {
      store_pages(s, stream, inode, data, &bytes)?;
      pending.progress = data
        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .saturating_sub(range_start);
      if pending.progress >= range_bytes {
        finish_range(pending);
      }
    }
    // No more data in the range, or the file ended early: the rest of the range reads zeros already.
    _ => finish_range(pending),
  }
  Ok(())
}

/// Moves the pending fill to its next range (the one done stays marked filled).
fn finish_range(pending: &mut Pending) {
  pending.next = pending.next.saturating_add(1);
  pending.progress = 0;
  pending.range_charge = 0;
}

/// Derived: the bytes one fill or ingest slice copies: the shard's copy slice, in whole pages, at least one.
fn slice_bytes(s: &ShardState) -> u64 {
  let page = u64::try_from(rustix::param::page_size().max(1)).unwrap_or(u64::MAX);
  let slice = s.config.copy_slice_bytes();
  slice
    .checked_div(page)
    .and_then(|pages| pages.checked_mul(page))
    .unwrap_or(page)
    .max(page)
}

/// Stores the pages of `bytes`, the file's from `at`, into `inode`'s object, skipping pages of zeros: an untouched page
/// reads zero already, so a page of zeros needs no memory.
fn store_pages(
  s: &mut ShardState,
  stream: &ShardsStream,
  inode: u64,
  at: u64,
  bytes: &[u8],
) -> Result<(), Refused> {
  let page = rustix::param::page_size().max(1);
  let page_bytes = u64::try_from(page).unwrap_or(u64::MAX);
  let target = backing(s, stream, inode).ok_or(Refused::Object)?;
  let mut offset = at;
  for chunk in bytes.chunks(page) {
    if !chunk.iter().all(|byte| *byte == 0) {
      target
        .object_mut()
        .store(offset, chunk)
        .map_err(|_| Refused::Object)?;
    }
    offset = offset.saturating_add(page_bytes);
  }
  Ok(())
}

/// The fill is done: marks the ranges asked writable, and owes the answer with a new descriptor of the object.
fn answered(s: &mut ShardState, stream: &mut ShardsStream) -> Result<(), Refused> {
  let pending = stream.pending.take().ok_or(Refused::Object)?;
  let target = backing(s, stream, pending.inode).ok_or(Refused::Object)?;
  if pending.ask.writable {
    let (first, last) = covered(pending.ask.range, pending.size);
    let mut range = first;
    while range <= last {
      target.mark_writable(range);
      range = range.saturating_add(1);
    }
  }
  let descriptor = rustix::io::fcntl_dupfd_cloexec(target.object().descriptor(), 0)
    .map_err(|_| Refused::Object)?;
  owe(stream, &descriptor_answer());
  stream.owed_descriptor = Some(descriptor);
  s.count(ANSWERED, 1);
  Ok(())
}

/// Answers the pending ask, or the one being taken, with `refused`, and counts it; the pending fill ends.
fn refuse(s: &mut ShardState, stream: &mut ShardsStream, refused: Refused) {
  stream.pending = None;
  owe(stream, &descriptor_refusal(refused.errno()));
  s.count(refused.counter(), 1);
}

/// Lets go of `inode`'s backing on `stream`, if it has one, and gives its charge back: the guest forgot the node.
pub(super) fn release(s: &mut ShardState, stream: &mut ShardsStream, inode: u64) {
  if let Some(handle) = stream.backings.remove(&inode)
    && let Some(gone) = s.store.shards_backings.remove(handle)
  {
    uncharge(s, charge_of(&gone));
  }
}

/// Lets go of every backing on `stream`, and of its pending fill and ingest: the stream ended.
pub(super) fn release_all(s: &mut ShardState, stream: &mut ShardsStream) {
  stream.pending = None;
  stream.ingest = None;
  stream.refill = None;
  let handles: Vec<Handle<ShardsBacking>> = stream.backings.values().copied().collect();
  stream.backings.clear();
  for handle in handles {
    if let Some(gone) = s.store.shards_backings.remove(handle) {
      uncharge(s, charge_of(&gone));
    }
  }
}

/// An ingest under way (Shards part 3, step 3): the guest's stores through its maps of some of the stream's objects,
/// compared with the volume over the ranges asked writable and written back where they differ, a slice a turn, before
/// what asked for it goes on.
pub(crate) struct Ingest {
  /// The inodes still to ingest, the current one first.
  inodes: VecDeque<u64>,
  /// The current inode's ranges asked writable, as they stood when its ingest began.
  ranges: Vec<u64>,
  /// Which of them is being compared.
  index: usize,
  /// How far into it, in bytes.
  progress: u64,
  /// The current file's size when its ingest began: nothing past its end is written.
  size: u64,
  /// Whether the current inode's ingest has begun (its ranges and size taken).
  begun: bool,
  /// Whether anything was written: the change is then published before what asked for it goes on.
  pub(super) wrote: bool,
  /// The errno of the first write-back the bridge refused, which the request that asked for it answers.
  pub(super) refused: Option<i32>,
  /// What asked for it.
  pub(super) then: Then,
  /// The object's and the volume's bytes of the slice being compared: kept from one slice to the next, so a slice
  /// allocates nothing (each at most one slice long).
  stored: Vec<u8>,
  held: Vec<u8>,
}

/// What an ingest was for.
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

/// The inodes of `nodes` whose backings hold ranges asked writable: what an ingest for them compares.
fn worth_ingesting(
  s: &ShardState,
  stream: &ShardsStream,
  nodes: impl IntoIterator<Item = u64>,
) -> VecDeque<u64> {
  let mut inodes: VecDeque<u64> = VecDeque::new();
  for node in nodes {
    let has_writable = stream
      .backings
      .get(&node)
      .and_then(|handle| s.store.shards_backings.get(*handle))
      .is_some_and(|backing| !backing.writable().is_empty());
    if has_writable && !inodes.contains(&node) {
      inodes.push_back(node);
    }
  }
  inodes
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
  if stream.backings.is_empty() {
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
  let inodes = worth_ingesting(s, stream, nodes);
  if inodes.is_empty() {
    return false;
  }
  stream.ingest = Some(new_ingest(inodes, Then::Serve { total }));
  true
}

/// Starts the ingest every backing with writable ranges takes before the stream ends. Whether there is one.
pub(super) fn ingest_before_end(s: &ShardState, stream: &mut ShardsStream) -> bool {
  let nodes: Vec<u64> = stream.backings.keys().copied().collect();
  let inodes = worth_ingesting(s, stream, nodes);
  if inodes.is_empty() {
    return false;
  }
  stream.ingest = Some(new_ingest(inodes, Then::End));
  true
}

fn new_ingest(inodes: VecDeque<u64>, then: Then) -> Ingest {
  Ingest {
    inodes,
    ranges: Vec::new(),
    index: 0,
    progress: 0,
    size: 0,
    begun: false,
    wrote: false,
    refused: None,
    then,
    stored: Vec::new(),
    held: Vec::new(),
  }
}

/// One slice of `stream`'s ingest: whether it is done (taken off the stream then, for the caller to go on with). An
/// inode the bridge refuses (the attachment revoked, its volume gone, or one that may not write) is passed over,
/// counted, its stores unwritten.
pub(super) fn ingest_step(s: &mut ShardState, stream: &mut ShardsStream) -> Option<Ingest> {
  let mut ingest = stream.ingest.take()?;
  loop {
    let Some(&inode) = ingest.inodes.front() else {
      return Some(ingest);
    };
    if !ingest.begun {
      match begin_inode(s, stream, inode) {
        Ok((ranges, size)) => {
          ingest.ranges = ranges;
          ingest.size = size;
          ingest.index = 0;
          ingest.progress = 0;
          ingest.begun = true;
        }
        Err(errno) => {
          s.count(INGEST_REFUSED, 1);
          ingest.refused.get_or_insert(errno);
          next_inode(&mut ingest);
          continue;
        }
      }
    }
    let Some(&range) = ingest.ranges.get(ingest.index) else {
      next_inode(&mut ingest);
      continue;
    };
    let range_start = range.saturating_mul(DAX_RANGE_BYTES);
    let range_bytes = ingest.size.saturating_sub(range_start).min(DAX_RANGE_BYTES);
    if ingest.progress >= range_bytes {
      ingest.index = ingest.index.saturating_add(1);
      ingest.progress = 0;
      continue;
    }
    let at = range_start.saturating_add(ingest.progress);
    let want = range_bytes
      .saturating_sub(ingest.progress)
      .min(slice_bytes(s));
    let (mut stored, mut held) = (
      std::mem::take(&mut ingest.stored),
      std::mem::take(&mut ingest.held),
    );
    let compared = compare_and_write(s, stream, inode, at, want, &mut stored, &mut held);
    (ingest.stored, ingest.held) = (stored, held);
    match compared {
      Ok(wrote) => {
        ingest.wrote |= wrote;
        ingest.progress = ingest.progress.saturating_add(want);
      }
      Err(errno) => {
        s.count(INGEST_REFUSED, 1);
        ingest.refused.get_or_insert(errno);
        next_inode(&mut ingest);
        continue;
      }
    }
    stream.ingest = Some(ingest);
    return None;
  }
}

/// Moves the ingest on to its next inode.
fn next_inode(ingest: &mut Ingest) {
  ingest.inodes.pop_front();
  ingest.begun = false;
  ingest.ranges.clear();
}

/// Takes `inode`'s ingest: its ranges asked writable and the file's size now. Refused, with the errno, where the
/// attachment may not write, or the bridge refuses the node.
fn begin_inode(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
) -> Result<(Vec<u64>, u64), i32> {
  let ranges: Vec<u64> = backing(s, stream, inode)
    .map(|target| target.writable().iter().copied().collect())
    .unwrap_or_default();
  let size = with_bridge(s, stream, |bridge, cx| {
    if !cx.rights.write {
      return Err(Refused::ReadOnly);
    }
    bridge
      .getattr(ObjectId::new(inode, 0), cx)
      .map(|attr| attr.size)
      .map_err(|e| Refused::Unreachable(errno(e)))
  })
  .map_err(Refused::errno)?;
  Ok((ranges, size))
}

/// Compares `len` bytes of `inode`'s object from `at` with the volume's, and writes each run that differs into the
/// volume through the attachment's bridge: whether anything was written, or the errno the bridge refused with. The
/// backing is suppressed in the store while its stores are written, so the write boundary does not copy them back into
/// it over a store the guest made since.
fn compare_and_write(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  at: u64,
  len: u64,
  stored: &mut Vec<u8>,
  held: &mut Vec<u8>,
) -> Result<bool, i32> {
  let handle = *stream.backings.get(&inode).ok_or(EIO)?;
  stored.clear();
  stored.resize(usize::try_from(len).map_err(|_| EINVAL)?, 0);
  s.store
    .shards_backings
    .get_mut(handle)
    .ok_or(EIO)?
    .object_mut()
    .load(at, stored)
    .map_err(|_| EIO)?;
  s.store.shards_backings.suppress(Some(handle));
  let runs = with_bridge(s, stream, |bridge, cx| {
    let object = ObjectId::new(inode, 0);
    held.clear();
    bridge
      .read(object, cx, at, u32::try_from(len).unwrap_or(u32::MAX), held)
      .map_err(|e| Refused::Unreachable(errno(e)))?;
    // A volume shorter than the object's span here (the file cut since) is compared as far as it reaches.
    let runs = differing_runs(stored, held);
    for &(from, to) in &runs {
      let bytes = stored.get(from..to).ok_or(Refused::Object)?;
      let offset = at.saturating_add(u64::try_from(from).unwrap_or(u64::MAX));
      bridge
        .write(object, cx, offset, bytes)
        .map_err(|e| Refused::Unreachable(errno(e)))?;
    }
    Ok(runs.len())
  });
  s.store.shards_backings.suppress(None);
  let runs = runs.map_err(Refused::errno)?;
  if runs > 0 {
    s.count(INGESTED, u64::try_from(runs).unwrap_or(u64::MAX));
  }
  Ok(runs > 0)
}

/// A refill under way (Shards part 3, step 4): the objects of the stream's stale backings copied again from the volume
/// over the ranges they have filled, a slice a turn, before anything more reaches the guest. The disk changed their
/// files without a write (an untouched file of an overlay follows the disk, D-25), so write-through never saw it.
pub(crate) struct Refill {
  /// The inodes still to refill, the current one first.
  inodes: VecDeque<u64>,
  /// The current inode's filled ranges, as they stood when its refill began, less those asked writable.
  ranges: Vec<u64>,
  /// Which of them is being refilled.
  index: usize,
  /// How far into it, in bytes.
  progress: u64,
  /// Whether the current inode's refill has begun (its ranges taken).
  begun: bool,
  /// The volume's and the object's bytes of the slice being refilled: kept from one slice to the next, so a slice
  /// allocates nothing (each at most one slice long).
  fresh: Vec<u8>,
  held: Vec<u8>,
}

/// Whether `stream` owes its guest a refill before anything more: one under way, or a backing of its marked stale since
/// it last looked. The store's count of marks says whether to look at all, so a stream whose files the disk leaves
/// alone pays one comparison a turn.
pub(super) fn refill_owed(s: &ShardState, stream: &mut ShardsStream) -> bool {
  if stream.refill.is_some() {
    return true;
  }
  let marks = s.store.shards_backings.marked_stale();
  if marks == stream.stale_seen {
    return false;
  }
  let stale = stream.backings.values().any(|handle| {
    s.store
      .shards_backings
      .get(*handle)
      .is_some_and(ShardsBacking::stale)
  });
  if !stale {
    stream.stale_seen = marks;
  }
  stale
}

/// One slice of `stream`'s refill, which begins by taking the stale marks of its backings when none is under way. A range
/// the volume refuses to read now keeps the bytes it had, counted: the map cannot answer an error, and the guest learns
/// of the refusal from its next request of the file, which the volume refuses too.
pub(super) fn refill_step(s: &mut ShardState, stream: &mut ShardsStream) {
  let mut refill = match stream.refill.take() {
    Some(refill) => refill,
    None => begin_refill(s, stream),
  };
  loop {
    let Some(&inode) = refill.inodes.front() else {
      return;
    };
    if !refill.begun {
      refill.ranges = refill_ranges(s, stream, inode);
      refill.index = 0;
      refill.progress = 0;
      refill.begun = true;
      s.count(REFILLED, 1);
    }
    let Some(&range) = refill.ranges.get(refill.index) else {
      refill.inodes.pop_front();
      refill.begun = false;
      continue;
    };
    if refill.progress >= DAX_RANGE_BYTES {
      refill.index = refill.index.saturating_add(1);
      refill.progress = 0;
      continue;
    }
    let at = range
      .saturating_mul(DAX_RANGE_BYTES)
      .saturating_add(refill.progress);
    let want = DAX_RANGE_BYTES
      .saturating_sub(refill.progress)
      .min(slice_bytes(s));
    let (mut fresh, mut held) = (
      std::mem::take(&mut refill.fresh),
      std::mem::take(&mut refill.held),
    );
    let refilled = refill_slice(s, stream, inode, at, want, &mut fresh, &mut held);
    (refill.fresh, refill.held) = (fresh, held);
    if refilled.is_ok() {
      refill.progress = refill.progress.saturating_add(want);
    } else {
      s.count(REFILL_REFUSED, 1);
      refill.index = refill.index.saturating_add(1);
      refill.progress = 0;
    }
    stream.refill = Some(refill);
    return;
  }
}

/// Takes the stale marks of `stream`'s backings: the refill of each that had one.
fn begin_refill(s: &mut ShardState, stream: &mut ShardsStream) -> Refill {
  stream.stale_seen = s.store.shards_backings.marked_stale();
  let mut inodes = VecDeque::new();
  for (&inode, &handle) in &stream.backings {
    if s
      .store
      .shards_backings
      .get_mut(handle)
      .is_some_and(ShardsBacking::take_stale)
    {
      inodes.push_back(inode);
    }
  }
  Refill {
    inodes,
    ranges: Vec::new(),
    index: 0,
    progress: 0,
    begun: false,
    fresh: Vec::new(),
    held: Vec::new(),
  }
}

/// The ranges of `inode`'s backing a refill copies again: those filled, less those asked writable, whose guest's
/// stores a refill would lose; any of those is counted, since a file is copied up before a writable ask of it.
fn refill_ranges(s: &mut ShardState, stream: &ShardsStream, inode: u64) -> Vec<u64> {
  let Some(target) = backing(s, stream, inode) else {
    return Vec::new();
  };
  let (ranges, writable): (Vec<u64>, Vec<u64>) = target
    .filled()
    .iter()
    .partition(|range| !target.writable().contains(range));
  if !writable.is_empty() {
    s.count(
      REFILL_WRITABLE,
      u64::try_from(writable.len()).unwrap_or(u64::MAX),
    );
  }
  ranges
}

/// Refills `len` bytes of `inode`'s object from `at` with the volume's. What lies past the file's end reads zero, and
/// only the pages that differ are written: a page of zeros by letting it go, so a hole commits no memory.
fn refill_slice(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  at: u64,
  len: u64,
  fresh: &mut Vec<u8>,
  held: &mut Vec<u8>,
) -> Result<(), i32> {
  let handle = *stream.backings.get(&inode).ok_or(EIO)?;
  let span = usize::try_from(len).map_err(|_| EINVAL)?;
  fresh.clear();
  with_bridge(s, stream, |bridge, cx| {
    bridge
      .read(
        ObjectId::new(inode, 0),
        cx,
        at,
        u32::try_from(len).unwrap_or(u32::MAX),
        fresh,
      )
      .map_err(|e| Refused::Unreachable(errno(e)))
  })
  .map_err(Refused::errno)?;
  fresh.resize(span, 0);
  held.clear();
  held.resize(span, 0);
  let object = s
    .store
    .shards_backings
    .get_mut(handle)
    .ok_or(EIO)?
    .object_mut();
  object.load(at, held).map_err(|_| EIO)?;
  let page = rustix::param::page_size().max(1);
  let mut offset = at;
  for (new, old) in fresh.chunks(page).zip(held.chunks(page)) {
    let page_len = u64::try_from(new.len()).unwrap_or(u64::MAX);
    if new != old {
      let written = if new.iter().all(|byte| *byte == 0) {
        object.zero(offset, page_len)
      } else {
        object.store(offset, new)
      };
      written.map_err(|_| EIO)?;
    }
    offset = offset.saturating_add(page_len);
  }
  Ok(())
}

/// Shape: the chunk the run finder compares whole before it looks at bytes: a cache line, so an unchanged chunk costs
/// one vectorized comparison, and a store the guest made costs a scan of its line alone.
const COMPARE_CHUNK: usize = 64;

/// The runs, `(from, to)`, where `stored` differs from `held`, within the shorter of the two. Chunks that compare equal
/// whole are passed over; only a chunk that differs is looked at byte by byte.
fn differing_runs(stored: &[u8], held: &[u8]) -> Vec<(usize, usize)> {
  let len = stored.len().min(held.len());
  let (stored, held) = (
    stored.get(..len).unwrap_or(&[]),
    held.get(..len).unwrap_or(&[]),
  );
  let mut runs = Vec::new();
  if stored == held {
    return runs;
  }
  let mut open: Option<usize> = None;
  for (chunk, (mine, theirs)) in stored
    .chunks(COMPARE_CHUNK)
    .zip(held.chunks(COMPARE_CHUNK))
    .enumerate()
  {
    let base = chunk.saturating_mul(COMPARE_CHUNK);
    if mine == theirs {
      if let Some(from) = open.take() {
        runs.push((from, base));
      }
      continue;
    }
    for (offset, (a, b)) in mine.iter().zip(theirs.iter()).enumerate() {
      let index = base.saturating_add(offset);
      match (a == b, open) {
        (false, None) => open = Some(index),
        (true, Some(from)) => {
          runs.push((from, index));
          open = None;
        }
        _ => {}
      }
    }
  }
  if let Some(from) = open {
    runs.push((from, len));
  }
  runs
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: compute the ranges a fill covers for asks with and without a range, at and past a file's end. Expect the
  /// asked range's indexes within the file, the whole file's without one, and nothing past the end or for an empty
  /// file. Serves Shards part 3, step 2.
  #[test]
  fn a_fill_covers_the_asked_ranges_within_the_file() {
    let range = DAX_RANGE_BYTES;
    assert_eq!(covered(None, 5 * range), (0, 4));
    assert_eq!(covered(None, 5 * range + 1), (0, 5));
    assert_eq!(covered(Some((2 * range, range)), 10 * range), (2, 2));
    assert_eq!(covered(Some((2 * range, 3 * range)), 10 * range), (2, 4));
    assert_eq!(
      covered(Some((2 * range, 3 * range)), 3 * range),
      (2, 2),
      "clipped at the file's end"
    );
    let (first, last) = covered(Some((4 * range, range)), 3 * range);
    assert!(first > last, "a range past the end covers nothing");
    let (first, last) = covered(None, 0);
    assert!(first > last, "an empty file covers nothing");
  }

  /// Do: find where stored bytes differ from the volume's: none, one in the middle, runs at both ends, a volume shorter
  /// than the object's span, and runs across and either side of a compared chunk. Expect each run whole, in order,
  /// within the shorter of the two. Serves Shards part 3, step 3.
  #[test]
  fn differing_runs_are_found_whole_and_in_order() {
    assert!(differing_runs(b"abcdef", b"abcdef").is_empty());
    assert_eq!(differing_runs(b"abXXef", b"abcdef"), vec![(2, 4)]);
    assert_eq!(differing_runs(b"XbcdeY", b"abcdef"), vec![(0, 1), (5, 6)]);
    assert_eq!(
      differing_runs(b"abcdXX", b"abcd"),
      Vec::<(usize, usize)>::new()
    );
    assert_eq!(differing_runs(b"aXcdXX", b"abcd"), vec![(1, 2)]);
    assert_eq!(differing_runs(b"abcXXX", b"abcdef"), vec![(3, 6)]);
    let held = vec![0u8; 3 * COMPARE_CHUNK];
    let mut stored = held.clone();
    stored[COMPARE_CHUNK - 2..COMPARE_CHUNK + 3].fill(1);
    stored[3 * COMPARE_CHUNK - 1] = 1;
    assert_eq!(
      differing_runs(&stored, &held),
      vec![
        (COMPARE_CHUNK - 2, COMPARE_CHUNK + 3),
        (3 * COMPARE_CHUNK - 1, 3 * COMPARE_CHUNK)
      ],
      "a run across a chunk boundary is one run, and an equal chunk between runs parts them"
    );
  }
}
