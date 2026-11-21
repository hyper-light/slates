//! Tests of the sliced intake (A-720) against the whole verification and keeping it replaces: the same fork row for
//! row, the same revisions passed over, a step's working set bounded by one revision's texts, and a refusal leaving
//! nothing held.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::{Intake, Intaken, KeptText, Refused, Stepped};
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
      Ok(stepped) if more(&stepped) => {}
      Ok(_) => break,
      Err(refused) => {
        intake.abandon(store).map_err(Unindexed::Fork)?;
        return Err(refused);
      }
    }
  }
  let (fork, intaken) = finished(intake);
  Ok((fork, intaken, steps))
}

/// Whether a whole changegroup's intake steps again: more to take, or done. It never wants a range of a payload it holds
/// whole.
fn more(stepped: &Stepped) -> bool {
  match stepped {
    Stepped::More => true,
    Stepped::Done => false,
    Stepped::Wanted(range) => panic!("a whole changegroup wanted {range:?}"),
  }
}

/// A done intake's fork and what it brought.
fn finished(intake: Intake) -> (HgIndex, Intaken) {
  intake.finish().ok().unwrap()
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
/// least the intake does. Expect the intake's less, apart from the `Texts` section it writes for the landing (A-767 C4,
/// the changeset's own section, O(revisions) as the record is): what it holds across steps is a node or two a
/// revision, and each step's texts are let go when it ends, where verification holds every arrived text at once.
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
  let texts = {
    let mut store = store();
    let mut index = HgIndex::new();
    let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
    fork.release(&mut store).unwrap();
    intaken.texts_charged()
  };
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
  })
  .saturating_sub(texts);
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

/// A-732, found by use: a restart's replay of hg's own history spent 130 s of CPU, its longest step 277 ms, each
/// manifest's predecessor rebuilt from its delta chain (a chain is up to its text's length of deltas, each applied over
/// a copy of the whole text). Do: take stock 7.2's bundle a step at a time. Expect no manifest's predecessor rebuilt: the
/// intake hands each append the text it kept last; and the bundle holding at least two manifests, so the count is not
/// vacuous.
#[test]
fn the_intake_hands_each_manifest_its_predecessors_text_never_rebuilding_it() {
  let part = stock();
  let verified = verify(&part, &Nothing, &mut room()).unwrap();
  assert!(
    verified.manifests.len() >= 2,
    "{}",
    verified.manifests.len()
  );
  let mut store = store();
  let mut index = HgIndex::new();
  crate::index::PREVIOUS_REBUILT.with(|count| count.set(0));
  let (fork, _, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let rebuilt = crate::index::PREVIOUS_REBUILT.with(std::cell::Cell::get);
  assert_eq!(
    rebuilt, 0,
    "manifest predecessors rebuilt from their chains"
  );
  for index in [fork, index] {
    index.release(&mut store).unwrap();
  }
}

/// Each verified revision's text and the path it is kept under (the changelog's and manifest log's NUL-led paths), by
/// node.
fn texts_and_paths(
  verified: &Verified,
) -> (
  std::collections::BTreeMap<Node, Vec<u8>>,
  std::collections::BTreeMap<Node, Vec<u8>>,
) {
  let mut texts = std::collections::BTreeMap::new();
  let mut paths = std::collections::BTreeMap::new();
  for arrived in &verified.changesets {
    texts.insert(arrived.node, arrived.text.clone());
    paths.insert(arrived.node, super::CHANGELOG_PATH.to_vec());
  }
  for arrived in &verified.manifests {
    texts.insert(arrived.node, arrived.text.clone());
    paths.insert(arrived.node, super::MANIFEST_PATH.to_vec());
  }
  for (path, revisions) in &verified.files {
    for arrived in revisions {
      texts.insert(arrived.node, arrived.text.clone());
      paths.insert(arrived.node, path.clone());
    }
  }
  (texts, paths)
}

/// Each revision of `decoded`'s groups: its delta's base and its data, by node.
fn deltas_by_node<'d>(
  decoded: &'d crate::changegroup::Changegroup<'_>,
) -> std::collections::BTreeMap<Node, (Node, &'d [u8])> {
  let mut deltas = std::collections::BTreeMap::new();
  for range in [&decoded.changelog, &decoded.manifests] {
    for revision in decoded.group(range) {
      deltas.insert(revision.node, (revision.base, revision.data));
    }
  }
  for group in &decoded.files {
    for revision in decoded.group(&group.revisions) {
      deltas.insert(revision.node, (revision.base, revision.data));
    }
  }
  deltas
}

