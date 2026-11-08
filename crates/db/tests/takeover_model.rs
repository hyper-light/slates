//! A-251 (a): the host-takeover counting rule (§4.8 "Promotion and takeover", `slates_db::takeover::readiness`) over a
//! model of a paged, multi-object takeover with an omission nemesis.
//!
//! The model: a retired host's objects, each recovered through one or two cohorts (the retired host and some
//! survivors), with a history of writes. A write goes to one cohort (an owner in the recovery chain commits under its
//! own placement) or to all of them (a joint write), is acknowledged by some of its cohorts' hosts, and commits when each
//! of its cohorts holds it as `Quorum::committed_within` says. Each survivor holds the newest record it
//! acknowledged. The successor learns its own copies when its lease gate is open (from the start, or once the scheduled answers are
//! in, as the gate opens when the retired owner's lease has lapsed), and asks every other survivor for
//! pages of what it holds, a few entries a page, answered in an order the history chooses. After every answer it adopts
//! each object the rule calls ready, with the newest record reported; an adoption is final.
//!
//! The nemesis leaves an object out of some holders' pages, as `takeover::list_page`'s routing filter did for a holder
//! whose routing disagreed with the successor's, while the page still ends complete. It is held to A-251 (b)'s scope:
//! every object a survivor holds is listed by at least one survivor, or learned by the successor itself. An object
//! every holder leaves out is never learned, under either rule. That case is outside this model and is recorded in GAPS.
//!
//! The oracle is the design's NoLoss: an adopted record is at least as new as every committed record of its object.
//! Under A-251's rule the successor also asks, by name, each survivor whose pages are complete for every object it
//! learned and that survivor did not list. A survivor answers with its record, or that it holds nothing. Every object
//! with a committed record must then end adopted.
//!
//! - `today_s_rule_adopts_an_older_record_when_a_holder_leaves_a_copy_out`: the minimal history, red on today's rule.
//! - `the_named_rule_holds_no_loss_on_every_history` (proptest): A-251's rule never loses a committed record and
//!   adopts every object with one.
//! - `the_nemesis_breaks_today_s_rule_and_only_with_omissions`: over generated histories, today's rule loses
//!   records only when the nemesis omits, and it does lose some (non-vacuity of the nemesis and of the oracle).

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
/// Shape: the most writes an object's history holds.
const MAX_WRITES: u64 = 3;
/// Shape: the most entries a page carries, so a holder's answer spans several pages.
const MAX_PAGE_ENTRIES: u64 = 3;
/// Shape: the answers a history's schedule delivers in its chosen order before the rest are delivered in turn.
const SCHEDULE_STEPS: u64 = 40;
/// Shape: the generated histories the counting test walks.
const SEARCH_HISTORIES: u64 = 2_000;

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

/// One object of the retired host's.
#[derive(Clone, Debug)]
struct Object {
  cohorts: Vec<Vec<HostId>>,
  /// The newest record each host holds.
  held: BTreeMap<HostId, Accepted>,
  /// The newest committed record's sequence, if any committed.
  committed: Option<u64>,
}

