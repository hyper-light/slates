//! BBR (draft-ietf-ccwg-bbr-06, B — "BBRv3"): a model-based congestion controller — one of the three
//! control laws in the session plane's congestion bake-off (see the module above). It estimates the
//! path's bottleneck bandwidth (a windowed maximum of delivery-rate samples) and round-trip propagation
//! delay (a windowed minimum RTT), paces at the bandwidth, and caps what is in flight near their product,
//! so the bottleneck queue stays near empty — the property that keeps a small request's wait short on a
//! thin link (research note §5.3). Loss bounds the model rather than driving it: a round with more than
//! 2 % loss while probing lowers the long-term inflight bound, and loss outside probing lowers the
//! short-term bounds by β = 0.7 per round.
//!
//! Each routine below is the draft's pseudocode of the same name, in the draft's order, with the draft's
//! state names (`BBR.*` fields here, `C.*` from the connection's events). Rates are bytes per second,
//! volumes bytes, times nanoseconds, gains per mille — integer arithmetic throughout, so a simulated
//! history replays exactly.

use slates_machine::stats::Xorshift;

use super::filter::WindowedMax;
use super::{AckEvent, INITIAL_WINDOW_DATAGRAMS, LossEvent, PERMILLE};
use crate::conn::SentPacket;
use crate::delivery::{self, RateSample};

/// Format: draft §2.5 `BBR.StartupPacingGain` = 4·ln 2 ≈ 2.77, per mille.
const STARTUP_PACING_GAIN: u64 = 2770;
/// Format: draft §2.5 `BBR.DrainPacingGain` = 0.5, per mille.
const DRAIN_PACING_GAIN: u64 = 500;
/// Format: draft §2.6 `BBR.DefaultCwndGain` = 2, per mille.
const DEFAULT_CWND_GAIN: u64 = 2000;
/// Format: draft §5.3.3.5 — ProbeBW_UP's cwnd gain, 2.25, per mille.
const PROBE_UP_CWND_GAIN: u64 = 2250;
/// Format: draft §5.3.3.1 — ProbeBW_DOWN's pacing gain, 0.90, per mille.
const PROBE_DOWN_PACING_GAIN: u64 = 900;
/// Format: draft §5.3.3.5 — ProbeBW_UP's pacing gain, 1.25, per mille.
const PROBE_UP_PACING_GAIN: u64 = 1250;
/// Format: a gain of one, per mille.
const UNITY_GAIN: u64 = PERMILLE;
/// Format: draft §2.16.2 `BBR.ProbeRTTCwndGain` = 0.5, per mille.
const PROBE_RTT_CWND_GAIN: u64 = 500;
/// Format: draft §2.5 `BBR.PacingMarginPercent` = 1 %, as the per-mille factor kept (99 %).
const PACING_MARGIN_KEEP: u64 = 990;
/// Format: draft §2.8 `BBR.LossThresh` = 2 %, per mille.
const LOSS_THRESH: u64 = 20;
/// Format: draft §2.8 `BBR.Beta` = 0.7, per mille.
const BETA: u64 = 700;
/// Format: draft §2.8 `BBR.Headroom` = 0.15, per mille.
const HEADROOM: u64 = 150;
/// Format: draft §2.8 `BBR.MinPipeCwnd` = 4 datagrams.
const MIN_PIPE_CWND_DATAGRAMS: u64 = 4;
/// Format: draft §5.3.1.2 — bandwidth still growing means at least 25 % more per round, per mille.
const FULL_BW_GROWTH: u64 = 1250;
/// Format: draft §5.3.1.2 — three rounds without that growth fill the pipe.
const FULL_BW_COUNT: u64 = 3;
/// Format: draft §5.3.1.3 `BBRStartupFullLossCnt` — six discontiguous lost ranges in a round.
const STARTUP_FULL_LOSS_COUNT: u64 = 6;
/// Format: draft §5.3.2 — Drain gives up after three rounds.
const DRAIN_MAX_ROUNDS: u64 = 3;
/// Format: draft §2.11 `BBR.MaxBwFilterLen` = 2 ProbeBW cycles.
const MAX_BW_FILTER_LEN: u64 = 2;
/// Format: draft §2.12 `BBR.ExtraAckedFilterLen` = 10 rounds.
const EXTRA_ACKED_FILTER_LEN: u64 = 10;
/// Format: draft §2.16.1 `BBR.MinRTTFilterLen` = 10 s.
const MIN_RTT_FILTER_LEN_NS: u64 = 10_000_000_000;
/// Format: draft §2.16.2 `BBR.ProbeRTTDuration` = 200 ms.
const PROBE_RTT_DURATION_NS: u64 = 200_000_000;
/// Format: draft §2.16.2 `BBR.ProbeRTTInterval` = 5 s.
const PROBE_RTT_INTERVAL_NS: u64 = 5_000_000_000;
/// Format: draft §5.3.3.8.3 — `T_bbr`'s floor, 2 s.
const PROBE_WAIT_BASE_NS: u64 = 2_000_000_000;
/// Format: draft §5.3.3.8.3 — `T_bbr`'s random span, up to 1 s more.
const PROBE_WAIT_SPAN_NS: u64 = 1_000_000_000;
/// Format: draft §5.3.3.8 `IsRenoCoexistenceProbeTime` — the Reno-coexistence round bound, 63.
const RENO_ROUND_BOUND: u64 = 63;
/// Format: draft §5.3.3.9 `RaiseInflightLongtermSlope` — the growth doubling is capped at 2^30.
const MAX_PROBE_UP_ROUNDS: u32 = 30;
/// Format: draft §5.6.2 `InitPacingRate` — the RTT assumed before any sample, 1 ms.
const INITIAL_PACING_RTT_NS: u64 = 1_000_000;

