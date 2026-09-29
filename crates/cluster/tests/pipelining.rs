//! Pipelined replication (`docs/wip/research/consensus-enhancements.md` §3.5; thesis §10.2.2), on the timed
//! simulation (`support::timed`) over the real `RaftNode`, across five Azure regions (`support::azure`). The
//! drive sends each follower one append a period, as the council's does; with a window, the next period's
//! append to a confirmed follower carries the next batch while the last is still unacknowledged, as long as
//! the backlog is more than one resend carries and what is in flight fits the follower's window
//! (`RaftNode::replicate_to`). Measured here: the commit latency of a proposal stream, the commits the group
//! sustains, and the messages and bytes it sends, with and without a window, across proposal rates and loss.
//! Test by use (R5).

// Test harness: an unwrap, expect or panic here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use std::time::Instant;

use slates_cluster::raft::RaftNode;
use slates_db::register::HostId;
use support::azure::{placement, profile};
use support::timed::{
  Campaign, ElectionOrder, MS, Outcome, Proposer, Scenario, Window, batch_budget, run,
};

/// Shape: the regions the group spans — all five of the matrix, so the leader's quorum round trip (83 to 185
/// ms) is at or past the 100 ms period.
const REGIONS: usize = 5;
/// Shape: when the proposal stream begins — after the first election on these paths.
const PROPOSE_FROM_NS: u64 = 10_000 * MS;
/// Shape: the jitter added to each one-way delay — a few milliseconds of queueing around a published median.
const JITTER_NS: u64 = 5 * MS;
/// One run: `seed`'s placement of the regions, proposals every `propose_every_ns` for `stream_ns`, the
/// given window, and `loss_ppm` of the messages lost.
fn scenario(
  seed: u64,
  window: Window,
  propose_every_ns: u64,
  stream_ns: u64,
  loss_ppm: u32,
) -> Outcome {
  let hosts = placement(REGIONS, seed);
  run(Scenario {
    voters: u64::try_from(REGIONS).unwrap(),
    profile: profile(&hosts, JITTER_NS, loss_ppm),
    faults: Vec::new(),
    duration_ns: PROPOSE_FROM_NS + stream_ns,
    propose_every_ns,
    propose_from_ns: PROPOSE_FROM_NS,
    campaign: Campaign::PreVote,
    order: ElectionOrder::ByPriority,
    seed,
    window,
    proposer: Proposer::Leader,
    fast_track: false,
  })
}

/// What `seeds` runs measured, summarized over the seeds: the median of the per-seed median and 99th
/// percentile commit latency, the commits per second over the stream, and the messages and bytes sent.
#[derive(Debug, PartialEq, Eq)]
struct Measured {
  median_ms: u64,
  p99_ms: u64,
  commits_per_s: u64,
  messages: u64,
  bytes: u64,
  sent_ahead: u64,
}

fn median(values: &mut [u64]) -> u64 {
  values.sort_unstable();
  values[values.len() / 2]
}

fn measure(
  seeds: u64,
  window: Window,
  propose_every_ns: u64,
  stream_ns: u64,
  loss_ppm: u32,
) -> Measured {
  let (mut medians, mut p99s, mut rates) = (Vec::new(), Vec::new(), Vec::new());
  let (mut messages, mut bytes, mut sent_ahead) = (0, 0, 0);
  for seed in 0..seeds {
    let outcome = scenario(seed, window, propose_every_ns, stream_ns, loss_ppm);
    medians.push(outcome.commit_latency_pct(50) / MS);
    p99s.push(outcome.commit_latency_pct(99) / MS);
    rates.push(u64::try_from(outcome.commit_latencies_ns.len()).unwrap() * 1_000 * MS / stream_ns);
    messages += outcome.messages;
    bytes += outcome.bytes;
    sent_ahead += outcome.sent_ahead;
  }
  Measured {
    median_ms: median(&mut medians),
    p99_ms: median(&mut p99s),
    commits_per_s: median(&mut rates),
    messages,
    bytes,
    sent_ahead,
  }
}

/// Shape: the seeds and the stream length CI's gate runs — enough for the overload to show in every seed.
const GATE_SEEDS: u64 = 4;
const GATE_STREAM_NS: u64 = 10_000 * MS;

