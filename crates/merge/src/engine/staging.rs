//! A held verdict staged a unit a step and published whole (A-752 part 3c; main 01a20182's rulings of 2026-11-28 00:42
//! to 01:42): its commit's changes made where no reader at the head sees them, and published in one step at its end,
//! so a reader never waits on a commit's turns and a commit refused part-way publishes nothing.
//!
//! **Built so far (U1, U2).** Every read names its version (U0), so what the commit appends at its own version is above
//! the head until the publish moves the head to it, last (`Green::publish_run`: its delta pushed and its increment
//! accepted). The value histories (modes, IPC nodes, directories, symbolic and hard links, attributes) are appended in
//! place and taken back at a refusal: where each key's history stood before the stage first touched it is marked
//! ([`Marks`]), and a refusal truncates it there. The change index's version list is recorded above the head and taken
//! out at a refusal, and the earlier entries it makes no longer last are marked at the publish (U3). **Owed (later
//! units):** the identity's changes held to the publish, the current maps (`latest_change` among them) forked into
//! the stage, and the content history's values copied with their chunks
//! held; until then a refusal leaves those as the steps made them, and the stage holds no bytes of its own.

use std::collections::{BTreeMap, BTreeSet};

use super::{ApplyStep, Committing, Green, Judged};

/// Where each history a stage appends in place stood before its first touch (A-752 part 3c U1 and U2): a key's
/// entries then, or none for a key with no history. Bounded by the keys the verdict's commit touches.
#[derive(Debug, Default)]
pub(super) struct Marks {
  /// The mode history's.
  pub(super) modes: BTreeMap<String, Option<usize>>,
  /// The IPC nodes' history's.
  pub(super) specials: BTreeMap<String, Option<usize>>,
  /// The directories' history's.
  pub(super) dirs: BTreeMap<String, Option<usize>>,
  /// The symbolic links' history's.
  pub(super) symlinks: BTreeMap<String, Option<usize>>,
  /// The hard links' history's.
  pub(super) hardlinks: BTreeMap<String, Option<usize>>,
  /// The extended attributes' history's, by path and name.
  pub(super) xattrs: BTreeMap<(String, String), Option<usize>>,
}

/// Marks where `key`'s history in `histories` stands, unless the stage marked it already: its entries, or none.
pub(super) fn note<K: Ord, E>(
  marks: &mut BTreeMap<K, Option<usize>>,
  histories: &BTreeMap<K, Vec<E>>,
  key: K,
) {
  if marks.contains_key(&key) {
    return;
  }
  let held = histories.get(&key).map(Vec::len);
  marks.insert(key, held);
}

/// Takes `histories` back to where `marks` say each marked key's stood: truncated to its entries then, or removed.
fn take_back<K: Ord, E>(marks: BTreeMap<K, Option<usize>>, histories: &mut BTreeMap<K, Vec<E>>) {
  for (key, held) in marks {
    match held {
      Some(len) => {
        if let Some(history) = histories.get_mut(&key) {
          history.truncate(len);
        }
      }
      None => {
        histories.remove(&key);
      }
    }
  }
}

/// A held verdict being staged ([`Green::begin_staging`]): its commit under way, and whether it is staged whole.
pub struct Staging {
  committing: Committing,
  staged: bool,
  /// Whether the identity's published backlog is folded ([`Green::staging_step`]'s first steps).
  drained: bool,
  units: u64,
}

impl core::fmt::Debug for Staging {
  fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    formatter
      .debug_struct("Staging")
      .field("units", &self.units)
      .field("staged", &self.staged)
      .finish_non_exhaustive()
  }
}

/// What a staging step did ([`Green::staging_step`]).
#[derive(Debug)]
pub enum StagingStep {
  /// More of the stage is left.
  More,
  /// The stage waits for these texts, held by name alone with none lent (as [`super::CommitStep::Lend`]).
  Lend(BTreeSet<crate::named::Identity>),
  /// Staged whole: [`Green::publish`] makes it the green's.
  Staged,
}

impl Staging {
  /// The bytes its stage holds now, charged to its owner: at most [`Green::staging_bound`] of its verdict. None yet:
  /// what it appends in place is charged as the commit's own retention (owed: the forked maps' and the held copies').
  pub fn bytes(&self) -> u64 {
    0
  }

  /// The steps it has taken.
  pub fn units(&self) -> u64 {
    self.units
  }
}

impl Green {
  /// The most bytes a stage of `judged` holds ([`Staging::bytes`]), stated before it begins so its owner charges it.
  /// None yet (module docs).
  pub fn staging_bound(&self, _judged: &Judged) -> u64 {
    0
  }

  /// Begins staging `judged`, a verdict held at this green's head ([`Green::staging_step`]): one stage at a time.
  pub fn begin_staging(&mut self, judged: Judged) -> Staging {
    self.stage_marks = Some(Box::default());
    Staging {
      committing: self.begin_committing(judged),
      staged: false,
      drained: false,
      units: 0,
    }
  }

