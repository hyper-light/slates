//! Credit-based flow control with absolute offsets per stream: the receiver grants credit as an
//! absolute byte offset the sender may reach; the sender never exceeds it; each class has its own
//! pool, so a stalled receiver stalls only its own class [A: Kung, Blackwell & Chapman,
//! SIGCOMM'94; B: RFC 9113 §5.2] (§4.9 "Flow control").
//!
//! The window is derived from the measured bandwidth-delay product and the class's latency
//! budget; the derivation lives here as `Derived` values.

use slates_machine::{Derived, derived};

use crate::error::WireError;

/// One stream's credit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Credit {
  /// The absolute offset the receiver has allowed the sender to reach.
  pub granted: u64,
  /// The absolute offset the sender has sent up to.
  pub sent: u64,
}

impl Credit {
  /// Bytes the sender may still send.
  pub const fn available(&self) -> u64 {
    self.granted.saturating_sub(self.sent)
  }

  /// The receiver grants more credit, as an absolute offset (a lower grant never shrinks it).
  pub fn grant(&mut self, up_to: u64) {
    self.granted = self.granted.max(up_to);
  }

  /// The sender reserves `bytes`; refused, unchanged, when the credit is short.
  pub fn reserve(&mut self, bytes: u64) -> Result<(), WireError> {
    let available = self.available();
    if bytes > available {
      return Err(WireError::CreditExceeded {
        wanted: bytes,
        available,
      });
    }
    self.sent += bytes;
    Ok(())
  }
}

/// The credit window for a class: the bandwidth-delay product, bounded below by one frame cap so a
/// single frame can always be in flight, and above by what the class's latency budget allows to
/// queue.
pub fn window_bytes(
  bandwidth_bytes_per_ns: u64,
  rtt_ns: u64,
  frame_cap: u64,
  budget_ns: u64,
) -> Derived<u64> {
  let bdp = bandwidth_bytes_per_ns.saturating_mul(rtt_ns);
  let by_budget = bandwidth_bytes_per_ns.saturating_mul(budget_ns);
  derived!(
    bdp.max(frame_cap).min(by_budget.max(frame_cap)),
    "clamp(bandwidth × rtt, one frame cap, bandwidth × class latency budget)",
    [
      "wire.bandwidth",
      "wire.rtt_p50",
      "wire.frame_cap",
      "wire.class_budget"
    ]
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_sender_never_exceeds_its_grant_and_grants_never_shrink() {
    let mut c = Credit::default();
    assert!(matches!(
      c.reserve(1),
      Err(WireError::CreditExceeded {
        wanted: 1,
        available: 0
      })
    ));
    c.grant(100);
    c.reserve(60).unwrap();
    assert_eq!(c.available(), 40);
    assert!(c.reserve(41).is_err());
    c.grant(50);
    assert_eq!(c.granted, 100);
    c.grant(200);
    c.reserve(140).unwrap();
    assert_eq!(c.available(), 0);
  }

  #[test]
  fn the_window_is_the_bandwidth_delay_product_within_its_bounds() {
    assert_eq!(window_bytes(10, 1_000, 4096, 1_000_000).get(), 10_000);
    assert_eq!(
      window_bytes(10, 10, 4096, 1_000_000).get(),
      4096,
      "one frame at least"
    );
    assert_eq!(
      window_bytes(10, 1_000_000, 4096, 1_000).get(),
      10_000,
      "the budget bounds it"
    );
  }
}
