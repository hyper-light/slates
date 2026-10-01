//! The io_uring driver (Linux): `SINGLE_ISSUER` and `DEFER_TASKRUN` when the kernel accepts
//! them, plain setup when it does not, and a refusal (seccomp, an old kernel) that selects epoll
//! [B: io_uring_setup(2); D-9]. Kicks are an eventfd watched by a multishot poll on the ring, so
//! any thread's write becomes a completion the waiting `io_uring_enter` returns for.
//! Retirement cancels requests and waits for a drain completion before closing the ring (§4.3):
//! Linux otherwise releases pending polls' socket references asynchronously, after shutdown returns
//! (`docs/bugs/2026-09-19-io-uring-retains-listener-after-shutdown.md`).

use std::time::Instant;

use io_uring::types::{CancelBuilder, Fd, SubmitArgs, Timespec};
use io_uring::{IoUring, opcode, squeue};

use crate::driver::{Completion, Driver, DriverKind, Kick, nanos_since};
use crate::error::RtError;

/// Format: the user word of the kick poll.
const KICK_TAG: u64 = u64::MAX;
/// Format: the retirement barrier's word, adjacent to the kick's reserved word and outside
/// the registry's shard-id range. No live task receives a completion after retirement begins.
const RETIRE_TAG: u64 = KICK_TAG - 1;

/// Format: the poll mask for read readiness — a socket with data, or a listener with a connection to
/// accept (`POLLIN`, io_uring `PollAdd`).
const POLL_READABLE: u32 = libc::POLLIN as u32;
/// Format: the poll mask for write readiness — a socket whose send buffer has space, or a connect
/// that has completed (`POLLOUT`).
const POLL_WRITABLE: u32 = libc::POLLOUT as u32;

/// Format: `IORING_ENTER_GETEVENTS`, the `io_uring_enter` flag that reaps completions (and, under
/// `DEFER_TASKRUN`, runs the ring's deferred task work) — `1U << 0` in the kernel's
/// `include/uapi/linux/io_uring.h` [B: io_uring_enter(2)]. The io-uring crate keeps its copy private
/// and libc carries none.
const ENTER_GETEVENTS: u32 = 1 << 0;

/// The driver.
pub struct UringDriver {
  ring: IoUring,
  efd: crate::driver::KickFd,
  epoch: Instant,
  notes: Vec<String>,
  multishot: bool,
}

impl std::fmt::Debug for UringDriver {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("UringDriver")
      .field("efd", &self.efd.raw())
      .field("notes", &self.notes)
      .field("multishot", &self.multishot)
      .finish()
  }
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
  let (ring, flags) = match IoUring::builder()
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
  }?;
  // A usable driver must be able to release a pending poll before shutdown returns. Probe the
  // cancellation operation on the empty ring too; unsupported kernels or seccomp policies
  // refuse this driver before it owns any socket, through the existing OS-driver selection.
  cancel_pending(&ring)?;
  Ok((ring, flags))
}

/// Cancels submitted requests. An empty ring has nothing to cancel; other refusals retain
/// their syscall and errno. Only poll and no-op requests are issued by this driver.
fn cancel_pending(ring: &IoUring) -> Result<(), RtError> {
  match ring
    .submitter()
    .register_sync_cancel(None, CancelBuilder::any())
  {
    Ok(()) => Ok(()),
    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(()),
    Err(error) => Err(RtError::DriverRefused {
      call: "io_uring_register(sync cancel)",
      code: error.raw_os_error(),
    }),
  }
}

