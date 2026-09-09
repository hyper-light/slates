//! Awaiting a socket's readiness through the shard's driver (§4.10a, §4.6): a future registers
//! one-shot interest — readable, or writable — with the current shard's driver on its first poll and
//! yields; the driver's completion (or the simulation fabric's wake) re-queues the task, and the next
//! poll returns ready so the caller retries its non-blocking syscall. One path serves both sockets:
//! the UDP datagram socket awaits readability (a real fd, or a simulated fabric port), and the TCP
//! stream awaits readability for `read`/`accept` and writability for a `write` whose send buffer
//! filled. The driver decides how the edge is watched (kqueue `EVFILT_READ`/`EVFILT_WRITE`, epoll
//! `EPOLLIN`/`EPOLLOUT`); the future is the same either way, which is why it lives here and not in the
//! socket modules.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::RtError;
use crate::registry;
use crate::waker::word_of;

/// Which readiness edge a caller awaits.
#[derive(Clone, Copy)]
enum Interest {
  /// The socket has data to read, or a listener has a connection to accept.
  Readable,
  /// The socket has send-buffer space for a write (or connect) that returned `EAGAIN`/`EINPROGRESS`.
  Writable,
}

/// Awaits one readiness edge on `raw` through the shard's driver: it registers one-shot interest on
/// the first poll and yields; the driver's completion re-queues the task, and the next poll is ready
/// so the caller retries the non-blocking syscall (a spurious wake just retries).
struct Ready {
  raw: i32,
  interest: Interest,
  armed: bool,
}

impl Future for Ready {
  type Output = Result<(), RtError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), RtError>> {
    if self.armed {
      // The driver woke us; let the caller retry the syscall.
      return Poll::Ready(Ok(()));
    }
    let Some(word) = word_of(cx.waker()) else {
      // A foreign waker cannot be armed on the driver; degrade to a retry (the caller's loop copes).
      return Poll::Ready(Ok(()));
    };
    let (raw, interest) = (self.raw, self.interest);
    let registered = registry::with_current(|ctx| match interest {
      Interest::Readable => ctx.register_readable(raw, word.word()),
      Interest::Writable => ctx.register_writable(raw, word.word()),
    });
    match registered {
      Some(Ok(())) => {
        self.armed = true;
        Poll::Pending
      }
      Some(Err(e)) => Poll::Ready(Err(e)),
      None => Poll::Ready(Err(RtError::NotOnShardThread)),
    }
  }
}

/// Awaits `raw`'s readability once (a real socket fd, or a simulated fabric port).
pub(crate) async fn readable(raw: i32) -> Result<(), RtError> {
  Ready {
    raw,
    interest: Interest::Readable,
    armed: false,
  }
  .await
}

/// Awaits `raw`'s writability once (a real socket fd whose send buffer filled, or a connect in
/// progress).
pub(crate) async fn writable(raw: i32) -> Result<(), RtError> {
  Ready {
    raw,
    interest: Interest::Writable,
    armed: false,
  }
  .await
}
