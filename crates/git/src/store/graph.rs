//! The store read as upload-pack's commit graph (A-640 part 1; §4.17.2 "A fetch"): A-614's [`Repository`] answering
//! A-615's [`Graph`], so the negotiation reads a repository's rows, versions and D(k) without either module naming the
//! other's internals.
//!
//! **What it reads.** A node is an object's position, the store's own handle: its id is its position row, its kind,
//! length and commit row (generation number, committer date, root tree, parents by position) its object row. A tree's
//! entries and a tag's object are read from the object's bytes by A-590's parsers, as the store's own walks read them,
//! a submodule's commit left out (no object of this repository). A green's versions are the store's chain: version `v`
//! is the chain's `v`-th, numbered from 0, D(v) the positions it brought, and the greens a commit heads the store's own
//! head map. Whether a version fast-forwarded the one before is the verdict the store gave its move at admission
//! (`Move::is_fast_forward`), found again by the store's rule: both heads are commits, and the later is the earlier or
//! reaches it, searched back through the commit rows no lower than the earlier head's generation. A tag moved onto
//! itself is no fast-forward, as the store judged it.
//!
//! **Refs.** The store holds greens, not names: the caller (the server, which keeps the repository's record) names each
//! ref's green. Every ref's head is a tip; the refs under `refs/tags/` whose head is an annotated tag are the tags
//! `include-tag` reads.
//!
//! **Positions met again** (A-911; goal 93, main 01a20182's sample of a partial clone: 22,771 of 23,095 samples under
//! [`StoreGraph::named`], clone-again's 71,056,607 content reads for 425,179 objects). A tree's entries are named by
//! id, and each becomes a position through the index, a lookup across its runs. A walk opens each tree once, but a
//! tree's versions repeat most of its entries: a root tree's unchanged entries are met again in every commit's root.
//! So the ids met are resolved through a [`PositionCache`], exact and of a fixed size, owned by the answer across its
//! steps and lent to each step's graph: a direct-mapped table of (id, position) slots, sized from the work as the walk
//! meets it (the entries one sent commit's trees resolve, the root tree's first), so its cost is the walk's working
//! set, never an entry an object (§4.17.1 "A fetch's answer" rule 1). Charged to the answer's admission as it grows;
//! with no room it stays as it is, down to no slots, where every lookup misses: the same code, the walk as slow as
//! before, never refused for it.
//!
//! **Faults.** The trait reads cannot fail, and a store whose index and content agree never fails them. Where one does
//! (a position with no row, a tree or tag git's parser refuses), the read answers nothing and the first fault is kept
//! ([`StoreGraph::fault`]); the service refuses an answer made while one was kept, so a broken store never yields a
//! wrong pack.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use slates_vcs::ContentStore;

use super::abbrev::Abbrev;
use super::{Chain, Place, Repository, StoreRefused};
use crate::negotiation::{CommitRow, Graph};
use crate::object_name::{Kind, ObjectId};
use crate::positions::Positions;
use crate::store::rows::ObjectRow;
use crate::{Admission, PastAdmission};

/// Format: what every tag ref's name begins with.
const TAGS: &[u8] = b"refs/tags/";

/// A ref, as the caller names it: its full name and its green.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NamedRef<'n> {
  /// Its full name (`refs/heads/main`).
  pub name: &'n [u8],
  /// Its green in the store.
  pub green: u64,
}

/// A read the store could not make while it was read as a graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphFault {
  /// A position with no rows.
  NoRow {
    /// The position.
    position: u64,
  },
  /// The store refused a read (an object's bytes, a tree or tag git's parser refuses).
  Store(StoreRefused),
}

/// A repository's store, and the refs that name its greens, read as the negotiation's [`Graph`].
pub struct StoreGraph<'r> {
  /// The repository.
  repository: &'r Repository,
  /// The null name of the repository's format, what a node with no rows is read as (and kept as a fault).
  null: ObjectId,
  /// The content its objects are in.
  store: &'r ContentStore,
  /// The refs, each naming its green.
  refs: &'r [NamedRef<'r>],
  /// The first read that failed, if any.
  fault: Cell<Option<GraphFault>>,
  /// The positions of ids met lately (module docs, **Positions met again**): none of its own unless lent one.
  positions: RefCell<PositionCache>,
}

/// Derived: the slots a [`PositionCache`] gives each id one sent commit resolves, at least. A direct-mapped table of `m`
/// slots holding `n` keys loses about `1 − e^(−n/m)` of them to a slot another took (each key's slot is uniform: an id
/// is a uniform hash); at a load of at most 1/8 that is under 12 % of the ids met again, and the slots, rounded up to a
/// power of two, hold the load between 1/16 and 1/8.
const SLOTS_AN_ID: u64 = 8;

