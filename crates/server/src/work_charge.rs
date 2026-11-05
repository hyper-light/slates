//! A work volume's charge (§4.2 all-cost admission; §4.16): a work keeps what it changed of its green (its own
//! files and removals, `crate::work_files`) and its journal of declared operations in the owner shard's memory, so it
//! is charged for them against the shard's budget like a dynamic volume's growth (`ShardBudget::grow`), and refused
//! `BudgetExceeded` when the budget cannot cover what a verb would add. The charge is taken before the verb changes
//! anything, so a refused verb changes nothing, and released as the work shrinks, is reset by a submit or rebase, or
//! is destroyed. The green's files a work has not changed are the green's charge, not the work's.
//!
//! The footprint counted is what the work keeps: each own file's path and bytes, each removal's path, and every
//! journal operation's size and heap ([`slates_merge::increment::VolumeOp::footprint`]). Edits and declarations adjust
//! the charge by exactly what they add and remove (an edit of a file the work has not changed yet adds the whole copy
//! it takes), so a large work is never recounted per edit; a reset (create, submit, rebase, recovery) recounts the new
//! state once ([`footprint`]). `WorkState::charged` always equals [`footprint`] of the work after each verb (proven by
//! the model test below against a recount after every step).

use slates_db::catalog::VolumeId;
use slates_mem::budget::Reservation;
use slates_merge::increment::VolumeOp;

use crate::state::ShardState;
use crate::work_files::WorkFiles;

/// The footprint of one file a work holds: its path and its bytes.
pub(crate) fn file_footprint(path: &str, bytes: usize) -> u64 {
  u64::try_from(path.len().saturating_add(bytes)).unwrap_or(u64::MAX)
}

/// The whole footprint of a work's own `files` and `journal`: what a reset recounts.
pub(crate) fn footprint(files: &WorkFiles, journal: &[VolumeOp]) -> u64 {
  journal
    .iter()
    .map(VolumeOp::footprint)
    .fold(files.footprint(), u64::saturating_add)
}

/// The change from a work's charge of `before` to `after`, plus `ops` recorded with it: `(added, removed)`, one of
/// the two charge figures zero, so a verb grows or shrinks once and never holds both at a time.
fn change(before: u64, after: u64, ops: u64) -> (u64, u64) {
  if after >= before {
    (after.saturating_sub(before).saturating_add(ops), 0)
  } else {
    let fall = before.saturating_sub(after);
    (ops.saturating_sub(fall), fall.saturating_sub(ops))
  }
}

/// Grows `work`'s charge by `bytes`, before the verb adds them: `Err(available)` when the budget cannot cover
/// them, with nothing changed.
pub(crate) fn grow(state: &mut ShardState, work: VolumeId, bytes: u64) -> Result<(), u64> {
  if bytes == 0 {
    return Ok(());
  }
  match state.store.grow(bytes) {
    Ok(_) => {
      if let Some(w) = state.works.get_mut(&work) {
        w.charged = w.charged.saturating_add(bytes);
      }
      Ok(())
    }
    Err(_) => Err(state.store.admittable()),
  }
}

/// Shrinks `work`'s charge by `bytes` the verb removed (never below zero).
pub(crate) fn shrink(state: &mut ShardState, work: VolumeId, bytes: u64) {
  let Some(w) = state.works.get_mut(&work) else {
    return;
  };
  let released = bytes.min(w.charged);
  w.charged = w.charged.saturating_sub(released);
  state.store.budget.release(Reservation { bytes: released });
}

/// Moves `work`'s charge to `wanted` (a reset's recounted footprint): grows first, refusing with nothing changed
/// when the budget cannot cover the rise; a fall is released.
pub(crate) fn set_to(state: &mut ShardState, work: VolumeId, wanted: u64) -> Result<(), u64> {
  let charged = state.works.get(&work).map_or(0, |w| w.charged);
  if wanted > charged {
    grow(state, work, wanted.saturating_sub(charged))
  } else {
    shrink(state, work, charged.saturating_sub(wanted));
    Ok(())
  }
}

