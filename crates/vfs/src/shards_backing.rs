//! The Shards route's DAX backings (§4.6; Shards part 3, steps 2–4; `docs/wip/shards/dax-protocol.md`, agreed with the
//! Shards owner 2026-10-24). They are owned by the shard's [`Store`](crate::volume::Store), so every change of a backed
//! file's content reaches its backings at the volume's single write boundary, before the change returns to any
//! transport (step 4).
//!
//! A backing is one file's DAX object for one attachment (`slates_mem::DaxObject`). It holds the file's bytes at the
//! file's own offsets, filled from the volume a range at a time as a guest asks for them, and records the ranges the
//! guest asked to write.
//!
//! **A file is named by its volume and its inode number together.** A number names a file only within a volume: a clone
//! shares its origin's nodes and their numbers, and a snapshot a device presents is such a clone, in the same store. So
//! the registry keeps backings by (volume prefix, inode), where the prefix is the volume's own
//! ([`crate::volume::Volume::prefix`]), never the one an inherited number carries. Keyed by the number alone, a clone's
//! write reached its origin's guest's map (`docs/bugs/2026-10-25-a-clones-change-reaches-its-origins-dax-backing.md`). The server's Shards serve task makes, fills and ingests backings and lets them go, holding
//! their handles. This module keeps them and writes changes through:
//! - **A write's bytes** go into every backing of its file, over the ranges filled. So a guest reading its map
//!   afterwards sees them. An unfilled range has them anyway: it is filled from the volume, which has them, when
//!   asked.
//! - **A size change** zeroes what it cuts or opens, so a store a guest made past the old end is never taken for data
//!   ([`ShardsBackings::zero_through`]).
//! - **A change that shifts bytes** (the SDK's edit) or replaces them beneath (a base file's drift) has its filled
//!   ranges copied again by the volume ([`ShardsBackings::filled_spans_from`]).
//!
//! Writes go through the object's own mapping, never `write(2)`, which would wake Shards' inotify watch once per call.
//! A backing whose ingest is writing the guest's own stores into the volume is suppressed meanwhile
//! ([`ShardsBackings::suppress`]). Writing those bytes back into it would overwrite any store the guest made after the
//! ingest read them. A write-through the OS refuses is counted, the change itself standing.
//!
//! **Beneath an overlay** (D-25) the disk can change a file's bytes without any write: an untouched file follows the
//! disk. The base plane marks the file's backings stale where it takes such a change ([`ShardsBackings::mark_stale`]):
//! an outsider's edit adopted, another file now at the name, a `rewitness`. The server refills them before anything
//! more reaches their guest. A backing a guest maps to write is never of an untouched file, because the server copies a
//! file up before it answers a writable ask. From then on the volume serves its witness or refuses `BaseDrift`, so a
//! write-back never takes an outsider's change for the guest's. `rewitness` refuses a file mapped to write
//! ([`ShardsBackings::mapped_writable`]).
//!
//! Bounded by the slab's capacity: the shard's chunk count, as the in-process DAX registry's. The registry also keeps
//! its fill budget ([`ShardsBackings::set_budget`]), which the server sets from the shard's content reserve: a backing
//! counts a range for each range it has filled, and at least one. A range is longer than a chunk, so the budget refuses
//! long before the slab is full. Until the budget is set it is zero, and nothing is backed.

use std::collections::{BTreeMap, BTreeSet};

use slates_mem::{DAX_RANGE_BYTES, DaxObject, Handle, MemError, Slab};

use crate::ids::InodeNo;

/// Shape: the slots one segment of the backings slab holds; the slab grows a segment at a time, to its capacity.
const SEGMENT_SLOTS: usize = 16;

/// One file's DAX backing for one attachment: its object, the ranges filled from the volume, the ranges asked
/// writable, and whether the disk has changed the file beneath it since.
pub struct ShardsBacking {
  object: DaxObject,
  /// The prefix of the volume whose file it backs: the attachment's volume's own.
  volume: u16,
  inode: InodeNo,
  filled: BTreeSet<u64>,
  writable: BTreeSet<u64>,
  stale: bool,
}

