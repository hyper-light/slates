//! A-614's D(k) and reached sets as a type of derived data (A-647 part 2): every version's value derived through
//! `slates-vcs`'s mechanism from its parents' values is the store's own D(k) and reached set, and is, identity for
//! identity, the value the store kept from its admission as it committed; and (A-647 part 3) the store serves every
//! version's two sets from that value alone.

use std::collections::BTreeSet;
use std::mem::size_of;

use proptest::prelude::*;
use slates_vcs::derived::{Derivations, DerivedRefused, DerivedType, Graph, Mode, Progress, Read};
use slates_vcs::{ContentId, ContentStore, Inserted};

use crate::object_name::{ObjectFormat, ObjectId};
use crate::objects::tree::Mode as FileMode;
use crate::store::repository::reach::Shape;
use crate::store::{Positions, ReachRefused, ReachSets, Repository, Sets};

use super::oracle::{GREENS, Model, Real, apply, op};
use super::{
  BUDGET, Made, Ticks, WINDOW, blob, commit, ids, line_and_branch, push_and_move, sorted, store,
  tree, wide,
};

/// A read's answer as the tests take it.
type Answer = Result<ContentId, DerivedRefused<(u64, u64), ReachRefused>>;

/// `version`'s value read from `values`, its fill run to its end.
fn read_to_end(
  values: &mut Derivations,
  ty: &mut ReachSets,
  repository: &Repository,
  store: &mut ContentStore,
  version: (u64, u64),
) -> Answer {
  let mut fill = match values.read(store, ty, repository, version) {
    Ok(Read::Ready(root)) => return Ok(root),
    Ok(Read::Fill(fill)) => fill,
    Err(refused) => return Err(refused),
  };
  let answer = loop {
    match fill.step(values, ty, repository, store) {
      Ok(Progress::More) => {}
      Ok(Progress::Done(root)) => break Ok(root),
      Err(refused) => break Err(refused),
    }
  };
  fill.release(store).unwrap();
  answer
}

/// What the store's walks have met: objects walked, and objects met already reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Walks {
  walked: u64,
  reached_met: u64,
}

impl Walks {
  /// What was walked after `earlier`.
  fn since(self, earlier: Walks) -> Walks {
    Walks {
      walked: self.walked - earlier.walked,
      reached_met: self.reached_met - earlier.reached_met,
    }
  }
}

/// The store's walk counters now.
fn walks(repository: &Repository) -> Walks {
  let counters = repository.counters();
  Walks {
    walked: counters.walked,
    reached_met: counters.reached_met,
  }
}

/// The versions each green has.
fn versions(repository: &Repository) -> Vec<usize> {
  (0..GREENS)
    .map(|green| repository.versions(green))
    .collect()
}

/// The owner's keep as it commits: each green that moved, its newest version's value as the store's admission computed
/// it, kept under (type, version) and new there.
fn keep_new(real: &mut Real, kept: &mut Derivations, ty: &ReachSets, before: &[usize]) {
  for green in 0..GREENS {
    if real.repository.versions(green) == before[usize::try_from(green).unwrap()] {
      continue;
    }
    let (version, root) = real.repository.reach_value(&mut real.store, green).unwrap();
    let answer = kept.keep(
      &mut real.store,
      ty,
      &real.repository,
      (green, version),
      root,
    );
    assert_eq!(answer, Ok(Inserted::New));
    real.store.release(&root).unwrap();
  }
}

/// Every version's value derived through `derived`: the store's own D(k) and reached set at that version (A-647 part 3:
/// every version serves both from its value), and the very value the store kept.
fn check(real: &mut Real, derived: &mut Derivations, kept: &Derivations, ty: &mut ReachSets) {
  for green in 0..GREENS {
    let count = real.repository.versions(green);
    for at in 0..count {
      let version = (green, at as u64);
      let root = read_to_end(derived, ty, &real.repository, &mut real.store, version).unwrap();
      let sets = Sets::read(&real.store, &root).unwrap();
      assert_eq!(
        Ok(sets.brought),
        real.repository.brought(&real.store, green, at)
      );
      assert_eq!(
        Ok(sets.reached),
        real.repository.reached(&real.store, green, at)
      );
      let kept = kept.value(&real.store, ty, &real.repository, version);
      assert_eq!(kept, Ok(Some(root)), "the store's value is the derived one");
    }
  }
}

