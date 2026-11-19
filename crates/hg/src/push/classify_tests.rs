//! A-657's classification tests: a stock 7.2 bundle placed on an empty repository as 7.2 lists its branches and
//! phases, generated histories against 7.2's rules stated naively (branch heads by descent, a maximum matching, the
//! phase roots, the change in heads), and the bookmark, phase and refusal cases in 7.2's words.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;

use super::{
  BookmarkMove, BookmarkRead, LineMove, LineRead, NewLine, Plan, RootChange, Unclassified, classify,
};
use crate::bundle2::Bundle2;
use crate::changegroup::Version;
use crate::fixtures::{CG03, LOG, node};
use crate::phases::{Phase, PhaseHead};
use crate::push::reply::{Refused, Replied, heads_changed};
use crate::push::verify::{Arrived, Held, verify};
use crate::push::{Asks, BookmarkAt, ChangegroupPart, PushkeyAsk, Taken, Unbundle, read};
use crate::serve::discovery::branch_head_revs;
use crate::serve::{Repository, Rev};
use crate::{Admission, NULL_NODE, Node};

/// Room enough for anything a test holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

/// A served view a test builds: changesets in revision order.
#[derive(Clone, Debug, Default)]
struct View {
  /// The changesets.
  changesets: Vec<Old>,
}

/// A changeset of a [`View`].
#[derive(Clone, Debug)]
struct Old {
  node: Node,
  parents: [Option<Rev>; 2],
  branch: Vec<u8>,
  closes: bool,
  phase: Phase,
}

impl View {
  fn at(&self, rev: Rev) -> &Old {
    &self.changesets[usize::try_from(rev).unwrap()]
  }
}

impl Repository for View {
  fn count(&self) -> Rev {
    Rev::try_from(self.changesets.len()).unwrap()
  }

  fn node(&self, rev: Rev) -> Option<Node> {
    self
      .changesets
      .get(usize::try_from(rev).ok()?)
      .map(|old| old.node)
  }

  fn rev(&self, node: &Node) -> Option<Rev> {
    let at = self.changesets.iter().position(|old| old.node == *node)?;
    Rev::try_from(at).ok()
  }

  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self.at(rev).parents
  }

  fn branch(&self, rev: Rev) -> &[u8] {
    &self.at(rev).branch
  }

  fn closes(&self, rev: Rev) -> bool {
    self.at(rev).closes
  }

  fn phase(&self, rev: Rev) -> Phase {
    self.at(rev).phase
  }

  fn publishing(&self) -> bool {
    false
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    core::iter::empty()
  }
}

/// Nothing held: an empty repository.
struct Nothing;

impl Held for Nothing {
  fn changeset_text(&self, _node: &Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }

  fn manifest_text(&self, _node: &Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }

  fn file_text(&self, _path: &[u8], _node: &Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }
}

/// A changeset's text on `branch`, closing it or not: the extras 7.2 writes (`branch` unless `default`, `close`).
fn text(branch: &[u8], closes: bool) -> Vec<u8> {
  let mut extras: Vec<Vec<u8>> = Vec::new();
  if branch != b"default" {
    extras.push([b"branch:".as_slice(), branch].concat());
  }
  if closes {
    extras.push(b"close:1".to_vec());
  }
  let mut text = b"0000000000000000000000000000000000000000\ntest\n0 0".to_vec();
  if !extras.is_empty() {
    text.push(b' ');
    text.extend_from_slice(&extras.join(&b'\0'));
  }
  text.extend_from_slice(b"\n\nchanged");
  text
}

/// A brought changeset.
fn arrived(node: Node, (p1, p2): (Node, Node), branch: &[u8], closes: bool) -> Arrived {
  Arrived {
    node,
    p1,
    p2,
    link: node,
    text: text(branch, closes),
  }
}

/// A part a push carries, mandatory.
fn part(id: u32, asks: Asks) -> Taken {
  Taken {
    id,
    mandatory: true,
    asks,
  }
}

/// A changegroup part (its payload is the verifier's, never the classifier's).
fn changegroup() -> Asks {
  Asks::Changegroup(ChangegroupPart {
    version: Version::V03,
    changesets: None,
    target_phase: None,
    tree_manifests: false,
    payload: Vec::new(),
  })
}

/// The revision `rev` of the A-514 fixture history (`log.txt`).
fn logged(rev: u32) -> Node {
  let line = LOG
    .lines()
    .find(|line| line.split(' ').next() == Some(rev.to_string().as_str()))
    .unwrap();
  node(line.split(' ').nth(1).unwrap())
}

