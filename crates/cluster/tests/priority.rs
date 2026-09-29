//! Priority elections (`docs/wip/research/consensus-enhancements.md` §3.4), on the timed simulation
//! (`support::timed`) over the real `RaftNode` and the real election timer, across real inter-region paths
//! (`support::azure`: Microsoft's published P50 round trips).
//!
//! A leader commits once a majority including itself holds an entry, so the round trip it commits in is its
//! quorum round trip: the `⌊n/2⌋`-th smallest round trip to the other voters. Measured here: which region
//! leads, and the commit latency of a steady proposal stream, over twenty seeds per region set, each seed
//! placing the regions on the hosts afresh. Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
