//! A hierarchical timing wheel [A: Varghese & Lauck, "Hashed and hierarchical timing wheels",
//! SOSP'87]: insertion, cancellation and expiry in O(1) amortized, with entries cascading to a
//! finer level as their deadline approaches.
//!
//! The tick is derived from the profile (`timer_tick_ns`: no shorter than a wake, no shorter than
//! the timer floor). Each level has 64 slots and there are six levels, so the wheel spans 2^36
//! ticks; a deadline beyond that is clamped to the horizon and re-armed when it cascades (tokio's
//! and Kafka's rule). Entries live in a slab and are linked through it, so arming a timer
//! allocates nothing once the slab's segments exist.

use slates_mem::{Handle, Slab};

use crate::error::RtError;

/// Format: slots per level and levels: 64^6 = 2^36 ticks of range.
pub const SLOTS_PER_LEVEL: usize = 64;
/// Format: levels.
pub const LEVELS: usize = 6;
/// Format: log2 of the slots per level.
const SLOT_BITS: u32 = 6;

const NONE: u32 = u32::MAX;

/// A timer entry.
#[derive(Debug)]
pub struct Entry {
  /// The deadline in ticks.
  pub deadline: u64,
  /// The packed waker word to wake when the deadline passes.
  pub word: u64,
  next: u32,
  prev: u32,
  level: u8,
  slot: u16,
}

/// A handle to an armed timer.
pub type TimerId = Handle<Entry>;

/// The wheel.
#[derive(Debug)]
pub struct Wheel {
  tick_ns: u64,
  now_tick: u64,
  entries: Slab<Entry>,
  heads: Vec<u32>,
  armed: usize,
  earliest: Option<u64>,
  earliest_exact: bool,
}

impl Wheel {
  /// A wheel with `tick_ns` per tick, room for `max_timers` timers, starting at `now_ns`.
  pub fn new(tick_ns: u64, max_timers: usize, now_ns: u64) -> Self {
    let tick_ns = tick_ns.max(1);
    let mut entries = Slab::new(SLOTS_PER_LEVEL, max_timers);
    entries.reserve_segments(max_timers.div_ceil(SLOTS_PER_LEVEL));
    Self {
      tick_ns,
      now_tick: now_ns / tick_ns,
      entries,
      heads: vec![NONE; SLOTS_PER_LEVEL * LEVELS],
      armed: 0,
      earliest: None,
      earliest_exact: true,
    }
  }

  /// Nanoseconds per tick.
  pub const fn tick_ns(&self) -> u64 {
    self.tick_ns
  }

  /// Armed timers.
  pub const fn armed(&self) -> usize {
    self.armed
  }

  /// Arms a timer to wake `word` at `deadline_ns`.
  pub fn insert(&mut self, deadline_ns: u64, word: u64) -> Result<TimerId, RtError> {
    let deadline = (deadline_ns.div_ceil(self.tick_ns)).max(self.now_tick + 1);
    let id = self.entries.insert(Entry {
      deadline,
      word,
      next: NONE,
      prev: NONE,
      level: 0,
      slot: 0,
    })?;
    self.link(id.index(), deadline);
    self.armed += 1;
    self.earliest = Some(self.earliest.map_or(deadline, |e| e.min(deadline)));
    Ok(id)
  }

  /// Disarms a timer; a stale id is refused. The arena removal comes **first** so the id's generation is
  /// validated before any list is touched: a stale id (its slot already fired and was reused by a *later*
  /// timer) is refused here and never reaches the unlink — unlinking by bare index would otherwise splice
  /// out whichever timer now occupies the slot, orphaning a live timer so it never fires (the SWIM probe's
  /// deadline stranded exactly this way, hanging a survivor's death detection under a fleet's own load).
  /// The removed entry carries its own recorded position, so the unlink needs no second read of it.
  pub fn cancel(&mut self, id: TimerId) -> Result<(), RtError> {
    let removed = self.entries.remove(id)?;
    let head = usize::from(removed.level) * SLOTS_PER_LEVEL + usize::from(removed.slot);
    self.unlink_at(removed.next, removed.prev, head);
    self.armed -= 1;
    if self.earliest == Some(removed.deadline) {
      // The earliest may have been this one; the next query rescans.
      self.earliest_exact = false;
    }
    Ok(())
  }

  /// The next deadline in nanoseconds, if any timer is armed. Exact after an insert; a cancel of
  /// the earliest entry or a firing tick marks it for one rescan.
  pub fn next_deadline_ns(&mut self) -> Option<u64> {
    if self.armed == 0 {
      self.earliest = None;
      self.earliest_exact = true;
      return None;
    }
    if !self.earliest_exact {
      let mut best: Option<u64> = None;
      for (_, entry) in self.entries.iter() {
        best = Some(best.map_or(entry.deadline, |b| b.min(entry.deadline)));
      }
      self.earliest = best;
      self.earliest_exact = true;
    }
    self
      .earliest
      .map(|ticks| ticks.saturating_mul(self.tick_ns))
  }

