//! Derived data, one mechanism (A-645 "Derived data, one mechanism"; A-647; §4.17.1 "Objects" item 2 and "Reads").
//!
//! Every structure a version-control server keeps that is not a change is derived data: git's trees and pack
//! fragments, Mercurial's manifests, Subversion's node revisions, a version's map of paths to contents, blame, merge
//! bases. A-645 asks for one mechanism for all of them, as Scion keeps (A-645's source, Anthropic's "Redesigning source
//! control") and as Sapling's server keeps under `eden/mononoke/derived_data` \[C\]: `research/edenfs-scale-distribution.md`
//! §2.1. This module is that mechanism; [`paths`] is its first type, [`ancestry`] (A-666) the type that answers
//! is-ancestor, merge bases and bisect's midpoint, [`history`] (A-666 part 2) the one that answers each path's
//! history, `log <path>` and `--follow`, and [`blame`] (A-666 part 3) each line's introducing version.
//!
//! **A type** ([`DerivedType`]) is a pure function of a version's change and its parents' values of the same type: its
//! [`DerivedType::derive`] reads the change from the history ([`Graph`]) and the parents' values from the store, and
//! gives the root of the new value. No clock, no randomness and no state outside the store enter it, so the same
//! version gives the same value on every server and tier that computes it.
//!
//! **Kept immutable under (type, version).** A value is content in the shared store ([`crate::content`]): a tree of
//! derived nodes ([`crate::content::Form::Derived`]) whose root a row of an index ([`crate::index`]) names, under the key
//! made of the type's name and the version's. The index keeps a key's row for ever and refuses another row for it, so
//! a value once kept never changes: a second computation of it, here or on another tier, is the same row
//! ([`DerivedCounters::identical`]), and a different one is refused [`DerivedRefused::Diverged`], the recomputation
//! mismatch §4.16 calls fatal and loud. Any server or cache tier can therefore compute, keep and serve a value without
//! asking anyone: the key names one value for good. A type's name carries every parameter its values depend on (the
//! path map's split window, say), so two tiers that chose different parameters never meet at a key.
//!
//! **Eager or lazy, by the type's own declaration** ([`Mode`]). An eager type's value is computed as each version is
//! recorded ([`Derivations::recorded`] answers the fill to run); a lazy type's when it is first read
//! ([`Derivations::read`]). Either way a value is computed by a [`Fill`].
//!
//! **A fill** walks up from its version through the parents whose values are not kept, to the nearest ancestors whose
//! values are (or to the history's roots), then derives downward, every parent before its child, and keeps each value
//! as it goes. A type whose change names another earlier version (a copy from it) says so ([`DerivedType::inputs`]),
//! and the fill walks and derives that version first as it does a parent. It does one unit a step ([`Fill::step`]): one version walked through, or one derived, so the owner runs
//! it in its shard's slices, a unit at a time while the slice lasts, however long the history it fills (the server's
//! `image::sliced` is that loop). What it waits on is charged to the store's budget as it grows and refused past it,
//! typed, so a fill over a history of any length is bounded. A derivation is one unit as large as its version's
//! change, which the change's own admission bounded.
//!
//! **Typed refusals** ([`DerivedRefused`]): a version or a parent the history does not hold, a history that is its own
//! ancestor, the type's own refusal (named with the version), a value that differs from the one kept, a row that does
//! not name one value, and the store's and the index's own.
//!
//! **Evidence** (`derived/tests.rs`): the path map's oracle runs generated histories with merges, an eager and a lazy
//! declaration, refusals and lazy reads, and holds every value to a naive recomputation from the root (the model's map
//! at that version, built from scratch), with each derivation's directories counted against the change's own (the
//! non-vacuity counter: an incremental derivation enters only the directories on its change's paths) and every fill's
//! derivations counted against the versions it had to fill.
//!
//! **Owed:** the other stores as types (A-553's node revisions, A-588's manifests; A-614's D(k) is one since A-647 part
//! 2, in `slates-git`); dropping a value no version needs, and releasing a value a slice at a time (one call today, as
//! A-581's releases); a derivation larger than a slice derived in parts; values replicated between tiers by identity.
//!
//! No I/O, no clock, no randomness, no panic.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::mem::size_of;

