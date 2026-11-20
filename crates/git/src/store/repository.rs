//! A git repository's index, versions and walks (A-614; §4.17.2 "The repository in git's terms", items 2 and 3; piece
//! 5, part 1): the store's module docs say what each holds and why.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::mem::size_of;

use slates_vcs::{ContentId, ContentRefused, ContentStore, Index, IndexRefused};

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tag::Tag;
use crate::objects::tree::Tree;
use crate::{Admission, to_u64};

use super::encoding::Encoding;
use super::fragment::{self, FragmentRefused, Learning, Placed, Stored};
use super::hook_view::Quantum;
use super::learn::{Clock, Depth, Exchange, Levels, Rebuild, Window};
use super::naming::{GitNaming, kind_of};
use super::positions::Positions;
use super::push::Push;
use super::rows::{self, CommitRow, ObjectRow};
use reach::Sets;

/// Objects by an abbreviated name (A-640 part 4), a child of this module so it reads the index kept here.
#[path = "abbrev.rs"]
pub(super) mod abbrev;

/// The store read as upload-pack's commit graph (A-640), a child of this module so it reads the rows and chains kept
/// here.
#[path = "graph.rs"]
pub(super) mod graph;

/// A protected branch's questions about a move (A-644): how it changes its ref and what it brings, a child of this
/// module so it reads the walks and chains kept here.
#[path = "protect.rs"]
pub(super) mod protect;

/// A-614's D(k) and reached sets as a type of derived data (A-647 part 2), a child of this module so its derivation
/// walks the store as the admission does.
#[path = "reach.rs"]
pub(super) mod reach;
/// A push sealed and staged a unit at a time (A-722 part 4), a child of this module so it writes the rows kept here.
#[path = "staging.rs"]
pub(super) mod staging;

/// A move admitted a unit at a time (A-722 part 4c), a child of this module so it walks the rows and chains kept here.
#[path = "admitting.rs"]
pub(super) mod admitting;

/// Shape: what one version's record costs the budget beside its set's runs. Derived from the type.
const VERSION_BYTES: usize = size_of::<Version>();

/// Shape: what one run of a set costs the budget. Derived from the type.
const RUN_BYTES: usize = size_of::<(u64, u64)>();

/// Shape: what one entry of the head map costs the budget, a position and a green. Derived from the type.
const HEAD_BYTES: usize = size_of::<(u64, u64)>();

/// Why the store refused. Each changes nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreRefused {
  /// An object a walk or a commit needs is in neither the push nor the repository.
  Missing {
    /// The object.
    id: ObjectId,
  },
  /// An object's bytes or a row it names are not in the store as its row says: the store and the index disagree.
  Unreadable {
    /// The object.
    id: ObjectId,
  },
  /// A tree or a tag git's parser refuses where the store must read what it names.
  Malformed {
    /// The object.
    id: ObjectId,
  },
  /// A commit's parent is no commit.
  NotACommit {
    /// The object.
    id: ObjectId,
  },
  /// A move was admitted before its push was sealed.
  Unsealed,
  /// A move's green is no longer where it was when the move was admitted, or two moves name it.
  Moved {
    /// The green.
    green: u64,
  },
  /// A commit moves no ref.
  NoMove,
  /// Every position is given.
  PositionsExhausted,
  /// A push made for a repository of another object format.
  ForeignPush,
  /// A green has no such version.
  NoVersion {
    /// The green.
    green: u64,
    /// The version asked for.
    version: u64,
  },
  /// A version's sets, its value of the reach sets (A-647, [`super::ReachSets`]), do not read as the type's.
  SetsUnreadable {
    /// The green.
    green: u64,
    /// The version.
    version: u64,
  },
  /// A pack asked for a version whose fragments are not encoded yet.
  NotEncoded {
    /// The version.
    version: u64,
  },
  /// A fragment or a base pack could not be encoded.
  Encoding(FragmentRefused),
  /// A pack of more entries than its header can count.
  PackTooLarge,
  /// A kept fragment's bytes are not in the store.
  FragmentUnreadable,
  /// The pack's trailer could not be made (SHA-1's collision detection refused it).
  TrailerRefused,
  /// An answer named a position the repository never gave (A-639).
  NoPosition {
    /// The position.
    position: u64,
  },
  /// A green's base pack was made again while a stream read it entry by entry (A-639): the owner makes base packs
  /// between streams.
  BaseRebuilt {
    /// The green.
    green: u64,
  },
  /// A stream sent other than the objects its header counted, its repository changed under it (A-639).
  CountChanged {
    /// The objects the header counted.
    counted: u64,
    /// The entries sent.
    sent: u64,
  },
  /// The index refused the rows.
  Index(IndexRefused),
  /// The store refused (the budget).
  Content(ContentRefused),
}

impl From<IndexRefused> for StoreRefused {
  fn from(refused: IndexRefused) -> StoreRefused {
    StoreRefused::Index(refused)
  }
}

impl From<ContentRefused> for StoreRefused {
  fn from(refused: ContentRefused) -> StoreRefused {
    StoreRefused::Content(refused)
  }
}

/// What the store has done, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StoreCounters {
  /// Positions given.
  pub positions: u64,
  /// Objects the walks visited that their green had not reached.
  pub walked: u64,
  /// Objects the walks met already reached, where each stopped.
  pub reached_met: u64,
  /// Of those, trees: each skipped whole with everything it names.
  pub trees_skipped: u64,
  /// Moves found to be fast-forwards.
  pub fast_forwards: u64,
  /// Fast-forward questions decided with no walk (A-644): the two heads one commit, or the new head's generation number
  /// no greater than the old head's, which no descendant's is.
  pub decided_without_walk: u64,
  /// Fast-forward questions decided by a walk back from the new head (A-644).
  pub descent_walks: u64,
  /// Commits a protected branch's checks judged: each commit a move brings, once (A-644).
  pub brought_judged: u64,
  /// Reached sets made again (a create, a rewrite).
  pub remade: u64,
  /// Of those, the ones that started from another green's set.
  pub bases_found: u64,
  /// Bytes the versions' values added to the content store as their commits put them: what the store held new, never
  /// what it held already and was held once more by (A-647 part 4).
  pub value_bytes: u64,
  /// Fragments encoded.
  pub fragments: u64,
  /// Entries written whole.
  pub whole: u64,
  /// Entries written as a delta against an earlier entry.
  pub offset_deltas: u64,
  /// Entries written as a delta against a named object.
  pub reference_deltas: u64,
  /// Base packs made.
  pub rebuilds: u64,
  /// Pushes a commit staged again whole, their stage overtaken by a landing since (A-722 part 4e: a push's judge
  /// stages it again a turn at a time before its append, [`Repository::needs_stage`], so a commit meets none).
  pub restaged: u64,
}

/// The entry a path names in a version's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
  /// Its mode as git wrote it.
  pub mode: u16,
  /// Its object.
  pub id: ObjectId,
}

/// One version of a ref: its head's position (`None` for a deleted ref), its value of the reach sets, and how its
/// reached set followed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
  head: Option<u64>,
  /// Its value of [`super::ReachSets`] (A-647 part 3), held once by the record: the set its head reaches and D(k), what
  /// it brought, which the store reads from it and keeps nowhere else.
  value: ContentId,
  /// Its D(k) encoded, once the owner asked (A-614 part 2).
  fragments: Option<Fragments>,
  /// How its reached set followed, kept so its derivation (A-647 part 2, [`reach`]) reads only its parents' values.
  followed: Followed,
}

/// How a version's reached set followed (A-647 part 2): the move's [`Reach`], with the version a remade set was taken
/// from named, since which green's head a rewrite or a create descends from is found among the greens' heads at its
/// admission, and a derivation reads only recorded history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Followed {
  /// A fast-forward: the green's previous set grown by what the version brought.
  Grown,
  /// A create or a rewrite: made again from the set of `base` (a green and its version when the move was admitted), or
  /// from nothing.
  Remade { base: Option<(u64, usize)> },
  /// A delete: nothing reached.
  Emptied,
}

/// Shape: what one fragment entry's record costs the budget. Derived from the type.
const PLACED_BYTES: usize = size_of::<Placed>();

/// Shape: what one entry's place in its part's position order costs the budget (A-653). Derived from the type.
const ORDER_BYTES: usize = size_of::<u32>();

/// One fragment, or a part of a base pack, kept: its bytes as content, its length, its entries; and
/// read from them once (A-639), the positions of its objects, the positions its reference deltas'
/// bases have, and how many of its entries are offset deltas; and its entries' indexes in their
/// objects' position order (A-653), so a stream finds the entries of the objects it wants by
/// halving, whatever the part's size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Kept {
  pub(super) content: ContentId,
  pub(super) len: u64,
  pub(super) placed: Vec<Placed>,
  pub(super) positions: Positions,
  pub(super) references: Positions,
  pub(super) offsets: u64,
  pub(super) order: Vec<u32>,
}