/// §3.5 across the five regions, as CI's gate: at 2,000 proposals a second — past what one batch a round
/// trip carries — a window of one batch keeps up where none does (measured 2026-09-29: 1,964 commits a
/// second at a 172 ms median, against 1,018 at 2,497 ms), and at 20 a second, a backlog within one batch,
/// it changes nothing at all: every run is the same, byte for byte.
#[test]
fn a_window_of_one_batch_keeps_up_where_none_does() {
  let budget = batch_budget(REGIONS);
  let overloaded = |window| measure(GATE_SEEDS, window, 500_000, GATE_STREAM_NS, 0);
  let (without, with) = (
    overloaded(Window::Bytes(0)),
    overloaded(Window::Bytes(budget)),
  );
  eprintln!("2,000 a second: without a window {without:?}; with one batch {with:?}");
  assert!(with.sent_ahead > 0, "batches went ahead");
  assert!(
    with.commits_per_s * 2 > without.commits_per_s * 3,
    "the window sustains half as much again"
  );
  assert!(
    with.median_ms * 5 < without.median_ms,
    "and commits five times sooner"
  );
  let quiet = |window| measure(GATE_SEEDS, window, 50 * MS, GATE_STREAM_NS, 0);
  assert_eq!(
    quiet(Window::Bytes(0)),
    quiet(Window::Bytes(budget)),
    "a backlog within one batch is resent, window or not"
  );
}

/// A measurement tool: every window × rate × loss named by the environment, printed as a table
/// (`docs/wip/BENCHMARKS.md`). `SLATES_PIPELINING_SEEDS=20 SLATES_PIPELINING_STREAM_S=30 cargo test -p
/// slates-cluster --release --test pipelining -- --ignored --exact pipelining_across_rates_and_loss
/// --nocapture`. Skips, saying so, without `SLATES_PIPELINING_SEEDS`.
#[test]
#[ignore = "a measurement tool, run by hand with its environment set"]
fn pipelining_across_rates_and_loss() {
  let Some(seeds) = std::env::var("SLATES_PIPELINING_SEEDS")
    .ok()
    .and_then(|seeds| seeds.parse::<u64>().ok())
  else {
    eprintln!("skipping: set SLATES_PIPELINING_SEEDS (and SLATES_PIPELINING_STREAM_S) to measure");
    return;
  };
  let stream_ns = std::env::var("SLATES_PIPELINING_STREAM_S")
    .ok()
    .and_then(|seconds| seconds.parse::<u64>().ok())
    .unwrap_or(30)
    * 1_000
    * MS;
  let budget = batch_budget(REGIONS);
  eprintln!("batch budget {budget} bytes; a command's entry is 21 bytes");
  let windows = [
    Window::Bytes(0),
    Window::Bytes(budget),
    Window::Bytes(4 * budget),
    Window::Derived,
  ];
  for loss_ppm in [0, 10_000] {
    for every_us in [50_000, 2_000, 1_000, 500, 250] {
      for window in windows {
        let measured = measure(seeds, window, every_us * MS / 1_000, stream_ns, loss_ppm);
        eprintln!(
          "loss {loss_ppm} ppm, a proposal every {every_us} us, window {window:?}: {measured:?}"
        );
      }
    }
  }
}

/// A measurement tool: what one proposal costs a leader as its backlog to the followers grows — a leader of
/// five whose followers acknowledge nothing, appending `SLATES_BACKLOG_ENTRIES` commands, timed per
/// thousand. `SLATES_BACKLOG_ENTRIES=50000 cargo test -p slates-cluster --release --test pipelining --
/// --ignored --exact a_proposal_costs_the_leader_the_same_at_any_backlog --nocapture`. Skips, saying so,
/// without the variable.
#[test]
#[ignore = "a measurement tool, run by hand with its environment set"]
fn a_proposal_costs_the_leader_the_same_at_any_backlog() {
  let Some(entries) = std::env::var("SLATES_BACKLOG_ENTRIES")
    .ok()
    .and_then(|entries| entries.parse::<u64>().ok())
  else {
    eprintln!("skipping: set SLATES_BACKLOG_ENTRIES to measure");
    return;
  };
  let voters: Vec<HostId> = (1..=5).map(HostId).collect();
  let mut leader = RaftNode::new(voters[0], voters.clone());
  let requests = leader.start_election();
  for (voter, request) in voters[1..].iter().zip(requests) {
    let reply = RaftNode::new(*voter, voters.clone()).on_request_vote(request);
    leader.on_vote_reply(reply);
  }
  assert!(leader.is_leader());
  const REPORT_EVERY: u64 = 1_000;
  let mut started = Instant::now();
  for appended in 1..=entries {
    assert!(leader.append_command(appended.to_le_bytes().to_vec()));
    if appended % REPORT_EVERY == 0 {
      eprintln!(
        "backlog {appended}: {:?} a proposal",
        started.elapsed() / u32::try_from(REPORT_EVERY).unwrap()
      );
      started = Instant::now();
    }
  }
}
