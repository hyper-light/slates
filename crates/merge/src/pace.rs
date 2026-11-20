//! Work done a unit at a time under a turn's deadline (A-752; §4.3's step budget: no step of a shard runs past its
//! quantum, so a push's work build, whose cost grows with the files a head holds, spans turns).
//!
//! **Why.** A push over a flat 3,500-file tree built its work in one step: 374 to 642 ms (debug, w-git's measure), so a
//! monorepo's push would outlast the watchdog. The build's walk, its base, the deriver's composition, the post-state and
//! the identity each loop over the files; each loop now asks before a unit, and a turn ends when its deadline is past.
//!
//! **How.** A [`Pace`] is a build's own: the turn's deadline and the owner's clock, a function the owner gives (the
//! shard's), so the merge engine reads no clock of its own and stays deterministic. The first unit of a turn always
//! runs, so every turn moves the work. An explicit state machine asks [`Pace::more`]. The deriver, an `async`
//! composition (R6: async where sensible, the sync facade a thin wrapper), awaits [`Ticks::tick`]: its owner polls it
//! through [`Pace::drive`], which lends the pace's turn to the thread for that poll alone (a `Copy` of its figures in a
//! thread-local cell, taken back after), so the composition owns no reference to the pace (R2: no shared ownership) and
//! is pending once the turn is spent. Outside a drive no turn is lent and a tick is always ready, so [`finish`] runs a
//! composition to its end in one call, as every caller that needs no turns does.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};

/// A deadline no clock reaches: an unbounded turn's.
const UNBOUNDED: u64 = u64::MAX;

/// A turn's figures: what [`Pace`] keeps, and lends a drive.
#[derive(Clone, Copy, Debug)]
struct Turn {
  /// The deadline in the clock's nanoseconds; [`UNBOUNDED`] for none.
  deadline: u64,
  /// Whether the turn has not yet run a unit: its first always runs.
  fresh: bool,
  /// Whether the turn began at its owner's mark ([`Pace::turn_from`]): its first ask measures the opening.
  opened: bool,
  /// The owner's clock, nanoseconds.
  clock: fn() -> u64,
  /// The units run, every turn.
  units: u64,
  /// When the last unit was asked for, on the clock.
  asked_at: u64,
  /// The longest unit measured, nanoseconds: from one ask to the next in the same turn.
  longest_unit: u64,
  /// The last turn's opening, nanoseconds: from its owner's mark to its first ask.
  opening: u64,
  /// The bytes a unit may copy or hash this turn ([`Slices`]).
  slices: Slices,
}

/// The bytes one unit may move this turn, its owner's derived slices (R3: the merge engine measures no machine, so its
/// owner gives them, as it gives the clock): what a unit copies or zeroes (the owner's copy slice, at its measured
/// memcpy rate) and what it hashes (its hash slice, at its measured BLAKE3 rate). Unbounded, a unit is the whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slices {
  /// The bytes a unit copies or zeroes.
  pub copy_bytes: u64,
  /// The bytes a unit hashes.
  pub hash_bytes: u64,
}

impl Slices {
  /// No bound: a unit moves the whole.
  pub const WHOLE: Slices = Slices {
    copy_bytes: u64::MAX,
    hash_bytes: u64::MAX,
  };
}

/// The clock of an unbounded turn, never read.
fn never() -> u64 {
  0
}

impl Turn {
  /// A turn that never ends and reads no clock.
  const UNBOUNDED: Turn = Turn {
    deadline: UNBOUNDED,
    fresh: true,
    opened: false,
    clock: never,
    units: 0,
    asked_at: 0,
    longest_unit: 0,
    opening: 0,
    slices: Slices::WHOLE,
  };

  /// Whether another unit runs this turn ([`Pace::more`]).
  fn more(&mut self) -> bool {
    let (fresh, opened) = (self.fresh, self.opened);
    self.fresh = false;
    self.opened = false;
    let granted = if self.deadline == UNBOUNDED {
      true
    } else {
      let now = (self.clock)();
      let since = now.saturating_sub(self.asked_at);
      if opened {
        self.opening = since;
      } else if !fresh {
        self.longest_unit = self.longest_unit.max(since);
      }
      self.asked_at = now;
      fresh || opened || now < self.deadline
    };
    if granted {
      self.units = self.units.saturating_add(1);
    }
    granted
  }
}

thread_local! {
  /// The turn lent to the composition being driven on this thread ([`Pace::drive`]); none outside a drive.
  static LENT: Cell<Option<Turn>> = const { Cell::new(None) };
}

