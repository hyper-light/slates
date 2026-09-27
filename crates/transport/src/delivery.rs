//! Delivery-rate sampling for the session-plane connection (§4.10a §8; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3): the per-connection and per-packet state
//! that turns each acknowledgement into a sample of how fast the path delivered data — the input a
//! model-based congestion controller builds its bandwidth estimate from, and the evidence the bake-off
//! compares controllers by. The algorithm is draft-ietf-ccwg-bbr-06 §4.1.2 [B], transcribed step for
//! step (`OnPacketSent`, `UpdateRateSample`, `GenerateRateSample`, `CheckIfApplicationLimited`): a sample
//! is the data delivered between a packet's send and its acknowledgement, over the longer of the send
//! interval and the acknowledgement interval, so neither ACK compression nor a sender burst inflates it,
//! and a sample taken while the sender had nothing to send is marked application-limited.
//!
//! Sans-io: every entry point takes the caller's clock reading. Rates are bytes per second, held in
//! integers (a sample is exact, comparable and replayable from a simulation seed).

/// Format: nanoseconds per second, the unit conversion of a rate in bytes per second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The delivery state a packet carries from its transmission (draft §4.1.2.1.2, `P.*`): what the
/// connection had delivered and lost when it was sent, the times that bound its sampling interval, and
/// whether the sender was application-limited then.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateSnapshot {
  /// `P.delivered`: the connection's delivered bytes when the packet was sent.
  pub delivered: u64,
  /// `P.delivered_time`: when those bytes had last advanced.
  pub delivered_time: u64,
  /// `P.first_send_time`: the send time of the packet most recently delivered when this one was sent.
  pub first_send_time: u64,
  /// `P.send_time`: when the packet was sent.
  pub send_time: u64,
  /// `P.is_app_limited`: the sender had run out of data when the packet was sent.
  pub is_app_limited: bool,
  /// `P.tx_in_flight`: the bytes in flight just after the packet was sent (including it).
  pub tx_in_flight: u64,
  /// `P.lost`: the connection's lost bytes when the packet was sent.
  pub lost: u64,
}

/// One acknowledgement's rate sample (draft §4.1.2.1.3 and §2.3, `RS.*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateSample {
  /// `RS.delivery_rate`: bytes per second over the sample's interval; zero when this acknowledgement gave
  /// no reliable sample.
  pub delivery_rate: u64,
  /// `RS.is_app_limited`: the newest acknowledged packet was sent while application-limited.
  pub is_app_limited: bool,
  /// `RS.interval`: the sampling interval, nanoseconds.
  pub interval: u64,
  /// `RS.delivered`: bytes delivered over the interval.
  pub delivered: u64,
  /// `RS.prior_delivered`: the newest acknowledged packet's `P.delivered`.
  pub prior_delivered: u64,
  /// `RS.newly_acked`: bytes this acknowledgement newly acknowledged.
  pub newly_acked: u64,
  /// `RS.newly_lost`: bytes newly declared lost while processing this acknowledgement.
  pub newly_lost: u64,
  /// `RS.tx_in_flight`: the newest acknowledged packet's `P.tx_in_flight`.
  pub tx_in_flight: u64,
  /// `RS.lost`: bytes lost between the newest acknowledged packet's send and now.
  pub lost: u64,
  /// `RS.rtt`: the round trip of the newest acknowledged packet, nanoseconds; `None` when nothing was
  /// acknowledged.
  pub rtt: Option<u64>,
}

/// The connection's delivery state (draft §4.1.2.1.1, `C.*`).
#[derive(Debug, Default)]
pub struct DeliveryRate {
  /// `C.delivered`: bytes delivered over the connection's life (never pure acknowledgements).
  delivered: u64,
  /// `C.delivered_time`: when `delivered` last advanced.
  delivered_time: u64,
  /// `C.first_send_time`: the send time of the packet most recently delivered (or, idle, of the most
  /// recently sent one).
  first_send_time: u64,
  /// `C.app_limited`: the delivered count at which the current application-limited phase ends, zero
  /// when the sender is not application-limited.
  app_limited: u64,
  /// `C.lost`: bytes declared lost over the connection's life.
  lost: u64,
  /// The sample under construction for the acknowledgement being processed.
  sample: SampleBuilder,
}

