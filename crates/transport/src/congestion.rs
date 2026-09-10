//! NewReno congestion control for the session-plane connection (RFC 9002 §7, §7.3), the sender's
//! self-limit on how much data it keeps in flight so it shares a path fairly and backs off on loss.
//!
//! This is the sans-io control law only: it counts bytes in flight, grows the window on
//! acknowledgement (slow start then congestion avoidance) and reduces it once per loss event
//! (multiplicative decrease, guarded by a recovery period so a burst of losses in one round-trip
//! reduces the window once, not once per packet). It owns no clock and no socket; the connection
//! drives it from the same acknowledgement and loss signals it already computes, and gates fresh
//! sends on [`Congestion::can_send`].
//!
//! Deliberately *not* here: the empirical tuning — the initial window's exact value, CUBIC vs Reno,
//! pacing, an RTT-derived probe timeout, ECN. Those need a real network to measure and are the fleet's
//! Phase 8 measurement work (§4.10a); the control *law* is exact and unit-testable at N=1, which is
//! what this module is. The state machine is proven by injecting acknowledgements and losses and
//! asserting the window (no network), the way the reliability and flow-control cores are.

/// Format: the initial congestion window, in max-size datagrams (RFC 9002 §7.2, `kInitialWindow` — "10
/// times the max datagram size"). A protocol constant, not a tunable: it is the standard's starting
/// point, from which the control law takes over.
const INITIAL_WINDOW_DATAGRAMS: u64 = 10;
/// Format: the minimum congestion window, in max-size datagrams (RFC 9002 §7.2, `kMinimumWindow` — "2
/// times the max datagram size"). The window never shrinks below this, so a connection always makes
/// progress. A protocol constant.
const MINIMUM_WINDOW_DATAGRAMS: u64 = 2;
/// Format: the loss-reduction divisor (RFC 9002 §7.3.1, `kLossReductionFactor` = 0.5): a congestion
/// event halves the window. A protocol constant expressed as the integer divisor of the factor's
/// reciprocal (divide by two = multiply by one half), so the arithmetic stays in integers.
const LOSS_REDUCTION_DIVISOR: u64 = 2;

/// The sender's NewReno congestion controller: the window (`cwnd`), the slow-start threshold
/// (`ssthresh`), the bytes currently in flight, and the recovery-period watermark.
#[derive(Debug)]
pub struct Congestion {
  /// The max datagram size the window is counted in (this dialect sends one frame per packet, so it is
  /// the connection's frame cap).
  max_datagram: u64,
  /// The congestion window in bytes: the most in-flight data the sender allows itself.
  window: u64,
  /// The slow-start threshold in bytes: above it the window grows by congestion avoidance, below it by
  /// slow start. "Infinite" (`u64::MAX`) until the first loss, so the connection starts in slow start.
  ssthresh: u64,
  /// Bytes sent but not yet acknowledged, freed, or declared lost.
  in_flight: u64,
  /// The largest packet number sent when the current recovery period began; a loss of a packet at or
  /// below it does not reduce the window again (RFC 9002 §7.3.1 "a single congestion event"). `None`
  /// before the first loss.
  recovery_pn: Option<u64>,
}

impl Congestion {
  /// A controller whose window is counted in `max_datagram`-byte units, starting at the initial window
  /// in slow start with nothing in flight.
  pub fn new(max_datagram: u64) -> Congestion {
    let max_datagram = max_datagram.max(1);
    Congestion {
      max_datagram,
      window: INITIAL_WINDOW_DATAGRAMS.saturating_mul(max_datagram),
      ssthresh: u64::MAX,
      in_flight: 0,
      recovery_pn: None,
    }
  }