/// The positions of the ids a walk met lately (A-911, module docs **Positions met again**): exact, direct-mapped, an id
/// in the slot its first bytes name. Sized from the work, as the walk meets it: the most ids one sent commit's trees
/// resolved, times [`SLOTS_AN_ID`], to a power of two. It costs `slots × size_of::<Slot>()` (48 bytes a slot), charged
/// to the answer's admission as it grows, so at tens of thousands of concurrent fetches its memory is bounded by theirs:
/// an answer whose room cannot take the next size keeps the size it has, none at first, and is counted
/// ([`PositionCounts::unroomed`]).
#[derive(Debug, Default)]
pub struct PositionCache {
  /// The slots: an id and its position, or none.
  slots: Vec<Option<(ObjectId, u64)>>,
  /// The ids resolved since the walk's last sent commit began.
  met: u64,
  /// The most ids one sent commit resolved.
  most: u64,
  /// Whether a size the work asked for was refused room (counted once an answer).
  unroomed: bool,
  /// What it did since it was last asked.
  counts: PositionCounts,
}

/// What a [`PositionCache`] did: the ids it resolved through the index (a lookup each), those it answered itself, and
/// whether a size the work asked was refused room.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PositionCounts {
  /// Ids resolved through the index.
  pub lookups: u64,
  /// Ids the cache answered.
  pub hits: u64,
  /// Of the ids resolved through the index, those whose slot held another id, which this one then put out. An upper
  /// bound on the ids lost to another's slot, not their count: an id met the first time also finds its slot taken once
  /// the cache is warm (A-911's 40-commit line: 59 of 157 lookups counted at 4 ways or at 32 slots an id, where nearly
  /// every miss is a first meeting). The A/B of more ways against more slots reads lookups and time.
  pub conflicts: u64,
  /// Answers whose cache was refused the room its work asked (one an answer).
  pub unroomed: u64,
}

impl PositionCache {
  /// The slot `id` lives in: its first bytes, under the slots' mask. With no slots, none.
  fn slot_of(&self, id: &ObjectId) -> usize {
    let mut first = [0u8; 8];
    for (to, from) in first.iter_mut().zip(id.as_bytes()) {
      *to = *from;
    }
    let mask = self.slots.len().wrapping_sub(1);
    usize::try_from(u64::from_le_bytes(first)).unwrap_or(usize::MAX) & mask
  }

  /// The position of `id`: the slot's, where it holds `id`, else `resolve`'s, kept in its slot.
  fn position(&mut self, id: &ObjectId, resolve: impl FnOnce() -> Option<u64>) -> Option<u64> {
    self.met = self.met.saturating_add(1);
    let at = self.slot_of(id);
    if let Some(Some((held, position))) = self.slots.get(at)
      && held == id
    {
      self.counts.hits = self.counts.hits.saturating_add(1);
      return Some(*position);
    }
    self.counts.lookups = self.counts.lookups.saturating_add(1);
    if matches!(self.slots.get(at), Some(Some(_))) {
      self.counts.conflicts = self.counts.conflicts.saturating_add(1);
    }
    let position = resolve()?;
    if let Some(slot) = self.slots.get_mut(at) {
      *slot = Some((*id, position));
    }
    Some(position)
  }

  /// A sent commit's trees begin: the ids the last one resolved measured, and the cache grown to what that asks.
  fn commit_begun(&mut self, admission: &mut Admission) {
    self.most = self.most.max(self.met);
    self.met = 0;
    self.grow_within(admission);
  }

  /// Grows to the size the work has asked so far, charged to `admission`; refused room, it keeps its size, counted once.
  fn grow_within(&mut self, admission: &mut Admission) {
    let asked = self
      .most
      .saturating_mul(SLOTS_AN_ID)
      .checked_next_power_of_two();
    let Some(asked) = asked.and_then(|slots| usize::try_from(slots).ok()) else {
      return;
    };
    let have = self.slots.len();
    if asked <= have {
      return;
    }
    let more = u64::try_from(asked.saturating_sub(have))
      .unwrap_or(u64::MAX)
      .saturating_mul(
        u64::try_from(core::mem::size_of::<Option<(ObjectId, u64)>>()).unwrap_or(u64::MAX),
      );
    if admission.charge(more).is_err() {
      if !self.unroomed {
        self.unroomed = true;
        self.counts.unroomed = self.counts.unroomed.saturating_add(1);
      }
      return;
    }
    let held = core::mem::replace(&mut self.slots, vec![None; asked]);
    for (id, position) in held.into_iter().flatten() {
      let at = self.slot_of(&id);
      if let Some(slot) = self.slots.get_mut(at) {
        *slot = Some((id, position));
      }
    }
  }

