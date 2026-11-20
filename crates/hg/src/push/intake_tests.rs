//! Tests of the sliced intake (A-720) against the whole verification and keeping it replaces: the same fork row for
//! row, the same revisions passed over, a step's working set bounded by one revision's texts, and a refusal leaving
//! nothing held.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::{Intake, Intaken, Refused, Stepped};
use crate::bundle2::Bundle2;
use crate::fixtures::CG03;
use crate::index::{HgIndex, Log};
use crate::push::commit::{Unindexed, fork_with};
use crate::push::verify::{Held, Unverified, Verified, verify};
use crate::push::{ChangegroupPart, read};
use crate::{Admission, Node};

/// The window the tests' stores are made with: room for a few rows a table, as the import's tests make theirs.
const WINDOW: usize = 1 << 12;

/// Room enough for anything a test holds.
fn room() -> Admission {
  Admission::new(u64::MAX)
}

fn store() -> ContentStore {
  ContentStore::new(1 << 24, WINDOW).unwrap()
}

/// Nothing held: an empty repository.
struct Nothing;

impl Held for Nothing {
  fn changeset_text(&self, _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }

  fn manifest_text(&self, _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }

  fn file_text(&self, _path: &[u8], _node: &Node) -> Option<Cow<'_, [u8]>> {
    None
  }
}

/// The changegroup part of stock 7.2's bundle.
fn stock() -> ChangegroupPart {
  let bundle = Bundle2::decode(CG03, &mut room()).unwrap();
  let unbundle = read(&bundle, &mut room()).unwrap();
  unbundle.changegroup().unwrap().1.clone()
}

/// Every unit of `part` taken into a fork of `index`, a step at a time, each within `admission`: the fork and what was
/// brought, and the steps taken.
fn intake(
  part: &ChangegroupPart,
  index: &mut HgIndex,
  store: &mut ContentStore,
  admission: &mut Admission,
) -> Result<(HgIndex, Intaken, u64), Refused> {
  let mut intake = Intake::begin(part, index, store, admission)?;
  let mut steps = 0_u64;
  loop {
    steps += 1;
    match intake.step(store, admission) {
      Ok(Stepped::More) => {}
      Ok(Stepped::Done) => break,
      Err(refused) => {
        intake.abandon(store).map_err(Unindexed::Fork)?;
        return Err(refused);
      }
    }
  }
  let (fork, intaken) = intake.finish().ok().unwrap();
  Ok((fork, intaken, steps))
}

/// An index's rows, each key with its body.
fn rows(index: &mut HgIndex, store: &ContentStore) -> Vec<(Vec<u8>, Vec<u8>)> {
  index
    .rows()
    .iter(store)
    .map(|row| (row.key().to_vec(), row.body().to_vec()))
    .collect()
}

/// A-720 (oracle: the whole verification and keeping it replaces). Do: take stock 7.2's bundle into a fork of an empty
/// index through the intake, a step at a time, and keep it through `verify` and `fork_with` into a fork of another;
/// then take it again into a fork of the first fork. Expect: the two forks equal row for row; the intake's new
/// changesets the verification's, in order; at least one step per revision; and the second intake bringing nothing,
/// every revision passed over and every changeset listed as held, in the bundle's order.
#[test]
fn the_intake_keeps_what_verifying_then_keeping_keeps_a_revision_a_step() {
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let (mut fork, intaken, steps) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let verified = verify(&part, &Nothing, &mut room()).unwrap();
  let mut whole_index = HgIndex::new();
  let mut whole = fork_with(&mut whole_index, &mut store, &verified, &mut room()).unwrap();
  assert_eq!(rows(&mut fork, &store), rows(&mut whole, &store));
  check_brought(&intaken, &verified, steps);
  let (again, nothing, _) = intake(&part, &mut fork, &mut store, &mut room()).unwrap();
  assert_eq!(nothing.kept, 0);
  assert_eq!(nothing.passed_over, intaken.kept);
  assert_eq!(nothing.held_changesets, intaken.changesets);
  for index in [again, fork, whole, whole_index, index] {
    index.release(&mut store).unwrap();
  }
}

