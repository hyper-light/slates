//! Linux's round-robin real-time class for the calling thread (84a's Linux half; §4.3; the counterpart of macOS's
//! [`crate::time_constraint`]). A woken time-share thread behind busy time-share neighbours runs only when the scheduler
//! lets it preempt one, and on CI's Linux runners that was a millisecond or more. Measured with the shards' parks
//! stamped (run 45017505026, 4 CPUs, two spinners a CPU):
//! - ubuntu-24.04-arm: a kicked shard ran 920 µs after its kick at the median and 2,990 µs at p99, in about 22,000 of
//!   35,000 kicks a round, its kick on time.
//! - ubuntu-latest: a runnable, spinning shard took a request 2.8-3.9 ms after it landed, in the p99 tail.
//!
//! A thread in `SCHED_RR` runs ahead of every time-share thread as soon as it is runnable.
//!
//! **Grant or skip, never a privilege (R10).** The thread takes the class only where the account's `RLIMIT_RTPRIO`
//! grants a priority, at the class's lowest (`sched_get_priority_min`), with `SCHED_RESET_ON_FORK`, so a child the
//! daemon spawns (a hook) starts time-share. It also requires `RLIMIT_RTTIME` to be unlimited. That limit sends
//! `SIGXCPU` to a real-time thread that runs past it without blocking, and that signal ends the process. Anywhere else
//! the thread stays time-share, and [`take_current`] says why.
//!
//! **Never spin at real-time priority.** A shard that holds the class parks when it has no work: its idle window and
//! its closed-loop holds are off (`slates_rt`'s shard). A spinning real-time thread would take its CPU from every
//! time-share task, and the kernel's real-time throttling would then stall it.

/// Why a thread did not take the round-robin class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declined {
  /// The account's `RLIMIT_RTPRIO` soft limit grants no real-time priority, or less than the class's lowest.
  NoPriorityGranted {
    /// The soft limit, `None` for unlimited.
    limit: Option<u64>,
  },
  /// `RLIMIT_RTTIME` is set: a real-time thread that runs past it without blocking is signalled `SIGXCPU`.
  RunTimeLimited {
    /// The soft limit, microseconds.
    limit_us: u64,
  },
  /// The kernel refused the class, with its `errno`.
  Refused {
    /// The `errno`.
    errno: i32,
  },
  /// Not Linux.
  Unsupported,
}

/// Puts the calling thread in `SCHED_RR` at the class's lowest priority, reset on fork, where the account's limits allow
/// it (the module doc): the priority it took, or why it took none.
#[cfg(target_os = "linux")]
pub fn take_current() -> Result<i32, Declined> {
  use rustix::process::{Resource, getrlimit};
  let rttime = getrlimit(Resource::Rttime).current;
  if let Some(limit_us) = rttime {
    return Err(Declined::RunTimeLimited { limit_us });
  }
  // SAFETY: a pure query of the kernel's priority range for a policy constant; no memory is passed.
  let lowest = unsafe { libc::sched_get_priority_min(libc::SCHED_RR) };
  let granted = getrlimit(Resource::Rtprio).current;
  let lowest_u64 = u64::try_from(lowest).unwrap_or(u64::MAX);
  if lowest < 0 || granted.is_some_and(|limit| limit < lowest_u64) {
    return Err(Declined::NoPriorityGranted { limit: granted });
  }
  let param = libc::sched_param {
    sched_priority: lowest,
  };
  // SAFETY: pid 0 is the calling thread; `param` is a valid `sched_param` on this frame, read for the call only.
  let rc = unsafe {
    libc::sched_setscheduler(
      0,
      libc::SCHED_RR | libc::SCHED_RESET_ON_FORK,
      &raw const param,
    )
  };
  if rc == 0 {
    Ok(lowest)
  } else {
    Err(Declined::Refused {
      errno: std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
    })
  }
}

/// Whether the calling thread is in `SCHED_RR` now; `None` where it cannot be read (off Linux).
#[cfg(target_os = "linux")]
pub fn holds() -> Option<bool> {
  // SAFETY: pid 0 is the calling thread; the call reads no memory of ours.
  let policy = unsafe { libc::sched_getscheduler(0) };
  (policy >= 0).then_some(policy & !libc::SCHED_RESET_ON_FORK == libc::SCHED_RR)
}

/// Off Linux: not taken.
#[cfg(not(target_os = "linux"))]
pub fn take_current() -> Result<i32, Declined> {
  Err(Declined::Unsupported)
}

/// Off Linux: unknown.
#[cfg(not(target_os = "linux"))]
pub fn holds() -> Option<bool> {
  None
}