  /// Whether the sender may put a fresh ack-eliciting frame of `bytes` on the wire now: there is window
  /// room for it, or nothing is in flight at all (so a lone frame larger than the window still goes,
  /// rather than deadlocking — RFC 9002 §7.5). Retransmissions and acknowledgement-only packets are not
  /// gated by this; only fresh data is.
  pub fn can_send(&self, bytes: u64) -> bool {
    self.in_flight == 0 || self.in_flight.saturating_add(bytes) <= self.window
  }

  /// The current congestion window in bytes (for the connection's assertions and diagnostics).
  pub fn window(&self) -> u64 {
    self.window
  }

  /// The bytes currently in flight (for the connection's assertions and diagnostics).
  pub fn in_flight(&self) -> u64 {
    self.in_flight
  }

  /// Records that `bytes` of ack-eliciting data left on the wire.
  pub fn on_sent(&mut self, bytes: u64) {
    self.in_flight = self.in_flight.saturating_add(bytes);
  }

  /// Processes an acknowledgement of `bytes` of in-flight data: frees them and grows the window — by the
  /// acknowledged bytes in slow start (window below the threshold, RFC 9002 §7.3.1), else by about one
  /// max datagram per window of acknowledged data in congestion avoidance (RFC 9002 §7.3.2's additive
  /// increase, `max_datagram * acked / cwnd`).
  pub fn on_ack(&mut self, bytes: u64) {
    self.in_flight = self.in_flight.saturating_sub(bytes);
    if self.window < self.ssthresh {
      self.window = self.window.saturating_add(bytes);
    } else {
      self.window = self
        .window
        .saturating_add(self.max_datagram.saturating_mul(bytes) / self.window.max(1));
    }
  }

  /// Processes a loss of `bytes` of in-flight data whose newest packet number is `highest_lost_pn`,
  /// with `largest_sent_pn` the largest packet number sent so far. Always frees the lost bytes; and if
  /// the loss is newer than the current recovery period (or none is open), it enters a new congestion
  /// event — halving the threshold (down to the minimum window) and collapsing the window to it, then
  /// opening a recovery period through `largest_sent_pn` so later losses of already-sent packets in the
  /// same round do not reduce the window again (RFC 9002 §7.3.1).
  pub fn on_loss(&mut self, bytes: u64, highest_lost_pn: u64, largest_sent_pn: u64) {
    self.in_flight = self.in_flight.saturating_sub(bytes);
    let already_in_recovery = self.recovery_pn.is_some_and(|pn| highest_lost_pn <= pn);
    if already_in_recovery {
      return;
    }
    let minimum = MINIMUM_WINDOW_DATAGRAMS.saturating_mul(self.max_datagram);
    self.ssthresh = (self.window / LOSS_REDUCTION_DIVISOR).max(minimum);
    self.window = self.ssthresh;
    self.recovery_pn = Some(largest_sent_pn);
  }

