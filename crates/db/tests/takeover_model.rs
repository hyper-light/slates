//! A-251: the host-takeover counting rule (§4.8 "Promotion and takeover", `slates_db::takeover::readiness`) and how a
//! successor learns the retired host's objects, over a model of a paged, multi-object takeover with an omission nemesis.
//!
//! **The model.** A retired host's objects, each recovered through one or two cohorts (the retired host and some
//! survivors), with a history of writes. A write goes to one cohort (an owner in the recovery chain commits under its
//! own placement) or to all of them (a joint write), is acknowledged by some of its cohorts' hosts, and commits when each
//! of its cohorts holds it as `Quorum::committed_within` says. Each survivor holds the newest record it acknowledged,
//! under the authority it was written under. Some objects were first written by an earlier owner outside the retired
//! host's recovery chain (a takeover the retired host itself completed): the retired host adopted the newest of them,
//! committed under its own authority, and wrote on; a holder that missed that adoption still holds the earlier owner's
//! older copy. The successor learns its own copies when its lease gate is open (from the start, or once the scheduled
//! answers are in, as the gate opens when the retired owner's lease has lapsed), and asks every other survivor for pages
//! of what it holds, a few entries a page, answered in an order the history chooses. After every answer it adopts each
//! object the rule calls ready, with the newest record reported; an adoption is final.
//!
//! **The nemesis** leaves objects out of what a holder's routing gives it, the successor's own learning included
//! (`takeover::list_page` and `promise_local_copies` select by routing, and a warm restart loses routing), while the
//! page still ends complete. Any holder may leave out any object, every holder of it included.
//!
//! **How a successor learns the objects** ([`Listing`]), the supervisor's two shapes for A-251 and the forms of (i):
//! - [`Listing::Routing`], before A-251: a holder lists what its routing gives it. An object every holder leaves out is
//!   never learned.
//! - [`Listing::RoutingOrChain`], shape (i) as first built: routing, plus every copy whose authority's line ends in the
//!   round. It misses a copy whose authority an install had moved to the asker before its routing was lost.
//! - [`Listing::ChainOrAsker`], shape (i) as one rule, literally: authority in the chain, or the asker. It learns the
//!   asker's own live objects in another host's round.
//! - [`Listing::MovedFrom`], shape (i) as one rule, chosen: the line of the copy's authority, or of the owner an install
//!   moved it from. Its cost per write is nothing: the authority and its last move are kept with the copy.
//! - [`Listing::Directory`], shape (ii): the retired host's recovery cohort keeps its object directory at `f + 1`, and
//!   the successor reads every name from it. Its cost is one more committed record per object created.
//!
//! **The oracle** is the design's NoLoss: an adopted record is at least as new as every committed record of its object,
//! and every object with a committed record is adopted (a round that confirms without one loses it silently). Under
//! A-251's rule the successor also asks, by name, each survivor whose pages are complete about every object it learned
//! and that survivor did not list, and answers itself the same way while its gate is open. A survivor answers with its
//! record, or that it holds nothing.
//!
//! - `today_s_rule_adopts_an_older_record_when_a_holder_leaves_a_copy_out`: the minimal history, red on today's rule.
//! - `the_named_rule_with_moved_from_listing_holds_no_loss_on_every_history` (proptest): the chosen rule.
//! - `the_nemesis_breaks_today_s_rule_and_each_shape_is_measured`: over generated histories, what each rule and shape
//!   loses with the nemesis off and on, and each shape's cost per write.

// Test harness: an unwrap or indexing here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_db::register::{Accepted, HostEpoch, HostId, ObjectId, Quorum};
use slates_db::takeover::{CountingRule, Heard, Readiness, readiness};

/// Shape: the retired host.
const DEPARTED: HostId = HostId(0);
/// Shape: the successor, the first survivor.
const SUCCESSOR: HostId = HostId(1);
/// Shape: the most objects a history's retired host owned: enough for pages to split across objects.
const MAX_OBJECTS: u64 = 5;
/// Shape: the most writes an object's history holds, each owner's.
const MAX_WRITES: u64 = 3;
/// Shape: the most entries a page carries, so a holder's answer spans several pages.
const MAX_PAGE_ENTRIES: u64 = 3;
/// Shape: the answers a history's schedule delivers in its chosen order before the rest are delivered in turn.
const SCHEDULE_STEPS: u64 = 40;
/// Shape: the generated histories the counting test walks.
const SEARCH_HISTORIES: u64 = 1_000;

