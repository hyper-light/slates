//! A git repository's index, versions and walks (A-614; §4.17.2 "The repository in git's terms", items 2 and 3; piece
//! 5, part 1): the store's module docs say what each holds and why.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
use std::mem::size_of;

use slates_vcs::{ContentId, ContentRefused, ContentStore, Index, IndexRefused, NewRow};

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tag::Tag;
use crate::objects::tree::Tree;

use super::naming::{GitNaming, kind_of};
use super::positions::Positions;
use super::push::Push;
use super::rows::{self, CommitRow, ObjectRow};

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
  /// Reached sets made again (a create, a rewrite).
  pub remade: u64,
  /// Of those, the ones that started from another green's set.
  pub bases_found: u64,
}

/// The entry a path names in a version's tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
  /// Its mode as git wrote it.
  pub mode: u16,
  /// Its object.
  pub id: ObjectId,
}

/// One version of a ref: its head's position (`None` for a deleted ref) and D(k), what it brought.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Version {
  head: Option<u64>,
  brought: Positions,
}

/// A ref's chain of versions and the set its current head reaches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Chain {
  versions: Vec<Version>,
  reached: Positions,
}

impl Chain {
  fn head(&self) -> Option<u64> {
    self.versions.last().and_then(|version| version.head)
  }

  /// What the chain charges the budget: its records and its sets' runs.
  fn footprint(&self) -> u64 {
    let runs = self
      .versions
      .iter()
      .fold(self.reached.runs(), |runs, version| {
        runs.saturating_add(version.brought.runs())
      });
    bytes(self.versions.len(), VERSION_BYTES).saturating_add(bytes(runs, RUN_BYTES))
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
  /// A create or a rewrite: the set made again, a base set and what a walk found beyond it.
  Remade { base: Positions, beyond: Members },
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

/// A row to write, owned until the batch is inserted.
struct OwnedRow {
  key: Vec<u8>,
  contents: Vec<ContentId>,
  payload: Vec<u8>,
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
  counters: Cell<StoreCounters>,
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
      counters: Cell::new(StoreCounters::default()),
    }
  }

