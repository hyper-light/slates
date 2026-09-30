//! The removal oracle's rules (§4.15 step 6; D-26; AUD-29-04), stated once for both of its legs: the
//! simulated host (`tests/removal.rs`) and a real directory (`tests/os_removal.rs`). A leg lands one
//! history per armed outsider edit and reports what it saw as a [`Seen`]; [`judge`] holds it to the rules:
//! no outsider file is lost that a later outsider edit did not itself replace; nothing of the landing's is
//! left but entries its report says it kept; an entry whose witnessed object the outsider replaced before
//! the landing moved it is not reported written; an entry not reported written did not remove its
//! witnessed object; and a removal reported written did.

use std::collections::{BTreeMap, BTreeSet};

use slates_land::engine::{Degradation, LandingRefusal, LandingReport, Outcome};

/// Shape: the first outsider edit's bytes, distinct from every base and overlay entry's.
pub(crate) const OUTSIDER: &[u8] = b"the outsider's own file";
/// Shape: a second outsider edit's bytes, distinct from the first's.
pub(crate) const LATER_OUTSIDER: &[u8] = b"the outsider's later file";
/// Format: the prefix of every name a landing gives an entry inside the target.
const LANDING_PREFIX: &str = ".slates-";

/// What a history lands, over a base entry at `/doomed`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Removal {
  /// A regular file deleted.
  File,
  /// A symlink deleted.
  Symlink,
  /// A directory with a file in it removed.
  Directory,
  /// A file replaced; the displaced original is removed. `exchange: false` is the fallback.
  Replace { exchange: bool },
  /// A directory removed and made again empty (a clear); the displaced original is removed.
  Clear { exchange: bool },
  /// A directory renamed to `/moved`, the outsider at its origin.
  Rename,
  /// A directory renamed to `/moved`, the outsider at its destination.
  RenameOnto,
}

/// Every kind of history.
pub(crate) const REMOVALS: [Removal; 9] = [
  Removal::File,
  Removal::Symlink,
  Removal::Directory,
  Removal::Replace { exchange: true },
  Removal::Replace { exchange: false },
  Removal::Clear { exchange: true },
  Removal::Clear { exchange: false },
  Removal::Rename,
  Removal::RenameOnto,
];

impl Removal {
  /// The path the outsider replaces.
  pub(crate) fn interfered(self) -> &'static str {
    match self {
      Self::RenameOnto => "/moved",
      _ => "/doomed",
    }
  }

  /// The manifest entry the history judges.
  pub(crate) fn entry(self) -> &'static str {
    match self {
      Self::Rename | Self::RenameOnto => "/moved",
      _ => "/doomed",
    }
  }

  /// Whether the landing may use the atomic exchange (the fallback's histories withhold it).
  pub(crate) fn exchange(self) -> bool {
    match self {
      Self::Replace { exchange } | Self::Clear { exchange } => exchange,
      _ => true,
    }
  }

  /// Whether the outsider replaces the path where the witnessed object sits until the landing moves it.
  fn interferes_with_witnessed(self) -> bool {
    !matches!(self, Self::RenameOnto)
  }

  /// Whether the entry removes its witnessed object once written (a rename keeps it, at its new name).
  fn removes_witnessed(self) -> bool {
    !matches!(self, Self::Rename | Self::RenameOnto)
  }
}

/// What one outsider edit did: the inode it created and the one it displaced from its path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fired {
  /// The inode the edit created.
  pub(crate) created: u64,
  /// The inode the path held before, if it held one.
  pub(crate) displaced: Option<u64>,
}

/// What a leg saw of one landed history.
pub(crate) struct Seen<'a> {
  /// Whether every armed edit fired.
  pub(crate) all_fired: bool,
  /// What each edit did, in firing order.
  pub(crate) fired: Vec<Fired>,
  /// Every entry under the target after the landing, by inode, with its path from the target (`/doomed`).
  pub(crate) inodes: BTreeMap<u64, String>,
  /// The inode of the witnessed base entry at `/doomed`.
  pub(crate) witnessed: u64,
  /// The landing's result.
  pub(crate) result: &'a Result<LandingReport, LandingRefusal>,
}

/// What a judged history showed, for a leg's non-vacuity counts.
pub(crate) struct Verdict {
  /// The judged entry was written.
  pub(crate) written: bool,
  /// An outsider edit replaced the witnessed object before the landing moved it.
  pub(crate) stale: bool,
}