use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::index::{Index, IndexRefused, Inserted, NewRow};

pub mod ancestry;
pub mod blame;
pub mod history;
pub mod paths;

#[cfg(test)]
mod tests;

/// Format: the bytes of a type's name's length at the head of a key, big-endian, so that no two (type, version) pairs
/// share a key whatever their names' and versions' bytes.
const NAME_LEN_BYTES: usize = size_of::<u16>();

/// A history: the versions derived data is computed over, each with its parents (A-647).
pub trait Graph {
  /// A version of the history.
  type Version: Copy + Ord + Debug;

  /// `version`'s parents in order, the first the one its change is stated against; `None` when the history holds no
  /// such version. A root has none.
  fn parents(&self, version: Self::Version) -> Option<Vec<Self::Version>>;

  /// Writes the bytes that name `version` in the keys its values are kept under: the same bytes for the same version on
  /// every tier, and never another version's.
  fn name(&self, version: Self::Version, out: &mut Vec<u8>);
}

/// When a type's values are computed: the type's own declaration (A-647).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  /// As each version is recorded ([`Derivations::recorded`]): a type nearly every read needs, as a checkout needs its
  /// version's map.
  Eager,
  /// When a version's value is first read ([`Derivations::read`]): a type few reads need, as few need a file's blame.
  Lazy,
}

/// A type of derived data: a pure function of a version's change and its parents' values of the same type (A-647;
/// module docs).
pub trait DerivedType {
  /// The history it is derived over, and what it reads of each version's change.
  type Source: Graph;
  /// Why it refuses to derive a version: its own reasons, typed.
  type Refused: Clone + Debug + PartialEq + Eq;

  /// Its name, with every parameter its values depend on: the head of every key its values are kept under, so two
  /// types, or one type under two parameters, never meet at a key.
  fn name(&self) -> &[u8];

  /// When its values are computed: its declaration.
  fn mode(&self) -> Mode;

  /// The versions whose values a derivation of `version` reads, in the order [`DerivedType::derive`] is given them: its
  /// parents, then any other version its change names (path history's copy from an earlier version, A-666 part 2). A
  /// fill derives each before `version`. `None` when the history holds no such version. Its parents, unless the type
  /// says more.
  fn inputs(
    &self,
    source: &Self::Source,
    version: <Self::Source as Graph>::Version,
  ) -> Option<Vec<<Self::Source as Graph>::Version>> {
    source.parents(version)
  }

  /// The value of `version`, from its change (read from `source`) and `inputs`, the values of its
  /// [`DerivedType::inputs`] in that order, its parents' first: the root of the value's contents in `store`, which the
  /// caller then holds once. The same version and the same inputs' values give the same root on every tier, at any
  /// time. Refused, it leaves the store as it was.
  fn derive(
    &mut self,
    source: &Self::Source,
    store: &mut ContentStore,
    version: <Self::Source as Graph>::Version,
    inputs: &[ContentId],
  ) -> Result<ContentId, Self::Refused>;
}

/// A type's versions.
pub type VersionOf<T> = <<T as DerivedType>::Source as Graph>::Version;

/// A type's refusals, as the mechanism answers them.
pub type RefusedOf<T> = DerivedRefused<VersionOf<T>, <T as DerivedType>::Refused>;