/// How a successor learns the retired host's objects (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Listing {
  /// A holder lists what its routing gives it (before A-251).
  Routing,
  /// A holder lists what its routing gives it, and every copy whose authority's line ends in this round: routing plus a
  /// second path, as A-251 (b) first built shape (i).
  RoutingOrChain,
  /// Shape (i) as one rule, literally: every copy whose authority is in the retired host's chain, or is the asker.
  ChainOrAsker,
  /// Shape (i) as one rule with the authority's history: every copy whose authority's line, or the line of the owner an
  /// install moved it from, ends in this round.
  MovedFrom,
  /// Shape (ii): the successor reads every name from the retired host's directory, kept by its cohort at `f + 1`.
  Directory,
}

/// A small deterministic generator, so a history is a function of its seed (xorshift64*).
struct Draw(u64);

impl Draw {
  fn new(seed: u64) -> Draw {
    Draw(seed | 1)
  }

  fn next(&mut self) -> u64 {
    self.0 ^= self.0 >> 12;
    self.0 ^= self.0 << 25;
    self.0 ^= self.0 >> 27;
    self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  /// A value in `low..=high`.
  fn within(&mut self, low: u64, high: u64) -> u64 {
    low + self.next() % (high - low + 1)
  }

  fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
    self.next() % denominator < numerator
  }
}

/// A copy a survivor holds: the newest record it acknowledged, and whether that record was written under an authority
/// in the retired host's recovery chain (else under an earlier owner's).
#[derive(Clone, Debug)]
struct Copy {
  record: Accepted,
  in_chain: bool,
}

/// One object: the retired host's, or (`foreign`) one of the asker's own live objects some survivors also hold.
#[derive(Clone, Debug)]
struct Object {
  /// The asker's own live object, under the asker's authority on every holder: never the round's to learn.
  foreign: bool,
  cohorts: Vec<Vec<HostId>>,
  /// The copy each host holds.
  held: BTreeMap<HostId, Copy>,
  /// The newest committed record's sequence, if any committed.
  committed: Option<u64>,
}

/// A takeover's history: the region, the objects, what the nemesis leaves out, and the order answers arrive in.
#[derive(Clone, Debug)]
struct History {
  quorum: Quorum,
  survivors: Vec<HostId>,
  objects: BTreeMap<ObjectId, Object>,
  /// The objects each holder's routing leaves out.
  omitted: BTreeMap<HostId, BTreeSet<ObjectId>>,
  /// Of those, the copies whose authority an install had already moved to the asker before routing lost them (a
  /// holder restored warm routes such a copy to the asker as a live owner): their authority alone no longer names the
  /// retired host.
  moved: BTreeSet<(HostId, ObjectId)>,
  page_entries: usize,
  /// Whether the successor may promise its own copies from the start; otherwise its gate opens after the schedule.
  gate_open: bool,
  /// Which survivor answers at each step of the schedule (an index into the asked survivors).
  schedule: Vec<usize>,
}

/// The object numbered `number`.
fn object_id(number: u64) -> ObjectId {
  let mut bytes = [0u8; 16];
  bytes[..8].copy_from_slice(&number.to_le_bytes());
  ObjectId(bytes)
}

fn record(sequence: u64) -> Accepted {
  Accepted {
    sequence,
    epoch: HostEpoch(1),
    value: sequence.to_le_bytes().to_vec(),
  }
}

