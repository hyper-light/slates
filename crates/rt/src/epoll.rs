//! The epoll driver: Linux's driver (D-9, amended 2026-10-18 — io_uring was removed after its
//! measured win proved confined to the parked path while it cost the kernel attack surface and a
//! boot-time `ENOMEM` under `RLIMIT_MEMLOCK`; see `BENCHMARKS.md`).
//! timeout for the wait [B: epoll(7); B: eventfd(2)], through rustix's safe wrappers. A timed wait's deadline is a
//! timerfd in the interest list, armed with the deadline's nanoseconds, and `epoll_wait` itself waits untimed (84a,
//! 2026-11-20): its timeout is whole milliseconds, so a shorter wait lasted a millisecond. The one unsafe
//! idiom is borrowing a caller-owned socket fd by number for a single `epoll_ctl` readiness
//! registration (a `UdpSocket` recv, a `TcpStream` read or write), each with a `// SAFETY:` note.

use std::os::fd::OwnedFd;
use std::time::Instant;

use rustix::event::epoll::{self, CreateFlags, EventData, EventFlags};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Creates the kick eventfd both Linux drivers wake on; the registry owns it until every shard driver has
/// retired.
pub fn prepare_eventfd() -> Result<OwnedFd, RtError> {
  rustix::event::eventfd(
    0,
    rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
  )
  .map_err(|e| refused("eventfd", e))
}

/// Format: the user word that marks the kick eventfd in epoll events.
const KICK_TAG: u64 = u64::MAX;

/// Format: the user word that marks the deadline timerfd in epoll events (a word no waker uses, as [`KICK_TAG`]).
const DEADLINE_TAG: u64 = u64::MAX - 1;

/// Shape: events drained per wait (see the kqueue driver for the reasoning).
const EVENTS_PER_WAIT: usize = 64;

/// The driver.
pub struct EpollDriver {
  epfd: OwnedFd,
  efd: crate::driver::KickFd,
  /// The timed wait's deadline, a timerfd in the interest list (84a): `epoll_wait` takes whole milliseconds and rustix
  /// rounds a shorter timeout up, so a wait for 200 µs lasted a millisecond; the timerfd keeps the nanoseconds, and a
  /// timed wait is an untimed `epoll_wait` this timer ends. `epoll_pwait2` would too, but it needs Linux 5.11 and the
  /// design's Linux floor is 5.10; the timerfd is 2.6.25's.
  deadline: OwnedFd,
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
  /// Creates the instance over its registry-owned eventfd. The owning runtime retires the
  /// descriptor after this driver and all foreign kick borrows have ended.
  pub fn with_eventfd(efd: crate::driver::KickFd) -> Result<EpollDriver, RtError> {
    let epfd = epoll::create(CreateFlags::CLOEXEC).map_err(|e| refused("epoll_create1", e))?;
    efd
      .with(|fd| epoll::add(&epfd, fd, EventData::new_u64(KICK_TAG), EventFlags::IN))
      .ok_or(RtError::DriverLost)?
      .map_err(|e| refused("epoll_ctl(ADD eventfd)", e))?;
    let deadline = rustix::time::timerfd_create(
      rustix::time::TimerfdClockId::Monotonic,
      rustix::time::TimerfdFlags::CLOEXEC | rustix::time::TimerfdFlags::NONBLOCK,
    )
    .map_err(|e| refused("timerfd_create", e))?;
    epoll::add(
      &epfd,
      &deadline,
      EventData::new_u64(DEADLINE_TAG),
      EventFlags::IN,
    )
    .map_err(|e| refused("epoll_ctl(ADD timerfd)", e))?;
    Ok(EpollDriver {
      epfd,
      efd,
      deadline,
      epoch: Instant::now(),
      events: Vec::with_capacity(EVENTS_PER_WAIT),
      nops: Vec::new(),
    })
  }

  fn drain_kick(&self) {
    let mut word = [0u8; size_of::<u64>()];
    let _ = self.efd.with(|efd| rustix::io::read(efd, &mut word));
  }

  /// Sets the deadline timer to fire `ns` from now, or disarms it for `None`: what a timed wait waits on in place of
  /// `epoll_wait`'s millisecond timeout.
  fn set_deadline(&self, ns: Option<u64>) -> Result<(), RtError> {
    let zero = timespec(0);
    let value = ns.map_or(zero, timespec);
    rustix::time::timerfd_settime(
      &self.deadline,
      rustix::time::TimerfdTimerFlags::empty(),
      &rustix::time::Itimerspec {
        it_interval: zero,
        it_value: value,
      },
    )
    .map(|_| ())
    .map_err(|e| refused("timerfd_settime", e))
  }

