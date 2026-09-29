//! Exhaustive search of a model's reachable states at small scope — the machinery the consensus models share
//! (`docs/wip/research/consensus-enhancements.md` §5, "model-level exploration"; `tests/slot_model.rs`,
//! `tests/prefix_model.rs`). A model names its states, actions, faults and the packed representative of a
//! state's class under renaming; this module visits every reachable class once and checks each step.
//!
//! Two searches. [`search`] is breadth first on one core over fingerprints, with each class's key and parent
//! in an id table: its first fault is reached by a shortest history, which it replays concretely. [`explore`] is breadth first level by level
//! across every core over 128-bit fingerprints of the representatives: each worker expands a slice of the
//! frontier against a read-only view of the visited shards, then each shard takes the new successors that
//! hash to it — scoped threads, joined before the level ends, nothing shared mutably. Two states share a
//! fingerprint with probability 2⁻¹²⁸, so some pair among `n` does with probability below `n²/2¹²⁹` (under
//! 10⁻²¹ for a billion states), and only then could a state be skipped. Traversal order does not change the
//! work — a breadth-first and a depth-first search of the same scope visited the same 463,715 classes in
//! 1.13 s and 1.16 s (2026-09-28) — so the lever is cores: the parallel search did it in 0.16 s.
//!
//! Every search holds a memory ceiling derived from the measured cost of a state and fails rather than
//! exhausting the machine: the first model's search held 18 GB after 300 s without finishing (2026-09-28).

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::hash::{BuildHasherDefault, Hash, Hasher};

/// Shape: the resident memory a search may hold. GitHub's smallest runner (the macOS image, 7 GB) runs the
/// full-scale step alone after the build; this leaves it 3 GB.
pub(crate) const MEMORY_CEILING_BYTES: usize = 4 << 30;

/// Derived: the bytes a visited class costs the parallel search at its peak: a 16-byte fingerprint and a
/// control byte per bucket, the set at 7/8 load, and — while a shard grows — its old table beside the new one
/// of twice the buckets: `17 × 8/7 × 3`. Rounded up.
const VISITED_BYTES: usize = (16 + 1) * 8 * 3 / 7 + 1;

/// Derived: resident memory over the bytes the parallel search accounts for — measured 2,689 MB resident
/// against 1,556 MB accounted at the widest level of the slot model's five nodes and four terms
/// (2026-09-28): the buckets' and frontiers' doubling capacities and the allocator's retained pages, which no
/// field count sees. Rounded up. The serial search holds it too (its id table doubles as it grows).
const RESIDENT_PER_ACCOUNTED: usize = 2;

/// Derived: the bytes one state costs the serial search, accounted: its fingerprint in the visited set
/// ([`VISITED_BYTES`]), its packed key in the id table, and its parent's id and its place in the queue (four
/// bytes each). Until 2026-09-29 the visited set held whole keys, so each key was held twice and a state cost
/// about 400 bytes at the peak for a 64-byte key (measured on the slot model at 32 bytes: 193 and 142, scaled);
/// the prefix model's shortest history under a rejected rule then outgrew the 4 GiB ceiling.
fn serial_bytes_per_state<K>() -> usize {
  VISITED_BYTES + size_of::<K>() + 2 * size_of::<u32>()
}

/// A model the searches can run: its states, steps, faults, and a packed representative per class.
pub(crate) trait Model: Sync {
  /// A state of the model.
  type State: Copy + Eq + Send + Sync + fmt::Debug;
  /// One step.
  type Action: Copy + fmt::Display + Send + Sync;
  /// Why a step is a violation.
  type Fault: Copy + fmt::Debug + PartialEq + Send + Sync;
  /// A packed representative of a state's class under the renamings the model's actions commute with.
  type Key: Copy + Eq + Hash + Ord + Send + Sync;

  /// The names of the paths a step can take, in the order of [`Step::paths`]'s bits.
  fn paths(&self) -> &'static [&'static str];
  /// The state every history starts from.
  fn initial(&self) -> Self::State;
  /// Every action worth trying from `state`; each is checked for being enabled when applied.
  fn candidates(&self, state: &Self::State, out: &mut Vec<Self::Action>);
  /// `action` applied to `state`, or `None` when it is not enabled there or changes nothing.
  fn apply(
    &self,
    state: &Self::State,
    action: Self::Action,
  ) -> Option<Step<Self::State, Self::Fault>>;
  /// The representative of `state`'s class, packed.
  fn canonical(&self, state: &Self::State) -> Self::Key;
  /// The state a packed representative stands for.
  fn unpack(&self, key: Self::Key) -> Self::State;
  /// A line describing `state`, for a printed history.
  fn describe(&self, state: &Self::State) -> String;
}

