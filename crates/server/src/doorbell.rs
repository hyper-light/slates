//! The shared-memory doorbell watcher (§4.7): on macOS and Windows a client rings
//! the bootstrap object's word (and, on Windows, signals the named doorbell Event, since the word's
//! wake is process-local there, D-10). This owned thread waits on it and kicks each shard; shutdown
//! wakes and joins it. Linux clients write eventfds directly, and the control shard
//! awaits rendezvous readiness through its driver, so Linux starts no watcher thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use slates_ipc::rendezvous::DoorbellWaiter;
use slates_rt::driver::Kick;

/// The daemon's doorbell thread.
pub struct DoorbellThread {
  /// The stop signal: a sender the thread's receiver sees go away (or deliver) at its next turn. A
  /// channel, not a shared flag, so nothing is leaked per boot and nothing is shared by reference.
  stop: Option<Sender<()>>,
  handle: Option<JoinHandle<()>>,
  /// The daemon's own handle on the doorbell, rung to wake the thread for its stop.
  waker: DoorbellWaiter,
}

impl std::fmt::Debug for DoorbellThread {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DoorbellThread")
      .field("running", &self.handle.is_some())
      .finish()
  }
}

impl DoorbellThread {
  /// Starts the thread: it waits on `waiter`, sets `rang`, and kicks `kicks` (every shard, the
  /// control shard first) each time the doorbell rings. `waker` is a second handle on the same
  /// doorbell, rung by [`Self::stop`].
  pub fn start(
    waiter: DoorbellWaiter,
    waker: DoorbellWaiter,
    kicks: Vec<Kick>,
    rang: &'static AtomicBool,
  ) -> Result<DoorbellThread, slates_ipc::IpcError> {
    let (stop, stop_signal) = channel();
    let handle = std::thread::Builder::new()
      .name("slates-doorbell".to_owned())
      .spawn(move || run(&waiter, &kicks, &stop_signal, rang))
      .map_err(|error| slates_ipc::IpcError::OsRefused {
        call: "spawn the doorbell thread",
        code: error.raw_os_error(),
      })?;
    Ok(DoorbellThread {
      stop: Some(stop),
      handle: Some(handle),
      waker,
    })
  }

  /// Stops the thread and joins it.
  pub fn stop(&mut self) {
    // Dropping the sender is the signal: the thread's next `try_recv` reads `Disconnected`.
    drop(self.stop.take());
    if let Err(error) = self.waker.ring() {
      // The thread still sees the stop when its wait's bound ends (`platform::POLL_NS`).
      eprintln!("slates-server: the doorbell stop could not wake its thread: {error}");
    }
    if let Some(handle) = self.handle.take()
      && handle.join().is_err()
    {
      eprintln!("slates-server: the doorbell thread panicked");
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

fn run(
  waiter: &DoorbellWaiter,
  kicks: &[Kick],
  stop: &Receiver<()>,
  rang_flag: &'static AtomicBool,
) {
  // The value last acted on: a ring that lands while the shards are being kicked shows as a
  // change on the next comparison, never lost (the wait compares against `seen`, not against
  // a fresh read).
  let mut seen = match waiter.current() {
    Ok(value) => value,
    Err(error) => {
      eprintln!("slates-server: the doorbell thread stopped: its word could not be read: {error}");
      return;
    }
  };
  while !stopped(stop) {
    // A refused wait ends the thread by name rather than retrying it in a loop that would spin:
    // the refusals left are the OS's own (a bad address), never a timeout or an interruption.
    let now = match waiter.wait(seen, platform::POLL_NS) {
      Ok(now) => now,
      Err(error) => {
        eprintln!("slates-server: the doorbell thread stopped: its wait was refused: {error}");
        return;
      }
    };
    let rang = now != seen;
    seen = now;
    if stopped(stop) {
      break;
    }
    if rang {
      rang_flag.store(true, Ordering::Release);
      for kick in kicks {
        kick.kick();
      }
    }
  }
}

mod platform {
  /// Shape: the doorbell wait's upper bound, so a stop is seen even if the stop's own ring fails.
  pub(super) const POLL_NS: u64 = 1_000_000_000;
}
