//! The kqueue driver (macOS): `EVFILT_USER` for kicks and readiness filters for sockets, every call through
//! `kevent64` [B: kevent(2)]. macOS has no completion I/O, so later phases drive sockets by readiness through the
//! same `wait`.
//!
//! A wait with a zero timeout is a poll, which the shard's idle spin makes on every turn (`Shard::spin_for_work`),
//! so it must never give the core up. kevent(2) names two ways to poll: `KEVENT_FLAG_IMMEDIATE` in `kevent64`'s
//! flags, or a zero-valued timeout. They are not alike. On macOS 26.4.1 (Apple M5 Max, 2026-11-05), each form on a
//! fresh queue, 5,000 calls a round, three rounds, every return checked: the zero timeout took 14.6–15.5 µs a call
//! and cost 1.00–1.07 involuntary context switches a call, through `kevent` and `kevent64` alike; the immediate flag
//! took 180–186 ns and none. Two threads spinning on a word, each turn carrying the poll, answered each other in
//! 15.1–15.5 µs at the median with the zero timeout and 416 ns with the flag. So a zero timeout is the immediate
//! flag (`tests/spin_keeps_core.rs`; `docs/bugs/2026-11-05-a-spinning-shard-gave-its-core-up-on-every-turn.md`).
//!
//! Every call on the queue is `kevent64`, the registrations too: XNU fixes a queue's record format at its first
//! call, and refused a `kevent64` on a queue `kevent` had registered with `EINVAL` (measured the same day). The
//! queue is macOS's alone: slates ships no other kqueue system, and `kevent64` is Apple's.
//!
//! The queue is created and its kick event registered by [`prepare`] on whatever thread builds
//! the shard's seed (the registry owns the descriptor and pins each foreign borrow);
//! the driver itself, which holds an event buffer of raw kernel records, is built on the shard's
//! own thread by [`KqueueDriver::from_prepared`]. Two calls are unsafe, as every `kevent64` is: [`apply_change`],
//! which passes one valid change record and no output, and the wait, which passes the driver's buffer and its
//! length.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::kqueue::kqueue;

use crate::driver::KickFd;
use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: u64 = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

/// Format: a kernel event record with every field zero, the wait buffer's filler; each wait overwrites the records
/// it returns, and only those are read.
const EMPTY_EVENT: libc::kevent64_s = libc::kevent64_s {
  ident: 0,
  filter: 0,
  flags: 0,
  fflags: 0,
  data: 0,
  udata: 0,
  ext: [0; 2],
};

/// The driver.
pub struct KqueueDriver {
  kq: KickFd,
  epoch: Instant,
  events: Vec<libc::kevent64_s>,
  nops: Vec<u64>,
}

impl std::fmt::Debug for KqueueDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("KqueueDriver")
      .field("events_capacity", &self.events.len())
      .finish()
  }
}

/// A change record for `ident` on `filter`, carrying `udata` back on each event it raises.
fn change_record(ident: u64, filter: i16, flags: u16, fflags: u32, udata: u64) -> libc::kevent64_s {
  libc::kevent64_s {
    ident,
    filter,
    flags,
    fflags,
    udata,
    ..EMPTY_EVENT
  }
}

/// Applies one change record to the queue and receives nothing; the OS error code on refusal.
fn apply_change(kq: &OwnedFd, change: &libc::kevent64_s) -> Result<(), Option<i32>> {
  // SAFETY: the queue is open (an owned descriptor borrowed for the call); the change list is the one record behind
  // the reference, live for the call; the output list is empty, so the kernel writes nothing, and with nothing to
  // receive the call returns once the change is applied.
  let returned = unsafe {
    libc::kevent64(
      kq.as_raw_fd(),
      change,
      1,
      std::ptr::null_mut(),
      0,
      0,
      std::ptr::null(),
    )
  };
  if returned < 0 {
    return Err(std::io::Error::last_os_error().raw_os_error());
  }
  Ok(())
}

/// Creates the queue and registers the kick event; the descriptor is owned by the registry slot
/// that the shard registers into (closed at unregistration, never leaked).
pub fn prepare() -> Result<OwnedFd, RtError> {
  let kq = kqueue().map_err(|e| refused("kqueue", e))?;
  let register = change_record(
    KICK_IDENT,
    libc::EVFILT_USER,
    libc::EV_ADD | libc::EV_CLEAR,
    0,
    0,
  );
  apply_change(&kq, &register).map_err(|code| RtError::DriverRefused {
    call: "kevent64(EV_ADD EVFILT_USER)",
    code,
  })?;
  Ok(kq)
}