/// A-657, AC-HG.3 (T-HG.3). Do: read, verify and place stock 7.2's bundle of a history with two branches, a merge and
/// an amended draft (`fixtures/cg03.hg`, changegroup 03 with its `phase-heads` and `obsmarkers` parts) on an empty
/// repository whose one line is `default`'s, empty. Expect: its `obsmarkers` part refused by kind (piece 7's); without
/// it, `default`'s empty line moves to revision 7 and a line of `stable` is made at revision 2 from that line, as 7.2's
/// branchmap lists the two heads; the change in heads is 7.2's `return` 1 (one head added, the empty repository's null
/// head removed); every changeset the `phase-heads` part names public is public and the amended draft is the one new
/// draft root; the hooks' first and last changesets are revisions 0 and 7.
#[test]
fn a_stock_history_lands_on_an_empty_repository_as_7_2_lists_it() {
  let bundle = Bundle2::decode(CG03, &mut room()).unwrap();
  let mut unbundle = read(&bundle, &mut room()).unwrap();
  let (_, cg) = unbundle.changegroup().unwrap();
  let verified = verify(cg, &Nothing, &mut room()).unwrap();
  let lines = [LineRead {
    branch: b"default".to_vec(),
    number: 0,
    head: None,
  }];
  let view = View::default();
  let brought = (
    verified.changesets.as_slice(),
    verified.held_changesets.as_slice(),
  );
  let refused = classify(&unbundle, brought, &view, (&lines, &[]), false, &mut room());
  assert!(
    matches!(refused, Err(Unclassified::Markers { .. })),
    "{refused:?}"
  );
  unbundle
    .parts
    .retain(|taken| !matches!(taken.asks, Asks::Obsmarkers(_)));
  let plan = classify(&unbundle, brought, &view, (&lines, &[]), false, &mut room()).unwrap();
  assert_stock_placement(&plan, &unbundle);
}

fn assert_stock_placement(plan: &Plan, unbundle: &Unbundle) {
  assert_eq!(
    plan.lines,
    vec![LineMove::Moved {
      line: 0,
      head: logged(7)
    }]
  );
  assert_eq!(
    plan.created,
    vec![NewLine {
      branch: b"stable".to_vec(),
      number: 0,
      head: logged(2),
      base: 0,
    }]
  );
  assert_eq!(plan.touched, vec![b"default".to_vec(), b"stable".to_vec()]);
  let (id, _) = unbundle.changegroup().unwrap();
  assert_eq!(
    plan.replied,
    vec![Replied::Changegroup {
      in_reply_to: id,
      heads: 1
    }]
  );
  assert_eq!(plan.roots, vec![RootChange::Added(logged(7))]);
  assert_eq!(plan.added, Some((logged(0), logged(7))));
}

/// Format: the branches generated histories use, one with a slash.
const BRANCHES: [&[u8]; 3] = [b"default", b"stable", b"x/y"];

/// A generated changeset: its two parent picks, its branch's pick, whether it closes, and whether it is a draft (the
/// view's) or carried by the push's `phase-heads` as public (the push's).
type Shape = (u8, u8, u8, bool, bool);

/// The whole generated graph, old then brought: each changeset's node, parents (indices), branch and closing.
#[derive(Clone, Debug, Default)]
struct Whole {
  nodes: Vec<Node>,
  parents: Vec<[Option<usize>; 2]>,
  branches: Vec<Vec<u8>>,
  closes: Vec<bool>,
  old: usize,
}

impl Whole {
  /// Every ancestor of `at`, itself included.
  fn ancestors(&self, at: usize) -> BTreeSet<usize> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![at];
    while let Some(next) = stack.pop() {
      if seen.insert(next) {
        stack.extend(self.parents[next].iter().flatten());
      }
    }
    seen
  }

  /// The branch heads among the first `upto` changesets, by descent (7.2's branchmap).
  fn branch_heads(&self, upto: usize) -> BTreeMap<Vec<u8>, BTreeSet<Node>> {
    let mut heads: BTreeMap<Vec<u8>, BTreeSet<Node>> = BTreeMap::new();
    for at in 0..upto {
      let continued = (0..upto).any(|other| {
        other != at
          && self.branches[other] == self.branches[at]
          && self.ancestors(other).contains(&at)
      });
      if !continued {
        heads
          .entry(self.branches[at].clone())
          .or_default()
          .insert(self.nodes[at]);
      }
    }
    heads
  }

  /// The topological heads among the first `upto` changesets.
  fn heads(&self, upto: usize) -> BTreeSet<usize> {
    (0..upto)
      .filter(|at| !(0..upto).any(|child| self.parents[child].contains(&Some(*at))))
      .collect()
  }
}

/// A generated history: `old` the view's changesets and `brought` the push's, each picking its parents among those
/// before it; and the view.
fn generated(old: &[Shape], brought: &[Shape]) -> (Whole, View) {
  let mut whole = Whole {
    old: old.len(),
    ..Whole::default()
  };
  for (index, (first, second, branch, closes, _)) in old.iter().chain(brought).enumerate() {
    let pick = |value: u8| {
      let value = usize::from(value) % (index + 1);
      (value < index).then_some(value)
    };
    let p1 = pick(*first);
    let p2 = pick(*second).filter(|p2| p1.is_some() && Some(*p2) != p1);
    whole.nodes.push([u8::try_from(index + 1).unwrap(); 20]);
    whole.parents.push([p1, p2]);
    whole
      .branches
      .push(BRANCHES[usize::from(*branch) % BRANCHES.len()].to_vec());
    whole.closes.push(*closes);
  }
  let mut view = View::default();
  for (index, (.., draft)) in old.iter().enumerate() {
    let inherited = whole.parents[index]
      .iter()
      .flatten()
      .any(|parent| view.changesets[*parent].phase == Phase::Draft);
    view.changesets.push(Old {
      node: whole.nodes[index],
      parents: whole.parents[index].map(|parent| parent.map(|at| Rev::try_from(at).unwrap())),
      branch: whole.branches[index].clone(),
      closes: whole.closes[index],
      phase: if *draft || inherited {
        Phase::Draft
      } else {
        Phase::Public
      },
    });
  }
  (whole, view)
}