/// The per-acknowledgement working state of `UpdateRateSample` (draft §4.1.2.3).
#[derive(Clone, Copy, Debug, Default)]
struct SampleBuilder {
  has_data: bool,
  prior_delivered: u64,
  prior_time: u64,
  is_app_limited: bool,
  send_elapsed: u64,
  ack_elapsed: u64,
  newest_send_time: u64,
  newest_packet: u64,
  tx_in_flight: u64,
  lost_at_send: u64,
  newly_acked: u64,
  newly_lost: u64,
}

impl DeliveryRate {
  /// A connection that has delivered nothing.
  pub fn new() -> DeliveryRate {
    DeliveryRate::default()
  }

  /// Bytes delivered over the connection's life (`C.delivered`).
  pub fn delivered(&self) -> u64 {
    self.delivered
  }

  /// Bytes lost over the connection's life (`C.lost`).
  pub fn lost(&self) -> u64 {
    self.lost
  }

  /// Whether the connection is in an application-limited phase.
  pub fn is_app_limited(&self) -> bool {
    self.app_limited != 0
  }

  /// `OnPacketSent` (draft §4.1.2.2): the snapshot a packet of `bytes` sent at `now` carries, given the
  /// bytes in flight before it. Starts the sampling interval afresh when nothing was in flight.
  pub fn on_sent(&mut self, now: u64, in_flight_before: u64, bytes: u64) -> RateSnapshot {
    if in_flight_before == 0 {
      self.first_send_time = now;
      self.delivered_time = now;
    }
    RateSnapshot {
      delivered: self.delivered,
      delivered_time: self.delivered_time,
      first_send_time: self.first_send_time,
      send_time: now,
      is_app_limited: self.app_limited != 0,
      tx_in_flight: in_flight_before.saturating_add(bytes),
      lost: self.lost,
    }
  }

  /// Starts the sample for one acknowledgement (`InitRateSample`).
  pub fn begin_ack(&mut self) {
    self.sample = SampleBuilder::default();
  }

  /// `UpdateRateSample` (draft §4.1.2.3) for one newly acknowledged packet `packet` of `bytes` carrying
  /// `snapshot`, acknowledged at `now`.
  pub fn on_packet_acked(&mut self, now: u64, packet: u64, bytes: u64, snapshot: &RateSnapshot) {
    self.delivered = self.delivered.saturating_add(bytes);
    self.delivered_time = now;
    self.sample.newly_acked = self.sample.newly_acked.saturating_add(bytes);
    let newest = !self.sample.has_data
      || snapshot.send_time > self.sample.newest_send_time
      || (snapshot.send_time == self.sample.newest_send_time && packet > self.sample.newest_packet);
    if newest {
      self.sample.has_data = true;
      self.sample.prior_delivered = snapshot.delivered;
      self.sample.prior_time = snapshot.delivered_time;
      self.sample.is_app_limited = snapshot.is_app_limited;
      self.sample.send_elapsed = snapshot.send_time.saturating_sub(snapshot.first_send_time);
      self.sample.ack_elapsed = self.delivered_time.saturating_sub(snapshot.delivered_time);
      self.sample.newest_send_time = snapshot.send_time;
      self.sample.newest_packet = packet;
      self.sample.tx_in_flight = snapshot.tx_in_flight;
      self.sample.lost_at_send = snapshot.lost;
      self.first_send_time = snapshot.send_time;
    }
  }

  /// Records `bytes` newly declared lost (`C.lost`, and `RS.newly_lost` for the acknowledgement being
  /// processed).
  pub fn on_lost(&mut self, bytes: u64) {
    self.lost = self.lost.saturating_add(bytes);
    self.sample.newly_lost = self.sample.newly_lost.saturating_add(bytes);
  }