/// A version's two fragments, or a base pack's two parts: commits, trees and tags, then blobs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Fragments {
  pub(super) objects: Kept,
  pub(super) blobs: Kept,
}

/// A green's base pack: every object its head reached at `version`, self-contained, as two parts
/// (A-639), so a fetch that wants no blobs takes the first whole.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BasePack {
  version: usize,
  parts: Fragments,
}

/// A ref's chain of versions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Chain {
  versions: Vec<Version>,
  base: Option<BasePack>,
  rebuild: Rebuild,
}

impl Chain {
  fn head(&self) -> Option<u64> {
    self.versions.last().and_then(|version| version.head)
  }

  /// The index of its newest version.
  fn newest(&self) -> Option<usize> {
    self.versions.len().checked_sub(1)
  }

  /// What the chain charges the budget: its records and its kept parts' runs. Its versions' sets are their values'
  /// bytes, which the content store charges as it keeps them (A-647 part 3).
  fn footprint(&self) -> u64 {
    let runs = self.kept().fold(0_usize, |runs, kept| {
      runs
        .saturating_add(kept.positions.runs())
        .saturating_add(kept.references.runs())
    });
    let records = bytes(self.versions.len(), VERSION_BYTES).saturating_add(bytes(runs, RUN_BYTES));
    records.saturating_add(bytes(
      self.placed(),
      PLACED_BYTES.saturating_add(ORDER_BYTES),
    ))
  }

  /// Every kept fragment's and the base pack's entries.
  fn placed(&self) -> usize {
    self.kept().fold(0_usize, |total, kept| {
      total.saturating_add(kept.placed.len())
    })
  }

  /// Every fragment kept and the base pack.
  fn kept(&self) -> impl Iterator<Item = &Kept> + '_ {
    self
      .versions
      .iter()
      .filter_map(|version| version.fragments.as_ref())
      .chain(self.base.as_ref().map(|base| &base.parts))
      .flat_map(|fragments| [&fragments.objects, &fragments.blobs])
  }
}

/// `count` records of `each` bytes.
fn bytes(count: usize, each: usize) -> u64 {
  u64::try_from(count.saturating_mul(each)).unwrap_or(u64::MAX)
}

/// The objects a walk met that its green had not reached: those indexed by position, those new to the repository by
/// id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Members {
  indexed: Positions,
  new: Vec<ObjectId>,
}

/// How a move's reached set follows from its green's.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reach {
  /// A fast-forward: the set grows by what the version brought.
  Grown,
  /// A create or a rewrite: the set made again, a base set (the reached set of `from`, a green and its version, when one
  /// was found) and what a walk found beyond it.
  Remade {
    base: Positions,
    beyond: Members,
    from: Option<(u64, usize)>,
  },
  /// A delete: nothing reached.
  Emptied,
}

/// One ref's move, admitted before its verdict ([`Repository::admit`]): what its version brings and how its reached set
/// follows, read from the push and the index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
  green: u64,
  from: Option<u64>,
  to: Option<ObjectId>,
  brought: Members,
  reach: Reach,
}

impl Move {
  /// The green it moves.
  pub fn green(&self) -> u64 {
    self.green
  }

  /// Whether it is a fast-forward of its green.
  pub fn is_fast_forward(&self) -> bool {
    self.reach == Reach::Grown
  }

  /// The position of its green's head when it was admitted; none for a green with no head.
  pub fn from_head(&self) -> Option<u64> {
    self.from
  }

  /// The object it moves its green to; none for a delete.
  pub fn to(&self) -> Option<ObjectId> {
    self.to
  }

  /// The same move of `green`: a move admitted for a ref before its append named the green that holds it (A-722 part
  /// 4c: a create's green is made by its append), taken by that green while its head is still this move's `from`.
  pub fn of_green(mut self, green: u64) -> Move {
    self.green = green;
    self
  }
}

/// Where an object is: indexed, with its row, or held by the push.
enum Place {
  Indexed {
    row: ObjectRow,
    content: ContentId,
  },
  Pushed {
    kind: Kind,
    content: ContentId,
    len: u64,
  },
}

impl Place {
  fn kind(&self) -> Kind {
    match self {
      Place::Indexed { row, .. } => row.kind,
      Place::Pushed { kind, .. } => *kind,
    }
  }

  fn content(&self) -> (ContentId, u64) {
    match self {
      Place::Indexed { row, content } => (*content, row.len),
      Place::Pushed { content, len, .. } => (*content, *len),
    }
  }
}

/// A git repository's store (module docs).
#[derive(Debug)]
pub struct Repository {
  format: ObjectFormat,
  index: Index,
  next_position: u64,
  chains: BTreeMap<u64, Chain>,
  /// For each commit some green's current head names, by its position, those greens.
  heads: BTreeMap<u64, BTreeSet<u64>>,
  /// What the chains and the head map charge the store.
  charged: u64,
  /// The version that brought each position, as runs: each run's first position to its end (exclusive), the green and
  /// that green's version (A-722: kept beside the rows, which no longer carry it, so a push can write its rows before it
  /// lands).
  landings: BTreeMap<u64, (u64, u64, u64)>,
  /// What the landing map charges the store.
  landings_charged: u64,
  counters: Cell<StoreCounters>,
  window: Window,
  levels: Levels,
  depth: Depth,
}

impl Repository {
  /// An empty repository of `format`.
  pub fn new(format: ObjectFormat) -> Repository {
    Repository {
      format,
      index: Index::new(),
      next_position: 0,
      chains: BTreeMap::new(),
      heads: BTreeMap::new(),
      charged: 0,
      landings: BTreeMap::new(),
      landings_charged: 0,
      counters: Cell::new(StoreCounters::default()),
      window: Window::default(),
      levels: Levels::default(),
      depth: Depth::default(),
    }
  }

  /// The repository's object format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// The green and that green's version that brought the object at `position` (A-722: the landing map's, the rows no
  /// longer carry it); `None` for a position no landing gave.
  pub fn bringer_of(&self, position: u64) -> Option<(u64, u64)> {
    let (_, (end, green, version)) = self.landings.range(..=position).next_back()?;
    (position < *end).then_some((*green, *version))
  }

  /// git's naming in the repository's format.
  pub fn naming(&self) -> GitNaming {
    GitNaming {
      format: self.format,
    }
  }

  /// Refuses a push made for a repository of another format.
  fn same_format(&self, push: &Push) -> Result<(), StoreRefused> {
    if push.format() != self.format {
      return Err(StoreRefused::ForeignPush);
    }
    Ok(())
  }

  /// What the store has done.
  pub fn counters(&self) -> StoreCounters {
    self.counters.get()
  }

  /// The deepest delta chain the store's encoder makes: the deepest any commit's push carried, at most git's limit
  /// (A-614 part 2's learned depth). What a store made again from its pushes' records must learn alike (A-662).
  pub fn learned_depth(&self) -> u64 {
    self.depth.limit()
  }

  fn count(&self, change: impl FnOnce(&mut StoreCounters)) {
    let mut counters = self.counters.get();
    change(&mut counters);
    self.counters.set(counters);
  }

  /// The row of the object `id` names, and its root content.
  pub(crate) fn indexed(
    &self,
    store: &ContentStore,
    id: &ObjectId,
  ) -> Option<(ObjectRow, ContentId)> {
    let found = self.index.get(store, &rows::object_key(id))?;
    let content = found.contents().next()?;
    Some((rows::decode(found.payload())?, content))
  }

  /// The row of the object `id` names.
  pub fn object(&self, store: &ContentStore, id: &ObjectId) -> Option<ObjectRow> {
    self.indexed(store, id).map(|(row, _)| row)
  }

  /// The type, root content and length of the object `id` the index holds: where its bytes are read from (A-753: a
  /// pack resending an object the repository holds is read back from the index, its quarantine holding none).
  pub fn stored(&self, store: &ContentStore, id: &ObjectId) -> Option<(Kind, ContentId, u64)> {
    let (row, content) = self.indexed(store, id)?;
    Some((row.kind, content, row.len))
  }

  /// The id of the object at `position`.
  pub fn id_at(&self, store: &ContentStore, position: u64) -> Option<ObjectId> {
    let found = self.index.get(store, &rows::position_key(position))?;
    rows::position_id(self.format, found.payload())
  }

  /// Where `id` is: indexed first, then held by the push.
  fn locate(&self, store: &ContentStore, push: &Push, id: &ObjectId) -> Option<Place> {
    if let Some((row, content)) = self.indexed(store, id) {
      return Some(Place::Indexed { row, content });
    }
    let (number, content, len) = push.object(id)?;
    Some(Place::Pushed {
      kind: kind_of(number)?,
      content,
      len,
    })
  }

