//! Mach's time-constraint scheduling policy for the calling thread (macOS; 84a, A-831). A thread under
//! `THREAD_TIME_CONSTRAINT_POLICY` is in the real-time band: woken, it runs ahead of the time-share threads beside it,
//! where a time-share thread woken behind busy neighbours waits out their quanta. No privilege is needed. The kernel
//! refuses a computation under its minimum real-time quantum (50 µs on Apple silicon) or a constraint under the
//! computation, and demotes a thread that keeps running past its computation, so only threads that park take it: a
//! latency-tracking runtime's shards (`slates_rt`) and a client's synchronous reply wait (`slates_ipc`).
//!
//! **Measured (BENCHMARKS "84a on macOS: Mach's time-constraint policy", CI macos-latest, runs 44934544995 and
//! 44934809949).** With the shards and the client's reply wait under it, a full edit and merge's loaded accept p99 fell
//! from 14.0-17.4 ms to 0.41-2.11 ms and its p999 from 16.6-20.8 ms to 0.70-4.52 ms across six rounds; the busy path's
//! zero-timeout harvests of 1 ms or more fell from 8.7-12.2% to 0.001-0.03%; quiet medians and R9's uncontended p99
//! unchanged.

/// The policy's terms, nanoseconds: `period` (0: aperiodic), the `computation` a wake needs, and the `constraint` it must
/// finish within; preemptible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeConstraint {
  /// The period, nanoseconds; 0 for aperiodic.
  pub period_ns: u64,
  /// The CPU time one wake needs, nanoseconds.
  pub computation_ns: u64,
  /// The time from the wake within which that computation must be done, nanoseconds.
  pub constraint_ns: u64,
}

/// Puts the calling thread under an aperiodic policy whose computation and constraint are `from_ns`, doubled until the
/// kernel takes them (it refuses less than its minimum real-time quantum, which is the device's, not a constant): the
/// computation it took, or `None` if it took none up to [`LONGEST_COMPUTATION_NS`].
pub fn set_current_from(from_ns: u64) -> Option<u64> {
  let mut computation_ns = from_ns.max(1);
  while computation_ns <= LONGEST_COMPUTATION_NS {
    let terms = TimeConstraint {
      period_ns: 0,
      computation_ns,
      constraint_ns: computation_ns,
    };
    if set_current(terms) {
      return Some(computation_ns);
    }
    computation_ns = computation_ns.saturating_mul(2);
  }
  None
}

/// Format: the longest computation asked for, nanoseconds: the kernel's maximum real-time quantum (50 ms, xnu's
/// `max_rt_quantum`), past which it refuses any.
pub const LONGEST_COMPUTATION_NS: u64 = 50_000_000;

/// Puts the calling thread under `terms`; whether the kernel took them. `false` off macOS.
#[cfg(target_os = "macos")]
pub fn set_current(terms: TimeConstraint) -> bool {
  let mut policy = mach::policy(terms);
  // SAFETY: the calling thread's own mach port and a four-integer policy record of the stated count, on this frame.
  let rc = unsafe {
    mach::thread_policy_set(
      libc::pthread_mach_thread_np(libc::pthread_self()),
      mach::THREAD_TIME_CONSTRAINT_POLICY,
      policy.as_mut_ptr(),
      mach::THREAD_TIME_CONSTRAINT_POLICY_COUNT,
    )
  };
  rc == libc::KERN_SUCCESS
}

/// Whether the calling thread is still under a time-constraint policy (not demoted by the kernel's fail-safe); `None`
/// where it cannot be read (off macOS).
#[cfg(target_os = "macos")]
pub fn holds() -> Option<bool> {
  let mut policy = [0 as libc::c_int; 4];
  let mut count = mach::THREAD_TIME_CONSTRAINT_POLICY_COUNT;
  let mut get_default: libc::boolean_t = 0;
  // SAFETY: the calling thread's own mach port, a four-integer record and its count, and the default flag, on this frame.
  let rc = unsafe {
    mach::thread_policy_get(
      libc::pthread_mach_thread_np(libc::pthread_self()),
      mach::THREAD_TIME_CONSTRAINT_POLICY,
      policy.as_mut_ptr(),
      &raw mut count,
      &raw mut get_default,
    )
  };
  (rc == libc::KERN_SUCCESS).then_some(get_default == 0)
}