/// A takeover's history: the region, the objects, what the nemesis leaves out, and the order answers arrive in.
#[derive(Clone, Debug)]
struct History {
  quorum: Quorum,
  survivors: Vec<HostId>,
  objects: BTreeMap<ObjectId, Object>,
  /// The objects each holder leaves out of its pages.
  omitted: BTreeMap<HostId, BTreeSet<ObjectId>>,
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
    let cohorts: Vec<Vec<HostId>> = (0..draw.within(1, 2))
      .map(|_| {
        // A cohort is the retired host and up to 2f of the survivors (fewer for a forming region's).
        let size = draw.within(1, (2 * f).min(survivor_count));
        let mut pool = survivors.clone();
        let mut cohort = vec![DEPARTED];
        for _ in 0..size {
          let pick =
            usize::try_from(draw.within(0, u64::try_from(pool.len()).unwrap() - 1)).unwrap();
          cohort.push(pool.remove(pick));
        }
        cohort.sort_unstable_by_key(|host| host.0);
        cohort
      })
      .collect();
    let mut held: BTreeMap<HostId, Accepted> = BTreeMap::new();
    let mut committed = None;
    for sequence in 1..=draw.within(1, MAX_WRITES) {
      // A write goes to one cohort, as an owner in the recovery chain commits under its own placement, or to every
      // cohort at once, as a joint write does.
      let targets: Vec<&Vec<HostId>> = if cohorts.len() > 1 && draw.chance(2, 3) {
        let pick =
          usize::try_from(draw.within(0, u64::try_from(cohorts.len()).unwrap() - 1)).unwrap();
        vec![&cohorts[pick]]
      } else {
        cohorts.iter().collect()
      };
      let hosts: BTreeSet<HostId> = targets.iter().copied().flatten().copied().collect();
      let acked: BTreeSet<HostId> = hosts
        .iter()
        .copied()
        .filter(|_| draw.chance(2, 3))
        .collect();
      for host in &acked {
        held.insert(*host, record(sequence));
      }
      let commits = targets.iter().all(|cohort| {
        let count = cohort.iter().filter(|host| acked.contains(host)).count();
        quorum.committed_within(count, cohort.len())
      });
      if commits {
        committed = Some(sequence);
      }
    }
    held.remove(&DEPARTED);
    objects.insert(
      object_id(object_index),
      Object {
        cohorts,
        held,
        committed,
      },
    );
  }
  let gate_open = draw.chance(3, 4);
  let mut omitted: BTreeMap<HostId, BTreeSet<ObjectId>> = BTreeMap::new();
  if omit {
    for (object, state) in &objects {
      let holders: Vec<HostId> = state
        .held
        .keys()
        .copied()
        .filter(|host| *host != SUCCESSOR)
        .collect();
      let mut left_out: Vec<HostId> = holders
        .iter()
        .copied()
        .filter(|_| draw.chance(1, 2))
        .collect();
      // A-251 (b)'s scope: some survivor lists each object a survivor holds, unless the successor learns it itself.
      let learned_locally = gate_open && state.held.contains_key(&SUCCESSOR);
      if !learned_locally && !holders.is_empty() && left_out.len() == holders.len() {
        left_out.pop();
      }
      for host in left_out {
        omitted.entry(host).or_default().insert(*object);
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
    page_entries: usize::try_from(draw.within(1, MAX_PAGE_ENTRIES)).unwrap(),
    gate_open,
    schedule,
  }
}

/// What the successor holds of the round.
#[derive(Default)]
struct Round {
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

/// The successor's run of `history` under `rule`: the records it adopted and the objects it found lost.
fn run(history: &History, rule: CountingRule) -> Round {
  let mut round = Round::default();
  let asked: Vec<HostId> = history
    .survivors
    .iter()
    .copied()
    .filter(|host| *host != SUCCESSOR)
    .collect();
  let mut gate_open = history.gate_open;
  if gate_open {
    open_gate(history, &mut round);
  }
  settle(history, &mut round, rule, gate_open);
  for step in history.schedule.iter().copied() {
    let Some(host) = asked.get(step).copied() else {
      continue;
    };
    answer(history, &mut round, host, rule);
    settle(history, &mut round, rule, gate_open);
  }
  // The lease gate opens once the retired owner's lease has lapsed, as it does in time; then every survivor answers in
  // turn until nothing is left to answer.
  if !gate_open {
    gate_open = true;
    open_gate(history, &mut round);
  }
  let rounds = asked.len() * (history.objects.len() + 2) * 2;
  for host in asked.iter().copied().cycle().take(rounds) {
    answer(history, &mut round, host, rule);
    settle(history, &mut round, rule, gate_open);
  }
  round
}

/// The successor learns its own copies once it may promise them.
fn open_gate(history: &History, round: &mut Round) {
  for (object, state) in &history.objects {
    if let Some(own) = state.held.get(&SUCCESSOR)
      && !round.adopted.contains_key(object)
    {
      round
        .reports
        .entry(*object)
        .or_default()
        .insert(SUCCESSOR, own.clone());
    }
  }
}

/// `host` answers the successor's next question: its next page while its pages are incomplete, else (under A-251's rule)
/// every learned object it neither listed nor named yet, by name.
fn answer(history: &History, round: &mut Round, host: HostId, rule: CountingRule) {
  if !round.complete.contains(&host) {
    let omitted = history.omitted.get(&host);
    let listed: Vec<ObjectId> = history
      .objects
      .iter()
      .filter(|(object, state)| {
        state.held.contains_key(&host) && omitted.is_none_or(|left_out| !left_out.contains(object))
      })
      .map(|(object, _)| *object)
      .collect();
    let from = round.listed_upto.get(&host).copied().unwrap_or(0);
    let to = (from + history.page_entries).min(listed.len());
    for object in &listed[from..to] {
      let held = history.objects[object].held[&host].clone();
      round.reports.entry(*object).or_default().insert(host, held);
    }
    round.listed_upto.insert(host, to);
    if to == listed.len() {
      round.complete.insert(host);
    }
    return;
  }
  if rule == CountingRule::NamedAnswer {
    let learned: Vec<ObjectId> = round.reports.keys().copied().collect();
    for object in learned {
      let reported = round.reports[&object].contains_key(&host);
      let named = round
        .named_nothing
        .get(&object)
        .is_some_and(|hosts| hosts.contains(&host));
      if reported || named {
        continue;
      }
      // By name, a direct look-up: no routing filter, so the nemesis cannot hide a copy here.
      match history.objects[&object].held.get(&host) {
        Some(held) => {
          round
            .reports
            .entry(object)
            .or_default()
            .insert(host, held.clone());
          round.named_reports.entry(object).or_default().insert(host);
        }
        None => {
          round.named_nothing.entry(object).or_default().insert(host);
        }
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
      Readiness::Lost => {
        round.lost.insert(*object);
      }
      Readiness::Waiting => {}
    }
  }
}

/// The objects whose adopted record is older than their newest committed one: NoLoss's violations.
fn losses(history: &History, round: &Round) -> Vec<(ObjectId, u64, u64)> {
  round
    .adopted
    .iter()
    .filter_map(|(object, adopted)| {
      let committed = history.objects[object].committed?;
      (adopted.sequence < committed).then_some((*object, adopted.sequence, committed))
    })
    .collect()
}

/// The minimal history the nemesis finds, by hand: f = 1, one object on the cohort {retired, a, b}; record 1 on a,
/// record 2 committed on the retired host and b; b's page leaves the object out and ends complete. Do: a answers, then
/// b. Expect: today's rule adopts record 1 over the committed record 2; A-251's rule waits for b's answer by name and
/// adopts record 2.
#[test]
fn today_s_rule_adopts_an_older_record_when_a_holder_leaves_a_copy_out() {
  let (a, b) = (HostId(2), HostId(3));
  let object = object_id(0);
  let history = History {
    quorum: Quorum { f: 1 },
    survivors: vec![SUCCESSOR, a, b],
    objects: BTreeMap::from([(
      object,
      Object {
        cohorts: vec![vec![DEPARTED, a, b]],
        held: BTreeMap::from([(a, record(1)), (b, record(2))]),
        committed: Some(2),
      },
    )]),
    omitted: BTreeMap::from([(b, BTreeSet::from([object]))]),
    page_entries: 1,
    gate_open: true,
    schedule: vec![0, 1],
  };
  let today = run(&history, CountingRule::CompleteAnswer);
  assert_eq!(
    losses(&history, &today),
    vec![(object, 1, 2)],
    "today's rule loses the committed record"
  );
  let named = run(&history, CountingRule::NamedAnswer);
  assert!(losses(&history, &named).is_empty());
  assert_eq!(named.adopted[&object].sequence, 2);
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(1024),
    ..ProptestConfig::default()
  }))]

  /// A-251's rule, on every generated history with the nemesis on: no adopted record is older than a committed one,
  /// and every object with a committed record ends adopted.
  #[test]
  fn the_named_rule_holds_no_loss_on_every_history(seed in any::<u64>()) {
    let history = history(seed, true);
    let round = run(&history, CountingRule::NamedAnswer);
    prop_assert!(losses(&history, &round).is_empty(), "{:?}", losses(&history, &round));
    for (object, state) in &history.objects {
      if state.committed.is_some() {
        prop_assert!(round.adopted.contains_key(object), "object {object:?} with a committed record never adopted");
      }
    }
  }
}

