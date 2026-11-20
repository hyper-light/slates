//! A-614 part 2: deltas, the learned rules, each version's pack fragments read back through
//! A-608's reader, entries' offsets and CRC32s, and a clone from the base pack and the fragments
//! after it. Two ignored tests are main's runs: a pack for `git verify-pack -v`, and the cost of
//! naming a pack's objects twice.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::time::Instant;

use proptest::prelude::*;
use slates_vcs::{ContentStore, Naming};

use crate::Admission;
use crate::delta::apply;
use crate::entry::PACK_HEADER_LEN;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tree::Mode;
use crate::store::learn::{Depth, Levels, Rebuild, Window};
use crate::store::{Repository, Stored, delta_of};
use crate::unpack::{NoIndex, read_whole};

use crate::store::Push;

use super::{
  EXCHANGE, Made, Source, Ticks, blob, brought_by, commit, pack_of, push_and_move, reached_now,
  store, tree,
};

/// A text of numbered lines, `lines` long, the lines in `edited` edited.
pub(super) fn text(lines: u32, edited: &[u32]) -> Vec<u8> {
  let mut out = Vec::new();
  for line in 0..lines {
    let words = if edited.contains(&line) {
      "an edited line"
    } else {
      "a line of a file"
    };
    out.extend_from_slice(format!("{line:04} {words}\n").as_bytes());
  }
  out
}

fn applied(source: &[u8], delta: &[u8]) -> Vec<u8> {
  apply(source, delta, &mut Admission::new(u64::MAX)).unwrap()
}

/// A-614 part 2 (`diff-delta.c`'s delta). Do: delta a text against the same text with one line
/// edited and lines added, and try targets a copy cannot help. Expect: the delta applies back to
/// the target exactly and is far shorter than it; no delta for unrelated bytes, for a target no
/// longer than a copy instruction, or from an empty source.
#[test]
fn a_delta_turns_its_source_into_its_target_and_none_saves_nothing() {
  let source = text(40, &[]);
  let mut target = text(40, &[17]);
  target.extend_from_slice(b"one more line at the end\n");
  let delta = delta_of(&source, &target).unwrap();
  assert_eq!(applied(&source, &delta), target);
  assert!(delta.len() * 4 < target.len());
  let unrelated: Vec<u8> = (0..400u32)
    .map(|at| u8::try_from(at.wrapping_mul(97) % 251).unwrap())
    .collect();
  assert_eq!(delta_of(&source, &unrelated), None);
  assert_eq!(delta_of(&source, b"tiny"), None);
  assert_eq!(delta_of(b"", &target), None);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(128), ..ProptestConfig::default() }))]

  /// A-614 part 2. Do: build targets from random cuts of a random source and random inserts, and
  /// delta them against the source. Expect: every delta made applies back to its target exactly
  /// and is shorter than it.
  #[test]
  fn every_delta_applies_back_to_its_target(source in proptest::collection::vec(any::<u8>(), 0..600), cuts in proptest::collection::vec((any::<usize>(), 0usize..80, proptest::collection::vec(any::<u8>(), 0..6)), 0..8)) {
    let mut target = Vec::new();
    for (from, len, inserted) in &cuts {
      if !source.is_empty() {
        let from = from % source.len();
        target.extend_from_slice(&source[from..(from + len).min(source.len())]);
      }
      target.extend_from_slice(inserted);
    }
    if let Some(delta) = delta_of(&source, &target) {
      prop_assert!(delta.len() < target.len());
      prop_assert_eq!(apply(&source, &delta, &mut Admission::new(u64::MAX)), Ok(target));
    }
  }
}

/// A-614 part 2 (§4.17.2's tunables). Do: ask each rule before and after measurements. Expect: the
/// window one candidate before any, then one rank past the deepest that paid; no level until every
/// level of a kind is measured, then the one whose savings most outweigh its time; the depth the
/// deepest chain seen, at most git's limit; a rebuild due at the first rent, then only once the
/// rent reaches the measured price.
#[test]
fn the_learned_rules_follow_what_was_measured() {
  let mut window = Window::default();
  assert_eq!((window.size(EXCHANGE, 9), window.size(EXCHANGE, 0)), (1, 0));
  window.record(1, 100, 10);
  window.record(2, 50, 10);
  window.record(3, 1, 10);
  assert_eq!(window.size(EXCHANGE, 9), 3);
  let mut levels = Levels::default();
  for level in 0..9 {
    levels.record(Kind::Blob, level, (1000, 900 - u64::from(level) * 50, 10));
  }
  assert_eq!(levels.level(Kind::Blob, EXCHANGE), None);
  levels.record(Kind::Blob, 9, (1000, 500, 1_000));
  assert_eq!(levels.level(Kind::Blob, EXCHANGE), Some(8));
  let mut depth = Depth::default();
  depth.seen(3);
  depth.seen(u64::MAX);
  assert_eq!(depth.limit(), crate::store::learn::DEPTH_LIMIT);
  rebuild_rule();
}

