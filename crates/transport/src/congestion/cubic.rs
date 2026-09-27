//! CUBIC (RFC 9438, B) with HyStart++ slow start (RFC 9406, B) — one of the three control laws in the
//! session plane's congestion bake-off (see the module above). The window grows along the cubic
//! `W(t) = C·(t − K)³ + W_max` from the last congestion event, never slower than an equivalent Reno flow
//! (the Reno-friendly region, §4.3), and drops by β = 0.7 on congestion with fast convergence (§4.7).
//! The initial slow start exits on a sustained RTT rise before the bottleneck's buffer overflows
//! (HyStart++'s delay-increase test with Conservative Slow Start). Everything is integer arithmetic in
//! bytes and nanoseconds — the cube and cube root exact over `i128`/`u128` — so a simulated history
//! replays bit for bit on every platform.

use super::{AckEvent, INITIAL_WINDOW_DATAGRAMS, LossEvent, MINIMUM_WINDOW_DATAGRAMS};

/// Format: RFC 9438 §4.6 `β_cubic` = 0.7, the numerator over [`BETA_DENOMINATOR`].
const BETA_NUMERATOR: u64 = 7;
/// Format: RFC 9438 §4.6 `β_cubic` = 0.7, the denominator.
const BETA_DENOMINATOR: u64 = 10;
/// Format: RFC 9438 §5.1 `C` = 0.4 segments per second³, the numerator over [`C_DENOMINATOR`].
const C_NUMERATOR: i128 = 2;
/// Format: RFC 9438 §5.1 `C` = 0.4, the denominator.
const C_DENOMINATOR: i128 = 5;
/// Format: RFC 9438 §4.3 `α_cubic` = 3·(1 − β)/(1 + β) = 9/17 with β = 0.7, the numerator.
const ALPHA_NUMERATOR: u64 = 9;
/// Format: RFC 9438 §4.3 `α_cubic`, the denominator.
const ALPHA_DENOMINATOR: u64 = 17;
/// Format: RFC 9438 §4.2 — the target is at most 1.5 × cwnd, as the ratio 3/2.
const TARGET_CAP_NUMERATOR: u64 = 3;
/// Format: RFC 9438 §4.2 — the target cap's denominator.
const TARGET_CAP_DENOMINATOR: u64 = 2;
/// Format: nanoseconds per second, cubed — the unit change of `(t − K)³` from ns³ to s³.
const NANOS_PER_SECOND_CUBED: i128 = 1_000_000_000_000_000_000_000_000_000;
/// Format: the largest `|t − K|` the cube is evaluated at, 100 s in nanoseconds: past it the window is
/// already far outside any real path's range, and the bound keeps `(t − K)³ · MSS` inside `i128`.
const MAX_CUBIC_OFFSET_NS: i128 = 100_000_000_000;

/// Format: RFC 9406 §4.3 `MIN_RTT_THRESH` = 4 ms.
const HYSTART_MIN_RTT_THRESH_NS: u64 = 4_000_000;
/// Format: RFC 9406 §4.3 `MAX_RTT_THRESH` = 16 ms.
const HYSTART_MAX_RTT_THRESH_NS: u64 = 16_000_000;
/// Format: RFC 9406 §4.3 `MIN_RTT_DIVISOR` = 8.
const HYSTART_MIN_RTT_DIVISOR: u64 = 8;
/// Format: RFC 9406 §4.3 `N_RTT_SAMPLE` = 8.
const HYSTART_N_RTT_SAMPLE: u64 = 8;
/// Format: RFC 9406 §4.3 `CSS_GROWTH_DIVISOR` = 4.
const HYSTART_CSS_GROWTH_DIVISOR: u64 = 4;
/// Format: RFC 9406 §4.3 `CSS_ROUNDS` = 5.
const HYSTART_CSS_ROUNDS: u64 = 5;
// RFC 9406 §4.3: `L` is infinity for a paced sender, which every law here is, so no per-ACK cap applies.

