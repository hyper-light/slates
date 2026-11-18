//! A guest's DAX maps over per-file backings, shared by every transport that hands a VMM a file to map (§4.6; A-163;
//! `docs/wip/shards/dax-protocol.md`). A backing is one file's DAX object for one attachment (`slates_mem::DaxObject`),
//! kept by the shard's store (`slates_vfs::shards_backing`), so every change of the file reaches it at the volume's
//! single write boundary before the change returns. This module is the session a transport drives over its own
//! attachment: it answers asks for a file's bytes, fills them from the volume, writes a guest's stores back at the
//! moments the guest asks for them to be durable, and refills a backing the disk changed beneath. Each step works
//! through the attachment's own bridge, so its scope and rights apply as to any request.
//!
//! **Why a backing and not the content itself.** A descriptor of slates' content carries every volume's blocks (A-135),
//! and content moves: an open block that grows is copied to a larger one, an idle one is sealed. A guest never learns of
//! either: Linux keeps each 2 MiB range it mapped in its own tree and serves every access from it without asking again
//! (`fs/fuse/dax.c`). The vhost-user protocol forbids mapping over a range already mapped, and unmapping one in crosvm
//! leaves read-only zero pages, which a real guest read as the file's bytes
//! (`docs/bugs/2026-10-24-slates-own-dax-window-shows-stale-bytes-then-zeros-after-another-paths-write.md`). A backing
//! never moves and covers any range a guest maps, so a window over it stays right for as long as the guest holds it.
//! The price is a copy per change (write-through measured 117 µs per MiB per backing, BENCHMARKS).
//!
//! **An ask** names a node, whether the mapping is to be written, and the range it maps, if the device names one.
//! - The node is resolved and its attributes read through the bridge. A node it does not reach, or one that is not a
//!   regular file, is refused.
//! - A writable ask of an attachment that may not write is refused, as its writes are.
//! - A writable ask's file is copied up first (§4.6 DAX rights), as its first write would be. Beneath an overlay an
//!   untouched file follows the disk (D-25), and an ingest comparing the guest's map against it would take each
//!   outsider's edit for the guest's store.
//! - The node's backing, made on its first ask, has the asked ranges filled from the volume a slice at a time. Holes
//!   are skipped (`SEEK_DATA`), and a page the volume holds as zeros is not copied, since an untouched page reads zero.
//! - Every range an ask touches is filled, past the file's end too, where the fill copies nothing. A guest maps by its
//!   own idea of the file's size, and after another path cuts the file that idea is stale; the range it maps must still
//!   take a later write into it (`docs/bugs/2026-10-25-a-range-mapped-past-the-files-end-missed-later-writes.md`).
//! - A range is marked filled when its fill starts, so a change written through meanwhile lands on it and the rest of
//!   the fill reads the volume as changed.
//! - A fill refused or given up part-way leaves its range uncleared (A-419): no longer filled, so no change reaches it,
//!   and still counted, since the slices done stored bytes. The next fill of the range clears it first. Unmarked and
//!   uncounted instead, as before, its stored pages outlived the refusal, and where the file had since become a hole or
//!   zeros a later fill skipped them, so a guest's map showed the old bytes
//!   (`docs/bugs/2026-11-18-a-dax-fill-refused-part-way-left-its-bytes-for-a-later-fill-to-show.md`).
//! - The fill done, the answer is a new descriptor of the backing's object (on Windows, a section handle of the ask's
//!   access, which the transport duplicates into the VMM's process). Every ask of the node is answered with the same
//!   object, so every range of the file stays coherent however the device maps them.
//! - Before a range's fill begins it is readied for the VMM's map (`DaxObject::prepare_range`): Windows commits it
//!   whole, since a reserved section's page faults any view until committed; Unix pages come on touch.
//! - A backing lays its file out as its session says (`DaxLayout`): slates' own device tells its VMM where each piece of
//!   a window lies, so its backings hold the file in the interiors of their pages, a guard of one chunk window held
//!   empty at both ends of each, and a window is mapped from interiors alone (A-194). The Shards route maps by the file's
//!   own offsets, as its protocol fixes (A-163).
//! - As a range's fill finishes it is placed in one huge page, where the host has one of a range's size
//!   (`DaxObject::place_range`; §4.6, the backed route's L1; A-193). A guest's reads of its window hammer the rows
//!   behind it as surely as its stores (Kim et al., ISCA 2014), so a placed range lies on 2 MiB of contiguous memory of
//!   its own, beside foreign rows only at its two ends where 2 MiB spans several DRAM rows of each bank (§4.6 says where
//!   it buys less). A placement the kernel declines is counted, and the range is served on base
//!   pages; a decline likely to hold is remembered until the daemon's reap tick (`ShardsBackings::place`), since each
//!   costs the shard milliseconds of the kernel's compaction. A range is placed only once it is filled and counted, so
//!   the placed bytes stay within the fill budget.
//!
//! **Ingest.** The guest's stores through a map reach the volume when the transport asks: before it serves a request
//! that asks for them (a sync, a flush, a release, a read of the file, a forget of it) and before the attachment ends.
//! The backing is compared with the volume over the ranges asked writable, up to the file's end, and each run that
//! differs is written through the bridge. The backing is suppressed in the store meanwhile, so its own stores are not
//! written back into it over a newer store. A write-back the volume refuses is reported, so the transport fails the
//! request that asked for it rather than claim bytes durable that are not.
//!
//! **Refill.** An untouched file of an overlay also changes when the disk does, and the base plane marks its backings
//! stale where it takes such a change. Before anything more reaches the guest, the transport refills the stale backings'
//! filled ranges from the volume, a slice at a time. Ranges asked writable are never refilled: their files are copied up
//! before such an ask, so they never follow the disk.
//!
//! **A slice at a time.** A fill, an ingest and a refill copy user-scaled bytes, so each runs in cooperative slices of
//! the transport's choosing (its shard's copy slice), one per turn.
//!
//! **Bounds.** The store's backings count against its fill budget (`ShardsBackings::set_budget`): a range for each range
//! a backing has filled or left uncleared, and at least one, what write-through can touch and what a part-way fill
//! stored. A backing past the budget is refused, and files stop being offered DAX before that ([`offer`]).
//!
//! Proven through the Shards route by `crates/server/tests/shards.rs` (a real guest on macOS, A-163) and through
//! slates' own virtio-fs device by `crates/bridge-virtiofs/tests/device.rs`.

use std::collections::{BTreeMap, VecDeque, btree_map::Entry};

use slates_mem::{DAX_RANGE_BYTES, DaxLayout, DaxObject, Handle, Placement, dax_object_bytes};
use slates_vfs::error::VfsError;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::Kind;
use slates_vfs::shards_backing::{HandedObject, ShardsBacking, ShardsBackings};

use crate::{Bridge, DaxOffer, ObjectId, OpContext};

/// Format: the FUSE node id that names the attachment's root (`FUSE_ROOT_ID`).
pub const ROOT_NODE: u64 = 1;

/// Shape: the chunk the run finder compares whole before it looks at bytes: a cache line, so an unchanged chunk costs
/// one vectorized comparison, and a store the guest made costs a scan of its line alone.
const COMPARE_CHUNK: usize = 64;

/// An ask for a file's bytes to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ask {
  /// The node, as the guest names it ([`ROOT_NODE`] for the attachment's root).
  pub node: u64,
  /// Whether the mapping is to be written.
  pub writable: bool,
  /// The range the mapping maps, `(offset, len)`, or none for the whole file.
  pub range: Option<(u64, u64)>,
}