/// The paths the report says it kept entries at.
pub(crate) fn kept_paths(result: &Result<LandingReport, LandingRefusal>) -> BTreeSet<String> {
  let Ok(report) = result else {
    return BTreeSet::new();
  };
  report
    .degraded
    .iter()
    .filter_map(|d| match d {
      Degradation::Kept { kept, .. } => Some(kept.to_string()),
      _ => None,
    })
    .collect()
}

/// Whether the history's judged entry was written.
pub(crate) fn written(removal: Removal, result: &Result<LandingReport, LandingRefusal>) -> bool {
  result.as_ref().is_ok_and(|report| {
    report
      .entries
      .iter()
      .any(|e| e.path.as_ref() == removal.entry() && e.outcome == Some(Outcome::Written))
  })
}

/// A reference history (no outsider): the entry is written and nothing of the landing's is kept or left.
pub(crate) fn assert_reference(removal: Removal, seen: &Seen<'_>) {
  assert!(
    written(removal, seen.result),
    "{removal:?}: the reference writes the entry: {:?}",
    seen.result
  );
  assert!(kept_paths(seen.result).is_empty(), "{removal:?}");
  assert!(
    seen
      .inodes
      .values()
      .all(|path| !path.contains(LANDING_PREFIX)),
    "{removal:?}: the reference left {:?}",
    seen.inodes
  );
}

/// Judges one interfered history by the module doc's rules; `label` names it in a failure.
pub(crate) fn judge(removal: Removal, label: &str, seen: &Seen<'_>) -> Verdict {
  assert!(seen.all_fired, "{label}: an armed edit never happened");
  let kept = kept_paths(seen.result);
  assert_outsiders_survive(removal, label, seen, &kept);
  assert_nothing_left_unreported(label, seen, &kept);
  assert_outcome_honest(removal, label, seen)
}

/// No outsider file is lost that a later outsider edit did not itself replace: each is at its path or at a
/// path the report says it kept it at.
fn assert_outsiders_survive(
  removal: Removal,
  label: &str,
  seen: &Seen<'_>,
  kept: &BTreeSet<String>,
) {
  for edit in &seen.fired {
    if seen.fired.iter().any(|e| e.displaced == Some(edit.created)) {
      continue;
    }
    let at = seen.inodes.get(&edit.created);
    assert!(
      at.is_some_and(|path| path == removal.interfered() || kept.contains(path)),
      "{label}: the outsider's file {} is at {at:?}, neither its path nor a kept one {kept:?}: {:?}",
      edit.created,
      seen.result
    );
  }
}

/// Nothing of the landing's is left but entries its report says it kept (and what is beneath them).
fn assert_nothing_left_unreported(label: &str, seen: &Seen<'_>, kept: &BTreeSet<String>) {
  for path in seen.inodes.values() {
    let first = path.split('/').find(|part| !part.is_empty()).unwrap_or("");
    let under_kept = kept
      .iter()
      .any(|k| path == k || path.starts_with(&format!("{k}/")));
    assert!(
      !first.starts_with(LANDING_PREFIX) || under_kept,
      "{label}: the landing left {path} unreported (kept {kept:?}): {:?}",
      seen.result
    );
  }
}

/// The judged entry's outcome is honest: not written when the outsider replaced its witnessed object first;
/// its witnessed object still under the target when it was not written and no outsider replaced it; and a
/// removal reported written removed it.
fn assert_outcome_honest(removal: Removal, label: &str, seen: &Seen<'_>) -> Verdict {
  let outsider_removed_it = seen
    .fired
    .iter()
    .any(|e| e.displaced == Some(seen.witnessed));
  let stale = removal.interferes_with_witnessed() && outsider_removed_it;
  let written = written(removal, seen.result);
  assert!(
    !(stale && written),
    "{label}: the outsider replaced the witnessed object first, yet the entry was written: {:?}",
    seen.result
  );
  let still_there = seen.inodes.contains_key(&seen.witnessed);
  assert!(
    written || outsider_removed_it || still_there,
    "{label}: the entry was not written, yet its witnessed object is gone: {:?}",
    seen.result
  );
  assert!(
    !(written && removal.removes_witnessed()) || !still_there,
    "{label}: the entry was written, yet its witnessed object is still at {:?}",
    seen.inodes.get(&seen.witnessed)
  );
  Verdict { written, stale }
}