/// The lines the push reads: one a branch head of the view, numbered in revision order, and `default`'s empty line
/// where the view has no head of `default` (a repository's first line).
fn lines_of(whole: &Whole) -> Vec<LineRead> {
  let mut lines = Vec::new();
  let heads = whole.branch_heads(whole.old);
  for (branch, nodes) in &heads {
    let mut ordered: Vec<usize> = (0..whole.old)
      .filter(|at| nodes.contains(&whole.nodes[*at]))
      .collect();
    ordered.sort_unstable();
    for (number, at) in ordered.iter().enumerate() {
      lines.push(LineRead {
        branch: branch.clone(),
        number: u32::try_from(number).unwrap(),
        head: Some(whole.nodes[*at]),
      });
    }
  }
  if !heads.contains_key(b"default".as_slice()) {
    lines.push(LineRead {
      branch: b"default".to_vec(),
      number: 0,
      head: None,
    });
  }
  lines
}

/// Each branch's heads afterwards, and each branch's numbers.
type Applied = (
  BTreeMap<Vec<u8>, BTreeSet<Node>>,
  BTreeMap<Vec<u8>, Vec<u32>>,
);

/// A line afterwards: its branch, number and head; none once retired.
type Slot = Option<(Vec<u8>, u32, Option<Node>)>;

/// The plan's lines applied to the lines read.
fn applied(lines: &[LineRead], plan: &Plan) -> Applied {
  let mut after: Vec<Slot> = lines
    .iter()
    .map(|line| Some((line.branch.clone(), line.number, line.head)))
    .collect();
  for moved in &plan.lines {
    match moved {
      LineMove::Moved { line, head } => after[*line].as_mut().unwrap().2 = Some(*head),
      LineMove::Retired { line } => after[*line] = None,
    }
  }
  after.extend(
    plan
      .created
      .iter()
      .map(|made| Some((made.branch.clone(), made.number, Some(made.head)))),
  );
  let mut heads: BTreeMap<Vec<u8>, BTreeSet<Node>> = BTreeMap::new();
  let mut numbers: BTreeMap<Vec<u8>, Vec<u32>> = BTreeMap::new();
  for (branch, number, head) in after.into_iter().flatten() {
    numbers.entry(branch.clone()).or_default().push(number);
    if let Some(head) = head {
      heads.entry(branch).or_default().insert(head);
    }
  }
  (heads, numbers)
}

/// The largest matching of new heads to lines a head descends from, found by trying every assignment.
fn largest(edges: &[Vec<usize>], taken: &mut BTreeSet<usize>) -> usize {
  let Some((first, rest)) = edges.split_first() else {
    return 0;
  };
  let mut best = largest(rest, taken);
  for line in first {
    if taken.insert(*line) {
      best = best.max(1 + largest(rest, taken));
      taken.remove(line);
    }
  }
  best
}

/// The moves a branch could make at most: its new heads matched to the lines (with heads, or empty) they descend from.
fn most_moves(whole: &Whole, lines: &[LineRead], branch: &[u8]) -> usize {
  let after = whole.branch_heads(whole.nodes.len());
  let new_heads: Vec<usize> = (whole.old..whole.nodes.len())
    .filter(|at| whole.branches[*at] == branch && after[branch].contains(&whole.nodes[*at]))
    .collect();
  let edges: Vec<Vec<usize>> = new_heads
    .iter()
    .map(|head| {
      let ancestors = whole.ancestors(*head);
      (0..lines.len())
        .filter(|line| {
          lines[*line].branch == branch
            && lines[*line]
              .head
              .is_none_or(|node| ancestors.iter().any(|at| whole.nodes[*at] == node))
        })
        .collect()
    })
    .collect();
  largest(&edges, &mut BTreeSet::new())
}

/// The draft roots the view holds before the push and the push leaves, by the definition: a draft whose parents are
/// all public.
fn draft_roots(whole: &Whole, phases: &[Phase]) -> BTreeSet<Node> {
  (0..phases.len())
    .filter(|at| {
      phases[*at] == Phase::Draft
        && whole.parents[*at]
          .iter()
          .flatten()
          .all(|parent| phases[*parent] == Phase::Public)
    })
    .map(|at| whole.nodes[at])
    .collect()
}

/// The phases after the push by 7.2's rules stated naively: brought changesets draft, or public with every ancestor on
/// a publishing repository; then each public head of `phase-heads` public with every ancestor.
fn phases_after(whole: &Whole, view: &View, (publishing, public): (bool, &[usize])) -> Vec<Phase> {
  let mut phases: Vec<Phase> = view.changesets.iter().map(|old| old.phase).collect();
  phases.extend((whole.old..whole.nodes.len()).map(|_| Phase::Draft));
  let mut seeds: Vec<usize> = public.to_vec();
  if publishing {
    seeds.extend(whole.old..whole.nodes.len());
  }
  for seed in seeds {
    for at in whole.ancestors(seed) {
      phases[at] = Phase::Public;
    }
  }
  phases
}