  /// `GenerateRateSample` (draft §4.1.2.3): finishes the acknowledgement's sample at `now`. A sample
  /// whose interval is below `min_rtt` is unreliable and carries no rate (`delivery_rate` zero); the rest
  /// of the sample is still filled, since the controller's round counting and loss accounting need it.
  pub fn finish_ack(&mut self, now: u64, min_rtt: Option<u64>) -> RateSample {
    if self.app_limited != 0 && self.delivered > self.app_limited {
      self.app_limited = 0;
    }
    let builder = self.sample;
    let mut sample = RateSample {
      newly_acked: builder.newly_acked,
      newly_lost: builder.newly_lost,
      ..RateSample::default()
    };
    if !builder.has_data {
      return sample;
    }
    sample.is_app_limited = builder.is_app_limited;
    sample.prior_delivered = builder.prior_delivered;
    sample.tx_in_flight = builder.tx_in_flight;
    sample.lost = self.lost.saturating_sub(builder.lost_at_send);
    sample.rtt = Some(now.saturating_sub(builder.newest_send_time));
    sample.delivered = self.delivered.saturating_sub(builder.prior_delivered);
    sample.interval = builder.send_elapsed.max(builder.ack_elapsed);
    if sample.interval == 0 || min_rtt.is_some_and(|min| sample.interval < min) {
      return sample;
    }
    sample.delivery_rate = rate(sample.delivered, sample.interval);
    sample
  }

  /// `CheckIfApplicationLimited` then `MarkConnectionAppLimited` (draft §4.1.2.4): the sender has no
  /// fresh data to send, nothing queued below the transport, room in the window, and no lost data
  /// awaiting retransmission — so the samples taken from here on reflect the application, not the path.
  pub fn check_app_limited(&mut self, nothing_to_send: bool, in_flight: u64, window: u64) {
    if nothing_to_send && in_flight < window {
      self.app_limited = self.delivered.saturating_add(in_flight).max(1);
    }
  }

  /// `MarkConnectionAppLimited` unconditionally (draft §5.3.4.3: ProbeRTT ignores its own low samples).
  pub fn mark_app_limited(&mut self, in_flight: u64) {
    self.app_limited = self.delivered.saturating_add(in_flight).max(1);
  }
}