  /// The repository's object format.
  pub fn format(&self) -> ObjectFormat {
    self.format
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

  fn count(&self, change: impl FnOnce(&mut StoreCounters)) {
    let mut counters = self.counters.get();
    change(&mut counters);
    self.counters.set(counters);
  }

  /// The row of the object `id` names, and its root content.
  fn indexed(&self, store: &ContentStore, id: &ObjectId) -> Option<(ObjectRow, ContentId)> {
    let found = self.index.get(store, &rows::object_key(id))?;
    let content = found.contents().next()?;
    Some((rows::decode(found.payload())?, content))
  }

  /// The row of the object `id` names.
  pub fn object(&self, store: &ContentStore, id: &ObjectId) -> Option<ObjectRow> {
    self.indexed(store, id).map(|(row, _)| row)
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

  /// Whether the commit `ancestor` is `head` or one of its ancestors: a walk back by generation number from `head`, which
  /// stops below the ancestor's generation, so it costs the commits between. Either not a commit: not a fast-forward.
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
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut heap = BinaryHeap::from([(self.generation(store, push, &head)?, head)]);
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

  /// The green whose current head is the newest commit `head` peels to or descends from, found back through generation
  /// numbers; `None` when no green's head is among them.
  fn base(
    &self,
    store: &ContentStore,
    push: &Push,
    head: ObjectId,
  ) -> Result<Option<u64>, StoreRefused> {
    let Some(start) = self.peel(store, push, head)? else {
      return Ok(None);
    };
    let mut seen: BTreeSet<ObjectId> = BTreeSet::new();
    let mut heap = BinaryHeap::from([(self.generation(store, push, &start)?, start)]);
    while let Some((_, id)) = heap.pop() {
      if !seen.insert(id) {
        continue;
      }
      if let Some(green) = self.green_at(store, &id) {
        return Ok(Some(green));
      }
      for parent in self.parents(store, push, &id)? {
        heap.push((self.generation(store, push, &parent)?, parent));
      }
    }
    Ok(None)
  }

  /// A green whose current head is the indexed commit `id`.
  fn green_at(&self, store: &ContentStore, id: &ObjectId) -> Option<u64> {
    let (row, _) = self.indexed(store, id)?;
    self.heads.get(&row.position)?.iter().next().copied()
  }

  /// The reached set made again for a head `to` (module docs): a base green's set, and what a walk finds beyond it.
  fn remade(
    &self,
    store: &ContentStore,
    push: &Push,
    to: ObjectId,
  ) -> Result<(Positions, Members), StoreRefused> {
    let base = match self.base(store, push, to)? {
      Some(green) => {
        self.count(|counters| counters.bases_found = counters.bases_found.saturating_add(1));
        self
          .chains
          .get(&green)
          .map(|chain| chain.reached.clone())
          .unwrap_or_default()
      }
      None => Positions::new(),
    };
    let beyond = self.walk(store, push, to, &base)?;
    self.count(|counters| counters.remade = counters.remade.saturating_add(1));
    Ok((base, beyond))
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
    match (from, chain) {
      (Some(from), Some(chain)) => self.admit_onto(store, push, (green, from, &chain.reached), to),
      _ => self.admit_create(store, push, green, to),
    }
  }

  /// A move of a green that has a head: D(k) walked against its reached set, then a fast-forward or a rewrite.
  fn admit_onto(
    &self,
    store: &ContentStore,
    push: &Push,
    (green, from, reached): (u64, u64, &Positions),
    to: ObjectId,
  ) -> Result<Move, StoreRefused> {
    let brought = self.walk(store, push, to, reached)?;
    let old = self
      .id_at(store, from)
      .ok_or(StoreRefused::Unreadable { id: to })?;
    let reach = if self.fast_forward(store, push, to, old)? {
      self.count(|counters| counters.fast_forwards = counters.fast_forwards.saturating_add(1));
      Reach::Grown
    } else {
      let (base, beyond) = self.remade(store, push, to)?;
      Reach::Remade { base, beyond }
    };
    Ok(Move {
      green,
      from: Some(from),
      to: Some(to),
      brought,
      reach,
    })
  }

  /// A move that makes a ref, or makes a deleted one again: D(0) is everything its head reaches, the set made again,
  /// with no walk of what a base green reaches.
  fn admit_create(
    &self,
    store: &ContentStore,
    push: &Push,
    green: u64,
    to: ObjectId,
  ) -> Result<Move, StoreRefused> {
    let (base, beyond) = self.remade(store, push, to)?;
    let brought = Members {
      indexed: base.union(&beyond.indexed),
      new: beyond.new.clone(),
    };
    Ok(Move {
      green,
      from: None,
      to: Some(to),
      brought,
      reach: Reach::Remade { base, beyond },
    })
  }

  /// Commits `push` and `moves` once the verdict accepts them: a position and rows for each object new to the
  /// repository, and a version for each move with what it brought, its reached set and the head map moved. The rows and
  /// the versions' bytes are admitted first: refused, nothing changed, and the push still holds its objects. Committed,
  /// the push holds nothing. The versions made, one for each move, in order.
  pub fn commit(
    &mut self,
    store: &mut ContentStore,
    push: &mut Push,
    moves: &[Move],
  ) -> Result<Vec<u64>, StoreRefused> {
    self.same_format(push)?;
    let first = moves.first().ok_or(StoreRefused::NoMove)?;
    self.check_moves(moves)?;
    let fresh = self.fresh(store, push)?;
    let (positions, next) = self.positions_for(&fresh)?;
    let brought_by = self.brought_by(moves, first);
    let owned = self.rows_for(store, push, &fresh, &positions, &brought_by)?;
    let (chains, heads) = self.plan(store, moves, &positions)?;
    let total = footprint(&self.chains, &chains, &heads);
    let more = total.saturating_sub(self.charged);
    store.charge(more)?;
    if let Err(refused) = self.insert(store, &owned) {
      store.discharge(more)?;
      return Err(refused);
    }
    store.discharge(self.charged.saturating_sub(total))?;
    self.charged = total;
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
    let given = u64::try_from(fresh.len()).unwrap_or(u64::MAX);
    self.count(|counters| counters.positions = counters.positions.saturating_add(given));
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

  /// The push's objects new to the repository, in their ids' order: id, type, root content and length.
  fn fresh(
    &self,
    store: &ContentStore,
    push: &Push,
  ) -> Result<Vec<(ObjectId, Kind, ContentId, u64)>, StoreRefused> {
    let mut fresh = Vec::new();
    for (bytes, number, content, len) in push.held() {
      let id = ObjectId::from_bytes(self.format, bytes).ok_or(StoreRefused::ForeignPush)?;
      if self.indexed(store, &id).is_none() {
        let kind = kind_of(number).ok_or(StoreRefused::Malformed { id })?;
        fresh.push((id, kind, content, len));
      }
    }
    Ok(fresh)
  }

  /// A position for each fresh object, the next ones in order; and the next position after them.
  fn positions_for(
    &self,
    fresh: &[(ObjectId, Kind, ContentId, u64)],
  ) -> Result<(BTreeMap<ObjectId, u64>, u64), StoreRefused> {
    let mut positions = BTreeMap::new();
    let mut next = self.next_position;
    for (id, ..) in fresh {
      positions.insert(*id, next);
      next = next
        .checked_add(1)
        .filter(|next| *next < u64::MAX)
        .ok_or(StoreRefused::PositionsExhausted)?;
    }
    Ok((positions, next))
  }

  /// For each object a move's walk found new, the green and version that brought it: the first move in order whose D(k)
  /// holds it; an object no move's holds is the first move's.
  fn brought_by(
    &self,
    moves: &[Move],
    first: &Move,
  ) -> (BTreeMap<ObjectId, (u64, u64)>, (u64, u64)) {
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

  /// The rows of the fresh objects in key order: each object's row, then each position's.
  fn rows_for(
    &self,
    store: &ContentStore,
    push: &Push,
    fresh: &[(ObjectId, Kind, ContentId, u64)],
    positions: &BTreeMap<ObjectId, u64>,
    (by, otherwise): &(BTreeMap<ObjectId, (u64, u64)>, (u64, u64)),
  ) -> Result<Vec<OwnedRow>, StoreRefused> {
    let mut owned = Vec::with_capacity(fresh.len().saturating_mul(2));
    for (id, kind, content, len) in fresh {
      let (green, version) = by.get(id).copied().unwrap_or(*otherwise);
      let row = ObjectRow {
        kind: *kind,
        len: *len,
        position: self.position_of(store, positions, id)?,
        green,
        version,
        commit: self.commit_row(store, push, positions, id, *kind)?,
      };
      owned.push(OwnedRow {
        key: rows::object_key(id),
        contents: vec![*content],
        payload: rows::encode(&row),
      });
    }
    for (id, position) in positions {
      owned.push(OwnedRow {
        key: rows::position_key(*position),
        contents: Vec::new(),
        payload: id.as_bytes().to_vec(),
      });
    }
    Ok(owned)
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

  /// Inserts the rows as one step.
  fn insert(&mut self, store: &mut ContentStore, owned: &[OwnedRow]) -> Result<(), StoreRefused> {
    let rows: Vec<NewRow<'_>> = owned
      .iter()
      .map(|row| NewRow {
        key: &row.key,
        contents: &row.contents,
        payload: &row.payload,
      })
      .collect();
    self.index.insert_all(store, &rows)?;
    Ok(())
  }

  /// The chains the moves make and the head map after them, planned on copies.
  fn plan(
    &self,
    store: &ContentStore,
    moves: &[Move],
    positions: &BTreeMap<ObjectId, u64>,
  ) -> Result<(BTreeMap<u64, Chain>, BTreeMap<u64, BTreeSet<u64>>), StoreRefused> {
    let mut chains: BTreeMap<u64, Chain> = BTreeMap::new();
    let mut heads = self.heads.clone();
    for movement in moves {
      let mut chain = self
        .chains
        .get(&movement.green)
        .cloned()
        .unwrap_or_default();
      let brought = self.members_positions(store, &movement.brought, positions)?;
      chain.reached = match &movement.reach {
        Reach::Grown => chain.reached.union(&brought),
        Reach::Remade { base, beyond } => {
          base.union(&self.members_positions(store, beyond, positions)?)
        }
        Reach::Emptied => Positions::new(),
      };
      let head = movement
        .to
        .map(|to| self.position_of(store, positions, &to))
        .transpose()?;
      move_head(&mut heads, movement.green, movement.from, head);
      chain.versions.push(Version { head, brought });
      chains.insert(movement.green, chain);
    }
    Ok((chains, heads))
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

  /// D(k): what `green`'s version `version` brought, as positions.
  pub fn brought(&self, green: u64, version: usize) -> Option<&Positions> {
    Some(&self.chains.get(&green)?.versions.get(version)?.brought)
  }

  /// The set `green`'s current head reaches.
  pub fn reached(&self, green: u64) -> Option<&Positions> {
    Some(&self.chains.get(&green)?.reached)
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

  /// Gives back everything the repository holds: its rows' holds, their tables, and its versions' charge. It goes on
  /// past a refusal and answers the first.
  pub fn release(self, store: &mut ContentStore) -> Result<(), StoreRefused> {
    let index = self.index.release(store).map_err(StoreRefused::from);
    index.and(store.discharge(self.charged).map_err(StoreRefused::from))
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
