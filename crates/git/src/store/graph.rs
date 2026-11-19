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
//! **Faults.** The trait reads cannot fail, and a store whose index and content agree never fails them. Where one does
//! (a position with no row, a tree or tag git's parser refuses), the read answers nothing and the first fault is kept
//! ([`StoreGraph::fault`]); the service refuses an answer made while one was kept, so a broken store never yields a
//! wrong pack.

use std::cell::Cell;
use std::collections::BTreeSet;

use slates_vcs::ContentStore;

use super::{Chain, Place, Repository, StoreRefused};
use crate::negotiation::{CommitRow, Graph};
use crate::object_name::{Kind, ObjectId};
use crate::store::rows::ObjectRow;

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
    }
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

  /// The objects the object at `position` names, read from its bytes: a tree's entries or a tag's object.
  fn named(&self, position: u64) -> Vec<u64> {
    let Some((id, _)) = self.row(position) else {
      return Vec::new();
    };
    let Some((row, content)) = self.repository.indexed(self.store, &id) else {
      self.failed(GraphFault::NoRow { position });
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
    let Some((id, _)) = self.row(position) else {
      return Vec::new();
    };
    let Some((row, content)) = self.repository.indexed(self.store, &id) else {
      self.failed(GraphFault::NoRow { position });
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

  /// The position of `id`, a fault when the store does not hold it.
  fn position_of(&self, id: &ObjectId) -> Option<u64> {
    let position = self
      .repository
      .object(self.store, id)
      .map(|row| row.position);
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
    self.row(node).map_or(Kind::Blob, |(_, row)| row.kind)
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
    let read = usize::try_from(version)
      .map_err(|_| StoreRefused::NoVersion { green, version })
      .and_then(|index| self.repository.brought(self.store, green, index));
    let positions = match read {
      Ok(positions) => positions.iter().collect::<Vec<u64>>(),
      Err(StoreRefused::NoVersion { .. }) => Vec::new(),
      Err(refused) => {
        self.failed(GraphFault::Store(refused));
        Vec::new()
      }
    };
    positions.into_iter()
  }
}

#[cfg(test)]
#[path = "../tests/store_graph.rs"]
mod tests;
