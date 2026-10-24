//! Awaiting a descriptor's readiness through the shard's driver (§4.10a, §4.6). On its first poll a future
//! registers one-shot interest — readable, or writable — and yields. The driver's completion (or the simulation
//! fabric's wake) names that registration, and the shard marks it fired and re-queues the task
//! ([`crate::interest`], A-149). The future answers ready only when *its own* descriptor fired, so the caller
//! retries its non-blocking syscall exactly when that descriptor became ready.
//!
//! One path serves every descriptor. The UDP datagram socket awaits readability (a real fd, or a simulated fabric
//! port). The TCP stream awaits readability for `read`/`accept`, and writability for a `write` whose send buffer
//! filled. The driver decides how the edge is watched (kqueue `EVFILT_READ`/`EVFILT_WRITE`, epoll
//! `EPOLLIN`/`EPOLLOUT`, IOCP's AFD poll); the future is the same either way, which is why it lives here and not in
//! the socket modules. [`readable`] is public because a bridge queue's doorbell is the same edge: the virtio-fs
//! device (`slates-bridge-virtiofs`, §4.6) awaits the kick descriptor its VMM handed it (an eventfd or a pipe)
//! through the shard's driver exactly as a socket is awaited, and drains it itself. The FUSE serve loop races its
//! device against its change signal the same way.
//!
//! **A task can race several waits.** Until A-149 a registered future answered ready on any wake of its task. A
//! completion carried only the task's word, so a wake from another source was taken for the descriptor's own:
//! - a daemon serving two FUSE mounts of a volume spun a core after one write, because the change signal's wake
//!   was read as the device's and the signal never drained;
//! - a deadline raced against a bare readiness wait (`futures::within`) could never be reported.
//!
//! (`docs/bugs/2026-10-22-a-readiness-wait-took-any-wake-of-its-task-for-its-own.md`.) A ready answer can still find
//! nothing to do — another reader took the data first, or the drivers' level and edge semantics differ — so callers
//! keep retrying their syscall on `EAGAIN` and awaiting again.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::RtError;
use crate::interest::{Direction, Taken};
use crate::registry;
use crate::waker::polling_task;

/// Awaits one readiness edge on `raw` through the shard's driver. It registers one-shot interest on the first
/// poll and yields. Once the driver reports that very registration fired, the next poll is ready, and the caller
/// retries the non-blocking syscall. A wake from anywhere else leaves it pending.
struct Ready {
  raw: i32,
  direction: Direction,
  /// The token of the registration this future holds, from its first poll until it answers or is dropped.
  token: Option<u64>,
}

impl Future for Ready {
  type Output = Result<(), RtError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), RtError>> {
    let (raw, direction) = (self.raw, self.direction);
    if let Some(token) = self.token {
      match registry::with_current(|ctx| ctx.take_interest(raw, direction, token)) {
        Some(Taken::Fired) => {
          self.token = None;
          return Poll::Ready(Ok(()));
        }
        // Another source woke the task; this registration stands and wakes it when its descriptor fires.
        Some(Taken::Waiting) => return Poll::Pending,
        // A later registration of the same descriptor and direction replaced this one: register again.
        Some(Taken::Lost) => self.token = None,
        None => return Poll::Ready(Err(RtError::NotOnShardThread)),
      }
    }
    let Some(word) = polling_task(cx.waker()) else {
      // A foreign waker cannot be registered on the driver; degrade to a retry (the caller's loop copes).
      return Poll::Ready(Ok(()));
    };
    match registry::with_current(|ctx| ctx.register_interest(raw, direction, word.word())) {
      Some(Ok(token)) => {
        self.token = Some(token);
        Poll::Pending
      }
      Some(Err(e)) => Poll::Ready(Err(e)),
      None => Poll::Ready(Err(RtError::NotOnShardThread)),
    }
  }
}

impl Drop for Ready {
  fn drop(&mut self) {
    if let Some(token) = self.token.take() {
      // Off the shard thread (a runtime dropping its tasks as it ends) there is no table to forget it from: the
      // shard's table ends with its context.
      let _ = registry::with_current(|ctx| ctx.forget_interest(self.raw, self.direction, token));
    }
  }
}

/// Awaits `raw`'s readability once (a real socket fd, a simulated fabric port, or a bridge
/// queue's doorbell descriptor).
pub async fn readable(raw: i32) -> Result<(), RtError> {
  Ready {
    raw,
    direction: Direction::Readable,
    token: None,
  }
  .await
}

/// Awaits `raw`'s writability once (a real socket whose send buffer filled, or a connect in progress; a Shards fs
/// stream whose peer has not yet read a reply, `slates-server`'s `shards`).
pub async fn writable(raw: i32) -> Result<(), RtError> {
  Ready {
    raw,
    direction: Direction::Writable,
    token: None,
  }
  .await
}
