//! Each key's last change in the maps the verdict reads it from (`file_removed` and the mode, symbolic link, hard
//! link, attribute and IPC node maps), written by a commit only through its stage (A-752 part 3c U4; main 01a20182's
//! ruling of 2026-11-28 02:22).
//!
//! **The rule.** A commit never reads these maps: every read is the verdict's, the stack's or a checkpoint's, outside
//! it. So a commit writes them aside ([`Aside`], insert and no read: a commit reading its own pending last changes
//! does not compile), and its publish moves them in ([`Published::publish`]), the stage's turns unseen by every reader,
//! and a refusal drops them. Every commit writes them so, whole or a unit at a time, a replay's apply too: no other
//! write reaches a published map but a checkpoint's restore ([`Published::restore`]). A commit-path read of a
//! published map (its `get`) would read the value before the commit: it is not made, and a change that wants one
//! must first make that read the stage's.

use std::collections::BTreeMap;

/// A last-change map as its readers read it: each key's version, written only by a publish or a checkpoint's restore.
#[derive(Debug, Default)]
pub(super) struct Published<K: Ord> {
  map: BTreeMap<K, u64>,
}

impl<K: Ord> Published<K> {
  /// `key`'s last change, if it has one.
  pub(super) fn get<Q>(&self, key: &Q) -> Option<&u64>
  where
    K: std::borrow::Borrow<Q>,
    Q: Ord + ?Sized,
  {
    self.map.get(key)
  }

  /// Whether `key` has a last change.
  #[cfg(test)]
  pub(super) fn contains_key<Q>(&self, key: &Q) -> bool
  where
    K: std::borrow::Borrow<Q>,
    Q: Ord + ?Sized,
  {
    self.map.contains_key(key)
  }

  /// Sets `key`'s last change from a checkpoint being restored: no commit's write.
  pub(super) fn restore(&mut self, key: K, version: u64) {
    self.map.insert(key, version);
  }

  /// Moves a published commit's last changes in, each over the key's earlier one.
  fn publish(&mut self, aside: BTreeMap<K, u64>) {
    for (key, version) in aside {
      self.map.insert(key, version);
    }
  }
}

/// A commit's last changes, written aside until its publish ([`Aside::publish_into`]): inserts and no read (module
/// docs). Bounded by the keys the commit changes.
#[derive(Debug, Default)]
pub(super) struct Aside {
  removed: BTreeMap<String, u64>,
  modes: BTreeMap<String, u64>,
  symlinks: BTreeMap<String, u64>,
  hardlinks: BTreeMap<String, u64>,
  xattrs: BTreeMap<(String, String), u64>,
  specials: BTreeMap<String, u64>,
}

/// The published maps an [`Aside`] moves into, each by its field's name on the green.
pub(super) struct Targets<'a> {
  pub(super) removed: &'a mut Published<String>,
  pub(super) modes: &'a mut Published<String>,
  pub(super) symlinks: &'a mut Published<String>,
  pub(super) hardlinks: &'a mut Published<String>,
  pub(super) xattrs: &'a mut Published<(String, String)>,
  pub(super) specials: &'a mut Published<String>,
}

impl Aside {
  /// `path`'s file removed at `version`.
  pub(super) fn removed(&mut self, path: String, version: u64) {
    self.removed.insert(path, version);
  }

  /// `path`'s mode changed at `version`.
  pub(super) fn mode(&mut self, path: String, version: u64) {
    self.modes.insert(path, version);
  }

  /// `path`'s symbolic link changed at `version`.
  pub(super) fn symlink(&mut self, path: String, version: u64) {
    self.symlinks.insert(path, version);
  }

  /// `path`'s hard link changed at `version`.
  pub(super) fn hardlink(&mut self, path: String, version: u64) {
    self.hardlinks.insert(path, version);
  }

  /// `key`'s attribute changed at `version`.
  pub(super) fn xattr(&mut self, key: (String, String), version: u64) {
    self.xattrs.insert(key, version);
  }

  /// `path`'s IPC node changed at `version`.
  pub(super) fn special(&mut self, path: String, version: u64) {
    self.specials.insert(path, version);
  }

  /// Moves every last change written aside into `into`: a commit's publish.
  pub(super) fn publish_into(self, into: Targets<'_>) {
    into.removed.publish(self.removed);
    into.modes.publish(self.modes);
    into.symlinks.publish(self.symlinks);
    into.hardlinks.publish(self.hardlinks);
    into.xattrs.publish(self.xattrs);
    into.specials.publish(self.specials);
  }
}