/// Releases a removed work's whole `charged`.
pub(crate) fn release(state: &mut ShardState, charged: u64) {
  state.store.budget.release(Reservation { bytes: charged });
}

/// What an edit of `path` changes in a work's footprint, before it is applied: `old` is the file as the work sees it
/// now (`None` when it is new) and `owned_cost` the charge of the work's own entry for it now
/// ([`WorkFiles::entry_cost`]: zero for a green file the work has not changed). After the edit the work owns the file
/// whole: its path and its spliced length (the deleted range clamped as the splice clamps it), plus the journal
/// operations the edit records.
pub(crate) fn edit_delta(
  (old, owned_cost): (Option<&[u8]>, u64),
  (path, at, delete_len, inserted): (&str, u64, u64, usize),
  ops: &[VolumeOp],
) -> (u64, u64) {
  let old_len = old.map_or(0, <[u8]>::len);
  let start = usize::try_from(at).unwrap_or(usize::MAX).min(old_len);
  let removed = usize::try_from(delete_len)
    .unwrap_or(usize::MAX)
    .min(old_len.saturating_sub(start));
  let after = file_footprint(
    path,
    old_len.saturating_sub(removed).saturating_add(inserted),
  );
  let recorded = ops
    .iter()
    .map(VolumeOp::footprint)
    .fold(0u64, u64::saturating_add);
  change(owned_cost, after, recorded)
}