/// The nemesis is what breaks today's rule, and it does. Do: walk the same generated histories with the nemesis off and
/// on, under both rules. Expect: today's rule loses no record without omissions and loses some with them; A-251's loses
/// none either way; and some histories had an omitted holder whose named answer was the newest record (the by-name
/// question did work: the adopted record came only from holders that named it).
#[test]
fn the_nemesis_breaks_today_s_rule_and_only_with_omissions() {
  let mut today_without = 0usize;
  let mut today_with = 0usize;
  let mut named_with = 0usize;
  let mut named_mattered = 0usize;
  for seed in 0..SEARCH_HISTORIES {
    let quiet = history(seed, false);
    today_without += losses(&quiet, &run(&quiet, CountingRule::CompleteAnswer)).len();
    let hostile = history(seed, true);
    today_with += losses(&hostile, &run(&hostile, CountingRule::CompleteAnswer)).len();
    let named = run(&hostile, CountingRule::NamedAnswer);
    named_with += losses(&hostile, &named).len();
    named_mattered += named
      .adopted
      .iter()
      .filter(|(object, adopted)| {
        // Only a holder that answered by name reported the adopted record: the pages alone would have missed it.
        let by_name = named.named_reports.get(*object);
        named.reports[*object]
          .iter()
          .filter(|(_, reported)| *reported == *adopted)
          .all(|(host, _)| by_name.is_some_and(|hosts| hosts.contains(host)))
      })
      .count();
  }
  eprintln!(
    "{SEARCH_HISTORIES} histories: today's rule lost {today_without} records without omissions and {today_with} with \
     them; A-251's lost {named_with}; {named_mattered} adoptions came from a named answer"
  );
  assert_eq!(
    today_without, 0,
    "today's rule is safe when no page leaves a copy out"
  );
  assert!(today_with > 0, "the nemesis found today's rule's loss");
  assert_eq!(named_with, 0);
  assert!(
    named_mattered > 0,
    "the by-name question carried the newest record"
  );
}
