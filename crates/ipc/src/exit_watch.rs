//! A watch on the daemon process's exit (§4.7 "Failure matrix"; AUD-29-20): how a client on macOS and
//! Windows tells a dead daemon from a stopped or slow one.
//!
//! The bootstrap object a daemon publishes outlives it: a POSIX shared-memory name stays until it is
//! unlinked, and a Windows section stays while any client maps it. So its start stamp alone still reads
//! "alive" after the daemon is killed with nothing to restart it, and before this watch a client's
//! calls to a killed daemon ended `Stalled` and recovery never began (measured 2026-10-01: the Node SDK
//! acceptance test, its daemon and anchor `SIGKILL`ed, every call `Stalled { after_ns: 1000000000 }`).
//!
//! The watch is taken on the daemon's process at the moment its claim answer is read (the daemon is
//! alive then), and it is bound to that process, not to its pid:
//! - macOS: `EVFILT_PROC` with `NOTE_EXIT` registered on a kqueue of the client's own (the kernel
//!   attaches the note to the process, so a recycled pid neither fires nor silences it; kqueue(2)), and
//!   read with a zero timeout.
//! - Windows: a `SYNCHRONIZE` handle to the process (a pid is not reused while a handle to its process
//!   is open; Microsoft, "Process Handles and Identifiers"), asked with `WaitForSingleObject` and a zero
//!   timeout.
//!
//! - Linux: a pidfd (`pidfd_open(2)`, Linux 5.3), bound to the process as the kqueue note is, readable once it
//!   exits.
//!
//! A stopped process has not exited, so a stall stays a stall. A Linux client needs no watch (the control socket's
//! peer end closes with the daemon's process); the supervising anchor does, to restart a daemon the moment it dies
//! rather than at its next observation (A-113's follow-up): on Unix the watch's descriptor
//! ([`ExitWatch::readiness`]) becomes readable at the exit, so the anchor waits on it beside its hold channel. For a
//! client the question is a cold path, asked only after a reply is overdue.

use crate::error::IpcError;

/// The exit watch on one daemon process.
pub struct ExitWatch {
  inner: platform::ExitWatch,
}

impl std::fmt::Debug for ExitWatch {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("ExitWatch")
  }
}

impl ExitWatch {
  /// Watches process `pid` for its exit. A process already gone when the watch is taken reads exited at
  /// once; a pid of zero (no process) is refused.
  pub fn on(pid: u32) -> Result<ExitWatch, IpcError> {
    if pid == 0 {
      return Err(IpcError::Layout {
        reason: "the bootstrap header names no daemon process",
      });
    }
    Ok(ExitWatch {
      inner: platform::ExitWatch::on(pid)?,
    })
  }

  /// Whether the watched process has exited, without waiting. A watch that can no longer be read
  /// answers exited, as an unusable control socket does on Linux: either way the daemon cannot be
  /// vouched for, and the caller's recovery reconnects to whichever daemon holds the instance now.
  pub fn exited(&self) -> bool {
    self.inner.exited()
  }

  /// A descriptor readable once the watched process has exited, to wait on with `poll` (the kqueue on macOS, the pidfd
  /// on Linux); `None` for a watch taken after the process was already gone, which needs no wait.
  #[cfg(unix)]
  pub fn readiness(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
    self.inner.readiness()
  }
}

#[cfg(target_os = "macos")]
mod platform {
  use std::cell::Cell;
  use std::os::fd::OwnedFd;
  use std::time::Duration;

