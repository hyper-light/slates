//! NewReno (RFC 9002 §7, B): slow start, congestion avoidance, one multiplicative decrease per recovery
//! period, and persistent congestion — one of the three control laws in the session plane's congestion
//! bake-off (see the module above). Sans-io: it reads the caller's clock from each event.

use super::{AckEvent, INITIAL_WINDOW_DATAGRAMS, LossEvent, MINIMUM_WINDOW_DATAGRAMS};

/// Format: RFC 9002 §7.3.1 `kLossReductionFactor` = 0.5, as the integer divisor it is (halve).
const LOSS_REDUCTION_DIVISOR: u64 = 2;

/// NewReno's state (RFC 9002 §B.2).
#[derive(Debug)]
pub struct NewReno {
  max_datagram: u64,
  /// `congestion_window`, bytes.
  window: u64,
  /// `ssthresh`, bytes; unbounded until the first congestion event.
  ssthresh: u64,
  /// `congestion_recovery_start_time`: acknowledgements of packets sent before it do not grow the window.
  recovery_start: Option<u64>,
  /// Bytes acknowledged in congestion avoidance not yet converted into window growth (RFC 9002 §7.3.3's
  /// "one maximum datagram per window acknowledged", kept exact rather than truncated per acknowledgement).
  avoidance_credit: u64,
}

impl NewReno {
  /// A controller starting in slow start at the initial window.
  pub fn new(max_datagram: u64) -> NewReno {
    NewReno {
      max_datagram,
      window: INITIAL_WINDOW_DATAGRAMS.saturating_mul(max_datagram),
      ssthresh: u64::MAX,
      recovery_start: None,
      avoidance_credit: 0,
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

  /// Whether the window is below the slow-start threshold.
  pub fn in_slow_start(&self) -> bool {
    self.window < self.ssthresh
  }

  fn minimum_window(&self) -> u64 {
    MINIMUM_WINDOW_DATAGRAMS.saturating_mul(self.max_datagram)
  }

  fn in_recovery(&self, sent_at: u64) -> bool {
    self.recovery_start.is_some_and(|start| sent_at <= start)
  }

  /// `OnPacketsAcked` (RFC 9002 §B.5): grows the window for each acknowledged packet sent outside the
  /// recovery period, while the sender is using its window (§7.8).
  pub fn on_ack(&mut self, event: &AckEvent<'_>) {
    if !event.cwnd_limited {
      return;
    }
    for packet in event.acked {
      if self.in_recovery(packet.sent_at) {
        continue;
      }
      if self.in_slow_start() {
        self.window = self.window.saturating_add(packet.bytes);
      } else {
        self.avoidance_credit = self.avoidance_credit.saturating_add(packet.bytes);
        if self.avoidance_credit >= self.window {
          self.avoidance_credit -= self.window;
          self.window = self.window.saturating_add(self.max_datagram);
        }
      }
    }
  }

  /// `OnPacketsLost` (RFC 9002 §B.8): a loss of a packet sent after the recovery period began starts a new
  /// congestion event (halve, once per period); persistent congestion collapses to the minimum window.
  pub fn on_loss(&mut self, event: &LossEvent<'_>) {
    let Some(newest) = event.lost.iter().map(|packet| packet.sent_at).max() else {
      return;
    };
    if !self.in_recovery(newest) {
      self.recovery_start = Some(event.now);
      self.ssthresh = (self.window / LOSS_REDUCTION_DIVISOR).max(self.minimum_window());
      self.window = self.ssthresh;
      self.avoidance_credit = 0;
    }
    if event.persistent {
      self.window = self.minimum_window();
      self.recovery_start = None;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::conn::SentPacket;
  use crate::delivery::{RateSample, RateSnapshot};
  use crate::rtt::RttEstimator;

  /// Shape: a datagram of a thousand bytes, so windows read as whole datagrams.
  const MD: u64 = 1000;

  fn packet(pn: u64, sent_at: u64) -> SentPacket {
    SentPacket {
      pn,
      sent_at,
      bytes: MD,
      rate: RateSnapshot::default(),
    }
  }

  fn ack(law: &mut NewReno, packets: &[SentPacket], now: u64, cwnd_limited: bool) {
    let rtt = RttEstimator::new();
    law.on_ack(&AckEvent {
      now,
      acked: packets,
      sample: RateSample::default(),
      in_flight: 0,
      delivered: 0,
      cwnd_limited,
      rtt: &rtt,
    });
  }

  fn lose(law: &mut NewReno, packets: &[SentPacket], now: u64, persistent: bool) {
    law.on_loss(&LossEvent {
      now,
      lost: packets,
      largest_sent: 100,
      in_flight: 0,
      lost_total: 0,
      persistent,
      srtt: 0,
    });
  }

  /// RFC 9002 §7.3.1: in slow start each acknowledged byte grows the window by a byte.
  #[test]
  fn slow_start_grows_by_the_bytes_acknowledged() {
    let mut law = NewReno::new(MD);
    ack(&mut law, &[packet(0, 0), packet(1, 0)], 10, true);
    assert_eq!(law.window(), 12 * MD);
  }

  /// RFC 9002 §7.8: a sender not using its window does not grow it.
  #[test]
  fn an_application_limited_sender_does_not_grow_its_window() {
    let mut law = NewReno::new(MD);
    ack(&mut law, &[packet(0, 0)], 10, false);
    assert_eq!(law.window(), 10 * MD);
  }

  /// RFC 9002 §7.3.2: a loss halves the window once per recovery period; packets sent before the period
  /// began neither reduce it again when lost nor grow it when acknowledged.
  #[test]
  fn one_decrease_per_recovery_period() {
    let mut law = NewReno::new(MD);
    lose(&mut law, &[packet(3, 30)], 100, false);
    assert_eq!(law.window(), 5 * MD);
    lose(&mut law, &[packet(4, 40)], 110, false);
    assert_eq!(law.window(), 5 * MD, "the same period");
    ack(&mut law, &[packet(5, 50)], 120, true);
    assert_eq!(law.window(), 5 * MD, "sent before recovery: no growth");
    lose(&mut law, &[packet(9, 150)], 200, false);
    assert_eq!(
      law.window(),
      2500,
      "a packet sent after recovery began starts a new event"
    );
  }

  /// RFC 9002 §7.3.3: congestion avoidance grows one datagram per window of acknowledged data.
  #[test]
  fn congestion_avoidance_adds_a_datagram_per_window() {
    let mut law = NewReno::new(MD);
    lose(&mut law, &[packet(0, 0)], 1, false); // window 5 MD, in avoidance
    let packets: Vec<SentPacket> = (1..=5).map(|pn| packet(pn, 10 + pn)).collect();
    ack(&mut law, &packets, 100, true);
    assert_eq!(law.window(), 6 * MD);
  }

  /// RFC 9002 §7.6.2: persistent congestion collapses the window to the minimum.
  #[test]
  fn persistent_congestion_collapses_to_the_minimum() {
    let mut law = NewReno::new(MD);
    lose(&mut law, &[packet(0, 0), packet(1, 10)], 1000, true);
    assert_eq!(law.window(), 2 * MD);
  }
}
