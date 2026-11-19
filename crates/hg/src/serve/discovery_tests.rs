//! A-667: each branch's heads by 7.2's rule (`branchmap.py`, `_BaseBranchCache._process_new`, lines 422-424 of 7.2: "A
//! branchhead is a revision such that no descendant is on the same branch"), closed heads kept as the wire's
//! `branchmap` keeps them (`wireprotov1server.branchmap`, `branchheads(branch, closed=True)`). Held to stock 7.2's
//! answers on the histories they were measured on (Homebrew's Mercurial 7.2, 2026-11-19, `hg heads <branch>`), and to
//! the rule itself, judged naively, over generated histories that leave a branch and come back.

use proptest::prelude::*;

use super::super::tests::{Changeset, History, ancestors, joined, node_for, room};
use super::super::{Repository, Rev};
use super::{branch_head_revs, branchmap};
use crate::phases::Phase;
use crate::quote::{self, PATH_SAFE};

/// One revision of a hand-made history: its parents and its branch.
type Made<'b> = ([Option<Rev>; 2], &'b [u8]);

/// Shape: the branches a generated history moves between: two, so a path that leaves a branch comes back to it often.
const BRANCHES: [&[u8]; 2] = [b"default", b"stable"];

/// Shape: the histories the non-vacuity check makes from fixed seeds.
const SAMPLES: u64 = 256;
/// Shape: the revisions a generated history holds at most: enough for paths that leave a branch and come back twice.
const REVISIONS: usize = 40;

