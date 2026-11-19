//! A rebase that restates a copy (A-626 part 4; §4.16 "Rebase, the only corrective path"; A-586, A-663). A clean rebase
//! restates a work's accepted effects as its journal over the head ([`Green::rebased_from`]): a copy as the deriver's
//! `VolumeOp::Copy` (its source, its destination, the source's version, whether it replaces), and every operation its
//! own increment makes in the copied tree after it, as the work made them there. A file the work edited beneath a copy is
//! no rename, though its effect names a source (`ContentSource::Copied`, which a rename's edited destination names too,
//! A-547): its edits are restated over the bytes the copy brings, at the copy's version, and no node moves.
//!
//! The deriver composes a journal holding a copy against a base that holds the copied tree where the copy puts it
//! ([`Green::copied_base`], from [`Green::base_for`]): the source's tree at the copy's version, every key re-rooted at the
//! destination, in place of whatever the destination's tree held, so what the journal does beneath the copy composes
//! against what the copy brings, as the verdict judges it (`copy`'s same-increment rule).

use std::collections::{BTreeMap, BTreeSet};

use super::copy::{in_tree, joined, relative};
use super::{ContentSource, Effect, Green, IdentityKey, content_volume_ops};
use crate::increment::{Base, VolumeOp};

/// Takes everything in `root`'s tree out of `base`.
fn drop_tree(base: &mut Base, root: &str) {
  base.files.retain(|(path, _)| !in_tree(path, root));
  base.specials.retain(|(path, _)| !in_tree(path, root));
  base.dirs.retain(|path| !in_tree(path, root));
  base.modes.retain(|(path, _)| !in_tree(path, root));
  base.symlinks.retain(|(path, _)| !in_tree(path, root));
  base.xattrs.retain(|(path, ..)| !in_tree(path, root));
  base
    .hardlinks
    .retain(|(link, target)| !in_tree(link, root) && !in_tree(target, root));
}

impl Green {
  /// `base` with `to`'s tree replaced by `from`'s tree as version `version` held it, every key re-rooted at `to`
  /// (the module docs): what a journal's `VolumeOp::Copy` brings, for the deriver to compose against. A hard link in
  /// the source's tree is left out, as the verdict refuses a copy of one (A-586).
  pub(super) fn copied_base(&self, (from, to, version): (&str, &str, u64), base: &mut Base) {
    drop_tree(base, to);
    for key in self.every_key(from) {
      if let Some(rel) = relative(key.path(), from) {
        self.push_copied(&key, (joined(to, rel), version), base);
      }
    }
  }

  /// `key`'s entry at `version`, where it holds one, pushed into `base` at `at`.
  fn push_copied(&self, key: &IdentityKey, (at, version): (String, u64), base: &mut Base) {
    match key {
      IdentityKey::File(path) => base.files.extend(
        self
          .content_len_at(path, version)
          .map(|len| (at, u64::try_from(len).unwrap_or(u64::MAX))),
      ),
      IdentityKey::Special(path) => base
        .specials
        .extend(self.special_at(path, version).map(|node| (at, node))),
      IdentityKey::Dir(path) => base.dirs.extend(self.dir_at(path, version).then_some(at)),
      IdentityKey::Mode(path) => base
        .modes
        .extend(self.mode_at(path, version).map(|mode| (at, mode))),
      IdentityKey::Symlink(path) => base
        .symlinks
        .extend(self.symlink_at(path, version).map(|target| (at, target))),
      IdentityKey::Xattr(path, name) => base.xattrs.extend(
        self
          .xattr_at(path, name, version)
          .map(|value| (at, name.clone(), value)),
      ),
      IdentityKey::Hardlink(_) | IdentityKey::Copy(_) => {}
    }
  }

  /// Restates a file effect that names a source (`ContentSource::Copied`, the module docs): beneath a copy restated
  /// before it (`copies`, each destination with its source's version, in the order restated), the edits the work made
  /// there over the bytes the copy brings; anywhere else the rename it is, its source's removal the rename's
  /// (`moved_from`). The file's bytes after the rebase go into `changed`.
  pub(super) fn restate_sourced(
    &self,
    (path, bytes, from, edits): (String, Vec<u8>, String, Vec<crate::ops_doc::Op>),
    (copies, moved_from): (&[(String, u64)], &mut BTreeSet<String>),
    (journal, changed): (&mut Vec<VolumeOp>, &mut BTreeMap<String, Option<Vec<u8>>>),
  ) {
    match copies.iter().rev().find(|(root, _)| in_tree(&path, root)) {
      Some((_, version)) => {
        let brought = self
          .content_len_at(&from, *version)
          .map_or(0, |len| u64::try_from(len).unwrap_or(u64::MAX));
        journal.extend(content_volume_ops(&path, &edits, brought));
      }
      None => {
        journal.push(VolumeOp::Rename {
          from: from.clone(),
          to: path.clone(),
        });
        // The edits the work made after the rename follow it, over the bytes it moved (A-547).
        let moved_len = self
          .content
          .get(&from)
          .map_or(0, |moved| moved.len() as u64);
        journal.extend(content_volume_ops(&path, &edits, moved_len));
        moved_from.insert(from);
      }
    }
    changed.insert(path, Some(bytes));
  }
}

/// The destinations of the copies among a version's effects: a file effect beneath one, which names its source in the
/// copied tree, moves no node there (`link_places`).
pub(super) fn copy_roots(effects: &[Effect]) -> Vec<&str> {
  effects
    .iter()
    .filter_map(|effect| match effect {
      Effect::CopyTree { dst, .. } => Some(dst.as_str()),
      _ => None,
    })
    .collect()
}

/// Whether `effect` sets a file beneath one of the copies' destinations (`roots`) from its source in the copied tree:
/// an edit there, which moves no node.
pub(super) fn copied_edit(effect: &Effect, roots: &[&str]) -> bool {
  matches!(effect, Effect::SetContent(path, _, _, ContentSource::Copied(..)) if roots.iter().any(|root| in_tree(path, root)))
}
