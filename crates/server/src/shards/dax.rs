//! The DAX half of the Shards route (§4.6; Shards part 3, step 2; `docs/wip/shards/dax-protocol.md`, agreed with the
//! Shards owner 2026-10-24). The daemon answers Shards' DESCRIPTOR ask with a descriptor of the asked file's DAX object
//! (`slates_mem::DaxObject`), one per attachment and file, filled from the volume through the attachment's own bridge.
//! The descriptor authorizes that one file's bytes and nothing of any other file or volume. A descriptor of slates'
//! content itself would carry the whole content arena, every volume's (A-135).
//!
//! **Answering an ask.** The ask names a node, whether the mapping is to be written, and, from a device that names it,
//! the mapping's range.
//! - The node is resolved and its attributes read through the attachment's bridge, so its scope and rights apply as
//!   to any request. A node it does not reach, or one that is not a regular file, is refused.
//! - A writable ask of an attachment that may not write is refused `EPERM`, as its writes are. A mapping the guest
//!   wrote would otherwise take stores that never reach the volume.
//! - The node's object, made on its first ask, has the asked ranges filled from the volume through the same bridge, a
//!   slice at a time (below). Holes are skipped (SEEK_DATA), and a page the volume holds as zeros is not copied, since
//!   an untouched page already reads zero, so a hole commits no memory.
//! - Then a new descriptor of the object goes back with the answer, by `SCM_RIGHTS`. Every ask of the node is answered
//!   with the same object. That keeps every range of the file coherent in Shards' window, which maps each range from
//!   its own ask's descriptor.
//!
//! **A slice a turn.** A fill copies user-scaled bytes, so it runs in cooperative slices of the shard's copy slice
//! ([`crate::config::DaemonConfig::copy_slice_bytes`]: half the live step quantum at the measured memcpy rate), one per
//! turn of the stream's task, the answer owed when the last slice is done. The device waits for that answer with its
//! stream held, so no other frame arrives meanwhile.
//!
//! **Bounds, derived (R3).** A shard's fills are counted, page by page as they are stored, against its fill budget: its
//! content reserve ([`crate::config::DaemonConfig::shard_content_layout`]). That is one copy of everything the shard can
//! hold, the cost the design measured for a copy (slope 1.0 over mapped bytes). Each backing counts at least one range,
//! so the objects, and the descriptors, a device can make are bounded too. So are the views slates holds of them, a
//! range each. A fill past the budget is refused `ENOMEM`, counted.
//!
//! **Ending.** A backing ends when its stream ends, or when the guest forgets its node: a Linux guest removes a DAX
//! inode's mappings, by waited `FUSE_REMOVEMAPPING` requests, before it queues the `FORGET` (`fs/fuse/inode.c`). Its
//! charge goes back. Step 3 adds ingesting its stores first, and step 4 writing other paths' changes into it while it
//! lives.
//!
//! Refusals, each counted by kind:
//! - `shards.dax_malformed`, `EINVAL`: an ask of no length the protocol has, or a range past the object;
//! - `shards.dax_unreachable`, the bridge's errno: a node the attachment does not reach;
//! - `shards.dax_not_a_file`: `EISDIR` for a directory, `EINVAL` for anything else that is not a regular file;
//! - `shards.dax_read_only`, `EPERM`;
//! - `shards.dax_budget`, `ENOMEM`;
//! - `shards.dax_object`, `EIO`: the OS refused the object, a view or a fill.
//!
//! An answered ask counts `shards.dax_answered`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use slates_bridge_core::{Bridge, DaxOffer, ObjectId, OpContext, VolumeBridge};
use slates_bridge_fuse::bridge::errno;
use slates_bridge_fuse::shards_fs::{
  DescriptorAsk, descriptor_answer, descriptor_ask, descriptor_refusal,
};
use slates_mem::{DAX_OBJECT_BYTES, DAX_RANGE_BYTES, DaxObject};
use slates_vfs::host::HostFs;
use slates_vfs::inode::Kind;

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
/// Format: Linux's `EISDIR`.
const EISDIR: i32 = 21;
/// Format: Linux's `EINVAL`.
const EINVAL: i32 = 22;