/// Off macOS: not taken.
#[cfg(not(target_os = "macos"))]
pub fn set_current(_terms: TimeConstraint) -> bool {
  false
}

/// Off macOS: unknown.
#[cfg(not(target_os = "macos"))]
pub fn holds() -> Option<bool> {
  None
}

#[cfg(target_os = "macos")]
mod mach {
  use super::TimeConstraint;

  /// Format: `THREAD_TIME_CONSTRAINT_POLICY` (mach/thread_policy.h).
  pub(super) const THREAD_TIME_CONSTRAINT_POLICY: libc::c_uint = 2;
  /// Format: its record's length in integers: period, computation, constraint, preemptible.
  pub(super) const THREAD_TIME_CONSTRAINT_POLICY_COUNT: libc::c_uint = 4;

  /// `mach_timebase_info_data_t`: Mach absolute time is `numer / denom` nanoseconds a unit.
  #[repr(C)]
  struct Timebase {
    numer: u32,
    denom: u32,
  }

  // libc's own binding is deprecated for the mach2 crate; one declaration here, as `crate::process_clock` does.
  unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    pub(super) fn thread_policy_set(
      thread: libc::mach_port_t,
      flavor: libc::c_uint,
      policy_info: *mut libc::c_int,
      count: libc::c_uint,
    ) -> libc::c_int;
    pub(super) fn thread_policy_get(
      thread: libc::mach_port_t,
      flavor: libc::c_uint,
      policy_info: *mut libc::c_int,
      count: *mut libc::c_uint,
      get_default: *mut libc::boolean_t,
    ) -> libc::c_int;
  }

  /// `ns` in Mach absolute time units, saturating at the record's 32 bits.
  fn ticks(ns: u64) -> libc::c_int {
    let mut timebase = Timebase { numer: 0, denom: 0 };
    // SAFETY: fills this frame's record of the C layout; a refusal leaves zeros, taken as 1 below.
    let _ = unsafe { mach_timebase_info(&raw mut timebase) };
    let (numer, denom) = (
      u128::from(timebase.numer.max(1)),
      u128::from(timebase.denom.max(1)),
    );
    let ticks = u128::from(ns)
      .saturating_mul(denom)
      .checked_div(numer)
      .unwrap_or(u128::MAX);
    // The record's fields are `uint32_t`, passed through an integer array: the bits of a u32 in a c_int.
    let ticks = u32::try_from(ticks).unwrap_or(u32::MAX);
    libc::c_int::from_ne_bytes(ticks.to_ne_bytes())
  }

  /// The policy record: period, computation, constraint, preemptible (true).
  pub(super) fn policy(terms: TimeConstraint) -> [libc::c_int; 4] {
    [
      ticks(terms.period_ns),
      ticks(terms.computation_ns),
      ticks(terms.constraint_ns),
      1,
    ]
  }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
  use super::*;

  /// 84a. Do: on a fresh thread, ask for an aperiodic policy of 50 µs computation and constraint (R9's floor); on
  /// another, one of 10 µs. Expect: the first taken and still held; the second refused, under the kernel's minimum
  /// real-time quantum, which is why the computation is never asked below R9's floor.
  #[test]
  fn the_kernel_takes_r9s_floor_and_refuses_less() {
    let floor = TimeConstraint {
      period_ns: 0,
      computation_ns: 50_000,
      constraint_ns: 50_000,
    };
    let (taken, held) = std::thread::spawn(move || (set_current(floor), holds()))
      .join()
      .unwrap();
    assert!(taken, "the kernel refused R9's floor");
    assert_eq!(held, Some(true));
    let less = TimeConstraint {
      period_ns: 0,
      computation_ns: 10_000,
      constraint_ns: 10_000,
    };
    assert!(
      !std::thread::spawn(move || set_current(less))
        .join()
        .unwrap(),
      "the kernel took a computation under its minimum real-time quantum"
    );
  }
}
