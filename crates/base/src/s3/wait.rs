//! How long an S3 base waits, and how long it pauses before asking again (A-471; §4.15 "An S3 base"; R3), learned from
//! the path as A-442's learned wait is (`slates_server`'s `learned_wait`): the round trips the requests measured, from
//! the request's last byte sent to its response's head received, folded into RFC 9002's estimator
//! (`slates_transport::rtt::RttEstimator`, the one the failover estimate's terms use); a request's bound is their tail
//! (`smoothed + max(4·deviation, granularity)`, the probe timeout before any sample) plus the liveness window its
//! caller states, the lateness a live party may show (A-228). A request cut short at its bound is no sample (Karn's
//! rule) but doubles the bound for the next (RFC 6298 §5.5), never past the ceiling its caller states once backed off;
//! the next that completes ends the back-off.
//!
//! An answer that asks the client to wait (`503 SlowDown`, `503 ServiceUnavailable`, `500 InternalError`: "Try again",
//! S3's error responses) is no lost request: its round trip is a sample, and the pause before the retry is exponential
//! back-off with full jitter, as AWS advises for `SlowDown`: uniformly random up to the learned tail doubled for each
//! retry in a row, never past the time left before the request's ceiling.

use slates_machine::stats::Xorshift;
use slates_transport::rtt::RttEstimator;

/// Format: RFC 6298 §5.5's back-off: each timeout doubles the timer; the same doubling paces the retries' jitter.
const BACKOFF_FACTOR: u64 = 2;

/// The waits of one S3 path: its measured round trips, the requests that ran out since the last that completed, and
/// the retries in a row the server asked for.
#[derive(Debug)]
pub struct Waits {
  round_trips: RttEstimator,
  samples: u64,
  backoffs: u32,
  retries: u32,
  jitter: Xorshift,
}

impl Waits {
  /// A path nothing has been measured on, its jitter seeded by `seed` (the caller's: a test's fixed one, or the clock's
  /// low bits; the jitter spreads clients apart and needs no secrecy).
  pub fn new(seed: u64) -> Waits {
    Waits {
      round_trips: RttEstimator::new(),
      samples: 0,
      backoffs: 0,
      retries: 0,
      jitter: Xorshift::new(seed),
    }
  }

  /// The round trips measured.
  pub fn samples(&self) -> u64 {
    self.samples
  }

  /// The tail of the measured round trips: RFC 9002's probe timeout over them, its initial value (twice the initial
  /// RTT) before any.
  fn tail_ns(&self) -> u64 {
    self.round_trips.pto(0)
  }

  /// Derived: the bound the next request's answer is waited for: the measured tail plus `liveness_ns`, doubled for
  /// each request that ran out since the last completed one, and never past `ceiling_ns` once backed off. The
  /// unbacked bound stands whatever the ceiling.
  pub fn bound_ns(&self, liveness_ns: u64, ceiling_ns: u64) -> u64 {
    let paced = self.tail_ns().saturating_add(liveness_ns);
    if self.backoffs == 0 {
      return paced;
    }
    paced
      .saturating_mul(BACKOFF_FACTOR.saturating_pow(self.backoffs))
      .min(ceiling_ns.max(paced))
  }

  /// A request answered `round_trip_ns` after it was sent: a sample, and the end of any back-off. An answer that is
  /// not a retry's end leaves the retries in a row as they are; [`Waits::settled`] ends them.
  pub fn answered(&mut self, round_trip_ns: u64) {
    self.round_trips.on_sample(round_trip_ns, 0);
    self.samples = self.samples.saturating_add(1);
    self.backoffs = 0;
  }

  /// A request that ran out at its bound: no sample (Karn's rule), and the bound backed off.
  pub fn ran_out(&mut self) {
    self.backoffs = self.backoffs.saturating_add(1);
  }

