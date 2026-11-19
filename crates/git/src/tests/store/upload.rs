//! upload-pack's pack from fragments (A-639; §4.17.2 "A fetch", item 3): a clone answered with the base pack and the
//! fragments after it, byte for byte; a fetch with its run's fragments and then the objects no fragment covers,
//! encoded; thin packs and offset deltas only for a client that said so; a pack streamed a unit a turn the same as in
//! one turn; a filter on a run; a base pack made again under a stream refused, typed; the set operations the stream
//! decides by against a set model; and an oracle over generated histories holding every answered pack to the wanted
//! closure less what the client holds.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_vcs::ContentStore;

use crate::Admission;
use crate::entry::{self, PACK_HEADER_LEN, Representation};
use crate::negotiation::Filter;
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tree::Mode;
use crate::store::{Positions, Progress, Repository, StoreRefused, Turn, Upload, Versions};
use crate::unpack::{NoIndex, Object, Outside, Reader, Refusal};

use super::fragments::{commit_over, text};
use super::oracle::{GREENS, Model, Real, apply, op};
use super::{
  Answer, EXCHANGE, Made, Ticks, blob, brought_by, commit, ids, push_and_move, store, streamed,
  tree,
};

/// The objects a client holds: a thin pack's bases.
struct Held(BTreeMap<ObjectId, (Kind, Vec<u8>)>);

impl Held {
  fn none() -> Held {
    Held(BTreeMap::new())
  }

  fn of<'m>(made: impl IntoIterator<Item = &'m Made>) -> Held {
    Held(
      made
        .into_iter()
        .map(|object| (object.id, (object.kind, object.content.clone())))
        .collect(),
    )
  }
}

impl Outside for Held {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self
      .0
      .get(id)
      .map(|(kind, content)| (*kind, content.as_slice()))
  }
}

/// A pack's objects read back through A-608's reader, `held`'s objects a thin pack's bases, in pack order.
fn read_back(format: ObjectFormat, pack: &[u8], held: &Held) -> Result<Vec<Object>, Refusal> {
  let mut admission = Admission::new(u64::MAX);
  let mut reader = Reader::new(format, &NoIndex);
  let used = reader.read(pack, &mut admission)?;
  if used < pack.len() {
    return Err(Refusal::TrailingBytes {
      at: u64::try_from(used).unwrap_or(u64::MAX),
    });
  }
  Ok(reader.finish_with(held, &mut admission)?.objects)
}

/// The ids read, each once: a repeat fails the test.
fn once(objects: &[Object]) -> BTreeSet<ObjectId> {
  let ids: BTreeSet<ObjectId> = objects.iter().map(|object| object.id).collect();
  assert_eq!(ids.len(), objects.len(), "an object sent twice");
  ids
}

/// How each entry holds its object, as the pack's bytes say.
fn representations(format: ObjectFormat, pack: &[u8], objects: &[Object]) -> Vec<Representation> {
  objects
    .iter()
    .map(|object| {
      let at = usize::try_from(object.offset).unwrap();
      let (header, _) = entry::read(
        &pack[at..],
        object.offset,
        format,
        &mut Admission::new(u64::MAX),
      )
      .unwrap()
      .unwrap();
      header.representation
    })
    .collect()
}

/// A ref, green 1, at three versions over a 200-line text, each editing one line more, and a small file, every version
/// encoded; its base pack made at version 1 when `base`. The repository and each version's objects.
fn three_versions(
  format: ObjectFormat,
  store: &mut ContentStore,
  base: bool,
) -> (Repository, Vec<Vec<Made>>) {
  let mut repository = Repository::new(format);
  let mut clock = Ticks(0);
  let mut versions: Vec<Vec<Made>> = Vec::new();
  for (version, edited) in [&[][..], &[120][..], &[120, 3][..]].into_iter().enumerate() {
    let parents: Vec<ObjectId> = versions.last().map(|last| last[0].id).into_iter().collect();
    let seconds = u64::try_from(version).unwrap() + 1;
    let objects = commit_over(format, &text(200, edited), &parents, seconds);
    let head = objects[0].id;
    push_and_move(&mut repository, store, objects.clone(), &[(1, Some(head))]).unwrap();
    repository
      .encode_fragments(store, (1, version), &mut clock, EXCHANGE)
      .unwrap();
    if base && version == 1 {
      repository
        .rebuild_base(store, 1, &mut clock, EXCHANGE)
        .unwrap();
    }
    versions.push(objects);
  }
  (repository, versions)
}

