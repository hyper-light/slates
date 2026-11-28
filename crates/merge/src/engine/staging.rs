//! A held verdict staged a unit a step and published whole (A-752 part 3c; main 01a20182's rulings of 2026-11-28 00:42
//! to 01:42): its commit's changes made where no reader at the head sees them, and published in one step at its end,
//! so a reader never waits on a commit's turns and a commit refused part-way publishes nothing.
//!
//! **What it holds.** Every read names its version (U0), so what the commit appends at its own version is above the
//! head until the publish moves the head to it, last (`Green::publish_run`: its delta pushed and its increment
//! accepted, U1). The value histories are appended in place and taken back at a refusal, each key marked where its
//! history stood at the stage's first touch ([`Marks`], U1 and U2). The change index's version list is recorded above
//! the head and taken out at a refusal, and the earlier entries it makes no longer last are marked at the publish
//! (U3). Each key's last change and the identity's marks are written aside and moved in at the publish
//! (`last_changes`, U4 and U5). The eight current maps and the content history are forked at the stage's begin, and
//! the commit's steps read and write the forks, every reader between steps the published ones ([`Forks`], U6 to U8):
//! a reader's fold of the identity reads only what is published, and a node of the content history the stage copies
//! holds its values' chunks (`history::HistoryHolders`), so the stage's copies and the published values release each
//! chunk once. A staged commit re-anchors a changed file's history in full, never by the layered route, which repoints
//! the path's shared anchor chunk (U8). The commit's scalars (the history's and the change index's running totals,
//! the chunk table's byte counters, the commit's counters) are the stage's own while it runs ([`Scalars`], U9): a
//! reader's retention charge between steps is the published one, the stage's bytes are what its scalars charge past
//! it ([`Green::staging_bytes`]), at most the increment's reservation ([`Green::staging_bound`]). **Owed:** the
//! staged commit wired into a push's landing (main 01a20182's order of 2026-11-28 03:10), with its own integration
//! red on that path.

use std::collections::{BTreeMap, BTreeSet};

use super::{ApplyStep, Committing, Green, Judged};
use crate::dims::{Dim, Files, Xattrs};
use crate::special::SpecialNode;

/// The stage's forks of the green's eight current maps (A-752 part 3c U6 and U7; main 01a20182's ruling of 2026-11-28
/// 03:10): made at the stage's begin, one count each, holding no arena of their own; the commit's steps
/// read and write them ([`Green::swap_forks`] puts them in place for a step and back), every reader between steps
/// the published maps. A publish swaps them in for good and releases the published roots; a refusal releases them.
/// What a release frees is the nodes the stage copied: O(the paths it changed × their depth).
struct Forks {
  /// The content history (A-752 part 3c U8): a node the stage copies holds its entries' chunks
  /// ([`crate::history::HistoryHolders`]), and a freed node gives them back, so each chunk is released once.
  content_history: Dim<crate::history::Entries>,
  /// The content map (A-752 part 3c U7): a node the stage copies holds the blocks it names, and a freed node lets them
  /// go, so the stage's copies and the published ones release each block once.
  content: Files,
  specials: Dim<SpecialNode>,
  dirs: Dim<()>,
  modes: Dim<u32>,
  symlinks: Dim<String>,
  hardlinks: Dim<String>,
  xattrs: Xattrs,
  links_to: Dim<BTreeSet<String>>,
}

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

/// The commit's scalars, a stage's own while it runs (A-752 part 3c U9; main 01a20182's rulings of 2026-11-28 03:10 and
/// 04:44): the history's and the change index's running totals, the chunk table's byte counters and the commit's
/// counters. Swapped in for each of the stage's steps with its forks and back after it, so a reader between steps
/// reads the published ones (the retention charge among them); written in at the publish, and dropped at a refusal.
#[derive(Debug)]
struct Scalars {
  history_current: usize,
  history_lists: usize,
  history_named: (u64, u64),
  history_reanchored: (u64, u64, u64),
  history_reanchored_values: u64,
  special_history_bytes: usize,
  changes_bytes: usize,
  chunks: crate::history::ChunkCounters,
  commit_paths_made: u64,
  named_buffers_freed: u64,
  commit_tree_paths: u64,
  node_moves_walked: u64,
  node_moves_units: u64,
  copies_grafted: u64,
  copies_refused: u64,
  mapped_files_made: u64,
  identities_taken_at_change: u64,
}

impl Scalars {
  /// The retention these scalars charge ([`Green::retention_charge`]'s rule, over them).
  fn retention(&self) -> usize {
    self
      .chunks
      .bytes()
      .saturating_add(self.history_lists)
      .saturating_sub(self.history_current)
      .saturating_add(self.special_history_bytes)
      .saturating_add(self.changes_bytes)
  }
}

