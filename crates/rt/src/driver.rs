//! The driver seam: every driver blocks until a kick, a completion or a deadline, and hands back
//! completions as `(user_data, result)` pairs; a `Kick` is the thread-safe handle any thread
//! uses to wake a shard's driver (§4.3, "the three OS drivers with a common completion seam").
//!
//! Phase 0 carries the seam itself, the kick, the wait with a deadline, and a no-op operation
//! whose completion proves the path; sockets, files and the bridge queues arrive with their
//! phases and use the same `wait`.

use std::ptr::NonNull;

use crate::error::RtError;
use crate::sim::SimShared;

/// A finished operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
  /// The word the submitter attached (a packed waker word, or a driver-private tag).
  pub user_data: u64,
  /// The operation's result as the OS reports it (bytes, or a negated errno).
  pub result: i32,
}

/// Which driver a shard runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriverKind {
  /// Linux io_uring.
  IoUring,
  /// Linux epoll with an eventfd kick.
  Epoll,
  /// macOS / BSD kqueue with an `EVFILT_USER` kick.
  Kqueue,
  /// Windows I/O completion ports.
  Iocp,
  /// The deterministic simulation.
  Simulation,
}

impl DriverKind {
  /// The name as the profile and the counters print it.
  pub const fn name(self) -> &'static str {
    match self {
      Self::IoUring => "io_uring",
      Self::Epoll => "epoll",
      Self::Kqueue => "kqueue",
      Self::Iocp => "iocp",
      Self::Simulation => "simulation",
    }
  }
}

/// The thread-safe handle that wakes a driver from anywhere.
#[derive(Clone, Copy, Debug)]
pub enum Kick {
  /// Write eight bytes to an eventfd (Linux; io_uring and epoll).
  #[cfg(target_os = "linux")]
  Eventfd(std::os::fd::RawFd),
  /// Trigger the `EVFILT_USER` event on a kqueue (macOS / BSD).
  #[cfg(any(target_os = "macos", target_os = "freebsd"))]
  Kqueue(std::os::fd::RawFd),
  /// Post a completion packet to a port (Windows), by its exposed address.
  #[cfg(target_os = "windows")]
  Iocp(usize),
  /// Set the simulation's kicked flag (single-threaded by construction).
  Sim(NonNull<SimShared>),
  /// No driver to kick (registry entries in tests).
  None,
}

// SAFETY: every variant is an OS handle the kernel serializes, or the simulation pointer, which
// is only ever used from the simulation's single thread (documented in `sim`).
unsafe impl Send for Kick {}
// SAFETY: as above; kicks are idempotent and racy by design (a lost race costs one spurious
// wake, never a lost one, because the ring is checked after every wait).
unsafe impl Sync for Kick {}

impl Kick {
  /// A kick that does nothing.
  pub const fn none() -> Kick {
    Kick::None
  }

  /// Wakes the driver. Errors are ignored: a closed driver belongs to a shard that exited, and
  /// the word it would have read stays in the ring for no one, which is the documented outcome.
  pub fn kick(&self) {
    match self {
      #[cfg(target_os = "linux")]
      Kick::Eventfd(fd) => {
        let one: u64 = 1;
        // SAFETY: an open eventfd and eight bytes from a u64.
        unsafe {
          libc::write(
            *fd,
            (&raw const one).cast::<std::ffi::c_void>(),
            std::mem::size_of::<u64>(),
          )
        };
      }
      #[cfg(any(target_os = "macos", target_os = "freebsd"))]
      Kick::Kqueue(kq) => crate::kqueue::trigger(*kq),
      #[cfg(target_os = "windows")]
      Kick::Iocp(port) => crate::iocp::post_kick(*port),
      Kick::Sim(shared) => {
        // SAFETY: the simulation's shared state is leaked for the process and used only on the
        // simulation thread.
        unsafe { shared.as_ref() }.set_kicked();
      }
      Kick::None => {}
    }
  }
}

/// The seam every driver implements.
pub trait Driver {
  /// Which driver this is.
  fn kind(&self) -> DriverKind;

  /// The kick that wakes this driver from any thread.
  fn kick_handle(&self) -> Kick;

  /// Monotonic nanoseconds (virtual under simulation).
  fn now_ns(&self) -> u64;

  /// Blocks until kicked, a completion arrives, or `timeout_ns` passes (`None` waits without
  /// bound). Completions are appended to `out`. Returns `DriverLost` when the driver died.
  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError>;

  /// Submits an operation that completes with `user_data` on a following `wait` (the seam's
  /// self-test; every real operation follows the same path).
  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError>;

  /// Whether a `wait` would return a completion or a kick without blocking, as far as the driver
  /// can tell without a syscall (an idle loop skips the wait when this is false).
  fn has_pending(&self) -> bool;
}

/// Builds the OS driver for this platform, probing and falling back as D-9 says.
#[cfg(target_os = "linux")]
pub fn os_driver(ring_entries: u32) -> Result<(Box<dyn Driver>, Vec<String>), RtError> {
  let mut notes = Vec::new();
  match crate::uring::UringDriver::new(ring_entries) {
    Ok(driver) => {
      notes.extend(driver.notes().iter().cloned());
      Ok((Box::new(driver), notes))
    }
    Err(e) => {
      notes.push(format!("io_uring = unavailable({e}); using epoll"));
      Ok((Box::new(crate::epoll::EpollDriver::new()?), notes))
    }
  }
}

/// Builds the OS driver for this platform.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub fn os_driver(_ring_entries: u32) -> Result<(Box<dyn Driver>, Vec<String>), RtError> {
  Ok((Box::new(crate::kqueue::KqueueDriver::new()?), Vec::new()))
}

/// Builds the OS driver for this platform.
#[cfg(target_os = "windows")]
pub fn os_driver(_ring_entries: u32) -> Result<(Box<dyn Driver>, Vec<String>), RtError> {
  Ok((Box::new(crate::iocp::IocpDriver::new()?), Vec::new()))
}

/// Monotonic nanoseconds since `epoch`, saturating.
pub fn nanos_since(epoch: std::time::Instant) -> u64 {
  u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn the_os_driver_wakes_on_a_kick_and_delivers_a_nop() {
    let (mut driver, notes) = os_driver(64).unwrap();
    eprintln!("driver {} notes {notes:?}", driver.kind().name());
    let kick = driver.kick_handle();
    let mut out = Vec::new();
    // A kick from another thread ends an unbounded wait.
    let t = std::thread::spawn(move || kick.kick());
    driver.wait(None, &mut out).unwrap();
    t.join().unwrap();
    // A no-op completes on the next wait with its word.
    driver.submit_nop(0xABCD).unwrap();
    driver.wait(Some(1_000_000_000), &mut out).unwrap();
    assert!(out.iter().any(|c| c.user_data == 0xABCD), "{out:?}");
    // A bounded wait with nothing pending returns at the deadline.
    let before = driver.now_ns();
    driver.wait(Some(2_000_000), &mut out).unwrap();
    assert!(driver.now_ns() - before >= 1_000_000);
  }
}
