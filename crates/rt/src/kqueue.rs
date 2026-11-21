//! The kqueue driver (macOS, BSD): `EVFILT_USER` for kicks, `kevent` with a timeout for the wait
//! [B: kqueue(2)], through rustix's wrappers. macOS has no completion I/O, so later phases drive
//! sockets by readiness through the same `wait`.
//!
//! The queue is created and its kick event registered by [`prepare`] on whatever thread builds
//! the shard's seed (the registry owns the descriptor and pins each foreign borrow);
//! the driver itself, which holds an event buffer of raw kernel records, is built on the shard's
//! own thread by [`KqueueDriver::from_prepared`]. rustix marks `kevent` unsafe because the
//! output buffer is filled by the kernel; the three calls here pass a change list of valid
//! events and a buffer with the capacity the call may fill.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::driver::KickFd;
use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: usize = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct KqueueDriver {
  kq: KickFd,
  epoch: Instant,
  events: Vec<sys::Record>,
  nops: Vec<u64>,
}

impl std::fmt::Debug for KqueueDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("KqueueDriver")
      .field("events_capacity", &self.events.capacity())
      .finish()
  }
}

/// Creates the queue and registers the kick event; the descriptor is owned by the registry slot
/// that the shard registers into (closed at unregistration, never leaked).
pub fn prepare() -> Result<OwnedFd, RtError> {
  let kq = rustix::event::kqueue::kqueue().map_err(|e| refused("kqueue", e))?;
  sys::change(kq.as_raw_fd(), sys::Change::RegisterKick)
    .map_err(|e| refused("kevent(EV_ADD EVFILT_USER)", e))?;
  Ok(kq)
}

/// Triggers the kick event on `kq`; safe from any thread. A closed slot (the shard unregistered)
/// makes it a no-op.
pub fn trigger(kq: &KickFd) {
  // A closed queue returns an error, ignored: the shard is gone and nothing waits on it.
  let _ = kq.with(|kq| sys::change(kq.as_raw_fd(), sys::Change::Trigger));
}

impl KqueueDriver {
  /// Builds the driver over the slot's prepared queue, on the shard's thread.
  pub fn from_prepared(kq: KickFd) -> KqueueDriver {
    KqueueDriver {
      kq,
      epoch: Instant::now(),
      events: Vec::with_capacity(EVENTS_PER_WAIT),
      nops: Vec::new(),
    }
  }

  /// Adds a one-shot filter for `raw` whose record carries `user_data`, the waker word the `wait` loop turns into a
  /// completion.
  fn register_once(&mut self, change: sys::Change, call: &'static str) -> Result<(), RtError> {
    self
      .kq
      .with(|kq| sys::change(kq.as_raw_fd(), change))
      .ok_or(RtError::DriverLost)?
      .map_err(|e| refused(call, e))
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
    let mut timeout = timeout_ns.map(Duration::from_nanos);
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout = Some(Duration::ZERO);
    }
    self.collect(timeout, out)
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot read filter whose udata carries the waker word; the `wait` loop above turns the
    // ready event into a completion keyed by that word (the same path the kick's siblings take).
    self.register_once(sys::Change::Readable(raw, user_data), "kevent(EVFILT_READ)")
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot write filter whose udata carries the waker word; the `wait` loop turns the ready
    // event into a completion keyed by that word, exactly as the read filter does. The socket becomes
    // writable when its send buffer has space again (§4.6, TCP send backpressure to a stalled client).
    self.register_once(
      sys::Change::Writable(raw, user_data),
      "kevent(EVFILT_WRITE)",
    )
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}

impl KqueueDriver {
  /// One call for the queue's events, waiting up to `timeout` (a zero one polls), each event but the kick's appended
  /// to `out` as a completion keyed by its udata.
  fn collect(
    &mut self,
    timeout: Option<Duration>,
    out: &mut Vec<Completion>,
  ) -> Result<(), RtError> {
    let events = &mut self.events;
    let outcome = self
      .kq
      .with(|kq| sys::poll(kq.as_raw_fd(), events, timeout))
      .ok_or(RtError::DriverLost)?;
    if let Err(e) = outcome {
      return match e {
        rustix::io::Errno::INTR => Ok(()),
        rustix::io::Errno::BADF => Err(RtError::DriverLost),
        other => Err(refused("kevent", other)),
      };
    }
    // Kick events carry nothing; other filters become completions keyed by udata.
    out.extend(
      self
        .events
        .iter()
        .filter_map(sys::user_data)
        .map(|user_data| Completion {
          user_data,
          result: 0,
        }),
    );
    Ok(())
  }
}