/// The rebuild rule's three states.
fn rebuild_rule() {
  let mut rebuild = Rebuild::default();
  assert!(!rebuild.due());
  rebuild.cloned(5);
  assert!(rebuild.due());
  rebuild.rebuilt(100);
  rebuild.cloned(99);
  assert!(!rebuild.due());
  rebuild.cloned(1);
  assert!(rebuild.due());
}

/// A-662 (a push's chain carried by its record, `Push::carried`). Do: commit a push whose pack
/// carried chains two deep; then move another ref to its head with a push of no object, handed a
/// chain three deep as a replay hands a record's; then a third ref with one handed a chain past
/// git's limit. Expect: the learned depth two, then three, then git's limit: a push that brings
/// no object teaches the store the chain its pack carried, as the live push's commit did.
#[test]
fn a_chain_carried_by_a_push_of_no_object_teaches_the_depth() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let objects = commit_over(format, &text(8, &[]), &[], 1);
  let head = objects[0].id;
  push_and_move(&mut repository, &mut store, objects, &[(1, Some(head))]).unwrap();
  assert_eq!(repository.learned_depth(), 2);
  let limit = crate::store::learn::DEPTH_LIMIT;
  for (green, carried, learned) in [(2, 3, 3), (3, u64::MAX, limit)] {
    let mut push = Push::new(format);
    push.carried(carried);
    repository.seal(&store, &mut push).unwrap();
    let admitted = repository.admit(&store, &push, green, Some(head)).unwrap();
    repository
      .commit(&mut store, &mut push, &[admitted])
      .unwrap();
    assert_eq!(repository.learned_depth(), learned);
  }
}

/// A commit over `big` (a text file) and `small`, and its objects.
pub(super) fn commit_over(
  format: ObjectFormat,
  big: &[u8],
  parents: &[ObjectId],
  seconds: u64,
) -> Vec<Made> {
  let (big, small) = (blob(format, big), blob(format, b"small\n"));
  let root = tree(
    format,
    &[
      (Mode::File, b"big", big.id),
      (Mode::File, b"small", small.id),
    ],
  );
  let head = commit(format, root.id, parents, seconds);
  vec![head, root, big, small]
}

/// A ref at a commit over a 200-line text, then at one with a line of it edited; both versions
/// encoded.
fn two_versions(
  format: ObjectFormat,
  store: &mut ContentStore,
) -> (Repository, Vec<Made>, Vec<Made>) {
  let mut repository = Repository::new(format);
  let first = commit_over(format, &text(200, &[]), &[], 1);
  let second = commit_over(format, &text(200, &[120]), &[first[0].id], 2);
  let mut clock = Ticks(0);
  for (version, objects) in [first.clone(), second.clone()].into_iter().enumerate() {
    let head = objects[0].id;
    push_and_move(&mut repository, store, objects, &[(1, Some(head))]).unwrap();
    repository
      .encode_fragments(store, (1, version), &mut clock, EXCHANGE)
      .unwrap();
  }
  (repository, first, second)
}

/// The objects a pack holds, read back through A-608's reader.
fn unpacked(format: ObjectFormat, pack: &[u8]) -> BTreeMap<ObjectId, Vec<u8>> {
  read_whole(pack, format, &NoIndex, &mut Admission::new(u64::MAX))
    .unwrap()
    .objects
    .into_iter()
    .map(|object| (object.id, object.content))
    .collect()
}

/// A-614 part 2 (§4.17.2 item 4). Do: make a ref at a commit over a 200-line file, then at one
/// that edits one line; encode both versions; read the pack of both back. Expect: every object of
/// D(0) and D(1) read back with its bytes; the edited file a delta against the object its path
/// held before (a REF delta, counted); each version encoded as two fragments.
#[test]
fn a_versions_fragments_read_back_and_an_edit_is_a_delta_against_its_previous_version() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let mut store = store();
    let (repository, first, second) = two_versions(format, &mut store);
    let pack = pack_of(
      &repository,
      &store,
      &brought_by(&repository, &store, 1, 0..=1),
    );
    let read = unpacked(format, &pack);
    let made: BTreeMap<ObjectId, Vec<u8>> = first
      .iter()
      .chain(&second)
      .map(|object| (object.id, object.content.clone()))
      .collect();
    assert_eq!(read, made);
    let (objects, blobs) = repository.fragments(1, 1).unwrap();
    let edited = blobs
      .iter()
      .find(|entry| entry.id == second[2].id)
      .map(|entry| entry.stored);
    assert_eq!((edited, objects.len()), (Some(Stored::Reference), 2));
    let counters = repository.counters();
    assert_eq!(counters.fragments, 4);
    assert!(counters.reference_deltas >= 1);
    repository.release(&mut store).unwrap();
    assert!(store.is_empty());
  }
}