/// The run of versions 1 and 2 of green 1, for a client holding version 0.
fn run_answer(thin: bool, filter: Option<Filter>) -> Answer {
  Answer {
    runs: vec![Versions {
      green: 1,
      after: 0,
      through: 2,
    }],
    objects: Vec::new(),
    thin,
    ofs: true,
    filter,
  }
}

/// The ids D(1) and D(2) of green 1 hold.
fn run_ids(repository: &Repository, store: &ContentStore) -> BTreeSet<ObjectId> {
  ids(repository, store, &brought_by(repository, 1, [1, 2]))
    .into_iter()
    .collect()
}

/// A-639 (§4.17.2 "A clone is the base pack and then the fragments after version C"). Do: make a ref's base pack at
/// version 1 of three and answer a clone of its head, every object it reaches one by one. Expect: between the header
/// and the trailer, the base pack's two parts and version 2's two fragments byte for byte, in that order; read back,
/// exactly the objects the head reaches, each once; nothing read entry by entry or encoded afresh.
#[test]
fn a_clone_is_the_base_pack_then_the_fragments_after_it() {
  for format in [ObjectFormat::Sha1, ObjectFormat::Sha256] {
    let mut store = store();
    let (repository, versions) = three_versions(format, &mut store, true);
    let reached = repository.reached(1).unwrap().clone();
    let (pack, counters) = streamed(&repository, &store, Answer::objects(&reached), u64::MAX);
    let parts = [
      repository.base_part(1, false).unwrap().1,
      repository.base_part(1, true).unwrap().1,
      repository.fragment_part(1, 2, false).unwrap(),
      repository.fragment_part(1, 2, true).unwrap(),
    ];
    let kept: Vec<u8> = parts
      .iter()
      .flat_map(|part| store.object_bytes(&part.content, part.len).unwrap())
      .collect();
    let header = usize::try_from(PACK_HEADER_LEN).unwrap();
    assert_eq!(
      &pack[header..pack.len() - format.raw_len()],
      kept.as_slice()
    );
    let read = read_back(format, &pack, &Held::none()).unwrap();
    let reach: BTreeSet<ObjectId> = versions.iter().flatten().map(|object| object.id).collect();
    assert_eq!(once(&read), reach);
    let loose = counters.loose_whole + counters.loose_deltas;
    assert_eq!(
      (counters.parts_copied, counters.parts_by_entry, loose),
      (4, 0, 0)
    );
    repository.release(&mut store).unwrap();
    assert!(store.is_empty());
  }
}

/// A-639 (§4.17.2 "A fetch" item 3: "the fragments for D(M+1) to D(N) in order, then any objects the walk found").
/// Do: answer, thin, a client holding a ref's version 0 that wants its version 2 (a run of versions 1 and 2), with the
/// objects of another ref one by one, a commit over three similar files never encoded. Expect: read back with the
/// client's objects as bases, exactly D(1), D(2) and the other ref's objects, each once; the run's four fragments
/// copied whole; the other ref's objects encoded, at least one as a delta.
#[test]
fn a_fetch_is_its_runs_fragments_then_what_no_fragment_covers() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (mut repository, versions) = three_versions(format, &mut store, false);
  let similar: Vec<Made> = [&[][..], &[5][..], &[5, 20][..]]
    .into_iter()
    .map(|edited| blob(format, &text(30, edited)))
    .collect();
  let root = tree(
    format,
    &[
      (Mode::File, b"p", similar[0].id),
      (Mode::File, b"q", similar[1].id),
      (Mode::File, b"r", similar[2].id),
    ],
  );
  let head = commit(format, root.id, &[], 9);
  let mut other = vec![head.clone(), root];
  other.extend(similar);
  push_and_move(
    &mut repository,
    &mut store,
    other.clone(),
    &[(2, Some(head.id))],
  )
  .unwrap();
  let mut answer = run_answer(true, None);
  answer.objects = repository.brought(2, 0).unwrap().iter().collect();
  let (pack, counters) = streamed(&repository, &store, answer, u64::MAX);
  let read = read_back(format, &pack, &Held::of(&versions[0])).unwrap();
  let mut expected = run_ids(&repository, &store);
  expected.extend(other.iter().map(|object| object.id));
  assert_eq!(once(&read), expected);
  assert_eq!(counters.parts_copied, 4);
  assert!(counters.loose_deltas >= 1, "{counters:?}");
  repository.release(&mut store).unwrap();
}

