//! The works below an upper (A-659; `super`, its module docs, "Restating an upper"): what each one declared, the span
//! of the green's versions it was mapped through and the version it landed as; and what changed at a path between the
//! topmost one's post-state and the version it landed as, read from the green's histories and deltas at the cost of
//! that path's own changes, and moved over each work on the way up ([`super::moved`]).
//!
//! **Up the stack, path by path.** A path in an upper's base names what its lower left there. A lower that made the
//! path (made a file, a directory or an IPC node there) holds its own content there, and only the mode and attributes
//! the green set meanwhile are changes it did not make. A lower that renamed a file to the path holds its source's
//! content there, which is followed from the source, and the path's own mode and attributes. A lower that removed it
//! leaves nothing an upper can change but by making it anew. Any other path the lower kept: what changed below it,
//! then what the green changed in the lower's span, moved over the lower's operations on it. A dimension the lower set
//! itself was accepted by the verdict as identical to the green's, so the upper's base holds it.
//!
//! **The guard.** Moving the changes over a lower also yields the operations the lower landed with, which the green
//! recorded when it committed the lower's version. Where they differ the restatement is refused
//! ([`RestateRefused::Diverged`]) rather than built on a guess; on a correct engine they never differ (the oracles run
//! it on every stack).

use std::collections::{BTreeMap, BTreeSet};

use super::RestateRefused;
use super::moved::{Change, move_edits};
use crate::engine::{Green, history_touches};
use crate::map::effect_of;
use crate::ops_doc::{Op, OpKind, OpsDoc};

/// What a work below an upper did to each path, as its ops document declares it.
#[derive(Debug, Default)]
struct Shape {
  /// The content operations on each path, in the document's order.
  content: BTreeMap<String, Vec<Op>>,
  /// The paths it made, each with the source it renamed there when it did.
  made: BTreeMap<String, Option<String>>,
  /// The paths it removed: unlinked, renamed away, or a directory removed.
  removed: BTreeSet<String>,
  /// The paths whose mode it set.
  modes: BTreeSet<String>,
  /// The paths and the attribute names it set or removed there.
  xattrs: BTreeSet<(String, String)>,
  /// The paths where it set a symbolic or hard link.
  links: BTreeSet<String>,
  /// The paths where it made an IPC node.
  specials: BTreeSet<String>,
  /// The destination of a copy it declares, which a restatement does not carry.
  copy: Option<String>,
}

impl Shape {
  /// The shape of the ops document `doc`.
  fn of(doc: &OpsDoc) -> Shape {
    let mut shape = Shape::default();
    for op in &doc.ops {
      shape.note(doc, op);
    }
    shape
  }

  /// Notes one operation of `doc`.
  fn note(&mut self, doc: &OpsDoc, op: &Op) {
    let name = |index: u64| doc.paths.path(index).unwrap_or_default().to_owned();
    let path = name(op.path);
    match op.kind {
      OpKind::Overwrite | OpKind::Extend | OpKind::Truncate | OpKind::Insert | OpKind::Delete => {
        self.content.entry(path).or_default().push(*op);
      }
      OpKind::Create | OpKind::Mkdir => {
        self.made.insert(path, None);
      }
      OpKind::Mknod => {
        self.made.insert(path.clone(), None);
        self.specials.insert(path);
      }
      OpKind::Rename => {
        let source = name(op.src);
        self.removed.insert(source.clone());
        self.made.insert(path, Some(source));
      }
      OpKind::Unlink | OpKind::Rmdir => {
        self.removed.insert(path);
      }
      OpKind::SetMode => {
        self.modes.insert(path);
      }
      OpKind::Symlink | OpKind::Link => {
        self.links.insert(path);
      }
      OpKind::SetXattr | OpKind::RemoveXattr => {
        self.xattrs.insert((path, name(op.at)));
      }
      OpKind::Copy => {
        self.copy.get_or_insert(path);
      }
    }
  }

  /// The content operations it declared on `path`.
  fn ops(&self, path: &str) -> &[Op] {
    self
      .content
      .get(path)
      .map(Vec::as_slice)
      .unwrap_or_default()
  }

  /// `change` less the dimensions this work set at `path` itself (module docs).
  fn absorb(&self, path: &str, mut change: Change) -> Change {
    if self.modes.contains(path) {
      change.mode = false;
    }
    if self.links.contains(path) {
      change.symlink = false;
      change.hardlink = false;
    }
    if self.specials.contains(path) {
      change.special = false;
    }
    change
      .xattrs
      .retain(|name| !self.xattrs.contains(&(path.to_owned(), name.clone())));
    change
  }
}

/// A work below an upper.
pub(super) struct Level {
  /// What it declared.
  shape: Shape,
  /// The green's versions the verdict mapped it through: after the version it was judged as based on, through the
  /// version before the one it landed as.
  span: (u64, u64),
  /// The version it landed as.
  landed: u64,
}

