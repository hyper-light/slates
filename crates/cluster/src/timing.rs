//! The configuration group's timing law (§4.8 "Derived constants": *"election timeout for the
//! configuration group ≥ 10 × broadcast RTT p99 with the randomization span from RTT variance"*) — pure
//! and sans-io, so the daemon's council and root-group drive loops share one law and the simulated fabric
//! can prove it on a WAN profile under a virtual clock.
//!
//! What it holds. Raft's timing requirement is `broadcastTime ≪ electionTimeout ≪ MTBF` (Ongaro &
//! Ousterhout, "In Search of an Understandable Consensus Algorithm", ATC 2014, §5.6): a leader must be
//! able to reach its followers several times over inside one election timeout, or followers presume it
//! dead and campaign against a live leader. The paper puts the ratio at an order of magnitude — its
//! example broadcast times are 0.5–20 ms against 10–500 ms timeouts — and the design fixes that at ten
//! ([`ELECTION_MARGIN`]). The broadcast time is measured, per voter path, by the transport's own RFC 9002
//! estimator ([`PathRtt`], fed by every round trip this node times to that voter: its SWIM probe every
//! period on every node, and its consensus rounds when it leads or campaigns), whose probe-timeout form
//! `smoothed + 4 · rttvar` is Jacobson's mean-deviation tail bound (SIGCOMM 1988) — the running form of
//! "RTT p99 × k" the probe deadline already uses (`server/src/fleet.rs`, `ProbeTiming`). The election
//! timeout is then `ELECTION_MARGIN × max(tail over the group's other voters, heartbeat)`: the heartbeat is
//! the coordinator's period, the smallest broadcast time it can observe, so on a loopback — where every
//! round trip sits inside one period (measured 2026-09-13: SWIM p99 17 ms, broadcast p99 33 ms) — the
//! timeout is exactly the ten periods it was before this law existed, and a laptop or LAN fleet is
//! unchanged by construction (R8: the floor is the same derivation at its floor, never a branch). The
//! randomization span (Raft §5.2, §9.3: timers drawn from `[T, T + span)` so co-timed followers do not
//! split the vote) is `ELECTION_MARGIN × max(spread, heartbeat)`, the spread being the same estimator's
//! variation term (the design's "from RTT variance"): on a loopback it is the ten periods it was; on a
//! far path it grows with the path's variation, not with the whole timeout, so the worst-case leader-loss
//! detection is `base + span`, not `2 × base`.
//!
//! The timer counts **coordinator periods**, not wall time, as the daemon's did: each period is at least
//! one heartbeat, so the wall-clock timeout is *at least* the derived one, and a follower whose own shard
//! is starved (its periods stretched) waits longer rather than campaigning on its own slowness — the
//! Lifeguard direction the tree fought for on 2026-09-13
//! (`docs/bugs/2026-09-13-swim-fixed-probe-deadline-kills-a-starved-live-peer.md`).
//!
//! The same measured tail derives the consensus round's collection budget ([`round_budget`]): a round to
//! voters farther away than one period used to expire at three quarters of a period with every reply still
//! in flight, and since a late pre-vote reply is dropped by design, no council whose voters sat more than
//! ~40 ms one way apart could ever elect a leader (`docs/bugs/2026-09-14-consensus-round-expires-inside-the-wan-rtt.md`).
//! Proven on the fabric at 80 ms ± 20 ms one way in `tests/wan_election.rs`.

use slates_db::register::HostId;
use slates_transport::rtt::RttEstimator;

use crate::CommitBudget;

/// Raft's order-of-magnitude ratio of election timeout to broadcast time — "broadcastTime should be an
/// order of magnitude less than the election timeout" (Ongaro & Ousterhout 2014, §5.6; the paper's
/// example ranges are 0.5–20 ms against 10–500 ms) — fixed at ten by the design's rule "election timeout
/// ≥ 10 × broadcast RTT p99" (§4.8 "Derived constants"). It is also the leader's CheckQuorum cadence
/// (Raft §6.2: a leader that hears from no majority within an election timeout steps down) and the cap on
/// a consensus round's progress extensions (one period each, so a round is never extended past the
/// election timeout it would be displacing the leader over).
/// Derived: ten — the paper's order of magnitude, the design's multiplier (§4.8), an anchor not a tunable.
pub const ELECTION_MARGIN: u64 = 10;

