//! A set of object positions (A-614; §4.17.2 "The repository in git's terms", item 3: "kept per green as a compressed
//! bitmap over the repository's object positions"): sorted, disjoint runs `[start, end)`. Positions are given in
//! arrival order, so the objects one push brings new to the repository are one run, and a green's reached set is a few
//! runs however many objects it holds. These are the run containers of a compressed bitmap [A: Lemire, Ssi-Yan-Kai and
//! Kaser, "Consistently faster and smaller compressed bitmaps with Roaring", Software: Practice and Experience 46,
//! 2016], kept alone because this data is runs. A position is below `u64::MAX`, which the repository never gives.
//!
//! It is the crate's, not the store's: the negotiation's answer and the pack's plan carry the objects they send as
//! positions too (A-745: a fetch holds what it sends as runs, never an object at a time), and the negotiation names no
//! store.

/// A set of positions as runs (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Positions {
  /// Runs `[start, end)`, ascending, never touching or overlapping.
  runs: Vec<(u64, u64)>,
}

impl Positions {
  /// The empty set.
  pub fn new() -> Positions {
    Positions::default()
  }

  /// The set of `positions`, given in any order, repeats allowed.
  pub fn of(mut positions: Vec<u64>) -> Positions {
    positions.sort_unstable();
    positions.dedup();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for position in positions {
      let end = position.saturating_add(1);
      match runs.last_mut() {
        Some(last) if last.1 == position => last.1 = end,
        _ => runs.push((position, end)),
      }
    }
    Positions { runs }
  }

  /// The positions from `start` up to `end`, one run, made without listing them (A-665: the view
  /// starts from every position the repository gave); empty when `end` is not past `start`.
  pub fn span(start: u64, end: u64) -> Positions {
    if end <= start {
      return Positions::new();
    }
    Positions {
      runs: vec![(start, end)],
    }
  }

  /// Whether the set holds `position`.
  pub fn contains(&self, position: u64) -> bool {
    let after = self.runs.partition_point(|run| run.0 <= position);
    after
      .checked_sub(1)
      .and_then(|at| self.runs.get(at))
      .is_some_and(|run| position < run.1)
  }

  /// The union of this set and `other`.
  pub fn union(&self, other: &Positions) -> Positions {
    let mut all: Vec<(u64, u64)> =
      Vec::with_capacity(self.runs.len().saturating_add(other.runs.len()));
    all.extend_from_slice(&self.runs);
    all.extend_from_slice(&other.runs);
    all.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::with_capacity(all.len());
    for (start, end) in all {
      match runs.last_mut() {
        Some(last) if start <= last.1 => last.1 = last.1.max(end),
        _ => runs.push((start, end)),
      }
    }
    Positions { runs }
  }

  /// The positions held.
  pub fn len(&self) -> u64 {
    self.runs.iter().fold(0_u64, |count, run| {
      count.saturating_add(run.1.saturating_sub(run.0))
    })
  }

  /// Whether the set holds no position.
  pub fn is_empty(&self) -> bool {
    self.runs.is_empty()
  }

  /// The runs the set is kept as.
  pub fn runs(&self) -> usize {
    self.runs.len()
  }

  /// The runs `[start, end)`, ascending, as the set keeps them: what a derived value encodes (A-647 part 2).
  pub(crate) fn run_pairs(&self) -> &[(u64, u64)] {
    &self.runs
  }

  /// The set kept as `runs`, or `None` unless each is non-empty and they ascend with a gap between each two: the one
  /// way a set is kept, so a decoded set is equal to the set encoded and to no other.
  pub(crate) fn from_runs(runs: Vec<(u64, u64)>) -> Option<Positions> {
    let each = runs.iter().all(|run| run.0 < run.1);
    let apart = runs
      .windows(2)
      .all(|pair| matches!(pair, [left, right] if left.1 < right.0));
    (each && apart).then_some(Positions { runs })
  }