impl UringDriver {
  /// Builds the driver over a prepared eventfd, on the shard's thread.
  pub fn with_eventfd(efd: crate::driver::KickFd, entries: u32) -> Result<UringDriver, RtError> {
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

  /// Arms a **one-shot** poll for `events` on `raw`, tagged with the waking task's `user_data`
  /// (its waker word): when the fd becomes ready, the completion is delivered by [`Self::wait`] as a
  /// `Completion { user_data }`, which the shard wakes the task by, so it re-polls its readiness
  /// future and retries the non-blocking syscall (`readable`/`writable`, [`crate::readiness`]).
  ///
  /// One-shot, not multishot: the readiness future arms afresh on every await, and io_uring removes
  /// a one-shot poll when it fires, so each await is an independent submission — no interest-list
  /// dedup as epoll needs (which had to track ADD vs MOD, the `EEXIST` bug of 2026-09-14). This
  /// closes the io_uring counterpart: `register_readable`/`register_writable` used to refuse, so an
  /// async socket on this driver — the NFS-mount server's per-connection reads and writes (§4.6),
  /// and any TCP on the runtime — died the first time it had to await readiness. Docker's default
  /// seccomp blocks io_uring so CI fell back to epoll and never exercised this; a bare-metal or VM
  /// Linux host with io_uring did (`docs/bugs/2026-09-16-io-uring-driver-carries-no-socket-readiness.md`).
  fn arm_poll(&mut self, raw: i32, events: u32, user_data: u64) -> Result<(), RtError> {
    let poll = opcode::PollAdd::new(Fd(raw), events)
      .build()
      .user_data(user_data);
    self.submit_entry(poll)
  }

  fn arm_kick(&mut self) -> Result<(), RtError> {
    let poll = opcode::PollAdd::new(
      Fd(self.efd.raw().unwrap_or(-1)),
      u32::try_from(libc::POLLIN).unwrap_or(0),
    )
    .multi(self.multishot)
    .build()
    .user_data(KICK_TAG);
    self.submit_entry(poll)
  }

  /// Submits only the buffer-free poll and no-op entries built in this module. Polls acquire
  /// their own kernel file reference; retirement cancels and completes them before returning.
  fn submit_entry(&mut self, entry: squeue::Entry) -> Result<(), RtError> {
    // SAFETY: every caller supplies a poll or no-op with no borrowed userspace buffer. The
    // kernel acquires a poll's file reference during submission and releases it at completion.
    unsafe { self.ring.submission().push(&entry) }.map_err(|_| RtError::DriverRefused {
      call: "sq push",
      code: None,
    })?;
    self.ring.submit().map_err(|e| RtError::DriverRefused {
      call: "io_uring_enter(submit)",
      code: e.raw_os_error(),
    })?;
    Ok(())
  }

  /// The ring's close queues asynchronous cleanup in Linux. Cancel first, then wait for an
  /// IO_DRAIN no-op: its completion follows every earlier request, including deferred poll
  /// cancellations. The finite set is closed to new submissions while this driver is dropped.
  fn retire(&mut self) -> Result<(), RtError> {
    // A refused earlier submit can leave an entry in the SQ. Cancellation only covers
    // submitted requests: publish that finite remainder before cancelling, or the drain
    // could submit a fresh poll after cancellation and then wait for it forever.
    while !self.ring.submission().is_empty() {
      let submitted = self.ring.submit().map_err(|error| RtError::DriverRefused {
        call: "io_uring_enter(retire submit)",
        code: error.raw_os_error(),
      })?;
      if submitted == 0 {
        return Err(RtError::DriverRefused {
          call: "io_uring_enter(retire made no submission progress)",
          code: None,
        });
      }
    }
    cancel_pending(&self.ring)?;
    self.submit_entry(
      opcode::Nop::new()
        .build()
        .flags(squeue::Flags::IO_DRAIN)
        .user_data(RETIRE_TAG),
    )?;
    loop {
      self
        .ring
        .submit_and_wait(1)
        .map_err(|error| RtError::DriverRefused {
          call: "io_uring_enter(retire)",
          code: error.raw_os_error(),
        })?;
      for completion in self.ring.completion() {
        if completion.user_data() == RETIRE_TAG {
          return if completion.result() >= 0 {
            Ok(())
          } else {
            Err(RtError::DriverRefused {
              call: "io_uring drain",
              code: completion.result().checked_neg(),
            })
          };
        }
      }
    }
  }

  /// Submits what is queued and posts every completion already due, without waiting: an
  /// `io_uring_enter` with `GETEVENTS` and `min_complete = 0` (liburing's `io_uring_get_events`).
  /// Under `DEFER_TASKRUN` a ready poll is task work the kernel runs only inside such an enter, so
  /// this is how a spinning or busy shard sees a socket become ready. Asking for one completion
  /// with a zero timeout instead sends the kernel down its sleeping path — it arms a timer,
  /// schedules the thread out and wakes it on expiry — which cost 0.3–1.0 ms per harvest on a
  /// loaded Linux VM and put that on every NFS request
  /// (`docs/bugs/2026-09-26-io-uring-zero-timeout-harvest-sleeps.md`). The crate's helpers set
  /// `GETEVENTS` only when waiting for at least one completion, hence the raw enter.
  fn harvest_ready(&mut self) -> std::io::Result<usize> {
    let queued = u32::try_from(self.ring.submission().len()).unwrap_or(u32::MAX);
    // SAFETY: no argument pointer is passed (`None`, so the size is ignored by the kernel without
    // `EXT_ARG`); every queued entry is a buffer-free poll or no-op built by this module (the
    // `submit_entry` invariant), so submitting them borrows no userspace memory.
    unsafe {
      self
        .ring
        .submitter()
        .enter::<libc::sigset_t>(queued, 0, ENTER_GETEVENTS, None)
    }
  }

  fn drain_kick(&self) {
    let mut word = [0u8; size_of::<u64>()];
    let _ = self.efd.with(|efd| rustix::io::read(efd, &mut word));
  }
}

impl Drop for UringDriver {
  fn drop(&mut self) {
    if let Err(error) = self.retire() {
      // Drop cannot return a refusal. Report the typed error before the ring's own cleanup;
      // never silently present an interrupted retirement as proof that its sockets are free.
      eprintln!("slates: io_uring retirement failed: {error}");
    }
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
      Some(0) => self.harvest_ready(),
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
    self.submit_entry(nop)
  }

  fn register_readable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    self.arm_poll(raw, POLL_READABLE, user_data)
  }

  fn register_writable(&mut self, raw: i32, user_data: u64) -> Result<(), RtError> {
    self.arm_poll(raw, POLL_WRITABLE, user_data)
  }
}

/// Format: nanoseconds per second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
