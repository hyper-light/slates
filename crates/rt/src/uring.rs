//! The io_uring driver (Linux): `SINGLE_ISSUER` and `DEFER_TASKRUN` when the kernel accepts
//! them, plain setup when it does not, and a refusal (seccomp, an old kernel) that selects epoll
//! [B: io_uring_setup(2); D-9]. Kicks are an eventfd watched by a multishot poll on the ring, so
//! any thread's write becomes a completion the waiting `io_uring_enter` returns for.

use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Instant;

use io_uring::types::{Fd, SubmitArgs, Timespec};
use io_uring::{IoUring, opcode};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since, refused};
use crate::error::RtError;

/// Format: the user word of the kick poll.
const KICK_TAG: u64 = u64::MAX;

/// The driver.
pub struct UringDriver {
  ring: IoUring,
  efd: &'static OwnedFd,
  epoch: Instant,
  notes: Vec<String>,
  multishot: bool,
}

impl std::fmt::Debug for UringDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("UringDriver")
      .field("efd", &self.efd.as_raw_fd())
      .field("notes", &self.notes)
      .field("multishot", &self.multishot)
      .finish()
  }
}

/// Creates the kick eventfd, leaked for the process (see the epoll driver for why).
pub fn prepare_eventfd() -> Result<&'static OwnedFd, RtError> {
  Ok(Box::leak(Box::new(
    rustix::event::eventfd(
      0,
      rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )
    .map_err(|e| refused("eventfd", e))?,
  )))
}

/// Probes whether io_uring is available, recording which flags the kernel accepts; a refusal
/// (seccomp, an old kernel) selects epoll.
pub fn probe(entries: u32, notes: &mut Vec<String>) -> bool {
  match build_ring(entries) {
    Ok((_, flags)) => {
      notes.push(format!("io_uring = {flags}"));
      true
    }
    Err(e) => {
      notes.push(format!("io_uring = unavailable({e}); using epoll"));
      false
    }
  }
}

fn build_ring(entries: u32) -> Result<(IoUring, &'static str), RtError> {
  let entries = entries.max(1).next_power_of_two();
  match IoUring::builder()
    .setup_single_issuer()
    .setup_defer_taskrun()
    .build(entries)
  {
    Ok(ring) => Ok((ring, "SINGLE_ISSUER | DEFER_TASKRUN")),
    Err(e) if e.raw_os_error() == Some(libc::EINVAL) => IoUring::builder()
      .build(entries)
      .map(|ring| {
        (
          ring,
          "plain (the kernel refused SINGLE_ISSUER | DEFER_TASKRUN)",
        )
      })
      .map_err(|e| RtError::DriverRefused {
        call: "io_uring_setup",
        code: e.raw_os_error(),
      }),
    Err(e) => Err(RtError::DriverRefused {
      call: "io_uring_setup",
      code: e.raw_os_error(),
    }),
  }
}

impl UringDriver {
  /// Builds the driver over a prepared eventfd, on the shard's thread.
  pub fn with_eventfd(efd: &'static OwnedFd, entries: u32) -> Result<UringDriver, RtError> {
    let (ring, flags) = build_ring(entries)?;
    let mut driver = UringDriver {
      ring,
      efd,
      epoch: Instant::now(),
      notes: vec![format!("io_uring = {flags}")],
      multishot: true,
    };
    driver.arm_kick()?;
    Ok(driver)
  }

  /// The setup notes for the profile.
  pub fn notes(&self) -> &[String] {
    &self.notes
  }

  fn arm_kick(&mut self) -> Result<(), RtError> {
    let poll = opcode::PollAdd::new(
      Fd(self.efd.as_raw_fd()),
      u32::try_from(libc::POLLIN).unwrap_or(0),
    )
    .multi(self.multishot)
    .build()
    .user_data(KICK_TAG);
    // SAFETY: the eventfd outlives the ring (it is leaked, see Drop) and the entry is valid.
    unsafe { self.ring.submission().push(&poll) }.map_err(|_| RtError::DriverRefused {
      call: "sq push(poll)",
      code: None,
    })?;
    self.ring.submit().map_err(|e| RtError::DriverRefused {
      call: "io_uring_enter(submit)",
      code: e.raw_os_error(),
    })?;
    Ok(())
  }

