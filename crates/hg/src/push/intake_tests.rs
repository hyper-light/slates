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
  let mut intake = Intake::begin((part.version, &part.payload), index, store, admission)?;
  let mut steps = 0_u64;
  loop {
    steps += 1;
    match intake.step(&part.payload, store, admission) {
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

/// Every row of `rebase` re-appended, a step at a time.
fn rebased(mut rebase: super::Rebase, store: &mut ContentStore) -> (HgIndex, Intaken) {
  while rebase.step(store).unwrap() == Stepped::More {}
  rebase.finish(store).unwrap()
}

/// A-720 part 2b (a stale fork re-based by node). Do: take stock 7.2's bundle into a fork of an empty index; then re-base
/// that fork onto a fork of another empty index (the index moved, holding nothing new), and onto an index that already
/// landed the same bundle (another push brought it first). Expect: the first re-base the fork row for row, every
/// revision kept again and its record the same; the second bringing nothing, every revision passed over and every
/// changeset counted held, the index's rows unchanged.
#[test]
fn a_stale_fork_is_re_based_by_node_onto_the_index_as_it_is_now() {
  let part = stock();
  let mut store = store();
  let mut empty = HgIndex::new();
  let (mut stale, intaken, _) = intake(&part, &mut empty, &mut store, &mut room()).unwrap();
  let expected = rows(&mut stale, &store);
  let record = crate::import::record::write_kept(&stale, &store, &intaken.logs, 1 << 20).unwrap();
  let mut moved = HgIndex::new();
  let rebase = super::Rebase::begin(stale, &intaken, &mut moved, &mut store)
    .ok()
    .unwrap();
  let (mut fork, again) = rebased(rebase, &mut store);
  assert_eq!(rows(&mut fork, &store), expected);
  assert_eq!(
    (again.kept, &again.changesets),
    (intaken.kept, &intaken.changesets)
  );
  let rerecord = crate::import::record::write_kept(&fork, &store, &again.logs, 1 << 20).unwrap();
  assert_eq!(rerecord, record);
  let (stale, _, _) = intake(&part, &mut empty, &mut store, &mut room()).unwrap();
  let rebase = super::Rebase::begin(stale, &intaken, &mut fork, &mut store)
    .ok()
    .unwrap();
  let (mut over, nothing) = rebased(rebase, &mut store);
  assert_eq!(rows(&mut over, &store), expected);
  assert_eq!(nothing.kept, 0);
  assert_eq!(nothing.held_changesets, intaken.changesets);
  for index in [over, fork, moved, empty] {
    index.release(&mut store).unwrap();
  }
  assert_eq!(store.len(), 0, "a hold was kept");
}

/// A-732 (found by use, w-interop: a 57,035-revision push over HTTP spun its shard past the watchdog's window at its
/// landing, its record written whole there). Do: take stock 7.2's bundle through the intake, then write its record a
/// step at a time with `record::Writer::begin_kept`. Expect at least one step per revision kept, and the record the one
/// `write_kept` writes whole, byte for byte.
#[test]
fn a_pushs_record_is_written_a_revision_a_step() {
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let mut writer = crate::import::record::Writer::begin_kept(&intaken.logs, 1 << 20).unwrap();
  let mut steps = 1_u64;
  while writer.step(&fork, &store).unwrap() {
    steps += 1;
  }
  let whole = crate::import::record::write_kept(&fork, &store, &intaken.logs, 1 << 20).unwrap();
  assert_eq!(writer.finish(), Some(whole));
  assert!(
    steps > intaken.kept,
    "{steps} steps for {} revisions",
    intaken.kept
  );
  fork.release(&mut store).unwrap();
  index.release(&mut store).unwrap();
}

/// `part`'s record written a part a step (`record::ChangegroupWriter`) is `whole`, the record written at once, and took
/// more than one step.
fn assert_written_a_part_a_step_as_whole(part: &ChangegroupPart, revisions: u64, whole: &[u8]) {
  use crate::import::record::ChangegroupWriter;
  let mut writer =
    ChangegroupWriter::begin(1 << 20, (part.version, part.payload.len()), revisions).unwrap();
  let mut steps = 1_u64;
  while writer.step(&part.payload).unwrap() {
    steps += 1;
  }
  assert_eq!(writer.finish().as_deref(), Some(whole));
  assert!(steps > 1, "the record was copied in one step");
}

/// The record `whole` cut a byte short is refused `Truncated`, and with a byte after it `Trailing`.
fn assert_cut_and_trailing_records_refused(whole: &[u8], store: &mut ContentStore) {
  use crate::import::record::{self, Refused as RecordRefused};
  let cut = record::replay(&whole[..whole.len() - 1], &mut HgIndex::new(), store);
  assert_eq!(cut.err(), Some(RecordRefused::Truncated));
  let mut trailing = whole.to_vec();
  trailing.push(0);
  let after = record::replay(&trailing, &mut HgIndex::new(), store);
  assert_eq!(after.err(), Some(RecordRefused::Trailing));
}

/// A-729 part 2 (a push's record holds its changegroup as it arrived). Do: take stock 7.2's bundle through the intake,
/// write its record from the changegroup a part a step (`record::ChangegroupWriter`) and whole
/// (`record::write_changegroup`), and replay it into another empty index (`record::replay`, which takes the changegroup
/// again through the intake). Expect the two writings equal byte for byte, the record the changegroup and its head;
/// the replayed index the intake's fork row for row, with the changesets and revisions counted; and a record cut short or with a byte after it refused by kind.
#[test]
fn a_pushs_record_holds_its_changegroup_and_replays_through_the_intake() {
  use crate::import::record;
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let (mut fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let revisions = intaken.kept + intaken.passed_over;
  let whole = record::write_changegroup(1 << 20, (part.version, &part.payload), revisions).unwrap();
  assert_written_a_part_a_step_as_whole(&part, revisions, &whole);
  // A delta's header is larger than this fixture's few-byte texts, so the record is held to the changegroup's size,
  // not to the texts' (the saving is a large history's: each text there is a short delta against its parent's).
  assert!(whole.len() < part.payload.len() + 64);
  let mut replayed_index = HgIndex::new();
  let replayed = record::replay(&whole, &mut replayed_index, &mut store).unwrap();
  assert_eq!(rows(&mut replayed_index, &store), rows(&mut fork, &store));
  assert_eq!(
    u64::from(replayed.changesets),
    u64::try_from(intaken.changesets.len()).unwrap()
  );
  assert_eq!(
    u64::from(replayed.changesets) + u64::from(replayed.manifests) + replayed.file_revisions,
    intaken.kept
  );
  assert_cut_and_trailing_records_refused(&whole, &mut store);
  for index in [replayed_index, fork, index] {
    index.release(&mut store).unwrap();
  }
  assert_eq!(store.len(), 0, "a hold was kept");
}
