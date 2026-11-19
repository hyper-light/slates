//! What a harness's measurement of one request owes the server, and what it owes the harness itself (A-559; §4.7
//! "Protocol", the producer's stamp in each slot).
//!
//! A harness that sends a request and later takes its reply measures `taken − sent`. Two producers' stamps place the
//! server's part inside that: the client stamps the request as it lands in the command ring, and the daemon stamps the
//! reply as it lands in the client's completion ring (`slates_ipc::slot`, the slot's stamp). Everything before the
//! request landed and after the reply landed is the harness's: it held the request before handing it over, or left the
//! reply in the ring until its sweep came to it. [`harness_part`] is that time, exact when both stamps are there and in
//! order. A measurement whose stamps are missing or out of order has no harness part, so the whole of it is the
//! server's: a server's time is never hidden in the harness's.
//!
//! **The verdict ([`judge`]).** A quantile of a latency is judged against a bound by its server part. One whose server
//! part is at or past the bound missed it, whatever the harness did; one whose latency is past the bound and whose
//! server part is within it is harness-bound, the harness's and never charged to the server; and one whose latency is
//! within the bound met it. "Within" is strictly below, as the 1,000-client bound states it (A-297: a p99 of exactly
//! 1 ms is not under 1 ms).
//!
//! **Why (measured).** Main's reading of r100's harness lines (2026-11-19, the Mac Studio, 1,000 clients): the rounds
//! with a bad p99 were the rounds where the harness itself stalled. Accepted submits' p99 was 29.2 and 22.9 ms in the
//! two rounds whose longest sweep was 33.8 and 37.5 ms and whose longest wait between a reply's arrival and its taking
//! could be 44.0 and 37.6 ms, and 2.7 and 0.42 ms in rounds whose sweeps and waits stayed within 11.0 and 5.1 ms. A
//! latency measured from the harness's own stamps alone charged its stalls to the server.

/// The instants a harness has of one answered request, on the host's monotonic clock (`crate::clock::monotonic_ns`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Exchange {
  /// Read before the request was handed to its ring.
  pub sent_ns: u64,
  /// When the request landed in its ring, by the stamp its producer wrote there; `None` unstamped.
  pub request_landed_ns: Option<u64>,
  /// When the reply landed in the client's ring, by the stamp the daemon wrote there; `None` unstamped.
  pub reply_landed_ns: Option<u64>,
  /// Read once the look that took the reply had ended.
  pub taken_ns: u64,
}

/// The harness's part of `exchange` (module docs): from its sending to its request's landing, and from its reply's
/// landing to its taking. `None` when a stamp is missing or the instants are not in the order the exchange makes them
/// (sent, request landed, reply landed, taken), so nothing is taken from the server's part.
pub fn harness_part(exchange: &Exchange) -> Option<u64> {
  let request = exchange.request_landed_ns?;
  let reply = exchange.reply_landed_ns?;
  if reply < request {
    return None;
  }
  let before = request.checked_sub(exchange.sent_ns)?;
  let after = exchange.taken_ns.checked_sub(reply)?;
  before.checked_add(after)
}

/// What a quantile judged against a bound came to (module docs, "The verdict").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
  /// The latency is within the bound.
  Met,
  /// The latency is past the bound and its server part within it: the harness's part took it past, and the server is
  /// not charged with it.
  HarnessBound,
  /// The server part is at or past the bound.
  Missed,
}

/// Judges one quantile against `bound_ns` (module docs, "The verdict"): `latency_ns`, the quantile of the latencies,
/// and `server_ns`, the same quantile of their server parts.
pub fn judge(latency_ns: u64, server_ns: u64, bound_ns: u64) -> Verdict {
  if server_ns >= bound_ns {
    Verdict::Missed
  } else if latency_ns >= bound_ns {
    Verdict::HarnessBound
  } else {
    Verdict::Met
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a millisecond, the 1,000-client bound (A-297).
  const BOUND_NS: u64 = 1_000_000;

  /// An exchange sent at 0 whose request landed at `request`, whose reply landed at `reply` and was taken at `taken`.
  fn exchange(request: Option<u64>, reply: Option<u64>, taken: u64) -> Exchange {
    Exchange {
      sent_ns: 0,
      request_landed_ns: request,
      reply_landed_ns: reply,
      taken_ns: taken,
    }
  }

  /// A-559. Do: a request handed over 5 µs after its sending, answered 300 µs after it landed, and left 29 ms in the
  /// client's ring by a stalled sweep, as r100's first round at a 20 ms think could have it. Expect: the harness's part
  /// is the 5 µs and the 29 ms, and the server's, the rest of the latency, the 300 µs.
  #[test]
  fn a_reply_left_in_the_ring_by_a_stalled_sweep_is_the_harnesss_time() {
    let taken = 5_000 + 300_000 + 29_000_000;
    let stalled = exchange(Some(5_000), Some(305_000), taken);
    let harness = harness_part(&stalled);
    assert_eq!(harness, Some(29_005_000), "the harness held it 29.005 ms");
    assert_eq!(
      harness.map(|harness| taken - harness),
      Some(300_000),
      "the server took 300 µs"
    );
  }

  /// A-559. Do: exchanges with a stamp missing, and with stamps out of the order an exchange makes them (a reply
  /// landed before its request, a request before its sending, a reply after its taking). Expect: no harness part, so
  /// the whole latency stays the server's.
  #[test]
  fn an_unstamped_or_disordered_exchange_has_no_harness_part() {
    for (why, unknown) in [
      ("no request stamp", exchange(None, Some(10), 20)),
      ("no reply stamp", exchange(Some(10), None, 20)),
      ("a reply before its request", exchange(Some(10), Some(5), 20)),
      ("a reply after its taking", exchange(Some(10), Some(30), 20)),
    ] {
      assert_eq!(harness_part(&unknown), None, "{why}");
    }
    let before_its_sending = Exchange {
      sent_ns: 10,
      ..exchange(Some(5), Some(15), 20)
    };
    assert_eq!(harness_part(&before_its_sending), None, "a request before its sending");
    assert_eq!(
      harness_part(&exchange(Some(0), Some(20), 20)),
      Some(0),
      "landed at its sending and taken as it landed: none of it the harness's"
    );
  }

  /// A-559. Do: judge quantiles against the 1 ms bound: a server part past it, alone and with the latency past it too; a
  /// latency past it with the server part within it; both within it; each exactly at it. Expect: missed whenever the
  /// server part is at or past the bound, harness-bound when only the latency is, met when the latency is within it.
  #[test]
  fn a_quantile_is_charged_to_the_server_only_for_its_server_part() {
    assert_eq!(judge(29_200_000, 1_500_000, BOUND_NS), Verdict::Missed);
    assert_eq!(judge(BOUND_NS, BOUND_NS, BOUND_NS), Verdict::Missed);
    assert_eq!(judge(29_200_000, 400_000, BOUND_NS), Verdict::HarnessBound);
    assert_eq!(judge(BOUND_NS, 999_999, BOUND_NS), Verdict::HarnessBound);
    assert_eq!(judge(420_000, 400_000, BOUND_NS), Verdict::Met);
  }
}
