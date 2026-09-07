//! The kqueue driver (macOS, BSD): `EVFILT_USER` for kicks, `kevent` with a timeout for the wait
//! [B: kqueue(2)], through rustix's wrappers. macOS has no completion I/O, so later phases drive
//! sockets by readiness through the same `wait`.
//!
//! The queue is created and its kick event registered by [`prepare`] on whatever thread builds
//! the shard's seed (the descriptor is leaked so the registry's kick can name it for the process);
//! the driver itself, which holds an event buffer of raw kernel records, is built on the shard's
//! own thread by [`KqueueDriver::from_prepared`]. rustix marks `kevent` unsafe because the
//! output buffer is filled by the kernel; the three calls here pass a change list of valid
//! events and a buffer with the capacity the call may fill.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::event::kqueue::{
  Event, EventFilter, EventFlags, UserDefinedFlags, UserFlags, kevent, kqueue,
};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the identifier of the kick event on the queue.
const KICK_IDENT: isize = 0;

/// Shape: events drained per wait; a wait that fills the buffer returns and the next wait drains
/// the rest (kqueue keeps them), so the size bounds latency, not correctness.
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct KqueueDriver {
  kq: &'static OwnedFd,
  epoch: Instant,
  events: Vec<Event>,
  nops: Vec<u64>,
}

impl std::fmt::Debug for KqueueDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("KqueueDriver")
      .field("events_capacity", &self.events.capacity())
      .finish()
  }
}

fn user_event(flags: UserFlags, event_flags: EventFlags) -> Event {
  Event::new(
    EventFilter::User {
      ident: KICK_IDENT,
      flags,
      user_flags: UserDefinedFlags::new(0),
    },
    event_flags,
    std::ptr::null_mut(),
  )
}

/// Creates the queue and registers the kick event; the descriptor is leaked for the process.
pub fn prepare() -> Result<&'static OwnedFd, RtError> {
  let kq: &'static OwnedFd = Box::leak(Box::new(kqueue().map_err(|e| refused("kqueue", e))?));
  let register = user_event(UserFlags::empty(), EventFlags::ADD | EventFlags::CLEAR);
  let mut none: Vec<Event> = Vec::new();
  // SAFETY: the change list is one valid event; the empty output vector receives nothing.
  unsafe { kevent(kq, &[register], &mut none, None) }
    .map_err(|e| refused("kevent(EV_ADD EVFILT_USER)", e))?;
  Ok(kq)
}

/// Triggers the kick event on `kq`; safe from any thread.
pub fn trigger(kq: &OwnedFd) {
  let trigger = user_event(UserFlags::TRIGGER, EventFlags::empty());
  let mut none: Vec<Event> = Vec::new();
  // SAFETY: one valid change record on an open queue; a closed queue returns an error we ignore.
  let _ = unsafe { kevent(kq, &[trigger], &mut none, None) };
}

impl KqueueDriver {
  /// Builds the driver over a prepared queue, on the shard's thread.
  pub fn from_prepared(kq: &'static OwnedFd) -> KqueueDriver {
    KqueueDriver {
      kq,
      epoch: Instant::now(),
      events: Vec::with_capacity(EVENTS_PER_WAIT),
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
    let mut timeout = timeout_ns.map(Duration::from_nanos);
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout = Some(Duration::ZERO);
    }
    self.events.clear();
    // SAFETY: no changes; the output buffer is the vector's spare capacity, which the call fills
    // and marks initialized up to the count it returns.
    let outcome = unsafe {
      kevent(
        self.kq,
        &[],
        rustix::buffer::spare_capacity(&mut self.events),
        timeout,
      )
    };
    if let Err(e) = outcome {
      return match e {
        rustix::io::Errno::INTR => Ok(()),
        rustix::io::Errno::BADF => Err(RtError::DriverLost),
        other => Err(refused("kevent", other)),
      };
    }
    // Kick events carry nothing; other filters (later phases) become completions keyed by udata.
    for ev in &self.events {
      if !matches!(ev.filter(), EventFilter::User { .. }) {
        out.push(Completion {
          user_data: u64::try_from(ev.udata().addr()).unwrap_or(0),
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
    let event = Event::new(
      EventFilter::Read(raw),
      EventFlags::ADD | EventFlags::ONESHOT,
      core::ptr::without_provenance_mut(usize::try_from(user_data).unwrap_or(usize::MAX)),
    );
    let mut none: Vec<Event> = Vec::new();
    // SAFETY: one valid change record on the open queue; the empty output buffer receives nothing.
    unsafe { kevent(self.kq, &[event], &mut none, None) }
      .map_err(|e| refused("kevent(EVFILT_READ)", e))?;
    Ok(())
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}
