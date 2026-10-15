//! Copa (Arun & Balakrishnan, "Copa: Practical Delay-Based Congestion Control for the Internet", NSDI
//! 2018 [A]; deployed in Meta's mvfst QUIC stack [C]) — a candidate in the session plane's congestion
//! bake-off (see the module above), added 2026-09-27 when the first bake-off runs showed every loss- or
//! probe-driven law letting the bottleneck queue reach a full buffer at its p99 (research note §5.3's goal:
//! a small request must not wait behind bulk data).
//!
//! Copa aims at a target rate `1/(δ·d_q)` where `d_q` is the measured queueing delay (`RTTstanding −
//! RTTmin`): a sender whose rate is below the target grows its window, above it shrinks it, by `v/(δ·cwnd)`
//! packets per acknowledged packet, the velocity `v` doubling after three round trips in one direction so a
//! far target is reached exponentially (paper §2.1). In equilibrium the queue oscillates between empty and
//! about `2.5/δ` packets every five round trips (§3) — a standing queue of a few packets, not a buffer. When a
//! buffer-filling flow shares the bottleneck (the queue is never nearly empty over four round trips), Copa
//! switches to a competitive mode that additively increases `1/δ` each loss-free round trip and halves it on
//! loss, so it keeps its share against Reno or CUBIC (§2.2); it returns to the default `δ` when the queue
//! empties again.
//!
//! Details fixed from the primary sources: `RTTmin` over 10 s, `RTTstanding` the minimum over the last
//! `srtt/2`, pacing at `2·cwnd/RTTstanding` (§2.1); the mode test "nearly empty" as a four-`srtt` minimum
//! RTT within 10 % of the four-`srtt` RTT spread above `RTTmin`, and competitive `1/δ` AIMD at most once per
//! RTT (the authors' reference implementation, `genericCC` `rtt-window.cc` and `markoviancc.cc` [C]); the
//! velocity capped at `cwnd·δ` packets (reference `update_amt`); slow start doubling once per `srtt` until
//! the first decrease, loss ignored in the default mode, persistent congestion collapsing to the minimum
//! window (mvfst `Copa.cpp` [C]). Windows are Nichols filters as in mvfst, so every structure is bounded.
//! `δ` is held as the integer `1/δ`, so the law is integer arithmetic throughout.
//!
//! One departure from the paper, measured (`docs/wip/BENCHMARKS.md`, "Copa reads independent jitter out of its queue
//! estimate", 2026-10-07): `d_q` is reduced by the jitter excess `RTTstanding` carries, estimated from each `srtt/2`
//! epoch's samples ([`Epoch`]), so independent non-congestive jitter of tens of milliseconds is not read as queue
//! (Arun, Alizadeh & Balakrishnan, "Starvation in End-to-End Congestion Control", SIGCOMM 2022 [A], §6). On a thin
//! 200 ms link with ±40 ms of independent jitter an 8 MiB pull went from 79–102 s to 24–33 s (median of 8 seeds),
//! with the 58-scenario grid's share +1.6% and ping p99 −1.9%. The first estimate modelled each sample's jitter as
//! uniform; a round trip's jitter is the sum of two directions', whose least sample sits about four times further
//! above the floor, so a second estimate reads that from the gap between an epoch's two least samples, guarded
//! against a slow link's queue (2026-10-08, "Copa reads summed jitter from its two least samples": the thin link's
//! reordering pulls 24.3 → 12.6 s and 32.8 → 13.3 s, the 200 ms crossing 20.7 → 9.6 s, the grid flat).

use super::filter::{WindowedMax, WindowedMin};
use super::{AckEvent, INITIAL_WINDOW_DATAGRAMS, LossEvent, MINIMUM_WINDOW_DATAGRAMS};

