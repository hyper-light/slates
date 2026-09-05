//! What a task can do: spawn siblings and children, join, cancel, yield, and sleep on the
//! shard's wheel (§4.3). Every future here is cancel-safe by construction: the timer a `Sleep`
//! arms lives in the wheel's slab and is disarmed when the future drops.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::RtError;
use crate::registry;
use crate::shard::{ShardId, TaskId, boxed};
use crate::task::Outcome;
use crate::timer::TimerId;
use crate::waker::word_of;

/// The current shard's id, if this thread runs one.
pub fn shard_id() -> Option<ShardId> {
  registry::with_current(|ctx| ShardId(ctx.id))
}

/// The task being polled on this thread, if any.
pub fn current_task() -> Option<TaskId> {
  registry::with_current(|ctx| ctx.current_task()).flatten()
}

/// Spawns a joinable task on the current shard with no parent.
pub fn spawn<F: Future<Output = ()> + 'static>(future: F) -> Result<TaskId, RtError> {
  registry::with_current(|ctx| ctx.spawn_local(boxed(future), None))
    .ok_or(RtError::NotOnShardThread)?
}

/// Spawns a joinable child of the current task; a parent's completion cancels and joins it.
pub fn spawn_child<F: Future<Output = ()> + 'static>(future: F) -> Result<TaskId, RtError> {
  registry::with_current(|ctx| {
    let parent = ctx.current_task().map(|t| t.0.slot());
    ctx.spawn_local(boxed(future), parent)
  })
  .ok_or(RtError::NotOnShardThread)?
}

/// Requests a task's cancellation (same shard only in this phase).
pub fn cancel(id: TaskId) -> Result<(), RtError> {
  registry::with_current(|ctx| ctx.cancel(id)).ok_or(RtError::NotOnShardThread)?
}

/// Detaches a joinable task.
pub fn detach(id: TaskId) -> Result<(), RtError> {
  registry::with_current(|ctx| ctx.detach(id)).ok_or(RtError::NotOnShardThread)?
}

/// Awaits a task's terminal state (same shard only in this phase).
pub fn join(id: TaskId) -> Join {
  Join { id }
}

/// The join future.
#[derive(Debug)]
pub struct Join {
  id: TaskId,
}

impl Future for Join {
  type Output = Result<Outcome, RtError>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let id = self.id;
    registry::with_current(|ctx| ctx.poll_join(id, cx.waker()))
      .unwrap_or(Poll::Ready(Err(RtError::NotOnShardThread)))
  }
}

/// Yields once: the task is re-queued and resumes after the rest of the batch.
pub fn yield_now() -> YieldNow {
  YieldNow { yielded: false }
}

/// The yield future.
#[derive(Debug)]
pub struct YieldNow {
  yielded: bool,
}

impl Future for YieldNow {
  type Output = ();

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    if self.yielded {
      return Poll::Ready(());
    }
    self.yielded = true;
    cx.waker().wake_by_ref();
    Poll::Pending
  }
}

/// Yields without re-queueing: the task resumes only when something wakes it (a registered
/// poller's ring, a foreign wake, a cancellation). The idle form of a ring-polling task.
pub fn idle() -> Idle {
  Idle { yielded: false }
}

/// The idle future.
#[derive(Debug)]
pub struct Idle {
  yielded: bool,
}

impl Future for Idle {
  type Output = ();

  fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
    if self.yielded {
      return Poll::Ready(());
    }
    self.yielded = true;
    Poll::Pending
  }
}

/// Sleeps for `ns` on the shard's wheel (accuracy: one tick).
pub fn sleep(ns: u64) -> Sleep {
  Sleep {
    ns,
    deadline: None,
    timer: None,
  }
}

/// The sleep future.
#[derive(Debug)]
pub struct Sleep {
  ns: u64,
  deadline: Option<u64>,
  timer: Option<TimerId>,
}

impl Future for Sleep {
  type Output = ();

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    let Some(word) = word_of(cx.waker()) else {
      // A foreign waker cannot be armed on the wheel; the sleep degrades to an immediate return,
      // which the caller's own clock will notice.
      return Poll::Ready(());
    };
    let now = registry::with_current(|ctx| ctx.now_ns()).unwrap_or(0);
    match self.deadline {
      None => {
        let deadline = now.saturating_add(self.ns);
        self.deadline = Some(deadline);
        let armed = registry::with_current(|ctx| ctx.arm_timer(deadline, word.word()));
        match armed {
          Some(Ok(id)) => {
            self.timer = Some(id);
            Poll::Pending
          }
          _ => Poll::Ready(()),
        }
      }
      Some(deadline) if now >= deadline => {
        self.timer = None;
        Poll::Ready(())
      }
      Some(_) => Poll::Pending,
    }
  }
}

impl Drop for Sleep {
  fn drop(&mut self) {
    if let Some(id) = self.timer.take() {
      let _ = registry::with_current(|ctx| ctx.disarm_timer(id));
    }
  }
}