  /// What it did since this was last asked.
  pub fn take_counts(&mut self) -> PositionCounts {
    core::mem::take(&mut self.counts)
  }
}

impl<'r> StoreGraph<'r> {
  /// `repository`'s objects in `store`, with `refs` naming its greens.
  pub fn new(
    repository: &'r Repository,
    store: &'r ContentStore,
    refs: &'r [NamedRef<'r>],
  ) -> StoreGraph<'r> {
    StoreGraph {
      repository,
      store,
      null: ObjectId::null(repository.format()),
      refs,
      fault: Cell::new(None),
      positions: RefCell::new(PositionCache::default()),
    }
  }

  /// This graph, resolving ids through `positions` (module docs, **Positions met again**).
  pub fn with_positions(mut self, positions: PositionCache) -> StoreGraph<'r> {
    self.positions = RefCell::new(positions);
    self
  }

  /// What its cache of positions did since this was last asked (A-911).
  pub fn take_position_counts(&self) -> PositionCounts {
    self.positions.borrow_mut().take_counts()
  }

  /// The positions it resolved through, to keep for the answer's next step.
  pub fn into_positions(self) -> PositionCache {
    self.positions.into_inner()
  }

  /// The first read that failed, if any: an answer made while one did is refused.
  pub fn fault(&self) -> Option<GraphFault> {
    self.fault.get()
  }

  /// The object `green`'s current version names, by position: `None` for a green with no version, or one deleted.
  pub fn head(&self, green: u64) -> Option<u64> {
    self.chain(green).and_then(Chain::head)
  }

  /// Keeps `fault` when it is the first.
  fn failed(&self, fault: GraphFault) {
    if self.fault.get().is_none() {
      self.fault.set(Some(fault));
    }
  }

  /// The id and row of the object at `position`.
  fn row(&self, position: u64) -> Option<(ObjectId, ObjectRow)> {
    let found = self
      .repository
      .id_at(self.store, position)
      .and_then(|id| self.repository.object(self.store, &id).map(|row| (id, row)));
    if found.is_none() {
      self.failed(GraphFault::NoRow { position });
    }
    found
  }

  /// The id of the object at `position`, and its row and content, read once each (A-911 (b): its id's row was read
  /// twice); a fault where either is missing.
  fn indexed_at(&self, position: u64) -> Option<(ObjectId, (ObjectRow, slates_vcs::ContentId))> {
    let found = self.repository.id_at(self.store, position).and_then(|id| {
      self
        .repository
        .indexed(self.store, &id)
        .map(|indexed| (id, indexed))
    });
    if found.is_none() {
      self.failed(GraphFault::NoRow { position });
    }
    found
  }

  /// The objects the object at `position` names, read from its bytes: a tree's entries or a tag's object.
  fn named(&self, position: u64) -> Vec<u64> {
    let Some((id, (row, content))) = self.indexed_at(position) else {
      return Vec::new();
    };
    let kind = row.kind;
    let place = Place::Indexed { row, content };
    let named = match kind {
      Kind::Tree => self.repository.tree_references(self.store, &id, &place),
      Kind::Tag => self
        .repository
        .tag_reference(self.store, &id, &place)
        .map(|target| vec![target]),
      Kind::Commit | Kind::Blob => Ok(Vec::new()),
    };
    match named {
      Ok(ids) => ids.iter().filter_map(|id| self.position_of(id)).collect(),
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        Vec::new()
      }
    }
  }

  /// The entries of the tree at `position`, each with its name, read from its bytes: none when it is no tree or the
  /// store cannot read it (kept as a fault).
  fn named_entries_of(&self, position: u64) -> Vec<(u64, Vec<u8>)> {
    let Some((id, (row, content))) = self.indexed_at(position) else {
      return Vec::new();
    };
    if row.kind != Kind::Tree {
      return Vec::new();
    }
    let place = Place::Indexed { row, content };
    match self
      .repository
      .tree_named_references(self.store, &id, &place)
    {
      Ok(named) => named
        .into_iter()
        .filter_map(|(id, name)| self.position_of(&id).map(|position| (position, name)))
        .collect(),
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        Vec::new()
      }
    }
  }

  /// The bytes of the object `id` names when it is of `kind`, its length charged to `admission` before they are read
  /// (A-640 part 4: the blob a `sparse:oid` filter reads its patterns from; a commit or a tag git describes when it
  /// lists an abbreviation's objects): `None` when the store holds no object of that name and kind, or cannot read it
  /// (kept as a fault).
  pub(crate) fn object_bytes(
    &self,
    id: &ObjectId,
    kind: Kind,
    admission: &mut Admission,
  ) -> Result<Option<Vec<u8>>, PastAdmission> {
    let Some((row, content)) = self.repository.indexed(self.store, id) else {
      return Ok(None);
    };
    if row.kind != kind {
      return Ok(None);
    }
    admission.charge(row.len)?;
    let place = Place::Indexed { row, content };
    match self.repository.bytes_of(self.store, id, &place) {
      Ok(bytes) => Ok(Some(bytes.into_owned())),
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        Ok(None)
      }
    }
  }

  /// The objects whose names begin with `abbrev`'s digits, in name order, each by name, position and kind (A-640 part
  /// 4: an abbreviated name's objects, `Repository::abbreviated`).
  pub(crate) fn abbreviated(
    &self,
    abbrev: &Abbrev,
  ) -> impl Iterator<Item = (ObjectId, u64, Kind)> + use<'r> {
    self
      .repository
      .abbreviated(self.store, abbrev)
      .map(|(id, row)| (id, row.position, row.kind))
  }

  /// How many digits of `id`'s name git writes for it with `core.abbrev` unset: the fewest from git's default that no
  /// other object's name begins with (A-640 part 4, `Repository::unique_digits`).
  pub(crate) fn unique_digits(&self, id: &ObjectId) -> usize {
    self
      .repository
      .unique_digits(self.store, id, self.repository.default_digits())
  }

  /// What git's tree walk finds for `path` in the tree at `position` (A-640 part 4: a `sparse:oid` filter's
  /// `<rev>:<path>`, walked as `find_tree_entry` walks it), a submodule's commit an entry like any other: the entry's
  /// object and the path left below it. `None` when the walk finds nothing or the object at `position` is no tree; a
  /// tree the store cannot read is kept as a fault.
  pub(crate) fn tree_entry<'p>(
    &self,
    position: u64,
    path: &'p [u8],
  ) -> Option<(ObjectId, &'p [u8])> {
    let (id, _) = self.row(position)?;
    let Some((row, content)) = self.repository.indexed(self.store, &id) else {
      self.failed(GraphFault::NoRow { position });
      return None;
    };
    if row.kind != Kind::Tree {
      return None;
    }
    let place = Place::Indexed { row, content };
    match self.repository.tree_entry(self.store, &id, &place, path) {
      Ok(found) => found,
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        None
      }
    }
  }

  /// The position of `id`, a fault when the store does not hold it.
  fn position_of(&self, id: &ObjectId) -> Option<u64> {
    let position = self.positions.borrow_mut().position(id, || {
      self
        .repository
        .object(self.store, id)
        .map(|row| row.position)
    });
    if position.is_none() {
      self.failed(GraphFault::Store(StoreRefused::Missing { id: *id }));
    }
    position
  }

  /// `green`'s chain.
  fn chain(&self, green: u64) -> Option<&'r Chain> {
    self.repository.chains.get(&green)
  }

  /// The head of `green`'s version `version`.
  fn head_of(&self, green: u64, version: u64) -> Option<u64> {
    let index = usize::try_from(version).ok()?;
    self.chain(green)?.versions.get(index)?.head
  }

  /// Whether the commit at `later` is the commit at `earlier` or reaches it, searched back through parents no lower
  /// than `earlier`'s generation; never when either is no commit.
  fn reaches(&self, later: u64, earlier: u64) -> bool {
    let Some(floor) = self.commit(earlier).map(|row| row.generation) else {
      return false;
    };
    let mut seen = BTreeSet::new();
    let mut stack = vec![later];
    while let Some(commit) = stack.pop() {
      if commit == earlier {
        return true;
      }
      let above = self
        .commit(commit)
        .is_some_and(|row| row.generation >= floor);
      if above && seen.insert(commit) {
        stack.extend(self.parents(commit));
      }
    }
    false
  }
}

