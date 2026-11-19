//! A-666 held to its definition (module docs): every answer equals a naive walk over generated histories; every value's
//! fields equal their definitions (Myers' digits, the immediate dominator, the run bottom); every question's visits are
//! held to its stated bound; a value is the same however its history was filled; hostile values are refused, typed;
//! and a long linear history is answered in a few hundred visits.

use std::cell::RefCell;
use std::cmp::Reverse;

use proptest::prelude::*;
use proptest::test_runner::TestRunner;

use super::{
  Ancestry, AncestryRefused, FIRST_PARENTS, Found, QueryCost, Walk, ancestor_at, bisect, decode,
  is_ancestor, merge_bases, read_node, skew_jump_depth,
};
use crate::content::{ContentId, ContentRefused, ContentStore};
use crate::derived::{Derivations, DerivedType, Fill, Graph, Mode, Progress, Read};

/// Shape: the store's budget in these tests: room for every history and question they make.
const BUDGET: u64 = 1 << 30;

/// Shape: the store's window: past any value these histories make (an octopus of three parents is 249 bytes).
const WINDOW: usize = 1024;

/// Shape: the most versions a generated history holds.
const MAX_VERSIONS: usize = 2000;

/// Shape: the longest run a generated history extends a branch by: long against L = 30 at 2,000 versions, so a
/// question that walked a run would pass its bound (two entries in one run: 428 visits).
const LONG_RUN: usize = 800;

/// Shape: the most branches a generated history keeps open.
const MAX_TIPS: usize = 12;

/// Shape: the skew-binary digits the test's own table of Myers' J writes, enough for any depth here.
const DIGITS: usize = 24;

/// Shape: the long linear history's versions, 2^17: L = 3⌊lg(2^17 + 1)⌋ = 51.
const LONG_HISTORY: u64 = 1 << 17;

/// A history: each version's parents, versions numbered in the order they were made.
#[derive(Clone, Debug, Default)]
struct History {
  parents: Vec<Vec<u64>>,
}

impl Graph for History {
  type Version = u64;

  fn parents(&self, version: u64) -> Option<Vec<u64>> {
    self.parents.get(usize::try_from(version).ok()?).cloned()
  }

  fn name(&self, version: u64, out: &mut Vec<u8>) {
    out.extend_from_slice(&version.to_be_bytes());
  }
}

impl History {
  fn push(&mut self, parents: Vec<u64>) -> u64 {
    self.parents.push(parents);
    number(self.parents.len() - 1)
  }

  fn len(&self) -> usize {
    self.parents.len()
  }
}

/// A version's index.
fn index(version: u64) -> usize {
  usize::try_from(version).unwrap()
}

/// A version's number.
fn number(index: usize) -> u64 {
  u64::try_from(index).unwrap()
}

/// The version a found value names.
fn version_of(found: &Found) -> usize {
  index(u64::from_be_bytes(
    found.name.as_slice().try_into().unwrap(),
  ))
}

/// The versions found values name.
fn versions_of(found: &[Found]) -> Vec<usize> {
  found.iter().map(version_of).collect()
}

/// ⌊lg x⌋ for x ≥ 1.
fn floor_lg(x: u64) -> u64 {
  u64::from(x.ilog2())
}

/// ⌈lg x⌉ for x ≥ 1, zero for zero.
fn ceil_lg(x: u64) -> u64 {
  if x <= 1 {
    0
  } else {
    u64::from(64 - (x - 1).leading_zeros())
  }
}

/// A generated history's step: a selector, two picks and a length (see [`Grower::apply`]).
type Op = (u8, usize, usize, usize);

/// A step, as plain tuples (no `Arc`-backed `prop_oneof!`, R2).
fn op() -> impl Strategy<Value = Op> {
  (0_u8..20, any::<usize>(), any::<usize>(), 1_usize..=LONG_RUN)
}

/// Grows a history from generated steps: runs long and short on open branches, merges of two and of three, branches
/// opened at any version, criss-cross merges, unrelated roots, and merges of a branch with an old version.
struct Grower {
  history: History,
  tips: Vec<u64>,
}

impl Grower {
  fn new() -> Grower {
    let mut history = History::default();
    let root = history.push(Vec::new());
    Grower {
      history,
      tips: vec![root],
    }
  }

  fn room(&self) -> usize {
    MAX_VERSIONS.saturating_sub(self.history.len())
  }

  fn extend(&mut self, pick: usize, length: usize) {
    let at = pick % self.tips.len();
    let mut tip = self.tips[at];
    for _ in 0..length.min(self.room()) {
      tip = self.history.push(vec![tip]);
    }
    self.tips[at] = tip;
  }

  /// Two distinct open branches, when there are two and room for two versions.
  fn two(&self, one: usize, two: usize) -> Option<(usize, usize)> {
    let count = self.tips.len();
    let first = one % count;
    let second = (first + 1 + two % count.max(2).saturating_sub(1)) % count;
    (count >= 2 && first != second && self.room() >= 2).then_some((first, second))
  }

  fn merge(&mut self, one: usize, two: usize, close: bool) {
    if let Some((first, second)) = self.two(one, two) {
      let merge = self.history.push(vec![self.tips[first], self.tips[second]]);
      self.tips[first] = merge;
      if close {
        self.tips.remove(second);
      }
    }
  }

  fn octopus(&mut self, one: usize) {
    if self.tips.len() >= 3 && self.room() >= 1 {
      let first = one % self.tips.len();
      let parents: Vec<u64> = (0..3)
        .map(|at| self.tips[(first + at) % self.tips.len()])
        .collect();
      if parents[0] != parents[1] && parents[1] != parents[2] && parents[0] != parents[2] {
        self.tips[first] = self.history.push(parents);
      }
    }
  }

  fn fork(&mut self, pick: usize) {
    if self.tips.len() < MAX_TIPS {
      self.tips.push(number(pick % self.history.len()));
    }
  }

