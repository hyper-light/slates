//! Round-trip-time estimation and the probe timeout for the session-plane connection (RFC 9002 §5.3
//! and §6.2.1). This is the sans-io control law only: it folds each RTT sample into a smoothed estimate
//! and its variation, tracks the minimum, and computes the probe timeout (PTO) the sender uses to
//! recover a tail loss the packet-reorder threshold cannot see (a lost last packet has no later
//! acknowledged packet to open a gap past it — RFC 9002 §6.2). It owns no clock: the caller measures a
//! sample as `now − the send time of the acknowledged packet` and folds it in here.
//!
//! Deliberately *not* here: the timer that fires the probe, and the empirical tuning — those need a real
//! clock and network (the endpoint's clock schedules the probe over the wire; the fleet's Phase 8
//! measurement work tunes it). The estimation *law* is exact and unit-testable at N=1 by injecting
//! samples and asserting the smoothed RTT, its variation, and the PTO — the way the congestion and
//! flow-control cores are, with no clock and no network.

/// Format: RFC 9002 §6.2.1 `kGranularity` — the timer granularity, 1 ms. The PTO's variation term never
/// falls below it, so the timeout never undercuts what a timer can resolve. A protocol constant.
pub const GRANULARITY_NS: u64 = 1_000_000;
/// Format: RFC 9002 §6.2.2 `kInitialRtt` — 333 ms, the RTT assumed before any sample is taken, from
/// which the PTO starts. A protocol constant.
const INITIAL_RTT_NS: u64 = 333_000_000;
/// Format: RFC 9002 §5.3 — the smoothed-RTT exponential average weights the new sample 1/8 (the old
/// estimate 7/8); a right shift by 3 is the division by 8. A protocol constant.
const SMOOTHED_RTT_SHIFT: u32 = 3;
/// Format: RFC 9002 §5.3 — the RTT-variation exponential average weights the new sample 1/4 (the old
/// 3/4); a right shift by 2 is the division by 4. A protocol constant.
const RTTVAR_SHIFT: u32 = 2;
/// Format: RFC 9002 §6.2.1 — the PTO adds four times the RTT variation to the smoothed RTT. A protocol
/// constant.
const PTO_RTTVAR_MULTIPLIER: u64 = 4;
/// Format: RFC 9002 §6.2.1 — before any RTT sample, the PTO is twice the initial RTT. A protocol
/// constant (the "2 ×" of `2 * kInitialRtt`).
const INITIAL_PTO_MULTIPLIER: u64 = 2;

/// The sender's RTT estimator (RFC 9002 §5.3): the minimum RTT seen, the smoothed RTT, and its
/// variation, all in nanoseconds, plus whether a sample has been taken yet.
#[derive(Debug, Default)]
pub struct RttEstimator {
  /// The smallest RTT sample seen (RFC 9002 §5.2), the ack-delay-removal floor.
  min_rtt: u64,
  /// The smoothed RTT (the exponential average).
  smoothed_rtt: u64,
  /// The mean deviation of the RTT (RFC 9002's `rttvar`).
  rttvar: u64,
  /// Whether any sample has been folded in (before the first, the PTO uses the initial RTT).
  have_sample: bool,
}

impl RttEstimator {
  /// An estimator with no sample yet.
  pub fn new() -> RttEstimator {
    RttEstimator::default()
  }

