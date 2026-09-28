//! Congestion control for the session-plane connection (§4.10a §8; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3): how much a sender keeps in flight and
//! how fast it paces, from the acknowledgements and losses it observes.
//!
//! **Copa** (Arun & Balakrishnan, NSDI 2018; [`copa`]), chosen by the session plane's congestion bake-off
//! (2026-09-28, `docs/wip/BENCHMARKS.md` "Session-plane congestion control and scheduling bake-offs"). Five
//! laws were built to their specifications and run end to end over the simulated constrained network — 57
//! scenarios, three seeds each, the selection rule fixed before any run. Copa was the only law that never
//! stalled and stayed RTT-fair, with the best goodput (a 1.068 shortfall geomean against NewReno's 15.8 and
//! CUBIC's 14.0) and a ping p99 within 3.05× of the best law in every scenario. NewReno (RFC 9002) and CUBIC
//! with HyStart++ (RFC 9438/9406) collapsed on lossy high-BDP paths (stalls at 100 Mbit/s with 1 % loss);
//! BBRv3 (draft-06) stalled at 100 Mbit/s, 300 ms, 5 %; Copa with Meta's δ = 0.04 failed RTT fairness
//! (Jain 0.840). The losers were deleted, not kept as a fallback (Ada, 2026-09-27).
//!
//! The law is sans-io: it reads the caller's clock from the event, owns no timer, and exposes three
//! outputs — the congestion window ([`Controller::window`]), the pacing rate ([`Controller::pacing_rate`])
//! and the send quantum ([`Controller::send_quantum`]) the pacer bursts at.

pub mod copa;
pub mod filter;

use crate::rtt::RttEstimator;

/// Format: RFC 9002 §7.2 `kInitialWindow` — ten maximum-size datagrams. A protocol constant; the law
/// starts from it.
pub const INITIAL_WINDOW_DATAGRAMS: u64 = 10;
/// Format: RFC 9002 §7.2 `kMinimumWindow` — two maximum-size datagrams, the floor the window never goes
/// below, so a connection always makes progress. A protocol constant.
pub const MINIMUM_WINDOW_DATAGRAMS: u64 = 2;
/// One acknowledgement, as the control law sees it.
#[derive(Debug)]
pub struct AckEvent<'a> {
  /// The caller's clock when the acknowledgement was processed, nanoseconds.
  pub now: u64,
  /// The acknowledgement's round-trip sample: from the send of the most recently sent packet it newly
  /// acknowledged to now (`None` when it acknowledged nothing new).
  pub rtt_sample: Option<u64>,
  /// The stream bytes it newly acknowledged.
  pub newly_acked: u64,
  /// Whether the sender filled its window at some point since sending the newest packet acknowledged; the
  /// law must not grow a window it is not using (RFC 9002 §7.8).
  pub cwnd_limited: bool,
  /// The RTT estimator, updated with this acknowledgement's sample.
  pub rtt: &'a RttEstimator,
}

/// One loss-detection pass that declared packets lost, as the control law sees it.
#[derive(Debug)]
pub struct LossEvent {
  /// The caller's clock, nanoseconds.
  pub now: u64,
  /// Whether these losses establish persistent congestion (RFC 9002 §7.6).
  pub persistent: bool,
  /// The smoothed RTT (the law acts at most once per round trip on loss).
  pub srtt: u64,
}

/// Format: Copa §4.3 — the recommended default δ = 1/2, as the integer `1/δ`.
const COPA_INV_DELTA: u64 = 2;

/// A connection's congestion controller: Copa (see the module doc for why).
#[derive(Debug)]
pub struct Controller {
  law: copa::Copa,
}

impl Controller {
  /// A fresh controller counting in `max_datagram`-byte datagrams.
  pub fn new(max_datagram: u64) -> Controller {
    Controller {
      law: copa::Copa::new(max_datagram.max(1), COPA_INV_DELTA),
    }
  }

  /// Processes an acknowledgement and the losses it revealed, in RFC 9002 §B.4's order: the acknowledged
  /// packets, then the lost ones.
  pub fn on_ack_and_loss(&mut self, ack: &AckEvent<'_>, loss: Option<&LossEvent>) {
    self.law.on_ack(ack);
    if let Some(loss) = loss {
      self.law.on_loss(loss);
    }
  }

  /// Processes a loss-detection pass that declared packets lost.
  pub fn on_loss(&mut self, event: &LossEvent) {
    self.law.on_loss(event);
  }

  /// The congestion window: the most bytes the sender keeps in flight.
  pub fn window(&self) -> u64 {
    self.law.window()
  }
  /// Follows a new datagram size (`crate::pmtud`; RFC 9002 §7.2).
  pub fn set_max_datagram(&mut self, max_datagram: u64) {
    self.law.set_max_datagram(max_datagram);
  }

  /// The pacing rate, bytes per second: how fast the pacer releases data.
  pub fn pacing_rate(&self, rtt: &RttEstimator) -> u64 {
    self.law.pacing_rate(rtt.smoothed_rtt_or_initial())
  }

  /// The send quantum, bytes: the largest burst the pacer releases at once.
  pub fn send_quantum(&self, rtt: &RttEstimator) -> u64 {
    send_quantum_for(self.pacing_rate(rtt), self.law.max_datagram())
  }

  /// Whether the law is in its first ramp (slow start).
  pub fn in_startup(&self) -> bool {
    self.law.in_slow_start()
  }

  /// The law itself (for reports and tests).
  pub fn copa(&self) -> &copa::Copa {
    &self.law
  }
}

/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum is a millisecond of the pacing rate, bounded
/// below by two datagrams and above by 64 KiB (the rule the bake-off ran every law under).
const QUANTUM_INTERVAL_NS: u64 = 1_000_000;
/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum's ceiling, 64 KiB.
const QUANTUM_CEILING_BYTES: u64 = 64 * 1024;
/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum's floor, two datagrams.
const QUANTUM_FLOOR_DATAGRAMS: u64 = 2;

/// The send quantum for `pacing_rate` (draft-ietf-ccwg-bbr-06 §5.6.3, `SetSendQuantum`).
pub fn send_quantum_for(pacing_rate: u64, max_datagram: u64) -> u64 {
  crate::pacer::volume(pacing_rate, QUANTUM_INTERVAL_NS)
    .min(QUANTUM_CEILING_BYTES)
    .max(QUANTUM_FLOOR_DATAGRAMS.saturating_mul(max_datagram))
}
