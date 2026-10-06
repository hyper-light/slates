//! Priority elections (`docs/wip/research/consensus-enhancements.md` §3.4), on the timed simulation
//! (`support::timed`) over the real `RaftNode` and the real election timer, across real inter-region paths
//! (`support::azure`: Microsoft's published P50 round trips).
//!
//! A leader commits once a majority including itself holds an entry, so the round trip it commits in is its
//! quorum round trip: the `⌊n/2⌋`-th smallest round trip to the other voters. Measured here: which region
//! leads, and the commit latency of a steady proposal stream, over twenty seeds per region set, each seed
//! placing the regions on the hosts afresh. Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod support;

use std::collections::BTreeMap;

use slates_db::register::HostId;
use support::azure::{REGIONS, placement, profile, quorum_round_trip_ms};
use support::timed::{Campaign, ElectionOrder, Fault, MS, Proposer, Scenario, Window, run};

/// Shape: the seeds each region set runs under.
const SEEDS: u64 = 20;
/// Shape: how long a scenario runs — the first election and a minute of proposals.
const DURATION_NS: u64 = 70_000 * MS;
/// Shape: when the proposal stream begins — after the first election on any of these paths.
const PROPOSE_FROM_NS: u64 = 10_000 * MS;
/// Shape: the leader's proposal cadence.
const PROPOSE_EVERY_NS: u64 = 50 * MS;
/// Shape: the jitter added to each one-way delay — a few milliseconds of queueing around a published
/// median, which the page gives no spread for.
const JITTER_NS: u64 = 5 * MS;

/// The median of `values` (non-empty).
fn median(values: &mut [u64]) -> u64 {
  values.sort_unstable();
  values[values.len() / 2]
}

/// What `SEEDS` scenarios over the first `regions` regions measured: how many seeds each region led at the
/// end, each seed's median commit latency, and the priority transfers and yielded timeouts in all.
struct Measured {
  leaders: BTreeMap<&'static str, u64>,
  medians: Vec<u64>,
  transfers: u64,
  yields: u64,
}

impl Measured {
  fn median_of_medians(&self) -> u64 {
    median(&mut self.medians.clone())
  }
}

/// The region whose voter led at the end of `outcome`, under `hosts`.
fn final_region(outcome: &support::timed::Outcome, hosts: &[HostId]) -> &'static str {
  let (_, last, _) = *outcome.leader_events.last().expect("a leader");
  REGIONS[hosts.iter().position(|host| *host == last).unwrap()]
}

fn measure(
  regions: usize,
  order: ElectionOrder,
  faults: impl Fn(&[HostId]) -> Vec<Fault>,
) -> Measured {
  let mut measured = Measured {
    leaders: BTreeMap::new(),
    medians: Vec::new(),
    transfers: 0,
    yields: 0,
  };
  for seed in 0..SEEDS {
    let hosts = placement(regions, seed);
    let outcome = run(Scenario {
      voters: u64::try_from(regions).unwrap(),
      profile: profile(&hosts, JITTER_NS, 0),
      faults: faults(&hosts),
      duration_ns: DURATION_NS,
      propose_every_ns: PROPOSE_EVERY_NS,
      propose_from_ns: PROPOSE_FROM_NS,
      campaign: Campaign::PreVote,
      order,
      seed,
      window: Window::Bytes(0),
      proposer: Proposer::Leader,
      fast_track: false,
    });
    *measured
      .leaders
      .entry(final_region(&outcome, &hosts))
      .or_default() += 1;
    let mut latencies: Vec<u64> = outcome
      .commit_latencies_ns
      .iter()
      .map(|ns| ns / MS)
      .collect();
    measured.medians.push(median(&mut latencies));
    measured.transfers += outcome.priority_transfers;
    measured.yields += outcome.yields;
  }
  measured
}