impl Graph for StoreGraph<'_> {
  type Node = u64;
  type Green = u64;

  fn node(&self, id: &ObjectId) -> Option<u64> {
    self
      .repository
      .object(self.store, id)
      .map(|row| row.position)
  }

  fn id(&self, node: u64) -> ObjectId {
    self.row(node).map_or(self.null, |(id, _)| id)
  }

  fn kind(&self, node: u64) -> Kind {
    // Read from the landing's runs by type, no row read: a walk asks every entry's type (A-722 part 3's partial clone).
    self.repository.kind_at(node).unwrap_or_else(|| {
      self.failed(GraphFault::NoRow { position: node });
      Kind::Blob
    })
  }

  fn commit(&self, node: u64) -> Option<CommitRow<u64>> {
    let (_, row) = self.row(node)?;
    row.commit.map(|commit| CommitRow {
      generation: commit.generation,
      date: commit.date,
      tree: commit.tree,
    })
  }

  fn parents(&self, commit: u64) -> impl Iterator<Item = u64> {
    self
      .row(commit)
      .and_then(|(_, row)| row.commit)
      .map(|commit| commit.parents)
      .unwrap_or_default()
      .into_iter()
  }

  fn entries(&self, tree: u64) -> impl Iterator<Item = u64> {
    self.named(tree).into_iter()
  }

  fn commit_begun(&self, admission: &mut Admission) {
    self.positions.borrow_mut().commit_begun(admission);
  }

  fn named_entries(&self, tree: u64) -> impl Iterator<Item = (u64, Vec<u8>)> {
    self.named_entries_of(tree).into_iter()
  }

  fn size(&self, blob: u64) -> u64 {
    self.row(blob).map_or(0, |(_, row)| row.len)
  }

  fn target(&self, tag: u64) -> Option<u64> {
    self.named(tag).first().copied()
  }

  fn tips(&self) -> impl Iterator<Item = u64> {
    self
      .refs
      .iter()
      .filter_map(|named| self.chain(named.green).and_then(Chain::head))
  }

  fn tags(&self) -> impl Iterator<Item = u64> {
    self
      .refs
      .iter()
      .filter(|named| named.name.starts_with(TAGS))
      .filter_map(|named| self.chain(named.green).and_then(Chain::head))
      .filter(|head| self.kind(*head) == Kind::Tag)
  }

  fn greens_at(&self, head: u64) -> impl Iterator<Item = (u64, u64)> {
    self
      .repository
      .heads
      .get(&head)
      .into_iter()
      .flatten()
      .filter_map(|green| {
        let versions = self.chain(*green)?.versions.len();
        let current = u64::try_from(versions.checked_sub(1)?).ok()?;
        Some((*green, current))
      })
  }

  fn versions_at(&self, green: u64, head: u64) -> impl Iterator<Item = u64> {
    self
      .chain(green)
      .map(|chain| chain.versions.as_slice())
      .unwrap_or_default()
      .iter()
      .enumerate()
      .filter(move |(_, version)| version.head == Some(head))
      .filter_map(|(index, _)| u64::try_from(index).ok())
  }

  fn fast_forward(&self, green: u64, version: u64) -> bool {
    let Some(before) = version.checked_sub(1) else {
      return false;
    };
    match (self.head_of(green, before), self.head_of(green, version)) {
      (Some(earlier), Some(later)) => self.reaches(later, earlier),
      _ => false,
    }
  }

  fn brought(&self, green: u64, version: u64) -> impl Iterator<Item = u64> {
    self
      .brought_set(green, version)
      .iter()
      .collect::<Vec<u64>>()
      .into_iter()
  }

  fn brought_set(&self, green: u64, version: u64) -> Positions {
    let read = usize::try_from(version)
      .map_err(|_| StoreRefused::NoVersion { green, version })
      .and_then(|index| self.repository.brought(self.store, green, index));
    match read {
      Ok(positions) => positions,
      Err(StoreRefused::NoVersion { .. }) => Positions::new(),
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        Positions::new()
      }
    }
  }

  fn tips_from(&self, from: usize) -> impl Iterator<Item = (usize, u64)> {
    self
      .refs
      .iter()
      .enumerate()
      .skip(from)
      .filter_map(|(at, named)| Some((at, self.chain(named.green).and_then(Chain::head)?)))
  }

  fn tags_from(&self, from: usize) -> impl Iterator<Item = (usize, u64)> {
    self
      .refs
      .iter()
      .enumerate()
      .skip(from)
      .filter(|(_, named)| named.name.starts_with(TAGS))
      .filter_map(|(at, named)| Some((at, self.chain(named.green).and_then(Chain::head)?)))
      .filter(|(_, head)| self.kind(*head) == Kind::Tag)
  }
}

#[cfg(test)]
#[path = "../tests/store_graph.rs"]
mod tests;
