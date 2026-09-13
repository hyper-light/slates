//! The attachment's credits (§4.6 A-9: "In-flight requests, mapped bytes, copy buffers and replies
//! consume the attachment's credits"; §4.9 "Flow control": credit-based, windows derived from the
//! measured bandwidth-delay product and the class's latency budget). Two credits per attachment:
//! the **requests** that may be in flight — taken from a ring and not yet returned — and the
//! **copy bytes** those requests may hold, the gathered request plus the room reserved for its
//! reply (mapped bytes are zero: DAX is not offered). A chain is charged after the ring walk has
//! validated it and before any of its buffers is touched, and released once its used element is
//! published; the device's service pass is bounded by the request credit (§4.3 "bounded work
//! everywhere").
//!
//! The derivations ([`AttachmentCredits::derive`]): the request credit is the owning shard's
//! admission limit — Little's law on the measured request rate and p99 service time,
//! `slates_rt::runtime::admission_limit`, the daemon's `requests_in_flight_per_shard` — since one
//! attachment may use its shard's whole in-flight budget and the daemon splits that budget among
//! the attachments it admits; the byte credit is the §4.9 credit window,
//! `slates_wire::credit::window_bytes` (bandwidth × round trip, clamped below by one frame and
//! above by what the class latency budget lets queue), with one request's worst case — the
//! readable cap plus the writable cap — as the frame.
//!
//! In this device every request completes before the next is taken, so a charge is never refused
//! for want of credit held by another request: a refusal means one chain alone exceeds the
//! attachment's credit, a hostile or misconfigured driver, and it faults the device rather than
//! skipping the chain. Backpressure across concurrently in-flight requests would belong to an
//! asynchronous completion path, which this device does not have; it is not modelled.

use std::fmt;

use slates_machine::{Derived, derived};
use slates_wire::credit::window_bytes;

use crate::virtqueue::DescriptorChain;

/// Which credit a refusal is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreditKind {
  /// Requests in flight.
  Requests,
  /// Copy bytes in flight.
  Bytes,
}

/// A typed credit refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CreditError {
  /// The charge exceeds what the attachment has left.
  Exhausted {
    /// Which credit.
    kind: CreditKind,
    /// The charge asked for.
    wanted: u64,
    /// The credit available.
    available: u64,
  },
}

impl fmt::Display for CreditError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Exhausted {
        kind,
        wanted,
        available,
      } => write!(
        f,
        "{kind:?} credit exhausted: wanted {wanted}, {available} available"
      ),
    }
  }
}

impl std::error::Error for CreditError {}

/// An attachment's credits, each with its derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentCredits {
  /// Requests that may be in flight at once.
  pub requests: Derived<u32>,
  /// Copy bytes that may be in flight at once.
  pub bytes: Derived<u64>,
}

impl AttachmentCredits {
  /// Derives an attachment's credits: the request credit from the owning shard's admission limit
  /// (`requests_in_flight_per_shard`, Little's law), the byte credit from the §4.9 credit window
  /// over `bandwidth_bytes_per_ns` and `rtt_ns` with `frame_cap` (one request's worst case) and the
  /// class `latency_budget_ns`.
  pub fn derive(
    requests_in_flight_per_shard: usize,
    bandwidth_bytes_per_ns: u64,
    rtt_ns: u64,
    frame_cap: u64,
    latency_budget_ns: u64,
  ) -> AttachmentCredits {
    AttachmentCredits {
      requests: derived!(
        u32::try_from(requests_in_flight_per_shard)
          .unwrap_or(u32::MAX)
          .max(1),
        "the owning shard's admission limit (Little's law: request rate × p99 service time); one attachment may use its shard's whole in-flight budget, which the daemon splits among attachments",
        ["rt.requests_in_flight_per_shard"]
      ),
      bytes: window_bytes(bandwidth_bytes_per_ns, rtt_ns, frame_cap, latency_budget_ns),
    }
  }
}

/// The ledger's counters: non-vacuity witnesses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LedgerCounters {
  /// Chains charged.
  pub charged: u64,
  /// Chains released.
  pub released: u64,
  /// Charges refused.
  pub refused: u64,
}

/// The attachment's credit ledger: what is in flight against each credit.
#[derive(Clone, Copy, Debug)]
pub struct CreditLedger {
  credits: AttachmentCredits,
  requests_in_flight: u32,
  bytes_in_flight: u64,
  counters: LedgerCounters,
}

impl CreditLedger {
  /// A ledger with nothing in flight.
  pub fn new(credits: AttachmentCredits) -> CreditLedger {
    CreditLedger {
      credits,
      requests_in_flight: 0,
      bytes_in_flight: 0,
      counters: LedgerCounters::default(),
    }
  }

  /// The credits.
  pub fn credits(&self) -> AttachmentCredits {
    self.credits
  }

  /// What is in flight: `(requests, bytes)`.
  pub fn in_flight(&self) -> (u32, u64) {
    (self.requests_in_flight, self.bytes_in_flight)
  }

