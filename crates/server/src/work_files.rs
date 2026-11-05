//! A work volume's files as a clone of its green version (§4.16 "a work volume is only ever a clone of a green
//! version"; D-5, clones by reference; §4.2 all-cost admission). A work holds only what it changed: a file it edited
//! or renamed into place (its whole bytes, copied from the green on its first change), or the removal of one. Every
//! other file is the green's as of the work's base version, read from the green's engine
//! ([`slates_merge::engine::Green::file_at`]), which the service never folds past a live work's base
//! (`merge_service::reachable_floor`). So creating a work, and resetting it after an accepted submit, costs what the
//! work changed, never the tree.
//!
//! Evidence (2026-11-04, `crates/client/examples/load_bench.rs` over Linux v7.4-rc1's `kernel/` directory, 754 files,
//! 16 MB): with a work seeded by a copy of every file of its green, one client's `CreateWork` took 4.2 ms and an
//! accepted submit 14.1 ms (each reset the work by a second whole copy), and at a hundred clients the median
//! `CreateWork` took 441 ms on the green's owner shard. Over the whole kernel tree a work was a 1.8 GB copy.
//!
//! The charge (`crate::work_charge`) counts exactly what this holds: each own file's path and bytes, and each removal's
//! path. The green's bytes are the green's charge, not the work's.

use std::collections::BTreeMap;

use slates_merge::engine::Green;

/// What a work changed, by path: `Some(bytes)` a file it holds its own bytes of, `None` a file it removed. A path
/// absent here is the green's as of the work's base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkFiles {
  own: BTreeMap<String, Option<Vec<u8>>>,
}

impl WorkFiles {
  /// A work's files after a rebase: the files the rebase changed relative to the head it moves onto.
  pub(crate) fn from_changed(changed: BTreeMap<String, Option<Vec<u8>>>) -> WorkFiles {
    WorkFiles { own: changed }
  }

  /// The file at `path` as the work sees it: its own bytes, nothing for a file it removed, else the green's as of
  /// `base` (none when the green is gone).
  pub(crate) fn get<'a>(
    &'a self,
    green: Option<&'a Green>,
    base: u64,
    path: &str,
  ) -> Option<&'a [u8]> {
    match self.own.get(path) {
      Some(entry) => entry.as_deref(),
      None => green.and_then(|green| green.file_at(path, base)),
    }
  }

  /// Whether the work holds its own entry (bytes or a removal) for `path`.
  pub(crate) fn owns(&self, path: &str) -> bool {
    self.own.contains_key(path)
  }

  /// The work's own bytes of `path`, made from `seed` (the file as the work sees it now, empty for a new one) on the
  /// first change: what an edit splices into.
  pub(crate) fn make_own(&mut self, path: &str, seed: Vec<u8>) -> &mut Vec<u8> {
    let entry = self.own.entry(path.to_owned()).or_insert(None);
    entry.get_or_insert(seed)
  }

  /// Records `path` removed in the work.
  pub(crate) fn remove(&mut self, path: &str) {
    self.own.insert(path.to_owned(), None);
  }

  /// Holds `bytes` as the work's own file at `path`.
  pub(crate) fn set(&mut self, path: &str, bytes: Vec<u8>) {
    self.own.insert(path.to_owned(), Some(bytes));
  }

  /// Forgets every change: the work equals its green at its (new) base.
  pub(crate) fn clear(&mut self) {
    self.own.clear();
  }

  /// The charge of the work's own entry for `path`: its path and bytes, its path alone for a removal, nothing for a
  /// file it has not changed.
  pub(crate) fn entry_cost(&self, path: &str) -> u64 {
    self.own.get(path).map_or(0, |entry| {
      crate::work_charge::file_footprint(path, entry.as_ref().map_or(0, Vec::len))
    })
  }

  /// The charge of everything the work holds of its own.
  pub(crate) fn footprint(&self) -> u64 {
    self
      .own
      .keys()
      .map(|path| self.entry_cost(path))
      .fold(0u64, u64::saturating_add)
  }

  /// Every file the work sees, with its bytes, in path order: the green's as of `base` with the work's own laid over
  /// it. Cost: the green's paths; a listing's or a status's, never an edit's or a submit's.
  pub(crate) fn merged<'a>(
    &'a self,
    green: Option<&'a Green>,
    base: u64,
  ) -> Vec<(&'a str, &'a [u8])> {
    let mut files: BTreeMap<&str, &[u8]> = green
      .map(|green| green.files_at(base))
      .unwrap_or_default()
      .into_iter()
      .collect();
    for (path, entry) in &self.own {
      match entry {
        Some(bytes) => {
          files.insert(path.as_str(), bytes.as_slice());
        }
        None => {
          files.remove(path.as_str());
        }
      }
    }
    files.into_iter().collect()
  }
}