  /// Every position, ascending.
  pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
    self.runs.iter().flat_map(|&(start, end)| start..end)
  }

  /// The positions both this set and `other` hold (A-639): each run of one met with the runs of the other it overlaps,
  /// in one pass over both. Two runs out never touch, since a gap of either set lies between them.
  pub fn intersection(&self, other: &Positions) -> Positions {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let (mut mine, mut theirs) = (self.runs.iter().peekable(), other.runs.iter().peekable());
    while let (Some(&&(start, end)), Some(&&(from, to))) = (mine.peek(), theirs.peek()) {
      let (low, high) = (start.max(from), end.min(to));
      if low < high {
        runs.push((low, high));
      }
      if end <= to {
        mine.next();
      } else {
        theirs.next();
      }
    }
    Positions { runs }
  }

  /// The positions this set holds and `other` does not (A-639): each run cut by the runs of `other` it overlaps, in one
  /// pass over both.
  pub fn difference(&self, other: &Positions) -> Positions {
    let mut runs: Vec<(u64, u64)> = Vec::with_capacity(self.runs.len());
    let mut first = 0_usize;
    for &(start, end) in &self.runs {
      while other.runs.get(first).is_some_and(|run| run.1 <= start) {
        first = first.saturating_add(1);
      }
      let mut from = start;
      let cuts = other.runs.iter().skip(first).take_while(|run| run.0 < end);
      for &(cut_start, cut_end) in cuts {
        if cut_start > from {
          runs.push((from, cut_start));
        }
        from = from.max(cut_end);
      }
      if from < end {
        runs.push((from, end));
      }
    }
    Positions { runs }
  }

  /// Whether `other` holds every position this set holds (A-639).
  pub fn is_subset(&self, other: &Positions) -> bool {
    self.difference(other).is_empty()
  }

  /// Adds `position` (A-639): in constant time at the end, as positions given in order arrive; anywhere else in place
  /// ([`Positions::insert_new`]).
  pub fn insert(&mut self, position: u64) {
    self.insert_new(position);
  }

  /// Adds `position` in place, wherever it falls (A-745: a walk meets positions in any order, a commit walk from the
  /// newest down): whether the set did not hold it. Halving finds the run it falls in or beside; it extends that run or
  /// the next, joins the two, or begins a run of its own, moving no more than the runs after it.
  pub fn insert_new(&mut self, position: u64) -> bool {
    let end = position.saturating_add(1);
    let after = self.runs.partition_point(|run| run.0 <= position);
    let before = after.checked_sub(1);
    let previous = before.and_then(|at| self.runs.get(at)).copied();
    if previous.is_some_and(|run| position < run.1) {
      return false;
    }
    let joins_previous = previous.is_some_and(|run| run.1 == position);
    let next = self.runs.get(after).copied();
    let joins_next = next.is_some_and(|run| run.0 == end);
    match (joins_previous, joins_next, before) {
      (true, true, Some(at)) => {
        let next_end = next.map_or(end, |run| run.1);
        if let Some(run) = self.runs.get_mut(at) {
          run.1 = next_end;
        }
        self.runs.remove(after);
      }
      (true, false, Some(at)) => {
        if let Some(run) = self.runs.get_mut(at) {
          run.1 = end;
        }
      }
      (false, true, _) => {
        if let Some(run) = self.runs.get_mut(after) {
          run.0 = position;
        }
      }
      _ => self.runs.insert(after, (position, end)),
    }
    true
  }

  /// The least position the set holds at `from` or after it (A-639): the first run that ends after `from`, found by
  /// halving.
  pub fn next_from(&self, from: u64) -> Option<u64> {
    let at = self.runs.partition_point(|run| run.1 <= from);
    self.runs.get(at).map(|run| run.0.max(from))
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeSet;

  use proptest::prelude::*;

  use super::Positions;

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-745 (a walk inserts positions in any order). Do: insert random positions, repeats among them, in the order
    /// drawn, into a set and into an ordered set beside it. Expect: after each insert, the same answer to "was it new",
    /// the same members, and runs kept in their one form (each non-empty, ascending, a gap between each two), so the
    /// set is equal to the set its members make.
    #[test]
    fn a745_positions_inserted_in_any_order_are_the_set_they_make(drawn in proptest::collection::vec(0u64..200, 0..300)) {
      let mut set = Positions::new();
      let mut model = BTreeSet::new();
      for position in drawn {
        prop_assert_eq!(set.insert_new(position), model.insert(position));
        prop_assert!(Positions::from_runs(set.run_pairs().to_vec()).is_some(), "{:?}", set);
      }
      prop_assert_eq!(set.iter().collect::<Vec<_>>(), model.iter().copied().collect::<Vec<_>>());
      prop_assert_eq!(set, Positions::of(model.into_iter().collect()));
    }
  }
}