/// A-767 C4 (the changeset log's `Texts` section, w-perf's format, agreed 2026-11-20). Do: take stock 7.2's bundle a
/// step at a time and verify it whole. Expect one kept text per revision kept, each naming its node, the BLAKE3 and
/// length of the text verification rebuilt for it, the identity of its delta's base text (none for a whole text), and
/// the place in the payload holding exactly its delta, or for a whole its raw text past the hunk it arrived in (the
/// store's contract, A-767 C1); the changelog and manifest log under their NUL-led paths.
#[test]
fn the_intake_names_each_kept_revisions_text_its_base_and_its_place() {
  let part = stock();
  let verified = verify(&part, &Nothing, &mut room()).unwrap();
  let (texts, paths) = texts_and_paths(&verified);
  let decoded =
    crate::changegroup::Changegroup::decode(&part.payload, part.version, &mut room()).unwrap();
  let deltas = deltas_by_node(&decoded);
  let mut store = store();
  let mut index = HgIndex::new();
  let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  assert_eq!(u64::try_from(intaken.texts.len()).unwrap(), intaken.kept);
  for kept in &intaken.texts {
    let text = &texts[&kept.node];
    assert_eq!(kept.identity, *blake3::hash(text).as_bytes());
    assert_eq!(kept.len, u64::try_from(text.len()).unwrap());
    assert_eq!(kept.path, paths[&kept.node]);
    let (base, data) = deltas[&kept.node];
    let base_identity = (base != crate::NULL_NODE).then(|| *blake3::hash(&texts[&base]).as_bytes());
    assert_eq!(kept.base, base_identity, "{:?}", kept.node);
    // A whole is kept as its raw text (the store's contract, A-767 C1); an edit as its delta.
    let whole = (base == crate::NULL_NODE).then_some(&text[..]);
    kept_bytes_are(&part.payload, kept, whole.unwrap_or(data));
  }
  for index in [fork, index] {
    index.release(&mut store).unwrap();
  }
}

/// The payload's bytes at `kept`'s place are `expected`.
fn kept_bytes_are(payload: &[u8], kept: &KeptText, expected: &[u8]) {
  let at = usize::try_from(kept.place.start).unwrap()..usize::try_from(kept.place.end).unwrap();
  assert_eq!(&payload[at], expected, "{:?}", kept.node);
}

/// The sectioned record `sectioned` refused by `Replay::begin` (its payload is not in it), and by kind cut short or with
/// a byte after it.
fn sectioned_refused(sectioned: &[u8], empty: &mut HgIndex, store: &mut ContentStore) {
  use crate::import::record::{self, Replay};
  assert!(matches!(
    Replay::begin(sectioned, empty, store),
    Err(record::Refused::Version { version: 6 })
  ));
  for cut in 0..sectioned.len() {
    assert!(
      Replay::begin_sectioned(&sectioned[..cut]).is_err(),
      "cut at {cut}"
    );
  }
  let mut trailing = sectioned.to_vec();
  trailing.push(0);
  assert!(matches!(
    Replay::begin_sectioned(&trailing),
    Err(record::Refused::Trailing)
  ));
}