/// Format: Copa §2.1 — the minimum-RTT window, 10 s.
const MIN_RTT_WINDOW_NS: u64 = 10_000_000_000;
/// Format: Copa §2.2 / reference `rtt-window.cc` — the mode-detection window, four smoothed RTTs.
const MODE_WINDOW_SRTTS: u64 = 4;
/// Format: Copa §2.2 — "nearly empty" is within a tenth of the RTT spread above the minimum.
const NEARLY_EMPTY_FRACTION: u64 = 10;
/// Format: Copa §2.1 — the velocity starts doubling after the direction held for three windows.
const VELOCITY_DIRECTION_THRESHOLD: u32 = 3;
/// Format: Copa §2.1 — the sender paces at twice `cwnd/RTTstanding`.
const PACING_MULTIPLE: u64 = 2;

/// The window's direction over the last `srtt` (Copa §2.1's velocity rule).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Direction {
  Up,
  Down,
}

/// Copa's state.
#[derive(Debug)]
pub struct Copa {
  smss: u64,
  cwnd: u64,
  /// `1/δ` in the default mode (the paper's δ = 0.5 is 2; Meta's live-video δ = 0.04 is 25).
  default_inv_delta: u64,
  /// The current `1/δ` (default, or raised by the competitive mode's additive increase).
  inv_delta: u64,
  competitive: bool,
  slow_start: bool,
  /// When slow start last doubled the window.
  last_double: Option<u64>,
  min_rtt: Option<WindowedMin>,
  standing_rtt: Option<WindowedMin>,
  /// The RTT samples of the current `srtt/2` epoch and of the last whole one ([`Epoch`]): the jitter estimate.
  epoch: Epoch,
  last_epoch: Option<Epoch>,
  mode_min: Option<WindowedMin>,
  mode_max: Option<WindowedMax>,
  /// The velocity: the window's step multiplier.
  velocity: u64,
  direction: Direction,
  same_direction: u32,
  /// When the direction was last sampled, and the window then.
  direction_mark: Option<(u64, u64)>,
  /// The remainder of window growth carried between acknowledgements (sub-byte steps).
  step_remainder: u64,
  /// When the competitive mode last changed `1/δ` for a loss, and for a loss-free round trip.
  last_loss_update: u64,
  last_increase_update: u64,
}

impl Copa {
  /// A Copa sender with `1/δ = default_inv_delta`, in slow start at the initial window.
  pub fn new(smss: u64, default_inv_delta: u64) -> Copa {
    Copa {
      smss,
      cwnd: INITIAL_WINDOW_DATAGRAMS.saturating_mul(smss),
      default_inv_delta: default_inv_delta.max(1),
      inv_delta: default_inv_delta.max(1),
      competitive: false,
      slow_start: true,
      last_double: None,
      min_rtt: None,
      standing_rtt: None,
      epoch: Epoch::default(),
      last_epoch: None,
      mode_min: None,
      mode_max: None,
      velocity: 1,
      direction: Direction::Up,
      same_direction: 0,
      direction_mark: None,
      step_remainder: 0,
      last_loss_update: 0,
      last_increase_update: 0,
    }
  }

  /// The congestion window, bytes.
  pub fn window(&self) -> u64 {
    self.cwnd
  }

  /// The datagram size.
  pub fn max_datagram(&self) -> u64 {
    self.smss
  }

  /// Follows a new datagram size (path MTU discovery confirmed a larger one, or a black hole fell back to
  /// the floor; RFC 9002 §7.2): the window stays in bytes and is lifted to at least the minimum window at
  /// the new size, so the window-to-datagram ratio never falls below what loss recovery needs.
  pub fn set_max_datagram(&mut self, smss: u64) {
    self.smss = smss.max(1);
    self.cwnd = self
      .cwnd
      .max(MINIMUM_WINDOW_DATAGRAMS.saturating_mul(self.smss));
  }

  /// Whether Copa is in slow start.
  pub fn in_slow_start(&self) -> bool {
    self.slow_start
  }

  /// Whether the competitive mode is active (a buffer-filling flow shares the bottleneck).
  pub fn competitive(&self) -> bool {
    self.competitive
  }

  fn minimum_window(&self) -> u64 {
    MINIMUM_WINDOW_DATAGRAMS.saturating_mul(self.smss)
  }