/// The cohorts of one object: one or two, each the retired host and up to `2f` survivors (fewer for a forming region's).
fn cohorts(draw: &mut Draw, f: u64, survivors: &[HostId]) -> Vec<Vec<HostId>> {
  (0..draw.within(1, 2))
    .map(|_| {
      let size = draw.within(1, (2 * f).min(u64::try_from(survivors.len()).unwrap()));
      let mut pool = survivors.to_vec();
      let mut cohort = vec![DEPARTED];
      for _ in 0..size {
        let pick = usize::try_from(draw.within(0, u64::try_from(pool.len()).unwrap() - 1)).unwrap();
        cohort.push(pool.remove(pick));
      }
      cohort.sort_unstable_by_key(|host| host.0);
      cohort
    })
    .collect()
}

/// One write at `sequence` to one of `cohorts` or all of them, acknowledged by some of their hosts (all of each
/// cohort's commit quorum when `commit`); returns whether it committed.
fn write(
  draw: &mut Draw,
  quorum: Quorum,
  cohorts: &[Vec<HostId>],
  held: &mut BTreeMap<HostId, Copy>,
  sequence: u64,
  in_chain: bool,
  commit: bool,
) -> bool {
  let targets: Vec<&Vec<HostId>> = if !commit && cohorts.len() > 1 && draw.chance(2, 3) {
    let pick = usize::try_from(draw.within(0, u64::try_from(cohorts.len()).unwrap() - 1)).unwrap();
    vec![&cohorts[pick]]
  } else {
    cohorts.iter().collect()
  };
  let mut acked: BTreeSet<HostId> = targets
    .iter()
    .copied()
    .flatten()
    .copied()
    .filter(|_| draw.chance(2, 3))
    .collect();
  if commit {
    // An adoption is confirmed only once placed: each cohort's commit quorum holds it.
    for cohort in &targets {
      let need = quorum.commit().min(cohort.len()).max(1);
      for host in cohort.iter().take(need) {
        acked.insert(*host);
      }
    }
  }
  for host in &acked {
    held.insert(
      *host,
      Copy {
        record: record(sequence),
        in_chain,
      },
    );
  }
  targets.iter().all(|cohort| {
    let count = cohort.iter().filter(|host| acked.contains(host)).count();
    quorum.committed_within(count, cohort.len())
  })
}

/// One object's cohorts and writes. Sometimes an earlier owner outside the chain wrote first, and the retired host
/// adopted its newest record under its own authority, placed before it confirmed.
fn object(draw: &mut Draw, quorum: Quorum, survivors: &[HostId]) -> Object {
  let f = u64::from(quorum.f);
  let cohorts = cohorts(draw, f, survivors);
  let mut held: BTreeMap<HostId, Copy> = BTreeMap::new();
  let mut committed = None;
  let mut sequence = 0;
  if draw.chance(1, 4) {
    for _ in 0..draw.within(1, MAX_WRITES) {
      sequence += 1;
      if write(draw, quorum, &cohorts, &mut held, sequence, false, false) {
        committed = Some(sequence);
      }
    }
    sequence += 1;
    if write(draw, quorum, &cohorts, &mut held, sequence, true, true) {
      committed = Some(sequence);
    }
  }
  for _ in 0..draw.within(1, MAX_WRITES) {
    sequence += 1;
    if write(draw, quorum, &cohorts, &mut held, sequence, true, false) {
      committed = Some(sequence);
    }
  }
  held.remove(&DEPARTED);
  Object {
    foreign: false,
    cohorts,
    held,
    committed,
  }
}

