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
  /// Where the op stood: its range and its first byte, each as mapped (a computed verdict's wire form writes them,
  /// `computed_wire`, A-658 part 3a).
  pub(super) fn parts(&self) -> (Mapped, Mapped) {
    (self.range, self.point)
  }

  /// A placement read back from its parts ([`Placed::parts`]).
  pub(super) fn from_parts(range: Mapped, point: Mapped) -> Placed {
    Placed { range, point }
  }

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

/// Where one unit's parts end in its verdict's flat lists ([`Verdict`]).
#[derive(Clone, Copy, Debug, Default)]
struct Ends {
  effects: usize,
  windows: usize,
  placed: usize,
}

/// A verdict's units, flat (module docs; A-920 3l): every unit's accepted effects, conflict windows and placements (a
/// content edit's ops, where each stood at the head it was judged at) in three lists, in the order the whole verdict
/// pushes them, and where each unit's parts end in them. A unit allocates nothing of its own: one list each had been
/// freed a unit at a time as a held verdict was gathered, and the free that emptied the allocator's region cost one
/// step 1.2 ms (debug probe, 18,001 units of 0 or 1 effect). The whole verdict is the lists taken whole.
#[derive(Default)]
pub(super) struct Verdict {
  /// Every unit's accepted effects, in order.
  pub(super) effects: Vec<Effect>,
  /// Every unit's conflict windows, in order.
  pub(super) windows: Vec<ConflictWindow>,
  /// Every content edit's placements, in order.
  pub(super) placed: Vec<Placed>,
  /// Where each unit's parts end.
  ends: Vec<Ends>,
}

/// One unit of a verdict, read in place ([`Verdict::units`]).
pub(super) struct UnitRef<'v> {
  /// Its accepted effects.
  pub(super) effects: &'v [Effect],
  /// Its conflict windows.
  pub(super) windows: &'v [ConflictWindow],
  /// A content edit's placements; empty for every other unit.
  pub(super) placed: &'v [Placed],
}

/// What a unit's verdict writes into ([`Units::unit`]): its verdict's flat lists, appended to and never read.
pub(super) struct UnitSink<'v> {
  /// The accepted effects.
  pub(super) effects: &'v mut Vec<Effect>,
  /// The conflict windows.
  pub(super) windows: &'v mut Vec<ConflictWindow>,
  /// A content edit's placements.
  pub(super) placed: &'v mut Vec<Placed>,
  /// The effect lists a verdict function answered of its own ([`UnitSink::effect_list`]).
  lists: &'v mut u64,
}

impl UnitSink<'_> {
  /// One verdict function's answer: an effect, nothing, or a window.
  pub(super) fn of(&mut self, answer: Result<Option<Effect>, ConflictWindow>) {
    super::record(self.effects, self.windows, answer);
  }

  /// A content edit's answer: an effect, nothing, or its windows.
  pub(super) fn content(&mut self, answer: Result<Option<Effect>, Vec<ConflictWindow>>) {
    match answer {
      Ok(effect) => self.effects.extend(effect),
      Err(windows) => self.windows.extend(windows),
    }
  }

  /// Windows alone.
  pub(super) fn windows(&mut self, windows: impl IntoIterator<Item = ConflictWindow>) {
    self.windows.extend(windows);
  }

  /// Effects a verdict function answered as a list of its own (a node's writes handed on to a link): appended, and the
  /// list counted where it allocated (`Green::unit_effect_lists`).
  pub(super) fn effect_list(&mut self, effects: Vec<Effect>) {
    if effects.capacity() > 0 {
      *self.lists = self.lists.saturating_add(1);
    }
    self.effects.extend(effects);
  }
}

impl Verdict {
  /// A verdict whose lists hold `(effects, windows, placed, units)` without growing.
  fn with_capacity((effects, windows, placed, units): (usize, usize, usize, usize)) -> Verdict {
    Verdict {
      effects: Vec::with_capacity(effects),
      windows: Vec::with_capacity(windows),
      placed: Vec::with_capacity(placed),
      ends: Vec::with_capacity(units),
    }
  }

