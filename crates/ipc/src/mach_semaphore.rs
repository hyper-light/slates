//! A client's reply wake on macOS as a Mach semaphore (A-867; 84a; §4.7 "Wake strategy").
//!
//! **What it answers.** A shard's reply wake of a parked client (`os_sync_wake_by_address_any` on the region's shared
//! wake word) spent 1 to 33 ms of the shard's own CPU in the kernel on CI's macOS runners, about one wake in a hundred
//! under load: 1,451 of 1,504 wakes over 1 ms were on CPU for at least 90% of their time (run 45074388808), and a
//! spindump put 69 of the wake's 106 samples at one kernel instruction (run 45082342692). A probe reproduced it where a
//! wake meets its waiter entering its wait, in bursts, with or without load (run 45086631612): the address wake and a
//! pipe spun so, a Mach semaphore's signal did not (worst 1.7 ms, mostly off CPU). The client parks at once (A-799), so
//! its reply meets it entering the wait on most requests.
//!
//! **The shape.** The daemon makes one semaphore with each region and sends its send right with the region's fileport
//! to the instance's owner, which hands both to the claimant (`crate::mach_claim`). A synchronous park on that region
//! raises a parked flag of its own (`crate::region::PARKED_SEMAPHORE_SLOT_ZERO`), then waits on the semaphore after
//! re-checking its ring; the daemon signals the semaphore for such a park in place of the address wake. A semaphore
//! counts its signals, so a signal made before the wait is not lost; one made for a reply the client's re-check already
//! found leaves a count that ends its next park early, which the client's loop answers by looking at its ring again.
//! Every other park keeps the address wake: an async SDK's arm, whose completion bridge waits on the word, and any
//! mapping of a region that came without the semaphore (a second mapping, an open by name).
//!
//! **Measured.** CI's macOS runner, 8 rounds a arm, interleaved (run 45091701553): reply wakes over 1 ms 3,001 with the
//! address wake against 36 with the semaphore; the loaded edit and merge's p99 (median of rounds) accept 1,766 µs
//! against 976, conflict 4,301 against 1,363; quiet no worse. On bare metal (a Mac Studio, 8 runs) the address wake
//! never ran 1 ms: the spin is the CI VM's, a lock holder's vCPU descheduled.

use crate::error::IpcError;

/// Format: a Mach port name (`mach_port_t`).
type Port = u32;
/// Format: `kern_return_t`.
type KernReturn = i32;
/// Format: `KERN_SUCCESS`.
const KERN_SUCCESS: KernReturn = 0;
/// Format: `KERN_ABORTED`, a wait interrupted: the caller looks at its ring and parks again.
const KERN_ABORTED: KernReturn = 14;
/// Format: `KERN_OPERATION_TIMED_OUT`.
const KERN_OPERATION_TIMED_OUT: KernReturn = 49;
/// Format: `KERN_TERMINATED`: the semaphore was destroyed, as its daemon's region was (the daemon gone or restarted).
const KERN_TERMINATED: KernReturn = 37;
/// Format: `KERN_INVALID_NAME`: the name no longer names a semaphore in this task.
const KERN_INVALID_NAME: KernReturn = 15;
/// Format: `SYNC_POLICY_FIFO` (`<mach/sync_policy.h>`).
const SYNC_POLICY_FIFO: i32 = 0;
/// Format: nanoseconds a second, for the wait's `mach_timespec_t`.
const NS_PER_S: u64 = 1_000_000_000;

/// `mach_timespec_t` (`<mach/clock_types.h>`).
#[repr(C)]
#[derive(Clone, Copy)]
struct MachTimespec {
  tv_sec: u32,
  tv_nsec: i32,
}