/// The history `seed` draws, with the nemesis on when `omit` is.
fn history(seed: u64, omit: bool) -> History {
  let mut draw = Draw::new(seed);
  let f = draw.within(1, 2);
  let quorum = Quorum {
    f: u32::try_from(f).unwrap(),
  };
  let survivor_count = draw.within(f + 1, 2 * f + 2);
  let survivors: Vec<HostId> = (1..=survivor_count).map(HostId).collect();
  let mut objects = BTreeMap::new();
  for object_index in 0..draw.within(1, MAX_OBJECTS) {
    objects.insert(
      object_id(object_index),
      object(&mut draw, quorum, &survivors),
    );
  }
  // The asker's own live objects, which some survivors hold under the asker's authority.
  let first_foreign = u64::try_from(objects.len()).unwrap();
  for index in 0..draw.within(0, 2) {
    let mut foreign = object(&mut draw, quorum, &survivors);
    foreign.foreign = true;
    foreign.committed = None;
    objects.insert(object_id(first_foreign + index), foreign);
  }
  let gate_open = draw.chance(3, 4);
  let mut omitted: BTreeMap<HostId, BTreeSet<ObjectId>> = BTreeMap::new();
  let mut moved = BTreeSet::new();
  if omit {
    for (object, state) in objects.iter().filter(|(_, state)| !state.foreign) {
      for host in state.held.keys() {
        if draw.chance(1, 2) {
          omitted.entry(*host).or_default().insert(*object);
          if draw.chance(1, 2) {
            moved.insert((*host, *object));
          }
        }
      }
    }
  }
  let asked = survivors.len().saturating_sub(1).max(1);
  let schedule = (0..SCHEDULE_STEPS)
    .map(|_| usize::try_from(draw.next() % u64::try_from(asked).unwrap()).unwrap())
    .collect();
  History {
    quorum,
    survivors,
    objects,
    omitted,
    moved,
    page_entries: usize::try_from(draw.within(1, MAX_PAGE_ENTRIES)).unwrap(),
    gate_open,
    schedule,
  }
}

/// What the successor holds of the round.
#[derive(Default)]
struct Round {
  /// Objects whose names the successor knows with no report yet (the directory's, shape (ii)).
  names: BTreeSet<ObjectId>,
  /// Per object: the members that reported it, with their records.
  reports: BTreeMap<ObjectId, BTreeMap<HostId, Accepted>>,
  /// Per object: the members that answered for it by name, holding nothing.
  named_nothing: BTreeMap<ObjectId, BTreeSet<HostId>>,
  /// Per object: the members whose report of it came by name, not in a page.
  named_reports: BTreeMap<ObjectId, BTreeSet<HostId>>,
  /// Per member: how far its pages have come, and whether its answer is complete.
  listed_upto: BTreeMap<HostId, usize>,
  complete: BTreeSet<HostId>,
  adopted: BTreeMap<ObjectId, Accepted>,
  lost: BTreeSet<ObjectId>,
}

impl Round {
  /// Every object the successor knows of: reported, or named by the directory.
  fn learned(&self) -> Vec<ObjectId> {
    let mut learned: BTreeSet<ObjectId> = self.reports.keys().copied().collect();
    learned.extend(self.names.iter().copied());
    learned.into_iter().collect()
  }
}

/// Whether `host` lists `object` in its pages (or, the successor, learns it from itself) under `listing`. A routed copy's
/// authority was moved to the asker by the install (`reconcile_held_authority`); a copy whose routing was lost before the
/// install keeps the retired host's chain authority; one whose routing was lost after it (`moved`) keeps the asker's.
fn lists(history: &History, listing: Listing, host: HostId, object: ObjectId) -> bool {
  let state = &history.objects[&object];
  let Some(copy) = state.held.get(&host) else {
    return false;
  };
  if state.foreign {
    // The asker's own object: under the asker's authority, routed to the asker as a live owner.
    return listing == Listing::ChainOrAsker;
  }
  let routed = history
    .omitted
    .get(&host)
    .is_none_or(|left_out| !left_out.contains(&object));
  // An earlier owner's copy outside the chain stays under that owner; a chain copy is moved to the asker by a routed
  // install, or was moved before its routing was lost.
  let moved = copy.in_chain && (routed || history.moved.contains(&(host, object)));
  let in_chain_now = copy.in_chain && !moved;
  match listing {
    Listing::Routing | Listing::Directory => routed,
    Listing::RoutingOrChain => routed || in_chain_now,
    Listing::ChainOrAsker | Listing::MovedFrom => in_chain_now || moved,
  }
}