  /// Takes a fired deadline's count, so its readiness does not end the next wait.
  fn drain_deadline(&self) {
    let mut count = [0u8; size_of::<u64>()];
    let _ = rustix::io::read(&self.deadline, &mut count);
  }

  /// Turns the events a wait returned into completions: the kick and the deadline drained, every other event a
  /// completion keyed by its waker word.
  fn harvest(&self, out: &mut Vec<Completion>) {
    let mut kicked = false;
    for ev in &self.events {
      // The event record is packed: copy the fields out before touching them.
      let (data, flags) = (ev.data, ev.flags);
      if data.u64() == KICK_TAG {
        kicked = true;
      } else if data.u64() == DEADLINE_TAG {
        self.drain_deadline();
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
    let mut timeout_ns = timeout_ns;
    if !self.nops.is_empty() {
      out.extend(self.nops.drain(..).map(|user_data| Completion {
        user_data,
        result: 0,
      }));
      timeout_ns = Some(0);
    }
    // A zero timeout polls; a positive one waits untimed for the deadline timer, which keeps its nanoseconds (84a).
    let timed = matches!(timeout_ns, Some(ns) if ns > 0);
    let timeout = match timeout_ns {
      Some(0) => Some(timespec(0)),
      _ => None,
    };
    if timed {
      self.set_deadline(timeout_ns)?;
    }
    self.events.clear();
    let outcome = epoll::wait(
      &self.epfd,
      rustix::buffer::spare_capacity(&mut self.events),
      timeout.as_ref(),
    );
    if timed {
      // Disarmed whatever ended the wait, so a deadline left over never ends a later one.
      self.set_deadline(None)?;
    }
    match outcome {
      Ok(_) => {
        self.harvest(out);
        // Data-derived harvest batching (R3): grow the event buffer on demand when a wait fills it so bursts of
        // concurrent ready sockets drain in O(1) syscalls instead of 64-event slices.
        if self.events.len() == self.events.capacity() {
          let current = self.events.capacity();
          self.events.reserve(current);
        }
        Ok(())
      }
      Err(rustix::io::Errno::INTR) => Ok(()),
      Err(rustix::io::Errno::BADF) => Err(RtError::DriverLost),
      Err(other) => Err(refused("epoll_wait", other)),
    }
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    self.nops.push(user_data);
    Ok(())
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // One-shot readable interest whose u64 data carries the waker word; the `wait` loop above turns
    // the ready event into a completion keyed by that word.
    self.arm(
      raw,
      user_data,
      EventFlags::IN | EventFlags::ONESHOT,
      ("epoll_ctl(ADD readable)", "epoll_ctl(MOD readable)"),
    )
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    // One-shot writable interest whose u64 data carries the waker word; the `wait` loop turns the
    // ready event into a completion keyed by that word (§4.6, TCP send backpressure to a stalled
    // client).
    self.arm(
      raw,
      user_data,
      EventFlags::OUT | EventFlags::ONESHOT,
      ("epoll_ctl(ADD writable)", "epoll_ctl(MOD writable)"),
    )
  }

  fn has_pending(&self) -> bool {
    !self.nops.is_empty()
  }
}

impl EpollDriver {
  /// Arms one-shot `flags` interest on `raw` under the waker word `user_data`: `EPOLL_CTL_ADD` for a
  /// descriptor this epoll instance has not seen, and `EPOLL_CTL_MOD` for one it has — a one-shot
  /// registration is *disabled* after it fires, not removed, so the descriptor stays in the interest
  /// list and a second `ADD` is refused `EEXIST` (epoll(7): "EPOLLONESHOT … the user must call
  /// epoll_ctl with EPOLL_CTL_MOD to rearm"). Every await re-arms, so a receive loop's second await
  /// — the fleet's serve sockets after their first datagram — is the `MOD`
  /// (docs/bugs/2026-09-14-epoll-readiness-re-add-eexist.md).
  fn arm(
    &self,
    raw: i32,
    user_data: u64,
    flags: EventFlags,
    calls: (&'static str, &'static str),
  ) -> Result<(), RtError> {
    let (add_call, modify_call) = calls;
    // SAFETY: `raw` is a live socket the caller (a UdpSocket or TcpStream) owns for the registration;
    // the borrow is used only for these epoll_ctl calls and not retained.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) };
    let data = EventData::new_u64(user_data);
    match epoll::add(&self.epfd, fd, data, flags) {
      Ok(()) => Ok(()),
      Err(rustix::io::Errno::EXIST) => {
        epoll::modify(&self.epfd, fd, data, flags).map_err(|e| refused(modify_call, e))
      }
      Err(e) => Err(refused(add_call, e)),
    }
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
