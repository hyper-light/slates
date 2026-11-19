//! When an index compacts: rent or buy, learned from the deployment's own measured times (§4.17.1 "Every tunable is
//! learned online"; the compaction rows of §4.17.3's and §4.17.4's tables of learned tunables).
//!
//! **What a compaction buys.** A lookup that misses probes every layer of the index (the tail, then each table), and
//! one that hits probes every layer above the row's. A compaction merges the layers this index alone holds into one
//! table, so each later lookup saves the probes it would have made into the merged layers beyond the first; the layers
//! shared with a fork stay as they are, since merging them would copy rows the fork still shares. So the **rent** is
//! the time lookups spent, since the last compaction, on probes into compactable layers beyond the first each one
//! probed, and the **price** is the compaction's own time.
//!
//! **The rule.** Compact once the rent paid since the last compaction reaches the predicted price. It is the
//! break-even rule of the ski-rental problem, whose cost is at most twice an offline optimum's that knew every future
//! lookup, the best ratio a deterministic rule can have [A: Karlin, Manasse, Rudolph and Sleator, "Competitive snoopy
//! caching", Algorithmica 3, 1988]. Each compaction ends one such problem and the next begins with the rent at zero.
//!
//! **What is learned, and from what.** No literal anywhere: each input is a time the caller measured on the
//! deployment's own traffic.
//! - The rent: the caller times its lookups, a batch at a time ([`crate::Index::lookups_took`]); the index counted
//!   each batch's probes and the compactable ones beyond each lookup's first, and the batch's time is shared among its
//!   probes.
//! - The price: the last compaction's measured time per row ([`crate::Index::compaction_took`]) times the rows the next
//!   one would merge.
//! - The first value, before any compaction was timed: what building the tail cost per row, from the caller's timed
//!   inserts ([`crate::Index::inserts_took`]), which is §4.17.3's stated first fold ("when the tail's lookups cost
//!   more than building the tail did").
//! - The bound: the store's budget (§4.2), which a tail is charged against; past it an insert is refused by kind, and
//!   the owner compacts or refuses the push.

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

/// The rule's state (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compaction {
  /// The rent paid since the last compaction.
  rent_ns: u64,
  /// The last compaction's measured cost.
  compacting: Option<Rate>,
  /// What building the tail cost, until a compaction is measured.
  building: Rate,
}

impl Compaction {
  /// Lookups that made `probes` probes in all took `elapsed_ns`, and `excess` of those probes were into compactable
  /// layers beyond the first each lookup probed: the rent grows by the excess probes' share of the time.
  pub fn lookups(&mut self, elapsed_ns: u64, probes: u64, excess: u64) {
    let share = u128::from(elapsed_ns)
      .saturating_mul(u128::from(excess.min(probes)))
      .checked_div(u128::from(probes))
      .unwrap_or(0);
    self.rent_ns = self
      .rent_ns
      .saturating_add(u64::try_from(share).unwrap_or(u64::MAX));
  }

  /// Inserts of `rows` rows took `elapsed_ns`: the first value's input.
  pub fn built(&mut self, elapsed_ns: u64, rows: u64) {
    self.building = Rate {
      ns: self.building.ns.saturating_add(elapsed_ns),
      rows: self.building.rows.saturating_add(rows),
    };
  }

  /// A compaction of `rows` rows took `elapsed_ns`: the price is learned again and the rent starts over.
  pub fn compacted(&mut self, elapsed_ns: u64, rows: u64) {
    if rows > 0 {
      self.compacting = Some(Rate {
        ns: elapsed_ns,
        rows,
      });
    }
    self.rent_ns = 0;
  }

  /// The predicted time of a compaction of `rows` rows, or `None` before anything was measured.
  pub fn predicted_ns(&self, rows: u64) -> Option<u64> {
    self.compacting.unwrap_or(self.building).of(rows)
  }

  /// Whether a compaction of `rows` rows is due: some rent was paid, and it has reached the predicted price.
  pub fn due(&self, rows: u64) -> bool {
    rows > 0
      && self.rent_ns > 0
      && self
        .predicted_ns(rows)
        .is_some_and(|price| self.rent_ns >= price)
  }

  /// The rent paid since the last compaction.
  pub fn rent_ns(&self) -> u64 {
    self.rent_ns
  }

  /// The rule for an index whose compactable layers just changed under it (a fork shares them): the measured rates
  /// kept, the rent started over, since what was paid bought nothing a compaction can still save.
  pub fn restarted(&self) -> Compaction {
    Compaction {
      rent_ns: 0,
      ..*self
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-581, §4.17.1 "Every tunable is learned online". Do: ask before anything was measured, then time inserts of
  /// 10 rows at 1,000 ns and lookups whose excess probes cost 600 ns, then 400 ns more. Expect: never due before a
  /// price exists; the first price is the build cost (1,000 ns for 10 rows, 2,000 for 20); due exactly when the rent
  /// reaches it, not a nanosecond before.
  #[test]
  fn the_first_compaction_is_due_when_the_rent_reaches_what_building_the_tail_cost() {
    let mut rule = Compaction::default();
    rule.lookups(900, 3, 2);
    assert_eq!(rule.rent_ns(), 600);
    assert!(!rule.due(10), "no price measured yet");
    rule.built(1_000, 10);
    assert_eq!(rule.predicted_ns(10), Some(1_000));
    assert_eq!(rule.predicted_ns(20), Some(2_000));
    assert!(!rule.due(10));
    rule.lookups(399, 1, 1);
    assert_eq!(rule.rent_ns(), 999);
    assert!(!rule.due(10));
    rule.lookups(1, 1, 1);
    assert!(rule.due(10));
    assert!(!rule.due(11), "eleven rows cost 1,100 ns");
    assert!(!rule.due(0), "nothing to compact");
  }

  /// A-581. Do: time a compaction of 100 rows at 5,000 ns after a build was timed, then pay rent. Expect: the price is
  /// the compaction's measured rate from then on (50 ns a row), the rent starts over at the compaction, and lookups
  /// that probed only one compactable layer each pay none.
  #[test]
  fn a_timed_compaction_sets_the_price_and_starts_the_rent_over() {
    let mut rule = Compaction::default();
    rule.built(9_000, 10);
    rule.lookups(10_000, 2, 1);
    rule.compacted(5_000, 100);
    assert_eq!(rule.rent_ns(), 0);
    assert_eq!(rule.predicted_ns(10), Some(500));
    rule.lookups(1_000_000, 50, 0);
    assert_eq!(rule.rent_ns(), 0);
    assert!(!rule.due(10));
    rule.lookups(1_000, 2, 1);
    assert!(rule.due(10));
    assert!(!rule.restarted().due(10));
    assert_eq!(rule.restarted().predicted_ns(10), Some(500));
  }

  /// A-581. Do: feed the rule the largest times, rows and probes there are. Expect: no overflow, every sum saturated.
  #[test]
  fn the_rule_saturates_at_the_largest_inputs() {
    let mut rule = Compaction::default();
    rule.lookups(u64::MAX, u64::MAX, u64::MAX);
    rule.lookups(u64::MAX, 1, 5);
    assert_eq!(rule.rent_ns(), u64::MAX);
    rule.built(u64::MAX, 1);
    rule.built(u64::MAX, u64::MAX);
    assert_eq!(rule.predicted_ns(u64::MAX), Some(u64::MAX));
    assert!(rule.due(1));
  }
}