  /// The pause before retrying a request the server asked to be retried, with `left_ns` until the request's ceiling:
  /// uniformly random up to the measured tail doubled for each retry in a row (full jitter), never past `left_ns`.
  pub fn retry_pause_ns(&mut self, left_ns: u64) -> u64 {
    let span = self
      .tail_ns()
      .saturating_mul(BACKOFF_FACTOR.saturating_pow(self.retries))
      .min(left_ns);
    self.retries = self.retries.saturating_add(1);
    match span.checked_add(1) {
      Some(bound) => self.jitter.next_u64().checked_rem(bound).unwrap_or(0),
      None => span,
    }
  }

  /// A request that ended as asked (answered, and not with a retry): the retries in a row end.
  pub fn settled(&mut self) {
    self.retries = 0;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a liveness window as a test's daemon states one.
  const LIVENESS_NS: u64 = 50_000_000;
  /// Shape: the ceiling a test states: four windows.
  const CEILING_NS: u64 = 4 * LIVENESS_NS;
  /// Shape: a round trip as a quick store answers one, 2 ms.
  const QUICK_NS: u64 = 2_000_000;

  /// A-471: do ask the bound with nothing measured, then after quick round trips. Expect RFC 9002's initial probe
  /// timeout plus the window at first, then a bound near the window once quick trips were measured: the path's pace,
  /// learned.
  #[test]
  fn a_bound_follows_the_round_trips_measured() {
    let mut waits = Waits::new(1);
    let first = waits.bound_ns(LIVENESS_NS, CEILING_NS);
    assert_eq!(first, slates_transport::rtt::INITIAL_PTO_NS + LIVENESS_NS);
    for _ in 0..8 {
      waits.answered(QUICK_NS);
    }
    let learned = waits.bound_ns(LIVENESS_NS, CEILING_NS);
    assert!(
      learned > LIVENESS_NS + QUICK_NS && learned < first,
      "the bound came down to the path's pace: {learned} ns"
    );
  }

  /// A-471 (RFC 6298 §5.5, Karn's rule): do let requests run out in a row, then answer one. Expect each to double the
  /// bound, the back-off never past the ceiling nor below the learned bound, and the answer to end it.
  #[test]
  fn a_request_that_ran_out_backs_off_to_the_ceiling_and_an_answer_ends_it() {
    let mut waits = Waits::new(1);
    for _ in 0..8 {
      waits.answered(QUICK_NS);
    }
    let paced = waits.bound_ns(LIVENESS_NS, CEILING_NS);
    waits.ran_out();
    assert_eq!(waits.bound_ns(LIVENESS_NS, CEILING_NS), 2 * paced);
    for _ in 0..4 {
      waits.ran_out();
    }
    assert_eq!(waits.bound_ns(LIVENESS_NS, CEILING_NS), CEILING_NS);
    assert_eq!(waits.bound_ns(LIVENESS_NS, paced / 2), paced);
    waits.answered(QUICK_NS);
    assert!(waits.bound_ns(LIVENESS_NS, CEILING_NS) < 2 * paced);
  }

  /// A-471 (exponential back-off with full jitter): do draw the pauses of eight retries in a row, then settle and draw
  /// again, and draw with little time left. Expect each pause within its doubled span, the spans growing, the pauses
  /// not all equal, a settled request starting over, and no pause past the time left.
  #[test]
  fn retry_pauses_are_jittered_within_a_doubling_span_and_the_time_left() {
    let mut waits = Waits::new(7);
    for _ in 0..8 {
      waits.answered(QUICK_NS);
    }
    let tail = waits.tail_ns();
    let mut pauses = Vec::new();
    for retry in 0..8u32 {
      let pause = waits.retry_pause_ns(u64::MAX);
      assert!(pause <= tail * 2u64.pow(retry), "retry {retry}: {pause} ns");
      pauses.push(pause);
    }
    assert!(pauses.windows(2).any(|pair| pair[0] != pair[1]));
    waits.settled();
    assert!(waits.retry_pause_ns(u64::MAX) <= tail);
    assert!(waits.retry_pause_ns(5) <= 5);
  }
}