/// The measured path to one peer: the transport's RFC 9002 §5.3 estimator over every round trip this node
/// timed to that peer, and how many round trips fed it — the witness that the estimate is measured, not
/// assumed (a path with no sample contributes nothing to a derivation, never the RFC's 333 ms initial
/// guess). Fed by the SWIM probe's acknowledgement each period on every node, and by the consensus
/// rounds' replies — timely and late — on a leader or candidate. Karn's rule is the caller's: only a
/// completed exchange is a sample; a timed-out one is not.
#[derive(Debug, Default)]
pub struct PathRtt {
  estimator: RttEstimator,
  samples: u64,
}

impl PathRtt {
  /// A path no round trip has been timed on yet.
  pub fn new() -> PathRtt {
    PathRtt::default()
  }

  /// Folds one completed round trip in.
  pub fn on_sample(&mut self, round_trip_ns: u64) {
    self.estimator.on_sample(round_trip_ns, 0);
    self.samples = self.samples.saturating_add(1);
  }

  /// How many round trips have fed this path's estimate.
  pub fn samples(&self) -> u64 {
    self.samples
  }

  /// The smoothed round trip (nanoseconds), zero before any sample.
  pub fn smoothed_rtt_ns(&self) -> u64 {
    self.estimator.smoothed_rtt()
  }

  /// The round trip's mean deviation (nanoseconds), zero before any sample.
  pub fn rttvar_ns(&self) -> u64 {
    self.estimator.rttvar()
  }

  /// The tail bound of this path's round trip — `smoothed + max(4 · rttvar, granularity)`, RFC 9002
  /// §6.2.1's probe timeout, Jacobson's mean-deviation bound on the round-trip tail — or `None` before any
  /// sample. The peer answers inline, so no acknowledgement delay is added.
  pub fn tail_ns(&self) -> Option<u64> {
    (self.samples > 0).then(|| self.estimator.pto(0))
  }

  /// The tail's width above the smoothed round trip — `max(4 · rttvar, granularity)`, the estimator's
  /// variation term — or `None` before any sample. The design's "RTT variance", in the tail's own units.
  pub fn spread_ns(&self) -> Option<u64> {
    self
      .tail_ns()
      .map(|tail| tail.saturating_sub(self.estimator.smoothed_rtt()))
  }
}

/// A voter's election priority from its measured paths to the other voters of a group of `voters`
/// (`docs/wip/research/consensus-enhancements.md` §3.4): the `⌊voters/2⌋`-th smallest smoothed round trip
/// among `paths` — a leader commits once a majority including itself holds an entry, so that is the round
/// trip it would commit in — with that path's spread. Unknown (zero) while a sole voter, or while fewer
/// measured paths than that exist: a voter that has not timed its quorum ranks nobody and is ranked by
/// nobody.
pub fn quorum_priority<'a>(
  paths: impl IntoIterator<Item = Option<&'a PathRtt>>,
  voters: usize,
) -> crate::raft::ElectionPriority {
  let quorum = voters / 2;
  let mut measured: Vec<(u64, u64)> = paths
    .into_iter()
    .flatten()
    .filter_map(|path| {
      path
        .spread_ns()
        .map(|spread| (path.smoothed_rtt_ns(), spread))
    })
    .collect();
  measured.sort_unstable();
  quorum
    .checked_sub(1)
    .and_then(|position| measured.get(position))
    .map_or_else(
      crate::raft::ElectionPriority::default,
      |&(quorum_ns, spread_ns)| crate::raft::ElectionPriority {
        quorum_ns,
        spread_ns,
      },
    )
}

/// A configuration group's election timing for one period, derived from the measured paths to its other
/// voters ([`ElectionTiming::derive`]). Periods are the coordinator's (one heartbeat or longer each).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElectionTiming {
  /// The base election timeout in periods: a follower campaigns after this many periods without leader
  /// contact (plus its jitter); a leader judges its quorum every this many.
  pub base_periods: u32,
  /// The randomization span in periods: a node's timeout is `base + jitter`, `jitter ∈ [0, span)`.
  pub span_periods: u32,
  /// The broadcast round-trip tail the base was derived from (nanoseconds) — the slowest measured voter
  /// path's [`PathRtt::tail_ns`]; zero when no voter path has a sample.
  pub broadcast_rtt_tail_ns: u64,
  /// The variation the span was derived from (nanoseconds) — the widest measured voter path's
  /// [`PathRtt::spread_ns`]; zero when no voter path has a sample.
  pub broadcast_rtt_spread_ns: u64,
  /// The round trips measured across the voter paths that fed this derivation — the non-vacuity witness
  /// an observer reads to tell a measured timing from the floor it would default to.
  pub samples: u64,
}

