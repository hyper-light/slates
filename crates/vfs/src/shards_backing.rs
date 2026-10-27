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
//! **A same-kernel client's copy** (§4.6, A-171). The direct map hands a client process one file's copy to map read
//! alone ([`BackingObject::Peer`]). It is kept here beside the VMMs' backings, so the one write boundary keeps it equal
//! to the file over its whole length, which is the file's page span when it was mapped: a write, a cut and a growth
//! within it reach the client's map before the change returns. A change past that length reaches no one there; the
//! client maps again to see it. The client cannot write the object, so nothing is ingested from it, and the server
//! refuses the direct map beneath an overlay, so it is never marked stale.
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

use slates_mem::{DAX_RANGE_BYTES, DaxObject, Handle, MemError, PeerObject, Slab};

use crate::ids::InodeNo;

/// Shape: the slots one segment of the backings slab holds; the slab grows a segment at a time, to its capacity.
const SEGMENT_SLOTS: usize = 16;

/// What a backing's bytes live in.
pub enum BackingObject {
  /// A VMM's: sized to the largest file a guest can have (`slates_mem::dax_object_bytes`) and viewed a range at a time
  /// (`slates_mem::DaxObject`).
  Vmm(DaxObject),
  /// A same-kernel client's direct map (A-171): sized to the file's page span when it was mapped, and sealed against
  /// the client (`slates_mem::PeerObject`).
  Peer(PeerObject),
}

impl std::fmt::Debug for BackingObject {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      BackingObject::Vmm(object) => object.fmt(f),
      BackingObject::Peer(object) => object.fmt(f),
    }
  }
}

impl BackingObject {
  /// The object's size in bytes, fixed at its creation: nothing is written through past it.
  #[must_use]
  pub fn len(&self) -> u64 {
    match self {
      BackingObject::Vmm(object) => object.len(),
      BackingObject::Peer(object) => object.len(),
    }
  }

  /// Never: every object holds at least a page. Here for the convention beside [`BackingObject::len`].
  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// Copies `from` into the object at `offset`, through atomics; refused as the object refuses it.
  pub fn store(&mut self, offset: u64, from: &[u8]) -> Result<(), MemError> {
    match self {
      BackingObject::Vmm(object) => object.store(offset, from),
      BackingObject::Peer(object) => object.store(offset, from),
    }
  }

  /// Copies the object's bytes at `offset` into `into`, through atomics; refused as the object refuses it.
  pub fn load(&mut self, offset: u64, into: &mut [u8]) -> Result<(), MemError> {
    match self {
      BackingObject::Vmm(object) => object.load(offset, into),
      BackingObject::Peer(object) => object.load(offset, into),
    }
  }

  /// Makes the object's bytes `offset .. offset + len` read zero; refused as the object refuses it.
  pub fn zero(&mut self, offset: u64, len: u64) -> Result<(), MemError> {
    match self {
      BackingObject::Vmm(object) => object.zero(offset, len),
      BackingObject::Peer(object) => object.zero(offset, len),
    }
  }

  /// The descriptor a peer is handed for the object.
  #[cfg(unix)]
  #[must_use]
  pub fn descriptor(&self) -> std::os::fd::BorrowedFd<'_> {
    match self {
      BackingObject::Vmm(object) => object.descriptor(),
      BackingObject::Peer(object) => object.descriptor(),
    }
  }

  /// A read-only handle of a peer's copy, in this process: what the Windows hand-off duplicates into the client's
  /// (A-175). A VMM's object is handed by [`BackingObject::handed_to_vmm`] instead, so it is refused here.
  #[cfg(windows)]
  pub fn read_only_handle(&self) -> Result<std::os::windows::io::OwnedHandle, MemError> {
    match self {
      BackingObject::Vmm(_) => Err(MemError::OsRefused {
        call: "a read-only handle of a VMM's object",
        code: None,
      }),
      BackingObject::Peer(object) => object.read_only_handle(),
    }
  }

  /// Readies range `range` of a VMM's object for the VMM's map before its ask is answered (Windows commits it; see
  /// `slates_mem::DaxObject::prepare_range`). A peer's copy is whole from its making, so nothing is done for one.
  pub fn prepare_range(&mut self, range: u64) -> Result<(), MemError> {
    match self {
      BackingObject::Vmm(object) => object.prepare_range(range),
      BackingObject::Peer(_) => Ok(()),
    }
  }

  /// What a VMM's ask is answered with: a new descriptor of the object, read-write whatever the ask (HVF maps a shared
  /// range only from a descriptor opened to write, M17; the guest's stores reach the volume only by an ingest, which a
  /// read-only attachment never runs). A peer's copy is never a VMM's, so it is refused.
  #[cfg(unix)]
  pub fn handed_to_vmm(&self, _writable: bool) -> Result<HandedObject, MemError> {
    match self {
      BackingObject::Vmm(object) => {
        object
          .descriptor()
          .try_clone_to_owned()
          .map_err(|e| MemError::OsRefused {
            call: "a VMM's descriptor of a DAX object",
            code: e.raw_os_error(),
          })
      }
      BackingObject::Peer(_) => Err(MemError::OsRefused {
        call: "a VMM's descriptor of a peer's copy",
        code: None,
      }),
    }
  }

  /// What a VMM's ask is answered with: a duplicate of the object's section in this process that maps it read alone,
  /// or read and write where the ask is `writable`, for the transport to duplicate into the VMM's process (Windows, plan
  /// G-shards-windows step 5). A peer's copy is never a VMM's, so it is refused.
  #[cfg(windows)]
  pub fn handed_to_vmm(&self, writable: bool) -> Result<HandedObject, MemError> {
    match self {
      BackingObject::Vmm(object) => object.handed(writable),
      BackingObject::Peer(_) => Err(MemError::OsRefused {
        call: "a VMM's handle of a peer's copy",
        code: None,
      }),
    }
  }
}

