//! When an index merges runs, and which: rent or buy, learned from the deployment's own measured times (§4.17.1 "Every
//! tunable is learned online"; the compaction rows of §4.17.3's and §4.17.4's tables of learned tunables).
//!
//! **What a merge buys.** An index is a tail over runs, each run a sorted sequence of tables ([`crate::index`]). A
//! lookup visits the tail, then the runs from the newest, one table a run, until one holds the key. Merging the newest
//! `j` runs this index alone holds into one saves each later lookup the visits it would make to those runs beyond the
//! first of them: a lookup that visited `p` of them saves `min(p, j) - 1`. Runs a fork shares are never merged, since
//! merging them would copy rows the fork still holds, so a visit to one is never rent.
//!
//! **The rule.** Each candidate `j` is its own rent-or-buy problem. Its **rent** is the time lookups spent, since the
//! last merge, on the visits merging `j` would have saved: each batch's measured time shared among its visits. Its
//! **price** is the merge's predicted time, the last merge's measured time a row times the rows of the newest `j` runs.
//! A merge is due for the largest `j` whose rent has reached its price. Each is the break-even rule of the ski-rental
//! problem, whose cost is at most twice an offline optimum's that knew every future lookup, the best ratio a
//! deterministic rule can have [A: Karlin, Manasse, Rudolph and Sleator, "Competitive snoopy caching", Algorithmica 3,
//! 1988]. So two small runs are merged as soon as their visits pay for that, long before the visits to a large run
//! below them pay for rewriting it: the merge point is the deployment's own, never a size ratio fixed in advance.
//!
//! **Carried over.** A run added on top (the tail sealed) changes which runs are the newest `j`: the rent paid toward
//! merging the old newest `j` is what merging the new newest `j + 1` would have saved of the past, since the new run
//! was in no past lookup's way, so each rent moves up one candidate and the new pair starts at zero. A merge, or a fork
//! changing which runs may be merged, starts every rent over.
//!
//! **What is learned, and from what.** No literal anywhere: each input is a time the caller measured on the
//! deployment's own traffic.
//! - The rent: the caller times its lookups a batch at a time ([`crate::Index::lookups_took`]); the index counted each
//!   batch's visits, and for each lookup how many compactable runs it visited.
//! - The price: the last merge's measured time per row ([`crate::Index::compaction_took`]).
//! - The first value, before any merge was timed: what building the tail cost per row, from the caller's timed inserts
//!   ([`crate::Index::inserts_took`]), which is §4.17.3's stated first fold ("when the tail's lookups cost more than
//!   building the tail did").
//! - The bound: the store's budget (§4.2), which every table, the tail and the runs' fences are charged against.

/// A measured cost: the nanoseconds spent over the rows they were spent on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rate {
  /// The nanoseconds.
  pub ns: u64,
  /// The rows.
  pub rows: u64,
}

impl Rate {
  /// The cost of `rows` rows at this rate, or `None` when it was measured over none.
  fn of(&self, rows: u64) -> Option<u64> {
    let ns = u128::from(self.ns)
      .checked_mul(u128::from(rows))?
      .checked_div(u128::from(self.rows))?;
    Some(u64::try_from(ns).unwrap_or(u64::MAX))
  }
}

/// The visits merging the newest `j` runs saves the lookups `depths` counts: `depths[p]` lookups visited `p` of the
/// compactable runs, and each saves `min(p, j) - 1`.
fn saved(depths: &[u64], j: usize) -> u64 {
  depths
    .iter()
    .enumerate()
    .map(|(visited, lookups)| {
      let each = visited.min(j).saturating_sub(1);
      lookups.saturating_mul(u64::try_from(each).unwrap_or(u64::MAX))
    })
    .fold(0_u64, u64::saturating_add)
}

/// The rule's state (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
  /// `rents[i]` is the rent paid toward merging the newest `i + 2` compactable runs; one for each run past the first.
  rents: Vec<u64>,
  /// The last merge's measured cost.
  compacting: Option<Rate>,
  /// What building the tail cost, until a merge is measured.
  building: Rate,
}

