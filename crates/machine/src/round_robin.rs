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

/// The real-time priority this account's limits grant a thread (the module doc): the class's lowest, where
/// `RLIMIT_RTPRIO` grants at least it and `RLIMIT_RTTIME` is unlimited; or why none.
#[cfg(target_os = "linux")]
pub fn granted_priority() -> Result<i32, Declined> {
  use rustix::process::{Resource, getrlimit};
  if let Some(limit_us) = getrlimit(Resource::Rttime).current {
    return Err(Declined::RunTimeLimited { limit_us });
  }
  // SAFETY: a pure query of the kernel's priority range for a policy constant; no memory is passed.
  let lowest = unsafe { libc::sched_get_priority_min(libc::SCHED_RR) };
  let granted = getrlimit(Resource::Rtprio).current;
  let lowest_u64 = u64::try_from(lowest).unwrap_or(u64::MAX);
  if lowest < 0 || granted.is_some_and(|limit| limit < lowest_u64) {
    return Err(Declined::NoPriorityGranted { limit: granted });
  }
  Ok(lowest)
}

/// Puts the calling thread in `SCHED_RR` at `priority`, reset on fork.
#[cfg(target_os = "linux")]
fn set_round_robin(priority: i32, reset_on_fork: bool) -> Result<(), Declined> {
  let param = libc::sched_param {
    sched_priority: priority,
  };
  let policy = if reset_on_fork {
    libc::SCHED_RR | libc::SCHED_RESET_ON_FORK
  } else {
    libc::SCHED_RR
  };
  // SAFETY: pid 0 is the calling thread; `param` is a valid `sched_param` on this frame, read for the call only.
  let rc = unsafe { libc::sched_setscheduler(0, policy, &raw const param) };
  if rc == 0 {
    Ok(())
  } else {
    Err(Declined::Refused {
      errno: last_errno(),
    })
  }
}

/// The calling thread's last `errno`.
#[cfg(target_os = "linux")]
fn last_errno() -> i32 {
  std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Puts the calling thread in `SCHED_RR` at the class's lowest priority, reset on fork, where the account's limits allow
/// it (the module doc): the priority it took, or why it took none.
#[cfg(target_os = "linux")]
pub fn take_current() -> Result<i32, Declined> {
  let priority = granted_priority()?;
  // A shard's thread holds the class for good, so a child it spawns (a hook) must start time-share.
  set_round_robin(priority, true)?;
  Ok(priority)
}

/// The kernel's `struct sched_attr` (sched_setattr(2)), its first version: what `sched_getattr` reads of a thread's
/// whole scheduling (policy, flags, nice, priority and a deadline thread's terms) and `sched_setattr` puts back.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SchedAttr {
  size: u32,
  sched_policy: u32,
  sched_flags: u64,
  sched_nice: i32,
  sched_priority: u32,
  sched_runtime: u64,
  sched_deadline: u64,
  sched_period: u64,
}

/// Format: `SCHED_ATTR_SIZE_VER0`, the first version's size in bytes, which every kernel since `sched_setattr` (3.14)
/// reads and writes.
#[cfg(target_os = "linux")]
const SCHED_ATTR_SIZE: u32 = 48;

/// Format: `SCHED_DEADLINE`, the deadline class's policy number (the libc crate names no constant for it).
#[cfg(target_os = "linux")]
const SCHED_DEADLINE: u32 = 6;

/// A thread's own scheduling, read before a real-time reply wait, and put back exactly by [`WaitClass::restore`].
#[derive(Debug)]
pub struct WaitClass {
  /// The thread's whole scheduling before the wait.
  #[cfg(target_os = "linux")]
  prior: SchedAttr,
}

/// Why a reply wait stayed in its thread's own class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitSkipped {
  /// The account's limits grant no real-time class, or the kernel refused it ([`Declined`]).
  Declined(Declined),
  /// The thread is in a real-time class of its own already, which it keeps.
  AlreadyRealTime,
  /// The thread's scheduling could not be read, so it could not be put back: the wait stays as it is.
  Unreadable {
    /// The `errno`.
    errno: i32,
  },
}

#[cfg(target_os = "linux")]
std::thread_local! {
  /// This thread's grant, decided at its first real-time wait: the priority, or why none.
  static GRANT: std::cell::Cell<Option<Result<i32, Declined>>> = const { std::cell::Cell::new(None) };
}

