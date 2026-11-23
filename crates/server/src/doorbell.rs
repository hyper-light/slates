//! The shared-memory doorbell watcher (§4.7): on macOS and Windows a client rings
//! the bootstrap object's word (and, on Windows, signals the named doorbell Event, since the word's
//! wake is process-local there, D-10). This owned thread waits on it and kicks the shards the rings
//! marked: a client's ring marks the shard its region names, a claim marks the control shard (A-381).
//! Shutdown wakes and joins it. Linux clients write eventfds directly, and the control shard
//! awaits rendezvous readiness through its driver, so Linux starts no watcher thread.
//!
//! **Only the shard rung is kicked (A-381).** Until 2026-11-18 each ring kicked every shard: at 1,000 clients over 23
//! shards on a Mac Studio each shard parked and woke about 7,500 times a second whatever its own clients asked (a
//! shard's parks were alike on shards serving one connection and two), so each request a client sent to a parked shard
//! woke the 22 others too, and the control shard ran an accept round for every request.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::thread::JoinHandle;

use slates_ipc::rendezvous::{DoorbellWaiter, Pending};
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
  /// Starts the thread: it waits on `waiter` and, each time the doorbell rings, kicks the shards the rings marked, each
  /// found in `kicks` by its id (the control shard first); a claim's ring sets `rang` and kicks the control shard.
  /// `waker` is a second handle on the same doorbell, rung by [`Self::stop`].
  pub fn start(
    waiter: DoorbellWaiter,
    waker: DoorbellWaiter,
    kicks: Vec<(u16, Kick)>,
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
      slates_machine::log_line!(
        "slates-server: the doorbell stop could not wake its thread: {error}"
      );
    }
    if let Some(handle) = self.handle.take()
      && handle.join().is_err()
    {
      slates_machine::log_line!("slates-server: the doorbell thread panicked");
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
  kicks: &[(u16, Kick)],
  stop: &Receiver<()>,
  rang_flag: &'static AtomicBool,
) {
  // The value last acted on: a ring that lands while the shards are being kicked shows as a
  // change on the next comparison, never lost (the wait compares against `seen`, not against
  // a fresh read).
  let mut seen = match waiter.current() {
    Ok(value) => value,
    Err(error) => {
      slates_machine::log_line!(
        "slates-server: the doorbell thread stopped: its word could not be read: {error}"
      );
      return;
    }
  };
  // A ring that landed before that read (a claim made between the daemon's publishing its rendezvous and this thread's
  // start, or into an anchor's object while no daemon ran) moved the word already, so no wait shows it: its marks are
  // taken now, once. A ring sets its mark before it moves the word, so a later one shows in the first wait.
  match waiter.take_pending() {
    Ok(pending) => kick_pending(&pending, kicks, rang_flag),
    Err(error) => {
      slates_machine::log_line!(
        "slates-server: the doorbell thread stopped: its rings' marks could not be read: {error}"
      );
      return;
    }
  }
  while !stopped(stop) {
    // A refused wait ends the thread by name rather than retrying it in a loop that would spin:
    // the refusals left are the OS's own (a bad address), never a timeout or an interruption.
    let now = match waiter.wait(seen, platform::POLL_NS) {
      Ok(now) => now,
      Err(error) => {
        slates_machine::log_line!(
          "slates-server: the doorbell thread stopped: its wait was refused: {error}"
        );
        return;
      }
    };
    let rang = now != seen;
    seen = now;
    if stopped(stop) {
      break;
    }
    if rang {
      let pending = match waiter.take_pending() {
        Ok(pending) => pending,
        Err(error) => {
          slates_machine::log_line!(
            "slates-server: the doorbell thread stopped: its rings' marks could not be read: {error}"
          );
          return;
        }
      };
      kick_pending(&pending, kicks, rang_flag);
    }
  }
}

/// Kicks what `pending` asked for: the control shard (`kicks`' first) for a claim, with `rang_flag` set for its claim
/// poller, and each shard a client rang for, found in `kicks` by its id. A mark naming no shard of this daemon (a
/// client of the daemon before a restart rang into the object the anchor held over it) kicks nothing: that client's
/// next request is on the shard its reconnect names.
fn kick_pending(pending: &Pending, kicks: &[(u16, Kick)], rang_flag: &'static AtomicBool) {
  if pending.claim {
    rang_flag.store(true, Ordering::Release);
    if let Some((_, control)) = kicks.first() {
      control.kick();
    }
  }
  for shard in &pending.shards {
    if let Some((_, kick)) = kicks.iter().find(|(id, _)| id == shard) {
      kick.kick();
    }
  }
}

mod platform {
  /// Shape: the doorbell wait's upper bound, so a stop is seen even if the stop's own ring fails.
  pub(super) const POLL_NS: u64 = 1_000_000_000;
}

#[cfg(all(test, any(target_os = "macos", windows)))]
mod tests {
  #![allow(clippy::unwrap_used)]

  use super::*;

  /// §4.7 (A-381's claim mark). Do: open a daemon's rendezvous, make a client's claim (which marks the control shard
  /// and rings the doorbell), and only then start the doorbell thread. Expect: the thread takes the claim's mark within
  /// its wait's bound, setting the claim poller's flag, so the claim is answered.
  #[test]
  #[cfg_attr(miri, ignore)] // Miri models no shared memory object or OS wake
  fn a_claim_rung_before_the_doorbell_thread_starts_is_taken() {
    let instance = format!("doorbell-early-{}", std::process::id());
    let listener = slates_ipc::rendezvous::Listener::open(&instance, None).unwrap();
    let claim = slates_ipc::begin_connect_as(&instance, 0).unwrap();
    let rang: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
    let mut thread = DoorbellThread::start(
      listener.doorbell_waiter().unwrap().unwrap(),
      listener.doorbell_waiter().unwrap().unwrap(),
      Vec::new(),
      rang,
    )
    .unwrap();
    let began = std::time::Instant::now();
    while !rang.load(Ordering::Acquire)
      && began.elapsed() < std::time::Duration::from_nanos(2 * platform::POLL_NS)
    {
      std::thread::yield_now();
    }
    thread.stop();
    drop(claim);
    assert!(
      rang.load(Ordering::Acquire),
      "the claim rung before the doorbell thread started was taken"
    );
  }
}
