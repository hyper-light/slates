//! The host's reports of a change in its memory pressure, as the kernel makes them (§4.2 "the pressure hold", A-570):
//! what wakes the daemon's pressure sampler, where the reap loop's tick sampled the host once a liveness window
//! whatever the host did. One source a platform, each the kernel's own notification, so nothing here is polled and no
//! threshold is chosen here (R3); the caller awaits the descriptor or handle each names through its shard and samples
//! the host then.
//! - **macOS**: libdispatch's memory-pressure source (`DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` at every level, normal,
//!   warning and critical), "a dispatch source that monitors the system for changes in the memory pressure condition"
//!   [B: Apple Developer Documentation, `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE`, dispatch/source.h]. XNU tells its
//!   listeners one process a pass until none is left (`memorystatus_update_vm_pressure` and
//!   `vm_pressure_select_optimal_candidate_to_notify` [C: XNU xnu-11215.1.10, `bsd/kern/kern_memorystatus_notify.c`]),
//!   so each listener hears each change. kqueue's `EVFILT_VM` (`NOTE_VM_PRESSURE`) was the other candidate: XNU's
//!   pressure notifier in that file names only the dispatch listeners, so it is not taken. The source's handler runs
//!   on a dispatch thread and writes one byte to a pipe this owns; the caller awaits the pipe's read end readable and
//!   empties it ([`Events::acknowledge`]).
//! - **Linux**: a pressure-stall trigger on `/proc/pressure/memory` for any stall in the shortest window an
//!   unprivileged process may ask for ("some 1 2000000": a microsecond, the kernel's unit, in two seconds), which the
//!   kernel signals with `POLLPRI`, "rate-limited to one per tracking window" [B: kernel
//!   `Documentation/accounting/psi.rst`, "Monitoring for pressure thresholds": "unprivileged users can also create
//!   monitors, with the only limitation that the window size must be a multiple of 2s"]. Any stall is the rule A-336's
//!   reading already takes (`crate::memory_stall`: a stall is the total risen). The trigger reports stalls and never
//!   their end, so the end is its silence: while stalls go on it reports once a window, its update interval a tenth of
//!   the window (`UPDATES_PER_WINDOW` in the kernel's `kernel/sched/psi.c`), so a window and an update interval with no
//!   report is the stall's end ([`Events::stall_ended_after_ns`]). The shard's runtime awaits readability alone, so the
//!   trigger sits in an epoll set of its own, which is readable while the trigger has fired (epoll(7): an epoll
//!   descriptor "is readable if one or more events are available"); the caller awaits that set readable and empties it
//!   ([`Events::acknowledge`]). A kernel built without PSI, or one before unprivileged triggers (Linux 6.5), refuses:
//!   [`Refused::Host`].
//! - **Windows**: the low- and high-memory resource notifications (`CreateMemoryResourceNotification`), each "signaled"
//!   while its condition holds [B: Microsoft Learn, "CreateMemoryResourceNotification function (memoryapi.h)"]: the
//!   caller awaits whichever names the next change ([`Events::awaited`]: the high one while memory is low, the low one
//!   otherwise) and says when it fired ([`Events::crossed`]). Neither stays awaited while it is signaled, so nothing
//!   spins.
//! - **Elsewhere**, and under Miri: no reports ([`Refused::Unsupported`]).
//!
//! A host whose kernel offers none, or refuses one, has its watch refused by kind at the start: the caller says so in
//! its log and counts it, and nothing else is polled in its place (§2's ban on a second path).

use std::fmt;

pub use platform::Events;

/// Why this process has no reports of the host's memory pressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// The platform offers none to this build (another platform, or Miri).
  Unsupported,
  /// The host refused one: the call it refused and the error it answered.
  Host {
    /// The call, in the platform's words.
    call: &'static str,
    /// The error number (`errno`, or Windows' last error) the call answered.
    errno: i32,
  },
}