impl Level {
  /// The work of `doc`, judged as based on `base` and landed as `landed`; refused when it declares a copy.
  pub(super) fn of(doc: &OpsDoc, (base, landed): (u64, u64)) -> Result<Level, RestateRefused> {
    let shape = Shape::of(doc);
    if let Some(path) = &shape.copy {
      return Err(RestateRefused::Copy { path: path.clone() });
    }
    Ok(Level {
      shape,
      span: (base, landed.saturating_sub(1)),
      landed,
    })
  }

  /// The version it landed as.
  pub(super) fn landed(&self) -> u64 {
    self.landed
  }
}

impl Green {
  /// What changed at `path` between the post-state of the topmost of `levels` and the version it landed as, in the frame
  /// of that post-state (module docs, "Up the stack, path by path").
  pub(super) fn change_at(&self, levels: &[Level], path: &str) -> Result<Change, RestateRefused> {
    let Some((level, below)) = levels.split_last() else {
      return Ok(Change::default());
    };
    match level.shape.made.get(path) {
      Some(None) => {
        let change = self.changes_in(path, level.span)?.metadata_only();
        Ok(level.shape.absorb(path, change))
      }
      Some(Some(source)) => self.renamed_change((level, below), (source, path)),
      None if level.shape.removed.contains(path) => Ok(Change::default()),
      None => self.kept_change((level, below), path),
    }
  }

  /// What changed at a path the level kept: below it, then in its span, moved over its operations on the path.
  fn kept_change(
    &self,
    (level, below): (&Level, &[Level]),
    path: &str,
  ) -> Result<Change, RestateRefused> {
    let change = self
      .change_at(below, path)?
      .then(self.changes_in(path, level.span)?);
    let moved = self.moved_over(change, level.shape.ops(path), Some((path, level.landed)))?;
    Ok(level.shape.absorb(path, moved))
  }

  /// What changed at a path the level renamed `source` to: the source's content moved over the level's operations on the
  /// path, which a rename declares against the source's bytes (A-547), and the path's own other dimensions.
  fn renamed_change(
    &self,
    (level, below): (&Level, &[Level]),
    (source, path): (&str, &str),
  ) -> Result<Change, RestateRefused> {
    let content = self
      .change_at(below, source)?
      .then(self.changes_in(source, level.span)?);
    let own = self
      .change_at(below, path)?
      .then(self.changes_in(path, level.span)?);
    // The green records a renamed file whole, never its edits: there is no recorded landing to hold them to.
    let moved = self.moved_over(content, level.shape.ops(path), None)?;
    Ok(level.shape.absorb(path, moved.with_metadata_of(own)))
  }

  /// `change`'s deltas moved over a level's operations `ops` on its path ([`move_edits`]), and its landed operations held
  /// to what the green recorded at `recorded` (the path and the version it landed as) when given.
  fn moved_over(
    &self,
    mut change: Change,
    ops: &[Op],
    recorded: Option<(&str, u64)>,
  ) -> Result<Change, RestateRefused> {
    if ops.is_empty() || change.node || change.reshaped {
      return Ok(change);
    }
    let Some((edits, landed)) = move_edits(&change.edits, ops) else {
      change.edits.clear();
      change.reshaped = true;
      return Ok(change);
    };
    if let Some((path, version)) = recorded {
      self.check_landing(path, version, &landed)?;
    }
    change.edits = edits;
    Ok(change)
  }

  /// The guard (module docs): the operations a level landed with on `path`, as moving says, are the ones the green
  /// recorded for the path at `version`, by their effect and their bytes' source.
  fn check_landing(&self, path: &str, version: u64, landed: &[Op]) -> Result<(), RestateRefused> {
    let recorded = self.delta_at(version, path).unwrap_or_default();
    let same = recorded.len() == landed.len()
      && recorded
        .iter()
        .zip(landed)
        .all(|(green, moved)| effect_of(green) == effect_of(moved) && green.src == moved.src);
    if same {
      return Ok(());
    }
    Err(RestateRefused::Diverged {
      path: path.to_owned(),
      version,
    })
  }

  /// What the green changed at `path` in the versions `(from, to]` (module docs): refused when they reach below what its
  /// histories and deltas still hold.
  fn changes_in(&self, path: &str, (from, to): (u64, u64)) -> Result<Change, RestateRefused> {
    let mut change = Change::default();
    if to <= from {
      return Ok(change);
    }
    let floor = self.folded_below.max(self.deltas_from);
    if from < floor {
      return Err(RestateRefused::Folded { floor });
    }
    let span = (from, to);
    self.content_changes_in(path, span, &mut change);
    change.mode = touched(self.mode_history.get(path), span);
    change.symlink = touched(self.symlink_history.get(path), span);
    change.hardlink = touched(self.hardlink_history.get(path), span);
    change.special = touched(self.special_history.get(path), span);
    change.xattrs = self.xattr_names_changed(path, span);
    change.node = change.node || self.node_changed_in(path, span);
    Ok(change)
  }

