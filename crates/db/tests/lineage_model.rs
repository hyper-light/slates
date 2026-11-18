//! A-309 (A-272 (ii)): where a retired owner's objects go, and when a holder may let its copy go, over generated
//! histories of a region driven through the real [`RegionalConfiguration`]. The executable counterpart of
//! `docs/wip/models/Lineage.tla`, which is read as text only (CLAUDE.md §2 item 13).
//!
//! **The model.** A region of members, each object created by one of them, and a ground truth the configuration never
//! sees: which host owns each object, how many times it has been adopted, and the copies the members hold, each with
//! the adoption count it was written at. An owner's write reaches a subset of its placement's candidates (both cohorts
//! while a change is in flight) that places it, as the record plane does; the record plane's re-ship of a head to a
//! new cohort is not modelled, so a copy can stay on a neighbourhood the owner has left. A member dies by the council's
//! takeover; a member joins by admission; an owner reports its neighbourhood settled once every record it owns is
//! placed on its current cohort. A survivor runs its share of a kept retirement as a round does: it asks the members of
//! the retirement's recovery neighbourhoods and itself, learns every copy among them whose line falls to it
//! ([`falls_to`]'s rule, by the copy's owner or the owner its authority moved from), judges each by [`readiness`] with
//! every asked member having answered (the counting rule is `crates/db/tests/takeover_model.rs`'s subject, not this
//! one's; past tolerance it waits for every surviving member of the object's cohorts, settled and current, A-309),
//! adopts the newest reported record under its own placement, and confirms its share only when every object it learned
//! was adopted (A-272). A holder resolves its copies at each install as `takeover::resolve_held_objects` does:
//! a line that names a successor moves the copy's routing to it; a line that ends at a completed takeover lets the copy
//! go when the holder took part in that takeover ([`RegionalConfiguration::settled_for`]); and a successor's
//! confirmation lets it go when the holder, asked in that round, is outside the successor's placement
//! (`stale_after_confirmation`). A holder sometimes misses installs and installs the newest configuration later, as a
//! lagging node does.
//!
//! **The oracle** is the property A-272 and A-309 exist for: a holder lets a copy go only if its object was adopted
//! after that copy was written (an adoption made a newer record, so the copy is stale). And every structure stays
//! bounded: the retirements by one per member, the completion records by one per member or the ones kept retirements
//! still need.
//!
//! **What it found while A-309 was built**, each a history that broke the oracle until the rule after it:
//! - A takeover with no survivor, or one carried into such a takeover, read settled (the minimal history below).
//! - An object no survivor of its cohorts was ranked for was learned by no round; every survivor confirmed, and the
//!   completed takeover read its line settled while a member still held its only copy. Fixed by ranking it among all
//!   the retirement's survivors (`RegionalConfiguration::successor`).
//! - The bound dropped a retirement another kept retirement carried; the carrier's objects were ranked anew, a survivor
//!   confirmed without the object it had first been ranked for, and a holder that had routed the copy to it read the
//!   confirmation as an adoption. Fixed by dropping the carriers with it (`bound_retirements`).
//! - A holder a neighbourhood the owner had left still reached held a copy no asked member held; the successor confirmed
//!   without learning it, and the holder read the confirmation as an adoption. Fixed by letting a copy go only for a
//!   holder the round asked.
//!
//! - `a_takeover_with_no_survivor_lets_no_holder_reclaim_an_object_nobody_adopted`: the minimal history by hand.
//! - `holders_let_go_only_of_copies_an_adoption_made_stale` (proptest): the oracle and the bounds on every history.
//! - `the_generated_histories_reach_every_verdict`: the non-vacuity counts over a fixed set of seeds.

// Test harness: an unwrap or indexing here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::indexing_slicing,
  clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use slates_db::register::{
  Accepted, HostEpoch, HostId, Lineage, ObjectId, Quorum, RegionalConfiguration,
};
use slates_db::takeover::{CountingRule, Heard, Readiness, falls_to, readiness};

