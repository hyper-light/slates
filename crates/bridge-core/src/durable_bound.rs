//! When a reply held for a live-shipped volume's durability gives up (§4.10 status "FUSE `fsync` at durability"; A-200):
//! the one rule every transport that holds such a reply judges by — the FUSE `fsync`, an NFS stable write or `COMMIT`
//! and a Shards `FSYNC` in the daemon (`slates-server`'s `live_ship`), and the virtio-fs device's `FSYNC`
//! (`slates-bridge-virtiofs`). Pure: the caller hands it the volume's count of acknowledgements heard on its durability
//! path and the clock, so its judgment is tested on every host and no transport keeps a second mechanism.
//!
//! **The rule.** A wait runs in windows of a fixed length (the daemon's liveness budget). A window that ends with
//! nothing heard gives up: no holder answered for a whole budget, so none is live to wait for. A window that ends with
//! something heard (a holder's acknowledged position advancing, the volume's head or catalog placement gaining an
//! acknowledgement) starts another. Before 2026-11-09 the wait was one wall-clock deadline of a single window from the
//! hold, so load that slowed the candidates or the owner spent it: a healthy FUSE `fsync` was answered `EIO` under load
//! with its bytes already held by a candidate (1 in 6 runs on main, 2 in 9 on raft-7).
//!
//! **The total bound.** Progress that trickles in, other writes' acknowledgements moving the count while this reply's
//! own position is never acknowledged, would start window after window without end (banned item 8; the supervisor's
//! landing condition, 2026-11-11). So a wait begins at most [`DurableBound::windows_within`] windows whatever it hears:
//! the failover SLO's worth, the operator's bound on how long an object may go unserved before a takeover serves it
//! (§4.8). A write the fleet has not made durable within it is past what the fleet promises to keep.

/// Why a [`DurableBound`] gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaveUp {
  /// A whole window passed with nothing heard on the volume's durability path.
  Silent,
  /// The total bound was reached, whatever was heard.
  AtTheBound,
}

impl GaveUp {
  /// How a refusal's record names it.
  #[must_use]
  pub const fn named(self) -> &'static str {
    match self {
      GaveUp::Silent => "nothing heard in its last window",
      GaveUp::AtTheBound => "its total bound reached, the failover SLO's worth of windows",
    }
  }
}

/// One held reply's wait for durability, in windows ([`DurableBound::judge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableBound {
  /// The volume's heard count as this window began.
  heard: u64,
  /// When this window ends.
  ends_ns: u64,
  /// The windows begun, this one included.
  windows: u32,
  /// The most windows the wait may begin: its total bound.
  most: u32,
  /// One window's length.
  window_ns: u64,
}

impl DurableBound {
  /// Derived: the most windows a wait may begin under a failover SLO of `failover_slo_ns` with windows of `window_ns`:
  /// the SLO's worth of windows (`failover_slo_ns / window_ns`), at least one.
  #[must_use]
  pub fn windows_within(failover_slo_ns: u64, window_ns: u64) -> u32 {
    u32::try_from(failover_slo_ns.checked_div(window_ns).unwrap_or(0).max(1)).unwrap_or(u32::MAX)
  }

  /// The first window, of `window_ns`, of a wait that has heard `heard` by `now_ns` and may begin `most` windows.
  #[must_use]
  pub const fn begin(heard: u64, now_ns: u64, window_ns: u64, most: u32) -> DurableBound {
    DurableBound {
      heard,
      ends_ns: now_ns.saturating_add(window_ns),
      windows: 1,
      most,
      window_ns,
    }
  }

  /// When the window now open ends: a waiter asks [`DurableBound::judge`] then.
  #[must_use]
  pub const fn ends_ns(&self) -> u64 {
    self.ends_ns
  }

  /// The windows begun so far.
  #[must_use]
  pub const fn windows(&self) -> u32 {
    self.windows
  }

