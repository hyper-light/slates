//! The epoll driver (Linux fallback when io_uring is refused, as in containers whose seccomp
//! profile blocks it): an eventfd for kicks registered on an epoll instance, `epoll_wait` with a
//! timeout for the wait [B: epoll(7); B: eventfd(2)].

use std::ffi::c_void;
use std::os::fd::RawFd;
use std::time::Instant;

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since};
use crate::error::RtError;

/// Format: the user word that marks the kick eventfd in epoll events.
const KICK_TAG: u64 = u64::MAX;

/// Shape: events drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
#[derive(Debug)]
pub struct EpollDriver {
  epfd: RawFd,
  efd: RawFd,
  epoch: Instant,
  events: Vec<libc::epoll_event>,
  nops: Vec<u64>,
}

impl EpollDriver {
  /// Creates the instance and the kick eventfd.
  pub fn new() -> Result<EpollDriver, RtError> {
    // SAFETY: no preconditions; results are checked.
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
      return Err(RtError::os("epoll_create1"));
    }
    // SAFETY: as above.
    let efd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if efd < 0 {
      let err = RtError::os("eventfd");
      // SAFETY: ours to close.
      unsafe { libc::close(epfd) };
      return Err(err);
    }
    let mut ev = libc::epoll_event {
      events: u32::try_from(libc::EPOLLIN).unwrap_or(0),
      u64: KICK_TAG,
    };
    // SAFETY: registering an open descriptor on an open instance with a valid event record.
    if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, efd, &raw mut ev) } < 0 {
      let err = RtError::os("epoll_ctl(ADD eventfd)");
      // SAFETY: ours to close.
      unsafe {
        libc::close(efd);
        libc::close(epfd);
      }
      return Err(err);
    }
    Ok(EpollDriver {
      epfd,
      efd,
      epoch: Instant::now(),
      events: (0..EVENTS_PER_WAIT)
        .map(|_| libc::epoll_event { events: 0, u64: 0 })
        .collect(),
      nops: Vec::new(),
    })
  }

  fn drain_kick(&self) {
    let mut word: u64 = 0;
    // SAFETY: an open non-blocking eventfd and an eight-byte buffer; EAGAIN is fine.
    unsafe {
      libc::read(
        self.efd,
        (&raw mut word).cast::<c_void>(),
        std::mem::size_of::<u64>(),
      )
    };
  }
}

impl Drop for EpollDriver {
  fn drop(&mut self) {
    // The eventfd is leaked on purpose: the registry's kick handle may still name it, and a
    // write into a reused descriptor number would be a fault in someone else's file.
    // SAFETY: ours to close.
    unsafe { libc::close(self.epfd) };
  }
}

impl Driver for EpollDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::Epoll
  }

  fn kick_handle(&self) -> Kick {
    Kick::Eventfd(self.efd)
  }

  fn now_ns(&self) -> u64 {
    nanos_since(self.epoch)
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let mut timeout_ms = timeout_ns.map_or(-1, millis_ceil);
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout_ms = 0;
    }
    let capacity = libc::c_int::try_from(self.events.len()).unwrap_or(libc::c_int::MAX);
    // SAFETY: the output buffer holds `capacity` records.
    let n = unsafe { libc::epoll_wait(self.epfd, self.events.as_mut_ptr(), capacity, timeout_ms) };
    if n < 0 {
      let code = std::io::Error::last_os_error().raw_os_error();
      return match code {
        Some(libc::EINTR) => Ok(()),
        Some(libc::EBADF) => Err(RtError::DriverLost),
        _ => Err(RtError::DriverRefused {
          call: "epoll_wait",
          code,
        }),
      };
    }
    for ev in self.events.iter().take(usize::try_from(n).unwrap_or(0)) {
      if ev.u64 == KICK_TAG {
        self.drain_kick();
      } else {
        out.push(Completion {
          user_data: ev.u64,
          result: i32::try_from(ev.events).unwrap_or(0),
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

/// Milliseconds rounded up, so a wait never returns before its deadline.
fn millis_ceil(ns: u64) -> libc::c_int {
  /// Format: nanoseconds per millisecond.
  const NANOS_PER_MILLI: u64 = 1_000_000;
  libc::c_int::try_from(ns.div_ceil(NANOS_PER_MILLI)).unwrap_or(libc::c_int::MAX)
}
