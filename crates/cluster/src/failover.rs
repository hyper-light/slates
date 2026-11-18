//! The failover delay as a live output (§4.8 "Derived constants", D-16; A-363): how long a lost owner's objects go
//! unserved, `F = D + E + R`, each term a running estimate this node keeps from what it measures, so `F` follows the
//! fleet it serves and nothing sets it. The daemon's control shard refreshes it every coordinator period and publishes
//! it in its status (`FleetReport::failover`); the waits that bound a failover are to read it (A-363's owed consumers).
//!
//! **Why an output.** `F` was the operator's SLO, a ten-second placeholder, and A-359 planned it as the membership
//! horizon, ten coordinator periods and the recovery budget summed once at boot, 2.9 s. Ada's rule of 2026-11-18: a value
//! computed once at boot is still a constant ("2.9 s is just an arbitrary constant, not a learned or data derived method
//! that adapts"). So each term here is the running estimate of the path it names, fed by the traffic that path carries:
//!
//! - **Detection `D`**, from a peer's last answer to this node's detector condemning it ([`detection`]): SWIM's count of
//!   `2(2m − 1) + 1` periods (round-robin probing, Das, Gupta & Motivala, DSN 2002, §4.3), each at the running tail of
//!   the detector's own measured period, so a starved or far member's `D` grows with its periods and falls back as they
//!   recover. Each pair is Chen, Toueg and Aguilera's NFD-E (IEEE Transactions on Computers 51(5), 2002) over the probe
//!   stream, with Lifeguard's local health (Dadgar, Phillips & Currey, DSN 2018): a member condemns only once another
//!   member has answered it since, and its own wake lateness floors every margin, so both stretch the periods `D` counts.
//!
//!   *The false-suspicion probability is derived, never set.* Each pair's margin `α` is the one that minimizes the
//!   expected unavailability `U = (E(D) + α + η + T_E)/MTBF + T_E·β/η` (hyper-timing's `qos`): a later deadline costs
//!   detection time against every failure the fleet's measured MTBF predicts, an earlier one costs a refutation round
//!   `T_E` each time a live peer's answer comes past it, which happens with probability `β(α)` a probe. `β` is bounded
//!   from the pair's measured lateness alone, by Cantelli's one-sided inequality with the record-indicator share for a
//!   lateness past every one seen (Rényi 1962): `β = u + (1 − u)·V/(V + (α − μ)²)`. In φ-accrual's terms (Hayashibara,
//!   Défago, Yared & Katayama, SRDS 2004) the detector suspects at `φ = −log10 β`, the threshold the minimization chose,
//!   with Cantelli's distribution-free tail in place of the normal fit, which a heavy-tailed stall breaks. Each peer's
//!   `β` is in the daemon's detector report (`DetectorPeerReport::mistake_milli`); slates prices a false condemnation as
//!   a false suspicion, since an owner wrongly retired keeps serving under its lease (`member_plane`, measured
//!   2026-10-12).
//! - **Confirmation `E`**, from the condemnation to the successor holding the committed retirement: the council
//!   leader's death watch holds a member dead for [`ElectionTiming::base_periods`] coordinator periods, Raft's election
//!   rule (Ongaro & Ousterhout, ATC 2014, §5.6) scaled from each voter path's `srtt + 4·rttvar` (RFC 6298 §2;
//!   [`crate::timing`]); the retirement then commits in one round, the slowest voter path's tail, and the successor
//!   installs it the next period. Each period counts at its measured tail ([`Failover::period_began`]), since a starved
//!   coordinator's periods stretch and the watch counts periods, not seconds.
//! - **Recovery `R`**, from the retirement installed to the objects served again: a running estimate (RFC 6298 §2's
//!   estimator, Jacobson's mean-deviation tail, SIGCOMM 1988) of each takeover this node completed, its round begun to
//!   its share confirmed, and of each volume it materialized, adopted to served. Before the first, the recovery budget
//!   the design ratified (`slates_db::replay::RECOVERY_BUDGET_NS`, RAMCloud's second; GAPS §5), as RFC 6298 §2.1 starts
//!   its timer at a stated second until a measurement exists. Every term reports the measurements behind it, so a
//!   reader tells a prior from a measurement.
//!
//! **Laptop.** With `f = 0` there is no peer to detect it and no council to confirm, so `E` is absent, and `D` is the one
//! detector it has: its anchor's supervision, which ends a stuck daemon after the watchdog's window and starts it again
//! (A-232), so `D` is that window ([`Failover::supervised`], A-405). `F` is then the window and the recovery: the same
//! derivation at its degenerate inputs (R8), never a branch; a node whose peers' detector has run a period takes that
//! detector's term instead.
//!
//! What it does not do yet (A-363, owed): the waits that bound a failover (a forward's bound, the location round's cap,
//! a live takeover's fetch, the takeover's attempts) and the lease terms still read the operator's placeholder; each
//! moves to this estimate, or to a φ-accrual detector of its own holder's renewals, in its own change.

