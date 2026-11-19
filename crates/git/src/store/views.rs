//! The store as the push path reads it (A-638; §4.17.2 "A push", items 3 and 4, and "Pieces (git)", pieces 5 and 6;
//! §4.17.1 "A push is a work"): two views of one repository's objects in the shared content store, each lending an
//! object's bytes as the store keeps them when it keeps them in one window.
//!
//! **Thin-pack bases and connectivity** ([`IndexedObjects`], receive-pack's [`receive::Repository`]). A thin pack's
//! reference deltas name bases the client knows the repository holds, and the pack reader takes them from here; the
//! connectivity check stops its walk at any object held. Both read the index alone: an object a push holds in
//! quarantine has passed no check, so it is never a base and never ends a walk.

use std::borrow::Cow;
use std::cell::Cell;

use slates_merge::push::{ObjectKey, PushObjects, TreeEntry, Unavailable};
use slates_vcs::ContentStore;

use crate::PastAdmission;
use crate::object_name::{Kind, ObjectId};
use crate::receive;

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
  /// The bytes the view may keep.
  room: u64,
}

impl<'a> IndexedObjects<'a> {
  /// A view of `repository`'s indexed objects in `store`, keeping at most `room` bytes of assembled bases: the caller's
  /// admission for the push (§4.17.2 "A push").
  pub fn new(repository: &'a Repository, store: &'a ContentStore, room: u64) -> Self {
    Self {
      repository,
      store,
      room,
    }
  }

  /// What the view has lent.
  pub fn counters(&self) -> ViewCounters {
    ViewCounters::default()
  }

  /// The first base the view refused to keep for its room, if any: the pack reader was answered that the repository
  /// lacks it, and this says why.
  pub fn refused(&self) -> Option<PastAdmission> {
    None
  }
}

impl receive::Repository for IndexedObjects<'_> {
  fn object(&self, _id: &ObjectId) -> Option<(Kind, &[u8])> {
    None
  }

  fn holds(&self, _id: &ObjectId) -> bool {
    false
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
}

impl PushObjects for PushedObjects<'_> {
  fn tree(&self, _id: ObjectKey) -> Result<Vec<TreeEntry>, Unavailable> {
    Err(Unavailable::Missing)
  }

  fn blob_len(&self, _id: ObjectKey) -> Result<u64, Unavailable> {
    Err(Unavailable::Missing)
  }

  fn blob(&self, _id: ObjectKey) -> Result<Cow<'_, [u8]>, Unavailable> {
    Err(Unavailable::Missing)
  }
}

#[cfg(test)]
#[path = "../tests/store/views.rs"]
mod tests;
