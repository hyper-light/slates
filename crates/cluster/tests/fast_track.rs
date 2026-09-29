//! The fast track's crossover (`docs/wip/research/consensus-enhancements.md` §3.7: "slates measures the same
//! crossover on its own profiles and decides from the data whether the fast track is always on or chosen per
//! group from the measured loss"), on the timed simulation (`support::timed`) over the real `RaftNode`,
//! across five Azure regions (`support::azure`), with windows derived as the daemon derives them.
//!
//! A proposer away from the leader is where the fast track can pay: a leader proposing for itself commits at a
//! classic quorum's round trip, and a fast quorum (four of five) is larger. Classic, the proposer forwards to
//! its leader, which commits at its quorum and answers back; fast, the proposer sends to every voter, each vote
//! goes to the leader, which commits at a fast quorum of votes and answers back. Measured here: the latency
//! from a proposal to when its proposer learns the commit, per proposer region, with the track closed and
//! open, across loss. Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use support::azure::{REGIONS, placement, profile};
use support::timed::{Campaign, ElectionOrder, MS, Outcome, Proposer, Scenario, Window, run};

/// Shape: when the proposal stream begins — after the first election on these paths.
const PROPOSE_FROM_NS: u64 = 10_000 * MS;
/// Shape: the jitter added to each one-way delay — a few milliseconds of queueing around a published median.
const JITTER_NS: u64 = 5 * MS;
/// Shape: the proposal cadence — twenty a second, a latency measurement rather than a load one.
const PROPOSE_EVERY_NS: u64 = 50 * MS;

/// One run: `seed`'s placement of the five regions, the proposer in region `region`, the track open or not,
/// `loss_ppm` of the messages lost, a stream of `stream_ns`.
fn scenario(seed: u64, region: usize, fast_track: bool, loss_ppm: u32, stream_ns: u64) -> Outcome {
  let hosts = placement(REGIONS.len(), seed);
  run(Scenario {
    voters: u64::try_from(REGIONS.len()).unwrap(),
    profile: profile(&hosts, JITTER_NS, loss_ppm),
    faults: Vec::new(),
    duration_ns: PROPOSE_FROM_NS + stream_ns,
    propose_every_ns: PROPOSE_EVERY_NS,
    propose_from_ns: PROPOSE_FROM_NS,
    campaign: Campaign::PreVote,
    order: ElectionOrder::ByPriority,
    seed,
    window: Window::Derived,
    proposer: Proposer::At(hosts[region]),
    fast_track,
  })
}

/// What `seeds` runs measured for one proposer region: the median over the seeds of each seed's median and
/// 99th percentile latency, and the commits in all.
#[derive(Debug, PartialEq, Eq)]
struct Measured {
  median_ms: u64,
  p99_ms: u64,
  commits: usize,
  holes_filled: u64,
}

fn median(values: &mut [u64]) -> u64 {
  values.sort_unstable();
  values[values.len() / 2]
}

fn measure(seeds: u64, region: usize, fast_track: bool, loss_ppm: u32, stream_ns: u64) -> Measured {
  let (mut medians, mut p99s, mut commits, mut holes_filled) = (Vec::new(), Vec::new(), 0, 0);
  for seed in 0..seeds {
    let outcome = scenario(seed, region, fast_track, loss_ppm, stream_ns);
    medians.push(outcome.commit_latency_pct(50) / MS);
    p99s.push(outcome.commit_latency_pct(99) / MS);
    commits += outcome.commit_latencies_ns.len();
    holes_filled += outcome.holes_filled;
  }
  Measured {
    median_ms: median(&mut medians),
    p99_ms: median(&mut p99s),
    commits,
    holes_filled,
  }
}

/// Shape: the seeds and the stream length CI's gate runs.
const GATE_SEEDS: u64 = 4;
const GATE_STREAM_NS: u64 = 10_000 * MS;
/// Format: the matrix's indices of the regions the gate names.
const EAST_US: usize = 0;
const SOUTHEAST_ASIA: usize = 3;

/// §3.7's crossover, as CI's gate, on the five regions (measured 2026-09-29 over 20 seeds: `docs/wip/
/// BENCHMARKS.md`):
/// - a proposer far from the leader (Southeast Asia) commits sooner on the fast track — 435 ms to 275 ms at the
///   median with no loss — since its votes reach the leader directly instead of through the leader's own round;
/// - a proposer beside the leader (East US, which leads by priority) commits later — 156 ms to 201 ms — since a
///   fast quorum (four of five) is larger than a classic one: the reason the groups, whose proposals all come
///   from their leader, keep the track closed;
/// - at 4 % loss the leader fills the indices lost votes leave short of a quorum, so the fast track commits as
///   many proposals as the classic one (until 2026-09-29 a proposer there lost up to half).
#[test]
fn the_fast_track_pays_away_from_the_leader_and_costs_beside_it() {
  let far = |fast, loss| measure(GATE_SEEDS, SOUTHEAST_ASIA, fast, loss, GATE_STREAM_NS);
  let (classic, fast) = (far(false, 0), far(true, 0));
  eprintln!("Southeast Asia, no loss: classic {classic:?}; fast {fast:?}");
  assert!(
    fast.median_ms * 4 < classic.median_ms * 3,
    "a quarter sooner, far from the leader"
  );
  let near = |fast| measure(GATE_SEEDS, EAST_US, fast, 0, GATE_STREAM_NS);
  let (classic, fast) = (near(false), near(true));
  eprintln!("East US, no loss: classic {classic:?}; fast {fast:?}");
  assert!(fast.median_ms > classic.median_ms, "later beside it");
  let (classic, fast) = (far(false, 40_000), far(true, 40_000));
  eprintln!("Southeast Asia, 4 % loss: classic {classic:?}; fast {fast:?}");
  assert!(
    fast.holes_filled > 0,
    "lost votes left holes, and the leader filled them"
  );
  assert!(
    fast.commits * 100 >= classic.commits * 99,
    "as many commits as the classic track"
  );
}

/// A measurement tool: every proposer region × loss, with the track closed and open, printed as a table
/// (`docs/wip/BENCHMARKS.md`). `SLATES_FAST_SEEDS=20 SLATES_FAST_STREAM_S=30 cargo test -p slates-cluster
/// --release --test fast_track -- --ignored --exact the_fast_track_across_regions_and_loss --nocapture`.
/// Skips, saying so, without `SLATES_FAST_SEEDS`.
#[test]
#[ignore = "a measurement tool, run by hand with its environment set"]
fn the_fast_track_across_regions_and_loss() {
  let Some(seeds) = std::env::var("SLATES_FAST_SEEDS")
    .ok()
    .and_then(|seeds| seeds.parse::<u64>().ok())
  else {
    eprintln!("skipping: set SLATES_FAST_SEEDS (and SLATES_FAST_STREAM_S) to measure");
    return;
  };
  let stream_ns = std::env::var("SLATES_FAST_STREAM_S")
    .ok()
    .and_then(|seconds| seconds.parse::<u64>().ok())
    .unwrap_or(30)
    * 1_000
    * MS;
  for loss_ppm in [0, 10_000, 40_000, 100_000] {
    for (region, name) in REGIONS.iter().enumerate() {
      let classic = measure(seeds, region, false, loss_ppm, stream_ns);
      let fast = measure(seeds, region, true, loss_ppm, stream_ns);
      eprintln!("loss {loss_ppm} ppm, proposer in {name}: classic {classic:?}; fast {fast:?}");
    }
  }
}
