//! A push's objects before its verdict (A-614; §4.17.2 "A push", items 2 and 3; §4.17.1 "Objects" items 4 and 5): each
//! named once by git's rule, checked against an id its source claims, and held as content by the push alone, so a
//! refused or abandoned push leaves nothing behind. A commit is read as it arrives for its tree, its parents and its
//! committer's date; its generation number waits for the push to be sealed ([`crate::store::Repository::seal`]), since
//! a pack sends a commit before its parents as often as after.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_vcs::{AdmitRefused, Admitted, ContentRefused, ContentStore, Naming, Quarantine};

use crate::Admission;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::Ident;
use crate::objects::commit::Commit;

use super::naming::NameRefused;

/// Objects coming into a repository from outside (module docs): a pack as a reader resolves it (A-608's `unpack`), an
/// import's loose objects (piece 15). The store depends on this trait and on no reader's module.
pub trait Entries {
  /// Why the source stopped.
  type Refused;

  /// The next object, `None` once the source is whole.
  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Self::Refused>>;
}

/// One object as its source hands it in.
#[derive(Clone, Copy, Debug)]
pub struct Incoming<'a> {
  /// Its type.
  pub kind: Kind,
  /// The id its source names it by (a reader names each object it resolves; an import's loose object is filed under
  /// one), checked against git's naming; `None` when the source names none.
  pub claimed: Option<ObjectId>,
  /// Its content: the bytes git hashes after the header.
  pub content: &'a [u8],
  /// The depth of the delta chain its source's pack stored it at, zero whole: what the
  /// deployment's own clients chose, which the fragments' depth learns from (A-614 part 2).
  pub depth: u64,
}

/// Why a push's admission was refused. The push keeps what it held before, for its caller to abandon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushRefused<R> {
  /// The source refused (a pack cut short, a delta whose base is missing): its own reason.
  Source(R),
  /// git's naming refused the object: a collision attack, in git's words through [`NameRefused::words`].
  NotNamed {
    /// git's reason.
    reason: NameRefused,
  },
  /// The object does not hash to the id its source claimed.
  IdMismatch {
    /// The id claimed.
    id: ObjectId,
  },
  /// The push brought the id once more with other bytes.
  Collision {
    /// The id.
    id: ObjectId,
  },
  /// A commit git's parser refuses, or whose committer names no date it can read (`fsck`'s whole judgement is piece
  /// 6's).
  BadCommit {
    /// The commit.
    id: ObjectId,
  },
  /// The store refused the bytes (the budget).
  Content(ContentRefused),
}

/// What a push holds of a commit beyond its bytes: its tree, parents and date, read as it arrived, and its generation
/// number once the push is sealed (zero before).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Arrived {
  pub(crate) tree: ObjectId,
  pub(crate) parents: Vec<ObjectId>,
  pub(crate) date: u64,
  pub(crate) generation: u64,
}

/// A push's objects (module docs).
#[derive(Debug)]
pub struct Push {
  format: ObjectFormat,
  quarantine: Quarantine,
  commits: BTreeMap<ObjectId, Arrived>,
  sealed: bool,
  /// The deepest delta chain the push's source carried.
  deepest: u64,
}

impl Push {
  /// An empty push into a repository of `format`.
  pub fn new(format: ObjectFormat) -> Push {
    Push {
      format,
      quarantine: Quarantine::new(),
      commits: BTreeMap::new(),
      sealed: false,
      deepest: 0,
    }
  }

  /// The repository's format.
  pub fn format(&self) -> ObjectFormat {
    self.format
  }

  /// Admits one object: named by `naming` once, checked against the id its source claims, its bytes held; a commit
  /// read for its tree, parents and date. The id, and whether the push held it already.
  pub fn admit<N, R>(
    &mut self,
    store: &mut ContentStore,
    naming: &N,
    incoming: Incoming<'_>,
  ) -> Result<(ObjectId, Admitted), PushRefused<R>>
  where
    N: Naming<Id = ObjectId, Refused = NameRefused>,
  {
    let number = incoming.kind.pack_number();
    let id = naming
      .name(number, &[], incoming.content)
      .map_err(|reason| PushRefused::NotNamed { reason })?;
    if let Some(claimed) = incoming.claimed
      && claimed != id
    {
      return Err(PushRefused::IdMismatch { id: claimed });
    }
    let admitted = self
      .quarantine
      .hold_named(store, id.as_bytes(), number, incoming.content)
      .map_err(|refused| held_refused(refused, id))?;
    if incoming.kind == Kind::Commit && admitted == Admitted::New {
      let room = store.room();
      self.read_commit(id, incoming.content, room)?;
    }
    self.sealed = false;
    self.deepest = self.deepest.max(incoming.depth);
    Ok((id, admitted))
  }