/// Shape: the most events a generated history runs.
const MAX_EVENTS: u64 = 48;
/// Shape: the most objects each initial member creates.
const OBJECTS_PER_MEMBER: u64 = 3;
/// Shape: the generated histories the property test walks.
const SEARCH_HISTORIES: u32 = 256;
/// Shape: the seeds the non-vacuity test walks.
const NON_VACUITY_SEEDS: u64 = 400;
/// Shape: the first id an admitted host takes, above every initial member's.
const FIRST_JOINER: u64 = 100;

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

  /// One of `items`, `None` when there are none.
  fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
    if items.is_empty() {
      return None;
    }
    let index = usize::try_from(self.within(0, u64::try_from(items.len()).unwrap() - 1)).unwrap();
    items.get(index).copied()
  }
}

/// A copy a member holds, as its holder keeps it.
#[derive(Clone, Debug)]
struct HeldCopy {
  /// The record's sequence.
  sequence: u64,
  /// The object's adoption count when this copy was written: the oracle's clock.
  adoptions: u64,
  /// The owner the holder routes it to (its acceptor's authority).
  owner: HostId,
  /// The retired owner an install moved its authority from (A-251's `moved_from`).
  moved_from: Option<HostId>,
  /// The retired host whose takeover this copy is in, as the lease gate records it (`departed_owners`).
  departed: Option<HostId>,
}

/// One object's ground truth.
#[derive(Clone, Debug)]
struct Object {
  /// The host that owns it now: its creator, or the last member that adopted it.
  owner: HostId,
  /// How many times it was adopted.
  adoptions: u64,
  /// The next record's sequence.
  next_sequence: u64,
  /// The copies each member holds.
  copies: BTreeMap<HostId, HeldCopy>,
}

/// What a history did, for the non-vacuity counts.
#[derive(Clone, Copy, Debug, Default)]
struct Counts {
  /// Copies let go because their line read settled.
  reclaimed_settled: u64,
  /// Copies let go because their successor confirmed with the holder outside its placement.
  reclaimed_confirmed: u64,
  /// Copies kept because their line read unresolved or lost.
  kept_unknown: u64,
  /// Copies kept because their line ended at a completed takeover that never asked their holder.
  kept_unasked: u64,
  /// Adoptions made by rounds.
  adopted: u64,
  /// Of those, adoptions past tolerance: a cohort short of its recovery quorum, every surviving holder heard (A-309).
  recovered_past_tolerance: u64,
  /// Takeovers completed into a record.
  completed: u64,
}

/// A violation of the oracle: the holder, the object, the copy's adoption count and the object's.
type Violation = (HostId, ObjectId, u64, u64);

/// What a successor's confirmation says of a holder's copy routed to it.
enum Confirmed {
  /// The successor confirmed and the holder was asked in its round, outside its placement: the copy is stale.
  Stale,
  /// The successor confirmed and the holder is outside its placement, but the round never asked the holder: kept.
  Unasked,
  /// Nothing to judge: no confirmation, or the holder is within the successor's placement.
  Not,
}

/// A region, its configuration and its ground truth.
struct World {
  regional: RegionalConfiguration,
  scatter: u64,
  objects: BTreeMap<ObjectId, Object>,
  next_host: u64,
  counts: Counts,
  violations: Vec<Violation>,
  /// The members that install configurations late.
  lagging: BTreeSet<HostId>,
}