impl Compaction {
  /// Lookups that made `visits` visits in all took `elapsed_ns`, and `depths[p]` of them visited `p` compactable runs:
  /// each candidate's rent grows by its saved visits' share of the time.
  pub fn lookups(&mut self, elapsed_ns: u64, visits: u64, depths: &[u64]) {
    for (at, rent) in self.rents.iter_mut().enumerate() {
      let j = at.saturating_add(2);
      let share = u128::from(elapsed_ns)
        .saturating_mul(u128::from(saved(depths, j).min(visits)))
        .checked_div(u128::from(visits))
        .unwrap_or(0);
      *rent = rent.saturating_add(u64::try_from(share).unwrap_or(u64::MAX));
    }
  }

  /// Inserts of `rows` rows took `elapsed_ns`: the first price's input.
  pub fn built(&mut self, elapsed_ns: u64, rows: u64) {
    self.building = Rate {
      ns: self.building.ns.saturating_add(elapsed_ns),
      rows: self.building.rows.saturating_add(rows),
    };
  }

  /// A merge of `rows` rows took `elapsed_ns`: the price is learned again.
  pub fn compacted(&mut self, elapsed_ns: u64, rows: u64) {
    if rows > 0 {
      self.compacting = Some(Rate {
        ns: elapsed_ns,
        rows,
      });
    }
  }

  /// A compactable run was added on top: each rent moves up one candidate, and the new pair starts at zero (module
  /// docs, "Carried over"). `runs` counts the compactable runs before it.
  pub fn run_added(&mut self, runs: usize) {
    if runs > 0 {
      self.rents.insert(0, 0);
    }
  }

  /// The compactable runs changed under the rule (a merge, a fork, a fork's release): every rent starts over, one for
  /// each of `runs` past the first; the measured rates are kept.
  pub fn restart(&mut self, runs: usize) {
    self.rents.clear();
    self.rents.resize(runs.saturating_sub(1), 0);
  }

  /// The predicted time of a merge of `rows` rows, or `None` before anything was measured.
  pub fn predicted_ns(&self, rows: u64) -> Option<u64> {
    self.compacting.unwrap_or(self.building).of(rows)
  }

  /// The largest `j` whose merge is due, given `rows[i]`, the rows of the newest `i + 1` compactable runs: some rent was
  /// paid toward it, and it has reached the predicted price.
  pub fn due(&self, rows: &[u64]) -> Option<usize> {
    self
      .rents
      .iter()
      .enumerate()
      .rev()
      .find_map(|(at, rent)| {
        let j = at.saturating_add(2);
        let rows = *rows.get(j.checked_sub(1)?)?;
        let price = self.predicted_ns(rows)?;
        (rows > 0 && *rent > 0 && *rent >= price).then_some(j)
      })
  }

