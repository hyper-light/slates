//! A version's node moves found a unit at a time (A-646's rule, [`Green::node_moves`]; A-920, main's ruling of
//! 2026-11-27 02:03).
//!
//! **Why.** A node moves only to a link that survives its home: when the version removes or replaces a home some link
//! still names, the least surviving link takes the node, and when it renames one, its links name the new home. Found
//! whole in the commit beginning's one unit, the walk gathered every effect of the version into sets of borrowed paths
//! before it looked at a link: a move of 9,000 files walked its 18,000 effects in 7.99 ms of one unit (debug). Since
//! then a green with no link, whose version makes none, skips the walk; a green holding one link anywhere still walked
//! every effect in that one unit, which §3 forbids whatever a probe finds.
//!
//! **What.** A [`NodeMoves`] takes the walk an effect a unit and keeps only what a link can touch, in owned paths, so it
//! lives across the steps of the commit's beginning:
//! - **Links:** the version's own link effects, each link's last target (none for a removal).
//! - **Walk:** each removal, rename and replacement whose path some link survives: a home no link survives moves no node,
//!   renamed or not, so it is passed over.
//! - **Pairs:** each kept rename whose source the version removes, its surviving links named at its new path.
//! - **Handing:** each kept home the version removes or replaces and does not rename, handed to its least surviving link
//!   ([`Green::hand_over`]), in path order.
//!
//! The moves are those the whole walk found, in its order: the kept sets are the whole walk's restricted to the paths a
//! link survives, and nothing outside them made a move. [`Green::node_moves`] is this run to its end.

use std::collections::{BTreeMap, BTreeSet};

use super::{ContentSource, Effect, Green};

/// A version's node moves being found, a unit a step ([`NodeMoves::step`]).
#[derive(Default)]
pub(super) struct NodeMoves {
  /// Where the walk is.
  phase: Phase,
  /// The next effect, or the next kept rename.
  at: usize,
  /// The version's links: each link's path and its last target, none for a removal.
  links: BTreeMap<String, Option<String>>,
  /// The paths the version's links target, any of their targets: a path one of them may survive.
  targets: BTreeSet<String>,
  /// The removed homes some link may survive.
  removed: BTreeSet<String>,
  /// The removed or replaced homes some link may survive, in path order: the hand-overs' candidates.
  left: BTreeSet<String>,
  /// The renames whose source some link may survive, in the version's order: each's source and its new path.
  pairs: Vec<(String, String)>,
  /// The removed homes renamed: their links follow them, nothing handed over.
  renamed: BTreeSet<String>,
  /// The moves found.
  moves: Vec<Effect>,
}

/// Where a [`NodeMoves`] is.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
  /// The version's links gathered, an effect a unit.
  #[default]
  Links,
  /// The removals, renames and replacements a link can touch kept, an effect a unit.
  Walk,
  /// The kept renames, a rename a unit.
  Pairs,
  /// The kept homes handed over, a home a unit.
  Handing,
  /// Found.
  Done,
}

impl NodeMoves {
  /// One unit of the walk over `effects`, the version's, on `green`'s head: whether the moves are all found.
  pub(super) fn step(&mut self, green: &Green, effects: &[Effect]) -> bool {
    match self.phase {
      Phase::Links => self.link_one(effects),
      Phase::Walk => self.walk_one(green, effects),
      Phase::Pairs => self.pair_one(green),
      Phase::Handing => self.hand_one(green),
      Phase::Done => {}
    }
    self.phase == Phase::Done
  }

  /// Whether the next unit visits one of the version's effects ([`Green::node_moves_walked`]'s count).
  pub(super) fn walking(&self) -> bool {
    matches!(self.phase, Phase::Links | Phase::Walk)
  }

  /// The moves found.
  pub(super) fn into_moves(self) -> Vec<Effect> {
    self.moves
  }

  /// [`Phase::Links`]: the next effect's link kept, if it makes or removes one.
  fn link_one(&mut self, effects: &[Effect]) {
    let Some(effect) = effects.get(self.at) else {
      self.phase = Phase::Walk;
      self.at = 0;
      return;
    };
    match effect {
      Effect::Hardlink(path, target) => {
        self.links.insert(path.clone(), Some(target.clone()));
        self.targets.insert(target.clone());
      }
      Effect::RemoveHardlink(path) => {
        self.links.insert(path.clone(), None);
      }
      _ => {}
    }
    self.at = self.at.saturating_add(1);
  }

  /// [`Phase::Walk`]: the next effect kept where it removes, renames or replaces a home some link may survive.
  fn walk_one(&mut self, green: &Green, effects: &[Effect]) {
    let Some(effect) = effects.get(self.at) else {
      self.phase = Phase::Pairs;
      self.at = 0;
      return;
    };
    match effect {
      Effect::RemoveContent(path) | Effect::RemoveSpecial(path) => {
        if self.linked(green, path) {
          self.removed.insert(path.clone());
          self.left.insert(path.clone());
        }
      }
      Effect::SetContent(to, _, _, ContentSource::Copied(from, _)) if from != to => {
        if self.linked(green, from) {
          self.pairs.push((from.clone(), to.clone()));
        }
        if self.linked(green, to) && green.content.contains_key(to) {
          self.left.insert(to.clone());
        }
      }
      _ => {}
    }
    self.at = self.at.saturating_add(1);
  }

  /// [`Phase::Pairs`]: the next kept rename's links named at its new path, where the version removes its source.
  fn pair_one(&mut self, green: &Green) {
    let Some((from, to)) = self.pairs.get(self.at).cloned() else {
      self.phase = Phase::Handing;
      return;
    };
    if self.removed.contains(&from) {
      for link in surviving_links(green, &from, &self.links) {
        self
          .moves
          .push(Effect::Hardlink(link.to_owned(), to.clone()));
      }
      self.renamed.insert(from);
    }
    self.at = self.at.saturating_add(1);
  }

  /// [`Phase::Handing`]: the least kept home handed over to its least surviving link, unless it was renamed.
  fn hand_one(&mut self, green: &Green) {
    let Some(home) = self.left.pop_first() else {
      self.phase = Phase::Done;
      return;
    };
    if !self.renamed.contains(&home) {
      green.hand_over(
        &home,
        surviving_links(green, &home, &self.links),
        &mut self.moves,
      );
    }
  }

  /// Whether some link may survive `path` as a home: a link of the head's to it the version keeps, or one the version
  /// makes to it (any target it gave one; the hand-over itself reads each link's last).
  fn linked(&self, green: &Green, path: &str) -> bool {
    let kept = green
      .links_to
      .get(path)
      .is_some_and(|links| links.iter().any(|link| !self.links.contains_key(link)));
    kept || self.targets.contains(path)
  }
}

/// The links naming the node at `home` once the version's link effects (`links`: each link's last target, none for a
/// removal) apply: the head's links to it the version keeps, and the links it makes to it.
fn surviving_links<'a>(
  green: &'a Green,
  home: &str,
  links: &'a BTreeMap<String, Option<String>>,
) -> BTreeSet<&'a str> {
  let kept = green
    .links_to
    .get(home)
    .into_iter()
    .flatten()
    .map(String::as_str)
    .filter(|link| !links.contains_key(*link));
  let made = links
    .iter()
    .filter(|(_, target)| target.as_deref() == Some(home))
    .map(|(link, _)| link.as_str());
  kept.chain(made).collect()
}
