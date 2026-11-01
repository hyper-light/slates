//! The purges Windows did not take, made again (§4.6 "Windows"; A-190 as amended 2026-11-01, Step 8 of the VFS and DAX
//! plan; `docs/bugs/2026-11-01-a-winfsp-purge-windows-did-not-take-left-a-mapped-view-stale.md`).
//!
//! A purge whose read back finds a page Windows kept ([`PurgeOutcome::Untaken`]) is made again by the mount's thread,
//! on its own, until Windows takes it. Only that purge is made again: the round it came with is taken, and the other
//! files' purges are not repeated. Its first attempt waits one tick of the mount's pacing, and each next attempt waits
//! twice as long as the one before, so a page Windows keeps for a time `T` costs about `log2(T / tick)` attempts.
//!
//! A purge is given up, and counted kept, once its next attempt would come later than the volume's request timeout
//! after Windows first kept its page. WinFsp cancels any request through the volume left unanswered that long
//! (`src/sys/ioq.c`, `FspIoqRemoveExpired`), so by then no I/O through the volume still holds the page. What holds it
//! is a program's own lock (`VirtualLock` on a view, say), which no purge can undo. A later change to the file purges it
//! again.
//!
//! The list holds at most `bound` purges: the caller passes the most purges a round can carry (one per open name of a
//! changed file, so the open-handle cap). An untaken purge past it is given up and counted kept.
//!
//! Pure and cfg-free (the mount thread passes its clock in), so it is tested on every host.

use crate::purge::{Purge, PurgeOutcome};

/// Format: the request timeout the volume runs with, in milliseconds: WinFsp's default (`fsctl.h`,
/// `FspFsctlIrpTimeoutDefault`; its minimum is 60,000 and its maximum 600,000), set in the volume's parameters
/// (`crate::host`) so that this module's horizon is the volume's own. WinFsp completes a request left unanswered this
/// long as cancelled (`src/sys/ioq.c`, `FspIoqRemoveExpired`; `src/sys/iop.c`, `STATUS_CANCELLED`).
pub const REQUEST_TIMEOUT_MS: u32 = 300_000;
/// Format: nanoseconds in a millisecond.
const NANOS_PER_MILLI: u64 = 1_000_000;
/// Derived: [`REQUEST_TIMEOUT_MS`] in nanoseconds, the horizon past which a purge Windows keeps is given up.
pub const REQUEST_TIMEOUT_NS: u64 = REQUEST_TIMEOUT_MS as u64 * NANOS_PER_MILLI;

/// What [`PurgeRetries::record`] made of one attempt's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
  /// Windows took it: nothing is owed for it, nor for any owed range of the same file it covers.
  Taken,
  /// Its open, a write or a read back was refused (the file renamed or removed since, say): nothing is owed for it.
  Refused,
  /// Windows kept a page: another attempt is owed after a wait.
  Retrying,
  /// Windows kept a page, and nothing more is owed for it: the next attempt would come past the horizon, or the list
  /// is full. A view of its range may stay stale until a later change purges it again.
  Kept,
}

/// A purge owed another attempt.
#[derive(Clone, Debug)]
struct Owed {
  purge: Purge,
  /// When Windows first kept a page of it, on the caller's clock (ns).
  first_kept_ns: u64,
  /// The wait before the attempt now owed.
  wait_ns: u64,
  /// When that attempt is due.
  due_ns: u64,
}

/// The purges owed another attempt, each with its wait (see the module docs).
#[derive(Debug)]
pub struct PurgeRetries {
  owed: Vec<Owed>,
  bound: usize,
  first_wait_ns: u64,
  horizon_ns: u64,
}

impl PurgeRetries {
  /// An empty list holding at most `bound` purges. A purge's first attempt waits `first_wait_ns` (at least 1 ns, so
  /// attempts never come back to back), and a purge is given up once an attempt would come later than `horizon_ns`
  /// after Windows first kept its page.
  pub fn new(bound: usize, first_wait_ns: u64, horizon_ns: u64) -> PurgeRetries {
    PurgeRetries {
      owed: Vec::new(),
      bound,
      first_wait_ns: first_wait_ns.max(1),
      horizon_ns,
    }
  }

  /// Records what an attempt at `purge` came to at `now_ns`, and says what is owed for it now.
  pub fn record(&mut self, purge: &Purge, outcome: PurgeOutcome, now_ns: u64) -> Recorded {
    match outcome {
      PurgeOutcome::Taken => {
        self.owed.retain(|owed| !covers(purge, &owed.purge));
        Recorded::Taken
      }
      PurgeOutcome::Refused => {
        self.owed.retain(|owed| owed.purge != *purge);
        Recorded::Refused
      }
      PurgeOutcome::Untaken => self.untaken(purge, now_ns),
    }
  }