  use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};
  use rustix::process::Pid;

  use crate::error::IpcError;

  pub(super) struct ExitWatch {
    queue: OwnedFd,
    /// The exit, once read: the note is one-shot, so it is remembered here.
    exited: Cell<bool>,
  }

  impl ExitWatch {
    pub(super) fn on(pid: u32) -> Result<ExitWatch, IpcError> {
      let refused = |call: &'static str, errno: rustix::io::Errno| IpcError::OsRefused {
        call,
        code: Some(errno.raw_os_error()),
      };
      let process = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(IpcError::Layout {
          reason: "the bootstrap header names a pid out of range",
        })?;
      let queue = kqueue().map_err(|e| refused("kqueue", e))?;
      let note = Event::new(
        EventFilter::Proc {
          pid: process,
          flags: ProcessEvents::EXIT,
        },
        EventFlags::ADD | EventFlags::ONESHOT,
        std::ptr::null_mut(),
      );
      let mut none: Vec<Event> = Vec::new();
      // SAFETY: one valid change record (no user data pointer) on the queue just created; the empty
      // output vector receives nothing.
      match unsafe { kevent(&queue, &[note], &mut none, None) } {
        Ok(_) => Ok(ExitWatch {
          queue,
          exited: Cell::new(false),
        }),
        // No such process: it exited between its answer and this watch.
        Err(rustix::io::Errno::SRCH) => Ok(ExitWatch {
          queue,
          exited: Cell::new(true),
        }),
        Err(e) => Err(refused("kevent(EVFILT_PROC)", e)),
      }
    }

    pub(super) fn exited(&self) -> bool {
      if self.exited.get() {
        return true;
      }
      let mut fired: Vec<Event> = Vec::with_capacity(1);
      // SAFETY: no changes; the output buffer is the vector's spare capacity (one record).
      let outcome = unsafe {
        kevent(
          &self.queue,
          &[],
          rustix::buffer::spare_capacity(&mut fired),
          Some(Duration::ZERO),
        )
      };
      let exited = match outcome {
        Ok(count) => count > 0,
        Err(rustix::io::Errno::INTR) => false,
        Err(_) => true,
      };
      self.exited.set(exited);
      exited
    }

    pub(super) fn readiness(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
      (!self.exited.get()).then(|| std::os::fd::AsFd::as_fd(&self.queue))
    }
  }
}

#[cfg(target_os = "linux")]
mod platform {
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use rustix::event::{PollFd, PollFlags, Timespec, poll};
  use rustix::process::{Pid, PidfdFlags, pidfd_open};

  use crate::error::IpcError;

  pub(super) struct ExitWatch {
    /// The pidfd; `None` when the process was already gone at the watch.
    process: Option<OwnedFd>,
  }

  impl ExitWatch {
    pub(super) fn on(pid: u32) -> Result<ExitWatch, IpcError> {
      let process = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(IpcError::Layout {
          reason: "the watched pid is out of range",
        })?;
      match pidfd_open(process, PidfdFlags::empty()) {
        Ok(fd) => Ok(ExitWatch { process: Some(fd) }),
        // No such process: it exited before the watch.
        Err(rustix::io::Errno::SRCH) => Ok(ExitWatch { process: None }),
        Err(e) => Err(IpcError::OsRefused {
          call: "pidfd_open",
          code: Some(e.raw_os_error()),
        }),
      }
    }

    pub(super) fn exited(&self) -> bool {
      let Some(process) = &self.process else {
        return true;
      };
      let mut interest = [PollFd::new(process, PollFlags::IN)];
      let zero = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
      };
      match poll(&mut interest, Some(&zero)) {
        Ok(ready) => ready > 0,
        Err(rustix::io::Errno::INTR) => false,
        Err(_) => true,
      }
    }

    pub(super) fn readiness(&self) -> Option<BorrowedFd<'_>> {
      self.process.as_ref().map(AsFd::as_fd)
    }
  }
}

#[cfg(windows)]
mod platform {
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

