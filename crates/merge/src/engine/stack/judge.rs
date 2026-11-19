//! An upper's verdict against what changed between its base and the version it is restated over (A-659; `super`, its
//! module docs, "Restating an upper").
//!
//! The verdict judges a work against the changes after its base and against the head's state. Restated over a
//! version, an upper is judged by the verdict as a work based on that version, which checks the head's state and the
//! changes after it; what this module adds is every check of the changes after its base made against the changes
//! between its base and that version ([`Change`]), each in the words of the `Green::merge_*` it mirrors: a rename's
//! source removed or its destination changed, a removal of a node changed in any dimension, a mode, an attribute, a
//! link or an IPC node another work set otherwise, a link's node removed, a node-exclusive operation's path changed at
//! all. A value the new base holds as the upper sets it is the verdict's identical case and accepted.
//!
//! An upper's content operations are mapped through the changes at the path they were declared against (the path, or
//! the source a rename moved there) as a work's are through the deltas since its base ([`crate::map::map_range`]): one
//! no change met lands at its moved place, a truncate the green's appends outgrew as the delete of what it saw
//! (A-612); one a change met is a conflict unless it overwrites exactly the bytes the new base holds where it lands.
//! A length-changing operation a change met is a conflict here, where the verdict would check the whole file against a
//! work based on a version: the upper's base file is no version's, so that check would rebuild it (owed).

use std::collections::{BTreeMap, BTreeSet};

use super::moved::Change;
use crate::engine::{
  ConflictWindow, Green, Increment, Resolved, at_head, is_content, node_exclusive_paths,
  span_bytes, touched_range,
};
use crate::map::{Mapped, map_range};
use crate::ops_doc::{Op, OpKind};
use crate::range::Range;
use crate::verdict::MergeConflictClass;

/// What changed at each path an upper names, and the version it is restated over.
pub(super) struct Against<'a> {
  /// The change at each path.
  pub(super) changes: &'a BTreeMap<String, Change>,
  /// The version.
  pub(super) at: u64,
  /// What a path no change names answers: nothing changed.
  pub(super) unchanged: Change,
  /// The version the stack's bottom is based on, and the versions the stack's works landed as: a link another version
  /// made after it is one the upper's base never held (A-670, [`Green::linked_by_another`]).
  pub(super) since: (u64, &'a BTreeSet<u64>),
}

impl Against<'_> {
  /// The change at `path`.
  fn change(&self, path: &str) -> &Change {
    self.changes.get(path).unwrap_or(&self.unchanged)
  }
}

/// The windows an upper's operations meet, a whole path named once.
#[derive(Default)]
pub(super) struct Windows {
  found: Vec<ConflictWindow>,
  whole: BTreeSet<String>,
}

impl Windows {
  /// A whole-path window, once a path.
  pub(super) fn whole(&mut self, path: &str, class: MergeConflictClass) {
    if self.whole.insert(path.to_owned()) {
      self.found.push(Green::window(path, class));
    }
  }

  /// An overlap at `range` of `path`.
  pub(super) fn range(&mut self, path: &str, range: Range) {
    self.found.push(ConflictWindow {
      path: path.to_owned(),
      range,
      class: MergeConflictClass::Overlap,
    });
  }

  /// `kept` when nothing was found, else the windows.
  pub(super) fn or<T>(self, kept: T) -> Result<T, Vec<ConflictWindow>> {
    if self.found.is_empty() {
      Ok(kept)
    } else {
      Err(self.found)
    }
  }
}

impl Green {
  /// The upper `work`, resolved as `r`, judged against `against` (module docs): its operations restated over
  /// `against.at` (those found identical left out), or the windows of the changes it meets.
  pub(super) fn judge_upper(
    &self,
    (work, r): (&Increment, &Resolved),
    against: &Against<'_>,
  ) -> Result<Vec<Op>, Vec<ConflictWindow>> {
    let mut windows = Windows::default();
    self.upper_renames(r, against, &mut windows);
    self.upper_removals(r, against, &mut windows);
    self.upper_unseen_links(r, against, &mut windows);
    self.upper_metadata(r, against, &mut windows);
    self.upper_links(r, against, &mut windows);
    upper_node_exclusive(work, against, &mut windows);
    let ops = self.upper_content((work, r), against, &mut windows);
    windows.or(ops)
  }

