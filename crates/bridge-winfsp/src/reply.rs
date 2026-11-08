//! How the receive thread waits for the shard's reply (A-212, §4.6 Windows). A reply usually comes back in less time than
//! parking and being woken costs: on CI's arm64 runner a quiet flush's whole shard hop took 19.4 µs at the p50 with the
//! thread parked and 10.1 µs with it spinning, and an open's 18.4 µs against 8.0 µs (runs 42047111024 and 42049771150).
//! So the thread spins for as long as a wake costs, then parks: the 2-competitive rule the shards use (§4.3), which never
//! pays more than twice the better choice.
//!
//! The window is measured, not assumed. The machine profile's `wake.mean_ns` is 0.15–1.2 µs on these runners, far below
//! what this channel's wake costs, so it cannot serve. The thread measures the wake it would otherwise pay, through the
//! same channel, once as it starts, off every data path.

use std::cell::Cell;
use std::sync::mpsc::{Receiver, RecvError, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use crate::host::WinFspError;

/// Shape: wakes timed for the median: enough that one scheduling delay cannot set the window, few enough to cost a mount
/// about a millisecond with [`PARK_PAUSE`].
const SAMPLES: usize = 15;
/// Shape: the pause before each timed send, so the other thread has stopped spinning in `recv` and parked: `std`'s channel
/// spins for microseconds before it parks, and a send it caught spinning would time no wake.
const PARK_PAUSE: Duration = Duration::from_micros(100);

thread_local! {
  /// This thread's spin before it parks for a reply. Zero, as on any thread that measured none, parks at once.
  static WINDOW: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// How a wait for a reply ended, for the test that the fast path is taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
  /// The reply came within the window.
  Spun,
  /// The window passed, and the thread parked.
  Parked,
  /// The window passed and the reply was already there: no sleep (diag only).
  Late,
}

/// Measures this machine's one-way wake through a bounded channel and adopts it as this thread's window. A thread the OS
/// refuses is a refused mount, as the receive thread's own would be.
pub(crate) fn measure() -> Result<(), WinFspError> {
  let window = one_way_wake()?;
  slates_machine::log_line!(
    "slates-winfsp: reply spin window = {} ns: the median one-way channel wake of {SAMPLES} (the 2-competitive spin)",
    window.as_nanos()
  );
  WINDOW.set(window);
  Ok(())
}

/// The median time from a send to a parked thread having it.
fn one_way_wake() -> Result<Duration, WinFspError> {
  let (sends, arrivals) = sync_channel::<Instant>(SAMPLES);
  std::thread::scope(|scope| {
    let woken = std::thread::Builder::new()
      .name("slates-winfsp-wake-probe".to_owned())
      .spawn_scoped(scope, move || {
        let mut wakes = Vec::with_capacity(SAMPLES);
        while let Ok(sent) = arrivals.recv() {
          wakes.push(sent.elapsed());
        }
        wakes
      })
      .map_err(|_| WinFspError::MountThread)?;
    for _ in 0..SAMPLES {
      let paused = Instant::now();
      while paused.elapsed() < PARK_PAUSE {
        std::hint::spin_loop();
      }
      sends
        .send(Instant::now())
        .map_err(|_| WinFspError::MountThread)?;
    }
    drop(sends);
    let mut wakes = woken.join().map_err(|_| WinFspError::MountThread)?;
    wakes.sort_unstable();
    wakes
      .get(wakes.len() / 2)
      .copied()
      .ok_or(WinFspError::MountThread)
  })
}

/// Waits for the reply on `answer`, spinning for this thread's window and then parking. A closed channel is the same
/// error from either.
pub(crate) fn await_reply<T>(answer: &Receiver<T>) -> Result<T, RecvError> {
  wait_within(answer, WINDOW.get()).0
}

/// [`await_reply`], answering too whether the thread parked and how long it spun first, in nanoseconds (diag only).
/// How a profiled wait ended (diag only): caught spinning, caught just after the window, or after a real sleep.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ended {
  Spun,
  Late,
  Slept,
}