  /// Windows kept a page of `purge` at `now_ns`: its next attempt waits twice the last wait, or one first wait for a
  /// purge not yet owed; a purge whose next attempt would pass the horizon (or whose wait no clock can hold), or that
  /// finds the list full, is given up.
  fn untaken(&mut self, purge: &Purge, now_ns: u64) -> Recorded {
    let horizon = self.horizon_ns;
    if let Some(owed) = self.owed.iter_mut().find(|owed| owed.purge == *purge) {
      // A wait no clock can hold is a wait past any horizon.
      let Some(wait) = owed.wait_ns.checked_mul(2) else {
        self.owed.retain(|owed| owed.purge != *purge);
        return Recorded::Kept;
      };
      let due = now_ns.saturating_add(wait);
      if due.saturating_sub(owed.first_kept_ns) > horizon {
        self.owed.retain(|owed| owed.purge != *purge);
        return Recorded::Kept;
      }
      owed.wait_ns = wait;
      owed.due_ns = due;
      return Recorded::Retrying;
    }
    if self.owed.len() >= self.bound || self.first_wait_ns > horizon {
      return Recorded::Kept;
    }
    self.owed.push(Owed {
      purge: purge.clone(),
      first_kept_ns: now_ns,
      wait_ns: self.first_wait_ns,
      due_ns: now_ns.saturating_add(self.first_wait_ns),
    });
    Recorded::Retrying
  }

  /// The purges whose attempt is due at `now_ns`, earliest first.
  pub fn due(&self, now_ns: u64) -> Vec<Purge> {
    let mut due: Vec<&Owed> = self
      .owed
      .iter()
      .filter(|owed| owed.due_ns <= now_ns)
      .collect();
    due.sort_by_key(|owed| owed.due_ns);
    due.into_iter().map(|owed| owed.purge.clone()).collect()
  }

  /// How long after `now_ns` the next attempt is due: zero when one is due already, `None` when nothing is owed.
  pub fn next_due_in(&self, now_ns: u64) -> Option<u64> {
    self
      .owed
      .iter()
      .map(|owed| owed.due_ns.saturating_sub(now_ns))
      .min()
  }

  /// How many purges are owed an attempt.
  pub fn len(&self) -> usize {
    self.owed.len()
  }

  /// Whether no purge is owed an attempt.
  pub fn is_empty(&self) -> bool {
    self.owed.is_empty()
  }
}

