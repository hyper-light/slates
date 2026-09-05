//! The kqueue driver (macOS, BSD): `EVFILT_USER` for kicks, `kevent` with a timeout for the wait
//! [B: kqueue(2)]. macOS has no completion I/O, so later phases drive sockets by readiness through
//! the same `wait`.

use std::ffi::c_void;
use std::os::fd::RawFd;
use std::time::Instant;

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: usize = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
#[derive(Debug)]
pub struct KqueueDriver {
  kq: RawFd,
  epoch: Instant,
  events: Vec<libc::kevent>,
  nops: Vec<u64>,
}

impl KqueueDriver {
  /// Creates the queue and registers the kick event.
  pub fn new() -> Result<KqueueDriver, RtError> {
    // SAFETY: no preconditions; the result is checked.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
      return Err(RtError::os("kqueue"));
    }
    let register = event(
      KICK_IDENT,
      libc::EVFILT_USER,
      libc::EV_ADD | libc::EV_CLEAR,
      0,
    );
    // SAFETY: one change record, no output buffer, no timeout.
    if unsafe {
      libc::kevent(
        kq,
        &raw const register,
        1,
        std::ptr::null_mut(),
        0,
        std::ptr::null(),
      )
    } < 0
    {
      let err = RtError::os("kevent(EV_ADD EVFILT_USER)");
      // SAFETY: ours to close.
      unsafe { libc::close(kq) };
      return Err(err);
    }
    Ok(KqueueDriver {
      kq,
      epoch: Instant::now(),
      events: (0..EVENTS_PER_WAIT).map(|_| event(0, 0, 0, 0)).collect(),
      nops: Vec::new(),
    })
  }
}

fn event(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
  libc::kevent {
    ident,
    filter,
    flags,
    fflags,
    data: 0,
    udata: std::ptr::null_mut::<c_void>(),
  }
}

/// Triggers the kick event on `kq`; safe from any thread.
pub fn trigger(kq: RawFd) {
  let trigger = event(KICK_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER);
  // SAFETY: one change record on an open queue; a closed queue returns an error we ignore.
  unsafe {
    libc::kevent(
      kq,
      &raw const trigger,
      1,
      std::ptr::null_mut(),
      0,
      std::ptr::null(),
    )
  };
}

impl Drop for KqueueDriver {
  fn drop(&mut self) {
    // SAFETY: ours to close.
    unsafe { libc::close(self.kq) };
  }
}

impl Driver for KqueueDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Kqueue
  }

  fn kick_handle(&self) -> Kick {
    Kick::Kqueue(self.kq)
  }

  fn now_ns(&self) -> u64 {
    nanos_since(self.epoch)
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let mut timeout = timeout_ns.map(timespec);
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout = Some(timespec(0));
    }
    let timeout_ptr = timeout
      .as_ref()
      .map_or(std::ptr::null(), |t| t as *const libc::timespec);
    let capacity = libc::c_int::try_from(self.events.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: the output buffer holds `capacity` records; the timeout points at a local or is null.
    let n = unsafe {
      libc::kevent(
        self.kq,
        std::ptr::null(),
        0,
        self.events.as_mut_ptr(),
        capacity,
        timeout_ptr,
      )
    };
    if n < 0 {
      let code = std::io::Error::last_os_error().raw_os_error();
      return match code {
        Some(libc::EINTR) => Ok(()),
        Some(libc::EBADF) => Err(RtError::DriverLost),
        _ => Err(RtError::DriverRefused {
          call: "kevent",
          code,
        }),
      };
    }
    // Kick events carry nothing; other filters (later phases) become completions keyed by udata.
    for ev in self.events.iter().take(usize::try_from(n).unwrap_or(0)) {
      if ev.filter != libc::EVFILT_USER {
        out.push(Completion {
          user_data: u64::try_from(ev.udata.addr()).unwrap_or(0),
          result: i32::try_from(ev.data).unwrap_or(i32::MAX),
        });
      }
    }
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}

fn timespec(ns: u64) -> libc::timespec {
  /// Format: nanoseconds per second.
  const NANOS_PER_SECOND: u64 = 1_000_000_000;
  libc::timespec {
    tv_sec: libc::time_t::try_from(ns / NANOS_PER_SECOND).unwrap_or(libc::time_t::MAX),
    tv_nsec: libc::c_long::try_from(ns % NANOS_PER_SECOND).unwrap_or(0),
  }
}