pub(crate) fn await_reply_timed<T>(answer: &Receiver<T>) -> (Result<T, RecvError>, Ended, u64) {
  let started = Instant::now();
  let (answered, waited) = wait_within(answer, WINDOW.get());
  let ended = match waited {
    Waited::Spun => Ended::Spun,
    Waited::Late => Ended::Late,
    Waited::Parked => Ended::Slept,
  };
  let spun = if ended == Ended::Spun {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
  } else {
    u64::try_from(WINDOW.get().as_nanos()).unwrap_or(u64::MAX)
  };
  (answered, ended, spun)
}

fn wait_within<T>(answer: &Receiver<T>, window: Duration) -> (Result<T, RecvError>, Waited) {
  let started = Instant::now();
  while started.elapsed() < window {
    match answer.try_recv() {
      Ok(reply) => return (Ok(reply), Waited::Spun),
      Err(TryRecvError::Disconnected) => return (Err(RecvError), Waited::Spun),
      Err(TryRecvError::Empty) => std::hint::spin_loop(),
    }
  }
  // Diag only: a reply already waiting when the window ends is a late catch, not a wake.
  if let Ok(reply) = answer.try_recv() {
    return (Ok(reply), Waited::Late);
  }
  (answer.recv(), Waited::Parked)
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::*;

  /// Shape: a window no reply in these tests outlasts; a stuck test fails within it rather than passing by parking.
  const LONG: Duration = Duration::from_secs(10);

  /// A-212. Do: send a reply from another thread while waiting with a long window. Expect: it is delivered without
  /// parking, so the fast path is taken and not merely present.
  #[test]
  fn a_reply_within_the_window_is_taken_without_parking() {
    let (reply, answer) = sync_channel(1);
    std::thread::scope(|scope| {
      scope.spawn(move || reply.send(7u32).unwrap());
      let (got, waited) = wait_within(&answer, LONG);
      assert_eq!(got, Ok(7));
      assert_eq!(waited, Waited::Spun);
    });
  }

  /// A-212. Do: wait with no window, the reply arriving only after the wait has begun. Expect: the thread parks and the
  /// reply still arrives, so a window too short for a reply loses no reply.
  #[test]
  fn a_reply_past_the_window_is_delivered_by_parking() {
    let (reply, answer) = sync_channel(1);
    let (waiting, is_waiting) = sync_channel::<()>(1);
    std::thread::scope(|scope| {
      scope.spawn(move || {
        is_waiting.recv().unwrap();
        reply.send(7u32).unwrap();
      });
      waiting.send(()).unwrap();
      let (got, waited) = wait_within(&answer, Duration::ZERO);
      assert_eq!(got, Ok(7));
      assert_eq!(waited, Waited::Parked);
    });
  }

  /// A-212. Do: close the channel during a wait, inside the window and with none. Expect: the error from both, never a
  /// hang: this is how a waiting thread learns the mount is being torn down.
  #[test]
  fn a_closed_channel_ends_a_wait_whichever_way_it_waits() {
    for window in [LONG, Duration::ZERO] {
      let (reply, answer) = sync_channel::<u32>(1);
      drop(reply);
      assert_eq!(wait_within(&answer, window).0, Err(RecvError));
    }
  }

  /// A-212. Do: measure, then wait on a thread that measured and one that did not. Expect: a positive wake within reason
  /// is adopted only by the thread that measured it, so no other thread starts spinning.
  #[test]
  fn only_the_thread_that_measured_spins() {
    std::thread::scope(|scope| {
      scope
        .spawn(|| {
          measure().unwrap();
          let window = WINDOW.get();
          assert!(!window.is_zero());
          assert!(window < LONG, "{window:?}");
        })
        .join()
        .unwrap();
    });
    assert_eq!(WINDOW.get(), Duration::ZERO);
  }
}
