//! Tests of the sliced intake (A-720) against the whole verification and keeping it replaces: the same fork row for
//! row, the same revisions passed over, a step's working set bounded by one revision's texts, and a refusal leaving
//! nothing held.

use std::borrow::Cow;

use slates_vcs::ContentStore;

use super::{Intake, Intaken, KeptText, PushSegment, Refused, Stepped};
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
  /// Texts in memory: no content store holds their bytes, so a push names no file for them.
  fn stores_bytes(&self) -> bool {
    false
  }

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
      // A test keeps no changeset store: the fork's own texts stand in for it.
      Ok(Stepped::Store) => intake.answer_from_fork(store),
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
    Stepped::Store => panic!("a base asked of the store where none is kept"),
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

/// Main's routing of 2026-11-22 (w-svn's sibling sweep of codex-review's refused-bring leak, svn's fixed in its own
/// writer). Do: take a changeset whose one file is two windows and a byte into a store whose budget is met once the
/// file's two whole windows are written, so its last step's finish is refused; give the intake back. Expect the step
/// refused by kind, and the store holding neither window, nor anything, once the fork is given back. Red before: the
/// finish's writer was dropped with its two windows held, and nothing read them.
#[test]
fn a_refused_finish_gives_its_written_windows_back() {
  let text: Vec<u8> = (0..2 * WINDOW + 1)
    .map(|at| u8::try_from(at % 251).unwrap().saturating_add(2))
    .collect();
  let (made, _) = crate::push::work_tests::Made3::one(&crate::NULL_NODE, &[("big", &text, "")]);
  let part = made.part();
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = room();
  let mut intake = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap();
  let windows = [
    slates_vcs::ContentId::of(&text[..WINDOW]),
    slates_vcs::ContentId::of(&text[WINDOW..2 * WINDOW]),
  ];
  let refused = loop {
    // Both whole windows written: no room for the last byte's.
    if store.contains(&windows[1]) {
      store.set_budget(store.used());
    }
    match intake.step(&part.payload, &mut store, &mut admission) {
      Ok(Stepped::More) => {}
      Ok(other) => panic!("the intake went on past its room: {other:?}"),
      Err(refused) => break refused,
    }
  };
  intake.abandon(&mut store).unwrap();
  assert!(matches!(refused, Refused::Unindexed(_)), "{refused:?}");
  assert_eq!(
    (
      store.contains(&windows[0]),
      store.contains(&windows[1]),
      store.len()
    ),
    (false, false, 0),
    "a refused finish kept its windows"
  );
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
      record::Progress::Store => replay.answer_from_fork(&store),
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
  let bytes = record::write_push_reference(
    1 << 20,
    (part.version, record::PayloadName::of(&part.payload)),
    5,
  );
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
  let mut intake = Intake::begin_framed(framed, index, store, u64::MAX).unwrap();
  loop {
    match intake.step(&window, store, &mut room()).unwrap() {
      Stepped::More => {}
      Stepped::Done => break,
      Stepped::Wanted(range) => fill(&mut window, range, &mut wants, &mut most),
      Stepped::Store => intake.answer_from_fork(store),
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
      Progress::Store => replay.answer_from_fork(&store),
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

/// A changeset a test made: its node, its two parents, its manifest text and its changelog entry.
type Made = (Node, Node, Node, Vec<u8>, Vec<u8>);

/// The changesets of `decoded` whose delta's base is neither the null node nor the changeset before it: the bases an
/// intake does not hold as the text it kept last, so rebuilds from the fork (a manifest's and a file revision's are read
/// over the fork's rows a window at a time since C1 (2), never rebuilt whole).
fn bases_not_kept_last(decoded: &crate::changegroup::Changegroup<'_>) -> u64 {
  let groups = [decoded.group(&decoded.changelog)];
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
/// the intake. Expect a whole text rebuilt from the fork's chains only for a changeset delta whose base is not the text
/// kept just before it (a manifest's and a file revision's base read over the fork's rows a window at a time since C1
/// (2)); none to check a new manifest's files, which reads the lines its delta wrote (14 rebuilt before, two a manifest).
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

/// A line of changesets' last changeset, manifest and each file revision's node with its text, for a push after it.
type PushedLine = (Node, Node, Vec<(Node, Vec<u8>)>);

/// One file `f` whose revisions are pushed as `deltas` name them (each its text and the revision it is a delta against,
/// none for the null base), each in a changeset of its own on one line of changesets after `parent` (`after`: the
/// changeset, manifest and file revisions before them, empty for a first push): the changegroup in version 03, and the
/// line's last changeset, manifest and each file revision's node, for a push after it.
fn file_push(
  texts: &[(&[u8], Option<usize>)],
  after: (Node, Node, &[(Node, Vec<u8>)]),
) -> (Vec<u8>, PushedLine) {
  use crate::changegroup::{self as cg, Revision, Version};
  use crate::{NULL_NODE, delta, node};
  let hex = |node: &Node| {
    let mut out = Vec::new();
    crate::pushkey::encode_hex(node, &mut out);
    out
  };
  let (mut changeset, mut manifest, before) = after;
  let mut files: Vec<(Node, Vec<u8>)> = before.to_vec();
  let mut changesets = Vec::new();
  let mut manifests = Vec::new();
  let mut revisions = Vec::new();
  for (text, base) in texts {
    let p1 = files.last().map_or(NULL_NODE, |(node, _)| *node);
    let file = node::compute(&p1, &NULL_NODE, text).unwrap();
    let manifest_text = [&b"f\0"[..], &hex(&file), b"\n"].concat();
    let manifest_node = node::compute(&manifest, &NULL_NODE, &manifest_text).unwrap();
    let changeset_text = [&hex(&manifest_node)[..], b"\ntest\n0 0\nf\n\nchange"].concat();
    let changeset_node = node::compute(&changeset, &NULL_NODE, &changeset_text).unwrap();
    changesets.push((changeset_node, changeset, changeset_text));
    manifests.push((manifest_node, manifest, manifest_text, changeset_node));
    let base = base.map(|at| files[at].clone());
    revisions.push((file, p1, text.to_vec(), base, changeset_node));
    files.push((file, text.to_vec()));
    (changeset, manifest) = (changeset_node, manifest_node);
  }
  let whole = |(node, p1, link): (Node, Node, Node), text: &[u8], out: &mut Vec<u8>| {
    let revision = Revision {
      node,
      p1,
      p2: NULL_NODE,
      base: NULL_NODE,
      link,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data: text,
      sidedata: None,
    };
    cg::put_revision_whole(&revision, Version::V03, out).unwrap();
  };
  let mut out = Vec::new();
  for (node, p1, text) in &changesets {
    whole((*node, *p1, *node), text, &mut out);
  }
  cg::put_end(&mut out);
  for (node, p1, text, link) in &manifests {
    whole((*node, *p1, *link), text, &mut out);
  }
  cg::put_end(&mut out);
  cg::put_end(&mut out);
  cg::put_name(b"f", &mut out).unwrap();
  for (node, p1, text, base, link) in &revisions {
    match base {
      None => whole((*node, *p1, *link), text, &mut out),
      Some((base, base_text)) => {
        let mut data = Vec::new();
        delta::encode_lines(base_text, text, &mut data).unwrap();
        let revision = Revision {
          node: *node,
          p1: *p1,
          p2: NULL_NODE,
          base: *base,
          link: *link,
          flags: 0,
          info: None,
          protocol_flags: 0,
          data: &data,
          sidedata: None,
        };
        cg::put_revision(&revision, Version::V03, &mut out).unwrap();
      }
    }
  }
  cg::put_end(&mut out);
  cg::put_end(&mut out);
  (out, (changeset, manifest, files))
}

/// Takes `payload`, a changegroup of version 03, into a fork of `index`, each store want answered with `given` where
/// given, else from the fork's own texts (the tests keep no changeset store): the fork, what was brought, and how many
/// times the store was asked.
fn taken_answering(
  payload: &[u8],
  (index, store): (&mut HgIndex, &mut ContentStore),
  given: Option<&[u8]>,
) -> (HgIndex, Intaken, u64) {
  let mut intake = Intake::begin(
    (crate::changegroup::Version::V03, payload),
    index,
    store,
    &mut room(),
  )
  .unwrap();
  let mut asked = 0;
  loop {
    match intake.step(payload, store, &mut room()).unwrap() {
      Stepped::More => {}
      Stepped::Done => break,
      Stepped::Wanted(range) => panic!("a whole changegroup wanted {range:?}"),
      Stepped::Store => {
        asked += 1;
        match given {
          Some(text) => intake.answer_text(Some(text.to_vec())),
          None => intake.answer_from_fork(store),
        }
      }
    }
  }
  let (fork, intaken) = intake.finish().ok().unwrap();
  (fork, intaken, asked)
}

/// [`taken_answering`] from the fork's own texts.
fn taken_counting(
  payload: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> (HgIndex, Intaken, u64) {
  taken_answering(payload, (index, store), None)
}

/// Goal 93, C1 (2) (a file delta's base read from the fork's rows, a window at a time; it replaces main's rulings (b1)
/// and (b2): the push's own deltas rebuilt and its own texts cached). Do: push one file's four revisions, the third and
/// the fourth each a delta against a revision before the one kept last (a merge's base, as generaldelta sends one); then
/// let the first revision's bytes go, as a landing does, and push a fifth revision as a delta against it. Expect every
/// text read back as it was pushed; the first push's bases read from the fork, nothing rebuilt and nothing asked; and
/// the let-go base asked of the store once, brought back, read, and let go again once its revision is taken (red before:
/// held for good, a landed base's bytes back in memory a push).
#[test]
fn a_file_delta_base_before_the_last_text_is_read_from_the_forks_rows() {
  let texts: [(&[u8], Option<usize>); 4] = [
    (b"a\n", None),
    (b"a\nb\n", Some(0)),
    (b"a\nc\n", Some(0)),
    (b"a\nb\nd\n", Some(1)),
  ];
  let (payload, after) = file_push(&texts, (crate::NULL_NODE, crate::NULL_NODE, &[]));
  let mut store = store();
  let (fork, first, first_asks) = taken_counting(&payload, &mut HgIndex::new(), &mut store);
  assert_eq!(
    (first.kept, first.base_store_reads, first_asks),
    (12, 0, 0),
    "every base read from the fork's rows: four changesets, manifests and file revisions kept"
  );
  let read_back: Vec<(Node, Vec<u8>)> = after
    .2
    .iter()
    .map(|(node, _)| {
      let rev = fork.rev(&store, Log::File(b"f"), node).unwrap();
      (
        *node,
        fork.revision_text(&store, Log::File(b"f"), rev).unwrap(),
      )
    })
    .collect();
  assert_eq!(read_back, after.2, "every text read back as it was pushed");
  let (first_node, first_text) = after.2.first().cloned().unwrap();
  let first_rev = fork.rev(&store, Log::File(b"f"), &first_node).unwrap();
  let row = fork.revision(&store, Log::File(b"f"), first_rev).unwrap();
  assert!(
    store.let_go_object(&row.bytes) > 0,
    "the first revision's bytes let go"
  );
  let mut fork = fork;
  let (second, _) = file_push(&[(b"a\ne\n", Some(0))], (after.0, after.1, &after.2));
  let (_, landed, landed_asks) = taken_answering(
    &second,
    (&mut fork, &mut store),
    Some(first_text.as_slice()),
  );
  assert_eq!(
    (landed.kept, landed.base_store_reads, landed_asks),
    (3, 1, 1),
    "the let-go base asked of the store once"
  );
  assert_eq!(
    store.object_bytes(&row.bytes, u64::try_from(first_text.len()).unwrap()),
    None,
    "the base brought back for its revision let go again after it"
  );
}

/// A-767 C1 (the push spin w-balance found: a 64 MiB file revision hashed and kept in one intake step spun its shard past
/// the watchdog). Do: take a push of one file revision three and a half content windows long and a delta over it, once
/// into a store whose window is the tests' (4 KiB) and once into one whose window holds the whole text. Expect the same
/// rows either way; with the small window, both revisions taken over several steps and no step hashing or keeping more
/// than one window; with the large one, none spread. Red before: one step took the whole text.
#[test]
fn a_revision_longer_than_a_window_is_taken_a_window_a_step() {
  let window = 1 << 12;
  let long: Vec<u8> = (0..window * 7 / 2)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut edited = long.clone();
  edited.extend_from_slice(b"\nand a line more\n");
  let texts: [(&[u8], Option<usize>); 2] = [(&long, None), (&edited, Some(0))];
  let (payload, _) = file_push(&texts, (crate::NULL_NODE, crate::NULL_NODE, &[]));
  let take = |window: usize| {
    let mut store = ContentStore::new(1 << 30, window).unwrap();
    let (mut fork, intaken, _) = taken_counting(&payload, &mut HgIndex::new(), &mut store);
    (rows(&mut fork, &store), intaken)
  };
  let (small_rows, small) = take(window);
  let (large_rows, large) = take(window * 8);
  assert_eq!(small_rows, large_rows, "the same rows either way");
  assert_eq!(small.spread, 2, "both file revisions spread over steps");
  assert!(
    small.longest_step_bytes <= u64::try_from(window).unwrap(),
    "no step past a window: {}",
    small.longest_step_bytes
  );
  assert_eq!(large.spread, 0);
  assert_eq!(small.kept, large.kept);
}

/// Goal 93, C1 (2) (main's order of 2026-11-24 16:10: the push intake keeping no whole text; main's r618 reading: a long
/// revision rebuilt into one buffer, its pending steps half the 57k intake's time). Do: take a push of one file revision
/// three and a half content windows long and a delta over it into a store of the tests' window (4 KiB). Expect no more
/// than two windows of text held at once (`Intaken::most_text_held`): the window being made
/// and the window of its base being read. Red before: the whole rebuilt text, its base and the text kept last.
#[test]
fn a_long_revision_is_rebuilt_holding_no_more_than_two_windows_of_text() {
  let window = 1 << 12;
  let long: Vec<u8> = (0..window * 7 / 2)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let mut edited = long.clone();
  edited.extend_from_slice(b"\nand a line more\n");
  let texts: [(&[u8], Option<usize>); 2] = [(&long, None), (&edited, Some(0))];
  let (payload, _) = file_push(&texts, (crate::NULL_NODE, crate::NULL_NODE, &[]));
  let mut store = ContentStore::new(1 << 30, window).unwrap();
  let (_, intaken, _) = taken_counting(&payload, &mut HgIndex::new(), &mut store);
  assert!(
    intaken.most_text_held <= u64::try_from(2 * window).unwrap(),
    "at most two windows of text held at once: {}",
    intaken.most_text_held
  );
}

/// Whether `refused` is the intake past its admission ([`Refused::past_admission`]).
fn past_admission(refused: &Refused) -> bool {
  refused.past_admission().is_some()
}

/// `part` taken a step at a time within one admission of `first` bytes, as a push's slices take it; each step refused
/// past its admission taken again in an admission grown by `first`, carrying what the push already holds (what the
/// shared room wait gives an intake, main 01a20182's ruling of 2026-11-22 03:03): the fork, what was brought, and the
/// steps refused and taken again.
fn intake_growing(
  part: &ChangegroupPart,
  (index, store): (&mut HgIndex, &mut ContentStore),
  first: u64,
) -> (HgIndex, Intaken, u64) {
  let mut admitted = first;
  let mut retaken = 0u64;
  // The framing first: nothing is held before it, so a begin past its admission begins again in a grown room.
  let (mut intake, mut admission) = loop {
    let mut admission = Admission::new(admitted);
    match Intake::begin((part.version, &part.payload), index, store, &mut admission) {
      Ok(intake) => break (intake, admission),
      Err(refused) => {
        assert!(
          past_admission(&refused),
          "only past its admission: {refused:?}"
        );
        admitted += first;
        retaken += 1;
      }
    }
  };
  loop {
    match intake.step(&part.payload, store, &mut admission) {
      Ok(Stepped::Store) => intake.answer_from_fork(store),
      Ok(stepped) if more(&stepped) => {}
      Ok(_) => break,
      Err(refused) => {
        assert!(
          past_admission(&refused),
          "only past its admission: {refused:?}"
        );
        let held = admitted - admission.left();
        admitted += first;
        admission = Admission::new(admitted);
        admission.charge(held).unwrap();
        retaken += 1;
      }
    }
  }
  let (fork, intaken) = finished(intake);
  (fork, intaken, retaken)
}

/// Main 01a20182's order of 2026-11-22 03:03 (w-interop's find on ubuntu: stock 7.2's push of hg's whole history
/// refused `Delta { log: Manifests, .. PastAdmission { asked: 73365, left: 50646 } }`, the push's own earlier deltas
/// having spent its one admission). Do: take the stock changegroup once within room enough and note what it held; then
/// take it within a quarter, a half and all but a byte of that, each step refused past its admission taken again in a
/// grown admission. Expect each to end with the same rows and the same brought as the take with room enough, at least one
/// step taken again for each, and every refusal one past the admission: a step refused leaves the intake where it was.
#[test]
fn a_step_refused_past_its_admission_is_taken_again_whole_in_a_grown_room() {
  let part = stock();
  let mut whole_store = store();
  let mut empty = HgIndex::new();
  let mut ample = Admission::new(1 << 30);
  let (mut whole_fork, whole_brought, _) =
    intake(&part, &mut empty, &mut whole_store, &mut ample).unwrap();
  let whole_rows = rows(&mut whole_fork, &whole_store);
  let held = (1u64 << 30) - ample.left();
  for first in [held / 4, held / 2, held - 1] {
    let mut store = store();
    let mut empty = HgIndex::new();
    let (mut fork, brought, retaken) = intake_growing(&part, (&mut empty, &mut store), first);
    assert!(retaken > 0, "a room of {first} for {held} refused a step");
    assert_eq!(
      rows(&mut fork, &store),
      whole_rows,
      "the rows of room enough, room {first}"
    );
    assert_eq!(
      brought, whole_brought,
      "what room enough brought, room {first}"
    );
  }
}

/// Main 01a20182's ruling (a) of 2026-11-22 04:23 (a push admitted its known need up front). Do: begin the stock
/// changegroup within room enough, note what the begin charged, ask the intake its bookkeeping (`Intake::bookkeeping`),
/// then take it whole and note what its steps left charged. Expect what the steps kept no more than the bookkeeping
/// told, and the bookkeeping more than nothing: the push's need, known before a revision is taken, covers what it keeps.
#[test]
fn an_intakes_bookkeeping_covers_what_its_steps_keep() {
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = Admission::new(1 << 30);
  let mut intake = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap();
  let after_begin = admission.left();
  let told = intake.bookkeeping();
  loop {
    match intake
      .step(&part.payload, &mut store, &mut admission)
      .unwrap()
    {
      Stepped::Store => intake.answer_from_fork(&store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  let kept = after_begin - admission.left();
  assert!(told > 0, "a push keeps something");
  assert!(kept <= told, "kept {kept} of the {told} told");
  finished(intake);
}

/// Format: the bytes of the stock changegroup a segment is cut at in [`an_intake_cuts_its_kept_texts_into_segments`]:
/// small, so the stock push cuts into several.
const SEGMENT_BYTES: u64 = 256;

/// Main 01a20182's ruling (b1) of 2026-11-22 04:44 (a push lands as a run of segment changesets; it holds one segment's
/// kept texts at a time). Do: take the stock changegroup whole once without cutting and once cut at [`SEGMENT_BYTES`],
/// taking each segment as it is cut and the rest once every unit is taken. Expect more than one segment; the segments'
/// texts, each place moved back by its slice's start, the texts the uncut take kept, in order; each text's place within
/// its slice; every log's kept revisions covered by the segments' ranges with none twice; what each charged more than
/// nothing; and the cut take bringing no texts of its own.
#[test]
fn an_intake_cuts_its_kept_texts_into_segments() {
  let part = stock();
  let mut whole_store = store();
  let mut empty = HgIndex::new();
  let (_, whole, _) = intake(&part, &mut empty, &mut whole_store, &mut room()).unwrap();
  let (segments, cut) = cut_take(&part, SEGMENT_BYTES);
  assert!(segments.len() > 1, "{} segments", segments.len());
  assert!(
    cut.texts.is_empty(),
    "the cut take holds no texts of its own"
  );
  assert_eq!(
    segments_texts(&segments),
    whole.texts,
    "the segments' texts the uncut take's"
  );
  assert_each_revision_once(&segments);
}

/// `part` taken whole into an empty index, cut at `bound`: each segment taken as it is cut, the rest once every unit is
/// taken; the segments and what the take brought.
fn cut_take(part: &ChangegroupPart, bound: u64) -> (Vec<PushSegment>, Intaken) {
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = room();
  let mut taking = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap()
  .cutting(bound);
  let mut segments = Vec::new();
  loop {
    let stepped = taking
      .step(&part.payload, &mut store, &mut admission)
      .unwrap();
    segments.extend(std::iter::from_fn(|| taking.take_segment()));
    match stepped {
      Stepped::Store => taking.answer_from_fork(&store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  taking.cut_rest();
  segments.extend(std::iter::from_fn(|| taking.take_segment()));
  let (_, cut) = finished(taking);
  (segments, cut)
}

/// The texts of `segments`, in order, each place moved back by its slice's start; each segment charged more than
/// nothing, and each place within its slice.
fn segments_texts(segments: &[PushSegment]) -> Vec<KeptText> {
  let mut texts = Vec::new();
  for segment in segments {
    assert!(segment.charged > 0);
    let len = segment.slice.end - segment.slice.start;
    for kept in &segment.texts {
      assert!(kept.place.end <= len, "a text's place within its slice");
      let mut whole_place = kept.clone();
      whole_place.place =
        kept.place.start + segment.slice.start..kept.place.end + segment.slice.start;
      texts.push(whole_place);
    }
  }
  texts
}

/// Every log's kept revisions covered by `segments`' ranges, each once and in order.
fn assert_each_revision_once(segments: &[PushSegment]) {
  let mut covered: std::collections::BTreeMap<String, Vec<u32>> = std::collections::BTreeMap::new();
  for (log, first, end) in segments.iter().flat_map(|segment| &segment.ranges) {
    covered
      .entry(format!("{log:?}"))
      .or_default()
      .extend(*first..*end);
  }
  for (log, revisions) in covered {
    let mut sorted = revisions.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
      sorted, revisions,
      "{log}'s kept revisions each once, in order"
    );
  }
}

/// Main 01a20182's order of 2026-11-22 05:04 (a push whose keeping and a step's least room are past its repository's
/// ceiling is refused typed at its begin, never waited for): the sound lower bound on a step's room the framing tells.
/// Do: begin stock 7.2's changegroup and read its widest full text; take it whole. Expect the widest text a delta
/// against the null base carries (its one hunk's insert) among the decoded revisions, more than nothing, and no more than
/// the longest text a step rebuilt and charged.
#[test]
fn an_intakes_widest_full_text_is_the_least_room_a_step_of_it_takes() {
  let part = stock();
  let decoded =
    crate::changegroup::Changegroup::decode(&part.payload, part.version, &mut room()).unwrap();
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
  let expected = groups
    .iter()
    .flat_map(|group| group.iter())
    .filter(|revision| revision.base == crate::NULL_NODE)
    .map(|revision| {
      (revision.data.len() as u64).saturating_sub(crate::delta::HUNK_HEADER_LEN as u64)
    })
    .max()
    .unwrap();
  let mut store = store();
  let mut index = HgIndex::new();
  let begun = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut room(),
  )
  .unwrap();
  let widest = begun.widest_full_text();
  begun.abandon(&mut store).unwrap();
  assert!(widest > 0, "stock 7.2's bundle carries whole texts");
  assert_eq!(widest, expected, "the widest whole text the framing tells");
  let (fork, intaken, _) = intake(&part, &mut index, &mut store, &mut room()).unwrap();
  assert!(
    widest <= intaken.longest_step_bytes,
    "{widest} past the longest text a step charged, {}",
    intaken.longest_step_bytes
  );
  fork.release(&mut store).unwrap();
}

/// Main 01a20182's ruling (b1) of 2026-11-22 04:44 (a push's segment changeset's `Record`: record v8, its rows by
/// identity, as an import's segment's). Do: take stock 7.2's changegroup into an empty index cut at [`SEGMENT_BYTES`],
/// writing each segment's record as it is cut and the rest's once every unit is taken; replay the records in order into
/// another empty index over the same content store. Expect more than one record, each a push's; and the replayed
/// index the intake's fork row for row.
#[test]
fn a_pushs_segment_records_replay_in_order_to_its_forks_rows() {
  let part = stock();
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = room();
  let mut taking = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap()
  .cutting(SEGMENT_BYTES);
  let mut records = Vec::new();
  let mut recorded = |taking: &mut Intake, store: &ContentStore| {
    while let Some(segment) = taking.take_segment() {
      records.push(whole_record(taking, &segment, 1 << 20, store));
    }
  };
  loop {
    let stepped = taking
      .step(&part.payload, &mut store, &mut admission)
      .unwrap();
    recorded(&mut taking, &store);
    match stepped {
      Stepped::Store => taking.answer_from_fork(&store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  taking.cut_rest();
  recorded(&mut taking, &store);
  let (mut fork, _) = finished(taking);
  assert!(records.len() > 1, "{} records", records.len());
  let mut replayed = HgIndex::new();
  for bytes in &records {
    crate::import::record::replay(bytes, &mut replayed, &mut store).unwrap();
  }
  assert_eq!(rows(&mut replayed, &store), rows(&mut fork, &store));
  replayed.release(&mut store).unwrap();
  fork.release(&mut store).unwrap();
}

/// A changegroup 03 of a line of `n` changesets, the `k`th adding file `f{k}`, its manifest listing every file so far
/// and sent whole against the null base (a full snapshot each), as a whole-history push of a revlog storing snapshots
/// sends them; every revision whole.
fn snapshots_push(n: usize) -> Vec<u8> {
  let paths: Vec<String> = (0..n).map(|k| format!("f{k:03}")).collect();
  adding_files(Line::default(), &paths).0
}

/// Where a line of changesets ends: its last changeset and manifest, and the manifest's lines.
#[derive(Clone, Debug, Default)]
struct Line {
  /// The last changeset.
  changeset: Node,
  /// Its manifest.
  manifest: Node,
  /// The manifest's lines.
  lines: Vec<Vec<u8>>,
}

/// A changegroup 03 of a changeset for each of `paths` after `after`, each adding its file (one whole revision, its
/// text naming its path) to the manifest before it, the manifest sent whole against the null base; and where the line
/// then ends.
fn adding_files(after: Line, paths: &[String]) -> (Vec<u8>, Line) {
  use crate::changegroup::{self as cg, Revision, Version};
  use crate::{NULL_NODE, node};
  let hex = |node: &Node| {
    let mut out = Vec::new();
    crate::pushkey::encode_hex(node, &mut out);
    out
  };
  let whole = |(node, p1, link): (Node, Node, Node), text: &[u8], out: &mut Vec<u8>| {
    let revision = Revision {
      node,
      p1,
      p2: NULL_NODE,
      base: NULL_NODE,
      link,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data: text,
      sidedata: None,
    };
    cg::put_revision_whole(&revision, Version::V03, out).unwrap();
  };
  let mut line = after;
  let (mut changesets, mut manifests, mut files) = (Vec::new(), Vec::new(), Vec::new());
  for path in paths {
    let path = path.as_bytes().to_vec();
    let text = [b"file ", &path[..], b"\n"].concat();
    let file = node::compute(&NULL_NODE, &NULL_NODE, &text).unwrap();
    line
      .lines
      .push([&path[..], b"\0", &hex(&file), b"\n"].concat());
    line.lines.sort();
    let manifest_text = line.lines.concat();
    let manifest_node = node::compute(&line.manifest, &NULL_NODE, &manifest_text).unwrap();
    let changeset_text = [
      &hex(&manifest_node)[..],
      b"\ntest\n0 0\n",
      &path,
      b"\n\nchange",
    ]
    .concat();
    let changeset_node = node::compute(&line.changeset, &NULL_NODE, &changeset_text).unwrap();
    changesets.push((changeset_node, line.changeset, changeset_text));
    manifests.push((manifest_node, line.manifest, manifest_text, changeset_node));
    files.push((path, file, text, changeset_node));
    (line.changeset, line.manifest) = (changeset_node, manifest_node);
  }
  let mut out = Vec::new();
  for (node, p1, text) in &changesets {
    whole((*node, *p1, *node), text, &mut out);
  }
  cg::put_end(&mut out);
  for (node, p1, text, link) in &manifests {
    whole((*node, *p1, *link), text, &mut out);
  }
  cg::put_end(&mut out);
  cg::put_end(&mut out);
  files.sort_by(|left, right| left.0.cmp(&right.0));
  for (path, node, text, link) in &files {
    cg::put_name(path, &mut out).unwrap();
    whole((*node, NULL_NODE, *link), text, &mut out);
    cg::put_end(&mut out);
  }
  cg::put_end(&mut out);
  (out, line)
}

/// What taking `payload` (a changegroup 03) whole into an empty index kept charged to its admission past its begin, and
/// what its bookkeeping told at the begin.
fn kept_and_told(payload: &[u8]) -> (u64, u64) {
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = room();
  let mut intake = Intake::begin(
    (crate::changegroup::Version::V03, payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap();
  let (after_begin, told) = (admission.left(), intake.bookkeeping());
  while more(&intake.step(payload, &mut store, &mut admission).unwrap()) {}
  let (fork, _) = finished(intake);
  fork.release(&mut store).unwrap();
  (after_begin - admission.left(), told)
}

/// Main 01a20182's order of 2026-11-22 05:13 (w-interop's whole-history push of hg: 1.37 GB held, the bulk each whole or
/// null-base manifest's every line, kept for the files' check once a snapshot). Do: take a line of 60 changesets whose
/// manifests are each a full snapshot, then of 120. Expect what the intake kept, and what its bookkeeping told, to grow
/// with the changesets (twice the line, less than three times the keeping): each manifest's (path, node) kept once
/// across the push, never a copy a snapshot (which grows with their square, four times); and the bookkeeping covering
/// the keeping. Red before: a snapshot's every line kept.
#[test]
fn a_push_of_many_whole_manifest_snapshots_keeps_one_copy_of_their_entries() {
  let (short_kept, short_told) = kept_and_told(&snapshots_push(60));
  let (long_kept, long_told) = kept_and_told(&snapshots_push(120));
  println!("kept {short_kept} then {long_kept}, told {short_told} then {long_told}");
  assert!(
    long_kept <= long_told,
    "kept {long_kept} of {long_told} told"
  );
  assert!(
    long_kept * 2 < short_kept * 5 && long_told * 2 < short_told * 5,
    "kept {short_kept} then {long_kept}, told {short_told} then {long_told}: past linear in the changesets"
  );
}

/// `payload`, a changegroup 03, taken into a fork of `index` cut at every kept text, each segment's record written as it
/// is cut and the rest's once every unit is taken, each store want answered from the fork; the records, and the fork,
/// which holds their contents (as a push's fork holds them to its entry), and what the take brought.
fn segment_records_over(
  payload: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
) -> (Vec<Vec<u8>>, HgIndex, Intaken) {
  let mut admission = room();
  let mut taking = Intake::begin(
    (crate::changegroup::Version::V03, payload),
    index,
    store,
    &mut admission,
  )
  .unwrap()
  .cutting(1);
  let mut records = Vec::new();
  let mut recorded = |taking: &mut Intake, store: &ContentStore| {
    while let Some(segment) = taking.take_segment() {
      records.push(whole_record(taking, &segment, 1 << 20, store));
    }
  };
  loop {
    let stepped = taking.step(payload, store, &mut admission).unwrap();
    recorded(&mut taking, store);
    match stepped {
      Stepped::More => {}
      Stepped::Store => taking.answer_from_fork(store),
      _ => break,
    }
  }
  taking.cut_rest();
  recorded(&mut taking, store);
  let (fork, intaken) = finished(taking);
  (records, fork, intaken)
}

/// Main 01a20182's ruling of 2026-11-22 06:57 (a push lands its segments apart from its repository's turn, which it
/// takes only for its entry's commit, so another push may land between its segments and its entry). Do: land a line of
/// four changesets, each adding a file; over it, cut a push of three more into segments and write their records; land
/// another push of one changeset over the same line meanwhile; replay the first push's records onto the index as that
/// left it. Expect every manifest the first push brought held and reading as its fork read it (some kept as a delta
/// against the manifest before it, A-729), though the other push's manifest now stands before it. Red before: a record
/// kept a manifest's stored delta by its base's number, the one before it, which the other push's manifest took.
#[test]
fn a_pushs_segments_replay_onto_an_index_another_push_moved_their_bases_by_node() {
  let names =
    |range: core::ops::Range<usize>| range.map(|k| format!("f{k:03}")).collect::<Vec<_>>();
  let (first, line) = adding_files(Line::default(), &names(0..4));
  let mut store = store();
  let (mut index, _, _) = taken_counting(&first, &mut HgIndex::new(), &mut store);
  let held = index.len(&store, Log::Manifest);
  let (pushed, _) = adding_files(line.clone(), &names(4..7));
  let (other, _) = adding_files(line, &names(9..10));
  let (records, fork, _) = segment_records_over(&pushed, &mut index, &mut store);
  assert!(records.len() > 1, "{} records", records.len());
  let (moved, _, _) = taken_counting(&other, &mut index, &mut store);
  core::mem::replace(&mut index, moved)
    .release(&mut store)
    .unwrap();
  for bytes in &records {
    crate::import::record::replay(bytes, &mut index, &mut store).unwrap();
  }
  let mut deltas = 0;
  for rev in held..fork.len(&store, Log::Manifest) {
    let row = fork.revision(&store, Log::Manifest, rev).expect("its row");
    deltas += usize::from(row.base != crate::index::NONE);
    let at = index.rev(&store, Log::Manifest, &row.node).expect("held");
    assert_eq!(
      index.revision_text(&store, Log::Manifest, at),
      fork.revision_text(&store, Log::Manifest, rev),
      "manifest {rev}, replayed at {at}"
    );
  }
  assert!(deltas > 0, "the push kept a manifest as a delta");
  index.release(&mut store).unwrap();
  fork.release(&mut store).unwrap();
}

/// Main 01a20182's ruling of 2026-11-22 07:32 (staging: a push's fork made stale by another push's landing is re-based
/// before its entry, and a restart replays its segments' records over the index as it stood: the two must agree). Do:
/// over a line of four changesets each adding a file, cut a push of three more into segments and write their records;
/// land another push of one changeset over the same line; re-base the first push's fork onto the index that left
/// (`Rebase`), and, apart, replay its records onto the same index. Expect the two indexes row for row the same, the
/// re-based one holding the push.
#[test]
fn a_stale_forks_re_base_and_its_segments_replay_make_the_same_index() {
  let names =
    |range: core::ops::Range<usize>| range.map(|k| format!("f{k:03}")).collect::<Vec<_>>();
  let (first, line) = adding_files(Line::default(), &names(0..4));
  let mut store = store();
  let (mut index, _, _) = taken_counting(&first, &mut HgIndex::new(), &mut store);
  let (pushed, _) = adding_files(line.clone(), &names(4..7));
  let (other, _) = adding_files(line, &names(9..10));
  let (records, stale, intaken) = segment_records_over(&pushed, &mut index, &mut store);
  let (moved, _, _) = taken_counting(&other, &mut index, &mut store);
  core::mem::replace(&mut index, moved)
    .release(&mut store)
    .unwrap();
  let mut replayed = index.fork(&mut store).unwrap();
  for bytes in &records {
    crate::import::record::replay(bytes, &mut replayed, &mut store).unwrap();
  }
  let rebase = super::Rebase::begin(stale, &intaken, &mut index, &mut store)
    .ok()
    .unwrap();
  let (mut rebased, again) = rebased(rebase, &mut store);
  assert_eq!(again.changesets.len(), 3, "{again:?}");
  assert_eq!(rows(&mut rebased, &store), rows(&mut replayed, &store));
  for held in [rebased, replayed, index] {
    held.release(&mut store).unwrap();
  }
}

/// Goal 93 (main's ruling of 2026-11-23 16:29: attribute a push's long steps by the kind of unit each takes;
/// `crates/hg/examples/intake_steps.rs` reads it). Do: take stock 7.2's bundle a step at a time, reading before each
/// step the kind of unit it takes. Expect the kinds in the intake's order, each a run with no kind coming back once
/// left: the changelog, the manifests, the files, the manifests' presence, the files', then done; and every step's kind
/// one of those (no merge: the stock bundle seals nothing in the tests' window).
#[test]
fn each_step_names_the_kind_of_unit_it_takes_in_the_intakes_order() {
  let part = stock();
  let (mut index, mut store) = (HgIndex::new(), store());
  let mut admission = room();
  let mut intake = Intake::begin(
    (part.version, &part.payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap();
  let mut kinds: Vec<&'static str> = Vec::new();
  loop {
    let unit = intake.unit();
    if kinds.last() != Some(&unit) {
      kinds.push(unit);
    }
    match intake
      .step(&part.payload, &mut store, &mut admission)
      .unwrap()
    {
      Stepped::Store => intake.answer_from_fork(&store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  kinds.push(intake.unit());
  assert_eq!(
    kinds,
    [
      "changelog",
      "manifest",
      "file",
      "manifests-present",
      "files-present",
      "done"
    ]
  );
  let (fork, _) = finished(intake);
  fork.release(&mut store).unwrap();
}

/// `segment`'s record, written by `intake` a step at a time to its end (as the daemon writes it, a turn's steps at a time).
fn whole_record(
  intake: &Intake,
  segment: &PushSegment,
  room: u64,
  store: &ContentStore,
) -> Vec<u8> {
  let mut writer = intake.record_writer(segment, room).unwrap();
  while intake.record_step(&mut writer, store).unwrap() {}
  writer.finish().unwrap()
}

/// Goal 93, C1 (1) P4 (main's ruling of 2026-11-23 20:53: the changelog's and the manifests' texts off the content
/// store, the push's intake reading one not at hand through the changeset store). Do: take stock 7.2's bundle into a
/// repository; make a changeset over its tip (`push::made`, its revisions whole against the null base, as a client's
/// push of one changeset); save every changeset's and manifest's text; let go every object those rows name; take the
/// made changegroup into a fork of the repository through the intake, its asks of a log's text answered from what was
/// saved (a file's from the fork). Expect: objects let go; the intake done, the made changeset in its fork; the
/// manifest it is kept as a delta against asked of the store (`Intaken::log_texts_asked`). Red before: refused
/// `Unread`, the previous manifest read from the content store (the edge red's `Index(Unread { rev: 5 })`).
#[test]
fn a_pushs_intake_reads_a_manifest_let_go_from_the_changeset_store() {
  use crate::push::made::{Authored, CHANGEGROUP_VERSION, Change, changegroup, made};
  use std::collections::BTreeMap;
  let mut store = store();
  let mut empty = HgIndex::new();
  let (mut repository, _, _) = intake(&stock(), &mut empty, &mut store, &mut room()).unwrap();
  let tip = {
    let len = repository.len(&store, Log::Changelog);
    repository.changeset(&store, len - 1).unwrap().node
  };
  let authored = Authored {
    user: b"Client <c@example.com>",
    seconds: 1,
    branch: b"default",
    message: b"one more",
  };
  let change = [Change {
    path: b"one-more.txt",
    after: Some((b"one more\n", crate::manifest::Flag::Regular)),
    copied_from: None,
  }];
  let verified = made(
    &crate::push::held::IndexHeld::new(&repository, &store),
    tip,
    &change,
    &authored,
    &mut room(),
  )
  .unwrap();
  let payload = changegroup(&verified, &mut room()).unwrap();
  let mut saved: BTreeMap<(Vec<u8>, Node), Vec<u8>> = BTreeMap::new();
  let mut objects = Vec::new();
  for rev in 0..repository.len(&store, Log::Changelog) {
    let row = repository.changeset(&store, rev).unwrap();
    let text = store.object_bytes(&row.text, row.len).unwrap();
    saved.insert((super::CHANGELOG_PATH.to_vec(), row.node), text);
    objects.push(row.text);
  }
  for rev in 0..repository.len(&store, Log::Manifest) {
    let row = repository.revision(&store, Log::Manifest, rev).unwrap();
    let text = repository
      .revision_text(&store, Log::Manifest, rev)
      .unwrap();
    saved.insert((super::MANIFEST_PATH.to_vec(), row.node), text);
    objects.push(row.bytes);
  }
  let let_go: u64 = objects
    .iter()
    .map(|object| store.let_go_object(object))
    .sum();
  assert!(let_go > 0, "objects let go");
  let mut intake = Intake::begin(
    (CHANGEGROUP_VERSION, &payload),
    &mut repository,
    &mut store,
    &mut room(),
  )
  .unwrap();
  let mut admission = room();
  loop {
    match intake.step(&payload, &mut store, &mut admission).unwrap() {
      Stepped::Store => match intake.asked_text() {
        Some(asked) if saved.contains_key(&asked) => intake.answer_text(saved.get(&asked).cloned()),
        _ => intake.answer_from_fork(&store),
      },
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  let (mut fork, intaken) = finished(intake);
  let made_node = verified.changesets[0].node;
  assert!(
    fork.rev(&store, Log::Changelog, &made_node).is_some(),
    "the made changeset taken"
  );
  assert!(intaken.log_texts_asked > 0, "a manifest asked of the store");
  let _ = fork.rows();
}

/// The stock bundle taken into a repository and a made changeset over its tip taken into a fork of it (a client's push
/// of one changeset); every changeset's and manifest's text of the repository saved by (log path, node) and the objects
/// their rows name let go: the repository, the push's fork and what it brought, and what was saved.
#[allow(clippy::type_complexity)]
fn a_push_over_a_let_go_repository(
  store: &mut ContentStore,
) -> (
  HgIndex,
  HgIndex,
  Intaken,
  std::collections::BTreeMap<(Vec<u8>, Node), Vec<u8>>,
) {
  use crate::push::made::{Authored, Change, changegroup, made};
  let mut empty = HgIndex::new();
  let (mut repository, _, _) = intake(&stock(), &mut empty, store, &mut room()).unwrap();
  let tip = {
    let len = repository.len(store, Log::Changelog);
    repository.changeset(store, len - 1).unwrap().node
  };
  let authored = Authored {
    user: b"Client <c@example.com>",
    seconds: 1,
    branch: b"default",
    message: b"one more",
  };
  let change = [Change {
    path: b"one-more.txt",
    after: Some((b"one more\n", crate::manifest::Flag::Regular)),
    copied_from: None,
  }];
  let verified = made(
    &crate::push::held::IndexHeld::new(&repository, store),
    tip,
    &change,
    &authored,
    &mut room(),
  )
  .unwrap();
  let payload = changegroup(&verified, &mut room()).unwrap();
  let mut pushing = Intake::begin(
    (crate::push::made::CHANGEGROUP_VERSION, &payload),
    &mut repository,
    store,
    &mut room(),
  )
  .unwrap();
  let mut admission = room();
  loop {
    match pushing.step(&payload, store, &mut admission).unwrap() {
      Stepped::Store => pushing.answer_from_fork(store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  let (pushed, intaken) = finished(pushing);
  let mut saved = std::collections::BTreeMap::new();
  let mut objects = Vec::new();
  for rev in 0..repository.len(store, Log::Changelog) {
    let row = repository.changeset(store, rev).unwrap();
    saved.insert(
      (super::CHANGELOG_PATH.to_vec(), row.node),
      store.object_bytes(&row.text, row.len).unwrap(),
    );
    objects.push(row.text);
  }
  for rev in 0..repository.len(store, Log::Manifest) {
    let row = repository.revision(store, Log::Manifest, rev).unwrap();
    saved.insert(
      (super::MANIFEST_PATH.to_vec(), row.node),
      repository.revision_text(store, Log::Manifest, rev).unwrap(),
    );
    objects.push(row.bytes);
  }
  let let_go: u64 = objects
    .iter()
    .map(|object| store.let_go_object(object))
    .sum();
  assert!(let_go > 0, "objects let go");
  (repository, pushed, intaken, saved)
}

/// Goal 93, C1 (1) (main's ruling of 2026-11-24 02:55: a stale re-base reads a manifest's text not at hand through the
/// changeset store, as the intake does). Do: push a made changeset over the stock bundle's repository into a fork; let
/// go every changeset's and manifest's object of the repository; re-base the push's fork onto the repository, its asks
/// answered from what was saved. Expect: the push's changeset re-based into the new fork, its manifest's text the same
/// as in the stale fork, and texts asked of the store. Red before: refused, the push's manifest kept as a delta against
/// the repository's tip, whose text was let go (`record::kept` reading its chain).
#[test]
fn a_stale_re_base_reads_a_manifest_let_go_from_the_changeset_store() {
  let mut store = store();
  let (mut repository, stale, intaken, saved) = a_push_over_a_let_go_repository(&mut store);
  let manifest_rev = stale.len(&store, Log::Manifest) - 1;
  let expected = stale
    .revision_text_over(
      &store,
      Log::Manifest,
      manifest_rev,
      &saved_manifests(&saved),
    )
    .unwrap();
  let mut rebase = super::Rebase::begin(stale, &intaken, &mut repository, &mut store)
    .ok()
    .unwrap();
  loop {
    match rebase.step(&mut store).unwrap() {
      Stepped::More => {}
      Stepped::Store => {
        let asked = rebase.asked_text().unwrap();
        rebase.answer_text(saved.get(&asked).cloned());
      }
      _ => break,
    }
  }
  let (fork, again) = rebase.finish(&mut store).unwrap();
  assert!(again.log_texts_asked > 0, "texts asked of the store");
  let made_node = intaken.changesets[0];
  assert!(
    fork.rev(&store, Log::Changelog, &made_node).is_some(),
    "the push re-based"
  );
  let rev = fork.len(&store, Log::Manifest) - 1;
  assert_eq!(
    fork.revision_text_over(&store, Log::Manifest, rev, &saved_manifests(&saved)),
    Some(expected)
  );
}

/// The manifests' texts of `saved`, by node.
fn saved_manifests(
  saved: &std::collections::BTreeMap<(Vec<u8>, Node), Vec<u8>>,
) -> Vec<(Node, Vec<u8>)> {
  saved
    .iter()
    .filter(|((path, _), _)| path == super::MANIFEST_PATH)
    .map(|((_, node), text)| (*node, text.clone()))
    .collect()
}

/// Goal 93, C1 (1), a guard (main's ruling of 2026-11-24 03:05): a push's segment record names each revision by its
/// contents' identities (`record::write_identity_revision`), reading no text, so it is written whole over a repository
/// whose changesets' and manifests' texts were let go. Do: push a made changeset over the stock bundle's repository into
/// a fork; let go every changeset's and manifest's object of the repository; write the push's segment record of every
/// revision it kept, a step at a time. Expect it written whole. Kept so the invariant holds if that writer ever reads a
/// text: it would be refused `Unreadable` here.
#[test]
fn a_pushs_segment_record_is_written_whole_over_let_go_logs() {
  use crate::import::record::{Source, Writer};
  let mut store = store();
  let (_repository, pushed, intaken, _) = a_push_over_a_let_go_repository(&mut store);
  let ranges: Vec<(Log<'_>, u32, u32)> = intaken
    .logs
    .iter()
    .map(|(log, first)| (super::log_named(log), *first, u32::MAX))
    .collect();
  let mut writer = Writer::begin_segment(Source::Push, &ranges, 1 << 20, false).unwrap();
  while writer.step(&pushed, &store).unwrap() {}
  assert!(writer.finish().is_some(), "the record written whole");
}

/// A made changeset's push over `index`'s tip (`change` its one file), taken into a fork of it: the fork and what it
/// brought.
fn made_push(index: &mut HgIndex, store: &mut ContentStore, change: &[u8]) -> (HgIndex, Intaken) {
  use crate::push::made::{Authored, CHANGEGROUP_VERSION, Change, changegroup, made};
  let tip = {
    let len = index.len(store, Log::Changelog);
    index.changeset(store, len - 1).unwrap().node
  };
  let authored = Authored {
    user: b"Client <c@example.com>",
    seconds: 1,
    branch: b"default",
    message: change,
  };
  let changes = [Change {
    path: change,
    after: Some((change, crate::manifest::Flag::Regular)),
    copied_from: None,
  }];
  let verified = made(
    &crate::push::held::IndexHeld::new(index, store),
    tip,
    &changes,
    &authored,
    &mut room(),
  )
  .unwrap();
  let payload = changegroup(&verified, &mut room()).unwrap();
  let mut pushing =
    Intake::begin((CHANGEGROUP_VERSION, &payload), index, store, &mut room()).unwrap();
  let mut admission = room();
  loop {
    match pushing.step(&payload, store, &mut admission).unwrap() {
      Stepped::Store => pushing.answer_from_fork(store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  finished(pushing)
}

/// `pushed`'s segment record (a push's, naming identities) of every revision `intaken` names.
fn segment_record(pushed: &HgIndex, intaken: &Intaken, store: &ContentStore) -> Vec<u8> {
  use crate::import::record::{Source, Writer};
  let ranges: Vec<(Log<'_>, u32, u32)> = intaken
    .logs
    .iter()
    .map(|(log, first)| (super::log_named(log), *first, u32::MAX))
    .collect();
  let mut writer = Writer::begin_segment(Source::Push, &ranges, 1 << 20, false).unwrap();
  while writer.step(pushed, store).unwrap() {}
  writer.finish().unwrap()
}

/// Every changeset's and manifest's text `indexes` hold, by (log path, node), saved, then every object their rows name
/// let go: what was saved.
fn saved_and_let_go(
  indexes: &[&HgIndex],
  store: &mut ContentStore,
) -> std::collections::BTreeMap<(Vec<u8>, Node), Vec<u8>> {
  let mut saved = std::collections::BTreeMap::new();
  let mut objects = Vec::new();
  for index in indexes {
    for rev in 0..index.len(store, Log::Changelog) {
      let row = index.changeset(store, rev).unwrap();
      if let Some(text) = store.object_bytes(&row.text, row.len) {
        saved.insert((super::CHANGELOG_PATH.to_vec(), row.node), text);
      }
      objects.push(row.text);
    }
    for rev in 0..index.len(store, Log::Manifest) {
      let row = index.revision(store, Log::Manifest, rev).unwrap();
      if let Some(text) = index.revision_text(store, Log::Manifest, rev) {
        saved.insert((super::MANIFEST_PATH.to_vec(), row.node), text);
      }
      objects.push(row.bytes);
    }
  }
  let let_go: u64 = objects
    .iter()
    .map(|object| store.let_go_object(object))
    .sum();
  assert!(let_go > 0, "objects let go");
  saved
}

/// `record` replayed into `index` a step at a time, every ask answered from `saved`: what was replayed, and each ask's
/// log path with how many texts it asked.
fn replayed_answering(
  record: &[u8],
  index: &mut HgIndex,
  store: &mut ContentStore,
  saved: &std::collections::BTreeMap<(Vec<u8>, Node), Vec<u8>>,
) -> (crate::import::record::Replayed, Vec<(Vec<u8>, usize)>) {
  use crate::import::record::Replay;
  let mut replay = Replay::begin(record, index, store).unwrap();
  let mut asks = Vec::new();
  while replay.step(record, store).unwrap() {
    if let Some((path, nodes)) = replay.asked_texts()
      && replay.asking()
    {
      let texts = nodes
        .iter()
        .map(|node| saved.get(&(path.clone(), *node)).cloned())
        .collect();
      asks.push((path, nodes.len()));
      replay.answer_texts(texts);
    }
  }
  (replay.finish(index, store).unwrap(), asks)
}

/// Goal 93, C1 (1) (3)(a) (main's ruling of 2026-11-24 03:17: a replay reads the changesets' texts let go once landed
/// from the changeset store, batched). Do: take the stock bundle into a repository and a made changeset's push into a
/// fork of it; write the push's segment record (identities, base by node); let go every changeset's and manifest's
/// object, the push's too, as a restart finds them after the let-go; replay the record over the repository, the
/// store's asks answered from what was saved. Expect: the push's changeset replayed into the repository; texts asked
/// (`Replayed::texts_asked`, the counter main's round reads). Red before: refused `Unheld`, the changeset's text read
/// from the content store by its identity.
#[test]
fn a_replay_reads_a_changesets_text_let_go_from_the_changeset_store() {
  let mut store = store();
  let mut empty = HgIndex::new();
  let (mut repository, _, _) = intake(&stock(), &mut empty, &mut store, &mut room()).unwrap();
  let (pushed, intaken) = made_push(&mut repository, &mut store, b"one-more.txt");
  let record = segment_record(&pushed, &intaken, &store);
  let saved = saved_and_let_go(&[&repository, &pushed], &mut store);
  let (replayed, asks) = replayed_answering(&record, &mut repository, &mut store, &saved);
  assert!(replayed.texts_asked > 0, "texts asked of the store");
  assert!(
    asks.iter().any(|(path, _)| path == super::CHANGELOG_PATH),
    "a changeset's text asked: {asks:?}"
  );
  assert!(
    repository
      .rev(&store, Log::Changelog, &intaken.changesets[0])
      .is_some(),
    "the push's changeset replayed"
  );
  let _ = pushed;
}

/// Goal 93, C1 (1) (3)(b): a replay over an index another push moved meanwhile keeps a manifest again against the one
/// the index holds last, its base's text and that one's read from the changeset store. Do: take the stock bundle into
/// a repository; take one made changeset's push into a fork and write its segment record; land another made changeset
/// into the repository meanwhile; let go every changeset's and manifest's object; replay the first push's record over
/// the moved repository, its asks answered from what was saved. Expect: the first push's changeset replayed, its
/// manifest's text the one it was pushed with; texts asked. Red before: refused `Unread`, its manifest kept again over
/// a base and a previous manifest let go.
#[test]
fn a_replay_over_a_moved_index_reads_its_manifests_let_go_from_the_changeset_store() {
  let mut store = store();
  let mut empty = HgIndex::new();
  let (mut repository, _, _) = intake(&stock(), &mut empty, &mut store, &mut room()).unwrap();
  let (pushed, intaken) = made_push(&mut repository, &mut store, b"first.txt");
  let record = segment_record(&pushed, &intaken, &store);
  let manifest = pushed.len(&store, Log::Manifest) - 1;
  let expected = pushed
    .revision_text(&store, Log::Manifest, manifest)
    .unwrap();
  let (mut moved, _) = made_push(&mut repository, &mut store, b"meanwhile.txt");
  let saved = saved_and_let_go(&[&repository, &pushed, &moved], &mut store);
  let (replayed, _) = replayed_answering(&record, &mut moved, &mut store, &saved);
  assert!(replayed.texts_asked > 0, "texts asked of the store");
  let rev = moved
    .rev(
      &store,
      Log::Manifest,
      &pushed
        .revision(&store, Log::Manifest, manifest)
        .unwrap()
        .node,
    )
    .unwrap();
  let manifests: Vec<(Node, Vec<u8>)> = saved
    .iter()
    .filter(|((path, _), _)| path == super::MANIFEST_PATH)
    .map(|((_, node), text)| (*node, text.clone()))
    .collect();
  assert_eq!(
    moved.revision_text_over(&store, Log::Manifest, rev, &manifests),
    Some(expected)
  );
}

/// The entries a manifest's delta wrote and its check's refusal, found as its text streams a `piece` at a time
/// (`manifest::Checking` and `delta::Written`): what a manifest taken a window a step reads.
fn streamed_entries(
  text: &[u8],
  revision: &crate::changegroup::Revision<'_>,
  piece: usize,
) -> Result<Vec<(Vec<u8>, Node)>, crate::manifest::Refusal> {
  let every = revision.is_full_text() || revision.base == crate::NULL_NODE;
  let mut checking = crate::manifest::Checking::default();
  let mut written = crate::delta::Written::default();
  let mut found = Vec::new();
  for chunk in text.chunks(piece.max(1)) {
    checking.update(chunk, &mut |range, entry| {
      if every || written.touches(revision.data, range).unwrap() {
        found.push((entry.path.to_vec(), entry.node));
      }
      Ok(())
    })?;
  }
  checking.finish()?;
  Ok(found)
}

/// Goal 93, C1 (2) (main's ruling (B) of 2026-11-24: a manifest taken a window a step; its check and its written entries
/// found as it streams). Do: over manifests and their deltas (line diffs adding, changing and removing entries at the
/// front, middle and end; a whole text; an empty manifest; hostile texts: unended, unsorted, a bad line), read each text
/// fed 1, 7, 64 bytes and whole at a time. Expect the entries the delta wrote, every piece size, equal to those the
/// whole-text reader finds (`written_entries`), and every refusal `manifest::decode`'s.
#[test]
fn a_manifest_read_as_it_streams_finds_what_its_whole_text_finds() {
  use crate::changegroup::Revision;
  let line = |path: &str, seed: u8| {
    let mut out = path.as_bytes().to_vec();
    out.push(0);
    out.extend(core::iter::repeat_n(
      b"0123456789abcdef"[usize::from(seed % 16)],
      40,
    ));
    out.push(b'\n');
    out
  };
  let manifest = |entries: &[(&str, u8)]| -> Vec<u8> {
    entries
      .iter()
      .flat_map(|(path, seed)| line(path, *seed))
      .collect()
  };
  let base = manifest(&[("a", 1), ("b/c", 2), ("d", 3), ("e/f/g", 4), ("z", 5)]);
  let texts = [
    manifest(&[("a", 9), ("b/c", 2), ("d", 3), ("e/f/g", 4), ("z", 5)]),
    manifest(&[
      ("a", 1),
      ("b/c", 2),
      ("c", 7),
      ("d", 3),
      ("e/f/g", 4),
      ("z", 5),
    ]),
    manifest(&[("a", 1), ("b/c", 2), ("e/f/g", 4), ("z", 5)]),
    manifest(&[
      ("a", 1),
      ("b/c", 2),
      ("d", 3),
      ("e/f/g", 4),
      ("z", 6),
      ("zz", 8),
    ]),
    manifest(&[("0", 1), ("a", 1), ("b/c", 3), ("d", 3), ("z", 5)]),
    Vec::new(),
  ];
  let node = [7; 20];
  for text in &texts {
    let mut delta = Vec::new();
    crate::delta::encode_lines(&base, text, &mut delta).unwrap();
    let mut whole = Vec::new();
    crate::delta::encode_full_text(text, &mut whole).unwrap();
    for (data, base_node) in [(&delta, [1; 20]), (&whole, crate::NULL_NODE)] {
      let revision = Revision {
        node,
        p1: [1; 20],
        p2: crate::NULL_NODE,
        base: base_node,
        link: node,
        flags: 0,
        info: None,
        protocol_flags: 0,
        data,
        sidedata: None,
      };
      let expected = super::written_entries(text, &revision).unwrap();
      for piece in [1, 7, 64, text.len().max(1)] {
        assert_eq!(
          streamed_entries(text, &revision, piece),
          Ok(expected.clone()),
          "a piece of {piece}"
        );
      }
    }
  }
  let mut unsorted = manifest(&[("b", 1)]);
  unsorted.extend(line("a", 2));
  let mut unended = manifest(&[("a", 1)]);
  unended.pop();
  let hostile = [
    unsorted,
    unended,
    b"not a manifest line\n".to_vec(),
    b"\n".to_vec(),
  ];
  let revision = Revision {
    node,
    p1: crate::NULL_NODE,
    p2: crate::NULL_NODE,
    base: crate::NULL_NODE,
    link: node,
    flags: 0,
    info: None,
    protocol_flags: 0,
    data: &[],
    sidedata: None,
  };
  for text in &hostile {
    let expected = crate::manifest::decode(text).err();
    assert!(expected.is_some(), "a hostile text refused whole");
    for piece in [1, 7, 64, text.len().max(1)] {
      assert_eq!(
        streamed_entries(text, &revision, piece).err(),
        expected,
        "a piece of {piece}"
      );
    }
  }
}

/// A push of `commits` into an empty repository, each its parents' places among those before it (a merge's two) and the
/// text of file `f` it makes, beside `padding` files `p/NNN` the first commit brings and none changes, so every manifest
/// is long and each differs from its first parent's by `f`'s line: the changegroup in version 03, as stock 7.2 sends a
/// generaldelta repository's (each changeset and padding file whole; each manifest and each revision of `f` a delta
/// against its first parent's, the first whole), and each commit's manifest's text in order.
fn dag_push(commits: &[([Option<usize>; 2], &[u8])], padding: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
  use crate::changegroup::{self as cg, Revision, Version};
  use crate::{NULL_NODE, delta, node};
  let hex = |node: &Node| {
    let mut out = Vec::new();
    crate::pushkey::encode_hex(node, &mut out);
    out
  };
  let pads: Vec<(Vec<u8>, Vec<u8>, Node)> = (0..padding)
    .map(|at| {
      let path = format!("p/{at:04}").into_bytes();
      let text = format!("padding file {at}\n").into_bytes();
      let file = node::compute(&NULL_NODE, &NULL_NODE, &text).unwrap();
      (path, text, file)
    })
    .collect();
  let mut made: Vec<Made> = Vec::new();
  let mut out = Vec::new();
  let (mut manifests, mut files) = (Vec::new(), Vec::new());
  for (at, (parents, text)) in commits.iter().enumerate() {
    let of = |parent: Option<usize>| parent.and_then(|parent| made.get(parent));
    let (first, second) = (of(parents[0]), of(parents[1]));
    let node_of = |made: Option<&Made>, pick: usize| {
      made.map_or(NULL_NODE, |made| [made.0, made.1, made.2][pick])
    };
    let file = node::compute(&node_of(first, 2), &node_of(second, 2), text).unwrap();
    let mut manifest_text = [&b"f\0"[..], &hex(&file), b"\n"].concat();
    for (path, _, pad) in &pads {
      manifest_text.extend_from_slice(&[&path[..], b"\0", &hex(pad), b"\n"].concat());
    }
    let manifest = node::compute(&node_of(first, 1), &node_of(second, 1), &manifest_text).unwrap();
    let changeset_text = [
      &hex(&manifest)[..],
      format!("\ntest\n0 0\nf\n\ncommit {at}").as_bytes(),
    ]
    .concat();
    let changeset =
      node::compute(&node_of(first, 0), &node_of(second, 0), &changeset_text).unwrap();
    let whole = Revision {
      node: changeset,
      p1: node_of(first, 0),
      p2: node_of(second, 0),
      base: NULL_NODE,
      link: changeset,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data: &changeset_text,
      sidedata: None,
    };
    cg::put_revision_whole(&whole, Version::V03, &mut out).unwrap();
    let based = |pick: usize, text: &[u8]| -> (Node, Vec<u8>) {
      match first {
        None => {
          let mut data = Vec::new();
          delta::encode_full_text(text, &mut data).unwrap();
          (NULL_NODE, data)
        }
        Some(first) => {
          let base_text = if pick == 1 { &first.3 } else { &first.4 };
          let mut data = Vec::new();
          delta::encode_lines(base_text, text, &mut data).unwrap();
          ([first.0, first.1, first.2][pick], data)
        }
      }
    };
    let (manifest_base, manifest_data) = based(1, &manifest_text);
    manifests.push((
      manifest,
      node_of(first, 1),
      node_of(second, 1),
      manifest_base,
      manifest_data,
      changeset,
    ));
    let (file_base, file_data) = based(2, text);
    files.push((
      file,
      node_of(first, 2),
      node_of(second, 2),
      file_base,
      file_data,
      changeset,
    ));
    made.push((changeset, manifest, file, manifest_text, text.to_vec()));
  }
  cg::put_end(&mut out);
  let put = |(node, p1, p2, base, data, link): &(Node, Node, Node, Node, Vec<u8>, Node),
             out: &mut Vec<u8>| {
    let revision = Revision {
      node: *node,
      p1: *p1,
      p2: *p2,
      base: *base,
      link: *link,
      flags: 0,
      info: None,
      protocol_flags: 0,
      data,
      sidedata: None,
    };
    cg::put_revision(&revision, Version::V03, out).unwrap();
  };
  for manifest in &manifests {
    put(manifest, &mut out);
  }
  cg::put_end(&mut out);
  cg::put_end(&mut out);
  cg::put_name(b"f", &mut out).unwrap();
  for file in &files {
    put(file, &mut out);
  }
  cg::put_end(&mut out);
  let first_changeset = made.first().map_or(NULL_NODE, |made| made.0);
  for (path, text, pad) in &pads {
    cg::put_name(path, &mut out).unwrap();
    let mut data = Vec::new();
    delta::encode_full_text(text, &mut data).unwrap();
    put(
      &(*pad, NULL_NODE, NULL_NODE, NULL_NODE, data, first_changeset),
      &mut out,
    );
    cg::put_end(&mut out);
  }
  cg::put_end(&mut out);
  (out, made.into_iter().map(|made| made.3).collect())
}

/// Goal 93, C1 (2) (main's ruling (B): a manifest base read in ranges over its chain, never whole). Do: push eight
/// commits on a line, each changing file `f` beside 40 padding files, so each manifest is kept as a delta against the
/// one before it ([`dag_push`]); read
/// every manifest of the fork through its chain (`chain_read::Chain`) front to back in pieces of 1, 7 and 64 bytes,
/// and again in pieces with gaps skipped between them. Expect each text the index's own (`HgIndex::revision_text`), and
/// each skipped read the same bytes at the same places; at least one chain more than its whole text.
#[test]
fn a_manifest_chain_read_in_ranges_is_its_text() {
  use crate::delta::BaseRead;
  use crate::push::chain_read::{Chain, Reading};
  use crate::read::to_u64;
  let commits: Vec<([Option<usize>; 2], Vec<u8>)> = (0..8)
    .map(|at: usize| {
      (
        [at.checked_sub(1), None],
        format!("f at {at}\n").into_bytes(),
      )
    })
    .collect();
  let commits: Vec<([Option<usize>; 2], &[u8])> = commits
    .iter()
    .map(|(parents, text)| (*parents, text.as_slice()))
    .collect();
  let (payload, _) = dag_push(&commits, 40);
  let mut store = store();
  let (fork, _, _) = taken_counting(&payload, &mut HgIndex::new(), &mut store);
  let mut longest_chain = 0usize;
  for rev in 0..fork.len(&store, Log::Manifest) {
    let text = fork.revision_text(&store, Log::Manifest, rev).unwrap();
    let chain = Chain::of(&fork, &store, rev).unwrap();
    assert_eq!(chain.len(), to_u64(text.len()));
    longest_chain = longest_chain.max(usize::from(chain.held() > 0));
    for piece in [1usize, 7, 64] {
      let mut cursor = crate::push::chain_read::Cursor::default();
      let mut reading = Reading {
        chain: &chain,
        cursor: &mut cursor,
        store: &store,
      };
      let mut read = Vec::new();
      let mut at = 0u64;
      while at < reading.len() {
        let n = usize::try_from(reading.len() - at).unwrap().min(piece);
        reading.read_into(at, n, &mut read).unwrap();
        at += to_u64(n);
      }
      assert_eq!(read, text, "manifest {rev} in pieces of {piece}");
      let mut cursor = crate::push::chain_read::Cursor::default();
      let mut reading = Reading {
        chain: &chain,
        cursor: &mut cursor,
        store: &store,
      };
      let mut at = 0u64;
      while at < reading.len() {
        let n = usize::try_from(reading.len() - at).unwrap().min(piece);
        let mut got = Vec::new();
        reading.read_into(at, n, &mut got).unwrap();
        let from = usize::try_from(at).unwrap();
        assert_eq!(got, text[from..from + n], "manifest {rev} at {at}");
        at += to_u64(n) * 2;
      }
    }
  }
  assert_eq!(
    longest_chain, 1,
    "a manifest kept as a delta over its chain"
  );
}

/// Each manifest's text the fork reads back, in log order.
fn manifest_texts(fork: &HgIndex, store: &ContentStore) -> Vec<Vec<u8>> {
  (0..fork.len(store, Log::Manifest))
    .map(|rev| fork.revision_text(store, Log::Manifest, rev).unwrap())
    .collect()
}

/// Goal 93, C1 (2) (main's ruling (B') of 2026-11-24, its red and oracle 1). Do: push 12 commits on a line, each changing
/// file `f` beside 300 padding files, so each manifest is several of the tests' windows long (4 KiB), sent as a delta
/// against the one before. Expect every manifest's text read back as it was made, and the text the intake held at once
/// within four windows and three lines (the window it makes, the one it hashes, and its diff's base and run: a window and
/// a line each). Red before: each manifest rebuilt whole beside its base's text and the text kept last.
#[test]
fn a_line_of_long_manifests_is_taken_within_the_window_bound() {
  let commits: Vec<([Option<usize>; 2], Vec<u8>)> = (0..12)
    .map(|at: usize| {
      (
        [at.checked_sub(1), None],
        format!("f at {at}\n").into_bytes(),
      )
    })
    .collect();
  let commits: Vec<([Option<usize>; 2], &[u8])> = commits
    .iter()
    .map(|(parents, text)| (*parents, text.as_slice()))
    .collect();
  let (payload, texts) = dag_push(&commits, 300);
  let window = 1 << 12;
  assert!(texts[0].len() > 3 * window, "a manifest of several windows");
  let mut store = ContentStore::new(1 << 30, window).unwrap();
  let (fork, intaken, _) = taken_counting(&payload, &mut HgIndex::new(), &mut store);
  assert_eq!(
    manifest_texts(&fork, &store),
    texts,
    "each manifest as made"
  );
  let line = texts[0]
    .split(|byte| *byte == b'\n')
    .map(<[u8]>::len)
    .max()
    .unwrap()
    + 1;
  let bound = u64::try_from(4 * window + 3 * line).unwrap();
  assert!(
    intaken.most_text_held <= bound,
    "held {} of text at once, past {bound}",
    intaken.most_text_held
  );
  assert_eq!(
    intaken.manifests_kept_whole_bytes,
    u64::try_from(texts[0].len()).unwrap(),
    "only the first kept whole"
  );
}

/// Goal 93, C1 (2) (main's ruling (B'), oracle 1, merges): a merge's manifest is a delta against its first parent's,
/// which is not the manifest the fork holds last, so its row is its diff against that last one. Do: push two branches
/// off a root, each changing `f` beside 40 padding files, merged twice; then replay the push's segment records into
/// another index. Expect every manifest's text read back as it was made, the merges kept as diffs (nothing kept whole but
/// the root), and the replay's rows the fork's, row for row.
#[test]
fn a_merge_heavy_push_keeps_its_manifests_as_diffs_and_replays_to_its_rows() {
  let commits: [([Option<usize>; 2], &[u8]); 8] = [
    ([None, None], b"root\n"),
    ([Some(0), None], b"left 1\n"),
    ([Some(0), None], b"right 1\n"),
    ([Some(1), Some(2)], b"merged 1\n"),
    ([Some(1), None], b"left 2\n"),
    ([Some(2), None], b"right 2\n"),
    ([Some(5), Some(4)], b"merged 2\n"),
    ([Some(6), Some(3)], b"merged both\n"),
  ];
  let (payload, texts) = dag_push(&commits, 40);
  let mut store = store();
  let mut index = HgIndex::new();
  let mut admission = room();
  let mut taking = Intake::begin(
    (crate::changegroup::Version::V03, &payload),
    &mut index,
    &mut store,
    &mut admission,
  )
  .unwrap()
  .cutting(SEGMENT_BYTES);
  let mut records = Vec::new();
  loop {
    let stepped = taking.step(&payload, &mut store, &mut admission).unwrap();
    while let Some(segment) = taking.take_segment() {
      records.push(whole_record(&taking, &segment, 1 << 20, &store));
    }
    match stepped {
      Stepped::Store => taking.answer_from_fork(&store),
      stepped if more(&stepped) => {}
      _ => break,
    }
  }
  taking.cut_rest();
  while let Some(segment) = taking.take_segment() {
    records.push(whole_record(&taking, &segment, 1 << 20, &store));
  }
  let (mut fork, intaken) = finished(taking);
  assert_eq!(
    manifest_texts(&fork, &store),
    texts,
    "each manifest as made"
  );
  assert_eq!(
    intaken.manifests_kept_whole_bytes,
    u64::try_from(texts[0].len()).unwrap(),
    "nothing kept whole but the root"
  );
  let mut replayed = HgIndex::new();
  for bytes in &records {
    crate::import::record::replay(bytes, &mut replayed, &mut store).unwrap();
  }
  assert_eq!(rows(&mut replayed, &store), rows(&mut fork, &store));
  replayed.release(&mut store).unwrap();
  fork.release(&mut store).unwrap();
}