  fn drain_kick(&self) {
    let mut word = [0u8; size_of::<u64>()];
    let _ = rustix::io::read(self.efd, &mut word);
  }
}

impl Driver for UringDriver {
  fn kind(&self) -> DriverKind {
    DriverKind::IoUring
  }

  fn kick_handle(&self) -> Kick {
    Kick::Eventfd(self.efd)
  }

  fn now_ns(&self) -> u64 {
    nanos_since(self.epoch)
  }

  fn wait(&mut self, timeout_ns: Option<u64>, out: &mut Vec<Completion>) -> Result<(), RtError> {
    let outcome = match timeout_ns {
      Some(ns) => {
        let ts = Timespec::new()
          .nsec(u32::try_from(ns % NANOS_PER_SECOND).unwrap_or(0))
          .sec(ns / NANOS_PER_SECOND);
        let args = SubmitArgs::new().timespec(&ts);
        self.ring.submitter().submit_with_args(1, &args)
      }
      None => self.ring.submitter().submit_and_wait(1),
    };
    if let Err(e) = outcome {
      return match e.raw_os_error() {
        Some(libc::ETIME) | Some(libc::EINTR) => Ok(()),
        Some(libc::EBADF) => Err(RtError::DriverLost),
        code => Err(RtError::DriverRefused {
          call: "io_uring_enter",
          code,
        }),
      };
    }
    let mut kick_seen = false;
    let mut rearm = false;
    for cqe in self.ring.completion() {
      if cqe.user_data() == KICK_TAG {
        kick_seen = true;
        if !io_uring::cqueue::more(cqe.flags()) {
          rearm = true;
        }
      } else {
        out.push(Completion {
          user_data: cqe.user_data(),
          result: cqe.result(),
        });
      }
    }
    if kick_seen {
      self.drain_kick();
    }
    if rearm {
      self.multishot = false;
      self.arm_kick()?;
    }
    Ok(())
  }

  fn has_pending(&self) -> bool {
    // The completion queue's readiness is visible through the shared ring memory; the io-uring
    // crate exposes it through a mutable accessor, so the answer is conservative here and the
    // `wait` with a zero timeout stays the precise check.
    false
  }

  fn submit_nop(&mut self, user_data: u64) -> Result<(), RtError> {
    let nop = opcode::Nop::new().build().user_data(user_data);
    // SAFETY: a no-op entry has no buffers.
    unsafe { self.ring.submission().push(&nop) }.map_err(|_| RtError::DriverRefused {
      call: "sq push(nop)",
      code: None,
    })?;
    self.ring.submit().map_err(|e| RtError::DriverRefused {
      call: "io_uring_enter(submit)",
      code: e.raw_os_error(),
    })?;
    Ok(())
  }

  fn register_readable(&mut self, _raw: i32, _user_data: u64) -> Result<(), RtError> {
    // Owed (§4.10a): this completion-native driver does not carry socket readiness yet; a
    // typed refusal, never a silent drop. The readiness-native drivers (kqueue, epoll) do.
    Err(RtError::DriverRefused {
      call: "register_readable",
      code: None,
    })
  }

  fn register_writable(&mut self, _raw: i32, _user_data: u64) -> Result<(), RtError> {
    // Owed (§4.6): this completion-native driver does not carry socket write-readiness yet; a
    // typed refusal, never a silent drop. The readiness-native drivers (kqueue, epoll) do.
    Err(RtError::DriverRefused {
      call: "register_writable",
      code: None,
    })
  }
}

/// Format: nanoseconds per second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