/// Why a fill, a read or a keep was refused (A-647). Each leaves the values kept and the store as they were; a fill
/// refused keeps what it had derived before, each of those values right.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DerivedRefused<V, R> {
  /// The history holds no version `version`: the one asked for, or a parent of one.
  UnknownVersion {
    /// The version.
    version: V,
  },
  /// `version` is its own ancestor: the history is not a history.
  Cycle {
    /// The version met again.
    version: V,
  },
  /// The type refused to derive `version`, for its own reason.
  Type {
    /// The version.
    version: V,
    /// The type's reason.
    refused: R,
  },
  /// The value kept under the type and `version` is `kept`, and the one derived or offered now is `derived`: two
  /// computations of one version disagree, which a pure type never does. Nothing is replaced.
  Diverged {
    /// The version.
    version: V,
    /// The root kept.
    kept: ContentId,
    /// The root derived or offered.
    derived: ContentId,
  },
  /// The row kept under the type and `version` does not name exactly one content: the values and their store disagree.
  Corrupt {
    /// The version.
    version: V,
  },
  /// The type's name is longer than a key's count of its bytes can say.
  NameTooLong {
    /// The name's length.
    len: usize,
  },
  /// The store refused: its budget, a count, a content not held.
  Content(ContentRefused),
  /// The index of values refused.
  Index(IndexRefused),
}

impl<V, R> From<ContentRefused> for DerivedRefused<V, R> {
  fn from(refused: ContentRefused) -> DerivedRefused<V, R> {
    DerivedRefused::Content(refused)
  }
}

impl<V, R> From<IndexRefused> for DerivedRefused<V, R> {
  fn from(refused: IndexRefused) -> DerivedRefused<V, R> {
    match refused {
      IndexRefused::Content(refused) => DerivedRefused::Content(refused),
      refused => DerivedRefused::Index(refused),
    }
  }
}

/// What the values have been through, for the tests' non-vacuity checks and the owner's report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DerivedCounters {
  /// Values derived: one derivation each.
  pub derived: u64,
  /// Versions a fill walked up through: each version whose value it found missing, once.
  pub walked: u64,
  /// Values a read, or a fill's step, found kept.
  pub found: u64,
  /// Values derived or offered that were kept already, the same.
  pub identical: u64,
  /// Versions a fill walked again because a parent's value it had seen kept was gone when it came to derive.
  pub rewalked: u64,
}

/// The values of every type that one owner keeps (a repository, or a tier's cache): one row a value, under the type's
/// name and the version's, naming the value's root (module docs).
#[derive(Debug, Default)]
pub struct Derivations {
  values: Index,
  counters: DerivedCounters,
}

/// A read's answer: the value kept, or the fill that computes it.
#[derive(Debug)]
pub enum Read<V> {
  /// The value's root, kept.
  Ready(ContentId),
  /// The fill to run, a step at a time, to the value.
  Fill(Fill<V>),
}

impl Derivations {
  /// No values.
  pub fn new() -> Derivations {
    Derivations::default()
  }

  /// The key `ty`'s value of `version` is kept under: the name's length, the name, then the version's bytes.
  fn key<T: DerivedType>(
    ty: &T,
    source: &T::Source,
    version: VersionOf<T>,
  ) -> Result<Vec<u8>, RefusedOf<T>> {
    let name = ty.name();
    let len =
      u16::try_from(name.len()).map_err(|_| DerivedRefused::NameTooLong { len: name.len() })?;
    let mut key = Vec::with_capacity(NAME_LEN_BYTES.saturating_add(name.len()));
    key.extend_from_slice(&len.to_be_bytes());
    key.extend_from_slice(name);
    source.name(version, &mut key);
    Ok(key)
  }

  /// The root kept under `key`, `version`'s.
  fn kept<V, R>(
    &self,
    store: &ContentStore,
    key: &[u8],
    version: V,
  ) -> Result<Option<ContentId>, DerivedRefused<V, R>> {
    let Some(row) = self.values.get(store, key) else {
      return Ok(None);
    };
    let mut contents = row.contents();
    match (contents.next(), contents.next(), row.payload().is_empty()) {
      (Some(root), None, true) => Ok(Some(root)),
      _ => Err(DerivedRefused::Corrupt { version }),
    }
  }

