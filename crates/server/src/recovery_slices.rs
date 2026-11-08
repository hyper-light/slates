//! A shard's recovery in slices (§2.6 boot step 2, §4.8 "Recovery", A-228): the shard's start runs as a task the shard
//! runs alone (`slates_rt::futures::spawn_alone`), and its recovery yields between units once a slice's time is spent,
//! so every slice ends a step and the watchdog, which withholds the daemon's beat from a shard that runs a liveness
//! window of CPU without one, sees the shard step through a recovery of any length.
//!
//! A slice's time is [`crate::config::DaemonConfig::copy_slice_ns`], half the live step quantum, the budget the
//! codebase already gives every other long piece of shard work (an image's build, a DAX fill); the clock is read after
//! every unit, so a slice overruns its budget by its last unit at most. The units are recovery's own: the catalog's
//! replay, each record rebuilt in the order recovery always took them, a green's origin walk one directory at a time,
//! its chain one entry at a time, and each work's log replay. Only where a poll ends moves: no unit is reordered or
//! split, and nothing else on the shard runs until the start ends, since the shard runs it alone.
//!
//! Before 2026-11-07 the whole start was one poll. Measured on this Mac (`crates/server/tests/recovery.rs`, the
//! recovery test): 1,024 merge units recovered in 5.52 s of CPU in one poll, and the beat went 9.3 s without
//! advancing; the anchor would kill such a daemon at every restart.

/// A recovery's slices: when the current one began, and what the whole recovery took, for the boot log.
#[derive(Debug)]
pub(crate) struct RecoverySlices {
  /// The time one slice may take: half the live step quantum.
  budget_ns: u64,
  /// When the current slice began, on the shard's clock.
  began_ns: u64,
  /// Slices ended so far.
  slices: u64,
  /// Units done so far.
  units: u64,
  /// The longest slice so far, by the shard's clock.
  longest_ns: u64,
  /// The shard thread's CPU when the recovery began and when the last slice ended, `None` where the thread's clock
  /// cannot be read (Miri).
  cpu_began_ns: Option<u64>,
  slice_cpu_began_ns: Option<u64>,
  /// The most CPU one slice took so far: what the watchdog judges, CPU without a step.
  longest_cpu_ns: u64,
  /// The units done when the slice that took the most CPU ended, naming where it was.
  longest_cpu_ended_at: u64,
}

/// What a recovery's slicing did, for the boot log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SliceReport {
  /// The slices the recovery ran in.
  pub(crate) slices: u64,
  /// The units it did.
  pub(crate) units: u64,
  /// Its longest slice, in nanoseconds of the shard's clock.
  pub(crate) longest_ns: u64,
  /// The time a slice was given.
  pub(crate) budget_ns: u64,
  /// The shard thread's CPU over the whole recovery; `None` where its clock cannot be read.
  pub(crate) cpu_ns: Option<u64>,
  /// The most CPU one slice took.
  pub(crate) longest_cpu_ns: u64,
  /// The units done when that slice ended: 1 when it was the first unit, the catalog's replay.
  pub(crate) longest_cpu_ended_at: u64,
}

impl RecoverySlices {
  /// A recovery's slicing, its first slice beginning now, each slice given `budget_ns`.
  pub(crate) fn begin(budget_ns: u64) -> RecoverySlices {
    let cpu = thread_cpu_ns();
    RecoverySlices {
      budget_ns: budget_ns.max(1),
      began_ns: slates_rt::futures::now_ns(),
      slices: 0,
      units: 0,
      longest_ns: 0,
      cpu_began_ns: cpu,
      slice_cpu_began_ns: cpu,
      longest_cpu_ns: 0,
      longest_cpu_ended_at: 0,
    }
  }

  /// One unit done: when the slice's time is spent, the slice ends and the task yields, so the shard completes a step
  /// before the next unit begins.
  pub(crate) async fn unit_done(&mut self) {
    self.units = self.units.saturating_add(1);
    let now = slates_rt::futures::now_ns();
    if now.saturating_sub(self.began_ns) < self.budget_ns {
      return;
    }
    self.end_slice(now);
    slates_rt::futures::yield_now().await;
    self.began_ns = slates_rt::futures::now_ns();
  }

  /// Ends the last slice and reports the whole.
  pub(crate) fn finish(mut self) -> SliceReport {
    let cpu = self.end_slice(slates_rt::futures::now_ns());
    SliceReport {
      slices: self.slices,
      units: self.units,
      longest_ns: self.longest_ns,
      budget_ns: self.budget_ns,
      cpu_ns: cpu
        .zip(self.cpu_began_ns)
        .map(|(now, began)| now.saturating_sub(began)),
      longest_cpu_ns: self.longest_cpu_ns,
      longest_cpu_ended_at: self.longest_cpu_ended_at,
    }
  }