impl ElectionTiming {
  /// The timing with no voter path measured: a laptop's sole voter, or a fleet before its first probe
  /// is acknowledged — [`ELECTION_MARGIN`] periods for both the base and the span, since the heartbeat is
  /// the smallest broadcast time the coordinator can observe.
  pub fn floor() -> ElectionTiming {
    ElectionTiming::derive(1, std::iter::empty())
  }

  /// Derives the timing from the measured `paths` to the group's other voters (§4.8 "Derived constants"):
  /// `base = ELECTION_MARGIN × max(tail over the paths, heartbeat)` and `span = ELECTION_MARGIN ×
  /// max(spread over the paths, heartbeat)`, each rounded up to whole periods of `heartbeat_ns`. The
  /// slowest path bounds the broadcast — a round completes when its last voter answers. A path with no
  /// sample contributes nothing; with none measured the result is the floor.
  pub fn derive<'a>(
    heartbeat_ns: u64,
    paths: impl IntoIterator<Item = &'a PathRtt>,
  ) -> ElectionTiming {
    let heartbeat = heartbeat_ns.max(1);
    let mut tail = 0u64;
    let mut spread = 0u64;
    let mut samples = 0u64;
    for path in paths {
      if let (Some(path_tail), Some(path_spread)) = (path.tail_ns(), path.spread_ns()) {
        tail = tail.max(path_tail);
        spread = spread.max(path_spread);
        samples = samples.saturating_add(path.samples());
      }
    }
    ElectionTiming {
      base_periods: periods_of(
        ELECTION_MARGIN.saturating_mul(tail.max(heartbeat)),
        heartbeat,
      ),
      span_periods: periods_of(
        ELECTION_MARGIN.saturating_mul(spread.max(heartbeat)),
        heartbeat,
      ),
      broadcast_rtt_tail_ns: tail,
      broadcast_rtt_spread_ns: spread,
      samples,
    }
  }

  /// This node's own timeout in periods for its `attempt`-th campaign: `base + (draw mod span)`, the draw a
  /// splitmix64 mix of the id and the attempt — deterministic, so a simulation reproduces from its seed, yet
  /// independent across nodes and across attempts, as Raft's randomized election timeout is (§5.2, §9.3). Two
  /// nodes that draw the same offset on one attempt draw independently on the next, so a collision — both
  /// timing out together, granting each other's pre-vote, and splitting the vote — repeats with probability
  /// about `1/span` per round.
  ///
  /// Until 2026-09-28 the draw was `(local + attempt) mod span`. Every campaign advances both nodes' attempts
  /// together, and a shared increment preserves the difference, so two nodes whose sums were congruent stayed
  /// congruent forever: the survivors of a leader loss split the vote round after round — 19 s on the
  /// multi-region simulation until the cut healed; a permanent loss would never have elected
  /// (`docs/bugs/2026-09-28-correlated-election-jitter-livelocked-a-split-vote.md`).
  pub fn timeout_periods(&self, local: HostId, attempt: u32) -> u32 {
    let span = u64::from(self.span_periods.max(1));
    let draw = splitmix64(local.0 ^ u64::from(attempt).wrapping_mul(GOLDEN_GAMMA));
    let jitter = u32::try_from(draw % span).unwrap_or(0);
    self.base_periods.saturating_add(jitter)
  }
}

/// Format: splitmix64's increment, the odd integer nearest 2^64/φ (Steele, Lea & Flood, *Fast splittable
/// pseudorandom number generators*, OOPSLA 2014) — here it spreads consecutive attempts across the draw.
const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;
/// Format: splitmix64's first finalizer multiplier (Steele, Lea & Flood 2014).
const MIX_ONE: u64 = 0xbf58_476d_1ce4_e5b9;
/// Format: splitmix64's second finalizer multiplier (Steele, Lea & Flood 2014).
const MIX_TWO: u64 = 0x94d0_49bb_1331_11eb;

/// splitmix64's finalizer: a bijective mix of `word` whose outputs are statistically independent across
/// nearby inputs.
fn splitmix64(word: u64) -> u64 {
  let mut z = word.wrapping_add(GOLDEN_GAMMA);
  z = (z ^ (z >> 30)).wrapping_mul(MIX_ONE);
  z = (z ^ (z >> 27)).wrapping_mul(MIX_TWO);
  z ^ (z >> 31)
}