/// The successor's run of `history` under `rule` and `listing`: the records it adopted and the objects it found lost.
fn run(history: &History, rule: CountingRule, listing: Listing) -> Round {
  let mut round = Round::default();
  if listing == Listing::Directory {
    // The directory names every object the retired host had placed: an object with a committed record.
    round.names = history
      .objects
      .iter()
      .filter(|(_, state)| !state.foreign && state.committed.is_some())
      .map(|(object, _)| *object)
      .collect();
  }
  let asked: Vec<HostId> = history
    .survivors
    .iter()
    .copied()
    .filter(|host| *host != SUCCESSOR)
    .collect();
  let mut gate_open = history.gate_open;
  if gate_open {
    open_gate(history, &mut round, listing);
  }
  settle(history, &mut round, rule, gate_open);
  for step in history.schedule.iter().copied() {
    let Some(host) = asked.get(step).copied() else {
      continue;
    };
    answer(history, &mut round, host, rule, listing);
    if gate_open {
      answer_self(history, &mut round, rule);
    }
    settle(history, &mut round, rule, gate_open);
  }
  // The lease gate opens once the retired owner's lease has lapsed, as it does in time; then every survivor answers in
  // turn until nothing is left to answer.
  if !gate_open {
    gate_open = true;
    open_gate(history, &mut round, listing);
  }
  let rounds = asked.len() * (history.objects.len() + 2) * 2;
  for host in asked.iter().copied().cycle().take(rounds) {
    answer(history, &mut round, host, rule, listing);
    answer_self(history, &mut round, rule);
    settle(history, &mut round, rule, gate_open);
  }
  if asked.is_empty() {
    answer_self(history, &mut round, rule);
    settle(history, &mut round, rule, gate_open);
  }
  round
}

/// Records `host`'s answer by name for `object`: its record, or that it holds nothing.
fn named_answer(history: &History, round: &mut Round, host: HostId, object: ObjectId) {
  match history.objects[&object].held.get(&host) {
    Some(copy) => {
      round
        .reports
        .entry(object)
        .or_default()
        .insert(host, copy.record.clone());
      round.named_reports.entry(object).or_default().insert(host);
    }
    None => {
      round.named_nothing.entry(object).or_default().insert(host);
    }
  }
}

/// Whether `host` has answered for `object`: reported it or named it as held nothing of.
fn answered(round: &Round, host: HostId, object: ObjectId) -> bool {
  round
    .reports
    .get(&object)
    .is_some_and(|reports| reports.contains_key(&host))
    || round
      .named_nothing
      .get(&object)
      .is_some_and(|hosts| hosts.contains(&host))
}

/// Under A-251's rule the successor answers itself by name, as it answers any member's question: for each learned
/// object it did not learn from its own copies (its routing may have lost one), a direct look-up of what it holds.
fn answer_self(history: &History, round: &mut Round, rule: CountingRule) {
  if rule != CountingRule::NamedAnswer {
    return;
  }
  for object in round.learned() {
    if !round.adopted.contains_key(&object) && !answered(round, SUCCESSOR, object) {
      named_answer(history, round, SUCCESSOR, object);
    }
  }
}

/// The successor learns its own copies once it may promise them, those its listing gives it.
fn open_gate(history: &History, round: &mut Round, listing: Listing) {
  for (object, state) in &history.objects {
    if let Some(own) = state.held.get(&SUCCESSOR)
      && !round.adopted.contains_key(object)
      && lists(history, listing, SUCCESSOR, *object)
    {
      round
        .reports
        .entry(*object)
        .or_default()
        .insert(SUCCESSOR, own.record.clone());
    }
  }
}

/// `host` answers the successor's next question: its next page while its pages are incomplete, else (under A-251's rule)
/// every learned object it has not answered for, by name.
fn answer(
  history: &History,
  round: &mut Round,
  host: HostId,
  rule: CountingRule,
  listing: Listing,
) {
  if !round.complete.contains(&host) {
    let listed: Vec<ObjectId> = history
      .objects
      .keys()
      .copied()
      .filter(|object| lists(history, listing, host, *object))
      .collect();
    let from = round.listed_upto.get(&host).copied().unwrap_or(0);
    let to = (from + history.page_entries).min(listed.len());
    for object in &listed[from..to] {
      let held = history.objects[object].held[&host].record.clone();
      round.reports.entry(*object).or_default().insert(host, held);
    }
    round.listed_upto.insert(host, to);
    if to == listed.len() {
      round.complete.insert(host);
    }
    return;
  }
  if rule == CountingRule::NamedAnswer {
    for object in round.learned() {
      // By name, a direct look-up: no routing filter, so the nemesis cannot hide a copy here.
      if !answered(round, host, object) {
        named_answer(history, round, host, object);
      }
    }
  }
}

