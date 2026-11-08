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
//! copy it holds (its routing disagreed with the successor's, or it could not promise the copy) then reads as holding
//! nothing, and the successor can adopt an older record over a committed newer one. The model finds that history.

use std::collections::{BTreeMap, BTreeSet};

use crate::register::{Accepted, HostId, Quorum};

/// Which answers count as a member's promise of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CountingRule {
  /// The rule before A-251 (b): a member promised an object when it listed it, or when its paged answer is complete.
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
  /// while it may not, when its own answer counts for nothing.
  pub local: Option<HostId>,
}

/// Where one object of a takeover stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
  /// Every cohort has its recovery quorum of promises: adopt the newest record reported.
  Ready,
  /// A cohort has not promised enough yet.
  Waiting,
  /// A cohort has fewer members left than its recovery quorum: the object cannot be recovered.
  Lost,
}

impl Heard<'_> {
  /// Whether `host` has promised the object under `rule`.
  fn promised(&self, host: HostId, rule: CountingRule) -> bool {
    if self.local == Some(host) || self.reports.contains_key(&host) {
      return true;
    }
    match rule {
      CountingRule::CompleteAnswer => self.complete.contains(&host),
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
  for cohort in cohorts {
    let needed = quorum.recovery(cohort.len());
    let members: Vec<HostId> = cohort
      .iter()
      .copied()
      .filter(|host| is_member(*host))
      .collect();
    if members.len() < needed {
      return Readiness::Lost;
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
  /// its quorum. Do: two cohorts at f = 1, one of them left with a single member of three; then two whole cohorts, only
  /// one of which has promised. Expect: lost; then waiting.
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
        Readiness::Lost
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

  /// The successor's own promise counts only while its lease gate is open. Do: a cohort of the retired host, the
  /// successor and one other that listed the object. Expect: ready with the successor's promise, waiting without it.
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
        CountingRule::NamedAnswer
      ),
      Readiness::Ready
    );
    assert_eq!(
      readiness(
        &cohorts,
        is_member,
        quorum,
        &shut,
        CountingRule::NamedAnswer
      ),
      Readiness::Waiting
    );
  }
}
