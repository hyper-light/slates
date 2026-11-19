//! A verdict computed anywhere and committed by position (A-658; A-645 "Merges computed anywhere, committed by
//! position"; §4.16 "Position mapping", "The verdict" and "Commit"; A-635's rule).
//!
//! **Units.** A verdict is reached unit by unit. A unit is one run of the verdict that reads the green only at its own
//! paths, its footprint: a special node's path; a rename's two ends; a created, removed or edited file; a directory made,
//! or removed together with what stands beneath it; a mode, a symbolic link or an attribute; a hard link and the node it
//! names; a node-exclusive path; a path the work writes and, when the work hands a node on to a link there with its
//! writes, the home the node passes from (A-669). A removal, or a replacement by a rename, reads the hard links naming
//! its file too, since a link made after the work's base refuses it (A-670). The units' effects and windows, taken in
//! order, are the whole verdict's, pushed in the order a whole verdict pushes them, and which units there are is the
//! work's alone, never the head's, so the units reached at one head pair one to one with those reached at another. Two
//! units are not footprints of paths. One holds what the work and its base decide (an op no verdict can apply, an edit
//! that does not fit what it edits), which the head turns in one place only: an edit declared at a link edits the file
//! whose node the work hands on to the link, so this unit reads the links naming the files the work removes or replaces.
//! The other is a work's copies, judged together. A content edit's unit also keeps where each of its ops stood at the
//! head it was judged at ([`Placed`]).
//!
//! **Committed by position.** The owner takes a [`Computed`](super::Computed) made anywhere
//! ([`Green::judge_computed`](super::Green::judge_computed)) and answers it as a submit does: an increment accepted
//! before its version, a conflict its cache holds its windows, a base below its checkpoint unknown. At the position the
//! verdict was computed against, every unit stands as computed. When the green has moved, the change index names every
//! path changed since (O(the changes since), A-264); a unit whose footprint meets none of them stands as computed, and
//! only the units they meet are judged again ([`Units`]). A content edit is judged again from where its ops stood:
//! each mapped on through the deltas since alone, which equals mapping it from the work's base because mapping composes
//! (`map(a..c) = map(b..c) ∘ map(a..b)`, §4.16 "Position mapping"). A work's copies read the head's version and the
//! green's fold floor (`Green::copy_fix`), so they are judged again whenever the green moved or the owner's floor is not
//! the one they were computed over. The hottest green's merges then cost its owner the changes since each work's
//! position and the appends, not the verdicts.
//!
//! **Why it is exact.** A unit's verdict is a function of the work and of the green's state at its footprint over the
//! versions from the work's base to the head: the values there at the head, their last changes, the copies above them,
//! the deltas on them, and what stood there at the base. Every commit records each path its effects change, its node
//! moves included (`Green::record_changes`), so a footprint that meets no path changed since read the same at both
//! heads, and its effects (a file's ops and the head's file the commit makes it over, a link's home) are the same too.
//! A copy made since changes the last change of every path in its tree without naming each, so a footprint in the tree
//! of a copy made since is met as well. What only the copies' verdict reads (the reverse map of hard links, the head's
//! version, the fold floor) is covered by judging the copies again. A hard link's change (made, removed, or turned to
//! another file, a node move's included) is recorded at the link's own name and never at the file it names, so what
//! changed since also names, for each link changed, the file it named at the earlier head and the one it names at the
//! later ([`Since`]): the links naming a file read the same at both heads unless one of them changed. No copy changes
//! them: a copy never brings a hard link, and never lands over one or over a file one names (its verdict refuses each,
//! `copy`'s module docs), so no link has a copy made since above it.
//!
//! **The oracle** (`compute_tests.rs`): generated concurrent histories on a green that judges every work serially at its
//! head and a green that commits by position the verdicts a lagging copy computed, both held equal at every step.

use std::collections::BTreeSet;

use super::copy::in_tree;
use super::{ConflictWindow, Effect};
use crate::map::{Mapped, map_range};
use crate::ops_doc::Op;
use crate::range::Range;

/// Where one of a work's content ops stood at the head a verdict was judged at (module docs): its range and its first
/// byte, each mapped from the work's base through the path's deltas since ([`crate::map`]). The verdict reads the
/// range to accept or meet a change, and the first byte to find where an overwrite that met a change stands at the head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placed {
  range: Mapped,
  point: Mapped,
}

impl Placed {
  /// `op` mapped from the work's base through `deltas`, the ops each version since the base applied to its path, oldest
  /// first.
  pub(super) fn of(op: &Op, deltas: &[&[Op]]) -> Placed {
    Placed {
      range: map_range(deltas, super::touched_range(op)),
      point: map_range(deltas, Range::new(op.at, 0)),
    }
  }

  /// This placement mapped on through `deltas`, the path's deltas after the head it was placed at, oldest first.
  pub(super) fn advanced(self, deltas: &[&[Op]]) -> Placed {
    Placed {
      range: advance(self.range, deltas),
      point: advance(self.point, deltas),
    }
  }