/// The result of one step: the next state, a fault when the step is a violation, and the paths it took
/// (a bit each, in [`Model::paths`] order — the non-vacuity counters' keys).
pub(crate) struct Step<S, F> {
  pub(crate) state: S,
  pub(crate) fault: Option<F>,
  pub(crate) paths: u64,
}

impl<S, F> Step<S, F> {
  /// A step to `state` that takes no counted path and raises no fault.
  pub(crate) fn plain(state: S) -> Step<S, F> {
    Step {
      state,
      fault: None,
      paths: 0,
    }
  }
}

/// A history: the actions taken from a start state, and the fault its last one raised.
pub(crate) type History<M> = (Vec<<M as Model>::Action>, <M as Model>::Fault);

/// What one search found: the classes it reached, how often each path was taken, the first fault that
/// stopped it (with the history that reached it), and the first fault it recorded and went past.
pub(crate) struct Report<M: Model> {
  pub(crate) states: usize,
  pub(crate) paths: Vec<u64>,
  pub(crate) stopped: Option<History<M>>,
  pub(crate) passed: Option<History<M>>,
}

impl<M: Model> Report<M> {
  fn new(model: &M) -> Report<M> {
    Report {
      states: 0,
      paths: vec![0; model.paths().len()],
      stopped: None,
      passed: None,
    }
  }

  /// How often the path named `name` was taken.
  pub(crate) fn taken(&self, model: &M, name: &str) -> u64 {
    model
      .paths()
      .iter()
      .position(|path| *path == name)
      .map_or(0, |at| self.paths[at])
  }
}

fn tally(paths: &mut [u64], taken: u64) {
  for (path, count) in paths.iter_mut().enumerate() {
    *count += (taken >> path) & 1;
  }
}

/// Packs fields of known widths into `W` words, least significant bit first.
pub(crate) struct Packer<const W: usize> {
  words: [u64; W],
  at: usize,
}

impl<const W: usize> Packer<W> {
  /// An empty packer, to write into.
  pub(crate) fn new() -> Packer<W> {
    Packer {
      words: [0; W],
      at: 0,
    }
  }

  /// A packer over `words`, to read from.
  pub(crate) fn over(words: [u64; W]) -> Packer<W> {
    Packer { words, at: 0 }
  }

  /// The packed words.
  pub(crate) fn words(&self) -> [u64; W] {
    self.words
  }

  /// Appends the low `width` bits of `value`.
  pub(crate) fn put(&mut self, width: usize, value: u64) {
    let (word, place) = (self.at / 64, self.at % 64);
    self.words[word] |= value << place;
    if place + width > 64 {
      self.words[word + 1] |= value >> (64 - place);
    }
    self.at += width;
  }

  /// Reads the next `width` bits.
  pub(crate) fn take(&mut self, width: usize) -> u64 {
    let (word, place) = (self.at / 64, self.at % 64);
    let mut value = self.words[word] >> place;
    if place + width > 64 {
      value |= self.words[word + 1] << (64 - place);
    }
    self.at += width;
    value & ((1 << width) - 1)
  }

  /// Reads the next `width` bits as a byte.
  pub(crate) fn small(&mut self, width: usize) -> u8 {
    u8::try_from(self.take(width)).unwrap()
  }

  /// Appends an optional small value as zero for none, else the value plus one.
  pub(crate) fn put_option(&mut self, width: usize, value: Option<u8>) {
    self.put(width, value.map_or(0, |value| u64::from(value) + 1));
  }

  /// Reads what [`Packer::put_option`] wrote.
  pub(crate) fn take_option(&mut self, width: usize) -> Option<u8> {
    self.small(width).checked_sub(1)
  }
}

/// Visits every order of `order[at..end]` for the first run in `runs`, then the orders of the runs after it.
fn permute_run(
  order: &mut [usize],
  at: usize,
  end: usize,
  runs: &[(usize, usize)],
  visit: &mut dyn FnMut(&[usize]),
) {
  if at + 1 >= end {
    each_tie_order(order, runs, visit);
    return;
  }
  for swap in at..end {
    order.swap(at, swap);
    permute_run(order, at + 1, end, runs, visit);
    order.swap(at, swap);
  }
}

/// Visits every order that permutes `order` only within `runs` (half-open ranges of equal signatures).
fn each_tie_order(order: &mut [usize], runs: &[(usize, usize)], visit: &mut dyn FnMut(&[usize])) {
  match runs.split_first() {
    None => visit(order),
    Some((&(start, end), rest)) => permute_run(order, start, end, rest, visit),
  }
}

