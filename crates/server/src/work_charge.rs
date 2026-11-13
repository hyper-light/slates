//! A work volume's charge (§4.2 all-cost admission; §4.16): a work keeps the files it changed since its base and its
//! journal of declared operations in the owner shard's memory (`crate::work_files`; every other file is its green's,
//! charged to the green), so it is charged for them against the shard's budget like a dynamic volume's growth
//! (`ShardBudget::grow`), and refused `BudgetExceeded` when the budget cannot cover what a verb would add. The charge
//! is taken before the verb changes anything, so a refused verb changes nothing, and released as the work shrinks, is
//! reset by a submit or rebase, or is destroyed.
//!
//! The footprint counted is what the work keeps: every file it holds, by path and bytes (a removal by its path), and
//! every journal operation's size and heap ([`slates_merge::increment::VolumeOp::footprint`]). Edits and declarations
//! adjust the charge by exactly what they add and remove (`WorkFiles::splice_delta`, `WorkFiles::declare_delta`), so
//! a large work is never recounted per edit; a reset recounts the new state once. `WorkState::charged` always equals
//! [`footprint`] of the work after each verb (proven by the model test below against a recount after every step).

use slates_db::catalog::VolumeId;
use slates_mem::budget::Reservation;
use slates_merge::increment::VolumeOp;

use crate::state::ShardState;
use crate::work_files::WorkFiles;

/// The footprint of one file a work holds: its path and its bytes.
pub(crate) fn file_footprint(path: &str, bytes: usize) -> u64 {
  u64::try_from(path.len().saturating_add(bytes)).unwrap_or(u64::MAX)
}

/// The footprint of journal operations: each one's size and heap.
pub(crate) fn journal_footprint(journal: &[VolumeOp]) -> u64 {
  journal
    .iter()
    .map(VolumeOp::footprint)
    .fold(0, u64::saturating_add)
}

/// The whole footprint of a work's `files` and `journal`: what a reset recounts.
pub(crate) fn footprint(files: &WorkFiles, journal: &[VolumeOp]) -> u64 {
  files.footprint().saturating_add(journal_footprint(journal))
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

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;
  use crate::work_files::Base;
  use proptest::prelude::*;
  use slates_merge::engine::Green;
  use slates_merge::origin::Origin;
  use std::collections::BTreeMap;

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

  /// Shape: the paths a history touches; the base holds some of them.
  const PATHS: [&str; 4] = ["a.txt", "dir/b.rs", "c", "base-only"];

  /// A step: three kinds in five are edits, one an unlink, one a rename (one tuple strategy mapped by its kind, so
  /// no boxed union of strategies is needed).
  fn step() -> impl Strategy<Value = Step> {
    (0u8..5, 0usize..4, 0usize..4, 0u64..64, 0u64..64, 0usize..48).prop_map(
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

  /// Applies `step` to the work's files, its journal and a plain map of what the work sees (the model), as the
  /// verbs do: the charge's change it made, by the deltas.
  fn apply(
    step: &Step,
    base: Base<'_>,
    (files, journal, model): (
      &mut WorkFiles,
      &mut Vec<VolumeOp>,
      &mut BTreeMap<String, Vec<u8>>,
    ),
  ) -> (u64, u64) {
    match step {
      Step::Edit {
        path,
        at,
        delete_len,
        inserted,
      } => {
        let path = PATHS[*path];
        let old = files
          .get(base, path)
          .unwrap()
          .map(std::borrow::Cow::into_owned);
        let ops = edit_ops(path, old.as_deref(), (*at, *delete_len, *inserted));
        let (spliced, removed) =
          files.splice_delta(path, old.as_deref(), (*at, *delete_len, *inserted));
        let inserted_bytes = vec![b'x'; *inserted];
        files
          .splice(base, path, (*at, *delete_len), &inserted_bytes)
          .unwrap();
        let file = model.entry(path.to_owned()).or_default();
        let start = usize::try_from(*at).unwrap().min(file.len());
        let del = usize::try_from(*delete_len)
          .unwrap()
          .min(file.len() - start);
        file.splice(start..start + del, inserted_bytes.iter().copied());
        let added = journal_footprint(&ops) + spliced;
        journal.extend(ops);
        (added, removed)
      }
      Step::Unlink { path } => {
        let op = VolumeOp::Unlink {
          path: PATHS[*path].to_owned(),
        };
        let (moved, removed) = files.declare_delta(base, &op).unwrap();
        files.declare(base, &op).unwrap();
        model.remove(PATHS[*path]);
        let added = op.footprint() + moved;
        journal.push(op);
        (added, removed)
      }
      Step::Rename { from, to } => {
        let (from, to) = (PATHS[*from], PATHS[*to]);
        let op = VolumeOp::Rename {
          from: from.to_owned(),
          to: to.to_owned(),
        };
        let (moved, removed) = files.declare_delta(base, &op).unwrap();
        files.declare(base, &op).unwrap();
        if from != to
          && let Some(bytes) = model.remove(from)
        {
          model.insert(to.to_owned(), bytes);
        }
        let added = op.footprint() + moved;
        journal.push(op);
        (added, removed)
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

    /// §4.2 (a work's charge) and §4.16 (a work is a delta over its base): do apply a generated history of edits,
    /// unlinks and renames to a work over a green that holds some of the paths, adjusting a running charge by each
    /// step's delta; expect the charge to equal a full recount (`footprint`) after every step, so the incremental
    /// accounting never drifts from what the work keeps, and the work's view (`WorkFiles::files`, every file read
    /// through `WorkFiles::get`) to equal a plain map the same history was applied to, seeded with the green.
    #[test]
    fn the_incremental_charge_equals_a_recount_after_every_step(
      seeded in prop::collection::vec((0usize..4, 0usize..24), 0..4),
      steps in prop::collection::vec(step(), 1..60),
    ) {
      let mut origin = Origin::default();
      let mut model: BTreeMap<String, Vec<u8>> = BTreeMap::new();
      for (path, len) in &seeded {
        model.insert(PATHS[*path].to_owned(), vec![b'b'; *len]);
      }
      origin.files = model.iter().map(|(path, bytes)| (path.clone(), bytes.clone())).collect();
      let green = Green::with_origin(origin.clone());
      let base = Base { green: Some(&green), version: 0 };
      let mut files = WorkFiles::default();
      let mut journal: Vec<VolumeOp> = Vec::new();
      let mut charged = 0u64;
      for step in &steps {
        let (added, removed) = apply(step, base, (&mut files, &mut journal, &mut model));
        charged = charged.saturating_add(added).saturating_sub(removed);
        prop_assert_eq!(charged, footprint(&files, &journal), "after {:?}", step);
        let seen: BTreeMap<String, Vec<u8>> = files
          .files(base)
          .into_iter()
          .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
          .collect();
        prop_assert_eq!(&seen, &model, "after {:?}", step);
        for path in PATHS {
          let seen = files.get(base, path).unwrap();
          prop_assert_eq!(seen.as_deref(), model.get(path).map(Vec::as_slice));
        }
      }
    }
  }
}