impl World {
  /// A region formed on `members` at `f`, each member creating some objects placed on its cohort.
  fn formed(draw: &mut Draw, members: u64, f: u32, scatter: u64) -> World {
    let regional = RegionalConfiguration::formed(
      (1..=members).map(HostId).collect(),
      Quorum { f },
      BTreeMap::new(),
      scatter,
      false,
    );
    let mut world = World {
      regional,
      scatter,
      objects: BTreeMap::new(),
      next_host: FIRST_JOINER,
      counts: Counts::default(),
      violations: Vec::new(),
      lagging: BTreeSet::new(),
    };
    for creator in 1..=members {
      for local in 0..draw.within(1, OBJECTS_PER_MEMBER) {
        let object = ObjectId::new(HostId(creator), local);
        world.objects.insert(
          object,
          Object {
            owner: HostId(creator),
            adoptions: 0,
            next_sequence: 0,
            copies: BTreeMap::new(),
          },
        );
        world.write(draw, object);
      }
    }
    world
  }

  /// The owner of `object` writes its next record to a subset of its placement's candidates that places it, when it
  /// is a member; a write that cannot place (too few members left in a cohort) still leaves its copies.
  fn write(&mut self, draw: &mut Draw, object: ObjectId) {
    let owner = self.objects[&object].owner;
    let Some(configuration) = self.regional.configuration_for(owner) else {
      return;
    };
    let placement = configuration.place(object);
    let members: Vec<HostId> = placement
      .candidates
      .iter()
      .copied()
      .filter(|host| self.regional.members.contains(host))
      .collect();
    let mut acked: Vec<HostId> = vec![owner];
    for host in &members {
      if *host != owner && draw.chance(2, 3) {
        acked.push(*host);
      }
    }
    for host in &members {
      if placement.placed_with(&acked, configuration.quorum) {
        break;
      }
      if !acked.contains(host) {
        acked.push(*host);
      }
    }
    let state = self.objects.get_mut(&object).unwrap();
    let sequence = state.next_sequence;
    state.next_sequence += 1;
    for host in acked {
      state.copies.insert(
        host,
        HeldCopy {
          sequence,
          adoptions: state.adoptions,
          owner,
          moved_from: None,
          departed: None,
        },
      );
    }
  }

  /// The council takes `host` over: its copies are gone with it.
  fn kill(&mut self, host: HostId) {
    if self.regional.take_over(host, self.scatter).is_none() {
      return;
    }
    for state in self.objects.values_mut() {
      state.copies.remove(&host);
    }
    self.lagging.remove(&host);
  }

  /// A fresh member joins, holding nothing.
  fn admit(&mut self) {
    let host = HostId(self.next_host);
    self.next_host += 1;
    self.regional.admit(host, None, self.scatter);
  }

  /// `owner` reports its neighbourhood settled if every record it owns is placed on its current cohort.
  fn settle(&mut self, owner: HostId) {
    let Some(configuration) = self.regional.configuration_for(owner) else {
      return;
    };
    let generation = configuration.generation;
    if !self.regional.settles(owner, generation) {
      return;
    }
    let placed = self
      .objects
      .iter()
      .filter(|(_, state)| state.owner == owner)
      .all(|(object, state)| {
        let newest = state.next_sequence.saturating_sub(1);
        let acked: Vec<HostId> = state
          .copies
          .iter()
          .filter(|(_, copy)| copy.sequence == newest && copy.adoptions == state.adoptions)
          .map(|(host, _)| *host)
          .collect();
        configuration.placed_on_current(*object, &acked)
      });
    if placed {
      self.regional.settle(owner, generation);
    }
  }

  /// Whether a copy falls to `successor` in `departed`'s round: its owner's line or its moved-from owner's ends there.
  fn falls(&self, object: ObjectId, copy: &HeldCopy, departed: HostId, successor: HostId) -> bool {
    falls_to(&self.regional, copy.owner, object, departed, successor)
      || copy
        .moved_from
        .is_some_and(|from| falls_to(&self.regional, from, object, departed, successor))
  }