  /// The content changes at `path` in `(from, to]`: each version's delta where the file was edited in place, the node
  /// changed where it was removed, made, or replaced (a rename onto it, or a delta that does not account for its change
  /// of length, which a replacement records as one insert of the new bytes).
  fn content_changes_in(&self, path: &str, (from, to): (u64, u64), change: &mut Change) {
    let Some(history) = self.content_history.get(path) else {
      return;
    };
    let first = history.partition_point(|(version, _)| *version <= from);
    let mut held = first
      .checked_sub(1)
      .and_then(|index| history.get(index))
      .and_then(|(_, value)| value.as_ref());
    for (version, value) in history.get(first..).unwrap_or_default() {
      if *version > to {
        break;
      }
      match (held, value) {
        (Some(was), Some(now)) => self.content_step(path, *version, (was.len(), now.len()), change),
        _ => change.node = true,
      }
      held = value.as_ref();
    }
  }

  /// One version's content change at `path`, the file `lengths` long before and after it.
  fn content_step(&self, path: &str, version: u64, lengths: (usize, usize), change: &mut Change) {
    let replaced = self.file_removed.get(path) == Some(&version);
    match self.delta_at(version, path) {
      Some(ops) if !replaced && accounts(ops, lengths) => change.edits.push(ops.to_vec()),
      _ => change.node = true,
    }
  }

  /// The delta the version `version` recorded at `path`.
  fn delta_at(&self, version: u64, path: &str) -> Option<&[Op]> {
    let index = version.checked_sub(self.deltas_from)?.checked_sub(1)?;
    self
      .deltas
      .get(usize::try_from(index).ok()?)?
      .get(path)
      .map(Vec::as_slice)
  }

  /// The attributes of `path` whose values changed in `span`.
  fn xattr_names_changed(&self, path: &str, span: (u64, u64)) -> BTreeSet<String> {
    self
      .xattr_history
      .range((path.to_owned(), String::new())..)
      .take_while(|((key, _), _)| key == path)
      .filter(|(_, history)| touched(Some(*history), span))
      .map(|((_, name), _)| name.clone())
      .collect()
  }

  /// Whether a node of any type at `path` was removed, made or replaced in `(from, to]`: a removal the green recorded
  /// there, a link or an IPC node removed or made, a directory made or removed, or a copy over it.
  fn node_changed_in(&self, path: &str, (from, to): (u64, u64)) -> bool {
    let span = (from, to);
    self
      .file_removed
      .get(path)
      .is_some_and(|version| from < *version && *version <= to)
      || removed_or_made(self.symlink_history.get(path), span)
      || removed_or_made(self.hardlink_history.get(path), span)
      || removed_or_made(self.special_history.get(path), span)
      || touched(self.dir_history.get(path), span)
      || self
        .copy_above(path)
        .is_some_and(|(_, record)| record.version > from)
  }
}

/// Whether a `(version, value)` history holds an entry in `(from, to]`.
fn touched<T>(history: Option<&Vec<(u64, T)>>, (from, to): (u64, u64)) -> bool {
  history.is_some_and(|history| history_touches(history, from, to))
}

/// Whether a `(version, value)` history holds, in `(from, to]`, a removal (no value) or an appearance (a value where none
/// stood: a key that held one before its first change records it at version 0, the one history rule, A-534).
fn removed_or_made<T>(history: Option<&Vec<(u64, Option<T>)>>, (from, to): (u64, u64)) -> bool {
  let Some(history) = history else {
    return false;
  };
  let first = history.partition_point(|(version, _)| *version <= from);
  let mut held = first
    .checked_sub(1)
    .and_then(|index| history.get(index))
    .is_some_and(|(_, value)| value.is_some());
  for (version, value) in history.get(first..).unwrap_or_default() {
    if *version > to {
      break;
    }
    if value.is_none() || !held {
      return true;
    }
    held = true;
  }
  false
}

/// Whether `ops` account for a file's change of length from `before` to `after` bytes: an edit's delta does, and a
/// replacement's one insert of the new bytes does not (A-596's reading of a delta).
fn accounts(ops: &[Op], (before, after): (usize, usize)) -> bool {
  let declared = ops.iter().try_fold(0i128, |sum, op| {
    let (_, old, new) = effect_of(op)?;
    sum.checked_add(i128::from(new).checked_sub(i128::from(old))?)
  });
  let measured = i128::try_from(after)
    .ok()
    .zip(i128::try_from(before).ok())
    .and_then(|(after, before)| after.checked_sub(before));
  declared.is_some() && declared == measured
}
