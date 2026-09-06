//! Quotas and accounting (D-13): a bounded volume reserves its quota and refuses the byte that
//! would exceed it before anything is copied; a dynamic volume grows by increments the pressure
//! source admits against the shard budget; `referenced_bytes` charges every chunk the head reaches
//! in full and `unique_bytes` what the head alone holds since its last snapshot.

use slates_mem::budget::ShardBudget;

/// Where a dynamic volume asks before growing (§4.2). Growth is a *check-and-acquire* against the
/// shard budget — the one capacity owner — passed in by the write path: the daemon's source debits
/// the live budget so no two volumes get the same capacity and growth reduces what bounded volumes
/// are later offered; a test source answers from a fixed ceiling and leaves the budget untouched.
pub trait PressureSource {
  /// Whether `bytes` more may be taken now, acquiring them from `budget` if so.
  fn may_grow(&mut self, bytes: u64, budget: &mut ShardBudget) -> bool;
}

/// The real source: a dynamic volume's growth admitted by, and debited from, the shard budget
/// (§4.2). Each increment is a check-and-acquire against the one capacity owner, so growth takes only
/// capacity neither committed to another volume nor reserved as the operation headroom, and no two
/// volumes ever receive the same bytes. Stateless — the budget it debits is the shard's, passed in.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetGrowth;

impl PressureSource for BudgetGrowth {
  fn may_grow(&mut self, bytes: u64, budget: &mut ShardBudget) -> bool {
    budget.grow(bytes).is_ok()
  }
}

/// A test source that agrees up to a fixed ceiling, independent of any shard budget — for volume-core
/// tests that bound growth without a daemon. It ignores the budget, so it neither reads nor debits it.
#[derive(Debug, Clone)]
pub struct Ceiling {
  /// Bytes granted so far.
  pub granted: u64,
  /// The ceiling.
  pub limit: u64,
}

impl PressureSource for Ceiling {
  fn may_grow(&mut self, bytes: u64, _budget: &mut ShardBudget) -> bool {
    match self.granted.checked_add(bytes) {
      Some(total) if total <= self.limit => {
        self.granted = total;
        true
      }
      _ => false,
    }
  }
}

/// The volume's size policy.
pub enum Quota {
  /// A fixed quota reserved at creation.
  Bounded {
    /// Bytes.
    limit: u64,
  },
  /// Grows while the pressure source allows, never past `max`.
  Dynamic {
    /// The ceiling.
    max: u64,
    /// The source asked before each increment.
    source: Box<dyn PressureSource>,
    /// Bytes the source has granted so far.
    granted: u64,
    /// Growth requests the source refused: each is one pressure event (T-1.5).
    denied: u64,
  },
}

impl std::fmt::Debug for Quota {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Bounded { limit } => f.debug_struct("Bounded").field("limit", limit).finish(),
      Self::Dynamic {
        max,
        granted,
        denied,
        ..
      } => f
        .debug_struct("Dynamic")
        .field("max", max)
        .field("granted", granted)
        .field("denied", denied)
        .finish(),
    }
  }
}

/// The exact counters of D-13.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
  /// Bytes of every chunk the head reaches, charged in full.
  pub referenced_bytes: u64,
  /// Bytes the head alone holds since its last snapshot (freed if the head were dropped).
  pub unique_bytes: u64,
}

impl Quota {
  /// Whether `referenced + more` fits; for a dynamic volume this may ask the source.
  pub fn admit(&mut self, referenced: u64, more: u64, budget: &mut ShardBudget) -> bool {
    let Some(total) = referenced.checked_add(more) else {
      return false;
    };
    match self {
      Self::Bounded { limit } => total <= *limit,
      Self::Dynamic {
        max,
        source,
        granted,
        denied,
      } => {
        if total > *max {
          return false;
        }
        if total <= *granted {
          return true;
        }
        let needed = total - *granted;
        if source.may_grow(needed, budget) {
          *granted = total;
          true
        } else {
          *denied += 1;
          false
        }
      }
    }
  }

  /// Bytes this quota holds against the shard budget: a dynamic quota's granted growth (a bounded
  /// quota's reservation is held by the server, so this is zero). Released to the budget on teardown.
  pub const fn budget_hold(&self) -> u64 {
    match self {
      Self::Bounded { .. } => 0,
      Self::Dynamic { granted, .. } => *granted,
    }
  }

  /// Growth requests the pressure source refused so far (zero for a bounded quota).
  pub fn denials(&self) -> u64 {
    match self {
      Self::Bounded { .. } => 0,
      Self::Dynamic { denied, .. } => *denied,
    }
  }

  /// Resizes (§4.4 `resize`): a bounded quota takes the new limit unless `referenced` already
  /// exceeds it; a dynamic quota takes the new maximum.
  pub fn resize(&mut self, referenced: u64, new_limit: u64) -> Result<(), crate::error::VfsError> {
    if referenced > new_limit {
      return Err(crate::error::VfsError::NoSpace);
    }
    match self {
      Self::Bounded { limit } => *limit = new_limit,
      Self::Dynamic { max, .. } => *max = new_limit,
    }
    Ok(())
  }

  /// The quota's ceiling.
  pub fn limit(&self) -> u64 {
    match self {
      Self::Bounded { limit } => *limit,
      Self::Dynamic { max, .. } => *max,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bounded_refuses_the_byte_past_its_limit_and_dynamic_asks_its_source() {
    // A budget large enough not to bind here; the Ceiling source is the binding limit.
    let mut budget = ShardBudget::new(1 << 40, 0);
    let mut b = Quota::Bounded { limit: 100 };
    assert!(b.admit(90, 10, &mut budget));
    assert!(!b.admit(90, 11, &mut budget));
    let mut d = Quota::Dynamic {
      max: 1000,
      source: Box::new(Ceiling {
        granted: 0,
        limit: 150,
      }),
      granted: 0,
      denied: 0,
    };
    assert!(d.admit(0, 100, &mut budget));
    assert!(d.admit(100, 50, &mut budget));
    assert!(!d.admit(150, 1, &mut budget), "the source's ceiling");
    assert_eq!(d.denials(), 1, "one pressure event");
    assert!(
      d.admit(100, 50, &mut budget),
      "within what was already granted"
    );
    assert!(!d.admit(999, 2, &mut budget), "the max");
  }

  /// The real (budget-backed) source: growth is admitted by and debited from the shard budget, so
  /// two dynamic quotas cannot spend the same capacity (§4.2).
  #[test]
  fn budget_backed_growth_debits_the_shared_budget_and_cannot_double_spend() {
    let mut budget = ShardBudget::new(100, 0); // 100 bytes, no headroom
    let mut a = Quota::Dynamic {
      max: 1000,
      source: Box::new(BudgetGrowth),
      granted: 0,
      denied: 0,
    };
    let mut b = Quota::Dynamic {
      max: 1000,
      source: Box::new(BudgetGrowth),
      granted: 0,
      denied: 0,
    };
    assert!(
      a.admit(0, 60, &mut budget),
      "A grows to 60 from the shared budget"
    );
    assert_eq!(budget.committed(), 60);
    assert!(b.admit(0, 40, &mut budget), "B takes the remaining 40");
    assert!(
      !b.admit(40, 1, &mut budget),
      "the budget is spent — B cannot take capacity A already holds"
    );
    assert_eq!(b.denials(), 1);
  }
}
