//! The shard's green engines (§4.16; §4.17.1 item 4, A-575): each green's engine stands alone, or is a member of its
//! repository's lineage, whose engines share the nodes of their maps (`slates_merge::engine::Lineage`), so a branch made
//! at another branch's version costs what changed after that version, never the tree.
//!
//! **One way to reach an engine.** Every verb reads and writes a green through [`Greens::get`] and [`Greens::get_mut`],
//! whatever its kind: a lone engine is lent as it stands; a member is lent out of its lineage with the lineage's arenas
//! in it ([`slates_merge::engine::Lent`]), and goes back when the guard is dropped. The guards dereference to the
//! engine, so a verb's code is the same for both.
//!
//! **One lend a lineage at a time.** A lineage's arenas are in one engine at a time, so a member is lent only while no
//! other member of its lineage is: the lineage sits in a cell, and a lend that finds it lent is refused and counted
//! ([`Greens::busy_lends`]), never undefined, as the runtime's queues refuse a re-entrant access. No verb holds two
//! members of one lineage at once; the count is the evidence.
//!
//! **A retire is sliced.** A member's destroy retires it from its lineage at once, and what only it held is given back
//! a node a unit afterwards, under the shard's slice, by one task the first queued release starts
//! (`crate::repository::run_releases`): a branch that copied much is freed without one long step.
//!
//! **Bounds.** The engines are the partition's greens, bounded by its volume capacity; a lineage holds a repository's
//! members, and goes when its last member's release is done; the queued releases are the members destroyed and not yet
//! released, each bounded by the nodes it alone held.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::{BTreeMap, VecDeque};
use std::ops::{Deref, DerefMut};

use slates_db::catalog::{RepositoryId, VolumeId};
use slates_merge::engine::{Green, Lent, Lineage, LineageGreen, LineageRefused, Release};

/// The shard's green engines (module docs).
#[derive(Default)]
pub struct Greens {
  /// The greens standing alone.
  alone: BTreeMap<VolumeId, Green>,
  /// Each member's lineage and its handle there.
  members: BTreeMap<VolumeId, (RepositoryId, LineageGreen)>,
  /// Each repository's lineage, in a cell so a shared borrow of the engines can lend one member.
  lineages: BTreeMap<RepositoryId, RefCell<Lineage>>,
  /// Lends refused because the member's lineage was lent already, or had a member taken out (module docs).
  busy: Cell<u64>,
  /// Retired members' releases still owed, oldest first (module docs, "A retire is sliced").
  pending: VecDeque<(RepositoryId, Release)>,
  /// Whether this shard's release task is running.
  releasing: bool,
}

/// What one unit of the queued releases did ([`Greens::release_unit`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseProgress {
  /// One node's hold given back; more are owed.
  More,
  /// Nothing more is owed: the release task ends.
  Done,
  /// The lineage of the oldest owed release has a member taken out (a recovery's replay), so nothing can move until it
  /// is put back.
  Blocked,
}

/// A green's engine lent for a shared borrow ([`Greens::get`]).
pub enum GreenRef<'a> {
  /// A lone engine.
  Alone(&'a Green),
  /// A member lent out of its lineage.
  Member(Lent<RefMut<'a, Lineage>>),
}

impl Deref for GreenRef<'_> {
  type Target = Green;

  fn deref(&self) -> &Green {
    match self {
      GreenRef::Alone(green) => green,
      GreenRef::Member(lent) => lent,
    }
  }
}

/// A green's engine lent for a write ([`Greens::get_mut`]).
pub enum GreenMut<'a> {
  /// A lone engine.
  Alone(&'a mut Green),
  /// A member lent out of its lineage.
  Member(Lent<&'a mut Lineage>),
}

impl Deref for GreenMut<'_> {
  type Target = Green;

  fn deref(&self) -> &Green {
    match self {
      GreenMut::Alone(green) => green,
      GreenMut::Member(lent) => lent,
    }
  }
}

impl DerefMut for GreenMut<'_> {
  fn deref_mut(&mut self) -> &mut Green {
    match self {
      GreenMut::Alone(green) => green,
      GreenMut::Member(lent) => lent,
    }
  }
}

