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
  let mut encoding = Encoding::base(&turned, &store_turned, 1).unwrap();
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

/// A-718 (§4.17.2 "A push" item 8: a version's fragments encoded once, off the answer's path). Do: ask a repository
/// whose green 1 has two versions for its first unencoded version, encode it, ask again, encode that, and ask again.
/// Expect: green 1's first version, then the version after it, then none: each version is offered once, in order, until
/// every one's fragments are kept.
#[test]
fn every_version_is_offered_for_encoding_once_in_order_until_all_are_kept() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = two_versions(format, &mut store);
  let (green, first) = repository.first_unencoded().unwrap();
  assert_eq!(green, 1);
  repository
    .encode_fragments(&mut store, (green, first), &mut Ticks(0), EXCHANGE)
    .unwrap();
  assert_eq!(repository.first_unencoded(), Some((1, first + 1)));
  repository
    .encode_fragments(&mut store, (1, first + 1), &mut Ticks(0), EXCHANGE)
    .unwrap();
  assert_eq!(repository.first_unencoded(), None);
  repository.release(&mut store).unwrap();
}

/// Shape: the bytes of search or deflate the red test's quantum buys at its rates ([`A_NANOSECOND_A_BYTE`]): under the
/// tests' 1 KiB window, so a unit sized by its cost is told from one sized by the window.
const PUT_BYTES: u64 = 64;

/// The tests' exchange with the machine's rates set to a nanosecond a byte for search and every deflate level (1,024
/// ns a KiB), so a quantum of [`PUT_BYTES`] nanoseconds buys [`PUT_BYTES`] bytes of either.
const A_NANOSECOND_A_BYTE: crate::store::learn::Exchange = crate::store::learn::Exchange {
  rates: crate::store::learn::Rates {
    deflate_ns_per_kib: [1024; 10],
    deflate_saved_per_mille: [500; 10],
    search_ns_per_kib: 1024,
  },
  ..EXCHANGE
};

/// A-718 (main's ruling of 2026-11-21 07:30; w-git's r52 on git/git: 844,380 of the encoder's 992,574 turns ran a unit
/// past the step budget, every search and deflate step a window's work at a quantum of a few microseconds). Do: encode
/// the second of two versions a turn at a time at a quantum of [`PUT_BYTES`] nanoseconds, the machine's rates a
/// nanosecond a byte; and the same version of a twin in one pass at those rates. Expect no search or deflate step given
/// more than the [`PUT_BYTES`] bytes the quantum buys, a deflate step one piece at least
/// (`fragment::DEFLATE_PIECE`), against the window each was given before; and both fragments the twin's, byte for byte.
#[test]
fn an_objects_search_and_deflate_steps_are_sized_by_their_measured_cost_within_the_quantum() {
  let format = ObjectFormat::Sha1;
  let (mut store_turned, mut store_whole) = (store(), store());
  let mut turned = two_versions(format, &mut store_turned);
  let mut whole = two_versions(format, &mut store_whole);
  whole
    .encode_fragments(&mut store_whole, (1, 1), &mut Ticks(0), A_NANOSECOND_A_BYTE)
    .unwrap();
  let mut encoding = Encoding::fragments(&turned, &store_turned, 1, 1).unwrap();
  let mut clock = Ticks(0);
  loop {
    let turn = Turn {
      clock: &mut clock,
      exchange: A_NANOSECOND_A_BYTE,
      quantum_ns: PUT_BYTES,
    };
    if encoding
      .advance(&mut turned, &mut store_turned, turn)
      .unwrap()
      == Progress::Done
    {
      break;
    }
  }
  let counters = encoding.counters();
  assert!(
    counters.objects > 0
      && counters.most_put_bytes <= PUT_BYTES.max(crate::store::fragment::DEFLATE_PIECE as u64),
    "{counters:?}"
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

/// A-856 (main's ruling of 2026-11-22 00:30: an encoding's parts were held outside every budget, clients' table naming
/// 2.5 GB live behind git/git's clone). Do: encode the second of [`two_versions`] a turn at a time with a quantum of
/// nothing, reading after each turn what the encoding holds and what the content store has charged; encode a twin's in
/// one pass; then begin it again on a third, turn it until it holds part bytes, and release it. Expect: after every turn
/// the store's charge covers what the encoding holds of its parts, which grows past nothing before the end; at the end
/// it holds nothing and the store's charge is the twin's, nothing of the parts left charged; released, the third's charge
/// is back where it began.
#[test]
fn an_encodings_parts_are_charged_to_the_store_as_they_grow_and_given_back_at_its_end() {
  let format = ObjectFormat::Sha1;
  let (mut store, mut store_whole) = (store(), store());
  let mut repository = two_versions(format, &mut store);
  let mut whole = two_versions(format, &mut store_whole);
  whole
    .encode_fragments(&mut store_whole, (1, 1), &mut Ticks(0), EXCHANGE)
    .unwrap();
  let charged = store.charged();
  let mut encoding = Encoding::fragments(&repository, &store, 1, 1).unwrap();
  let mut clock = Ticks(0);
  let mut most = 0;
  loop {
    let turn = Turn {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: 0,
    };
    let progress = encoding.advance(&mut repository, &mut store, turn).unwrap();
    assert!(
      store.charged() >= charged + encoding.held_bytes(),
      "charged as held: {} past {charged}, {} held",
      store.charged(),
      encoding.held_bytes()
    );
    most = most.max(encoding.held_bytes());
    if progress == Progress::Done {
      break;
    }
  }
  assert!(most > 0, "the parts were held between turns");
  assert_eq!(encoding.held_bytes(), 0);
  assert_eq!(
    store.charged(),
    store_whole.charged(),
    "no part left charged"
  );
  repository.release(&mut store).unwrap();
  whole.release(&mut store_whole).unwrap();
  let mut store = super::store();
  let mut repository = two_versions(format, &mut store);
  let charged = store.charged();
  let mut encoding = Encoding::fragments(&repository, &store, 1, 1).unwrap();
  while encoding.held_bytes() == 0 {
    let turn = Turn {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: 0,
    };
    assert_eq!(
      encoding.advance(&mut repository, &mut store, turn).unwrap(),
      Progress::More
    );
  }
  assert!(store.charged() >= charged + encoding.held_bytes());
  encoding.release(&mut store);
  assert_eq!(store.charged(), charged, "released, nothing charged");
  repository.release(&mut store).unwrap();
}