  /// `ty`'s value of `version`, if it is kept.
  pub fn value<T: DerivedType>(
    &self,
    store: &ContentStore,
    ty: &T,
    source: &T::Source,
    version: VersionOf<T>,
  ) -> Result<Option<ContentId>, RefusedOf<T>> {
    let key = Derivations::key(ty, source, version)?;
    self.kept(store, &key, version)
  }

  /// `ty`'s value of `version` if it is kept; else the fill that computes it, whatever the type's mode. The fill charges
  /// the store for what it waits on until it is released ([`Fill::release`]).
  pub fn read<T: DerivedType>(
    &mut self,
    store: &mut ContentStore,
    ty: &T,
    source: &T::Source,
    version: VersionOf<T>,
  ) -> Result<Read<VersionOf<T>>, RefusedOf<T>> {
    if let Some(root) = self.value(store, ty, source, version)? {
      self.counters.found = self.counters.found.saturating_add(1);
      return Ok(Read::Ready(root));
    }
    Ok(Read::Fill(Fill::start(store, version)?))
  }

  /// What recording `version` in `ty`'s history starts: for an eager type the fill that computes its value now (its
  /// parents' values are kept already, unless the type was declared after them, and then the fill walks up to them as
  /// any fill does); for a lazy type nothing.
  pub fn recorded<T: DerivedType>(
    &mut self,
    store: &mut ContentStore,
    ty: &T,
    version: VersionOf<T>,
  ) -> Result<Option<Fill<VersionOf<T>>>, RefusedOf<T>> {
    match ty.mode() {
      Mode::Lazy => Ok(None),
      Mode::Eager => Ok(Some(Fill::start(store, version)?)),
    }
  }

  /// Keeps `root` as `ty`'s value of `version`, computed elsewhere (another tier) over contents this store holds: the
  /// row holds the root, and the caller keeps its own holds. `Identical` when the same value is kept already; refused
  /// [`DerivedRefused::Diverged`] when another is.
  pub fn keep<T: DerivedType>(
    &mut self,
    store: &mut ContentStore,
    ty: &T,
    source: &T::Source,
    version: VersionOf<T>,
    root: ContentId,
  ) -> Result<Inserted, RefusedOf<T>> {
    let key = Derivations::key(ty, source, version)?;
    self.insert(store, &key, version, root)
  }

  /// Inserts the row naming `root` under `key`, the tail sealed first once it is due, so it never outgrows a table by
  /// more than a row, and a refusal of either leaves the values as they were.
  fn insert<V: Copy, R>(
    &mut self,
    store: &mut ContentStore,
    key: &[u8],
    version: V,
    root: ContentId,
  ) -> Result<Inserted, DerivedRefused<V, R>> {
    if self.values.seal_due(store) {
      self.values.seal(store)?;
    }
    let contents = [root];
    let row = NewRow {
      key,
      contents: &contents,
      payload: &[],
    };
    match self.values.insert(store, row) {
      Ok(Inserted::Identical) => {
        self.counters.identical = self.counters.identical.saturating_add(1);
        Ok(Inserted::Identical)
      }
      Ok(Inserted::New) => Ok(Inserted::New),
      Err(IndexRefused::RowDiffers) => {
        let Some(kept) = self.kept::<V, R>(store, key, version)? else {
          return Err(DerivedRefused::Corrupt { version });
        };
        Err(DerivedRefused::Diverged {
          version,
          kept,
          derived: root,
        })
      }
      Err(refused) => Err(refused.into()),
    }
  }

  /// Keeps `derived`, the root a derivation of `version` gave with one hold of the fill's own, under `key`: the row
  /// holds it, and the derivation's hold is given back whatever the answer, so a refused value leaves nothing behind.
  fn keep_derived<V: Copy, R>(
    &mut self,
    store: &mut ContentStore,
    key: &[u8],
    version: V,
    derived: ContentId,
  ) -> Result<ContentId, DerivedRefused<V, R>> {
    let inserted: Result<Inserted, DerivedRefused<V, R>> =
      self.insert(store, key, version, derived);
    let released = store.release(&derived);
    inserted?;
    released?;
    Ok(derived)
  }