/// A-639 (gitprotocol-capabilities: "A client MUST NOT request the 'thin-pack' capability if it cannot turn a thin
/// pack into a self-contained pack"). Do: answer the run of versions 1 and 2 to a client holding version 0, thin and
/// then not. Expect: the thin pack refused without the client's objects and read back with them; the other read back
/// alone, its deltas against the client's objects sent whole instead; both exactly the run's objects, each once.
#[test]
fn a_thin_pack_names_the_clients_objects_and_a_full_one_never_does() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, versions) = three_versions(format, &mut store, false);
  let expected = run_ids(&repository, &store);
  let (thin, _) = streamed(&repository, &store, run_answer(true, None), u64::MAX);
  assert!(read_back(format, &thin, &Held::none()).is_err());
  let read = read_back(format, &thin, &Held::of(&versions[0])).unwrap();
  assert_eq!(once(&read), expected);
  let (full, counters) = streamed(&repository, &store, run_answer(false, None), u64::MAX);
  assert_eq!(
    once(&read_back(format, &full, &Held::none()).unwrap()),
    expected
  );
  assert!(
    counters.made_whole >= 1 && counters.parts_by_entry >= 1,
    "{counters:?}"
  );
  repository.release(&mut store).unwrap();
}

/// One version of green 1 over three similar 200-line files, encoded: its blobs offset deltas in its fragment.
fn similar_files(format: ObjectFormat, store: &mut ContentStore) -> (Repository, Vec<Made>) {
  let mut repository = Repository::new(format);
  let files: Vec<Made> = [&[][..], &[50, 51][..], &[50, 51, 150][..]]
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
  push_and_move(
    &mut repository,
    store,
    objects.clone(),
    &[(1, Some(head.id))],
  )
  .unwrap();
  repository
    .encode_fragments(store, (1, 0), &mut Ticks(0), EXCHANGE)
    .unwrap();
  (repository, objects)
}

/// A-639 (gitprotocol-capabilities `ofs-delta`). Do: answer a version whose fragment holds offset deltas, its objects
/// one by one, to a client without `ofs-delta` and then to one with it. Expect: without, no entry names its base by
/// offset, those deltas named by id and their headers written again; with, at least one offset delta; both read back
/// to the version's objects, each once.
#[test]
fn without_ofs_delta_no_entry_names_its_base_by_offset() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, objects) = similar_files(format, &mut store);
  let expected: BTreeSet<ObjectId> = objects.iter().map(|object| object.id).collect();
  let mut answer = Answer::objects(repository.brought(1, 0).unwrap());
  let is_offset =
    |representation: &Representation| matches!(representation, Representation::OfsDelta { .. });
  for ofs in [false, true] {
    answer.ofs = ofs;
    let (pack, counters) = streamed(&repository, &store, answer.clone(), u64::MAX);
    let read = read_back(format, &pack, &Held::none()).unwrap();
    assert_eq!(once(&read), expected);
    let offsets = representations(format, &pack, &read)
      .iter()
      .filter(|representation| is_offset(representation))
      .count();
    assert_eq!(offsets > 0, ofs, "{counters:?}");
    assert_eq!(counters.headers_rewritten > 0, !ofs, "{counters:?}");
  }
  repository.release(&mut store).unwrap();
}

/// A-639 ("streamed in slices under the shard's budget, never whole"). Do: answer the clone and the thin fetch in
/// turns of one nanosecond of the tests' clock, and again in one unbounded turn. Expect: the same bytes either way; a
/// unit a turn, so more turns than the pack has parts and objects; one turn unbounded.
#[test]
fn a_pack_streamed_a_unit_a_turn_is_the_pack_in_one_turn() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, _) = three_versions(format, &mut store, true);
  let clone = Answer::objects(repository.reached(1).unwrap());
  for answer in [clone, run_answer(true, None)] {
    let (whole, single) = streamed(&repository, &store, answer.clone(), u64::MAX);
    let (sliced, turns) = streamed(&repository, &store, answer, 1);
    assert_eq!(sliced, whole);
    assert_eq!(single.turns, 1);
    assert!(turns.turns > 8, "{turns:?}");
  }
  repository.release(&mut store).unwrap();
}