  /// `successor` runs its share of `departed`'s takeover to its end: every object some member's copy of which falls
  /// to it is judged with every member having answered; a ready one is adopted with the newest record reported and
  /// placed under the successor; the share is confirmed when every object it learned was adopted.
  fn round(&mut self, draw: &mut Draw, departed: HostId, successor: HostId) {
    if !self.regional.owes_confirmation(departed, successor) {
      return;
    }
    // The round asks the members among the hosts that may hold the retired host's records, and the successor answers
    // itself (`takeover::begin_round`, `answer_own_copies_by_name`).
    let members: BTreeSet<HostId> = self.regional.members.iter().copied().collect();
    let mut asked: BTreeSet<HostId> = self
      .regional
      .recovery_hosts(departed)
      .into_iter()
      .filter(|host| members.contains(host))
      .collect();
    asked.insert(successor);
    let learned: Vec<ObjectId> = self
      .objects
      .iter()
      .filter(|(object, state)| {
        state.copies.iter().any(|(holder, copy)| {
          asked.contains(holder) && self.falls(**object, copy, departed, successor)
        })
      })
      .map(|(object, _)| *object)
      .collect();
    let mut every_adopted = true;
    for object in learned {
      let state = &self.objects[&object];
      let mut reports: BTreeMap<HostId, Accepted> = BTreeMap::new();
      let mut named_nothing: BTreeSet<HostId> = BTreeSet::new();
      for member in &asked {
        match state.copies.get(member) {
          Some(copy) if self.falls(object, copy, departed, successor) => {
            reports.insert(
              *member,
              Accepted {
                sequence: copy.sequence,
                epoch: HostEpoch(copy.adoptions + 1),
                value: Vec::new(),
              },
            );
          }
          // A copy under another line is refused by name: no promise.
          Some(_) => {}
          None => {
            named_nothing.insert(*member);
          }
        }
      }
      let heard = Heard {
        reports: &reports,
        complete: &asked,
        named_nothing: &named_nothing,
        local: Some(successor),
      };
      let cohorts = self.regional.recovery_cohorts(departed, object);
      let current = self.regional.current_cohorts(departed, object);
      let verdict = readiness(
        &cohorts,
        &current,
        |host| members.contains(&host),
        self.regional.quorum,
        &heard,
        CountingRule::NamedAnswer,
      );
      match verdict {
        Readiness::Ready => self.adopt(draw, object, successor, &reports),
        Readiness::ReadyPastTolerance { .. } => {
          self.counts.recovered_past_tolerance += 1;
          self.adopt(draw, object, successor, &reports);
        }
        Readiness::Waiting => every_adopted = false,
      }
    }
    if every_adopted {
      self.regional.confirm(departed, successor);
      if !self.regional.retired.contains_key(&departed)
        && self.regional.completed.contains_key(&departed)
      {
        self.counts.completed += 1;
      }
    }
  }

  /// `successor` adopts `object` with the newest record `reports` hold and places it under its own placement.
  fn adopt(
    &mut self,
    draw: &mut Draw,
    object: ObjectId,
    successor: HostId,
    reports: &BTreeMap<HostId, Accepted>,
  ) {
    let Some(newest) = reports.values().map(|record| record.sequence).max() else {
      return;
    };
    let state = self.objects.get_mut(&object).unwrap();
    state.owner = successor;
    state.adoptions += 1;
    state.next_sequence = state.next_sequence.max(newest + 1);
    self.counts.adopted += 1;
    // The adoption re-commits the newest record under the successor: a write at its own placement.
    let sequence = newest;
    let Some(configuration) = self.regional.configuration_for(successor) else {
      return;
    };
    let placement = configuration.place(object);
    let mut acked: Vec<HostId> = vec![successor];
    for host in placement
      .candidates
      .iter()
      .copied()
      .filter(|host| self.regional.members.contains(host) && *host != successor)
    {
      if draw.chance(2, 3) || !placement.placed_with(&acked, configuration.quorum) {
        acked.push(host);
      }
    }
    let state = self.objects.get_mut(&object).unwrap();
    for host in acked {
      state.copies.insert(
        host,
        HeldCopy {
          sequence,
          adoptions: state.adoptions,
          owner: successor,
          moved_from: None,
          departed: None,
        },
      );
    }
  }

