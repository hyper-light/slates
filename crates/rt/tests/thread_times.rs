//! The calling thread's user and system CPU time read apart (`registry::calling_thread_user_system_ns`, A-920): what
//! splits a long unit's CPU into the kernel's work and the thread's own, where the platform keeps them per thread.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

/// Shape: how much user CPU time the test spins for: enough for a microsecond-grained clock to move.
const SPIN: Duration = Duration::from_millis(20);
/// Shape: the wall time the spin may take to be given [`SPIN`] of the CPU, a hundred times the spin, so a machine loaded
/// past its cores (the norm here: load 20 to 100 on a 16-core Mac) still gives it; past it the test fails, naming the
/// user time it got.
const WALL_BOUND: Duration = Duration::from_secs(2);
/// Shape: the adds between two reads of the thread's clock, few enough that the spin reads it every few microseconds.
const SPIN_BATCH: u64 = 1_000;

/// A-920. Do: read the calling thread's user and system time, spin in user code for [`SPIN`], and read them again.
/// Expect: on macOS, the user time grown by most of the spin and never either time going back; elsewhere, no reading
/// (the platform keeps no split this reads), said loudly.
#[test]
#[cfg_attr(miri, ignore)]
fn a_threads_user_time_grows_with_its_own_spin() {
  let Some((user_before, system_before)) = slates_rt::registry::calling_thread_user_system_ns()
  else {
    if cfg!(target_os = "macos") {
      panic!("macOS reads a thread's split");
    }
    eprintln!("skipped: this platform keeps no per-thread user and system split read here");
    return;
  };
  // Spun by the thread's own clock, not the wall's: a loaded machine preempts the spin for most of a wall window (red at
  // load 20: a 20 ms wall spin added under half of it as user time).
  let spin = u64::try_from(SPIN.as_nanos()).unwrap();
  let began = Instant::now();
  let mut spun = 0u64;
  let (mut user_after, mut system_after) = (user_before, system_before);
  while user_after.saturating_sub(user_before) < spin && began.elapsed() < WALL_BOUND {
    for _ in 0..SPIN_BATCH {
      spun = std::hint::black_box(spun.wrapping_add(1));
    }
    (user_after, system_after) = slates_rt::registry::calling_thread_user_system_ns().unwrap();
  }
  assert!(system_after >= system_before, "the system time went back");
  let user = user_after.saturating_sub(user_before);
  assert!(
    user >= spin,
    "a spin of {} ms of wall added {user} ns of user time, short of {spin} ({spun} turns)",
    began.elapsed().as_millis()
  );
}