/// The pack of both versions reads back as every object `first` and `second` made.
fn assert_both_versions_read_back(
  format: ObjectFormat,
  (repository, store): (&Repository, &ContentStore),
  first: &[Made],
  second: &[Made],
) {
  let pack = pack_of(repository, store, &brought_by(repository, store, 1, 0..=1));
  let made: BTreeMap<ObjectId, Vec<u8>> = first
    .iter()
    .chain(second)
    .map(|object| (object.id, object.content.clone()))
    .collect();
  assert_eq!(unpacked(format, &pack), made);
}

/// A-724 piece 4 (§4.17.5 "The cold trigger"). Do: encode two versions, then drop the second's
/// fragments, then encode it again. Expect: both listed as kept with the bytes their content
/// holds; the drop giving exactly those bytes back to the content store and leaving the version to
/// encode next; a second drop finding nothing; the version encoded again to the same fragments,
/// so the pack reads back as before; and the store empty once the repository is released.
#[test]
fn a_versions_fragments_dropped_give_their_bytes_back_and_encode_again_the_same() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let mut store = store();
    let (mut repository, first, second) = two_versions(format, &mut store);
    let kept = repository.kept_fragments(&store);
    assert_eq!(
      kept.iter().map(|(at, _)| *at).collect::<Vec<_>>(),
      vec![(1, 0), (1, 1)]
    );
    let (_, second_bytes) = kept[1];
    assert!(second_bytes > 0);
    let before = repository
      .fragments(1, 1)
      .map(|(o, b)| (o.to_vec(), b.to_vec()));
    let used = store.used();
    assert!(repository.drop_fragments(&mut store, (1, 1)).unwrap());
    assert_eq!(store.used(), used - second_bytes);
    assert_eq!(repository.first_unencoded(), Some((1, 1)));
    assert!(!repository.drop_fragments(&mut store, (1, 1)).unwrap());
    repository
      .encode_fragments(&mut store, (1, 1), &mut Ticks(0), EXCHANGE)
      .unwrap();
    let after = repository
      .fragments(1, 1)
      .map(|(o, b)| (o.to_vec(), b.to_vec()));
    assert_eq!(after, before);
    assert_both_versions_read_back(format, (&repository, &store), &first, &second);
    repository.release(&mut store).unwrap();
    assert!(store.is_empty());
  }
}