/// A held verdict being staged ([`Green::begin_staging`]): its commit under way, and whether it is staged whole.
pub struct Staging {
  committing: Committing,
  /// The stage's scalars ([`Scalars`]), out of place between its steps.
  scalars: Scalars,
  /// The stage's forks of the current maps; `None` once given back.
  forks: Option<Forks>,
  staged: bool,
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
  /// The steps it has taken.
  pub fn units(&self) -> u64 {
    self.units
  }
}

impl Green {
  /// The most bytes a stage of increment `inc` holds ([`Green::staging_bytes`]), stated before it begins so its owner
  /// charges it: the retention its commit may add, as the owner secures it before a verdict (`history_reservation` and
  /// `changes_reservation`, A-16). The nodes the stage's forks copy are the arenas' own, counted there.
  pub fn staging_bound(&self, inc: &crate::engine::Increment) -> u64 {
    let reserved = self
      .history_reservation(inc)
      .saturating_add(self.changes_reservation(inc));
    u64::try_from(reserved).unwrap_or(u64::MAX)
  }

  /// The bytes `staging`'s stage holds now, charged to its owner: the retention its scalars charge past the published
  /// ones (A-752 part 3c U9), at most [`Green::staging_bound`] of its increment.
  pub fn staging_bytes(&self, staging: &Staging) -> u64 {
    let published = self.scalars_now().retention();
    u64::try_from(staging.scalars.retention().saturating_sub(published)).unwrap_or(u64::MAX)
  }

  /// The green's scalars now ([`Scalars`]).
  fn scalars_now(&self) -> Scalars {
    Scalars {
      history_current: self.history_current,
      history_lists: self.history_lists,
      history_named: self.history_named,
      history_reanchored: self.history_reanchored,
      history_reanchored_values: self.history_reanchored_values,
      special_history_bytes: self.special_history_bytes,
      changes_bytes: self.changes_bytes,
      chunks: self.chunks.counters(),
      commit_paths_made: self.commit_paths_made,
      named_buffers_freed: self.named_buffers_freed,
      commit_tree_paths: self.commit_tree_paths,
      node_moves_walked: self.node_moves_walked,
      node_moves_units: self.node_moves_units,
      copies_grafted: self.copies_grafted,
      copies_refused: self.copies_refused,
      mapped_files_made: self.mapped_files_made.get(),
      identities_taken_at_change: self.identities_taken_at_change.get(),
    }
  }

  /// Exchanges the green's scalars with `scalars`: a stage's in place for its step, the published back after it. Twice
  /// is none.
  fn swap_scalars(&mut self, scalars: &mut Scalars) {
    std::mem::swap(&mut self.history_current, &mut scalars.history_current);
    std::mem::swap(&mut self.history_lists, &mut scalars.history_lists);
    std::mem::swap(&mut self.history_named, &mut scalars.history_named);
    std::mem::swap(
      &mut self.history_reanchored,
      &mut scalars.history_reanchored,
    );
    std::mem::swap(
      &mut self.history_reanchored_values,
      &mut scalars.history_reanchored_values,
    );
    std::mem::swap(
      &mut self.special_history_bytes,
      &mut scalars.special_history_bytes,
    );
    std::mem::swap(&mut self.changes_bytes, &mut scalars.changes_bytes);
    self.chunks.swap_counters(&mut scalars.chunks);
    std::mem::swap(&mut self.commit_paths_made, &mut scalars.commit_paths_made);
    std::mem::swap(
      &mut self.named_buffers_freed,
      &mut scalars.named_buffers_freed,
    );
    std::mem::swap(&mut self.commit_tree_paths, &mut scalars.commit_tree_paths);
    std::mem::swap(&mut self.node_moves_walked, &mut scalars.node_moves_walked);
    std::mem::swap(&mut self.node_moves_units, &mut scalars.node_moves_units);
    std::mem::swap(&mut self.copies_grafted, &mut scalars.copies_grafted);
    std::mem::swap(&mut self.copies_refused, &mut scalars.copies_refused);
    scalars.mapped_files_made = self.mapped_files_made.replace(scalars.mapped_files_made);
    scalars.identities_taken_at_change = self
      .identities_taken_at_change
      .replace(scalars.identities_taken_at_change);
  }