fn check_brought(intaken: &Intaken, verified: &Verified, steps: u64) {
  let nodes: Vec<Node> = verified
    .changesets
    .iter()
    .map(|arrived| arrived.node)
    .collect();
  assert_eq!(intaken.changesets, nodes);
  let total = verified.changesets.len()
    + verified.manifests.len()
    + verified
      .files
      .iter()
      .map(|(_, revisions)| revisions.len())
      .sum::<usize>();
  assert_eq!(intaken.kept, u64::try_from(total).unwrap());
  assert!(
    steps >= intaken.kept,
    "{steps} steps for {} revisions",
    intaken.kept
  );
}

/// A-720 (the working set). Do: find the least admission the whole verification takes stock 7.2's bundle within, and the
/// least the intake does. Expect the intake's less: what it holds across steps is a node or two a revision, and each
/// step's texts are let go when it ends, where verification holds every arrived text at once.
#[test]
fn the_intake_holds_less_than_verification_whole() {
  let part = stock();
  let least = |takes: &dyn Fn(u64) -> bool| {
    let (mut low, mut high) = (0_u64, 1_u64 << 24);
    while high - low > 1 {
      let middle = low + (high - low) / 2;
      if takes(middle) {
        high = middle;
      } else {
        low = middle;
      }
    }
    high
  };
  let verifying = least(&|room| verify(&part, &Nothing, &mut Admission::new(room)).is_ok());
  let taking = least(&|room| {
    let mut store = store();
    let mut index = HgIndex::new();
    match intake(&part, &mut index, &mut store, &mut Admission::new(room)) {
      Ok((fork, ..)) => {
        fork.release(&mut store).unwrap();
        true
      }
      Err(_) => false,
    }
  });
  assert!(
    taking < verifying,
    "the intake took {taking}, verification {verifying}"
  );
}

/// A-720 (all of it or none). Do: take stock 7.2's bundle through the intake into a store whose budget is a few hundred
/// bytes. Expect it refused by kind, the index still empty and the store holding nothing once the fork is given back.
#[test]
fn a_refused_intake_leaves_the_index_and_the_store_as_they_were() {
  let mut store = ContentStore::new(1 << 9, WINDOW).unwrap();
  let mut index = HgIndex::new();
  let refused = intake(&stock(), &mut index, &mut store, &mut room());
  assert!(
    matches!(
      refused,
      Err(Refused::Unindexed(_) | Refused::Unverified(Unverified::PastAdmission(_)))
    ),
    "{refused:?}"
  );
  assert_eq!(index.len(&store, Log::Changelog), 0);
  assert_eq!(store.len(), 0, "a refused intake kept a hold");
}

/// A-720 part 2 (the record from the fork). Do: take stock 7.2's bundle through the intake, then write its record from
/// the fork (`record::write_kept`) and from the whole verification (`record::write_push`), and read its changesets back
/// from the fork's rows. Expect the two records equal byte for byte, and each changeset read back with the parents and
/// branch its text names.
#[test]
fn the_record_and_the_changesets_read_from_the_fork_are_the_whole_verifications() {
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let verified = verify(&part, &Nothing, &mut room()).unwrap();
  let from_fork = crate::import::record::write_kept(&fork, &store, &intaken.logs, 1 << 20).unwrap();
  let whole = crate::import::record::write_push(&verified, 1 << 20).unwrap();
  assert_eq!(from_fork, whole);
  let read = super::read_changesets(&fork, &store, &intaken, &mut room())
    .unwrap()
    .unwrap();
  for (read, arrived) in read.iter().zip(&verified.changesets) {
    let text = crate::changeset::decode(&arrived.text).unwrap();
    let (branch, closes) = crate::import::branch_of(&text, &mut room()).unwrap();
    assert_eq!(
      (read.node, read.p1, read.p2),
      (arrived.node, arrived.p1, arrived.p2)
    );
    assert_eq!((read.branch.clone(), read.closes), (branch, closes));
  }
  assert_eq!(read.len(), verified.changesets.len());
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
}