/// Triggers the kick event on `kq`; safe from any thread. A closed slot (the shard unregistered)
/// makes it a no-op.
pub fn trigger(kq: &KickFd) {
  let _ = kq.with(|kq| {
    let trigger = change_record(KICK_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
    // A closed queue refuses the change, which a no-op kick ignores.
    let _ = apply_change(kq, &trigger);
  });
}

impl KqueueDriver {
  /// Registers a one-shot `filter` on descriptor `raw` whose event carries `user_data`. A negative descriptor is
  /// refused as the kernel would refuse it, `EBADF`.
  fn register_once(
    &mut self,
    raw: i32,
    filter: i16,
    user_data: u64,
    call: &'static str,
  ) -> Result<(), RtError> {
    let ident = u64::try_from(raw).map_err(|_| RtError::DriverRefused {
      call,
      code: Some(libc::EBADF),
    })?;
    let change = change_record(ident, filter, libc::EV_ADD | libc::EV_ONESHOT, 0, user_data);
    self
      .kq
      .with(|kq| apply_change(kq, &change))
      .ok_or(RtError::DriverLost)?
      .map_err(|code| RtError::DriverRefused { call, code })
  }

  /// Builds the driver over the slot's prepared queue, on the shard's thread.
  pub fn from_prepared(kq: KickFd) -> KqueueDriver {
    KqueueDriver {
      kq,
      epoch: Instant::now(),
      events: vec![EMPTY_EVENT; EVENTS_PER_WAIT],
      nops: Vec::new(),
    }
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
    let mut timeout_ns = timeout_ns;
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout_ns = Some(0);
    }
    // A zero timeout is a poll: the immediate flag, never a zero-valued timeout (the module doc's measurement).
    let (flags, timeout) = match timeout_ns {
      Some(0) => (libc::KEVENT_FLAG_IMMEDIATE, None),
      Some(ns) => (0, Some(timespec_of(ns))),
      None => (0, None),
    };
    let capacity = libc::c_int::try_from(self.events.len()).unwrap_or(libc::c_int::MAX);
    let events = self.events.as_mut_ptr();
    let returned = self
      .kq
      .with(|kq| {
        let timeout = timeout
          .as_ref()
          .map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: the queue is open for the call (the registry pins it inside `with`); the change list is empty;
        // the output is the driver's own buffer, `capacity` records long, which the kernel fills no further; the
        // timeout, when given, points to this frame's value, live for the call.
        unsafe {
          libc::kevent64(
            kq.as_raw_fd(),
            std::ptr::null(),
            0,
            events,
            capacity,
            flags,
            timeout,
          )
        }
      })
      .ok_or(RtError::DriverLost)?;
    let Ok(count) = usize::try_from(returned) else {
      let code = std::io::Error::last_os_error().raw_os_error();
      return match code {
        Some(libc::EINTR) => Ok(()),
        Some(libc::EBADF) => Err(RtError::DriverLost),
        _ => Err(RtError::DriverRefused {
          call: "kevent64",
          code,
        }),
      };
    };
    // Kick events carry nothing; other filters (later phases) become completions keyed by udata.
    for ev in self.events.iter().take(count) {
      if ev.filter != libc::EVFILT_USER {
        out.push(Completion {
          user_data: ev.udata,
          result: 0,
        });
      }
    }
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot read filter whose udata carries the waker word; the `wait` loop above turns the
    // ready event into a completion keyed by that word (the same path the kick's siblings take).
    self.register_once(raw, libc::EVFILT_READ, user_data, "kevent64(EVFILT_READ)")
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot write filter whose udata carries the waker word; the `wait` loop turns the ready
    // event into a completion keyed by that word, exactly as the read filter does. The socket becomes
    // writable when its send buffer has space again (§4.6, TCP send backpressure to a stalled client).
    self.register_once(raw, libc::EVFILT_WRITE, user_data, "kevent64(EVFILT_WRITE)")
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}

/// A wait's timeout as the kernel's `timespec`; a span past its seconds field waits as long as the field holds.
fn timespec_of(timeout_ns: u64) -> libc::timespec {
  let span = Duration::from_nanos(timeout_ns);
  libc::timespec {
    tv_sec: libc::time_t::try_from(span.as_secs()).unwrap_or(libc::time_t::MAX),
    tv_nsec: libc::c_long::from(span.subsec_nanos()),
  }
}