/// Each green's newest value derived from nothing, a fill walking up to the history's roots, is the one derived a
/// version at a time.
fn check_from_nothing(real: &mut Real, derived: &Derivations, ty: &mut ReachSets) {
  let mut fresh = Derivations::new();
  for green in 0..GREENS {
    let Some(newest) = real.repository.versions(green).checked_sub(1) else {
      continue;
    };
    let version = (green, newest as u64);
    let root = read_to_end(&mut fresh, ty, &real.repository, &mut real.store, version);
    let known = derived.value(&real.store, ty, &real.repository, version);
    assert_eq!(known, Ok(Some(root.unwrap())));
  }
  fresh.release(&mut real.store).unwrap();
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(64), ..ProptestConfig::default() }))]

  /// A-647 part 2 (A-645 "Derived data, one mechanism"; A-614's D(k)). Do: run A-614's own generated histories (commits
  /// on a ref's head, on any earlier commit and as roots, refs moved to other refs' heads, deletes, refused pushes, in
  /// both object formats); after each step keep the moved green's newest value from the store's own sets, and read every
  /// version's value through the type. Expect: every derived value is the store's D(k), and at the newest version its
  /// reached set, and is the value the store kept, identity for identity; each step derives exactly the versions it made
  /// (the non-vacuity counter: every earlier value is reused, never derived again), and a version's derivation walks
  /// exactly what its admission walked, so it stops where the admission stopped (at its parent's set, and at the set it
  /// was remade from); a fill from nothing reaches the same newest values; and once all is released the store is empty.
  #[test]
  fn t_a647_every_versions_d_k_is_its_derived_value(sha256 in any::<bool>(), ops in proptest::collection::vec(op(), 1..24)) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let mut real = Real { store: store(), repository: Repository::new(format), clock: Ticks(0) };
    let mut model = Model::new(format);
    let mut ty = ReachSets::declare(Mode::Eager);
    let (mut kept, mut derived) = (Derivations::new(), Derivations::new());
    for op in &ops {
      let before = versions(&real.repository);
      let admitting = walks(&real.repository);
      apply(&mut real, &mut model, op);
      let admitted = walks(&real.repository).since(admitting);
      keep_new(&mut real, &mut kept, &ty, &before);
      let (counted, deriving) = (derived.counters().derived, walks(&real.repository));
      check(&mut real, &mut derived, &kept, &mut ty);
      let made: usize = versions(&real.repository).iter().sum::<usize>() - before.iter().sum::<usize>();
      prop_assert_eq!(derived.counters().derived - counted, made as u64);
      if made == 1 {
        prop_assert_eq!(walks(&real.repository).since(deriving), admitted);
      }
    }
    check_from_nothing(&mut real, &derived, &mut ty);
    let Real { mut store, repository, .. } = real;
    kept.release(&mut store).unwrap();
    derived.release(&mut store).unwrap();
    repository.release(&mut store).unwrap();
    prop_assert!(store.is_empty());
  }
}

/// A-614's rewrite scenario: ref 1 at c1, c2 and c3, ref 2 made at c1, then ref 1 rewritten to c2b over c1.
fn rewritten() -> (ContentStore, Repository) {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let [first, second, third, rewrite] = line_and_branch(format);
  let heads: Vec<ObjectId> = [&first, &second, &third, &rewrite]
    .iter()
    .map(|objects| objects[0].id)
    .collect();
  for (objects, head) in [first, second, third].into_iter().zip(&heads) {
    push_and_move(&mut repository, &mut store, objects, &[(1, Some(*head))]).unwrap();
  }
  push_and_move(
    &mut repository,
    &mut store,
    Vec::new(),
    &[(2, Some(heads[0]))],
  )
  .unwrap();
  push_and_move(&mut repository, &mut store, rewrite, &[(1, Some(heads[3]))]).unwrap();
  (store, repository)
}