  /// Whether, and why, the wait has given up at `now_ns`, the volume having heard `heard` by then: a window not yet
  /// ended goes on; one that ended with nothing heard gives up silent; one that ended with something heard starts the
  /// next, from `now_ns`, unless the wait has begun its most windows already.
  pub fn judge(&mut self, heard: u64, now_ns: u64) -> Option<GaveUp> {
    if now_ns < self.ends_ns {
      return None;
    }
    if heard == self.heard {
      return Some(GaveUp::Silent);
    }
    if self.windows >= self.most {
      return Some(GaveUp::AtTheBound);
    }
    self.heard = heard;
    self.ends_ns = now_ns.saturating_add(self.window_ns);
    self.windows = self.windows.saturating_add(1);
    None
  }
}

#[cfg(test)]
mod tests {
  use super::{DurableBound, GaveUp};

  /// Shape: a window's length in these tests: the daemon's liveness budget.
  const WINDOW_NS: u64 = 1_000_000_000;
  /// Shape: the default failover SLO, ten windows.
  const SLO_NS: u64 = 10_000_000_000;
  /// Shape: the hold's instant.
  const HELD_AT_NS: u64 = 1_000;

  /// The bound's total. Do: derive the most windows from the default failover SLO, from one shorter than a window, and
  /// with no window at all. Expect ten, and one at the least.
  #[test]
  fn a_held_replys_windows_are_the_failover_slos_worth() {
    assert_eq!(DurableBound::windows_within(SLO_NS, WINDOW_NS), 10);
    assert_eq!(DurableBound::windows_within(WINDOW_NS / 2, WINDOW_NS), 1);
    assert_eq!(DurableBound::windows_within(SLO_NS, 0), 1);
  }

  /// A window with nothing heard ends the wait, and one not yet ended goes on. Do: begin a wait, judge it within its
  /// first window and at its end with nothing heard. Expect nothing within, `Silent` at the end, after one window.
  #[test]
  fn a_window_with_nothing_heard_gives_up_and_one_not_yet_ended_goes_on() {
    let mut bound = DurableBound::begin(7, HELD_AT_NS, WINDOW_NS, 10);
    assert_eq!(bound.judge(7, HELD_AT_NS + WINDOW_NS - 1), None);
    assert_eq!(bound.judge(7, bound.ends_ns()), Some(GaveUp::Silent));
    assert_eq!(bound.windows(), 1);
  }

  /// The total bound (banned item 8). Do: begin a wait and, at every window's end, hear one more acknowledgement (other
  /// writes' placements moving while this reply's own position is never acknowledged), until the wait ends or a
  /// thousand windows pass. Expect it to end `AtTheBound` after exactly the bound's windows. Red with the bound removed:
  /// a thousand windows and no end.
  #[test]
  fn acknowledgements_that_trickle_without_the_target_end_the_wait_at_its_bound() {
    let mut bound = DurableBound::begin(0, HELD_AT_NS, WINDOW_NS, 10);
    let mut heard = 0;
    let mut ended = None;
    for _ in 0..1_000 {
      heard += 1;
      if let Some(why) = bound.judge(heard, bound.ends_ns()) {
        ended = Some(why);
        break;
      }
    }
    assert_eq!(
      ended,
      Some(GaveUp::AtTheBound),
      "the wait ends at its bound"
    );
    assert_eq!(bound.windows(), 10, "after exactly the bound's windows");
  }

  /// A window that heard something starts the next from the moment it was judged. Do: hear once at the first window's
  /// end. Expect the wait to go on, its next window ending a whole window later.
  #[test]
  fn a_window_that_heard_something_starts_the_next() {
    let mut bound = DurableBound::begin(0, HELD_AT_NS, WINDOW_NS, 10);
    let judged_at = bound.ends_ns() + 5;
    assert_eq!(bound.judge(1, judged_at), None);
    assert_eq!(bound.ends_ns(), judged_at + WINDOW_NS);
    assert_eq!(bound.windows(), 2);
  }
}