  /// Advances to `now_ns`, collecting the words of every expired timer into `fired` in deadline
  /// order within a tick.
  pub fn advance(&mut self, now_ns: u64, fired: &mut Vec<u64>) {
    let target = now_ns / self.tick_ns;
    let before = fired.len();
    while self.now_tick < target {
      // Ticks before the earliest deadline hold nothing at level 0, but a higher level cascades
      // at every multiple of the slot count, so a skip stops just before the next such boundary.
      if let Some(earliest) = self.earliest
        && self.earliest_exact
        && earliest > self.now_tick + 1
      {
        let boundary = (self.now_tick | (SLOTS_PER_LEVEL as u64 - 1)) + 1;
        let jump = (earliest - 1).min(target - 1).min(boundary - 1);
        if jump > self.now_tick {
          self.now_tick = jump;
        }
      }
      self.now_tick += 1;
      self.expire_tick(fired);
    }
    if fired.len() > before {
      self.earliest_exact = false;
    }
  }

  fn expire_tick(&mut self, fired: &mut Vec<u64>) {
    let tick = self.now_tick;
    // Level 0 slot for this tick fires; a higher level's slot that this tick enters cascades.
    for level in 0..LEVELS {
      let shift = SLOT_BITS * u32::try_from(level).unwrap_or(0);
      if level > 0 && tick & ((1u64 << shift) - 1) != 0 {
        break;
      }
      let slot = usize::try_from((tick >> shift) & (SLOTS_PER_LEVEL as u64 - 1)).unwrap_or(0);
      let head = level * SLOTS_PER_LEVEL + slot;
      let mut index = self.heads[head];
      self.heads[head] = NONE;
      while index != NONE {
        let (next, deadline, word) = {
          let Ok(entry) = self
            .entries
            .get(Handle::from_raw(index, self.generation_of(index)))
          else {
            break;
          };
          (entry.next, entry.deadline, entry.word)
        };
        if deadline <= tick || level == 0 {
          fired.push(word);
          let _ = self
            .entries
            .remove(Handle::from_raw(index, self.generation_of(index)));
          self.armed -= 1;
        } else {
          self.link(index, deadline);
        }
        index = next;
      }
    }
  }

  fn generation_of(&self, index: u32) -> u32 {
    self.entries.generation_at(index).unwrap_or(0)
  }

  fn level_and_slot(&self, deadline: u64) -> (usize, usize) {
    let delta = deadline.saturating_sub(self.now_tick).max(1);
    let level = (usize::try_from((u64::BITS - 1 - delta.leading_zeros()) / SLOT_BITS).unwrap_or(0))
      .min(LEVELS - 1);
    let shift = SLOT_BITS * u32::try_from(level).unwrap_or(0);
    let slot = usize::try_from((deadline >> shift) & (SLOTS_PER_LEVEL as u64 - 1)).unwrap_or(0);
    (level, slot)
  }

  fn link(&mut self, index: u32, deadline: u64) {
    let (level, slot) = self.level_and_slot(deadline);
    let head = level * SLOTS_PER_LEVEL + slot;
    let old = self.heads[head];
    if let Ok(entry) = self
      .entries
      .get_mut(Handle::from_raw(index, self.generation_of(index)))
    {
      entry.next = old;
      entry.prev = NONE;
      entry.level = u8::try_from(level).unwrap_or(0);
      entry.slot = u16::try_from(slot).unwrap_or(0);
    }
    if old != NONE
      && let Ok(next) = self
        .entries
        .get_mut(Handle::from_raw(old, self.generation_of(old)))
    {
      next.prev = index;
    }
    self.heads[head] = index;
  }