  /// Frees `bytes` that a probe pulled out of flight to retransmit, without a window change: a tail-loss
  /// probe (RFC 9002 §7.6.1's PTO) is not a congestion signal, so it decrements the in-flight count (the
  /// retransmission re-adds it through [`Congestion::on_sent`]) but never reduces the window.
  pub fn on_probe_removed(&mut self, bytes: u64) {
    self.in_flight = self.in_flight.saturating_sub(bytes);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A max datagram of a round number so the arithmetic reads clearly.
  const MD: u64 = 1000;

  /// In slow start (window below the threshold) each acknowledgement grows the window by exactly the
  /// acknowledged bytes — the exponential ramp.
  #[test]
  fn slow_start_grows_the_window_by_the_acknowledged_bytes() {
    let mut cc = Congestion::new(MD);
    let start = cc.window();
    cc.on_sent(MD);
    cc.on_ack(MD);
    assert_eq!(cc.window(), start + MD, "slow start adds the acked bytes");
    assert_eq!(cc.in_flight(), 0, "the acked bytes left flight");
  }

  /// A loss halves the window to the slow-start threshold and never goes below the minimum window, and
  /// leaves the connection in congestion avoidance (window == ssthresh, so the next ack grows it slowly).
  #[test]
  fn a_loss_halves_the_window_to_the_threshold_and_floors_at_the_minimum() {
    let mut cc = Congestion::new(MD);
    let before = cc.window(); // 10 * MD
    cc.on_sent(3 * MD);
    cc.on_loss(MD, 0, 0);
    assert_eq!(cc.window(), before / 2, "the window halves");
    assert_eq!(
      cc.window(),
      cc.ssthresh_for_test(),
      "and equals the new threshold"
    );
    assert_eq!(cc.in_flight(), 2 * MD, "the lost bytes left flight");

    // Drive the window down to the minimum: repeated losses (each a fresh event) never go below 2*MD.
    for pn in 1..40u64 {
      cc.on_sent(MD);
      cc.on_loss(MD, pn, pn);
    }
    assert_eq!(
      cc.window(),
      MINIMUM_WINDOW_DATAGRAMS * MD,
      "floored at the minimum window"
    );
  }

  /// One congestion event reduces the window once, even if several packets are declared lost within the
  /// same recovery period (a loss whose packet number is not newer than the period's watermark does not
  /// reduce again) — the RFC 9002 §7.3.1 guard against collapsing on a single round's burst.
  #[test]
  fn a_burst_of_losses_in_one_recovery_period_reduces_the_window_once() {
    let mut cc = Congestion::new(MD);
    let before = cc.window();
    // Ten packets (pn 0..10) sent; the ack/loss round declares 0,1,2 lost. The first reduction opens a
    // recovery period through the largest sent pn (9); the later two are older, so they only free bytes.
    cc.on_sent(10 * MD);
    cc.on_loss(MD, 0, 9); // first loss of the event: reduces
    let after_first = cc.window();
    cc.on_loss(MD, 1, 9); // same period (1 <= 9): no further reduction
    cc.on_loss(MD, 2, 9); // same period
    assert_eq!(after_first, before / 2, "the first loss halved the window");
    assert_eq!(
      cc.window(),
      after_first,
      "later losses in the same period do not reduce again"
    );
    assert_eq!(cc.in_flight(), 7 * MD, "all three lost packets left flight");

    // A loss of a packet sent AFTER recovery started (pn 10 > 9) is a new event and reduces again.
    cc.on_sent(MD);
    cc.on_loss(MD, 10, 10);
    assert_eq!(
      cc.window(),
      (after_first / 2).max(MINIMUM_WINDOW_DATAGRAMS * MD),
      "a new event reduces"
    );
  }

  /// `can_send` gates fresh data at the window but never deadlocks: with nothing in flight even a frame
  /// larger than the window may go; once the window is full, a further frame is refused until an
  /// acknowledgement frees room.
  #[test]
  fn can_send_gates_at_the_window_but_never_deadlocks() {
    let mut cc = Congestion::new(MD);
    let window = cc.window();
    assert!(
      cc.can_send(window + MD),
      "a lone oversized frame sends when nothing is in flight"
    );
    cc.on_sent(window);
    assert!(!cc.can_send(MD), "no room once the window is full");
    cc.on_ack(MD);
    assert!(
      cc.can_send(MD),
      "an acknowledgement frees room to send again"
    );
  }

  /// A probe frees its bytes from flight without touching the window (a PTO is not a congestion signal).
  #[test]
  fn a_probe_frees_flight_without_reducing_the_window() {
    let mut cc = Congestion::new(MD);
    cc.on_sent(2 * MD);
    let window = cc.window();
    cc.on_probe_removed(MD);
    assert_eq!(cc.in_flight(), MD, "the probed bytes left flight");
    assert_eq!(cc.window(), window, "the window is unchanged by a probe");
  }

  impl Congestion {
    /// The slow-start threshold, for the tests to compare the window against after a loss.
    fn ssthresh_for_test(&self) -> u64 {
      self.ssthresh
    }
  }
}