  fn criss_cross(&mut self, one: usize, two: usize) {
    if let Some((first, second)) = self.two(one, two) {
      let (a, b) = (self.tips[first], self.tips[second]);
      self.tips[first] = self.history.push(vec![a, b]);
      self.tips[second] = self.history.push(vec![b, a]);
    }
  }

  fn root(&mut self) {
    if self.tips.len() < MAX_TIPS && self.room() >= 1 {
      let root = self.history.push(Vec::new());
      self.tips.push(root);
    }
  }

  fn merge_old(&mut self, one: usize, two: usize) {
    let first = one % self.tips.len();
    let old = number(two % self.history.len());
    if old != self.tips[first] && self.room() >= 1 {
      self.tips[first] = self.history.push(vec![self.tips[first], old]);
    }
  }

  fn apply(&mut self, (kind, one, two, length): Op) {
    match kind {
      0..=2 => self.extend(one, length),
      3..=7 => self.extend(one, length % 8 + 1),
      8..=10 => self.merge(one, two, kind == 10),
      11 => self.octopus(one),
      12..=14 => self.fork(one),
      15 => self.criss_cross(one, two),
      16 => self.root(),
      _ => self.merge_old(one, two),
    }
  }
}

/// The history the steps grow, and its open branches' tips.
fn grown(ops: &[Op]) -> (History, Vec<u64>) {
  let mut grower = Grower::new();
  for op in ops {
    grower.apply(*op);
  }
  (grower.history, grower.tips)
}

/// A bitset over a history's versions.
type Bits = Vec<u64>;

fn has(bits: &Bits, at: usize) -> bool {
  bits[at / 64] >> (at % 64) & 1 == 1
}

fn set(bits: &mut Bits, at: usize) {
  bits[at / 64] |= 1 << (at % 64);
}

fn members(bits: &Bits) -> Vec<usize> {
  (0..bits.len() * 64).filter(|at| has(bits, *at)).collect()
}

fn count(bits: &Bits) -> u64 {
  bits.iter().map(|word| u64::from(word.count_ones())).sum()
}

/// The union of two bitsets.
fn union(one: &Bits, two: &Bits) -> Bits {
  one.iter().zip(two).map(|(a, b)| a | b).collect()
}

/// The naive model, by definition from the history alone: each version's generation, first-parent depth, run bottom,
/// ancestors (itself included), dominators (itself included), immediate dominator and skip-tree depth.
#[derive(Default)]
struct Model {
  parents: Vec<Vec<usize>>,
  generation: Vec<u64>,
  depth: Vec<u64>,
  bottom: Vec<usize>,
  ancestors: Vec<Bits>,
  dominators: Vec<Bits>,
  idom: Vec<Option<usize>>,
  skip_depth: Vec<u64>,
}

impl Model {
  fn of(history: &History) -> Model {
    let words = history.len().div_ceil(64);
    let mut model = Model::default();
    for (at, parents) in history.parents.iter().enumerate() {
      let parents: Vec<usize> = parents.iter().map(|parent| index(*parent)).collect();
      model.record(at, parents, words);
    }
    for at in 0..model.len() {
      let idom = members(&model.dominators[at])
        .into_iter()
        .filter(|dominator| *dominator != at)
        .max_by_key(|dominator| model.generation[*dominator]);
      model.idom.push(idom);
      model.skip_depth.push(count(&model.dominators[at]) - 1);
    }
    model
  }

  fn record(&mut self, at: usize, parents: Vec<usize>, words: usize) {
    let generation = 1
      + parents
        .iter()
        .map(|parent| self.generation[*parent])
        .max()
        .unwrap_or(0);
    let depth = parents.first().map_or(0, |first| self.depth[*first] + 1);
    let bottom = match parents.as_slice() {
      [only] if self.parents[*only].len() == 1 => self.bottom[*only],
      [only] => *only,
      _ => at,
    };
    let mut ancestors = vec![0; words];
    let mut dominators = parents
      .first()
      .map_or(vec![0; words], |first| self.dominators[*first].clone());
    for parent in &parents {
      ancestors = union(&ancestors, &self.ancestors[*parent]);
      for (word, more) in dominators.iter_mut().zip(&self.dominators[*parent]) {
        *word &= more;
      }
    }
    set(&mut ancestors, at);
    set(&mut dominators, at);
    self.parents.push(parents);
    self.generation.push(generation);
    self.depth.push(depth);
    self.bottom.push(bottom);
    self.ancestors.push(ancestors);
    self.dominators.push(dominators);
  }

  fn len(&self) -> usize {
    self.parents.len()
  }

  /// The history's L: 3⌊lg(n + 1)⌋ for its greatest generation n, past every depth.
  fn l(&self) -> u64 {
    3 * floor_lg(self.generation.iter().max().unwrap() + 1)
  }

  /// The first-parent ancestor of `at` at `depth`, walked.
  fn first_parent_at(&self, mut at: usize, depth: u64) -> usize {
    while self.depth[at] > depth {
      at = self.parents[at][0];
    }
    at
  }

  /// The skip-tree ancestor of `at` at skip depth `depth`, walked along immediate dominators.
  fn dominator_at(&self, mut at: usize, depth: u64) -> usize {
    while self.skip_depth[at] > depth {
      at = self.idom[at].unwrap();
    }
    at
  }

  fn is_ancestor(&self, ancestor: usize, descendant: usize) -> bool {
    has(&self.ancestors[descendant], ancestor)
  }

  /// The parents of the merges `among` holds whose generation is above `above`: what a question may take in.
  fn merge_parents(&self, among: &Bits, above: u64) -> u64 {
    members(among)
      .into_iter()
      .filter(|at| self.parents[*at].len() > 1 && self.generation[*at] > above)
      .map(|at| number(self.parents[at].len()))
      .sum()
  }

