//! Congestion control for the session-plane connection (§4.10a §8; the constrained-link design,
//! `docs/wip/research/nfs-transport-constrained-links.md` §5.3): how much a sender keeps in flight and
//! how fast it paces, from the acknowledgements and losses it observes.
//!
//! **Bake-off in progress (2026-09-27).** Three control laws are built to their specifications and
//! compared end to end over the simulated constrained network (`crates/rt/src/sim.rs`) — NewReno
//! (RFC 9002 §7), CUBIC (RFC 9438 with HyStart++, RFC 9406) and BBR (draft-ietf-ccwg-bbr-06). The winner
//! becomes the connection's only controller and the others are deleted with [`ControllerKind`] (Ada,
//! 2026-09-27: "select the winner and remove the loser … do NOT keep legacy code as a fallback").
//!
//! Every law is sans-io: it reads the caller's clock from the event, owns no timer, and exposes three
//! outputs — the congestion window ([`Controller::window`]), the pacing rate ([`Controller::pacing_rate`])
//! and the send quantum ([`Controller::send_quantum`]) the pacer bursts at. The connection keeps the
//! bytes in flight and the delivery-rate sampler (`crate::delivery`) and hands each law what it needs.

pub mod bbr;
pub mod copa;
pub mod cubic;
pub mod filter;
pub mod newreno;

use crate::conn::SentPacket;
use crate::delivery::RateSample;
use crate::rtt::RttEstimator;

/// Format: RFC 9002 §7.2 `kInitialWindow` — ten maximum-size datagrams. A protocol constant; every law
/// starts from it (BBR takes "the transport's initial window", draft §5.6.4.1).
pub const INITIAL_WINDOW_DATAGRAMS: u64 = 10;
/// Format: RFC 9002 §7.2 `kMinimumWindow` — two maximum-size datagrams, the floor a loss-based law's
/// window never goes below, so a connection always makes progress. A protocol constant.
pub const MINIMUM_WINDOW_DATAGRAMS: u64 = 2;
/// Format: RFC 9002 §7.7 — a loss-based sender paces at `N × cwnd / smoothed_rtt`, with `N` "small, but
/// at least 1 (for example, 1.25)"; the RFC's example value, held as a per-mille ratio so the arithmetic
/// stays in integers.
pub const LOSS_BASED_PACING_PERMILLE: u64 = 1250;
/// Format: parts per thousand, the unit every gain below is stated in.
pub const PERMILLE: u64 = 1000;

/// One acknowledgement, as a control law sees it.
#[derive(Debug)]
pub struct AckEvent<'a> {
  /// The caller's clock when the acknowledgement was processed, nanoseconds.
  pub now: u64,
  /// The packets it newly acknowledged, in packet-number order.
  pub acked: &'a [SentPacket],
  /// The delivery-rate sample it produced.
  pub sample: RateSample,
  /// The bytes still in flight after it.
  pub in_flight: u64,
  /// The connection's delivered bytes over its life, after this acknowledgement (`C.delivered`).
  pub delivered: u64,
  /// Whether the sender filled its window at some point since sending the newest packet acknowledged —
  /// `C.is_cwnd_limited` (draft-ietf-ccwg-bbr §2.2); a law must not grow a window it is not using
  /// (RFC 9002 §7.8).
  pub cwnd_limited: bool,
  /// The RTT estimator, updated with this acknowledgement's sample.
  pub rtt: &'a RttEstimator,
}

/// One loss-detection pass that declared packets lost, as a control law sees it.
#[derive(Debug)]
pub struct LossEvent<'a> {
  /// The caller's clock, nanoseconds.
  pub now: u64,
  /// The lost packets, in packet-number order.
  pub lost: &'a [SentPacket],
  /// The largest packet number sent so far (the recovery period's watermark).
  pub largest_sent: u64,
  /// The bytes still in flight after the lost ones left.
  pub in_flight: u64,
  /// The connection's lost bytes over its life, after these (`C.lost`).
  pub lost_total: u64,
  /// Whether these losses establish persistent congestion (RFC 9002 §7.6).
  pub persistent: bool,
  /// The smoothed RTT (a law that acts at most once per round trip on loss reads it).
  pub srtt: u64,
}

