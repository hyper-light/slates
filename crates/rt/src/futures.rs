//! What a task can do: spawn siblings and children, join, cancel, yield, and sleep on the
//! shard's wheel (§4.3). Every future here is cancel-safe by construction: the timer a `Sleep`
//! arms lives in the wheel's slab and is disarmed when the future drops.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use slates_mem::MemError;

use crate::error::RtError;
use crate::registry;
use crate::shard::{ShardContext, ShardId, TaskId, boxed};
use crate::task::Outcome;
use crate::timer::TimerId;
use crate::waker::polling_task;

/// The current shard's id, if this thread runs one.
pub fn shard_id() -> Option<ShardId> {
  registry::with_current(|ctx| ShardId(ctx.id))
}

/// The current shard's monotonic clock in nanoseconds, or zero if this thread runs no shard (a caller
/// that reads it off a shard thread, such as timing a round trip, gets the driver's clock; the sim's
/// clock in a simulation).
pub fn now_ns() -> u64 {
  registry::with_current(|ctx| ctx.now_ns()).unwrap_or(0)
}

/// The current shard's measured scheduler quantum in nanoseconds — how late its steps have been
/// running after the waits before them, an exponentially-forgetting maximum
/// ([`crate::shard::ShardContext::scheduler_overrun_ns`]: only an idle shard waits, so this is the
/// operating system's descheduling of the shard, not the latency of its own tasks). Zero off a shard
/// thread. The fleet's failure detector floors its windows at it (§4.8 "SWIM period = max(k × RTT
/// p99, scheduler quantum)").
pub fn scheduler_overrun_ns() -> u64 {
  registry::with_current(|ctx| ctx.scheduler_overrun_ns()).unwrap_or(0)
}

/// The current shard's step quantum in nanoseconds (§4.3, "a step longer than a peer's wake starves the
/// shard"): its online wake estimate while it tracks one, else its configured step budget
/// ([`crate::shard::ShardContext::quantum_ns`]). A cooperative operation sizes its slices by it (a
/// destroy's, an archive's). `None` off a shard thread.
pub fn step_budget_ns() -> Option<u64> {
  registry::with_current(|ctx| ctx.quantum_ns())
}

/// The current shard's online wake estimate in nanoseconds (the configured step budget when it tracks
/// none); what the daemon's own idle window scales. `None` off a shard thread.
pub fn wake_cost_ns() -> Option<u64> {
  registry::with_current(|ctx| ctx.wake_cost_ns())
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

/// The sleep future: completes, `Ok`, no earlier than its deadline — the shard's clock at the first poll
/// plus the span. A wheel with no free timer does not end it early: the task waits for a timer to free
/// (counted, `Counters::timer_waits`) and arms then, or completes if its deadline passed meanwhile. It is
/// refused only where no wait can succeed: polled off a shard, or with a waker that is not a task's
/// (`NotOnShardThread`). A caller racing it against other work must tell `Ready(Err)` from the deadline —
/// `Poll::is_ready` does not. Until 2026-09-30 every
/// arming failure, a full wheel included, completed the sleep at once, so an overloaded periodic loop spun
/// exactly when the runtime was under pressure (AUD-29-39).
#[derive(Debug)]
pub struct Sleep {
  ns: u64,
  deadline: Option<u64>,
  timer: Option<TimerId>,
}

impl Future for Sleep {
  type Output = Result<(), RtError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let Some(word) = polling_task(cx.waker()) else {
      return Poll::Ready(Err(RtError::NotOnShardThread));
    };
    let Some(now) = registry::with_current(ShardContext::now_ns) else {
      return Poll::Ready(Err(RtError::NotOnShardThread));
    };
    // A deadline past the clock's range saturates: the sleep then never completes, which is exactly "no
    // earlier than its deadline" (the range is some 584 years of nanoseconds), and its timer is the future's
    // own, disarmed when it drops.
    let span = self.ns;
    let deadline = *self.deadline.get_or_insert(now.saturating_add(span));
    if now >= deadline {
      // A timer still armed (the clock passed the deadline before the wheel fired it) is given back.
      if let Some(id) = self.timer.take() {
        let _ = registry::with_current(|ctx| ctx.disarm_timer(id));
      }
      return Poll::Ready(Ok(()));
    }
    if self.timer.is_some() {
      return Poll::Pending;
    }
    match registry::with_current(|ctx| {
      ctx
        .arm_timer(deadline, word.word())
        .map(Some)
        .or_else(|refusal| match refusal {
          // The wheel is full: wait for a timer to free, and arm on the poll that wakes.
          RtError::Mem(MemError::SlabFull { .. }) => ctx.wait_for_timer(word.word()).map(|()| None),
          refusal => Err(refusal),
        })
    }) {
      Some(Ok(armed)) => {
        self.timer = armed;
        Poll::Pending
      }
      Some(Err(refusal)) => Poll::Ready(Err(refusal)),
      None => Poll::Ready(Err(RtError::NotOnShardThread)),
    }
  }
}

/// Races `work` against a deadline `ns` from now on the shard's wheel: `Ok(Some(output))` when the work
/// finishes first (preferred when both are ready), `Ok(None)` once the deadline has passed, and the sleep's
/// refusal when no deadline can be kept (polled off a shard) — never a deadline that did not pass
/// (AUD-29-39). On a wheel with no free timer the deadline, like any sleep, arms once a timer frees (counted,
/// `Counters::timer_waits`), so under timer exhaustion it is enforced late, never early; the shard's timer
/// bound is derived to cover its tasks' concurrent waits, which makes that count an overload tripwire. The one race every timed wait in the workspace uses; before 2026-09-30 each call site raced
/// a pinned sleep by hand and read a refused sleep's completion as the deadline.
pub async fn within<F: Future>(ns: u64, work: F) -> Result<Option<F::Output>, RtError> {
  let mut work = std::pin::pin!(work);
  let mut deadline = std::pin::pin!(sleep(ns));
  std::future::poll_fn(|cx| {
    if let Poll::Ready(output) = work.as_mut().poll(cx) {
      return Poll::Ready(Ok(Some(output)));
    }
    match deadline.as_mut().poll(cx) {
      Poll::Ready(Ok(())) => Poll::Ready(Ok(None)),
      Poll::Ready(Err(refusal)) => Poll::Ready(Err(refusal)),
      Poll::Pending => Poll::Pending,
    }
  })
  .await
}

impl Drop for Sleep {
  fn drop(&mut self) {
    if let Some(id) = self.timer.take() {
      let _ = registry::with_current(|ctx| ctx.disarm_timer(id));
    }
  }
}