/// The BBR state machine's states (draft §5.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
  /// Startup: doubling each round.
  Startup,
  /// Drain: emptying the queue Startup built.
  Drain,
  /// ProbeBW_DOWN: sending slower than delivery.
  ProbeBwDown,
  /// ProbeBW_CRUISE: sending at delivery.
  ProbeBwCruise,
  /// ProbeBW_REFILL: refilling the pipe before probing.
  ProbeBwRefill,
  /// ProbeBW_UP: probing for more bandwidth.
  ProbeBwUp,
  /// ProbeRTT: draining to measure the propagation delay.
  ProbeRtt,
}

/// The meaning of the ACK feedback being received (draft §2.14 `BBR.ack_phase`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AckPhase {
  Init,
  Refilling,
  ProbeStarting,
  ProbeFeedback,
  ProbeStopping,
}

/// BBR's state (draft §2).
#[derive(Debug)]
pub struct Bbr {
  smss: u64,
  initial_cwnd: u64,
  rng: Xorshift,
  // Output control parameters (§2.4).
  cwnd: u64,
  pacing_rate: u64,
  send_quantum: u64,
  // Pacing and cwnd gains (§2.5, §2.6).
  pacing_gain: u64,
  cwnd_gain: u64,
  // General algorithm state (§2.7).
  state: State,
  round_count: u64,
  round_start: bool,
  next_round_delivered: u64,
  idle_restart: bool,
  drain_start_round: u64,
  // Data rate model (§2.9.1).
  max_bw: u64,
  bw_shortterm: Option<u64>,
  bw: u64,
  // Data volume model (§2.9.2).
  min_rtt: Option<u64>,
  min_rtt_stamp: u64,
  bdp: u64,
  extra_acked: u64,
  offload_budget: u64,
  max_inflight: u64,
  inflight_longterm: Option<u64>,
  inflight_shortterm: Option<u64>,
  // Congestion signals (§2.10).
  bw_latest: u64,
  inflight_latest: u64,
  loss_round_delivered: u64,
  loss_round_start: bool,
  is_loss_in_round: bool,
  // Max bw filter (§2.11).
  max_bw_filter: WindowedMax,
  cycle_count: u64,
  // Extra acked (§2.12).
  extra_acked_interval_start: u64,
  extra_acked_delivered: u64,
  extra_acked_filter: WindowedMax,
  // Startup (§2.13).
  full_bw_reached: bool,
  full_bw_now: bool,
  full_bw: u64,
  full_bw_count: u64,
  // ProbeBW (§2.14).
  ack_phase: AckPhase,
  is_bw_probe_sample: bool,
  bw_probe_up_acked: u64,
  probe_up_acked_per_inc: Option<u64>,
  bw_probe_up_rounds: u32,
  rounds_since_probe_up: u64,
  bw_probe_wait: u64,
  cycle_stamp: u64,
  prev_probe_too_high: bool,
  prev_probe_precautionary: bool,
  // Undo (§2.15).
  undo_state: Option<State>,
  undo_bw_shortterm: Option<u64>,
  undo_inflight_shortterm: Option<u64>,
  undo_inflight_longterm: Option<u64>,
  prior_cwnd: u64,
  // ProbeRTT (§2.16).
  probe_rtt_done_stamp: Option<u64>,
  probe_rtt_round_done: bool,
  probe_rtt_min_delay: Option<u64>,
  probe_rtt_min_stamp: u64,
  probe_rtt_expired: bool,
  // Loss recovery, as the transport tracks it (§5.6.4.4): the send time recovery began at, and the round
  // it began in, and Startup's per-round loss accounting (§5.3.1.3).
  recovery_start: Option<u64>,
  recovery_round: u64,
  lost_at_round_start: u64,
  delivered_at_round_start: u64,
  loss_ranges_in_round: u64,
  last_lost_pn: Option<u64>,
  // The connection's delivered and lost bytes at the last event (`C.delivered`, `C.lost`).
  delivered: u64,
  lost: u64,
  inflight: u64,
  is_cwnd_limited: bool,
  /// The clock reading of the event being processed (ProbeBW's transitions stamp it as `cycle_stamp`).
  event_now: u64,
  /// Whether an RTT sample has been seen (the pacing rate is re-initialized from the first one, §5.6.2).
  has_seen_rtt: bool,
  /// Set by ProbeRTT's `MarkConnectionAppLimited` (§5.3.4.3) for the connection to apply to its
  /// delivery-rate sampler, which owns `C.app_limited`.
  app_limited_request: bool,
}