  /// The versions `among` holds at or above generation `floor`: what a walk by generation visits at least.
  fn walked(&self, among: &Bits, floor: u64) -> u64 {
    number(
      members(among)
        .into_iter()
        .filter(|at| self.generation[*at] >= floor)
        .count(),
    )
  }

  /// The merge bases by definition: the common ancestors that are no proper ancestor of another, the greatest
  /// generation first, then by version.
  fn bases(&self, one: usize, two: usize) -> Vec<usize> {
    let common: Bits = self.ancestors[one]
      .iter()
      .zip(&self.ancestors[two])
      .map(|(a, b)| a & b)
      .collect();
    let mut below = vec![0; common.len()];
    for at in members(&common) {
      for parent in &self.parents[at] {
        below = union(&below, &self.ancestors[*parent]);
      }
    }
    let mut bases: Vec<usize> = members(&common)
      .into_iter()
      .filter(|at| !has(&below, *at))
      .collect();
    bases.sort_by_key(|at| (Reverse(self.generation[*at]), *at));
    bases
  }

  /// Bisect by definition: the candidates, each one's weight the candidates it reaches, the best by
  /// `min(weight, candidates - weight)`, then the greater generation, then the lesser version.
  fn bisect(&self, bad: usize, goods: &[usize]) -> Option<(usize, u64, u64)> {
    let mut candidates = self.ancestors[bad].clone();
    for good in goods {
      for (word, more) in candidates.iter_mut().zip(&self.ancestors[*good]) {
        *word &= !more;
      }
    }
    let total = count(&candidates);
    members(&candidates)
      .into_iter()
      .map(|at| {
        let reached: Bits = self.ancestors[at]
          .iter()
          .zip(&candidates)
          .map(|(a, c)| a & c)
          .collect();
        let weight = count(&reached);
        (
          (weight.min(total - weight), self.generation[at], Reverse(at)),
          (at, weight, total),
        )
      })
      .max_by_key(|(rank, _)| *rank)
      .map(|(_, answer)| answer)
  }
}

/// Myers' J by his definition, as a table: each depth's canonical skew-binary numeral made by incrementing the one
/// before (a 2 as the lowest non-zero digit becomes 0 and carries one up; else the lowest digit grows), its lowest
/// non-zero digit less one, read back as a number, position i worth 2^(i + 1) - 1.
fn digit_jumps(up_to: u64) -> Vec<u64> {
  let worth = |digits: &[u64]| -> u64 {
    digits
      .iter()
      .enumerate()
      .map(|(at, digit)| digit * ((2 << at) - 1))
      .sum()
  };
  let mut digits = vec![0_u64; DIGITS];
  let mut jumps = vec![0];
  for _ in 1..=up_to {
    match digits.iter().position(|digit| *digit != 0) {
      Some(at) if digits[at] == 2 => {
        digits[at] = 0;
        digits[at + 1] += 1;
      }
      _ => digits[0] += 1,
    }
    let lowest = digits.iter().position(|digit| *digit != 0).unwrap();
    let mut jumped = digits.clone();
    jumped[lowest] -= 1;
    jumps.push(worth(&jumped));
  }
  jumps
}

/// One store, one history's values and a declaration of the type: eager, every version filled as recorded, or lazy,
/// read in a given order.
struct Fixture {
  store: ContentStore,
  derivations: Derivations,
  ty: Ancestry<History>,
  roots: Vec<ContentId>,
}

/// Drives a fill to its end.
fn drive(
  fill: &mut Fill<u64>,
  derivations: &mut Derivations,
  ty: &mut Ancestry<History>,
  history: &History,
  store: &mut ContentStore,
) -> ContentId {
  loop {
    match fill.step(derivations, ty, history, store).unwrap() {
      Progress::More => {}
      Progress::Done(root) => return root,
    }
  }
}

impl Fixture {
  fn empty(mode: Mode) -> Fixture {
    Fixture {
      store: ContentStore::new(BUDGET, WINDOW).unwrap(),
      derivations: Derivations::new(),
      ty: Ancestry::declare(mode),
      roots: Vec::new(),
    }
  }

  /// Every version derived as it is recorded, in order: one walk and one derivation each.
  fn eager(history: &History) -> Fixture {
    let mut fixture = Fixture::empty(Mode::Eager);
    for version in 0..number(history.len()) {
      let recorded = fixture
        .derivations
        .recorded(&mut fixture.store, &fixture.ty, version);
      let mut fill = recorded.unwrap().unwrap();
      let root = drive(
        &mut fill,
        &mut fixture.derivations,
        &mut fixture.ty,
        history,
        &mut fixture.store,
      );
      fill.release(&mut fixture.store).unwrap();
      fixture.roots.push(root);
    }
    fixture
  }

  /// Every version read lazily, in `order`, each read filling what it must.
  fn lazy(history: &History, order: &[u64]) -> Fixture {
    let mut fixture = Fixture::empty(Mode::Lazy);
    let mut roots = vec![None; history.len()];
    for version in order {
      let read = fixture
        .derivations
        .read(&mut fixture.store, &fixture.ty, history, *version);
      let root = match read.unwrap() {
        Read::Ready(root) => root,
        Read::Fill(mut fill) => {
          let root = drive(
            &mut fill,
            &mut fixture.derivations,
            &mut fixture.ty,
            history,
            &mut fixture.store,
          );
          fill.release(&mut fixture.store).unwrap();
          root
        }
      };
      roots[index(*version)] = Some(root);
    }
    fixture.roots = roots.into_iter().map(Option::unwrap).collect();
    fixture
  }

  fn root(&self, at: usize) -> ContentId {
    self.roots[at]
  }

  /// Gives every value back: the store is then empty and charges nothing, so no question kept a charge.
  fn finish(self) {
    let Fixture {
      mut store,
      derivations,
      ..
    } = self;
    derivations.release(&mut store).unwrap();
    assert!(store.is_empty(), "the values freed whole");
    assert_eq!((store.used(), store.charged()), (0, 0));
  }
}