/// The least key `arrange` gives over every order of the nodes sorted by `signatures` that permutes only
/// nodes of equal signature — the representative of a class under renaming nodes, when `signatures` are
/// unchanged by renaming and `arrange` places the node at `order[k]` as node `k`.
pub(crate) fn least_over_ties<K: Ord + Copy, S: Ord>(
  signatures: &[S],
  arrange: &mut dyn FnMut(&[usize]) -> K,
) -> K {
  let mut order: Vec<usize> = (0..signatures.len()).collect();
  order.sort_by(|left, right| signatures[*left].cmp(&signatures[*right]));
  let mut runs = Vec::new();
  let mut start = 0;
  for end in 1..=order.len() {
    if end == order.len() || signatures[order[end]] != signatures[order[start]] {
      if end - start > 1 {
        runs.push((start, end));
      }
      start = end;
    }
  }
  let mut least: Option<K> = None;
  each_tie_order(&mut order, &runs, &mut |candidate| {
    let key = arrange(candidate);
    if least.is_none_or(|least| key < least) {
      least = Some(key);
    }
  });
  least.unwrap()
}

/// A multiplicative hasher for the visited sets (FxHash's mixing step): their keys are packed words or
/// fingerprints hashed millions of times, and SipHash's resistance to chosen keys buys nothing here.
#[derive(Default)]
pub(crate) struct Mix(u64);

impl Hasher for Mix {
  fn finish(&self) -> u64 {
    self.0
  }

  fn write(&mut self, bytes: &[u8]) {
    for byte in bytes {
      self.write_u64(u64::from(*byte));
    }
  }

  fn write_u64(&mut self, word: u64) {
    /// Format: FxHash's multiplier (`rustc-hash`).
    const MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;
    self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(MULTIPLIER);
  }

  fn write_u128(&mut self, word: u128) {
    self.write_u64(u64::try_from(word >> 64).unwrap());
    self.write_u64(u64::try_from(word & u128::from(u64::MAX)).unwrap());
  }
}

/// A set hashed with [`Mix`].
type Set<T> = HashSet<T, BuildHasherDefault<Mix>>;

/// A 128-bit fingerprint of a packed representative: SipHash-1-3 (`DefaultHasher`, fixed keys) over it with a
/// domain byte, twice.
fn fingerprint<K: Hash>(key: &K) -> u128 {
  let lane = |domain: u8| {
    let mut hasher = DefaultHasher::new();
    domain.hash(&mut hasher);
    key.hash(&mut hasher);
    hasher.finish()
  };
  (u128::from(lane(0)) << 64) | u128::from(lane(1))
}

/// The concrete actions that lead from `start` along the chain of representatives ending at `id`, then to a
/// step with a fault like `fault` — replayed, since each representative renames its state.
fn replay<M: Model>(
  model: &M,
  start: M::State,
  keys: &[M::Key],
  parents: &[u32],
  id: u32,
  fault: M::Fault,
) -> Vec<M::Action> {
  let mut chain = vec![id];
  while *chain.last().unwrap() != 0 {
    chain.push(parents[usize::try_from(*chain.last().unwrap()).unwrap()]);
  }
  chain.reverse();
  let mut state = start;
  let mut actions = Vec::new();
  let mut tried = Vec::new();
  for step in chain.iter().skip(1) {
    let key = keys[usize::try_from(*step).unwrap()];
    tried.clear();
    model.candidates(&state, &mut tried);
    let (action, next) = tried
      .iter()
      .find_map(|action| {
        model
          .apply(&state, *action)
          .filter(|next| model.canonical(&next.state) == key)
          .map(|next| (*action, next.state))
      })
      .expect("a step to the next representative on the path");
    actions.push(action);
    state = next;
  }
  tried.clear();
  model.candidates(&state, &mut tried);
  let last = tried
    .iter()
    .find(|action| {
      model.apply(&state, **action).is_some_and(|next| {
        next
          .fault
          .is_some_and(|found| std::mem::discriminant(&found) == std::mem::discriminant(&fault))
      })
    })
    .expect("the faulting step");
  actions.push(*last);
  actions
}