/// A-639 (A-615: a run's objects pass the request's filter, and the want is sent whatever it is). Do: answer the run
/// of versions 1 and 2, thin, under `blob:none` and then under `object:type=blob`. Expect: under `blob:none` the run's
/// commits and trees and no blob; under `object:type=blob` its blobs and the head it wants, no other commit and no
/// tree; each read back with the client's objects the filter sends as bases, each object once.
#[test]
fn a_filter_sends_the_runs_objects_it_admits_and_the_want_always() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (repository, versions) = three_versions(format, &mut store, false);
  let run: Vec<&Made> = versions[1..].iter().flatten().collect();
  let want = versions[2][0].id;
  for filter in [Filter::BlobNone, Filter::ObjectType(Kind::Blob)] {
    let admits =
      |made: &Made| filter.admits(made.kind, u64::try_from(made.content.len()).unwrap(), 0);
    let expected: BTreeSet<ObjectId> = run
      .iter()
      .filter(|made| admits(made) || made.id == want)
      .filter(|made| !versions[0].iter().any(|old| old.id == made.id))
      .map(|made| made.id)
      .collect();
    let held = Held::of(versions[0].iter().filter(|made| admits(made)));
    let answer = run_answer(true, Some(filter.clone()));
    let (pack, _) = streamed(&repository, &store, answer, u64::MAX);
    assert_eq!(once(&read_back(format, &pack, &held).unwrap()), expected);
  }
  repository.release(&mut store).unwrap();
}

/// A-639 (the stream holds no borrow between turns; a base pack made again under it is refused, never mixed). Do:
/// answer a clone of a ref whose base pack holds offset deltas to a client without `ofs-delta`, a unit a turn, and make
/// the base pack again at a newer version as soon as the stream reads one of its parts entry by entry. Expect: a later
/// turn refused `BaseRebuilt`, naming the green.
#[test]
fn a_base_pack_made_again_under_a_stream_ends_it_refused() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let (mut repository, _) = three_versions(format, &mut store, true);
  let mut answer = Answer::objects(repository.reached(1).unwrap());
  answer.ofs = false;
  let mut upload = Upload::new(answer, &repository);
  let mut clock = Ticks(0);
  let mut pack = Vec::new();
  let mut rebuilt = false;
  let refused = loop {
    let turn = Turn {
      clock: &mut clock,
      exchange: EXCHANGE,
      quantum_ns: 1,
    };
    match upload.advance(&repository, &store, turn, &mut pack) {
      Ok(Progress::More) => {}
      Ok(Progress::Done) => panic!("the stream ended over a base pack made again"),
      Err(refused) => break refused,
    }
    if !rebuilt && upload.counters().parts_by_entry > 0 {
      repository
        .rebuild_base(&mut store, 1, &mut Ticks(0), EXCHANGE)
        .unwrap();
      rebuilt = true;
    }
  };
  assert!(rebuilt);
  assert_eq!(refused, StoreRefused::BaseRebuilt { green: 1 });
  repository.release(&mut store).unwrap();
}

/// The maximal runs of a sorted set of positions.
fn runs_of(set: &BTreeSet<u64>) -> usize {
  set
    .iter()
    .filter(|position| {
      position
        .checked_sub(1)
        .is_none_or(|before| !set.contains(&before))
    })
    .count()
}