  /// Ends the current slice at `now`, and returns the thread's CPU then. The thread's clock is read once a slice: the
  /// next slice's CPU counts from this reading, so it includes the step its yield ends, an upper bound on the CPU the
  /// shard spent without a step.
  fn end_slice(&mut self, now: u64) -> Option<u64> {
    self.slices = self.slices.saturating_add(1);
    self.longest_ns = self.longest_ns.max(now.saturating_sub(self.began_ns));
    let cpu = thread_cpu_ns();
    if let (Some(now_cpu), Some(began)) = (cpu, self.slice_cpu_began_ns) {
      let spent = now_cpu.saturating_sub(began);
      if spent > self.longest_cpu_ns {
        self.longest_cpu_ns = spent;
        self.longest_cpu_ended_at = self.units;
      }
    }
    self.slice_cpu_began_ns = cpu;
    cpu
  }
}

/// The calling shard thread's CPU, from the clock the watchdog reads (`slates_rt::registry::shard_cpu`); read at a
/// slice's ends only, never per unit. `None` off a shard or where the thread's clock cannot be read.
fn thread_cpu_ns() -> Option<u64> {
  let shard = slates_rt::futures::shard_id()?;
  let holder = slates_rt::registry::holder_of(shard.0)?;
  slates_rt::registry::shard_cpu(holder).map(|reading| reading.spent_ns)
}

#[cfg(test)]
mod tests {
  use super::RecoverySlices;

  /// What slicing may add to a recovery's time, in percent: the recovery-slicing plan's limit on its CPU cost.
  const ALLOWED_RISE_PERCENT: u64 = 10;
  /// Shape: how many yields are timed for the cost of one; the least of them is a yield nothing interrupted.
  const YIELDS_TIMED: u64 = 1024;
  /// Shape: the recovery's units, enough that the slices a limit allows are many more than the one a recovery always
  /// ends with.
  const UNITS: u64 = 4096;
  /// Shape: how long the test thread waits for the shard's report.
  const REPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

  /// §2.6 boot step 2, A-228: a recovery whose every unit is cheaper than a yield, on a shard whose live step quantum
  /// is the least there is, spends no more than the plan's limit of its time yielding. The units are as long as the
  /// cheapest yield this machine ran, so a recovery that yielded after each spent half its time doing it.
  #[test]
  // The test thread waits off the shard for what the shard's task reports.
  #[allow(clippy::disallowed_methods)]
  fn a_recovery_of_units_cheaper_than_a_yield_keeps_its_yields_within_the_limit() {
    let daemon = crate::daemon::audit_daemon();
    let (report, report_seen) = std::sync::mpsc::sync_channel::<(u64, u64, u64)>(1);
    crate::daemon::observe_first(&daemon, move |_state| {
      let recovery = slates_rt::futures::spawn(async move {
        let mut least_yield_ns = u64::MAX;
        for _ in 0..YIELDS_TIMED {
          let before = slates_rt::futures::now_ns();
          slates_rt::futures::yield_now().await;
          least_yield_ns = least_yield_ns.min(slates_rt::futures::now_ns().saturating_sub(before));
        }
        let began = slates_rt::futures::now_ns();
        let mut slices = RecoverySlices::begin(1);
        for _ in 0..UNITS {
          let unit_began = slates_rt::futures::now_ns();
          while slates_rt::futures::now_ns().saturating_sub(unit_began) < least_yield_ns {
            std::hint::spin_loop();
          }
          slices.unit_done().await;
        }
        let sliced = slices.finish();
        let elapsed = slates_rt::futures::now_ns().saturating_sub(began);
        let _ = report.try_send((least_yield_ns, sliced.slices, elapsed));
      })
      .unwrap();
      slates_rt::futures::detach(recovery).unwrap();
    });
    let (least_yield_ns, slices, elapsed_ns) = report_seen.recv_timeout(REPORT_WAIT).unwrap();
    let yielding_ns = slices.saturating_sub(1).saturating_mul(least_yield_ns);
    assert!(
      yielding_ns.saturating_mul(100) <= elapsed_ns.saturating_mul(ALLOWED_RISE_PERCENT),
      "{slices} slices of a cheapest yield of {least_yield_ns} ns are {yielding_ns} ns of a recovery of {elapsed_ns} ns"
    );
  }
}