/// The queue's system calls. macOS: `kevent64` for every call on the queue, so a poll with a zero timeout passes
/// `KEVENT_FLAG_IMMEDIATE` and returns at once; `kevent` with a zero timespec there sleeps 12–15 µs with an involuntary
/// switch (A-218's probe), and under load the switch loses the core for up to a time slice (A-722). XNU refuses the two
/// record formats on one queue (`EINVAL`), so the registrations and the kick go through `kevent64` too. FreeBSD:
/// `kevent` through rustix, whose zero timeout polls.
#[cfg(target_os = "macos")]
mod sys {
  use std::time::Duration;

  /// One kernel record of the queue.
  pub(super) type Record = libc::kevent64_s;

  /// A change the queue is told of.
  pub(super) enum Change {
    /// Adds the kick's `EVFILT_USER` event, cleared as it is delivered.
    RegisterKick,
    /// Triggers the kick.
    Trigger,
    /// Adds a one-shot read filter on the descriptor, carrying the waker word.
    Readable(i32, u64),
    /// Adds a one-shot write filter on the descriptor, carrying the waker word.
    Writable(i32, u64),
  }

  fn record(ident: u64, filter: i16, flags: u16, fflags: u32, udata: u64) -> Record {
    Record {
      ident,
      filter,
      flags,
      fflags,
      data: 0,
      udata,
      ext: [0; 2],
    }
  }

  /// Applies one change, receiving nothing.
  pub(super) fn change(kq: i32, change: Change) -> Result<(), rustix::io::Errno> {
    let kick = u64::try_from(super::KICK_IDENT).unwrap_or(0);
    let descriptor = |raw: i32| u64::try_from(raw).unwrap_or(u64::MAX);
    let one = match change {
      Change::RegisterKick => record(kick, libc::EVFILT_USER, libc::EV_ADD | libc::EV_CLEAR, 0, 0),
      Change::Trigger => record(kick, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0),
      Change::Readable(raw, udata) => record(
        descriptor(raw),
        libc::EVFILT_READ,
        libc::EV_ADD | libc::EV_ONESHOT,
        0,
        udata,
      ),
      Change::Writable(raw, udata) => record(
        descriptor(raw),
        libc::EVFILT_WRITE,
        libc::EV_ADD | libc::EV_ONESHOT,
        0,
        udata,
      ),
    };
    // SAFETY: the change list is one valid record on this frame; no output list; `KEVENT_FLAG_IMMEDIATE` so the call
    // never waits; the status is checked before anything else is read.
    let status = unsafe {
      libc::kevent64(
        kq,
        &raw const one,
        1,
        std::ptr::null_mut(),
        0,
        libc::KEVENT_FLAG_IMMEDIATE,
        std::ptr::null(),
      )
    };
    if status < 0 {
      return Err(rustix::io::Errno::from_raw_os_error(
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
      ));
    }
    Ok(())
  }

  /// Waits up to `timeout` (`None`: until an event) for the queue's events into `events`, replacing what it held; a
  /// zero timeout returns at once without giving up the thread (`KEVENT_FLAG_IMMEDIATE`).
  pub(super) fn poll(
    kq: i32,
    events: &mut Vec<Record>,
    timeout: Option<Duration>,
  ) -> Result<(), rustix::io::Errno> {
    events.clear();
    let immediate = timeout.is_some_and(|t| t.is_zero());
    let spec = timeout.map(|t| libc::timespec {
      tv_sec: libc::time_t::try_from(t.as_secs()).unwrap_or(libc::time_t::MAX),
      tv_nsec: libc::c_long::from(t.subsec_nanos()),
    });
    let capacity = libc::c_int::try_from(events.capacity()).unwrap_or(libc::c_int::MAX);
    // SAFETY: no changes; the output list is the vector's own allocation with room for `capacity` records, which the
    // kernel fills from the start; the length is set to the count it reports, never past the capacity; the timespec
    // lives on this frame for the call.
    let count = unsafe {
      libc::kevent64(
        kq,
        std::ptr::null(),
        0,
        events.as_mut_ptr(),
        capacity,
        if immediate {
          libc::KEVENT_FLAG_IMMEDIATE
        } else {
          0
        },
        spec.as_ref().map_or(std::ptr::null(), std::ptr::from_ref),
      )
    };
    let Ok(filled) = usize::try_from(count) else {
      return Err(rustix::io::Errno::from_raw_os_error(
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
      ));
    };
    // SAFETY: the kernel wrote `filled` records, at most the capacity it was given.
    unsafe { events.set_len(filled.min(events.capacity())) };
    Ok(())
  }

