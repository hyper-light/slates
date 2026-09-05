//! The pre-fault batch scheduler: fault a region's pages in batches sized so that one batch fits
//! the shard's idle slice, yielding between batches so a shard never stalls its clients on a
//! long region (§4.2, "the shard schedules a pre-fault task on its idle time").
//!
//! The batch size is derived, not chosen: `slice_ns / fault_ns` pages per step, where the fault
//! cost is the profile's measured base-page fault and the slice is the caller's idle budget
//! (the runtime's step budget, itself derived from the measured wake cost).

use slates_machine::{Derived, derived};

use crate::region::Region;

/// A pre-fault job over one region.
#[derive(Debug)]
pub struct PreFault {
  next_page: usize,
  batch_pages: Derived<usize>,
}

/// What one step did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// Pages faulted in this step; more remain.
  Faulted {
    /// Pages touched.
    pages: usize,
  },
  /// The region is fully resident.
  Done,
}

impl PreFault {
  /// A job that faults `slice_ns / fault_ns` pages per step (at least one).
  pub fn new(slice_ns: u64, fault_ns: u64) -> Self {
    let batch = derived!(
      usize::try_from(slice_ns / fault_ns.max(1))
        .unwrap_or(usize::MAX)
        .max(1),
      "idle slice / measured base-page fault cost, at least one page",
      ["rt.task_step_budget_ns", "faults.base_ns"]
    );
    Self {
      next_page: 0,
      batch_pages: batch,
    }
  }

  /// Pages per step with its derivation.
  pub const fn batch(&self) -> Derived<usize> {
    self.batch_pages
  }

  /// The next page to fault.
  pub const fn position(&self) -> usize {
    self.next_page
  }

  /// Faults one batch; call again until `Done`.
  pub fn step(&mut self, region: &mut Region) -> Progress {
    let pages = region.pages();
    if self.next_page >= pages {
      return Progress::Done;
    }
    let to = self
      .next_page
      .saturating_add(self.batch_pages.get())
      .min(pages);
    region.touch_pages(self.next_page, to);
    let done = to - self.next_page;
    self.next_page = to;
    Progress::Faulted { pages: done }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_region_is_faulted_in_batches_derived_from_the_slice_and_fault_cost() {
    let page = usize::try_from(slates_machine::facts::Facts::query().page.base).unwrap();
    let mut region = Region::map(page * 10, page, false).unwrap();
    let mut job = PreFault::new(3_000, 1_000);
    assert_eq!(job.batch().get(), 3);
    let mut steps = Vec::new();
    while let Progress::Faulted { pages } = job.step(&mut region) {
      steps.push(pages);
    }
    assert_eq!(steps, vec![3, 3, 3, 1]);
    assert_eq!(job.position(), 10);
    assert_eq!(job.step(&mut region), Progress::Done);
  }

  #[test]
  fn a_zero_fault_cost_still_faults_at_least_one_page_per_step() {
    let job = PreFault::new(0, 0);
    assert_eq!(job.batch().get(), 1);
    assert!(job.batch().formula.contains("at least one page"));
  }
}