/// Whether a taken purge `taken` covers the owed purge `owed`: the same path, and a range holding the owed one, so its
/// write purged the owed range too.
fn covers(taken: &Purge, owed: &Purge) -> bool {
  taken.path == owed.path
    && taken.offset <= owed.offset
    && owed.offset.saturating_add(owed.len) <= taken.offset.saturating_add(taken.len)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a tick of the mount's pacing, as the server derives it (`HEARTBEAT_NS / POLL_PER_PERIOD`, 10 ms).
  const TICK: u64 = 10_000_000;
  /// Format: the volume's request timeout (300 s), the horizon the mount gives purges.
  const HORIZON: u64 = REQUEST_TIMEOUT_NS;
  /// Shape: a list large enough that no test here fills it but the one that means to.
  const ROOMY: usize = 64;
  /// Shape: more attempts than any horizon allows, since a doubling wait passes the clock's range within 64: a purge
  /// still owed after this many is owed forever, the failure the horizon exists to prevent.
  const ATTEMPTS_CEILING: usize = 128;

  fn purge(path: &str, offset: u64, len: u64) -> Purge {
    Purge {
      path: path.to_owned(),
      offset,
      len,
    }
  }

  /// Attempts `owed` whenever it falls due, Windows keeping its page each time, from a first refusal at 0: the instants
  /// of the attempts made, until the list gives it up.
  fn attempts_until_given_up(retries: &mut PurgeRetries, owed: &Purge) -> Vec<u64> {
    let mut made = Vec::new();
    let mut now = 0;
    let mut recorded = retries.record(owed, PurgeOutcome::Untaken, now);
    while recorded == Recorded::Retrying {
      let Some(wait) = retries.next_due_in(now) else {
        break;
      };
      now = now.saturating_add(wait);
      assert_eq!(
        retries.due(now),
        vec![owed.clone()],
        "due when its wait ends"
      );
      made.push(now);
      assert!(
        made.len() <= ATTEMPTS_CEILING,
        "attempts never end: {} made",
        made.len()
      );
      recorded = retries.record(owed, PurgeOutcome::Untaken, now);
    }
    assert_eq!(recorded, Recorded::Kept, "given up at last");
    made
  }

  /// Step 8 of the VFS and DAX plan. Do: a purge Windows keeps at 0, then again at each attempt. Expect: its first
  /// attempt is due one tick on and not before, and each next wait is twice the last.
  #[test]
  fn a_purge_windows_keeps_is_made_again_after_a_tick_then_twice_as_long_each_time() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let owed = purge("\\file.bin", 0, 4096);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 0),
      Recorded::Retrying
    );
    assert_eq!(retries.next_due_in(0), Some(TICK));
    assert!(retries.due(TICK - 1).is_empty(), "not due before its tick");
    assert_eq!(retries.due(TICK), vec![owed.clone()]);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, TICK),
      Recorded::Retrying
    );
    assert_eq!(retries.next_due_in(TICK), Some(2 * TICK));
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 3 * TICK),
      Recorded::Retrying
    );
    assert_eq!(retries.next_due_in(3 * TICK), Some(4 * TICK));
  }

  /// Step 8 of the VFS and DAX plan: the bound on a purge's attempts. Do: a purge Windows keeps at every attempt, each
  /// made when due. Expect: it is given up, counted kept, once its next attempt would come past the horizon; no attempt
  /// was made past it, the last wait doubled the one before, and nothing is owed after. With a 10 ms tick and WinFsp's
  /// 300 s timeout that is 14 attempts: 2^14 - 1 ticks fit in 300 s, 2^15 - 1 do not.
  #[test]
  fn a_purge_windows_keeps_past_the_horizon_is_given_up_and_nothing_more_is_owed() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let owed = purge("\\file.bin", 0, 4096);
    let made = attempts_until_given_up(&mut retries, &owed);
    assert_eq!(made.len(), 14, "attempts made: {made:?}");
    assert!(
      made.iter().all(|at| *at <= HORIZON),
      "no attempt past the horizon: {made:?}"
    );
    assert!(
      made.windows(2).all(|pair| pair[1] > pair[0]),
      "attempts in order: {made:?}"
    );
    assert_eq!(
      retries.next_due_in(*made.last().unwrap()),
      None,
      "nothing owed once given up"
    );
    assert!(retries.is_empty());
  }

  /// Step 8 of the VFS and DAX plan. Do: a purge Windows keeps, then takes at its first attempt. Expect: nothing is
  /// owed after.
  #[test]
  fn a_purge_windows_takes_at_a_later_attempt_is_owed_nothing_more() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let owed = purge("\\file.bin", 0, 4096);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 0),
      Recorded::Retrying
    );
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Taken, TICK),
      Recorded::Taken
    );
    assert_eq!(retries.next_due_in(TICK), None);
    assert!(retries.due(u64::MAX).is_empty());
  }

  /// Step 8 of the VFS and DAX plan. Do: two ranges of a file kept, then a taken purge of the file covering one of them.
  /// Expect: the covered range is owed nothing more, and the other stays owed, as does a range of another file.
  #[test]
  fn a_taken_purge_covering_an_owed_range_drops_it_and_other_ranges_stay_owed() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let first = purge("\\file.bin", 0, 4096);
    let second = purge("\\file.bin", 65536, 4096);
    let other = purge("\\other.bin", 0, 4096);
    for owed in [&first, &second, &other] {
      assert_eq!(
        retries.record(owed, PurgeOutcome::Untaken, 0),
        Recorded::Retrying
      );
    }
    let covering = purge("\\file.bin", 0, 8192);
    assert_eq!(
      retries.record(&covering, PurgeOutcome::Taken, 1),
      Recorded::Taken
    );
    assert_eq!(retries.due(TICK), vec![second, other]);
  }

  /// Step 8 of the VFS and DAX plan. Do: a purge kept, whose attempt is then refused (its file renamed away). Expect:
  /// nothing is owed for it.
  #[test]
  fn a_refused_attempt_is_owed_nothing_more() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let owed = purge("\\file.bin", 0, 4096);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 0),
      Recorded::Retrying
    );
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Refused, TICK),
      Recorded::Refused
    );
    assert!(retries.is_empty());
  }

  /// Step 8 of the VFS and DAX plan: the list's bound. Do: three different purges kept with room for two. Expect: the
  /// third is given up at once, counted kept, and the first two stay owed.
  #[test]
  fn a_purge_kept_with_the_list_full_is_given_up_and_the_owed_ones_stay() {
    let mut retries = PurgeRetries::new(2, TICK, HORIZON);
    let first = purge("\\a.bin", 0, 4096);
    let second = purge("\\b.bin", 0, 4096);
    let third = purge("\\c.bin", 0, 4096);
    assert_eq!(
      retries.record(&first, PurgeOutcome::Untaken, 0),
      Recorded::Retrying
    );
    assert_eq!(
      retries.record(&second, PurgeOutcome::Untaken, 0),
      Recorded::Retrying
    );
    assert_eq!(
      retries.record(&third, PurgeOutcome::Untaken, 0),
      Recorded::Kept
    );
    assert_eq!(retries.len(), 2);
    assert_eq!(retries.due(TICK), vec![first.clone(), second]);
    assert_eq!(
      retries.record(&first, PurgeOutcome::Untaken, TICK),
      Recorded::Retrying,
      "an owed one keeps its place"
    );
  }

  /// Step 8 of the VFS and DAX plan. Do: purges kept at different instants. Expect: they come due earliest first, and
  /// the wait to the next is to the earliest.
  #[test]
  fn owed_purges_come_due_earliest_first() {
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    let late = purge("\\late.bin", 0, 4096);
    let early = purge("\\early.bin", 0, 4096);
    assert_eq!(
      retries.record(&late, PurgeOutcome::Untaken, 5),
      Recorded::Retrying
    );
    assert_eq!(
      retries.record(&early, PurgeOutcome::Untaken, 2),
      Recorded::Retrying
    );
    assert_eq!(retries.next_due_in(0), Some(TICK + 2));
    assert_eq!(retries.due(TICK + 5), vec![early, late]);
    assert_eq!(retries.next_due_in(TICK + 9), Some(0), "overdue is due now");
  }

  /// Step 8 of the VFS and DAX plan: a hostile first wait. Do: a purge kept with a zero first wait. Expect: the wait is
  /// floored at a nanosecond, so no attempt comes due at the instant Windows kept the page.
  #[test]
  fn a_zero_first_wait_never_brings_an_attempt_back_to_back() {
    let owed = purge("\\file.bin", 0, 4096);
    let mut retries = PurgeRetries::new(ROOMY, 0, HORIZON);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 7),
      Recorded::Retrying
    );
    assert!(retries.due(7).is_empty());
    assert_eq!(retries.next_due_in(7), Some(1));
  }

  /// Step 8 of the VFS and DAX plan: a hostile horizon. Do: a purge kept with a zero horizon. Expect: it is given up at
  /// once, and nothing is owed.
  #[test]
  fn a_zero_horizon_gives_a_kept_purge_up_at_once() {
    let owed = purge("\\file.bin", 0, 4096);
    let mut retries = PurgeRetries::new(ROOMY, TICK, 0);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, 0),
      Recorded::Kept
    );
    assert!(retries.is_empty());
  }

  /// Step 8 of the VFS and DAX plan: hostile instants. Do: a purge kept a nanosecond before the clock's end, then again
  /// at its end. Expect: its due instant saturates at the end without overflow, and it is still owed.
  #[test]
  fn instants_at_the_clocks_end_saturate_without_overflow() {
    let owed = purge("\\file.bin", 0, 4096);
    let mut retries = PurgeRetries::new(ROOMY, TICK, HORIZON);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, u64::MAX - 1),
      Recorded::Retrying
    );
    assert_eq!(retries.next_due_in(u64::MAX - 1), Some(1));
    assert_eq!(retries.due(u64::MAX), vec![owed.clone()]);
    assert_eq!(
      retries.record(&owed, PurgeOutcome::Untaken, u64::MAX),
      Recorded::Retrying
    );
  }

  /// Step 8 of the VFS and DAX plan: a horizon no clock reaches. Do: a purge Windows keeps at every attempt, with the
  /// horizon at the clock's end. Expect: the waits double until no clock can hold the next, and then the purge is given
  /// up, so even an endless horizon ends its attempts.
  #[test]
  fn an_endless_horizon_still_ends_a_purges_attempts() {
    let owed = purge("\\file.bin", 0, 4096);
    let mut retries = PurgeRetries::new(ROOMY, TICK, u64::MAX);
    let made = attempts_until_given_up(&mut retries, &owed);
    assert!(!made.is_empty());
    assert!(retries.is_empty());
  }
}