impl Bbr {
  /// `OnInit` (draft §5.2.1), at `now`, datagrams of `smss` bytes, randomized timing from `seed`.
  pub fn new(smss: u64, now: u64, seed: u64) -> Bbr {
    let initial_cwnd = INITIAL_WINDOW_DATAGRAMS.saturating_mul(smss);
    let mut bbr = Bbr {
      smss,
      initial_cwnd,
      rng: Xorshift::new(seed),
      cwnd: initial_cwnd,
      pacing_rate: 0,
      send_quantum: 0,
      pacing_gain: STARTUP_PACING_GAIN,
      cwnd_gain: DEFAULT_CWND_GAIN,
      state: State::Startup,
      round_count: 0,
      round_start: false,
      next_round_delivered: 0,
      idle_restart: false,
      drain_start_round: 0,
      max_bw: 0,
      bw_shortterm: None,
      bw: 0,
      min_rtt: None,
      min_rtt_stamp: now,
      bdp: 0,
      extra_acked: 0,
      offload_budget: 0,
      max_inflight: 0,
      inflight_longterm: None,
      inflight_shortterm: None,
      bw_latest: 0,
      inflight_latest: 0,
      loss_round_delivered: 0,
      loss_round_start: false,
      is_loss_in_round: false,
      max_bw_filter: WindowedMax::new(0, 0),
      cycle_count: 0,
      extra_acked_interval_start: now,
      extra_acked_delivered: 0,
      extra_acked_filter: WindowedMax::new(0, 0),
      full_bw_reached: false,
      full_bw_now: false,
      full_bw: 0,
      full_bw_count: 0,
      ack_phase: AckPhase::Init,
      is_bw_probe_sample: false,
      bw_probe_up_acked: 0,
      probe_up_acked_per_inc: None,
      bw_probe_up_rounds: 0,
      rounds_since_probe_up: 0,
      bw_probe_wait: 0,
      cycle_stamp: 0,
      prev_probe_too_high: false,
      prev_probe_precautionary: false,
      undo_state: None,
      undo_bw_shortterm: None,
      undo_inflight_shortterm: None,
      undo_inflight_longterm: None,
      prior_cwnd: 0,
      probe_rtt_done_stamp: None,
      probe_rtt_round_done: false,
      probe_rtt_min_delay: None,
      probe_rtt_min_stamp: now,
      probe_rtt_expired: false,
      recovery_start: None,
      recovery_round: 0,
      lost_at_round_start: 0,
      delivered_at_round_start: 0,
      loss_ranges_in_round: 0,
      last_lost_pn: None,
      delivered: 0,
      lost: 0,
      inflight: 0,
      is_cwnd_limited: false,
      event_now: now,
      has_seen_rtt: false,
      app_limited_request: false,
    };
    bbr.reset_congestion_signals();
    bbr.reset_short_term_model();
    bbr.reset_full_bw();
    bbr.init_pacing_rate(None);
    bbr.enter_startup();
    bbr.send_quantum = super::send_quantum_for(bbr.pacing_rate, smss);
    bbr
  }

  /// Reseeds the randomized probe timing.
  pub fn reseed(&mut self, seed: u64) {
    self.rng = Xorshift::new(seed);
  }

  /// The congestion window, bytes.
  pub fn window(&self) -> u64 {
    self.cwnd
  }

  /// The pacing rate, bytes per second.
  pub fn pacing_rate(&self) -> u64 {
    self.pacing_rate
  }

  /// The send quantum, bytes.
  pub fn send_quantum(&self) -> u64 {
    self.send_quantum
  }

  /// The datagram size.
  pub fn max_datagram(&self) -> u64 {
    self.smss
  }

  /// Whether the flow is in Startup.
  pub fn in_startup(&self) -> bool {
    self.state == State::Startup
  }

  /// Whether ProbeRTT asked for the connection to be marked application-limited since the last call
  /// (`MarkConnectionAppLimited`, §5.3.4.3), clearing the request.
  pub fn take_app_limited(&mut self) -> bool {
    std::mem::take(&mut self.app_limited_request)
  }

  /// The current state (for tests and reports).
  pub fn state(&self) -> State {
    self.state
  }

  /// The bandwidth estimate `BBR.bw`, bytes per second.
  pub fn bw(&self) -> u64 {
    self.bw
  }

  /// The propagation-delay estimate `BBR.min_rtt`, nanoseconds.
  pub fn min_rtt(&self) -> Option<u64> {
    self.min_rtt
  }

  fn min_pipe_cwnd(&self) -> u64 {
    MIN_PIPE_CWND_DATAGRAMS.saturating_mul(self.smss)
  }

  // ── Per-transmit (§5.2.2, §5.4.1) ─────────────────────────────────────────────────────────────────────

  /// `OnTransmit` → `HandleRestartFromIdle`: a packet is about to leave at `now` with `inflight` bytes
  /// already in flight, the sender application-limited or not.
  pub fn on_transmit(&mut self, now: u64, inflight: u64, app_limited: bool) {
    self.event_now = now;
    if inflight == 0 && app_limited {
      self.idle_restart = true;
      self.extra_acked_interval_start = now;
      if self.is_in_a_probe_bw_state() {
        self.set_pacing_rate_with_gain(UNITY_GAIN);
      } else if self.state == State::ProbeRtt {
        self.check_probe_rtt_done(now);
      }
    }
  }

  // ── Per-ACK (§5.2.3) ───────────────────────────────────────────────────────────────────────────────────

  /// `UpdateOnACK`: `UpdateModelAndState` then `UpdateControlParameters`.
  pub fn on_ack(&mut self, event: &AckEvent<'_>) {
    let now = event.now;
    let rs = event.sample;
    self.event_now = now;
    self.delivered = event.delivered;
    self.inflight = event.in_flight;
    self.is_cwnd_limited = event.cwnd_limited;
    if rs.newly_acked == 0 && rs.rtt.is_none() {
      return;
    }
    if !self.has_seen_rtt && event.rtt.has_sample() {
      // §5.6.2: the initial pacing rate uses the first RTT sample in place of the assumed one.
      self.has_seen_rtt = true;
      self.init_pacing_rate(Some(event.rtt.smoothed_rtt()));
    }
    // Loss recovery ends when a packet sent after it began is acknowledged (§5.6.4.4: RestoreCwnd).
    if let Some(start) = self.recovery_start
      && event.acked.iter().any(|packet| packet.sent_at > start)
    {
      self.recovery_start = None;
      self.restore_cwnd();
    }
    // UpdateModelAndState
    self.update_latest_delivery_signals(&rs);
    self.update_congestion_signals(&rs);
    self.update_ack_aggregation(now, &rs);
    self.check_full_bw_reached(&rs);
    self.check_startup_done();
    self.check_drain_done();
    self.update_probe_bw_cycle_phase(now, &rs);
    self.update_min_rtt(now, &rs);
    self.check_probe_rtt(now, &rs);
    self.advance_latest_delivery_signals(&rs);
    self.bound_bw_for_model();
    // UpdateControlParameters
    self.set_pacing_rate();
    self.set_send_quantum();
    self.set_cwnd(&rs);
  }