  /// Folds one RTT sample in (RFC 9002 §5.3). `latest` is the measured round trip (`now` minus the send
  /// time of the newly acknowledged packet); `ack_delay` is the delay the peer reported between
  /// receiving that packet and acknowledging it (zero if this dialect does not carry it). The ack delay
  /// is removed from the sample only when doing so keeps it at or above the minimum RTT, so a spuriously
  /// large reported delay cannot drag the estimate below the path's floor.
  pub fn on_sample(&mut self, latest: u64, ack_delay: u64) {
    self.min_rtt = if self.have_sample {
      self.min_rtt.min(latest)
    } else {
      latest
    };
    if !self.have_sample {
      // The first sample seeds the estimate (RFC 9002 §5.3): smoothed = sample, variation = sample / 2.
      self.smoothed_rtt = latest;
      self.rttvar = latest / 2;
      self.have_sample = true;
      return;
    }
    // Remove the peer's ack delay when the sample still sits at or above the minimum RTT (RFC 9002 §5.3).
    // The guard makes `latest - ack_delay` non-negative (latest >= min_rtt + ack_delay >= ack_delay).
    let adjusted = if latest >= self.min_rtt.saturating_add(ack_delay) {
      latest - ack_delay
    } else {
      latest
    };
    // rttvar = 3/4 · rttvar + 1/4 · |smoothed − adjusted|; smoothed = 7/8 · smoothed + 1/8 · adjusted.
    let var_sample = self.smoothed_rtt.abs_diff(adjusted);
    self.rttvar = self.rttvar - (self.rttvar >> RTTVAR_SHIFT) + (var_sample >> RTTVAR_SHIFT);
    self.smoothed_rtt = self.smoothed_rtt - (self.smoothed_rtt >> SMOOTHED_RTT_SHIFT)
      + (adjusted >> SMOOTHED_RTT_SHIFT);
  }

  /// The probe timeout in nanoseconds (RFC 9002 §6.2.1): `smoothed_rtt + max(4 · rttvar, granularity) +
  /// max_ack_delay`. Before any sample it is twice the initial RTT (§6.2.2). `max_ack_delay` is the
  /// largest delay the peer may take to acknowledge (zero if this dialect does not negotiate it).
  pub fn pto(&self, max_ack_delay: u64) -> u64 {
    if !self.have_sample {
      return INITIAL_PTO_MULTIPLIER.saturating_mul(INITIAL_RTT_NS);
    }
    let variation = PTO_RTTVAR_MULTIPLIER
      .saturating_mul(self.rttvar)
      .max(GRANULARITY_NS);
    self
      .smoothed_rtt
      .saturating_add(variation)
      .saturating_add(max_ack_delay)
  }

  /// The initial PTO (RFC 9002 §6.2.2): twice the initial RTT, the timeout before any sample — the same
  /// value [`pto`](RttEstimator::pto) returns before a sample, but returned even after one. The handshake
  /// caps its retransmit interval at this: once the first flight's round trip has seeded a *tiny* smoothed
  /// RTT (a loopback or same-host peer), `pto` drops to about the timer granularity, which would retry the
  /// handshake so aggressively it exhausts its retransmit budget in tens of milliseconds — too impatient
  /// for a peer whose shard is briefly busy establishing the rest of a mesh. The conservative initial PTO
  /// is the right ceiling for establishing a connection, where the in-progress sample is not yet a
  /// trustworthy basis for giving up.
  pub fn initial_pto(&self) -> u64 {
    INITIAL_PTO_MULTIPLIER.saturating_mul(INITIAL_RTT_NS)
  }

  /// The smoothed RTT (nanoseconds), for the connection's diagnostics and assertions.
  pub fn smoothed_rtt(&self) -> u64 {
    self.smoothed_rtt
  }

  /// The RTT variation (nanoseconds).
  pub fn rttvar(&self) -> u64 {
    self.rttvar
  }