  /// The rent paid toward merging the newest `j` compactable runs.
  pub fn rent_ns(&self, j: usize) -> u64 {
    j.checked_sub(2)
      .and_then(|at| self.rents.get(at))
      .copied()
      .unwrap_or(0)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Two compactable runs, 900 ns paid toward merging them by lookups that each visited both: one batch of one lookup
  /// over three visits in 900 ns (a third of it saved), one of two lookups over two visits in 600 ns (all of it).
  fn two_runs_with_900_ns_paid() -> Compaction {
    let mut rule = Compaction::default();
    rule.restart(2);
    rule.lookups(900, 3, &[0, 0, 1]);
    assert_eq!(rule.rent_ns(2), 300);
    rule.lookups(600, 2, &[0, 0, 2]);
    assert_eq!(rule.rent_ns(2), 900);
    rule
  }

  /// A-581, §4.17.1 "Every tunable is learned online". Do: with two compactable runs of ten rows in all and 900 ns paid
  /// toward merging them, ask before anything was measured, then time inserts of 10 rows at 1,000 ns, then pay 99 ns
  /// and 1 ns more. Expect: never due before a price exists; the first price is the build cost (1,000 ns for 10 rows,
  /// 2,000 for 20); due exactly when the rent reaches it, not a nanosecond before, and not for rows the rent has not
  /// paid for.
  #[test]
  fn the_first_merge_is_due_when_the_rent_reaches_what_building_the_rows_cost() {
    let mut rule = two_runs_with_900_ns_paid();
    assert_eq!(rule.due(&[5, 10]), None, "no price measured yet");
    rule.built(1_000, 10);
    assert_eq!(
      (rule.predicted_ns(10), rule.predicted_ns(20)),
      (Some(1_000), Some(2_000))
    );
    rule.lookups(99, 1, &[0, 0, 1]);
    assert_eq!((rule.rent_ns(2), rule.due(&[5, 10])), (999, None));
    rule.lookups(1, 1, &[0, 0, 1]);
    assert_eq!(rule.due(&[5, 10]), Some(2));
    assert_eq!(rule.due(&[5, 11]), None, "eleven rows cost 1,100 ns");
    assert_eq!(rule.due(&[0, 0]), None, "nothing to merge");
  }

  /// A-581. Do: three compactable runs, the oldest of 100 rows and two newer of 10 each, at 10 ns a row; misses that
  /// visit all three, 100 ns a visit. Expect: each miss pays 100 ns toward merging the newest two (one visit saved) and
  /// 200 ns toward merging all three (two saved); the newest two are due after 2 misses (their 20 rows cost 200 ns),
  /// all three only after 6 (120 rows, 1,200 ns); and a lookup found in the first run it visits pays no rent.
  #[test]
  fn two_small_runs_are_due_long_before_the_large_run_below_them() {
    let mut rule = Compaction::default();
    rule.restart(3);
    rule.built(10, 1);
    let rows = [10, 20, 120];
    rule.lookups(1_000_000, 10, &[0, 10, 0, 0]);
    assert_eq!((rule.rent_ns(2), rule.rent_ns(3)), (0, 0));
    rule.lookups(300, 3, &[0, 0, 0, 1]);
    assert_eq!((rule.rent_ns(2), rule.rent_ns(3)), (100, 200));
    assert_eq!(rule.due(&rows), None);
    rule.lookups(300, 3, &[0, 0, 0, 1]);
    assert_eq!(rule.due(&rows), Some(2));
    rule.lookups(1_200, 12, &[0, 0, 0, 4]);
    assert_eq!((rule.rent_ns(2), rule.rent_ns(3)), (600, 1_200));
    assert_eq!(rule.due(&rows), Some(3));
  }

  /// A-581. Do: pay rent toward merging the newest two of two runs, add a run on top, then time a merge. Expect: the
  /// paid rent moves to the newest three (the new run was in no past lookup's way) and the new pair starts at zero; a
  /// timed merge of 100 rows at 5,000 ns sets the price to 50 ns a row; a restart clears every rent and keeps the price.
  #[test]
  fn a_run_added_on_top_carries_the_rent_paid_toward_the_runs_below() {
    let mut rule = Compaction::default();
    rule.restart(2);
    rule.lookups(500, 2, &[0, 0, 1]);
    assert_eq!(rule.rent_ns(2), 250);
    rule.run_added(2);
    assert_eq!((rule.rent_ns(2), rule.rent_ns(3)), (0, 250));
    rule.compacted(5_000, 100);
    assert_eq!(rule.predicted_ns(10), Some(500));
    rule.restart(3);
    assert_eq!((rule.rent_ns(2), rule.rent_ns(3)), (0, 0));
    assert_eq!(rule.predicted_ns(10), Some(500));
    let mut first = Compaction::default();
    first.run_added(0);
    assert_eq!(first.rent_ns(2), 0, "one run has nothing to merge with");
  }

  /// A-581. Do: feed the rule the largest times, rows and visits there are. Expect: no overflow, every sum saturated.
  #[test]
  fn the_rule_saturates_at_the_largest_inputs() {
    let mut rule = Compaction::default();
    rule.restart(2);
    rule.lookups(u64::MAX, u64::MAX, &[0, 0, u64::MAX]);
    rule.lookups(u64::MAX, 1, &[0, 0, 5]);
    assert_eq!(rule.rent_ns(2), u64::MAX);
    rule.built(u64::MAX, 1);
    rule.built(u64::MAX, u64::MAX);
    assert_eq!(rule.predicted_ns(u64::MAX), Some(u64::MAX));
    assert_eq!(rule.due(&[1, u64::MAX]), Some(2));
  }
}
