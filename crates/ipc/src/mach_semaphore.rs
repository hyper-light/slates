//! A client's reply wake on macOS as a Mach semaphore (84a's macOS arm, w-perf 2026-11-22; §4.7 "Wake strategy").
//!
//! **What it answers.** A shard's reply wake of a parked client (`os_sync_wake_by_address_any` on the region's shared
//! wake word) spent 1 to 33 ms of the shard's own CPU in the kernel on CI's macOS runners, about one wake in a hundred
//! under load: 1,451 of 1,504 wakes over 1 ms were on CPU for at least 90% of their time (run 45074388808), and a
//! spindump put 69 of the wake's 106 samples at one kernel instruction (run 45082342692). A probe reproduced it where a
//! wake meets its waiter entering its wait, in bursts, with or without load (run 45086631612): the address wake and a
//! pipe spun so, a Mach semaphore's signal did not (worst 1.7 ms, mostly off CPU). The client parks at once (A-799), so
//! its reply meets it entering the wait on most requests.
//!
//! **The shape.** The daemon makes one semaphore with each region. A synchronous park waits on it after publishing its
//! parked flag and re-checking the ring, and the daemon signals it, in place of the address wake, for a client parked
//! synchronously. A semaphore counts its signals, so a signal made before the wait is not lost; one made for a reply
//! the client's re-check already found leaves a count that ends the next park early, which the client's loop answers
//! by looking at its ring again. An async SDK's park keeps the address wake, which its completion bridge waits on.
//!
//! **Diagnostic arm.** The client finds the semaphore by its port name in the region's header, which names it only in
//! the daemon's own task: the A/B runs `edit_merge_bench`, whose daemon is in the client's process. A landing passes the
//! semaphore's send right with the region's fileport (`crate::mach_claim`).

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
  fn semaphore_create(task: Port, semaphore: *mut Port, policy: i32, value: i32) -> KernReturn;
  fn semaphore_destroy(task: Port, semaphore: Port) -> KernReturn;
  fn semaphore_signal(semaphore: Port) -> KernReturn;
  fn semaphore_wait(semaphore: Port) -> KernReturn;
  fn semaphore_timedwait(semaphore: Port, wait_time: MachTimespec) -> KernReturn;
}

/// A region's wake semaphore: made and destroyed by the daemon (`owned`), named by the client.
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

  /// The semaphore a region's header names (the daemon's task's name; see the module doc), not this end's to destroy.
  /// `None` for a header that names none.
  pub(crate) fn named(port: Port) -> Option<Semaphore> {
    (port != 0).then_some(Semaphore { port, owned: false })
  }

  /// The port name, for the region's header.
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

  /// Waits for a signal up to `timeout_ns` (`None`: until signaled): `true` when signaled or interrupted (the caller
  /// looks at its ring either way), `false` at the timeout.
  pub(crate) fn wait(&self, timeout_ns: Option<u64>) -> Result<bool, IpcError> {
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
      KERN_SUCCESS | KERN_ABORTED => Ok(true),
      KERN_OPERATION_TIMED_OUT => Ok(false),
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
      // SAFETY: this task made the semaphore and destroys it once, at the region's end.
      let _ = unsafe { semaphore_destroy(mach_task_self_, self.port) };
    }
  }
}