  /// The pacing rate, bytes per second: `2·cwnd/RTTstanding` (§2.1), from the initial RTT before any sample.
  pub fn pacing_rate(&self, fallback_rtt: u64) -> u64 {
    let rtt = self
      .standing_rtt
      .map_or(fallback_rtt, |filter| filter.get())
      .max(1);
    crate::pacer::rate(self.cwnd.saturating_mul(PACING_MULTIPLE), rtt)
  }

  /// Processes an acknowledgement (§2.1, §2.2).
  pub fn on_ack(&mut self, event: &AckEvent<'_>) {
    let Some(rtt) = event.rtt_sample else {
      return;
    };
    let now = event.now;
    let srtt = event.rtt.smoothed_rtt_or_initial().max(1);
    let rtt_min = update_min(&mut self.min_rtt, now, MIN_RTT_WINDOW_NS, rtt);
    let standing = update_min(&mut self.standing_rtt, now, srtt / 2, rtt);
    let mode_window = srtt.saturating_mul(MODE_WINDOW_SRTTS);
    let recent_min = update_min(&mut self.mode_min, now, mode_window, rtt);
    let recent_max = match self.mode_max.as_mut() {
      Some(filter) => filter.update(now, mode_window, rtt),
      None => self.mode_max.insert(WindowedMax::new(now, rtt)).get(),
    };
    self.update_mode(now, srtt, (rtt_min, recent_min, recent_max));
    if self.epoch.count > 0 && now.saturating_sub(self.epoch.began) > srtt / 2 {
      self.last_epoch = Some(self.epoch);
      self.epoch = Epoch::default();
    }
    // The sender's own interval per packet, `smss·RTTstanding/cwnd`: a queue's samples are at least this far apart.
    let packet_interval = u128::from(self.smss)
      .saturating_mul(u128::from(standing))
      .checked_div(u128::from(self.cwnd.max(1)))
      .and_then(|value| u64::try_from(value).ok())
      .unwrap_or(0);
    self.epoch.take(now, rtt, packet_interval);
    let queueing = standing
      .saturating_sub(rtt_min)
      .saturating_sub(self.last_epoch.map_or(0, |epoch| epoch.jitter_excess()));
    // Increase when the current rate `cwnd/RTTstanding` is at or below the target `1/(δ·d_q)` (in bytes,
    // `inv_delta·smss/d_q`): `cwnd·d_q ≤ inv_delta·smss·RTTstanding`; an empty queue always increases.
    let increase = queueing == 0
      || u128::from(self.cwnd).saturating_mul(u128::from(queueing))
        <= u128::from(self.inv_delta)
          .saturating_mul(u128::from(self.smss))
          .saturating_mul(u128::from(standing));
    if increase && !event.cwnd_limited {
      // RFC 9002 §7.8: a window the sender is not using does not grow (Copa's delay signal would otherwise
      // keep raising an idle flow's window without bound). It still shrinks: the guard once skipped every
      // update, so a window slow start had overshot stayed frozen while losses kept the sender from looking
      // window-limited — 1.5 MB at a 250 kB BDP, 92,500 queue drops per run at 100 Mbit/s (2026-09-28).
      return;
    }
    if self.slow_start && increase {
      match self.last_double {
        None => self.last_double = Some(now),
        Some(then) if now.saturating_sub(then) > srtt => {
          self.cwnd = self.cwnd.saturating_mul(2);
          self.last_double = Some(now);
        }
        Some(_) => {}
      }
      return;
    }
    self.update_direction(now, srtt);
    let wanted = if increase {
      Direction::Up
    } else {
      Direction::Down
    };
    if wanted != self.direction && self.velocity > 1 {
      // A sudden reversal while the velocity is high: reset it (mvfst `changeDirection`).
      self.direction = wanted;
      self.velocity = 1;
      self.same_direction = 0;
      self.direction_mark = Some((now, self.cwnd));
    }
    // cwnd ± v/(δ·cwnd) packets per acknowledged packet, in bytes: acked · smss · v · (1/δ) / cwnd.
    let numerator = u128::from(event.newly_acked)
      .saturating_mul(u128::from(self.smss))
      .saturating_mul(u128::from(self.velocity))
      .saturating_mul(u128::from(self.inv_delta))
      .saturating_add(u128::from(self.step_remainder));
    let denominator = u128::from(self.cwnd.max(1));
    let step = u64::try_from(numerator.checked_div(denominator).unwrap_or(0)).unwrap_or(u64::MAX);
    self.step_remainder =
      u64::try_from(numerator.checked_rem(denominator).unwrap_or(0)).unwrap_or(0);
    if increase {
      self.cwnd = self.cwnd.saturating_add(step);
    } else {
      self.slow_start = false;
      self.cwnd = self.cwnd.saturating_sub(step).max(self.minimum_window());
    }
  }

