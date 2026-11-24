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
use slates_merge::engine::{Green, Judged, Lent, Lineage, LineageGreen, LineageRefused, Release};

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
  /// Lends refused because the green owes a push (P2, `crate::green_ready`): the reach around the choke point, counted.
  owing_reached: Cell<u64>,
  /// Retired members' releases still owed, oldest first (module docs, "A retire is sliced").
  pending: VecDeque<(RepositoryId, Release)>,
  /// Whether this shard's release task is running.
  releasing: bool,
  /// The holds the engines let go gave back (A-760 (b) 4d), owed to the content store's next free decision.
  given_back: Vec<slates_merge::engine::NameHold>,
}

/// A held verdict previewed as a member of its green's lineage ([`Greens::preview`]).
#[must_use = "a preview holds its lineage's nodes until it is retired"]
pub struct Preview {
  /// The lineage.
  repository: RepositoryId,
  /// The preview's handle there.
  handle: LineageGreen,
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
  /// A member lent out of its lineage: the guard is the lineage and a handle, its engine in the lineage's slot.
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
  /// A member lent out of its lineage: the guard is the lineage and a handle, its engine in the lineage's slot.
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
  /// `id`'s engine, lent: `None` when there is none, when its lineage is lent already (counted), or when it owes a push
  /// and holds no content yet (P2; counted): every reader reaches a green's content through `crate::green_ready`, which
  /// materializes it first, so nothing reads around it. [`Greens::lend`] lends one that owes, for the materializer.
  pub fn get(&self, id: &VolumeId) -> Option<GreenRef<'_>> {
    let lent = self.lend(id)?;
    if lent.is_materialized() {
      return Some(lent);
    }
    self
      .owing_reached
      .set(self.owing_reached.get().saturating_add(1));
    None
  }

  /// `id`'s engine, lent for a write: as [`Greens::get`], `None` for a green that owes a push (counted).
  pub fn get_mut(&mut self, id: &VolumeId) -> Option<GreenMut<'_>> {
    let owing = self.lend(id).is_some_and(|lent| !lent.is_materialized());
    if owing {
      self
        .owing_reached
        .set(self.owing_reached.get().saturating_add(1));
      return None;
    }
    self.lend_mut(id)
  }

  /// Makes `id`'s green owe `push` (P2, `Green::defer`): the version it owes, or none (no such green, or one that cannot
  /// owe it).
  pub fn defer(&mut self, id: &VolumeId, push: slates_merge::push::DeferredPush) -> Option<u64> {
    self.lend_mut(id)?.defer(push)
  }

  /// The members `repository`'s lineage holds, its previews among them: a test's view.
  #[cfg(test)]
  pub fn lineage_len(&mut self, repository: RepositoryId) -> usize {
    self
      .lineages
      .get_mut(&repository)
      .map_or(0, |lineage| lineage.get_mut().len())
  }

  /// Whether `id`'s green owes a push (P2): held, and not materialized.
  pub fn is_owing(&self, id: &VolumeId) -> bool {
    self.lend(id).is_some_and(|lent| !lent.is_materialized())
  }

  /// Reaches refused because the green owed a push ([`Greens::get`]): zero in a build where every reader goes through
  /// `crate::green_ready`.
  pub fn owing_reached(&self) -> u64 {
    self.owing_reached.get()
  }

  /// `id`'s engine, lent whether or not it owes a push: the materializer's, a replay's and a create's lend alone (P2).
  pub fn lend(&self, id: &VolumeId) -> Option<GreenRef<'_>> {
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

  /// `id`'s engine, lent for a write whether or not it owes a push: the materializer's, a replay's and a create's (P2).
  /// `None` when there is none, or when a member of its lineage is taken out (counted).
  pub fn lend_mut(&mut self, id: &VolumeId) -> Option<GreenMut<'_>> {
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

  /// The holds every engine here took and gave back on its named objects since the last call (A-760 (b) 4d), each
  /// engine tracked from its first drain (`Green::track_name_holds`: a hold for each named block it holds then), and
  /// those given back by the engines let go since: what the shard applies at its next free decision
  /// (`vcs_store::name_holds::freeing`). A lineage with a member taken out keeps its own until it is put back.
  pub fn take_name_holds(&mut self) -> Vec<slates_merge::engine::NameHold> {
    let mut holds = std::mem::take(&mut self.given_back);
    for green in self.alone.values_mut() {
      holds.extend(green.track_name_holds());
    }
    for lineage in self.lineages.values_mut() {
      if let Ok(taken) = lineage.get_mut().take_name_holds() {
        holds.extend(taken);
      }
    }
    holds
  }

  /// Installs a lone engine for `id`; the engine it replaced, its holds given back (it is the caller's, untracked).
  pub fn insert(&mut self, id: VolumeId, green: Green) -> Option<Green> {
    let mut replaced = self.alone.insert(id, green)?;
    self.given_back.extend(replaced.give_back_name_holds());
    Some(replaced)
  }

  /// Takes `id`'s lone engine out, its holds given back (A-760 (b) 4d: the engine is the caller's, untracked, tracked
  /// again if it is installed again); a member is retired from its lineage instead ([`Greens::retire`]).
  pub fn remove(&mut self, id: &VolumeId) -> Option<Green> {
    let mut green = self.alone.remove(id)?;
    self.given_back.extend(green.give_back_name_holds());
    Some(green)
  }

  /// Keeps the greens `keep` holds to, the others dropped: a lone engine at once, a member retired and released whole.
  /// Only a rolled-back verb's greens leave this way (`verbs::reconcile_unpublished_effects`), each made by that one
  /// verb, so a member's release frees only the nodes its own writes copied.
  pub fn retain(&mut self, mut keep: impl FnMut(&VolumeId) -> bool) {
    let given_back = &mut self.given_back;
    self.alone.retain(|id, green| {
      let kept = keep(id);
      if !kept {
        given_back.extend(green.give_back_name_holds());
      }
      kept
    });
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

  /// Whether `repository` has a lineage here.
  pub fn has_lineage(&self, repository: RepositoryId) -> bool {
    self.lineages.contains_key(&repository)
  }

  /// Whether `id` is a member of a lineage, and whose.
  pub fn lineage_of(&self, id: &VolumeId) -> Option<RepositoryId> {
    self.members.get(id).map(|(repository, _)| *repository)
  }

  /// Starts `repository`'s lineage with `root` as its first member, `id`: `root`'s arenas become the lineage's, nothing
  /// copied. A lineage the repository already has is left as it is and the engine handed back.
  pub fn found(
    &mut self,
    repository: RepositoryId,
    id: VolumeId,
    root: Green,
  ) -> Result<(), Box<Green>> {
    if self.lineages.contains_key(&repository) || self.contains_key(&id) {
      return Err(Box::new(root));
    }
    let (lineage, handle) = Lineage::new(root);
    self.members.insert(id, (repository, handle));
    self.lineages.insert(repository, RefCell::new(lineage));
    Ok(())
  }

  /// Makes `id` a member of `from`'s lineage at `from`'s `version`, without a walk (`Lineage::branch`).
  pub fn branch(
    &mut self,
    from: &VolumeId,
    version: u64,
    id: VolumeId,
  ) -> Result<(), LineageRefused> {
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

  /// The held verdict `judged` of member `id`'s green previewed as a member of its lineage of its own (A-805,
  /// `Lineage::preview`): what a reader that must see the version before it commits reads ([`Greens::with_preview`]),
  /// while `id`'s engine shows nothing of it. Retire it with [`Greens::retire_preview`] in the same step. Refused as the
  /// lineage refuses it; `Stale` for a green standing alone.
  pub fn preview(&mut self, id: &VolumeId, judged: Judged) -> Result<Preview, LineageRefused> {
    let (repository, source) = *self.members.get(id).ok_or(LineageRefused::Stale)?;
    let lineage = self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?;
    let handle = lineage.get_mut().preview(source, judged)?;
    Ok(Preview { repository, handle })
  }

  /// Runs `call` on `preview`'s engine.
  pub fn with_preview<R>(
    &mut self,
    preview: &Preview,
    call: impl FnOnce(&Green) -> R,
  ) -> Result<R, LineageRefused> {
    let lineage = self
      .lineages
      .get_mut(&preview.repository)
      .ok_or(LineageRefused::Stale)?;
    lineage.get_mut().with(preview.handle, |green| call(green))
  }

  /// A scratch member of member `id`'s lineage at its head, O(1) (`Lineage::branch`): what an import's batch advances its
  /// revisions in (C2 stage C, A-814), the green showing nothing of them until the batch commits. Retire it with
  /// [`Greens::retire_preview`]. Refused as the lineage refuses a branch; `Stale` for a green standing alone.
  pub fn preview_at_head(&mut self, id: &VolumeId) -> Result<Preview, LineageRefused> {
    let (repository, source) = *self.members.get(id).ok_or(LineageRefused::Stale)?;
    let lineage = self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?;
    let head = lineage.get_mut().with(source, |green| green.head())?;
    let handle = lineage.get_mut().branch(source, head)?;
    Ok(Preview { repository, handle })
  }

  /// Runs `call` on `preview`'s engine, for a write.
  pub fn with_preview_mut<R>(
    &mut self,
    preview: &Preview,
    call: impl FnOnce(&mut Green) -> R,
  ) -> Result<R, LineageRefused> {
    let lineage = self
      .lineages
      .get_mut(&preview.repository)
      .ok_or(LineageRefused::Stale)?;
    lineage.get_mut().with(preview.handle, call)
  }

  /// Retires `preview`, its holds given back at once: a preview at its green's head shares every node but the ones its
  /// one version copied, so its release is that version's paths, never the tree.
  pub fn retire_preview(&mut self, preview: Preview) {
    let Some(lineage) = self.lineages.get_mut(&preview.repository) else {
      return;
    };
    if let Ok(mut release) = lineage.get_mut().retire(preview.handle) {
      while self
        .release_step(preview.repository, &mut release)
        .is_ok_and(|more| more)
      {}
    }
  }

  /// Retires member `id` from its lineage: its engine is gone at once, and what it held comes back as a [`Release`] to
  /// step under the shard's budget ([`Greens::release_step`]). `None` when `id` is no member.
  pub fn retire(
    &mut self,
    id: &VolumeId,
  ) -> Option<(RepositoryId, Result<Release, LineageRefused>)> {
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
  pub fn release_step(
    &mut self,
    repository: RepositoryId,
    release: &mut Release,
  ) -> Result<bool, LineageRefused> {
    let lineage = self
      .lineages
      .get_mut(&repository)
      .ok_or(LineageRefused::Stale)?;
    let more = lineage.get_mut().release_step(release)?;
    if !more && lineage.get_mut().is_empty() {
      // Its holds given back before its arenas go (A-760 (b) 4d).
      if let Ok(given) = lineage.get_mut().give_back_name_holds() {
        self.given_back.extend(given);
      }
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

#[cfg(test)]
mod preview_tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use super::*;
  use slates_merge::engine::{Increment, Judgment, Outcome};
  use slates_merge::increment::{VolumeOp, compose_volume_planned};

  /// The property list's attribute, as a Subversion repository's green keeps it.
  const PROPS: &str = "slates.svn.props";

  /// One path's reads as a Subversion revision is made of them (`vcs_store::svn_made`).
  type Reads = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    bool,
    Option<String>,
    bool,
    Option<String>,
    bool,
  );

  fn reads_at(green: &Green, path: &str, since: u64) -> Reads {
    (
      green.content_bytes(path).map(<[u8]>::to_vec),
      green.xattr(path, PROPS).map(<[u8]>::to_vec),
      green.is_dir(path),
      green.symlink(path).map(str::to_owned),
      green.special(path).is_some(),
      green.hardlink(path).map(str::to_owned),
      green.copied_after(path, since),
    )
  }

  /// `journal` composed against `green`'s head as increment `id`, `bytes` its post-state.
  fn increment(green: &Green, id: u8, journal: &[VolumeOp], bytes: &[u8]) -> Increment {
    let (doc, _) = compose_volume_planned(&green.current_base(), journal).unwrap();
    Increment {
      id: [id; 32],
      base: green.head(),
      doc,
      post_state: bytes.to_vec(),
      evidence: Vec::new(),
      named: Default::default(),
    }
  }

  /// A-805 through the server's `Greens::preview` (the wrapper svn's landing previews a held verdict with). Do: found a
  /// repository's lineage with an empty green; for each of six versions (a tree made with a file, its attribute and mode
  /// set, a symbolic link and a hard link to the file, the file renamed, the renamed file's mode set, the tree copied),
  /// judge it through the member's engine, preview the held verdict, read every path through `with_preview`, retire the
  /// preview, then commit the version for real. Expect: the green unchanged by the preview (its head, and the lineage
  /// back to its one member once retired); the preview's changed paths and every path's reads (bytes, property list,
  /// kind, link target, IPC node, hard link's primary, copied after) the committed green's at the new version.
  #[test]
  fn a_preview_through_greens_reads_as_the_commit_does() {
    let repository = RepositoryId { bytes: [1; 16] };
    let id = VolumeId { bytes: [2; 16] };
    let mut greens = Greens::default();
    assert!(greens.found(repository, id, Green::new()).is_ok());
    let paths = [
      "trunk", "trunk/a", "trunk/b", "trunk/l", "trunk/h", "branch", "branch/b", "branch/l",
      "branch/h",
    ];
    let steps: Vec<(Vec<VolumeOp>, &[u8])> = vec![
      (
        vec![
          VolumeOp::Mkdir {
            path: "trunk".to_owned(),
          },
          VolumeOp::Create {
            path: "trunk/a".to_owned(),
          },
          VolumeOp::Extend {
            path: "trunk/a".to_owned(),
            at: 0,
            len: 5,
          },
        ],
        b"alpha",
      ),
      (
        vec![
          VolumeOp::SetXattr {
            path: "trunk/a".to_owned(),
            name: PROPS.to_owned(),
            value: b"p".to_vec(),
          },
          VolumeOp::SetMode {
            path: "trunk/a".to_owned(),
            mode: 0o640,
          },
        ],
        b"",
      ),
      (
        vec![
          VolumeOp::Symlink {
            path: "trunk/l".to_owned(),
            target: "a".to_owned(),
          },
          VolumeOp::Link {
            path: "trunk/h".to_owned(),
            target: "trunk/a".to_owned(),
          },
        ],
        b"",
      ),
      (
        vec![VolumeOp::Rename {
          from: "trunk/a".to_owned(),
          to: "trunk/b".to_owned(),
        }],
        b"",
      ),
      (
        vec![VolumeOp::SetMode {
          path: "trunk/b".to_owned(),
          mode: 0o600,
        }],
        b"",
      ),
      (vec![], b""),
    ];
    for (index, (journal, bytes)) in steps.into_iter().enumerate() {
      let step = u8::try_from(index).unwrap();
      let head = greens.get(&id).unwrap().head();
      let journal = if journal.is_empty() {
        vec![VolumeOp::Copy {
          from: "trunk".to_owned(),
          to: "branch".to_owned(),
          version: head,
          replace: false,
        }]
      } else {
        journal
      };
      let inc = increment(&greens.get(&id).unwrap(), step, &journal, bytes);
      let Judgment::Accepted(judged) = greens.get_mut(&id).unwrap().judge(&inc) else {
        panic!("step {index} not accepted");
      };
      let preview = greens.preview(&id, judged).unwrap();
      let (changed, viewed) = greens
        .with_preview(&preview, |view| {
          let reads: Vec<Reads> = paths.iter().map(|path| reads_at(view, path, 0)).collect();
          (view.changed_between(0, 1), reads)
        })
        .unwrap();
      greens.retire_preview(preview);
      assert_eq!(
        greens.get(&id).unwrap().head(),
        head,
        "step {index}: the preview moved the green"
      );
      assert_eq!(
        greens.lineage_len(repository),
        1,
        "step {index}: the preview stayed a member"
      );
      let outcome = greens.get_mut(&id).unwrap().submit(&inc);
      assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "step {index}: {outcome:?}"
      );
      let green = greens.get(&id).unwrap();
      assert_eq!(
        changed,
        green.changed_between(head, head + 1),
        "step {index}"
      );
      let committed: Vec<Reads> = paths
        .iter()
        .map(|path| reads_at(&green, path, head))
        .collect();
      assert_eq!(viewed, committed, "step {index}");
    }
  }
}
