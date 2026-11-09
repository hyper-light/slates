//! The counting rule of a host takeover's phase one (§4.8 "Promotion and takeover"; A-251): when the successor of a
//! retired host may adopt one of its objects. Pure, so the rule a successor runs is the rule a model checks
//! (`crates/db/tests/takeover_model.rs`).
//!
//! An object is adopted once every one of its recovery cohorts has its recovery quorum of members that promised it
//! ([`Quorum::recovery`]: `cohort − f`, so the promises meet every set of `f + 1` a record committed on). It is lost
//! when a cohort has fewer members left than that quorum: more of the cohort failed than the region tolerates.
//!
//! What counts as a member's promise is the question A-251 is about ([`CountingRule`]). A member that listed the object
//! has promised it and reported its highest record. Until A-251 (b), a member whose paged answer was complete also
//! counted for every object it did not list: listing nothing meant holding nothing. A holder whose page leaves out a
//! copy it holds (its routing disagreed with the successor's, or it could not promise the copy) then read as holding
//! nothing, and the successor could adopt an older record over a committed newer one. The model finds that history
//! under [`CountingRule::CompleteAnswer`], which is kept for the model alone. The successor counts under
//! [`CountingRule::NamedAnswer`], and a holder answers by name through [`answer_by_name`].

use std::collections::{BTreeMap, BTreeSet};

use crate::register::{
  Accepted, Acceptor, Authority, HostEpoch, HostId, HostPrepare, HostPromise, Lineage,
  OBJECT_BYTES, ObjectId, Prepare, Quorum, RegionalConfiguration, RegisterError,
};

/// Which answers count as a member's promise of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountingRule {
  /// The rule before A-251 (b), kept so the model can show what it loses: a member promised an object when it listed
  /// it, or when its paged answer is complete.
  CompleteAnswer,
  /// A-251 (b): a member promised an object when it listed it, or answered for it by name (holding nothing of it).
  /// A whole answer's silence counts for nothing.
  NamedAnswer,
}

/// What a successor has heard of one object of the retired host's.
#[derive(Clone, Copy, Debug)]
pub struct Heard<'a> {
  /// The members that listed the object, each with the highest record it holds.
  pub reports: &'a BTreeMap<HostId, Accepted>,
  /// The members whose paged answer is complete.
  pub complete: &'a BTreeSet<HostId>,
  /// The members that answered for the object by name, holding nothing of it.
  pub named_nothing: &'a BTreeSet<HostId>,
  /// The successor itself when it may promise its own copies (its lease gate open, §4.8 "Leases and reads"); `None`
  /// while it may not. Counted as a promise of its own only under [`CountingRule::CompleteAnswer`]: under
  /// [`CountingRule::NamedAnswer`] the successor promises as any member does, by its report or its named "nothing",
  /// which it gives itself only while its gate is open.
  pub local: Option<HostId>,
}

/// Where one object of a takeover stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
  /// Every cohort has its recovery quorum of promises: adopt the newest record reported.
  Ready,
  /// A cohort has not promised enough yet.
  Waiting,
  /// A cohort has fewer members left than its recovery quorum: the object cannot be recovered. Carries what was
  /// judged, so a caller can say why without recomputing it.
  Lost {
    /// The short cohort's index in the cohorts [`readiness`] was given.
    cohort: usize,
    /// Its recovery quorum.
    needed: usize,
    /// How many of its hosts are still members.
    members: usize,
  },
}

impl Heard<'_> {
  /// Whether `host` has promised the object under `rule`.
  fn promised(&self, host: HostId, rule: CountingRule) -> bool {
    if self.reports.contains_key(&host) {
      return true;
    }
    match rule {
      CountingRule::CompleteAnswer => self.local == Some(host) || self.complete.contains(&host),
      // The successor too: its own copies are learned through its routing, which can lose one as any holder's can, so
      // its silence is no promise; it answers itself by name (`named_nothing`) as it asks any member.
      CountingRule::NamedAnswer => self.named_nothing.contains(&host),
    }
  }
}