impl fmt::Display for Refused {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Refused::Unsupported => write!(f, "no memory-pressure reports on this platform"),
      Refused::Host { call, errno } => write!(f, "{call} refused (error {errno})"),
    }
  }
}

#[cfg(all(target_os = "macos", not(miri)))]
mod platform {
  use std::ffi::c_void;
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use super::Refused;

  /// libdispatch's memory-pressure source type, named only by its address: `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` is
  /// `&_dispatch_source_type_memorypressure` (dispatch/source.h).
  #[repr(C)]
  struct SourceType {
    _opaque: [u8; 0],
  }

  // libdispatch is part of libSystem, which every macOS binary links; no wrapper crate is taken for nine calls.
  unsafe extern "C" {
    static _dispatch_source_type_memorypressure: SourceType;
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
    fn dispatch_source_create(
      kind: *const SourceType,
      handle: usize,
      mask: usize,
      queue: *mut c_void,
    ) -> *mut c_void;
    fn dispatch_set_context(object: *mut c_void, context: *mut c_void);
    fn dispatch_source_set_event_handler_f(
      source: *mut c_void,
      handler: Option<extern "C" fn(*mut c_void)>,
    );
    fn dispatch_source_set_cancel_handler_f(
      source: *mut c_void,
      handler: Option<extern "C" fn(*mut c_void)>,
    );
    fn dispatch_resume(object: *mut c_void);
    fn dispatch_source_cancel(source: *mut c_void);
    fn dispatch_release(object: *mut c_void);
  }

  /// Format: every level the source can report (dispatch/source.h): `DISPATCH_MEMORYPRESSURE_NORMAL` (1), `_WARN` (2)
  /// and `_CRITICAL` (4), so a return to normal is a report as a rise is.
  const EVERY_LEVEL: usize = 0x1 | 0x2 | 0x4;
  /// Format: `DISPATCH_QUEUE_PRIORITY_DEFAULT`, the global queue the handler runs on: it writes one byte.
  const DEFAULT_PRIORITY: isize = 0;
  /// Format: the byte a report writes; its value means nothing, its arrival does.
  const REPORT: [u8; 1] = [1];
  /// Format: how many waiting reports one read takes: they mean one thing, so any size empties the pipe in a few reads.
  const READ_AT_ONCE: usize = 64;

  /// What the source's handler holds: the pipe's write end, closed by the cancel handler once no report can run.
  struct Writer {
    end: OwnedFd,
  }

  /// The host's memory-pressure reports (macOS): a dispatch source and the pipe its handler writes to. Dropped, the
  /// source is cancelled; its cancel handler, which runs after any report in flight, closes the write end.
  #[derive(Debug)]
  pub struct Events {
    /// The dispatch source, by address (`expose_provenance`), so the reports can move with the task that awaits them.
    source: usize,
    /// The pipe's read end: readable once a report is waiting.
    read: OwnedFd,
  }

  /// The handler of a report: one byte into the pipe. A full pipe already holds a report, and the reader takes them
  /// all at once, so a refused write loses nothing.
  extern "C" fn on_report(context: *mut c_void) {
    // SAFETY: `context` is the `Writer` `Events::open` leaked for this source, freed only by `on_cancel`, which
    // libdispatch runs after the last report handler has returned and never alongside one.
    let writer = unsafe { &*context.cast::<Writer>() };
    let _full_or_closed = rustix::io::write(&writer.end, &REPORT);
  }

  /// The source's cancel handler: frees its `Writer`, closing the pipe's write end.
  extern "C" fn on_cancel(context: *mut c_void) {
    // SAFETY: `context` is the `Writer` `Events::open` leaked for this source; libdispatch runs the cancel handler
    // once, after every report handler, so nothing reads it after this.
    drop(unsafe { Box::from_raw(context.cast::<Writer>()) });
  }