/// What a VMM is handed for its object: a descriptor (Unix) or a section handle in this process (Windows).
#[cfg(unix)]
pub type HandedObject = std::os::fd::OwnedFd;
/// What a VMM is handed for its object: a descriptor (Unix) or a section handle in this process (Windows).
#[cfg(windows)]
pub type HandedObject = std::os::windows::io::OwnedHandle;

/// One file's backing for one holder: its object, the ranges filled from the volume, the ranges asked writable, and
/// whether the disk has changed the file beneath it since.
pub struct ShardsBacking {
  object: BackingObject,
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
  /// A VMM's backing of file `inode` of the volume of prefix `volume` over `object`, nothing filled or asked writable
  /// yet.
  #[must_use]
  pub fn new(object: DaxObject, volume: u16, inode: InodeNo) -> ShardsBacking {
    ShardsBacking::over(BackingObject::Vmm(object), volume, inode)
  }

  /// A same-kernel client's backing of file `inode` of the volume of prefix `volume` over `object` (A-171), nothing
  /// filled yet. A client maps it read alone, so no range is ever asked writable and nothing is ingested from it.
  #[must_use]
  pub fn for_peer(object: PeerObject, volume: u16, inode: InodeNo) -> ShardsBacking {
    ShardsBacking::over(BackingObject::Peer(object), volume, inode)
  }