/// Where an object recovered through `cohorts` stands, its members being the hosts `is_member` admits, under `quorum`
/// and `rule`, given what was `heard` of it. A cohort's hosts that are no longer members (the retired host among them)
/// count toward its size, not its promises.
pub fn readiness(
  cohorts: &[Vec<HostId>],
  is_member: impl Fn(HostId) -> bool,
  quorum: Quorum,
  heard: &Heard<'_>,
  rule: CountingRule,
) -> Readiness {
  let mut ready = true;
  for (index, cohort) in cohorts.iter().enumerate() {
    let needed = quorum.recovery(cohort.len());
    let members: Vec<HostId> = cohort
      .iter()
      .copied()
      .filter(|host| is_member(*host))
      .collect();
    if members.len() < needed {
      return Readiness::Lost {
        cohort: index,
        needed,
        members: members.len(),
      };
    }
    let promised = members
      .iter()
      .filter(|host| heard.promised(**host, rule))
      .count();
    ready &= promised >= needed;
  }
  if ready {
    Readiness::Ready
  } else {
    Readiness::Waiting
  }
}

/// Promises `object` to the successor `prepare` names: installs its authority under the round's generation and runs the
/// prepare through the object's acceptor, raising its fence to the round's epoch. The highest record held, `None` when
/// the acceptor holds nothing of it, or the refusal (a fence already above the epoch, or a generation newer than the
/// round's). A page's listing and a by-name answer promise alike. `from_retired`: whether the copy's current owner is a
/// retired host, so the install records it as the owner the authority moved from ([`Acceptor::move_authority`]).
pub fn promise_object(
  acceptor: &mut Acceptor,
  object: ObjectId,
  prepare: &HostPrepare,
  from_retired: bool,
) -> Result<Option<Accepted>, RegisterError> {
  acceptor.move_authority(
    Authority {
      generation: prepare.generation,
      owner: prepare.owner,
    },
    from_retired,
  )?;
  let promise = acceptor.prepare(&Prepare {
    owner: prepare.owner,
    object,
    epoch: prepare.epoch,
    generation: prepare.generation,
  })?;
  Ok(promise.highest)
}

/// Where a holder keeps its acceptors, one per object, as [`answer_by_name`] reaches them: the holder's own map, or a
/// map that tracks what changed (a held image written incrementally). The answer only looks one up and promises
/// through it.
pub trait AcceptorStore {
  /// The acceptor of `object`, if this holder has one.
  fn acceptor_mut(&mut self, object: &ObjectId) -> Option<&mut Acceptor>;
}

impl AcceptorStore for BTreeMap<ObjectId, Acceptor> {
  fn acceptor_mut(&mut self, object: &ObjectId) -> Option<&mut Acceptor> {
    self.get_mut(object)
  }
}

/// Whether a holder's copy of `object`, its acceptor serving `serving`, falls to `asker` in `departed`'s takeover, by
/// the committed configuration alone (A-251 (b), shape (i)): the object's line from the owner the copy was written under
/// ends in `departed`'s round with `asker` its successor ([`RegionalConfiguration::lineage`]). Routing plays no part, and
/// the authority is kept with the copy, so a warm restart keeps it. A copy under a live member's authority, or one
/// whose line ends in another retired host's round (an unconfirmed successor's own object), does not fall here.
pub fn falls_to(
  regional: &RegionalConfiguration,
  serving: HostId,
  object: ObjectId,
  departed: HostId,
  asker: HostId,
) -> bool {
  matches!(
    regional.lineage(serving, object),
    Lineage::Successor { departed: round, successor } if round == departed && successor == asker
  )
}