use slates_transport::rtt::RttEstimator;

use crate::timing::ElectionTiming;

/// A running estimate of one kind of duration: RFC 6298 §2's estimator (the transport's RFC 9002 §5.3 form with no
/// acknowledgement delay), its smoothed mean and mean deviation folded per sample with gains 1/8 and 1/4, and its tail
/// `smoothed + max(4·deviation, G)` (Jacobson, SIGCOMM 1988; RFC 6298 §2.3's `RTO`), with the samples behind it. Only a
/// completed duration is a sample: one cut short is not (Karn's rule, the caller's).
#[derive(Debug, Default)]
pub struct RunningDuration {
  estimator: RttEstimator,
  samples: u64,
}

impl RunningDuration {
  /// An estimate no duration has been folded into yet.
  pub fn new() -> RunningDuration {
    RunningDuration::default()
  }

  /// Folds one completed duration in.
  pub fn on_sample(&mut self, duration_ns: u64) {
    self.estimator.on_sample(duration_ns, 0);
    self.samples = self.samples.saturating_add(1);
  }

  /// How many durations have been folded in.
  pub fn samples(&self) -> u64 {
    self.samples
  }

  /// The smoothed mean, nanoseconds; zero before any sample.
  pub fn smoothed_ns(&self) -> u64 {
    self.estimator.smoothed_rtt()
  }

  /// The tail bound `smoothed + max(4·deviation, G)`, nanoseconds, or `None` before any sample: what a wait that must
  /// outlast one more such duration waits.
  pub fn tail_ns(&self) -> Option<u64> {
    (self.samples > 0).then(|| self.estimator.pto(0))
  }
}

/// One term of the estimate: its value now, and how many measurements stand behind it (zero: a prior).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Term {
  /// The term, nanoseconds.
  pub ns: u64,
  /// The measurements behind it: round trips for detection, timed coordinator periods for confirmation, completed
  /// recoveries for recovery. Zero where the value is a prior.
  pub samples: u64,
}

/// The detection term (A-363): SWIM's count of the periods from a crashed peer's last answer to its condemnation pending,
/// `2(2m − 1) + 1` for `m` members besides this one, each at the tail of the detector's measured period. Under SWIM's
/// round-robin probing the next probe of a peer is at most `2m − 1` periods away (Das, Gupta & Motivala, DSN 2002,
/// §4.3); unanswered, it suspects the peer; Lifeguard's buddy probe that tells it is at most as far again, and its own
/// period resolves the condemnation (hyper-swim's `Detector::detection_bound` counts the same). hyper-swim's bound counts
/// each period at the longest the member ever ran, a ratchet that one slow period sets for good: on a quiet three-node
/// loopback fleet it read 7.0 and 14.0 s before any loss where other runs read 0.9 s (2026-11-18, the Mac Studio). The
/// running tail forgets a slow period as later ones run at their pace, so the term keeps learning. `None` with no member
/// besides this one (nothing to detect), or before a period was timed.
pub fn detection(members_besides: u64, periods: &RunningDuration) -> Option<Term> {
  if members_besides == 0 {
    return None;
  }
  let count = members_besides
    .saturating_mul(2)
    .saturating_sub(1)
    .saturating_mul(2)
    .saturating_add(1);
  periods.tail_ns().map(|tail| Term {
    ns: count.saturating_mul(tail),
    samples: periods.samples(),
  })
}

/// The recovery's two running estimates: per takeover (a retired host's round begun to this node's share of it
/// confirmed) and per volume (a taken-over volume adopted to served).
#[derive(Debug, Default)]
pub struct Recovery {
  takeovers: RunningDuration,
  volumes: RunningDuration,
}