  /// A rename's source removed is rename against rename; its destination changed is a type change for a link or an IPC
  /// node there, and rename against rename for a file (`Green::merge_rename`, `replaced_by_rename`).
  fn upper_renames(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    for (path, source) in &r.renames {
      let (from, to) = (against.change(source), against.change(path));
      if from.node {
        windows.whole(path, MergeConflictClass::RenameRename);
      } else if self.link_or_special_at(path, against.at) && to.any() {
        windows.whole(path, MergeConflictClass::TypeChanged);
      } else if to.content() {
        windows.whole(path, MergeConflictClass::RenameRename);
      }
    }
  }

  /// A removal, or a replacement by a rename, of a file a link names that another version made after the stack's base
  /// (A-670): the upper's base, its lower's post-state, never held that link, so the upper's writes to the file, if it
  /// wrote, went with the node in its own volume while the link keeps the node at the head. Refused as the verdict
  /// refuses one after a work's base (`Green::leaves_to_an_unseen_link`): delete against modify, or rename against
  /// rename for a replaced destination; unless the upper hands the node on itself (A-669, `Green::handed_home`). A link
  /// a work of the stack made is one the upper's base held. Links made after the version the upper is restated over
  /// are the verdict's, which judges the restated upper as a work based on it.
  fn upper_unseen_links(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    let cleared = r.cleared();
    let homes: BTreeSet<&str> = r
      .content
      .keys()
      .filter_map(|heir| self.handed_home(r, heir, &cleared))
      .collect();
    let removed = r
      .unlinks
      .iter()
      .map(|path| (path, MergeConflictClass::DeleteModify));
    let replaced = r
      .renames
      .keys()
      .filter(|dst| !cleared.contains(*dst))
      .map(|path| (path, MergeConflictClass::RenameRename));
    for (path, class) in removed.chain(replaced) {
      let file = self.content_len_at(path, against.at).is_some();
      if file && !homes.contains(path.as_str()) && self.linked_by_another(path, against) {
        windows.whole(path, class);
      }
    }
  }

  /// Whether a link naming the file at `path` was last made or turned by a version after the stack's base that no work
  /// of the stack landed as (`against.since`).
  fn linked_by_another(&self, path: &str, against: &Against<'_>) -> bool {
    let (base, landed) = against.since;
    self.links_to.get(path).into_iter().flatten().any(|link| {
      let changed = self.changed_last(self.hardlink_changed.get(link), link);
      changed > base && !landed.contains(&changed)
    })
  }

  /// Whether a symbolic link, a hard link or an IPC node stands at `path` at `version`.
  fn link_or_special_at(&self, path: &str, version: u64) -> bool {
    self.symlink_at(path, version).is_some()
      || self.hardlink_at(path, version).is_some()
      || self.special_at(path, version).is_some()
  }

  /// An unlink or a directory's removal of a node changed is delete against modify (`Green::merge_unlink`,
  /// `merge_rmdir`, A-643).
  fn upper_removals(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    for path in &r.unlinks {
      if self.unlink_meets(path, against) {
        windows.whole(path, MergeConflictClass::DeleteModify);
      }
    }
    for path in &r.rmdirs {
      if self.dir_at(path, against.at) && against.change(path).any() {
        windows.whole(path, MergeConflictClass::DeleteModify);
      }
    }
  }