  /// The op's range at the head, or that it met a change since the base.
  pub(super) fn range(self) -> Mapped {
    self.range
  }

  /// Where the op's first byte stands at the head; its offset at the base when the point met a change itself (an insert
  /// made at that very offset), as the verdict has always read it.
  pub(super) fn head_start(self, op: &Op) -> u64 {
    match self.point {
      Mapped::Shifted(head) => head.start,
      Mapped::Overlaps => op.at,
    }
  }
}

/// `mapped` carried on through `deltas`: a range that met a change has met it whatever follows.
fn advance(mapped: Mapped, deltas: &[&[Op]]) -> Mapped {
  match mapped {
    Mapped::Shifted(range) => map_range(deltas, range),
    Mapped::Overlaps => Mapped::Overlaps,
  }
}

/// One unit of a verdict (module docs): what one run of the verdict decided, in the order the whole verdict pushes it.
#[derive(Default)]
pub(super) struct Unit {
  /// The accepted effects.
  pub(super) effects: Vec<Effect>,
  /// The conflict windows.
  pub(super) windows: Vec<ConflictWindow>,
  /// A content edit's ops, where each stood at the head the unit was judged at; empty for every other unit.
  pub(super) placed: Vec<Placed>,
}

impl Unit {
  /// The unit of one verdict function's answer: an effect, nothing, or a window.
  pub(super) fn of(answer: Result<Option<Effect>, ConflictWindow>) -> Unit {
    let mut unit = Unit::default();
    super::record(&mut unit.effects, &mut unit.windows, answer);
    unit
  }

  /// The unit of a content edit's answer: an effect, nothing, or its windows.
  pub(super) fn content(answer: Result<Option<Effect>, Vec<ConflictWindow>>) -> Unit {
    match answer {
      Ok(effect) => Unit {
        effects: effect.into_iter().collect(),
        ..Unit::default()
      },
      Err(windows) => Unit::windows(windows),
    }
  }

  /// A unit of windows alone.
  pub(super) fn windows(windows: Vec<ConflictWindow>) -> Unit {
    Unit {
      windows,
      ..Unit::default()
    }
  }
}

/// The paths a unit's verdict reads (module docs): at most two paths, each read at itself; a directory whose children
/// are read; and files whose naming hard links are read.
#[derive(Clone, Copy)]
pub(super) struct Footprint<'a> {
  paths: [Option<&'a str>; 2],
  beneath: Option<&'a str>,
  /// The files whose naming hard links the unit reads (A-669, A-670): which links name each at the head, and when each
  /// last changed.
  linked: &'a [&'a str],
}

impl<'a> Footprint<'a> {
  /// One path.
  pub(super) fn path(path: &'a str) -> Footprint<'a> {
    Footprint {
      paths: [Some(path), None],
      beneath: None,
      linked: &[],
    }
  }

  /// Two paths: a rename's ends, a hard link and the node it names, a link a node passes to and its home.
  pub(super) fn pair(first: &'a str, second: &'a str) -> Footprint<'a> {
    Footprint {
      paths: [Some(first), Some(second)],
      beneath: None,
      linked: &[],
    }
  }

  /// A directory and what stands beneath it: a directory's removal reads whether anything is left there.
  pub(super) fn directory(path: &'a str) -> Footprint<'a> {
    Footprint {
      paths: [Some(path), None],
      beneath: Some(path),
      linked: &[],
    }
  }

  /// The hard links naming each of `files`, and no path of its own: whether an edit declared at a link fits the file
  /// whose node the work hands on to the link (A-669).
  pub(super) fn links_naming(files: &'a [&'a str]) -> Footprint<'a> {
    Footprint {
      paths: [None, None],
      beneath: None,
      linked: files,
    }
  }

  /// This footprint and the hard links naming each of `files`: a removal, or a replacement by a rename, is refused while
  /// a link made after the work's base names its file (A-670).
  pub(super) fn and_links_naming(self, files: &'a [&'a str]) -> Footprint<'a> {
    Footprint {
      linked: files,
      ..self
    }
  }
}

/// What changed between the head a verdict was reached at and the head it is reached at again (module docs).
pub(super) struct Since {
  /// The head the verdict was reached at.
  at: u64,
  /// Every path the change index names in `(at, head]`.
  changed: BTreeSet<String>,
  /// The destinations of the copies made in `(at, head]`, each standing for its tree.
  copied: Vec<String>,
  /// The files whose naming hard links changed in `(at, head]`: the file each link changed there named at `at`, and the
  /// one it names at the head. A link's change is recorded at the link's own name, never at its file's.
  relinked: BTreeSet<String>,
  /// Whether a work's copies are judged again: the green moved, or its fold floor is not the one they were judged over.
  copies: bool,
}

