//! Adaptive reordering tolerance (§4.10a; RFC 9002 §6.1.1, which lets an implementation adapt its loss
//! thresholds, and RACK's reordering window, RFC 8985 §6.2). A path that reorders — jitter across parallel
//! links, a reordering middlebox — delivers packets out of order by more than RFC 9002's fixed thresholds (3
//! packets, 9/8 of the RTT) allow. Every such packet is declared lost and retransmitted, although it arrives:
//! at 10 Mbit/s and 20 ms with 8 ms of jitter a session retransmitted 27,016 frames with no real loss and
//! carried a quarter of the link (the session-plane congestion bench, 2026-09-28).
//!
//! The evidence is the spurious loss: a packet declared lost that is acknowledged afterwards. Each one shows
//! how far out of order it arrived — the largest acknowledged packet when it was declared, less its own number
//! — and the tolerance widens to cover it:
//! - the packet threshold rises to that distance plus one, never past the packets a window holds;
//! - the time threshold gains a reordering window, a quarter of the minimum RTT per round trip that saw a
//!   spurious loss (RACK's `reo_wnd_mult`), never past the smoothed RTT.
//!
//! Both return to the RFC's defaults once [`QUIET_RECOVERIES`] loss recoveries pass with no spurious loss (RACK's
//! `reo_wnd_persist`), so a path that stops reordering regains prompt loss detection. The memory of recent
//! losses is bounded by count and by age, so it never grows with a session's life.

use std::collections::BTreeMap;

use crate::conn::REORDER_THRESHOLD;

/// Format: RFC 8985 §6.2 — the loss recoveries without reordering after which the reordering window returns to
/// its default (`reo_wnd_persist`, 16).
pub const QUIET_RECOVERIES: u32 = 16;
/// Format: RFC 8985 §6.2 — the reordering window grows in steps of a quarter of the minimum RTT.
const WINDOW_STEP_DIVISOR: u64 = 4;
/// Derived: how long a declared loss is remembered — two smoothed round trips: a late acknowledgement of a
/// reordered packet arrives within the round trip after the declaration, and one more covers a delayed ACK.
pub const MEMORY_SRTTS: u64 = 2;

/// A loss this end declared: the largest packet acknowledged when it was declared, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Declared {
  largest_acked: u64,
  at: u64,
}

/// One connection's reordering tolerance.
#[derive(Clone, Debug, Default)]
pub struct Reordering {
  /// The packet threshold above the RFC's three, learnt from spurious losses (zero: the default).
  extra_packets: u64,
  /// Quarters of the minimum RTT added to the time threshold (RACK's `reo_wnd_mult`).
  window_quarters: u64,
  /// When the window last widened (it widens at most once per round trip).
  widened_at: Option<u64>,
  /// Loss recoveries since the last spurious loss.
  quiet: u32,
  /// Recently declared losses, by packet number.
  declared: BTreeMap<u64, Declared>,
  /// Spurious losses detected over the connection's life (the counter the tests read).
  spurious: u64,
}

impl Reordering {
  /// The packet threshold now: RFC 9002's three, raised by what spurious losses showed.
  pub fn packet_threshold(&self) -> u64 {
    REORDER_THRESHOLD.saturating_add(self.extra_packets)
  }

  /// What the time threshold gains now: the reordering window, `window_quarters · min_rtt/4`, never past
  /// `srtt`.
  pub fn extra_delay(&self, min_rtt: u64, srtt: u64) -> u64 {
    (min_rtt / WINDOW_STEP_DIVISOR)
      .saturating_mul(self.window_quarters)
      .min(srtt)
  }

  /// Whether any declared loss is still remembered — when none is, an acknowledgement cannot show one
  /// spurious, and the caller skips [`Reordering::on_acknowledged`].
  pub fn remembers_declared_losses(&self) -> bool {
    !self.declared.is_empty()
  }

  /// Spurious losses detected so far.
  pub fn spurious(&self) -> u64 {
    self.spurious
  }

  /// Records the packets `pns` declared lost at `now`, with `largest_acked` the largest acknowledged packet
  /// then, and counts one loss recovery; forgets declarations older than [`MEMORY_SRTTS`] round trips and
  /// keeps at most `cap` of them (the caller passes the packets [`MEMORY_SRTTS`] windows hold — no more can
  /// be declared within the memory).
  pub fn on_declared_lost(
    &mut self,
    pns: &[u64],
    largest_acked: u64,
    now: u64,
    srtt: u64,
    cap: usize,
  ) {
    if pns.is_empty() {
      return;
    }
    for &pn in pns {
      self.declared.insert(
        pn,
        Declared {
          largest_acked,
          at: now,
        },
      );
    }
    let horizon = now.saturating_sub(srtt.saturating_mul(MEMORY_SRTTS));
    self.declared.retain(|_, declared| declared.at >= horizon);
    while self.declared.len() > cap.max(1) {
      if self.declared.pop_first().is_none() {
        break;
      }
    }
    self.quiet = self.quiet.saturating_add(1);
    if self.quiet >= QUIET_RECOVERIES {
      self.extra_packets = 0;
      self.window_quarters = 0;
      self.quiet = 0;
    }
  }