/// 7.2's `return` stated naively: the topological heads before and after.
fn naive_return(whole: &Whole) -> i64 {
  let before = whole.heads(whole.old);
  let after = whole.heads(whole.nodes.len());
  let removed =
    before.difference(&after).count() + usize::from(whole.old == 0 && !whole.nodes.is_empty());
  let added: Vec<usize> = after.difference(&before).copied().collect();
  let closing = added.iter().filter(|at| whole.closes[**at]).count();
  let count = |count: usize| u64::try_from(count).unwrap();
  heads_changed(count(removed), count(added.len()), count(closing))
}

/// The push's parts: the changegroup, and a `phase-heads` part naming the brought changesets its shapes mark public.
fn pushed(whole: &Whole, brought: &[Shape]) -> (Unbundle, Vec<usize>) {
  let public: Vec<usize> = brought
    .iter()
    .enumerate()
    .filter(|(_, shape)| shape.4)
    .map(|(index, _)| whole.old + index)
    .collect();
  let heads: Vec<PhaseHead> = public
    .iter()
    .map(|at| PhaseHead {
      phase: Phase::Public,
      node: whole.nodes[*at],
    })
    .collect();
  let unbundle = Unbundle {
    parts: vec![part(0, changegroup()), part(1, Asks::PhaseHeads(heads))],
  };
  (unbundle, public)
}

/// The brought changesets as the verifier hands them over.
fn brought_of(whole: &Whole) -> Vec<Arrived> {
  (whole.old..whole.nodes.len())
    .map(|at| {
      let parent =
        |index: usize| whole.parents[at][index].map_or(NULL_NODE, |parent| whole.nodes[parent]);
      arrived(
        whole.nodes[at],
        (parent(0), parent(1)),
        &whole.branches[at],
        whole.closes[at],
      )
    })
    .collect()
}

/// The placement against the naive rules: every branch's lines hold exactly its heads, each branch's numbers distinct,
/// and as many lines moved as a largest matching allows.
fn check_lines(whole: &Whole, lines: &[LineRead], plan: &Plan) -> Result<(), TestCaseError> {
  let (heads, numbers) = applied(lines, plan);
  let mut expected = whole.branch_heads(whole.nodes.len());
  expected.retain(|_, nodes| !nodes.is_empty());
  prop_assert_eq!(heads, expected);
  for numbers in numbers.values() {
    let distinct: BTreeSet<u32> = numbers.iter().copied().collect();
    prop_assert_eq!(distinct.len(), numbers.len());
  }
  for branch in BRANCHES {
    let on_branch = |placed: &&LineMove| match placed {
      LineMove::Moved { line, .. } => lines[*line].branch == branch,
      LineMove::Retired { .. } => false,
    };
    let moved = plan.lines.iter().filter(on_branch).count();
    prop_assert_eq!(moved, most_moves(whole, lines, branch));
  }
  Ok(())
}

/// The phase roots against the naive rules: each removed root held before, each added one not, and the roots after
/// exactly the definition's.
fn check_phases(
  whole: &Whole,
  view: &View,
  (publishing, public): (bool, &[usize]),
  plan: &Plan,
) -> Result<(), TestCaseError> {
  let before: Vec<Phase> = view.changesets.iter().map(|old| old.phase).collect();
  let after = phases_after(whole, view, (publishing, public));
  let mut roots = draft_roots(whole, &before);
  for change in &plan.roots {
    match change {
      RootChange::Removed(node) => prop_assert!(roots.remove(node)),
      RootChange::Added(node) => prop_assert!(roots.insert(*node)),
    }
  }
  prop_assert_eq!(roots, draft_roots(whole, &after));
  Ok(())
}

/// The view with the push's changesets added after its own (their phases are not read).
fn view_after(whole: &Whole, view: &View) -> View {
  let mut after = view.clone();
  for at in whole.old..whole.nodes.len() {
    after.changesets.push(Old {
      node: whole.nodes[at],
      parents: whole.parents[at].map(|parent| parent.map(|at| Rev::try_from(at).unwrap())),
      branch: whole.branches[at].clone(),
      closes: whole.closes[at],
      phase: Phase::Draft,
    });
  }
  after
}

/// Each branch's heads as the server answers `branchmap` and judges `check:updated-heads` (A-667's
/// `branch_head_revs`).
fn served_heads(view: &View) -> BTreeMap<Vec<u8>, BTreeSet<Node>> {
  let mut heads: BTreeMap<Vec<u8>, BTreeSet<Node>> = BTreeMap::new();
  for rev in branch_head_revs(view, &mut room()).unwrap() {
    heads
      .entry(view.branch(rev).to_vec())
      .or_default()
      .insert(view.node(rev).unwrap());
  }
  heads
}