/// A build's turns (module docs): its deadline, the owner's clock, and what its units measured.
#[derive(Debug)]
pub struct Pace {
  /// The turn's figures.
  turn: Cell<Turn>,
}

impl Pace {
  /// A pace that never ends a turn and reads no clock.
  pub fn unbounded() -> Pace {
    Pace {
      turn: Cell::new(Turn::UNBOUNDED),
    }
  }

  /// A pace read by `clock` (nanoseconds), unbounded until [`Pace::turn`] gives a deadline.
  pub fn new(clock: fn() -> u64) -> Pace {
    Pace {
      turn: Cell::new(Turn {
        clock,
        ..Turn::UNBOUNDED
      }),
    }
  }

  /// The pace's figures changed by `change`.
  fn with<T>(&self, change: impl FnOnce(&mut Turn) -> T) -> T {
    let mut turn = self.turn.get();
    let answer = change(&mut turn);
    self.turn.set(turn);
    answer
  }

  /// Begins a turn that ends once `clock` passes `deadline_ns`: its first unit runs whatever the clock says.
  pub fn turn(&self, deadline_ns: u64) {
    self.with(|turn| {
      turn.deadline = deadline_ns;
      turn.fresh = true;
      turn.opened = false;
    });
  }

  /// [`Pace::turn`] for a turn its owner began at `began_ns` on the clock: what the owner did before its first ask is the
  /// turn's opening ([`Pace::opening_ns`]), the units between its asks ([`Pace::longest_unit_ns`]), and what follows
  /// its last ask ([`Pace::asked_at_ns`]) its owner's again.
  pub fn turn_from(&self, began_ns: u64, (deadline_ns, slices): (u64, Slices)) {
    self.with(|turn| {
      turn.slices = slices;
      turn.deadline = deadline_ns;
      turn.fresh = false;
      turn.opened = true;
      turn.asked_at = began_ns;
    });
  }

  /// Whether another unit runs this turn: the turn's first, or any before the deadline. A unit granted is counted.
  pub fn more(&self) -> bool {
    self.with(Turn::more)
  }

  /// One turn of `work`, which ticks this pace's turn ([`Ticks::tick`]): its output once done, else pending until the
  /// next turn drives it again. Nothing wakes it: its owner drives it each turn.
  pub fn drive<F: Future + ?Sized>(&self, work: Pin<&mut F>) -> Poll<F::Output> {
    let before = LENT.with(|lent| lent.replace(Some(self.turn.get())));
    let polled = poll(work);
    let after = LENT.with(|lent| lent.replace(before));
    if let Some(turn) = after {
      self.turn.set(turn);
    }
    polled
  }

  /// The bytes a unit may copy or hash this turn ([`Slices`]): the whole until a turn gives its owner's.
  pub fn slices(&self) -> Slices {
    self.turn.get().slices
  }

  /// The units granted so far, every turn.
  pub fn units(&self) -> u64 {
    self.turn.get().units
  }

  /// The last turn's opening on the clock, nanoseconds: what its owner did before its first ask ([`Pace::turn_from`]).
  pub fn opening_ns(&self) -> u64 {
    self.turn.get().opening
  }

  /// The longest unit measured on the clock, nanoseconds (none for an unbounded pace, which reads no clock): what a
  /// turn may pass its deadline by.
  pub fn longest_unit_ns(&self) -> u64 {
    self.turn.get().longest_unit
  }

  /// When the turn last asked for a unit, on the clock: what it did after is its owner's, not a unit's (a turn of its
  /// owner is its opening, its units and what follows this).
  pub fn asked_at_ns(&self) -> u64 {
    self.turn.get().asked_at
  }
}

/// The permission to run a unit of a composition being driven ([`Pace::drive`]): ready while the lent turn grants one,
/// pending once it is spent; always ready outside a drive. A handle with no state of its own.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ticks;

impl Ticks {
  /// A unit's permission.
  pub fn tick(&self) -> Tick {
    Tick
  }
}

/// [`Ticks::tick`]'s future.
#[derive(Debug)]
pub struct Tick;

impl Future for Tick {
  type Output = ();

  fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
    let granted = LENT.with(|lent| match lent.get() {
      Some(mut turn) => {
        let granted = turn.more();
        lent.set(Some(turn));
        granted
      }
      None => true,
    });
    if granted {
      Poll::Ready(())
    } else {
      Poll::Pending
    }
  }
}