  /// The bytes of the object at `place`.
  fn bytes_of(store: &ContentStore, id: &ObjectId, place: &Place) -> Result<Vec<u8>, StoreRefused> {
    let (content, len) = place.content();
    store
      .object_bytes(&content, len)
      .ok_or(StoreRefused::Unreadable { id: *id })
  }

  /// The objects `id` names: a commit's tree and parents, a tree's entries (a submodule's commit is no object of this
  /// repository), a tag's object.
  fn references(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    match place.kind() {
      Kind::Blob => Ok(Vec::new()),
      Kind::Commit => {
        let mut named = vec![self.commit_tree(store, push, id, place)?];
        named.extend(self.parents(store, push, id)?);
        Ok(named)
      }
      Kind::Tree => self.tree_references(store, id, place),
      Kind::Tag => self
        .tag_reference(store, id, place)
        .map(|target| vec![target]),
    }
  }

  /// A commit's root tree.
  fn commit_tree(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
    place: &Place,
  ) -> Result<ObjectId, StoreRefused> {
    match place {
      Place::Indexed { row, .. } => {
        let commit = row
          .commit
          .as_ref()
          .ok_or(StoreRefused::Unreadable { id: *id })?;
        self
          .id_at(store, commit.tree)
          .ok_or(StoreRefused::Unreadable { id: *id })
      }
      Place::Pushed { .. } => push
        .commits()
        .get(id)
        .map(|arrived| arrived.tree)
        .ok_or(StoreRefused::Unreadable { id: *id }),
    }
  }

  /// A commit's parents, from the push or from its row.
  fn parents(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    if let Some(arrived) = push.commits().get(id) {
      return Ok(arrived.parents.clone());
    }
    let (row, _) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    let commit = row.commit.ok_or(StoreRefused::NotACommit { id: *id })?;
    commit
      .parents
      .iter()
      .map(|parent| {
        self
          .id_at(store, *parent)
          .ok_or(StoreRefused::Unreadable { id: *id })
      })
      .collect()
  }

  /// A commit's generation number, from the push (sealed) or from its row.
  fn generation(
    &self,
    store: &ContentStore,
    push: &Push,
    id: &ObjectId,
  ) -> Result<u64, StoreRefused> {
    if let Some(arrived) = push.commits().get(id) {
      return Ok(arrived.generation);
    }
    let (row, _) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    row
      .commit
      .map(|commit| commit.generation)
      .ok_or(StoreRefused::NotACommit { id: *id })
  }

