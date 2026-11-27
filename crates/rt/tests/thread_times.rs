//! The calling thread's user and system CPU time read apart (`registry::calling_thread_user_system_ns`, A-920): what
//! splits a long unit's CPU into the kernel's work and the thread's own, where the platform keeps them per thread.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::{Duration, Instant};

/// Shape: how long the test spins on the CPU in user code: enough for a microsecond-grained clock to move.
const SPIN: Duration = Duration::from_millis(20);

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
  let began = Instant::now();
  let mut spun = 0u64;
  while began.elapsed() < SPIN {
    spun = std::hint::black_box(spun.wrapping_add(1));
  }
  let (user_after, system_after) = slates_rt::registry::calling_thread_user_system_ns().unwrap();
  assert!(system_after >= system_before, "the system time went back");
  let user = user_after.saturating_sub(user_before);
  let spin = u64::try_from(SPIN.as_nanos()).unwrap();
  assert!(
    user >= spin / 2,
    "a {spin} ns spin added {user} ns of user time ({spun} turns)"
  );
}