/// One file's DAX backing for one attachment: its object, what of it is filled, and what was asked writable.
pub(crate) struct Backing {
  /// The object the VMM maps the file's ranges from.
  object: DaxObject,
  /// The ranges filled from the volume, by index (offset / [`DAX_RANGE_BYTES`]).
  filled: BTreeSet<u64>,
  /// The ranges a mapping asked to write, by index: what an ingest compares (step 3).
  writable: BTreeSet<u64>,
  /// The bytes of the pages stored into the object, which hold memory.
  stored: u64,
  /// A range whose fill stopped part-way (a refusal mid-range), by index, and how far it got: the next fill of it resumes
  /// there, so no page is stored, and counted, twice.
  partial: Option<(u64, u64)>,
}

impl Backing {
  /// What the backing counts against the shard's fill budget: its stored pages, at least one range.
  fn charge(&self) -> u64 {
    self.stored.max(DAX_RANGE_BYTES)
  }
}

/// An attachment's backings, by inode.
pub(crate) type Backings = BTreeMap<u64, Backing>;

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
}

impl Refused {
  /// The errno the device returns to the guest's mapping.
  fn errno(self) -> i32 {
    match self {
      Refused::Malformed => EINVAL,
      Refused::Unreachable(errno) | Refused::NotAFile(errno) => errno,
      Refused::ReadOnly => EPERM,
      Refused::Budget => ENOMEM,
      Refused::Object => EIO,
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
    }
  }
}

/// Derived: the bytes a shard's DAX fills may hold: its content reserve (`shard_content_layout().arena`), one copy of
/// everything the shard can hold.
fn fill_budget(s: &ShardState) -> u64 {
  u64::try_from(s.config.shard_content_layout().arena.1).unwrap_or(u64::MAX)
}

