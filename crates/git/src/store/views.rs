//! The store as the push path reads it (A-638; §4.17.2 "A push", items 3 and 4, and "Pieces (git)", pieces 5 and 6;
//! §4.17.1 "A push is a work"): two views of one repository's objects in the shared content store, each lending an
//! object's bytes as the store keeps them when it keeps them in one window.
//!
//! **Thin-pack bases and connectivity** ([`IndexedObjects`], receive-pack's [`receive::Repository`]). A thin pack's
//! reference deltas name bases the client knows the repository holds, and the pack reader takes them from here; the
//! connectivity check stops its walk at any object held. Both read the index alone: an object a push holds in
//! quarantine has passed no check, so it is never a base and never ends a walk. `holds` reads the object's row and no
//! bytes. An object of at most one window is that window in the store (slates-vcs, "One window at most"), so it is lent
//! as kept, with no copy. A longer one is assembled from its windows once and kept for the view's life, since the trait
//! lends `&[u8]` for as long as the view is borrowed, then lent again from there to every delta that names it. What is
//! kept is charged to the view's room before it is assembled; a base past the room is answered as not held, and the
//! view names the refusal ([`IndexedObjects::refused`]), so the caller can tell it from a base the repository lacks.
//!
//! **A push's trees and blobs** ([`PushedObjects`], A-618's [`PushObjects`]). A push becomes a work by reading its tree
//! against its base version's, by id: the index first, then the objects the push holds in quarantine, the store's own
//! order. A tree's entries take their kinds from their modes as git's tree walk reads them
//! ([`Entry::canonical_mode`](crate::objects::tree::Entry::canonical_mode)), so a legacy `100664` is a plain file and
//! a mode of no known type a submodule's link. A tree is parsed and its entries listed within the store's room, as the
//! store parses one; a blob of one window is lent as kept, and a longer one assembled for the caller, which charged its
//! length to the work's room before asking (A-618).
//!
//! **Counters** ([`ViewCounters`]): the objects lent as kept, assembled, and lent again. The tests read them, so a dead
//! path that lends without a copy, or a dead reuse, cannot pass for a working one.

use std::borrow::Cow;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::mem::size_of;

use slates_merge::push::{EntryKind, ObjectKey, PushObjects, TreeEntry, Unavailable};
use slates_vcs::{ContentId, ContentStore};

use crate::object_name::{Kind, ObjectId};
use crate::objects::Refusal;
use crate::objects::tree::{Mode, Tree};
use crate::receive;
use crate::{Admission, PastAdmission, to_u64};

use super::naming::kind_of;
use super::push::Push;
use super::repository::Repository;

/// What a view lent, by how (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewCounters {
  /// Objects lent as the store keeps them, in one window: no copy.
  pub lent_as_kept: u64,
  /// Objects assembled from several windows.
  pub assembled: u64,
  /// Objects lent again from what the view had kept.
  pub lent_again: u64,
}

/// A repository's indexed objects as receive-pack reads them: a thin pack's bases and the connectivity check's stopping
/// points (module docs).
pub struct IndexedObjects<'a> {
  /// The repository.
  repository: &'a Repository,
  /// The store its objects are in.
  store: &'a ContentStore,
  /// What the view may still keep.
  admission: Cell<Admission>,
  /// The first base refused for the room.
  refused: Cell<Option<PastAdmission>>,
  /// The bases assembled and kept.
  kept: Kept,
  /// What the view has lent.
  counters: Cell<ViewCounters>,
}

impl<'a> IndexedObjects<'a> {
  /// A view of `repository`'s indexed objects in `store`, keeping at most `room` bytes of assembled bases: the caller's
  /// admission for the push (§4.17.2 "A push").
  pub fn new(repository: &'a Repository, store: &'a ContentStore, room: u64) -> Self {
    Self {
      repository,
      store,
      admission: Cell::new(Admission::new(room)),
      refused: Cell::new(None),
      kept: Kept::default(),
      counters: Cell::new(ViewCounters::default()),
    }
  }

  /// What the view has lent.
  pub fn counters(&self) -> ViewCounters {
    self.counters.get()
  }

  /// The first base the view refused to keep for its room, if any: the pack reader was answered that the repository
  /// lacks it, and this says why.
  pub fn refused(&self) -> Option<PastAdmission> {
    self.refused.get()
  }

  /// Charges `bytes` to the view's room: whether it had them. The first refusal is kept.
  fn charge(&self, bytes: u64) -> bool {
    let mut admission = self.admission.get();
    match admission.charge(bytes) {
      Ok(()) => {
        self.admission.set(admission);
        true
      }
      Err(past) => {
        if self.refused.get().is_none() {
          self.refused.set(Some(past));
        }
        false
      }
    }
  }
}

