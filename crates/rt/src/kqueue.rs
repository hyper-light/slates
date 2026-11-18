//! The kqueue driver (macOS, BSD): `EVFILT_USER` for kicks, a wait with a timeout for the rest [B: kqueue(2)]. macOS
//! has no completion I/O, so sockets are driven by readiness through the same `wait`.
//!
//! The queue is created and its kick event registered by [`prepare`] on whatever thread builds
//! the shard's seed (the registry owns the descriptor and pins each foreign borrow);
//! the driver itself, which holds an event buffer of raw kernel records, is built on the shard's
//! own thread by [`KqueueDriver::from_prepared`]. Every call on the queue goes through `sys`, paired `#[cfg]` modules
//! with the same four operations, so the driver's logic is written once.
//!
//! **A poll never sleeps (A-355).** A spinning shard harvests the driver with a zero timeout on every turn of its idle
//! spin, and a busy one between tasks (`ShardContext::harvest_io`). kqueue(2) names a zero `timespec` as its poll, but
//! XNU takes it for a deadline of now: the thread blocks on it, is switched out, and runs again when the timer fires.
//! Measured on an M4 Max Mac Studio under macOS 26.2 (2026-11-18): 12.0–13.7 µs a call, 2.7 µs of it on the CPU, and
//! one context switch a call (20,002 over 20,000 calls), so a spinning shard looked at its rings once every 13 µs and
//! was switched out in between. `kevent64` with `KEVENT_FLAG_IMMEDIATE` took 0.135 µs a call with no switch, and still
//! returned a pending kick [B: kevent(2), `KEVENT_FLAG_IMMEDIATE`]. So on macOS every call on the queue goes through
//! `kevent64` — XNU refuses a queue that both `kevent` and `kevent64` have used (`EINVAL` on every call, measured the
//! same day) — and a zero timeout is `KEVENT_FLAG_IMMEDIATE` with no `timespec`. Other kqueue systems keep the
//! portable `kevent` through rustix's wrapper. Record: `docs/bugs/2026-11-18-a-macos-poll-slept-thirteen-microseconds.md`.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use crate::driver::{Completion, Driver, DriverKind, Kick, KickFd, nanos_since, refused};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: u8 = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

/// What one harvested record was: the kick, which carries nothing, or a readiness, named by the waker word its
/// registration carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fired {
  /// The kick event.
  Kick,
  /// A one-shot readiness filter fired; its waker word.
  Ready(u64),
}

/// The readiness a registration asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Interest {
  /// The descriptor has something to read (data, an incoming connection, end of file, an error).
  Read,
  /// The descriptor's send buffer has room (or a connect finished, or an error).
  Write,
}

/// macOS: `kevent64` for every call, a zero timeout as `KEVENT_FLAG_IMMEDIATE` (the module doc).
#[cfg(target_os = "macos")]
mod sys {
  use std::os::fd::{AsRawFd, OwnedFd};
  use std::time::Duration;

  use rustix::io::Errno;

  use super::{EVENTS_PER_WAIT, Fired, Interest, KICK_IDENT};

  /// The records one wait fills, kept by the driver.
  pub(super) type Records = [libc::kevent64_s; EVENTS_PER_WAIT];

  /// A record with every field given.
  const fn record(
    ident: u64,
    filter: i16,
    flags: u16,
    fflags: u32,
    udata: u64,
  ) -> libc::kevent64_s {
    libc::kevent64_s {
      ident,
      filter,
      flags,
      fflags,
      data: 0,
      udata,
      ext: [0; 2],
    }
  }

  /// An empty buffer for the waits.
  pub(super) const fn records() -> Records {
    [record(0, 0, 0, 0, 0); EVENTS_PER_WAIT]
  }