unsafe extern "C" {
  static mach_task_self_: Port;
  fn mach_port_deallocate(task: Port, name: Port) -> KernReturn;
  fn semaphore_create(task: Port, semaphore: *mut Port, policy: i32, value: i32) -> KernReturn;
  fn semaphore_destroy(task: Port, semaphore: Port) -> KernReturn;
  fn semaphore_signal(semaphore: Port) -> KernReturn;
  fn semaphore_wait(semaphore: Port) -> KernReturn;
  fn semaphore_timedwait(semaphore: Port, wait_time: MachTimespec) -> KernReturn;
}

/// A region's wake semaphore: made and destroyed by the daemon (`owned`), or a send right to it the client received with
/// its region, given back when dropped.
#[derive(Debug)]
pub(crate) struct Semaphore {
  port: Port,
  owned: bool,
}

impl Semaphore {
  /// A fresh semaphore with no count, this task's to destroy.
  pub(crate) fn create() -> Result<Semaphore, IpcError> {
    let mut port: Port = 0;
    // SAFETY: this task's own port, read from the kernel-set global, and an out-parameter on this frame.
    let rc = unsafe { semaphore_create(mach_task_self_, &mut port, SYNC_POLICY_FIFO, 0) };
    if rc != KERN_SUCCESS {
      return Err(IpcError::OsRefused {
        call: "semaphore_create",
        code: Some(rc),
      });
    }
    Ok(Semaphore { port, owned: true })
  }

  /// The semaphore a send right received with a region names (`crate::mach_claim::fetch`): this task's to give back, not
  /// to destroy.
  pub(crate) fn received(port: Port) -> Semaphore {
    Semaphore { port, owned: false }
  }

  /// The port name in this task, for the daemon's keep (`crate::mach_claim::keep`), which copies a send right from it.
  pub(crate) fn port(&self) -> Port {
    self.port
  }

  /// Signals it: wakes the waiter, or leaves a count for its next wait.
  pub(crate) fn signal(&self) -> Result<(), IpcError> {
    // SAFETY: a semaphore port name this task holds for the region's life.
    let rc = unsafe { semaphore_signal(self.port) };
    if rc != KERN_SUCCESS {
      return Err(IpcError::OsRefused {
        call: "semaphore_signal",
        code: Some(rc),
      });
    }
    Ok(())
  }

  /// Waits for a signal up to `timeout_ns` (`None`: until signaled): `Some(true)` when signaled or interrupted (the
  /// caller looks at its ring either way), `Some(false)` at the timeout, and `None` when the semaphore is gone (its
  /// daemon's region was dropped), so the caller waits as it would with none.
  pub(crate) fn wait(&self, timeout_ns: Option<u64>) -> Result<Option<bool>, IpcError> {
    let rc = match timeout_ns {
      // SAFETY: a semaphore port name this task holds for the region's life.
      None => unsafe { semaphore_wait(self.port) },
      Some(ns) => {
        let wait_time = MachTimespec {
          tv_sec: u32::try_from(ns / NS_PER_S).unwrap_or(u32::MAX),
          tv_nsec: i32::try_from(ns % NS_PER_S).unwrap_or(0),
        };
        // SAFETY: as above; the time is passed by value.
        unsafe { semaphore_timedwait(self.port, wait_time) }
      }
    };
    match rc {
      KERN_SUCCESS | KERN_ABORTED => Ok(Some(true)),
      KERN_OPERATION_TIMED_OUT => Ok(Some(false)),
      KERN_TERMINATED | KERN_INVALID_NAME => Ok(None),
      code => Err(IpcError::OsRefused {
        call: "semaphore_wait",
        code: Some(code),
      }),
    }
  }
}

impl Drop for Semaphore {
  fn drop(&mut self) {
    if self.owned {
      // SAFETY: this task made the semaphore and destroys it once, at the region's end; a client's right to it then
      // names a dead port, whose wait is refused typed.
      let _ = unsafe { semaphore_destroy(mach_task_self_, self.port) };
    } else {
      // SAFETY: a send right this task received with its region and gives back once, at the mapping's end.
      let _ = unsafe { mach_port_deallocate(mach_task_self_, self.port) };
    }
  }
}