impl Recovery {
  /// The estimates with nothing timed yet.
  pub fn new() -> Recovery {
    Recovery::default()
  }

  /// A takeover this node took part in confirmed its share after `duration_ns`.
  pub fn takeover_done(&mut self, duration_ns: u64) {
    self.takeovers.on_sample(duration_ns);
  }

  /// A taken-over volume was served `duration_ns` after its adoption.
  pub fn volume_served(&mut self, duration_ns: u64) {
    self.volumes.on_sample(duration_ns);
  }

  /// The per-takeover estimate.
  pub fn takeovers(&self) -> &RunningDuration {
    &self.takeovers
  }

  /// The per-volume estimate.
  pub fn volumes(&self) -> &RunningDuration {
    &self.volumes
  }

  /// The recovery term: the larger tail of the two estimates once either has a sample, with the completed recoveries
  /// behind it; else `prior_ns` with none. A takeover's share is confirmed only once its volumes are served, so its
  /// tail is normally the larger; a volume a mirror's promotion materialized has no takeover round, and counts alone.
  pub fn term(&self, prior_ns: u64) -> Term {
    let measured = self.takeovers.tail_ns().max(self.volumes.tail_ns());
    measured.map_or(
      Term {
        ns: prior_ns,
        samples: 0,
      },
      |ns| Term {
        ns,
        samples: self
          .takeovers
          .samples()
          .saturating_add(self.volumes.samples()),
      },
    )
  }
}

/// The confirmation term: the council's death-confirmation window at the coordinator's measured period. The death
/// watch holds a member dead for `timing.base_periods` periods, floored as the watch floors it
/// ([`ElectionTiming::floor`]); the successor installs the committed retirement one period later; and the commit is one
/// round, the slowest voter path's tail. A period counts at the measured period's tail, never below `heartbeat_ns`, the
/// period the coordinator paces itself to; before any period is timed, at `heartbeat_ns`.
pub fn confirmation(timing: &ElectionTiming, periods: &RunningDuration, heartbeat_ns: u64) -> Term {
  let period_ns = periods.tail_ns().unwrap_or(heartbeat_ns).max(heartbeat_ns);
  let watched = u64::from(
    timing
      .base_periods
      .max(ElectionTiming::floor().base_periods),
  );
  let installed = watched.saturating_add(1);
  Term {
    ns: installed
      .saturating_mul(period_ns)
      .saturating_add(timing.broadcast_rtt_tail_ns),
    samples: periods.samples(),
  }
}

/// The failover estimate (§4.8 "Derived constants"; A-363): each term with the measurements behind it, and their sum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailoverEstimate {
  /// Detection, `None` where there is no peer to detect (a laptop, or a fleet node before its detector's first period).
  pub detection: Option<Term>,
  /// Confirmation, `None` where there is no council to confirm (a laptop).
  pub confirmation: Option<Term>,
  /// Recovery.
  pub recovery: Term,
}

impl FailoverEstimate {
  /// The failover delay: detection, confirmation and recovery, summed.
  pub fn total_ns(&self) -> u64 {
    self
      .detection
      .map_or(0, |term| term.ns)
      .saturating_add(self.confirmation.map_or(0, |term| term.ns))
      .saturating_add(self.recovery.ns)
  }
}

/// This node's failover estimator: the coordinator's period and the recoveries it times, and the detection and
/// confirmation terms as the last period derived them. The control shard owns one.
#[derive(Debug)]
pub struct Failover {
  periods: RunningDuration,
  last_period_began_ns: Option<u64>,
  recovery: Recovery,
  detection: Option<Term>,
  confirmation: Option<Term>,
  /// The detection a node has with no peer's detector to judge it: its anchor's supervision window ([`Failover::supervised`]).
  supervision: Option<Term>,
}

impl Default for Failover {
  fn default() -> Failover {
    Failover::new()
  }
}

impl Failover {
  /// An estimator with nothing measured: no detection, no confirmation, and the recovery's prior.
  pub fn new() -> Failover {
    Failover {
      periods: RunningDuration::new(),
      last_period_began_ns: None,
      recovery: Recovery::new(),
      detection: None,
      confirmation: None,
      supervision: None,
    }
  }