/// What the oracle met, summed over every case: proof that each path it holds to the model ran.
#[derive(Debug, Default)]
struct Census {
  versions: u64,
  merges: u64,
  octopus: u64,
  roots: u64,
  ancestors: u64,
  not_ancestors: u64,
  one_base: u64,
  several_bases: u64,
  no_base: u64,
  bisects: u64,
  no_candidates: u64,
  bisects_over_merges: u64,
  jumps: u64,
  fewer_opened: u64,
  walks_saved: u64,
  most_visits: u64,
}

impl Census {
  fn add(&mut self, other: &Census) {
    self.versions += other.versions;
    self.merges += other.merges;
    self.octopus += other.octopus;
    self.roots += other.roots;
    self.ancestors += other.ancestors;
    self.not_ancestors += other.not_ancestors;
    self.one_base += other.one_base;
    self.several_bases += other.several_bases;
    self.no_base += other.no_base;
    self.bisects += other.bisects;
    self.no_candidates += other.no_candidates;
    self.bisects_over_merges += other.bisects_over_merges;
    self.jumps += other.jumps;
    self.fewer_opened += other.fewer_opened;
    self.walks_saved += other.walks_saved;
    self.most_visits = self.most_visits.max(other.most_visits);
  }

  /// Each kind met at least once: merges of two and of three, unrelated roots, both answers to is-ancestor, one base,
  /// several (criss-cross) and none (unrelated histories), bisects with and without candidates and over merges,
  /// skip-tree jumps, lowerings that opened fewer merges than lay above the target, and questions a walk would have
  /// cost four times over.
  fn assert_met(&self) {
    let met = [
      self.merges,
      self.octopus,
      self.roots,
      self.ancestors,
      self.not_ancestors,
      self.one_base,
      self.several_bases,
      self.no_base,
      self.no_candidates,
      self.bisects_over_merges,
      self.jumps,
      self.fewer_opened,
      self.walks_saved,
    ];
    assert!(met.iter().all(|count| *count > 0), "{self:?}");
  }

  fn note_history(&mut self, model: &Model) {
    self.versions += number(model.len());
    for parents in &model.parents {
      self.merges += u64::from(parents.len() >= 2);
      self.octopus += u64::from(parents.len() >= 3);
      self.roots += u64::from(parents.is_empty());
    }
  }

  fn note_cost(&mut self, cost: &QueryCost, walked: u64) {
    self.jumps += cost.jumps;
    self.walks_saved += u64::from(walked >= 4 * cost.visits.max(1));
    self.most_visits = self.most_visits.max(cost.visits);
  }
}

/// The bound on an is-ancestor question's visits (module docs): 2 + (1 + P)(2L + 3).
fn ancestor_bound(cost: &QueryCost, l: u64) -> u64 {
  2 + (1 + cost.parents) * (2 * l + 3)
}

/// The bound on a paint's visits (module docs): 4 + T(2L + 4) + 3L·E(⌈lg E⌉ + 1) + 2P.
fn paint_bound(cost: &QueryCost, l: u64) -> u64 {
  let entries = cost.entries;
  4 + cost.runs * (2 * l + 4) + 3 * l * entries * (ceil_lg(entries) + 1) + 2 * cost.parents
}

/// The bound on a bisect's visits (module docs): the paint's, P more, and L more for each segment.
fn bisect_bound(cost: &QueryCost, l: u64) -> u64 {
  paint_bound(cost, l) + cost.parents + cost.segments * l
}

/// Asks whether `one` is an ancestor of `two`: the model's answer, within its bound, never opening a merge the model
/// does not put above the target.
fn ask_ancestor(
  fixture: &mut Fixture,
  model: &Model,
  (one, two): (usize, usize),
  census: &mut Census,
) {
  let (ancestor, descendant) = (fixture.root(one), fixture.root(two));
  let (answer, cost) = is_ancestor(&mut fixture.store, ancestor, descendant).unwrap();
  assert_eq!(
    answer,
    model.is_ancestor(one, two),
    "is {one} an ancestor of {two}"
  );
  let above = model.merge_parents(&model.ancestors[two], model.generation[one]);
  let within = (
    cost.parents <= above,
    cost.reads <= cost.visits,
    cost.visits <= ancestor_bound(&cost, model.l()),
  );
  assert_eq!(
    within,
    (true, true, true),
    "{one} in {two}: {cost:?}, {above} parents above"
  );
  census.ancestors += u64::from(answer);
  census.not_ancestors += u64::from(!answer);
  census.fewer_opened += u64::from(cost.parents < above);
  census.note_cost(
    &cost,
    model.walked(&model.ancestors[two], model.generation[one]),
  );
}

/// Asks for the merge bases of `one` and `two`: the model's, each with its root and generation, within the paint's
/// bound, its entries at most the two asked of and what merges and jumps brought.
fn ask_bases(
  fixture: &mut Fixture,
  model: &Model,
  (one, two): (usize, usize),
  census: &mut Census,
) {
  let (first, second) = (fixture.root(one), fixture.root(two));
  let (found, cost) = merge_bases(&mut fixture.store, first, second).unwrap();
  let versions = versions_of(&found);
  assert_eq!(
    versions,
    model.bases(one, two),
    "the merge bases of {one} and {two}"
  );
  let named: Vec<(ContentId, u64)> = found
    .iter()
    .map(|base| (base.root, base.generation))
    .collect();
  let expected: Vec<(ContentId, u64)> = versions
    .iter()
    .map(|at| (fixture.root(*at), model.generation[*at]))
    .collect();
  assert_eq!(named, expected);
  let reach = union(&model.ancestors[one], &model.ancestors[two]);
  let parents = model.merge_parents(&reach, 0);
  let seeded = cost.entries <= 2 + cost.parents + cost.jumps;
  let within = (
    cost.parents <= parents,
    seeded,
    cost.visits <= paint_bound(&cost, model.l()),
  );
  assert_eq!(
    within,
    (true, true, true),
    "bases of {one} and {two}: {cost:?}, {parents} parents"
  );
  census.one_base += u64::from(versions.len() == 1);
  census.several_bases += u64::from(versions.len() > 1);
  census.no_base += u64::from(versions.is_empty());
  let floor = versions
    .iter()
    .map(|at| model.generation[*at])
    .min()
    .unwrap_or(1);
  census.note_cost(&cost, model.walked(&reach, floor));
}