  /// The counters.
  pub fn counters(&self) -> LedgerCounters {
    self.counters
  }

  /// Charges `requests` and `bytes`; refused, unchanged, when either credit is short.
  pub fn charge(&mut self, requests: u32, bytes: u64) -> Result<(), CreditError> {
    let requests_available = self
      .credits
      .requests
      .get()
      .saturating_sub(self.requests_in_flight);
    if requests > requests_available {
      self.counters.refused = self.counters.refused.saturating_add(1);
      return Err(CreditError::Exhausted {
        kind: CreditKind::Requests,
        wanted: u64::from(requests),
        available: u64::from(requests_available),
      });
    }
    let bytes_available = self
      .credits
      .bytes
      .get()
      .saturating_sub(self.bytes_in_flight);
    if bytes > bytes_available {
      self.counters.refused = self.counters.refused.saturating_add(1);
      return Err(CreditError::Exhausted {
        kind: CreditKind::Bytes,
        wanted: bytes,
        available: bytes_available,
      });
    }
    self.requests_in_flight = self.requests_in_flight.saturating_add(requests);
    self.bytes_in_flight = self.bytes_in_flight.saturating_add(bytes);
    self.counters.charged = self.counters.charged.saturating_add(1);
    Ok(())
  }

  /// Releases a charge once its request has completed.
  pub fn release(&mut self, requests: u32, bytes: u64) {
    self.requests_in_flight = self.requests_in_flight.saturating_sub(requests);
    self.bytes_in_flight = self.bytes_in_flight.saturating_sub(bytes);
    self.counters.released = self.counters.released.saturating_add(1);
  }

  /// The terminal step's reclaim: returns what was in flight and restores the credits whole.
  pub fn reclaim(&mut self) -> (u32, u64) {
    let in_flight = self.in_flight();
    self.requests_in_flight = 0;
    self.bytes_in_flight = 0;
    in_flight
  }
}

/// The copy bytes one chain holds: its gathered request and the room reserved for its reply.
fn chain_bytes(chain: &DescriptorChain) -> u64 {
  chain.readable_bytes.saturating_add(chain.writable_bytes)
}

/// What the device asks before it takes a validated chain and after it has answered it. The
/// ledger implements it; [`Unlimited`] is the unaccounted form for a device driven without an
/// attachment (the codec-level tests).
pub trait ChainAdmission {
  /// Charges the chain — one request, its copy bytes — before any of its buffers is touched. A
  /// refusal faults the device.
  fn admit(&mut self, chain: &DescriptorChain) -> Result<(), CreditError>;
  /// Releases the chain's charge once its used element is published (`written` bytes of reply).
  fn complete(&mut self, chain: &DescriptorChain, written: u32);
}

impl ChainAdmission for CreditLedger {
  fn admit(&mut self, chain: &DescriptorChain) -> Result<(), CreditError> {
    self.charge(1, chain_bytes(chain))
  }

  fn complete(&mut self, chain: &DescriptorChain, _written: u32) {
    self.release(1, chain_bytes(chain));
  }
}

/// No accounting: every chain is admitted.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unlimited;

impl ChainAdmission for Unlimited {
  fn admit(&mut self, _chain: &DescriptorChain) -> Result<(), CreditError> {
    Ok(())
  }

  fn complete(&mut self, _chain: &DescriptorChain, _written: u32) {}
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_charge_past_either_credit_is_refused_unchanged_and_reclaim_restores_the_whole_credit() {
    let credits = AttachmentCredits {
      requests: derived!(2, "test", ["test"]),
      bytes: derived!(100, "test", ["test"]),
    };
    let mut ledger = CreditLedger::new(credits);
    ledger.charge(1, 60).unwrap();
    assert_eq!(
      ledger.charge(1, 50).unwrap_err(),
      CreditError::Exhausted {
        kind: CreditKind::Bytes,
        wanted: 50,
        available: 40
      }
    );
    ledger.charge(1, 40).unwrap();
    assert_eq!(
      ledger.charge(1, 0).unwrap_err(),
      CreditError::Exhausted {
        kind: CreditKind::Requests,
        wanted: 1,
        available: 0
      }
    );
    assert_eq!(ledger.in_flight(), (2, 100));
    ledger.release(1, 60);
    assert_eq!(ledger.in_flight(), (1, 40));
    assert_eq!(ledger.reclaim(), (1, 40));
    assert_eq!(ledger.in_flight(), (0, 0));
    assert_eq!(ledger.counters().refused, 2);
  }

  #[test]
  fn the_derivation_takes_the_shards_admission_limit_and_the_credit_window() {
    let credits = AttachmentCredits::derive(20, 10, 1_000, 4096, 1_000_000);
    assert_eq!(credits.requests.get(), 20);
    assert_eq!(
      credits.bytes.get(),
      10_000,
      "bandwidth × rtt within the bounds"
    );
    assert!(credits.requests.formula.contains("Little"));
  }
}
