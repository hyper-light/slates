//! The pre-vote audit (Raft thesis §9.6; `docs/wip/research/consensus-enhancements.md` §3.1): the disruption
//! scenarios the thesis names, run on the timed simulation (`support::timed`) over the real `RaftNode` and the
//! real election timer, on a single-cluster profile and a multi-region one, twenty seeds each.
//!
//! - **An isolated follower rejoins.** A follower cut off for thirty seconds keeps timing out. With pre-vote
//!   its pre-votes are refused and its term never moves, so when the cut heals it rejoins as a follower and
//!   the leader it never could reach keeps leading. The control campaigns directly (`start_election`, no
//!   pre-vote round — a harness variant, never a production path): its term climbs every timeout, and on
//!   the heal its vote request deposes the healthy leader. The control proves the scenario bites.
//! - **The leader is isolated.** CheckQuorum steps the cut-off leader down, the majority elects a successor
//!   within about an election timeout, and when the cut heals the old leader — its term now behind — rejoins
//!   without deposing anyone.
//!
//! Measured and printed per profile: leadership changes after the heal, the isolated node's final term
//! against the leader's, and the longest the proposal stream went without a commit. Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use support::timed::{Campaign, ElectionOrder, Fault, MS, Outcome, Profile, Scenario, Window, run};

/// Shape: the seeds each scenario runs under.
const SEEDS: u64 = 20;
/// Shape: when the cut begins — well after the first election settles on either profile.
const CUT_AT_NS: u64 = 10_000 * MS;
/// Shape: how long the cut lasts — thirty seconds, fifteen or more election timeouts on either profile, so
/// a node that campaigns without a pre-vote climbs many terms.
const CUT_FOR_NS: u64 = 30_000 * MS;
/// Shape: how long the scenario runs after the heal — long enough for any disruption the heal causes to
/// play out (several election timeouts on the multi-region profile).
const AFTER_HEAL_NS: u64 = 30_000 * MS;
/// Shape: the proposal stream's cadence at the leader.
const PROPOSE_EVERY_NS: u64 = 50 * MS;
/// Shape: when the proposal stream begins — after the first election on either profile (its commit gaps are
/// the faults', not the startup's).
const PROPOSE_FROM_NS: u64 = 5_000 * MS;

/// Shape: a single cluster — 0.25 ms one way, 0.1 ms of jitter, no loss.
fn cluster() -> Profile {
  Profile::uniform(250_000, 100_000, 0)
}

/// Shape: multi-region — 80 ms one way ± 20 ms, the inter-region path `tests/wan_election.rs` models
/// (Japan East → East US, a 162 ms P50 round trip published by Microsoft for the 30 days ending
/// 2026-07-30).
fn multi_region() -> Profile {
  Profile::uniform(80 * MS, 20 * MS, 0)
}

fn scenario(profile: &Profile, fault: Fault, campaign: Campaign, seed: u64) -> Scenario {
  Scenario {
    voters: 3,
    profile: profile.clone(),
    faults: vec![fault],
    duration_ns: CUT_AT_NS + CUT_FOR_NS + AFTER_HEAL_NS,
    propose_every_ns: PROPOSE_EVERY_NS,
    propose_from_ns: PROPOSE_FROM_NS,
    campaign,
    order: ElectionOrder::ByPriority,
    seed,
    window: Window::Bytes(0),
  }
}

/// The node the scenario cut off, and the leader's term at the end.
fn isolated_and_leader_term(outcome: &Outcome) -> (slates_db::register::HostId, u64) {
  let (_, isolated) = *outcome.isolated.first().expect("the cut resolved a node");
  let leader_term = outcome
    .leader_events
    .last()
    .map(|(_, _, term)| *term)
    .expect("a leader");
  (isolated, leader_term)
}

/// Runs the isolated-follower scenario over every seed with `campaign`: per seed, the leadership changes
/// after the heal, and the isolated node's final term less the last leader's term.
fn isolated_follower(profile: &Profile, campaign: Campaign) -> Vec<(usize, i64, u64)> {
  (0..SEEDS)
    .map(|seed| {
      let outcome = run(scenario(
        profile,
        Fault::IsolateFollower {
          from_ns: CUT_AT_NS,
          until_ns: CUT_AT_NS + CUT_FOR_NS,
        },
        campaign,
        seed,
      ));
      // Term inflation: how far the group's term moved from the leader's at the cut to the end.
      let (_, final_term) = isolated_and_leader_term(&outcome);
      let inflation = final_term.saturating_sub(outcome.leader_term_at(CUT_AT_NS));
      (
        outcome.leader_changes_after(CUT_AT_NS + CUT_FOR_NS),
        i64::try_from(inflation).unwrap(),
        outcome.longest_gap_ns,
      )
    })
    .collect()
}

fn summarize(label: &str, runs: &[(usize, i64, u64)]) {
  let changes: Vec<usize> = runs.iter().map(|(changes, _, _)| *changes).collect();
  let excess: Vec<i64> = runs.iter().map(|(_, excess, _)| *excess).collect();
  let gaps_ms: Vec<u64> = runs.iter().map(|(_, _, gap)| gap / MS).collect();
  eprintln!(
    "{label}: post-heal leader changes {changes:?}; term inflation {excess:?}; longest commit gap ms {gaps_ms:?}"
  );
}

