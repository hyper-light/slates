//! When a wait that cannot see its peer's departure asks after the peer's life (§4.7 "Failure matrix"; A-443's
//! doubling, A-456, A-523). A call waits while its daemon lives (A-232). Where the wait cannot wait on the departure
//! itself, it asks the daemon's liveness on this schedule. The first look comes once the wait has lasted twice the
//! longer of the time it had waited when it was first judged and the client's wake estimate: a look costs a wake, so
//! none comes sooner than twice the measured cost of one. Each later look comes at twice the time waited at the one
//! before. A wait of `t` then makes at most `log2(t / first) + 1` looks, and a departure is seen by twice the time
//! waited when it happened, or at the first look when it came before that. Nothing on the schedule is a fixed time:
//! it is made of the wait's own length and this machine's measured wake.
//!
//! Two waits keep it. The synchronous reply wait's park on macOS, and on Windows for a daemon restarted inside its own
//! process (`crate::endpoint::ClientEnd::wait_while_daemon_lives`, A-456), and every call the async driver owns
//! (`slates_client::Driver`, A-523), whose event loop learns of a reply from the completion descriptor but of nothing
//! about the daemon's departure. Until A-523 the async driver asked at every reply deadline instead, the anchor's
//! liveness budget, a fixed second.

/// One wait's look schedule (see the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub struct Looks {
  /// The time waited at which the next look is due; `None` until the wait is first judged.
  next_ns: Option<u64>,
}

impl Looks {
  /// Whether a look is due at `elapsed_ns` waited. The first call fixes the first look from `elapsed_ns` and the wake
  /// estimate `wake_ns` ([`first_look_ns`]).
  pub fn due(&mut self, elapsed_ns: u64, wake_ns: u64) -> bool {
    let next_ns = *self
      .next_ns
      .get_or_insert_with(|| first_look_ns(elapsed_ns, wake_ns));
    elapsed_ns >= next_ns
  }

  /// A look made at `elapsed_ns` found the daemon alive: the next comes at twice the time waited.
  pub fn looked(&mut self, elapsed_ns: u64) {
    self.next_ns = Some(
      elapsed_ns
        .saturating_mul(2)
        .max(elapsed_ns.saturating_add(1)),
    );
  }

  /// How long the wait may go on before the next look is due (`None` until the wait was first judged).
  pub fn until(&self, elapsed_ns: u64) -> Option<u64> {
    self
      .next_ns
      .map(|next_ns| next_ns.saturating_sub(elapsed_ns))
  }

  /// How long the wait may go on before the next look is due, `elapsed_ns` into it, without judging it: the first
  /// look's time from `elapsed_ns` and `wake_ns` when the wait has not been judged yet, as [`Self::due`] would fix it. A
  /// caller asked when to wake next, holding the schedule unchanged, answers this (the async driver's timer).
  pub fn until_or_first(&self, elapsed_ns: u64, wake_ns: u64) -> u64 {
    self
      .next_ns
      .unwrap_or_else(|| first_look_ns(elapsed_ns, wake_ns))
      .saturating_sub(elapsed_ns)
  }
}

/// Derived: the time waited at a wait's first look ([`Looks`]): twice the longer of the time waited when the wait was
/// first judged (a synchronous wait's first park, the async driver's first tick over the call) and the wake estimate.
pub fn first_look_ns(elapsed_ns: u64, wake_ns: u64) -> u64 {
  slates_machine::derived!(
    elapsed_ns.max(wake_ns).saturating_mul(2).max(1),
    "2 × max(time waited when first judged, wake estimate)",
    ["elapsed_ns", "wake_ns"]
  )
  .get()
}

#[cfg(test)]
mod tests {
  use super::{Looks, first_look_ns};

  /// Shape: where the schedule is walked from, as (the time waited when the wait is first judged, the wake estimate) in
  /// nanoseconds: a first judgment inside the wake (a caller that spins for none of it), one at it, and one well past it
  /// (a spin window chosen long).
  const STARTS: [(u64, u64); 3] = [(0, 3_000), (3_000, 3_000), (200_000, 3_000)];
  /// Shape: the wait the schedule is walked across: half again the second the wait re-armed at before A-456.
  const WAIT_NS: u64 = 1_500_000_000;

  /// A-456 on A-443's doubling. Do: walk a look schedule across a wait from each start, each wait timing out when the
  /// next look is due, and look then. Expect: no look at the first judgment; the first at twice the longer of the time
  /// waited when first judged and the wake estimate; each later one at twice the time waited at the one before; and no
  /// more looks across the wait than the logarithm of the wait over the first look's time, plus two (the last one past
  /// it).
  #[test]
  fn looks_come_at_twice_the_time_waited_from_twice_the_longer_of_the_first_judgment_and_the_wake()
  {
    for (first_judged_ns, wake_ns) in STARTS {
      let first = first_look_ns(first_judged_ns, wake_ns);
      assert_eq!(first, 2 * first_judged_ns.max(wake_ns), "the first look");
      let mut looks = Looks::default();
      assert!(
        !looks.due(first_judged_ns, wake_ns),
        "no look at the first judgment"
      );
      let mut elapsed = first_judged_ns;
      let mut made: Vec<u64> = Vec::new();
      while elapsed < WAIT_NS {
        elapsed += looks.until(elapsed).unwrap();
        assert!(looks.due(elapsed, wake_ns), "a look due at {elapsed} ns");
        made.push(elapsed);
        looks.looked(elapsed);
      }
      assert_eq!(made.first(), Some(&first));
      for pair in made.windows(2) {
        assert_eq!(pair[1], 2 * pair[0], "the looks at {made:?}");
      }
      let bound = u64::from((WAIT_NS / first).ilog2()) + 2;
      assert!(
        u64::try_from(made.len()).unwrap() <= bound,
        "{} looks across {WAIT_NS} ns from {first} ns, past the bound {bound}",
        made.len()
      );
    }
  }

  /// A-523. Do: ask a schedule not yet judged when it next looks, from each start, then judge it there. Expect: the time
  /// to the first look the judgment then fixes, and the schedule unchanged by the asking (the async driver's timer reads
  /// it between ticks without moving it).
  #[test]
  fn a_schedule_not_yet_judged_names_its_first_look_without_fixing_it() {
    for (first_judged_ns, wake_ns) in STARTS {
      let mut looks = Looks::default();
      let asked = looks.until_or_first(first_judged_ns, wake_ns);
      assert_eq!(looks.until(first_judged_ns), None, "asking fixed nothing");
      assert_eq!(
        asked,
        first_look_ns(first_judged_ns, wake_ns) - first_judged_ns
      );
      assert!(!looks.due(first_judged_ns, wake_ns));
      assert_eq!(looks.until(first_judged_ns), Some(asked), "the look judged");
    }
  }
}