  /// Once per `srtt`, compares the window with its value a round trip ago: the same direction three times
  /// running doubles the velocity, a change resets it (§2.1); the velocity is capped at `cwnd·δ` packets
  /// (reference `update_amt`), so one step never exceeds the window's own scale.
  fn update_direction(&mut self, now: u64, srtt: u64) {
    let Some((then, cwnd_then)) = self.direction_mark else {
      self.direction_mark = Some((now, self.cwnd));
      return;
    };
    if now.saturating_sub(then) < srtt {
      return;
    }
    let direction = if self.cwnd > cwnd_then {
      Direction::Up
    } else {
      Direction::Down
    };
    if direction == self.direction {
      self.same_direction = self.same_direction.saturating_add(1);
      if self.same_direction >= VELOCITY_DIRECTION_THRESHOLD {
        self.velocity = self.velocity.saturating_mul(2);
      }
    } else {
      self.velocity = 1;
      self.same_direction = 0;
    }
    let cap = self
      .cwnd
      .checked_div(self.smss.max(1))
      .and_then(|packets| packets.checked_div(self.inv_delta.max(1)))
      .unwrap_or(0)
      .max(1);
    self.velocity = self.velocity.min(cap);
    self.direction = direction;
    self.direction_mark = Some((now, self.cwnd));
  }

  /// The mode test (§2.2; reference `is_copa`): the queue was nearly empty in the last four `srtt` when the
  /// window's minimum RTT came within a tenth of the spread above `RTTmin`. Competitive mode raises `1/δ`
  /// by one each loss-free `srtt` (its loss halving is in `on_loss`); the default mode restores `1/δ`.
  fn update_mode(
    &mut self,
    now: u64,
    srtt: u64,
    (rtt_min, recent_min, recent_max): (u64, u64, u64),
  ) {
    let spread = recent_max.saturating_sub(rtt_min);
    let nearly_empty =
      recent_min < rtt_min.saturating_add(spread / NEARLY_EMPTY_FRACTION) || recent_min == rtt_min;
    if nearly_empty {
      self.competitive = false;
      self.inv_delta = self.default_inv_delta;
      return;
    }
    if !self.competitive {
      self.competitive = true;
      self.last_increase_update = now;
    }
    if now.saturating_sub(self.last_increase_update) > srtt
      && now.saturating_sub(self.last_loss_update) > srtt
    {
      self.inv_delta = self.inv_delta.saturating_add(1);
      self.last_increase_update = now;
    }
  }

  /// A loss: in the competitive mode, `1/δ` halves (at most once per RTT, never below the default); in the
  /// default mode a loss is not a signal (Copa is loss-insensitive, §1); persistent congestion collapses to
  /// the minimum window (mvfst).
  pub fn on_loss(&mut self, event: &LossEvent) {
    let srtt = event.srtt;
    if self.competitive && event.now.saturating_sub(self.last_loss_update) > srtt {
      self.inv_delta = (self.inv_delta / 2).max(self.default_inv_delta);
      self.last_loss_update = event.now;
    }
    if event.persistent {
      self.cwnd = self.minimum_window();
      self.slow_start = false;
    }
  }
}

/// One `srtt/2` epoch's RTT samples: when it began, how many, the least and the greatest, the first and the last.
#[derive(Clone, Copy, Debug, Default)]
struct Epoch {
  began: u64,
  count: u64,
  min: u64,
  max: u64,
  first: u64,
  last: u64,
  /// The path length: the sum of the absolute differences between successive samples.
  path: u64,
  /// The second-least sample (`u64::MAX` until there are two).
  second: u64,
  /// The turning points: samples where the direction of change reversed.
  turns: u64,
  /// The direction of the last non-zero change (`Some(true)` when rising).
  rising: Option<bool>,
  /// The sender's own interval per packet at the epoch's last sample, nanoseconds.
  packet_interval: u64,
}