/// A-647 part 2. Do: A-614's rewrite scenario, then a lazy read of the rewrite's value. Expect: the history names each
/// fast-forward's parent the version before it, no parent for a create with no base, and for the rewrite ref 1's
/// version 2 and ref 2's version 0, the version its set was remade from; the read derives all five versions from
/// nothing; and the value is the store's own sets.
#[test]
fn t_a647_a_rewrite_is_derived_from_its_previous_version_and_its_sets_base() {
  let (mut store, repository) = rewritten();
  let parents = |version| Graph::parents(&repository, version);
  assert_eq!(parents((1, 0)), Some(Vec::new()));
  assert_eq!(parents((1, 2)), Some(vec![(1, 1)]));
  assert_eq!(parents((2, 0)), Some(Vec::new()));
  assert_eq!(parents((1, 3)), Some(vec![(1, 2), (2, 0)]));
  assert_eq!(parents((1, 4)), None);
  let mut ty = ReachSets::declare(Mode::Lazy);
  let mut values = Derivations::new();
  let root = read_to_end(&mut values, &mut ty, &repository, &mut store, (1, 3)).unwrap();
  assert_eq!(values.counters().derived, 5);
  let sets = Sets::read(&store, &root).unwrap();
  let store_sets = (
    repository.brought(&store, 1, 3),
    repository.reached(&store, 1, 3),
  );
  assert_eq!((Ok(sets.brought), Ok(sets.reached)), store_sets);
  values.release(&mut store).unwrap();
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// A-647 part 2: the type's refusals. Do: derive a version the repository lacks, one with too few and with too many
/// parents' values, and one whose parent's value is not the type's. Expect: `NoVersion`, `Parents` twice and
/// `Malformed`, each naming what it refused.
#[test]
fn t_a647_the_reach_type_refuses_by_kind() {
  let (mut store, repository) = rewritten();
  let mut ty = ReachSets::declare(Mode::Lazy);
  let foreign = store.put_derived(&[], b"not two sets").unwrap().identity;
  let mut values = Derivations::new();
  let first = read_to_end(&mut values, &mut ty, &repository, &mut store, (1, 0)).unwrap();
  let mut derive =
    |version, parents: &[ContentId]| ty.derive(&repository, &mut store, version, parents);
  assert_eq!(
    derive((7, 0), &[]),
    Err(ReachRefused::NoVersion {
      green: 7,
      version: 0
    })
  );
  assert_eq!(
    derive((1, 1), &[]),
    Err(ReachRefused::Parents {
      green: 1,
      version: 1
    })
  );
  assert_eq!(
    derive((1, 1), &[first, first]),
    Err(ReachRefused::Parents {
      green: 1,
      version: 1
    })
  );
  assert_eq!(
    derive((1, 1), &[foreign]),
    Err(ReachRefused::Malformed { node: foreign })
  );
}

/// The little-endian bytes of `value`.
fn word(value: u64) -> Vec<u8> {
  value.to_le_bytes().to_vec()
}

/// A set's bytes: its count of runs, then each run's start and end.
fn set_bytes(runs: &[(u64, u64)]) -> Vec<u8> {
  let mut bytes = word(runs.len() as u64);
  for (start, end) in runs {
    bytes.extend(word(*start));
    bytes.extend(word(*end));
  }
  bytes
}

/// A value of the type's form holding `bytes` as its sets' object and saying its length is `len`, held once by the
/// caller.
fn value_of(store: &mut ContentStore, bytes: &[u8], len: u64) -> ContentId {
  let object = store.put_object(bytes).unwrap();
  let node = store.put_derived(&[object], &word(len)).unwrap().identity;
  store.release(&object).unwrap();
  node
}

/// A-647 parts 2 and 3, hostile values. Do: read values whose sets' object is not two sets kept one way (a count past
/// the bytes, runs out of order, touching or empty, a byte left over, a byte short), and nodes not of the value's form
/// (the sets' bytes in the node itself, as part 2 kept them; two objects named; a length past the object's; a length
/// cut short). Expect: each refused, never read past its end, and the sound value read back.
#[test]
fn t_a647_a_hostile_reach_value_is_refused() {
  let mut store = store();
  let good = [set_bytes(&[(0, 2), (4, 5)]), set_bytes(&[(4, 5)])].concat();
  let hostile = [
    [set_bytes(&[(0, 2)]), word(u64::MAX)].concat(),
    [set_bytes(&[(4, 5), (0, 2)]), set_bytes(&[])].concat(),
    [set_bytes(&[(0, 2), (2, 3)]), set_bytes(&[])].concat(),
    [set_bytes(&[(3, 3)]), set_bytes(&[])].concat(),
    [good.clone(), vec![0]].concat(),
    good[..good.len() - 1].to_vec(),
  ];
  for bytes in &hostile {
    let root = value_of(&mut store, bytes, bytes.len() as u64);
    assert_eq!(Sets::read(&store, &root), None, "{bytes:?}");
  }
  let len = good.len() as u64;
  let root = value_of(&mut store, &good, len);
  let sets = Sets::read(&store, &root).unwrap();
  assert_eq!((sets.reached.len(), sets.brought.len()), (3, 1));
  let object = store.put_object(&good).unwrap();
  let forms = [
    store.put_derived(&[], &good).unwrap().identity,
    store
      .put_derived(&[object, object], &word(len))
      .unwrap()
      .identity,
    store
      .put_derived(&[object], &word(len + 1))
      .unwrap()
      .identity,
    store
      .put_derived(&[object], &word(len)[..7])
      .unwrap()
      .identity,
  ];
  for (way, node) in forms.iter().enumerate() {
    assert_eq!(Sets::read(&store, node), None, "form {way}");
  }
}

/// The ids of every set `green`'s versions serve, version by version: each D(k) and each reached set.
fn served(
  repository: &Repository,
  store: &ContentStore,
  green: u64,
) -> Vec<(Vec<ObjectId>, Vec<ObjectId>)> {
  (0..repository.versions(green))
    .map(|version| {
      let brought = repository.brought(store, green, version).unwrap();
      let reached = repository.reached(store, green, version).unwrap();
      (
        ids(repository, store, &brought),
        ids(repository, store, &reached),
      )
    })
    .collect()
}

/// A-647 part 3. Do: make ref 1 at a commit, fast-forward it, rewrite it to a root of its own, delete it, and make it
/// again at its first commit. Expect: every version, not only the newest, serves its D(k) and its reached set (its
/// head's closure) from its own value: the first commit's objects twice over, the second's beside both commits', the
/// root's own both ways, nothing, then the first commit's again both ways.
#[test]
fn t_a647_p3_every_version_serves_its_own_sets() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let (first, second, c1, c2) = wide(format);
  let text = blob(format, b"a root of its own\n");
  let root = tree(format, &[(FileMode::File, b"own", text.id)]);
  let own = commit(format, root.id, &[], 50);
  let own_head = own.id;
  let own_ids = sorted(vec![own.id, root.id, text.id]);
  let first_ids = sorted(first.iter().map(|object| object.id).collect());
  let second_ids = sorted(second.iter().map(|object| object.id).collect());
  let both = sorted([first_ids.clone(), second_ids.clone()].concat());
  let moves: [(Vec<_>, Option<ObjectId>); 5] = [
    (first, Some(c1.id)),
    (second, Some(c2.id)),
    (vec![own, root, text], Some(own_head)),
    (Vec::new(), None),
    (Vec::new(), Some(c1.id)),
  ];
  for (objects, to) in moves {
    push_and_move(&mut repository, &mut store, objects, &[(1, to)]).unwrap();
  }
  let expected = vec![
    (first_ids.clone(), first_ids.clone()),
    (second_ids, both),
    (own_ids.clone(), own_ids),
    (Vec::new(), Vec::new()),
    (first_ids.clone(), first_ids),
  ];
  assert_eq!(served(&repository, &store, 1), expected);
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// Shape: the bytes one run of a set takes in a value, its start and its end.
const RUN_BYTES: usize = size_of::<(u64, u64)>();

/// A-647 part 3, a value past one window. Do: push commits to refs 1 and 2 by turns, each commit a blob and a tree of
/// its own over its ref's previous head, until ref 1's reached set is more runs than one window of the store holds (each
/// turn's objects take the positions after the other ref's, so each of a ref's commits is a run of its own). Expect:
/// every push commits, and every version of both refs serves its D(k), its commit's three objects in one run, and its
/// reached set, its head's closure, a run a commit.
#[test]
fn t_a647_p3_a_reached_set_past_one_window_is_kept_and_served() {
  let format = ObjectFormat::Sha1;
  let mut store = store();
  let mut repository = Repository::new(format);
  let commits = WINDOW / RUN_BYTES + 1;
  let mut heads: [Vec<ObjectId>; 2] = [Vec::new(), Vec::new()];
  for turn in 0..commits * 2 {
    let at = turn % 2;
    let text = blob(format, format!("turn {turn}\n").as_bytes());
    let root = tree(format, &[(FileMode::File, b"turn", text.id)]);
    let made = commit(format, root.id, &heads[at], turn as u64);
    heads[at] = vec![made.id];
    let moved = [(at as u64 + 1, Some(made.id))];
    push_and_move(&mut repository, &mut store, vec![made, root, text], &moved).unwrap();
  }
  for green in [1, 2] {
    for version in 0..commits {
      let brought = repository.brought(&store, green, version).unwrap();
      let reached = repository.reached(&store, green, version).unwrap();
      let count = version + 1;
      assert_eq!(
        (brought.len(), brought.runs(), reached.len(), reached.runs()),
        (3, 1, 3 * count as u64, count),
        "green {green} version {version}"
      );
    }
  }
  let newest = repository.reached(&store, 1, commits - 1).unwrap();
  assert!(
    newest.runs() * RUN_BYTES > WINDOW,
    "the runs alone pass one window"
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}

/// Shape: a window as short as the server's store-level tests hold content in (A-662), so a ref's reached set passes
/// many blocks and several levels of its tree within a few hundred pushes.
const SHORT_WINDOW: usize = 1 << 8;

/// Shape: the turns of each of the storage test's phases: enough that ref 1's newest reached set, kept as one sorted
/// sequence of runs, passes the bound every one of its versions is held to.
const TURNS: usize = 400;

/// A commit of its own over `parents`, a blob of `text` and a tree naming it beneath it: its objects, and its id.
fn commit_of(
  format: ObjectFormat,
  text: &str,
  parents: &[ObjectId],
  seconds: u64,
) -> (Vec<Made>, ObjectId) {
  let text = blob(format, text.as_bytes());
  let files = tree(format, &[(FileMode::File, b"file", text.id)]);
  let made = commit(format, files.id, parents, seconds);
  let id = made.id;
  (vec![made, files, text], id)
}

/// Pushes `objects` and moves `green` to `head`: what the commit's values added to the store.
fn push_counted(
  repository: &mut Repository,
  store: &mut ContentStore,
  objects: Vec<Made>,
  (green, head): (u64, ObjectId),
) -> u64 {
  let before = repository.counters().value_bytes;
  push_and_move(repository, store, objects, &[(green, Some(head))]).unwrap();
  repository.counters().value_bytes - before
}

/// Root commits made on refs 2 and 3 by turns, so each of ref 2's is a run of its own: ref 2's, oldest first.
fn roots_by_turns(
  repository: &mut Repository,
  store: &mut ContentStore,
  format: ObjectFormat,
) -> Vec<ObjectId> {
  let mut roots = Vec::with_capacity(TURNS);
  for at in 0..TURNS {
    for green in [2, 3] {
      let (objects, id) = commit_of(format, &format!("ref {green} root {at}\n"), &[], at as u64);
      push_and_move(repository, store, objects, &[(green, Some(id))]).unwrap();
      if green == 2 {
        roots.push(id);
      }
    }
  }
  roots
}

/// Ref 1 made at a root of its own, pushed by turns with ref 3 (a commit over its head; a root of ref 3's), then merged
/// with each of `roots`, newest first: what each of its versions added to the store for its value, in order.
fn ref_one_by_turns_then_merges(
  repository: &mut Repository,
  store: &mut ContentStore,
  format: ObjectFormat,
  roots: &[ObjectId],
) -> Vec<u64> {
  let (objects, mut head) = commit_of(format, "ref 1's own root\n", &[], 0);
  let mut added = vec![push_counted(repository, store, objects, (1, head))];
  for at in 0..TURNS {
    let (objects, made) = commit_of(format, &format!("ref 1 turn {at}\n"), &[head], at as u64);
    head = made;
    added.push(push_counted(repository, store, objects, (1, head)));
    let (objects, root) = commit_of(format, &format!("ref 3 turn {at}\n"), &[], at as u64);
    push_and_move(repository, store, objects, &[(3, Some(root))]).unwrap();
  }
  for (at, root) in roots.iter().enumerate().rev() {
    let text = format!("merge {at}\n");
    let (objects, made) = commit_of(format, &text, &[head, *root], at as u64);
    head = made;
    added.push(push_counted(repository, store, objects, (1, head)));
  }
  added
}

/// The blocks of `shape` the positions of `set` touch.
fn blocks_touched(shape: Shape, set: &Positions) -> u64 {
  let blocks: BTreeSet<u64> = set.iter().map(|position| position / shape.block).collect();
  blocks.len() as u64
}

/// The nodes on a path from a set's tree's root down to a leaf: the root's level, the least whose range covers the
/// set's last block, and one for the leaf; none for the empty set.
fn path_nodes(shape: Shape, set: &Positions) -> u64 {
  set.run_pairs().last().map_or(0, |&(_, end)| {
    u64::from(shape.level_of((end - 1) / shape.block)) + 1
  })
}

/// What a version may add for its value: a window for each node on the paths from its two trees' roots down to the
/// blocks its D(k) touches, and one for its value's own node. Its reached set is the one before it grown by D(k), so
/// that set's tree differs from the one before only on those paths; D(k)'s tree is those paths.
fn bound(shape: Shape, sets: &Sets) -> u64 {
  let paths = blocks_touched(shape, &sets.brought);
  let nodes = paths * (path_nodes(shape, &sets.reached) + path_nodes(shape, &sets.brought));
  (nodes + 1) * SHORT_WINDOW as u64
}

/// Ref 1's `versions` read back from their values, each held to what its push brought: D(k) new to the version
/// before's reached set, a commit's three objects (a merge's six: the merged root's too), and its reached set the one
/// before grown by D(k). Each version's bound, in order.
fn bounds_of(
  repository: &Repository,
  store: &ContentStore,
  shape: Shape,
  versions: usize,
) -> Vec<u64> {
  let mut before = Positions::new();
  let mut bounds = Vec::with_capacity(versions);
  for version in 0..versions {
    let sets = Sets {
      reached: repository.reached(store, 1, version).unwrap(),
      brought: repository.brought(store, 1, version).unwrap(),
    };
    let objects = if version > TURNS { 6 } else { 3 };
    assert_eq!(sets.brought.len(), objects, "version {version}'s D(k)");
    assert!(
      sets.brought.intersection(&before).is_empty(),
      "version {version} brought no object anew"
    );
    assert_eq!(
      sets.reached,
      before.union(&sets.brought),
      "version {version}'s reached set"
    );
    bounds.push(bound(shape, &sets));
    before = sets.reached;
  }
  bounds
}

/// A-647 part 4. Do: in a store of a 256-byte window, make 400 root commits on ref 2 and as many on ref 3 by turns, so
/// each of ref 2's roots is a run of its own; make ref 1 at a root of its own and push it by turns with ref 3, a commit
/// over its head each turn, so each of those versions brings a run past every run it reached; then merge ref 2's roots
/// into ref 1, newest first, each merge a commit, a tree and a blob of its own, so each of those versions brings a run
/// below every run ref 1 reached (the worst case for a set kept as one sorted sequence, which such a version writes
/// again from that run on). Record what each of ref 1's versions added to the store for its value. Expect: every
/// version adds at most a window for each node on the paths from its two trees' roots down to the blocks its D(k)
/// touches and one for its value's node, a bound set by what it brought and its trees' depth, never by what the ref
/// reaches; every version, not only the newest, still answers its D(k) (its push's objects) and its reached set (the
/// one before grown by them); and ref 1's newest reached set, kept as one sorted sequence, passes every version's bound.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the test's evidence, printed with every run"
)]
fn t_a647_p4_a_version_stores_what_it_adds_not_what_it_reaches() {
  let format = ObjectFormat::Sha1;
  let mut store = ContentStore::new(BUDGET, SHORT_WINDOW).unwrap();
  let mut repository = Repository::new(format);
  let roots = roots_by_turns(&mut repository, &mut store, format);
  let added = ref_one_by_turns_then_merges(&mut repository, &mut store, format, &roots);
  let shape = Shape::of(SHORT_WINDOW).unwrap();
  let bounds = bounds_of(&repository, &store, shape, added.len());
  let newest = repository.reached(&store, 1, added.len() - 1).unwrap();
  let whole = (newest.runs() * RUN_BYTES) as u64;
  let past: Vec<(usize, u64, u64)> = (0..added.len())
    .filter(|&version| added[version] > bounds[version])
    .map(|version| (version, added[version], bounds[version]))
    .collect();
  let (turns, merges) = added.split_at(TURNS + 1);
  eprintln!(
    "reach sets census: shape {shape:?}, versions {}, most added by a turn {} and by a merge {} bytes, most bound {}, \
     added in all {}, newest set {} runs ({whole} bytes whole), versions past their bound {}",
    added.len(),
    turns.iter().max().unwrap(),
    merges.iter().max().unwrap(),
    bounds.iter().max().unwrap(),
    added.iter().sum::<u64>(),
    newest.runs(),
    past.len()
  );
  assert_eq!(past.first(), None, "(version, added, bound)");
  assert_eq!(
    newest.runs(),
    2 * TURNS + 1,
    "each root a run, each turn's commit one, and the merges one"
  );
  assert!(
    whole > *bounds.iter().max().unwrap(),
    "kept whole, the newest set passes every version's bound"
  );
  repository.release(&mut store).unwrap();
  assert!(store.is_empty());
}