  /// The waker word of an event other than the kick's.
  pub(super) fn user_data(event: &Record) -> Option<u64> {
    (event.filter != libc::EVFILT_USER).then_some(event.udata)
  }
}

/// The queue's system calls on FreeBSD: `kevent` through rustix (see the macOS form's doc).
#[cfg(not(target_os = "macos"))]
mod sys {
  use std::os::fd::BorrowedFd;
  use std::time::Duration;

  use rustix::event::kqueue::{
    Event, EventFilter, EventFlags, UserDefinedFlags, UserFlags, kevent,
  };

  /// One kernel record of the queue.
  pub(super) type Record = Event;

  /// A change the queue is told of.
  pub(super) enum Change {
    /// Adds the kick's `EVFILT_USER` event, cleared as it is delivered.
    RegisterKick,
    /// Triggers the kick.
    Trigger,
    /// Adds a one-shot read filter on the descriptor, carrying the waker word.
    Readable(i32, u64),
    /// Adds a one-shot write filter on the descriptor, carrying the waker word.
    Writable(i32, u64),
  }

  fn user_event(flags: UserFlags, event_flags: EventFlags) -> Event {
    Event::new(
      EventFilter::User {
        ident: isize::try_from(super::KICK_IDENT).unwrap_or(0),
        flags,
        user_flags: UserDefinedFlags::new(0),
      },
      event_flags,
      std::ptr::null_mut(),
    )
  }

  fn udata(user_data: u64) -> *mut core::ffi::c_void {
    core::ptr::without_provenance_mut(usize::try_from(user_data).unwrap_or(usize::MAX))
  }

  /// Applies one change, receiving nothing.
  pub(super) fn change(kq: i32, change: Change) -> Result<(), rustix::io::Errno> {
    let one = match change {
      Change::RegisterKick => user_event(UserFlags::empty(), EventFlags::ADD | EventFlags::CLEAR),
      Change::Trigger => user_event(UserFlags::TRIGGER, EventFlags::empty()),
      Change::Readable(raw, user_data) => Event::new(
        EventFilter::Read(raw),
        EventFlags::ADD | EventFlags::ONESHOT,
        udata(user_data),
      ),
      Change::Writable(raw, user_data) => Event::new(
        EventFilter::Write(raw),
        EventFlags::ADD | EventFlags::ONESHOT,
        udata(user_data),
      ),
    };
    let mut none: Vec<Event> = Vec::new();
    // SAFETY: `kq` is the caller's open queue for the call; the change list is one valid event; the empty output
    // vector receives nothing.
    unsafe { kevent(BorrowedFd::borrow_raw(kq), &[one], &mut none, None) }.map(|_| ())
  }

  /// Waits up to `timeout` (`None`: until an event) for the queue's events into `events`, replacing what it held; a
  /// zero timeout polls here, so both forms are one call.
  pub(super) fn poll(
    kq: i32,
    events: &mut Vec<Record>,
    timeout: Option<Duration>,
  ) -> Result<(), rustix::io::Errno> {
    events.clear();
    // SAFETY: `kq` is the caller's open queue for the call; no changes; the output buffer is the vector's spare
    // capacity.
    unsafe {
      kevent(
        BorrowedFd::borrow_raw(kq),
        &[],
        rustix::buffer::spare_capacity(events),
        timeout,
      )
    }
    .map(|_| ())
  }

  /// The waker word of an event other than the kick's.
  pub(super) fn user_data(event: &Record) -> Option<u64> {
    (!matches!(event.filter(), EventFilter::User { .. }))
      .then(|| u64::try_from(event.udata().addr()).unwrap_or(0))
  }
}
