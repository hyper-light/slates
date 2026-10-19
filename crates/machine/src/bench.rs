//! The measurement harness: run an operation until its bootstrapped interval converges or a
//! wall-time bound is hit, batching sub-resolution operations so the timer's own cost stays a
//! stated fraction of every reading.
//!
//! Two shape constants live here, both ratified in the gap ledger (GAPS §5): the per-probe
//! wall-time bound, so a slow machine still boots, and the timer-overhead fraction that sets the
//! batch size. Everything else is derived from what the harness itself measures first: the cost
//! of reading the clock.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::stats::{Interval, Percentile, Sample, Xorshift, bootstrap_interval, converged};

/// Shape: the wall-time bound per probe. The boot profile must not dominate daemon start; with
/// the dozen probes of §4.1 this bounds a slow machine's full profile to a few seconds, after
/// which the profile is marked "quick" and its wider intervals are reported (GAPS §5).
pub const PROBE_WALL_BUDGET: Duration = Duration::from_millis(250);

/// Shape: a reading must be at least this many times the timer's own cost so that the timer
/// contributes at most one percent of it (lmbench's rule, GAPS §5).
pub const TIMER_OVERHEAD_FACTOR: u64 = 100;

/// The process's involuntary context switches so far (`getrusage`'s `ru_nivcsw`): a stretch of work during
/// which this count moved was taken off the CPU by the scheduler, and its wall time is the machine's doing
/// rather than the code's.
///
/// This is the second of the three busy-machine discriminators (`GAPS.md`, 2026-10-18), and it lives here
/// because it is a measurement primitive every crate's timing assertions need. It was copied by hand into
/// three places first (`crates/vfs/examples/vfs_bench.rs`, `crates/client/tests/driver.rs`,
/// `crates/ipc/tests/rendezvous.rs`), and a fourth site is what made the copies worth collapsing — the same
/// way eleven copies of the profile-escalation ladder hid the fixture that failed sixty tests.
///
/// Use it to attribute a tight "was this fast" bound: assert the bound **or** that the count moved. A
/// *sleeping* sampler's wake lateness is not a substitute — it measures a different quantity and under-covers
/// by a margin that varies with nothing in particular (measured: 20 ms against 65 ms for near-identical steps).
///
/// **`None` where the platform has no such counter** (Windows today), which a caller must tell apart from
/// `Some(0)`, "the scheduler did not take this thread away". A test that cannot attribute must not enforce a
/// tight bound it has no way to defend — it should say so and skip that assertion, the way this project skips
/// an env-gated test loudly. Returning a bare `0` there, as the three hand-copied versions of this did, quietly
/// turns "cannot tell" into "the code's own fault" on exactly the platform that is hardest to reproduce on.
#[cfg(unix)]
pub fn involuntary_switches() -> Option<u64> {
  let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
  // SAFETY: `getrusage` fills the `rusage` the pointer names and returns zero on success; `RUSAGE_SELF` is a
  // valid selector; the buffer is ours, fully sized and zeroed before the call.
  let usage = unsafe {
    if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
      return None;
    }
    usage.assume_init()
  };
  Some(u64::try_from(usage.ru_nivcsw).unwrap_or(0))
}

/// No counter on this platform: the caller cannot attribute, and is told so rather than handed a zero.
#[cfg(not(unix))]
pub fn involuntary_switches() -> Option<u64> {
  None
}

/// The involuntary switches since `before`, or `None` when the question cannot be answered on this platform.
/// Take `before` with [`involuntary_switches`], run the work, then ask this; `Some(0)` means the scheduler left
/// the thread alone, which is the reading that lets a tight timing bound stand.
pub fn involuntary_switches_since(before: Option<u64>) -> Option<u64> {
  Some(involuntary_switches()?.saturating_sub(before?))
}

/// Shape: the smallest sample the stopping rule may accept; below this the bootstrap has too few
/// distinct resamples to mean anything (Kalibera and Jones's minimum repetition count).
pub const MIN_SAMPLES: usize = 16;

/// The result of measuring one operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurement {
  /// The 95% bootstrap interval around the median, in nanoseconds per operation.
  pub interval: Interval,
  /// The p99 reading, in nanoseconds per operation.
  pub p99_ns: u64,
  /// The smallest reading, in nanoseconds per operation.
  pub min_ns: u64,
  /// How many samples were taken.
  pub samples: u32,
  /// How many operations each sample batched.
  pub batch: u32,
  /// True when the wall-time bound stopped the probe before its interval converged.
  pub quick: bool,
}

impl Measurement {
  /// The median in nanoseconds per operation.
  pub const fn median_ns(&self) -> u64 {
    self.interval.median
  }
}

