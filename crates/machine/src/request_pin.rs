//! A client thread's synchronous request on its shard's CPU (84a's Linux arm F; §4.7 "Wake strategy"). Under Linux's
//! lazy preemption a fair wakeup waits for the tick only because its waker keeps running; when the waker blocks, its CPU
//! reschedules at once and EEVDF picks the earliest eligible deadline, which a short-sliced thread that mostly sleeps
//! holds (arm E's slices, `crate::fair_slice`). So a client that waits for its reply on the CPU its shard runs on makes
//! each hop a hand-off: it writes the request and blocks, and the shard is picked at that block; the shard writes the
//! reply and parks, and the client is picked at that park. Measured first (CI's Linux runners, PREEMPT_LAZY at HZ=1000,
//! run 45056790029): under arm E a kicked shard ran 998-999 µs after its kick at the p99, one tick a hop.
//!
//! The thread's own affinity is read before the pin and put back exactly after the request, so the caller's code runs
//! where it ran before; a pin the kernel refuses leaves the thread where it was.

/// Why a request was not pinned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinSkipped {
  /// The shard's CPU is not known (it has published none yet).
  Unknown,
  /// The shard's CPU is outside the thread's own mask (its cgroup or its caller narrowed it), so it stays put.
  OutsideMask,
  /// The thread's mask could not be read.
  Unreadable {
    /// The `errno`.
    errno: i32,
  },
  /// The kernel refused the pin.
  Refused {
    /// The `errno`.
    errno: i32,
  },
  /// Not Linux.
  Unsupported,
}

/// A request's pin: the thread's own mask, to be put back by [`RequestPin::restore`].
#[derive(Debug)]
pub struct RequestPin {
  #[cfg(target_os = "linux")]
  own: rustix::thread::CpuSet,
}

/// Pins the calling thread to `cpu` for one request, keeping its own mask to put back. `cpu` must be in the thread's own
/// mask; a thread already pinned there alone is pinned again at no harm.
#[cfg(target_os = "linux")]
pub fn pin_for_request(cpu: usize) -> Result<RequestPin, PinSkipped> {
  use rustix::thread::{CpuSet, sched_getaffinity, sched_setaffinity};
  let own = sched_getaffinity(None).map_err(|e| PinSkipped::Unreadable {
    errno: e.raw_os_error(),
  })?;
  if cpu >= CpuSet::MAX_CPU || !own.is_set(cpu) {
    return Err(PinSkipped::OutsideMask);
  }
  let mut only = CpuSet::new();
  only.set(cpu);
  sched_setaffinity(None, &only).map_err(|e| PinSkipped::Refused {
    errno: e.raw_os_error(),
  })?;
  Ok(RequestPin { own })
}

/// Not Linux: never pinned.
#[cfg(not(target_os = "linux"))]
pub fn pin_for_request(_cpu: usize) -> Result<RequestPin, PinSkipped> {
  Err(PinSkipped::Unsupported)
}

impl RequestPin {
  /// Puts the thread's own mask back; the `errno` of a refusal.
  #[cfg(target_os = "linux")]
  pub fn restore(self) -> Result<(), i32> {
    rustix::thread::sched_setaffinity(None, &self.own).map_err(|e| e.raw_os_error())
  }

  /// Not Linux: nothing was pinned.
  #[cfg(not(target_os = "linux"))]
  pub fn restore(self) -> Result<(), i32> {
    Ok(())
  }
}

/// The CPU the calling thread is running on now (`sched_getcpu`; Linux's rseq or vDSO read, no syscall on a current
/// glibc), or `None` where the OS has no such read.
pub fn current_cpu() -> Option<usize> {
  #[cfg(target_os = "linux")]
  {
    Some(rustix::thread::sched_getcpu())
  }
  #[cfg(not(target_os = "linux"))]
  {
    None
  }
}