  /// The unit being written ends here.
  fn end_unit(&mut self) {
    self.ends.push(Ends {
      effects: self.effects.len(),
      windows: self.windows.len(),
      placed: self.placed.len(),
    });
  }

  /// A unit read back ([`super::computed_wire`]): its parts appended, the unit ended.
  pub(super) fn push_unit(
    &mut self,
    (effects, windows, placed): (
      impl IntoIterator<Item = Effect>,
      &[ConflictWindow],
      &[Placed],
    ),
  ) {
    self.effects.extend(effects);
    self.windows.extend_from_slice(windows);
    self.placed.extend_from_slice(placed);
    self.end_unit();
  }

  /// How many units it holds.
  pub(super) fn len(&self) -> usize {
    self.ends.len()
  }

  /// Its units, in order, each read in place.
  pub(super) fn units(&self) -> impl Iterator<Item = UnitRef<'_>> {
    let mut last = Ends::default();
    self.ends.iter().map(move |end| {
      let unit = UnitRef {
        effects: self.effects.get(last.effects..end.effects).unwrap_or(&[]),
        windows: self.windows.get(last.windows..end.windows).unwrap_or(&[]),
        placed: self.placed.get(last.placed..end.placed).unwrap_or(&[]),
      };
      last = *end;
      unit
    })
  }
}

/// The units reached at an earlier head, taken in order as the later head's are reached ([`Units::again`]): each unit
/// that stands moves its parts across, and each judged again drops its effects and windows and hands its placements on.
struct Prior {
  effects: std::vec::IntoIter<Effect>,
  windows: std::vec::IntoIter<ConflictWindow>,
  placed: Vec<Placed>,
  ends: std::vec::IntoIter<Ends>,
  /// Where the last unit taken ended.
  last: Ends,
  since: Since,
}

impl Prior {
  /// The next unit's span: where it begins and where it ends.
  fn next(&mut self) -> Option<(Ends, Ends)> {
    let end = self.ends.next()?;
    let from = core::mem::replace(&mut self.last, end);
    Some((from, end))
  }

  /// The unit spanning `(from, to)`, standing as reached: its parts moved into `done`.
  fn stand(&mut self, (from, to): (Ends, Ends), done: &mut Verdict) {
    let effects = to.effects.saturating_sub(from.effects);
    done.effects.extend(self.effects.by_ref().take(effects));
    let windows = to.windows.saturating_sub(from.windows);
    done.windows.extend(self.windows.by_ref().take(windows));
    done
      .placed
      .extend_from_slice(self.placed.get(from.placed..to.placed).unwrap_or(&[]));
  }

  /// The unit spanning `(from, to)`, judged again: its effects and windows dropped, its placements answered.
  fn judged_again(&mut self, (from, to): (Ends, Ends)) -> &[Placed] {
    let effects = to.effects.saturating_sub(from.effects);
    self.effects.by_ref().take(effects).for_each(drop);
    let windows = to.windows.saturating_sub(from.windows);
    self.windows.by_ref().take(windows).for_each(drop);
    self.placed.get(from.placed..to.placed).unwrap_or(&[])
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
  done: Verdict,
  /// The units reached at the earlier head, in order, and what changed since: none on a first judgment.
  prior: Option<Prior>,
  /// The units judged again at the later head.
  again: u64,
  /// The effect lists the units judged answered of their own (A-920 3l's non-vacuity counter, [`UnitSink::effect_list`]).
  effect_lists: u64,
}

impl Units {
  /// A verdict reached for the first time.
  pub(super) fn first() -> Units {
    Units {
      done: Verdict::default(),
      prior: None,
      again: 0,
      effect_lists: 0,
    }
  }