/// Derived: the views of its object a backing may hold at once: the fill budget in ranges, the most it can fill.
fn most_views(s: &ShardState) -> usize {
  usize::try_from(fill_budget(s).checked_div(DAX_RANGE_BYTES).unwrap_or(0)).unwrap_or(usize::MAX)
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
  if let std::collections::btree_map::Entry::Vacant(slot) = stream.backings.entry(inode) {
    // The floor every backing counts: one range, so the objects a device can make are bounded.
    charge(s, DAX_RANGE_BYTES)?;
    match DaxObject::create(most_views(s)) {
      Ok(object) => {
        slot.insert(Backing {
          object,
          filled: BTreeSet::new(),
          writable: BTreeSet::new(),
          stored: 0,
          partial: None,
        });
      }
      Err(_) => {
        s.dax_filled = s.dax_filled.saturating_sub(DAX_RANGE_BYTES);
        return Err(Refused::Object);
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
  let rights_write = with_bridge(s, stream, |bridge, cx| {
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
  let (inode, size, may_write) = rights_write;
  if ask.writable && !may_write {
    return Err(Refused::ReadOnly);
  }
  Ok((inode, size))
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
  let filled_already = |stream: &ShardsStream, range: u64| {
    stream
      .backings
      .get(&pending.inode)
      .is_some_and(|backing| backing.filled.contains(&range))
  };
  while pending.next <= pending.last && filled_already(stream, pending.next) {
    pending.next = pending.next.saturating_add(1);
    pending.progress = 0;
  }
  if pending.next > pending.last {
    stream.pending = Some(pending);
    return Ok(true);
  }
  if pending.progress == 0
    && let Some((range, progress)) = stream
      .backings
      .get(&pending.inode)
      .and_then(|backing| backing.partial)
    && range == pending.next
  {
    pending.progress = progress;
  }
  let range_start = pending.next.saturating_mul(DAX_RANGE_BYTES);
  let range_bytes = pending
    .size
    .saturating_sub(range_start)
    .min(DAX_RANGE_BYTES);
  let slice = slice_bytes(s);
  let at = range_start.saturating_add(pending.progress);
  let want = range_bytes.saturating_sub(pending.progress).min(slice);
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
      let (processed, failed) = store_pages(s, stream, inode, data, &bytes);
      pending.progress = data.saturating_add(processed).saturating_sub(range_start);
      if let Some(refused) = failed {
        if let Some(backing) = stream.backings.get_mut(&inode) {
          backing.partial = Some((pending.next, pending.progress));
        }
        return Err(refused);
      }
      if pending.progress >= range_bytes {
        finish_range(stream, &mut pending);
      }
    }
    // No more data in the range, or the file ended early: the rest of the range reads zeros already.
    _ => finish_range(stream, &mut pending),
  }
  stream.pending = Some(pending);
  Ok(false)
}

/// Marks the pending fill's current range filled and moves to the next.
fn finish_range(stream: &mut ShardsStream, pending: &mut Pending) {
  if let Some(backing) = stream.backings.get_mut(&pending.inode) {
    backing.filled.insert(pending.next);
    if backing
      .partial
      .is_some_and(|(range, _)| range == pending.next)
    {
      backing.partial = None;
    }
  }
  pending.next = pending.next.saturating_add(1);
  pending.progress = 0;
}

/// Derived: the bytes one fill slice copies: the shard's copy slice, in whole pages, at least one.
fn slice_bytes(s: &ShardState) -> u64 {
  let page = u64::try_from(rustix::param::page_size().max(1)).unwrap_or(u64::MAX);
  let slice = s.config.copy_slice_bytes();
  slice
    .checked_div(page)
    .and_then(|pages| pages.checked_mul(page))
    .unwrap_or(page)
    .max(page)
}

/// Stores the pages of `bytes`, the file's from `at`, into `inode`'s object, skipping pages of zeros, each counted
/// against the fill budget as it is stored: the bytes of `bytes` done, and the refusal that stopped it before the end.
fn store_pages(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  at: u64,
  bytes: &[u8],
) -> (u64, Option<Refused>) {
  let page = rustix::param::page_size().max(1);
  let page_bytes = u64::try_from(page).unwrap_or(u64::MAX);
  let mut done = 0u64;
  for chunk in bytes.chunks(page) {
    let chunk_bytes = u64::try_from(chunk.len()).unwrap_or(u64::MAX);
    if !chunk.iter().all(|byte| *byte == 0)
      && let Err(refused) = store_page(s, stream, inode, at.saturating_add(done), chunk)
    {
      return (done, Some(refused));
    }
    done = done.saturating_add(chunk_bytes.min(page_bytes));
  }
  (done, None)
}

/// Stores one page, `chunk`, at `offset` in `inode`'s object, counting it against the fill budget first.
fn store_page(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  offset: u64,
  chunk: &[u8],
) -> Result<(), Refused> {
  let backing = stream.backings.get_mut(&inode).ok_or(Refused::Object)?;
  let before = backing.charge();
  let stored = backing
    .stored
    .saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
  charge(s, stored.max(DAX_RANGE_BYTES).saturating_sub(before))?;
  let backing = stream.backings.get_mut(&inode).ok_or(Refused::Object)?;
  backing
    .object
    .store(offset, chunk)
    .map_err(|_| Refused::Object)?;
  backing.stored = stored;
  Ok(())
}

/// The fill is done: marks the ranges asked writable, and owes the answer with a new descriptor of the object.
fn answered(s: &mut ShardState, stream: &mut ShardsStream) -> Result<(), Refused> {
  let pending = stream.pending.take().ok_or(Refused::Object)?;
  let backing = stream
    .backings
    .get_mut(&pending.inode)
    .ok_or(Refused::Object)?;
  if pending.ask.writable {
    let (first, last) = covered(pending.ask.range, pending.size);
    let mut range = first;
    while range <= last {
      backing.writable.insert(range);
      range = range.saturating_add(1);
    }
  }
  let descriptor =
    rustix::io::fcntl_dupfd_cloexec(backing.object.descriptor(), 0).map_err(|_| Refused::Object)?;
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
  if let Some(backing) = stream.backings.remove(&inode) {
    s.dax_filled = s.dax_filled.saturating_sub(backing.charge());
  }
}

/// Lets go of every backing on `stream`, and of its pending fill: the stream ended.
pub(super) fn release_all(s: &mut ShardState, stream: &mut ShardsStream) {
  stream.pending = None;
  stream.ingest = None;
  let charges: u64 = stream
    .backings
    .values()
    .map(Backing::charge)
    .fold(0, u64::saturating_add);
  stream.backings.clear();
  s.dax_filled = s.dax_filled.saturating_sub(charges);
}

/// Format: the refusal-ledger name of an ingest the bridge refused (the attachment revoked or its volume gone): that
/// file's stores are not written.
const INGEST_REFUSED: &str = "shards.dax_ingest_refused";
/// Format: the ledger name of each run of bytes an ingest wrote into the volume.
const INGESTED: &str = "shards.dax_ingested";

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
fn worth_ingesting(stream: &ShardsStream, nodes: impl IntoIterator<Item = u64>) -> VecDeque<u64> {
  let mut inodes: VecDeque<u64> = VecDeque::new();
  for node in nodes {
    if stream
      .backings
      .get(&node)
      .is_some_and(|backing| !backing.writable.is_empty())
      && !inodes.contains(&node)
    {
      inodes.push_back(node);
    }
  }
  inodes
}

/// Holds the request of `total` bytes at the head of `stream` for an ingest, where it is one that asks for the guest's
/// stores to reach the volume first: a `FSYNC`, `FLUSH` or `RELEASE` of a node with a writable backing, or a `FORGET` or
/// `BATCH_FORGET` of such nodes. Whether it was held.
pub(super) fn hold_for_ingest(stream: &mut ShardsStream, request: &[u8], total: usize) -> bool {
  use slates_bridge_fuse::Opcode;
  use slates_bridge_fuse::bridge::{KernelChange, kernel_changes};
  if stream.backings.is_empty() {
    return false;
  }
  let Ok(header) = slates_bridge_fuse::InHeader::parse(request) else {
    return false;
  };
  let nodes: Vec<u64> = match Opcode::from_wire(header.opcode) {
    Some(Opcode::FSync | Opcode::Flush | Opcode::Release) => vec![header.nodeid],
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
  let inodes = worth_ingesting(stream, nodes);
  if inodes.is_empty() {
    return false;
  }
  stream.ingest = Some(new_ingest(inodes, Then::Serve { total }));
  true
}

/// Starts the ingest every backing with writable ranges takes before the stream ends. Whether there is one.
pub(super) fn ingest_before_end(stream: &mut ShardsStream) -> bool {
  let inodes = worth_ingesting(stream, stream.backings.keys().copied().collect::<Vec<_>>());
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
        Err(()) => {
          s.count(INGEST_REFUSED, 1);
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
      Err(()) => {
        s.count(INGEST_REFUSED, 1);
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

/// Takes `inode`'s ingest: its ranges asked writable and the file's size now. Refused where the attachment may not
/// write, or the bridge refuses the node.
fn begin_inode(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
) -> Result<(Vec<u64>, u64), ()> {
  let ranges: Vec<u64> = stream
    .backings
    .get(&inode)
    .map(|backing| backing.writable.iter().copied().collect())
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
  .map_err(|_| ())?;
  Ok((ranges, size))
}

/// Compares `len` bytes of `inode`'s object from `at` with the volume's, and writes each run that differs into the
/// volume through the attachment's bridge: whether anything was written.
fn compare_and_write(
  s: &mut ShardState,
  stream: &mut ShardsStream,
  inode: u64,
  at: u64,
  len: u64,
  stored: &mut Vec<u8>,
  held: &mut Vec<u8>,
) -> Result<bool, ()> {
  stored.clear();
  stored.resize(usize::try_from(len).map_err(|_| ())?, 0);
  stream
    .backings
    .get_mut(&inode)
    .ok_or(())?
    .object
    .load(at, stored)
    .map_err(|_| ())?;
  let runs = with_bridge(s, stream, |bridge, cx| {
    let object = ObjectId::new(inode, 0);
    held.clear();
    bridge
      .read(object, cx, at, u32::try_from(len).unwrap_or(u32::MAX), held)
      .map_err(|_| Refused::Object)?;
    // A volume shorter than the object's span here (the file cut since) is compared as far as it reaches.
    let runs = differing_runs(stored, held);
    for &(from, to) in &runs {
      let bytes = stored.get(from..to).ok_or(Refused::Object)?;
      let offset = at.saturating_add(u64::try_from(from).unwrap_or(u64::MAX));
      bridge
        .write(object, cx, offset, bytes)
        .map_err(|_| Refused::Object)?;
    }
    Ok(runs.len())
  })
  .map_err(|_| ())?;
  if runs > 0 {
    s.count(INGESTED, u64::try_from(runs).unwrap_or(u64::MAX));
  }
  Ok(runs > 0)
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

  /// Do: find where stored bytes differ from the volume's: none, one in the middle, runs at both ends, and a volume
  /// shorter than the object's span. Expect each run whole, in order, within the shorter of the two. Serves Shards part
  /// 3, step 3.
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