/// Adopts every learned object the rule calls ready, with the newest record reported, and notes the lost ones.
fn settle(history: &History, round: &mut Round, rule: CountingRule, gate_open: bool) {
  let empty = BTreeSet::new();
  let members: BTreeSet<HostId> = history.survivors.iter().copied().collect();
  for (object, reports) in &round.reports {
    if round.adopted.contains_key(object) || round.lost.contains(object) {
      continue;
    }
    let heard = Heard {
      reports,
      complete: &round.complete,
      named_nothing: round.named_nothing.get(object).unwrap_or(&empty),
      local: gate_open.then_some(SUCCESSOR),
    };
    let cohorts = &history.objects[object].cohorts;
    match readiness(
      cohorts,
      |host| members.contains(&host),
      history.quorum,
      &heard,
      rule,
    ) {
      Readiness::Ready => {
        let newest = reports
          .values()
          .cloned()
          .reduce(|best, next| if next.newer_than(&best) { next } else { best })
          .unwrap();
        round.adopted.insert(*object, newest);
      }
      Readiness::Lost { .. } => {
        round.lost.insert(*object);
      }
      Readiness::Waiting => {}
    }
  }
}

/// NoLoss's violations: each object whose adopted record is older than its newest committed one, and each object with a
/// committed record the round never adopted (it would confirm without it).
fn losses(history: &History, round: &Round) -> Vec<(ObjectId, Option<u64>, u64)> {
  history
    .objects
    .iter()
    .filter_map(|(object, state)| {
      let committed = state.committed?;
      match round.adopted.get(object) {
        Some(adopted) if adopted.sequence >= committed => None,
        Some(adopted) => Some((*object, Some(adopted.sequence), committed)),
        None => Some((*object, None, committed)),
      }
    })
    .collect()
}

/// The asker's own objects the round learned: a page or answer of the retired host's round that reached one raised its
/// fence to the round's epoch, which can refuse the asker's own writes, and adopting it re-commits it at that epoch.
fn foreign_learned(history: &History, round: &Round) -> usize {
  round
    .reports
    .keys()
    .filter(|object| history.objects[*object].foreign)
    .count()
}

/// The minimal history the nemesis finds, by hand: f = 1, one object on the cohort {retired, a, b}; record 1 on a,
/// record 2 committed on the retired host and b; b's page leaves the object out and ends complete. Do: a answers, then
/// b. Expect: today's rule adopts record 1 over the committed record 2; A-251's rule waits for b's answer by name and
/// adopts record 2.
#[test]
fn today_s_rule_adopts_an_older_record_when_a_holder_leaves_a_copy_out() {
  let (a, b) = (HostId(2), HostId(3));
  let object = object_id(0);
  let copy = |sequence| Copy {
    record: record(sequence),
    in_chain: true,
  };
  let history = History {
    quorum: Quorum { f: 1 },
    survivors: vec![SUCCESSOR, a, b],
    objects: BTreeMap::from([(
      object,
      Object {
        foreign: false,
        cohorts: vec![vec![DEPARTED, a, b]],
        held: BTreeMap::from([(a, copy(1)), (b, copy(2))]),
        committed: Some(2),
      },
    )]),
    omitted: BTreeMap::from([(b, BTreeSet::from([object]))]),
    moved: BTreeSet::new(),
    page_entries: 1,
    gate_open: true,
    schedule: vec![0, 1],
  };
  let today = run(&history, CountingRule::CompleteAnswer, Listing::Routing);
  assert_eq!(
    losses(&history, &today),
    vec![(object, Some(1), 2)],
    "today's rule loses the committed record"
  );
  let named = run(&history, CountingRule::NamedAnswer, Listing::Routing);
  assert!(losses(&history, &named).is_empty());
  assert_eq!(named.adopted[&object].sequence, 2);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(256),
    ..ProptestConfig::default()
  }))]

  /// A-251's rule with shape (i) as one rule (a copy's authority's line, or its moved-from owner's), on every generated
  /// history with the widened nemesis on: no adopted record is older than a committed one, every object with a committed
  /// record ends adopted, and none of the asker's own objects is learned in the round.
  #[test]
  fn the_named_rule_with_moved_from_listing_holds_no_loss_on_every_history(seed in any::<u64>()) {
    let history = history(seed, true);
    let round = run(&history, CountingRule::NamedAnswer, Listing::MovedFrom);
    prop_assert!(losses(&history, &round).is_empty(), "{:?}", losses(&history, &round));
    prop_assert_eq!(foreign_learned(&history, &round), 0);
  }
}

