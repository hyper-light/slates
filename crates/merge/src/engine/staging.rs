//! A held verdict staged a unit a step and published whole (A-752 part 3c; main 01a20182's rulings of 2026-11-28 00:42
//! to 01:42): its commit's changes made where no reader at the head sees them, and published in one step at its end,
//! so a reader never waits on a commit's turns and a commit refused part-way publishes nothing.
//!
//! **Built so far (U1).** Every read names its version (U0), so what the commit appends at its own version is above
//! the head until the publish moves the head to it, last (`Green::publish_run`: its delta pushed and its increment
//! accepted). The mode history is appended in place and taken back at a refusal: where each path's history stood
//! before the stage first touched it is marked ([`Marks`]), and a refusal truncates it there. **Owed (later units):**
//! the other histories and the change index taken back alike, the identity's changes and `change_one`'s `last` held to
//! the publish, the current maps forked into the stage, and the content history's values copied with their chunks
//! held; until then a refusal leaves those as the steps made them, and the stage holds no bytes of its own.

use std::collections::{BTreeMap, BTreeSet};

use super::{ApplyStep, Committing, Green, Judged};

/// Where each history a stage appends in place stood before its first touch (A-752 part 3c U1): a key's entries then,
/// or none for a key with no history. Bounded by the keys the verdict's commit touches.
#[derive(Debug, Default)]
pub(super) struct Marks {
  /// The mode history's.
  pub(super) modes: BTreeMap<String, Option<usize>>,
}

/// A held verdict being staged ([`Green::begin_staging`]): its commit under way, and whether it is staged whole.
pub struct Staging {
  committing: Committing,
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
    drop(staging);
    let Some(marks) = self.stage_marks.take() else {
      return;
    };
    for (path, held) in marks.modes {
      match held {
        Some(len) => {
          if let Some(history) = self.mode_history.get_mut(&path) {
            history.truncate(len);
          }
        }
        None => {
          self.mode_history.remove(&path);
        }
      }
    }
  }
}