/// The lines a push reads where every line is a served head: each branch's heads numbered from 0 in revision order,
/// and `default`'s empty line where the view has no head of `default`.
fn served_lines(view: &View) -> Vec<LineRead> {
  let mut numbers: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
  let mut lines = Vec::new();
  for rev in branch_head_revs(view, &mut room()).unwrap() {
    let branch = view.branch(rev).to_vec();
    let number = numbers.entry(branch.clone()).or_default();
    lines.push(LineRead {
      branch,
      number: *number,
      head: view.node(rev),
    });
    *number += 1;
  }
  if !numbers.contains_key(b"default".as_slice()) {
    lines.push(LineRead {
      branch: b"default".to_vec(),
      number: 0,
      head: None,
    });
  }
  lines
}

/// Places the push of `whole`'s brought changesets on the lines the served heads of `view` give, and holds the lines
/// afterwards to the served heads of the view with the push added; the lines read and the plan.
fn placed_on_served(
  whole: &Whole,
  view: &View,
  (unbundle, publishing): (&Unbundle, bool),
) -> Result<(Vec<LineRead>, Plan), TestCaseError> {
  let lines = served_lines(view);
  let arrived = brought_of(whole);
  let plan = classify(
    unbundle,
    (&arrived, &[]),
    view,
    (&lines, &[]),
    publishing,
    &mut room(),
  )
  .map_err(|refused| TestCaseError::fail(format!("{refused:?}")))?;
  let (heads, _) = applied(&lines, &plan);
  prop_assert_eq!(heads, served_heads(&view_after(whole, view)));
  Ok((lines, plan))
}

/// Whether the push ended a line's head only through another branch: the head read keeps no child on its branch, so the
/// rule A-619 kept (a head is what no child on its branch continues) leaves it a head, and is no served head after.
fn ended_through_another_branch(lines: &[LineRead], after: &View) -> bool {
  let served = served_heads(after);
  lines.iter().filter_map(|line| line.head).any(|head| {
    let rev = after.rev(&head).unwrap();
    let branch = after.branch(rev);
    let child_on_branch = (0..after.count())
      .any(|child| after.branch(child) == branch && after.parents(child).contains(&Some(rev)));
    !child_on_branch
      && !served
        .get(branch)
        .is_some_and(|heads| heads.contains(&head))
  })
}

/// A history made by hand: each changeset's parents (earlier indices), branch and close, the first `old` the view's,
/// every one of the view's public.
fn made(changesets: &[([Option<usize>; 2], &[u8], bool)], old: usize) -> (Whole, View) {
  let mut whole = Whole {
    old,
    ..Whole::default()
  };
  for (index, (parents, branch, closes)) in changesets.iter().enumerate() {
    whole.nodes.push([u8::try_from(index + 1).unwrap(); 20]);
    whole.parents.push(*parents);
    whole.branches.push(branch.to_vec());
    whole.closes.push(*closes);
  }
  let mut view = View::default();
  for at in 0..old {
    view.changesets.push(Old {
      node: whole.nodes[at],
      parents: whole.parents[at].map(|parent| parent.map(|at| Rev::try_from(at).unwrap())),
      branch: whole.branches[at].clone(),
      closes: whole.closes[at],
      phase: Phase::Public,
    });
  }
  (whole, view)
}

/// A push of a changegroup alone.
fn changegroup_alone() -> Unbundle {
  Unbundle {
    parts: vec![part(0, changegroup())],
  }
}

/// A-657 against A-667 (g304's history; stock 7.2's `hg heads default` answers 2 alone): do push `default` 2 on
/// `stable` 1, itself on `default` 0, onto `default`'s line at 0 and `stable`'s at 1. Expect `default`'s line moved to
/// 2, no line made or retired, since 2 descends from 0 through `stable`; the lines' heads afterwards the served heads.
#[test]
fn a_line_left_through_another_branch_moves_to_its_new_head() {
  let (whole, view) = made(
    &[
      ([None, None], b"default", false),
      ([Some(0), None], b"stable", false),
      ([Some(1), None], b"default", false),
    ],
    2,
  );
  let (lines, plan) = placed_on_served(&whole, &view, (&changegroup_alone(), false)).unwrap();
  assert_eq!(lines.len(), 2);
  assert_eq!(
    plan.lines,
    vec![LineMove::Moved {
      line: 0,
      head: whole.nodes[2]
    }]
  );
  assert!(plan.created.is_empty());
}

/// A-657 against A-667 (a merge whose parent on its branch is an ancestor of its other parent): do push `default` 3 on
/// `stable` 2 and `default` 0 onto `default`'s line at 1 (on 0) and `stable`'s at 2 (on 1). Expect `default`'s line
/// moved to 3 and nothing made, by the rule, where stock 7.2's shortcut (`branchmap.py` 478-483) keeps 1 a head.
#[test]
fn a_merge_reaching_a_line_through_another_branch_moves_it() {
  let (whole, view) = made(
    &[
      ([None, None], b"default", false),
      ([Some(0), None], b"default", false),
      ([Some(1), None], b"stable", false),
      ([Some(2), Some(0)], b"default", false),
    ],
    3,
  );
  let (_, plan) = placed_on_served(&whole, &view, (&changegroup_alone(), false)).unwrap();
  assert_eq!(
    plan.lines,
    vec![LineMove::Moved {
      line: 0,
      head: whole.nodes[3]
    }]
  );
  assert!(plan.created.is_empty());
}