/// A-614 part 2 (offset deltas within a fragment, chains no deeper than the learned depth). Do:
/// push, from a client whose pack was one delta deep, a commit over a file, the file with eleven
/// lines edited, and that with one more: the third is nearest the second. Expect: the later two
/// offset deltas, every chain at most one deep (the second, itself a delta, passed over as the
/// third's base for the first), and the pack read back whole.
#[test]
fn similar_files_in_one_version_are_offset_deltas_within_the_learned_depth() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let region: Vec<u32> = (50..61).collect();
  let further: Vec<u32> = region.iter().copied().chain([150]).collect();
  let files: Vec<Made> = [&[][..], &region, &further]
    .into_iter()
    .map(|edited| blob(format, &text(200, edited)))
    .collect();
  let root = tree(
    format,
    &[
      (Mode::File, b"a", files[0].id),
      (Mode::File, b"b", files[1].id),
      (Mode::File, b"c", files[2].id),
    ],
  );
  let head = commit(format, root.id, &[], 1);
  let mut objects = vec![head.clone(), root];
  objects.extend(files);
  let mut push = Push::new(format);
  let mut source = Source {
    objects: objects.clone(),
    at: 0,
    claim: true,
    depth: 1,
  };
  push
    .admit_all(&mut store, &repository.naming(), &mut source)
    .unwrap();
  repository.seal(&store, &mut push).unwrap();
  let moved = repository.admit(&store, &push, 1, Some(head.id)).unwrap();
  repository.commit(&mut store, &mut push, &[moved]).unwrap();
  repository
    .encode_fragments(&mut store, (1, 0), &mut Ticks(0), EXCHANGE)
    .unwrap();
  let (_, blobs) = repository.fragments(1, 0).unwrap();
  let offsets = blobs
    .iter()
    .filter(|entry| entry.stored == Stored::Offset)
    .count();
  assert_eq!(
    (offsets, blobs.iter().map(|entry| entry.depth).max()),
    (2, Some(1))
  );
  let read = unpacked(
    format,
    &pack_of(
      &repository,
      &store,
      &brought_by(&repository, &store, 1, [0]),
    ),
  );
  assert_eq!(read.len(), objects.len());
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-614 part 2 (each entry's CRC32 kept, as a pack index keeps it). Do: read the first version's
/// pack back and hold its entries' offsets and CRC32s against the fragments' records. Expect: each
/// entry where its record says, and the CRC32 of its bytes, from its offset to the next entry's,
/// the record's.
#[test]
fn a_fragments_entries_keep_their_offsets_and_crcs() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, ..) = two_versions(format, &mut store);
  let pack = pack_of(
    &repository,
    &store,
    &brought_by(&repository, &store, 1, [0]),
  );
  let objects = read_whole(&pack, format, &NoIndex, &mut Admission::new(u64::MAX))
    .unwrap()
    .objects;
  let (others, blobs) = repository.fragments(1, 0).unwrap();
  let objects_len = objects[others.len()].offset - PACK_HEADER_LEN;
  let mut starts: Vec<u64> = objects.iter().map(|object| object.offset).collect();
  starts.push(u64::try_from(pack.len() - format.raw_len()).unwrap());
  let placed = others.iter().map(|entry| (entry, entry.offset)).chain(
    blobs
      .iter()
      .map(|entry| (entry, objects_len + entry.offset)),
  );
  for (at, (entry, offset)) in placed.enumerate() {
    assert_eq!(
      (objects[at].id, objects[at].offset),
      (entry.id, PACK_HEADER_LEN + offset)
    );
    let (from, to) = (
      usize::try_from(starts[at]).unwrap(),
      usize::try_from(starts[at + 1]).unwrap(),
    );
    let bytes = &pack[from..to];
    assert_eq!(crc32fast::hash(bytes), entry.crc);
  }
  repository.release(&mut store).unwrap();
}

/// A-614 part 2 (the base pack and its rebuild point). Do: with two versions encoded, report a
/// clone's time, rebuild when due, read the clone's pack; push a third version and read the clone
/// again. Expect: no rebuild due before a clone, one due after; the clone's pack every object the
/// head reaches; after the third version, the base pack and that version's fragments, together
/// everything its head reaches; a rebuild not due again until the rent reaches what it cost.
#[test]
fn a_clone_is_its_base_pack_and_the_fragments_after_it() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (mut repository, first, second) = two_versions(format, &mut store);
  let mut clock = Ticks(0);
  assert!(!repository.rebuild_due(1));
  repository.clone_took(1, 1_000);
  assert!(repository.rebuild_due(1));
  repository
    .rebuild_base(&mut store, 1, &mut clock, EXCHANGE)
    .unwrap();
  assert!(!repository.rebuild_due(1));
  let reach: BTreeSet<ObjectId> = first
    .iter()
    .chain(&second)
    .map(|object| object.id)
    .collect();
  let cloned = unpacked(
    format,
    &pack_of(&repository, &store, &reached_now(&repository, &store, 1)),
  );
  assert_eq!(cloned.keys().copied().collect::<BTreeSet<_>>(), reach);
  let third = commit_over(format, &text(200, &[3]), &[second[0].id], 3);
  push_and_move(
    &mut repository,
    &mut store,
    third[..3].to_vec(),
    &[(1, Some(third[0].id))],
  )
  .unwrap();
  repository
    .encode_fragments(&mut store, (1, 2), &mut clock, EXCHANGE)
    .unwrap();
  let cloned = unpacked(
    format,
    &pack_of(&repository, &store, &reached_now(&repository, &store, 1)),
  );
  assert!(third.iter().all(|object| cloned.contains_key(&object.id)));
  assert_eq!(repository.counters().rebuilds, 1);
  repository.clone_took(1, u64::MAX / 2);
  assert!(repository.rebuild_due(1));
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// Main's run (A-614 part 2): a pack of a fast-forward history, in hexadecimal on standard output,
/// for `git verify-pack -v` to judge once written to a pack file with its index made by `git
/// index-pack`.
#[test]
#[ignore = "main's run: git verify-pack over a sample pack"]
fn a_sample_pack_for_verify_pack() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, ..) = two_versions(format, &mut store);
  let pack = pack_of(
    &repository,
    &store,
    &brought_by(&repository, &store, 1, 0..=1),
  );
  let hex: String = pack.iter().map(|byte| format!("{byte:02x}")).collect();
  std::io::stdout().write_all(hex.as_bytes()).unwrap();
  repository.release(&mut store).unwrap();
}

