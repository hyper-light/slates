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
//! A stopped process has not exited, so a call to it waits (A-232: a call waits while its daemon lives, and a stuck one
//! is ended by its supervisor or its own watchdog, which this watch then sees). A Linux client needs no watch (the control socket's
//! peer end closes with the daemon's process); the supervising anchor does, to restart a daemon the moment it dies
//! rather than at its next observation (A-113's follow-up): on Unix the watch's descriptor
//! (`ExitWatch::readiness`, the Unix arm) becomes readable at the exit, so the anchor waits on it beside its hold channel. For a
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

  /// A watch on the process `pid` names only if it is the one created at `created` (`GetProcessTimes`' creation time,
  /// in 100 ns intervals since 1601): a process exited and its pid reused by another is never taken for it, and the watch
  /// reads exited at once. The handle is opened for `SYNCHRONIZE` and `PROCESS_QUERY_LIMITED_INFORMATION` alone, the
  /// least that reads the creation time: it keeps the process object (and so its pid) but none of the objects the process
  /// named, so a bootstrap object or an owner's token it made dies with it (A-176, A-447).
  #[cfg(windows)]
  pub fn on_created(pid: u32, created: u64) -> Result<ExitWatch, IpcError> {
    Ok(ExitWatch {
      inner: platform::ExitWatch::on_created(pid, created)?,
    })
  }

  /// A watch on the process `pid` names only if it is the one that started at `started`, the mark a claimant names
  /// itself by beside its pid (`ExitWatch::own_start`; A-522): a pid whose process ended and was reused by another is
  /// never taken for it, and the watch reads exited at once. A claim carries both because its pid is only the claimant's
  /// word until its watch is taken, and a pid is reused once its process is gone. macOS registers the exit note first and
  /// reads the start after, so the right process's exit between the two is never missed: the note is bound to the process
  /// it was registered on (kqueue(2)), and a start that does not match says that process is not the claimant. Windows
  /// judges the creation time (`ExitWatch::on_created`), on a handle that keeps the pid while it is open.
  #[cfg(any(target_os = "macos", windows))]
  pub fn on_started(pid: u32, started: u64) -> Result<ExitWatch, IpcError> {
    if pid == 0 {
      return Err(IpcError::Layout {
        reason: "the claim names no process",
      });
    }
    Ok(ExitWatch {
      inner: platform::ExitWatch::on_started(pid, started)?,
    })
  }

  /// When this process started, the mark it names itself by beside its pid (A-522): on macOS the start time the kernel
  /// keeps for it (`proc_pidinfo(PROC_PIDTBSDINFO)`'s `pbi_start_tvsec` and `pbi_start_tvusec`, in microseconds since
  /// 1970); on Windows its creation time (`GetProcessTimes`, in 100 ns intervals since 1601). Two processes that held
  /// one pid never share it: the second starts after the first has ended.
  #[cfg(any(target_os = "macos", windows))]
  pub fn own_start() -> Result<u64, IpcError> {
    platform::own_start()
  }

  /// When process `pid` started, read as `ExitWatch::own_start` reads this process's; `None` when the system answers
  /// nothing for it (no such process).
  #[cfg(any(target_os = "macos", windows))]
  pub fn start_of(pid: u32) -> Result<Option<u64>, IpcError> {
    platform::start_of(pid)
  }

  /// The watched process's handle, to wait on beside another object (`crate::wake::Event::wait_or_exit`); `None` when
  /// the process was already gone at the watch.
  #[cfg(windows)]
  pub fn handle(&self) -> Option<std::os::windows::io::BorrowedHandle<'_>> {
    self.inner.handle()
  }

  /// This process's creation time (`GetProcessTimes`, 100 ns intervals since 1601): what a claimant judges a pid by
  /// ([`ExitWatch::on_created`]).
  #[cfg(windows)]
  pub fn own_creation() -> Result<u64, IpcError> {
    platform::own_creation()
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

/// The kernel's BSD record of a process (macOS), read by the claim service for a sender's parent too
/// (`crate::mach_claim`).
#[cfg(target_os = "macos")]
pub(crate) use platform::bsd_info;
#[cfg(target_os = "macos")]
pub use platform::{ExitMux, ProcessStamp};

#[cfg(target_os = "macos")]
mod platform {
  use std::cell::Cell;
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
  use std::time::Duration;

  use rustix::event::kqueue::{Event, EventFilter, EventFlags, ProcessEvents, kevent, kqueue};
  use rustix::process::Pid;

  use crate::error::IpcError;

  /// Format: `SZOMB` from macOS `<sys/proc.h>` (`pbi_status == 5` marks an exited, unreaped zombie).
  const PROCESS_ZOMBIE: u32 = 5;

  /// Single `kevent(2)` wrapper shared by [`ExitWatch`] and [`ExitMux`], keeping all unsafe
  /// `kevent` calls in one site. Pass an empty `&mut [MaybeUninit<Event>]` for change-only
  /// registrations (so `nevents == 0` returns immediately without blocking or dequeuing events)
  /// or `rustix::buffer::spare_capacity(&mut vec)` for draining fired events.
  fn kevent_raw<Buf: rustix::buffer::Buffer<Event>>(
    queue: &OwnedFd,
    changes: &[Event],
    events: Buf,
    timeout: Option<Duration>,
  ) -> rustix::io::Result<Buf::Output> {
    // SAFETY: `queue` is our live owned kqueue descriptor, `changes` is a valid slice, and
    // `events` is a valid `rustix::buffer::Buffer<Event>` output buffer for `kevent`.
    unsafe { kevent(queue, changes, events, timeout) }
  }

  /// A descriptor-free liveness check for a macOS process identified by `(pid, start_time)`
  /// (`proc_pidinfo(PROC_PIDTBSDINFO)`): detects exit, unreaped zombies (`SZOMB`), and PID reuse
  /// without allocating a `kqueue` descriptor per client connection.
  #[derive(Debug)]
  pub struct ProcessStamp {
    pid: u32,
    started: u64,
    exited: Cell<bool>,
  }

  impl ProcessStamp {
    /// Captures process `pid`'s start stamp. Refuses `pid == 0`; a process already exited or
    /// zombie reads exited at once.
    pub fn of(pid: u32) -> Result<ProcessStamp, IpcError> {
      if pid == 0 {
        return Err(IpcError::Layout {
          reason: "the bootstrap header names no daemon process",
        });
      }
      match alive_start_of(pid) {
        Some(started) => Ok(ProcessStamp {
          pid,
          started,
          exited: Cell::new(false),
        }),
        None => Ok(ProcessStamp {
          pid,
          started: 0,
          exited: Cell::new(true),
        }),
      }
    }

    /// Whether the process has exited (including unreaped zombies or PID reuse), remembered once true.
    pub fn exited(&self) -> bool {
      if self.exited.get() {
        return true;
      }
      let gone = alive_start_of(self.pid) != Some(self.started);
      if gone {
        self.exited.set(true);
      }
      gone
    }
  }

  /// One `kqueue` descriptor multiplexing `EVFILT_PROC | NOTE_EXIT` watches for any number of
  /// distinct macOS client processes on a shard (1 FD per shard instead of 1 per client).
  #[derive(Debug)]
  pub struct ExitMux {
    queue: OwnedFd,
  }

  impl ExitMux {
    /// Opens a fresh `kqueue` descriptor for multiplexed process-exit watching.
    pub fn open() -> Result<ExitMux, IpcError> {
      let queue = kqueue().map_err(|e| IpcError::OsRefused {
        call: "kqueue",
        code: Some(e.raw_os_error()),
      })?;
      Ok(ExitMux { queue })
    }

    /// The `kqueue` descriptor, readable whenever any watched process has exited.
    pub fn readiness(&self) -> BorrowedFd<'_> {
      self.queue.as_fd()
    }

    /// Registers an `EVFILT_PROC | NOTE_EXIT | EV_ONESHOT` watch on `pid` verified against
    /// `started`. Returns `Ok(true)` if the process is alive and now watched, or `Ok(false)` if it
    /// already exited or `pid` was reused by another process (cleaning up the filter in that case).
    pub fn watch(&self, pid: u32, started: u64) -> Result<bool, IpcError> {
      let process = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or(IpcError::Layout {
          reason: "the claim names a pid out of range",
        })?;
      let note = Event::new(
        EventFilter::Proc {
          pid: process,
          flags: ProcessEvents::EXIT,
        },
        EventFlags::ADD | EventFlags::ONESHOT,
        std::ptr::null_mut(),
      );
      let mut none: [std::mem::MaybeUninit<Event>; 0] = [];
      match kevent_raw(&self.queue, &[note], &mut none[..], None) {
        Ok(_) => {
          // Register first, verify start after (A-522): if another process reused `pid` before
          // registration, remove the note and report already exited.
          if start_of(pid)? != Some(started) {
            self.unwatch(pid);
            return Ok(false);
          }
          Ok(true)
        }
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(e) => Err(IpcError::OsRefused {
          call: "kevent(EVFILT_PROC)",
          code: Some(e.raw_os_error()),
        }),
      }
    }

    /// Removes the exit watch for `pid` when no clients of that process remain on this shard.
    pub fn unwatch(&self, pid: u32) {
      let Some(process) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return;
      };
      let delete = Event::new(
        EventFilter::Proc {
          pid: process,
          flags: ProcessEvents::EXIT,
        },
        EventFlags::DELETE,
        std::ptr::null_mut(),
      );
      let mut none: [std::mem::MaybeUninit<Event>; 0] = [];
      let _ = kevent_raw(&self.queue, &[delete], &mut none[..], None);
    }

    /// Drains up to `max` exited process ids without blocking, appending them to `out`.
    pub fn drain_exited(&self, max: usize, out: &mut Vec<u32>) -> Result<(), IpcError> {
      let mut fired: Vec<Event> = Vec::with_capacity(max.max(1));
      match kevent_raw(
        &self.queue,
        &[],
        rustix::buffer::spare_capacity(&mut fired),
        Some(Duration::ZERO),
      ) {
        Ok(_) => {
          for event in fired {
            if let EventFilter::Proc { pid, .. } = event.filter()
              && let Ok(raw) = u32::try_from(pid.as_raw_nonzero().get())
            {
              out.push(raw);
            }
          }
          Ok(())
        }
        Err(rustix::io::Errno::INTR) => Ok(()),
        Err(e) => Err(IpcError::OsRefused {
          call: "kevent(drain EVFILT_PROC)",
          code: Some(e.raw_os_error()),
        }),
      }
    }
  }

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
      let mut none: [std::mem::MaybeUninit<Event>; 0] = [];
      match kevent_raw(&queue, &[note], &mut none[..], None) {
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
      let outcome = kevent_raw(
        &self.queue,
        &[],
        rustix::buffer::spare_capacity(&mut fired),
        Some(Duration::ZERO),
      );
      let exited = match outcome {
        Ok(count) => count > 0,
        Err(rustix::io::Errno::INTR) => false,
        Err(_) => true,
      };
      self.exited.set(exited);
      exited
    }

    pub(super) fn readiness(&self) -> Option<BorrowedFd<'_>> {
      (!self.exited.get()).then(|| self.queue.as_fd())
    }

    pub(super) fn on_started(pid: u32, started: u64) -> Result<ExitWatch, IpcError> {
      let watch = Self::on(pid)?;
      // The note first, then the start: the note holds the process it was registered on, so if that process is the
      // claimant its exit is caught however late, and if the start read after does not match, the process the note
      // holds is another (the claimant's pid reused) and the claimant is gone.
      if !watch.exited.get() && start_of(pid)? != Some(started) {
        watch.exited.set(true);
      }
      Ok(watch)
    }
  }

  /// Format: microseconds per second, the unit the start's two fields combine in.
  const MICROS_PER_SECOND: u64 = 1_000_000;

  fn start_from_info(info: &libc::proc_bsdinfo) -> u64 {
    info
      .pbi_start_tvsec
      .saturating_mul(MICROS_PER_SECOND)
      .saturating_add(info.pbi_start_tvusec)
  }

  /// Process `pid`'s start stamp when it exists and is not an exited zombie (`SZOMB`).
  fn alive_start_of(pid: u32) -> Option<u64> {
    let info = bsd_info(pid)?;
    (info.pbi_status != PROCESS_ZOMBIE).then(|| start_from_info(&info))
  }

  pub(super) fn start_of(pid: u32) -> Result<Option<u64>, IpcError> {
    Ok(bsd_info(pid).map(|info| start_from_info(&info)))
  }

  pub(super) fn own_start() -> Result<u64, IpcError> {
    let own = rustix::process::getpid()
      .as_raw_nonzero()
      .get()
      .unsigned_abs();
    start_of(own)?.ok_or(IpcError::OsRefused {
      call: "proc_pidinfo(PROC_PIDTBSDINFO) of this process",
      code: None,
    })
  }

  /// The kernel's BSD record of process `pid` (`proc_pidinfo(PROC_PIDTBSDINFO)`, `<libproc.h>`): its parent and its
  /// start among the rest; `None` when the kernel answers nothing for it (no such process, or none this user may read).
  pub(crate) fn bsd_info(pid: u32) -> Option<libc::proc_bsdinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::ffi::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is a live, writable, zero-initialized `proc_bsdinfo` of `size` bytes; the call writes at most
    // `size` bytes into it and returns how many it wrote.
    let written = unsafe {
      libc::proc_pidinfo(
        std::ffi::c_int::try_from(pid).ok()?,
        libc::PROC_PIDTBSDINFO,
        0,
        info.as_mut_ptr().cast(),
        size,
      )
    };
    if written != size {
      return None;
    }
    // SAFETY: zero-initialized and then written whole by the kernel (`written == size`); every field is a plain integer
    // or byte array, for which any bytes are a value.
    Some(unsafe { info.assume_init() })
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

  use windows_sys::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_TIMEOUT};
  use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
  };

  use crate::error::IpcError;

  pub(super) struct ExitWatch {
    /// The process handle; `None` when the process was already gone at the watch.
    process: Option<OwnedHandle>,
  }

  impl ExitWatch {
    pub(super) fn on(pid: u32) -> Result<ExitWatch, IpcError> {
      Ok(ExitWatch {
        process: open(pid, PROCESS_SYNCHRONIZE, "OpenProcess(SYNCHRONIZE)")?,
      })
    }

    pub(super) fn on_created(pid: u32, created: u64) -> Result<ExitWatch, IpcError> {
      let Some(process) = open(
        pid,
        PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
        "OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION)",
      )?
      else {
        return Ok(ExitWatch { process: None });
      };
      // Another process created at another time holds the pid now: the process watched for is gone, its pid reused.
      if creation_of(process.as_raw_handle())? != created {
        return Ok(ExitWatch { process: None });
      }
      Ok(ExitWatch {
        process: Some(process),
      })
    }

    pub(super) fn on_started(pid: u32, started: u64) -> Result<ExitWatch, IpcError> {
      Self::on_created(pid, started)
    }

    pub(super) fn handle(&self) -> Option<std::os::windows::io::BorrowedHandle<'_>> {
      use std::os::windows::io::AsHandle;
      self.process.as_ref().map(AsHandle::as_handle)
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

  /// The creation time of the process `process` names, a handle with a query right.
  fn creation_of(process: HANDLE) -> Result<u64, IpcError> {
    let mut times = [FILETIME {
      dwLowDateTime: 0,
      dwHighDateTime: 0,
    }; 4];
    let [creation, exit, kernel, user] = &mut times;
    // SAFETY: `process` is a live handle with a query right for the call, and the four out-parameters are live records
    // the call writes and nothing else reads meanwhile.
    let read = unsafe { GetProcessTimes(process, creation, exit, kernel, user) };
    if read == 0 {
      return Err(IpcError::OsRefused {
        call: "GetProcessTimes",
        code: std::io::Error::last_os_error().raw_os_error(),
      });
    }
    let [creation, ..] = times;
    Ok((u64::from(creation.dwHighDateTime) << u32::BITS) | u64::from(creation.dwLowDateTime))
  }

  /// This process's creation time.
  pub(super) fn own_creation() -> Result<u64, IpcError> {
    // SAFETY: the pseudo-handle of the calling process, always valid and never closed.
    creation_of(unsafe { GetCurrentProcess() })
  }

  /// This process's start: its creation time.
  pub(super) fn own_start() -> Result<u64, IpcError> {
    own_creation()
  }

  /// Process `pid`'s start, its creation time, read on a handle opened for the query alone; `None` when no such
  /// process is there.
  pub(super) fn start_of(pid: u32) -> Result<Option<u64>, IpcError> {
    let Some(process) = open(
      pid,
      PROCESS_QUERY_LIMITED_INFORMATION,
      "OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)",
    )?
    else {
      return Ok(None);
    };
    creation_of(process.as_raw_handle()).map(Some)
  }

  /// Process `pid`, opened for `rights` and owned; `None` when no such process exists (`ERROR_INVALID_PARAMETER`, the
  /// answer for a pid no process holds), and any other refusal typed with `call`.
  fn open(pid: u32, rights: u32, call: &'static str) -> Result<Option<OwnedHandle>, IpcError> {
    // SAFETY: plain values; a null return is the failure, read from the thread's last error below.
    let raw = unsafe { OpenProcess(rights, 0, pid) };
    if raw.is_null() {
      let error = std::io::Error::last_os_error();
      if error.raw_os_error() == i32::try_from(ERROR_INVALID_PARAMETER).ok() {
        return Ok(None);
      }
      return Err(IpcError::OsRefused {
        call,
        code: error.raw_os_error(),
      });
    }
    // SAFETY: `raw` is the open handle `OpenProcess` just returned, owned by nothing else.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(raw) }))
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

  /// A-522 (a claim's pid bound to its process). Do: watch a live child by its pid and its own start, and again by its
  /// pid and a start one later, another process's, as a pid reused before the watch would name it. Expect: the first not
  /// exited while the child runs and exited once it is killed; the second exited at once, though a process holds the
  /// pid; and this process's own start read as any process's is.
  #[cfg(any(target_os = "macos", windows))]
  #[test]
  fn a_watch_by_pid_and_start_takes_no_other_process_for_its_own() {
    let _gate = crate::descriptor_test_gate();
    #[cfg(target_os = "macos")]
    let mut child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    #[cfg(windows)]
    let mut child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .unwrap();
    let started = ExitWatch::start_of(child.id())
      .unwrap()
      .expect("a live child has a start");
    let watch = ExitWatch::on_started(child.id(), started).unwrap();
    let reused = ExitWatch::on_started(child.id(), started.wrapping_add(1)).unwrap();
    assert!(
      !watch.exited(),
      "the claimant's own process was taken for gone"
    );
    assert!(
      reused.exited(),
      "a process started at another time was taken for the claimant"
    );
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(watch.exited(), "a killed claimant has exited");
    assert_eq!(
      ExitWatch::own_start().unwrap(),
      ExitWatch::start_of(std::process::id()).unwrap().unwrap()
    );
  }

  /// macOS descriptor-free liveness check ([`ProcessStamp`]): detects running, stopped (`SIGSTOP`),
  /// unreaped zombie (`SIGKILL` before `wait`), reaped exit, and `pid == 0`, while opening zero FDs.
  #[cfg(target_os = "macos")]
  #[test]
  fn process_stamp_tracks_lifecycle_without_opening_descriptors() {
    let _gate = crate::descriptor_test_gate();
    let mut child = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    #[allow(clippy::disallowed_methods)] // Read-only count of this process's descriptors.
    let before = std::fs::read_dir("/dev/fd").map_or(0, Iterator::count);
    let stamps: Vec<ProcessStamp> = (0..64)
      .map(|_| ProcessStamp::of(child.id()).unwrap())
      .collect();
    #[allow(clippy::disallowed_methods)]
    let after = std::fs::read_dir("/dev/fd").map_or(0, Iterator::count);
    assert!(
      after <= before + 4,
      "ProcessStamp must not open per-watch descriptors: before={before}, after={after}"
    );
    assert!(
      stamps.iter().all(|s| !s.exited()),
      "running process is alive"
    );
    let pid = rustix::process::Pid::from_raw(i32::try_from(child.id()).unwrap()).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
    assert!(
      stamps.iter().all(|s| !s.exited()),
      "stopped process is alive"
    );
    child.kill().unwrap();
    // Wait briefly for the kernel to transition the killed child into zombie (`SZOMB`) before wait().
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !stamps[0].exited() && std::time::Instant::now() < deadline {
      std::thread::yield_now();
    }
    assert!(
      stamps.iter().all(ProcessStamp::exited),
      "killed zombie process reads exited even before wait()"
    );
    child.wait().unwrap();
    assert!(stamps[0].exited(), "exit stays remembered after wait()");
    assert!(ProcessStamp::of(0).is_err());
  }

  /// macOS multiplexed process-exit watcher ([`ExitMux`]): one `kqueue` watches many PIDs, wakes
  /// on exit, respects `unwatch`, and rejects mismatched start stamps.
  #[cfg(target_os = "macos")]
  #[test]
  fn exit_mux_watches_many_processes_on_one_descriptor() {
    let _gate = crate::descriptor_test_gate();
    let mux = ExitMux::open().unwrap();
    let mut first = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let mut second = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let start_one = ExitWatch::start_of(first.id()).unwrap().unwrap();
    let start_two = ExitWatch::start_of(second.id()).unwrap().unwrap();
    assert!(mux.watch(first.id(), start_one).unwrap());
    assert!(mux.watch(second.id(), start_two).unwrap());
    // Mismatched start time is rejected immediately and does not leave a stale watch.
    let mut third = std::process::Command::new("/bin/sleep")
      .arg("30")
      .spawn()
      .unwrap();
    let start_three = ExitWatch::start_of(third.id()).unwrap().unwrap();
    assert!(!mux.watch(third.id(), start_three.wrapping_add(1)).unwrap());

    let mut exited = Vec::new();
    mux.drain_exited(8, &mut exited).unwrap();
    assert!(exited.is_empty(), "no process exited yet");

    // Unwatch second before killing it; kill first and expect only first to report.
    mux.unwatch(second.id());
    second.kill().unwrap();
    second.wait().unwrap();
    first.kill().unwrap();
    first.wait().unwrap();

    let fd = mux.readiness();
    let mut interest = [rustix::event::PollFd::new(
      &fd,
      rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
      tv_sec: 5,
      tv_nsec: 0,
    };
    let ready = rustix::event::poll(&mut interest, Some(&timeout)).unwrap();
    assert_eq!(ready, 1, "mux readiness woke on first child's exit");
    mux.drain_exited(8, &mut exited).unwrap();
    assert_eq!(exited, vec![first.id()]);

    third.kill().unwrap();
    third.wait().unwrap();
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