/// A-767 C4 (a push landed on the changeset log: its `Record` section and its `Payload` section, the changegroup held
/// once). Do: take stock 7.2's bundle through the intake; write its sectioned record (`record::write_push_record`) and
/// replay it with the changegroup as its payload (`Replay::begin_sectioned`, a step at a time); replay the A-729 v2
/// record of the same push too. Expect the sectioned record a few dozen bytes, never the changegroup; both replays the
/// intake's fork row for row; the sectioned record refused by `Replay::begin` (its payload is not in it), and refused by
/// kind cut short or with a byte after it, nothing held after.
#[test]
fn a_pushs_sectioned_record_and_payload_replay_as_its_whole_record_does() {
  use crate::import::record::{self, Replay};
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let (mut fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let revisions = intaken.kept + intaken.passed_over;
  let sectioned = record::write_push_record(1 << 20, part.version);
  assert!(sectioned.len() < 64, "{} bytes", sectioned.len());
  let mut by_sections = HgIndex::new();
  let mut replay = Replay::begin_sectioned(&sectioned).unwrap();
  loop {
    match replay
      .step_sectioned(part.payload.as_slice(), &mut by_sections, &mut store)
      .unwrap()
    {
      record::Progress::More => {}
      record::Progress::Done => break,
      record::Progress::Wanted(range) => panic!("a whole payload wanted {range:?}"),
    }
  }
  replay.finish(&mut by_sections, &mut store).unwrap();
  let whole = record::write_changegroup(1 << 20, (part.version, &part.payload), revisions).unwrap();
  let mut by_whole = HgIndex::new();
  record::replay(&whole, &mut by_whole, &mut store).unwrap();
  let expected = rows(&mut fork, &store);
  assert_eq!(rows(&mut by_sections, &store), expected);
  assert_eq!(rows(&mut by_whole, &store), expected);
  let held = store.len();
  let mut empty = HgIndex::new();
  sectioned_refused(&sectioned, &mut empty, &mut store);
  assert_eq!(store.len(), held, "a refused replay held something");
  for index in [by_sections, by_whole, fork, index, empty] {
    index.release(&mut store).unwrap();
  }
}

/// A-767 C4 (the register's reference to a push's changeset). Do: write the reference of stock 7.2's push as changeset
/// 5, read it back, and hand it to `Replay::begin`. Expect it read back as written (the room, the changegroup's
/// version, its payload named by the BLAKE3 of the changegroup's bytes and their length, the sequence), its record the
/// sectioned head; a cut or a byte after it not read as a reference; and `Replay::begin` refusing it by version, its
/// payload being the log's.
#[test]
fn a_pushs_reference_names_its_changeset_and_payload_by_identity() {
  use crate::import::record::{self, Reference, Replay};
  let part = stock();
  let bytes = record::write_push_reference(1 << 20, (part.version, &part.payload), 5);
  let read = record::reference_of(&bytes).unwrap();
  assert_eq!(
    read,
    Reference {
      room: 1 << 20,
      version: part.version,
      payload: *blake3::hash(&part.payload).as_bytes(),
      len: u64::try_from(part.payload.len()).unwrap(),
      sequence: 5,
      made: None,
    }
  );
  assert_eq!(
    read.record(),
    record::write_push_record(1 << 20, part.version)
  );
  for cut in 0..bytes.len() {
    assert_eq!(record::reference_of(&bytes[..cut]), None, "cut at {cut}");
  }
  let mut trailing = bytes.clone();
  trailing.push(0);
  assert_eq!(record::reference_of(&trailing), None);
  let mut store = store();
  let mut index = HgIndex::new();
  assert!(matches!(
    Replay::begin(&bytes, &mut index, &mut store),
    Err(record::Refused::Version { version: 7 })
  ));
}

/// Takes `part` into a fork of `index` from a window filled `piece` bytes at a time (A-767 C1, the piece-reading
/// intake): framed first, each want put in from the changegroup's bytes, then a step at a time, each want put in. The
/// fork, what was brought, how many wants were met, and the most bytes the window held at once.
fn intake_from_pieces(
  part: &ChangegroupPart,
  index: &mut HgIndex,
  store: &mut ContentStore,
  piece: usize,
) -> (HgIndex, Intaken, u64, u64) {
  use crate::framing::{Collect, Framed, Framing, Halt, Source, Window};
  let whole = part.payload.as_slice();
  let mut window = Window::new(Source::len(whole));
  let (mut wants, mut most) = (0_u64, 0_u64);
  let fill =
    |window: &mut Window, range: core::ops::Range<u64>, wants: &mut u64, most: &mut u64| {
      *wants += 1;
      window.keep_from(range.start);
      while window.end() < range.end {
        let from = usize::try_from(window.end()).unwrap();
        let to = (from + piece).min(whole.len());
        window.put(&whole[from..to], &mut room()).unwrap();
      }
      *most = (*most).max(window.held());
    };
  let mut framing = Framing::new(part.version, false);
  let mut framed = Framed::default();
  loop {
    let mut sink = Collect {
      framed: &mut framed,
      admission: &mut room(),
    };
    match framing.step(&window, &mut sink) {
      Ok(true) => {}
      Ok(false) => break,
      Err(Halt::Wanted(wanted)) => fill(&mut window, wanted.range, &mut wants, &mut most),
      Err(Halt::Refused(refusal)) => panic!("{refusal:?}"),
    }
  }
  let mut intake = Intake::begin_framed(framed, index, store).unwrap();
  loop {
    match intake.step(&window, store, &mut room()).unwrap() {
      Stepped::More => {}
      Stepped::Done => break,
      Stepped::Wanted(range) => fill(&mut window, range, &mut wants, &mut most),
    }
  }
  let (fork, intaken) = intake.finish().ok().unwrap();
  (fork, intaken, wants, most)
}

/// A-767 C1 (the piece-reading intake; R2: a replay holding no payload whole). Do: take stock 7.2's push whole, and
/// again from a window filled 64 bytes at a time. Expect the same fork row for row and the same texts kept, each named
/// at the same place; the window wanted bytes, and held at most its longest delta and a header past a piece, never the
/// whole changegroup.
#[test]
fn a_changegroup_taken_from_pieces_keeps_what_the_whole_one_keeps() {
  let part = stock();
  let mut whole_store = store();
  let mut whole_index = HgIndex::new();
  let (mut whole_fork, whole_intaken, _) =
    intake(&part, &mut whole_index, &mut whole_store, &mut room()).unwrap();
  let mut piece_store = store();
  let mut piece_index = HgIndex::new();
  let (mut piece_fork, piece_intaken, wants, most) =
    intake_from_pieces(&part, &mut piece_index, &mut piece_store, 64);
  assert_eq!(
    rows(&mut piece_fork, &piece_store),
    rows(&mut whole_fork, &whole_store)
  );
  assert_eq!(piece_intaken, whole_intaken);
  assert!(wants > 1, "the window wanted bytes {wants} times");
  let longest = whole_intaken
    .texts
    .iter()
    .map(|kept| kept.place.end - kept.place.start)
    .max()
    .unwrap_or(0);
  let header = crate::read::to_u64(part.version.header_len() + 8);
  assert!(
    most <= longest + header + 64,
    "held {most} of {} at once",
    part.payload.len()
  );
  assert!(most < crate::read::to_u64(part.payload.len()));
}

/// A-767 C1 (R2: a replay holding no payload whole). Do: write stock 7.2's push's sectioned record and replay it with
/// its payload read from a window filled 64 bytes at a time (`Replay::step_sectioned`, each want put in from the
/// payload). Expect the fork row for row the whole intake's, and the window never holding the whole payload: at most
/// its longest delta and a chunk's header past a piece.
#[test]
fn a_pushs_changeset_replays_from_its_payload_a_piece_at_a_time() {
  use crate::framing::{Source, Window};
  use crate::import::record::{self, Progress, Replay};
  let part = stock();
  let whole = part.payload.as_slice();
  let mut store = store();
  let mut index = HgIndex::new();
  let (mut fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  let sectioned = record::write_push_record(1 << 20, part.version);
  let mut by_pieces = HgIndex::new();
  let mut replay = Replay::begin_sectioned(&sectioned).unwrap();
  let mut window = Window::new(Source::len(whole));
  let (mut wants, mut most) = (0_u64, 0_u64);
  loop {
    match replay
      .step_sectioned(&window, &mut by_pieces, &mut store)
      .unwrap()
    {
      Progress::More => {}
      Progress::Done => break,
      Progress::Wanted(range) => {
        wants += 1;
        window.keep_from(range.start);
        while window.end() < range.end {
          let from = usize::try_from(window.end()).unwrap();
          window
            .put(&whole[from..(from + 64).min(whole.len())], &mut room())
            .unwrap();
        }
        most = most.max(window.held());
      }
    }
  }
  replay.finish(&mut by_pieces, &mut store).unwrap();
  assert_eq!(rows(&mut by_pieces, &store), rows(&mut fork, &store));
  assert!(wants > 1, "wanted {wants} times");
  let longest = intaken
    .texts
    .iter()
    .map(|kept| kept.place.end - kept.place.start)
    .max()
    .unwrap_or(0);
  let header = crate::read::to_u64(part.version.header_len() + 8);
  assert!(
    most <= longest + header + 64,
    "held {most} of {}",
    whole.len()
  );
  for index in [by_pieces, fork, index] {
    index.release(&mut store).unwrap();
  }
}

/// The revisions of `decoded` whose delta's base is neither the null node nor the revision before it in its group: the
/// bases an intake does not hold as the text it kept last, so must read back from the fork.
fn bases_not_kept_last(decoded: &crate::changegroup::Changegroup<'_>) -> u64 {
  let mut groups = vec![
    decoded.group(&decoded.changelog),
    decoded.group(&decoded.manifests),
  ];
  groups.extend(
    decoded
      .files
      .iter()
      .map(|group| decoded.group(&group.revisions)),
  );
  groups
    .iter()
    .map(|group| {
      let mut previous = crate::NULL_NODE;
      group
        .iter()
        .filter(|revision| {
          let far = revision.base != crate::NULL_NODE && revision.base != previous;
          previous = revision.node;
          far
        })
        .count() as u64
    })
    .sum()
}

/// Main's ruling of 2026-11-21 05:12 (c), red first (w-perf's profile of hg-src's first push, base 94c1696bb: 57% of the
/// shard's time in checking each new manifest's files, which rebuilt the manifest's text and its first parent's from
/// their chains, two whole texts a manifest, the push's memory 5.8 GiB at its end). Do: take stock 7.2's bundle through
/// the intake. Expect a whole text rebuilt from the fork's chains only for a delta whose base is not the text kept just
/// before it in its group; none to check a new manifest's files, which reads the lines its delta wrote (14 rebuilt
/// before, two a manifest).
#[test]
fn a_pushs_intake_rebuilds_a_text_only_for_a_base_it_does_not_hold() {
  let part = stock();
  let decoded =
    crate::changegroup::Changegroup::decode(&part.payload, part.version, &mut room()).unwrap();
  let mut index = HgIndex::new();
  let mut store = store();
  let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  assert!(!intaken.changesets.is_empty(), "{intaken:?}");
  assert_eq!(
    intaken.rebuilt,
    bases_not_kept_last(&decoded),
    "texts rebuilt from the fork's chains"
  );
  fork.release(&mut store).unwrap();
}

/// Main's ruling of 2026-11-21 05:12 (a), the refusal side (hostile input): a new manifest's files are checked from
/// the lines its delta wrote, so a manifest naming a file revision nobody holds is still refused. Do: take stock 7.2's
/// bundle through the intake with every file group left out, its manifests naming file revisions neither the
/// repository nor the push holds. Expect it refused `MissingFile`.
#[test]
fn a_manifest_naming_a_file_revision_nobody_holds_is_refused() {
  let part = stock();
  let mut decoded =
    crate::changegroup::Changegroup::decode(&part.payload, part.version, &mut room()).unwrap();
  decoded.files.clear();
  let mut payload = Vec::new();
  decoded.encode(&mut payload).unwrap();
  let fileless = ChangegroupPart {
    payload,
    ..part.clone()
  };
  let mut index = HgIndex::new();
  let mut store = store();
  let refused = intake(&fileless, &mut index, &mut store, &mut room()).err();
  assert!(
    matches!(
      refused,
      Some(Refused::Unverified(Unverified::MissingFile { .. }))
    ),
    "{refused:?}"
  );
}