/// §3.4, on the published matrix: ordered by priority, the leader sits where commits are fastest — never in
/// a region every other region outranks (Japan East, 162 ms, among three; any but East US, 117 ms, among
/// five) — and the proposal stream commits no slower than with elections left to the first timeout. Both
/// orders are printed for the record (`docs/wip/BENCHMARKS.md`).
#[test]
fn priority_places_the_leader_where_commits_are_fastest() {
  for (regions, outranked) in [
    (3, vec!["Japan East"]),
    (
      5,
      vec![
        "West Europe",
        "Japan East",
        "Southeast Asia",
        "Brazil South",
      ],
    ),
  ] {
    let quorum: Vec<(&str, u64)> = (0..regions)
      .map(|node| (REGIONS[node], quorum_round_trip_ms(node, regions)))
      .collect();
    let by_timeout = measure(regions, ElectionOrder::ByTimeout, |_| Vec::new());
    let by_priority = measure(regions, ElectionOrder::ByPriority, |_| Vec::new());
    eprintln!("{regions} regions: quorum round trips ms {quorum:?}");
    for (name, measured) in [("by timeout", &by_timeout), ("by priority", &by_priority)] {
      eprintln!(
        "  {name}: final leaders {:?}; median commit latency ms per seed {:?} (median {}); priority \
         transfers {}; yielded timeouts {}",
        measured.leaders,
        measured.medians,
        measured.median_of_medians(),
        measured.transfers,
        measured.yields
      );
    }
    for region in &outranked {
      assert!(
        !by_priority.leaders.contains_key(region),
        "{regions} regions: {region} is outranked, yet led: {:?}",
        by_priority.leaders
      );
    }
    assert!(by_priority.median_of_medians() <= by_timeout.median_of_medians());
  }
}

/// Shape: when the best region goes down, and for how long — after the first election has settled, and for
/// long enough that another region is elected and leads for a while.
const DOWN_FROM_NS: u64 = 20_000 * MS;
const DOWN_FOR_NS: u64 = 20_000 * MS;

/// §3.4's transfer: among five regions, East US — the one that commits fastest — goes down for twenty
/// seconds, another region is elected, and East US returns. Ordered by priority, leadership goes back to
/// East US on every seed; left to the first timeout, the region elected meanwhile keeps it.
#[test]
fn a_returning_central_region_takes_leadership_back() {
  let east_us_down = |hosts: &[HostId]| {
    vec![Fault::Crash {
      node: hosts[0],
      from_ns: DOWN_FROM_NS,
      until_ns: DOWN_FROM_NS + DOWN_FOR_NS,
    }]
  };
  let by_timeout = measure(5, ElectionOrder::ByTimeout, east_us_down);
  let by_priority = measure(5, ElectionOrder::ByPriority, east_us_down);
  for (name, measured) in [("by timeout", &by_timeout), ("by priority", &by_priority)] {
    eprintln!(
      "East US down and back — {name}: final leaders {:?}; median commit latency ms per seed {:?} (median \
       {}); priority transfers {}",
      measured.leaders,
      measured.medians,
      measured.median_of_medians(),
      measured.transfers
    );
  }
  assert_eq!(
    by_priority.leaders.get("East US").copied(),
    Some(SEEDS),
    "leadership went back to East US on every seed"
  );
  assert!(by_priority.transfers >= SEEDS, "by transfers");
}

/// What a loss of the leader cost on each seed, among the first `regions` regions: whichever region leads at
/// `DOWN_FROM_NS` is cut off for `DOWN_FOR_NS`. Per seed: milliseconds from the loss to a successor, the
/// campaigns begun in between, the successor's region and its quorum round trip, and the proposal stream's
/// longest gap.
fn leader_loss(
  regions: usize,
  seeds: u64,
  order: ElectionOrder,
) -> Vec<(u64, usize, &'static str, u64, u64)> {
  (0..seeds)
    .map(|seed| {
      let hosts = placement(regions, seed);
      let outcome = run(Scenario {
        voters: u64::try_from(regions).unwrap(),
        profile: profile(&hosts, JITTER_NS, 0),
        faults: vec![Fault::IsolateLeader {
          from_ns: DOWN_FROM_NS,
          until_ns: DOWN_FROM_NS + DOWN_FOR_NS,
        }],
        duration_ns: DURATION_NS,
        propose_every_ns: PROPOSE_EVERY_NS,
        propose_from_ns: PROPOSE_FROM_NS,
        campaign: Campaign::PreVote,
        order,
        seed,
        window: Window::Bytes(0),
        proposer: Proposer::Leader,
        fast_track: false,
      });
      let (_, lost) = *outcome.isolated.first().expect("the cut found a leader");
      let (at, successor, _) = *outcome
        .leader_events
        .iter()
        .find(|(at, node, _)| *at >= DOWN_FROM_NS && *node != lost)
        .expect("a successor");
      let campaigns = outcome
        .campaign_events
        .iter()
        .filter(|(when, node)| *when >= DOWN_FROM_NS && *when <= at && *node != lost)
        .count();
      let region = hosts.iter().position(|host| *host == successor).unwrap();
      (
        (at - DOWN_FROM_NS) / MS,
        campaigns,
        REGIONS[region],
        quorum_round_trip_ms(region, regions),
        outcome.longest_gap_ns / MS,
      )
    })
    .collect()
}