/// Measures the cost of reading the monotonic clock itself, in nanoseconds per read.
pub fn timer_overhead_ns() -> u64 {
  /// Shape: enough back-to-back clock reads to amortize the loop and any first-call warmup.
  const READS: u32 = 4096;
  let start = Instant::now();
  let mut last = start;
  for _ in 0..READS {
    last = Instant::now();
  }
  let total = last.saturating_duration_since(start);
  nanos(total).checked_div(u64::from(READS)).unwrap_or(0)
}

/// Nanoseconds in a duration, saturating at `u64::MAX` (no narrowing cast).
pub fn nanos(d: Duration) -> u64 {
  u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Runs `op` until the interval around its per-operation median converges or the wall budget
/// ends. `op` is batched so that one sample lasts at least `TIMER_OVERHEAD_FACTOR` times the
/// measured timer overhead.
pub fn measure<F: FnMut()>(mut op: F, budget: Duration) -> Measurement {
  let overhead = timer_overhead_ns();
  let floor = overhead.saturating_mul(TIMER_OVERHEAD_FACTOR).max(1);
  let started = Instant::now();
  let mut batch: u32 = 1;
  let mut sample = Sample::new(Vec::new());
  let mut rng = Xorshift::new(Xorshift::SEED);
  loop {
    let t = Instant::now();
    for _ in 0..batch {
      op();
    }
    let elapsed = nanos(t.elapsed());
    if elapsed < floor && batch < u32::MAX >> 1 {
      batch = batch.saturating_mul(2);
      continue;
    }
    if elapsed < floor {
      // The operation is unmeasurable at any batch (it was optimized away or costs nothing);
      // report what was seen, marked quick, rather than a converged zero.
      sample.push(elapsed.checked_div(u64::from(batch)).unwrap_or(elapsed));
      break;
    }
    sample.push(elapsed.checked_div(u64::from(batch)).unwrap_or(elapsed));
    if sample.len() >= MIN_SAMPLES
      && let Some(interval) = bootstrap_interval(&sample, &mut rng)
      && converged(&interval)
    {
      return finish(&sample, interval, batch, false);
    }
    if started.elapsed() >= budget {
      break;
    }
  }
  let interval = bootstrap_interval(&sample, &mut rng).unwrap_or(Interval {
    median: 0,
    lower: 0,
    upper: 0,
  });
  finish(&sample, interval, batch, true)
}

fn finish(sample: &Sample, interval: Interval, batch: u32, quick: bool) -> Measurement {
  Measurement {
    interval,
    p99_ns: sample.percentile(Percentile::P99).unwrap_or(0),
    min_ns: sample.min().unwrap_or(0),
    samples: u32::try_from(sample.len()).unwrap_or(u32::MAX),
    batch,
    quick,
  }
}

/// Throughput in bytes per second from bytes processed and nanoseconds elapsed; `u128` arithmetic
/// so the product never overflows, saturating on a zero elapsed time.
pub fn bytes_per_second(bytes: u64, elapsed_ns: u64) -> u64 {
  if elapsed_ns == 0 {
    return u64::MAX;
  }
  /// Format: nanoseconds per second.
  const NANOS_PER_SECOND: u128 = 1_000_000_000;
  let bps = u128::from(bytes)
    .saturating_mul(NANOS_PER_SECOND)
    .checked_div(u128::from(elapsed_ns))
    .unwrap_or(u128::MAX);
  u64::try_from(bps).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn measuring_a_trivial_operation_batches_and_converges_or_stops_at_the_budget() {
    let mut counter = 0u64;
    let m = measure(
      || counter = counter.wrapping_add(1),
      Duration::from_millis(200),
    );
    assert!(m.batch >= 1);
    assert!(m.samples >= 1);
    assert!(m.interval.lower <= m.interval.median && m.interval.median <= m.interval.upper);
    assert!(counter > 0);
  }

  #[test]
  // A test may sleep: the wall bound around a slow operation is what is under test.
  #[allow(clippy::disallowed_methods)]
  fn the_budget_bounds_a_slow_operation_and_marks_it_quick() {
    let m = measure(
      || std::thread::sleep(Duration::from_millis(30)),
      Duration::from_millis(60),
    );
    assert!(m.quick, "{m:?}");
    assert!(m.median_ns() >= 30_000_000, "{m:?}");
  }

  #[test]
  fn throughput_arithmetic_never_overflows() {
    assert_eq!(
      bytes_per_second(1_000_000_000, 1_000_000_000),
      1_000_000_000
    );
    assert_eq!(bytes_per_second(u64::MAX, 1), u64::MAX);
    assert_eq!(bytes_per_second(1, 0), u64::MAX);
  }
}