impl std::fmt::Debug for ShardsBacking {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardsBacking")
      .field("volume", &self.volume)
      .field("inode", &self.inode)
      .field("filled", &self.filled.len())
      .field("writable", &self.writable.len())
      .field("stale", &self.stale)
      .finish()
  }
}

impl ShardsBacking {
  /// A backing of file `inode` of the volume of prefix `volume` over `object`, nothing filled or asked writable yet.
  #[must_use]
  pub fn new(object: DaxObject, volume: u16, inode: InodeNo) -> ShardsBacking {
    ShardsBacking {
      object,
      volume,
      inode,
      filled: BTreeSet::new(),
      writable: BTreeSet::new(),
      stale: false,
    }
  }

  /// The prefix of the volume whose file it backs.
  #[must_use]
  pub fn volume(&self) -> u16 {
    self.volume
  }

  /// The file it backs, within its volume.
  #[must_use]
  pub fn inode(&self) -> InodeNo {
    self.inode
  }

  /// Its object.
  #[must_use]
  pub fn object(&self) -> &DaxObject {
    &self.object
  }

  /// Its object, to fill, load or zero.
  pub fn object_mut(&mut self) -> &mut DaxObject {
    &mut self.object
  }

  /// The ranges filled (or being filled), by index (offset / [`DAX_RANGE_BYTES`]).
  #[must_use]
  pub fn filled(&self) -> &BTreeSet<u64> {
    &self.filled
  }

  /// Marks range `range` filled: from now on write-through reaches it.
  pub fn mark_filled(&mut self, range: u64) {
    self.filled.insert(range);
  }

  /// Unmarks range `range`: a fill that did not complete.
  pub fn unmark_filled(&mut self, range: u64) {
    self.filled.remove(&range);
  }

  /// The ranges a mapping asked to write, by index: what an ingest compares.
  #[must_use]
  pub fn writable(&self) -> &BTreeSet<u64> {
    &self.writable
  }

  /// Marks range `range` asked writable.
  pub fn mark_writable(&mut self, range: u64) {
    self.writable.insert(range);
  }

  /// Whether the disk has changed the file beneath the object since it was filled or last refilled.
  #[must_use]
  pub fn stale(&self) -> bool {
    self.stale
  }

  /// Takes the stale mark as a refill begins, so a change the disk makes during the refill marks it again.
  pub fn take_stale(&mut self) -> bool {
    std::mem::take(&mut self.stale)
  }
}

/// The shard's Shards DAX backings, by handle, and which backings each file has, by (volume prefix, inode).
pub struct ShardsBackings {
  slab: Slab<ShardsBacking>,
  by_file: BTreeMap<(u16, InodeNo), Vec<Handle<ShardsBacking>>>,
  /// The backing an ingest is writing from, which write-through passes over meanwhile.
  suppressed: Option<Handle<ShardsBacking>>,
  /// Bytes written through, monotone: the non-vacuity counter a test asserts moved.
  written: u64,
  /// Write-throughs the OS refused, monotone.
  refused: u64,
  /// Backings marked stale, monotone: the non-vacuity counter of the overlay's refill.
  marked_stale: u64,
  /// The bytes the backings count against the budget: a range for each range each has filled, at least one each.
  charged: u64,
  /// The most they may count: the shard's content reserve, set by the server ([`ShardsBackings::set_budget`]).
  budget: u64,
}

impl std::fmt::Debug for ShardsBackings {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ShardsBackings")
      .field("files", &self.by_file.len())
      .field("written", &self.written)
      .field("refused", &self.refused)
      .field("marked_stale", &self.marked_stale)
      .field("charged", &self.charged)
      .field("budget", &self.budget)
      .finish()
  }
}

impl ShardsBackings {
  /// An empty registry of at most `cap` backings.
  #[must_use]
  pub fn new(cap: usize) -> ShardsBackings {
    ShardsBackings {
      slab: Slab::new(SEGMENT_SLOTS, cap),
      by_file: BTreeMap::new(),
      suppressed: None,
      written: 0,
      refused: 0,
      marked_stale: 0,
      charged: 0,
      budget: 0,
    }
  }

