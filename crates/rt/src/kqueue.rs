//! The kqueue driver (macOS): `EVFILT_USER` for kicks, `kevent64` with a timeout for the wait [B: kqueue(2), XNU's
//! `kevent64`], sockets driven by readiness through the same `wait`, since macOS has no completion I/O.
//!
//! Every call on the queue is `kevent64`, through one helper ([`kevent64_on`]): XNU refuses mixing `kevent`'s record
//! format with `kevent64`'s on one queue (`EINVAL`, A-218), and only `kevent64` takes `KEVENT_FLAG_IMMEDIATE`, the poll
//! that returns at once. A zero-timeout `kevent` sleeps 14.6-15.5 µs with an involuntary switch on Apple silicon where
//! the immediate form takes 180-186 ns (A-218's probe), and a shard harvests its driver between busy steps, so every
//! step after the first on a busy macOS shard paid that sleep: two of them, about 30 µs, in each live write's exchange
//! (g228, 85a: `fleet.live.hop.exchange.{spawn,wake}` 15.4 µs each at the median). [`Driver::poll_now`] is the immediate
//! form; whether a harvest may use it (a core free for it) is the shard's decision, not the driver's.
//!
//! The queue is created and its kick event registered by [`prepare`] on whatever thread builds the shard's seed (the
//! registry owns the descriptor and pins each foreign borrow); the driver itself, which holds an event buffer of raw
//! kernel records, is built on the shard's own thread by [`KqueueDriver::from_prepared`].

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crate::driver::KickFd;
use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: u64 = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

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
      .field("events_capacity", &self.events.capacity())
      .finish()
  }
}

