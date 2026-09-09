//! The epoll driver (Linux fallback when io_uring is refused, as in containers whose seccomp
//! profile blocks it): an eventfd for kicks registered on an epoll instance, `epoll_wait` with a
//! timeout for the wait [B: epoll(7); B: eventfd(2)], through rustix's safe wrappers. The one unsafe
//! idiom is borrowing a caller-owned socket fd by number for a single `epoll_ctl` readiness
//! registration (a `UdpSocket` recv, a `TcpStream` read or write), each with a `// SAFETY:` note.

use std::os::fd::OwnedFd;
use std::time::Instant;

use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the user word that marks the kick eventfd in epoll events.
const KICK_TAG: u64 = u64::MAX;

/// Shape: events drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct EpollDriver {
  epfd: OwnedFd,
  efd: &'static OwnedFd,
  epoch: Instant,
  events: Vec<epoll::Event>,
  nops: Vec<u64>,
}

impl std::fmt::Debug for EpollDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("EpollDriver")
      .field("events_capacity", &self.events.capacity())
      .finish()
  }
}

impl EpollDriver {
  /// Creates the instance over a prepared kick eventfd (leaked for the process so the registry's
  /// kick handle can name it after the driver is gone; a write into a reused descriptor number
  /// would otherwise be a fault in someone else's file).
  pub fn with_eventfd(efd: &'static OwnedFd) -> Result<EpollDriver, RtError> {
    let epfd = epoll::create(CreateFlags::CLOEXEC).map_err(|e| refused("epoll_create1", e))?;
    epoll::add(&epfd, efd, EventData::new_u64(KICK_TAG), EventFlags::IN)
      .map_err(|e| refused("epoll_ctl(ADD eventfd)", e))?;
    Ok(EpollDriver {
      epfd,
      efd,
      epoch: Instant::now(),
      events: Vec::with_capacity(EVENTS_PER_WAIT),
      nops: Vec::new(),
    })
  }

  fn drain_kick(&self) {
    let mut word = [0u8; size_of::<u64>()];
    let _ = rustix::io::read(self.efd, &mut word);
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
    let mut timeout = timeout_ns.map(timespec);
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout = Some(timespec(0));
    }
    self.events.clear();
    let outcome = epoll::wait(
      &self.epfd,
      rustix::buffer::spare_capacity(&mut self.events),
      timeout.as_ref(),
    );
    if let Err(e) = outcome {
      return match e {
        rustix::io::Errno::INTR => Ok(()),
        rustix::io::Errno::BADF => Err(RtError::DriverLost),
        other => Err(refused("epoll_wait", other)),
      };
    }
    let mut kicked = false;
    for ev in &self.events {
      // The event record is packed: copy the fields out before touching them.
      let (data, flags) = (ev.data, ev.flags);
      if data.u64() == KICK_TAG {
        kicked = true;
      } else {
        out.push(Completion {
          user_data: data.u64(),
          result: i32::try_from(flags.bits()).unwrap_or(0),
        });
      }
    }
    if kicked {
      self.drain_kick();
    }
    Ok(())
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // SAFETY: `raw` is a live socket the caller (a UdpSocket) owns for the registration; the borrow
    // is used only for this epoll_ctl call and not retained.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    // One-shot readable interest whose u64 data carries the waker word; the `wait` loop above turns
    // the ready event into a completion keyed by that word.
    epoll::add(
      &self.epfd,
      fd,
      EventData::new_u64(user_data),
      EventFlags::IN | EventFlags::ONESHOT,
    )
    .map_err(|e| refused("epoll_ctl(ADD readable)", e))?;
    Ok(())
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // SAFETY: `raw` is a live socket the caller (a TcpStream) owns for the registration; the borrow
    // is used only for this epoll_ctl call and not retained.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    // One-shot writable interest whose u64 data carries the waker word; the `wait` loop turns the
    // ready event into a completion keyed by that word (§4.6, TCP send backpressure to a stalled
    // client).
    epoll::add(
      &self.epfd,
      fd,
      EventData::new_u64(user_data),
      EventFlags::OUT | EventFlags::ONESHOT,
    )
    .map_err(|e| refused("epoll_ctl(ADD writable)", e))?;
    Ok(())
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}

fn timespec(ns: u64) -> rustix::event::Timespec {
  /// Format: nanoseconds per second.
  const NANOS_PER_SECOND: u64 = 1_000_000_000;
  rustix::event::Timespec {
    tv_sec: i64::try_from(ns / NANOS_PER_SECOND).unwrap_or(i64::MAX),
    tv_nsec: i64::try_from(ns % NANOS_PER_SECOND).unwrap_or(0),
  }
}