/// What a namespace declaration `op` changes in a work's footprint, before it is applied to `files` (as the work
/// sees them over `green` at `base`): the operation itself; an unlink leaves a removal in place of the work's entry;
/// a rename of a file the work sees removes it at its old path and holds its bytes at the new one (onto itself it
/// changes nothing).
pub(crate) fn declare_delta(
  files: &WorkFiles,
  (green, base): (Option<&slates_merge::engine::Green>, u64),
  op: &VolumeOp,
) -> (u64, u64) {
  let own = op.footprint();
  match op {
    VolumeOp::Unlink { path } => change(files.entry_cost(path), file_footprint(path, 0), own),
    VolumeOp::Rename { from, to } if from != to => match files.get(green, base, from) {
      Some(bytes) => {
        let before = files.entry_cost(from).saturating_add(files.entry_cost(to));
        let after = file_footprint(from, 0).saturating_add(file_footprint(to, bytes.len()));
        change(before, after, own)
      }
      None => (own, 0),
    },
    _ => (own, 0),
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;
  use proptest::prelude::*;

  /// One step of a generated history: an edit of one of a few paths, an unlink, or a rename (onto an existing
  /// file or onto itself included).
  #[derive(Clone, Debug)]
  enum Step {
    Edit {
      path: usize,
      at: u64,
      delete_len: u64,
      inserted: usize,
    },
    Unlink {
      path: usize,
    },
    Rename {
      from: usize,
      to: usize,
    },
  }

  /// A step: three kinds in five are edits, one an unlink, one a rename (one tuple strategy mapped by its kind, so
  /// no boxed union of strategies is needed).
  fn step() -> impl Strategy<Value = Step> {
    (0u8..5, 0usize..3, 0usize..3, 0u64..64, 0u64..64, 0usize..48).prop_map(
      |(kind, first, second, at, delete_len, inserted)| match kind {
        3 => Step::Unlink { path: first },
        4 => Step::Rename {
          from: first,
          to: second,
        },
        _ => Step::Edit {
          path: first,
          at,
          delete_len,
          inserted,
        },
      },
    )
  }

  /// The journal operations an edit records, as `crate::verbs::edit` records them.
  fn edit_ops(
    path: &str,
    old: Option<&[u8]>,
    (at, delete_len, inserted): (u64, u64, usize),
  ) -> Vec<VolumeOp> {
    let mut ops = Vec::new();
    if old.is_none() {
      ops.push(VolumeOp::Create {
        path: path.to_owned(),
      });
    }
    if delete_len > 0 {
      ops.push(VolumeOp::Delete {
        path: path.to_owned(),
        at,
        len: delete_len,
      });
    }
    if inserted > 0 {
      ops.push(VolumeOp::Insert {
        path: path.to_owned(),
        at,
        len: u64::try_from(inserted).unwrap(),
      });
    }
    ops
  }

  /// The green a work in the model is cloned from: two of the three paths exist at version 0.
  fn green() -> slates_merge::engine::Green {
    let mut origin = slates_merge::origin::Origin::default();
    origin
      .files
      .push(("a.txt".to_owned(), b"base bytes of a".to_vec()));
    origin.files.push(("c".to_owned(), vec![b'c'; 40]));
    slates_merge::engine::Green::with_origin(&origin)
  }

  /// Applies `step` to the work's `files` and `journal` as the verbs do: the charge's change it made, by the deltas.
  fn apply(
    step: &Step,
    green: &slates_merge::engine::Green,
    files: &mut WorkFiles,
    journal: &mut Vec<VolumeOp>,
  ) -> (u64, u64) {
    let paths = ["a.txt", "dir/b.rs", "c"];
    match step {
      Step::Edit {
        path,
        at,
        delete_len,
        inserted,
      } => {
        let path = paths[*path];
        let old = files.get(Some(green), 0, path).map(<[u8]>::to_vec);
        let ops = edit_ops(path, old.as_deref(), (*at, *delete_len, *inserted));
        let delta = edit_delta(
          (old.as_deref(), files.entry_cost(path)),
          (path, *at, *delete_len, *inserted),
          &ops,
        );
        let file = files.make_own(path, old.unwrap_or_default());
        let start = usize::try_from(*at).unwrap().min(file.len());
        let del = usize::try_from(*delete_len)
          .unwrap()
          .min(file.len() - start);
        file.splice(start..start + del, std::iter::repeat_n(b'x', *inserted));
        journal.extend(ops);
        delta
      }
      Step::Unlink { path } => {
        let op = VolumeOp::Unlink {
          path: paths[*path].to_owned(),
        };
        let delta = declare_delta(files, (Some(green), 0), &op);
        files.remove(paths[*path]);
        journal.push(op);
        delta
      }
      Step::Rename { from, to } => {
        let (from, to) = (paths[*from], paths[*to]);
        let op = VolumeOp::Rename {
          from: from.to_owned(),
          to: to.to_owned(),
        };
        let delta = declare_delta(files, (Some(green), 0), &op);
        if from != to
          && let Some(bytes) = files.get(Some(green), 0, from).map(<[u8]>::to_vec)
        {
          files.set(to, bytes);
          files.remove(from);
        }
        journal.push(op);
        delta
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// §4.2 (a work's charge): do apply a generated history of edits, unlinks and renames to a work cloned from a
    /// green holding two of its paths, adjusting a running charge by each step's delta (`edit_delta`,
    /// `declare_delta`); expect the charge to equal a full recount (`footprint`) after every step, so the incremental
    /// accounting never drifts from what the work keeps, including the copy a first change of a green file takes.
    #[test]
    fn the_incremental_charge_equals_a_recount_after_every_step(steps in prop::collection::vec(step(), 1..60)) {
      let green = green();
      let mut files = WorkFiles::default();
      let mut journal: Vec<VolumeOp> = Vec::new();
      let mut charged = 0u64;
      for step in &steps {
        let (added, removed) = apply(step, &green, &mut files, &mut journal);
        charged = charged.saturating_add(added).saturating_sub(removed);
        prop_assert_eq!(charged, footprint(&files, &journal), "after {:?}", step);
      }
    }
  }
}