impl Epoch {
  fn take(&mut self, now: u64, rtt: u64, packet_interval: u64) {
    if self.count == 0 {
      *self = Epoch {
        began: now,
        count: 1,
        min: rtt,
        max: rtt,
        first: rtt,
        last: rtt,
        path: 0,
        second: u64::MAX,
        turns: 0,
        rising: None,
        packet_interval,
      };
      return;
    }
    self.packet_interval = packet_interval;
    if rtt != self.last {
      let up = rtt > self.last;
      if self.rising.is_some_and(|was| was != up) {
        self.turns = self.turns.saturating_add(1);
      }
      self.rising = Some(up);
    }
    self.count = self.count.saturating_add(1);
    if rtt < self.min {
      self.second = self.min;
      self.min = rtt;
    } else if rtt < self.second {
      self.second = rtt;
    }
    self.max = self.max.max(rtt);
    self.path = self.path.saturating_add(self.last.abs_diff(rtt));
    self.last = rtt;
  }

  /// The part of `RTTstanding − RTTmin` independent jitter explains (Arun, Alizadeh & Balakrishnan, SIGCOMM 2022,
  /// §6: a delay-bounding law must not read non-congestive jitter as queue). The larger of two estimates:
  ///
  /// - **Uniform jitter.** With jitter of width `W` independent per sample, the least of an epoch's `n` samples
  ///   sits `W/(n+1)` above the path's floor (order statistics of `n` uniform draws), and successive samples differ
  ///   by `W/3` on average, so the epoch's path length is about `(n−1)·W/3`. A queue moves the RTT monotonically
  ///   within half a round trip, so its path length is its net change `|last − first|`: the path length beyond the
  ///   net change is spread no trend explains, `W ≈ 3·(path − |last − first|)/(n−1)`, and the excess `W/(n+1)`.
  /// - **Any lower tail, from the two least samples.** A round trip's jitter is the sum of the two directions', so
  ///   near its floor its density rises from zero instead of starting flat, and the least of `n` samples sits much
  ///   further above the floor than `W/(n+1)`. For a lower tail `F(x) ∝ x^α` the least sample's expected excess
  ///   over the floor is `α` times the expected gap to the second least (`E[X₍₁₎] ∝ Γ(1+1/α)`,
  ///   `E[X₍₂₎] ∝ Γ(2+1/α)`; extreme-value order statistics, as in Hill's tail estimator, Annals of Statistics
  ///   1975). The sum of two independent jitters has `α = 2`. Two guards keep a queue out of it. The gap first
  ///   loses the sender's own interval per packet, because a queue's successive samples are spaced by at least a
  ///   service time and a slow link's queue would otherwise pass for jitter. It then counts in proportion to the
  ///   epoch's turning points against the `2(n−2)/3` independent samples show (the turning-point test, Brockwell
  ///   & Davis, *Introduction to Time Series and Forecasting*, §1.6): a filling or draining queue turns rarely.
  ///
  /// Two samples cannot tell jitter from a queue and estimate none, so a slow link's few samples (a packet's 150 ms
  /// at 64 kbit/s) are never read as jitter. Measured (`docs/wip/BENCHMARKS.md`, "Copa reads summed jitter from its
  /// two least samples", 2026-10-08).
  fn jitter_excess(&self) -> u64 {
    let zigzag = self.path.saturating_sub(self.last.abs_diff(self.first));
    let width = zigzag
      .saturating_mul(UNIFORM_MEAN_STEP_DIVISOR)
      .checked_div(self.count.saturating_sub(1).max(1))
      .unwrap_or(0);
    let uniform = width.checked_div(self.count.saturating_add(1)).unwrap_or(0);
    if self.count < TAIL_ESTIMATE_MIN_SAMPLES || self.path == 0 || self.second == u64::MAX {
      return uniform;
    }
    let spacing = self
      .second
      .saturating_sub(self.min)
      .saturating_sub(self.packet_interval);
    let independent_turns = self
      .count
      .saturating_sub(2)
      .saturating_mul(TURNING_POINTS_NUMERATOR)
      / TURNING_POINTS_DENOMINATOR;
    let tail = u128::from(spacing)
      .saturating_mul(u128::from(SUMMED_JITTER_TAIL_SHAPE))
      .saturating_mul(u128::from(self.turns.min(independent_turns)))
      .checked_div(u128::from(independent_turns.max(1)))
      .unwrap_or(0);
    uniform.max(u64::try_from(tail).unwrap_or(u64::MAX))
  }
}

