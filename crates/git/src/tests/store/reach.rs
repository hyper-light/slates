//! A-614's D(k) and reached sets as a type of derived data (A-647 part 2): every version's value derived through
//! `slates-vcs`'s mechanism from its parents' values is the store's own D(k) and reached set, and is, identity for
//! identity, the value the store kept from its admission as it committed.

use proptest::prelude::*;
use slates_vcs::derived::{Derivations, DerivedRefused, DerivedType, Graph, Mode, Progress, Read};
use slates_vcs::{ContentId, ContentStore, Inserted};

use crate::object_name::{ObjectFormat, ObjectId};
use crate::store::{ReachRefused, ReachSets, Repository, Sets};

use super::oracle::{GREENS, Model, Real, apply, op};
use super::{Ticks, line_and_branch, push_and_move, store};

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

/// Every version's value derived through `derived`: the store's own D(k), its reached set at the newest version, and
/// the very value the store kept.
fn check(real: &mut Real, derived: &mut Derivations, kept: &Derivations, ty: &mut ReachSets) {
  for green in 0..GREENS {
    let count = real.repository.versions(green);
    for at in 0..count {
      let version = (green, at as u64);
      let root = read_to_end(derived, ty, &real.repository, &mut real.store, version).unwrap();
      let sets = Sets::read(&real.store, &root).unwrap();
      assert_eq!(Some(&sets.brought), real.repository.brought(green, at));
      if at + 1 == count {
        assert_eq!(Some(&sets.reached), real.repository.reached(green));
      }
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
  /// (the non-vacuity counter: every earlier value is reused, never derived again); a fill from nothing reaches the same
  /// newest values; and once all is released the store is empty.
  #[test]
  fn t_a647_every_versions_d_k_is_its_derived_value(sha256 in any::<bool>(), ops in proptest::collection::vec(op(), 1..24)) {
    let format = if sha256 { ObjectFormat::Sha256 } else { ObjectFormat::Sha1 };
    let mut real = Real { store: store(), repository: Repository::new(format), clock: Ticks(0) };
    let mut model = Model::new(format);
    let mut ty = ReachSets::declare(Mode::Eager);
    let (mut kept, mut derived) = (Derivations::new(), Derivations::new());
    for op in &ops {
      let before = versions(&real.repository);
      apply(&mut real, &mut model, op);
      keep_new(&mut real, &mut kept, &ty, &before);
      let counted = derived.counters().derived;
      check(&mut real, &mut derived, &kept, &mut ty);
      let made: usize = versions(&real.repository).iter().sum::<usize>() - before.iter().sum::<usize>();
      prop_assert_eq!(derived.counters().derived - counted, made as u64);
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
  let store_sets = (repository.brought(1, 3), repository.reached(1));
  assert_eq!((Some(&sets.brought), Some(&sets.reached)), store_sets);
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

/// A-647 part 2, hostile values: bytes that are not two sets kept one way are refused, never read past their end.
#[test]
fn t_a647_a_hostile_reach_value_is_refused() {
  let mut store = store();
  let word = |value: u64| value.to_le_bytes().to_vec();
  let set = |runs: &[(u64, u64)]| {
    let mut bytes = word(runs.len() as u64);
    for (start, end) in runs {
      bytes.extend(word(*start));
      bytes.extend(word(*end));
    }
    bytes
  };
  let good = [set(&[(0, 2), (4, 5)]), set(&[(4, 5)])].concat();
  let hostile = [
    [set(&[(0, 2)]), word(u64::MAX)].concat(),
    [set(&[(4, 5), (0, 2)]), set(&[])].concat(),
    [set(&[(0, 2), (2, 3)]), set(&[])].concat(),
    [set(&[(3, 3)]), set(&[])].concat(),
    [good.clone(), vec![0]].concat(),
    good[..good.len() - 1].to_vec(),
  ];
  for bytes in &hostile {
    let root = store.put_derived(&[], bytes).unwrap().identity;
    assert_eq!(Sets::read(&store, &root), None, "{bytes:?}");
  }
  let root = store.put_derived(&[], &good).unwrap().identity;
  let sets = Sets::read(&store, &root).unwrap();
  assert_eq!((sets.reached.len(), sets.brought.len()), (3, 1));
  let named = store.put_derived(&[root], &good).unwrap().identity;
  assert_eq!(Sets::read(&store, &named), None, "a value names no content");
}
