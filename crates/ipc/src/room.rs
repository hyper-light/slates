//! The wait for room on a full command ring (§4.7 "Failure matrix", §4.3; A-232, A-523, A-571): one rule for every
//! client end ([`crate::ClientEnd::send_when_room`]), the SDK's (`slates_client::Client`) and a test's raw one alike. A
//! ring is full only until its daemon drains it, which a live daemon does in microseconds, so the wait spins for the
//! client's wake estimate; then it parks, each pause twice the last from the wake estimate and never past the next look
//! at the daemon's life, which comes on A-443's doubling ([`crate::looks`]): first at twice the longer of the wait when
//! first judged and the wake estimate, then at twice the time waited. While the daemon lives the wait goes on, so a
//! ring a starved daemon does not drain costs the waiting thread no core; a look that finds the daemon gone ends it,
//! the request not written ([`Roomed::DaemonGone`]). Its pauses and looks are made of the wait's own length and this
//! machine's measured wake, never a fixed time, and a wait of `t` wakes about `log2(t / wake)` times. A slot that frees
//! wakes nothing, so it is found at the next try, within the time already waited (owed in GAPS: the daemon waking a
//! client that waits for room, as it wakes one that waits for a reply).
//!
//! Until A-523 the SDK's wait spun for a whole reply deadline (a fixed second) and then paced its tries at a tenth of
//! it. Until A-571 the schedule was the SDK's alone, and each raw client of the daemon's tests (`ack_release`,
//! `attach_forms`, `shards`) spun on a full ring for five seconds (`CREDIT_WAIT`) and failed `RingFull` there, its
//! daemon alive or gone.

use crate::looks::Looks;

/// What a wait for room came to ([`crate::ClientEnd::send_when_room`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Roomed {
  /// The request was written into the ring.
  Sent,
  /// A look at the daemon's life found it gone before a slot freed: the request was not written.
  DaemonGone,
}

/// One wait for room's schedule (the module doc's rule): what it does next, `elapsed_ns` into the wait.
#[derive(Debug, Default)]
pub(crate) struct RoomWait {
  /// When the wait next asks the daemon's liveness.
  looks: Looks,
  /// The last pause, nanoseconds; `None` before the first.
  pause_ns: Option<u64>,
}

/// What a wait for room does next ([`RoomWait::next`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomStep {
  /// Try the ring again at once.
  Spin,
  /// Ask the daemon's liveness, then try again.
  Look,
  /// Park this long, nanoseconds, then try again.
  Park(u64),
}

impl RoomWait {
  /// The next step, `elapsed_ns` into the wait, the client's wake estimate being `wake_ns`.
  pub(crate) fn next(&mut self, elapsed_ns: u64, wake_ns: u64) -> RoomStep {
    if elapsed_ns < wake_ns {
      return RoomStep::Spin;
    }
    if self.looks.due(elapsed_ns, wake_ns) {
      return RoomStep::Look;
    }
    let to_look_ns = self.looks.until(elapsed_ns).unwrap_or(wake_ns).max(1);
    let pause_ns = self
      .pause_ns
      .map_or(wake_ns, |pause| pause.saturating_mul(2))
      .max(1)
      .min(to_look_ns);
    self.pause_ns = Some(pause_ns);
    RoomStep::Park(pause_ns)
  }

  /// A look made `elapsed_ns` into the wait found the daemon alive: the next comes at twice the time waited.
  pub(crate) fn looked(&mut self, elapsed_ns: u64) {
    self.looks.looked(elapsed_ns);
  }
}

#[cfg(test)]
mod tests {
  use super::{RoomStep, RoomWait};

  /// Shape: the client's wake estimate the wait is walked with: 3 µs, a wake measured on Apple silicon.
  const WAKE_NS: u64 = 3_000;
  /// Shape: how far one spin moves the clock in the walk: a tenth of the wake.
  const SPIN_NS: u64 = WAKE_NS / 10;
  /// Shape: the wait walked across: half again the reply deadline the wait spun through before A-523 (a second).
  const WAIT_NS: u64 = 1_500_000_000;

  /// What a walked wait did: the times waited at which it looked, and each pause it parked for.
  #[derive(Debug, Default)]
  struct Walk {
    looks: Vec<u64>,
    parks: Vec<u64>,
  }

  /// Walks a wait for a ring slot across [`WAIT_NS`] from the first refusal, each step held to its rule as it is taken
  /// ([`step`]): what the wait did.
  fn walk() -> Walk {
    let mut wait = RoomWait::default();
    let mut walked = Walk::default();
    let mut elapsed = 0;
    while elapsed < WAIT_NS {
      elapsed = step(&mut wait, elapsed, &mut walked);
    }
    walked
  }

  /// Takes the wait's next step `elapsed` into it, the daemon found alive at every look: a spin only inside the wake
  /// estimate, a park held to its rule ([`assert_park`]). The time waited after the step.
  fn step(wait: &mut RoomWait, elapsed: u64, walked: &mut Walk) -> u64 {
    match wait.next(elapsed, WAKE_NS) {
      RoomStep::Spin => {
        assert!(
          elapsed < WAKE_NS,
          "spun at {elapsed} ns, past the wake estimate"
        );
        elapsed + SPIN_NS
      }
      RoomStep::Look => {
        walked.looks.push(elapsed);
        wait.looked(elapsed);
        elapsed
      }
      RoomStep::Park(pause) => {
        assert_park(wait, elapsed, pause, walked.parks.last().copied());
        walked.parks.push(pause);
        elapsed + pause
      }
    }
  }

  /// Holds a pause of `pause` ns, `elapsed` into the wait, to its rule: never past the next look, and no more than twice
  /// the `last` pause.
  fn assert_park(wait: &RoomWait, elapsed: u64, pause: u64, last: Option<u64>) {
    let to_look = wait.looks.until(elapsed).unwrap();
    assert!(
      pause <= to_look,
      "a {pause} ns pause past the next look, {to_look} ns on"
    );
    if let Some(last) = last {
      assert!(pause <= 2 * last, "a {pause} ns pause after {last} ns");
    }
  }

  /// A-523 (R3's class C). Do: walk a wait for a ring slot across 1.5 s from the first refusal, spinning, looking (the
  /// daemon found alive each time) and parking as the schedule says. Expect: a spin only inside the wake estimate; each
  /// pause no more than twice the last and never past the next look; the looks at twice the time waited from twice the
  /// wake; and no more looks, nor parks between them, than the logarithm of the wait over the wake plus two. Before
  /// A-523 the wait spun for a whole reply deadline, a second, and then paced its tries at a tenth of it.
  #[test]
  fn a_wait_for_a_ring_slot_spins_for_the_wake_then_parks_and_looks_on_the_doubling() {
    let walked = walk();
    assert_eq!(
      walked.looks.first(),
      Some(&(2 * WAKE_NS)),
      "the first look at twice the wake"
    );
    for pair in walked.looks.windows(2) {
      assert_eq!(pair[1], 2 * pair[0], "the looks at {:?}", walked.looks);
    }
    let bound = u64::from((WAIT_NS / WAKE_NS).ilog2()) + 2;
    for (kind, count) in [("looks", walked.looks.len()), ("parks", walked.parks.len())] {
      assert!(
        u64::try_from(count).unwrap() <= bound,
        "{count} {kind}, past {bound}"
      );
    }
  }
}
