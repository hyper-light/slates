//! Task slots: the pinned future, its state, its parent and child links, the joiner's waker, and
//! the counters the watchdog keeps (§4.3, `TaskSlot`).
//!
//! Children are linked through their parent's slot (first child, siblings) so that a parent's
//! completion cancels them in O(children) and a child's completion unlinks in O(1); no task is
//! untracked (hecate's task-lifecycle law): a task is either joinable (its slot stays until
//! someone joins it) or detached (its slot is reaped the moment it terminates).

use std::future::Future;
use std::pin::Pin;
use std::task::Waker;

use slates_mem::Encoded;

/// A pinned, boxed future with no output: results flow through channels and handles, never
/// through the task table (hecate's model).
pub type BoxedFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// The "no link" sentinel for child and sibling links.
pub const NO_LINK: u32 = u32::MAX;

/// A spawn request that may cross threads: the future is `Send` because it may be created on any
/// thread and run on the shard's.
pub struct SpawnRequest {
  /// The future to run.
  pub future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
  /// The parent task on the target shard, if structured under one.
  pub parent: Option<Encoded>,
}

impl SpawnRequest {
  /// A request.
  pub fn new(
    future: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    parent: Option<Encoded>,
  ) -> Self {
    Self { future, parent }
  }
}

impl std::fmt::Debug for SpawnRequest {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SpawnRequest")
      .field("parent", &self.parent)
      .finish()
  }
}

/// Where a task is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
  /// Waiting to be woken.
  Idle,
  /// On the run queue.
  Queued,
  /// Being polled right now (its future is out of the slot).
  Running,
  /// The future finished or was dropped; waiting for live children to terminate.
  Finishing,
  /// Terminal.
  Done,
}

/// How a task ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
  /// The future returned.
  Completed,
  /// The future was dropped before returning (cancelled by request, by its parent, or by
  /// shutdown).
  Cancelled,
}

/// One task slot.
pub struct TaskSlot {
  /// The state.
  pub state: State,
  /// The future while it lives (taken out while polled).
  pub future: Option<BoxedFuture>,
  /// The parent's slot index on this shard, if any.
  pub parent: Option<u32>,
  /// Live children.
  pub children: u32,
  /// The first child's slot, or `NO_LINK`.
  pub first_child: u32,
  /// The next sibling's slot, or `NO_LINK`.
  pub next_sibling: u32,
  /// The previous sibling's slot, or `NO_LINK`.
  pub prev_sibling: u32,
  /// The waker of whoever awaits this task's terminal state.
  pub join_waker: Option<Waker>,
  /// The outcome once terminal.
  pub outcome: Option<Outcome>,
  /// Set when cancellation was requested; honoured at the next poll boundary.
  pub cancel_requested: bool,
  /// Whether the slot stays until joined (true) or is reaped at termination (false).
  pub joinable: bool,
  /// Polls so far.
  pub polls: u64,
  /// Polls that ran longer than the step budget.
  pub long_steps: u32,
  /// The longest poll, in nanoseconds.
  pub longest_step_ns: u64,
}

impl TaskSlot {
  /// A fresh slot for `future` under `parent`.
  pub fn new(future: BoxedFuture, parent: Option<u32>, joinable: bool) -> Self {
    Self {
      state: State::Idle,
      future: Some(future),
      parent,
      children: 0,
      first_child: NO_LINK,
      next_sibling: NO_LINK,
      prev_sibling: NO_LINK,
      join_waker: None,
      outcome: None,
      cancel_requested: false,
      joinable,
      polls: 0,
      long_steps: 0,
      longest_step_ns: 0,
    }
  }

  /// Whether the task reached its terminal state.
  pub const fn is_done(&self) -> bool {
    matches!(self.state, State::Done)
  }
}

impl std::fmt::Debug for TaskSlot {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TaskSlot")
      .field("state", &self.state)
      .field("parent", &self.parent)
      .field("children", &self.children)
      .field("outcome", &self.outcome)
      .field("joinable", &self.joinable)
      .field("polls", &self.polls)
      .finish()
  }
}