/// `span_ns` in whole periods of `heartbeat_ns`, rounded up, at least one; saturating.
fn periods_of(span_ns: u64, heartbeat_ns: u64) -> u32 {
  let heartbeat = heartbeat_ns.max(1);
  let periods = span_ns.div_ceil(heartbeat).max(1);
  u32::try_from(periods).unwrap_or(u32::MAX)
}

/// A group's election timer as its coordinator counts it — one tick per period, persisting across periods:
/// the follower's age since leader contact, the last contact value it saw, its jitter rotation, and the
/// timeouts it has yielded to more central voters (`docs/wip/research/consensus-enhancements.md` §3.4). The
/// daemon's council and root group each own one; the fabric harness drives the same type.
#[derive(Debug, Default)]
pub struct ElectionTimer {
  idle_periods: u32,
  seen_contact: u64,
  attempt: u32,
  yielded: u32,
}

impl ElectionTimer {
  /// A timer at zero age, no contact seen, first attempt.
  pub fn new() -> ElectionTimer {
    ElectionTimer::default()
  }

  /// A **follower's** period. `contact` is the group's leader-contact count (Raft Figure 2's two follower
  /// timer-resets: a leader's append answered, a vote granted); while it advances the age resets, the
  /// yielded timeouts clear, and no campaign runs. Otherwise the timer ages one period and, once at this
  /// node's jittered timeout under `timing` ([`ElectionTiming::timeout_periods`]), rotates the attempt and
  /// resets the age — and returns `true`, campaign now, when this node's election `rank` (how many live
  /// voters outrank it, `RaftNode::election_rank`) is within the timeouts it has already yielded since it
  /// last heard a leader. Otherwise it yields this timeout to the more central voters and admits the next
  /// rank (SOFAJRaft's decaying target priority, in ranks): an election waits at most one timeout per live
  /// voter that outranks this one, and none when the best live voter is up. Rank zero — no measurements, or
  /// nothing outranks this node — campaigns at every timeout, exactly as without priorities.
  pub fn follower_period(
    &mut self,
    contact: u64,
    timing: &ElectionTiming,
    local: HostId,
    rank: usize,
  ) -> bool {
    if contact != self.seen_contact {
      self.seen_contact = contact;
      self.idle_periods = 0;
      self.yielded = 0;
      return false;
    }
    self.idle_periods = self.idle_periods.saturating_add(1);
    if self.idle_periods < timing.timeout_periods(local, self.attempt) {
      return false;
    }
    self.attempt = self.attempt.saturating_add(1);
    self.idle_periods = 0;
    if u32::try_from(rank).unwrap_or(u32::MAX) > self.yielded {
      self.yielded = self.yielded.saturating_add(1);
      return false;
    }
    true
  }

  /// The timeouts this follower has yielded to more central voters since it last heard a leader.
  pub fn yielded(&self) -> u32 {
    self.yielded
  }

  /// A **leader's** period: ages one period and returns `true` every `base_periods` — judge the quorum now
  /// (Raft §6.2 CheckQuorum on the election-timeout cadence) — resetting the age.
  pub fn leader_period(&mut self, timing: &ElectionTiming) -> bool {
    self.idle_periods = self.idle_periods.saturating_add(1);
    if self.idle_periods < timing.base_periods.max(1) {
      return false;
    }
    self.idle_periods = 0;
    true
  }

  /// Resets the age — the sole voter's period (it self-elects with no messages), or a role change.
  pub fn reset(&mut self) {
    self.idle_periods = 0;
  }

  /// Re-baselines the contact after a campaign, so the campaign's own vote and append echoes do not
  /// retrigger a fresh one next period: a won election makes this node leader; a lost one waits out the
  /// timer again.
  pub fn rebaseline(&mut self, contact: u64) {
    self.seen_contact = contact;
  }

  /// How many campaigns this timer has fired.
  pub fn attempts(&self) -> u32 {
    self.attempt
  }

  /// Periods since the last leader contact (or the last campaign or quorum check).
  pub fn idle_periods(&self) -> u32 {
    self.idle_periods
  }
}