  /// Whether an unlink of `path` meets a change, as `Green::merge_unlink` judges one against its base: a file or an IPC
  /// node changed in any dimension, a link whose node changed or that was removed or made.
  fn unlink_meets(&self, path: &str, against: &Against<'_>) -> bool {
    let (change, at) = (against.change(path), against.at);
    if self.special_at(path, at).is_some() || self.content_len_at(path, at).is_some() {
      return change.any();
    }
    if self.symlink_at(path, at).is_some() {
      return change.symlink || change.node;
    }
    self.hardlink_at(path, at).is_some() && (change.hardlink || change.node)
  }

  /// A mode, an attribute, a symbolic link and an IPC node, each as its `Green::merge_*` judges it against its base.
  fn upper_metadata(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    for (path, mode) in &r.modes {
      let change = against.change(path);
      let differs = change.mode && self.mode_at(path, against.at) != Some(*mode);
      let found = gone(r.establishes(path), change)
        .or_else(|| differs.then_some(MergeConflictClass::MetaMeta));
      if let Some(class) = found {
        windows.whole(path, class);
      }
    }
    self.upper_xattrs(r, against, windows);
    for (path, target) in &r.symlinks {
      let now = self.symlink_at(path, against.at);
      if against.change(path).symlink && now.as_deref() != Some(target.as_str()) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
    for (path, node) in &r.specials {
      if against.change(path).special && self.special_at(path, against.at) != Some(*node) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
  }

  /// Attributes set and removed (`Green::merge_set_xattr`, `merge_remove_xattr`).
  fn upper_xattrs(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    for (path, name, value) in &r.xattr_sets {
      let change = against.change(path);
      let now = self.xattr_at(path, name, against.at);
      let differs = change.xattrs.contains(name) && now.as_deref() != Some(value.as_slice());
      let found = gone(r.establishes(path), change)
        .or_else(|| differs.then_some(MergeConflictClass::MetaMeta));
      if let Some(class) = found {
        windows.whole(path, class);
      }
    }
    for (path, name) in &r.xattr_removes {
      let change = against.change(path);
      let differs = self.xattr_at(path, name, against.at).is_some() && change.xattrs.contains(name);
      let found = gone(r.establishes(path), change)
        .or_else(|| differs.then_some(MergeConflictClass::MetaMeta));
      if let Some(class) = found {
        windows.whole(path, class);
      }
    }
  }

  /// A hard link to a node changed is delete against modify, and a link another work set otherwise is create against
  /// create (`Green::merge_hardlink`, `node_home`).
  fn upper_links(&self, r: &Resolved, against: &Against<'_>, windows: &mut Windows) {
    for (path, target) in &r.hardlinks {
      // A link naming a node the work does not make is refused by the verdict that judges the restated work.
      let Some((node, made)) = r.link_node(target) else {
        continue;
      };
      if made.is_none() && self.link_node_meets(node, against) {
        windows.whole(path, MergeConflictClass::DeleteModify);
        continue;
      }
      let home = match made {
        Some(home) => home.to_owned(),
        None => self
          .hardlink_at(node, against.at)
          .unwrap_or_else(|| node.to_owned()),
      };
      let now = self.hardlink_at(path, against.at);
      if against.change(path).hardlink && now.as_deref() != Some(home.as_str()) {
        windows.whole(path, MergeConflictClass::CreateCreate);
      }
    }
  }

  /// Whether the node a hard link names at `node` changed, as `Green::node_home` judges it against its base.
  fn link_node_meets(&self, node: &str, against: &Against<'_>) -> bool {
    let (change, at) = (against.change(node), against.at);
    if self.hardlink_at(node, at).is_some() {
      return change.any();
    }
    (self.content_len_at(node, at).is_some() || self.special_at(node, at).is_some()) && change.node
  }

  /// The upper's operations with its content operations restated over `against.at` (module docs), in the document's
  /// order, those found identical left out.
  fn upper_content(
    &self,
    (work, r): (&Increment, &Resolved),
    against: &Against<'_>,
    windows: &mut Windows,
  ) -> Vec<Op> {
    let mut restated: BTreeMap<&str, std::vec::IntoIter<Option<Op>>> = BTreeMap::new();
    for (path, ops) in &r.content {
      // A file the upper makes holds its own bytes: nothing changed under them.
      if r.creates.contains(path) {
        continue;
      }
      let frame = r.renames.get(path).map_or(path.as_str(), String::as_str);
      let moved = self.restate_path((path, frame), ops, (work, against), windows);
      restated.insert(path.as_str(), moved.into_iter());
    }
    let names = &work.doc.paths;
    work
      .doc
      .ops
      .iter()
      .filter_map(|op| {
        let path = names.path(op.path).unwrap_or_default();
        match restated.get_mut(path) {
          Some(moved) if is_content(op.kind) => moved.next().flatten(),
          _ => Some(*op),
        }
      })
      .collect()
  }

  /// One path's content operations moved through the change at `frame`, the path they were declared against (module
  /// docs); `None` for one left out as identical.
  fn restate_path(
    &self,
    (path, frame): (&str, &str),
    ops: &[Op],
    (work, against): (&Increment, &Against<'_>),
    windows: &mut Windows,
  ) -> Vec<Option<Op>> {
    let change = against.change(frame);
    if change.node {
      windows.whole(path, MergeConflictClass::DeleteModify);
      return Vec::new();
    }
    if change.reshaped {
      for op in ops {
        windows.range(path, touched_range(op));
      }
      return Vec::new();
    }
    let edits: Vec<&[Op]> = change.edits.iter().map(Vec::as_slice).collect();
    let length = self
      .content_len_at(frame, against.at)
      .and_then(|len| u64::try_from(len).ok())
      .unwrap_or(0);
    let mut file = None;
    let mut moved = Vec::with_capacity(ops.len());
    for op in ops {
      match map_range(&edits, touched_range(op)) {
        Mapped::Shifted(range) => moved.push(Some(at_head(
          Op {
            at: range.start,
            ..*op
          },
          length,
        ))),
        Mapped::Overlaps => {
          let base = file.get_or_insert_with(|| self.content_ref_at(frame, against.at));
          if !identical(op, (&edits, base.as_deref()), &work.post_state) {
            windows.range(path, touched_range(op));
          }
          moved.push(None);
        }
      }
    }
    moved
  }
}

/// The class a metadata operation on a path not its own making meets when the path's node changed: delete against
/// modify for a node removed or replaced, a type change for an IPC node changed (`Green::merge_setmode`'s and the
/// attributes' checks).
fn gone(established: bool, change: &Change) -> Option<MergeConflictClass> {
  if established {
    return None;
  }
  if change.node {
    return Some(MergeConflictClass::DeleteModify);
  }
  change.special.then_some(MergeConflictClass::TypeChanged)
}

/// A node-exclusive operation's paths changed at all are a conflict (§4.17.4 item 3, A-542).
fn upper_node_exclusive(work: &Increment, against: &Against<'_>, windows: &mut Windows) {
  for op in work.doc.ops.iter().filter(|op| op.is_node_exclusive()) {
    for (index, class) in node_exclusive_paths(op) {
      let path = work.doc.paths.path(index).unwrap_or_default();
      if against.change(path).any() {
        windows.whole(path, class);
      }
    }
  }
}

/// Whether `op`, an overwrite meeting a change, writes exactly the bytes the new base holds where its start lands: the
/// verdict's identical case (§4.16 "The verdict", pass two), read in `base`, the new base's file.
fn identical(op: &Op, (edits, base): (&[&[Op]], Option<&[u8]>), post_state: &[u8]) -> bool {
  if op.kind != OpKind::Overwrite {
    return false;
  }
  let start = match map_range(edits, Range::new(op.at, 0)) {
    Mapped::Shifted(point) => point.start,
    Mapped::Overlaps => op.at,
  };
  let (Ok(start), Ok(len)) = (usize::try_from(start), usize::try_from(op.len)) else {
    return false;
  };
  let held = base.and_then(|base| base.get(start..start.checked_add(len)?));
  held == Some(span_bytes(post_state, op.src, op.len))
}