  // ── Per-loss (§5.2.4, §5.5.10) ─────────────────────────────────────────────────────────────────────────

  /// `HandleLostPacket` for each lost packet; persistent congestion is the RTO case (§5.6.4.4).
  pub fn on_loss(&mut self, event: &LossEvent<'_>) {
    self.event_now = event.now;
    self.inflight = event.in_flight;
    let batch: u64 = event.lost.iter().map(|packet| packet.bytes).sum();
    let mut lost_so_far = event.lost_total.saturating_sub(batch);
    if self.recovery_start.is_none() {
      // Entering loss recovery (OnEnterFastRecovery): SaveCwnd, SaveStateUponLoss via NoteLoss below.
      self.save_cwnd();
      self.recovery_start = Some(event.now);
      self.recovery_round = self.round_count;
    }
    for packet in event.lost {
      lost_so_far = lost_so_far.saturating_add(packet.bytes);
      self.lost = lost_so_far;
      // Startup's loss-range count (§5.3.1.3): a lost packet not adjacent to the previous one opens a range.
      if self
        .last_lost_pn
        .is_none_or(|last| packet.pn != last.saturating_add(1))
      {
        self.loss_ranges_in_round = self.loss_ranges_in_round.saturating_add(1);
      }
      self.last_lost_pn = Some(packet.pn);
      self.handle_lost_packet(packet);
    }
    if event.persistent {
      // OnEnterRTO: SaveCwnd, SaveStateUponLoss, cwnd = inflight + 1 SMSS.
      self.save_cwnd();
      self.save_state_upon_loss();
      self.cwnd = event.in_flight.saturating_add(self.smss);
    }
  }

  fn handle_lost_packet(&mut self, packet: &SentPacket) {
    self.note_loss();
    if !self.is_bw_probe_sample {
      return;
    }
    let mut rs = RateSample {
      tx_in_flight: packet.rate.tx_in_flight,
      lost: self.lost.saturating_sub(packet.rate.lost),
      is_app_limited: packet.rate.is_app_limited,
      ..RateSample::default()
    };
    if self.is_inflight_too_high(&rs) {
      rs.tx_in_flight = self.inflight_at_loss(&rs, packet.bytes);
      self.handle_inflight_too_high(&rs);
    }
  }

  fn note_loss(&mut self) {
    if !self.is_loss_in_round {
      self.loss_round_delivered = self.delivered;
      self.save_state_upon_loss();
    }
    self.is_loss_in_round = true;
  }

  /// `InflightAtLoss` (§5.5.10.2): the inflight at which losses crossed `LossThresh`, within packet P.
  fn inflight_at_loss(&self, rs: &RateSample, size: u64) -> u64 {
    let inflight_prev = i128::from(rs.tx_in_flight.saturating_sub(size));
    let lost_prev = i128::from(rs.lost.saturating_sub(size));
    let permille = i128::from(PERMILLE);
    let thresh = i128::from(LOSS_THRESH);
    // lost_prefix = (LossThresh · inflight_prev − lost_prev) / (1 − LossThresh), in per mille; it is
    // negative when the threshold was crossed before this packet, and the crossing point is then below it.
    let lost_prefix = (thresh * inflight_prev - lost_prev * permille) / (permille - thresh);
    u64::try_from((inflight_prev + lost_prefix).max(0)).unwrap_or(u64::MAX)
  }

  fn is_inflight_too_high(&self, rs: &RateSample) -> bool {
    // `C.has_selective_acks` is true for every QUIC connection (§2.2), so only the rate test applies.
    u128::from(rs.lost) * u128::from(PERMILLE)
      > u128::from(rs.tx_in_flight) * u128::from(LOSS_THRESH)
  }

  fn handle_inflight_too_high(&mut self, rs: &RateSample) {
    self.prev_probe_too_high = true;
    self.is_bw_probe_sample = false;
    if !rs.is_app_limited {
      self.inflight_longterm = Some(
        rs.tx_in_flight
          .max(self.target_inflight().saturating_mul(BETA) / PERMILLE),
      );
    }
    if self.state == State::ProbeBwUp {
      self.undo_state = Some(State::ProbeBwUp);
      self.start_probe_bw_down_at(self.cycle_stamp_now());
    }
  }

  // ── Startup and Drain (§5.3.1, §5.3.2) ─────────────────────────────────────────────────────────────────

  fn enter_startup(&mut self) {
    self.state = State::Startup;
    self.pacing_gain = STARTUP_PACING_GAIN;
    self.cwnd_gain = DEFAULT_CWND_GAIN;
  }

  fn reset_full_bw(&mut self) {
    self.full_bw = 0;
    self.full_bw_count = 0;
    self.full_bw_now = false;
  }

  fn check_full_bw_reached(&mut self, rs: &RateSample) {
    if self.full_bw_now || !self.round_start || rs.is_app_limited {
      return;
    }
    if u128::from(rs.delivery_rate) * u128::from(PERMILLE)
      >= u128::from(self.full_bw) * u128::from(FULL_BW_GROWTH)
    {
      self.reset_full_bw();
      self.full_bw = rs.delivery_rate;
      return;
    }
    self.full_bw_count = self.full_bw_count.saturating_add(1);
    self.full_bw_now = self.full_bw_count >= FULL_BW_COUNT;
    if self.full_bw_now {
      self.full_bw_reached = true;
    }
  }