  /// What git's tree walk finds for `path` in the tree `id` ([`Tree::find`]; A-640 part 4: a `sparse:oid` filter's
  /// `<rev>:<path>`), a submodule's commit an entry like any other: the entry's object and the path left below it.
  fn tree_entry<'p>(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
    path: &'p [u8],
  ) -> Result<Option<(ObjectId, &'p [u8])>, StoreRefused> {
    let bytes = Repository::bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .find(path)
      .map(|(entry, below)| {
        entry
          .object_id(self.format)
          .map(|object| (object, below))
          .ok_or(malformed)
      })
      .transpose()
  }

  /// A tree's entries' objects with their names, in the tree's order, a submodule's commit left out (A-640 part 4: what
  /// a sparse filter asks its patterns of).
  fn tree_named_references(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<(ObjectId, Vec<u8>)>, StoreRefused> {
    let bytes = Repository::bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
      .map(|entry| {
        entry
          .object_id(self.format)
          .map(|object| (object, entry.name.to_vec()))
          .ok_or(malformed)
      })
      .collect()
  }

  /// A tree's entries' objects, a submodule's commit left out.
  fn tree_references(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<Vec<ObjectId>, StoreRefused> {
    let bytes = Repository::bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tree =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tree
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
      .map(|entry| entry.object_id(self.format).ok_or(malformed))
      .collect()
  }

  /// The object a tag names.
  fn tag_reference(
    &self,
    store: &ContentStore,
    id: &ObjectId,
    place: &Place,
  ) -> Result<ObjectId, StoreRefused> {
    let bytes = Repository::bytes_of(store, id, place)?;
    let malformed = StoreRefused::Malformed { id: *id };
    let tag = Tag::parse(&bytes, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    tag.object(self.format).ok_or(malformed)
  }

  /// Seals `push`: each of its commits' generation numbers, one more than its parents' greatest, its parents read from
  /// the push or the repository's commit rows. Refused at a parent neither holds, or one that is no commit.
  pub fn seal(&self, store: &ContentStore, push: &mut Push) -> Result<(), StoreRefused> {
    self.same_format(push)?;
    let mut generations: BTreeMap<ObjectId, u64> = BTreeMap::new();
    let ids: Vec<ObjectId> = push.commits().keys().copied().collect();
    for id in ids {
      self.generations_from(store, push, id, &mut generations)?;
    }
    push.seal_with(&generations);
    Ok(())
  }

  /// The generation numbers of `start` and of every commit of the push below it, parents first, without recursion.
  fn generations_from(
    &self,
    store: &ContentStore,
    push: &Push,
    start: ObjectId,
    done: &mut BTreeMap<ObjectId, u64>,
  ) -> Result<(), StoreRefused> {
    let mut stack = vec![(start, false)];
    while let Some((id, parents_done)) = stack.pop() {
      let Some(arrived) = push.commits().get(&id).filter(|_| !done.contains_key(&id)) else {
        continue;
      };
      if parents_done {
        let greatest = self.parents_generation(store, &arrived.parents, done)?;
        done.insert(id, greatest.saturating_add(1));
        continue;
      }
      stack.push((id, true));
      for parent in &arrived.parents {
        if push.commits().contains_key(parent) && !done.contains_key(parent) {
          stack.push((*parent, false));
        }
      }
    }
    Ok(())
  }

  /// The greatest generation number among `parents`, each from `done` or the repository's commit rows; zero for none.
  fn parents_generation(
    &self,
    store: &ContentStore,
    parents: &[ObjectId],
    done: &BTreeMap<ObjectId, u64>,
  ) -> Result<u64, StoreRefused> {
    let mut greatest: u64 = 0;
    for parent in parents {
      let generation = match done.get(parent) {
        Some(generation) => *generation,
        None => {
          let (row, _) = self
            .indexed(store, parent)
            .ok_or(StoreRefused::Missing { id: *parent })?;
          let commit = row.commit.ok_or(StoreRefused::NotACommit { id: *parent })?;
          commit.generation
        }
      };
      greatest = greatest.max(generation);
    }
    Ok(greatest)
  }

  /// The objects reachable from `from` that `stop` does not hold, read from the push and the index; a walk stops at each
  /// object `stop` holds, so a reached tree is skipped whole.
  fn walk(
    &self,
    store: &ContentStore,
    push: &Push,
    from: ObjectId,
    stop: &Positions,
  ) -> Result<Members, StoreRefused> {
    let (mut indexed, mut new) = (Vec::new(), Vec::new());
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut pending = vec![from];
    while let Some(id) = pending.pop() {
      if !seen.insert(id) {
        continue;
      }
      let place = self
        .locate(store, push, &id)
        .ok_or(StoreRefused::Missing { id })?;
      match &place {
        Place::Indexed { row, .. } if stop.contains(row.position) => {
          self.met_reached(row.kind);
          continue;
        }
        Place::Indexed { row, .. } => indexed.push(row.position),
        Place::Pushed { .. } => new.push(id),
      }
      self.count(|counters| counters.walked = counters.walked.saturating_add(1));
      pending.extend(self.references(store, push, &id, &place)?);
    }
    Ok(Members {
      indexed: Positions::of(indexed),
      new,
    })
  }

  /// A walk met an object already reached.
  fn met_reached(&self, kind: Kind) {
    self.count(|counters| {
      counters.reached_met = counters.reached_met.saturating_add(1);
      if kind == Kind::Tree {
        counters.trees_skipped = counters.trees_skipped.saturating_add(1);
      }
    });
  }

  /// The commit `id` peels to: itself, or the commit a chain of tags names; `None` for a tree or a blob.
  fn peel(
    &self,
    store: &ContentStore,
    push: &Push,
    id: ObjectId,
  ) -> Result<Option<ObjectId>, StoreRefused> {
    let mut at = id;
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    while seen.insert(at) {
      let place = self
        .locate(store, push, &at)
        .ok_or(StoreRefused::Missing { id: at })?;
      match place.kind() {
        Kind::Commit => return Ok(Some(at)),
        Kind::Tag => at = self.tag_reference(store, &at, &place)?,
        Kind::Tree | Kind::Blob => return Ok(None),
      }
    }
    Ok(None)
  }

  /// Whether the commit `ancestor` is `head` or one of its ancestors. Decided with no walk where the ids or the
  /// generation numbers decide it (A-644): `head` is `ancestor`; or `head`'s generation number is no greater than
  /// `ancestor`'s, and since a commit's is one more than its parents' greatest, no descendant of `ancestor` has one
  /// that small. Otherwise a walk back by generation number from `head`, which stops below the ancestor's generation,
  /// so it costs the commits between. Either not a commit: not a fast-forward.
  fn fast_forward(
    &self,
    store: &ContentStore,
    push: &Push,
    head: ObjectId,
    ancestor: ObjectId,
  ) -> Result<bool, StoreRefused> {
    let commits = (
      self.locate(store, push, &head).map(|place| place.kind()),
      self
        .locate(store, push, &ancestor)
        .map(|place| place.kind()),
    );
    if commits != (Some(Kind::Commit), Some(Kind::Commit)) {
      return Ok(false);
    }
    let floor = self.generation(store, push, &ancestor)?;
    let top = self.generation(store, push, &head)?;
    if head == ancestor || top <= floor {
      self.count(|counters| {
        counters.decided_without_walk = counters.decided_without_walk.saturating_add(1);
      });
      return Ok(head == ancestor);
    }
    self.count(|counters| counters.descent_walks = counters.descent_walks.saturating_add(1));
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut heap = BinaryHeap::from([(top, head)]);
    while let Some((generation, id)) = heap.pop() {
      if id == ancestor {
        return Ok(true);
      }
      if generation <= floor || !seen.insert(id) {
        continue;
      }
      for parent in self.parents(store, push, &id)? {
        heap.push((self.generation(store, push, &parent)?, parent));
      }
    }
    Ok(false)
  }

  /// A green whose current head is the indexed commit `id`.
  fn green_at(&self, store: &ContentStore, id: &ObjectId) -> Option<u64> {
    let (row, _) = self.indexed(store, id)?;
    self.heads.get(&row.position)?.iter().next().copied()
  }

  /// Admits `green`'s move to `to` (`None` deletes the ref) before its verdict: what its version brings and how its
  /// reached set follows, read from the sealed `push` and the index (module docs).
  pub fn admit(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: Option<ObjectId>,
  ) -> Result<Move, StoreRefused> {
    self.same_format(push)?;
    if !push.is_sealed() {
      return Err(StoreRefused::Unsealed);
    }
    let chain = self.chains.get(&green);
    let from = chain.and_then(Chain::head);
    let Some(to) = to else {
      return Ok(Move {
        green,
        from,
        to: None,
        brought: Members::default(),
        reach: Reach::Emptied,
      });
    };
    let mut admitting = self.admitting(store, push, green, to)?;
    let mut clock = staging::Unbounded;
    while !self.admit_turn(
      store,
      push,
      &mut admitting,
      Quantum {
        clock: &mut clock,
        ns: u64::MAX,
      },
    )? {}
    admitting.admitted().ok_or(StoreRefused::Unsealed)
  }

  /// Commits `push` and `moves` once the verdict accepts them: a position and rows for each object new to the
  /// repository, and a version for each move with its value of the reach sets (what it brought and its reached set,
  /// A-647 part 3) and the head map moved. The values, the rows and the versions' bytes are admitted first: refused,
  /// nothing changed, and the push still holds its objects. Committed, the push holds nothing. The versions made, one
  /// for each move, in order.
  pub fn commit(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    moves: &[Move],
  ) -> Result<Vec<u64>, StoreRefused> {
    self.same_format(push)?;
    let first = moves.first().ok_or(StoreRefused::NoMove)?;
    self.check_moves(moves)?;
    let staged = match self.adoptable_stage(store, push)? {
      Some(staged) => staged,
      None => {
        if !push.is_empty() {
          self.count(|counters| counters.restaged = counters.restaged.saturating_add(1));
        }
        self.stage(store, push)?;
        push.take_staged().ok_or(StoreRefused::Unsealed)?
      }
    };
    let fresh_ids: Vec<ObjectId> = staged.positions.keys().copied().collect();
    let positions = staged.positions.clone();
    // A stage that gives no position leaves the next position where it is, however long ago it was made.
    let next = if staged.positions.is_empty() {
      self.next_position
    } else {
      staged
        .base
        .saturating_add(u64::try_from(staged.positions.len()).unwrap_or(u64::MAX))
    };
    let brought_by = self.brought_by(moves, first);
    let landed = landed_runs(&fresh_ids, &positions, &brought_by);
    let (planned, heads) = self.plan(store, moves, &positions)?;
    let before = store.used();
    let values = put_values(store, &planned)?;
    let added = store.used().saturating_sub(before);
    let chains = self.chains_with(planned, &values);
    let total = footprint(&self.chains, &chains, &heads);
    let more = total.saturating_sub(self.charged);
    if let Err(refused) = store.charge(more) {
      give_back(store, &values)?;
      return Err(refused.into());
    }
    let landing_cost = bytes(landed.len(), LANDING_BYTES);
    if let Err(refused) = store.charge(landing_cost) {
      store.discharge(more)?;
      give_back(store, &values)?;
      return Err(refused.into());
    }
    // A stage that gives no position writes no row: its fork is given back, never adopted.
    let inserted = if staged.positions.is_empty() {
      staged.fork.release(store).map_err(StoreRefused::from)
    } else {
      self
        .index
        .adopt(store, staged.fork)
        .map_err(|refused| match refused {
          slates_vcs::AdoptRefused::Refused(refused, fork) => {
            let _ = fork.release(store);
            StoreRefused::from(refused)
          }
          slates_vcs::AdoptRefused::Stale(fork) | slates_vcs::AdoptRefused::NotAFork(fork) => {
            let _ = fork.release(store);
            StoreRefused::Moved { green: first.green }
          }
        })
    };
    if let Err(refused) = inserted {
      store.discharge(more.saturating_add(landing_cost))?;
      give_back(store, &values)?;
      return Err(refused);
    }
    self.landings.extend(landed);
    self.landings_charged = self.landings_charged.saturating_add(landing_cost);
    store.discharge(self.charged.saturating_sub(total))?;
    self.charged = total;
    self.count(|counters| counters.value_bytes = counters.value_bytes.saturating_add(added));
    let versions = moves
      .iter()
      .map(|movement| {
        chains.get(&movement.green).map_or(0, |chain| {
          u64::try_from(chain.versions.len())
            .unwrap_or(u64::MAX)
            .saturating_sub(1)
        })
      })
      .collect();
    self.chains.extend(chains);
    self.heads = heads;
    self.next_position = next;
    let given = u64::try_from(fresh_ids.len()).unwrap_or(u64::MAX);
    self.count(|counters| counters.positions = counters.positions.saturating_add(given));
    self.depth.seen(push.deepest());
    push.release_holds(store)?;
    Ok(versions)
  }

  /// Every move's green is where it was when the move was admitted, and no green is moved twice.
  fn check_moves(&self, moves: &[Move]) -> Result<(), StoreRefused> {
    let mut moved: BTreeSet<u64> = BTreeSet::new();
    for movement in moves {
      let now = self.chains.get(&movement.green).and_then(Chain::head);
      if now != movement.from || !moved.insert(movement.green) {
        return Err(StoreRefused::Moved {
          green: movement.green,
        });
      }
    }
    Ok(())
  }

  /// Stages `push`, sealed, whole (A-722 part 3): each object it holds that the repository does not index given a
  /// position after the repository's next, in order by type and then id as A-639 orders them, and its rows (its object
  /// row with a commit's part, and its position row) written into a fork of the index made now and sealed into
  /// tables, so the push's landing adopts them and writes none. Its positions are tentative: the landing takes them
  /// when nothing landed since, and otherwise stages the push again. A push staged before gives its stage back first.
  /// Every landing adopts a fork, so the repository's own index keeps no tail of rows, and a fork made at any time (a
  /// push abandoned, another staging at once) changes nothing a store made again from the log would not make alike.
  pub fn stage(&mut self, store: &mut ContentStore, push: &mut Push) -> Result<(), StoreRefused> {
    let staging = self.staging(store, push)?;
    self.stage_whole(store, push, staging)
  }

  /// Derived: the most staging `push` could charge the store ([`Self::stage`]): each object's object row (a commit's
  /// part and parents included) and position row with their tail slots, doubled for the tables a seal writes beside the
  /// tail it replaces; and the fork staging makes.
  pub fn stage_room(&self, push: &Push) -> u64 {
    let raw = self.format.raw_len();
    let object_row = raw
      .saturating_add(1)
      .saturating_add(rows::OBJECT_PAYLOAD_BYTES)
      .saturating_add(slates_vcs::TAIL_SLOT_BYTES);
    let position_row = raw
      .saturating_add(1)
      .saturating_add(size_of::<u64>())
      .saturating_add(slates_vcs::TAIL_SLOT_BYTES);
    let objects = bytes(
      push.len(),
      object_row.saturating_add(position_row).saturating_mul(2),
    );
    let commits = push.commits().values().fold(0_u64, |total, arrived| {
      let part = arrived
        .parents
        .len()
        .saturating_mul(size_of::<u64>())
        .saturating_add(rows::COMMIT_PART_BYTES);
      total.saturating_add(bytes(1, part.saturating_mul(2)))
    });
    objects
      .saturating_add(commits)
      .saturating_add(self.index.fork_room())
  }

  /// Whether `push` holds objects its commit would stage again (A-722 part 4e): none staged, or a stage a landing
  /// overtook since ([`Self::adoptable_stage`]'s rule). A push's judge stages it again a turn at a time while it does.
  pub fn needs_stage(&self, push: &Push) -> bool {
    if push.is_empty() {
      return false;
    }
    !push
      .staged()
      .is_some_and(|staged| self.stage_current(staged))
  }

  /// Whether `staged` is a stage this index takes as it is: one that gives no position (every object the push holds
  /// indexed already, as they stay), which no landing can overtake (A-722 part 4e); or one no landing overtook, no row
  /// taken in nor position given since its fork was made.
  fn stage_current(&self, staged: &super::push::Staged) -> bool {
    staged.positions.is_empty()
      || (staged.base == self.next_position && self.index.adoptable(&staged.fork))
  }

  /// The push's staged rows when this index would adopt them as they are (A-722 part 3): no row taken in nor position
  /// given by the repository since its fork was made. A stage this repository outran is given back, for the push to be
  /// staged again ([`Self::stage`]).
  fn adoptable_stage(
    &self,
    store: &mut ContentStore,
    push: &mut Push,
  ) -> Result<Option<super::push::Staged>, StoreRefused> {
    let current = push
      .staged()
      .is_some_and(|staged| self.stage_current(staged));
    let Some(staged) = push.take_staged() else {
      return Ok(None);
    };
    if current {
      return Ok(Some(staged));
    }
    staged.fork.release(store)?;
    Ok(None)
  }

  /// For each object a move's walk found new, the green and version that brought it: the first move in order whose D(k)
  /// holds it; an object no move's holds is the first move's.
  fn brought_by(&self, moves: &[Move], first: &Move) -> BroughtBy {
    let version_of = |green: u64| {
      self.chains.get(&green).map_or(0, |chain| {
        u64::try_from(chain.versions.len()).unwrap_or(u64::MAX)
      })
    };
    let mut by = BTreeMap::new();
    for movement in moves {
      for id in &movement.brought.new {
        by.entry(*id)
          .or_insert((movement.green, version_of(movement.green)));
      }
    }
    (by, (first.green, version_of(first.green)))
  }

  /// The position of `id`: given now, or its row's.
  fn position_of(
    &self,
    store: &ContentStore,
    positions: &BTreeMap<ObjectId, u64>,
    id: &ObjectId,
  ) -> Result<u64, StoreRefused> {
    if let Some(position) = positions.get(id) {
      return Ok(*position);
    }
    self
      .indexed(store, id)
      .map(|(row, _)| row.position)
      .ok_or(StoreRefused::Missing { id: *id })
  }

  /// A fresh commit's part of its row: its tree and parents by position, its date and generation number.
  fn commit_row(
    &self,
    store: &ContentStore,
    push: &Push,
    positions: &BTreeMap<ObjectId, u64>,
    id: &ObjectId,
    kind: Kind,
  ) -> Result<Option<CommitRow>, StoreRefused> {
    if kind != Kind::Commit {
      return Ok(None);
    }
    let arrived = push
      .commits()
      .get(id)
      .ok_or(StoreRefused::Unreadable { id: *id })?;
    let parents = arrived
      .parents
      .iter()
      .map(|parent| self.position_of(store, positions, parent))
      .collect::<Result<Vec<u64>, StoreRefused>>()?;
    Ok(Some(CommitRow {
      tree: self.position_of(store, positions, &arrived.tree)?,
      date: arrived.date,
      generation: arrived.generation,
      parents,
    }))
  }

  /// The versions the moves make, their sets computed and their values not yet put, and the head map after them.
  fn plan(
    &self,
    store: &ContentStore,
    moves: &[Move],
    positions: &BTreeMap<ObjectId, u64>,
  ) -> Result<(Vec<Planned>, HeadMap), StoreRefused> {
    let mut planned = Vec::with_capacity(moves.len());
    let mut heads = self.heads.clone();
    for movement in moves {
      let brought = self.members_positions(store, &movement.brought, positions)?;
      let reached = match &movement.reach {
        Reach::Grown => self
          .current_reached(store, movement.green)?
          .unwrap_or_default()
          .union(&brought),
        Reach::Remade { base, beyond, .. } => {
          base.union(&self.members_positions(store, beyond, positions)?)
        }
        Reach::Emptied => Positions::new(),
      };
      let head = movement
        .to
        .map(|to| self.position_of(store, positions, &to))
        .transpose()?;
      move_head(&mut heads, movement.green, movement.from, head);
      planned.push(Planned {
        green: movement.green,
        head,
        followed: followed(&movement.reach),
        sets: Sets { reached, brought },
      });
    }
    Ok((planned, heads))
  }

  /// The chains the planned versions make, on copies: each version with its value, `values`' in order.
  fn chains_with(&self, planned: Vec<Planned>, values: &[ContentId]) -> BTreeMap<u64, Chain> {
    let mut chains: BTreeMap<u64, Chain> = BTreeMap::new();
    for (planned, value) in planned.into_iter().zip(values) {
      let mut chain = self.chains.get(&planned.green).cloned().unwrap_or_default();
      chain.versions.push(Version {
        head: planned.head,
        value: *value,
        fragments: None,
        followed: planned.followed,
      });
      chains.insert(planned.green, chain);
    }
    chains
  }

  /// A walk's members as positions, the new ones by the positions given them now.
  fn members_positions(
    &self,
    store: &ContentStore,
    members: &Members,
    positions: &BTreeMap<ObjectId, u64>,
  ) -> Result<Positions, StoreRefused> {
    let new = members
      .new
      .iter()
      .map(|id| self.position_of(store, positions, id))
      .collect::<Result<Vec<u64>, StoreRefused>>()?;
    Ok(members.indexed.union(&Positions::of(new)))
  }

  /// The versions `green` has.
  pub fn versions(&self, green: u64) -> usize {
    self
      .chains
      .get(&green)
      .map_or(0, |chain| chain.versions.len())
  }

  /// D(k): what `green`'s version `version` brought, as positions, read from the version's value (A-647 part 3).
  pub fn brought(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Positions, StoreRefused> {
    Ok(self.sets(store, green, version)?.brought)
  }

  /// The set `green`'s version `version`'s head reaches, read from the version's value (A-647 part 3): any version's,
  /// not only the newest.
  pub fn reached(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Positions, StoreRefused> {
    Ok(self.sets(store, green, version)?.reached)
  }

  /// `green`'s version `version`'s two sets, read from its value.
  fn sets(&self, store: &ContentStore, green: u64, version: usize) -> Result<Sets, StoreRefused> {
    let record = self.version_record(green, version)?;
    Sets::read(store, &record.value).ok_or(StoreRefused::SetsUnreadable {
      green,
      version: to_u64(version),
    })
  }

  /// The set `green`'s newest version's head reaches, read from its value; `None` for a green with no version.
  fn current_reached(
    &self,
    store: &ContentStore,
    green: u64,
  ) -> Result<Option<Positions>, StoreRefused> {
    let Some(newest) = self.chains.get(&green).and_then(Chain::newest) else {
      return Ok(None);
    };
    Ok(Some(self.sets(store, green, newest)?.reached))
  }

  /// The position of `green`'s current head; none while it has none (A-722 part 4c: what a move admitted before its
  /// append must still be from).
  pub fn current_head(&self, green: u64) -> Option<u64> {
    self.chains.get(&green).and_then(Chain::head)
  }

  /// The head of `green`'s version `version`; `None` for a deleted ref.
  pub fn head(&self, store: &ContentStore, green: u64, version: usize) -> Option<ObjectId> {
    let position = self.chains.get(&green)?.versions.get(version)?.head?;
    self.id_at(store, position)
  }

  /// The root tree of `green`'s version `version`, read from its head's commit row.
  pub fn tree(&self, store: &ContentStore, green: u64, version: usize) -> Option<ObjectId> {
    let head = self.head(store, green, version)?;
    let commit = self.object(store, &head)?.commit?;
    self.id_at(store, commit.tree)
  }

  /// The entry `path` (`/`-separated) names in `green`'s version `version`, resolved one tree a component down: no walk
  /// of history and none of the tree. `None` when the version has no tree or the path names nothing.
  pub fn entry(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
    path: &[u8],
  ) -> Result<Option<Found>, StoreRefused> {
    let Some(mut tree) = self.tree(store, green, version) else {
      return Ok(None);
    };
    let mut found = None;
    let mut names = path
      .split(|byte| *byte == b'/')
      .filter(|name| !name.is_empty())
      .peekable();
    while let Some(name) = names.next() {
      let Some((entry, is_dir)) = self.entry_in(store, &tree, name)? else {
        return Ok(None);
      };
      if names.peek().is_some() && !is_dir {
        return Ok(None);
      }
      tree = entry.id;
      found = Some(entry);
    }
    Ok(found)
  }

  /// The entry named `name` in the indexed tree `tree`, and whether it is a subtree.
  fn entry_in(
    &self,
    store: &ContentStore,
    tree: &ObjectId,
    name: &[u8],
  ) -> Result<Option<(Found, bool)>, StoreRefused> {
    let (row, content) = self
      .indexed(store, tree)
      .ok_or(StoreRefused::Missing { id: *tree })?;
    let bytes = store
      .object_bytes(&content, row.len)
      .ok_or(StoreRefused::Unreadable { id: *tree })?;
    let malformed = StoreRefused::Malformed { id: *tree };
    let parsed =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    let Some(entry) = parsed.entries().iter().find(|entry| entry.name == name) else {
      return Ok(None);
    };
    let id = entry.object_id(self.format).ok_or(malformed)?;
    Ok(Some((
      Found {
        mode: entry.mode,
        id,
      },
      entry.is_dir(),
    )))
  }

  /// A fork of the repository as it stands now (A-745, §4.17.1 "A fetch's answer" rule 4: a fetch reads the index as it
  /// stood when it began): the index forked (its rows as they stand, their tables shared), its versions, base packs,
  /// heads and landings copied, and every content they name held once more, so whatever lands, is re-based or rebuilt
  /// in the repository meanwhile, the fork reads as this generation did, until [`Repository::release`] gives back what
  /// it holds. Its records charge the store as the repository's do. Refused, everything it took is given back.
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<Repository, StoreRefused> {
    let charge = self.charged.saturating_add(self.landings_charged);
    store.charge(charge).map_err(StoreRefused::from)?;
    let index = match self.index.fork(store) {
      Ok(index) => index,
      Err(refused) => {
        let _ = store.discharge(charge);
        return Err(StoreRefused::from(refused));
      }
    };
    let named: Vec<ContentId> = self
      .chains
      .values()
      .flat_map(|chain| {
        chain
          .kept()
          .map(|kept| kept.content)
          .chain(chain.versions.iter().map(|version| version.value))
      })
      .collect();
    if let Err(refused) = hold_all(store, &named) {
      let _ = index.release(store);
      let _ = store.discharge(charge);
      return Err(refused);
    }
    Ok(Repository {
      format: self.format,
      index,
      next_position: self.next_position,
      chains: self.chains.clone(),
      heads: self.heads.clone(),
      charged: self.charged,
      landings: self.landings.clone(),
      landings_charged: self.landings_charged,
      counters: Cell::new(StoreCounters::default()),
      window: self.window.clone(),
      levels: self.levels.clone(),
      depth: self.depth,
    })
  }

  /// Where the repository stands (A-745 rule 4): the positions it has given and the versions its greens have, both only
  /// ever growing, so every landing moves it and two moments with the same stamp read the same objects, refs' heads and
  /// versions. A fork taken at a stamp serves every fetch that begins at it.
  pub fn stamp(&self) -> (u64, u64) {
    let versions = self.chains.values().fold(0_u64, |count, chain| {
      count.saturating_add(u64::try_from(chain.versions.len()).unwrap_or(u64::MAX))
    });
    (self.next_position, versions)
  }

  /// Gives back everything the repository holds: its rows' holds, their tables, its versions' values and their charge.
  /// It goes on past a refusal and answers the first.
  pub fn release(self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    let mut first = self.index.release(store).map_err(StoreRefused::from);
    for kept in self.chains.values().flat_map(Chain::kept) {
      first = first.and(
        store
          .release(&kept.content)
          .map(drop)
          .map_err(StoreRefused::from),
      );
    }
    let values: Vec<ContentId> = self
      .chains
      .values()
      .flat_map(|chain| chain.versions.iter().map(|version| version.value))
      .collect();
    first = first.and(give_back(store, &values));
    first.and(
      store
        .discharge(self.charged.saturating_add(self.landings_charged))
        .map_err(StoreRefused::from),
    )
  }
}

/// A move's version as its commit plans it: its green, its head's position, how its reached set followed, and its sets,
/// which become its value once the commit puts them.
struct Planned {
  green: u64,
  head: Option<u64>,
  followed: Followed,
  sets: Sets,
}

/// Puts each planned version's sets as its value (A-647 part 3), held once for its record, in order; refused, every
/// value put is given back.
fn put_values(
  store: &mut ContentStore,
  planned: &[Planned],
) -> Result<Vec<ContentId>, StoreRefused> {
  let shape = reach::shape_of(store.window())?;
  let mut values = Vec::with_capacity(planned.len());
  for version in planned {
    match version.sets.put(store, shape) {
      Ok(value) => values.push(value),
      Err(refused) => {
        give_back(store, &values)?;
        return Err(refused.into());
      }
    }
  }
  Ok(values)
}

/// Holds every one of `ids` in `store`; refused one, gives back those it held and answers the refusal.
fn hold_all(store: &mut ContentStore, ids: &[ContentId]) -> Result<(), StoreRefused> {
  for (at, id) in ids.iter().enumerate() {
    if let Err(refused) = store.hold(id) {
      let _ = give_back(store, ids.get(..at).unwrap_or_default());
      return Err(StoreRefused::from(refused));
    }
  }
  Ok(())
}

/// Gives back one hold of each of `values`; past a refusal the rest still, and the first refusal is the answer.
fn give_back(store: &mut ContentStore, values: &[ContentId]) -> Result<(), StoreRefused> {
  let mut first = Ok(());
  for value in values {
    if let Err(refused) = store.release(value) {
      first = first.and(Err(StoreRefused::from(refused)));
    }
  }
  first
}

/// What a version records of its move's [`Reach`].
fn followed(reach: &Reach) -> Followed {
  match reach {
    Reach::Grown => Followed::Grown,
    Reach::Remade { from, .. } => Followed::Remade { base: *from },
    Reach::Emptied => Followed::Emptied,
  }
}

/// Moves `green` in the head map from `from` to `to`.
fn move_head(
  heads: &mut BTreeMap<u64, BTreeSet<u64>>,
  green: u64,
  from: Option<u64>,
  to: Option<u64>,
) {
  if let Some(from) = from
    && let Some(greens) = heads.get_mut(&from)
  {
    greens.remove(&green);
    if greens.is_empty() {
      heads.remove(&from);
    }
  }
  if let Some(to) = to {
    heads.entry(to).or_default().insert(green);
  }
}

/// What the chains and the head map charge once the planned chains replace theirs.
fn footprint(
  current: &BTreeMap<u64, Chain>,
  planned: &BTreeMap<u64, Chain>,
  heads: &BTreeMap<u64, BTreeSet<u64>>,
) -> u64 {
  let chains = current
    .iter()
    .filter(|(green, _)| !planned.contains_key(green))
    .map(|(_, chain)| chain)
    .chain(planned.values())
    .fold(0_u64, |total, chain| {
      total.saturating_add(chain.footprint())
    });
  let entries = heads
    .values()
    .fold(0_usize, |total, greens| total.saturating_add(greens.len()));
  chains.saturating_add(bytes(entries, HEAD_BYTES))
}

/// For each object a move found new, the green and version that brought it; and the pair for an
/// object no move holds.
type BroughtBy = (BTreeMap<ObjectId, (u64, u64)>, (u64, u64));

/// Derived: what one run of the landing map charges the store: its key and its value, four words.
const LANDING_BYTES: usize = size_of::<(u64, (u64, u64, u64))>();

/// The landing map's runs for a commit's fresh objects (`Repository::landings`): each object's position with the green
/// and version that brought it, consecutive positions of one pair joined into one run.
fn landed_runs(
  fresh: &[ObjectId],
  positions: &BTreeMap<ObjectId, u64>,
  (by, otherwise): &BroughtBy,
) -> Vec<(u64, (u64, u64, u64))> {
  let mut placed: Vec<(u64, (u64, u64))> = fresh
    .iter()
    .filter_map(|id| {
      let position = *positions.get(id)?;
      Some((position, by.get(id).copied().unwrap_or(*otherwise)))
    })
    .collect();
  placed.sort_unstable();
  let mut runs: Vec<(u64, (u64, u64, u64))> = Vec::new();
  for (position, (green, version)) in placed {
    match runs.last_mut() {
      Some((_, (end, run_green, run_version)))
        if *end == position && (*run_green, *run_version) == (green, version) =>
      {
        *end = position.saturating_add(1);
      }
      _ => runs.push((position, (position.saturating_add(1), green, version))),
    }
  }
  runs
}

/// For each commit some green's head names, by its position, those greens.
type HeadMap = BTreeMap<u64, BTreeSet<u64>>;

/// A tree's entries that are objects of this repository: name, whether a subtree, id.
pub(super) type TreeEntries = BTreeMap<Vec<u8>, (bool, ObjectId)>;

/// The slot of `green`'s version `version`'s fragments.
fn fragments_slot(
  chains: &mut BTreeMap<u64, Chain>,
  green: u64,
  version: usize,
) -> Option<&mut Option<Fragments>> {
  chains
    .get_mut(&green)
    .and_then(|chain| chain.versions.get_mut(version))
    .map(|record| &mut record.fragments)
}

/// A fragment whose bytes are kept as `content` already (A-718: put a window at a time), and what
/// its entries hold read once (`Kept`); the caller's hold on `content` becomes the fragment's.
fn keep(((bytes, placed), content): (fragment::Encoded, ContentId)) -> Result<Kept, StoreRefused> {
  let order = position_order(&placed)?;
  let positions = Positions::of(placed.iter().map(|entry| entry.position).collect());
  let references = Positions::of(
    placed
      .iter()
      .filter(|entry| entry.stored == Stored::Reference)
      .filter_map(|entry| entry.base)
      .collect(),
  );
  let offsets = placed
    .iter()
    .filter(|entry| entry.stored == Stored::Offset)
    .count();
  Ok(Kept {
    content,
    len: to_u64(bytes.len()),
    placed,
    positions,
    references,
    offsets: to_u64(offsets),
    order,
  })
}

/// The indexes of `placed` in their objects' position order; refused for a part of more entries
/// than a pack's header counts.
fn position_order(placed: &[Placed]) -> Result<Vec<u32>, StoreRefused> {
  let mut order = (0..placed.len())
    .map(|index| u32::try_from(index).map_err(|_| StoreRefused::PackTooLarge))
    .collect::<Result<Vec<u32>, StoreRefused>>()?;
  order.sort_by_key(|index| {
    usize::try_from(*index)
      .ok()
      .and_then(|index| placed.get(index))
      .map_or(u64::MAX, |entry| entry.position)
  });
  Ok(order)
}

/// A version's two fragments, or a base pack's two parts, each kept as content already (its root
/// held once by the caller), recorded; refused, both roots are given back.
fn keep_both(
  store: &mut ContentStore,
  objects: (fragment::Encoded, ContentId),
  blobs: (fragment::Encoded, ContentId),
) -> Result<Fragments, StoreRefused> {
  let roots = [objects.1, blobs.1];
  match (keep(objects), keep(blobs)) {
    (Ok(objects), Ok(blobs)) => Ok(Fragments { objects, blobs }),
    (Err(refused), _) | (_, Err(refused)) => {
      for root in roots {
        store.release(&root)?;
      }
      Err(refused)
    }
  }
}

/// The fragments' part (A-614 part 2; module docs and `fragment`'s).
impl Repository {
  /// A version's record, or why there is none.
  fn version_record(&self, green: u64, version: usize) -> Result<&Version, StoreRefused> {
    self
      .chains
      .get(&green)
      .and_then(|chain| chain.versions.get(version))
      .ok_or(StoreRefused::NoVersion {
        green,
        version: to_u64(version),
      })
  }

  /// An indexed object's type and bytes.
  pub(super) fn read(
    &self,
    store: &ContentStore,
    id: &ObjectId,
  ) -> Result<(Kind, Vec<u8>), StoreRefused> {
    let (row, content) = self
      .indexed(store, id)
      .ok_or(StoreRefused::Missing { id: *id })?;
    let bytes = store
      .object_bytes(&content, row.len)
      .ok_or(StoreRefused::Unreadable { id: *id })?;
    Ok((row.kind, bytes))
  }

  /// A tree's entries that are objects of this repository.
  pub(super) fn tree_entries(
    &self,
    store: &ContentStore,
    tree: &ObjectId,
  ) -> Result<TreeEntries, StoreRefused> {
    let (_, bytes) = self.read(store, tree)?;
    let malformed = StoreRefused::Malformed { id: *tree };
    let parsed =
      Tree::parse(&bytes, self.format, &mut Admission::new(store.room())).map_err(|_| malformed)?;
    let mut entries = TreeEntries::new();
    for entry in parsed
      .entries()
      .iter()
      .filter(|entry| entry.kind() != Kind::Commit)
    {
      let id = entry.object_id(self.format).ok_or(malformed)?;
      entries.insert(entry.name.to_vec(), (entry.is_dir(), id));
    }
    Ok(entries)
  }

  /// Counts a kept fragment's entries by how each holds its object.
  fn count_entries(&self, placed: &[Placed]) {
    for entry in placed {
      self.count(|counters| match entry.stored {
        Stored::Whole => counters.whole = counters.whole.saturating_add(1),
        Stored::Offset => counters.offset_deltas = counters.offset_deltas.saturating_add(1),
        Stored::Reference => {
          counters.reference_deltas = counters.reference_deltas.saturating_add(1);
        }
      });
    }
  }

  /// Charges what the chains and the head map take now against what was charged.
  fn recharge(&mut self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    let total = footprint(&self.chains, &BTreeMap::new(), &self.heads);
    if total > self.charged {
      store.charge(total.saturating_sub(self.charged))?;
    } else {
      store.discharge(self.charged.saturating_sub(total))?;
    }
    self.charged = total;
    Ok(())
  }

  /// Encodes `green`'s version `version` as its two fragments (`fragment`'s docs), kept as
  /// content, in one call: an [`Encoding`] driven to its end in one turn, for tests and tools. The
  /// owner encodes a version a turn at a time under its quantum ([`Encoding::advance`], A-653
  /// continued (2)), off the commit's answer's path. A version is encoded once. Refused, nothing is
  /// kept.
  pub fn encode_fragments(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<(), StoreRefused> {
    let encoding = Encoding::fragments(self, store, green, version)?;
    encoding.finish(self, store, clock, exchange)
  }

  /// The first version whose fragments are not kept yet, by green and then version (A-718; §4.17.2
  /// "A push" item 8): what the owner encodes next, so each version a push made is encoded once and
  /// every fetch after copies its fragments rather than searching its objects for deltas again.
  /// None once every version's fragments are kept.
  pub fn first_unencoded(&self) -> Option<(u64, usize)> {
    self.chains.iter().find_map(|(green, chain)| {
      chain
        .versions
        .iter()
        .position(|version| version.fragments.is_none())
        .map(|version| (*green, version))
    })
  }

  /// The bytes `green`'s version `version`'s two fragments hold, their parts' lengths as encoded (the pack bytes a clone
  /// copies of them); `None` while they are not kept.
  pub fn fragments_len(&self, green: u64, version: usize) -> Option<u64> {
    let fragments = self.version_record(green, version).ok()?.fragments.as_ref()?;
    Some(fragments.objects.len.saturating_add(fragments.blobs.len))
  }

  /// Every version whose fragments are kept, by green and then version, with the bytes dropping
  /// them gives back: their content and what their entries are charged (A-724 piece 4): what a repository over its memory share may drop, since a version
  /// whose fragments are gone is encoded again by the next fetch (A-718), as after a recovery.
  pub fn kept_fragments(&self, store: &ContentStore) -> Vec<((u64, usize), u64)> {
    self
      .chains
      .iter()
      .flat_map(|(green, chain)| {
        chain
          .versions
          .iter()
          .enumerate()
          .filter_map(move |(version, record)| {
            let fragments = record.fragments.as_ref()?;
            let bytes = [&fragments.objects, &fragments.blobs]
              .into_iter()
              .map(|kept| {
                let content = store.len_of(&kept.content).unwrap_or(0);
                let runs = kept.positions.runs().saturating_add(kept.references.runs());
                content
                  .saturating_add(bytes(
                    kept.placed.len(),
                    PLACED_BYTES.saturating_add(ORDER_BYTES),
                  ))
                  .saturating_add(bytes(runs, RUN_BYTES))
              })
              .fold(0u64, u64::saturating_add);
            Some(((*green, version), bytes))
          })
      })
      .collect()
  }

  /// Drops `green`'s version `version`'s kept fragments (A-724 piece 4): their content let go and
  /// the store's charge trued up, so the next fetch encodes the version again (A-718). Whether
  /// fragments were kept to drop.
  pub fn drop_fragments(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
  ) -> Result<bool, StoreRefused> {
    let Some(fragments) = fragments_slot(&mut self.chains, green, version).and_then(Option::take)
    else {
      return Ok(false);
    };
    store.release(&fragments.objects.content)?;
    store.release(&fragments.blobs.content)?;
    self.recharge(store)?;
    Ok(true)
  }

  /// What `green`'s version `version` brought, still to encode as its fragments; `None` once they
  /// are kept (A-653 continued (2)).
  pub(super) fn unencoded(
    &self,
    store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Option<Positions>, StoreRefused> {
    let record = self.version_record(green, version)?;
    if record.fragments.is_some() {
      return Ok(None);
    }
    Ok(Some(self.sets(store, green, version)?.brought))
  }

  /// `green`'s head version and every object it reaches, read from the version's value (A-647 part 3): what its base
  /// pack is made from.
  pub(super) fn head_reach(
    &self,
    store: &ContentStore,
    green: u64,
  ) -> Result<(usize, Positions), StoreRefused> {
    let none = StoreRefused::NoVersion { green, version: 0 };
    let chain = self.chains.get(&green).ok_or(none)?;
    let version = chain.versions.len().checked_sub(1).ok_or(none)?;
    Ok((version, self.sets(store, green, version)?.reached))
  }

  /// The encoder's learned rules, this repository's own, with the owner's exchange (its measured rates among it).
  pub(super) fn learning(&mut self, exchange: Exchange) -> Learning<'_> {
    Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth.limit(),
      exchange,
    }
  }

  /// Records a version's two encoded fragments, each kept as content already under its root (A-718:
  /// put a window a unit); a version whose fragments were kept meanwhile keeps those, the roots given
  /// back. Refused, nothing is kept.
  pub(super) fn keep_version(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    (objects, blobs): (
      (fragment::Encoded, ContentId),
      (fragment::Encoded, ContentId),
    ),
  ) -> Result<(), StoreRefused> {
    if self.version_record(green, version)?.fragments.is_some() {
      store.release(&objects.1)?;
      store.release(&blobs.1)?;
      return Ok(());
    }
    let fragments = keep_both(store, objects, blobs)?;
    self.install(store, (green, version), fragments)
  }

  /// Records a base pack's two encoded parts, each kept as content already under its root, in place
  /// of the old ones, made from `green`'s version `version` in `ns` of the owner's time. Refused,
  /// the old base pack stays.
  pub(super) fn keep_base(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    (objects, blobs): (
      (fragment::Encoded, ContentId),
      (fragment::Encoded, ContentId),
    ),
    ns: u64,
  ) -> Result<(), StoreRefused> {
    let parts = keep_both(store, objects, blobs)?;
    self.install_base(store, green, BasePack { version, parts }, ns)
  }

  /// Records a version's fragments and charges their entries; refused, the fragments' content is
  /// let go and nothing changed.
  fn install(
    &mut self,
    store: &mut ContentStore,
    (green, version): (u64, usize),
    fragments: Fragments,
  ) -> Result<(), StoreRefused> {
    let contents = [fragments.objects.content, fragments.blobs.content];
    self.count_entries(&fragments.objects.placed);
    self.count_entries(&fragments.blobs.placed);
    if let Some(kept) = fragments_slot(&mut self.chains, green, version) {
      *kept = Some(fragments);
    }
    if let Err(refused) = self.recharge(store) {
      if let Some(kept) = fragments_slot(&mut self.chains, green, version) {
        *kept = None;
      }
      for content in contents {
        store.release(&content)?;
      }
      return Err(refused);
    }
    self.count(|counters| counters.fragments = counters.fragments.saturating_add(2));
    Ok(())
  }

  /// Makes `green`'s base pack again from every object its current head reaches, self-contained
  /// (offset deltas only), as two parts (commits, trees and tags; then blobs), and teaches the
  /// rebuild rule its measured time, in one call: an [`Encoding`] driven to its end in one turn,
  /// for tests and tools. The owner makes it a turn at a time under its quantum
  /// ([`Encoding::advance`], A-653 continued (2)). Refused, the old base pack stays.
  pub fn rebuild_base(
    &mut self,
    store: &mut ContentStore,
    green: u64,
    clock: &mut dyn Clock,
    exchange: Exchange,
  ) -> Result<(), StoreRefused> {
    let encoding = Encoding::base(self, store, green)?;
    encoding.finish(self, store, clock, exchange)
  }

  /// Records a base pack in place of the old one, charges it, and teaches the rule its time.
  fn install_base(
    &mut self,
    store: &mut ContentStore,
    green: u64,
    base: BasePack,
    ns: u64,
  ) -> Result<(), StoreRefused> {
    let contents = [base.parts.objects.content, base.parts.blobs.content];
    let placed: Vec<Placed> = [&base.parts.objects, &base.parts.blobs]
      .into_iter()
      .flat_map(|kept| kept.placed.iter().copied())
      .collect();
    let old = self
      .chains
      .get_mut(&green)
      .and_then(|chain| chain.base.replace(base));
    if let Err(refused) = self.recharge(store) {
      if let Some(chain) = self.chains.get_mut(&green) {
        chain.base = old;
      }
      for content in contents {
        store.release(&content)?;
      }
      return Err(refused);
    }
    if let Some(old) = old {
      store.release(&old.parts.objects.content)?;
      store.release(&old.parts.blobs.content)?;
    }
    if let Some(chain) = self.chains.get_mut(&green) {
      chain.rebuild.rebuilt(ns);
    }
    self.count_entries(&placed);
    self.count(|counters| counters.rebuilds = counters.rebuilds.saturating_add(1));
    Ok(())
  }

  /// A clone of `green` spent `ns` serving the fragments after its base pack: the rebuild rule's
  /// rent.
  pub fn clone_took(&mut self, green: u64, ns: u64) {
    if let Some(chain) = self.chains.get_mut(&green) {
      chain.rebuild.cloned(ns);
    }
  }

  /// Whether `green`'s base pack is due to be made again (`learn`'s rebuild rule).
  pub fn rebuild_due(&self, green: u64) -> bool {
    self
      .chains
      .get(&green)
      .is_some_and(|chain| chain.rebuild.due())
  }

  /// The entries of `green`'s version `version`'s fragments: commits, trees and tags, then blobs.
  pub fn fragments(&self, green: u64, version: usize) -> Option<(&[Placed], &[Placed])> {
    let fragments = self
      .version_record(green, version)
      .ok()?
      .fragments
      .as_ref()?;
    Some((&fragments.objects.placed, &fragments.blobs.placed))
  }
}

/// What upload-pack's stream reads (A-639, `upload`): every part by green, version and class, never
/// a borrow it keeps.
impl Repository {
  /// A copy of the learned rules a stream encodes with: the window, the levels and the depth.
  pub(super) fn rules(&self) -> (Window, Levels, u64) {
    (self.window.clone(), self.levels.clone(), self.depth.limit())
  }

  /// `green`'s version `version`'s fragment of blobs, or of the other kinds, once encoded.
  pub(super) fn fragment_part(&self, green: u64, version: usize, blobs: bool) -> Option<&Kept> {
    let fragments = self
      .chains
      .get(&green)?
      .versions
      .get(version)?
      .fragments
      .as_ref()?;
    Some(if blobs {
      &fragments.blobs
    } else {
      &fragments.objects
    })
  }

  /// `green`'s base pack's part of blobs, or of the other kinds, with the version it was made at.
  pub(super) fn base_part(&self, green: u64, blobs: bool) -> Option<(usize, &Kept)> {
    let base = self.chains.get(&green)?.base.as_ref()?;
    let part = if blobs {
      &base.parts.blobs
    } else {
      &base.parts.objects
    };
    Some((base.version, part))
  }

  /// The position the next object new to the repository gets: every position below it is given
  /// (A-665: the hook's view shows every object the repository holds).
  pub(super) fn next_position(&self) -> u64 {
    self.next_position
  }

  /// Every green with a chain, in order (A-665: the hook's view weighs each one's base pack).
  pub(super) fn greens(&self) -> impl Iterator<Item = u64> + '_ {
    self.chains.keys().copied()
  }

  /// The position of `green`'s version `version`'s head; `None` for a deleted ref.
  pub(super) fn head_position(&self, green: u64, version: usize) -> Option<u64> {
    self.chains.get(&green)?.versions.get(version)?.head
  }

  /// Each green, in order, with the position of each head its versions had.
  pub(super) fn heads(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
    self.chains.iter().flat_map(|(green, chain)| {
      chain
        .versions
        .iter()
        .filter_map(move |version| Some((*green, version.head?)))
    })
  }

  /// The object at `position`: its id, its row and its root content.
  pub(super) fn stored_at(
    &self,
    store: &ContentStore,
    position: u64,
  ) -> Result<(ObjectId, ObjectRow, ContentId), StoreRefused> {
    let id = self
      .id_at(store, position)
      .ok_or(StoreRefused::NoPosition { position })?;
    let (row, content) = self
      .indexed(store, &id)
      .ok_or(StoreRefused::Missing { id })?;
    Ok((id, row, content))
  }
}