/// HyStart++'s phase during the initial slow start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HyStart {
  /// Standard slow start, watching for a delay increase.
  SlowStart,
  /// Conservative Slow Start, entered at a delay increase, with the RTT baseline and rounds left.
  Conservative { baseline: u64, rounds: u64 },
  /// Done: congestion avoidance (or any later slow start, which is standard, §4.3).
  Done,
}

/// CUBIC's state (RFC 9438 §4.1.2), in bytes and nanoseconds.
#[derive(Debug)]
pub struct Cubic {
  max_datagram: u64,
  window: u64,
  ssthresh: u64,
  /// `cwnd_prior`: the window when `ssthresh` was last set.
  window_prior: u64,
  /// `W_max`.
  w_max: u64,
  /// `K`, nanoseconds.
  k_ns: u64,
  /// `t_epoch`: when the current congestion-avoidance stage began (shifted past application-limited time).
  epoch_start: Option<u64>,
  /// `W_est`, bytes, and the sub-byte remainder its growth carries between acknowledgements.
  w_est: u64,
  w_est_remainder: u64,
  /// The remainder of the cubic increment carried between acknowledgements.
  increment_remainder: u64,
  /// The start of the recovery period (one reduction per period, by send time).
  recovery_start: Option<u64>,
  /// When the last acknowledgement was processed (to exclude application-limited time from `t`).
  last_ack_at: Option<u64>,
  hystart: HyStart,
  /// HyStart++'s round: the largest packet number sent when the round began (the round ends when a
  /// packet at or past it is acknowledged), the last and current rounds' minimum RTTs, and the samples.
  window_end: Option<u64>,
  last_round_min_rtt: Option<u64>,
  current_round_min_rtt: Option<u64>,
  rtt_sample_count: u64,
  /// The largest packet number sent so far.
  largest_sent: u64,
}

impl Cubic {
  /// A controller starting in HyStart++ slow start at the initial window.
  pub fn new(max_datagram: u64) -> Cubic {
    let window = INITIAL_WINDOW_DATAGRAMS.saturating_mul(max_datagram);
    Cubic {
      max_datagram,
      window,
      ssthresh: u64::MAX,
      window_prior: window,
      w_max: window,
      k_ns: 0,
      epoch_start: None,
      w_est: window,
      w_est_remainder: 0,
      increment_remainder: 0,
      recovery_start: None,
      last_ack_at: None,
      hystart: HyStart::SlowStart,
      window_end: None,
      last_round_min_rtt: None,
      current_round_min_rtt: None,
      rtt_sample_count: 0,
      largest_sent: 0,
    }
  }

  /// The congestion window, bytes.
  pub fn window(&self) -> u64 {
    self.window
  }

  /// The datagram size the window counts in.
  pub fn max_datagram(&self) -> u64 {
    self.max_datagram
  }

  /// Whether the window is at or below the slow-start threshold (RFC 9438 §4.10).
  pub fn in_slow_start(&self) -> bool {
    self.window < self.ssthresh
  }

  fn minimum_window(&self) -> u64 {
    MINIMUM_WINDOW_DATAGRAMS.saturating_mul(self.max_datagram)
  }

  /// Records packet `pn` leaving; opens HyStart++'s first round.
  pub fn on_sent(&mut self, pn: u64) {
    self.largest_sent = self.largest_sent.max(pn);
    if self.window_end.is_none() {
      self.window_end = Some(pn);
    }
  }

  fn in_recovery(&self, sent_at: u64) -> bool {
    self.recovery_start.is_some_and(|start| sent_at <= start)
  }

