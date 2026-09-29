//! Priority elections (`docs/wip/research/consensus-enhancements.md` §3.4), on the timed simulation
//! (`support::timed`) over the real `RaftNode` and the real election timer, across real inter-region paths:
//! Microsoft's published P50 round trips ("Azure network round-trip latency statistics",
//! learn.microsoft.com/en-us/azure/networking/azure-network-latency, page dated 2026-07-30, fetched
//! 2026-09-28; directional, one way taken as half the round trip).
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
use support::timed::{Campaign, ElectionOrder, Fault, MS, Profile, Scenario, run};

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

/// Format: the regions, in the matrix's order.
const REGIONS: [&str; 5] = [
  "East US",
  "West Europe",
  "Japan East",
  "Southeast Asia",
  "Brazil South",
];
/// Format: the published P50 round trips, in milliseconds, source row to destination column, among
/// [`REGIONS`] (read from the page's tables; `East US` → `West Europe` is 83 ms and `West Europe` →
/// `East US` 85 ms — the page is directional).
const ROUND_TRIPS_MS: [[u64; 5]; 5] = [
  [0, 83, 162, 224, 117],
  [85, 0, 233, 169, 185],
  [162, 234, 0, 72, 262],
  [224, 169, 72, 0, 330],
  [118, 185, 262, 331, 0],
];

/// Which host sits in each of the first `regions` regions under `seed`: a permutation of the hosts
/// `1..=regions` (Fisher–Yates over splitmix64). The simulation numbers its voters `1..=n` and the
/// election timer's jitter is a draw from the node's id, so with a fixed placement the same region would win
/// every first election — one region led all twenty seeds before this (2026-09-28); a fleet's member ids are
/// random 64-bit values, which the permutation models.
fn placement(regions: usize, seed: u64) -> Vec<HostId> {
  let mut hosts: Vec<HostId> = (1..=u64::try_from(regions).unwrap()).map(HostId).collect();
  let mut state = seed;
  for index in (1..regions).rev() {
    state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^= mixed >> 31;
    let pick = usize::try_from(mixed % u64::try_from(index + 1).unwrap()).unwrap();
    hosts.swap(index, pick);
  }
  hosts
}

/// The profile of the regions on `hosts` (region `i` on `hosts[i]`): each direction half the published
/// round trip from its source, with [`JITTER_NS`].
fn azure(hosts: &[HostId]) -> Profile {
  let mut profile = Profile::uniform(0, JITTER_NS, 0);
  for (from, from_host) in hosts.iter().enumerate() {
    for (to, to_host) in hosts.iter().enumerate() {
      if from != to {
        let one_way = ROUND_TRIPS_MS[from][to] * MS / 2;
        profile
          .pairs
          .insert((*from_host, *to_host), (one_way, JITTER_NS));
      }
    }
  }
  profile
}

/// The published quorum round trip of region `node` among the first `regions`: the `⌊n/2⌋`-th smallest
/// round trip from it to the others.
fn quorum_round_trip_ms(node: usize, regions: usize) -> u64 {
  let mut trips: Vec<u64> = (0..regions)
    .filter(|other| *other != node)
    .map(|other| ROUND_TRIPS_MS[node][other])
    .collect();
  trips.sort_unstable();
  trips[regions / 2 - 1]
}

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
      profile: azure(&hosts),
      faults: faults(&hosts),
      duration_ns: DURATION_NS,
      propose_every_ns: PROPOSE_EVERY_NS,
      propose_from_ns: PROPOSE_FROM_NS,
      campaign: Campaign::PreVote,
      order,
      seed,
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
