//! The idle windows a live write's shards hold instead of parking (§4.10 "Live shipping", §4.7 "Wake strategy"; A-396).
//!
//! A stable write to a live-shipped volume is answered once a candidate holds it (A-200), so its path runs through four
//! shards on three nodes: the volume's owner shard builds and appends the entry, the owner node's control shard sends it
//! and folds the answer back, and each holder's control shard keeps it and answers. Every one of them waited for its
//! part by parking in its driver, so a write paid a parked thread's wake at each step: the control shard woken for the
//! dispatch and again for the holder's answer, the holder woken for the entry, the owner woken for the fold. Measured
//! with the contract command (`measure_what_live_shipping_adds_to_a_stable_nfs_write`, adacachy at load 45-70 in a
//! capped unit, 2026-11-18): live shipping added 136-291 µs at the median on main, and holding the three shards' windows
//! (the owner while it held a reply, the control shard through and after its exchange, the holders after each entry)
//! took that to 63-120 µs; the owner and control shard alone, 141-221 µs. Removing the cross-shard hop instead (one
//! shard a daemon, so the owner is the control shard) did less: 101-163 µs, and 78-107 µs with the holds, where two held
//! shards working in parallel did better (BENCHMARKS, A-396). So the cost was parked threads, not the hop.
//!
//! Two kinds of wait, each learned from the traffic it waits on and never a fixed number (R3):
//! - [`AnswerWindow`]: a shard waits for the answer to a message it caused (the owner for the fold that makes its write
//!   durable, the control shard for a holder's answer). It learns how long those answers take by RFC 9002 §5.3's law
//!   (the transport's own estimator) and holds for the bound an answer is expected within (`smoothed + 4 · variation`,
//!   §6.2.1's terms without the timer granularity a spinning waiter does not arm).
//! - [`StreamWindow`]: a shard waits for the next message of a stream in a closed loop (the control shard for the next
//!   dispatch, a holder for the next entry of an owner's log). It learns the stream's arrival gaps by the same law and
//!   holds for the bound, but never longer than the stream's current run has lasted and nothing for a run's first
//!   message: A-326's ski-rental rule for a closed loop's holds, so a run's end costs at most the spin the run itself
//!   took, and a paced sender, whose gaps outrun the bound, starts no run.
//!
//! Every hold is asked of the runtime's `ShardContext::hold_idle_window`, under the idle spin's ceiling (A-361), and
//! counted by its site, the fast path's non-vacuity count. A hold pays only while a held shard sees a message sooner than
//! a kicked park would wake for it, and the runtime decides that from the shard's own measurements, the same code on
//! every host (A-387's measured predicate): it keeps a hold while the idle turn a message lands in, learned from the
//! shard's own spins, is shorter than its wake estimate, and declines it otherwise ([`hold`]). Linux's epoll and Windows'
//! completion port answer a zero-timeout poll at once, so their shards hold; a macOS shard's zero-timeout `kevent` naps
//! 12–15 µs a turn (A-218), longer than its wake, so it declines; and a shard whose turns a loaded machine preempts stops
//! holding until they are cheap again. The windows are learned all the same, wherever the holds are declined.

use slates_transport::rtt::RttEstimator;

/// What a shard learns of the answers to the messages it causes: how long they take.
#[derive(Debug, Default)]
pub(crate) struct AnswerWindow {
  answers: RttEstimator,
}

impl AnswerWindow {
  /// An answer came `ns` after the message that caused it.
  pub(crate) fn answered(&mut self, ns: u64) {
    self.answers.on_sample(ns, 0);
  }

  /// How long after its message an answer is expected within, nanoseconds; `None` until one answer has been timed, so a
  /// cold start holds nothing.
  pub(crate) fn window(&self) -> Option<u64> {
    self.answers.expected_within()
  }
}

/// What a shard learns of a stream it handles in a closed loop: the gaps between the stream's messages, the last one's
/// arrival, and when the current run began.
#[derive(Debug, Default)]
pub(crate) struct StreamWindow {
  gaps: RttEstimator,
  last_ns: Option<u64>,
  run_began_ns: u64,
}