/// Format: the mean absolute difference of two independent draws uniform over a width `W` is `W/3`; the jitter
/// estimate multiplies the mean step by this to recover `W`.
const UNIFORM_MEAN_STEP_DIVISOR: u64 = 3;

/// Format: the lower-tail shape `α` of a round trip's jitter, the sum of two independent one-way jitters. Each
/// has a density near its floor; their sum's density rises linearly from its floor (`F(x) ∝ x²`), so `α = 2` and
/// the least sample's excess is twice the gap to the second least ([`Epoch::jitter_excess`]).
const SUMMED_JITTER_TAIL_SHAPE: u64 = 2;
/// Format: the turning-point test — `n` independent samples have `2(n−2)/3` turning points in expectation
/// (Brockwell & Davis, *Introduction to Time Series and Forecasting*, §1.6); this is the 2.
const TURNING_POINTS_NUMERATOR: u64 = 2;
/// Format: the fewest samples the tail estimate reads — two least samples and an interior point, the least a
/// turning point needs (one sample on each side).
const TAIL_ESTIMATE_MIN_SAMPLES: u64 = 3;
/// Format: the turning-point test's 3 (see [`TURNING_POINTS_NUMERATOR`]).
const TURNING_POINTS_DENOMINATOR: u64 = 3;