  /// A verdict reached again from `units`, reached at `since`'s head for the same work.
  pub(super) fn again(units: Verdict, since: Since) -> Units {
    let Verdict {
      effects,
      windows,
      placed,
      ends,
    } = units;
    let done = Verdict::with_capacity((effects.len(), windows.len(), placed.len(), ends.len()));
    Units {
      done,
      prior: Some(Prior {
        effects: effects.into_iter(),
        windows: windows.into_iter(),
        placed,
        ends: ends.into_iter(),
        last: Ends::default(),
        since,
      }),
      again: 0,
      effect_lists: 0,
    }
  }

  /// A work's copies: judged again whenever the green moved or its floor differs (module docs).
  pub(super) fn copies(&mut self, judge: impl FnOnce(&mut UnitSink<'_>)) {
    let Units {
      done,
      prior,
      again,
      effect_lists,
    } = self;
    let reached = prior
      .as_mut()
      .map(|prior| (prior.next(), prior.since.copies));
    match (prior.as_mut(), reached) {
      (Some(prior), Some((Some(span), false))) => prior.stand(span, done),
      (prior, reached) => {
        if let (Some(prior), Some((Some(span), _))) = (prior, reached) {
          prior.judged_again(span);
        }
        if reached.is_some() {
          *again = again.saturating_add(1);
        }
        judge(&mut sink(done, effect_lists));
      }
    }
    done.end_unit();
  }

  /// Whether the next unit, whose verdict reads `footprint`, is judged ([`Units::unit`]): on a first judgment, where no
  /// unit was reached at the earlier head, or where something it reads changed since. A unit judged over more than one
  /// step asks it at its first (A-920 3j, `Green::decide_first`), so a unit taken as reached costs nothing.
  pub(super) fn judges(&self, footprint: &Footprint<'_>) -> bool {
    match self.prior.as_ref() {
      None => true,
      Some(prior) => prior.ends.as_slice().is_empty() || prior.since.meets(footprint),
    }
  }

  /// A unit whose verdict reads `footprint`: taken as reached when nothing it reads changed since, else judged
  /// (`judge`, handed the earlier head and where a content edit's ops stood there when it is judged again), writing into
  /// the verdict's lists.
  pub(super) fn unit(
    &mut self,
    footprint: Footprint<'_>,
    judge: impl FnOnce(Option<(u64, &[Placed])>, &mut UnitSink<'_>),
  ) {
    let Units {
      done,
      prior,
      again,
      effect_lists,
    } = self;
    match prior.as_mut() {
      None => judge(None, &mut sink(done, effect_lists)),
      Some(prior) => match prior.next() {
        Some(span) if !prior.since.meets(&footprint) => prior.stand(span, done),
        reached => {
          *again = again.saturating_add(1);
          let at = prior.since.at;
          let placed = reached.map(|span| (at, prior.judged_again(span)));
          judge(placed, &mut sink(done, effect_lists));
        }
      },
    }
    done.end_unit();
  }

  /// The effect lists the units judged so far answered of their own.
  pub(super) fn effect_lists(&self) -> u64 {
    self.effect_lists
  }

  /// The units reached, in order, and how many were judged again.
  pub(super) fn into_units(self) -> (Verdict, u64) {
    (self.done, self.again)
  }
}

/// What a unit writes into: `done`'s lists, its own effect lists counted in `lists`.
fn sink<'v>(done: &'v mut Verdict, lists: &'v mut u64) -> UnitSink<'v> {
  UnitSink {
    effects: &mut done.effects,
    windows: &mut done.windows,
    placed: &mut done.placed,
    lists,
  }
}

/// The whole verdict of `units`: every effect, in order, when no unit holds a window, else every window, in order.
pub(super) fn verdict(units: Verdict) -> Result<Vec<Effect>, Vec<ConflictWindow>> {
  if units.windows.is_empty() {
    Ok(units.effects)
  } else {
    Err(units.windows)
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