/// A-657 against A-667 (a closed head): do push `stable` 1 on `default` 0, closing its branch, and `default` 2 on 1,
/// onto `default`'s line at 0. Expect `default`'s line moved to 2 through `stable`, and a line of `stable` made at its
/// closed head 1 from `default`'s line, as 7.2's branchmap keeps a closed head.
#[test]
fn a_closed_head_keeps_a_line_and_ends_one_through_it() {
  let (whole, view) = made(
    &[
      ([None, None], b"default", false),
      ([Some(0), None], b"stable", true),
      ([Some(1), None], b"default", false),
    ],
    1,
  );
  let (_, plan) = placed_on_served(&whole, &view, (&changegroup_alone(), false)).unwrap();
  assert_eq!(
    plan.lines,
    vec![LineMove::Moved {
      line: 0,
      head: whole.nodes[2]
    }]
  );
  let stable = NewLine {
    branch: b"stable".to_vec(),
    number: 0,
    head: whole.nodes[1],
    base: 0,
  };
  assert_eq!(plan.created, vec![stable]);
}

/// Shape: the fixed samples the cross-check's non-vacuity places.
const SAMPLES: u64 = 256;
/// Shape: the changesets of a fixed sample's view, and of its push: the generated tests' most.
const SAMPLE_CHANGESETS: u64 = 8;

/// The shape of changeset `index` of fixed sample `sample`: bytes of a BLAKE3 hash of both, so samples are fixed and
/// spread.
fn fixed_shape(sample: u64, index: u64) -> Shape {
  let seed = [sample.to_le_bytes(), index.to_le_bytes()].concat();
  let hash = blake3::hash(&seed);
  let [first, second, branch, closes, public, ..] = *hash.as_bytes();
  (
    first,
    second,
    branch,
    closes.is_multiple_of(2),
    public.is_multiple_of(2),
  )
}

/// A-657 against A-667 (the cross-check's non-vacuity): do place pushes made from fixed samples, as the generated
/// cross-check makes them, on lines read from the served heads. Expect each held to the served heads afterwards, and
/// some ending a line's head only through another branch, where the rule A-619 kept would leave it a head, so the
/// cross-check cannot be passed by that rule.
#[test]
fn fixed_pushes_end_a_line_through_another_branch() {
  let mut ended = 0_u64;
  for sample in 0..SAMPLES {
    let shapes = |from: u64| -> Vec<Shape> {
      (from..from + SAMPLE_CHANGESETS)
        .map(|index| fixed_shape(sample, index))
        .collect()
    };
    let (old, brought) = (shapes(0), shapes(SAMPLE_CHANGESETS));
    let (whole, view) = generated(&old, &brought);
    let (unbundle, _) = pushed(&whole, &brought);
    let (lines, _) = placed_on_served(&whole, &view, (&unbundle, false)).unwrap();
    ended += u64::from(ended_through_another_branch(
      &lines,
      &view_after(&whole, &view),
    ));
  }
  assert!(
    ended > 0,
    "no sample of {SAMPLES} ends a line's head through another branch"
  );
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-657 against A-667 (the served heads): do place generated pushes, as the test below, on lines read from the heads
  /// the server answers `branchmap` with (`branch_head_revs`). Expect the lines afterwards to hold exactly the served
  /// heads of the view with the push added: no line `branchmap` does not list, and no listed head without the line
  /// `check:updated-heads` finds it by.
  #[test]
  fn lines_after_a_push_hold_the_served_branch_heads(
    old in prop::collection::vec(any::<Shape>(), 0..8),
    brought in prop::collection::vec(any::<Shape>(), 0..8),
    publishing in any::<bool>(),
  ) {
    let (whole, view) = generated(&old, &brought);
    let (unbundle, _) = pushed(&whole, &brought);
    placed_on_served(&whole, &view, (&unbundle, publishing))?;
  }

  /// A-657, AC-HG.3 and AC-HG.4 (T-HG.3, T-HG.4: the serial model of lines and phases). Do: place generated pushes of
  /// up to eight changesets on three branches, some marked public by a `phase-heads` part, on generated histories of up
  /// to eight with drafts, on a publishing repository or not. Expect: every branch's lines afterwards hold exactly its
  /// heads by 7.2's branchmap, each branch's numbers distinct; as many lines moved as a largest matching allows (so as
  /// few names changed); the draft roots afterwards exactly 7.2's, the removed ones held before; `return` 7.2's.
  #[test]
  fn placements_phases_and_heads_agree_with_7_2s_rules_stated_naively(
    old in prop::collection::vec(any::<Shape>(), 0..8),
    brought in prop::collection::vec(any::<Shape>(), 0..8),
    publishing in any::<bool>(),
  ) {
    let (whole, view) = generated(&old, &brought);
    let lines = lines_of(&whole);
    let (unbundle, public) = pushed(&whole, &brought);
    let arrived = brought_of(&whole);
    let plan = classify(&unbundle, (&arrived, &[]), &view, (&lines, &[]), publishing, &mut room())
      .map_err(|refused| TestCaseError::fail(format!("{refused:?}")))?;
    check_lines(&whole, &lines, &plan)?;
    check_phases(&whole, &view, (publishing, &public), &plan)?;
    let returned = plan.replied.first().cloned();
    let expected = Replied::Changegroup { in_reply_to: 0, heads: naive_return(&whole) };
    prop_assert_eq!(returned, Some(expected));
  }
}