  /// Processes an acknowledgement (RFC 9438 §4.2–§4.5, RFC 9406 §4.2).
  pub fn on_ack(&mut self, event: &AckEvent<'_>) {
    let now = event.now;
    let previous_ack = self.last_ack_at.replace(now);
    let acked: u64 = event
      .acked
      .iter()
      .filter(|packet| !self.in_recovery(packet.sent_at))
      .map(|packet| packet.bytes)
      .sum();
    if !event.cwnd_limited {
      // RFC 9438 §4.2 / §5.8: `t` excludes application-limited periods — move the epoch forward by the
      // time since the last acknowledgement, and do not grow a window the sender is not using.
      if let (Some(start), Some(previous)) = (self.epoch_start.as_mut(), previous_ack) {
        *start = start.saturating_add(now.saturating_sub(previous));
      }
      return;
    }
    if acked == 0 {
      return;
    }
    let newest_pn = event.acked.iter().map(|packet| packet.pn).max();
    if self.in_slow_start() {
      self.slow_start(acked, event.sample.rtt, newest_pn);
      return;
    }
    self.congestion_avoidance(now, acked, event.rtt.smoothed_rtt_or_initial());
  }

  /// Slow start with HyStart++ during the first one (RFC 9406 §4.2).
  fn slow_start(&mut self, acked: u64, rtt_sample: Option<u64>, newest_pn: Option<u64>) {
    let growth = match self.hystart {
      HyStart::Conservative { .. } => acked / HYSTART_CSS_GROWTH_DIVISOR,
      _ => acked,
    };
    self.window = self.window.saturating_add(growth);
    if self.hystart == HyStart::Done {
      return;
    }
    self.hystart_sample(rtt_sample);
    self.hystart_round_end(newest_pn);
  }

  /// Folds one RTT sample into the round's minimum and applies HyStart++'s delay-increase test (in slow
  /// start) or its spurious-exit test (in Conservative Slow Start).
  fn hystart_sample(&mut self, rtt_sample: Option<u64>) {
    if let Some(rtt) = rtt_sample {
      self.current_round_min_rtt = Some(self.current_round_min_rtt.map_or(rtt, |min| min.min(rtt)));
      self.rtt_sample_count = self.rtt_sample_count.saturating_add(1);
    }
    if self.rtt_sample_count < HYSTART_N_RTT_SAMPLE {
      return;
    }
    match (
      self.hystart,
      self.current_round_min_rtt,
      self.last_round_min_rtt,
    ) {
      (HyStart::SlowStart, Some(current), Some(last)) => {
        let threshold = (last / HYSTART_MIN_RTT_DIVISOR)
          .clamp(HYSTART_MIN_RTT_THRESH_NS, HYSTART_MAX_RTT_THRESH_NS);
        if current >= last.saturating_add(threshold) {
          self.hystart = HyStart::Conservative {
            baseline: current,
            rounds: 0,
          };
        }
      }
      (HyStart::Conservative { baseline, .. }, Some(current), _) if current < baseline => {
        self.hystart = HyStart::SlowStart;
      }
      _ => {}
    }
  }

  /// Ends the round when a packet sent at or after its start is acknowledged, and counts Conservative
  /// Slow Start's rounds toward its limit.
  fn hystart_round_end(&mut self, newest_pn: Option<u64>) {
    let (Some(end), Some(newest)) = (self.window_end, newest_pn) else {
      return;
    };
    if newest < end {
      return;
    }
    self.window_end = Some(self.largest_sent.saturating_add(1));
    self.last_round_min_rtt = self.current_round_min_rtt;
    self.current_round_min_rtt = None;
    self.rtt_sample_count = 0;
    if let HyStart::Conservative { baseline, rounds } = self.hystart {
      let rounds = rounds.saturating_add(1);
      if rounds >= HYSTART_CSS_ROUNDS {
        self.exit_slow_start();
      } else {
        self.hystart = HyStart::Conservative { baseline, rounds };
      }
    }
  }