/// Updates a lazily started windowed minimum and returns it.
fn update_min(filter: &mut Option<WindowedMin>, now: u64, window: u64, value: u64) -> u64 {
  match filter.as_mut() {
    Some(filter) => filter.update(now, window, value),
    None => filter.insert(WindowedMin::new(now, value)).get(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::rtt::RttEstimator;

  /// Shape: a thousand-byte datagram.
  const MD: u64 = 1000;
  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;

  fn ack(law: &mut Copa, now: u64, rtt: u64, acked: u64, estimator: &RttEstimator) {
    ack_with(law, now, rtt, acked, estimator, true);
  }

  /// [`ack`], stating whether the sender was window-limited when the acknowledged packet left.
  fn ack_with(
    law: &mut Copa,
    now: u64,
    rtt: u64,
    acked: u64,
    estimator: &RttEstimator,
    cwnd_limited: bool,
  ) {
    law.on_ack(&AckEvent {
      now,
      rtt_sample: Some(rtt),
      newly_acked: acked,
      cwnd_limited,
      rtt: estimator,
    });
  }

  /// §2.1: with no queue (every RTT at the minimum) the window only grows — slow start doubles it once per
  /// round trip.
  #[test]
  fn an_empty_queue_doubles_the_window_per_round_trip_in_slow_start() {
    let mut law = Copa::new(MD, 2);
    let mut estimator = RttEstimator::new();
    estimator.on_sample(100 * MS, 0);
    let start = law.window();
    for step in 0..=21u64 {
      ack(&mut law, step * 10 * MS, 100 * MS, MD, &estimator);
    }
    assert_eq!(
      law.window(),
      2 * start,
      "one doubling after a round trip past the first"
    );
  }

  /// The samples of one epoch, taken in order, by a sender whose own interval per packet is `packet_interval`.
  fn epoch_paced(samples: &[u64], packet_interval: u64) -> Epoch {
    let mut epoch = Epoch::default();
    for (at, rtt) in samples.iter().enumerate() {
      epoch.take(u64::try_from(at).unwrap(), *rtt, packet_interval);
    }
    epoch
  }

  /// The samples of one epoch, taken in order by a fast sender (no interval per packet to discount).
  fn epoch_of(samples: &[u64]) -> Epoch {
    epoch_paced(samples, 0)
  }

  /// The order statistics of summed jitter: a round trip's jitter is the sum of the two directions', and the least
  /// of `n` such samples sits about `a·√(π/2n)` above the floor, far more than `W/(n+1)`. Do: draw 400 epochs of 20
  /// round trips, each a 100 ms floor plus two independent one-way jitters uniform over 0–80 ms (a fixed xorshift,
  /// so the test is deterministic), and estimate each epoch's excess. Expect: the mean estimate within half to one
  /// and a half times the mean true excess (the epoch's least sample less the floor), where the uniform model alone
  /// reads about a quarter of it.
  #[test]
  fn summed_jitter_is_estimated_from_the_two_least_samples() {
    let ms = 1_000_000u64;
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut draw = |width: u64| {
      state ^= state << 13;
      state ^= state >> 7;
      state ^= state << 17;
      state % width
    };
    let (epochs, samples_per_epoch, floor, one_way) = (400u64, 20, 100 * ms, 80 * ms);
    let (mut estimated, mut actual) = (0u64, 0u64);
    for _ in 0..epochs {
      let samples: Vec<u64> = (0..samples_per_epoch)
        .map(|_| floor + draw(one_way) + draw(one_way))
        .collect();
      let least = samples.iter().copied().min().unwrap();
      actual += least - floor;
      estimated += epoch_of(&samples).jitter_excess();
    }
    let (estimated, actual) = (estimated / epochs, actual / epochs);
    assert!(
      estimated * 2 >= actual && estimated * 2 <= actual * 3,
      "estimated {estimated} ns against a true excess of {actual} ns"
    );
  }

  /// A slow link's own queue is not summed jitter: a paced sender at 1 Mbit/s spaces its packets 8 ms apart, and
  /// its samples climb a staircase that dips between bursts (it turns often). Do: estimate the excess of such an
  /// epoch with the sender's 8 ms interval. Expect: no more than the uniform model's millisecond, because the gap
  /// between the two least samples is within one packet interval.
  #[test]
  fn a_paced_slow_link_queue_is_not_read_as_summed_jitter() {
    let ms = 1_000_000u64;
    let staircase = [100, 108, 104, 112, 108, 116, 112, 120, 116].map(|value| value * ms);
    let excess = epoch_paced(&staircase, 8 * ms).jitter_excess();
    assert!(
      excess <= 2 * ms,
      "a paced queue read as {excess} ns of jitter"
    );
    assert!(
      epoch_of(&staircase).jitter_excess() > excess,
      "non-vacuity: without the sender's interval the same samples do read as jitter"
    );
  }

  /// Arun, Alizadeh & Balakrishnan (SIGCOMM 2022) §6: jitter is not read as queue, and a queue is not read as
  /// jitter. Do: estimate the jitter excess of a filling queue (RTT rising monotonically), of two samples, and of
  /// zigzagging samples spread over 80 ms. Expect: none for the queue and for two samples (a straight line either
  /// way), and for the zigzag about `W/(n+1)`: within a factor of two of 80 ms over nine samples.
  #[test]
  fn the_jitter_estimate_reads_zigzag_and_never_a_monotone_queue() {
    let ms = 1_000_000;
    let filling: Vec<u64> = (0..9).map(|step| 200 * ms + step * 5 * ms).collect();
    assert_eq!(
      epoch_of(&filling).jitter_excess(),
      0,
      "a filling queue is not jitter"
    );
    let draining: Vec<u64> = (0..9).rev().map(|step| 200 * ms + step * 5 * ms).collect();
    assert_eq!(
      epoch_of(&draining).jitter_excess(),
      0,
      "a draining queue is not jitter"
    );
    assert_eq!(
      epoch_of(&[200 * ms, 280 * ms]).jitter_excess(),
      0,
      "two samples cannot tell jitter from a queue"
    );
    let zigzag = [200, 270, 210, 260, 230, 280, 205, 250, 220].map(|value| value * ms);
    let excess = epoch_of(&zigzag).jitter_excess();
    let expected = 80 * ms / 10;
    assert!(
      excess >= expected / 2 && excess <= expected * 2,
      "about W/(n+1) = {expected} ns: {excess} ns"
    );
  }

  /// §2.1: once the standing queue's delay exceeds the target's, the window shrinks and slow start ends.
  #[test]
  fn a_queue_past_the_target_shrinks_the_window() {
    let mut law = Copa::new(MD, 2);
    let mut estimator = RttEstimator::new();
    estimator.on_sample(100 * MS, 0);
    ack(&mut law, 0, 100 * MS, MD, &estimator);
    let before = law.window();
    // Queueing of 100 ms at a 10-packet window: target 1/(0.5 · 0.1 s) = 20 packets/s, current 10 packets
    // per 200 ms = 50 packets/s — above the target, so shrink.
    // RTTstanding is the minimum over the last srtt/2 (50 ms), so the samples must come after that window
    // has moved past the 100 ms one.
    for step in 1..=5u64 {
      ack(&mut law, 100 * MS + step * MS, 200 * MS, MD, &estimator);
    }
    assert!(!law.in_slow_start(), "a decrease ends slow start");
    assert!(law.window() < before, "the window shrank");
  }

  /// RFC 9002 §7.8 bounds only growth: a window the sender is not filling does not grow, but a standing
  /// queue still shrinks it. Do X (a queue past the target while the sender is not window-limited), expect
  /// Y (the window shrinks); and with no queue, it does not grow. Regression: the guard skipped every
  /// update, so a window slow start had overshot stayed frozen at 1.5 MB (six BDPs) while losses kept the
  /// sender from looking window-limited — 92,500 queue drops per run and a 103 ms ping p99 on a 20 ms
  /// path (2026-09-28).
  #[test]
  fn a_window_the_sender_is_not_filling_still_shrinks_but_never_grows() {
    let mut law = Copa::new(MD, 2);
    let mut estimator = RttEstimator::new();
    estimator.on_sample(100 * MS, 0);
    ack_with(&mut law, 0, 100 * MS, MD, &estimator, false);
    let before = law.window();
    for step in 1..=5u64 {
      ack_with(
        &mut law,
        100 * MS + step * MS,
        200 * MS,
        MD,
        &estimator,
        false,
      );
    }
    assert!(
      law.window() < before,
      "the standing queue shrank an unfilled window"
    );
    let shrunk = law.window();
    for step in 1..=5u64 {
      ack_with(
        &mut law,
        10_000 * MS + step * MS,
        100 * MS,
        MD,
        &estimator,
        false,
      );
    }
    assert!(
      law.window() <= shrunk,
      "an empty queue did not grow an unfilled window"
    );
  }

  /// §2.2: when the queue never nearly empties over four round trips (a buffer-filling competitor), Copa
  /// switches to the competitive mode and raises `1/δ`; a loss halves it back toward the default.
  #[test]
  fn a_standing_queue_switches_to_the_competitive_mode() {
    let mut law = Copa::new(MD, 2);
    let mut estimator = RttEstimator::new();
    estimator.on_sample(100 * MS, 0);
    ack(&mut law, 0, 100 * MS, MD, &estimator);
    // Every later sample carries at least 80 ms of queueing, oscillating up to 120 ms: never nearly empty.
    for step in 1..=200u64 {
      let rtt = if step % 2 == 0 { 180 * MS } else { 220 * MS };
      ack(&mut law, 500 * MS + step * 10 * MS, rtt, MD, &estimator);
    }
    assert!(law.competitive(), "the standing queue was detected");
    assert!(
      law.inv_delta > 2,
      "1/δ grew each loss-free round trip ({})",
      law.inv_delta
    );
    let raised = law.inv_delta;
    law.on_loss(&LossEvent {
      now: 3_000 * MS,
      persistent: false,
      srtt: 100 * MS,
    });
    assert_eq!(law.inv_delta, (raised / 2).max(2), "a loss halved 1/δ");
  }
}