/// Whether a holder's copy of `object`, kept by `acceptor`, falls to `asker` in `departed`'s takeover (A-251 (b), shape
/// (i) as one rule): the line of its authority's owner ends there ([`falls_to`]), or the line of the owner an install
/// moved that authority from does ([`Acceptor::moved_from`]). The second case is the common one: an install moves a
/// retired owner's copies to its successor (`reconcile_held_authority`), and a holder restored warm then routes the copy
/// to the successor as a live owner, so neither routing nor the authority alone names the retired host. Routing plays no
/// part. The asker's own objects never fall here: no install moved them from the round's line.
pub fn copy_falls_to(
  regional: &RegionalConfiguration,
  acceptor: &Acceptor,
  object: ObjectId,
  departed: HostId,
  asker: HostId,
) -> bool {
  falls_to(regional, acceptor.owner(), object, departed, asker)
    || acceptor
      .moved_from()
      .is_some_and(|from| falls_to(regional, from, object, departed, asker))
}

/// A holder's answer to a by-name question, and what it could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedAnswer {
  /// The answer: each named object it holds among `entries`, each it holds nothing of in `nothing`, a fence above the
  /// epoch in `fenced`, and the copies it could not promise for any other reason in `unpromised`.
  pub promise: HostPromise,
  /// Named objects it refused to answer for this asker: the configuration names another successor, or the copy's line
  /// ([`copy_falls_to`]) puts it in another round or with a live owner. Such an object stays unanswered.
  pub refused: u32,
}

/// `holder`'s answer to the by-name question `prepare` (A-251 (b)), each object answered alone from `acceptors` (the
/// holder's own records) with no routing in the way: a warm-restarted holder keeps its acceptors and loses its routing
/// (`docs/bugs/2026-11-07-a-holder-restarted-warm-lost-its-copies-routing.md`), so routing cannot be what decides.
/// Two checks stand in its place, both from what the holder has itself:
/// - `regional`, the committed configuration, names the asker the object's successor;
/// - the object's acceptor, when there is one, is in this round by its authority's line or the line of the owner an
///   install moved it from ([`copy_falls_to`]). A question never installs the asker over a live owner's object, its own
///   included, nor over another round's.
///
/// A held copy is promised as a page promises it ([`promise_object`]) and reported; no acceptor means the holder holds
/// nothing of the object, which it says for that object alone. The answer stops before `budget` bytes, at least one
/// object answered; what it leaves unanswered is asked again.
pub fn answer_by_name(
  holder: HostId,
  prepare: &HostPrepare,
  regional: &RegionalConfiguration,
  acceptors: &mut impl AcceptorStore,
  budget: usize,
) -> NamedAnswer {
  let mut answer = NamedAnswer {
    promise: HostPromise {
      holder,
      departed: prepare.departed,
      epoch: prepare.epoch,
      generation: prepare.generation,
      entries: Vec::new(),
      next: None,
      fenced: None,
      unpromised: 0,
      nothing: Vec::new(),
    },
    refused: 0,
  };
  let mut used = crate::register::HOST_PROMISE_HEADER_BYTES
    .saturating_add(crate::register::OBJECT_LIST_HEADER_BYTES);
  for object in &prepare.named {
    if regional.successor(prepare.departed, *object) != Some(prepare.owner) {
      answer.refused = answer.refused.saturating_add(1);
      continue;
    }
    let Some(acceptor) = acceptors.acceptor_mut(object) else {
      if used.saturating_add(OBJECT_BYTES) > budget && answered(&answer.promise) {
        break;
      }
      used = used.saturating_add(OBJECT_BYTES);
      answer.promise.nothing.push(*object);
      continue;
    };
    if !copy_falls_to(regional, acceptor, *object, prepare.departed, prepare.owner) {
      answer.refused = answer.refused.saturating_add(1);
      continue;
    }
    let from_retired = regional.retired.contains_key(&acceptor.owner());
    match promise_object(acceptor, *object, prepare, from_retired) {
      Ok(Some(highest)) => {
        let size = HostPromise::entry_bytes(&highest);
        if used.saturating_add(size) > budget && answered(&answer.promise) {
          break;
        }
        used = used.saturating_add(size);
        answer.promise.entries.push((*object, highest));
      }
      Ok(None) => {
        if used.saturating_add(OBJECT_BYTES) > budget && answered(&answer.promise) {
          break;
        }
        used = used.saturating_add(OBJECT_BYTES);
        answer.promise.nothing.push(*object);
      }
      Err(RegisterError::StaleEpoch { current }) => {
        let fence = HostEpoch(current);
        answer.promise.fenced = Some(answer.promise.fenced.map_or(fence, |seen| seen.max(fence)));
      }
      Err(_) => answer.promise.unpromised = answer.promise.unpromised.saturating_add(1),
    }
  }
  answer
}