  /// Admits every object `entries` hands in, in order; how many were new to the push.
  pub fn admit_all<N, E>(
    &mut self,
    store: &mut ContentStore,
    naming: &N,
    entries: &mut E,
  ) -> Result<u64, PushRefused<E::Refused>>
  where
    N: Naming<Id = ObjectId, Refused = NameRefused>,
    E: Entries,
  {
    let mut new: u64 = 0;
    while let Some(next) = entries.next_entry() {
      let incoming = next.map_err(PushRefused::Source)?;
      if self.admit(store, naming, incoming)?.1 == Admitted::New {
        new = new.saturating_add(1);
      }
    }
    Ok(new)
  }

  /// Reads a commit's tree, parents and committer's date, within an admission of the store's room.
  fn read_commit<R>(
    &mut self,
    id: ObjectId,
    content: &[u8],
    room: u64,
  ) -> Result<(), PushRefused<R>> {
    let bad = || PushRefused::BadCommit { id };
    let commit = Commit::parse(content, &mut Admission::new(room)).map_err(|_| bad())?;
    let tree = commit.tree(self.format).ok_or_else(bad)?;
    let parents: Vec<ObjectId> = commit.parents(self.format).collect();
    let date = commit
      .committer()
      .and_then(|line| Ident::parse(line).ok())
      .map(|ident| ident.seconds)
      .ok_or_else(bad)?;
    self.commits.insert(
      id,
      Arrived {
        tree,
        parents,
        date,
        generation: 0,
      },
    );
    Ok(())
  }

  /// The objects held.
  pub fn len(&self) -> usize {
    self.quarantine.len()
  }

  /// Whether no object is held.
  pub fn is_empty(&self) -> bool {
    self.quarantine.is_empty()
  }

  /// The deepest delta chain the push's source carried.
  pub fn deepest(&self) -> u64 {
    self.deepest
  }

  /// Whether the push is sealed: every commit's generation number is known.
  pub fn is_sealed(&self) -> bool {
    self.sealed
  }

  /// The kind, root content and length of the object held under `id`.
  pub(crate) fn object(&self, id: &ObjectId) -> Option<(u8, slates_vcs::ContentId, u64)> {
    self.quarantine.object(id.as_bytes())
  }

  /// The commits held, with what was read of each.
  pub(crate) fn commits(&self) -> &BTreeMap<ObjectId, Arrived> {
    &self.commits
  }

  /// Seals the push with every commit's generation number.
  pub(crate) fn seal_with(&mut self, generations: &BTreeMap<ObjectId, u64>) {
    for (id, arrived) in &mut self.commits {
      arrived.generation = generations.get(id).copied().unwrap_or(0);
    }
    self.sealed = true;
  }

  /// Every object held, in its ids' order: its id's bytes, its type's number, its root content and its length.
  pub(crate) fn held(&self) -> impl Iterator<Item = (&[u8], u8, slates_vcs::ContentId, u64)> + '_ {
    self.quarantine.objects()
  }

  /// Gives back the push's holds once its objects' rows hold them, leaving the push empty.
  pub(crate) fn release_holds(&mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    self.commits.clear();
    self.quarantine.abandon(store)
  }

  /// Gives back every hold the push took, leaving nothing behind.
  pub fn abandon(mut self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    self.quarantine.abandon(store)
  }
}

/// A refusal of holding the named object `id`, in the push's terms.
fn held_refused<R>(refused: AdmitRefused<Infallible>, id: ObjectId) -> PushRefused<R> {
  match refused {
    AdmitRefused::Content(content) => PushRefused::Content(content),
    AdmitRefused::Collision { .. } => PushRefused::Collision { id },
    AdmitRefused::IdMismatch { .. } => PushRefused::IdMismatch { id },
    AdmitRefused::NotNamed { reason, .. } => match reason {},
  }
}
