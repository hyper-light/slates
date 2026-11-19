//! A-653 continued (2) (CLAUDE.md §3 "Bounded work everywhere"; §4.3's step budget): a version's fragments and a
//! green's base pack encoded a unit a turn, no unit encoding more than one object, walking more than one pair of
//! trees or reading more than a window's worth of rows; and the same fragments whatever the turns.

use slates_vcs::ContentStore;

use crate::object_name::{ObjectFormat, ObjectId};
use crate::objects::tree::Mode;
use crate::store::{Encoding, EncodingCounters, Progress, Repository, Turn};

use super::{EXCHANGE, Made, Ticks, WINDOW, blob, commit, push_and_move, store, tree};

/// Shape: the files of the tests' repository: more than one unit's worth of rows (a window's worth of handles, eight
/// bytes each, at the tests' 1 KiB window: 128) whichever version is encoded, so the rows are read over units.
const FILES: usize = 300;

/// Shape: the lines each file holds, so a changed file is long enough for a delta against its old bytes to win.
const LINES: usize = 16;

/// The rows one unit reads: a window's worth of handles, eight bytes each (`store::upload`'s `handles`).
fn handles() -> u64 {
  u64::try_from(WINDOW / size_of::<u64>()).unwrap()
}

/// File `at`'s bytes in version `version`: its number on each line, and from version 1, for every other file, the
/// version on its last line.
fn file(at: usize, version: u64) -> String {
  let mut text = format!("file {at}\n").repeat(LINES);
  if version > 0 && at.is_multiple_of(2) {
    text.push_str(&format!("version {version}\n"));
  }
  text
}

/// Version `version`'s commit over its files at `f000` on, its parent `parent`: the objects to push (the commit, the
/// tree, and the files the version changed, every file in version 0) and the commit's id.
fn version_objects(
  format: ObjectFormat,
  version: u64,
  parent: Option<ObjectId>,
) -> (Vec<Made>, ObjectId) {
  let files: Vec<Made> = (0..FILES)
    .map(|at| blob(format, file(at, version).as_bytes()))
    .collect();
  let names: Vec<String> = (0..FILES).map(|at| format!("f{at:03}")).collect();
  let entries: Vec<(Mode, &[u8], ObjectId)> = names
    .iter()
    .zip(&files)
    .map(|(name, file)| (Mode::File, name.as_bytes(), file.id))
    .collect();
  let root = tree(format, &entries);
  let parents: Vec<ObjectId> = parent.into_iter().collect();
  let head = commit(format, root.id, &parents, version + 1);
  let id = head.id;
  let mut objects = vec![head, root];
  objects.extend(
    files
      .into_iter()
      .enumerate()
      .filter(|(at, _)| version == 0 || at.is_multiple_of(2))
      .map(|(_, file)| file),
  );
  (objects, id)
}

/// Green 1 at two versions of [`FILES`] files in one tree, the second changing every other file at its path (so the
/// walk of the two trees names each old file as the base a reference delta may name); neither version encoded.
fn two_versions(format: ObjectFormat, store: &mut ContentStore) -> Repository {
  let mut repository = Repository::new(format);
  let mut parent = None;
  for version in 0..2 {
    let (objects, head) = version_objects(format, version, parent);
    push_and_move(&mut repository, store, objects, &[(1, Some(head))]).unwrap();
    parent = Some(head);
  }
  repository
}

/// One turn at a quantum of nothing did one unit: at most one object encoded, one pair of trees walked, a window's
/// worth of rows read.
fn one_unit(before: EncodingCounters, after: EncodingCounters) {
  assert_eq!(after.units - before.units, 1, "{before:?} {after:?}");
  assert!(after.objects - before.objects <= 1, "{before:?} {after:?}");
  assert!(
    after.trees_walked - before.trees_walked <= 1,
    "{before:?} {after:?}"
  );
  assert!(
    after.rows_read - before.rows_read <= handles(),
    "{before:?} {after:?}"
  );
}

/// `encoding` driven to its end a turn at a time, each with a quantum of nothing, each turn checked by [`one_unit`]:
/// the turns it took.
fn one_unit_a_turn(
  encoding: &mut Encoding,
  repository: &mut Repository,
  store: &mut ContentStore,
) -> u64 {
  let mut clock = Ticks(0);
  let mut turns = 0;
  loop {
    let before = encoding.counters();
    let turn = Turn {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: 0,
    };
    let progress = encoding.advance(repository, store, turn).unwrap();
    one_unit(before, encoding.counters());
    turns += 1;
    if progress == Progress::Done {
      return turns;
    }
  }
}

/// A-653 continued (2) (CLAUDE.md §3 "Bounded work everywhere": a version's encoding is sliced under the owner's
/// quantum). Do: encode the second of two versions of 300 files, which changed 150 of them, a turn at a time with a
/// quantum of nothing; and the same version of a twin repository in one pass. Expect: every turn one unit, none
/// encoding more than one object, walking more than one pair of trees or reading more than a window's worth of rows;
/// the rows read over more than one unit; and both fragments the twin's, byte for byte.
#[test]
fn a_versions_fragments_are_encoded_an_object_a_unit_as_one_pass_encodes_them() {
  let format = ObjectFormat::Sha1;
  let (mut store_turned, mut store_whole) = (store(), store());
  let mut turned = two_versions(format, &mut store_turned);
  let mut whole = two_versions(format, &mut store_whole);
  whole
    .encode_fragments(&mut store_whole, (1, 1), &mut Ticks(0), EXCHANGE)
    .unwrap();
  let mut encoding = Encoding::fragments(&turned, &store_turned, 1, 1).unwrap();
  let turns = one_unit_a_turn(&mut encoding, &mut turned, &mut store_turned);
  let counters = encoding.counters();
  assert!(
    counters.rows_read > handles() && turns > counters.objects,
    "{turns} turns, {counters:?}"
  );
  for blobs in [false, true] {
    assert_eq!(
      turned.fragment_part(1, 1, blobs),
      whole.fragment_part(1, 1, blobs)
    );
  }
  turned.release(&mut store_turned).unwrap();
  whole.release(&mut store_whole).unwrap();
}

/// A-653 continued (2) (a base pack, every object a head reaches, made under the owner's quantum too). Do: make green
/// 1's base pack a turn at a time with a quantum of nothing, and the twin's in one pass. Expect: every turn one unit,
/// none encoding more than one object or reading more than a window's worth of rows; and both parts the twin's, byte
/// for byte.
#[test]
fn a_base_pack_is_made_an_object_a_unit_as_one_pass_makes_it() {
  let format = ObjectFormat::Sha1;
  let (mut store_turned, mut store_whole) = (store(), store());
  let mut turned = two_versions(format, &mut store_turned);
  let mut whole = two_versions(format, &mut store_whole);
  whole
    .rebuild_base(&mut store_whole, 1, &mut Ticks(0), EXCHANGE)
    .unwrap();
  let mut encoding = Encoding::base(&turned, 1).unwrap();
  let turns = one_unit_a_turn(&mut encoding, &mut turned, &mut store_turned);
  assert!(
    turns > encoding.counters().objects,
    "{turns} turns, {:?}",
    encoding.counters()
  );
  for blobs in [false, true] {
    assert_eq!(turned.base_part(1, blobs), whole.base_part(1, blobs));
  }
  turned.release(&mut store_turned).unwrap();
  whole.release(&mut store_whole).unwrap();
}