/// Breadth-first search on one core from `start` until every reachable class is visited or a fault `stops`
/// accepts is found; the first other fault is recorded, and the search goes on past it. Fails past the
/// memory ceiling.
pub(crate) fn search<M: Model>(
  model: &M,
  start: M::State,
  stops: &dyn Fn(&M::Fault) -> bool,
) -> Report<M> {
  let budget = MEMORY_CEILING_BYTES / RESIDENT_PER_ACCOUNTED / serial_bytes_per_state::<M::Key>();
  let initial = model.canonical(&start);
  let mut visited: Set<u128> = Set::default();
  visited.insert(fingerprint(&initial));
  let mut keys = vec![initial];
  let mut parents = vec![0_u32];
  let mut queue = VecDeque::from([0_u32]);
  let mut report = Report::new(model);
  let mut tried = Vec::new();
  while let Some(id) = queue.pop_front() {
    let state = model.unpack(keys[usize::try_from(id).unwrap()]);
    tried.clear();
    model.candidates(&state, &mut tried);
    for next in tried
      .iter()
      .filter_map(|action| model.apply(&state, *action))
    {
      tally(&mut report.paths, next.paths);
      if let Some(fault) = next.fault {
        let found = Some((replay(model, start, &keys, &parents, id, fault), fault));
        if stops(&fault) {
          report.stopped = found;
          report.states = visited.len();
          return report;
        }
        if report.passed.is_none() {
          report.passed = found;
        }
      }
      let key = model.canonical(&next.state);
      if visited.insert(fingerprint(&key)) {
        assert!(
          keys.len() < budget,
          "the serial search exceeds its budget of {budget} states"
        );
        queue.push_back(u32::try_from(keys.len()).unwrap());
        keys.push(key);
        parents.push(id);
      }
    }
  }
  report.states = visited.len();
  report
}