  fn check_startup_done(&mut self) {
    self.check_startup_high_loss();
    if self.state == State::Startup && self.full_bw_reached {
      self.enter_drain();
    }
  }

  /// `CheckStartupHighLoss` (§5.3.1.3), for a connection with selective acknowledgements: in recovery
  /// for a full round, more than `LossThresh` of the round's data lost, in at least six ranges.
  fn check_startup_high_loss(&mut self) {
    if self.state != State::Startup || !self.round_start {
      return;
    }
    let lost_in_round = self.lost.saturating_sub(self.lost_at_round_start);
    let delivered_in_round = self.delivered.saturating_sub(self.delivered_at_round_start);
    let high_loss = self.recovery_start.is_some()
      && self.round_count > self.recovery_round
      && u128::from(lost_in_round) * u128::from(PERMILLE)
        > u128::from(lost_in_round.saturating_add(delivered_in_round)) * u128::from(LOSS_THRESH)
      && self.loss_ranges_in_round >= STARTUP_FULL_LOSS_COUNT;
    self.lost_at_round_start = self.lost;
    self.delivered_at_round_start = self.delivered;
    self.loss_ranges_in_round = 0;
    if high_loss {
      self.undo_state = Some(State::Startup);
      self.full_bw_reached = true;
      self.inflight_longterm = Some(self.bdp.max(self.inflight_latest));
    }
  }

  fn enter_drain(&mut self) {
    self.state = State::Drain;
    self.pacing_gain = DRAIN_PACING_GAIN;
    self.cwnd_gain = DEFAULT_CWND_GAIN;
    self.drain_start_round = self.round_count;
  }

  fn check_drain_done(&mut self) {
    if self.state == State::Drain
      && (self.inflight <= self.inflight_for(UNITY_GAIN)
        || self.round_count > self.drain_start_round.saturating_add(DRAIN_MAX_ROUNDS))
    {
      self.enter_probe_bw();
    }
  }

  // ── ProbeBW (§5.3.3) ───────────────────────────────────────────────────────────────────────────────────

  fn enter_probe_bw(&mut self) {
    self.cwnd_gain = DEFAULT_CWND_GAIN;
    let now = self.cycle_stamp_now();
    self.start_probe_bw_down_at(now);
  }

  /// The clock reading ProbeBW transitions stamp: the last event's time (the model has no clock of its
  /// own; `cycle_stamp` records the ACK time each transition happened at).
  fn cycle_stamp_now(&self) -> u64 {
    self.event_now
  }

  fn start_probe_bw_down_at(&mut self, now: u64) {
    self.reset_congestion_signals();
    self.probe_up_acked_per_inc = None;
    self.pick_probe_wait();
    self.cycle_stamp = now;
    self.ack_phase = AckPhase::ProbeStopping;
    self.start_round();
    self.state = State::ProbeBwDown;
    self.pacing_gain = PROBE_DOWN_PACING_GAIN;
    self.cwnd_gain = DEFAULT_CWND_GAIN;
  }

  fn start_probe_bw_cruise(&mut self) {
    self.state = State::ProbeBwCruise;
    self.pacing_gain = UNITY_GAIN;
    self.cwnd_gain = DEFAULT_CWND_GAIN;
  }

  fn start_probe_bw_refill(&mut self) {
    self.reset_short_term_model();
    self.bw_probe_up_rounds = 0;
    self.bw_probe_up_acked = 0;
    self.prev_probe_precautionary = false;
    self.ack_phase = AckPhase::Refilling;
    self.start_round();
    self.state = State::ProbeBwRefill;
    self.pacing_gain = UNITY_GAIN;
    self.cwnd_gain = DEFAULT_CWND_GAIN;
  }

  fn start_probe_bw_up(&mut self, rs: &RateSample) {
    self.ack_phase = AckPhase::ProbeStarting;
    self.start_round();
    self.reset_full_bw();
    self.full_bw = rs.delivery_rate;
    self.state = State::ProbeBwUp;
    self.pacing_gain = PROBE_UP_PACING_GAIN;
    self.cwnd_gain = PROBE_UP_CWND_GAIN;
    self.raise_inflight_longterm_slope();
  }

  fn update_probe_bw_cycle_phase(&mut self, now: u64, rs: &RateSample) {
    if !self.full_bw_reached {
      return;
    }
    if self.adapt_long_term_model(rs) {
      return;
    }
    if !self.is_in_a_probe_bw_state() {
      return;
    }
    if self.state == State::ProbeBwUp {
      if self.is_time_to_go_down(rs) {
        self.prev_probe_too_high = false;
        self.start_probe_bw_down_at(now);
      }
      return;
    }
    match self.state {
      State::ProbeBwDown => {
        if self.is_time_to_probe_bw(now) {
          return;
        }
        if self.is_time_to_cruise() {
          self.start_probe_bw_cruise();
        }
      }
      State::ProbeBwCruise => {
        let _ = self.is_time_to_probe_bw(now);
      }
      State::ProbeBwRefill if self.round_start => {
        self.is_bw_probe_sample = true;
        self.start_probe_bw_up(rs);
      }
      _ => {}
    }
  }

  fn is_in_a_probe_bw_state(&self) -> bool {
    matches!(
      self.state,
      State::ProbeBwDown | State::ProbeBwCruise | State::ProbeBwRefill | State::ProbeBwUp
    )
  }

  fn is_probing_bw(&self) -> bool {
    matches!(
      self.state,
      State::Startup | State::ProbeBwRefill | State::ProbeBwUp
    )
  }

