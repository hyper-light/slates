//! How the receive thread waits for the shard's reply (A-212, A-560; §4.6 Windows). A reply usually comes back in less
//! time than parking and being woken costs: on CI's arm64 runner a quiet flush's whole shard hop took 19.4 µs at the p50
//! with the thread parked and 10.1 µs with it spinning, and an open's 18.4 µs against 8.0 µs (runs 42047111024 and
//! 42049771150). So the thread spins for as long as a wake costs, then parks: the 2-competitive rule the shards use
//! (§4.3), which never pays more than twice the better choice.
//!
//! The window is learned, not assumed, and keeps being learned (A-560, [`crate::spin_window`]). Every reply carries the
//! instant it was sent ([`Stamped`]), so each reply the thread parked for is one of its own wakes, timed from the send to
//! the thread running again. The machine profile's `wake.mean_ns` (0.15–1.2 µs on these runners) is far below what this
//! channel's wake costs, so it cannot serve; until A-560 the thread timed fifteen probe wakes once as it started and kept
//! their median for good, unbounded, which a noisy start set at 108 to 144 µs. The window is zero until the thread's
//! wakes agree, and never longer than the step quantum of the shard that started the mount ([`start`]).

use std::cell::Cell;
use std::sync::mpsc::{Receiver, RecvError, TryRecvError};
use std::time::{Duration, Instant};

use crate::spin_window::SpinWindow;

/// A reply and the instant it was sent, so a thread that parked for it times its own wake (A-560).
pub(crate) struct Stamped<T> {
  /// The reply.
  pub(crate) reply: T,
  /// When the shard sent it.
  pub(crate) sent: Instant,
}

impl<T> Stamped<T> {
  /// `reply`, sent now.
  pub(crate) fn now(reply: T) -> Self {
    Self {
      reply,
      sent: Instant::now(),
    }
  }
}

thread_local! {
  /// This thread's spin window, learned from its own wakes. A thread that started none spins never: under a ceiling of
  /// zero it parks at once.
  static WINDOW: Cell<SpinWindow> = const { Cell::new(SpinWindow::new(0)) };
}

/// How a wait for a reply ended, for the tests that the fast path is taken and that only a park teaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
  /// The reply came within the window.
  Spun,
  /// The window passed, and the thread parked at this instant.
  Parked(Instant),
}

/// Starts this thread's learning: a window of zero until its wakes agree, never longer than `ceiling`, the most the
/// machine allows a spin (the step quantum of the shard that started the mount; zero off any shard).
pub(crate) fn start(ceiling: Duration) {
  WINDOW.set(SpinWindow::new(
    u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX),
  ));
}

/// Waits for the reply on `answer`, spinning for this thread's window and then parking. A reply the thread parked for
/// and that was sent after it parked is one of its wakes, folded into its window. A closed channel is the same error
/// from either.
pub(crate) fn await_reply<T>(answer: &Receiver<Stamped<T>>) -> Result<T, RecvError> {
  let mut window = WINDOW.get();
  let (stamped, waited) = wait_within(answer, Duration::from_nanos(window.window_ns()));
  let stamped = stamped?;
  if let Waited::Parked(parked_at) = waited
    && stamped.sent >= parked_at
  {
    window.record(u64::try_from(stamped.sent.elapsed().as_nanos()).unwrap_or(u64::MAX));
    WINDOW.set(window);
  }
  Ok(stamped.reply)
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
  let parked_at = Instant::now();
  (answer.recv(), Waited::Parked(parked_at))
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use std::sync::mpsc::sync_channel;

  use super::*;

  /// Shape: a window no reply in these tests outlasts; a stuck test fails within it rather than passing by parking.
  const LONG: Duration = Duration::from_secs(10);
  /// Shape: how long a replier waits after the receiver said it is waiting, so the receiver has parked by the send.
  const PAUSE: Duration = Duration::from_millis(1);
  /// Shape: the replies a learning thread parks for in the test.
  const PARKED_REPLIES: u32 = 4;

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
      assert!(matches!(waited, Waited::Parked(_)), "{waited:?}");
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

  /// A-560. Do: on a thread that started learning, take a reply already waiting, then park for replies sent after the
  /// wait began; on another thread that never started, check its window. Expect: the waiting reply teaches nothing,
  /// each parked-for reply is one wake folded in, and the other thread has learned nothing and spins never.
  #[test]
  fn a_thread_learns_its_window_only_from_the_replies_it_parked_for() {
    std::thread::scope(|scope| {
      scope
        .spawn(|| {
          start(LONG);
          let (reply, answer) = sync_channel(1);
          reply.send(Stamped::now(7u32)).unwrap();
          assert_eq!(await_reply(&answer), Ok(7));
          assert_eq!(
            WINDOW.get().samples(),
            0,
            "a reply already there is no wake"
          );
          for _ in 0..PARKED_REPLIES {
            let (reply, answer) = sync_channel(1);
            let (waiting, is_waiting) = sync_channel::<()>(1);
            std::thread::scope(|inner| {
              inner.spawn(move || {
                is_waiting.recv().unwrap();
                std::thread::sleep(PAUSE);
                reply.send(Stamped::now(7u32)).unwrap();
              });
              waiting.send(()).unwrap();
              assert_eq!(await_reply(&answer), Ok(7));
            });
          }
          assert!(WINDOW.get().samples() > 0, "{:?}", WINDOW.get());
        })
        .join()
        .unwrap();
    });
    assert_eq!(WINDOW.get().samples(), 0);
    assert_eq!(WINDOW.get().window_ns(), 0);
  }
}