  /// Every member resolves its copies against the configuration it installs (a lagging one skips this install).
  fn install(&mut self, everyone: bool) {
    let holders: Vec<HostId> = self
      .regional
      .members
      .iter()
      .copied()
      .filter(|host| everyone || !self.lagging.contains(host))
      .collect();
    for holder in holders {
      self.resolve(holder);
    }
  }

  /// `holder` resolves every copy it holds, as `takeover::resolve_held_objects` does, and the oracle judges each copy
  /// it lets go.
  fn resolve(&mut self, holder: HostId) {
    let objects: Vec<ObjectId> = self
      .objects
      .iter()
      .filter(|(_, state)| state.copies.contains_key(&holder))
      .map(|(object, _)| *object)
      .collect();
    for object in objects {
      let copy = self.objects[&object].copies[&holder].clone();
      let owner = copy.owner;
      if self.regional.members.contains(&owner) && !self.regional.retired.contains_key(&owner) {
        match self.stale_after_confirmation(holder, object, &copy) {
          Confirmed::Stale => {
            self.counts.reclaimed_confirmed += 1;
            self.reclaim(holder, object);
          }
          Confirmed::Unasked => self.counts.kept_unasked += 1,
          Confirmed::Not => {}
        }
        continue;
      }
      match self.regional.lineage(owner, object) {
        Lineage::Successor {
          departed,
          successor,
        } => {
          // `reconcile_held_authority` records the owner an install moved the authority from only when that owner's
          // retirement is kept.
          let retired_owner = self.regional.retired.contains_key(&owner);
          let entry = self
            .objects
            .get_mut(&object)
            .unwrap()
            .copies
            .get_mut(&holder)
            .unwrap();
          if entry.owner != successor && retired_owner {
            entry.moved_from = Some(entry.owner);
          }
          entry.owner = successor;
          entry.departed = Some(departed);
        }
        Lineage::Settled { takeover } => {
          if self.regional.settled_for(takeover, holder) {
            self.counts.reclaimed_settled += 1;
            self.reclaim(holder, object);
          } else {
            self.counts.kept_unasked += 1;
          }
        }
        Lineage::Lost | Lineage::Unresolved => self.counts.kept_unknown += 1,
      }
    }
  }

  /// Whether the holder's copy, routed to the live member `copy.owner` after a takeover, is stale: that member's share
  /// of the copy's takeover is confirmed (in the kept retirement or its completion record), the holder was asked in that
  /// round (a survivor of it), and the holder is outside that member's placement of the object.
  fn stale_after_confirmation(
    &self,
    holder: HostId,
    object: ObjectId,
    copy: &HeldCopy,
  ) -> Confirmed {
    let Some(departed) = copy.departed else {
      return Confirmed::Not;
    };
    let outside = copy.owner != holder
      && self
        .regional
        .configuration_for(copy.owner)
        .is_some_and(|placement| !placement.place(object).candidates.contains(&holder));
    let (confirmed, asked) = match (
      self.regional.retired.get(&departed),
      self.regional.completed.get(&departed),
    ) {
      (Some(retirement), _) => (
        retirement.confirmed.contains(&copy.owner),
        retirement.survivors.contains(&holder),
      ),
      (None, Some(record)) => (
        record.confirmed.contains(&copy.owner),
        record.confirmed.contains(&holder),
      ),
      (None, None) => (false, false),
    };
    match (confirmed && outside, asked) {
      (true, true) => Confirmed::Stale,
      (true, false) => Confirmed::Unasked,
      (false, _) => Confirmed::Not,
    }
  }

  /// `holder` lets its copy of `object` go; the oracle records it unless an adoption came after the copy was written.
  fn reclaim(&mut self, holder: HostId, object: ObjectId) {
    let state = self.objects.get_mut(&object).unwrap();
    if let Some(copy) = state.copies.remove(&holder)
      && state.adoptions <= copy.adoptions
    {
      self
        .violations
        .push((holder, object, copy.adoptions, state.adoptions));
    }
  }