  /// Leaves the initial slow start for congestion avoidance at the current window (RFC 9406 §4.2).
  fn exit_slow_start(&mut self) {
    self.hystart = HyStart::Done;
    self.ssthresh = self.window;
    self.window_prior = self.window;
    // The first congestion-avoidance stage starts at the window slow start reached, with no congestion
    // event behind it: W_max is that window and K is zero, so the cubic grows convexly from it (the shape
    // RFC 9438 §4.8 gives a stage with no prior reduction), and W_est starts there too (§4.3).
    self.w_max = self.window;
    self.k_ns = 0;
    self.w_est = self.window;
    self.w_est_remainder = 0;
    self.increment_remainder = 0;
    self.epoch_start = None;
  }

  /// Congestion avoidance (RFC 9438 §4.2–§4.5): the larger of the cubic target and the Reno estimate.
  fn congestion_avoidance(&mut self, now: u64, acked: u64, srtt: u64) {
    // A new stage starts at the first acknowledgement in congestion avoidance (`t_epoch`, §4.2); `W_est`,
    // `W_max` and `K` were set when the stage was entered (a congestion event or slow start's exit).
    let start = *self.epoch_start.get_or_insert(now);
    let elapsed = now.saturating_sub(start).saturating_add(srtt);
    let target = self.w_cubic(elapsed).clamp(
      self.window,
      self.window.saturating_mul(TARGET_CAP_NUMERATOR) / TARGET_CAP_DENOMINATOR,
    );
    // W_est (Figure 4) in bytes: += α · acked · MSS / cwnd, α → 1 once W_est reaches cwnd_prior.
    let (alpha_numerator, alpha_denominator) = if self.w_est >= self.window_prior {
      (1, 1)
    } else {
      (ALPHA_NUMERATOR, ALPHA_DENOMINATOR)
    };
    let est_numerator =
      u128::from(alpha_numerator) * u128::from(acked) * u128::from(self.max_datagram)
        + u128::from(self.w_est_remainder);
    let est_denominator = u128::from(alpha_denominator) * u128::from(self.window.max(1));
    self.w_est = self
      .w_est
      .saturating_add(u64::try_from(est_numerator / est_denominator).unwrap_or(u64::MAX));
    self.w_est_remainder = u64::try_from(est_numerator % est_denominator).unwrap_or(0);
    if self.w_cubic(now.saturating_sub(start)) < self.w_est {
      // The Reno-friendly region (§4.3).
      self.window = self.w_est.max(self.window);
      return;
    }
    // Concave or convex (§4.4, §4.5): cwnd += (target − cwnd) / cwnd per acknowledged segment, in bytes.
    let numerator = u128::from(target.saturating_sub(self.window)) * u128::from(acked)
      + u128::from(self.increment_remainder);
    let denominator = u128::from(self.window.max(1));
    self.window = self
      .window
      .saturating_add(u64::try_from(numerator / denominator).unwrap_or(u64::MAX));
    self.increment_remainder = u64::try_from(numerator % denominator).unwrap_or(0);
  }

  /// `W_cubic(t)` in bytes for `t` nanoseconds into the stage (§4.2 Figure 1).
  fn w_cubic(&self, t_ns: u64) -> u64 {
    let offset =
      (i128::from(t_ns) - i128::from(self.k_ns)).clamp(-MAX_CUBIC_OFFSET_NS, MAX_CUBIC_OFFSET_NS);
    let cube = offset * offset * offset;
    let scaled =
      cube * i128::from(self.max_datagram) * C_NUMERATOR / (C_DENOMINATOR * NANOS_PER_SECOND_CUBED);
    let window = i128::from(self.w_max) + scaled;
    u64::try_from(window.max(0)).unwrap_or(u64::MAX)
  }