  fn is_time_to_cruise(&mut self) -> bool {
    if self
      .inflight_with_headroom()
      .is_some_and(|headroom| self.inflight > headroom)
    {
      return false;
    }
    self.inflight <= self.inflight_with(self.max_bw, UNITY_GAIN)
  }

  fn is_time_to_go_down(&mut self, rs: &RateSample) -> bool {
    if self.prev_probe_too_high
      && self
        .inflight_longterm
        .is_some_and(|longterm| self.inflight >= longterm)
    {
      self.prev_probe_precautionary = true;
      return true;
    }
    if self.is_cwnd_limited
      && self
        .inflight_longterm
        .is_some_and(|longterm| self.cwnd >= longterm)
    {
      self.reset_full_bw();
      self.full_bw = rs.delivery_rate;
    } else if self.full_bw_now {
      return true;
    }
    false
  }

  fn has_elapsed_in_phase(&self, now: u64, interval: u64) -> bool {
    now > self.cycle_stamp.saturating_add(interval)
  }

  fn is_time_to_probe_bw(&mut self, now: u64) -> bool {
    if self.has_elapsed_in_phase(now, self.bw_probe_wait) || self.is_reno_coexistence_probe_time() {
      self.start_probe_bw_refill();
      return true;
    }
    false
  }

  fn pick_probe_wait(&mut self) {
    self.rounds_since_probe_up = u64::from(self.rng.next_u64() & 1 == 1);
    let span = usize::try_from(PROBE_WAIT_SPAN_NS).unwrap_or(usize::MAX);
    self.bw_probe_wait =
      PROBE_WAIT_BASE_NS.saturating_add(u64::try_from(self.rng.below(span)).unwrap_or(0));
  }

  fn is_reno_coexistence_probe_time(&self) -> bool {
    // `reno_rounds = TargetInflight()`, in datagrams (the draft's BDP in packets, §5.3.3.8.2).
    let reno_rounds = self.target_inflight() / self.smss.max(1);
    self.rounds_since_probe_up >= reno_rounds.min(RENO_ROUND_BOUND)
  }

  fn target_inflight(&self) -> u64 {
    self.bdp.min(self.cwnd)
  }

  fn inflight_with_headroom(&self) -> Option<u64> {
    let longterm = self.inflight_longterm?;
    let headroom = self.smss.max(longterm.saturating_mul(HEADROOM) / PERMILLE);
    Some(longterm.saturating_sub(headroom).max(self.min_pipe_cwnd()))
  }

  fn raise_inflight_longterm_slope(&mut self) {
    let growth_this_round = 1u64 << self.bw_probe_up_rounds;
    self.bw_probe_up_rounds = (self.bw_probe_up_rounds + 1).min(MAX_PROBE_UP_ROUNDS);
    self.probe_up_acked_per_inc = Some((self.cwnd / growth_this_round).max(self.smss));
  }

  fn probe_inflight_longterm_upward(&mut self, rs: &RateSample) {
    let Some(longterm) = self.inflight_longterm else {
      return;
    };
    if !self.is_cwnd_limited || self.cwnd < longterm {
      return;
    }
    self.bw_probe_up_acked = self.bw_probe_up_acked.saturating_add(rs.newly_acked);
    if let Some(per_inc) = self.probe_up_acked_per_inc
      && self.bw_probe_up_acked >= per_inc
    {
      let delta = self.bw_probe_up_acked / per_inc;
      self.bw_probe_up_acked -= delta * per_inc;
      self.inflight_longterm = Some(longterm.saturating_add(delta.saturating_mul(self.smss)));
    }
    if self.round_start {
      self.raise_inflight_longterm_slope();
    }
  }

  fn adapt_long_term_model(&mut self, rs: &RateSample) -> bool {
    if self.ack_phase == AckPhase::ProbeStarting && self.round_start {
      self.ack_phase = AckPhase::ProbeFeedback;
    }
    if self.ack_phase == AckPhase::ProbeStopping && self.round_start {
      self.is_bw_probe_sample = false;
      self.ack_phase = AckPhase::Init;
      if self.is_in_a_probe_bw_state() && !rs.is_app_limited {
        self.advance_max_bw_filter();
      }
      if self.is_in_a_probe_bw_state() && self.prev_probe_precautionary && !self.prev_probe_too_high
      {
        self.start_probe_bw_refill();
        return true;
      }
    }
    if !self.is_inflight_too_high(rs) {
      let Some(longterm) = self.inflight_longterm else {
        return false;
      };
      if rs.tx_in_flight > longterm {
        self.inflight_longterm = Some(rs.tx_in_flight);
      }
      if self.state == State::ProbeBwUp {
        self.probe_inflight_longterm_upward(rs);
      }
    }
    false
  }

  // ── ProbeRTT (§5.3.4) ──────────────────────────────────────────────────────────────────────────────────

  fn update_min_rtt(&mut self, now: u64, rs: &RateSample) {
    self.probe_rtt_expired = now
      > self
        .probe_rtt_min_stamp
        .saturating_add(PROBE_RTT_INTERVAL_NS);
    if let Some(rtt) = rs.rtt
      && (self.probe_rtt_min_delay.is_none_or(|min| rtt < min) || self.probe_rtt_expired)
    {
      self.probe_rtt_min_delay = Some(rtt);
      self.probe_rtt_min_stamp = now;
    }
    let min_rtt_expired = now > self.min_rtt_stamp.saturating_add(MIN_RTT_FILTER_LEN_NS);
    if let Some(probe_min) = self.probe_rtt_min_delay
      && (self.min_rtt.is_none_or(|min| probe_min < min) || min_rtt_expired)
    {
      self.min_rtt = Some(probe_min);
      self.min_rtt_stamp = self.probe_rtt_min_stamp;
    }
  }