/// A view of one public changeset, `a`, and a draft child of it, `b`, both on `default`.
fn two() -> View {
  let mut view = View::default();
  view.changesets.push(Old {
    node: [1; 20],
    parents: [None, None],
    branch: b"default".to_vec(),
    closes: false,
    phase: Phase::Public,
  });
  view.changesets.push(Old {
    node: [2; 20],
    parents: [Some(0), None],
    branch: b"default".to_vec(),
    closes: false,
    phase: Phase::Draft,
  });
  view
}

/// `default`'s one line, at `b`.
fn line_at_b() -> Vec<LineRead> {
  vec![LineRead {
    branch: b"default".to_vec(),
    number: 0,
    head: Some([2; 20]),
  }]
}

/// A pushkey part's request.
fn pushkey(namespace: &[u8], key: &[u8], (old, new): (&[u8], &[u8])) -> Asks {
  Asks::Pushkey(PushkeyAsk {
    namespace: namespace.to_vec(),
    key: key.to_vec(),
    old: old.to_vec(),
    new: new.to_vec(),
  })
}

/// The hex of a node, as a pushkey carries it.
fn hex(node: Node) -> Vec<u8> {
  crate::fixtures::hex(&node).into_bytes()
}

/// A-657 (§4.17.3 decision 2; `applychanges`, `pushbookmark`). Do: with bookmark `m` at `a`, push a `bookmarks` part
/// moving `m` to `b` and making `n` at `b`, then an advisory `bookmarks` pushkey deleting `m` from `a` (stale) and a
/// mandatory one deleting `n` from `b`; then the same push with a mandatory pushkey deleting `m` from `a` added.
/// Expect: `m` moved forward to `b` and `n` made then deleted, so nothing of it; the stale pushkey's `reply:pushkey` 0
/// and the other's 1; the stale mandatory one refuses the whole push in 7.2's `error:pushkey` form, its parameters as
/// sent.
#[test]
fn bookmarks_move_as_7_2_moves_them() {
  let view = two();
  let marks = [BookmarkRead {
    name: b"m".to_vec(),
    head: [1; 20],
  }];
  let mut unbundle = Unbundle {
    parts: vec![
      part(
        0,
        Asks::Bookmarks(vec![
          BookmarkAt {
            name: b"m".to_vec(),
            node: Some([2; 20]),
          },
          BookmarkAt {
            name: b"n".to_vec(),
            node: Some([2; 20]),
          },
        ]),
      ),
      Taken {
        id: 1,
        mandatory: false,
        asks: pushkey(b"bookmarks", b"m", (&hex([1; 20]), b"")),
      },
      part(2, pushkey(b"bookmarks", b"n", (&hex([2; 20]), b""))),
    ],
  };
  let plan = classify(
    &unbundle,
    (&[], &[]),
    &view,
    (&line_at_b(), &marks),
    false,
    &mut room(),
  )
  .unwrap();
  assert_bookmarks_moved(&plan);
  unbundle
    .parts
    .push(part(3, pushkey(b"bookmarks", b"m", (&hex([1; 20]), b""))));
  let refused = classify(
    &unbundle,
    (&[], &[]),
    &view,
    (&line_at_b(), &marks),
    false,
    &mut room(),
  );
  assert_eq!(
    refused.map_err(|refused| refused.refused()),
    Err(Refused::Pushkey {
      in_reply_to: 3,
      namespace: Some(b"bookmarks".to_vec()),
      key: Some(b"m".to_vec()),
      new: Some(Vec::new()),
      old: Some(hex([1; 20])),
      ret: None,
    })
  );
}

fn assert_bookmarks_moved(plan: &Plan) {
  assert_eq!(
    plan.bookmarks,
    vec![BookmarkMove::Moved {
      name: b"m".to_vec(),
      from: [1; 20],
      to: [2; 20],
      forward: true,
    }]
  );
  assert_eq!(
    plan.replied,
    vec![
      Replied::Pushkey {
        in_reply_to: 1,
        moved: false
      },
      Replied::Pushkey {
        in_reply_to: 2,
        moved: true
      },
    ]
  );
  assert!(plan.lines.is_empty() && plan.created.is_empty() && plan.roots.is_empty());
}