  /// Splices an entry out of its doubly-linked slot list given the position it recorded — its `next`,
  /// `prev`, and the `head` of its `(level, slot)`. The entry itself is already gone from the arena (the
  /// caller removed it after validating its generation), so this only mends its former neighbours and the
  /// head pointer. Its neighbours are the entry's own list-mates, so they are live at their current
  /// generation.
  fn unlink_at(&mut self, next: u32, prev: u32, head: usize) {
    if prev == NONE {
      self.heads[head] = next;
    } else if let Ok(p) = self
      .entries
      .get_mut(Handle::from_raw(prev, self.generation_of(prev)))
    {
      p.next = next;
    }
    if next != NONE
      && let Ok(n) = self
        .entries
        .get_mut(Handle::from_raw(next, self.generation_of(next)))
    {
      n.prev = prev;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_machine::stats::Xorshift;

  #[test]
  fn timers_fire_in_deadline_order_within_one_tick_of_accuracy() {
    let tick = 1_000;
    let mut wheel = Wheel::new(tick, 20_000, 0);
    let mut rng = Xorshift::new(Xorshift::SEED);
    let mut expected: Vec<(u64, u64)> = Vec::new();
    for word in 0..10_000u64 {
      let deadline_ns = u64::try_from(rng.below(5_000_000)).unwrap() + 1;
      wheel.insert(deadline_ns, word).unwrap();
      expected.push((deadline_ns.div_ceil(tick), word));
    }
    assert_eq!(wheel.armed(), 10_000);
    let mut fired = Vec::new();
    let mut now = 0;
    let mut last_tick = 0;
    while wheel.armed() > 0 {
      now += tick;
      let before = fired.len();
      wheel.advance(now, &mut fired);
      for word in &fired[before..] {
        let (deadline_tick, _) = expected[usize::try_from(*word).unwrap()];
        assert!(deadline_tick <= now / tick, "timer {word} fired early");
        assert!(
          now / tick - deadline_tick <= 1,
          "timer {word} fired {} ticks late",
          now / tick - deadline_tick
        );
        assert!(deadline_tick >= last_tick, "out of order");
      }
      if fired.len() > before {
        last_tick = now / tick;
      }
    }
    assert_eq!(fired.len(), 10_000);
  }

  #[test]
  fn cancel_disarms_and_refuses_a_stale_id() {
    let mut wheel = Wheel::new(10, 8, 0);
    let a = wheel.insert(50, 1).unwrap();
    let b = wheel.insert(50, 2).unwrap();
    wheel.cancel(a).unwrap();
    assert!(matches!(wheel.cancel(a), Err(RtError::Mem(_))));
    assert_eq!(wheel.next_deadline_ns(), Some(50));
    let mut fired = Vec::new();
    wheel.advance(60, &mut fired);
    assert_eq!(fired, vec![2]);
    assert!(wheel.cancel(b).is_err());
    assert_eq!(wheel.next_deadline_ns(), None);
  }

  /// A stale `cancel` — one whose slot has already fired and been reused by a *later* timer — must be
  /// refused without disturbing the reused slot's live timer. Regression: `cancel` unlinked by bare index
  /// (at the slot's current generation) *before* validating the id's generation, so a stale cancel spliced
  /// the timer that had reused the slot out of its list — orphaning it in the arena, in no slot list, so
  /// it never fired. That is the runtime hazard that hung a fleet survivor's SWIM probe of a dead node
  /// (the probe's deadline timer, orphaned when a healthy probe's fired-timer slot was reused and its id
  /// then cancelled late). Here A fires and frees its slot, B reuses it, the stale cancel of A is refused,
  /// and B must still fire.
  #[test]
  fn a_stale_cancel_does_not_orphan_the_timer_that_reused_the_slot() {
    let mut wheel = Wheel::new(10, 4, 0);
    let a = wheel.insert(50, 1).unwrap();
    let mut fired = Vec::new();
    // Fire A, freeing its slot for reuse.
    wheel.advance(60, &mut fired);
    assert_eq!(fired, vec![1], "A fired, freeing its slot");
    fired.clear();
    // B reuses A's freed slot (same index, a new generation) — the reuse the bug needs.
    let b = wheel.insert(100, 2).unwrap();
    assert_eq!(a.index(), b.index(), "B reused A's slot");
    // The now-stale cancel of A is refused and must NOT unlink B.
    assert!(
      wheel.cancel(a).is_err(),
      "a stale cancel (fired-and-reused slot) is refused"
    );
    // B was not orphaned: it is still the earliest, and it still fires.
    assert_eq!(wheel.next_deadline_ns(), Some(100));
    wheel.advance(110, &mut fired);
    assert_eq!(
      fired,
      vec![2],
      "B still fires — the stale cancel did not orphan it"
    );
    let _ = b;
  }

  #[test]
  fn skipping_idle_ticks_never_misses_a_cascade() {
    let mut wheel = Wheel::new(1, 64, 0);
    let deadlines = [63, 64, 65, 127, 128, 4095, 4096, 4097, 262_144, 262_145];
    for (i, d) in deadlines.iter().enumerate() {
      wheel.insert(*d, u64::try_from(i).unwrap()).unwrap();
    }
    let mut fired = Vec::new();
    let mut now = 0;
    let mut order = Vec::new();
    while wheel.armed() > 0 {
      let next = wheel.next_deadline_ns().unwrap();
      now = now.max(next);
      let before = fired.len();
      wheel.advance(now, &mut fired);
      for w in &fired[before..] {
        let d = deadlines[usize::try_from(*w).unwrap()];
        assert_eq!(d, now, "timer {w} fired at {now}, deadline {d}");
        order.push(d);
      }
    }
    assert_eq!(order, deadlines.to_vec());
  }

  #[test]
  fn a_far_deadline_cascades_down_the_levels_and_fires_on_time() {
    let mut wheel = Wheel::new(1, 8, 0);
    let far = 64 * 64 * 3 + 7;
    wheel.insert(far, 42).unwrap();
    let mut fired = Vec::new();
    wheel.advance(far - 1, &mut fired);
    assert!(fired.is_empty());
    wheel.advance(far, &mut fired);
    assert_eq!(fired, vec![42]);
  }
}