/// The control laws in the bake-off. Deleted, with the losers, once one is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControllerKind {
  /// RFC 9002 §7 NewReno, paced.
  NewReno,
  /// RFC 9438 CUBIC with HyStart++, paced.
  Cubic,
  /// draft-ietf-ccwg-bbr-06 (BBRv3).
  Bbr,
  /// Copa (NSDI 2018) with the paper's default δ = 0.5.
  Copa,
  /// Copa with Meta's live-video δ = 0.04 (mvfst production setting).
  CopaMeta,
}

/// Format: Copa §4.3 — the recommended default δ = 1/2, as the integer `1/δ`.
const COPA_INV_DELTA: u64 = 2;
/// Format: Meta's live-video δ = 0.04 (engineering.fb.com, 2019-11-17), as the integer `1/δ`.
const COPA_META_INV_DELTA: u64 = 25;

/// A connection's congestion controller.
#[derive(Debug)]
pub enum Controller {
  /// NewReno.
  NewReno(newreno::NewReno),
  /// CUBIC.
  Cubic(cubic::Cubic),
  /// BBR (boxed: its model is several times the others' size).
  Bbr(Box<bbr::Bbr>),
  /// Copa, with its `1/δ` (distinguishing the two Copa kinds).
  Copa(copa::Copa, ControllerKind),
}

impl Controller {
  /// A fresh controller of `kind`, counting in `max_datagram`-byte datagrams, starting at `now`, drawing
  /// any randomized timing from `seed`.
  pub fn new(kind: ControllerKind, max_datagram: u64, now: u64, seed: u64) -> Controller {
    let max_datagram = max_datagram.max(1);
    match kind {
      ControllerKind::NewReno => Controller::NewReno(newreno::NewReno::new(max_datagram)),
      ControllerKind::Cubic => Controller::Cubic(cubic::Cubic::new(max_datagram)),
      ControllerKind::Bbr => Controller::Bbr(Box::new(bbr::Bbr::new(max_datagram, now, seed))),
      ControllerKind::Copa => Controller::Copa(copa::Copa::new(max_datagram, COPA_INV_DELTA), kind),
      ControllerKind::CopaMeta => {
        Controller::Copa(copa::Copa::new(max_datagram, COPA_META_INV_DELTA), kind)
      }
    }
  }

  /// Reseeds the law's randomized timing (BBR's probe wait, draft §5.3.3.8).
  pub fn reseed(&mut self, seed: u64) {
    if let Controller::Bbr(law) = self {
      law.reseed(seed);
    }
  }

  /// The kind this controller is.
  pub fn kind(&self) -> ControllerKind {
    match self {
      Controller::NewReno(_) => ControllerKind::NewReno,
      Controller::Cubic(_) => ControllerKind::Cubic,
      Controller::Bbr(_) => ControllerKind::Bbr,
      Controller::Copa(_, kind) => *kind,
    }
  }

  /// Packet `pn` is about to leave at `now`, with `in_flight` bytes in flight before it and the sender
  /// application-limited or not.
  pub fn on_sent(&mut self, now: u64, pn: u64, in_flight: u64, app_limited: bool) {
    match self {
      Controller::NewReno(_) => {}
      Controller::Cubic(law) => law.on_sent(pn),
      Controller::Bbr(law) => law.on_transmit(now, in_flight, app_limited),
      Controller::Copa(..) => {}
    }
  }

  /// Whether the controller asked for the connection to be marked application-limited (BBR's ProbeRTT),
  /// clearing the request.
  pub fn take_app_limited(&mut self) -> bool {
    match self {
      Controller::Bbr(law) => law.take_app_limited(),
      _ => false,
    }
  }

  /// Processes an acknowledgement and the losses it revealed, in each law's specified order: RFC 9002
  /// §B.4 processes the acknowledged packets and then the lost ones (NewReno, CUBIC); BBR marks losses
  /// before its per-ACK model update (draft §5.2.4 `HandleLostPacket` runs as packets are marked lost,
  /// ahead of `UpdateOnACK`, as in Linux's `tcp_mark_lost` then `bbr_main`).
  pub fn on_ack_and_loss(&mut self, ack: &AckEvent<'_>, loss: Option<&LossEvent<'_>>) {
    match self {
      Controller::NewReno(law) => {
        law.on_ack(ack);
        if let Some(loss) = loss {
          law.on_loss(loss);
        }
      }
      Controller::Cubic(law) => {
        law.on_ack(ack);
        if let Some(loss) = loss {
          law.on_loss(loss);
        }
      }
      Controller::Bbr(law) => {
        if let Some(loss) = loss {
          law.on_loss(loss);
        }
        law.on_ack(ack);
      }
      Controller::Copa(law, _) => {
        law.on_ack(ack);
        if let Some(loss) = loss {
          law.on_loss(loss);
        }
      }
    }
  }