impl receive::Repository for IndexedObjects<'_> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    if let Some(kept) = self.kept.get(id) {
      count(&self.counters, |counters| &mut counters.lent_again);
      return Some(kept);
    }
    let (row, content) = self.repository.indexed(self.store, id)?;
    // A window the store lends is lent again with no copy; one it copies out is kept below, as an assembled object is.
    if let Some(Cow::Borrowed(window)) = as_kept(self.store, &content, row.len) {
      count(&self.counters, |counters| &mut counters.lent_as_kept);
      return Some((row.kind, window));
    }
    if !self.charge(row.len.saturating_add(kept_overhead())) {
      return None;
    }
    let bytes = self.store.object_bytes(&content, row.len)?;
    count(&self.counters, |counters| &mut counters.assembled);
    self.kept.keep(*id, row.kind, bytes)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.repository.object(self.store, id).is_some()
  }
}

/// A push's objects above its repository's, as A-618 reads a pushed tree against its base version's (module docs).
pub struct PushedObjects<'a> {
  /// The repository.
  repository: &'a Repository,
  /// The store its objects and the push's are in.
  store: &'a ContentStore,
  /// The push, its objects in quarantine.
  push: &'a Push,
  /// What the view has lent.
  counters: Cell<ViewCounters>,
}

impl<'a> PushedObjects<'a> {
  /// A view of `push`'s objects above `repository`'s, in `store`.
  pub fn new(repository: &'a Repository, store: &'a ContentStore, push: &'a Push) -> Self {
    Self {
      repository,
      store,
      push,
      counters: Cell::new(ViewCounters::default()),
    }
  }

  /// What the view has lent.
  pub fn counters(&self) -> ViewCounters {
    self.counters.get()
  }

  /// The object `key` names: its type, its content's root and its length, from the index first and then the push.
  fn locate(&self, key: ObjectKey) -> Result<(Kind, ContentId, u64), Unavailable> {
    let format = self.repository.format();
    let id = ObjectId::from_bytes(format, key.as_bytes()).ok_or(Unavailable::Missing)?;
    if let Some((row, content)) = self.repository.indexed(self.store, &id) {
      return Ok((row.kind, content, row.len));
    }
    let (number, content, len) = self.push.object(&id).ok_or(Unavailable::Missing)?;
    let kind = kind_of(number).ok_or(Unavailable::Malformed)?;
    Ok((kind, content, len))
  }

  /// An object's bytes: as kept when they are one window, otherwise assembled from their windows.
  fn bytes(&self, content: &ContentId, len: u64) -> Result<Cow<'a, [u8]>, Unavailable> {
    if let Some(window) = as_kept(self.store, content, len) {
      count(&self.counters, |counters| &mut counters.lent_as_kept);
      return Ok(window);
    }
    let bytes =
      self
        .store
        .object_bytes(content, len)
        .ok_or_else(|| Unavailable::LengthDiffers {
          said: len,
          given: stored_len(self.store, content),
        })?;
    count(&self.counters, |counters| &mut counters.assembled);
    Ok(Cow::Owned(bytes))
  }
}

impl PushObjects for PushedObjects<'_> {
  fn tree(&self, id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable> {
    let (kind, content, len) = self.locate(id)?;
    if kind != Kind::Tree {
      return Err(Unavailable::NotATree);
    }
    let bytes = self.bytes(&content, len)?;
    let mut admission = Admission::new(self.store.room());
    let tree = Tree::parse(bytes.as_ref(), self.repository.format(), &mut admission)
      .map_err(|refused| unreadable(refused.refusal))?;
    let entries = tree.entries();
    admission
      .charge_items::<TreeEntry>(entries.len())
      .and_then(|()| admission.charge(to_u64(bytes.len())))
      .map_err(past_room)?;
    entries
      .iter()
      .map(|entry| {
        Ok(TreeEntry {
          name: entry.name.to_vec(),
          kind: entry_kind(entry.canonical_mode()),
          id: ObjectKey::new(entry.id).ok_or(Unavailable::Malformed)?,
        })
      })
      .collect()
  }

  fn blob_len(&self, id: ObjectKey) -> Result<u64, Unavailable> {
    let (kind, _, len) = self.locate(id)?;
    if kind != Kind::Blob {
      return Err(Unavailable::NotABlob);
    }
    Ok(len)
  }

  fn blob(&self, id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    let (kind, content, len) = self.locate(id)?;
    if kind != Kind::Blob {
      return Err(Unavailable::NotABlob);
    }
    self.bytes(&content, len)
  }
}

/// Adds one to the counter `which` picks.
fn count(counters: &Cell<ViewCounters>, which: impl FnOnce(&mut ViewCounters) -> &mut u64) {
  let mut all = counters.get();
  let counter = which(&mut all);
  *counter = counter.saturating_add(1);
  counters.set(all);
}

