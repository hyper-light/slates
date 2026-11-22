//! Linux's EEVDF requested slice for the calling thread (84a's Linux half, arm E; §4.3). A fair-class thread may ask
//! for its own slice through `sched_setattr`'s `sched_runtime` (Linux 6.12 and later), with no privilege: the unprivileged
//! checks of a fair policy judge its nice alone (`user_check_sched_setscheduler`, kernel/sched/syscalls.c), and
//! `sched_getattr` reports the slice back. With `PREEMPT_SHORT` on (its default, and on CI's runners) a waking task
//! with a shorter slice may cancel the running task's `RUN_TO_PARITY` protection, so it can preempt where a default-slice
//! wakeup waited for the running task's slice.
//!
//! Measured first (CI's Linux runners, kernel 7.0, `CONFIG_PREEMPT_LAZY`, base slice 2.1 ms, two spinners a CPU; runs
//! 45017505026 and 45020845000): a kicked shard ran 920 µs after its kick at the median, and a client woken by its reply
//! waited whole slices. The round-robin real-time class cut the shard's 920 µs to 8 µs but needs a grant the runners do
//! not give, and costs the spinners their share (BENCHMARKS "84a on Linux: the round-robin real-time class"). The
//! requested slice stays in the fair class, its weight unchanged, so neighbours keep their share.
//!
//! The slice asked for is derived, never a literal (R3): R9's provisioning floor, the window a shard spins before it
//! parks, which the kernel raises to its own least slice where it is shorter.

/// The kernel's `struct sched_attr` (sched_setattr(2)), its first version: what `sched_getattr` reads of a thread's whole
/// scheduling and `sched_setattr` puts back.
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

/// Format: `SCHED_ATTR_SIZE_VER0`, the first version's size in bytes.
#[cfg(target_os = "linux")]
const SCHED_ATTR_SIZE: u32 = 48;

/// Why a thread's slice was not set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceSkipped {
  /// The thread is not in the plain fair class (`SCHED_OTHER` or `SCHED_BATCH`), so it keeps its own scheduling.
  NotFair,
  /// The thread's scheduling could not be read.
  Unreadable {
    /// The `errno`.
    errno: i32,
  },
  /// The kernel refused the slice (a kernel before 6.12 ignores it rather than refusing).
  Refused {
    /// The `errno`.
    errno: i32,
  },
  /// Not Linux.
  Unsupported,
}

/// The calling thread's last `errno`.
#[cfg(target_os = "linux")]
fn last_errno() -> i32 {
  std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The calling thread's whole scheduling.
#[cfg(target_os = "linux")]
fn read() -> Result<SchedAttr, SliceSkipped> {
  let mut attr = SchedAttr {
    size: SCHED_ATTR_SIZE,
    ..SchedAttr::default()
  };
  // SAFETY: pid 0 is the calling thread; `attr` is a `sched_attr` of the stated size on this frame, which the kernel
  // fills; flags 0, as the call requires.
  let rc = unsafe {
    libc::syscall(
      libc::SYS_sched_getattr,
      0,
      &raw mut attr,
      SCHED_ATTR_SIZE,
      0,
    )
  };
  if rc == 0 {
    Ok(attr)
  } else {
    Err(SliceSkipped::Unreadable {
      errno: last_errno(),
    })
  }
}

/// Puts `attr` on the calling thread.
#[cfg(target_os = "linux")]
fn write(attr: &SchedAttr) -> Result<(), i32> {
  // SAFETY: pid 0 is the calling thread; `attr` is a `sched_attr` of its stated size, read for the call only; flags 0.
  let rc = unsafe { libc::syscall(libc::SYS_sched_setattr, 0, attr as *const SchedAttr, 0) };
  if rc == 0 { Ok(()) } else { Err(last_errno()) }
}

/// The calling thread's scheduling with its slice asked to be `slice_ns`, its policy, flags and nice kept; `NotFair`
/// for a thread outside the plain fair class.
#[cfg(target_os = "linux")]
fn with_slice(slice_ns: u64) -> Result<(SchedAttr, SchedAttr), SliceSkipped> {
  let prior = read()?;
  let other = u32::try_from(libc::SCHED_OTHER).unwrap_or(u32::MAX);
  let batch = u32::try_from(libc::SCHED_BATCH).unwrap_or(u32::MAX);
  if prior.sched_policy != other && prior.sched_policy != batch {
    return Err(SliceSkipped::NotFair);
  }
  let asked = SchedAttr {
    sched_runtime: slice_ns.max(1),
    ..prior
  };
  Ok((prior, asked))
}

/// Asks the kernel for `slice_ns` as the calling thread's slice (a shard's, for good): the slice it reports back, which
/// it may have raised to its least.
#[cfg(target_os = "linux")]
pub fn set_current(slice_ns: u64) -> Result<u64, SliceSkipped> {
  let (_, asked) = with_slice(slice_ns)?;
  write(&asked).map_err(|errno| SliceSkipped::Refused { errno })?;
  Ok(read()?.sched_runtime)
}

/// A thread's own scheduling, read before a park whose slice was asked, put back exactly by [`ParkSlice::restore`].
/// `sched_getattr` reports a fair thread's slice whether or not it was asked, so the restore asks the slice the thread
/// had: the same slice, from then on held by the thread itself.
#[derive(Debug)]
pub struct ParkSlice {
  /// The thread's whole scheduling before the park.
  #[cfg(target_os = "linux")]
  prior: SchedAttr,
}

/// Asks `slice_ns` as the calling thread's slice for one park (a client's synchronous reply wait, 84a's arm E), its whole
/// scheduling read first to be put back after ([`ParkSlice::restore`]).
#[cfg(target_os = "linux")]
pub fn raise_for_park(slice_ns: u64) -> Result<ParkSlice, SliceSkipped> {
  let (prior, asked) = with_slice(slice_ns)?;
  write(&asked).map_err(|errno| SliceSkipped::Refused { errno })?;
  Ok(ParkSlice { prior })
}

impl ParkSlice {
  /// Puts the thread's own scheduling back as [`raise_for_park`] read it; a refusal is the `errno` (no privilege is
  /// needed to put back a fair thread's own slice and nice).
  #[cfg(target_os = "linux")]
  pub fn restore(self) -> Result<(), i32> {
    write(&self.prior)
  }

  /// Off Linux nothing was asked.
  #[cfg(not(target_os = "linux"))]
  pub fn restore(self) -> Result<(), i32> {
    Ok(())
  }
}

/// Off Linux: no slice to ask.
#[cfg(not(target_os = "linux"))]
pub fn set_current(_slice_ns: u64) -> Result<u64, SliceSkipped> {
  Err(SliceSkipped::Unsupported)
}

/// Off Linux: no slice to ask.
#[cfg(not(target_os = "linux"))]
pub fn raise_for_park(_slice_ns: u64) -> Result<ParkSlice, SliceSkipped> {
  Err(SliceSkipped::Unsupported)
}

/// The calling thread's slice now, nanoseconds, as the kernel reports it; `None` where it cannot be read.
#[cfg(target_os = "linux")]
pub fn current() -> Option<u64> {
  read().ok().map(|attr| attr.sched_runtime)
}

/// Off Linux: none.
#[cfg(not(target_os = "linux"))]
pub fn current() -> Option<u64> {
  None
}