/// Why an ask was refused. The transport turns it into the errno its device passes to the guest's mapping, and counts
/// it by kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
  /// A range past the backing's object.
  Malformed,
  /// A node the attachment does not reach: the bridge's refusal.
  Unreachable(VfsError),
  /// A node that is not a regular file: a directory, or anything else.
  NotAFile {
    /// Whether it is a directory.
    directory: bool,
  },
  /// A writable ask of an attachment that may not write.
  ReadOnly,
  /// A backing past the store's fill budget.
  Budget,
  /// An object, a view or a fill the OS refused.
  Object,
  /// A writable ask whose file the bridge refused to copy up first: the bridge's refusal.
  Witness(VfsError),
}

/// Why a write-back or a refill of one file did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
  /// The backing's object refused (the OS).
  Object,
  /// The attachment may not write.
  ReadOnly,
  /// The volume refused, through the bridge.
  Volume(VfsError),
}

/// What the session did that a transport counts, since it was last asked ([`DaxBackingSession::take_counts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
  /// Asks answered with a descriptor.
  pub answered: u64,
  /// Runs of bytes an ingest wrote into the volume.
  pub ingested: u64,
  /// Files whose write-back the bridge refused.
  pub ingest_refused: u64,
  /// Backings refilled because the disk changed their file beneath them.
  pub refilled: u64,
  /// Ranges a refill could not read from the volume: their objects keep the bytes they had.
  pub refill_refused: u64,
  /// Ranges asked writable in a backing marked stale, which a refill passes over rather than lose the guest's stores:
  /// never expected, since a file is copied up before any writable ask of it.
  pub refill_writable: u64,
  /// Ranges placed in one huge page each as their fills finished (§4.6, the backed route's L1; A-193).
  pub placed: u64,
  /// Ranges whose placement the kernel declined, or that a decline remembered since the daemon's last reap tick passed
  /// over: they are served on base pages.
  pub placement_refused: u64,
}

/// One slice of a fill, as it came out.
#[derive(Debug)]
pub enum FillStep {
  /// No fill pending.
  Idle,
  /// The fill goes on next turn.
  Filling,
  /// The fill is done: what to hand the VMM for the backing's object (a new descriptor on Unix; on Windows a section
  /// handle in this process, of the ask's access, for the transport to duplicate into the VMM's process).
  Done(HandedObject),
  /// The fill was refused: nothing is pending any more.
  Refused(Refused),
}

/// What an ingest was for, the transport's own mark it gets back when the ingest is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
  /// A request the transport holds, named by its own token, is served next.
  Serve(u64),
  /// The attachment ends next.
  End,
  /// Nothing waits on it: the transport goes on as before once the stores are in (a resynced stream's adopted
  /// backings, ingested before it serves anything, A-365).
  Resume,
}

/// A finished ingest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ingested {
  /// Whether anything was written: the change is then published before what asked for it goes on.
  pub wrote: bool,
  /// The first write-back that was refused, which the request that asked for it answers.
  pub refused: Option<Failure>,
  /// What asked for it.
  pub then: Then,
}

/// A fill under way: an ask's ranges copied from the volume into its backing, a slice at a time.
#[derive(Debug)]
struct Fill {
  ask: Ask,
  inode: u64,
  /// The file's size when the ask was taken: the fill stops at it.
  size: u64,
  /// The range being filled, by index, and the last the fill covers.
  next: u64,
  last: u64,
  /// How far into the range being filled the fill has got, in bytes: more than none once the range's fill began and
  /// until it finishes.
  progress: u64,
}

/// An ingest under way: the guest's stores through its maps of some files compared with the volume over the ranges
/// asked writable and written back where they differ, a slice at a time.
#[derive(Debug)]
struct Ingest {
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
  wrote: bool,
  refused: Option<Failure>,
  then: Then,
  /// The object's and the volume's bytes of the slice being compared: kept from one slice to the next, so a slice
  /// allocates nothing (each at most one slice long).
  stored: Vec<u8>,
  held: Vec<u8>,
}

/// A refill under way: the stale backings' filled ranges copied again from the volume, a slice at a time.
#[derive(Debug)]
struct Refill {
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
  /// The volume's and the object's bytes of the slice being refilled, kept from one slice to the next.
  fresh: Vec<u8>,
  held: Vec<u8>,
}

/// One attachment's backed DAX session: its backings by inode (handles into the store's registry), and the fill, the
/// ingest and the refill under way, each a slice per turn of the transport that drives it.
#[derive(Debug, Default)]
pub struct DaxBackingSession {
  /// How the backings it makes lay out their files: guarded for a VMM told where each piece lies (slates' own device,
  /// A-194), the identity for one that maps by the file's own offsets (the Shards protocol, A-163).
  layout: DaxLayout,
  backings: BTreeMap<u64, Handle<ShardsBacking>>,
  pending: Option<Fill>,
  ingest: Option<Ingest>,
  refill: Option<Refill>,
  /// The store's count of stale marks when this session last found none of its backings stale.
  stale_seen: u64,
  counts: Counts,
}

/// The DAX offer an attachment's requests are answered under: per-inode DAX for regular files of a range or more,
/// marked while the store's fill budget has room for another backing's first range in `layout` (a page under the
/// identity, two under a guard), so files stop being offered DAX before a fill would be refused.
#[must_use]
pub fn offer(backings: &ShardsBackings, layout: DaxLayout) -> DaxOffer {
  let first_range = layout
    .pages_holding([0])
    .max(1)
    .saturating_mul(DAX_RANGE_BYTES);
  DaxOffer::PerInode {
    smallest: DAX_RANGE_BYTES,
    new_files: backings.charged().saturating_add(first_range) <= backings.budget(),
  }
}

/// The layout of the backings of a device that tells its VMM where each piece of a window lies (slates' own device,
/// A-168; §4.6, the backed route's L1; A-194): guarded by one chunk window at each end of every page, the slot the
/// arena's zones hold as their guard rows (`slates_vfs::content::chunk_bytes`), where this host can place a range in
/// one huge page; the identity elsewhere, where a guard would isolate nothing. A host fact, so it is asked once a
/// process: the virtio-fs device's DAX offer reads it with every request.
#[must_use]
pub fn guarded_layout() -> DaxLayout {
  static LAYOUT: std::sync::OnceLock<DaxLayout> = std::sync::OnceLock::new();
  *LAYOUT.get_or_init(|| {
    let page = usize::try_from(slates_machine::facts::Facts::page_bytes()).unwrap_or(usize::MAX);
    let guard = u64::try_from(slates_vfs::content::chunk_bytes(page).get()).unwrap_or(u64::MAX);
    DaxLayout::for_backings(guard)
  })
}

/// Derived: the views of its object a backing may hold at once: the fill budget in pages, the most it can fill. Public
/// for a backing kept again from a dead daemon's object (A-365), which holds views as one made here does.
#[must_use]
pub fn most_views(backings: &ShardsBackings) -> usize {
  usize::try_from(backings.budget().checked_div(DAX_RANGE_BYTES).unwrap_or(0)).unwrap_or(usize::MAX)
}

/// What a backing counts against the fill budget: a page for each page its filled ranges lie in (one a range under the
/// identity; under a guard two for a range alone and sixteen for fifteen in a row), at least one. That is what its
/// placed pages hold resident, so a placement never takes memory the budget does not count. A range a fill began and
/// did not finish counts as a filled one until it is cleared (A-419): its object may hold what the fill stored.
fn charge_of(backing: &ShardsBacking) -> u64 {
  filled_charge(
    backing.object().layout(),
    backing.filled().union(backing.uncleared()).copied(),
  )
}