/// The set operations on `a` and `b` against the set model, and the runs each result is kept as.
fn check_sets(a: &BTreeSet<u64>, b: &BTreeSet<u64>) -> Result<(), TestCaseError> {
  let (pa, pb) = (
    Positions::of(a.iter().copied().collect()),
    Positions::of(b.iter().copied().collect()),
  );
  let both: BTreeSet<u64> = a.intersection(b).copied().collect();
  let only: BTreeSet<u64> = a.difference(b).copied().collect();
  let (meet, rest) = (pa.intersection(&pb), pa.difference(&pb));
  prop_assert_eq!(meet.iter().collect::<BTreeSet<u64>>(), both.clone());
  prop_assert_eq!(rest.iter().collect::<BTreeSet<u64>>(), only.clone());
  prop_assert_eq!((meet.runs(), rest.runs()), (runs_of(&both), runs_of(&only)));
  prop_assert_eq!(pa.is_subset(&pb), a.is_subset(b));
  for from in 0..66 {
    prop_assert_eq!(pa.next_from(from), a.range(from..).next().copied());
  }
  Ok(())
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(256), ..ProptestConfig::default() }))]

  /// A-639 (the stream decides by sets of positions, kept as runs). Do: make two sets of positions from a narrow range,
  /// so runs touch and overlap, and insert more one at a time, in any order. Expect: intersection, difference, subset,
  /// insertion and the least position at or after each one as the set model gives them, each result kept as the
  /// fewest runs.
  #[test]
  fn position_sets_answer_as_a_set_model(a in proptest::collection::vec(0u64..64, 0..40), b in proptest::collection::vec(0u64..64, 0..40), more in proptest::collection::vec(0u64..64, 0..12)) {
    let (mut a, b): (BTreeSet<u64>, BTreeSet<u64>) = (a.into_iter().collect(), b.into_iter().collect());
    check_sets(&a, &b)?;
    let mut grown = Positions::of(a.iter().copied().collect());
    for position in more {
      grown.insert(position);
      a.insert(position);
      prop_assert_eq!(grown.iter().collect::<BTreeSet<u64>>(), a.clone());
      prop_assert_eq!(grown.runs(), runs_of(&a));
    }
    check_sets(&a, &b)?;
  }
}

/// What a generated answer asks: thin, ofs-delta, the filter's choice, the quantum's choice.
type Caps = (bool, bool, u8, u8);

/// The filter a choice names: none twice as often as each filter.
fn filter_of(choice: u8) -> Option<Filter> {
  match choice {
    0 | 1 => None,
    2 => Some(Filter::BlobNone),
    3 => Some(Filter::BlobLimit(5)),
    _ => Some(Filter::ObjectType(Kind::Blob)),
  }
}

/// The quantum a choice names: a unit a turn, a few, or one turn.
fn quantum_of(choice: u8) -> u64 {
  match choice {
    0 => 1,
    1 => 5,
    _ => u64::MAX,
  }
}

/// Whether every version of `heads` after `after` up to `through` fast-forwarded the one before it.
fn fast_forwards(model: &Model, heads: &[Option<ObjectId>], after: usize, through: usize) -> bool {
  (after + 1..=through).all(|version| match (heads[version - 1], heads[version]) {
    (Some(before), Some(now)) => model.reach(Some(now)).contains(&before),
    _ => false,
  })
}

/// The answer the negotiation (A-615) gives a client holding `heads[after]` (nothing, for a clone) that wants
/// `heads[through]`: a run where every version between fast-forwarded, the wanted objects one by one otherwise; and
/// those wanted objects.
fn answer_for(
  real: &Real,
  model: &Model,
  (green, after, through): (u64, Option<usize>, usize),
  (thin, ofs, filter): (bool, bool, Option<Filter>),
) -> (Answer, BTreeSet<ObjectId>) {
  let heads = &model.heads[&green];
  let want = heads[through].unwrap();
  let held = model.reach(after.and_then(|after| heads[after]));
  let admits = |id: &ObjectId| {
    let made = &model.objects[id];
    let size = u64::try_from(made.content.len()).unwrap();
    *id == want
      || filter
        .as_ref()
        .is_none_or(|filter| filter.admits(made.kind, size, 0))
  };
  let wanted: BTreeSet<ObjectId> = model
    .reach(Some(want))
    .difference(&held)
    .filter(|id| admits(id))
    .copied()
    .collect();
  let mut answer = Answer {
    runs: Vec::new(),
    objects: Vec::new(),
    thin,
    ofs,
    filter,
  };
  match after.filter(|after| fast_forwards(model, heads, *after, through)) {
    Some(after) => answer.runs.push(Versions {
      green,
      after: u64::try_from(after).unwrap(),
      through: u64::try_from(through).unwrap(),
    }),
    None => {
      let positions = wanted
        .iter()
        .map(|id| real.repository.object(&real.store, id).unwrap().position)
        .collect();
      answer.objects = Positions::of(positions).iter().collect();
    }
  }
  (answer, wanted)
}