  fn check_probe_rtt(&mut self, now: u64, rs: &RateSample) {
    if self.state != State::ProbeRtt && self.probe_rtt_expired && !self.idle_restart {
      self.enter_probe_rtt();
      self.save_cwnd();
      self.probe_rtt_done_stamp = None;
      self.ack_phase = AckPhase::ProbeStopping;
      self.start_round();
    }
    if self.state == State::ProbeRtt {
      self.handle_probe_rtt(now);
    }
    if rs.delivered > 0 {
      self.idle_restart = false;
    }
  }

  fn enter_probe_rtt(&mut self) {
    self.state = State::ProbeRtt;
    self.pacing_gain = UNITY_GAIN;
    self.cwnd_gain = PROBE_RTT_CWND_GAIN;
  }

  fn handle_probe_rtt(&mut self, now: u64) {
    // Ignore low rate samples during ProbeRTT: the connection marks itself application-limited (it owns
    // `C.app_limited` in its delivery-rate sampler, and reads this request through `take_app_limited`).
    self.app_limited_request = true;
    if self.probe_rtt_done_stamp.is_none() && self.inflight <= self.probe_rtt_cwnd() {
      self.probe_rtt_done_stamp = Some(now.saturating_add(PROBE_RTT_DURATION_NS));
      self.probe_rtt_round_done = false;
      self.start_round();
    } else if self.probe_rtt_done_stamp.is_some() {
      if self.round_start {
        self.probe_rtt_round_done = true;
      }
      if self.probe_rtt_round_done {
        self.check_probe_rtt_done(now);
      }
    }
  }

  fn check_probe_rtt_done(&mut self, now: u64) {
    if self.probe_rtt_done_stamp.is_some_and(|done| now > done) {
      self.probe_rtt_min_stamp = now;
      self.restore_cwnd();
      self.exit_probe_rtt(now);
    }
  }

  fn exit_probe_rtt(&mut self, now: u64) {
    self.reset_short_term_model();
    if self.full_bw_reached {
      self.start_probe_bw_down_at(now);
      self.start_probe_bw_cruise();
    } else {
      self.enter_startup();
    }
  }

  // ── The path model (§5.5) ──────────────────────────────────────────────────────────────────────────────

  fn update_round(&mut self, rs: &RateSample) {
    if rs.prior_delivered >= self.next_round_delivered {
      self.start_round();
      self.round_count = self.round_count.saturating_add(1);
      self.rounds_since_probe_up = self.rounds_since_probe_up.saturating_add(1);
      self.round_start = true;
    } else {
      self.round_start = false;
    }
  }

  fn start_round(&mut self) {
    self.next_round_delivered = self.delivered;
  }

  fn update_max_bw(&mut self, rs: &RateSample) {
    self.update_round(rs);
    if rs.delivery_rate > 0 && (rs.delivery_rate >= self.max_bw || !rs.is_app_limited) {
      self.max_bw =
        self
          .max_bw_filter
          .update(self.cycle_count, MAX_BW_FILTER_LEN, rs.delivery_rate);
    }
  }

  fn advance_max_bw_filter(&mut self) {
    self.cycle_count = self.cycle_count.saturating_add(1);
  }

  fn update_ack_aggregation(&mut self, now: u64, rs: &RateSample) {
    let interval = now.saturating_sub(self.extra_acked_interval_start);
    let mut expected = delivery::volume(self.bw, interval);
    if self.extra_acked_delivered <= expected {
      self.extra_acked_delivered = 0;
      self.extra_acked_interval_start = now;
      expected = 0;
    }
    self.extra_acked_delivered = self.extra_acked_delivered.saturating_add(rs.newly_acked);
    let extra = self
      .extra_acked_delivered
      .saturating_sub(expected)
      .min(self.cwnd);
    let filter_len = if self.full_bw_reached {
      EXTRA_ACKED_FILTER_LEN
    } else {
      1
    };
    self.extra_acked = self
      .extra_acked_filter
      .update(self.round_count, filter_len, extra);
  }

  fn update_latest_delivery_signals(&mut self, rs: &RateSample) {
    self.loss_round_start = false;
    self.bw_latest = self.bw_latest.max(rs.delivery_rate);
    self.inflight_latest = self.inflight_latest.max(rs.delivered);
    if rs.prior_delivered >= self.loss_round_delivered {
      self.loss_round_delivered = self.delivered;
      self.loss_round_start = true;
    }
  }

  fn advance_latest_delivery_signals(&mut self, rs: &RateSample) {
    if self.loss_round_start {
      self.bw_latest = rs.delivery_rate;
      self.inflight_latest = rs.delivered;
    }
  }

  fn reset_congestion_signals(&mut self) {
    self.is_loss_in_round = false;
    self.bw_latest = 0;
    self.inflight_latest = 0;
  }

  fn update_congestion_signals(&mut self, rs: &RateSample) {
    self.update_max_bw(rs);
    if !self.loss_round_start {
      return;
    }
    self.adapt_lower_bounds_from_congestion();
    self.is_loss_in_round = false;
  }

  fn adapt_lower_bounds_from_congestion(&mut self) {
    if self.is_probing_bw() {
      return;
    }
    if self.is_loss_in_round {
      self.init_lower_bounds();
      self.loss_lower_bounds();
    }
  }

  fn init_lower_bounds(&mut self) {
    if self.bw_shortterm.is_none() {
      self.bw_shortterm = Some(self.max_bw);
    }
    if self.inflight_shortterm.is_none() {
      self.inflight_shortterm = Some(self.cwnd);
    }
  }