  /// Takes `staging` on a unit at a time, at least one, until `spent`: the head unmoved until [`Green::publish`].
  pub fn staging_step(
    &mut self,
    staging: &mut Staging,
    spent: &mut dyn FnMut() -> bool,
  ) -> StagingStep {
    staging.units = staging.units.saturating_add(1);
    // The identity's published backlog first, a key a step, before any of the commit's in-place writes (A-752 part 3c
    // U5): a reader's fold between the stage's turns then finds nothing but what is published, and no key it would
    // recompute from the current maps is one the commit wrote. Ends with the current maps forked into the stage.
    if !staging.drained {
      staging.drained = self.fold_identity_step(1);
      if !staging.drained {
        return StagingStep::More;
      }
    }
    let Committing {
      run,
      post_state,
      runs,
      ..
    } = &mut staging.committing;
    let Some(commit) = run.commit.as_mut() else {
      return match self.prepare_step(run, (post_state, runs), spent) {
        ApplyStep::Lend(unlent) => StagingStep::Lend(unlent),
        _ => StagingStep::More,
      };
    };
    if !self.commit_step(commit, spent) {
      return StagingStep::More;
    }
    staging.staged = true;
    StagingStep::Staged
  }

  /// Publishes a staged `staging`, moving the head to its version last: its version and what the chain records of it;
  /// `None` for one not staged whole, which is given up.
  pub fn publish(&mut self, staging: Staging) -> Option<(u64, Vec<crate::chain_entry::Applied>)> {
    if !staging.staged {
      self.abandon_staging(staging);
      return None;
    }
    let mut staging = staging;
    let Committing { run, applied, .. } = &mut staging.committing;
    let commit = run.commit.as_mut()?;
    self.stage_marks = None;
    let version = self.publish_run(commit, run.id);
    Some((version, core::mem::take(applied)))
  }

  /// Gives `staging` up before it is published: the head and the accepted increments unmoved, and each history it
  /// appended in place taken back to where it stood (module docs: owed for the maps later units stage).
  pub fn abandon_staging(&mut self, staging: Staging) {
    // A run that ended recorded its version's change entries, above the head (A-752 part 3c U3): taken out. The earlier
    // entries it would have marked no longer last are marked only at a publish.
    let ended = staging
      .committing
      .run
      .commit
      .as_ref()
      .map(|commit| commit.version);
    if let Some(version) = ended.filter(|version| *version > self.head()) {
      self.changes.remove(&version);
    }
    drop(staging);
    // The last changes and identity marks it wrote aside are dropped (A-752 part 3c U4, U5).
    self.aside = super::last_changes::Aside::default();
    self.identity_aside = super::last_changes::IdentityAside::default();
    let Some(marks) = self.stage_marks.take() else {
      return;
    };
    let Marks {
      modes,
      specials,
      dirs,
      symlinks,
      hardlinks,
      xattrs,
    } = *marks;
    take_back(modes, &mut self.mode_history);
    take_back(specials, &mut self.special_history);
    take_back(dirs, &mut self.dir_history);
    take_back(symlinks, &mut self.symlink_history);
    take_back(hardlinks, &mut self.hardlink_history);
    take_back(xattrs, &mut self.xattr_history);
  }

  /// Marks, while a verdict is staged, where `path`'s IPC node history stood before the stage first touched it.
  pub(super) fn mark_special(&mut self, path: &str) {
    if let Some(marks) = self.stage_marks.as_mut() {
      note(&mut marks.specials, &self.special_history, path.to_owned());
    }
  }

  /// Marks, while a verdict is staged, where `path`'s directory history stood before the stage first touched it.
  pub(super) fn mark_dir(&mut self, path: &str) {
    if let Some(marks) = self.stage_marks.as_mut() {
      note(&mut marks.dirs, &self.dir_history, path.to_owned());
    }
  }

  /// Marks, while a verdict is staged, where `path`'s symbolic link history stood before the stage first touched it.
  pub(super) fn mark_symlink(&mut self, path: &str) {
    if let Some(marks) = self.stage_marks.as_mut() {
      note(&mut marks.symlinks, &self.symlink_history, path.to_owned());
    }
  }

  /// Marks, while a verdict is staged, where `path`'s hard link history stood before the stage first touched it.
  pub(super) fn mark_hardlink(&mut self, path: &str) {
    if let Some(marks) = self.stage_marks.as_mut() {
      note(
        &mut marks.hardlinks,
        &self.hardlink_history,
        path.to_owned(),
      );
    }
  }

  /// Marks, while a verdict is staged, where `key`'s attribute history stood before the stage first touched it.
  pub(super) fn mark_xattr(&mut self, key: &(String, String)) {
    if let Some(marks) = self.stage_marks.as_mut() {
      note(&mut marks.xattrs, &self.xattr_history, key.clone());
    }
  }
}