  /// One `kevent64` on `kq`: applies `changes`, then fills at most `out.len()` records, waiting at most `timeout`
  /// (`None`: until a record comes). A zero timeout is `KEVENT_FLAG_IMMEDIATE` with no `timespec`, the poll that
  /// returns at once. The records filled.
  fn call(
    kq: &OwnedFd,
    changes: &[libc::kevent64_s],
    out: &mut [libc::kevent64_s],
    timeout: Option<Duration>,
  ) -> Result<usize, Errno> {
    let changes_len = i32::try_from(changes.len()).map_err(|_| Errno::INVAL)?;
    let out_len = i32::try_from(out.len()).map_err(|_| Errno::INVAL)?;
    let (flags, spec) = match timeout {
      Some(timeout) if timeout.is_zero() => (libc::KEVENT_FLAG_IMMEDIATE, None),
      Some(timeout) => (
        libc::KEVENT_FLAG_NONE,
        Some(libc::timespec {
          tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(i64::MAX),
          tv_nsec: i64::from(timeout.subsec_nanos()),
        }),
      ),
      None => (libc::KEVENT_FLAG_NONE, None),
    };
    let spec = spec.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
    // SAFETY: `kq` is an open queue for the whole call; the change list and the output buffer are slices with their
    // lengths stated, so the kernel reads `changes_len` records and fills at most `out_len`; the `timespec`, when
    // given, lives in this frame past the call.
    let filled = unsafe {
      libc::kevent64(
        kq.as_raw_fd(),
        changes.as_ptr(),
        changes_len,
        out.as_mut_ptr(),
        out_len,
        flags,
        spec,
      )
    };
    usize::try_from(filled).map_err(|_| {
      Errno::from_raw_os_error(std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    })
  }

  /// Registers `kq`'s kick: a user event, cleared as it is delivered.
  pub(super) fn add_kick(kq: &OwnedFd) -> Result<(), Errno> {
    let add = record(
      u64::from(KICK_IDENT),
      libc::EVFILT_USER,
      libc::EV_ADD | libc::EV_CLEAR,
      0,
      0,
    );
    call(kq, &[add], &mut [], None).map(drop)
  }

  /// Fires `kq`'s kick.
  pub(super) fn trigger_kick(kq: &OwnedFd) -> Result<(), Errno> {
    let trigger = record(
      u64::from(KICK_IDENT),
      libc::EVFILT_USER,
      0,
      libc::NOTE_TRIGGER,
      0,
    );
    call(kq, &[trigger], &mut [], None).map(drop)
  }

  /// Registers a one-shot filter for `interest` on the descriptor `raw`, its record carrying `user_data`.
  pub(super) fn add_interest(
    kq: &OwnedFd,
    interest: Interest,
    raw: i32,
    user_data: u64,
  ) -> Result<(), Errno> {
    let ident = u64::try_from(raw).map_err(|_| Errno::BADF)?;
    let filter = match interest {
      Interest::Read => libc::EVFILT_READ,
      Interest::Write => libc::EVFILT_WRITE,
    };
    let add = record(ident, filter, libc::EV_ADD | libc::EV_ONESHOT, 0, user_data);
    call(kq, &[add], &mut [], None).map(drop)
  }

  /// Waits at most `timeout` for records into `records`, handing what each filled one was to `fired`.
  pub(super) fn wait(
    kq: &OwnedFd,
    records: &mut Records,
    timeout: Option<Duration>,
    fired: &mut dyn FnMut(Fired),
  ) -> Result<(), Errno> {
    let filled = call(kq, &[], records, timeout)?;
    for record in records.iter().take(filled) {
      fired(if record.filter == libc::EVFILT_USER {
        Fired::Kick
      } else {
        Fired::Ready(record.udata)
      });
    }
    Ok(())
  }
}

/// Other kqueue systems: the portable `kevent`, through rustix's wrapper, which marks it unsafe because the kernel fills
/// the output buffer.
#[cfg(not(target_os = "macos"))]
mod sys {
  use std::os::fd::OwnedFd;
  use std::time::Duration;

  use rustix::event::kqueue::{
    Event, EventFilter, EventFlags, UserDefinedFlags, UserFlags, kevent,
  };
  use rustix::io::Errno;

  use super::{EVENTS_PER_WAIT, Fired, Interest, KICK_IDENT};

  /// The records one wait fills, kept by the driver.
  pub(super) type Records = Vec<Event>;

  /// An empty buffer for the waits.
  pub(super) fn records() -> Records {
    Vec::with_capacity(EVENTS_PER_WAIT)
  }

  /// One change on `kq`, nothing harvested.
  fn submit(kq: &OwnedFd, change: Event) -> Result<(), Errno> {
    let mut none: Vec<Event> = Vec::new();
    // SAFETY: one valid change record on an open queue; the empty output vector receives nothing.
    unsafe { kevent(kq, &[change], &mut none, None) }.map(drop)
  }

  /// The kick's user event with `flags` and `event_flags`.
  fn kick_event(flags: UserFlags, event_flags: EventFlags) -> Event {
    Event::new(
      EventFilter::User {
        ident: isize::from(KICK_IDENT),
        flags,
        user_flags: UserDefinedFlags::new(0),
      },
      event_flags,
      std::ptr::null_mut(),
    )
  }

  /// Registers `kq`'s kick: a user event, cleared as it is delivered.
  pub(super) fn add_kick(kq: &OwnedFd) -> Result<(), Errno> {
    submit(
      kq,
      kick_event(UserFlags::empty(), EventFlags::ADD | EventFlags::CLEAR),
    )
  }