  /// The minimum RTT seen (nanoseconds).
  pub fn min_rtt(&self) -> u64 {
    self.min_rtt
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A millisecond in nanoseconds, so the samples read as round times.
  const MS: u64 = 1_000_000;

  /// Before any sample the PTO is twice the initial RTT (RFC 9002 §6.2.2), so the connection has a
  /// timeout to arm from the very first packet.
  #[test]
  fn the_initial_pto_is_twice_the_initial_rtt() {
    let rtt = RttEstimator::new();
    assert_eq!(rtt.pto(0), 2 * 333 * MS, "2 × kInitialRtt before a sample");
  }

  /// The first sample seeds the estimate: smoothed = the sample, variation = half it (RFC 9002 §5.3),
  /// and the PTO is smoothed + 4·variation (which dominates the 1 ms granularity here).
  #[test]
  fn the_first_sample_seeds_the_estimate() {
    let mut rtt = RttEstimator::new();
    rtt.on_sample(80 * MS, 0);
    assert_eq!(rtt.smoothed_rtt(), 80 * MS, "smoothed = the first sample");
    assert_eq!(rtt.rttvar(), 40 * MS, "variation = half the first sample");
    assert_eq!(rtt.min_rtt(), 80 * MS, "the minimum is the only sample");
    // PTO = 80 + max(4 × 40, granularity) = 80 + 160 = 240 ms.
    assert_eq!(rtt.pto(0), 240 * MS);
  }

  /// A second, larger sample moves the smoothed RTT a fraction of the way and grows the variation, by
  /// the RFC 9002 §5.3 exponential averages (7/8 and 3/4 on the old, 1/8 and 1/4 on the new).
  #[test]
  fn a_later_sample_moves_the_estimate_a_fraction() {
    let mut rtt = RttEstimator::new();
    rtt.on_sample(80 * MS, 0); // smoothed 80, rttvar 40, min 80
    rtt.on_sample(160 * MS, 0);
    // rttvar = 40 − 40/4 + |80 − 160|/4 = 40 − 10 + 20 = 50 ms.
    assert_eq!(rtt.rttvar(), 50 * MS);
    // smoothed = 80 − 80/8 + 160/8 = 80 − 10 + 20 = 90 ms.
    assert_eq!(rtt.smoothed_rtt(), 90 * MS);
    assert_eq!(
      rtt.min_rtt(),
      80 * MS,
      "the minimum stays the smaller sample"
    );
    // PTO = 90 + max(4 × 50, 1) = 90 + 200 = 290 ms.
    assert_eq!(rtt.pto(0), 290 * MS);
  }

  /// The peer's ack delay is removed from a sample when the result stays at or above the minimum RTT
  /// (RFC 9002 §5.3), so the estimate reflects the path, not the peer's acknowledgement scheduling.
  #[test]
  fn the_ack_delay_is_removed_above_the_minimum() {
    let mut rtt = RttEstimator::new();
    rtt.on_sample(80 * MS, 0); // smoothed 80, rttvar 40, min 80
    // latest 88, ack_delay 8: 88 >= min(80) + 8, so adjusted = 88 − 8 = 80 (back to the minimum).
    rtt.on_sample(88 * MS, 8 * MS);
    // |80 − 80| = 0, so rttvar shrinks: 40 − 10 + 0 = 30 ms; smoothed = 80 − 10 + 10 = 80 ms (unchanged).
    assert_eq!(
      rtt.smoothed_rtt(),
      80 * MS,
      "the delay-removed sample equals the estimate"
    );
    assert_eq!(
      rtt.rttvar(),
      30 * MS,
      "no deviation, so the variation decays"
    );
  }

  /// A reported ack delay that would drag the sample below the minimum RTT is *not* removed (RFC 9002
  /// §5.3's floor), so a bogus large delay cannot pull the estimate under the path's real floor.
  #[test]
  fn an_ack_delay_below_the_minimum_is_not_removed() {
    let mut rtt = RttEstimator::new();
    rtt.on_sample(80 * MS, 0); // min 80
    // latest 84, ack_delay 40: 84 < min(80) + 40 = 120, so the delay is NOT removed; adjusted = 84.
    rtt.on_sample(84 * MS, 40 * MS);
    // smoothed = 80 − 10 + 84/8 = 80 − 10 + 10.5 → integer 80 − 10 + 10 = 80 (84/8 = 10 with truncation).
    assert_eq!(rtt.min_rtt(), 80 * MS, "the minimum holds the path floor");
    assert!(
      rtt.smoothed_rtt() >= 80 * MS,
      "the estimate was not dragged below the floor by a bogus delay"
    );
  }
}