  use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, WAIT_TIMEOUT};
  use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
  };

  use crate::error::IpcError;

  pub(super) struct ExitWatch {
    /// The process handle; `None` when the process was already gone at the watch.
    process: Option<OwnedHandle>,
  }

  impl ExitWatch {
    pub(super) fn on(pid: u32) -> Result<ExitWatch, IpcError> {
      // SAFETY: plain values; a null return is the failure, read from the thread's last error below.
      let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
      if raw.is_null() {
        let error = std::io::Error::last_os_error();
        // No such process: it exited between its answer and this watch.
        if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
          return Ok(ExitWatch { process: None });
        }
        return Err(IpcError::OsRefused {
          call: "OpenProcess(SYNCHRONIZE)",
          code: error.raw_os_error(),
        });
      }
      // SAFETY: `raw` is the open handle `OpenProcess` just returned, owned by nothing else.
      let process = unsafe { OwnedHandle::from_raw_handle(raw) };
      Ok(ExitWatch {
        process: Some(process),
      })
    }

    pub(super) fn exited(&self) -> bool {
      let Some(process) = &self.process else {
        return true;
      };
      // SAFETY: the handle is open for the watch's life; a zero timeout never blocks.
      let waited = unsafe { WaitForSingleObject(process.as_raw_handle(), 0) };
      // Signalled (exited) or failed (unusable): not vouched for.
      waited != WAIT_TIMEOUT
    }
  }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
  use crate::error::IpcError;

  /// No watch on this platform (Linux reads its control socket instead): an uninhabited type.
  pub(super) enum ExitWatch {}

  impl ExitWatch {
    pub(super) fn on(_pid: u32) -> Result<ExitWatch, IpcError> {
      Err(IpcError::Unsupported {
        feature: "a process exit watch",
      })
    }

    pub(super) fn exited(&self) -> bool {
      match *self {}
    }

    #[cfg(unix)]
    pub(super) fn readiness(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
      match *self {}
    }
  }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux", windows)))]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use super::*;

  /// AUD-29-20 (a dead daemon is told from a stopped one). Do: watch a child process, stop it, then kill
  /// it and reap it. Expect: not exited while it runs and while it is stopped; exited once it has died;
  /// and a watch on a pid of zero refused.
  #[cfg(target_os = "macos")]
  #[test]
  fn a_stopped_process_is_alive_and_a_killed_one_has_exited() {
    let _gate = crate::descriptor_test_gate();
    let mut child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let watch = ExitWatch::on(child.id()).unwrap();
    assert!(!watch.exited(), "a running process has not exited");
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
    assert!(!watch.exited(), "a stopped process has not exited");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(watch.exited(), "a killed process has exited");
    assert!(watch.exited(), "the exit is remembered once read");
    assert!(ExitWatch::on(0).is_err());
  }

  /// A-113's follow-up (the anchor restarts a daemon the moment it dies). Do: watch a child process and wait on its
  /// readiness descriptor with a long timeout while it runs briefly, then kill it from another thread. Expect: the wait
  /// returns at the kill, not at its timeout, and the watch reads exited.
  #[cfg(any(target_os = "macos", target_os = "linux"))]
  #[test]
  fn the_readiness_descriptor_wakes_a_wait_at_the_exit() {
    let _gate = crate::descriptor_test_gate();
    let mut child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let watch = ExitWatch::on(child.id()).unwrap();
    let pid = child.id();
    let killer = std::thread::spawn(move || {
      #[allow(clippy::disallowed_methods)] // the test lets the wait begin before the kill
      std::thread::sleep(std::time::Duration::from_millis(50));
      let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
      rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    });
    let started = std::time::Instant::now();
    let fd = watch
      .readiness()
      .expect("a running process has a readiness descriptor");
    let mut interest = [rustix::event::PollFd::new(
      &fd,
      rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
      tv_sec: 10,
      tv_nsec: 0,
    };
    let ready = rustix::event::poll(&mut interest, Some(&timeout)).unwrap();
    let waited = started.elapsed();
    killer.join().unwrap();
    child.wait().unwrap();
    assert_eq!(ready, 1, "the wait returned readable");
    assert!(
      waited < std::time::Duration::from_secs(5),
      "it returned at the kill ({waited:?}), not its timeout"
    );
    assert!(watch.exited(), "the watch reads exited");
  }

  /// AUD-29-20 (Windows). Do: watch a child process, then kill it and reap it. Expect: not exited while
  /// it runs; exited once it has died.
  #[cfg(windows)]
  #[test]
  fn a_killed_process_has_exited() {
    let _gate = crate::descriptor_test_gate();
    let mut child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .unwrap();
    let watch = ExitWatch::on(child.id()).unwrap();
    assert!(!watch.exited(), "a running process has not exited");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(watch.exited(), "a killed process has exited");
  }
}