  /// A fork: every value kept here, shared by identity (the index's fork: no table written, no row copied), and what
  /// either keeps after, its own.
  pub fn fork(&mut self, store: &mut ContentStore) -> Result<Derivations, IndexRefused> {
    Ok(Derivations {
      values: self.values.fork(store)?,
      counters: DerivedCounters::default(),
    })
  }

  /// Gives back every value's hold and the index's tables; a value no one else holds is freed whole. It goes on past a
  /// refusal and answers the first.
  pub fn release(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.values.release(store)
  }

  /// The values kept.
  pub fn len(&self) -> u64 {
    self.values.rows()
  }

  /// Whether no value is kept.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// The index of values, for the owner's merges and the times its rule learns from (A-581's `Compaction`).
  pub fn index(&mut self) -> &mut Index {
    &mut self.values
  }

  /// What the values have been through.
  pub fn counters(&self) -> DerivedCounters {
    self.counters
  }
}

/// What a fill's step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// One unit done, more to do.
  More,
  /// The fill's version's value, kept: its root.
  Done(ContentId),
}

/// Shape: what one version a fill waits on charges the store: its place on the stack, and its place among the versions
/// walked through. Derived from the types.
fn slot_bytes<V>() -> u64 {
  let bytes = size_of::<(V, bool)>().saturating_add(size_of::<V>());
  u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// The computation of one version's value (module docs): a walk up to the nearest kept ancestors, then derivations
/// downward, one unit a step.
#[derive(Debug)]
pub struct Fill<V> {
  target: V,
  /// The versions waiting, the newest on top, each with whether its parents were walked.
  waiting: Vec<(V, bool)>,
  /// The versions whose parents were walked and whose values are not kept yet: met again as a parent, a cycle.
  open: BTreeSet<V>,
  /// What the waiting versions charge the store.
  charged: u64,
  done: Option<ContentId>,
}

impl<V: Copy + Ord + Debug> Fill<V> {
  /// A fill of `target`, its one waiting version charged.
  fn start(store: &mut ContentStore, target: V) -> Result<Fill<V>, ContentRefused> {
    let slot = slot_bytes::<V>();
    store.charge(slot)?;
    Ok(Fill {
      target,
      waiting: vec![(target, false)],
      open: BTreeSet::new(),
      charged: slot,
      done: None,
    })
  }

  /// The version whose value it computes.
  pub fn target(&self) -> V {
    self.target
  }

  /// The versions it waits on now.
  pub fn waiting(&self) -> usize {
    self.waiting.len()
  }

  /// One unit (module docs): the top version's value found kept, its parents walked, or its value derived and kept.
  /// `Done` with the target's value once it is kept, and again on every later step.
  pub fn step<T>(
    &mut self,
    derivations: &mut Derivations,
    ty: &mut T,
    source: &T::Source,
    store: &mut ContentStore,
  ) -> Result<Progress, DerivedRefused<V, T::Refused>>
  where
    T: DerivedType,
    T::Source: Graph<Version = V>,
  {
    if let Some(root) = self.done {
      return Ok(Progress::Done(root));
    }
    let Some(&(version, walked)) = self.waiting.last() else {
      return Err(DerivedRefused::Corrupt {
        version: self.target,
      });
    };
    let key = Derivations::key(ty, source, version)?;
    if let Some(root) = derivations.kept::<V, T::Refused>(store, &key, version)? {
      derivations.counters.found = derivations.counters.found.saturating_add(1);
      return self.finished(store, version, root);
    }
    if walked {
      self.derive_top(derivations, ty, source, store, (version, &key))
    } else {
      self.walk_top(derivations, ty, source, store, version)
    }
  }

  /// Walks the top version's inputs ([`DerivedType::inputs`], its parents unless the type says more): each whose value
  /// is not kept waits above it, the first on top.
  fn walk_top<T>(
    &mut self,
    derivations: &mut Derivations,
    ty: &T,
    source: &T::Source,
    store: &mut ContentStore,
    version: V,
  ) -> Result<Progress, DerivedRefused<V, T::Refused>>
  where
    T: DerivedType,
    T::Source: Graph<Version = V>,
  {
    let Some(parents) = ty.inputs(source, version) else {
      return Err(DerivedRefused::UnknownVersion { version });
    };
    let mut missing = Vec::with_capacity(parents.len());
    for parent in parents.iter().rev() {
      if *parent == version || self.open.contains(parent) {
        return Err(DerivedRefused::Cycle { version: *parent });
      }
      let key = Derivations::key(ty, source, *parent)?;
      if derivations
        .kept::<V, T::Refused>(store, &key, *parent)?
        .is_none()
      {
        missing.push((*parent, false));
      }
    }
    let count = u64::try_from(missing.len()).unwrap_or(u64::MAX);
    let need = slot_bytes::<V>().saturating_mul(count);
    store.charge(need)?;
    self.charged = self.charged.saturating_add(need);
    if let Some(top) = self.waiting.last_mut() {
      top.1 = true;
    }
    self.open.insert(version);
    self.waiting.extend(missing);
    derivations.counters.walked = derivations.counters.walked.saturating_add(1);
    Ok(Progress::More)
  }

  /// Derives the top version from its inputs' values and keeps it; an input's value gone since the walk sends the
  /// version back to be walked again.
  fn derive_top<T>(
    &mut self,
    derivations: &mut Derivations,
    ty: &mut T,
    source: &T::Source,
    store: &mut ContentStore,
    (version, key): (V, &[u8]),
  ) -> Result<Progress, DerivedRefused<V, T::Refused>>
  where
    T: DerivedType,
    T::Source: Graph<Version = V>,
  {
    let Some(parents) = ty.inputs(source, version) else {
      return Err(DerivedRefused::UnknownVersion { version });
    };
    let mut values = Vec::with_capacity(parents.len());
    for parent in parents {
      let parent_key = Derivations::key(ty, source, parent)?;
      match derivations.kept::<V, T::Refused>(store, &parent_key, parent)? {
        Some(root) => values.push(root),
        None => return Ok(self.walk_again(derivations, version)),
      }
    }
    let derived = ty
      .derive(source, store, version, &values)
      .map_err(|refused| DerivedRefused::Type { version, refused })?;
    let root = derivations.keep_derived::<V, T::Refused>(store, key, version, derived)?;
    derivations.counters.derived = derivations.counters.derived.saturating_add(1);
    self.finished(store, version, root)
  }

  /// The top version back to be walked again.
  fn walk_again(&mut self, derivations: &mut Derivations, version: V) -> Progress {
    if let Some(top) = self.waiting.last_mut() {
      top.1 = false;
    }
    self.open.remove(&version);
    derivations.counters.rewalked = derivations.counters.rewalked.saturating_add(1);
    Progress::More
  }

  /// The top version's value is kept: it stops waiting, its charge given back; the target's ends the fill.
  fn finished<R>(
    &mut self,
    store: &mut ContentStore,
    version: V,
    root: ContentId,
  ) -> Result<Progress, DerivedRefused<V, R>> {
    let slot = slot_bytes::<V>();
    store.discharge(slot)?;
    self.charged = self.charged.saturating_sub(slot);
    self.waiting.pop();
    self.open.remove(&version);
    if self.waiting.is_empty() {
      self.done = Some(root);
      return Ok(Progress::Done(root));
    }
    Ok(Progress::More)
  }

  /// Gives back what the fill charges the store: what it still waits on, nothing once it is done.
  pub fn release(self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    store.discharge(self.charged)
  }
}