/// One `kevent64` record: `filter` on `ident` with `flags` and `fflags`, carrying `udata`.
fn record(ident: u64, filter: i16, flags: u16, fflags: u32, udata: u64) -> libc::kevent64_s {
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

/// How long a [`kevent64_on`] call may wait for an event.
#[derive(Clone, Copy, Debug)]
enum Patience {
  /// Until an event, without bound.
  Unbounded,
  /// At most this long; zero is a poll, which XNU serves by sleeping (A-218).
  Within(Duration),
  /// Not at all: `KEVENT_FLAG_IMMEDIATE`, a poll that returns at once.
  Immediate,
}

/// Applies `changes` to the queue `kq` and gathers ready events into `out`'s spare capacity, waiting as `patience`
/// says; the events gathered are appended to `out`. The one `kevent64` call of the driver.
fn kevent64_on(
  kq: &OwnedFd,
  changes: &[libc::kevent64_s],
  out: &mut Vec<libc::kevent64_s>,
  patience: Patience,
) -> Result<(), rustix::io::Errno> {
  let nchanges = libc::c_int::try_from(changes.len()).map_err(|_| rustix::io::Errno::INVAL)?;
  let spare = out.capacity().saturating_sub(out.len());
  let nevents = libc::c_int::try_from(spare).map_err(|_| rustix::io::Errno::INVAL)?;
  let (flags, timeout) = match patience {
    Patience::Unbounded => (0, None),
    Patience::Within(within) => (
      0,
      Some(libc::timespec {
        tv_sec: libc::time_t::try_from(within.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(within.subsec_nanos()),
      }),
    ),
    Patience::Immediate => (libc::KEVENT_FLAG_IMMEDIATE, None),
  };
  let timeout_ptr = timeout
    .as_ref()
    .map_or(std::ptr::null(), |timeout| std::ptr::from_ref(timeout));
  let start = out.len();
  // SAFETY: `changes` is `nchanges` valid records; the event list is `out`'s spare capacity, `nevents` records from
  // its end, which the kernel fills with at most `nevents` and whose count it returns; `timeout_ptr` is null or points
  // at a `timespec` that lives across the call; `kq` is an open queue for the call's duration.
  let gathered = unsafe {
    libc::kevent64(
      kq.as_raw_fd(),
      changes.as_ptr(),
      nchanges,
      out.as_mut_ptr().add(start),
      nevents,
      flags,
      timeout_ptr,
    )
  };
  let gathered =
    usize::try_from(gathered).map_err(|_| rustix::io::Errno::from_raw_os_error(errno()))?;
  // SAFETY: the kernel initialized `gathered` records (at most `nevents`, the spare capacity) after `start`.
  unsafe { out.set_len(start.saturating_add(gathered.min(spare))) };
  Ok(())
}

/// The calling thread's last OS error number.
fn errno() -> i32 {
  std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Creates the queue and registers the kick event; the descriptor is owned by the registry slot
/// that the shard registers into (closed at unregistration, never leaked).
pub fn prepare() -> Result<OwnedFd, RtError> {
  let kq = rustix::event::kqueue::kqueue().map_err(|e| refused("kqueue", e))?;
  let register = record(
    KICK_IDENT,
    libc::EVFILT_USER,
    libc::EV_ADD | libc::EV_CLEAR,
    0,
    0,
  );
  kevent64_on(&kq, &[register], &mut Vec::new(), Patience::Unbounded)
    .map_err(|e| refused("kevent64(EV_ADD EVFILT_USER)", e))?;
  Ok(kq)
}

/// Triggers the kick event on `kq`; safe from any thread. A closed slot (the shard unregistered)
/// makes it a no-op.
pub fn trigger(kq: &KickFd) {
  let _ = kq.with(|kq| {
    let trigger = record(KICK_IDENT, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER, 0);
    // A closed queue returns an error we ignore.
    let _ = kevent64_on(kq, &[trigger], &mut Vec::new(), Patience::Unbounded);
  });
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

  /// Gathers what is ready, waiting as `patience` says, and turns it into completions on `out`.
  fn gather(&mut self, patience: Patience, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let mut patience = patience;
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      patience = Patience::Immediate;
    }
    self.events.clear();
    let events = &mut self.events;
    let outcome = self
      .kq
      .with(|kq| kevent64_on(kq, &[], events, patience))
      .ok_or(RtError::DriverLost)?;
    if let Err(e) = outcome {
      return match e {
        rustix::io::Errno::INTR => Ok(()),
        rustix::io::Errno::BADF => Err(RtError::DriverLost),
        other => Err(refused("kevent64", other)),
      };
    }
    // Kick events carry nothing; other filters become completions keyed by udata.
    for ev in &self.events {
      if ev.filter != libc::EVFILT_USER {
        out.push(Completion {
          user_data: ev.udata,
          result: 0,
        });
      }
    }
    Ok(())
  }

  /// Registers one-shot interest of `filter` in `raw`, its udata the waker word `user_data`.
  fn register(
    &mut self,
    raw: i32,
    filter: i16,
    user_data: u64,
    what: &'static str,
  ) -> Result<(), RtError> {
    let ident = u64::try_from(raw).map_err(|_| refused(what, rustix::io::Errno::BADF))?;
    let event = record(ident, filter, libc::EV_ADD | libc::EV_ONESHOT, 0, user_data);
    self
      .kq
      .with(|kq| kevent64_on(kq, &[event], &mut Vec::new(), Patience::Unbounded))
      .ok_or(RtError::DriverLost)?
      .map_err(|e| refused(what, e))
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
    let patience = timeout_ns.map_or(Patience::Unbounded, |ns| {
      Patience::Within(Duration::from_nanos(ns))
    });
    self.gather(patience, out)
  }

  fn poll_now(&mut self, out: &mut Vec<Completion>) -> Result<(), RtError> {
    self.gather(Patience::Immediate, out)
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // A one-shot read filter whose udata carries the waker word; `gather` turns the ready event into a completion keyed
    // by that word (the same path the kick's siblings take).
    self.register(raw, libc::EVFILT_READ, user_data, "kevent64(EVFILT_READ)")
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // The socket becomes writable when its send buffer has space again (§4.6, TCP send backpressure to a stalled
    // client), turned into a completion exactly as the read filter is.
    self.register(raw, libc::EVFILT_WRITE, user_data, "kevent64(EVFILT_WRITE)")
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}