/// Main's run (the double hashing): the time to read a pack of many objects through A-608's
/// reader, which names each, against the time the store's naming takes over the same objects
/// again; both on standard output.
#[test]
#[ignore = "main's run: the cost of naming a pack's objects twice"]
fn double_hashing_costs() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let objects: Vec<Made> = (0..2_000u32)
    .map(|at| {
      blob(
        format,
        &text(20, &[at % 20]).repeat(1 + usize::try_from(at % 7).unwrap()),
      )
    })
    .collect();
  let names: Vec<String> = (0..objects.len()).map(|at| format!("f{at:04}")).collect();
  let entries: Vec<(Mode, &[u8], ObjectId)> = names
    .iter()
    .zip(&objects)
    .map(|(name, object)| (Mode::File, name.as_bytes(), object.id))
    .collect();
  let root = tree(format, &entries);
  let head = commit(format, root.id, &[], 1);
  let mut pushed = vec![head.clone(), root];
  pushed.extend(objects.iter().cloned());
  push_and_move(&mut repository, &mut store, pushed, &[(1, Some(head.id))]).unwrap();
  repository
    .encode_fragments(&mut store, (1, 0), &mut Ticks(0), EXCHANGE)
    .unwrap();
  let pack = pack_of(
    &repository,
    &store,
    &brought_by(&repository, &store, 1, [0]),
  );
  let start = Instant::now();
  let read = read_whole(&pack, format, &NoIndex, &mut Admission::new(u64::MAX)).unwrap();
  let reading = start.elapsed();
  let start = Instant::now();
  let naming = repository.naming();
  for object in &read.objects {
    naming
      .name(object.kind.pack_number(), b"", &object.content)
      .unwrap();
  }
  let renaming = start.elapsed();
  let bytes: usize = read.objects.iter().map(|object| object.content.len()).sum();
  let line = format!(
    "objects {} bytes {bytes} read_whole {reading:?} named_again {renaming:?}\n",
    read.objects.len()
  );
  std::io::stdout().write_all(line.as_bytes()).unwrap();
  repository.release(&mut store).unwrap();
}

/// A clock whose every read jumps by a different, large amount: what a loaded shard's clock looks like to the encoder.
struct Jumps(u64, u64);

impl crate::store::learn::Clock for Jumps {
  fn now_ns(&mut self) -> u64 {
    // Format: xorshift64's shifts (Marsaglia, 2003), the jump its low 20 bits.
    self.1 ^= self.1 << 13;
    self.1 ^= self.1 >> 7;
    self.1 ^= self.1 << 17;
    self.0 = self.0.saturating_add(self.1 & 0xF_FFFF);
    self.0
  }
}

/// Main's ruling (2026-11-20; CLAUDE.md §4's determinism gates): the encoder's bytes are one function of its input. Do:
/// in two fresh repositories, push the same two versions (a 200-line file, then one line of it edited) and encode each
/// version's fragments, one under a clock that ticks a nanosecond a read, the other under one that jumps by a different
/// large amount every read. Expect: both versions' fragments byte for byte the same. Before the level and the window
/// were chosen from measured rates, each level sampled and each trial was timed on the clock, so the two could differ.
#[test]
fn a_versions_fragments_are_the_same_bytes_whatever_the_owners_clock_reads() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let encode = |clock: &mut dyn crate::store::learn::Clock| {
      let mut store = store();
      let mut repository = Repository::new(format);
      let first = commit_over(format, &text(200, &[]), &[], 1);
      let second = commit_over(format, &text(200, &[120]), &[first[0].id], 2);
      let mut kept = Vec::new();
      for (version, objects) in [first, second].into_iter().enumerate() {
        let head = objects[0].id;
        push_and_move(&mut repository, &mut store, objects, &[(1, Some(head))]).unwrap();
        repository
          .encode_fragments(&mut store, (1, version), clock, EXCHANGE)
          .unwrap();
        kept.push(
          repository
            .fragments(1, version)
            .map(|(objects, blobs)| (objects.to_vec(), blobs.to_vec())),
        );
      }
      repository.release(&mut store).unwrap();
      kept
    };
    let ticking = encode(&mut Ticks(0));
    let jumping = encode(&mut Jumps(0, 0x9E37_79B9_7F4A_7C15));
    assert!(ticking.iter().all(Option::is_some));
    assert_eq!(ticking, jumping);
  }
}