/// Whether `promise` has answered any object yet, so a budget never leaves a question with no answer at all.
fn answered(promise: &HostPromise) -> bool {
  !promise.entries.is_empty() || !promise.nothing.is_empty()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::register::HostEpoch;

  fn record(sequence: u64) -> Accepted {
    Accepted {
      sequence,
      epoch: HostEpoch(1),
      value: vec![u8::try_from(sequence).unwrap_or(u8::MAX)],
    }
  }

  /// The two rules differ only on a complete answer that did not list the object: today's counts it, A-251's needs it
  /// named. Do: a cohort of three at f = 1 with the retired host, one member listing a record and the other complete
  /// without listing it. Expect: ready under `CompleteAnswer`, waiting under `NamedAnswer` until the other names it.
  #[test]
  fn a_complete_answer_counts_for_an_unlisted_object_only_under_todays_rule() {
    let (departed, a, b) = (HostId(9), HostId(1), HostId(2));
    let cohorts = vec![vec![departed, a, b]];
    let is_member = |host: HostId| host != departed;
    let reports = BTreeMap::from([(a, record(1))]);
    let complete = BTreeSet::from([b]);
    let unnamed = BTreeSet::new();
    let named_b = BTreeSet::from([b]);
    let heard = Heard {
      reports: &reports,
      complete: &complete,
      named_nothing: &unnamed,
      local: None,
    };
    let quorum = Quorum { f: 1 };
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &heard,
        CountingRule::CompleteAnswer
      ),
      Readiness::Ready
    );
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &heard,
        CountingRule::NamedAnswer
      ),
      Readiness::Waiting
    );
    let named = Heard {
      named_nothing: &named_b,
      ..heard
    };
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &named,
        CountingRule::NamedAnswer
      ),
      Readiness::Ready
    );
  }

  /// A cohort with fewer members left than its recovery quorum is lost under either rule, and every cohort must have
  /// its quorum. Do: two cohorts at f = 1, the second left with a single member of three; then two whole cohorts, only
  /// one of which has promised. Expect: lost, naming the second cohort, its quorum of two and its one member; then
  /// waiting.
  #[test]
  fn every_cohort_needs_its_quorum_and_a_short_one_is_lost() {
    let (departed, gone, a, b, c) = (HostId(9), HostId(8), HostId(1), HostId(2), HostId(3));
    let quorum = Quorum { f: 1 };
    let reports = BTreeMap::from([(a, record(1)), (b, record(1))]);
    let empty = BTreeSet::new();
    let heard = Heard {
      reports: &reports,
      complete: &empty,
      named_nothing: &empty,
      local: None,
    };
    let is_member = |host: HostId| host != departed && host != gone;
    let short = vec![vec![departed, a, b], vec![departed, gone, c]];
    for rule in [CountingRule::CompleteAnswer, CountingRule::NamedAnswer] {
      assert_eq!(
        readiness(&short, is_member, quorum, &heard, rule),
        Readiness::Lost {
          cohort: 1,
          needed: 2,
          members: 1
        }
      );
    }
    let whole = vec![vec![departed, a, b], vec![departed, b, c]];
    for rule in [CountingRule::CompleteAnswer, CountingRule::NamedAnswer] {
      assert_eq!(
        readiness(&whole, is_member, quorum, &heard, rule),
        Readiness::Waiting
      );
    }
  }

  /// A region of five at f = 1 whose host 1 has retired: the configuration, a successor of some of its objects (the
  /// asker), objects it is the successor of, and one it is not.
  fn retired_region() -> (RegionalConfiguration, HostId, Vec<ObjectId>, ObjectId) {
    let departed = HostId(1);
    let mut regional = RegionalConfiguration::formed(
      (1..=5).map(HostId).collect(),
      Quorum { f: 1 },
      BTreeMap::new(),
      3,
      false,
    );
    regional.take_over(departed, 3).unwrap();
    let objects: Vec<ObjectId> = (0..64)
      .map(|local| ObjectId::new(departed, local))
      .collect();
    let asker = regional.successor(departed, objects[0]).unwrap();
    let theirs: Vec<ObjectId> = objects
      .iter()
      .copied()
      .filter(|object| regional.successor(departed, *object) == Some(asker))
      .take(4)
      .collect();
    let other = objects
      .iter()
      .copied()
      .find(|object| regional.successor(departed, *object) != Some(asker))
      .unwrap();
    (regional, asker, theirs, other)
  }

  /// An acceptor on `holder` serving `owner` at generation `generation`, holding `sequence` of `object` when given.
  fn acceptor(
    holder: HostId,
    owner: HostId,
    generation: u64,
    held: Option<(ObjectId, u64)>,
  ) -> Acceptor {
    let mut acceptor = Acceptor::new(holder, Authority { generation, owner });
    if let Some((object, sequence)) = held {
      acceptor
        .accept(&crate::register::Record {
          owner,
          object,
          sequence,
          epoch: HostEpoch(1),
          generation,
          value: b"held".to_vec(),
        })
        .unwrap();
    }
    acceptor
  }

  /// Shape (i)'s rule, by the configuration alone: a copy under the retired host's authority falls to its successor in
  /// its round; a copy under a live member's does not; and once that successor itself retires before confirming, the
  /// object's line ends in the successor's own round, not the first one, so it is never listed in two rounds. Do: retire
  /// host 1, then the successor of one of its objects before it confirms. Expect: before the second retirement the
  /// object falls to that successor in host 1's round; after it, to the next successor in the successor's round, and in
  /// host 1's round to no one, not even that next successor; a live member's copy falls nowhere.
  #[test]
  fn a_copy_falls_to_one_round_by_its_authority_alone() {
    let (mut regional, asker, theirs, _) = retired_region();
    let departed = HostId(1);
    let object = theirs[0];
    assert!(falls_to(&regional, departed, object, departed, asker));
    let live = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != asker)
      .unwrap();
    assert!(
      !falls_to(&regional, live, object, departed, asker),
      "a live member's copy"
    );
    regional.take_over(asker, 3).unwrap();
    let Lineage::Successor {
      departed: round,
      successor,
    } = regional.lineage(departed, object)
    else {
      panic!(
        "the object still has a successor: {:?}",
        regional.lineage(departed, object)
      );
    };
    assert_eq!(
      round, asker,
      "the line now ends in the retired successor's round"
    );
    assert!(!falls_to(&regional, departed, object, departed, asker));
    assert!(falls_to(&regional, departed, object, asker, successor));
    assert!(
      !falls_to(&regional, departed, object, departed, successor),
      "the new successor's own round, not the first host's, even though it is the successor named"
    );
  }

  /// A-251's persisted field, as the supervisor bounded it: an install records the owner it moved the authority from only
  /// when that owner is retired, one owner overwritten by the next such move, never a list. Do: move a copy from a
  /// retired host to its successor, then from that successor (retired too) to the next; then install a new live owner
  /// with an ordinary install. Expect: the first owner, then the second, kept through the ordinary install.
  #[test]
  fn an_acceptor_keeps_only_the_last_retired_owner_its_authority_moved_from() {
    let generation = 3;
    let (first, second, third, live) = (HostId(1), HostId(2), HostId(3), HostId(4));
    let mut copy = acceptor(HostId(9), first, generation, None);
    assert_eq!(copy.moved_from(), None);
    copy
      .move_authority(
        Authority {
          generation,
          owner: second,
        },
        true,
      )
      .unwrap();
    assert_eq!(copy.moved_from(), Some(first));
    copy
      .move_authority(
        Authority {
          generation,
          owner: third,
        },
        true,
      )
      .unwrap();
    assert_eq!(copy.moved_from(), Some(second), "overwritten, one owner");
    copy
      .move_authority(
        Authority {
          generation,
          owner: live,
        },
        false,
      )
      .unwrap();
    assert_eq!(
      copy.moved_from(),
      Some(second),
      "a move from a live owner records nothing"
    );
    copy
      .install_authority(Authority {
        generation,
        owner: third,
      })
      .unwrap();
    assert_eq!(
      copy.moved_from(),
      Some(second),
      "nor does an ordinary install"
    );
  }

  /// Shape (i) as one rule: a copy an install moved from the retired host to its successor still falls to that successor
  /// in the retired host's round, by the owner it moved from; the successor's own object, which no install moved from
  /// the round's line, never does, though the configuration may name it successor of that object too. Do: in a retired
  /// region, move a retired host's copy to its successor; and hold one of the successor's own objects whose successor in
  /// the round is the successor by hash. Expect: the moved copy falls to the successor there; the successor's own does
  /// not.
  #[test]
  fn a_moved_copy_falls_by_its_moved_from_owner_and_the_askers_own_never_does() {
    let (regional, asker, theirs, _) = retired_region();
    let departed = HostId(1);
    let holder = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != asker)
      .unwrap();
    let generation = regional.version;
    let mut moved = acceptor(holder, departed, generation, Some((theirs[0], 1)));
    moved
      .move_authority(
        Authority {
          generation,
          owner: asker,
        },
        true,
      )
      .unwrap();
    assert_eq!(moved.moved_from(), Some(departed));
    assert!(
      !falls_to(&regional, moved.owner(), theirs[0], departed, asker),
      "its authority alone no longer says"
    );
    assert!(copy_falls_to(&regional, &moved, theirs[0], departed, asker));
    // One of the asker's own objects whose successor in the retired host's round is the asker, by hash alone.
    let own = theirs[1];
    let askers = acceptor(holder, asker, generation, Some((own, 1)));
    assert_eq!(askers.moved_from(), None);
    assert!(
      !copy_falls_to(&regional, &askers, own, departed, asker),
      "the asker's own object"
    );
  }

  /// A-251 (b), the holder's side, with no routing at all (the warm-restarted holder's case): asked by name, a holder
  /// reports a copy it holds, says it holds nothing of one it has no acceptor for, and refuses an object the
  /// configuration gives another successor, one whose acceptor serves a live owner outside the retired host's chain, and
  /// one of the asker's own. Do: five named objects (held; not held; another live owner's; the asker's own; one whose
  /// successor is not the asker). Expect: one entry, one nothing, three refused, and the held copy fenced to the
  /// asker's epoch under its authority.
  #[test]
  fn a_holder_answers_each_named_object_from_its_own_records() {
    let (regional, asker, theirs, other) = retired_region();
    let departed = HostId(1);
    let holder = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != asker)
      .unwrap();
    let live_owner = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != asker && *host != holder)
      .unwrap();
    let generation = regional.version;
    let mut acceptors = BTreeMap::from([
      (
        theirs[0],
        acceptor(holder, departed, generation, Some((theirs[0], 3))),
      ),
      (
        theirs[2],
        acceptor(holder, live_owner, generation, Some((theirs[2], 5))),
      ),
      // One of the asker's own objects, which no install moved from the round's line.
      (
        theirs[3],
        acceptor(holder, asker, generation, Some((theirs[3], 2))),
      ),
    ]);
    let prepare = HostPrepare {
      departed,
      owner: asker,
      epoch: HostEpoch(7),
      generation,
      after: None,
      named: vec![theirs[0], theirs[1], theirs[2], theirs[3], other],
    };
    let answer = answer_by_name(holder, &prepare, &regional, &mut acceptors, usize::MAX);
    assert_eq!(
      answer
        .promise
        .entries
        .iter()
        .map(|(object, held)| (*object, held.sequence))
        .collect::<Vec<_>>(),
      vec![(theirs[0], 3)]
    );
    assert_eq!(answer.promise.nothing, vec![theirs[1]]);
    assert_eq!(
      answer.refused, 3,
      "another successor's object, a live owner's, and the asker's own"
    );
    assert_eq!(
      acceptors[&theirs[0]].authority().owner,
      asker,
      "the asker's authority installed"
    );
    assert_eq!(
      acceptors[&theirs[2]].authority().owner,
      live_owner,
      "a live owner's object untouched"
    );
    assert!(
      promise_object(
        acceptors.get_mut(&theirs[0]).unwrap(),
        theirs[0],
        &HostPrepare {
          epoch: HostEpoch(6),
          ..prepare.clone()
        },
        false
      )
      .is_err(),
      "the copy's fence was raised to the asker's epoch"
    );
  }

  /// A by-name answer is bounded as a page is, and never empty: past its budget it stops, and what it leaves out is
  /// asked again. A fence above the epoch is reported for the asker to retry above. Do: name four objects held nothing
  /// of with a budget that fits one; then name a held copy fenced above the epoch. Expect: one answered; then the fence,
  /// nothing answered for it.
  #[test]
  fn a_named_answer_keeps_to_its_budget_and_reports_a_higher_fence() {
    let (regional, asker, theirs, _) = retired_region();
    let departed = HostId(1);
    let holder = regional
      .members
      .iter()
      .copied()
      .find(|host| *host != asker)
      .unwrap();
    let generation = regional.version;
    let prepare = HostPrepare {
      departed,
      owner: asker,
      epoch: HostEpoch(7),
      generation,
      after: None,
      named: theirs.clone(),
    };
    let one = crate::register::HOST_PROMISE_HEADER_BYTES
      + crate::register::OBJECT_LIST_HEADER_BYTES
      + OBJECT_BYTES;
    let answer = answer_by_name(holder, &prepare, &regional, &mut BTreeMap::new(), one);
    assert_eq!(answer.promise.nothing, vec![theirs[0]]);
    let mut fenced = acceptor(holder, departed, generation, Some((theirs[0], 2)));
    promise_object(
      &mut fenced,
      theirs[0],
      &HostPrepare {
        epoch: HostEpoch(9),
        ..prepare.clone()
      },
      true,
    )
    .unwrap();
    let mut acceptors = BTreeMap::from([(theirs[0], fenced)]);
    let answer = answer_by_name(
      holder,
      &HostPrepare {
        named: vec![theirs[0]],
        ..prepare
      },
      &regional,
      &mut acceptors,
      usize::MAX,
    );
    assert_eq!(answer.promise.fenced, Some(HostEpoch(9)));
    assert!(answer.promise.entries.is_empty() && answer.promise.nothing.is_empty());
  }

  /// The successor's own promise: under today's rule its open gate alone counts; under A-251's it counts only by its own
  /// report or its named "nothing", since its routing can lose a copy it holds. Do: a cohort of the retired host, the
  /// successor and one other that listed the object; the successor's gate open, then shut; then the successor named.
  /// Expect: today's rule ready with the gate open and waiting with it shut; A-251's waiting until the successor names
  /// the object, then ready.
  #[test]
  fn the_successors_own_promise_counts_only_with_its_gate_open() {
    let (departed, local, a) = (HostId(9), HostId(1), HostId(2));
    let cohorts = vec![vec![departed, local, a]];
    let reports = BTreeMap::from([(a, record(3))]);
    let empty = BTreeSet::new();
    let open = Heard {
      reports: &reports,
      complete: &empty,
      named_nothing: &empty,
      local: Some(local),
    };
    let shut = Heard {
      local: None,
      ..open
    };
    let quorum = Quorum { f: 1 };
    let is_member = |host: HostId| host != departed;
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &open,
        CountingRule::CompleteAnswer
      ),
      Readiness::Ready
    );
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &shut,
        CountingRule::CompleteAnswer
      ),
      Readiness::Waiting
    );
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &open,
        CountingRule::NamedAnswer
      ),
      Readiness::Waiting,
      "an open gate is no answer under A-251's rule"
    );
    let named = BTreeSet::from([local]);
    let answered = Heard {
      named_nothing: &named,
      ..open
    };
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &answered,
        CountingRule::NamedAnswer
      ),
      Readiness::Ready
    );
  }
}