impl Since {
  /// What changed since `at`: the paths the change index names, the copies made among them, the files whose naming
  /// links changed, and whether copies are judged again.
  pub(super) fn new(
    at: u64,
    (changed, copied): (BTreeSet<String>, Vec<String>),
    relinked: BTreeSet<String>,
    copies: bool,
  ) -> Since {
    Since {
      at,
      changed,
      copied,
      relinked,
      copies,
    }
  }

  /// Whether nothing any unit reads changed: every unit stands as it was reached.
  pub(super) fn is_nothing(&self) -> bool {
    self.changed.is_empty() && !self.copies
  }

  /// Whether `footprint` meets anything changed since.
  fn meets(&self, footprint: &Footprint<'_>) -> bool {
    footprint
      .paths
      .iter()
      .flatten()
      .any(|path| self.changed.contains(*path) || self.copied_above(path))
      || footprint
        .beneath
        .is_some_and(|dir| self.changed_beneath(dir) || self.copied_above(dir))
      || footprint
        .linked
        .iter()
        .any(|file| self.relinked.contains(*file))
  }

  /// Whether a copy made since holds `path` in its tree.
  fn copied_above(&self, path: &str) -> bool {
    self.copied.iter().any(|dst| in_tree(path, dst))
  }

  /// Whether a path beneath the directory `dir` changed since.
  fn changed_beneath(&self, dir: &str) -> bool {
    let prefix = format!("{dir}/");
    self
      .changed
      .range(prefix.clone()..)
      .next()
      .is_some_and(|path| path.starts_with(&prefix))
  }
}

/// A verdict being reached, unit by unit (module docs): at a head for the first time, or again at a later head from the
/// units reached at an earlier one, taking each unit nothing it reads changed since and judging the rest again.
pub(super) struct Units {
  /// The units reached, in order.
  done: Vec<Unit>,
  /// The units reached at the earlier head, in order, and what changed since: none on a first judgment.
  prior: Option<(std::vec::IntoIter<Unit>, Since)>,
  /// The units judged again at the later head.
  again: u64,
}

impl Units {
  /// A verdict reached for the first time.
  pub(super) fn first() -> Units {
    Units {
      done: Vec::new(),
      prior: None,
      again: 0,
    }
  }

  /// A verdict reached again from `units`, reached at `since`'s head for the same work.
  pub(super) fn again(units: Vec<Unit>, since: Since) -> Units {
    Units {
      done: Vec::with_capacity(units.len()),
      prior: Some((units.into_iter(), since)),
      again: 0,
    }
  }

  /// A work's copies: judged again whenever the green moved or its floor differs (module docs).
  pub(super) fn copies(&mut self, judge: impl FnOnce() -> Unit) {
    let unit = match self.prior.as_mut() {
      None => judge(),
      Some((units, since)) => match units.next() {
        Some(unit) if !since.copies => unit,
        _ => {
          self.again = self.again.saturating_add(1);
          judge()
        }
      },
    };
    self.done.push(unit);
  }

  /// A unit whose verdict reads `footprint`: taken as reached when nothing it reads changed since, else judged
  /// (`judge`, handed the earlier head and where a content edit's ops stood there when it is judged again).
  pub(super) fn unit(
    &mut self,
    footprint: Footprint<'_>,
    judge: impl FnOnce(Option<(u64, Vec<Placed>)>) -> Unit,
  ) {
    let unit = match self.prior.as_mut() {
      None => judge(None),
      Some((units, since)) => match units.next() {
        Some(unit) if !since.meets(&footprint) => unit,
        reached => {
          self.again = self.again.saturating_add(1);
          judge(reached.map(|unit| (since.at, unit.placed)))
        }
      },
    };
    self.done.push(unit);
  }

  /// The units reached, in order, and how many were judged again.
  pub(super) fn into_units(self) -> (Vec<Unit>, u64) {
    (self.done, self.again)
  }
}

/// The whole verdict of `units`: every effect, in order, when no unit holds a window, else every window, in order.
pub(super) fn verdict(units: Vec<Unit>) -> Result<Vec<Effect>, Vec<ConflictWindow>> {
  let mut effects = Vec::new();
  let mut windows = Vec::new();
  for unit in units {
    effects.extend(unit.effects);
    windows.extend(unit.windows);
  }
  if windows.is_empty() {
    Ok(effects)
  } else {
    Err(windows)
  }
}

/// Why the owner takes no verdict computed elsewhere ([`Green::judge_computed`](super::Green::judge_computed), A-658),
/// typed. Each is a caller's error, never a verdict: nothing is judged and nothing applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionRefused {
  /// The verdict was computed for another increment, or for the same one on another base.
  OtherWork,
  /// The verdict was computed at a head past this green's: no copy of a green is ahead of its owner.
  Ahead {
    /// The head it was computed against.
    at: u64,
    /// This green's head.
    head: u64,
  },
  /// The copy that computed it had accepted the increment as `version`, which this green's record does not hold: the
  /// copy was not of this green.
  NotRecorded {
    /// The version the copy had accepted it as.
    version: u64,
  },
}