impl Greens {
  /// `id`'s engine, lent: `None` when there is none, or when its lineage is lent already (counted).
  pub fn get(&self, id: &VolumeId) -> Option<GreenRef<'_>> {
    if let Some(green) = self.alone.get(id) {
      return Some(GreenRef::Alone(green));
    }
    let (repository, handle) = self.members.get(id)?;
    let lent = self
      .lineages
      .get(repository)
      .and_then(|lineage| lineage.try_borrow_mut().ok())
      .and_then(|lineage| Lent::new(lineage, *handle).ok());
    if lent.is_none() {
      self.busy.set(self.busy.get().saturating_add(1));
    }
    lent.map(GreenRef::Member)
  }

  /// `id`'s engine, lent for a write: `None` when there is none, or when a member of its lineage is taken out
  /// (counted).
  pub fn get_mut(&mut self, id: &VolumeId) -> Option<GreenMut<'_>> {
    if let Some(green) = self.alone.get_mut(id) {
      return Some(GreenMut::Alone(green));
    }
    let (repository, handle) = *self.members.get(id)?;
    let lent = self
      .lineages
      .get_mut(&repository)
      .and_then(|lineage| Lent::new(lineage.get_mut(), handle).ok());
    if lent.is_none() {
      self.busy.set(self.busy.get().saturating_add(1));
    }
    lent.map(GreenMut::Member)
  }

  /// Whether `id` has an engine here.
  pub fn contains_key(&self, id: &VolumeId) -> bool {
    self.alone.contains_key(id) || self.members.contains_key(id)
  }

  /// Every green's id, the lone ones first.
  pub fn keys(&self) -> impl Iterator<Item = &VolumeId> + '_ {
    self.alone.keys().chain(self.members.keys())
  }

  /// Every green with its engine, each lent in turn (module docs): a member whose lineage is lent already is left out
  /// and counted, so a consumer takes one at a time.
  pub fn iter(&self) -> impl Iterator<Item = (&VolumeId, GreenRef<'_>)> + '_ {
    self
      .alone
      .iter()
      .map(|(id, green)| (id, GreenRef::Alone(green)))
      .chain(
        self
          .members
          .keys()
          .filter_map(|id| self.get(id).map(|green| (id, green))),
      )
  }

  /// Installs a lone engine for `id`; the engine it replaced.
  pub fn insert(&mut self, id: VolumeId, green: Green) -> Option<Green> {
    self.alone.insert(id, green)
  }

  /// Takes `id`'s lone engine out; a member is retired from its lineage instead ([`Greens::retire`]).
  pub fn remove(&mut self, id: &VolumeId) -> Option<Green> {
    self.alone.remove(id)
  }

  /// Keeps the greens `keep` holds to, the others dropped: a lone engine at once, a member retired and released whole.
  /// Only a rolled-back verb's greens leave this way (`verbs::reconcile_unpublished_effects`), each made by that one
  /// verb, so a member's release frees only the nodes its own writes copied.
  pub fn retain(&mut self, mut keep: impl FnMut(&VolumeId) -> bool) {
    self.alone.retain(|id, _| keep(id));
    let gone: Vec<VolumeId> = self
      .members
      .keys()
      .filter(|id| !keep(id))
      .copied()
      .collect();
    for id in gone {
      if let Some((repository, Ok(mut release))) = self.retire(&id) {
        while self
          .release_step(repository, &mut release)
          .is_ok_and(|more| more)
        {}
      }
    }
  }

  /// Whether `id` is a member of a lineage, and whose.
  pub fn lineage_of(&self, id: &VolumeId) -> Option<RepositoryId> {
    self.members.get(id).map(|(repository, _)| *repository)
  }

  /// Starts `repository`'s lineage with `root` as its first member, `id`: `root`'s arenas become the lineage's, nothing
  /// copied. A lineage the repository already has is left as it is and the engine handed back.
  pub fn found(&mut self, repository: RepositoryId, id: VolumeId, root: Green) -> Result<(), Green> {
    if self.lineages.contains_key(&repository) || self.contains_key(&id) {
      return Err(root);
    }
    let (lineage, handle) = Lineage::new(root);
    self.members.insert(id, (repository, handle));
    self.lineages.insert(repository, RefCell::new(lineage));
    Ok(())
  }

  /// Makes `id` a member of `from`'s lineage at `from`'s `version`, without a walk (`Lineage::branch`).
  pub fn branch(&mut self, from: &VolumeId, version: u64, id: VolumeId) -> Result<(), LineageRefused> {
    let (repository, source) = *self.members.get(from).ok_or(LineageRefused::Stale)?;
    if self.contains_key(&id) {
      return Err(LineageRefused::Stale);
    }
    let lineage = self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?;
    let handle = lineage.get_mut().branch(source, version)?;
    self.members.insert(id, (repository, handle));
    Ok(())
  }

  /// Retires member `id` from its lineage: its engine is gone at once, and what it held comes back as a [`Release`] to
  /// step under the shard's budget ([`Greens::release_step`]). `None` when `id` is no member.
  pub fn retire(&mut self, id: &VolumeId) -> Option<(RepositoryId, Result<Release, LineageRefused>)> {
    let (repository, handle) = *self.members.get(id)?;
    let lineage = self.lineages.get_mut(&repository)?;
    let retired = lineage.get_mut().retire(handle);
    if retired.is_ok() {
      self.members.remove(id);
    }
    Some((repository, retired))
  }

  /// Retires member `id` and queues its release (module docs, "A retire is sliced"): `None` when `id` is no member;
  /// otherwise whether the shard's release task must be started, since none runs.
  pub fn retire_queued(&mut self, id: &VolumeId) -> Option<Result<bool, LineageRefused>> {
    let (repository, retired) = self.retire(id)?;
    Some(retired.map(|release| {
      self.pending.push_back((repository, release));
      let start = !self.releasing;
      self.releasing = true;
      start
    }))
  }

  /// One unit of the queued releases: one node's hold of the oldest owed release given back, the release dropped once
  /// it is done. [`ReleaseProgress::Done`] once nothing is owed, when the release task is marked ended.
  pub fn release_unit(&mut self) -> ReleaseProgress {
    let Some((repository, mut release)) = self.pending.pop_front() else {
      self.releasing = false;
      return ReleaseProgress::Done;
    };
    match self.release_step(repository, &mut release) {
      Ok(true) => {
        self.pending.push_front((repository, release));
        ReleaseProgress::More
      }
      // Done, or its lineage gone with every node (nothing left to give back): the next release is owed.
      Ok(false) | Err(LineageRefused::Stale) => ReleaseProgress::More,
      Err(_) => {
        self.pending.push_front((repository, release));
        ReleaseProgress::Blocked
      }
    }
  }

  /// The release task could not start: the releases stay queued for the next retire to start it again.
  pub fn release_task_refused(&mut self) {
    self.releasing = false;
  }

  /// The releases still owed.
  pub fn releases_owed(&self) -> usize {
    self.pending.len()
  }

  /// One step of a retired member's release in `repository`'s lineage: false once it is done, when an emptied lineage
  /// goes too.
  pub fn release_step(&mut self, repository: RepositoryId, release: &mut Release) -> Result<bool, LineageRefused> {
    let lineage = self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?;
    let more = lineage.get_mut().release_step(release)?;
    if !more && lineage.get_mut().is_empty() {
      self.lineages.remove(&repository);
    }
    Ok(more)
  }

  /// Member `id`'s engine taken out of its lineage with the arenas in it, for a replay that runs over many slices (a
  /// recovery's): every other lend of the lineage is refused until [`Greens::put_back`].
  pub fn take(&mut self, id: &VolumeId) -> Result<Green, LineageRefused> {
    let (repository, handle) = *self.members.get(id).ok_or(LineageRefused::Stale)?;
    self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?
      .get_mut()
      .take(handle)
  }

  /// Puts back the engine [`Greens::take`] took out for `id`.
  pub fn put_back(&mut self, id: &VolumeId, green: Green) -> Result<(), LineageRefused> {
    let (repository, handle) = *self.members.get(id).ok_or(LineageRefused::Stale)?;
    self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?
      .get_mut()
      .put_back(handle, green)
  }

  /// Lends refused because a lineage was lent already: zero in every correct use (module docs).
  pub fn busy_lends(&self) -> u64 {
    self.busy.get()
  }

  /// Nodes alive in `repository`'s lineage, when it has one.
  pub fn lineage_nodes(&self, repository: RepositoryId) -> Option<usize> {
    self
      .lineages
      .get(&repository)
      .and_then(|lineage| lineage.try_borrow().ok())
      .map(|lineage| lineage.live_nodes())
  }

  /// Nodes any walk of `repository`'s lineage visited, when it has one: a branch moves it by nothing.
  pub fn lineage_walked(&self, repository: RepositoryId) -> Option<u64> {
    self
      .lineages
      .get(&repository)
      .and_then(|lineage| lineage.try_borrow().ok())
      .map(|lineage| lineage.walked())
  }
}