/// Thesis §9.6, on both profiles: with pre-vote, a follower cut off for thirty seconds never raises its term
/// and, healed, never deposes the leader; the direct-election control does both on most seeds, so the
/// scenario is not vacuous.
#[test]
fn an_isolated_follower_rejoins_without_deposing_the_leader() {
  for (name, profile) in [("cluster", cluster()), ("multi-region", multi_region())] {
    let with_prevote = isolated_follower(&profile, Campaign::PreVote);
    let direct = isolated_follower(&profile, Campaign::Direct);
    summarize(&format!("{name} pre-vote"), &with_prevote);
    summarize(&format!("{name} direct (control)"), &direct);
    for (seed, (changes, excess, _)) in with_prevote.iter().enumerate() {
      assert_eq!(
        *changes, 0,
        "{name} seed {seed}: pre-vote — no leader change after the heal"
      );
      assert_eq!(
        *excess, 0,
        "{name} seed {seed}: pre-vote — the term never moved"
      );
    }
    let disrupted = direct.iter().filter(|(changes, _, _)| *changes > 0).count();
    assert!(
      disrupted * 2 > direct.len(),
      "{name}: the control deposed the leader on the heal in most seeds ({disrupted} of {})",
      direct.len()
    );
  }
}

/// Thesis §6.2 with §9.6, on both profiles: a leader cut off steps down under CheckQuorum, the majority
/// elects a successor, and the old leader's return deposes nobody.
#[test]
fn an_isolated_leader_is_succeeded_and_its_return_deposes_nobody() {
  for (name, profile) in [("cluster", cluster()), ("multi-region", multi_region())] {
    let runs: Vec<(usize, u64, usize)> = (0..SEEDS)
      .map(|seed| {
        let outcome = run(scenario(
          &profile,
          Fault::IsolateLeader {
            from_ns: CUT_AT_NS,
            until_ns: CUT_AT_NS + CUT_FOR_NS,
          },
          Campaign::PreVote,
          seed,
        ));
        eprintln!(
          "  {name} seed {seed}: timings (base, span, tail ns) {:?}; campaigns during the cut {:?}",
          outcome.timings,
          outcome
            .campaign_events
            .iter()
            .filter(|(at, _)| *at >= CUT_AT_NS && *at < CUT_AT_NS + CUT_FOR_NS)
            .map(|(at, node)| ((*at - CUT_AT_NS) / MS, node.0))
            .collect::<Vec<_>>()
        );
        (
          outcome.leader_changes_after(CUT_AT_NS + CUT_FOR_NS),
          outcome.longest_gap_ns,
          outcome.leader_changes_after(CUT_AT_NS),
        )
      })
      .collect();
    eprintln!(
      "{name}: post-heal changes {:?}; longest commit gap ms {:?}; changes during the cut {:?}",
      runs.iter().map(|r| r.0).collect::<Vec<_>>(),
      runs.iter().map(|r| r.1 / MS).collect::<Vec<_>>(),
      runs.iter().map(|r| r.2).collect::<Vec<_>>()
    );
    for (seed, (after_heal, _, during)) in runs.iter().enumerate() {
      assert_eq!(
        *after_heal, 0,
        "{name} seed {seed}: the old leader's return deposed nobody"
      );
      assert!(
        *during >= 1,
        "{name} seed {seed}: the majority elected a successor during the cut"
      );
    }
  }
}

/// Shape: the election rounds a leader loss may cost before the survivors have a successor. With the timer's
/// per-attempt draws independent, two survivors collide (time out together and split the vote) with
/// probability about 1/span per round, so four rounds fail about once in a thousand losses; a timer whose
/// draws stay correlated across rounds never breaks a collision and blows through any bound.
const ROUNDS_BOUND: u64 = 4;

/// Raft §5.2 / §9.3 (randomized election timeouts; `docs/bugs/2026-09-28-correlated-election-jitter-livelocked-a-split-vote.md`):
/// after the leader is cut off, the survivors elect a successor within `ROUNDS_BOUND` election timeouts
/// — `base + span` periods each, as each survivor derived them — on both profiles and every seed. A pair of
/// survivors whose jitter stays equal round after round would split the vote until the cut healed.
#[test]
fn survivors_of_a_leader_loss_elect_within_a_few_election_timeouts() {
  for (name, profile) in [("cluster", cluster()), ("multi-region", multi_region())] {
    let mut took_ms = Vec::new();
    for seed in 0..SEEDS {
      let outcome = run(scenario(
        &profile,
        Fault::IsolateLeader {
          from_ns: CUT_AT_NS,
          until_ns: CUT_AT_NS + CUT_FOR_NS,
        },
        Campaign::PreVote,
        seed,
      ));
      let (_, cut) = *outcome
        .isolated
        .first()
        .expect("the cut resolved the leader");
      let successor_at = outcome
        .leader_events
        .iter()
        .find(|(at, node, _)| *at >= CUT_AT_NS && *node != cut)
        .map(|(at, _, _)| *at)
        .expect("a successor was elected during the cut");
      let took = successor_at - CUT_AT_NS;
      let timeout_ns = outcome
        .timings
        .iter()
        .filter(|(node, _)| **node != cut)
        .map(|(_, (base, span, _))| u64::from(*base + *span) * support::timed::HEARTBEAT_NS)
        .max()
        .unwrap();
      took_ms.push(took / MS);
      assert!(
        took <= ROUNDS_BOUND * timeout_ns,
        "{name} seed {seed}: a successor after {} ms, past {ROUNDS_BOUND} election timeouts of {} ms",
        took / MS,
        timeout_ns / MS
      );
    }
    eprintln!("{name}: successor elected after ms {took_ms:?}");
  }
}