/// Asks bisect for its midpoint between `bad` and `goods`: the model's version, weight and count, within its bound.
fn ask_bisect(
  fixture: &mut Fixture,
  model: &Model,
  bad: usize,
  goods: &[usize],
  census: &mut Census,
) {
  let good_roots: Vec<ContentId> = goods.iter().map(|good| fixture.root(*good)).collect();
  let bad_root = fixture.root(bad);
  let (midpoint, cost) = bisect(&mut fixture.store, bad_root, &good_roots).unwrap();
  let answer = midpoint.map(|midpoint| {
    (
      version_of(&midpoint.found),
      midpoint.weight,
      midpoint.candidates,
    )
  });
  assert_eq!(
    answer,
    model.bisect(bad, goods),
    "bisect {bad} against {goods:?}"
  );
  let bound = bisect_bound(&cost, model.l());
  assert!(
    cost.visits <= bound,
    "bisect {bad} against {goods:?}: {cost:?} past {bound}"
  );
  census.bisects += 1;
  census.no_candidates += u64::from(answer.is_none());
  census.bisects_over_merges += u64::from(cost.segments >= 2);
  census.note_cost(&cost, answer.map_or(0, |(_, _, total)| total));
}

/// Every value's fields against their definitions: its generation, depth and skip-tree depth (its proper dominators'
/// count); its parents' values; its run bottom; its first-parent jump to Myers' depth on its first-parent chain; its
/// skip-tree parent the immediate dominator, and its skip jump to Myers' depth on its dominators' chain.
fn check_values(fixture: &Fixture, model: &Model) {
  let jumps = digit_jumps(number(model.len()));
  for at in 0..model.len() {
    let node = read_node(&fixture.store, fixture.root(at)).unwrap().made;
    let parents: Vec<ContentId> = model.parents[at]
      .iter()
      .map(|parent| fixture.root(*parent))
      .collect();
    let bottom = (model.parents[at].len() == 1).then(|| fixture.root(model.bottom[at]));
    let numbers = (model.generation[at], model.depth[at], model.skip_depth[at]);
    assert_eq!(
      (
        (node.generation, node.depth, node.skip_depth),
        &node.parents,
        node.bottom
      ),
      (numbers, &parents, bottom)
    );
    let jump =
      (node.depth > 0).then(|| fixture.root(model.first_parent_at(at, jumps[index(node.depth)])));
    let skip_jump = |idom| {
      (
        fixture.root(idom),
        fixture.root(model.dominator_at(at, jumps[index(model.skip_depth[at])])),
      )
    };
    assert_eq!(
      (node.jump, node.skip),
      (jump, model.idom[at].map(skip_jump)),
      "version {at}'s links"
    );
  }
}

/// A value is the same however its history was filled: read lazily, first in a stride the case picks, then every
/// version from the newest down, every version's value is the eager one, identity for identity.
fn same_however_filled(history: &History, eager: &Fixture, stride: usize) {
  let count = history.len();
  let step = stride % count;
  let mut order: Vec<u64> = (0..count).map(|at| number((at * step) % count)).collect();
  order.extend((0..number(count)).rev());
  let lazy = Fixture::lazy(history, &order);
  assert_eq!(
    lazy.roots, eager.roots,
    "the same value whatever the filling's order"
  );
  lazy.finish();
}

/// Asks every question of one pair: is-ancestor both ways, the merge bases, and a bisect of the second against the
/// first, or against nothing, or against the first and a third.
fn ask_all(
  fixture: &mut Fixture,
  model: &Model,
  (one, two, three): (usize, usize, usize),
  census: &mut Census,
) {
  ask_ancestor(fixture, model, (one, two), census);
  ask_ancestor(fixture, model, (two, one), census);
  ask_bases(fixture, model, (one, two), census);
  let goods = match three % 4 {
    0 => Vec::new(),
    1 => vec![one],
    _ => vec![one, three % model.len()],
  };
  ask_bisect(fixture, model, two, &goods, census);
}

/// One case of the oracle: a history grown from `ops`, every value checked, the lazy filling compared, and every
/// question asked of each generated pair and of each two neighbouring branch tips (where criss-cross merges and
/// unrelated histories leave their marks).
fn case(ops: &[Op], queries: &[(usize, usize, usize)]) -> Census {
  let (history, tips) = grown(ops);
  let model = Model::of(&history);
  let mut fixture = Fixture::eager(&history);
  let mut census = Census::default();
  census.note_history(&model);
  check_values(&fixture, &model);
  same_however_filled(&history, &fixture, ops.len() * 7 + 1);
  for (x, y, z) in queries {
    ask_all(
      &mut fixture,
      &model,
      (x % model.len(), y % model.len(), *z),
      &mut census,
    );
  }
  for pair in tips.windows(2) {
    ask_all(
      &mut fixture,
      &model,
      (index(pair[0]), index(pair[1]), index(pair[0])),
      &mut census,
    );
  }
  fixture.finish();
  census
}

