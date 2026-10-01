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
//! A stopped process has not exited, so a stall stays a stall. Linux needs no watch: the control
//! socket's peer end closes with the daemon's process. The question is a cold path, asked only after a
//! reply is overdue.

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

#[cfg(not(any(target_os = "macos", windows)))]
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
  }
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

  use super::*;

  /// AUD-29-20 (a dead daemon is told from a stopped one). Do: watch a child process, stop it, then kill
  /// it and reap it. Expect: not exited while it runs and while it is stopped; exited once it has died;
  /// and a watch on a pid of zero refused.
  #[cfg(target_os = "macos")]
  #[test]
  fn a_stopped_process_is_alive_and_a_killed_one_has_exited() {
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

  /// AUD-29-20 (Windows). Do: watch a child process, then kill it and reap it. Expect: not exited while
  /// it runs; exited once it has died.
  #[cfg(windows)]
  #[test]
  fn a_killed_process_has_exited() {
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