/// An object's bytes as the store keeps them when they are one window (slates-vcs: an object of at most one window is
/// that window), lent with no copy where the store's storage lends it and copied out where it cannot (A-727); `None`
/// when they span several.
fn as_kept<'s>(store: &'s ContentStore, root: &ContentId, len: u64) -> Option<Cow<'s, [u8]>> {
  let mut windows = store.windows(root);
  let first = windows.next()?;
  (windows.next().is_none() && to_u64(first.len()) == len).then_some(first)
}

/// The bytes the store gives for `root`, counted: what a read of an object's windows found.
fn stored_len(store: &ContentStore, root: &ContentId) -> u64 {
  store.windows(root).fold(0_u64, |total, window| {
    total.saturating_add(to_u64(window.len()))
  })
}

/// A tree the store gave whose listing git's tree walk cannot read, or which is past the store's room.
fn unreadable(refusal: Refusal) -> Unavailable {
  match refusal {
    Refusal::PastAdmission(past) => past_room(past),
    _ => Unavailable::Malformed,
  }
}

/// A listing refused for the store's room.
fn past_room(past: PastAdmission) -> Unavailable {
  Unavailable::PastRoom {
    asked: past.asked,
    room: past.left,
  }
}

/// A tree entry's kind in a work, from its mode as git's tree walk reads it.
fn entry_kind(mode: Mode) -> EntryKind {
  match mode {
    Mode::File => EntryKind::File,
    Mode::Executable => EntryKind::Executable,
    Mode::Symlink => EntryKind::Symlink,
    Mode::Directory => EntryKind::Directory,
    Mode::Gitlink => EntryKind::Submodule,
  }
}

/// One kept base: its type and bytes, placed once.
type Slot = OnceCell<(Kind, Box<[u8]>)>;

/// What keeping one assembled base costs beside its bytes. Derived: its slot, twice over, since the segments double
/// and the last may be half empty, and its entry in the slot map.
fn kept_overhead() -> u64 {
  let slot = size_of::<Slot>().saturating_mul(2);
  to_u64(slot.saturating_add(size_of::<(ObjectId, usize)>()))
}

/// Bases longer than a window, assembled and kept for the view's life (module docs): segments of slots, each twice the
/// one before, so a kept base never moves once placed and finding one walks the O(log n) segments before it. Every base
/// kept was charged to the view's room first, so the slots are bounded by the room over a window.
#[derive(Default)]
struct Kept {
  /// The first segment, of one slot.
  first: OnceCell<Box<Segment>>,
  /// Each kept base's slot, by id.
  slots: RefCell<BTreeMap<ObjectId, usize>>,
}

/// One segment of [`Kept`]: its slots, then the next segment.
struct Segment {
  /// The slots.
  slots: Box<[Slot]>,
  /// The next segment, of twice the slots.
  next: OnceCell<Box<Segment>>,
}

impl Segment {
  /// A segment of `slots` empty slots.
  fn of(slots: usize) -> Box<Segment> {
    Box::new(Segment {
      slots: (0..slots).map(|_| OnceCell::new()).collect(),
      next: OnceCell::new(),
    })
  }
}

impl Kept {
  /// The slot numbered `slot`: segment `k`, of `2^k` slots, holds slots `2^k - 1` to `2^(k+1) - 2`.
  fn slot(&self, slot: usize) -> Option<&Slot> {
    let ordinal = slot.checked_add(1)?;
    let depth = ordinal.checked_ilog2()?;
    let within = ordinal.checked_sub(1_usize.checked_shl(depth)?)?;
    let mut segment = self.first.get_or_init(|| Segment::of(1));
    for _ in 0..depth {
      let twice = segment.slots.len().saturating_mul(2);
      segment = segment.next.get_or_init(|| Segment::of(twice));
    }
    segment.slots.get(within)
  }

  /// The base kept as `id`, if any.
  fn get(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    let slot = *self.slots.try_borrow().ok()?.get(id)?;
    let (kind, bytes) = self.slot(slot)?.get()?;
    Some((*kind, bytes))
  }

  /// Keeps `bytes` as `id`'s, a `kind`, in the next slot, and lends them.
  fn keep(&self, id: ObjectId, kind: Kind, bytes: Vec<u8>) -> Option<(Kind, &[u8])> {
    let mut slots = self.slots.try_borrow_mut().ok()?;
    let next = slots.len();
    let slot = self.slot(next)?;
    slot.set((kind, bytes.into_boxed_slice())).ok()?;
    slots.insert(id, next);
    drop(slots);
    let (kind, bytes) = slot.get()?;
    Some((*kind, bytes))
  }
}

#[cfg(test)]
#[path = "../tests/store/views.rs"]
mod tests;