/// A-666 (A-645 "Queries by API"). Do: grow histories of up to 2,000 versions from generated steps (runs of up to 800
/// versions and short ones, merges of two and three, branches opened anywhere, criss-cross merges, unrelated roots,
/// merges of old versions), derive every version's value eagerly and again lazily in another order, and ask
/// is-ancestor both ways, the merge bases and bisect's midpoint of generated pairs. Expect: every value's fields equal
/// their definitions (Myers' jump depths, the immediate dominator, the run bottom); the lazy values equal the eager
/// ones; every answer equals the naive walk's; every question's visits are within its stated bound and its parents
/// taken in within the merges the model puts in its reach; the store is empty after; and the census, printed every run,
/// met every kind, among them questions a walk would have cost four times over.
#[test]
#[allow(
  clippy::print_stderr,
  reason = "the census is the oracle's evidence, printed with every run"
)]
fn t_a666_every_answer_equals_a_naive_walk_within_its_bound() {
  let census = RefCell::new(Census::default());
  let config = ProptestConfig {
    cases: slates_test_seeds::cases(32),
    ..ProptestConfig::default()
  };
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(config));
  let ops = proptest::collection::vec(op(), 1..48);
  let queries = proptest::collection::vec((any::<usize>(), any::<usize>(), any::<usize>()), 8..20);
  let result = runner.run(&(ops, queries), |(ops, queries)| {
    census.borrow_mut().add(&case(&ops, &queries));
    Ok(())
  });
  if let Err(failure) = result {
    panic!("{failure}");
  }
  let census = census.into_inner();
  eprintln!("ancestry census: {census:?}");
  census.assert_met();
}

/// The values of a history derived one after another by the type alone, each held once by the caller.
struct Derived {
  store: ContentStore,
  history: History,
  ty: Ancestry<History>,
  roots: Vec<ContentId>,
}

impl Derived {
  fn new() -> Derived {
    Derived {
      store: ContentStore::new(BUDGET, WINDOW).unwrap(),
      history: History::default(),
      ty: Ancestry::declare(Mode::Eager),
      roots: Vec::new(),
    }
  }

  /// Records a version of `parents` and derives its value.
  fn push(&mut self, parents: &[u64]) -> u64 {
    let version = self.history.push(parents.to_vec());
    let values: Vec<ContentId> = parents
      .iter()
      .map(|parent| self.roots[index(*parent)])
      .collect();
    let root = self
      .ty
      .derive(&self.history, &mut self.store, version, &values)
      .unwrap();
    self.roots.push(root);
    version
  }

  /// A run of `length` versions on `from`; its last.
  fn run(&mut self, from: u64, length: u64) -> u64 {
    (0..length).fold(from, |tip, _| self.push(&[tip]))
  }

  fn root(&self, version: u64) -> ContentId {
    self.roots[index(version)]
  }

  /// Gives every value back: the store is then empty, so no question kept a charge.
  fn finish(mut self) {
    for root in std::mem::take(&mut self.roots) {
      self.store.release(&root).unwrap();
    }
    assert!(self.store.is_empty());
    assert_eq!((self.store.used(), self.store.charged()), (0, 0));
  }
}

/// A linear history of 2^17 versions with one side branch of 5,000 from version 60,000; its tip and the branch's.
fn long_history() -> (Derived, u64, u64) {
  let mut derived = Derived::new();
  let root = derived.push(&[]);
  let tip = derived.run(root, LONG_HISTORY - 1);
  let side = derived.run(60_000, 5_000);
  (derived, tip, side)
}

/// The merge bases of `one` and `two` in a derived history, and whether the paint stayed within its bound and under a
/// thousand visits.
fn bases_within(derived: &mut Derived, (one, two): (u64, u64), l: u64) -> (Vec<usize>, bool, bool) {
  let (first, second) = (derived.root(one), derived.root(two));
  let (bases, cost) = merge_bases(&mut derived.store, first, second).unwrap();
  (
    versions_of(&bases),
    cost.visits <= paint_bound(&cost, l),
    cost.visits < 1_000,
  )
}

/// A-666 (the bound's point). Do: derive a linear history of 2^17 versions and a side branch of 5,000 off version
/// 60,000; ask whether version 1,000 is an ancestor of the tip and the tip of version 1,000; the merge bases of the tip
/// and version 50,000, and of the side branch's tip and the tip; and bisect between the tip and the root. Expect the
/// answers a linear history gives (true, false, version 50,000, version 60,000, version 65,536 of 131,071 candidates
/// weighing 65,536), each within its bound at L = 51 and in fewer than a thousand visits, where a walk visits over
/// 60,000 versions.
#[test]
fn t_a666_a_long_linear_history_is_answered_in_logarithmic_visits() {
  let (mut derived, tip, side) = long_history();
  let l = 3 * floor_lg(LONG_HISTORY + 1);
  let (early, tip_root) = (derived.root(1_000), derived.root(tip));
  let (yes, cost) = is_ancestor(&mut derived.store, early, tip_root).unwrap();
  assert_eq!(
    (yes, cost.visits <= ancestor_bound(&cost, l)),
    (true, true),
    "{cost:?}"
  );
  let (no, cost) = is_ancestor(&mut derived.store, tip_root, early).unwrap();
  assert_eq!((no, cost.visits <= 2), (false, true), "{cost:?}");
  assert_eq!(
    bases_within(&mut derived, (tip, 50_000), l),
    (vec![50_000], true, true)
  );
  assert_eq!(
    bases_within(&mut derived, (side, tip), l),
    (vec![60_000], true, true)
  );
  let root = derived.root(0);
  let (midpoint, cost) = bisect(&mut derived.store, tip_root, &[root]).unwrap();
  let midpoint = midpoint.unwrap();
  let answer = (
    version_of(&midpoint.found),
    midpoint.weight,
    midpoint.candidates,
  );
  let within = (cost.visits <= bisect_bound(&cost, l), cost.visits < 1_000);
  assert_eq!(
    (answer, within),
    ((65_536, 65_536, 131_071), (true, true)),
    "{cost:?}"
  );
  derived.finish();
}

/// Shape: how far back on the main line a pull request's branch is opened, at most, in main versions: the most
/// branches open across any one version of the main line.
const FORK_BACK: usize = 4;