  fn loss_lower_bounds(&mut self) {
    if let Some(shortterm) = self.bw_shortterm {
      self.bw_shortterm = Some(
        self
          .bw_latest
          .max(shortterm.saturating_mul(BETA) / PERMILLE),
      );
    }
    if let Some(shortterm) = self.inflight_shortterm {
      self.inflight_shortterm = Some(
        self
          .inflight_latest
          .max(shortterm.saturating_mul(BETA) / PERMILLE),
      );
    }
  }

  fn reset_short_term_model(&mut self) {
    self.bw_shortterm = None;
    self.inflight_shortterm = None;
  }

  fn bound_bw_for_model(&mut self) {
    self.bw = self
      .bw_shortterm
      .map_or(self.max_bw, |short| self.max_bw.min(short));
  }

  fn save_state_upon_loss(&mut self) {
    self.save_cwnd();
    self.undo_state = None;
    self.undo_bw_shortterm = self.bw_shortterm;
    self.undo_inflight_shortterm = self.inflight_shortterm;
    self.undo_inflight_longterm = self.inflight_longterm;
  }

  // ── Control parameters (§5.6) ──────────────────────────────────────────────────────────────────────────

  fn init_pacing_rate(&mut self, srtt: Option<u64>) {
    let rtt = srtt.filter(|rtt| *rtt > 0).unwrap_or(INITIAL_PACING_RTT_NS);
    let nominal = delivery::rate(self.initial_cwnd, rtt);
    self.pacing_rate = nominal.saturating_mul(STARTUP_PACING_GAIN) / PERMILLE;
  }

  fn set_pacing_rate_with_gain(&mut self, gain: u64) {
    let rate = u64::try_from(
      u128::from(gain) * u128::from(self.bw) * u128::from(PACING_MARGIN_KEEP)
        / (u128::from(PERMILLE) * u128::from(PERMILLE)),
    )
    .unwrap_or(u64::MAX);
    if self.full_bw_reached || rate > self.pacing_rate {
      self.pacing_rate = rate;
    }
  }

  fn set_pacing_rate(&mut self) {
    self.set_pacing_rate_with_gain(self.pacing_gain);
  }

  fn set_send_quantum(&mut self) {
    self.send_quantum = super::send_quantum_for(self.pacing_rate, self.smss);
  }

  fn bdp_multiple(&mut self, bw: u64, gain: u64) -> u64 {
    let Some(min_rtt) = self.min_rtt else {
      return self.initial_cwnd;
    };
    self.bdp = delivery::volume(bw, min_rtt);
    self.bdp.saturating_mul(gain) / PERMILLE
  }

  fn quantization_budget(&mut self, inflight_cap: u64) -> u64 {
    // UpdateOffloadBudget for QUIC (§5.5.8.2): one send quantum.
    self.offload_budget = self.send_quantum;
    let mut cap = inflight_cap
      .max(self.offload_budget)
      .max(self.min_pipe_cwnd());
    if self.state == State::ProbeBwUp {
      cap = cap.saturating_add(2 * self.smss);
    }
    cap
  }

  fn inflight_with(&mut self, bw: u64, gain: u64) -> u64 {
    let cap = self.bdp_multiple(bw, gain);
    self.quantization_budget(cap)
  }

  fn inflight_for(&mut self, gain: u64) -> u64 {
    self.inflight_with(self.bw, gain)
  }

  fn update_max_inflight(&mut self) {
    let cap = self
      .bdp_multiple(self.bw, self.cwnd_gain)
      .saturating_add(self.extra_acked);
    self.max_inflight = self.quantization_budget(cap);
  }

  fn save_cwnd(&mut self) {
    if self.recovery_start.is_none() && self.state != State::ProbeRtt {
      self.prior_cwnd = self.cwnd;
    } else {
      self.prior_cwnd = self.prior_cwnd.max(self.cwnd);
    }
  }

  fn restore_cwnd(&mut self) {
    self.cwnd = self.cwnd.max(self.prior_cwnd);
  }

  fn probe_rtt_cwnd(&mut self) -> u64 {
    self
      .bdp_multiple(self.bw, PROBE_RTT_CWND_GAIN)
      .max(self.min_pipe_cwnd())
  }

  fn bound_cwnd_for_probe_rtt(&mut self) {
    if self.state == State::ProbeRtt {
      self.cwnd = self.cwnd.min(self.probe_rtt_cwnd());
    }
  }

  fn set_cwnd(&mut self, rs: &RateSample) {
    self.update_max_inflight();
    if self.full_bw_reached {
      self.cwnd = self
        .cwnd
        .saturating_add(rs.newly_acked)
        .min(self.max_inflight);
    } else if self.cwnd < self.max_inflight || self.delivered < self.initial_cwnd {
      self.cwnd = self.cwnd.saturating_add(rs.newly_acked);
    }
    self.cwnd = self.cwnd.max(self.min_pipe_cwnd());
    self.bound_cwnd_for_probe_rtt();
    self.bound_cwnd_for_model();
  }

  fn bound_cwnd_for_model(&mut self) {
    let mut cap = u64::MAX;
    if self.is_in_a_probe_bw_state() && self.state != State::ProbeBwCruise {
      cap = self.inflight_longterm.unwrap_or(u64::MAX);
    } else if self.state == State::ProbeRtt || self.state == State::ProbeBwCruise {
      cap = self.inflight_with_headroom().unwrap_or(u64::MAX);
    }
    cap = cap.min(self.inflight_shortterm.unwrap_or(u64::MAX));
    cap = cap.max(self.min_pipe_cwnd());
    self.cwnd = self.cwnd.min(cap);
  }
}