/// What one history's runs lost and showed, summed over histories by the measuring test.
#[derive(Clone, Copy, Default)]
struct Tally {
  today_without: usize,
  today_with: usize,
  named_routing_older: usize,
  named_routing_unlearned: usize,
  routing_or_chain: usize,
  chain_or_asker_lost: usize,
  chain_or_asker_foreign: usize,
  moved_from_lost: usize,
  moved_from_foreign: usize,
  directory_lost: usize,
  directory_foreign: usize,
  named_mattered: usize,
  named_nothing: usize,
  creates: usize,
}

impl Tally {
  fn add(self, other: Tally) -> Tally {
    Tally {
      today_without: self.today_without + other.today_without,
      today_with: self.today_with + other.today_with,
      named_routing_older: self.named_routing_older + other.named_routing_older,
      named_routing_unlearned: self.named_routing_unlearned + other.named_routing_unlearned,
      routing_or_chain: self.routing_or_chain + other.routing_or_chain,
      chain_or_asker_lost: self.chain_or_asker_lost + other.chain_or_asker_lost,
      chain_or_asker_foreign: self.chain_or_asker_foreign + other.chain_or_asker_foreign,
      moved_from_lost: self.moved_from_lost + other.moved_from_lost,
      moved_from_foreign: self.moved_from_foreign + other.moved_from_foreign,
      directory_lost: self.directory_lost + other.directory_lost,
      directory_foreign: self.directory_foreign + other.directory_foreign,
      named_mattered: self.named_mattered + other.named_mattered,
      named_nothing: self.named_nothing + other.named_nothing,
      creates: self.creates + other.creates,
    }
  }
}

/// History `seed`'s runs: today's rule with the nemesis off and on, and A-251's with each listing.
fn measure(seed: u64) -> Tally {
  let quiet = history(seed, false);
  let hostile = history(seed, true);
  let named_routing = run(&hostile, CountingRule::NamedAnswer, Listing::Routing);
  let named_losses = losses(&hostile, &named_routing);
  // Only a holder that answered by name reported the adopted record: the pages alone would have missed it.
  let named_mattered = named_routing
    .adopted
    .iter()
    .filter(|(object, adopted)| {
      let by_name = named_routing.named_reports.get(*object);
      named_routing.reports[*object]
        .iter()
        .filter(|(_, reported)| *reported == *adopted)
        .all(|(host, _)| by_name.is_some_and(|hosts| hosts.contains(host)))
    })
    .count();
  let named = |listing| run(&hostile, CountingRule::NamedAnswer, listing);
  let (chain_or_asker, moved_from, directory) = (
    named(Listing::ChainOrAsker),
    named(Listing::MovedFrom),
    named(Listing::Directory),
  );
  Tally {
    today_without: losses(
      &quiet,
      &run(&quiet, CountingRule::CompleteAnswer, Listing::Routing),
    )
    .len(),
    today_with: losses(
      &hostile,
      &run(&hostile, CountingRule::CompleteAnswer, Listing::Routing),
    )
    .len(),
    named_routing_older: named_losses
      .iter()
      .filter(|(_, adopted, _)| adopted.is_some())
      .count(),
    named_routing_unlearned: named_losses
      .iter()
      .filter(|(_, adopted, _)| adopted.is_none())
      .count(),
    routing_or_chain: losses(&hostile, &named(Listing::RoutingOrChain)).len(),
    chain_or_asker_lost: losses(&hostile, &chain_or_asker).len(),
    chain_or_asker_foreign: foreign_learned(&hostile, &chain_or_asker),
    moved_from_lost: losses(&hostile, &moved_from).len(),
    moved_from_foreign: foreign_learned(&hostile, &moved_from),
    directory_lost: losses(&hostile, &directory).len(),
    directory_foreign: foreign_learned(&hostile, &directory),
    named_mattered,
    // The by-name question still carries the promises of holders that list nothing of an object.
    named_nothing: moved_from.named_nothing.values().map(BTreeSet::len).sum(),
    creates: hostile
      .objects
      .values()
      .filter(|state| !state.foreign)
      .count(),
  }
}