/// §3.4 with thesis §4.2.3 (`docs/bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md`): among
/// three regions, when the leader — East US, by priority — is lost, the most central survivor, West Europe,
/// succeeds it on every seed at its first campaign, within an election timeout: Japan East, which it
/// outranks, yields its own timeout and grants its pre-vote. Before the fix Japan East, holding its lost
/// leader's lease until its own campaign, refused it and was elected instead a timeout later, on 195 seeds
/// of 200 (6,766 ms median; 3,322 ms since, measured 2026-09-29, `docs/wip/BENCHMARKS.md`).
#[test]
fn a_lost_leader_passes_to_the_most_central_survivor() {
  let losses = leader_loss(3, SEEDS, ElectionOrder::ByPriority);
  eprintln!("(ms, campaigns, region, quorum ms, gap ms) per seed: {losses:?}");
  for (seed, loss) in losses.iter().enumerate() {
    assert_eq!(loss.2, "West Europe", "seed {seed}: {loss:?}");
    assert_eq!(loss.1, 1, "seed {seed}: at its first campaign: {loss:?}");
  }
}

/// Shape: the KIND lane's succession profile (`docs/wip/kind-lane.md`, Piece 6; `xtask/src/kind.rs`
/// `SUCCESSION_DELAYS`): each pod's egress delay in milliseconds — pod 0 80, pod 1 20, pod 2 unshaped — with
/// the pod network's own one-way delay under all of them.
const KIND_EGRESS_MS: [u64; 3] = [80, 20, 0];
/// Shape: the pod network's one-way delay, 0.2 ms.
const KIND_POD_NETWORK_NS: u64 = 200_000;

/// The lane's profile as the timed simulation's directional pair delays: a datagram from pod `i` takes pod
/// `i`'s egress delay plus the pod network's, ± the lane's 5 ms jitter.
fn kind_profile(hosts: &[HostId]) -> support::timed::Profile {
  let mut profile = support::timed::Profile::uniform(KIND_POD_NETWORK_NS, JITTER_NS, 0);
  for (from, from_host) in hosts.iter().enumerate() {
    for to_host in hosts.iter().filter(|to| *to != from_host) {
      profile.pairs.insert(
        (*from_host, *to_host),
        (KIND_EGRESS_MS[from] * MS + KIND_POD_NETWORK_NS, JITTER_NS),
      );
    }
  }
  profile
}

/// The KIND lane's succession measurement in simulation (`docs/wip/kind-lane.md`, Piece 6): on its profile,
/// a central leader lost is succeeded by the other central pod on every seed, never by the outranked pod 0
/// — the lane's gate, predicted before it ran. Before the lease fix the outranked pod succeeded on every seed
/// (200 of 200, 3,976 ms median); with it the central one does (200 of 200, 1,524 ms median; measured
/// 2026-09-29 over 200 seeds, `docs/wip/BENCHMARKS.md`). The lane then measured the same on real pods, where
/// the round budget's defect (which this simulation does not model) had also to be fixed.
#[test]
fn on_the_kind_profile_a_central_leader_passes_to_the_other_central_pod() {
  let hosts: Vec<HostId> = (1..=3).map(HostId).collect();
  let outranked = hosts[0];
  for seed in 0..SEEDS {
    let outcome = run(Scenario {
      voters: 3,
      profile: kind_profile(&hosts),
      faults: vec![Fault::IsolateLeader {
        from_ns: DOWN_FROM_NS,
        until_ns: DOWN_FROM_NS + DOWN_FOR_NS,
      }],
      duration_ns: DURATION_NS,
      propose_every_ns: PROPOSE_EVERY_NS,
      propose_from_ns: PROPOSE_FROM_NS,
      campaign: Campaign::PreVote,
      order: ElectionOrder::ByPriority,
      seed,
      window: Window::Bytes(0),
      proposer: Proposer::Leader,
      fast_track: false,
    });
    let (_, lost) = *outcome.isolated.first().expect("the cut found a leader");
    assert_ne!(
      lost, outranked,
      "seed {seed}: priority put the leader on a central pod"
    );
    let (_, successor, _) = *outcome
      .leader_events
      .iter()
      .find(|(at, node, _)| *at >= DOWN_FROM_NS && *node != lost)
      .expect("a successor");
    assert_ne!(
      successor, outranked,
      "seed {seed}: the outranked pod succeeded a central leader"
    );
  }
}