/// A-657 (§4.17.3 decision 3; `pushphase`, `updatephases`). Do: push a `phases` pushkey moving `b` from draft to
/// public, one moving `a` from secret to draft (it is public, in neither: refused, advisory), and one naming `b` public
/// to public (it holds: already there, 7.2's "raced, but got correct result"); then a `phase-heads` part naming a node
/// the repository lacks. Expect: `b`'s root removed, the replies 1, 0 and 1; the unknown node refused in 7.2's lookup
/// words.
#[test]
fn phases_move_only_toward_public_as_7_2_moves_them() {
  let view = two();
  let unbundle = Unbundle {
    parts: vec![
      part(0, pushkey(b"phases", &hex([2; 20]), (b"1", b"0"))),
      Taken {
        id: 1,
        mandatory: false,
        asks: pushkey(b"phases", &hex([1; 20]), (b"2", b"1")),
      },
      part(2, pushkey(b"phases", &hex([2; 20]), (b"0", b"0"))),
    ],
  };
  let plan = classify(
    &unbundle,
    (&[], &[]),
    &view,
    (&line_at_b(), &[]),
    false,
    &mut room(),
  )
  .unwrap();
  assert_eq!(plan.roots, vec![RootChange::Removed([2; 20])]);
  let moved: Vec<bool> = plan
    .replied
    .iter()
    .map(|replied| matches!(replied, Replied::Pushkey { moved: true, .. }))
    .collect();
  assert_eq!(moved, vec![true, false, true]);
  let unknown = Unbundle {
    parts: vec![part(
      0,
      Asks::PhaseHeads(vec![PhaseHead {
        phase: Phase::Public,
        node: [9; 20],
      }]),
    )],
  };
  let refused = classify(
    &unknown,
    (&[], &[]),
    &view,
    (&line_at_b(), &[]),
    false,
    &mut room(),
  );
  let words = b"unknown revision '0909090909090909090909090909090909090909'".to_vec();
  assert_eq!(
    refused.map_err(|refused| refused.refused()),
    Err(Refused::Abort {
      message: words,
      hint: None
    })
  );
}

/// A-657 (§4.17.3 decision 2; `handlebookmark`). Do: push a `bookmarks` part naming `a@b`, then one naming `a@` and a
/// node the repository lacks. Expect: the first refused in 7.2's words for a divergent bookmark; `a@`, whose `@` ends
/// it, is no divergent name, so the second is refused for its unknown node instead.
#[test]
fn a_divergent_bookmark_is_refused_in_7_2s_words() {
  let view = two();
  let named = |name: &[u8], node: Node| Unbundle {
    parts: vec![part(
      0,
      Asks::Bookmarks(vec![BookmarkAt {
        name: name.to_vec(),
        node: Some(node),
      }]),
    )],
  };
  let divergent = classify(
    &named(b"a@b", [2; 20]),
    (&[], &[]),
    &view,
    (&line_at_b(), &[]),
    false,
    &mut room(),
  );
  assert_eq!(
    divergent.map_err(|refused| refused.refused()),
    Err(Refused::Abort {
      message: b"cannot accept divergent bookmark a@b!".to_vec(),
      hint: None
    })
  );
  let unknown = classify(
    &named(b"a@", [7; 20]),
    (&[], &[]),
    &view,
    (&line_at_b(), &[]),
    false,
    &mut room(),
  );
  assert_eq!(unknown, Err(Unclassified::UnknownNode { node: [7; 20] }));
}

/// A-657 (§4.17.3 decision 1). Do: push two children of `b` on `default` (a fork: the second a `--force` push's new
/// head), then a merge of them alone. Expect: the line moves to the first and a line numbered 1 is made from it at
/// `b` for the second, `return` 2 (one head more); after it, a push of the merge onto the two lines moves line 0 and
/// retires line 1, `return` -2 (one head fewer).
#[test]
fn a_fork_makes_a_line_and_a_merge_retires_one() {
  let view = two();
  let forked = [
    arrived([3; 20], ([2; 20], NULL_NODE), b"default", false),
    arrived([4; 20], ([2; 20], NULL_NODE), b"default", false),
  ];
  let unbundle = Unbundle {
    parts: vec![part(0, changegroup())],
  };
  let plan = classify(
    &unbundle,
    (&forked, &[]),
    &view,
    (&line_at_b(), &[]),
    false,
    &mut room(),
  )
  .unwrap();
  assert_eq!(
    plan.lines,
    vec![LineMove::Moved {
      line: 0,
      head: [3; 20]
    }]
  );
  assert_eq!(
    plan.created,
    vec![NewLine {
      branch: b"default".to_vec(),
      number: 1,
      head: [4; 20],
      base: 0
    }]
  );
  assert_eq!(
    plan.replied,
    vec![Replied::Changegroup {
      in_reply_to: 0,
      heads: 2
    }]
  );
  assert_a_merge_retires_a_line(&unbundle);
}

fn assert_a_merge_retires_a_line(unbundle: &Unbundle) {
  let mut view = two();
  for (node, parent) in [([3; 20], 1), ([4; 20], 1)] {
    view.changesets.push(Old {
      node,
      parents: [Some(parent), None],
      branch: b"default".to_vec(),
      closes: false,
      phase: Phase::Draft,
    });
  }
  let lines = [
    LineRead {
      branch: b"default".to_vec(),
      number: 0,
      head: Some([3; 20]),
    },
    LineRead {
      branch: b"default".to_vec(),
      number: 1,
      head: Some([4; 20]),
    },
  ];
  let merge = [arrived([5; 20], ([3; 20], [4; 20]), b"default", false)];
  let plan = classify(
    unbundle,
    (&merge, &[]),
    &view,
    (&lines, &[]),
    false,
    &mut room(),
  )
  .unwrap();
  assert_eq!(
    plan.lines,
    vec![
      LineMove::Moved {
        line: 0,
        head: [5; 20]
      },
      LineMove::Retired { line: 1 }
    ]
  );
  assert_eq!(
    plan.replied,
    vec![Replied::Changegroup {
      in_reply_to: 0,
      heads: -2
    }]
  );
}