/// The objects the client holds that the filter sends: a thin pack's bases.
fn held_for(model: &Model, held: Option<ObjectId>, filter: Option<&Filter>) -> Held {
  let reach = model.reach(held);
  Held::of(reach.iter().map(|id| &model.objects[id]).filter(|made| {
    let size = u64::try_from(made.content.len()).unwrap();
    filter.is_none_or(|filter| filter.admits(made.kind, size, 0))
  }))
}

/// One answer streamed and read back (the oracle's module docs).
fn check_answer(real: &Real, model: &Model, fetch: (u64, Option<usize>, usize), caps: Caps) {
  let (green, after, through) = fetch;
  let (thin, ofs, filter, quantum) = caps;
  let filter = filter_of(filter);
  let (answer, wanted) = answer_for(real, model, fetch, (thin, ofs, filter.clone()));
  let (pack, counters) = streamed(&real.repository, &real.store, answer, quantum_of(quantum));
  let heads = &model.heads[&green];
  let held = if thin {
    held_for(model, after.and_then(|after| heads[after]), filter.as_ref())
  } else {
    Held::none()
  };
  let read = read_back(model.format, &pack, &held).unwrap();
  assert_eq!(once(&read), wanted, "{fetch:?} {caps:?}");
  for object in &read {
    assert_eq!(object.content, model.objects[&object.id].content);
  }
  let brought = real.repository.brought(green, through).unwrap();
  if after.is_none() && filter.is_none() && !brought.is_empty() {
    assert!(
      counters.parts_copied + counters.parts_by_entry > 0,
      "{counters:?}"
    );
  }
}

/// A clone of every green with a head, and a fetch from each earlier version's head it had, each with the next caps.
fn check_answers(real: &Real, model: &Model, caps: &[Caps]) {
  let mut next = caps.iter().cycle();
  for (green, heads) in &model.heads {
    let Some(through) = heads
      .len()
      .checked_sub(1)
      .filter(|last| heads[*last].is_some())
    else {
      continue;
    };
    let afters = (0..through)
      .filter(|after| heads[*after].is_some())
      .map(Some);
    for after in [None].into_iter().chain(afters) {
      check_answer(real, model, (*green, after, through), *next.next().unwrap());
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(48), ..ProptestConfig::default() }))]

  /// A-639 (the oracle: "every answered pack read back through `unpack` gives exactly the wanted closure minus the
  /// client's haves"). Do: run a generated history (the store oracle's), making some greens' base packs halfway; then,
  /// for every green with a head, answer a clone of it and a fetch from each earlier version's head as A-615 would (a
  /// run where every version after the have fast-forwarded, the wanted objects one by one otherwise), each under a
  /// generated filter, thin or not, `ofs-delta` or not, streamed in turns of a generated quantum. Expect: every pack
  /// read back through A-608's reader, the client's objects the filter sends its thin bases, exactly what the want
  /// reaches less what the have reaches, the filter applied and the want sent whatever it is; each object once with its
  /// bytes; and a clone copying kept parts whenever its head's version brought anything.
  #[test]
  fn every_answered_pack_reads_back_as_the_wanted_closure_less_the_clients(sha256 in any::<bool>(), ops in proptest::collection::vec(op(), 1..20), bases in proptest::collection::vec(any::<bool>(), 3), caps in proptest::collection::vec((any::<bool>(), any::<bool>(), 0u8..5, 0u8..3), 1..8)) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let mut real = Real { store: store(), repository: Repository::new(format), clock: Ticks(0) };
    let mut model = Model::new(format);
    let half = ops.len() / 2;
    for (at, op) in ops.iter().enumerate() {
      if at == half {
        for (green, base) in (0..GREENS).zip(&bases) {
          if *base && model.heads.get(&green).and_then(|heads| heads.last()).is_some_and(Option::is_some) {
            real.repository.rebuild_base(&mut real.store, green, &mut Ticks(0), EXCHANGE).unwrap();
          }
        }
      }
      apply(&mut real, &mut model, op);
    }
    check_answers(&real, &model, &caps);
    let Real { mut store, repository, .. } = real;
    repository.release(&mut store).unwrap();
    prop_assert!(store.is_empty());
  }
}