/// Shape: the pull requests merged into the main line.
const PULL_REQUESTS: usize = 1_000;

/// A main line of `PULL_REQUESTS` merges: each a commit on the main line merged with a two-commit branch opened up to
/// `FORK_BACK` main versions back. The main line's versions, in order.
fn pull_request_history() -> (Derived, Vec<u64>) {
  let mut derived = Derived::new();
  let mut main = vec![derived.push(&[])];
  for request in 0..PULL_REQUESTS {
    let fork = main[main.len().saturating_sub(1 + request % FORK_BACK)];
    let branch = derived.run(fork, 2);
    let direct = derived.push(&[*main.last().unwrap()]);
    let merge = derived.push(&[direct, branch]);
    main.extend([direct, merge]);
  }
  (derived, main)
}

/// A-666 (the skip tree's point). Do: merge 1,000 pull requests into a main line, each a branch opened up to four main
/// versions back; ask whether main version 403, a commit that open branches pass over, is an ancestor of the main
/// line's tip, and the merge bases of the tip and a branch of ten commits opened at it. Expect true and that version,
/// the lowering opening at most the merges of the branches open across it (two a branch, against some 800 merges above
/// it), and the paint jumping along the skip tree.
#[test]
fn t_a666_a_merge_heavy_history_opens_only_the_branches_open_across_the_target() {
  let (mut derived, main) = pull_request_history();
  let tip = *main.last().unwrap();
  let target = main[403];
  let (target_root, tip_root) = (derived.root(target), derived.root(tip));
  let (yes, cost) = is_ancestor(&mut derived.store, target_root, tip_root).unwrap();
  let bounded = cost.opened <= 2 * number(FORK_BACK);
  assert_eq!((yes, bounded), (true, true), "{cost:?}");
  let feature = derived.run(target, 10);
  let feature_root = derived.root(feature);
  let (bases, cost) = merge_bases(&mut derived.store, tip_root, feature_root).unwrap();
  assert_eq!(
    (versions_of(&bases), cost.jumps > 0),
    (vec![index(target)], true),
    "{cost:?}"
  );
  derived.finish();
}

/// The visits a search from version `depth` of a chain to the version at `target` takes, after reading its start,
/// and whether it landed there.
fn search(derived: &mut Derived, depth: u64, target: u64) -> (u64, bool) {
  let (start, expected) = (derived.root(depth), derived.root(target));
  let mut walk = Walk::new(&mut derived.store);
  let at = walk.get(start).unwrap();
  let found = ancestor_at(&mut walk, FIRST_PARENTS, at, target).unwrap();
  let landed = walk.node(found).unwrap().root == expected;
  let ((), cost) = walk.finish(Ok(())).unwrap();
  (cost.visits - 1, landed)
}

/// A-666 (Myers' bound, exactly). Do: derive a chain of 512 versions and search, from every depth D, the ancestor at
/// every depth below it. Expect: every search lands on the version at its depth; the jump depths equal Myers' J by his
/// digits; every search to a depth past the root visits at most 3⌊lg(D + 1)⌋ - 2 values and meets it exactly from
/// D = 2^k - k \[A: Myers 1983, Corollary 5: "tight"\]; and every search to the root at most ⌊lg(D + 1)⌋ + 1.
#[test]
fn t_a666_a_skew_search_meets_myers_bound_exactly() {
  let mut derived = Derived::new();
  let root = derived.push(&[]);
  derived.run(root, 511);
  let jumps = digit_jumps(511);
  for depth in 1..=511_u64 {
    assert_eq!(skew_jump_depth(depth), jumps[index(depth)], "J({depth})");
    let searches: Vec<(u64, bool)> = (0..depth)
      .map(|target| search(&mut derived, depth, target))
      .collect();
    let most = searches
      .iter()
      .skip(1)
      .map(|(visits, _)| *visits)
      .max()
      .unwrap_or(0);
    let bound = (3 * floor_lg(depth + 1)).saturating_sub(2);
    let attained = (2..10).any(|k| depth == (1 << k) - k);
    let to_root = searches[0].0 <= floor_lg(depth + 1) + 1;
    let landed = searches.iter().all(|(_, landed)| *landed);
    assert_eq!(
      (landed, most <= bound, !attained || most == bound, to_root),
      (true, true, true, true),
      "from {depth}"
    );
  }
  derived.finish();
}

/// A-666 (A-647's identity). Do: write a root's value and its child's by hand, field by field as the module documents
/// them, and derive the same two versions. Expect the derived roots to be BLAKE3's key derivation over exactly those
/// bytes under the store's derived-node context: the format is the bytes, on every tier.
#[test]
fn t_a666_a_value_is_the_formats_bytes() {
  let mut derived = Derived::new();
  let root = derived.push(&[]);
  let child = derived.push(&[root]);
  let mut root_bytes = 0_u32.to_le_bytes().to_vec();
  root_bytes.extend(0_u32.to_le_bytes());
  root_bytes.push(0);
  for number in [1_u64, 0, 0] {
    root_bytes.extend(number.to_le_bytes());
  }
  root_bytes.extend(0_u64.to_be_bytes());
  let root_identity = derived_identity(&root_bytes);
  let mut child_bytes = 5_u32.to_le_bytes().to_vec();
  for _ in 0..5 {
    child_bytes.extend(root_identity.0);
  }
  child_bytes.extend(1_u32.to_le_bytes());
  child_bytes.push(1);
  for number in [2_u64, 1, 1] {
    child_bytes.extend(number.to_le_bytes());
  }
  child_bytes.extend(1_u64.to_be_bytes());
  let hand = (root_identity, derived_identity(&child_bytes));
  assert_eq!((derived.root(root), derived.root(child)), hand);
  derived.finish();
}