  impl Events {
    /// The host's memory-pressure reports, from now: a source at every level and an empty pipe.
    pub fn open() -> Result<Events, Refused> {
      let (read, write) = rustix::pipe::pipe().map_err(|errno| host("pipe", errno))?;
      for end in [&read, &write] {
        rustix::fs::fcntl_setfl(end, rustix::fs::OFlags::NONBLOCK)
          .map_err(|errno| host("fcntl(F_SETFL, O_NONBLOCK)", errno))?;
        rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC)
          .map_err(|errno| host("fcntl(F_SETFD, FD_CLOEXEC)", errno))?;
      }
      // SAFETY: asks for one of the process's global queues, which libdispatch keeps for the process's life.
      let queue = unsafe { dispatch_get_global_queue(DEFAULT_PRIORITY, 0) };
      // SAFETY: the type is libdispatch's own static; the handle is unused by this type (0); the mask names levels
      // the type accepts; the queue is the global one above. A null answer is a refusal, checked below.
      let source = unsafe {
        dispatch_source_create(
          &raw const _dispatch_source_type_memorypressure,
          0,
          EVERY_LEVEL,
          queue,
        )
      };
      if source.is_null() {
        return Err(Refused::Host {
          call: "dispatch_source_create(DISPATCH_SOURCE_TYPE_MEMORYPRESSURE)",
          errno: 0,
        });
      }
      let context = Box::into_raw(Box::new(Writer { end: write }));
      // SAFETY: `source` is the live source made above, suspended until `dispatch_resume`, so no handler runs before
      // its context and handlers are set; `context` is freed only by `on_cancel`.
      unsafe {
        dispatch_set_context(source, context.cast());
        dispatch_source_set_event_handler_f(source, Some(on_report));
        dispatch_source_set_cancel_handler_f(source, Some(on_cancel));
        dispatch_resume(source);
      }
      Ok(Events {
        source: source.expose_provenance(),
        read,
      })
    }

    /// The descriptor the caller awaits readable: a report is waiting.
    pub fn descriptor(&self) -> BorrowedFd<'_> {
      self.read.as_fd()
    }

    /// None: the source reports a return to normal itself.
    pub fn stall_ended_after_ns(&self) -> Option<u64> {
      None
    }

    /// Takes every waiting report, so the descriptor is readable again only at the next.
    pub fn acknowledge(&self) {
      let mut taken = [0u8; READ_AT_ONCE];
      while rustix::io::read(&self.read, &mut taken).is_ok_and(|read| read > 0) {}
    }
  }

  impl Drop for Events {
    fn drop(&mut self) {
      let source: *mut c_void = std::ptr::with_exposed_provenance_mut(self.source);
      // SAFETY: `source` is the source `open` made and this owns; cancelling stops later reports and runs
      // `on_cancel` after any in flight, and the release gives back this reference once, here.
      unsafe {
        dispatch_source_cancel(source);
        dispatch_release(source);
      }
    }
  }

  /// A refused call's error, typed.
  fn host(call: &'static str, errno: rustix::io::Errno) -> Refused {
    Refused::Host {
      call,
      errno: errno.raw_os_error(),
    }
  }
}

#[cfg(all(target_os = "linux", not(miri)))]
mod platform {
  use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

