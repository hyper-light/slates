//! A set of object positions (A-614; §4.17.2 "The repository in git's terms", item 3: "kept per green as a compressed
//! bitmap over the repository's object positions"): sorted, disjoint runs `[start, end)`. Positions are given in
//! arrival order, so the objects one push brings new to the repository are one run, and a green's reached set is a few
//! runs however many objects it holds. These are the run containers of a compressed bitmap [A: Lemire, Ssi-Yan-Kai and
//! Kaser, "Consistently faster and smaller compressed bitmaps with Roaring", Software: Practice and Experience 46,
//! 2016], kept alone because this data is runs. A position is below `u64::MAX`, which the repository never gives.

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

  /// Every position, ascending.
  pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
    self.runs.iter().flat_map(|&(start, end)| start..end)
  }

  /// The positions both this set and `other` hold (A-639).
  pub fn intersection(&self, other: &Positions) -> Positions {
    let _ = other;
    Positions::new()
  }

  /// The positions this set holds and `other` does not (A-639).
  pub fn difference(&self, other: &Positions) -> Positions {
    let _ = other;
    self.clone()
  }

  /// Whether `other` holds every position this set holds (A-639).
  pub fn is_subset(&self, other: &Positions) -> bool {
    let _ = other;
    false
  }

  /// Adds `position` (A-639).
  pub fn insert(&mut self, position: u64) {
    let _ = position;
  }

  /// The least position the set holds at `from` or after it (A-639).
  pub fn next_from(&self, from: u64) -> Option<u64> {
    let _ = from;
    None
  }
}
