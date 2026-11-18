//! Room in a full control channel, and the wake that tells a waiting shard of it (§4.3 "admission limit": a spawn a
//! full control channel refuses is "backpressure, never a drop", kept by its sender and sent again once the channel
//! has room).
//!
//! A shard's control channel is bounded at its admission limit, and a sender that meets it full keeps its message (the
//! daemon's kept forwards, `slates_server::verbs::ask_room_for_kept_forwards`). Until 2026-11-17 nothing told the
//! sender when the channel had room again. While it kept a forward, the daemon's serve loop woke once per idle window
//! to try it again: the shard's wake estimate times the daemon's idle-window ratio of 100, which measured 0.3 to 21 ms
//! on an 18-CPU Mac under 36 spinning threads (2026-11-17, 30 runs). The estimate is the mean wake the shard measures,
//! with no cap, so a wake that costs 10 ms makes the window a second. The retry lagged the room by up to a window, and
//! the loop armed a timer every window while it waited
//! (`docs/bugs/2026-11-17-a-kept-forward-polled-for-room-and-its-test-timed-the-owners-queue.md`). Now the sender asks
//! to be told ([`crate::shard::ShardContext::want_room`]), and the drain that makes room tells it.
//!
//! **The protocol.** Two halves, each the store-buffering shape of the parking protocol ([`crate::parking`]).
//! - Asking. The sender records its waiting task on its own shard, marks its shard in the target's `room_wanted` set,
//!   fences `SeqCst`, and tries its send once more. The target, after a drain that took at least one message, fences
//!   `SeqCst` and looks at `room_wanted`. Each side writes, fences, then reads what the other side wrote, so whichever
//!   fence comes first in the total order, the later side sees the earlier side's write ([B: C++20 `[atomics.order]`,
//!   the fence–fence case]): either the sender's second try finds the room the drain made, or the drain finds the mark
//!   and tells the sender. A mark the drain finds after the second try already landed costs one spurious wake.
//! - Telling. The target marks itself in each asking shard's `room_given` set, fences `SeqCst`, publishes that shard's
//!   control flag, and kicks it if it is parked: a control message's own publication
//!   ([`crate::parking::ControlFlag`]), so the asking shard finds the mark at its next drain, or at its park's
//!   re-check, as it would find a message. That drain wakes every task of the shard waiting for room on the target.
//!   The fence answers the drain's own: a drain that stops at its batch bound re-arms the flag with a plain store,
//!   which ends the publication's release sequence, so the take that reads the re-arm acquires nothing from the
//!   teller. The re-arming drain fences after it, so whichever fence comes first, the shard's next look finds the
//!   mark ([B: C++20 `[atomics.order]` p4]: the mark before one fence, the re-arm before the other, and each side's
//!   coherence order forces the opposite fence order). A message needs no such fence, since the channel claims its
//!   slot with a `SeqCst` operation.
//!
//! An exiting shard tells every shard waiting on it, since it will never drain again (`registry::note_exited`); a send
//! to it is then refused `ShardGone`, and an ask after its exit is refused at once.
//!
//! **The cost.** The sender pays only on a refusal: a map insert on its own shard, two read-modify-writes and a fence.
//! The target pays one fence and one load per drain that took a message, and nothing when no message came. Telling is
//! a fence, one control publication and, for a parked shard, one kick.
//!
//! **The bound.** A set holds one bit per shard id the registry can name ([`MAX_SHARDS`]), and the tasks a shard
//! records as waiting are each `(target, task slot)` at most once, so the record never exceeds the arena's slots for
//! each shard asked.
//!
//! Under `--cfg loom` the models below drive exactly this code: a sender refused for a full channel against a target
//! draining it, a target telling a shard that parks, and a target telling a shard that re-arms its flag; a lost wake
//! leaves a thread blocked with no runnable thread left, which loom reports as a deadlock.

#[cfg(loom)]
use loom::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(loom))]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::registry::MAX_SHARDS;