/// The 50th, 90th and 99th percentiles and the maximum of `values` (non-empty).
fn spread_of(values: &mut [u64]) -> [u64; 4] {
  values.sort_unstable();
  let at = |p: usize| values[p * (values.len() - 1) / 100];
  [at(50), at(90), at(99), values[values.len() - 1]]
}

/// A measurement tool: [`leader_loss`] under both orders, among three and five regions, over
/// `SLATES_LEADER_LOSS_SEEDS` seeds (twenty without it).
#[test]
#[ignore = "a measurement tool, run by hand"]
fn a_leader_loss_measured() {
  let seeds = std::env::var("SLATES_LEADER_LOSS_SEEDS")
    .ok()
    .and_then(|seeds| seeds.parse::<u64>().ok())
    .unwrap_or(SEEDS);
  for regions in [3, 5] {
    for (name, order) in [
      ("by priority", ElectionOrder::ByPriority),
      ("by timeout", ElectionOrder::ByTimeout),
    ] {
      let losses = leader_loss(regions, seeds, order);
      let mut successor_ms: Vec<u64> = losses.iter().map(|loss| loss.0).collect();
      let mut gaps: Vec<u64> = losses.iter().map(|loss| loss.4).collect();
      let campaigns: usize = losses.iter().map(|loss| loss.1).sum();
      let mut successors: BTreeMap<&str, u64> = BTreeMap::new();
      for loss in &losses {
        *successors.entry(loss.2).or_default() += 1;
      }
      eprintln!(
        "{regions} regions {name}: successor ms p50/p90/p99/max {:?}; longest gap ms {:?}; campaigns {campaigns} \
         over {seeds} seeds; successors {successors:?}",
        spread_of(&mut successor_ms),
        spread_of(&mut gaps),
      );
    }
  }
  kind_leader_loss_measured(seeds);
}

/// The leader-loss tool's KIND block: on the lane's succession profile ([`kind_profile`]), the time to a
/// successor and which pod it was, over `seeds` seeds.
fn kind_leader_loss_measured(seeds: u64) {
  let hosts: Vec<HostId> = (1..=3).map(HostId).collect();
  let mut successor_ms = Vec::new();
  let mut successors: BTreeMap<u64, u64> = BTreeMap::new();
  for seed in 0..seeds {
    let outcome = run(Scenario {
      voters: 3,
      profile: kind_profile(&hosts),
      faults: vec![Fault::IsolateLeader {
        from_ns: DOWN_FROM_NS,
        until_ns: DOWN_FROM_NS + DOWN_FOR_NS,
      }],
      duration_ns: DURATION_NS,
      propose_every_ns: PROPOSE_EVERY_NS,
      propose_from_ns: PROPOSE_FROM_NS,
      campaign: Campaign::PreVote,
      order: ElectionOrder::ByPriority,
      seed,
      window: Window::Bytes(0),
      proposer: Proposer::Leader,
      fast_track: false,
    });
    let (_, lost) = *outcome.isolated.first().expect("the cut found a leader");
    let (at, successor, _) = *outcome
      .leader_events
      .iter()
      .find(|(at, node, _)| *at >= DOWN_FROM_NS && *node != lost)
      .expect("a successor");
    successor_ms.push((at - DOWN_FROM_NS) / MS);
    *successors.entry(successor.0 - 1).or_default() += 1;
  }
  eprintln!(
    "the KIND profile: successor ms p50/p90/p99/max {:?} over {seeds} seeds; successors by pod {successors:?}",
    spread_of(&mut successor_ms)
  );
}