  /// A congestion event (§4.6, §4.7), once per recovery period; persistent congestion collapses to the
  /// minimum window and restarts slow start toward the reduced threshold (RFC 9002 §7.6.2, §4.8).
  pub fn on_loss(&mut self, event: &LossEvent<'_>) {
    let Some(newest) = event.lost.iter().map(|packet| packet.sent_at).max() else {
      return;
    };
    if !self.in_recovery(newest) {
      self.recovery_start = Some(event.now);
      // Fast convergence (§4.7).
      self.w_max = if self.window < self.w_max {
        u64::try_from(
          u128::from(self.window) * u128::from(BETA_DENOMINATOR + BETA_NUMERATOR)
            / (2 * u128::from(BETA_DENOMINATOR)),
        )
        .unwrap_or(u64::MAX)
      } else {
        self.window
      };
      self.window_prior = self.window;
      self.ssthresh =
        (self.window.saturating_mul(BETA_NUMERATOR) / BETA_DENOMINATOR).max(self.minimum_window());
      self.window = self.ssthresh;
      self.hystart = HyStart::Done;
      // A new congestion-avoidance stage: K from the distance back up to W_max (§4.2 Figure 2).
      self.epoch_start = None;
      self.w_est = self.window;
      self.w_est_remainder = 0;
      self.increment_remainder = 0;
      self.k_ns = cubic_k_ns(self.w_max.saturating_sub(self.window), self.max_datagram);
    }
    if event.persistent {
      self.window = self.minimum_window();
      self.recovery_start = None;
      self.epoch_start = None;
    }
  }
}

/// `K` in nanoseconds (RFC 9438 §4.2 Figure 2): `∛((W_max − cwnd_epoch) / C)` seconds, with the window
/// distance in bytes over `C · MSS` bytes per second³.
fn cubic_k_ns(distance_bytes: u64, max_datagram: u64) -> u64 {
  let radicand = u128::from(distance_bytes)
    .saturating_mul(C_DENOMINATOR.unsigned_abs())
    .saturating_mul(NANOS_PER_SECOND_CUBED.unsigned_abs())
    / (C_NUMERATOR.unsigned_abs() * u128::from(max_datagram.max(1)));
  u64::try_from(integer_cbrt(radicand)).unwrap_or(u64::MAX)
}

/// Format: the exponent of a cube, for the integer cube root.
const CUBE: u32 = 3;