  use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};

  use super::Refused;

  /// Format: the least stall a trigger can be set to, in the kernel's unit, a microsecond (psi.rst: `<stall amount in
  /// us>`; the kernel refuses zero): any stall at all, the rule A-336's reading takes.
  const ANY_STALL_US: u64 = 1;
  /// Format: the shortest window an unprivileged process may give a trigger, in microseconds (psi.rst: "the window
  /// size must be a multiple of 2s"); no privilege is ever asked for (R10).
  const UNPRIVILEGED_WINDOW_US: u64 = 2_000_000;
  /// Format: the reports one look takes: the set holds one descriptor.
  const AT_ONCE: usize = 1;
  /// Format: the kernel's trigger updates per tracking window (`UPDATES_PER_WINDOW` in `kernel/sched/psi.c`; psi.rst:
  /// windows of 500 ms to 10 s give update intervals of 50 ms to 1 s): a report can come up to an update interval past
  /// the window.
  const UPDATES_PER_WINDOW: u64 = 10;
  /// Format: nanoseconds per microsecond, the trigger's unit.
  const NS_PER_US: u64 = 1_000;

  /// The host's memory-pressure reports (Linux): the trigger's descriptor and the epoll set it sits in alone.
  #[derive(Debug)]
  pub struct Events {
    /// The trigger: closing it removes it (psi.rst: "the trigger will de-register when the file descriptor used to
    /// define the trigger is closed").
    _trigger: OwnedFd,
    /// The set the caller awaits readable: the trigger has fired.
    set: OwnedFd,
  }

  impl Events {
    /// A trigger for any memory stall in the shortest unprivileged window, from now.
    pub fn open() -> Result<Events, Refused> {
      let trigger = rustix::fs::open(
        "/proc/pressure/memory",
        // structural: allow — the kernel's PSI trigger is written to its interface file, not a host path (psi.rst).
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
      )
      .map_err(|errno| host("open(/proc/pressure/memory)", errno))?;
      let text = format!("some {ANY_STALL_US} {UNPRIVILEGED_WINDOW_US}");
      rustix::io::write(&trigger, text.as_bytes())
        .map_err(|errno| host("write(memory pressure trigger)", errno))?;
      let set =
        epoll::create(CreateFlags::CLOEXEC).map_err(|errno| host("epoll_create1", errno))?;
      epoll::add(&set, &trigger, EventData::new_u64(0), EventFlags::PRI)
        .map_err(|errno| host("epoll_ctl(EPOLLPRI)", errno))?;
      Ok(Events {
        _trigger: trigger,
        set,
      })
    }

    /// The descriptor the caller awaits readable: the trigger has fired.
    pub fn descriptor(&self) -> BorrowedFd<'_> {
      self.set.as_fd()
    }

    /// How long after a report the silence of the trigger is the stall's end: a tracking window and one update
    /// interval, since while stalls go on the kernel reports once a window.
    pub fn stall_ended_after_ns(&self) -> Option<u64> {
      let window = UNPRIVILEGED_WINDOW_US.saturating_mul(NS_PER_US);
      Some(window.saturating_add(window / UPDATES_PER_WINDOW))
    }

    /// Takes the trigger's report, so the set is readable again only at the next.
    pub fn acknowledge(&self) {
      let mut taken: Vec<epoll::Event> = Vec::with_capacity(AT_ONCE);
      let now = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
      };
      let _reports = epoll::wait(
        &self.set,
        rustix::buffer::spare_capacity(&mut taken),
        Some(&now),
      );
    }
  }

  /// A refused call's error, typed.
  fn host(call: &'static str, errno: rustix::io::Errno) -> Refused {
    Refused::Host {
      call,
      errno: errno.raw_os_error(),
    }
  }
}

