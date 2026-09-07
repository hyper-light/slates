//! The driver seam: every driver blocks until a kick, a completion or a deadline, and hands back
//! completions as `(user_data, result)` pairs; a `Kick` is the thread-safe handle any thread
//! uses to wake a shard's driver (§4.3, "the three OS drivers with a common completion seam").
//!
//! Phase 0 carries the seam itself, the kick, the wait with a deadline, a pending check, and a
//! no-op operation whose completion proves the path; sockets, files and the bridge queues arrive
//! with their phases and use the same `wait`. The descriptors a kick names are leaked for the
//! process, so a kick is a `Copy` of a `&'static` handle and needs no unsafe code.

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
  Eventfd(&'static std::os::fd::OwnedFd),
  /// Trigger the `EVFILT_USER` event on a kqueue (macOS / BSD).
  #[cfg(any(target_os = "macos", target_os = "freebsd"))]
  Kqueue(&'static std::os::fd::OwnedFd),
  /// Post a completion packet to a port (Windows), by its exposed address.
  #[cfg(target_os = "windows")]
  Iocp(usize),
  /// Set the simulation's kicked flag.
  Sim(&'static SimShared),
  /// No driver to kick (registry entries in tests).
  None,
}

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
        let _ = rustix::io::write(fd, &1u64.to_ne_bytes());
      }
      #[cfg(any(target_os = "macos", target_os = "freebsd"))]
      Kick::Kqueue(kq) => crate::kqueue::trigger(kq),
      #[cfg(target_os = "windows")]
      Kick::Iocp(port) => crate::iocp::post_kick(*port),
      Kick::Sim(shared) => shared.set_kicked(),
      Kick::None => {}
    }
  }
}

/// The seam every driver implements. A driver is built on the thread that runs it, from a
/// [`DriverSeed`] the runtime prepared, so it may hold buffers of raw kernel records.
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

  /// Registers one-shot interest in `raw`'s readability (a UDP socket for the fleet transport,
  /// §4.10a): when it next becomes readable, a completion carrying `user_data` arrives on a following
  /// `wait` (re-registered after each read). `raw` is the OS handle (a `RawFd` on Unix). The
  /// readiness-native drivers (kqueue, epoll) register it; the completion-native drivers (io_uring,
  /// IOCP) do not carry it yet and refuse with a typed [`RtError`] (owed).
  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError>;

  /// Whether a `wait` would return a completion or a kick without blocking, as far as the driver
  /// can tell without a syscall (an idle loop skips the wait when this is false).
  fn has_pending(&self) -> bool;
}

/// What builds a driver on the shard's thread: a closure the runtime prepared with the OS
/// resources the kick needs (created up front, so the kick is known before the thread exists).
pub type DriverSeed = Box<dyn FnOnce() -> Result<Box<dyn Driver>, RtError> + Send>;

/// A prepared driver: the seed, the kick it will answer to, and the notes of the probe.
pub struct Prepared {
  /// Builds the driver on the shard's thread.
  pub seed: DriverSeed,
  /// The kick that wakes it.
  pub kick: Kick,
  /// Which driver and which flags the probe settled on.
  pub notes: Vec<String>,
}

impl std::fmt::Debug for Prepared {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Prepared")
      .field("notes", &self.notes)
      .finish()
  }
}

/// Prepares the OS driver for this platform, probing and falling back as D-9 says.
#[cfg(target_os = "linux")]
pub fn os_driver(ring_entries: u32) -> Result<Prepared, RtError> {
  let efd = crate::uring::prepare_eventfd()?;
  let mut notes = Vec::new();
  let uring = crate::uring::probe(ring_entries, &mut notes);
  let seed: DriverSeed = if uring {
    Box::new(move || {
      Ok(Box::new(crate::uring::UringDriver::with_eventfd(efd, ring_entries)?) as Box<dyn Driver>)
    })
  } else {
    Box::new(move || Ok(Box::new(crate::epoll::EpollDriver::with_eventfd(efd)?) as Box<dyn Driver>))
  };
  Ok(Prepared {
    seed,
    kick: Kick::Eventfd(efd),
    notes,
  })
}

/// Prepares the OS driver for this platform.
#[cfg(any(target_os = "macos", target_os = "freebsd"))]
pub fn os_driver(_ring_entries: u32) -> Result<Prepared, RtError> {
  let kq = crate::kqueue::prepare()?;
  Ok(Prepared {
    seed: Box::new(move || {
      Ok(Box::new(crate::kqueue::KqueueDriver::from_prepared(kq)) as Box<dyn Driver>)
    }),
    kick: Kick::Kqueue(kq),
    notes: Vec::new(),
  })
}

/// Prepares the OS driver for this platform.
#[cfg(target_os = "windows")]
pub fn os_driver(_ring_entries: u32) -> Result<Prepared, RtError> {
  let port = crate::iocp::prepare()?;
  Ok(Prepared {
    seed: Box::new(move || {
      Ok(Box::new(crate::iocp::IocpDriver::from_prepared(port)) as Box<dyn Driver>)
    }),
    kick: Kick::Iocp(port),
    notes: Vec::new(),
  })
}

/// Monotonic nanoseconds since `epoch`, saturating.
pub fn nanos_since(epoch: std::time::Instant) -> u64 {
  u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A rustix refusal as the driver's typed error.
#[cfg(unix)]
pub(crate) fn refused(call: &'static str, e: rustix::io::Errno) -> RtError {
  RtError::DriverRefused {
    call,
    code: Some(e.raw_os_error()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  #[cfg_attr(miri, ignore)]
  fn the_os_driver_wakes_on_a_kick_and_delivers_a_nop() {
    let prepared = os_driver(64).unwrap();
    let notes = prepared.notes.clone();
    let mut driver = (prepared.seed)().unwrap();
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