/// Format: shard ids per word of a set (the bits of a `u64`).
const IDS_PER_WORD: usize = 64;
/// Derived: the words that hold one bit per shard id the registry can name, `MAX_SHARDS / 64` rounded up (16 for
/// 1,024 shard ids). The summary word has a bit per word, so this must be at most 64, which it is for every
/// [`MAX_SHARDS`] up to 4,096.
const WORDS: usize = MAX_SHARDS.div_ceil(IDS_PER_WORD);

/// A set of shard ids that any thread adds to and one thread takes whole (a shard's `room_wanted` and `room_given`,
/// above). Adding sets the id's bit, then the summary's bit for that bit's word; taking clears the summary, then each
/// word the summary named, so an id added meanwhile is either taken now or left marked in the summary for the next
/// take.
#[derive(Debug)]
pub struct ShardSet {
  /// One bit per word of `words` that may hold an id: what a taker looks at first, so an empty set costs one load.
  summary: AtomicU64,
  /// One bit per shard id.
  words: [AtomicU64; WORDS],
}

impl Default for ShardSet {
  fn default() -> Self {
    Self::new()
  }
}

impl ShardSet {
  /// An empty set.
  pub fn new() -> Self {
    Self {
      summary: AtomicU64::new(0),
      words: std::array::from_fn(|_| AtomicU64::new(0)),
    }
  }

  /// The word index and the bit within it of `shard`'s id; `None` past the registry's ids.
  fn place(shard: u16) -> Option<(usize, u64)> {
    let id = usize::from(shard);
    let word = id.checked_div(IDS_PER_WORD)?;
    let bit = u32::try_from(id.checked_rem(IDS_PER_WORD)?).ok()?;
    (word < WORDS).then(|| (word, 1u64.checked_shl(bit).unwrap_or(0)))
  }

  /// Adds `shard`: its bit, then its word's summary bit. Both are read-modify-writes, so a taker whose summary swap
  /// reads this summary bit, or any later change of the summary, synchronizes with it and finds the id's bit. An id past
  /// the registry's ids is ignored: no shard has it.
  pub fn insert(&self, shard: u16) {
    let Some((index, bit)) = Self::place(shard) else {
      return;
    };
    let (Some(word), Some(word_bit)) = (
      self.words.get(index),
      u32::try_from(index)
        .ok()
        .and_then(|index| 1u64.checked_shl(index)),
    ) else {
      return;
    };
    word.fetch_or(bit, Ordering::Release);
    self.summary.fetch_or(word_bit, Ordering::AcqRel);
  }

  /// Whether the set may hold any id: one load, for a caller deciding whether to take it. A caller that must not miss
  /// an id added by another thread fences `SeqCst` before it looks (the module's protocol).
  pub fn maybe_any(&self) -> bool {
    self.summary.load(Ordering::Relaxed) != 0
  }

  /// Takes every id out of the set, calling `each` with it, lowest first.
  pub fn take_each(&self, mut each: impl FnMut(u16)) {
    for index in set_bits(self.summary.swap(0, Ordering::AcqRel)) {
      let Some(word) = usize::try_from(index)
        .ok()
        .and_then(|index| self.words.get(index))
      else {
        continue;
      };
      set_bits(word.swap(0, Ordering::AcqRel))
        .filter_map(|bit| id_of(index, bit))
        .for_each(&mut each);
    }
  }
}

/// The positions of `bits`' set bits, lowest first.
fn set_bits(mut bits: u64) -> impl Iterator<Item = u32> {
  std::iter::from_fn(move || {
    (bits != 0).then(|| {
      let bit = bits.trailing_zeros();
      bits &= bits.wrapping_sub(1);
      bit
    })
  })
}