/// What a backing in `layout` whose ranges `filled` are filled counts against the fill budget ([`charge_of`]'s rule),
/// for a backing kept again before it is made (a successor's adoption, A-365).
#[must_use]
pub fn filled_charge(layout: DaxLayout, filled: impl IntoIterator<Item = u64>) -> u64 {
  layout
    .pages_holding(filled)
    .max(1)
    .saturating_mul(DAX_RANGE_BYTES)
}

/// The ranges a fill covers, by index, first and last. An asked range covers every range it touches, past the file's end
/// too: a guest maps by its own idea of the file's size, which another path's cut makes stale, and a range it maps must
/// take every later change, an extension into it included. Without a range, the whole file's. A first past the last
/// covers nothing.
#[must_use]
pub fn covered(range: Option<(u64, u64)>, size: u64) -> (u64, u64) {
  let (from, to) = match range {
    Some((offset, len)) => (offset, offset.saturating_add(len)),
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

/// The runs, `(from, to)`, where `stored` differs from `held`, within the shorter of the two. Chunks that compare equal
/// whole are passed over; only a chunk that differs is looked at byte by byte.
#[must_use]
pub fn differing_runs(stored: &[u8], held: &[u8]) -> Vec<(usize, usize)> {
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

/// The host's page in bytes, at least one: the unit pages of zeros are skipped in. Asked once (a slice asks per call).
fn page_bytes() -> usize {
  static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
  *PAGE.get_or_init(|| {
    usize::try_from(slates_machine::facts::Facts::page_bytes())
      .unwrap_or(1)
      .max(1)
  })
}

impl DaxBackingSession {
  /// A session with no backings, which lays its files out by their own offsets (the Shards protocol, A-163).
  #[must_use]
  pub fn new() -> DaxBackingSession {
    DaxBackingSession::default()
  }

  /// A session with no backings, which lays its files out in `layout` ([`guarded_layout`] for slates' own device).
  #[must_use]
  pub fn with_layout(layout: DaxLayout) -> DaxBackingSession {
    DaxBackingSession {
      layout,
      ..DaxBackingSession::default()
    }
  }

  /// How the backings it makes lay out their files: what a transport maps a window's pieces by.
  #[must_use]
  pub fn layout(&self) -> DaxLayout {
    self.layout
  }

  /// Whether the session backs no file.
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.backings.is_empty()
  }

  /// How many files it backs.
  #[must_use]
  pub fn len(&self) -> usize {
    self.backings.len()
  }

  /// The inode of the fill under way, if one: the ask its answer will be for.
  #[must_use]
  pub fn pending_inode(&self) -> Option<u64> {
    self.pending.as_ref().map(|pending| pending.inode)
  }

  /// The backing it holds for `inode`, if one: its handle in the store's registry, which the anchor's hold of its object
  /// is named by (A-365).
  #[must_use]
  pub fn backing_of(&self, inode: u64) -> Option<Handle<ShardsBacking>> {
    self.backings.get(&inode).copied()
  }

  /// Takes `handle`, a backing of `inode` kept again in the store (a resynced stream's, adopted from the anchor's hold,
  /// A-365), as this session's: every ask of the node is answered with its object from now on, and its ranges asked
  /// writable are what an ingest compares. A backing it holds already for `inode` is kept, and `handle` is not taken.
  /// Whether it was taken.
  pub fn adopt(&mut self, inode: u64, handle: Handle<ShardsBacking>) -> bool {
    match self.backings.entry(inode) {
      Entry::Vacant(vacant) => {
        vacant.insert(handle);
        true
      }
      Entry::Occupied(_) => false,
    }
  }

  /// Whether it backs file `inode`.
  #[must_use]
  pub fn backs(&self, inode: u64) -> bool {
    self.backings.contains_key(&inode)
  }

  /// Whether a fill is pending.
  #[must_use]
  pub fn filling(&self) -> bool {
    self.pending.is_some()
  }

  /// Whether an ingest is under way.
  #[must_use]
  pub fn ingesting(&self) -> bool {
    self.ingest.is_some()
  }

  /// What it did since it was last asked; the counts start again.
  pub fn take_counts(&mut self) -> Counts {
    std::mem::take(&mut self.counts)
  }

  /// Takes `ask`: checks it, makes the node's backing if it has none, and leaves its fill pending, run by
  /// [`DaxBackingSession::fill_step`]: the inode the node resolved to. A refusal leaves nothing pending.
  pub fn ask(&mut self, bridge: &mut dyn Bridge, cx: &OpContext, ask: Ask) -> Result<u64, Refused> {
    // A slice no view can begin at is refused before anything is made or filled: a view of a section starts on the
    // system's allocation granularity, 64 KiB on Windows, and a map on a page elsewhere (A-211).
    if let Some((offset, len)) = ask.range
      && (offset.saturating_add(len) > self.layout.file_bytes(dax_object_bytes())
        || !offset.is_multiple_of(slates_machine::facts::Facts::allocation_granularity()))
    {
      return Err(Refused::Malformed);
    }
    let (inode, size) = checked(bridge, cx, ask)?;
    // A writable map's file is copied up first (§4.6 DAX rights), as its first write would be: what the volume serves
    // for it then changes only through the write boundary, so a write-back never takes an outsider's change for the
    // guest's. Its size is the witnessed file's.
    let size = if ask.writable {
      witnessed(bridge, cx, inode)?
    } else {
      size
    };
    if let Entry::Vacant(vacant) = self.backings.entry(inode) {
      let state = bridge.dax_state().ok_or(Refused::Object)?;
      // The floor every backing counts: one range, so the backings a session can make are bounded.
      if !state.backings.charge(DAX_RANGE_BYTES) {
        return Err(Refused::Budget);
      }
      let made = DaxObject::create(most_views(state.backings), self.layout)
        .map_err(|_| Refused::Object)
        .and_then(|object| {
          state
            .backings
            .insert(ShardsBacking::new(object, state.volume, InodeNo(inode)))
            .map_err(|_| Refused::Object)
        });
      match made {
        Ok(handle) => {
          vacant.insert(handle);
        }
        Err(refused) => {
          state.backings.uncharge(DAX_RANGE_BYTES);
          return Err(refused);
        }
      }
    }
    let (next, last) = covered(ask.range, size);
    self.pending = Some(Fill {
      ask,
      inode,
      size,
      next,
      last,
      progress: 0,
    });
    Ok(inode)
  }

  /// One slice of the pending fill, of at most `slice_bytes`. When the fill is done, the answer is a new descriptor of
  /// the backing's object. A refusal mid-fill leaves the range uncleared (counted, and cleared before its next fill
  /// begins; A-419), and nothing pending.
  pub fn fill_step(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
  ) -> FillStep {
    let Some(mut pending) = self.pending.take() else {
      return FillStep::Idle;
    };
    match self.stepped(bridge, cx, &mut pending, slice_bytes) {
      Ok(false) => {
        self.pending = Some(pending);
        FillStep::Filling
      }
      Ok(true) => match self.answered(bridge, &pending) {
        Ok(descriptor) => {
          self.counts.answered = self.counts.answered.saturating_add(1);
          FillStep::Done(descriptor)
        }
        Err(refused) => FillStep::Refused(refused),
      },
      Err(refused) => FillStep::Refused(refused),
    }
  }

  /// Gives up the pending fill (its attachment is gone): a range whose fill began is left uncleared, as a refused fill
  /// leaves it (A-419).
  pub fn abandon_fill(&mut self, backings: &mut ShardsBackings) {
    if let Some(pending) = self.pending.take()
      && pending.progress > 0
      && let Some(target) = self
        .backings
        .get(&pending.inode)
        .and_then(|handle| backings.get_mut(*handle))
    {
      target.mark_uncleared(pending.next);
    }
  }

  /// [`DaxBackingSession::fill_step`]'s work: whether the fill is done.
  fn stepped(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    pending: &mut Fill,
    slice_bytes: u64,
  ) -> Result<bool, Refused> {
    let handle = *self.backings.get(&pending.inode).ok_or(Refused::Object)?;
    // Ranges filled already (by an earlier ask, the floor's first range or another) are passed over.
    {
      let state = bridge.dax_state().ok_or(Refused::Object)?;
      while pending.progress == 0
        && pending.next <= pending.last
        && state
          .backings
          .get(handle)
          .is_some_and(|backing| backing.filled().contains(&pending.next))
      {
        pending.next = pending.next.saturating_add(1);
      }
      if pending.next > pending.last {
        return Ok(true);
      }
      if pending.progress == 0 {
        // The range's fill begins: it is counted, then marked filled, so write-through reaches it from now on.
        let target = state.backings.get_mut(handle).ok_or(Refused::Object)?;
        // Readied for the VMM's map first: a Windows section's range is committed whole, past the file's end and over
        // its holes too, where the fill copies nothing and the VMM's touch would otherwise fault.
        target
          .object_mut()
          .prepare_range(pending.next)
          .map_err(|_| Refused::Object)?;
        // A range an earlier fill began and did not finish may hold bytes the file no longer has, and this fill skips
        // the volume's holes and pages of zeros: it is cleared first, so it reads as a range never filled (A-419).
        // Refused, it stays uncleared and counted, and the next ask clears it again.
        if target.uncleared().contains(&pending.next) {
          target
            .object_mut()
            .zero(
              pending.next.saturating_mul(DAX_RANGE_BYTES),
              DAX_RANGE_BYTES,
            )
            .map_err(|_| Refused::Object)?;
        }
        // Counted with what it holds before and after: an uncleared range is counted already, so it adds nothing.
        let before = charge_of(target);
        target.mark_filled(pending.next);
        target.cleared(pending.next);
        let after = charge_of(target);
        let delta = after.saturating_sub(before);
        if !state.backings.charge(delta) {
          if let Some(target) = state.backings.get_mut(handle) {
            target.unmark_filled(pending.next);
          }
          return Err(Refused::Budget);
        }
      }
    }
    match fill_slice(bridge, cx, handle, pending, slice_bytes) {
      Ok(placement) => self.count_placement(placement),
      Err(refused) => {
        // A fill that failed part-way may have stored bytes the file will not hold when its range is asked again, and
        // no write reaches a range not filled: it is left uncleared, still counted, and cleared before the next fill
        // of it begins (A-419).
        if let Some(state) = bridge.dax_state()
          && let Some(target) = state.backings.get_mut(handle)
        {
          target.mark_uncleared(pending.next);
        }
        return Err(refused);
      }
    }
    Ok(false)
  }

  /// Counts how the placement of a range whose fill finished came out, if one finished.
  fn count_placement(&mut self, placement: Option<Placement>) {
    match placement {
      Some(Placement::Placed) => self.counts.placed = self.counts.placed.saturating_add(1),
      Some(Placement::Declined { .. }) => {
        self.counts.placement_refused = self.counts.placement_refused.saturating_add(1);
      }
      Some(Placement::Kept | Placement::Unavailable) | None => {}
    }
  }

  /// The fill is done: marks the ranges asked writable, and makes what the answer hands the VMM for the object.
  fn answered(&mut self, bridge: &mut dyn Bridge, pending: &Fill) -> Result<HandedObject, Refused> {
    let handle = *self.backings.get(&pending.inode).ok_or(Refused::Object)?;
    let state = bridge.dax_state().ok_or(Refused::Object)?;
    let target = state.backings.get_mut(handle).ok_or(Refused::Object)?;
    if pending.ask.writable {
      let (first, last) = covered(pending.ask.range, pending.size);
      let mut range = first;
      while range <= last {
        target.mark_writable(range);
        range = range.saturating_add(1);
      }
    }
    target
      .object()
      .handed_to_vmm(pending.ask.writable)
      .map_err(|_| Refused::Object)
  }

  /// Lets go of `inode`'s backing, if the session has one, and gives its charge back: the guest forgot the node.
  pub fn release(&mut self, backings: &mut ShardsBackings, inode: u64) {
    if let Some(handle) = self.backings.remove(&inode)
      && let Some(gone) = backings.remove(handle)
    {
      backings.uncharge(charge_of(&gone));
    }
  }

  /// Lets go of every backing, and of the fill, the ingest and the refill under way: the attachment ended.
  pub fn release_all(&mut self, backings: &mut ShardsBackings) {
    self.abandon_fill(backings);
    self.ingest = None;
    self.refill = None;
    for (_, handle) in std::mem::take(&mut self.backings) {
      if let Some(gone) = backings.remove(handle) {
        backings.uncharge(charge_of(&gone));
      }
    }
  }

  /// The inodes of `nodes` whose backings hold ranges asked writable, each once: what an ingest for them compares.
  #[must_use]
  pub fn worth_ingesting(
    &self,
    backings: &ShardsBackings,
    nodes: impl IntoIterator<Item = u64>,
  ) -> VecDeque<u64> {
    let mut inodes: VecDeque<u64> = VecDeque::new();
    for node in nodes {
      let has_writable = self
        .backings
        .get(&node)
        .and_then(|handle| backings.get(*handle))
        .is_some_and(|backing| !backing.writable().is_empty());
      if has_writable && !inodes.contains(&node) {
        inodes.push_back(node);
      }
    }
    inodes
  }

  /// Starts an ingest of `inodes`, for `then`. An empty list starts nothing: whether one started.
  pub fn begin_ingest(&mut self, inodes: VecDeque<u64>, then: Then) -> bool {
    if inodes.is_empty() {
      return false;
    }
    self.ingest = Some(Ingest {
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
    });
    true
  }

  /// Starts the ingest every backing with writable ranges takes before the attachment ends. Whether there is one.
  pub fn ingest_before_end(&mut self, backings: &ShardsBackings) -> bool {
    self.begin_ingest_all(backings, Then::End)
  }

  /// Starts an ingest of every backing with writable ranges, for `then`: what a guest's unmount asks before it is served.
  /// Whether there is one.
  pub fn begin_ingest_all(&mut self, backings: &ShardsBackings, then: Then) -> bool {
    let nodes: Vec<u64> = self.backings.keys().copied().collect();
    let inodes = self.worth_ingesting(backings, nodes);
    self.begin_ingest(inodes, then)
  }

  /// Marks every backing of the session stale: the content its guest sees moved without a write (a snapshot device's
  /// advance to another snapshot), so each is refilled from the view before the guest sees anything more.
  pub fn mark_all_stale(&mut self, backings: &mut ShardsBackings) {
    for &handle in self.backings.values() {
      backings.mark_backing_stale(handle);
    }
  }

  /// Ends the ingest under way as refused with `failure` (its attachment's bridge is gone): what it was for, to go on
  /// with. Nothing under way, nothing returned.
  pub fn abort_ingest(&mut self, failure: Failure) -> Option<Ingested> {
    let ingest = self.ingest.take()?;
    self.counts.ingest_refused = self.counts.ingest_refused.saturating_add(1);
    Some(Ingested {
      wrote: ingest.wrote,
      refused: Some(ingest.refused.unwrap_or(failure)),
      then: ingest.then,
    })
  }

  /// One slice of the ingest, of at most `slice_bytes`: the finished ingest when it is done. An inode the bridge refuses
  /// (the attachment revoked, its volume gone, or one that may not write) is passed over, counted, its stores unwritten.
  pub fn ingest_step(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    slice_bytes: u64,
  ) -> Option<Ingested> {
    let mut ingest = self.ingest.take()?;
    loop {
      let Some(&inode) = ingest.inodes.front() else {
        return Some(Ingested {
          wrote: ingest.wrote,
          refused: ingest.refused,
          then: ingest.then,
        });
      };
      if !ingest.begun {
        match self.begin_inode(bridge, cx, inode) {
          Ok((ranges, size)) => {
            ingest.ranges = ranges;
            ingest.size = size;
            ingest.index = 0;
            ingest.progress = 0;
            ingest.begun = true;
          }
          Err(failure) => {
            self.counts.ingest_refused = self.counts.ingest_refused.saturating_add(1);
            ingest.refused.get_or_insert(failure);
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
      let want = range_bytes.saturating_sub(ingest.progress).min(slice_bytes);
      let (mut stored, mut held) = (
        std::mem::take(&mut ingest.stored),
        std::mem::take(&mut ingest.held),
      );
      let compared =
        self.compare_and_write(bridge, cx, inode, (at, want), (&mut stored, &mut held));
      (ingest.stored, ingest.held) = (stored, held);
      match compared {
        Ok(runs) => {
          ingest.wrote |= runs > 0;
          self.counts.ingested = self.counts.ingested.saturating_add(runs);
          ingest.progress = ingest.progress.saturating_add(want);
        }
        Err(failure) => {
          self.counts.ingest_refused = self.counts.ingest_refused.saturating_add(1);
          ingest.refused.get_or_insert(failure);
          next_inode(&mut ingest);
          continue;
        }
      }
      self.ingest = Some(ingest);
      return None;
    }
  }

  /// Takes `inode`'s ingest: its ranges asked writable and the file's size now. Refused where the attachment may not
  /// write, or the bridge refuses the node.
  fn begin_inode(
    &self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    inode: u64,
  ) -> Result<(Vec<u64>, u64), Failure> {
    let ranges: Vec<u64> = match (self.backings.get(&inode), bridge.dax_state()) {
      (Some(handle), Some(state)) => state
        .backings
        .get(*handle)
        .map(|target| target.writable().iter().copied().collect())
        .unwrap_or_default(),
      _ => Vec::new(),
    };
    if !cx.rights.write {
      return Err(Failure::ReadOnly);
    }
    let size = bridge
      .getattr(ObjectId::new(inode, 0), cx)
      .map(|attr| attr.size)
      .map_err(Failure::Volume)?;
    Ok((ranges, size))
  }

  /// Compares `span.1` bytes of `inode`'s object from `span.0` with the volume's, and writes each run that differs into
  /// the volume through the bridge: how many runs were written, or why not ([`write_back_slice`]).
  fn compare_and_write(
    &self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    inode: u64,
    span: (u64, u64),
    buffers: (&mut Vec<u8>, &mut Vec<u8>),
  ) -> Result<u64, Failure> {
    let handle = *self.backings.get(&inode).ok_or(Failure::Object)?;
    write_back_slice(bridge, cx, (handle, inode), span, buffers).map(|wrote| wrote.runs)
  }

  /// Whether the session owes its guest a refill before anything more: one under way, or a backing of its marked stale
  /// since it last looked. The store's count of marks says whether to look at all, so a session whose files the disk
  /// leaves alone pays one comparison a turn.
  pub fn refill_owed(&mut self, backings: &ShardsBackings) -> bool {
    if self.refill.is_some() {
      return true;
    }
    let marks = backings.marked_stale();
    if marks == self.stale_seen {
      return false;
    }
    let stale = self
      .backings
      .values()
      .any(|handle| backings.get(*handle).is_some_and(ShardsBacking::stale));
    if !stale {
      self.stale_seen = marks;
    }
    stale
  }

  /// One slice of the refill, of at most `slice_bytes`, which begins by taking the stale marks of the session's backings
  /// when none is under way. A range the volume refuses to read now keeps the bytes it had, counted: the map cannot
  /// answer an error, and the guest learns of the refusal from its next request of the file, which the volume refuses
  /// too.
  pub fn refill_step(&mut self, bridge: &mut dyn Bridge, cx: &OpContext, slice_bytes: u64) {
    let mut refill = match self.refill.take() {
      Some(refill) => refill,
      None => match bridge.dax_state() {
        Some(state) => self.begin_refill(state.backings),
        None => return,
      },
    };
    loop {
      let Some(&inode) = refill.inodes.front() else {
        return;
      };
      if !refill.begun {
        refill.ranges = self.refill_ranges(bridge, inode);
        refill.index = 0;
        refill.progress = 0;
        refill.begun = true;
        self.counts.refilled = self.counts.refilled.saturating_add(1);
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
        .min(slice_bytes);
      let (mut fresh, mut held) = (
        std::mem::take(&mut refill.fresh),
        std::mem::take(&mut refill.held),
      );
      let refilled = self.refill_slice(bridge, cx, inode, (at, want), (&mut fresh, &mut held));
      (refill.fresh, refill.held) = (fresh, held);
      if refilled.is_ok() {
        refill.progress = refill.progress.saturating_add(want);
      } else {
        self.counts.refill_refused = self.counts.refill_refused.saturating_add(1);
        refill.index = refill.index.saturating_add(1);
        refill.progress = 0;
      }
      self.refill = Some(refill);
      return;
    }
  }

  /// Ends the refill under way, if any (its attachment's bridge is gone).
  pub fn abort_refill(&mut self) {
    self.refill = None;
  }

  /// Takes the stale marks of the session's backings: the refill of each that had one.
  fn begin_refill(&mut self, backings: &mut ShardsBackings) -> Refill {
    self.stale_seen = backings.marked_stale();
    let mut inodes = VecDeque::new();
    for (&inode, &handle) in &self.backings {
      if backings
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
  fn refill_ranges(&mut self, bridge: &mut dyn Bridge, inode: u64) -> Vec<u64> {
    let Some(&handle) = self.backings.get(&inode) else {
      return Vec::new();
    };
    let Some(state) = bridge.dax_state() else {
      return Vec::new();
    };
    let Some(target) = state.backings.get(handle) else {
      return Vec::new();
    };
    let (ranges, writable): (Vec<u64>, Vec<u64>) = target
      .filled()
      .iter()
      .partition(|range| !target.writable().contains(range));
    if !writable.is_empty() {
      self.counts.refill_writable = self
        .counts
        .refill_writable
        .saturating_add(u64::try_from(writable.len()).unwrap_or(u64::MAX));
    }
    ranges
  }

  /// Refills `span.1` bytes of `inode`'s object from `span.0` with the volume's. What lies past the file's end reads
  /// zero, and only the pages that differ are written: a page of zeros by letting it go, so a hole commits no memory.
  fn refill_slice(
    &self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    inode: u64,
    (at, len): (u64, u64),
    (fresh, held): (&mut Vec<u8>, &mut Vec<u8>),
  ) -> Result<(), Failure> {
    let handle = *self.backings.get(&inode).ok_or(Failure::Object)?;
    let span = usize::try_from(len).map_err(|_| Failure::Object)?;
    read_whole(bridge, cx, ObjectId::new(inode, 0), (at, len), fresh).map_err(Failure::Volume)?;
    fresh.resize(span, 0);
    held.clear();
    held.resize(span, 0);
    let state = bridge.dax_state().ok_or(Failure::Object)?;
    let object = state
      .backings
      .get_mut(handle)
      .ok_or(Failure::Object)?
      .object_mut();
    object.load(at, held).map_err(|_| Failure::Object)?;
    let page = page_bytes();
    let mut offset = at;
    for (new, old) in fresh.chunks(page).zip(held.chunks(page)) {
      let page_len = u64::try_from(new.len()).unwrap_or(u64::MAX);
      if new != old {
        let written = if new.iter().all(|byte| *byte == 0) {
          object.zero(offset, page_len)
        } else {
          object.store(offset, new)
        };
        written.map_err(|_| Failure::Object)?;
      }
      offset = offset.saturating_add(page_len);
    }
    Ok(())
  }
}

/// Reads `len` bytes of `object` from `at` into `held` through the bridge, and writes each run where `stored` (the
/// backing's bytes there) differs back into the volume: how many runs were written, or why not.
fn write_back_runs(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  (at, len): (u64, u64),
  (stored, held): (&[u8], &mut Vec<u8>),
) -> Result<WroteBack, Failure> {
  read_whole(bridge, cx, object, (at, len), held).map_err(Failure::Volume)?;
  // A volume shorter than the object's span here (the file cut since) is compared as far as it reaches.
  let runs = differing_runs(stored, held);
  let mut wrote = WroteBack::default();
  for &(from, to) in &runs {
    let bytes = stored.get(from..to).ok_or(Failure::Object)?;
    let offset = at.saturating_add(u64::try_from(from).unwrap_or(u64::MAX));
    bridge
      .write(object, cx, offset, bytes)
      .map_err(Failure::Volume)?;
    wrote.runs = wrote.runs.saturating_add(1);
    wrote.bytes = wrote
      .bytes
      .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
  }
  Ok(wrote)
}

/// What one slice of a write-back wrote into the volume ([`write_back_slice`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WroteBack {
  /// The runs of bytes that differed and were written.
  pub runs: u64,
  /// Their bytes.
  pub bytes: u64,
}

/// Compares `len` bytes of the copy `handle` names, file `inode`'s, from `at` with the volume's, and writes each run
/// that differs into the volume through the bridge, so its scope, rights and every gate in front of a write apply: what
/// it wrote, or why not. The one compare-back every copy another process stores into takes: a guest's DAX backing
/// (A-163's ingest) and a client's writable direct map (A-186). The copy is suppressed in the store while its bytes are
/// written, so the write boundary does not copy them back into it over a store made since; every other backing of the
/// file takes them. The volume is read as far as the file reaches now, so a run past an end another path cut is never
/// written back. `stored` and `held` are the caller's buffers, kept across slices so a slice allocates nothing new.
pub fn write_back_slice(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  (handle, inode): (Handle<ShardsBacking>, u64),
  (at, len): (u64, u64),
  (stored, held): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<WroteBack, Failure> {
  stored.clear();
  stored.resize(usize::try_from(len).map_err(|_| Failure::Object)?, 0);
  {
    let state = bridge.dax_state().ok_or(Failure::Object)?;
    state
      .backings
      .get_mut(handle)
      .ok_or(Failure::Object)?
      .object_mut()
      .load(at, stored)
      .map_err(|_| Failure::Object)?;
    state.backings.suppress(Some(handle));
  }
  let wrote = write_back_runs(
    bridge,
    cx,
    ObjectId::new(inode, 0),
    (at, len),
    (stored, held),
  );
  if let Some(state) = bridge.dax_state() {
    state.backings.suppress(None);
  }
  wrote
}

/// Reads `len` bytes of `object` from `at` into `out`, cleared first, in as many calls as the bridge's read cap takes,
/// stopping short only at the file's end. A bridge answers at most its cap a call (the volume bridge's, 256 KiB), so a
/// single call reads a slice past the cap short; a short call is the file's end only when it reads nothing. Taken for
/// the end, a capped call lost a guest's stores past it at an ingest and zeroed its map past it at a refill, on every
/// machine whose copy slice is longer than the cap
/// (`docs/bugs/2026-10-25-a-dax-slice-past-the-read-cap-dropped-a-guests-stores.md`). Bounded by `len`: each call reads
/// at least a byte or ends the loop. Also what a cache write-back's stale bytes are compared with (A-248, the FUSE
/// transports' `judge_writeback` reader).
pub fn read_whole(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  object: ObjectId,
  (at, len): (u64, u64),
  out: &mut Vec<u8>,
) -> Result<(), VfsError> {
  out.clear();
  let want = usize::try_from(len).unwrap_or(usize::MAX);
  while out.len() < want {
    let before = out.len();
    let offset = at.saturating_add(u64::try_from(before).unwrap_or(u64::MAX));
    let rest = u32::try_from(want.saturating_sub(before)).unwrap_or(u32::MAX);
    bridge.read(object, cx, offset, rest, out)?;
    if out.len() == before {
      break;
    }
  }
  Ok(())
}

/// Moves an ingest on to its next inode.
fn next_inode(ingest: &mut Ingest) {
  ingest.inodes.pop_front();
  ingest.begun = false;
  ingest.ranges.clear();
}

/// Resolves the ask's node through the bridge and checks it may be backed: its inode and size.
fn checked(bridge: &mut dyn Bridge, cx: &OpContext, ask: Ask) -> Result<(u64, u64), Refused> {
  let inode = if ask.node == ROOT_NODE {
    bridge.root(cx).map_err(Refused::Unreachable)?
  } else {
    ask.node
  };
  let attr = bridge
    .getattr(ObjectId::new(inode, 0), cx)
    .map_err(Refused::Unreachable)?;
  match attr.kind {
    Kind::File => {}
    Kind::Dir => return Err(Refused::NotAFile { directory: true }),
    _ => return Err(Refused::NotAFile { directory: false }),
  }
  if ask.writable && !cx.rights.write {
    return Err(Refused::ReadOnly);
  }
  Ok((inode, attr.size))
}

/// Copies file `inode` up through the bridge before a writable map of it is filled: its size afterwards, the witnessed
/// file's.
fn witnessed(bridge: &mut dyn Bridge, cx: &OpContext, inode: u64) -> Result<u64, Refused> {
  let object = ObjectId::new(inode, 0);
  bridge
    .witness_for_writable_map(object, cx)
    .map_err(Refused::Witness)?;
  bridge
    .getattr(object, cx)
    .map(|attr| attr.size)
    .map_err(Refused::Unreachable)
}

/// One slice of the range `pending` is on: read from the volume (holes skipped), stored into the object (pages of zeros
/// skipped), and the range placed and finished when it is done: how its placement came out, if it finished. A placement
/// the OS refuses the range's view for fails the slice, the range not yet finished, so the caller leaves it uncleared as
/// for any failed slice (A-419).
fn fill_slice(
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  handle: Handle<ShardsBacking>,
  pending: &mut Fill,
  slice_bytes: u64,
) -> Result<Option<Placement>, Refused> {
  let range_start = pending.next.saturating_mul(DAX_RANGE_BYTES);
  let range_bytes = pending
    .size
    .saturating_sub(range_start)
    .min(DAX_RANGE_BYTES);
  let at = range_start.saturating_add(pending.progress);
  let want = range_bytes
    .saturating_sub(pending.progress)
    .min(slice_bytes);
  let object = ObjectId::new(pending.inode, 0);
  // A hole needs no fill: skip to the range's next data, if any is before its end.
  let data = bridge
    .seek(object, cx, at, true)
    .map_err(|_| Refused::Object)?;
  let range_end = range_start.saturating_add(range_bytes);
  let read = match data {
    Some(data) if data < range_end => {
      let mut bytes = Vec::new();
      let size = u32::try_from(want.min(range_end.saturating_sub(data))).unwrap_or(u32::MAX);
      bridge
        .read(object, cx, data, size, &mut bytes)
        .map_err(|_| Refused::Object)?;
      Some((data, bytes))
    }
    _ => None,
  };
  match read {
    Some((data, bytes)) if !bytes.is_empty() => {
      store_pages(bridge, handle, data, &bytes)?;
      pending.progress = data
        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
        .saturating_sub(range_start);
      if pending.progress >= range_bytes {
        return finish_range(bridge, handle, pending).map(Some);
      }
      Ok(None)
    }
    // No more data in the range, or the file ended early: the rest of the range reads zeros already.
    _ => finish_range(bridge, handle, pending).map(Some),
  }
}

/// Places the range a fill has just copied, then moves the fill to its next range (the one done stays marked filled):
/// how the placement came out (§4.6, the backed route's L1; A-193). Placed once the range is filled, never before: a
/// fill that fails part-way leaves its range uncleared (A-419), and a huge page placed in it would stay resident whole
/// through its clear, since a placed range is zeroed by stores. The collapse copies the pages the fill stored.
fn finish_range(
  bridge: &mut dyn Bridge,
  handle: Handle<ShardsBacking>,
  pending: &mut Fill,
) -> Result<Placement, Refused> {
  let state = bridge.dax_state().ok_or(Refused::Object)?;
  let placement = state
    .backings
    .place(handle, pending.next)
    .map_err(|_| Refused::Object)?;
  pending.next = pending.next.saturating_add(1);
  pending.progress = 0;
  Ok(placement)
}

/// Stores the pages of `bytes`, the file's from `at`, into the backing's object, skipping pages of zeros: an untouched
/// page reads zero already, so a page of zeros needs no memory.
fn store_pages(
  bridge: &mut dyn Bridge,
  handle: Handle<ShardsBacking>,
  at: u64,
  bytes: &[u8],
) -> Result<(), Refused> {
  let page = page_bytes();
  let page_len = u64::try_from(page).unwrap_or(u64::MAX);
  let state = bridge.dax_state().ok_or(Refused::Object)?;
  let target = state.backings.get_mut(handle).ok_or(Refused::Object)?;
  let mut offset = at;
  for chunk in bytes.chunks(page) {
    if !chunk.iter().all(|byte| *byte == 0) {
      target
        .object_mut()
        .store(offset, chunk)
        .map_err(|_| Refused::Object)?;
    }
    offset = offset.saturating_add(page_len);
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do: compute the ranges a fill covers for asks with and without a range, within, across and past a file's end.
  /// Expect every range an asked range touches, past the end too; the whole file's without one; and nothing for an empty
  /// ask or an empty file's whole. Serves Shards part 3, step 2, and the past-the-end rule
  /// (`docs/bugs/2026-10-25-a-range-mapped-past-the-files-end-missed-later-writes.md`).
  #[test]
  fn a_fill_covers_every_range_an_ask_touches_and_a_whole_file_ask_the_file() {
    let range = DAX_RANGE_BYTES;
    assert_eq!(covered(None, 5 * range), (0, 4));
    assert_eq!(covered(None, 5 * range + 1), (0, 5));
    assert_eq!(covered(Some((2 * range, range)), 10 * range), (2, 2));
    assert_eq!(covered(Some((2 * range, 3 * range)), 10 * range), (2, 4));
    assert_eq!(
      covered(Some((2 * range, 3 * range)), 3 * range),
      (2, 4),
      "a range across the file's end covers its ranges past it"
    );
    assert_eq!(
      covered(Some((4 * range, range)), 3 * range),
      (4, 4),
      "a range wholly past the end is covered: a guest with a stale size maps it"
    );
    let (first, last) = covered(Some((4 * range, 0)), 10 * range);
    assert!(first > last, "an empty ask covers nothing");
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

  /// Shape: the store's page in the fill tests.
  const PAGE: usize = 4096;
  /// Shape: the pages the store's region maps: room for the files the fill tests write.
  const REGION_PAGES: usize = 2048;
  /// Shape: a journal that holds every record the fill tests make.
  const JOURNAL: usize = 1 << 16;
  /// Shape: the fill budget, in ranges: more than any fill test counts.
  const BUDGET_RANGES: u64 = 8;
  /// Shape: the bytes one fill slice copies: a host page on any host these tests run on (4 KiB to 16 KiB), so a fill of
  /// a range is part-way after its first slice.
  const SLICE: u64 = 16 * 1024;
  /// Shape: the bytes the fill tests write into a range: four slices.
  const WRITTEN: usize = 4 * 16 * 1024;
  /// Format: the byte the fill tests' files hold.
  const BYTE: u8 = 0x41;

  /// A store with the fill budget set, and a plain volume in it of prefix 1.
  fn store_and_volume() -> (slates_vfs::volume::Store, slates_vfs::volume::Volume) {
    let mut arena = slates_mem::arena::ChunkArena::new(PAGE);
    arena
      .add_region(slates_mem::region::Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
      .unwrap();
    let mut store = slates_vfs::volume::Store::new(
      &slates_vfs::volume::StoreConfig {
        page: PAGE,
        cache_line: 128,
        max_dirs: 64,
        max_inodes: 256,
        max_chunks: REGION_PAGES,
        max_dir_blocks: 64,
        dir_cutover: 16,
      },
      arena,
      0,
    );
    store
      .shards_backings
      .set_budget(BUDGET_RANGES * DAX_RANGE_BYTES);
    let volume = slates_vfs::volume::Volume::create(
      &mut store,
      slates_vfs::volume::VolumeConfig {
        prefix: 1,
        names: slates_vfs::names::NameEquivalence::Exact,
        quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
        journal_bytes: JOURNAL,
        clock: Box::new(slates_vfs::clock::HostClock::default()),
      },
    )
    .unwrap();
    (store, volume)
  }

  /// A read-write context of an attachment to the volume `id` names.
  fn context(id: slates_db::catalog::VolumeId) -> OpContext {
    let mut attachments = crate::Attachments::new();
    let attachment = attachments
      .attach(
        id,
        crate::View::Current,
        slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        crate::Rights {
          read: true,
          write: true,
        },
      )
      .unwrap();
    attachments.context(attachment).unwrap()
  }

  /// The id the fill tests' bridges serve their volume as.
  fn volume_id() -> slates_db::catalog::VolumeId {
    slates_db::catalog::VolumeId { bytes: [0; 16] }
  }

  /// Asks range `range` of file `node`, read-only, through `session`.
  fn ask_range(
    session: &mut DaxBackingSession,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    node: u64,
    range: u64,
  ) {
    let asked = Ask {
      node,
      writable: false,
      range: Some((range * DAX_RANGE_BYTES, DAX_RANGE_BYTES)),
    };
    assert_eq!(session.ask(bridge, cx, asked), Ok(node), "the ask is taken");
  }

  /// Runs the session's pending fill to its end.
  fn fill_whole(session: &mut DaxBackingSession, bridge: &mut dyn Bridge, cx: &OpContext) {
    loop {
      match session.fill_step(bridge, cx, SLICE) {
        FillStep::Filling => {}
        FillStep::Done(_) => return,
        other => panic!("the fill did not complete: {other:?}"),
      }
    }
  }

  /// `len` bytes of file `node`'s object from `at`, read through the store as slates reads it.
  fn object_bytes(
    store: &mut slates_vfs::volume::Store,
    session: &DaxBackingSession,
    node: u64,
    (at, len): (u64, usize),
  ) -> Vec<u8> {
    let handle = session.backing_of(node).unwrap();
    let mut bytes = vec![0u8; len];
    store
      .shards_backings
      .get_mut(handle)
      .unwrap()
      .object_mut()
      .load(at, &mut bytes)
      .unwrap();
    bytes
  }

  /// How a test leaves a fill part-way, after its first slice.
  #[derive(Clone, Copy, Debug)]
  enum PartWay {
    /// The bridge refuses the next slice.
    Refused,
    /// The transport gives the fill up ([`DaxBackingSession::abandon_fill`]: the Shards route's step when the
    /// attachment's bridge is gone).
    Abandoned,
  }

  /// The fill tests' common course: a file whose range `range` holds [`WRITTEN`] bytes of [`BYTE`]; the range asked and
  /// filled one slice, then left part-way as `how` says; zeros written over those bytes through the bridge, as another
  /// path writes them; the range asked again and filled whole. Returns the store, the session and the file's node, and
  /// what the object held over the bytes written after the first slice (the non-vacuity read: the part-way fill did
  /// store bytes).
  fn left_part_way_then_zeroed(
    range: u64,
    how: PartWay,
  ) -> (slates_vfs::volume::Store, DaxBackingSession, u64, Vec<u8>) {
    let (mut store, mut volume) = store_and_volume();
    let cx = context(volume_id());
    let at = range * DAX_RANGE_BYTES;
    let mut session = DaxBackingSession::new();
    let node = {
      let mut bridge = crate::VolumeBridge::new(volume_id(), &mut volume, &mut store);
      let root = bridge.root(&cx).unwrap();
      let (file, _) = bridge
        .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
        .unwrap();
      bridge
        .write(ObjectId::new(file.ino, 0), &cx, at, &[BYTE; WRITTEN])
        .unwrap();
      if range > 0 {
        // The ranges before it filled whole first, so the part-way range is counted beyond the backing's floor.
        ask_range(&mut session, &mut bridge, &cx, file.ino, 0);
        fill_whole(&mut session, &mut bridge, &cx);
      }
      ask_range(&mut session, &mut bridge, &cx, file.ino, range);
      assert!(
        matches!(
          session.fill_step(&mut bridge, &cx, SLICE),
          FillStep::Filling
        ),
        "the first slice is filled and the fill goes on"
      );
      file.ino
    };
    let first_slice = object_bytes(
      &mut store,
      &session,
      node,
      (at, usize::try_from(SLICE).unwrap()),
    );
    {
      let mut bridge = crate::VolumeBridge::new(volume_id(), &mut volume, &mut store);
      match how {
        PartWay::Refused => {
          // A context of another volume's attachment, which the bridge refuses as it refuses any read it cannot serve:
          // the stand-in for a disk's I/O error beneath an overlay part-way through a fill.
          let foreign = context(slates_db::catalog::VolumeId { bytes: [1; 16] });
          assert!(
            matches!(
              session.fill_step(&mut bridge, &foreign, SLICE),
              FillStep::Refused(Refused::Object)
            ),
            "the bridge refuses the next slice, and the fill with it"
          );
        }
        PartWay::Abandoned => {
          let state = bridge.dax_state().unwrap();
          session.abandon_fill(state.backings);
        }
      }
      bridge
        .write(ObjectId::new(node, 0), &cx, at, &[0; WRITTEN])
        .unwrap();
      ask_range(&mut session, &mut bridge, &cx, node, range);
      fill_whole(&mut session, &mut bridge, &cx);
    }
    (store, session, node, first_slice)
  }

  /// §4.6, the backed route's fill (A-163; A-419): a fill refused part-way leaves its range holding nothing a later fill
  /// of it shows. Do: write a range of a file with `A`; ask the range, read-only, and fill one slice; have the bridge
  /// refuse the next slice; write zeros over those bytes through the bridge, as another path writes them; ask the range
  /// again and fill it whole. Expect the object to read zeros there, as the file does. Before A-419 the slice the
  /// refused fill stored still read `A` after the second fill: the refusal unmarked the range, so the zeros were never
  /// written through, and a fill skips the volume's holes and pages of zeros, so the stale page stayed.
  #[test]
  fn a_fill_refused_part_way_leaves_nothing_a_later_fill_shows() {
    let (mut store, session, node, first_slice) = left_part_way_then_zeroed(0, PartWay::Refused);
    assert!(
      first_slice.iter().all(|byte| *byte == BYTE),
      "the refused fill's first slice stored the file's bytes"
    );
    assert!(
      object_bytes(&mut store, &session, node, (0, WRITTEN))
        .iter()
        .all(|byte| *byte == 0),
      "the object shows the file's zeros, not the bytes the refused fill stored"
    );
  }

  /// §4.6 (A-419): a fill given up part-way, as the Shards route gives one up when its attachment's bridge is gone,
  /// leaves its range as a refused fill does. Do: as the refused case, but give the fill up after its first slice. Expect
  /// the object to read zeros where the file was zeroed.
  #[test]
  fn a_fill_abandoned_part_way_leaves_nothing_a_later_fill_shows() {
    let (mut store, session, node, first_slice) = left_part_way_then_zeroed(0, PartWay::Abandoned);
    assert!(
      first_slice.iter().all(|byte| *byte == BYTE),
      "the abandoned fill's first slice stored the file's bytes"
    );
    assert!(
      object_bytes(&mut store, &session, node, (0, WRITTEN))
        .iter()
        .all(|byte| *byte == 0),
      "the object shows the file's zeros, not the bytes the abandoned fill stored"
    );
  }

  /// §4.6, the fill budget (A-419): what a part-way fill stored is counted until its range is filled again or the
  /// backing goes, so the backings never hold more than the budget counts. Do: fill a file's first range whole; ask its
  /// second, fill one slice, have the bridge refuse the next; read the count; ask the second range again and fill it
  /// whole; read the count; let every backing go. Expect two ranges counted after the refusal (the first filled, the
  /// second holding the slice it stored), two after the second fill, which clears the range rather than count it again,
  /// and none once let go. Before A-419 the refusal gave the second range's charge back while its slice stayed stored.
  #[test]
  fn a_part_way_fill_stays_counted_until_its_range_is_filled_again() {
    let (mut store, mut volume) = store_and_volume();
    let cx = context(volume_id());
    let foreign = context(slates_db::catalog::VolumeId { bytes: [1; 16] });
    let mut session = DaxBackingSession::new();
    let mut bridge = crate::VolumeBridge::new(volume_id(), &mut volume, &mut store);
    let root = bridge.root(&cx).unwrap();
    let (file, _) = bridge
      .create(ObjectId::new(root, 0), &cx, "f", 0o644, 0)
      .unwrap();
    bridge
      .write(
        ObjectId::new(file.ino, 0),
        &cx,
        DAX_RANGE_BYTES,
        &[BYTE; WRITTEN],
      )
      .unwrap();
    ask_range(&mut session, &mut bridge, &cx, file.ino, 0);
    fill_whole(&mut session, &mut bridge, &cx);
    ask_range(&mut session, &mut bridge, &cx, file.ino, 1);
    assert!(matches!(
      session.fill_step(&mut bridge, &cx, SLICE),
      FillStep::Filling
    ));
    assert!(matches!(
      session.fill_step(&mut bridge, &foreign, SLICE),
      FillStep::Refused(Refused::Object)
    ));
    let charged =
      |bridge: &mut crate::VolumeBridge<'_>| bridge.dax_state().unwrap().backings.charged();
    assert_eq!(
      charged(&mut bridge),
      2 * DAX_RANGE_BYTES,
      "the refused range is still counted: its first slice is stored"
    );
    ask_range(&mut session, &mut bridge, &cx, file.ino, 1);
    fill_whole(&mut session, &mut bridge, &cx);
    assert_eq!(
      charged(&mut bridge),
      2 * DAX_RANGE_BYTES,
      "filled again, the range is counted once"
    );
    let state = bridge.dax_state().unwrap();
    session.release_all(state.backings);
    assert_eq!(charged(&mut bridge), 0, "let go, nothing is counted");
  }
}