/// Raises the calling thread to `SCHED_RR` for one reply wait (a client's synchronous wait, 84a's Linux half, A-872):
/// its whole scheduling read first (`sched_getattr`), to be put back exactly once the wait returns
/// ([`WaitClass::restore`]). A thread already in a real-time class keeps its own; one the account's limits deny stays as
/// it is (decided once a thread). A woken time-share thread behind busy neighbours waited out their slices for its
/// reply: on CI's Linux runners, with the shards in the class, the client's wake was 2.5-3.0 ms in the p999 tail (run
/// 45020845000). Held for the whole wait, not a park: raised a park at a time, the thread went back to time-share as
/// each park returned, before it had taken its reply, and on bare metal the loaded tail's misses sat one tick over
/// (adacachy2, main's A/B of 2026-11-23).
#[cfg(target_os = "linux")]
pub fn raise_for_wait() -> Result<WaitClass, WaitSkipped> {
  let grant = GRANT.with(|cell| {
    let grant = cell.get().unwrap_or_else(granted_priority);
    cell.set(Some(grant));
    grant
  });
  let priority = grant.map_err(WaitSkipped::Declined)?;
  let mut prior = SchedAttr {
    size: SCHED_ATTR_SIZE,
    ..SchedAttr::default()
  };
  // SAFETY: pid 0 is the calling thread; `prior` is a `sched_attr` of the stated size on this frame, which the kernel
  // fills; flags 0, as the call requires.
  let read = unsafe {
    libc::syscall(
      libc::SYS_sched_getattr,
      0,
      &raw mut prior,
      SCHED_ATTR_SIZE,
      0,
    )
  };
  if read != 0 {
    return Err(WaitSkipped::Unreadable {
      errno: last_errno(),
    });
  }
  let rr = u32::try_from(libc::SCHED_RR).unwrap_or(u32::MAX);
  let fifo = u32::try_from(libc::SCHED_FIFO).unwrap_or(u32::MAX);
  if [rr, fifo, SCHED_DEADLINE].contains(&prior.sched_policy) {
    return Err(WaitSkipped::AlreadyRealTime);
  }
  // Without `SCHED_RESET_ON_FORK`: an unprivileged thread cannot clear that flag once set (sched(7)), so a wait raised
  // with it could never be put back (CI ubuntu-24.04-arm, run 45023871879: every restore refused EPERM). The thread is
  // blocked in its wait for the whole raise and forks nothing meanwhile, so the flag buys nothing here.
  set_round_robin(priority, false).map_err(WaitSkipped::Declined)?;
  Ok(WaitClass { prior })
}

impl WaitClass {
  /// Puts the thread's own scheduling back exactly as [`raise_for_wait`] read it. A refusal is the `errno`, and the
  /// thread is then lowered to the plain time-share class, so it never runs its caller's code at real-time priority;
  /// lowering one's own thread asks no privilege.
  #[cfg(target_os = "linux")]
  pub fn restore(self) -> Result<(), i32> {
    // SAFETY: pid 0 is the calling thread; `prior` is the `sched_attr` read from it, of its stated size; flags 0.
    let put = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, &raw const self.prior, 0) };
    if put == 0 {
      return Ok(());
    }
    let errno = last_errno();
    let plain = libc::sched_param { sched_priority: 0 };
    // Lowered to time-share keeping `SCHED_RESET_ON_FORK`, which an unprivileged thread may set but never clear, so the
    // lowering cannot be refused for the flag (CI run 45023871879: a plain `SCHED_OTHER` was).
    // SAFETY: as in `set_round_robin`: the calling thread, a valid `sched_param` on this frame.
    let _ = unsafe {
      libc::sched_setscheduler(
        0,
        libc::SCHED_OTHER | libc::SCHED_RESET_ON_FORK,
        &raw const plain,
      )
    };
    Err(errno)
  }

  /// Off Linux nothing was raised.
  #[cfg(not(target_os = "linux"))]
  pub fn restore(self) -> Result<(), i32> {
    Ok(())
  }
}

/// Off Linux: no class to raise to.
#[cfg(not(target_os = "linux"))]
pub fn raise_for_wait() -> Result<WaitClass, WaitSkipped> {
  Err(WaitSkipped::Declined(Declined::Unsupported))
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

/// Off Linux: no real-time class is granted here.
#[cfg(not(target_os = "linux"))]
pub fn granted_priority() -> Result<i32, Declined> {
  Err(Declined::Unsupported)
}

/// Off Linux: unknown.
#[cfg(not(target_os = "linux"))]
pub fn holds() -> Option<bool> {
  None
}