  /// Begins staging `judged`, a verdict held at this green's head ([`Green::staging_step`]): one stage at a time.
  pub fn begin_staging(&mut self, judged: Judged) -> Staging {
    self.stage_marks = Some(Box::default());
    let forks = self.fork_maps();
    let scalars = self.scalars_now();
    Staging {
      committing: self.begin_committing(judged),
      scalars,
      forks: Some(forks),
      staged: false,
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
    let Some(forks) = staging.forks.as_mut() else {
      return StagingStep::Staged;
    };
    // The commit reads and writes the stage's forks and scalars, never the published ones (A-752 part 3c U6, U9).
    self.swap_forks(forks);
    self.swap_scalars(&mut staging.scalars);
    let stepped = self.commit_unit(&mut staging.committing, spent);
    self.swap_scalars(&mut staging.scalars);
    self.swap_forks(forks);
    if let StagingStep::Staged = stepped {
      staging.staged = true;
    }
    stepped
  }

  /// One unit of `committing`'s prepare or commit steps, the publish left to [`Green::publish`].
  fn commit_unit(
    &mut self,
    committing: &mut Committing,
    spent: &mut dyn FnMut() -> bool,
  ) -> StagingStep {
    let Committing {
      run,
      post_state,
      runs,
      ..
    } = committing;
    let Some(commit) = run.commit.as_mut() else {
      return match self.prepare_step(run, (post_state, runs), spent) {
        ApplyStep::Lend(unlent) => StagingStep::Lend(unlent),
        _ => StagingStep::More,
      };
    };
    if self.commit_step(commit, spent) {
      StagingStep::Staged
    } else {
      StagingStep::More
    }
  }

  /// The stage's forks of the eight current maps, one count each ([`Forks`]).
  fn fork_maps(&mut self) -> Forks {
    Forks {
      content_history: self.content_history.fork(),
      content: self.content.fork(),
      specials: self.specials.fork(),
      dirs: self.dirs.fork(),
      modes: self.modes.fork(),
      symlinks: self.symlinks.fork(),
      hardlinks: self.hardlinks.fork(),
      xattrs: self.xattrs.fork(),
      links_to: self.links_to.fork(),
    }
  }

  /// Exchanges the green's eight current maps with `forks`, the arenas staying in the green: the stage's
  /// in place for a step, and the published back after it. Twice is none.
  fn swap_forks(&mut self, forks: &mut Forks) {
    self
      .content_history
      .swap_with_fork(&mut forks.content_history);
    self.content.swap_with_fork(&mut forks.content);
    self.specials.swap_with_fork(&mut forks.specials);
    self.dirs.swap_with_fork(&mut forks.dirs);
    self.modes.swap_with_fork(&mut forks.modes);
    self.symlinks.swap_with_fork(&mut forks.symlinks);
    self.hardlinks.swap_with_fork(&mut forks.hardlinks);
    self.xattrs.swap_with_fork(&mut forks.xattrs);
    self.links_to.swap_with_fork(&mut forks.links_to);
  }

  /// Gives `forks`' holds back in the green's arenas: the nodes no published map shares freed.
  fn release_forks(&mut self, forks: Forks) {
    let mut holders = crate::history::HistoryHolders {
      chunks: &mut self.chunks,
      blocks: self.content.blocks_mut(),
    };
    self
      .content_history
      .release_fork_held(forks.content_history, &mut holders);
    self.content.release_fork(forks.content);
    self.specials.release_fork(forks.specials);
    self.dirs.release_fork(forks.dirs);
    self.modes.release_fork(forks.modes);
    self.symlinks.release_fork(forks.symlinks);
    self.hardlinks.release_fork(forks.hardlinks);
    self.xattrs.release_fork(forks.xattrs);
    self.links_to.release_fork(forks.links_to);
  }

  /// Publishes a staged `staging`, moving the head to its version last: its version and what the chain records of it;
  /// `None` for one not staged whole, which is given up.
  pub fn publish(&mut self, staging: Staging) -> Option<(u64, Vec<crate::chain_entry::Applied>)> {
    if !staging.staged {
      self.abandon_staging(staging);
      return None;
    }
    let mut staging = staging;
    // The stage's scalars become the green's, then its forks the green's maps, the published roots they replace given
    // back against the stage's scalars (A-752 part 3c U6, U9): what those roots alone held leaves the new totals.
    self.swap_scalars(&mut staging.scalars);
    if let Some(mut forks) = staging.forks.take() {
      self.swap_forks(&mut forks);
      self.release_forks(forks);
    }
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
    let mut staging = staging;
    // The forks given back against the stage's scalars, which are then dropped: the published ones never moved (A-752
    // part 3c U9).
    if let Some(forks) = staging.forks.take() {
      self.swap_scalars(&mut staging.scalars);
      self.release_forks(forks);
      self.swap_scalars(&mut staging.scalars);
    }
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