  /// Sets the most the backings may count against the budget, in bytes.
  pub fn set_budget(&mut self, budget: u64) {
    self.budget = budget;
  }

  /// The most the backings may count, in bytes.
  #[must_use]
  pub fn budget(&self) -> u64 {
    self.budget
  }

  /// What the backings count now, in bytes.
  #[must_use]
  pub fn charged(&self) -> u64 {
    self.charged
  }

  /// Counts `bytes` more against the budget, if it has room: whether it did. Refused, nothing is counted.
  pub fn charge(&mut self, bytes: u64) -> bool {
    match self
      .charged
      .checked_add(bytes)
      .filter(|after| *after <= self.budget)
    {
      Some(after) => {
        self.charged = after;
        true
      }
      None => false,
    }
  }

  /// Gives `bytes` back to the budget.
  pub fn uncharge(&mut self, bytes: u64) {
    self.charged = self.charged.saturating_sub(bytes);
  }

  /// Keeps `backing`; its handle. Refused at the registry's capacity.
  pub fn insert(&mut self, backing: ShardsBacking) -> Result<Handle<ShardsBacking>, MemError> {
    let file = (backing.volume, backing.inode);
    let handle = self.slab.insert(backing)?;
    self.by_file.entry(file).or_default().push(handle);
    Ok(handle)
  }

  /// Lets go of the backing `handle` names, if it is live: it, its object's mappings in this process ending with it.
  pub fn remove(&mut self, handle: Handle<ShardsBacking>) -> Option<ShardsBacking> {
    let backing = self.slab.remove(handle).ok()?;
    let file = (backing.volume, backing.inode);
    if let Some(handles) = self.by_file.get_mut(&file) {
      handles.retain(|held| *held != handle);
      if handles.is_empty() {
        self.by_file.remove(&file);
      }
    }
    if self.suppressed == Some(handle) {
      self.suppressed = None;
    }
    Some(backing)
  }

  /// The backing `handle` names, if it is live.
  #[must_use]
  pub fn get(&self, handle: Handle<ShardsBacking>) -> Option<&ShardsBacking> {
    self.slab.get(handle).ok()
  }

  /// The backing `handle` names, if it is live, mutably.
  pub fn get_mut(&mut self, handle: Handle<ShardsBacking>) -> Option<&mut ShardsBacking> {
    self.slab.get_mut(handle).ok()
  }

  /// Whether file `inode` of the volume of prefix `volume` has any backing: the write boundary's one check, a map
  /// lookup, on files no guest maps.
  #[must_use]
  pub fn backs(&self, volume: u16, inode: InodeNo) -> bool {
    self.by_file.contains_key(&(volume, inode))
  }

  /// Has write-through pass over backing `handle` (an ingest writing the guest's stores from it into the volume), or
  /// over none.
  pub fn suppress(&mut self, handle: Option<Handle<ShardsBacking>>) {
    self.suppressed = handle;
  }

  /// Bytes written through so far.
  #[must_use]
  pub fn written(&self) -> u64 {
    self.written
  }

  /// Write-throughs the OS refused so far.
  #[must_use]
  pub fn refused(&self) -> u64 {
    self.refused
  }

  /// Backings marked stale so far.
  #[must_use]
  pub fn marked_stale(&self) -> u64 {
    self.marked_stale
  }

  /// Marks every backing of file `inode` of the volume of prefix `volume` stale: the disk changed its bytes without a
  /// write (an untouched file of an overlay follows the disk), so their objects are refilled before their guests see
  /// anything more.
  pub fn mark_stale(&mut self, volume: u16, inode: InodeNo) {
    let Some(handles) = self.by_file.get(&(volume, inode)) else {
      return;
    };
    for &handle in handles {
      if let Ok(backing) = self.slab.get_mut(handle) {
        backing.stale = true;
        self.marked_stale = self.marked_stale.saturating_add(1);
      }
    }
  }