/// The shard id of bit `bit` of word `index`; `None` past a shard id's range.
fn id_of(index: u32, bit: u32) -> Option<u16> {
  let per_word = u32::try_from(IDS_PER_WORD).ok()?;
  u16::try_from(index.checked_mul(per_word)?.checked_add(bit)?).ok()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Takes `set` whole, the ids in the order given.
  fn taken(set: &ShardSet) -> Vec<u16> {
    let mut ids = Vec::new();
    set.take_each(|shard| ids.push(shard));
    ids
  }

  /// §4.3, the room wake. Do: add ids in the first word, a later word and the registry's last id, one of them twice,
  /// then take the set twice. Expect: the first take finds each id once, in order, and empties the set; the second
  /// finds nothing; an id past the registry's ids is never held.
  #[test]
  fn a_set_gives_each_id_added_once_and_is_empty_after() {
    let set = ShardSet::new();
    assert!(!set.maybe_any(), "a new set is empty");
    let last = u16::try_from(MAX_SHARDS - 1).unwrap();
    for shard in [3, 64, 3, last, 0] {
      set.insert(shard);
    }
    set.insert(u16::try_from(MAX_SHARDS).unwrap_or(u16::MAX));
    assert!(set.maybe_any());
    assert_eq!(taken(&set), vec![0, 3, 64, last]);
    assert!(!set.maybe_any(), "a take empties the set");
    assert_eq!(taken(&set), Vec::<u16>::new());
  }

  /// §4.3, the room wake. Do: four threads each add their own sixteen ids while this thread takes the set over and
  /// over, then take once more after they finish. Expect: every id added is taken exactly once across all the takes.
  #[test]
  #[cfg_attr(miri, ignore)] // four threads racing a taker: thousands of steps, minutes under Miri
  fn ids_added_while_the_set_is_taken_are_each_taken_once() {
    /// Shape: the adding threads.
    const THREADS: u16 = 4;
    /// Shape: the ids each thread adds, one in each of sixteen words.
    const PER_THREAD: u16 = 16;
    /// Thread `thread`'s `n`th id: word `n`, a bit of its own in it.
    fn id(thread: u16, n: u16) -> u16 {
      n * 64 + thread * 16
    }
    let set = ShardSet::new();
    let mut seen = vec![0u32; MAX_SHARDS];
    std::thread::scope(|scope| {
      for thread in 0..THREADS {
        let set = &set;
        scope.spawn(move || {
          for n in 0..PER_THREAD {
            set.insert(id(thread, n));
          }
        });
      }
      for _ in 0..1_000 {
        set.take_each(|shard| seen[usize::from(shard)] += 1);
      }
    });
    set.take_each(|shard| seen[usize::from(shard)] += 1);
    let mut expected = vec![0u32; MAX_SHARDS];
    for thread in 0..THREADS {
      for n in 0..PER_THREAD {
        expected[usize::from(id(thread, n))] = 1;
      }
    }
    assert_eq!(seen, expected, "each id added was taken exactly once");
  }
}

// Two attributes rather than `all(test, loom)`: clippy's test-context rule (unwrap allowed in tests) recognizes only a
// bare `cfg(test)`, and an unwrap here is a failed model, as it should be.
#[cfg(test)]
#[cfg(loom)]
mod loom_tests {
  use std::sync::atomic::{AtomicU64 as StdAtomicU64, Ordering as StdOrdering};

  // loom's `Notify` stands in for the room wake's delivery and the driver's kick: test scaffolding under `cfg(loom)`
  // only (D-8 exception 3, a test harness); the two owners are each model's two threads.
  use loom::sync::Notify;
  use loom::sync::atomic::fence;
  use slates_mem::{MpscRing, loom_bounds};

  use super::*;
  use crate::parking::{ControlFlag, Parked, Parking};

  /// Shape: the target's control channel in the model, the smallest ring: full at two messages.
  const CAPACITY: usize = 2;
  /// Format: a message in the model's channel.
  const MESSAGE: u64 = 0x5a;
  /// Format: the asking shard's id in the model.
  const SENDER: u16 = 1;
  /// Format: the target's id in the model.
  const TARGET: u16 = 2;