  fn over(object: BackingObject, volume: u16, inode: InodeNo) -> ShardsBacking {
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
  pub fn object(&self) -> &BackingObject {
    &self.object
  }

  /// Its object, to fill, load or zero.
  pub fn object_mut(&mut self) -> &mut BackingObject {
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

/// Why a same-kernel client's backing was not kept ([`ShardsBackings::insert_peer`]); nothing was kept or counted.
#[derive(Debug)]
pub enum PeerRefusal {
  /// The fill budget has no room for the object.
  Budget {
    /// The object's size.
    object_bytes: u64,
    /// What the backings counted already.
    charged: u64,
    /// The most they may count.
    budget: u64,
  },
  /// The registry is at its capacity.
  Full(MemError),
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
  /// Same-kernel clients' backings held, and their objects' bytes (A-171): what a released or reaped map gives back.
  peers: usize,
  peer_bytes: u64,
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
      .field("peers", &self.peers)
      .field("peer_bytes", &self.peer_bytes)
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
      peers: 0,
      peer_bytes: 0,
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

  /// Keeps a same-kernel client's backing of file `inode` of the volume of prefix `volume` over `object` (A-171), and
  /// counts the whole object against the budget: every byte of it is filled and every change within it written
  /// through. Refused, with nothing kept or counted, when the budget has no room or the registry is full.
  pub fn insert_peer(
    &mut self,
    object: PeerObject,
    volume: u16,
    inode: InodeNo,
  ) -> Result<Handle<ShardsBacking>, PeerRefusal> {
    let object_bytes = object.len();
    if !self.charge(object_bytes) {
      return Err(PeerRefusal::Budget {
        object_bytes,
        charged: self.charged,
        budget: self.budget,
      });
    }
    match self.insert(ShardsBacking::for_peer(object, volume, inode)) {
      Ok(handle) => {
        self.peers = self.peers.saturating_add(1);
        self.peer_bytes = self.peer_bytes.saturating_add(object_bytes);
        Ok(handle)
      }
      Err(full) => {
        self.uncharge(object_bytes);
        Err(PeerRefusal::Full(full))
      }
    }
  }

  /// How many same-kernel clients' backings are held, and their objects' bytes.
  #[must_use]
  pub fn peers_held(&self) -> (usize, u64) {
    (self.peers, self.peer_bytes)
  }

  /// The object's bytes of the same-kernel client's backing `handle` names, if it is one and live: what it counts
  /// against its client's share.
  #[must_use]
  pub fn peer_bytes(&self, handle: Handle<ShardsBacking>) -> Option<u64> {
    match &self.slab.get(handle).ok()?.object {
      BackingObject::Peer(object) => Some(object.len()),
      BackingObject::Vmm(_) => None,
    }
  }

  /// Detaches every same-kernel client's backing of a file of the volume of prefix `volume`: the volume's teardown. A
  /// detached backing is written through no more, since a prefix is handed to a new volume once its count wraps and a
  /// copy left indexed would take that volume's writes into a client's map of a file it never mapped. It stays held,
  /// and its object's bytes stay counted, until its client releases it or is reaped
  /// ([`ShardsBackings::remove`]): the client's own map keeps the object's pages, which this process wrote. How many.
  pub fn detach_volume_peers(&mut self, volume: u16) -> usize {
    let files: Vec<(u16, InodeNo)> = self
      .by_file
      .range((volume, InodeNo(0))..=(volume, InodeNo(u64::MAX)))
      .map(|(file, _)| *file)
      .collect();
    files
      .into_iter()
      .map(|file| self.detach_peers_of(file))
      .fold(0usize, usize::saturating_add)
  }

  /// Detaches every same-kernel client's backing of file `inode` of the volume of prefix `volume`: the file has left
  /// its volume for good (its last link and its last open gone), so nothing writes it again. Held and counted as
  /// [`ShardsBackings::detach_volume_peers`] leaves them. How many.
  pub fn detach_file_peers(&mut self, volume: u16, inode: InodeNo) -> usize {
    self.detach_peers_of((volume, inode))
  }

  /// Takes the same-kernel clients' backings of `file` out of the index write-through walks, leaving a VMM's.
  fn detach_peers_of(&mut self, file: (u16, InodeNo)) -> usize {
    let ShardsBackings { slab, by_file, .. } = self;
    let Some(handles) = by_file.get_mut(&file) else {
      return 0;
    };
    let before = handles.len();
    handles.retain(|handle| {
      slab
        .get(*handle)
        .is_ok_and(|backing| matches!(backing.object, BackingObject::Vmm(_)))
    });
    let detached = before.saturating_sub(handles.len());
    if handles.is_empty() {
      by_file.remove(&file);
    }
    detached
  }

  /// Lets go of the backing `handle` names, if it is live: it, its object's mappings in this process ending with it. A
  /// same-kernel client's gives its object's bytes back to the budget, which [`ShardsBackings::insert_peer`] counted; a
  /// VMM's charge is its session's to give back.
  pub fn remove(&mut self, handle: Handle<ShardsBacking>) -> Option<ShardsBacking> {
    let backing = self.slab.remove(handle).ok()?;
    if let BackingObject::Peer(object) = &backing.object {
      self.uncharge(object.len());
      self.peers = self.peers.saturating_sub(1);
      self.peer_bytes = self.peer_bytes.saturating_sub(object.len());
    }
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

  /// Marks the one backing `handle` names stale, if it is live: its guest's view moved to other content (a snapshot
  /// device's advance), so its object is refilled before its guest sees anything more.
  pub fn mark_backing_stale(&mut self, handle: Handle<ShardsBacking>) {
    if let Ok(backing) = self.slab.get_mut(handle) {
      backing.stale = true;
      self.marked_stale = self.marked_stale.saturating_add(1);
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
  /// within a range it has filled and within its object, counting what it wrote and what the OS refused. A client's
  /// object is the file's page span when it was mapped, so a change past it is nobody's to see there (A-171).
  fn each_filled_span(
    &mut self,
    file: (u16, InodeNo),
    from: u64,
    to: u64,
    mut apply: impl FnMut(&mut BackingObject, u64, u64) -> bool,
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
      let object_end = object.len();
      for &range in filled.range(first..=last) {
        let range_start = range.saturating_mul(DAX_RANGE_BYTES);
        let start = from.max(range_start);
        let end = to
          .min(range_start.saturating_add(DAX_RANGE_BYTES))
          .min(object_end);
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