  /// Fires `kq`'s kick.
  pub(super) fn trigger_kick(kq: &OwnedFd) -> Result<(), Errno> {
    submit(kq, kick_event(UserFlags::TRIGGER, EventFlags::empty()))
  }

  /// Registers a one-shot filter for `interest` on the descriptor `raw`, its record carrying `user_data`.
  pub(super) fn add_interest(
    kq: &OwnedFd,
    interest: Interest,
    raw: i32,
    user_data: u64,
  ) -> Result<(), Errno> {
    let filter = match interest {
      Interest::Read => EventFilter::Read(raw),
      Interest::Write => EventFilter::Write(raw),
    };
    let udata = core::ptr::without_provenance_mut(usize::try_from(user_data).unwrap_or(usize::MAX));
    submit(
      kq,
      Event::new(filter, EventFlags::ADD | EventFlags::ONESHOT, udata),
    )
  }

  /// Waits at most `timeout` for records into `records`, handing what each filled one was to `fired`.
  pub(super) fn wait(
    kq: &OwnedFd,
    records: &mut Records,
    timeout: Option<Duration>,
    fired: &mut dyn FnMut(Fired),
  ) -> Result<(), Errno> {
    records.clear();
    // SAFETY: no changes; the output buffer is the vector's spare capacity, which the kernel may fill.
    unsafe { kevent(kq, &[], rustix::buffer::spare_capacity(records), timeout) }?;
    for record in records.iter() {
      fired(match record.filter() {
        EventFilter::User { .. } => Fired::Kick,
        _ => Fired::Ready(u64::try_from(record.udata().addr()).unwrap_or(0)),
      });
    }
    Ok(())
  }
}

/// The driver.
pub struct KqueueDriver {
  kq: KickFd,
  epoch: Instant,
  records: sys::Records,
  nops: Vec<u64>,
}

impl std::fmt::Debug for KqueueDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("KqueueDriver")
      .field("events_capacity", &EVENTS_PER_WAIT)
      .finish()
  }
}

/// Creates the queue and registers the kick event; the descriptor is owned by the registry slot
/// that the shard registers into (closed at unregistration, never leaked).
pub fn prepare() -> Result<OwnedFd, RtError> {
  let kq = rustix::event::kqueue::kqueue().map_err(|e| refused("kqueue", e))?;
  sys::add_kick(&kq).map_err(|e| refused("kevent(EV_ADD EVFILT_USER)", e))?;
  Ok(kq)
}

/// Triggers the kick event on `kq`; safe from any thread. A closed slot (the shard unregistered)
/// makes it a no-op.
pub fn trigger(kq: &KickFd) {
  let _ = kq.with(|kq| {
    // A queue being torn down with its shard refuses the trigger: the documented outcome of a kick to an exiting shard.
    let _ = sys::trigger_kick(kq);
  });
}

impl KqueueDriver {
  /// Builds the driver over the slot's prepared queue, on the shard's thread.
  pub fn from_prepared(kq: KickFd) -> KqueueDriver {
    KqueueDriver {
      kq,
      epoch: Instant::now(),
      records: sys::records(),
      nops: Vec::new(),
    }
  }

  /// Registers a one-shot `interest` in the descriptor `raw` for the waker word `user_data`; `call` names it in a
  /// refusal.
  fn register(
    &mut self,
    interest: Interest,
    raw: i32,
    user_data: u64,
    call: &'static str,
  ) -> Result<(), RtError> {
    self
      .kq
      .with(|kq| sys::add_interest(kq, interest, raw, user_data))
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
    let records = &mut self.records;
    let outcome = self
      .kq
      .with(|kq| {
        sys::wait(kq, records, timeout, &mut |fired| {
          // The kick carries nothing; a readiness becomes a completion keyed by its waker word.
          if let Fired::Ready(user_data) = fired {
            out.push(Completion {
              user_data,
              result: 0,
            });
          }
        })
      })
      .ok_or(RtError::DriverLost)?;
    match outcome {
      Ok(()) | Err(rustix::io::Errno::INTR) => Ok(()),
      Err(rustix::io::Errno::BADF) => Err(RtError::DriverLost),
      Err(other) => Err(refused("kevent", other)),
    }
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot read filter whose record carries the waker word; `wait` turns the ready record into a completion keyed
    // by that word (the same path the kick's siblings take).
    self.register(Interest::Read, raw, user_data, "kevent(EVFILT_READ)")
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot write filter whose record carries the waker word, exactly as the read filter does. The socket becomes
    // writable when its send buffer has space again (§4.6, TCP send backpressure to a stalled client).
    self.register(Interest::Write, raw, user_data, "kevent(EVFILT_WRITE)")
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}