  /// Interleavings in which the sender waited to be told, across every explored one.
  static TOLD: StdAtomicU64 = StdAtomicU64::new(0);
  /// Interleavings in which the sender's second try found the room, across every explored one.
  static RETRIED: StdAtomicU64 = StdAtomicU64::new(0);
  /// Interleavings in which the asking shard parked before it was told, across every explored one.
  static PARKED: StdAtomicU64 = StdAtomicU64::new(0);
  /// Interleavings in which the asking shard learned of the room only after it re-armed its flag, across every explored
  /// one.
  static AFTER_REARM: StdAtomicU64 = StdAtomicU64::new(0);

  /// §4.3, the room wake's asking half (`ShardContext::want_room` against `ShardContext::drain_control`). Do: fill the
  /// target's channel and have one send refused; the target drains one message, fences, and tells whoever its set
  /// names; the sender marks itself, fences, tries again, and waits to be told only if refused again, asking again
  /// after any wake. Expect: in every interleaving the sender's message lands (a lost wake is the sender blocked with
  /// nothing left to run: loom's deadlock), and both endings happen in some interleaving, so neither half is vacuous.
  /// The tell is loom's `Notify`, a release/acquire point as the real tell's control flag is
  /// ([`a_shard_told_of_room_learns_it_parked_or_not`] models that half).
  #[test]
  fn a_sender_refused_for_a_full_channel_finds_the_room_or_is_told_of_it() {
    loom_bounds::explore("room: a refused sender against a draining target", || {
      let channel: &'static MpscRing = Box::leak(Box::new(MpscRing::new(CAPACITY).unwrap()));
      let wanted: &'static ShardSet = Box::leak(Box::new(ShardSet::new()));
      let told: &'static Notify = Box::leak(Box::new(Notify::new()));
      for _ in 0..CAPACITY {
        channel.push(MESSAGE).unwrap();
      }
      // The send that is refused: before the target runs, so the channel is full when it is made.
      assert!(channel.push(MESSAGE).is_err(), "the channel starts full");
      let consumer = channel.consumer().unwrap();
      let target = loom::thread::spawn(move || {
        // `drain_control`'s order: take a message, fence, look at who asked, tell each.
        assert!(consumer.pop().is_some());
        fence(Ordering::SeqCst);
        if wanted.maybe_any() {
          wanted.take_each(|shard| {
            assert_eq!(shard, SENDER);
            told.notify();
          });
        }
      });
      // The sender's order (the daemon's serve loop): ask, fence, try again, and wait only if refused; after any wake,
      // ask and try again. `Notify::wait` may return spuriously, as a task's poll may come for another reason.
      let mut waits = 0u32;
      loop {
        wanted.insert(SENDER);
        fence(Ordering::SeqCst);
        if channel.push(MESSAGE).is_ok() {
          break;
        }
        told.wait();
        waits += 1;
      }
      if waits == 0 {
        RETRIED.fetch_add(1, StdOrdering::Relaxed);
      } else {
        TOLD.fetch_add(1, StdOrdering::Relaxed);
      }
      target.join().unwrap();
    });
    assert!(
      TOLD.load(StdOrdering::Relaxed) > 0,
      "some interleaving told the sender"
    );
    assert!(
      RETRIED.load(StdOrdering::Relaxed) > 0,
      "some interleaving found the room on the second try"
    );
  }