/// What each rule and listing loses over the same generated histories, with the nemesis off and on, and what each listing
/// costs per write. Do: walk the histories under today's rule with today's listing, and under A-251's rule with each
/// listing. Expect:
/// - today's rule loses nothing without omissions and loses records with them;
/// - A-251's rule with today's listing adopts nothing older, but leaves objects every holder left out unlearned;
/// - routing plus the chain path (A-251 (b)'s first shape (i)) still loses objects whose authority an install had moved
///   to the asker before their routing was lost;
/// - "the chain or the asker" as one rule loses nothing but learns the asker's own objects in the round;
/// - "the line of the authority or of the owner it moved from" (shape (i) as one rule) loses nothing and learns none;
/// - shape (ii) loses nothing and learns none, at one more committed record per object created;
/// - and the by-name question did work: under today's listing some adoptions came only from a named answer, and under the
///   chosen rule holders answered by name for objects they hold nothing of.
#[test]
#[allow(clippy::cognitive_complexity)]
fn the_nemesis_breaks_today_s_rule_and_each_shape_is_measured() {
  let tally = (0..SEARCH_HISTORIES)
    .map(measure)
    .fold(Tally::default(), Tally::add);
  eprintln!(
    "{SEARCH_HISTORIES} histories, {} objects: today's rule lost {} without omissions and {} with them; A-251's rule \
     with today's listing adopted {} older records and left {} committed objects unlearned; routing plus the chain \
     path lost {}; chain-or-asker lost {} and learned {} of the asker's own objects; moved-from lost {} and learned {}; \
     the directory lost {} and learned {}, at {} extra committed records (moved-from: 0); {} adoptions under today's \
     listing came only from a named answer; under moved-from, {} named answers of holding nothing",
    tally.creates,
    tally.today_without,
    tally.today_with,
    tally.named_routing_older,
    tally.named_routing_unlearned,
    tally.routing_or_chain,
    tally.chain_or_asker_lost,
    tally.chain_or_asker_foreign,
    tally.moved_from_lost,
    tally.moved_from_foreign,
    tally.directory_lost,
    tally.directory_foreign,
    tally.creates,
    tally.named_mattered,
    tally.named_nothing,
  );
  assert_eq!(
    tally.today_without, 0,
    "today's rule is safe when no page leaves a copy out"
  );
  assert!(
    tally.today_with > 0,
    "the nemesis found today's rule's loss"
  );
  assert_eq!(
    tally.named_routing_older, 0,
    "the named rule adopts nothing older"
  );
  assert!(
    tally.named_routing_unlearned > 0,
    "the widened nemesis leaves objects nobody listed"
  );
  assert!(
    tally.routing_or_chain > 0,
    "routing plus the chain path misses copies moved before routing was lost"
  );
  assert_eq!(tally.chain_or_asker_lost, 0);
  assert!(
    tally.chain_or_asker_foreign > 0,
    "chain-or-asker learns the asker's own objects"
  );
  assert_eq!(
    (tally.moved_from_lost, tally.moved_from_foreign),
    (0, 0),
    "the chosen rule"
  );
  assert_eq!((tally.directory_lost, tally.directory_foreign), (0, 0));
  assert!(
    tally.named_mattered > 0,
    "the by-name question carried the newest record"
  );
  assert!(
    tally.named_nothing > 0,
    "holders answer by name for what they do not hold"
  );
}
