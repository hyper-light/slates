//! The doorbell thread (§4.7 "a parked shard is woken by the driver kick the client sends
//! through the control fd"): on Linux a client writes the shard's kick eventfd itself and this
//! thread only watches the rendezvous socket for new connections; on macOS and Windows a
//! client rings the daemon-wide word of the bootstrap object, and this thread waits on that
//! word and kicks every shard. One thread per daemon, owned by it, stopped and joined at
//! shutdown (no fire-and-forget).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use slates_rt::driver::Kick;

/// The daemon's doorbell thread.
pub struct DoorbellThread {
  /// The stop signal: a sender the thread's receiver sees go away (or deliver) at its next turn. A
  /// channel, not a shared flag, so nothing is leaked per boot and nothing is shared by reference.
  stop: Option<Sender<()>>,
  handle: Option<JoinHandle<()>>,
  waker: Option<(slates_mem::SharedObject, usize)>,
}

impl std::fmt::Debug for DoorbellThread {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DoorbellThread")
      .field("running", &self.handle.is_some())
      .finish()
  }
}

/// What the thread waits on.
pub enum Waits {
  /// The bootstrap object's doorbell word (macOS, Windows): the thread's own mapping.
  Word {
    /// The mapping.
    object: slates_mem::SharedObject,
    /// The word's offset.
    offset: usize,
  },
  /// A listening socket's readiness, by raw descriptor (Linux).
  Socket(i32),
}

impl DoorbellThread {
  /// Starts the thread: it waits as `waits` says, sets `rang`, and kicks `kicks` (every
  /// shard, the control shard first) each time.
  pub fn start(waits: Waits, kicks: Vec<Kick>, rang: &'static AtomicBool) -> DoorbellThread {
    let (stop, stop_signal) = channel();
    let waker = match &waits {
      Waits::Word { object, offset } => object
        .handoff()
        .ok()
        .and_then(|h| slates_mem::SharedObject::open(&h, object.len()).ok())
        .map(|o| (o, *offset)),
      Waits::Socket(_) => None,
    };
    let handle = std::thread::Builder::new()
      .name("slates-doorbell".to_owned())
      .spawn(move || run(waits, kicks, &stop_signal, rang))
      .ok();
    DoorbellThread {
      stop: Some(stop),
      handle,
      waker,
    }
  }

  /// Stops the thread and joins it.
  pub fn stop(&mut self) {
    // Dropping the sender is the signal: the thread's next `try_recv` reads `Disconnected`.
    drop(self.stop.take());
    if let Some((object, offset)) = &self.waker
      && let Ok(word) = object.atomic_u32(*offset)
    {
      word.fetch_add(1, Ordering::AcqRel);
      let _ = slates_ipc::wake::wake_one(word);
    }
    if let Some(handle) = self.handle.take() {
      let _ = handle.join();
    }
  }
}

impl Drop for DoorbellThread {
  fn drop(&mut self) {
    self.stop();
  }
}

/// Whether the daemon has asked the thread to stop: the sender was dropped (or sent).
fn stopped(stop: &Receiver<()>) -> bool {
  !matches!(stop.try_recv(), Err(TryRecvError::Empty))
}

fn run(waits: Waits, kicks: Vec<Kick>, stop: &Receiver<()>, rang_flag: &'static AtomicBool) {
  // The value last acted on: a ring that lands while the shards are being kicked shows as a
  // change on the next comparison, never lost (the wait compares against `seen`, not against
  // a fresh read).
  let mut seen = match &waits {
    Waits::Word { object, offset } => object
      .atomic_u32(*offset)
      .map(|w| w.load(Ordering::Acquire))
      .unwrap_or(0),
    Waits::Socket(_) => 0,
  };
  while !stopped(stop) {
    let rang = match &waits {
      Waits::Word { object, offset } => match object.atomic_u32(*offset) {
        Ok(word) => {
          let _ = slates_ipc::wake::wait(word, seen, Some(platform::POLL_NS));
          let now = word.load(Ordering::Acquire);
          let changed = now != seen;
          seen = now;
          changed
        }
        Err(_) => return,
      },
      Waits::Socket(fd) => platform::socket_readable(*fd),
    };
    if stopped(stop) {
      break;
    }
    if rang {
      rang_flag.store(true, Ordering::Release);
      for kick in &kicks {
        kick.kick();
      }
    }
  }
}

#[cfg(target_os = "linux")]
mod platform {
  /// Shape: the wait's upper bound so a stop is seen within it (the thread otherwise blocks
  /// on readiness); a second, far above any rendezvous cadence.
  pub(super) const POLL_NS: u64 = 1_000_000_000;
  /// Format: nanoseconds per second.
  const NS_PER_S: u64 = 1_000_000_000;

  /// Blocks until the listening socket is readable (a connection pending) or the bound.
  pub(super) fn socket_readable(fd: i32) -> bool {
    use rustix::event::{PollFd, PollFlags};
    // SAFETY: the number names the daemon's listening socket, which lives as long as the
    // daemon and is borrowed here for one poll.
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(&borrowed, PollFlags::IN)];
    let timeout = rustix::event::Timespec {
      tv_sec: i64::try_from(POLL_NS / NS_PER_S).unwrap_or(1),
      tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&timeout)).is_ok_and(|n| n > 0)
  }
}

#[cfg(not(target_os = "linux"))]
mod platform {
  /// Shape: the wait's upper bound so a stop is seen within it; a second.
  pub(super) const POLL_NS: u64 = 1_000_000_000;

  pub(super) fn socket_readable(_fd: i32) -> bool {
    false
  }
}