  /// Whether a guest maps any part of file `inode` of the volume of prefix `volume` to write: a backing of it with a
  /// range asked writable.
  #[must_use]
  pub fn mapped_writable(&self, volume: u16, inode: InodeNo) -> bool {
    self.by_file.get(&(volume, inode)).is_some_and(|handles| {
      handles.iter().any(|handle| {
        self
          .slab
          .get(*handle)
          .is_ok_and(|backing| !backing.writable.is_empty())
      })
    })
  }

  /// Writes `bytes`, file `inode`'s from `at`, of the volume of prefix `volume`, into each of its backings over the
  /// ranges they have filled.
  pub fn write_through(&mut self, volume: u16, inode: InodeNo, at: u64, bytes: &[u8]) {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    self.each_filled_span(
      (volume, inode),
      at,
      at.saturating_add(len),
      |object, from, to| {
        let start = usize::try_from(from.saturating_sub(at)).unwrap_or(usize::MAX);
        let end = usize::try_from(to.saturating_sub(at)).unwrap_or(usize::MAX);
        match bytes.get(start..end) {
          Some(piece) => object.store(from, piece).is_ok(),
          None => false,
        }
      },
    );
  }

  /// Zeroes the bytes `from .. to` of file `inode` of the volume of prefix `volume` in each of its backings over the
  /// ranges they have filled: what a size change cuts or opens.
  pub fn zero_through(&mut self, volume: u16, inode: InodeNo, from: u64, to: u64) {
    self.each_filled_span((volume, inode), from, to, |object, start, end| {
      object.zero(start, end.saturating_sub(start)).is_ok()
    });
  }

  /// The spans of the filled ranges from `from` on of file `inode` of the volume of prefix `volume`, across its
  /// backings, in order and each once: what the volume copies again when a change shifts or replaces the file's bytes
  /// from `from`.
  #[must_use]
  pub fn filled_spans_from(&self, volume: u16, inode: InodeNo, from: u64) -> Vec<(u64, u64)> {
    let first = from.checked_div(DAX_RANGE_BYTES).unwrap_or(0);
    let mut ranges = BTreeSet::new();
    for &handle in self
      .by_file
      .get(&(volume, inode))
      .map(Vec::as_slice)
      .unwrap_or(&[])
    {
      if let Ok(backing) = self.slab.get(handle) {
        ranges.extend(backing.filled.range(first..).copied());
      }
    }
    ranges
      .into_iter()
      .map(|range| {
        let start = range.saturating_mul(DAX_RANGE_BYTES);
        (start.max(from), start.saturating_add(DAX_RANGE_BYTES))
      })
      .collect()
  }

  /// Runs `apply` on each backing of `file` (volume prefix, inode) but the suppressed one, for each part of `from .. to`
  /// within a range it has filled, counting what it wrote and what the OS refused.
  fn each_filled_span(
    &mut self,
    file: (u16, InodeNo),
    from: u64,
    to: u64,
    mut apply: impl FnMut(&mut DaxObject, u64, u64) -> bool,
  ) {
    if to <= from {
      return;
    }
    // The fields are borrowed apart, so the walk allocates nothing on the write path.
    let ShardsBackings {
      slab,
      by_file,
      suppressed,
      written,
      refused,
      ..
    } = self;
    let Some(handles) = by_file.get(&file) else {
      return;
    };
    let first = from.checked_div(DAX_RANGE_BYTES).unwrap_or(0);
    let last = to
      .saturating_sub(1)
      .checked_div(DAX_RANGE_BYTES)
      .unwrap_or(0);
    for &handle in handles {
      if *suppressed == Some(handle) {
        continue;
      }
      let Ok(backing) = slab.get_mut(handle) else {
        continue;
      };
      let ShardsBacking { object, filled, .. } = backing;
      for &range in filled.range(first..=last) {
        let range_start = range.saturating_mul(DAX_RANGE_BYTES);
        let start = from.max(range_start);
        let end = to.min(range_start.saturating_add(DAX_RANGE_BYTES));
        if end <= start {
          continue;
        }
        if apply(object, start, end) {
          *written = written.saturating_add(end.saturating_sub(start));
        } else {
          *refused = refused.saturating_add(1);
        }
      }
    }
  }
}