/// A derived node's identity, by BLAKE3's key derivation under the store's context, computed here.
fn derived_identity(bytes: &[u8]) -> ContentId {
  let mut hasher =
    blake3::Hasher::new_derive_key("slates-vcs 2026-11-19 object store: a derived value's node");
  hasher.update(bytes);
  ContentId(*hasher.finalize().as_bytes())
}

/// A payload of `parents` parents, the skip byte `skip`, the numbers (generation, depth, skip depth) and no name.
fn payload(parents: u32, skip: u8, numbers: [u64; 3]) -> Vec<u8> {
  let mut payload = parents.to_le_bytes().to_vec();
  payload.push(skip);
  for number in numbers {
    payload.extend(number.to_le_bytes());
  }
  payload
}

/// A-666 (hostile bytes). Do: read values whose bytes are each wrong one way: a payload cut short, a skip byte of 2, a
/// count of parents past the names, a name left over, a root of generation 2, a version of one parent with no
/// skip-tree parent, a skip-tree parent at skip depth zero. Expect each refused by the reader, and the well-formed root
/// read.
#[test]
fn t_a666_hostile_bytes_are_refused_by_the_reader() {
  let id = ContentId([7; 32]);
  let root = payload(0, 0, [1, 0, 0]);
  assert!(decode(&[], &root).is_some(), "the well-formed root reads");
  let cases: [(&[ContentId], Vec<u8>); 7] = [
    (&[], root[..root.len() - 1].to_vec()),
    (&[], payload(0, 2, [1, 0, 0])),
    (&[id], payload(2, 1, [2, 1, 1])),
    (&[id], root.clone()),
    (&[], payload(0, 0, [2, 0, 0])),
    (&[id, id, id], payload(1, 0, [2, 1, 0])),
    (&[id, id, id, id, id], payload(1, 1, [2, 1, 0])),
  ];
  for (names, bytes) in cases {
    assert!(decode(names, &bytes).is_none(), "{names:?} {bytes:?}");
  }
}

/// A chain of `length` versions derived by the type, and a forged value of one parent whose five links all name `link`
/// (a version's value, or a window when `None`) and whose numbers are `numbers` (generation, depth, skip depth).
fn forged(length: u64, link: Option<u64>, numbers: [u64; 3]) -> (Derived, ContentId) {
  let mut derived = Derived::new();
  let root = derived.push(&[]);
  derived.run(root, length - 1);
  let link = match link {
    Some(version) => derived.root(version),
    None => derived.store.put(b"not a value").unwrap(),
  };
  let mut bytes = payload(1, 1, numbers);
  bytes.extend(u64::MAX.to_be_bytes());
  let forged = derived
    .store
    .put_derived(&[link; 5], &bytes)
    .unwrap()
    .identity;
  (derived, forged)
}

/// Asks is-ancestor (when `ancestry`), merge bases and bisect about `forged` and the root: each refused `Malformed`,
/// and the store's charge given back.
fn refused(mut derived: Derived, forged: ContentId, ancestry: bool) {
  let root = derived.root(0);
  let charged = derived.store.charged();
  let mut refusals = vec![
    merge_bases(&mut derived.store, forged, root).map(|_| ()),
    bisect(&mut derived.store, forged, &[]).map(|_| ()),
  ];
  if ancestry {
    refusals.push(is_ancestor(&mut derived.store, root, forged).map(|_| ()));
  }
  for refusal in refusals {
    assert!(
      matches!(refusal, Err(AncestryRefused::Malformed { .. })),
      "{refusal:?}"
    );
  }
  assert_eq!(derived.store.charged(), charged, "every charge given back");
}

/// A-666 (hostile values asked about). Do: forge a value whose links name a window, not a value; one whose links name
/// a version four deep as its parent at depth zero; one of generation one with a parent; and one whose depth does not
/// follow its parent's. Ask merge bases, bisect and is-ancestor of each. Expect each refused `Malformed`, typed, with
/// the store's charge given back; the last, whose skip-tree links are well formed, is refused by the questions that
/// read its run (is-ancestor never reads its first-parent depth).
#[test]
fn t_a666_hostile_values_are_refused_by_every_question() {
  let (derived, forged_value) = forged(1, None, [2, 1, 1]);
  refused(derived, forged_value, true);
  let (derived, forged_value) = forged(5, Some(4), [2, 1, 1]);
  refused(derived, forged_value, true);
  let (derived, forged_value) = forged(1, Some(0), [1, 1, 1]);
  refused(derived, forged_value, true);
  let (derived, forged_value) = forged(1, Some(0), [2, 5, 1]);
  refused(derived, forged_value, false);
}

/// A-666 (refusals by kind). Do: derive a version given too few parents' values, and one the history does not hold;
/// and ask a question in a store whose budget leaves no room for what the question keeps. Expect `Parents`,
/// `NoVersion`, and the store's `OverBudget`, the store as it was each time.
#[test]
fn t_a666_refusals_by_kind_leave_the_store_as_it_was() {
  let mut derived = Derived::new();
  let root = derived.push(&[]);
  let child = derived.history.push(vec![root]);
  let used = derived.store.used();
  let parents = derived
    .ty
    .derive(&derived.history, &mut derived.store, child, &[]);
  assert_eq!(
    parents,
    Err(AncestryRefused::Parents {
      expected: 1,
      given: 0
    })
  );
  let unknown = derived
    .ty
    .derive(&derived.history, &mut derived.store, 99, &[]);
  assert_eq!(
    (unknown, derived.store.used()),
    (Err(AncestryRefused::NoVersion), used)
  );
  derived.store.set_budget(used);
  let question = is_ancestor(&mut derived.store, derived.roots[0], derived.roots[0]);
  let over = matches!(
    question,
    Err(AncestryRefused::Content(ContentRefused::OverBudget { .. }))
  );
  assert_eq!(
    (over, derived.store.charged(), derived.store.used()),
    (true, 0, used)
  );
  derived.store.set_budget(BUDGET);
  derived.finish();
}