  /// The configuration's structures stay within their bounds (banned item 8).
  fn bounded(&self) -> bool {
    let needed: BTreeSet<HostId> = self
      .regional
      .retired
      .values()
      .flat_map(|retirement| {
        retirement
          .survivors
          .iter()
          .copied()
          .filter(|survivor| !retirement.confirmed.contains(survivor))
      })
      .filter(|host| self.regional.completed.contains_key(host))
      .collect();
    self.regional.retired.len() <= self.regional.retirement_bound()
      && self.regional.completed.len() <= self.regional.completed_bound().max(needed.len())
  }
}

impl World {
  /// Every (retired host, survivor) pair whose share is still owed.
  fn owed(&self) -> Vec<(HostId, HostId)> {
    self
      .regional
      .retired
      .iter()
      .flat_map(|(departed, retirement)| {
        retirement
          .survivors
          .iter()
          .map(move |survivor| (*departed, *survivor))
      })
      .filter(|(departed, survivor)| self.regional.owes_confirmation(*departed, *survivor))
      .collect()
  }

  /// The objects whose owner is a member, so it can write them.
  fn writable(&self) -> Vec<ObjectId> {
    self
      .objects
      .iter()
      .filter(|(_, state)| self.regional.members.contains(&state.owner))
      .map(|(object, _)| *object)
      .collect()
  }

  /// One drawn event: an admission, a takeover (never the last member), a write, a settle report, a round of an owed
  /// share, or a member starting or ending to lag.
  fn event(&mut self, draw: &mut Draw) {
    let alive: Vec<HostId> = self.regional.members.clone();
    match draw.within(0, 9) {
      0 | 1 => self.admit(),
      2 if alive.len() > 1 => {
        if let Some(host) = draw.pick(&alive) {
          self.kill(host);
        }
      }
      3 | 4 => {
        if let Some(object) = draw.pick(&self.writable()) {
          self.write(draw, object);
        }
      }
      5 => {
        if let Some(owner) = draw.pick(&alive) {
          self.settle(owner);
        }
      }
      6 | 7 => {
        if let Some((departed, successor)) = draw.pick(&self.owed()) {
          self.round(draw, departed, successor);
        }
      }
      8 => {
        if let Some(host) = draw.pick(&alive) {
          self.toggle_lagging(host);
        }
      }
      _ => {}
    }
  }

  /// `host` starts lagging, or catches up.
  fn toggle_lagging(&mut self, host: HostId) {
    if !self.lagging.remove(&host) {
      self.lagging.insert(host);
    }
  }

  /// Quiescence: every member installs, and every share still owed is run, until none is owed or the bound on passes
  /// is spent; whether the bounds held throughout.
  fn quiesce(&mut self, draw: &mut Draw) -> bool {
    let mut bounded = true;
    self.lagging.clear();
    self.install(true);
    for _ in 0..self.regional.retirement_bound() + 4 {
      let owed = self.owed();
      if owed.is_empty() {
        break;
      }
      for (departed, successor) in owed {
        self.round(draw, departed, successor);
        self.install(true);
        bounded &= self.bounded();
      }
    }
    bounded
  }
}

/// Runs the history `seed` draws: its events, a check of the bounds after each, and every owing share run at the end.
fn run(seed: u64) -> (Counts, Vec<Violation>, bool) {
  let mut draw = Draw::new(seed);
  let f = u32::try_from(draw.within(1, 2)).unwrap();
  let floor = 2 * u64::from(f) + 1;
  // A region formed on one node (a bootstrap, A-234) or wider than its scatter.
  let members = if draw.chance(1, 4) {
    1
  } else {
    draw.within(2, floor + 2)
  };
  let scatter = draw.within(floor, floor + 2);
  let mut world = World::formed(&mut draw, members, f, scatter);
  let mut bounded = true;
  for _ in 0..draw.within(8, MAX_EVENTS) {
    world.event(&mut draw);
    world.install(false);
    bounded &= world.bounded();
  }
  bounded &= world.quiesce(&mut draw);
  (world.counts, world.violations, bounded)
}

