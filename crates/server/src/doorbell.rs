//! The shared-memory doorbell watcher (§4.7): on macOS and Windows a client rings
//! the bootstrap object's word. This owned thread waits on that word and kicks each shard;
//! shutdown wakes and joins it. Linux clients write eventfds directly, and the control shard
//! awaits rendezvous readiness through its driver, so Linux starts no watcher thread.

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

mod platform {
  /// Shape: the word wait's upper bound so a stop is seen even if the wake syscall fails.
  pub(super) const POLL_NS: u64 = 1_000_000_000;
}