  /// This estimator for a node its anchor supervises within `window_ns` (A-405): a stuck daemon is ended by its watchdog
  /// after that window and started again by its anchor (A-232), so where no peer's detector judges the node (a laptop, a
  /// fleet node before its detector's first period) its detection term is that window, with no measurement behind it.
  #[must_use]
  pub fn supervised(mut self, window_ns: u64) -> Failover {
    self.supervision = Some(Term {
      ns: window_ns,
      samples: 0,
    });
    self
  }

  /// A coordinator period began at `now_ns`: the time since the one before is a sample of the period's length. A clock
  /// that read earlier than the last (none on one host's monotonic clock) is no sample.
  pub fn period_began(&mut self, now_ns: u64) {
    if let Some(last) = self.last_period_began_ns
      && let Some(length) = now_ns.checked_sub(last)
    {
      self.periods.on_sample(length);
    }
    self.last_period_began_ns = Some(now_ns);
  }

  /// A takeover this node took part in confirmed its share after `duration_ns`.
  pub fn takeover_done(&mut self, duration_ns: u64) {
    self.recovery.takeover_done(duration_ns);
  }

  /// A taken-over volume was served `duration_ns` after its adoption.
  pub fn volume_served(&mut self, duration_ns: u64) {
    self.recovery.volume_served(duration_ns);
  }

  /// Derives this period's detection and confirmation terms: `detection` from the detector ([`detection`]), the
  /// confirmation from the council's `timing` at the measured period ([`confirmation`]). Returns the estimate.
  pub fn refresh(
    &mut self,
    detection: Option<Term>,
    timing: &ElectionTiming,
    heartbeat_ns: u64,
  ) -> FailoverEstimate {
    self.detection = detection;
    self.confirmation = Some(confirmation(timing, &self.periods, heartbeat_ns));
    self.estimate()
  }

  /// The estimate now: the last period's detection and confirmation, and the recovery as timed so far, before any
  /// timing the ratified recovery budget (`slates_db::replay::RECOVERY_BUDGET_NS`).
  pub fn estimate(&self) -> FailoverEstimate {
    FailoverEstimate {
      detection: self.detection.or(self.supervision),
      confirmation: self.confirmation,
      recovery: self.recovery.term(slates_db::replay::RECOVERY_BUDGET_NS),
    }
  }

  /// The recoveries timed.
  pub fn recovery(&self) -> &Recovery {
    &self.recovery
  }