/// The minimal history, by hand: a region formed on host 1 alone admits hosts 2 and 3; host 1 writes an object, which
/// reaches both through its joint placement (its settled neighbourhood still itself alone, A-234); host 1 is taken over
/// before it reports its grown neighbourhood placed; both holders install. Expect neither to let its copy go: nobody
/// adopted the object. Before A-309 the retirement, with no survivor, was dropped at once, its line read settled, and
/// both holders let the object's only copies go.
#[test]
fn a_takeover_with_no_survivor_lets_no_holder_reclaim_an_object_nobody_adopted() {
  let mut draw = Draw::new(7);
  let mut world = World::formed(&mut draw, 1, 1, 3);
  world.admit();
  world.admit();
  let object = ObjectId::new(HostId(1), 0);
  world.write(&mut draw, object);
  let holders: Vec<HostId> = world.objects[&object]
    .copies
    .keys()
    .copied()
    .filter(|host| *host != HostId(1))
    .collect();
  assert!(!holders.is_empty(), "the joint write reached the joiners");
  world.kill(HostId(1));
  world.install(true);
  assert!(world.violations.is_empty(), "{:?}", world.violations);
  for holder in holders {
    assert!(
      world.objects[&object].copies.contains_key(&holder),
      "{holder:?} keeps its copy of an object nobody adopted"
    );
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
    cases: slates_test_seeds::cases(SEARCH_HISTORIES),
    ..ProptestConfig::default()
  }))]

  /// A-272, A-309: on every generated history a holder lets a copy go only if its object was adopted after the copy was
  /// written, and the retirements and completion records stay within their bounds after every event.
  #[test]
  fn holders_let_go_only_of_copies_an_adoption_made_stale(seed in any::<u64>()) {
    let (_, violations, bounded) = run(seed);
    prop_assert!(violations.is_empty(), "{:?}", violations);
    prop_assert!(bounded, "a structure outgrew its bound");
  }
}

/// Non-vacuity: over a fixed set of seeds the histories reach every verdict the oracle judges, so a silently dead path
/// can never pass as a held property. Do: run the seeds and add up what they did. Expect copies let go by a settled line
/// and by a confirmed successor, copies kept on an unresolved or lost line and on a completed takeover that never asked
/// their holder, adoptions (some past tolerance, A-309) and completed takeovers, each at least once, and no violation.
#[test]
fn the_generated_histories_reach_every_verdict() {
  let mut total = Counts::default();
  for seed in 0..NON_VACUITY_SEEDS {
    let (counts, violations, bounded) = run(seed);
    assert!(violations.is_empty(), "seed {seed}: {violations:?}");
    assert!(bounded, "seed {seed}: a structure outgrew its bound");
    total.reclaimed_settled += counts.reclaimed_settled;
    total.reclaimed_confirmed += counts.reclaimed_confirmed;
    total.kept_unknown += counts.kept_unknown;
    total.kept_unasked += counts.kept_unasked;
    total.adopted += counts.adopted;
    total.recovered_past_tolerance += counts.recovered_past_tolerance;
    total.completed += counts.completed;
  }
  println!("lineage model over {NON_VACUITY_SEEDS} seeds: {total:?}");
  assert!(total.reclaimed_settled > 0, "{total:?}");
  assert!(total.reclaimed_confirmed > 0, "{total:?}");
  assert!(total.kept_unknown > 0, "{total:?}");
  assert!(total.kept_unasked > 0, "{total:?}");
  assert!(total.adopted > 0, "{total:?}");
  assert!(total.recovered_past_tolerance > 0, "{total:?}");
  assert!(total.completed > 0, "{total:?}");
}