  /// Folds the acknowledged runs `runs` (inclusive `(low, high)` packet-number ranges) received at `now`: every
  /// declared loss they cover was spurious. Returns those packet numbers, so the caller can treat their data
  /// as delivered. `max_packets` caps the packet threshold (the packets a window holds).
  pub fn on_acknowledged(
    &mut self,
    runs: &[(u64, u64)],
    now: u64,
    srtt: u64,
    max_packets: u64,
  ) -> Vec<u64> {
    let mut found = Vec::new();
    for &(low, high) in runs {
      let covered: Vec<(u64, Declared)> = self
        .declared
        .range(low..=high)
        .map(|(&pn, &declared)| (pn, declared))
        .collect();
      for (pn, declared) in covered {
        self.declared.remove(&pn);
        found.push(pn);
        let distance = declared.largest_acked.saturating_sub(pn);
        let needed = distance.saturating_add(1).saturating_sub(REORDER_THRESHOLD);
        let ceiling = max_packets.saturating_sub(REORDER_THRESHOLD);
        self.extra_packets = self.extra_packets.max(needed.min(ceiling));
      }
    }
    if !found.is_empty() {
      self.spurious = self.spurious.saturating_add(found.len() as u64);
      self.quiet = 0;
      let widen = self
        .widened_at
        .is_none_or(|at| now.saturating_sub(at) >= srtt);
      if widen {
        self.window_quarters = self.window_quarters.saturating_add(1);
        self.widened_at = Some(now);
      }
    }
    found
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a millisecond.
  const MS: u64 = 1_000_000;

  /// RFC 8985 §6.2 / RFC 9002 §6.1.1: a spurious loss widens the tolerance to cover what it showed — the packet
  /// threshold to its distance plus one, the time window by a quarter of the minimum RTT — and the window
  /// widens at most once per round trip.
  #[test]
  fn a_spurious_loss_widens_the_tolerance_to_cover_it() {
    let mut reordering = Reordering::default();
    assert_eq!(reordering.packet_threshold(), REORDER_THRESHOLD);
    assert_eq!(reordering.extra_delay(20 * MS, 20 * MS), 0);
    reordering.on_declared_lost(&[10], 18, 0, 20 * MS, 64);
    let spurious = reordering.on_acknowledged(&[(9, 12)], MS, 20 * MS, 64);
    assert_eq!(spurious, vec![10]);
    assert_eq!(
      reordering.packet_threshold(),
      9,
      "distance 8 needs a threshold of 9"
    );
    assert_eq!(reordering.extra_delay(20 * MS, 20 * MS), 5 * MS);
    reordering.on_declared_lost(&[30], 33, 2 * MS, 20 * MS, 64);
    reordering.on_acknowledged(&[(30, 30)], 3 * MS, 20 * MS, 64);
    assert_eq!(
      reordering.extra_delay(20 * MS, 20 * MS),
      5 * MS,
      "once per round trip"
    );
    assert_eq!(reordering.spurious(), 2);
  }

  /// The tolerance is bounded: the packet threshold never exceeds what a window holds, and the time window
  /// never exceeds the smoothed RTT.
  #[test]
  fn the_tolerance_is_bounded_by_the_window_and_the_rtt() {
    let mut reordering = Reordering::default();
    for round in 0..100u64 {
      let pn = round * 1_000;
      reordering.on_declared_lost(&[pn], pn + 900, round * 20 * MS, 20 * MS, 64);
      reordering.on_acknowledged(&[(pn, pn)], round * 20 * MS + MS, 20 * MS, 32);
    }
    assert_eq!(reordering.packet_threshold(), 32);
    assert_eq!(reordering.extra_delay(20 * MS, 20 * MS), 20 * MS);
  }

  /// RFC 8985 §6.2: after `QUIET_RECOVERIES` loss recoveries with no spurious loss the tolerance returns to the
  /// RFC defaults; and the memory of declarations stays bounded by the cap and by age.
  #[test]
  fn a_quiet_path_returns_to_the_defaults_and_memory_stays_bounded() {
    let mut reordering = Reordering::default();
    reordering.on_declared_lost(&[5], 12, 0, 20 * MS, 64);
    reordering.on_acknowledged(&[(5, 5)], MS, 20 * MS, 64);
    assert!(reordering.packet_threshold() > REORDER_THRESHOLD);
    for recovery in 0..u64::from(QUIET_RECOVERIES) {
      let pn = 100 + recovery * 10;
      reordering.on_declared_lost(
        &[pn, pn + 1, pn + 2],
        pn + 5,
        (10 + recovery) * 100 * MS,
        20 * MS,
        4,
      );
      assert!(reordering.declared.len() <= 4, "bounded by the cap");
    }
    assert_eq!(reordering.packet_threshold(), REORDER_THRESHOLD);
    assert_eq!(reordering.extra_delay(20 * MS, 20 * MS), 0);
    assert!(reordering.declared.len() <= 3, "old declarations forgotten");
  }
}
