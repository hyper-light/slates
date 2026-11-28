//! A commit's walk held to its own moves (P2, CLAUDE.md §3 bounded work; A-722 part 4e's owed "store commit's whole
//! passes"): what a commit reads to make and charge its versions is its moved greens' own records, whatever the
//! repository's other greens and their histories hold, and a commit refused at any point after its plan leaves the
//! repository and its store as they were.

use std::collections::BTreeSet;

use crate::object_name::{ObjectFormat, ObjectId};
use crate::objects::tree::Mode;

use super::{
  CommitPoint, Made, Repository, blob, commit, push_and_move, push_and_move_refused_first, store,
  tree,
};

/// A push's moves: each green it moves, by key, and the head it moves the green to (none: the green deleted).
type Moves = Vec<(u64, Option<ObjectId>)>;

/// Shape: the greens the long-lived repository holds, each moved by every push of its line.
const GREENS: u64 = 1_000;

/// Shape: the commits of the line, a version of every green each.
const LINE: u64 = 20;

/// A line of `length` commits over one tree, each push's objects: the blob, the tree and the first commit, then a
/// commit each.
fn line(format: ObjectFormat, length: u64) -> Vec<Vec<Made>> {
  let file = blob(format, b"line\n");
  let root = tree(format, &[(Mode::File, b"file", file.id)]);
  let mut parent: Option<ObjectId> = None;
  let mut pushes = Vec::new();
  for seconds in 0..length {
    let made = commit(format, root.id, parent.as_slice(), seconds);
    parent = Some(made.id);
    pushes.push(if seconds == 0 {
      vec![file.clone(), root.clone(), made]
    } else {
      vec![made]
    });
  }
  pushes
}

/// The head of a push of `line`: its commit, the last object it holds.
fn head_of(objects: &[Made]) -> ObjectId {
  objects.last().map(|made| made.id).unwrap()
}

/// P2 (CLAUDE.md §3: a step over user-scaled data is bounded). Do: move 1,000 greens together along a line of 20
/// commits, every green moved by each push, then push one commit more and move one green to it. Expect: that commit
/// reads three chain records and head entries, the version it pushes and the head entry it moves out and in, whatever
/// the other greens' 19,980 versions and 999 head entries hold; and the store's charge then equal to the whole
/// recomputation. Red before: the commit's charge summed every chain and cloned the head map, about 22,000.
#[test]
fn a_commit_reads_its_own_moves_records_whatever_the_repositorys_other_greens_hold() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let mut pushes = line(format, LINE.saturating_add(1));
  let last = pushes.pop().unwrap();
  for objects in pushes {
    let moves: Moves = (1..=GREENS)
      .map(|green| (green, Some(head_of(&objects))))
      .collect();
    push_and_move(&mut repository, &mut store, objects, &moves).unwrap();
  }
  let walked = repository.counters().commit_walked;
  push_and_move(
    &mut repository,
    &mut store,
    last.clone(),
    &[(1, Some(head_of(&last)))],
  )
  .unwrap();
  let walked = repository.counters().commit_walked.saturating_sub(walked);
  assert!(
    walked <= 3,
    "the commit read {walked} chain records and head entries for its one move"
  );
  repository.assert_charged_whole();
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// P2, main 01a20182's condition of 2026-11-27 19:22 (a version pushed in place and the head map moved must be put
/// back exactly by any refusal after the plan). Do: make a green, fast-forward it with a commit and a new file, make a
/// second green at the first's head and fast-forward both at once, each push refused first at each point after its
/// plan in turn. Expect: every refusal leaves the repository's records and its store's bytes as they were (the
/// helper's check), every point refused at least once, and each push committed after.
#[test]
fn a_commit_refused_at_each_point_after_its_plan_leaves_the_store_as_it_was() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let pushes = line(format, 3);
  let mut refused = BTreeSet::new();
  let heads: Vec<ObjectId> = pushes.iter().map(|objects| head_of(objects)).collect();
  let histories: [(&[Made], Moves); 3] = [
    (&pushes[0], vec![(1, Some(heads[0]))]),
    (&pushes[1], vec![(1, Some(heads[1]))]),
    (&pushes[2], vec![(1, Some(heads[2])), (2, Some(heads[2]))]),
  ];
  for (objects, moves) in histories {
    let (points, _) = push_and_move_refused_first(&mut repository, &mut store, objects, &moves);
    refused.extend(points);
    repository.assert_charged_whole();
  }
  assert_eq!(refused, BTreeSet::from(CommitPoint::ALL));
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}