  /// The coordinator's periods timed.
  pub fn periods(&self) -> &RunningDuration {
    &self.periods
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::timing::PathRtt;

  /// Format: a coordinator period of a tenth of a second, the daemon's heartbeat on any machine whose recovery budget is
  /// a second (`slates-server` `HEARTBEAT_NS`).
  const HEARTBEAT: u64 = 100_000_000;

  /// Do: ask an estimator that has measured nothing. Expect no detection, no confirmation, the recovery at its prior
  /// with no measurement behind it, and the failover delay that prior alone: the laptop's estimate (R8).
  #[test]
  fn an_estimator_that_measured_nothing_is_the_recovery_prior_alone() {
    let failover = Failover::new();
    let estimate = failover.estimate();
    assert_eq!(estimate.detection, None);
    assert_eq!(estimate.confirmation, None);
    assert_eq!(
      estimate.recovery,
      Term {
        ns: slates_db::replay::RECOVERY_BUDGET_NS,
        samples: 0,
      }
    );
    assert_eq!(estimate.total_ns(), slates_db::replay::RECOVERY_BUDGET_NS);
  }

  /// Do: time one takeover of 400 ms, then three more of 400 ms. Expect the recovery to leave its prior at the first,
  /// at RFC 6298 §2.2's first-sample tail (`R + 4·R/2 = 3R`, 1.2 s), and to fall toward the measured 400 ms as the
  /// deviation decays, the samples counted: the estimate learns from what it times.
  #[test]
  fn the_recovery_follows_the_takeovers_it_times() {
    let mut failover = Failover::new();
    failover.takeover_done(400_000_000);
    let first = failover.estimate().recovery;
    assert_eq!(
      first,
      Term {
        ns: 1_200_000_000,
        samples: 1,
      }
    );
    for _ in 0..3 {
      failover.takeover_done(400_000_000);
    }
    let later = failover.estimate().recovery;
    assert_eq!(later.samples, 4);
    assert!(
      later.ns < first.ns && later.ns > 400_000_000,
      "the tail falls toward the steady 400 ms as its deviation decays: {later:?}"
    );
  }

  /// Do: time a volume served 2 s after its adoption with no takeover round timed (a mirror's promotion), then a
  /// takeover of 300 ms. Expect the recovery to take the volume's tail alone first, and to keep the larger tail once
  /// both have samples, with both counted.
  #[test]
  fn the_recovery_takes_the_larger_of_the_takeover_and_volume_tails() {
    let mut recovery = Recovery::new();
    recovery.volume_served(2_000_000_000);
    let volume_only = recovery.term(slates_db::replay::RECOVERY_BUDGET_NS);
    assert_eq!(volume_only.ns, 6_000_000_000, "3R at the first sample");
    assert_eq!(volume_only.samples, 1);
    recovery.takeover_done(300_000_000);
    let both = recovery.term(slates_db::replay::RECOVERY_BUDGET_NS);
    assert_eq!(both.ns, 6_000_000_000, "the volume's tail is the larger");
    assert_eq!(both.samples, 2);
  }

  /// Do: refresh a fresh estimator under the floor timing (no voter path measured) with no period timed yet. Expect the
  /// confirmation to be the death watch's ten periods plus the install's one at the heartbeat, nothing measured behind
  /// it: eleven periods.
  #[test]
  fn the_confirmation_is_the_death_watch_and_the_install_at_the_heartbeat_before_any_period() {
    let mut failover = Failover::new();
    let estimate = failover.refresh(None, &ElectionTiming::floor(), HEARTBEAT);
    assert_eq!(
      estimate.confirmation,
      Some(Term {
        ns: 11 * HEARTBEAT,
        samples: 0,
      })
    );
  }

  /// Do: time coordinator periods stretched to 300 ms (a starved control shard), then refresh. Expect the confirmation
  /// to count each of its eleven periods at the measured tail, past three times the heartbeat's eleven: the watch
  /// counts periods, so a starved coordinator confirms later, and the estimate says so.
  #[test]
  fn the_confirmation_stretches_with_the_measured_period() {
    let mut failover = Failover::new();
    let mut now = 0u64;
    for _ in 0..16 {
      failover.period_began(now);
      now = now.saturating_add(3 * HEARTBEAT);
    }
    let estimate = failover.refresh(None, &ElectionTiming::floor(), HEARTBEAT);
    let confirmation = estimate.confirmation.expect("a council confirms");
    assert_eq!(confirmation.samples, 15);
    assert!(
      confirmation.ns >= 11 * 3 * HEARTBEAT,
      "each period at its measured length or more: {confirmation:?}"
    );
  }

  /// Do: refresh under the timing a far voter path derives (round trips of 400 ms, so the election base is ten times
  /// their tail), against the floor's. Expect the far council's confirmation to wait its larger base and its commit
  /// round: Raft's rule scaled from the measured path (RFC 6298's `srtt + 4·rttvar`).
  #[test]
  fn the_confirmation_scales_with_the_measured_voter_paths() {
    let mut far_path = PathRtt::new();
    for _ in 0..8 {
      far_path.on_sample(400_000_000);
    }
    let far = ElectionTiming::derive(HEARTBEAT, [&far_path]);
    assert!(far.base_periods > ElectionTiming::floor().base_periods);
    let periods = RunningDuration::new();
    let near = confirmation(&ElectionTiming::floor(), &periods, HEARTBEAT);
    let measured = confirmation(&far, &periods, HEARTBEAT);
    assert_eq!(
      measured.ns,
      (u64::from(far.base_periods) + 1) * HEARTBEAT + far.broadcast_rtt_tail_ns
    );
    assert!(measured.ns > near.ns);
  }

  /// Do: refresh with a detection term of 900 ms on 12 periods and a timed takeover of 500 ms. Expect the failover delay
  /// to be the three terms summed, each with its measurements, and a detector that ran no period to contribute nothing.
  #[test]
  fn the_failover_delay_is_the_sum_of_its_terms() {
    let mut failover = Failover::new();
    failover.takeover_done(500_000_000);
    let detected = Some(Term {
      ns: 900_000_000,
      samples: 12,
    });
    let estimate = failover.refresh(detected, &ElectionTiming::floor(), HEARTBEAT);
    assert_eq!(
      estimate.detection,
      Some(Term {
        ns: 900_000_000,
        samples: 12,
      })
    );
    let confirmation = estimate.confirmation.expect("a council confirms");
    assert_eq!(
      estimate.total_ns(),
      900_000_000 + confirmation.ns + 1_500_000_000
    );
    let undetected = failover.refresh(None, &ElectionTiming::floor(), HEARTBEAT);
    assert_eq!(undetected.detection, None);
    assert_eq!(
      undetected.total_ns(),
      confirmation.ns + 1_500_000_000,
      "a detector with no period adds nothing"
    );
  }

  /// Do: count detection for two members besides this one over periods timed at 128 ms. Expect SWIM's seven periods
  /// (`2(2m − 1) + 1`) at the period's tail, the periods counted; and nothing to detect with no member besides this one,
  /// or before a period was timed.
  #[test]
  fn detection_counts_swims_periods_at_the_measured_period() {
    let mut periods = RunningDuration::new();
    assert_eq!(detection(2, &periods), None, "no period timed yet");
    for _ in 0..32 {
      periods.on_sample(128_000_000);
    }
    let tail = periods.tail_ns().expect("periods timed");
    assert_eq!(
      detection(2, &periods),
      Some(Term {
        ns: 7 * tail,
        samples: 32,
      })
    );
    assert_eq!(detection(0, &periods), None, "no member to detect");
    assert_eq!(
      detection(4, &periods).map(|term| term.ns),
      Some(15 * tail),
      "four members besides: fifteen periods"
    );
  }

  /// Do: time one detector period that ran 2 s (a stall at formation), then periods of 128 ms. Expect the detection term
  /// to stand far above its steady value after the stall, then to fall back within a few periods' worth of samples to
  /// under twice the steady seven periods: a running estimate forgets what the detector's own bound keeps for good.
  #[test]
  fn detection_forgets_a_period_that_ran_long_once() {
    let mut periods = RunningDuration::new();
    for _ in 0..16 {
      periods.on_sample(128_000_000);
    }
    periods.on_sample(2_000_000_000);
    let stalled = detection(2, &periods).expect("periods timed").ns;
    assert!(
      stalled > 7 * 1_000_000_000,
      "the stall shows at once: {stalled}"
    );
    for _ in 0..32 {
      periods.on_sample(128_000_000);
    }
    let recovered = detection(2, &periods).expect("periods timed").ns;
    assert!(
      recovered < 2 * 7 * 128_000_000,
      "the stall is forgotten as periods run at their pace: {recovered}"
    );
  }

  /// A-405. Do: ask a supervised estimator with no peer detection, then refresh it with a detector's term, then with
  /// none again. Expect the supervision window as its detection (the laptop's degenerate: its anchor detects it), its
  /// failover delay that window and the recovery prior; the detector's own term in its place once measured; and the
  /// supervision again when no peer detects.
  #[test]
  fn a_node_no_peer_detects_is_detected_by_its_supervision_window() {
    let window = 10 * HEARTBEAT;
    let mut failover = Failover::new().supervised(window);
    let laptop = failover.estimate();
    assert_eq!(
      laptop.detection,
      Some(Term {
        ns: window,
        samples: 0,
      })
    );
    assert_eq!(laptop.confirmation, None);
    assert_eq!(
      laptop.total_ns(),
      window + slates_db::replay::RECOVERY_BUDGET_NS
    );
    let peers = Some(Term {
      ns: 900_000_000,
      samples: 7,
    });
    assert_eq!(
      failover
        .refresh(peers, &ElectionTiming::floor(), HEARTBEAT)
        .detection,
      peers
    );
    assert_eq!(
      failover
        .refresh(None, &ElectionTiming::floor(), HEARTBEAT)
        .detection,
      Some(Term {
        ns: window,
        samples: 0,
      })
    );
  }

  /// Do: feed a period clock that reads earlier than the last. Expect no sample: a duration is never negative.
  #[test]
  fn a_period_clock_that_reads_earlier_is_no_sample() {
    let mut failover = Failover::new();
    failover.period_began(5 * HEARTBEAT);
    failover.period_began(4 * HEARTBEAT);
    assert_eq!(failover.periods().samples(), 0);
    failover.period_began(5 * HEARTBEAT);
    assert_eq!(failover.periods().samples(), 1);
  }
}
