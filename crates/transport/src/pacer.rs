//! The pacer (RFC 9002 §7.7; draft-ietf-ccwg-bbr-06 §5.6.2): spaces a sender's ack-eliciting packets at
//! its congestion controller's pacing rate, so a window's worth of data is not dumped on the bottleneck
//! as one burst — the burst whose queue is the delay a small request waits behind on a thin link
//! (research note §5.3). A token bucket: tokens accrue at the pacing rate up to one send quantum (the
//! largest burst the controller allows), a packet spends its size in tokens, and a packet with too few
//! tokens waits until they accrue. Acknowledgement-only packets are not paced (RFC 9002 §7.7 exempts
//! them), so feedback never waits behind data.
//!
//! Sans-io: it reads the caller's clock and the controller's rate; the connection reports when the next
//! paced packet may leave, and the endpoint sleeps until then.

/// Format: nanoseconds per second, the unit conversion of a rate in bytes per second.
pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

/// The pacer's state.
#[derive(Debug, Default)]
pub struct Pacer {
  /// Tokens, bytes, as of `refilled_at`.
  tokens: u64,
  /// When the tokens were last brought up to date; `None` before the first packet (the bucket starts
  /// full).
  refilled_at: Option<u64>,
}

impl Pacer {
  /// A pacer whose bucket starts full.
  pub fn new() -> Pacer {
    Pacer::default()
  }

  /// Brings the tokens up to `now` at `rate` bytes per second, capped at `quantum`.
  fn refill(&mut self, now: u64, rate: u64, quantum: u64) {
    self.tokens = match self.refilled_at {
      None => quantum,
      Some(then) => self
        .tokens
        .saturating_add(volume(rate, now.saturating_sub(then)))
        .min(quantum),
    };
    self.refilled_at = Some(now);
  }

  /// When a packet of `bytes` may leave: `now` when the bucket holds enough, else the time the tokens
  /// reach `bytes` (or the quantum, for a packet larger than the quantum) at `rate`.
  pub fn release_time(&mut self, now: u64, bytes: u64, rate: u64, quantum: u64) -> u64 {
    self.refill(now, rate, quantum);
    let need = bytes.min(quantum);
    if self.tokens >= need {
      return now;
    }
    now.saturating_add(duration(need.saturating_sub(self.tokens), rate))
  }

  /// Spends `bytes` of tokens for a packet that left at `now`.
  pub fn on_sent(&mut self, now: u64, bytes: u64, rate: u64, quantum: u64) {
    self.refill(now, rate, quantum);
    self.tokens = self.tokens.saturating_sub(bytes);
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

  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;
  /// Shape: a 1 MB/s pacing rate, so a thousand-byte packet is a millisecond of tokens.
  const RATE: u64 = 1_000_000;
  /// Shape: a quantum of two thousand-byte packets.
  const QUANTUM: u64 = 2000;

  /// The bucket starts full, so the first quantum leaves at once; then one packet per millisecond at
  /// 1 MB/s — the spacing the rate implies.
  #[test]
  fn a_burst_of_one_quantum_then_the_pacing_rate() {
    let mut pacer = Pacer::new();
    let mut now = 0;
    let mut departures = Vec::new();
    for _ in 0..6 {
      now = pacer.release_time(now, 1000, RATE, QUANTUM);
      departures.push(now);
      pacer.on_sent(now, 1000, RATE, QUANTUM);
    }
    assert_eq!(departures, vec![0, 0, MS, 2 * MS, 3 * MS, 4 * MS]);
  }

  /// Idle time refills the bucket only up to one quantum, so a sender returning from idle bursts at most
  /// a quantum, never the whole idle period's worth.
  #[test]
  fn idle_refills_at_most_one_quantum() {
    let mut pacer = Pacer::new();
    pacer.on_sent(0, 2000, RATE, QUANTUM);
    let later = 1000 * MS;
    assert_eq!(pacer.release_time(later, 1000, RATE, QUANTUM), later);
    pacer.on_sent(later, 1000, RATE, QUANTUM);
    assert_eq!(pacer.release_time(later, 1000, RATE, QUANTUM), later);
    pacer.on_sent(later, 1000, RATE, QUANTUM);
    assert_eq!(
      pacer.release_time(later, 1000, RATE, QUANTUM),
      later + MS,
      "the third packet after idle waits"
    );
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