/// The workers a parallel search runs on: the machine's parallelism.
fn workers() -> usize {
  std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// The shard of `shards` that owns fingerprint `print`.
fn shard_of(print: u128, shards: usize) -> usize {
  usize::try_from(print % u128::try_from(shards).unwrap()).unwrap()
}

/// What one worker made of its part of a frontier: its successors not yet visited, bucketed by the shard
/// that owns their fingerprints; how often it took each path; and the first fault it met, with the state it
/// stepped from and the step.
struct Expansion<M: Model> {
  buckets: Vec<Vec<(u128, M::Key)>>,
  paths: Vec<u64>,
  fault: Option<Met<M>>,
}

/// A fault the parallel search met, with the state it was met from and the step that raised it.
pub(crate) type Met<M> = (
  <M as Model>::State,
  <M as Model>::Action,
  <M as Model>::Fault,
);

/// Expands `part` of a frontier against a read-only view of the visited shards, bucketing at most
/// `allowance` successors (`None` once it would take more).
fn expand<M: Model>(
  model: &M,
  part: &[M::Key],
  visited: &[Set<u128>],
  allowance: usize,
) -> Option<Expansion<M>> {
  let mut expansion = Expansion {
    buckets: (0..visited.len()).map(|_| Vec::new()).collect(),
    paths: vec![0; model.paths().len()],
    fault: None,
  };
  let mut bucketed = 0_usize;
  let mut actions = Vec::new();
  for key in part {
    let state = model.unpack(*key);
    actions.clear();
    model.candidates(&state, &mut actions);
    for action in &actions {
      let Some(next) = model.apply(&state, *action) else {
        continue;
      };
      tally(&mut expansion.paths, next.paths);
      if let Some(fault) = next.fault {
        expansion.fault = Some((state, *action, fault));
        return Some(expansion);
      }
      let representative = model.canonical(&next.state);
      let print = fingerprint(&representative);
      let shard = shard_of(print, visited.len());
      if !visited[shard].contains(&print) {
        bucketed += 1;
        if bucketed > allowance {
          return None;
        }
        expansion.buckets[shard].push((print, representative));
      }
    }
  }
  Some(expansion)
}

/// Inserts into one shard the successors every worker bucketed for it, returning those it had not seen.
fn settle<K: Copy>(shard: &mut Set<u128>, buckets: Vec<&Vec<(u128, K)>>) -> Vec<K> {
  let mut fresh = Vec::new();
  for (print, key) in buckets.into_iter().flatten() {
    if shard.insert(*print) {
      fresh.push(*key);
    }
  }
  fresh
}

/// Breadth-first search, level by level across every core, over fingerprints of representatives, from the
/// model's initial state: every reachable class is visited once. Stops at the first level with a fault,
/// reporting the step that met it ([`search`] gives the whole shortest history, where it fits). Fails past
/// `ceiling_bytes` of resident memory; `print_levels` prints each level's accounting.
pub(crate) fn explore<M: Model>(
  model: &M,
  ceiling_bytes: usize,
  print_levels: bool,
) -> (Report<M>, Option<Met<M>>) {
  let workers = workers();
  let frontier_bytes = size_of::<M::Key>();
  let bucket_bytes = size_of::<(u128, M::Key)>();
  let mut visited: Vec<Set<u128>> = (0..workers).map(|_| Set::default()).collect();
  let initial = model.canonical(&model.initial());
  let print = fingerprint(&initial);
  visited[shard_of(print, workers)].insert(print);
  let mut frontier = vec![initial];
  let mut report = Report::new(model);
  report.states = 1;
  while !frontier.is_empty() {
    let held = report.states * VISITED_BYTES + frontier.len() * frontier_bytes;
    let allowance =
      (ceiling_bytes / RESIDENT_PER_ACCOUNTED).saturating_sub(held) / bucket_bytes / workers;
    let part = frontier.len().div_ceil(workers);
    let expansions: Vec<Expansion<M>> = std::thread::scope(|threads| {
      let running: Vec<_> = frontier
        .chunks(part)
        .map(|slice| threads.spawn(|| expand(model, slice, &visited, allowance)))
        .collect();
      running
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>()
    })
    .into_iter()
    .collect::<Option<_>>()
    .unwrap_or_else(|| {
      panic!(
        "the search exceeds {ceiling_bytes} bytes at {} classes and a frontier of {}",
        report.states,
        frontier.len()
      )
    });
    for expansion in &expansions {
      tally_all(&mut report.paths, &expansion.paths);
    }
    if let Some(met) = expansions.iter().find_map(|expansion| expansion.fault) {
      report.states = visited.iter().map(HashSet::len).sum();
      return (report, Some(met));
    }
    let width = frontier.len();
    // The expanded level is done with; free it before the next is gathered.
    frontier = Vec::new();
    let fresh: Vec<Vec<M::Key>> = std::thread::scope(|threads| {
      let running: Vec<_> = visited
        .iter_mut()
        .enumerate()
        .map(|(shard, set)| {
          let buckets: Vec<&Vec<(u128, M::Key)>> = expansions
            .iter()
            .map(|expansion| &expansion.buckets[shard])
            .collect();
          threads.spawn(move || settle(set, buckets))
        })
        .collect();
      running
        .into_iter()
        .map(|shard| shard.join().unwrap())
        .collect()
    });
    let bucketed: usize = expansions
      .iter()
      .flat_map(|expansion| expansion.buckets.iter().map(Vec::len))
      .sum();
    if print_levels {
      eprintln!(
        "level: visited {} frontier {width} bucketed {bucketed} accounted {} MB",
        report.states,
        (report.states * VISITED_BYTES + width * frontier_bytes + bucketed * bucket_bytes) >> 20
      );
    }
    drop(expansions);
    frontier.reserve_exact(fresh.iter().map(Vec::len).sum());
    for part in fresh {
      frontier.extend(part);
    }
    report.states = visited.iter().map(HashSet::len).sum();
  }
  (report, None)
}

fn tally_all(paths: &mut [u64], more: &[u64]) {
  for (count, added) in paths.iter_mut().zip(more) {
    *count += added;
  }
}

/// Replays `actions` from `start`, requiring each to be enabled where it is taken; returns the state they
/// reach and the fault the last one raised.
pub(crate) fn run_script<M: Model>(
  model: &M,
  start: M::State,
  actions: &[M::Action],
) -> (M::State, Option<M::Fault>) {
  let mut state = start;
  let mut fault = None;
  for action in actions {
    let next = model
      .apply(&state, *action)
      .unwrap_or_else(|| panic!("{action} is not enabled"));
    fault = next.fault;
    state = next.state;
  }
  (state, fault)
}

/// Replays `actions` from `start` and prints each step with the model's description of the state after it.
pub(crate) fn print_trace<M: Model>(model: &M, start: M::State, actions: &[M::Action]) {
  let mut state = start;
  for (step, action) in actions.iter().enumerate() {
    state = model.apply(&state, *action).unwrap().state;
    eprintln!(
      "  {:>2}. {:<36} | {}",
      step + 1,
      action.to_string(),
      model.describe(&state)
    );
  }
}

/// Prints `report` under `label`: the classes, each path's count, and any recorded history.
pub(crate) fn print_report<M: Model>(model: &M, label: &str, start: M::State, report: &Report<M>) {
  eprintln!(
    "{label}: {} states; {}",
    report.states,
    model
      .paths()
      .iter()
      .zip(&report.paths)
      .map(|(name, count)| format!("{name} {count}"))
      .collect::<Vec<_>>()
      .join(", ")
  );
  for (heading, found) in [
    ("stopped by", &report.stopped),
    ("went past", &report.passed),
  ] {
    if let Some((actions, fault)) = found {
      eprintln!("{heading} {fault:?} after {} steps:", actions.len());
      print_trace(model, start, actions);
    }
  }
}