  /// §4.3, the room wake's telling half (`registry::give_room` against `ShardContext::drain_control` and `park`). Do:
  /// the target marks itself in the asking shard's `room_given`, fences, publishes its control flag and kicks it if
  /// parked; the
  /// asking shard's loop takes the flag and the set, and parks unless the flag is pending. Expect: in every
  /// interleaving the asking shard learns of the room (a lost one is the shard parked for good: loom's deadlock), and
  /// some interleaving made it park first.
  #[test]
  fn a_shard_told_of_room_learns_it_parked_or_not() {
    loom_bounds::explore("room: a telling target against a parking shard", || {
      let given: &'static ShardSet = Box::leak(Box::new(ShardSet::new()));
      let flag: &'static ControlFlag = Box::leak(Box::new(ControlFlag::new()));
      let parking: &'static Parking = Box::leak(Box::new(Parking::new()));
      let kick: &'static Notify = Box::leak(Box::new(Notify::new()));
      let target = loom::thread::spawn(move || {
        // `give_room`'s order: the mark, the fence, the control flag, then a kick if the shard is parked.
        given.insert(TARGET);
        fence(Ordering::SeqCst);
        flag.publish();
        parking.kick_if_parked(|| kick.notify());
      });
      let mut learned = false;
      let mut parked = false;
      while !learned {
        // `drain_control`'s order: take the flag, then the set.
        if flag.take() && given.maybe_any() {
          given.take_each(|shard| {
            assert_eq!(shard, TARGET);
            learned = true;
          });
        }
        if learned {
          break;
        }
        if matches!(
          parking.park_unless_pending(|| flag.is_pending(), || kick.wait()),
          Parked::Waited(_)
        ) {
          parked = true;
        }
      }
      target.join().unwrap();
      if parked {
        PARKED.fetch_add(1, StdOrdering::Relaxed);
      }
    });
    assert!(
      PARKED.load(StdOrdering::Relaxed) > 0,
      "some interleaving made the shard park first"
    );
  }

  /// §4.3, the room wake's telling half against a drain that stops at its batch bound (`registry::give_room` against
  /// `ShardContext::drain_control`'s re-arm). Do: with a message pending, the asking shard takes its flag, looks at
  /// `room_given`, drains a whole batch, re-arms the flag with a plain store and fences, as `drain_control` does; the
  /// target meanwhile marks itself, fences, publishes and kicks if parked; then the shard runs its later steps (take,
  /// look, park unless pending). Expect: in every interleaving the shard learns of the room, and some interleaving
  /// had it learn only after its re-arm. Without the fence in `give_room`, a publication between the take and the
  /// re-arm leaves the re-arm's store as the value the next take reads, which does not synchronize with the mark, and
  /// the C++ model lets the shard look past the mark and park for good. Loom passed this model without that fence too
  /// (81 interleavings, 2026-11-18): it does not reach that execution, so the fence rests on the standard's fence rules
  /// (the module doc) and this model records the re-arm's path.
  #[test]
  fn a_shard_told_of_room_while_it_rearms_its_flag_learns_it() {
    loom_bounds::explore(
      "room: a telling target against a shard re-arming its flag",
      || {
        let given: &'static ShardSet = Box::leak(Box::new(ShardSet::new()));
        let flag: &'static ControlFlag = Box::leak(Box::new(ControlFlag::new()));
        let parking: &'static Parking = Box::leak(Box::new(Parking::new()));
        let kick: &'static Notify = Box::leak(Box::new(Notify::new()));
        // A message pending before either thread runs: the batch the shard's first step drains.
        flag.publish();
        let target = loom::thread::spawn(move || {
          given.insert(TARGET);
          fence(Ordering::SeqCst);
          flag.publish();
          parking.kick_if_parked(|| kick.notify());
        });
        let mut learned = false;
        // The step that drains a whole batch: the take, the look at `room_given`, the re-arm, the drain's fence.
        if flag.take() && given.maybe_any() {
          given.take_each(|_| learned = true);
        }
        flag.rearm();
        fence(Ordering::SeqCst);
        let after_rearm = !learned;
        while !learned {
          if flag.take() && given.maybe_any() {
            given.take_each(|_| learned = true);
          }
          if learned {
            break;
          }
          let _ = parking.park_unless_pending(|| flag.is_pending(), || kick.wait());
        }
        target.join().unwrap();
        if after_rearm {
          AFTER_REARM.fetch_add(1, StdOrdering::Relaxed);
        }
      },
    );
    assert!(
      AFTER_REARM.load(StdOrdering::Relaxed) > 0,
      "some interleaving had the shard learn of the room only after its re-arm"
    );
  }
}