/// One poll of `work` with no waker: its owner polls it again.
fn poll<F: Future + ?Sized>(work: Pin<&mut F>) -> Poll<F::Output> {
  work.poll(&mut Context::from_waker(Waker::noop()))
}

/// `work` run to its end in one call, no turn lent, so it never pends: what a caller that needs no turns does. Inside
/// a drive the drive's turn is set aside for it and given back after.
pub fn finish<F: Future>(work: F) -> F::Output {
  let set_aside = LENT.with(|lent| lent.replace(None));
  let mut work = core::pin::pin!(work);
  let done = loop {
    if let Poll::Ready(done) = poll(work.as_mut()) {
      break done;
    }
  };
  LENT.with(|lent| lent.set(set_aside));
  done
}

/// `items` sorted by `order`, stably, a unit an item moved (A-752): a bottom-up merge of the input's own runs, so no
/// unit is the whole sort. The first pass cuts the items into their maximal non-decreasing runs, an item a unit, so the
/// runs' length is the data's own (an input in order is one run, merged no further), never a tuned number (R3). Equal
/// items keep their order, as `slice::sort_by` keeps it.
pub async fn sort_by<T>(
  items: Vec<T>,
  ticks: &Ticks,
  mut order: impl FnMut(&T, &T) -> core::cmp::Ordering,
) -> Vec<T> {
  let mut runs: std::collections::VecDeque<Vec<T>> = std::collections::VecDeque::new();
  let mut run: Vec<T> = Vec::new();
  for item in items {
    ticks.tick().await;
    if run
      .last()
      .is_some_and(|last| order(&item, last) == core::cmp::Ordering::Less)
    {
      runs.push_back(core::mem::take(&mut run));
    }
    run.push(item);
  }
  if !run.is_empty() {
    runs.push_back(run);
  }
  while runs.len() > 1 {
    let mut merged = std::collections::VecDeque::with_capacity(runs.len().div_ceil(2));
    while let Some(left) = runs.pop_front() {
      let Some(right) = runs.pop_front() else {
        merged.push_back(left);
        break;
      };
      merged.push_back(merge(left, right, ticks, &mut order).await);
    }
    runs = merged;
  }
  runs.pop_front().unwrap_or_default()
}

/// Two sorted runs merged, a unit an item, the left's first among equals.
async fn merge<T>(
  left: Vec<T>,
  right: Vec<T>,
  ticks: &Ticks,
  order: &mut impl FnMut(&T, &T) -> core::cmp::Ordering,
) -> Vec<T> {
  let mut out = Vec::with_capacity(left.len().saturating_add(right.len()));
  let (mut left, mut right) = (left.into_iter().peekable(), right.into_iter().peekable());
  loop {
    ticks.tick().await;
    let take_right = match (left.peek(), right.peek()) {
      (Some(a), Some(b)) => order(b, a) == core::cmp::Ordering::Less,
      (Some(_), None) => false,
      (None, Some(_)) => true,
      (None, None) => break,
    };
    let next = if take_right {
      right.next()
    } else {
      left.next()
    };
    out.extend(next);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-752. Do: sort shuffled pairs by their first half a unit at a time, driven over turns of a few units each. Expect:
  /// as `sort_by` sorts them (stably), over many turns; and the same in one call outside a drive.
  #[test]
  fn a_paced_sort_is_the_stable_sort_over_many_turns() {
    use core::sync::atomic::{AtomicU64, Ordering};
    static NOW: AtomicU64 = AtomicU64::new(0);
    fn clock() -> u64 {
      NOW.fetch_add(1, Ordering::Relaxed)
    }
    let items: Vec<(u32, u32)> = (0..1000u32).map(|i| ((i * 7919) % 97, i)).collect();
    let mut expected = items.clone();
    expected.sort_by_key(|pair| pair.0);
    let pace = Pace::new(clock);
    let mut work = core::pin::pin!(sort_by(items.clone(), &Ticks, |a, b| a.0.cmp(&b.0)));
    let mut turns = 0u32;
    let sorted = loop {
      turns += 1;
      pace.turn(NOW.load(Ordering::Relaxed) + 8);
      if let Poll::Ready(sorted) = pace.drive(work.as_mut()) {
        break sorted;
      }
    };
    assert_eq!(sorted, expected);
    assert!(turns > 100, "{turns} turns");
    assert_eq!(
      finish(sort_by(items, &Ticks, |a, b| a.0.cmp(&b.0))),
      expected
    );
  }
}