impl StreamWindow {
  /// A message of the stream arrived at `now_ns`: how long from its arrival the shard is to hold its idle window for the
  /// next one. A gap within the bound the gaps so far are expected within continues the run; a longer one, the pause of
  /// a paced sender, begins a new run at this message. The window is the bound after this gap, at most the run's length
  /// so far, so a run's first message holds nothing and a run that ends leaves at most its own length of spin.
  pub(crate) fn arrived(&mut self, now_ns: u64) -> u64 {
    let Some(last) = self.last_ns.replace(now_ns) else {
      self.run_began_ns = now_ns;
      return 0;
    };
    let gap = now_ns.saturating_sub(last);
    let continues = self.gaps.expected_within().is_none_or(|bound| gap <= bound);
    if !continues {
      self.run_began_ns = now_ns;
    }
    self.gaps.on_sample(gap, 0);
    let run = now_ns.saturating_sub(self.run_began_ns);
    self
      .gaps
      .expected_within()
      .map_or(0, |bound| bound.min(run))
  }
}

/// The part of a window that `from_ns + window_ns` leaves after `now_ns`: what a hold applied now must still cover.
pub(crate) fn remaining(from_ns: u64, window_ns: u64, now_ns: u64) -> u64 {
  from_ns.saturating_add(window_ns).saturating_sub(now_ns)
}

/// Asks this shard to hold its idle window open for `window_ns` from now (`ShardContext::hold_idle_window`, under the idle
/// spin's ceiling, A-361): `Some(true)` when the shard kept the hold, `Some(false)` when it declined it, its measured idle
/// turn no shorter than its wake (A-387), and `None` when nothing was asked: an empty window, or a caller off a shard.
pub(crate) fn hold(window_ns: u64) -> Option<bool> {
  if window_ns == 0 {
    return None;
  }
  slates_rt::registry::with_current(|ctx| ctx.hold_idle_window(window_ns))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A microsecond in nanoseconds, so the samples read as round times.
  const US: u64 = 1_000;

  /// A-396: do time nothing, then one answer of 40 µs and one of 40 µs again; expect no window before an answer was timed
  /// (a cold start holds nothing), then the bound RFC 9002 §5.3 and §6.2.1 give the answers: 120 µs, then 100 µs as the
  /// variation settles.
  #[test]
  fn an_answer_window_is_the_answers_bound_and_none_before_the_first() {
    let mut window = AnswerWindow::default();
    assert_eq!(window.window(), None);
    window.answered(40 * US);
    assert_eq!(window.window(), Some(120 * US));
    window.answered(40 * US);
    assert_eq!(window.window(), Some(100 * US));
  }

  /// A-396: do feed a stream messages 100 µs apart; expect its first message to hold nothing, each later one to hold no
  /// longer than the run has lasted, and a steady run's window to settle at the gaps' bound (here the gap itself, the
  /// variation decaying), never past it.
  #[test]
  fn a_streams_first_message_holds_nothing_and_a_run_holds_no_longer_than_it_has_lasted() {
    let mut stream = StreamWindow::default();
    assert_eq!(stream.arrived(0), 0, "a run's first message holds nothing");
    let mut now = 0;
    let mut windows = Vec::new();
    for _ in 0..40 {
      now += 100 * US;
      windows.push(stream.arrived(now));
    }
    let first = windows.first().copied().unwrap();
    assert_eq!(
      first,
      100 * US,
      "the second message holds the run so far, one gap"
    );
    for (index, window) in windows.iter().enumerate() {
      let run = u64::try_from(index + 1).unwrap() * 100 * US;
      assert!(
        *window <= run,
        "a hold never outlasts its run: {window} after {run}"
      );
    }
    let last = windows.last().copied().unwrap();
    assert!(
      (100 * US..=110 * US).contains(&last),
      "a steady run settles at the gaps' bound, the gap: {last}"
    );
  }

  /// A-396: do run a stream at 100 µs, then pause 50 ms, then resume; expect the message after the pause to begin a new
  /// run and hold nothing, and the run after it to grow again from one gap, so a paced sender's pause leaves no long spin.
  #[test]
  fn a_pause_past_the_bound_ends_the_run_and_the_next_message_holds_nothing() {
    let mut stream = StreamWindow::default();
    let mut now = 0;
    for _ in 0..20 {
      stream.arrived(now);
      now += 100 * US;
    }
    now += 50_000 * US;
    assert_eq!(
      stream.arrived(now),
      0,
      "the message after a pause begins a new run"
    );
    now += 100 * US;
    assert_eq!(
      stream.arrived(now),
      100 * US,
      "the new run holds what it has lasted, one gap, not the pause"
    );
  }

  /// A-396: do ask what is left of a window opened before now, at its end and past it; expect the remainder, then none.
  #[test]
  fn a_window_opened_earlier_leaves_only_its_remainder() {
    assert_eq!(remaining(1_000, 500, 1_200), 300);
    assert_eq!(remaining(1_000, 500, 1_500), 0);
    assert_eq!(remaining(1_000, 500, 9_000), 0);
  }
}