/// `bytes` over `interval_ns`, in bytes per second (saturating, never dividing by zero).
pub fn rate(bytes: u64, interval_ns: u64) -> u64 {
  if interval_ns == 0 {
    return 0;
  }
  let scaled =
    u128::from(bytes).saturating_mul(u128::from(NANOS_PER_SECOND)) / u128::from(interval_ns);
  u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The bytes a `rate` (bytes per second) carries in `interval_ns`.
pub fn volume(rate: u64, interval_ns: u64) -> u64 {
  let scaled =
    u128::from(rate).saturating_mul(u128::from(interval_ns)) / u128::from(NANOS_PER_SECOND);
  u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// The time, nanoseconds, `bytes` take at `rate` bytes per second (rounded up; `u64::MAX` at rate zero).
pub fn duration(bytes: u64, rate: u64) -> u64 {
  if rate == 0 {
    return u64::MAX;
  }
  let scaled = u128::from(bytes)
    .saturating_mul(u128::from(NANOS_PER_SECOND))
    .div_ceil(u128::from(rate));
  u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a millisecond in nanoseconds.
  const MS: u64 = 1_000_000;
  /// Shape: a packet of a thousand bytes.
  const PACKET: u64 = 1000;

  /// A sender pacing one packet per millisecond over a path that delivers them one per millisecond
  /// samples exactly that rate (a thousand bytes per millisecond, a million bytes per second) once the
  /// first round trip has passed — the steady state the sampler exists to measure.
  #[test]
  fn a_paced_flow_samples_its_own_rate() {
    let mut rate_state = DeliveryRate::new();
    let rtt = 20 * MS;
    let rtt_ms = rtt / MS;
    let mut snapshots = Vec::new();
    let mut in_flight = 0;
    let mut last = RateSample::default();
    // Each millisecond: the packet sent one RTT ago is acknowledged, then a new packet is sent — events
    // in the order a real path produces them.
    for ms in 0..100u64 {
      let now = ms * MS;
      if ms >= rtt_ms {
        let k = ms - rtt_ms;
        rate_state.begin_ack();
        rate_state.on_packet_acked(now, k, PACKET, &snapshots[usize::try_from(k).unwrap()]);
        in_flight -= PACKET;
        last = rate_state.finish_ack(now, Some(rtt));
      }
      snapshots.push(rate_state.on_sent(now, in_flight, PACKET));
      in_flight += PACKET;
    }
    assert_eq!(
      last.delivery_rate, 1_000_000,
      "a thousand bytes a millisecond is a million a second"
    );
    assert!(!last.is_app_limited);
    assert_eq!(last.rtt, Some(rtt));
  }

  /// ACK compression cannot inflate a sample: when a path holds acknowledgements and releases a burst of
  /// them at once, the interval is the longer send interval, so the rate stays the sending rate.
  #[test]
  fn compressed_acknowledgements_do_not_inflate_the_rate() {
    let mut rate_state = DeliveryRate::new();
    let mut snapshots = Vec::new();
    let mut in_flight = 0;
    for k in 0..10u64 {
      snapshots.push(rate_state.on_sent(k * MS, in_flight, PACKET));
      in_flight += PACKET;
    }
    // All ten acknowledged at one instant.
    let now = 50 * MS;
    rate_state.begin_ack();
    for (k, snapshot) in snapshots.iter().enumerate() {
      rate_state.on_packet_acked(now, k as u64, PACKET, snapshot);
    }
    let sample = rate_state.finish_ack(now, Some(MS));
    // The newest packet (sent at 9 ms, first-send 0) spans 9 ms of sending; the ack interval from the
    // start is 50 ms: the sample is 10 kB over 50 ms — never 10 kB over the zero-length ack burst.
    assert_eq!(sample.delivered, 10 * PACKET);
    assert_eq!(sample.interval, 50 * MS);
    assert_eq!(sample.delivery_rate, rate(10 * PACKET, 50 * MS));
  }

  /// A sample taken while the sender was application-limited says so, and the phase ends once the data in
  /// flight when it began has been delivered.
  #[test]
  fn an_application_limited_phase_marks_its_samples_and_ends() {
    let mut rate_state = DeliveryRate::new();
    let first = rate_state.on_sent(0, 0, PACKET);
    rate_state.check_app_limited(true, PACKET, 10 * PACKET);
    assert!(rate_state.is_app_limited());
    let second = rate_state.on_sent(MS, PACKET, PACKET);
    assert!(second.is_app_limited, "sent inside the phase");
    rate_state.begin_ack();
    rate_state.on_packet_acked(20 * MS, 0, PACKET, &first);
    rate_state.on_packet_acked(21 * MS, 1, PACKET, &second);
    let sample = rate_state.finish_ack(21 * MS, Some(MS));
    assert!(sample.is_app_limited, "the newest packet was app-limited");
    assert!(
      !rate_state.is_app_limited(),
      "the phase ended once its in-flight data was delivered"
    );
  }

  /// An interval shorter than the minimum RTT gives no rate (the sample would overestimate), but the rest
  /// of the sample — bytes acknowledged, the RTT — is still reported.
  #[test]
  fn an_interval_below_the_minimum_rtt_gives_no_rate() {
    let mut rate_state = DeliveryRate::new();
    let snapshot = rate_state.on_sent(0, 0, PACKET);
    rate_state.begin_ack();
    rate_state.on_packet_acked(MS, 0, PACKET, &snapshot);
    let sample = rate_state.finish_ack(MS, Some(10 * MS));
    assert_eq!(sample.delivery_rate, 0);
    assert_eq!(sample.newly_acked, PACKET);
    assert_eq!(sample.rtt, Some(MS));
  }

  /// The unit conversions agree with each other and with their definitions.
  #[test]
  fn rate_volume_and_duration_are_inverses() {
    assert_eq!(rate(1_000_000, NANOS_PER_SECOND), 1_000_000);
    assert_eq!(volume(1_000_000, 20 * MS), 20_000);
    assert_eq!(duration(20_000, 1_000_000), 20 * MS);
    assert_eq!(duration(1, 0), u64::MAX, "no rate, no finite time");
  }
}