/// A history of `revisions` in order, none closing its branch, every one public.
fn history_of(revisions: &[Made<'_>]) -> History {
  let mut history = History::default();
  for (rev, (parents, branch)) in (0_u64..).zip(revisions) {
    history.push(Changeset {
      node: node_for(rev),
      parents: *parents,
      branch: branch.to_vec(),
      closes: false,
      phase: Phase::Public,
    });
  }
  history
}

/// Each branch's heads as `branch_head_revs` takes them.
fn heads(history: &History) -> Vec<Rev> {
  branch_head_revs(history, &mut room()).unwrap()
}

/// `branchmap`'s answer.
fn answer(history: &History) -> Vec<u8> {
  let mut out = Vec::new();
  branchmap(history, &mut room(), &mut out).unwrap();
  out
}

/// One line of `branchmap`'s answer: `branch` quoted, then the nodes of `revs`.
fn line(history: &History, branch: &[u8], revs: &[Rev]) -> Vec<u8> {
  let mut line = Vec::new();
  quote::quote(branch, PATH_SAFE, &mut line);
  line.push(b' ');
  line.extend(joined(revs.iter().map(|rev| history.node(*rev).unwrap())));
  line
}

/// 7.2's rule, judged naively: each revision no later revision on its branch has as an ancestor, oldest first.
fn naive(history: &History) -> Vec<Rev> {
  let count = history.count();
  (0..count)
    .filter(|rev| {
      let branch = history.branch(*rev);
      !(rev + 1..count)
        .any(|later| history.branch(later) == branch && ancestors(history, &[later]).contains(rev))
    })
    .collect()
}

/// The rule A-619 kept, which A-667 found wrong: each revision no child on its branch has as a parent.
fn by_child(history: &History) -> Vec<Rev> {
  let count = history.count();
  (0..count)
    .filter(|rev| {
      let branch = history.branch(*rev);
      !(0..count).any(|child| {
        history.branch(child) == branch && history.parents(child).contains(&Some(*rev))
      })
    })
    .collect()
}

/// A history from per-revision choices: a first parent any revision before it (none for the first, and a new root one
/// time in eleven), a second any other revision before it where asked (an ancestor of the first among them, as `hg
/// debugsetparents` makes one), its branch one of two, and a close where asked.
fn interleaved(choices: &[(u32, u32, bool, bool, bool)], seed: u64) -> History {
  let mut history = History::default();
  for (rev, (first, second, merges, stable, closes)) in (0_u32..).zip(choices) {
    let p1 = (rev > 0 && !first.is_multiple_of(11)).then(|| first % rev);
    let p2 = (rev > 1 && *merges)
      .then(|| second % rev)
      .filter(|parent| p1.is_some() && Some(*parent) != p1);
    history.push(Changeset {
      node: node_for(seed.wrapping_add(u64::from(rev))),
      parents: [p1, p2],
      branch: BRANCHES[usize::from(*stable)].to_vec(),
      closes: *closes,
      phase: Phase::Public,
    });
  }
  history
}

/// Histories that move between two branches and merge any earlier revisions, as [`interleaved`] makes them.
fn interleaved_history() -> impl Strategy<Value = History> {
  (
    proptest::collection::vec(
      (
        any::<u32>(),
        any::<u32>(),
        proptest::bool::weighted(0.33),
        any::<bool>(),
        proptest::bool::weighted(0.1),
      ),
      0..REVISIONS,
    ),
    any::<u64>(),
  )
    .prop_map(|(choices, seed)| interleaved(&choices, seed))
}

/// The choices of revision `rev` of sample `sample`, taken from a node's bytes so they are fixed and spread.
fn fixed_choices(sample: u64, rev: u64) -> (u32, u32, bool, bool, bool) {
  let bytes = node_for(sample.wrapping_mul(0x1_0000).wrapping_add(rev));
  let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
  let (first, second, flags) = (word(0), word(4), word(8));
  (first, second, flags.is_multiple_of(3), flags & 0x10 != 0, flags.is_multiple_of(10))
}

/// A-667 (g304, against stock 7.2): do take each branch's heads of `default` 0, `stable` 1 on 0 and `default` 2 on 1,
/// and answer `branchmap`. Expect `default`'s head 2 alone, since 2 descends from 0 through `stable`, and `stable`'s 1,
/// as stock 7.2's `hg heads default` and `hg heads stable` answer them; `branchmap`'s `default` line naming 2 alone.
#[test]
fn a_descendant_reached_through_another_branch_ends_a_head() {
  let history = history_of(&[
    ([None, None], b"default"),
    ([Some(0), None], b"stable"),
    ([Some(1), None], b"default"),
  ]);
  assert_eq!(heads(&history), [1, 2]);
  let lines = [line(&history, b"default", &[2]), line(&history, b"stable", &[1])];
  assert_eq!(answer(&history), lines.join(&b'\n'));
}

/// A-667 (7.2's shortcut): do take the heads of `default` 0, `default` 1 on 0, `other` 2 on 1 and `default` 3 on 2 and
/// 0, a merge whose parent on its branch is an ancestor of its other parent (as `hg debugsetparents` makes one). Expect
/// `default`'s head 3 alone, by the rule: stock 7.2 answers 3 and 1, its cache kept or computed whole, since its
/// shortcut for one known head and one parent on the branch (`branchmap.py` 478-483) never looks at the other parent.
#[test]
fn a_merge_with_an_ancestor_on_its_branch_leaves_no_head_behind() {
  let history = history_of(&[
    ([None, None], b"default"),
    ([Some(0), None], b"default"),
    ([Some(1), None], b"other"),
    ([Some(2), Some(0)], b"default"),
  ]);
  assert_eq!(heads(&history), [2, 3]);
}

/// A-667 (7.2's cache, history by history): do take the heads of `default` 0, `other` 1 on 0, `default` 2 on 1, `other`
/// 3 on 2 and `default` 4 on 3 and 0. Expect `default`'s head 4 alone and `other`'s 3. Stock 7.2 answers `default`'s 4
/// and 2 with its cache kept commit by commit, and 4 alone with it computed whole, so the rule is what a server can
/// answer, not one cache's history.
#[test]
fn heads_follow_the_rule_whatever_a_cache_s_history() {
  let history = history_of(&[
    ([None, None], b"default"),
    ([Some(0), None], b"other"),
    ([Some(1), None], b"default"),
    ([Some(2), None], b"other"),
    ([Some(3), Some(0)], b"default"),
  ]);
  assert_eq!(heads(&history), [3, 4]);
}

/// A-667: do take the heads where `stable` 1 on `default` 0 closes its branch and `default` 2 is on 1. Expect `stable`'s
/// closed head 1 kept beside `default`'s 2, `default`'s 0 ended through it, as `branchheads(closed=True)` keeps one.
#[test]
fn a_closed_head_is_kept_and_ends_a_head_through_it() {
  let mut history = history_of(&[
    ([None, None], b"default"),
    ([Some(0), None], b"stable"),
    ([Some(1), None], b"default"),
  ]);
  history.revs[1].closes = true;
  assert_eq!(heads(&history), [1, 2]);
}

/// A-667 (the oracle's non-vacuity): do make histories from fixed seeds as the oracle's generator makes them. Expect
/// some whose heads by the rule A-619 kept are not 7.2's, so the oracle's test cannot be passed by that rule.
#[test]
fn generated_histories_end_a_head_through_another_branch() {
  let differing = (0..SAMPLES)
    .filter(|sample| {
      let revisions = u64::try_from(REVISIONS).unwrap();
      let choices: Vec<_> = (0..revisions).map(|rev| fixed_choices(*sample, rev)).collect();
      let history = interleaved(&choices, *sample);
      by_child(&history) != naive(&history)
    })
    .count();
  assert!(differing > 0, "no sample of {SAMPLES} ends a head through another branch");
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-667 (oracle): do take each branch's heads of generated histories that move between two branches and merge any
  /// earlier revisions, an ancestor of the other parent among them, with closes. Expect exactly the revisions no later
  /// revision on their branch descends from, oldest first.
  #[test]
  fn heads_are_the_revisions_no_later_one_on_their_branch_descends_from(history in interleaved_history()) {
    prop_assert_eq!(heads(&history), naive(&history));
  }
}