  /// Processes a loss-detection pass that declared packets lost.
  pub fn on_loss(&mut self, event: &LossEvent<'_>) {
    match self {
      Controller::NewReno(law) => law.on_loss(event),
      Controller::Cubic(law) => law.on_loss(event),
      Controller::Bbr(law) => law.on_loss(event),
      Controller::Copa(law, _) => law.on_loss(event),
    }
  }

  /// The congestion window: the most bytes the sender keeps in flight.
  pub fn window(&self) -> u64 {
    match self {
      Controller::NewReno(law) => law.window(),
      Controller::Cubic(law) => law.window(),
      Controller::Bbr(law) => law.window(),
      Controller::Copa(law, _) => law.window(),
    }
  }

  /// The pacing rate, bytes per second: how fast the pacer releases data.
  pub fn pacing_rate(&self, rtt: &RttEstimator) -> u64 {
    match self {
      Controller::NewReno(law) => loss_based_pacing_rate(law.window(), rtt),
      Controller::Cubic(law) => loss_based_pacing_rate(law.window(), rtt),
      Controller::Bbr(law) => law.pacing_rate(),
      Controller::Copa(law, _) => law.pacing_rate(rtt.smoothed_rtt_or_initial()),
    }
  }

  /// The send quantum, bytes: the largest burst the pacer releases at once.
  pub fn send_quantum(&self, rtt: &RttEstimator) -> u64 {
    match self {
      Controller::Bbr(law) => law.send_quantum(),
      other => send_quantum_for(other.pacing_rate(rtt), other.max_datagram()),
    }
  }

  fn max_datagram(&self) -> u64 {
    match self {
      Controller::NewReno(law) => law.max_datagram(),
      Controller::Cubic(law) => law.max_datagram(),
      Controller::Bbr(law) => law.max_datagram(),
      Controller::Copa(law, _) => law.max_datagram(),
    }
  }

  /// Whether the controller is in its first ramp (slow start, or BBR's Startup) — for the bake-off's
  /// reports.
  pub fn in_startup(&self) -> bool {
    match self {
      Controller::NewReno(law) => law.in_slow_start(),
      Controller::Cubic(law) => law.in_slow_start(),
      Controller::Bbr(law) => law.in_startup(),
      Controller::Copa(law, _) => law.in_slow_start(),
    }
  }
}

/// The pacing rate of a loss-based law (RFC 9002 §7.7): `N × cwnd / smoothed_rtt`, bytes per second. With
/// no RTT sample yet, the initial RTT stands in (the estimator's own starting value).
pub fn loss_based_pacing_rate(window: u64, rtt: &RttEstimator) -> u64 {
  let srtt = rtt.smoothed_rtt_or_initial().max(1);
  let scaled = u128::from(window)
    .saturating_mul(u128::from(LOSS_BASED_PACING_PERMILLE))
    .saturating_mul(u128::from(crate::delivery::NANOS_PER_SECOND))
    / (u128::from(PERMILLE) * u128::from(srtt));
  u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum is a millisecond of the pacing rate, bounded
/// below by two datagrams and above by 64 KiB; the same rule serves every law, so the bake-off compares
/// control laws, not burst policies.
const QUANTUM_INTERVAL_NS: u64 = 1_000_000;
/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum's ceiling, 64 KiB.
const QUANTUM_CEILING_BYTES: u64 = 64 * 1024;
/// Format: draft-ietf-ccwg-bbr-06 §5.6.3 — the send quantum's floor, two datagrams.
const QUANTUM_FLOOR_DATAGRAMS: u64 = 2;

/// The send quantum for `pacing_rate` (draft-ietf-ccwg-bbr-06 §5.6.3, `SetSendQuantum`).
pub fn send_quantum_for(pacing_rate: u64, max_datagram: u64) -> u64 {
  crate::delivery::volume(pacing_rate, QUANTUM_INTERVAL_NS)
    .min(QUANTUM_CEILING_BYTES)
    .max(QUANTUM_FLOOR_DATAGRAMS.saturating_mul(max_datagram))
}