/// The anchors a consensus round's budget is derived from — the coordinator's own periods, so nothing in
/// the budget is a hidden constant (`server/src/fleet.rs` names each with its derivation).
#[derive(Clone, Copy, Debug)]
pub struct RoundAnchors {
  /// The coordinator's period (the daemon's heartbeat).
  pub heartbeat_ns: u64,
  /// Periods of no new reply after which a round is judged stalled, not merely slow (the SWIM suspicion
  /// span: as long as a peer may go unheard before it is suspected).
  pub stall_periods: u32,
  /// How many times the collection loop polls per period.
  pub polls_per_period: u64,
  /// The lookahead fraction `numerator / denominator` of the deadline at which an extension is first
  /// considered.
  pub lookahead: (u64, u64),
}

/// The consensus round's collection budget for one period (§4.8 "late work"), derived from the same
/// measured tail as the election timing: the **base deadline** is `max(heartbeat, tail)` — a round is
/// given the slowest voter's round-trip tail before it can be judged stalled with no reply at all (the
/// witness has not advanced, so the extender expires it at the lookahead point: a fixed one-period base
/// expired every WAN round at 75 ms with every reply still in flight); the **stall window** is
/// `max(stall_periods × heartbeat, tail)` (a reply within one tail is progress); each **extension** is one
/// heartbeat, up to [`ELECTION_MARGIN`] grants — the election-timeout cap, so a round is never extended past
/// the timeout the election timer would displace the leader over; **polled** `polls_per_period` times a
/// period. With no tail measured, or a tail inside one period, every value is the loopback's — the same
/// budget the daemon ran before this derivation (R8).
pub fn round_budget(anchors: &RoundAnchors, tail_ns: Option<u64>) -> CommitBudget {
  let heartbeat = anchors.heartbeat_ns.max(1);
  let tail = tail_ns.unwrap_or(0);
  let stall = heartbeat.saturating_mul(u64::from(anchors.stall_periods));
  CommitBudget::with_extension(
    heartbeat.max(tail),
    (heartbeat / anchors.polls_per_period.max(1)).max(1),
    anchors.lookahead.0,
    anchors.lookahead.1,
    heartbeat,
    u32::try_from(ELECTION_MARGIN).unwrap_or(u32::MAX),
    stall.max(tail),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A millisecond in nanoseconds, so the samples read as round times.
  const MS: u64 = 1_000_000;
  /// The daemon's heartbeat (its coordinator period), 100 ms.
  const HEARTBEAT: u64 = 100 * MS;
  /// The loopback round trips measured on 2026-09-13 (docs/bugs/2026-09-13-consensus-voters-outside-record-neighbourhood.md):
  /// SWIM p99 17 ms, consensus broadcast p50 11 ms and p99 33 ms.
  const LOOPBACK_SAMPLES_MS: [u64; 4] = [11, 17, 11, 33];
  /// An inter-region path of 80 ms ± 20 ms one way: round trips of 160 ms with a ± 40 ms spread.
  const WAN_SAMPLES_MS: [u64; 6] = [160, 200, 120, 160, 190, 130];
  /// The same path once the estimate has settled: a cycle of round trips within its ± 40 ms spread.
  const WAN_SETTLED_CYCLE_MS: [u64; 6] = [160, 200, 140, 180, 120, 160];

  fn path_of(samples_ms: &[u64]) -> PathRtt {
    let mut path = PathRtt::new();
    for sample in samples_ms {
      path.on_sample(sample * MS);
    }
    path
  }

  /// With no voter path measured — a laptop's sole voter, a fleet before its first acknowledgement — the
  /// timing is the floor: ten periods base and span, nothing measured. R8's N=1 degenerate.
  #[test]
  fn with_no_measured_voter_path_the_timing_is_the_floor() {
    let timing = ElectionTiming::derive(HEARTBEAT, std::iter::empty());
    assert_eq!(timing, ElectionTiming::floor());
    assert_eq!(timing.base_periods, 10);
    assert_eq!(timing.span_periods, 10);
    assert_eq!(timing.samples, 0);
  }

  /// A loopback path whose tail sits inside one heartbeat leaves both the base and the span at the floor —
  /// the LAN differential: the derived timing is the ten periods the daemon ran before, by construction,
  /// with the measurement recorded beside it.
  #[test]
  fn a_path_inside_the_heartbeat_leaves_the_timing_at_the_floor() {
    let lan = path_of(&LOOPBACK_SAMPLES_MS);
    let timing = ElectionTiming::derive(HEARTBEAT, [&lan]);
    assert!(
      timing.broadcast_rtt_tail_ns < HEARTBEAT,
      "the tail is inside one period"
    );
    assert_eq!(timing.base_periods, ElectionTiming::floor().base_periods);
    assert_eq!(timing.span_periods, ElectionTiming::floor().span_periods);
    assert_eq!(timing.samples, 4, "the floor was measured, not defaulted");
  }

  /// A WAN path raises the base to ten times its tail, in whole periods rounded up, while the span follows
  /// the path's variation and stays at the floor when that is inside a period.
  #[test]
  fn a_wan_path_raises_the_base_to_ten_times_its_tail_in_whole_periods() {
    let wan = path_of(&WAN_SAMPLES_MS);
    let timing = ElectionTiming::derive(HEARTBEAT, [&wan]);
    let tail = wan.tail_ns().expect("sampled");
    assert!(tail > HEARTBEAT, "a WAN tail exceeds one period: {tail}");
    assert_eq!(
      timing.base_periods,
      u32::try_from((10 * tail).div_ceil(HEARTBEAT)).unwrap(),
      "base = ⌈10 × tail / heartbeat⌉"
    );
    assert!(
      u64::from(timing.base_periods) * HEARTBEAT >= 10 * tail,
      "the timeout in wall time is at least ten times the tail"
    );
    assert_eq!(timing.broadcast_rtt_tail_ns, tail);
  }

  /// The span follows the path's **variation**: after six samples the RFC 9002 estimator still carries the
  /// first sample's seeded deviation (half the sample), so the spread is above a period and the span is
  /// ten times it in whole periods; once the estimate has settled on the path's real deviation (± 40 ms
  /// round trip, a mean deviation near 20 ms) the variation term sits inside a period and the span returns
  /// to the floor while the base stays at ten times the tail.
  #[test]
  fn the_span_follows_the_paths_variation_and_settles_to_the_floor() {
    let early = path_of(&WAN_SAMPLES_MS);
    let timing = ElectionTiming::derive(HEARTBEAT, [&early]);
    let spread = early.spread_ns().expect("sampled");
    assert!(
      spread > HEARTBEAT,
      "the early variation is above a period: {spread}"
    );
    assert_eq!(
      timing.span_periods,
      u32::try_from((10 * spread).div_ceil(HEARTBEAT)).unwrap(),
      "span = ⌈10 × spread / heartbeat⌉"
    );
    let mut settled = PathRtt::new();
    for sample in WAN_SETTLED_CYCLE_MS.iter().cycle().take(40) {
      settled.on_sample(sample * MS);
    }
    let converged = ElectionTiming::derive(HEARTBEAT, [&settled]);
    let settled_spread = settled.spread_ns().expect("sampled");
    assert!(
      settled_spread < HEARTBEAT,
      "converged variation inside a period: {settled_spread}"
    );
    assert_eq!(converged.span_periods, 10, "so the span is the floor's");
    assert!(converged.base_periods > 10, "while the base stays above it");
  }

  /// The slowest voter path bounds the broadcast — a round completes when its last voter answers — so a
  /// group with one far voter derives from that voter, not from the near ones.
  #[test]
  fn the_slowest_voter_path_bounds_the_broadcast() {
    let near = path_of(&LOOPBACK_SAMPLES_MS);
    let far = path_of(&WAN_SAMPLES_MS);
    let mixed = ElectionTiming::derive(HEARTBEAT, [&near, &far]);
    let far_only = ElectionTiming::derive(HEARTBEAT, [&far]);
    assert_eq!(mixed.base_periods, far_only.base_periods);
    assert_eq!(mixed.broadcast_rtt_tail_ns, far.tail_ns().unwrap());
    assert_eq!(mixed.samples, near.samples() + far.samples());
  }

  /// A voter path with no sample contributes nothing — never the estimator's 333 ms initial guess, which
  /// would put a fresh fleet's timeout at 67 periods before its first probe is answered.
  #[test]
  fn a_voter_without_a_sample_contributes_nothing() {
    let fresh = PathRtt::new();
    let lan = path_of(&LOOPBACK_SAMPLES_MS);
    let timing = ElectionTiming::derive(HEARTBEAT, [&fresh, &lan]);
    assert_eq!(timing.base_periods, 10);
    assert_eq!(fresh.tail_ns(), None, "no sample, no tail");
  }

  /// Ages a follower timer `periods` times with `contact` unchanged and returns whether any period fired.
  fn ages_without_firing(
    timer: &mut ElectionTimer,
    contact: u64,
    timing: &ElectionTiming,
    local: HostId,
    periods: u32,
  ) -> bool {
    (0..periods).all(|_| !timer.follower_period(contact, timing, local, 0))
  }

  /// A follower campaigns once it has aged past its jittered timeout with no leader contact — the base and
  /// its per-attempt draw in `[0, span)` — and the campaign advances its attempt.
  #[test]
  fn a_follower_campaigns_after_its_jittered_timeout() {
    let timing = ElectionTiming::floor();
    let local = HostId(3);
    let timeout = timing.timeout_periods(local, 0);
    assert!(timeout >= timing.base_periods && timeout < timing.base_periods + timing.span_periods);
    let mut timer = ElectionTimer::new();
    assert!(
      ages_without_firing(&mut timer, 0, &timing, local, timeout - 1),
      "inside the timeout nothing fires"
    );
    assert!(
      timer.follower_period(0, &timing, local, 0),
      "the timeout's own period fires"
    );
    assert_eq!(timer.attempts(), 1);
  }

  /// Any advance of the leader-contact count resets a follower's age (Raft Figure 2's timer resets), and the
  /// next campaign waits the next attempt's timeout.
  #[test]
  fn contact_resets_the_follower_and_the_next_attempt_draws_afresh() {
    let timing = ElectionTiming::floor();
    let local = HostId(3);
    let mut timer = ElectionTimer::new();
    let first = timing.timeout_periods(local, 0);
    assert!(ages_without_firing(
      &mut timer,
      0,
      &timing,
      local,
      first - 1
    ));
    assert!(
      timer.follower_period(0, &timing, local, 0),
      "the first campaign"
    );
    assert!(ages_without_firing(&mut timer, 0, &timing, local, 5));
    assert!(
      !timer.follower_period(1, &timing, local, 0),
      "contact advanced: reset"
    );
    assert_eq!(timer.idle_periods(), 0);
    let second = timing.timeout_periods(local, 1);
    assert!(ages_without_firing(
      &mut timer,
      1,
      &timing,
      local,
      second - 1
    ));
    assert!(
      timer.follower_period(1, &timing, local, 0),
      "the second attempt's timeout fires"
    );
  }

  /// Raft §5.2 / §9.3 (`docs/bugs/2026-09-28-correlated-election-jitter-livelocked-a-split-vote.md`): two
  /// nodes' draws are independent across attempts they take together, so a collision does not persist. Over
  /// every pair of 32 ids (small and hashed) and every attempt offset between them (4,960 trials of 64 shared
  /// attempts), independent draws collide about once in `span` (ten at the floor): the mean is 6.4 per 64,
  /// and the largest of 4,960 binomial draws reaches 24 with probability 2.2 × 10⁻⁵ (measured 17, which a fair
  /// draw's maximum reaches with probability 0.49). The draw it replaced, `(id + attempt) mod span`, kept a
  /// colliding pair colliding on all 64.
  #[test]
  fn two_nodes_draws_stay_independent_across_shared_attempts() {
    let timing = ElectionTiming::floor();
    let ids: Vec<HostId> = (1..=16)
      .map(HostId)
      .chain((1..=16).map(|seed: u64| HostId(seed.wrapping_mul(0x2545_f491_4f6c_dd1d))))
      .collect();
    let mut worst = 0;
    let mut total = 0;
    let mut trials = 0;
    for (index, left) in ids.iter().enumerate() {
      for right in ids.iter().skip(index + 1) {
        for offset in 0..timing.span_periods {
          let collisions = (0..64u32)
            .filter(|attempt| {
              timing.timeout_periods(*left, *attempt)
                == timing.timeout_periods(*right, attempt + offset)
            })
            .count();
          worst = worst.max(collisions);
          total += collisions;
          trials += 1;
        }
      }
    }
    assert!(
      worst < 24,
      "a pair collided on {worst} of 64 shared attempts"
    );
    let mean_tenths = total * 10 / trials;
    assert!(
      (54..=74).contains(&mean_tenths),
      "collisions average {mean_tenths} tenths per 64 attempts, not about 64 in 10"
    );
  }

  /// A leader judges its quorum every base period — ten at the floor, the derived base on a WAN.
  #[test]
  fn a_leader_checks_its_quorum_every_base_period() {
    let floor = ElectionTiming::floor();
    let mut timer = ElectionTimer::new();
    let fired: Vec<u32> = (1..=20).filter(|_| timer.leader_period(&floor)).collect();
    assert_eq!(fired.len(), 2, "twice in twenty periods at the floor");
    let wan = ElectionTiming::derive(HEARTBEAT, [&path_of(&WAN_SAMPLES_MS)]);
    let mut timer = ElectionTimer::new();
    let first = (1..=100).find(|_| timer.leader_period(&wan)).unwrap();
    assert_eq!(first, wan.base_periods, "on the WAN, every derived base");
  }

  /// The round budget at the floor is the loopback's — base one period, stall two, ten one-period
  /// extensions — and on a WAN its base and stall window open to the measured tail, so a round is not
  /// judged stalled before its first reply can have arrived.
  #[test]
  fn the_round_budget_opens_to_the_measured_tail_and_is_the_loopbacks_at_the_floor() {
    let anchors = RoundAnchors {
      heartbeat_ns: HEARTBEAT,
      stall_periods: 2,
      polls_per_period: 10,
      lookahead: (3, 4),
    };
    let floor = round_budget(&anchors, None);
    assert_eq!(floor.deadline_ns, HEARTBEAT);
    assert_eq!(floor.poll_interval_ns, HEARTBEAT / 10);
    assert_eq!(floor.max_deadline_ns(), HEARTBEAT + 10 * HEARTBEAT);
    let inside = round_budget(&anchors, path_of(&LOOPBACK_SAMPLES_MS).tail_ns());
    assert_eq!(
      inside.deadline_ns, floor.deadline_ns,
      "a tail inside a period changes nothing"
    );
    let wan = path_of(&WAN_SAMPLES_MS);
    let far = round_budget(&anchors, wan.tail_ns());
    assert_eq!(
      far.deadline_ns,
      wan.tail_ns().unwrap(),
      "the base opens to the tail"
    );
    assert_eq!(
      far.max_deadline_ns(),
      wan.tail_ns().unwrap() + 10 * HEARTBEAT
    );
  }

  /// §3.4's gate: a follower of rank two yields its first two timeouts and campaigns at the third; any
  /// leader contact clears the yielded count, so the next leader loss starts from rank zero again; rank
  /// zero campaigns at every timeout.
  #[test]
  fn a_follower_yields_one_timeout_per_rank() {
    let timing = ElectionTiming::floor();
    let local = HostId(9);
    let mut timer = ElectionTimer::new();
    let mut fired = Vec::new();
    for _ in 0..3 {
      let mut periods = 0;
      loop {
        periods += 1;
        let campaign = timer.follower_period(0, &timing, local, 2);
        if campaign || timer.idle_periods() == 0 {
          fired.push(campaign);
          break;
        }
        assert!(periods < 1_000, "a timeout fires");
      }
    }
    assert_eq!(
      fired,
      vec![false, false, true],
      "two yields, then a campaign"
    );
    assert!(!timer.follower_period(1, &timing, local, 2), "contact");
    assert_eq!(timer.yielded(), 0, "contact clears the yields");
    let mut rank_zero = ElectionTimer::new();
    let mut periods = 0;
    while !rank_zero.follower_period(0, &timing, local, 0) {
      periods += 1;
      assert!(periods < 1_000, "rank zero campaigns at its first timeout");
    }
  }

  /// §3.4's measure: the quorum round trip is the `⌊n/2⌋`-th smallest measured path to the other voters,
  /// with that path's spread — the nearest path of three voters, the second of five — and unknown until
  /// that many paths are measured, or for a sole voter.
  #[test]
  fn the_quorum_round_trip_is_the_majoritys_farthest_path() {
    let path = |rtt_ms: u64| {
      let mut path = PathRtt::new();
      for _ in 0..8 {
        path.on_sample(rtt_ms * 1_000_000);
      }
      path
    };
    let near = path(72);
    let middle = path(162);
    let far = path(262);
    let three = quorum_priority([Some(&middle), Some(&near)], 3);
    assert_eq!(three.quorum_ns, near.smoothed_rtt_ns());
    assert_eq!(Some(three.spread_ns), near.spread_ns());
    let five = quorum_priority([Some(&far), None, Some(&near), Some(&middle)], 5);
    assert_eq!(five.quorum_ns, middle.smoothed_rtt_ns());
    assert_eq!(
      quorum_priority([None, None, Some(&near), None], 5),
      crate::raft::ElectionPriority::default(),
      "one measured path does not reach a majority of five"
    );
    assert_eq!(
      quorum_priority(std::iter::empty(), 1),
      crate::raft::ElectionPriority::default()
    );
  }
}