#[cfg(all(windows, not(miri)))]
mod platform {
  use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};

  use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
  use windows_sys::Win32::System::Memory::{
    CreateMemoryResourceNotification, HighMemoryResourceNotification,
    LowMemoryResourceNotification, QueryMemoryResourceNotification,
  };

  use super::Refused;

  /// The host's memory-pressure reports (Windows): its low- and high-memory notifications, and which side of them
  /// memory was on at the last report.
  #[derive(Debug)]
  pub struct Events {
    low: OwnedHandle,
    high: OwnedHandle,
    /// Whether memory was low at the last report (or at the open): the high notification names the next change then.
    low_now: bool,
  }

  impl Events {
    /// The two notifications, and whether memory is low now.
    pub fn open() -> Result<Events, Refused> {
      let low = notification(true)?;
      let high = notification(false)?;
      let mut signaled: windows_sys::core::BOOL = 0;
      // SAFETY: `low` is the notification made above, open while this frame holds it; `signaled` is this frame's own
      // value for the call to write, read only when the call succeeds.
      let queried =
        unsafe { QueryMemoryResourceNotification(low.as_raw_handle(), &raw mut signaled) };
      if queried == 0 {
        return Err(last("QueryMemoryResourceNotification"));
      }
      Ok(Events {
        low,
        high,
        low_now: signaled != 0,
      })
    }

    /// The notification whose signal is the next change: the high one while memory is low, the low one otherwise.
    pub fn awaited(&self) -> BorrowedHandle<'_> {
      if self.low_now {
        self.high.as_handle()
      } else {
        self.low.as_handle()
      }
    }

    /// None: the high-memory notification reports the recovery itself.
    pub fn stall_ended_after_ns(&self) -> Option<u64> {
      None
    }

    /// The awaited notification signaled: memory is on its other side now.
    pub fn crossed(&mut self) {
      self.low_now = !self.low_now;
    }
  }

  /// The low (`low`) or high memory notification, owned.
  fn notification(low: bool) -> Result<OwnedHandle, Refused> {
    let kind = if low {
      LowMemoryResourceNotification
    } else {
      HighMemoryResourceNotification
    };
    // SAFETY: takes the notification type by value and returns a handle the process owns, or null on a refusal; a
    // non-null one is taken into an `OwnedHandle` at once, which closes it once.
    let handle: HANDLE = unsafe { CreateMemoryResourceNotification(kind) };
    if handle.is_null() {
      return Err(last("CreateMemoryResourceNotification"));
    }
    // SAFETY: see above: a live handle nothing else owns.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
  }

  /// The calling thread's last error, typed.
  fn last(call: &'static str) -> Refused {
    // SAFETY: reads the calling thread's last-error value; no argument.
    let code = unsafe { GetLastError() };
    Refused::Host {
      call,
      errno: i32::try_from(code).unwrap_or(i32::MAX),
    }
  }
}

#[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
mod platform {
  use super::Refused;

  /// No reports here.
  #[derive(Debug)]
  pub struct Events {
    _none: (),
  }

  impl Events {
    /// Refused: the platform offers none to this build.
    pub fn open() -> Result<Events, Refused> {
      Err(Refused::Unsupported)
    }

    /// None: no report comes here.
    pub fn stall_ended_after_ns(&self) -> Option<u64> {
      None
    }
  }
}

#[cfg(test)]
mod tests {
  /// A-570. Do: open the host's memory-pressure reports on this platform, then take whatever is waiting. Expect: open
  /// on macOS and Windows, which offer reports to every process; on Linux open or refused by the host (a kernel without
  /// PSI, or one before unprivileged triggers), never refused as unsupported; elsewhere refused as unsupported. Taking
  /// with nothing waiting returns at once, and a dropped source frees what it holds without a report.
  #[test]
  fn the_hosts_memory_pressure_reports_open_where_the_platform_offers_them() {
    let opened = super::Events::open();
    #[cfg(all(any(target_os = "macos", windows), not(miri)))]
    assert!(opened.is_ok(), "the platform's reports open: {opened:?}");
    #[cfg(all(target_os = "linux", not(miri)))]
    assert!(
      !matches!(opened, Err(super::Refused::Unsupported)),
      "Linux opens a trigger or names the host's refusal: {opened:?}"
    );
    #[cfg(any(not(any(target_os = "linux", target_os = "macos", windows)), miri))]
    assert_eq!(opened.as_ref().err(), Some(&super::Refused::Unsupported));
    #[cfg(all(any(target_os = "linux", target_os = "macos"), not(miri)))]
    if let Ok(events) = &opened {
      events.acknowledge();
    }
    drop(opened);
  }
}