/// The integer cube root of `value` (the largest `r` with `r³ ≤ value`), by Newton's method from an
/// over-estimate, exact over the whole `u128` range.
fn integer_cbrt(value: u128) -> u128 {
  if value < 2 {
    return value;
  }
  // An over-estimate: 2^ceil(bits/3).
  let bits = u128::BITS - value.leading_zeros();
  let mut root: u128 = 1u128 << bits.div_ceil(CUBE);
  loop {
    let next = (2 * root + value / (root * root)) / u128::from(CUBE);
    if next >= root {
      break;
    }
    root = next;
  }
  while root.checked_pow(CUBE).is_none_or(|cube| cube > value) {
    root -= 1;
  }
  root
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::conn::SentPacket;
  use crate::delivery::{RateSample, RateSnapshot};
  use crate::rtt::RttEstimator;

  /// Shape: a datagram of a thousand bytes.
  const MD: u64 = 1000;
  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;

  /// The cube root is exact at and around perfect cubes, across the whole range.
  #[test]
  fn the_integer_cube_root_is_exact() {
    for root in [0u128, 1, 2, 3, 10, 1000, 123_456, 1 << 42] {
      let cube = root * root * root;
      assert_eq!(integer_cbrt(cube), root, "∛{cube}");
      if cube > 0 {
        assert_eq!(integer_cbrt(cube - 1), root - 1, "∛({cube} − 1)");
      }
      if root > 0 {
        assert_eq!(integer_cbrt(cube + 1), root, "∛({cube} + 1)");
      }
    }
    assert_eq!(
      integer_cbrt(u128::MAX),
      6_981_463_658_331,
      "the top of the range"
    );
  }

  /// RFC 9438 §4.2: `K` is the time the cubic takes to climb back to `W_max`; `W(K)` equals `W_max`, and
  /// `W(0)` is the reduced window it starts from.
  #[test]
  fn the_cubic_climbs_back_to_w_max_at_k() {
    let mut law = Cubic::new(MD);
    law.w_max = 100 * MD;
    law.window = 70 * MD;
    law.k_ns = cubic_k_ns(30 * MD, MD);
    // K = ∛(30 / 0.4) s = ∛75 ≈ 4.217 s.
    assert!(
      (4_216 * MS..=4_218 * MS).contains(&law.k_ns),
      "K = {}",
      law.k_ns
    );
    assert!(
      law.w_cubic(law.k_ns).abs_diff(100 * MD) <= 1,
      "W(K) = W_max"
    );
    assert!(
      law.w_cubic(0).abs_diff(70 * MD) <= MD / 100,
      "W(0) is the reduced window"
    );
  }

  /// RFC 9438 §4.6/§4.7: a congestion event reduces the window by β = 0.7 and, when the window is below
  /// the previous `W_max`, lowers `W_max` further (fast convergence).
  #[test]
  fn a_congestion_event_reduces_by_beta_with_fast_convergence() {
    let mut law = Cubic::new(MD);
    law.window = 100 * MD;
    law.ssthresh = 50 * MD;
    law.w_max = 120 * MD;
    let lost = [SentPacket {
      pn: 5,
      sent_at: 10,
      bytes: MD,
      rate: RateSnapshot::default(),
    }];
    law.on_loss(&LossEvent {
      now: 20,
      lost: &lost,
      largest_sent: 9,
      in_flight: 0,
      lost_total: MD,
      persistent: false,
      srtt: 0,
    });
    assert_eq!(law.window(), 70 * MD, "cwnd × 0.7");
    assert_eq!(law.w_max, 85 * MD, "100 × (1 + 0.7) / 2");
  }

  /// RFC 9406 §4.2: an RTT that rises by the threshold across rounds moves slow start into Conservative
  /// Slow Start, which grows a quarter as fast.
  #[test]
  fn a_delay_increase_enters_conservative_slow_start() {
    let mut law = Cubic::new(MD);
    let rtt = RttEstimator::new();
    let mut pn = 0u64;
    let mut round = |law: &mut Cubic, round_rtt: u64| {
      let first = pn;
      for _ in 0..HYSTART_N_RTT_SAMPLE {
        law.on_sent(pn);
        pn += 1;
      }
      for acked_pn in first..pn {
        let packet = [SentPacket {
          pn: acked_pn,
          sent_at: 0,
          bytes: MD,
          rate: RateSnapshot::default(),
        }];
        law.on_ack(&AckEvent {
          now: 0,
          acked: &packet,
          sample: RateSample {
            rtt: Some(round_rtt),
            ..RateSample::default()
          },
          in_flight: 0,
          delivered: 0,
          cwnd_limited: true,
          rtt: &rtt,
        });
      }
    };
    round(&mut law, 100 * MS);
    round(&mut law, 100 * MS);
    assert_eq!(
      law.hystart,
      HyStart::SlowStart,
      "a flat RTT stays in slow start"
    );
    // A round ends when a packet sent at or after its start is acknowledged (RFC 9406 §4.2 `windowEnd`),
    // so the first acknowledgement of each flight closes the previous round: the rise is seen once a whole
    // round of samples at the higher RTT has been compared with the last round's minimum.
    round(&mut law, 120 * MS);
    round(&mut law, 120 * MS);
    assert!(
      matches!(law.hystart, HyStart::Conservative { .. }),
      "a 20 ms rise over a 100 ms RTT (threshold 12.5 ms) enters CSS: {:?}",
      law.hystart
    );
    let before = law.window();
    round(&mut law, 120 * MS);
    assert_eq!(
      law.window() - before,
      HYSTART_N_RTT_SAMPLE * MD / HYSTART_CSS_GROWTH_DIVISOR,
      "CSS grows a quarter as fast"
    );
  }
}
