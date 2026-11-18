//! Cross-shard calls for the daemon's own loops (§4.3 "cross-shard work is a message on a bounded ring";
//! D-7 "one owning shard per volume; bridge queues pinned to the owner"): run a closure on another
//! shard's state and, when the caller needs it, await the result on this shard. The fleet's record plane
//! uses it to reach every owner shard's volumes from the control shard, which alone holds the peer
//! sessions: a seal is walked where its volume lives, its archive moved to the coordinator by value, and
//! the placement recorded back where the volume lives.
//!
//! The mechanism is the one the verbs' forward and the NFS bridge queue already use — a `Control::Spawn`
//! to the target shard, whose task runs the work under that shard's `with_state` and spawns a task back
//! that delivers the result — with the reply kept in a per-shard thread-local pending map (no lock, D-7)
//! and the awaiting task woken. Every call is bounded: a spawn is refused typed (`ControlFull`) at the
//! shard's admission bound rather than queued without limit, and an awaited call is raced against a
//! deadline ([`call_within`]) so a shard that never answers cannot hold the caller forever (banned item
//! 8). A closure whose result no one needs runs fire-and-forget ([`run_on`]), which is not a swallowed
//! error (banned item 9): the work runs to completion on its shard, or the spawn is refused to the caller.
//! A call to the caller's own shard runs the closure directly, so the same code path serves one shard and
//! many (R8).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_rt::control::Control;
use slates_rt::error::RtError;
use slates_rt::registry;
use slates_rt::task::SpawnRequest;

use crate::state::{self, ShardState};

thread_local! {
  /// Calls this shard has sent to another and is awaiting the result of, by call id; only this shard's
  /// thread touches it, so it needs no lock (D-7: no locks on data paths).
  static PENDING: RefCell<BTreeMap<u64, Slot>> = const { RefCell::new(BTreeMap::new()) };
  /// The next call id this shard hands out.
  static NEXT_CALL: Cell<u64> = const { Cell::new(1) };
}

/// A call awaiting its result: the result once it lands (type-erased for the map, downcast by the
/// awaiting future), and the waker of the task awaiting it.
struct Slot {
  result: Option<Box<dyn Any + Send>>,
  waker: Option<Waker>,
}

fn register() -> u64 {
  let id = NEXT_CALL.with(|next| {
    let id = next.get();
    next.set(id.wrapping_add(1));
    id
  });
  PENDING.with(|map| {
    map.borrow_mut().insert(
      id,
      Slot {
        result: None,
        waker: None,
      },
    )
  });
  id
}

fn forget(id: u64) {
  PENDING.with(|map| map.borrow_mut().remove(&id));
}

/// Hands a call's result to the awaiting task and wakes it (run on the origin shard, by the task the
/// target spawned back). A call already forgotten (timed out) drops the result.
fn deliver(id: u64, result: Box<dyn Any + Send>) {
  PENDING.with(|map| {
    if let Some(slot) = map.borrow_mut().get_mut(&id) {
      slot.result = Some(result);
      if let Some(waker) = slot.waker.take() {
        waker.wake();
      }
    }
  });
}

std::thread_local! {
  /// [`run_on_counted`]'s refused spawns on this thread: work that never ran on its shard. Per thread, so each shard
  /// reports its own, and never through the shard's state, which a caller may be borrowing.
  static RUN_REFUSED: Cell<u64> = const { Cell::new(0) };
}

/// [`run_on`] for a caller with nothing to do on a refusal (a placement fact recorded on its owner shard, which the
/// next period re-derives): the refusal is counted on this thread (`xshard.run_refused`), where eight callers dropped
/// it before 2026-10-07.
pub fn run_on_counted(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) + Send + 'static,
) {
  if run_on(origin, shard, work).is_err() {
    RUN_REFUSED.with(|count| count.set(count.get().saturating_add(1)));
  }
}

/// The spawns [`run_on_counted`] saw refused on this thread, so far.
pub fn run_refused() -> u64 {
  RUN_REFUSED.with(Cell::get)
}

/// Format: the status counter of refused cross-shard runs ([`run_on_counted`]).
pub(crate) const RUN_REFUSED_COUNTER: &str = "xshard.run_refused";

/// Runs `work` on `shard`'s state — directly when `shard` is this shard (`origin`), else as a task there —
/// delivering nothing back. The spawn is refused typed at the target's admission bound.
pub fn run_on(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) + Send + 'static,
) -> Result<(), RtError> {
  if shard == origin {
    state::with_state_counted(work);
    return Ok(());
  }
  let task = SpawnRequest::new(
    Box::pin(async move {
      state::with_state_counted(work);
    }),
    None,
  );
  registry::send_control(shard, Control::Spawn(Box::new(task)))
}

/// The future a call's result arrives on: ready when the target's task delivered it, or at once when the
/// call ran on this shard; `None` if the target's state was unavailable, the call was delivered back
/// as something else, or the entry vanished.
pub struct CrossShardCall<T> {
  /// The shard the call runs on, whose own time [`within_shard_time`] judges the wait by.
  target: u16,
  id: Option<u64>,
  ready: Option<Option<T>>,
  // The registration belongs to this thread; neither polling nor dropping may migrate it.
  _thread: PhantomData<*const ()>,
}

impl<T> Drop for CrossShardCall<T> {
  fn drop(&mut self) {
    if let Some(id) = self.id.take() {
      forget(id);
    }
  }
}

// The call holds no self-reference (an id, an optional ready value, a marker), so moving it is sound.
impl<T> Unpin for CrossShardCall<T> {}

impl<T: 'static> Future for CrossShardCall<T> {
  type Output = Option<T>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
    let this = self.get_mut();
    if let Some(ready) = this.ready.take() {
      return Poll::Ready(ready);
    }
    let Some(id) = this.id else {
      return Poll::Ready(None);
    };
    PENDING.with(|map| {
      let mut map = map.borrow_mut();
      let Some(slot) = map.get_mut(&id) else {
        return Poll::Ready(None);
      };
      match slot.result.take() {
        Some(result) => {
          map.remove(&id);
          Poll::Ready(result.downcast::<Option<T>>().ok().and_then(|boxed| *boxed))
        }
        None => {
          slot.waker = Some(cx.waker().clone());
          Poll::Pending
        }
      }
    })
  }
}

/// Sends `back`, the task that carries a result home to `origin` (a reply to its client, an awaited call's answer), and
/// waits for room when the origin's control channel is full, retried at [`REPLY_PACE_NS`], for as long as the origin
/// lives (§4.3: backpressure, never a drop). A full channel drains as the origin turns. Only an origin gone (its client
/// with it) leaves the result undelivered, counted on this shard (`xshard.reply_dropped`).
///
/// **What bounds the waiting (CLAUDE.md §2 item 8).** A result waits inside a task that exists for it anyway (the task
/// that ran a forwarded verb, a scatter part, or a call), or, from a caller that cannot await, in the one task
/// [`carry_home`] spawns for it when the channel is full. So the results waiting on a shard are at most the tasks it
/// holds, and every one answers a request in flight: a client's (each client holds at most its region's `slots` of
/// credit, so a shard serves at most `clients_per_shard × slots` of them at once), an acknowledgement's parts (at most
/// `ack_scatter_bound` scatters a shard) or a call (its caller's own in-flight bound). The hard bound is the shard's task
/// arena: a waiting task it cannot take is refused at its spawn with a typed error the caller counts
/// (`xshard.unstarted_unanswered`, a caller's own undelivered counter), never queued. The waits are counted
/// ([`ROOM_WAITS_STARTED`], [`ROOM_WAITS_ENDED`], [`ROOM_WAITS_PEAK`]), so the bound is seen held.
/// Before 2026-10-07 every refusal dropped the reply at
/// once and unseen; until 2026-11-09 one still refused after a coordinator period was dropped: about 4,300 a run of the
/// 3,000-client rung on main, every one an acknowledgement scatter's part, whose loss refused its client
/// `AcknowledgementOwed` until a later acknowledgement settled the release (`crates/server/tests/reply_room.rs` shows the
/// same loss of a forwarded verb's answer).
pub async fn send_back(origin: u16, back: SpawnRequest) {
  if !given_room(origin, Control::Spawn(Box::new(back))).await {
    state::with_state_counted(|s| s.count(REPLY_DROPPED, 1));
  }
}

/// Sends `message` to `target`, waiting out a full channel for as long as `target` lives: whether it was sent; `false`
/// only for a shard gone, or a pause the runtime refused (off a shard, a tripwire). A wait is counted from the first
/// full channel to its end ([`RoomWait`]).
async fn given_room(target: u16, mut message: Control) -> bool {
  let mut waiting: Option<RoomWait> = None;
  loop {
    match registry::send_control_or_return(target, message) {
      Ok(()) => return true,
      Err(registry::RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => message = unsent,
      Err(_) => return false,
    }
    waiting.get_or_insert_with(RoomWait::begin);
    if slates_rt::futures::sleep(REPLY_PACE_NS).await.is_err() {
      state::with_state_counted(|s| s.count(ROOM_PAUSE_REFUSED, 1));
      return false;
    }
  }
}

/// [`run_on`] for work whose loss would leave state behind on `shard` (a closed connection's byte states, A-192): a
/// channel full at once hands the message to a task of this shard that waits for room as a result carried home does
/// ([`send_back`]). Counted under `refused` on this shard when it is refused past that, the shard is gone, or this shard
/// could not take the waiting task.
pub fn run_on_waiting(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) + Send + 'static,
  refused: &'static str,
) {
  if shard == origin {
    state::with_state_counted(work);
    return;
  }
  let task = SpawnRequest::new(
    Box::pin(async move {
      state::with_state_counted(work);
    }),
    None,
  );
  let unsent = match registry::send_control_or_return(shard, Control::Spawn(Box::new(task))) {
    Ok(()) => return,
    Err(registry::RefusedControl {
      error: RtError::ControlFull { .. },
      message: Some(unsent),
    }) => unsent,
    Err(_) => {
      state::with_state_counted(|s| s.count(refused, 1));
      return;
    }
  };
  let waiting = slates_rt::futures::spawn(async move {
    if !given_room(shard, unsent).await {
      state::with_state_counted(|s| s.count(refused, 1));
    }
  })
  .and_then(slates_rt::futures::detach);
  if waiting.is_err() {
    state::with_state_counted(|s| s.count(refused, 1));
  }
}

/// Sends `message` to `target`, waiting out a full channel for at most `budget_ns` of time, asked again every
/// [`REPLY_PACE_NS`] and never past the budget's deadline: whether it was sent. `false` past the deadline, at once for a
/// shard gone, or once a pause is refused (off a shard, where no wait can succeed; counted). A scatter's part waits this
/// way (`crate::verbs::send_part`), within a budget its client can still hear the answer in. The budget is a span of the
/// shard's clock, not a count of tries: until 2026-11-06 it was `budget / pace + 1` tries of one pause each, and on a
/// starved shard every pause ran late, so the wait stretched with the starvation (51 tries took 1.54 s against a
/// 500 ms budget with the shard kept off its CPU 20 ms at a time; `a_status_on_a_starved_shard_is_refused_overloaded_on_time`
/// in `crates/server/tests/ack_release.rs`).
pub(crate) async fn given_room_within(target: u16, mut message: Control, budget_ns: u64) -> bool {
  let deadline = slates_rt::futures::now_ns().saturating_add(budget_ns);
  let mut waiting: Option<RoomWait> = None;
  loop {
    match registry::send_control_or_return(target, message) {
      Ok(()) => return true,
      Err(registry::RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => message = unsent,
      Err(_) => return false,
    }
    waiting.get_or_insert_with(RoomWait::begin);
    let left = deadline.saturating_sub(slates_rt::futures::now_ns());
    if left == 0 {
      return false;
    }
    if slates_rt::futures::sleep(left.min(REPLY_PACE_NS))
      .await
      .is_err()
    {
      state::with_state_counted(|s| s.count(ROOM_PAUSE_REFUSED, 1));
      return false;
    }
  }
}

/// Counter: a scatter part's pause for room refused ([`given_room_within`]): only off a shard, so a tripwire.
pub(crate) const ROOM_PAUSE_REFUSED: &str = "xshard.room_pause_refused";

/// Shape: the pace of a reply's retries for room — a tenth of a coordinator period, the collection loops' cadence
/// (`crate::fleet::POLL_PER_PERIOD`), so a waiting reply asks about as often as the shard's loops turn.
const REPLY_PACE_NS: u64 = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;

/// Format: the status counter of waits for room this shard began ([`given_room`], [`given_room_within`]).
pub(crate) const ROOM_WAITS_STARTED: &str = "xshard.room_waits_started";
/// Format: the status counter of waits for room this shard ended, sent or not: with [`ROOM_WAITS_STARTED`], the waits
/// it holds now.
pub(crate) const ROOM_WAITS_ENDED: &str = "xshard.room_waits_ended";
/// Format: the status counter that rises each time this shard holds more waits for room at once than it ever had, by
/// the difference: its value is the most it has held at once.
pub(crate) const ROOM_WAITS_PEAK: &str = "xshard.room_waits_peak";

thread_local! {
  /// This shard thread's waits for room now, and the most it has held at once ([`RoomWait`]).
  static ROOM_WAITS: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// One wait for room in progress on this shard ([`given_room`], [`given_room_within`]): counted when it begins and,
/// dropped, when it ends,
/// however it ends (sent, the target gone, its task cancelled).
struct RoomWait;

impl RoomWait {
  fn begin() -> RoomWait {
    let (now, peak) = ROOM_WAITS.with(Cell::get);
    let now = now.saturating_add(1);
    ROOM_WAITS.with(|waits| waits.set((now, peak.max(now))));
    let raised = now.saturating_sub(peak);
    state::with_state_counted(|s| {
      s.count(ROOM_WAITS_STARTED, 1);
      if raised > 0 {
        s.count(ROOM_WAITS_PEAK, raised);
      }
    });
    RoomWait
  }
}

impl Drop for RoomWait {
  fn drop(&mut self) {
    ROOM_WAITS.with(|waits| {
      let (now, peak) = waits.get();
      waits.set((now.saturating_sub(1), peak));
    });
    state::with_state_counted(|s| s.count(ROOM_WAITS_ENDED, 1));
  }
}

/// Format: the status counter of results that could not be carried home because the shard they went to is gone
/// ([`send_back`]).
pub(crate) const REPLY_DROPPED: &str = "xshard.reply_dropped";

/// Runs `work` on `shard` and returns its result on this shard (`origin`). The closure borrows
/// shard state only as needed; the NFS dispatcher may take several short borrows. On the same shard
/// the closure runs directly. The spawn is refused typed at the target's admission
/// bound; await the result through [`call_within`], which bounds the wait.
pub fn call_on<T: Send + 'static>(
  origin: u16,
  shard: u16,
  work: impl FnOnce() -> Option<T> + Send + 'static,
) -> Result<CrossShardCall<T>, RtError> {
  if shard == origin {
    return Ok(CrossShardCall {
      target: shard,
      id: None,
      ready: Some(work()),
      _thread: PhantomData,
    });
  }
  let id = register();
  let task = SpawnRequest::new(
    Box::pin(async move {
      let result = work();
      let back = SpawnRequest::new(
        Box::pin(async move {
          deliver(id, Box::new(result));
        }),
        None,
      );
      send_back(origin, back).await;
    }),
    None,
  );
  match registry::send_control(shard, Control::Spawn(Box::new(task))) {
    Ok(()) => Ok(CrossShardCall {
      target: shard,
      id: Some(id),
      ready: None,
      _thread: PhantomData,
    }),
    Err(error) => {
      forget(id);
      Err(error)
    }
  }
}

/// Runs `work` on `shard` and awaits its result for at most `deadline_ns` of wall time; `None` if the call could not
/// be sent, the target's state was unavailable, or the deadline passed (the entry is forgotten, so a late result is
/// dropped rather than delivered to a caller that moved on). For a wait bounded by the caller's own clock (a coordinator
/// period, a client's remaining deadline); a wait for the target's answer while it lives is
/// [`call_in_shard_time`].
pub async fn call_within<T: Send + 'static>(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) -> T + Send + 'static,
  deadline_ns: u64,
) -> Option<T> {
  let Ok(call) = call_on(origin, shard, move || state::with_state(work)) else {
    return None;
  };
  within(call, deadline_ns).await
}

/// Runs `work` on `shard` and awaits its result while the target is working, up to `budget_ns` of the target's own
/// CPU time ([`within_shard_time`]); `None` as [`call_within`] answers, and when the target made no progress for a
/// window or ran its budget without answering.
pub async fn call_in_shard_time<T: Send + 'static>(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) -> T + Send + 'static,
  budget_ns: u64,
) -> Option<T> {
  let Ok(call) = call_on(origin, shard, move || state::with_state(work)) else {
    return None;
  };
  within_shard_time(call, budget_ns).await
}

/// Runs the future `start` makes as a task on `shard`, and awaits its result on this shard (`origin`) for at most
/// `deadline_ns` of wall time: the call for work that must yield on its shard as it goes (a write-back a copy slice a
/// turn, §4.3's bounded work, A-188), which [`call_within`]'s closure, run in one turn, cannot be. The future is made on
/// the target, so it may hold what is not `Send` across its turns; only its result crosses back. `None` as
/// [`call_within`] answers, and when the target's task arena refused the task. A late result is dropped, as
/// `call_within`'s is, while the task runs to its end on its shard. A call to this shard is the same task, spawned
/// here: it too runs to its end however long it takes, so dropping it never loses what the future holds mid-way
/// (§4.3). A wait for the target's answer while it lives is [`task_in_shard_time`].
pub async fn task_within<T, F>(
  origin: u16,
  shard: u16,
  start: impl FnOnce() -> F + Send + 'static,
  deadline_ns: u64,
) -> Option<T>
where
  T: Send + 'static,
  F: Future<Output = T> + 'static,
{
  within(task_on(origin, shard, start)?, deadline_ns).await
}

/// [`task_within`], awaited while the target is working, up to `budget_ns` of the target's own CPU time
/// ([`within_shard_time`]).
pub async fn task_in_shard_time<T, F>(
  origin: u16,
  shard: u16,
  start: impl FnOnce() -> F + Send + 'static,
  budget_ns: u64,
) -> Option<T>
where
  T: Send + 'static,
  F: Future<Output = T> + 'static,
{
  within_shard_time(task_on(origin, shard, start)?, budget_ns).await
}

/// Sends the task of a [`task_within`] to `shard` and returns the call its result arrives on; `None` when it could not
/// be sent or this shard's task arena refused it.
fn task_on<T, F>(
  origin: u16,
  shard: u16,
  start: impl FnOnce() -> F + Send + 'static,
) -> Option<CrossShardCall<T>>
where
  T: Send + 'static,
  F: Future<Output = T> + 'static,
{
  let id = register();
  let call = CrossShardCall {
    target: shard,
    id: Some(id),
    ready: None,
    _thread: PhantomData,
  };
  if shard == origin {
    // A call to this shard is a task here, carrying its result home like any other: dropped at the deadline in place,
    // the future lost whatever it held mid-way (a live dispatch's borrowed sessions, which were never returned).
    // Dropping `call` forgets the registration.
    return spawn_carrying_home(origin, id, start).then_some(call);
  }
  let task = SpawnRequest::new(
    Box::pin(async move {
      if !spawn_carrying_home(origin, id, start) {
        let back = SpawnRequest::new(
          Box::pin(async move {
            deliver(id, Box::new(None::<T>));
          }),
          None,
        );
        send_back(origin, back).await;
      }
    }),
    None,
  );
  registry::send_control(shard, Control::Spawn(Box::new(task)))
    .is_ok()
    .then_some(call)
}

/// On the target of a [`task_within`]: spawns the future `start` makes as a task of this shard, which carries its result
/// home to `origin` as call `id` when it ends. Whether the shard's task arena took it. Kept out of the task that called
/// it, so that task holds nothing but `Send` values across its own await.
fn spawn_carrying_home<T, F>(origin: u16, id: u64, start: impl FnOnce() -> F + 'static) -> bool
where
  T: Send + 'static,
  F: Future<Output = T> + 'static,
{
  let work = async move {
    let result = start().await;
    let back = SpawnRequest::new(
      Box::pin(async move {
        deliver(id, Box::new(Some(result)));
      }),
      None,
    );
    send_back(origin, back).await;
  };
  slates_rt::futures::spawn(work)
    .and_then(slates_rt::futures::detach)
    .is_ok()
}

/// Awaits a registered call within the caller's remaining time budget (§4.3): its answer, or `None` when
/// the call failed, the deadline passed, or no deadline could be armed (off a shard — never read as time
/// passing, AUD-29-39). Dropping this future cancels its registration even when neither the call nor its
/// deadline has completed.
pub async fn within<T: 'static>(call: CrossShardCall<T>, deadline_ns: u64) -> Option<T> {
  slates_rt::futures::within(deadline_ns, call)
    .await
    .ok()
    .flatten()
    .flatten()
}

/// Awaits a registered call in its target shard's own time (§4.3, §4.14, A-65; the R3 debt of `LIVENESS_BUDGET_NS`,
/// class A): a wall window of `budget_ns` first, then, while the target made CPU progress in the window just ended,
/// another for the part of `budget_ns` of CPU it has not yet run ([`crate::observe::ShardTime`]). A starved target,
/// runnable but given little CPU, is waited for until it answers; a wedged one, which runs nothing, is given up at the
/// end of the first whole window it spends off the CPU; a busy one that never answers, once it has run `budget_ns` of
/// CPU, which an uncontended shard does at the wall clock's pace. A target whose clock cannot be read (no registry slot,
/// or Miri) is waited for the one wall window. `None` as [`within`] answers. Measured: a bootstrap from a client on
/// the second shard while the control shard ran one poll of 1.5 s on the CPU a fifth of the time was refused
/// `Overloaded` after 1.003 s on main (2026-11-06, `crates/server/tests/cross_shard_wait.rs`).
pub async fn within_shard_time<T: 'static>(
  mut call: CrossShardCall<T>,
  budget_ns: u64,
) -> Option<T> {
  let mut time = registry::holder_of(call.target)
    .map(|holder| crate::observe::ShardTime::new(holder, budget_ns));
  let mut window_ns = budget_ns;
  loop {
    match slates_rt::futures::within(window_ns, &mut call).await {
      Ok(Some(answer)) => return answer,
      // The window ended with no answer: another while the target is working, else given up (dropping the call
      // forgets its registration, so a late answer is dropped).
      Ok(None) => window_ns = time.as_mut()?.next_window_ns()?,
      Err(_) => return None,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the bounded wait the starved-shard test gives a scatter part's room: one coordinator period.
  const REPLY_ROOM_NS: u64 = crate::daemon::HEARTBEAT_NS;

  /// A result carried home waits for room (2026-10-07). Do: on a shard, register a stand-in slot whose control channel
  /// holds one message, fill it, and spawn a task that sends a result home to it with `send_back`; from the test
  /// thread, wait two reply paces, then take the filler off the channel. Expect: the result's message arrives after
  /// the filler, and nothing was counted dropped. Before, the full channel dropped it at once.
  #[test]
  // The test thread stands for the origin shard draining later: it must wait off-shard while the reply retries.
  #[allow(clippy::disallowed_methods)]
  fn a_result_sent_home_to_a_full_channel_waits_for_room() {
    let daemon = crate::daemon::audit_daemon();
    let (registration, receiver) = crate::daemon::observe_first(&daemon, |_state| {
      let (registration, receiver) = registry::register(
        4,
        1,
        registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
      )
      .unwrap();
      let home = registration.shard();
      let filler = SpawnRequest::new(Box::pin(async {}), None);
      registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
      let back = SpawnRequest::new(Box::pin(async {}), None);
      let task = slates_rt::futures::spawn(send_back(home, back)).unwrap();
      slates_rt::futures::detach(task).unwrap();
      (registration, receiver)
    });
    // The filler occupies the one slot, so the reply behind it can only land once this takes it. Wait for the
    // filler within the same bound the reply's own wait uses, rather than assuming two pacing intervals are
    // enough: on a loaded shared runner they are not, and this failed with the daemon behaving correctly
    // (CI run 39073360651, macOS lane). The assertion that matters is the next one — the reply arrived once
    // there was room — and it keeps its bound.
    let room = std::time::Duration::from_nanos(REPLY_ROOM_NS.saturating_add(REPLY_PACE_NS));
    assert!(receiver.recv_timeout(room).is_ok(), "the filler");
    let carried = receiver.recv_timeout(room);
    assert!(
      carried.is_ok(),
      "the result's message arrived once there was room"
    );
    drop(registration);
    daemon.stop();
  }

  /// A reply handed home by a caller that cannot await waits for room too (§4.3 "backpressure, never a drop";
  /// [`hand_reply_home`], [`AnswerUnstarted`]). Do: on a shard, register a stand-in slot whose control channel holds one
  /// message, fill it, and hand a reply home to it with `hand_reply_home`, from the shard's state as its callers do; from
  /// the test thread, take the filler off the channel. Expect: the hand-off reported no refusal, and the reply's message
  /// arrives after the filler. Handed to the channel once, as until 2026-11-07, or dropped on a full channel, nothing
  /// arrives.
  #[test]
  // The test thread stands for the client's shard draining later: it must wait off-shard while the reply waits.
  #[allow(clippy::disallowed_methods)]
  fn a_reply_handed_home_to_a_full_channel_waits_for_room() {
    let daemon = crate::daemon::audit_daemon();
    let (registration, receiver, handed) = crate::daemon::observe_first(&daemon, |_state| {
      let (registration, receiver) = registry::register(
        4,
        1,
        registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
      )
      .unwrap();
      let home = registration.shard();
      let filler = SpawnRequest::new(Box::pin(async {}), None);
      registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
      let handed = hand_reply_home(home, || {});
      (registration, receiver, handed)
    });
    assert!(handed.is_ok(), "the hand-off was refused: {handed:?}");
    let room = std::time::Duration::from_nanos(REPLY_ROOM_NS.saturating_add(REPLY_PACE_NS));
    assert!(receiver.recv_timeout(room).is_ok(), "the filler");
    assert!(
      receiver.recv_timeout(room).is_ok(),
      "the reply's message arrived once there was room"
    );
    drop(registration);
    daemon.stop();
  }

  /// §4.3 cancellation safety, the same on every shard (R8). Do: on a shard, from a task there, make a `task_within`
  /// call to that same shard whose future parks past the call's deadline and then reports that it ended. Expect the
  /// call answered `None` at its deadline and the future run to its end anyway, as a call to another shard runs. Until
  /// 2026-11-06 a call to its own shard dropped the future at the deadline: a live dispatch on the control shard
  /// (`live_ship::send_from_control`) was dropped mid-exchange, the fleet sessions it had borrowed were never returned
  /// nor their links dropped, and an owner sent to no candidate again (`crates/cli/tests/live_chaos.rs`, the rolling
  /// partition: both of the owner's sessions `lent=true` for good, a write never answered).
  #[test]
  // The test thread waits off-shard for what the shard's task reports.
  #[allow(clippy::disallowed_methods)]
  fn a_call_to_its_own_shard_runs_its_future_to_the_end_past_the_deadline() {
    /// Shape: the call's deadline, a few timer ticks.
    const DEADLINE_NS: u64 = 5_000_000;
    /// Shape: how long the future parks: well past the deadline.
    const PARKED_NS: u64 = 50_000_000;
    /// Shape: how long the test thread waits for each report.
    const REPORT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
    let daemon = crate::daemon::audit_daemon();
    let (ended, ended_seen) = std::sync::mpsc::sync_channel::<()>(1);
    let (answered, answer_seen) = std::sync::mpsc::sync_channel::<bool>(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let home = state.shard;
      let caller = slates_rt::futures::spawn(async move {
        let answer = task_within(
          home,
          home,
          move || async move {
            let _ = slates_rt::futures::sleep(PARKED_NS).await;
            let _ = ended.try_send(());
          },
          DEADLINE_NS,
        )
        .await;
        let _ = answered.try_send(answer.is_some());
      })
      .unwrap();
      slates_rt::futures::detach(caller).unwrap();
    });
    assert_eq!(
      answer_seen.recv_timeout(REPORT_WAIT),
      Ok(false),
      "the call answered None at its deadline"
    );
    assert!(
      ended_seen.recv_timeout(REPORT_WAIT).is_ok(),
      "the future ran to its end past the call's deadline"
    );
    daemon.stop();
  }

  /// A bounded wait for room (`given_room_within`, a scatter part's) is a span of time on a busy shard too (2026-11-06).
  /// Do: on a shard, fill a stand-in slot's one-message channel and never drain it; keep the shard busy 20 ms at a time
  /// back to back with a task that spins and yields; and wait for room there within one coordinator period. Expect: the wait gives up about one
  /// coordinator period after it began, not much more. Before, it was a count of tries of one pause each, and each try
  /// lasted a pause plus the shard's busy poll.
  #[test]
  // The test thread waits for the result off the shard.
  #[allow(clippy::disallowed_methods)]
  fn a_bounded_wait_for_room_waits_its_period_in_time_on_a_starved_shard() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    /// Shape: how long the starver keeps the shard busy at a time: twice the room wait's pause.
    const STARVED_FOR_NS: u64 = 2 * REPLY_PACE_NS;
    static DONE: AtomicBool = AtomicBool::new(false);
    static WAITED_NS: AtomicU64 = AtomicU64::new(0);
    static SENT: AtomicBool = AtomicBool::new(true);
    static LONGEST_POLL_NS: AtomicU64 = AtomicU64::new(0);
    DONE.store(false, Ordering::Release);
    LONGEST_POLL_NS.store(0, Ordering::Release);
    let daemon = crate::daemon::audit_daemon();
    let (registration, receiver) = crate::daemon::observe_first(&daemon, |_state| {
      let (registration, receiver) = registry::register(
        4,
        1,
        registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
      )
      .unwrap();
      let home = registration.shard();
      let filler = SpawnRequest::new(Box::pin(async {}), None);
      registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
      let starver = slates_rt::futures::spawn(async {
        while !DONE.load(Ordering::Acquire) {
          // A busy poll of a fixed length: a spin, since a short sleep on macOS under a launchd job's timer policy
          // comes back several times late (CLAUDE.md, the loop-responsiveness gotcha).
          let began = std::time::Instant::now();
          while began.elapsed() < std::time::Duration::from_nanos(STARVED_FOR_NS) {
            std::hint::spin_loop();
          }
          // How long the poll really held the shard, preemption by the host included: the wait's overshoot past its
          // deadline is at most the poll under way then, so the bound is measured, not assumed.
          let held = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
          LONGEST_POLL_NS.fetch_max(held, Ordering::AcqRel);
          slates_rt::futures::yield_now().await;
        }
      })
      .unwrap();
      slates_rt::futures::detach(starver).unwrap();
      let waiter = slates_rt::futures::spawn(async move {
        let began = slates_rt::futures::now_ns();
        let sent = given_room_within(
          home,
          Control::Spawn(Box::new(SpawnRequest::new(Box::pin(async {}), None))),
          REPLY_ROOM_NS,
        )
        .await;
        WAITED_NS.store(
          slates_rt::futures::now_ns().saturating_sub(began),
          Ordering::Release,
        );
        SENT.store(sent, Ordering::Release);
        DONE.store(true, Ordering::Release);
      })
      .unwrap();
      slates_rt::futures::detach(waiter).unwrap();
      (registration, receiver)
    });
    let started = std::time::Instant::now();
    while !DONE.load(Ordering::Acquire) {
      assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the wait ended"
      );
      std::thread::sleep(std::time::Duration::from_nanos(REPLY_PACE_NS));
    }
    let waited = WAITED_NS.load(Ordering::Acquire);
    let longest = LONGEST_POLL_NS.load(Ordering::Acquire);
    // The budget, then at most the busy poll under way at the deadline and the waiter's own next turn behind another,
    // and one pause: a deadline overshoots by that, where a count of tries overshoots by a poll for every try.
    let bound = REPLY_ROOM_NS + 2 * longest + REPLY_PACE_NS;
    println!(
      "a result's wait for room on a busy shard: {waited} ns (longest busy poll {longest} ns, bound {bound} ns)"
    );
    assert!(!SENT.load(Ordering::Acquire), "the channel never had room");
    assert!(
      waited < bound,
      "the wait for room took {waited} ns on a busy shard, against a {REPLY_ROOM_NS} ns budget and a {longest} ns \
       longest poll"
    );
    drop(receiver);
    drop(registration);
    daemon.stop();
  }

  struct Released(std::sync::mpsc::SyncSender<()>);

  impl Drop for Released {
    fn drop(&mut self) {
      let _ = self.0.try_send(());
    }
  }

  /// T-0.3, §4.3 cancellation; AUD-17: drop a pending call before or after its reply arrives.
  /// Expect the reply's owned resource to be released, including when a late delivery races cancellation.
  #[test]
  fn cancelling_a_call_releases_its_reply_before_or_after_delivery() {
    for delivered_first in [false, true] {
      let id = register();
      let call = CrossShardCall::<Released> {
        target: 0,
        id: Some(id),
        ready: None,
        _thread: PhantomData,
      };
      let (sent, received) = std::sync::mpsc::sync_channel(1);
      let reply = Box::new(Some(Released(sent)));
      if delivered_first {
        deliver(id, reply);
        drop(call);
      } else {
        drop(call);
        deliver(id, reply);
      }
      assert_eq!(
        received.try_recv(),
        Ok(()),
        "cancelled call retained the reply; delivered first: {delivered_first}"
      );
    }
  }
}

/// A guard moved into a task that owes a client its reply: dropped before the task's first poll, it answers the client
/// with `reply` instead (§4.9: a forwarded verb is waited for while the daemon lives, so a task lost before it ran left
/// its client waiting for ever). A shard refuses a spawn whose arena is full at admission, and then drops the task's
/// future unrun with only its own `tasks_refused` count to show for it; `registry::send_control` reports only that the
/// channel took the message. Found by fixtures' 1,000-client replica-kill rung (2026-11-07): the client node's control
/// shard counted `tasks_refused` 117, and 87 forwards handed to it never started, their clients stalled for good
/// (`fleet.forward.client.to_control` 85,750 against `control_started` 85,663, every later hop balanced).
///
/// `at` is the shard the task runs on and `home` the client's shard. Dropped on `at` unstarted, it delivers `reply`
/// at once when `at` is `home`, else sends it home on a task carrying its own guard, so a reply refused at the client's
/// shard is delivered there directly. Dropped anywhere else, its sender's refused send answers the client already, and
/// it does nothing. `reply` is one the client may retry under its own id (`Overloaded`): the verb never ran here.
pub(crate) struct AnswerUnstarted {
  home: u16,
  at: u16,
  client_index: u32,
  request: u64,
  reply: Option<slates_ipc::protocol::ReplyBody>,
}

impl AnswerUnstarted {
  /// A guard for a task on shard `at` owing client `client_index`'s `request` on shard `home`.
  pub(crate) fn new(
    home: u16,
    at: u16,
    client_index: u32,
    request: u64,
    reply: slates_ipc::protocol::ReplyBody,
  ) -> Self {
    Self {
      home,
      at,
      client_index,
      request,
      reply: Some(reply),
    }
  }

  /// The task started, and answers its client itself.
  pub(crate) fn started(&mut self) {
    self.reply = None;
  }
}

impl Drop for AnswerUnstarted {
  fn drop(&mut self) {
    // A send its sender saw refused: the sender answers the client on its own refusal path.
    if DISCARDING_UNSENT.with(Cell::get) {
      return;
    }
    let Some(reply) = self.reply.take() else {
      return;
    };
    if state::with_state(|s| s.shard) != Some(self.at) {
      return;
    }
    state::with_state_counted(|s| s.count(UNSTARTED_ANSWERED, 1));
    let (home, client_index, request) = (self.home, self.client_index, self.request);
    if home == self.at {
      // The completion is the executing owner's, or nobody's: the refusal is never recorded here.
      state::deliver(client_index, request, reply, true);
      return;
    }
    // Carried home as [`carry_home`] carries a reply: this shard's arena refused the task the guard was in, so the
    // reply goes to the client's shard's channel needing nothing of it, and only waits on a task here for room there.
    if carry_home(home, move || {
      state::deliver(client_index, request, reply, true);
    })
    .is_err()
    {
      state::with_state_counted(|s| s.count(UNSTARTED_UNANSWERED, 1));
    }
  }
}

thread_local! {
  /// Set while [`send_owing`] drops a task its target's channel refused, so the task's [`AnswerUnstarted`] stands
  /// down: the sender answers that client itself (a client on the control shard forwarding through it would otherwise
  /// be answered twice).
  static DISCARDING_UNSENT: Cell<bool> = const { Cell::new(false) };
}

/// Sends `task`, which carries an [`AnswerUnstarted`], to `target`'s control channel: the channel's refusal is returned
/// for the sender to answer its client, and the task is dropped without its guard answering too. Once accepted, a task
/// dropped unrun (refused at admission) is answered by its guard.
pub(crate) fn send_owing(target: u16, task: SpawnRequest) -> Result<(), RtError> {
  match registry::send_control_or_return(target, Control::Spawn(Box::new(task))) {
    Ok(()) => Ok(()),
    Err(refused) => {
      DISCARDING_UNSENT.with(|discarding| discarding.set(true));
      drop(refused.message);
      DISCARDING_UNSENT.with(|discarding| discarding.set(false));
      Err(refused.error)
    }
  }
}

/// Counter: a task owing a client its reply that was dropped before it ran (its spawn refused at admission, its shard
/// shutting down), its client answered instead ([`AnswerUnstarted`]).
pub(crate) const UNSTARTED_ANSWERED: &str = "xshard.unstarted_answered";
/// Counter: such a task whose answer could not be sent home either (the client's shard's channel refused it).
pub(crate) const UNSTARTED_UNANSWERED: &str = "xshard.unstarted_unanswered";

/// The synchronous end of a reply carried home ([`send_reply_home`]): run on the client's shard by its task, or, when
/// that shard drops the task unrun (its arena full at admission), run there by the drop. Dropped anywhere else (its
/// sender's wait for room ran out, counted `xshard.reply_dropped`), it does nothing.
struct RunUnstarted {
  at: u16,
  body: Option<Box<dyn FnOnce() + Send>>,
}

impl RunUnstarted {
  /// Runs the body now (the task started), once.
  fn run(&mut self) {
    if let Some(body) = self.body.take() {
      body();
    }
  }
}

impl Drop for RunUnstarted {
  fn drop(&mut self) {
    if DISCARDING_UNSENT.with(Cell::get) || self.body.is_none() {
      return;
    }
    if state::with_state(|s| s.shard) != Some(self.at) {
      return;
    }
    state::with_state_counted(|s| s.count(REPLY_RUN_UNSTARTED, 1));
    self.run();
  }
}

/// Carries a reply home to `origin`, the client's shard, as [`send_back`] carries a result: `body` is the reply's
/// synchronous end there (writing it into the client's ring, with what goes with it). It runs on `origin` whether its
/// arena admits the task or refuses it at admission, so a reply that reached the client's shard is never lost to a full
/// arena there. The sibling of [`AnswerUnstarted`] the supervisor ordered fixed with it (2026-11-07).
pub async fn send_reply_home(origin: u16, body: impl FnOnce() + Send + 'static) {
  let mut guard = RunUnstarted {
    at: origin,
    body: Some(Box::new(body)),
  };
  let back = SpawnRequest::new(
    Box::pin(async move {
      guard.run();
    }),
    None,
  );
  send_back(origin, back).await;
}

/// Counter: a reply carried home whose task the client's shard refused at admission, run there by its drop instead
/// ([`send_reply_home`]).
pub(crate) const REPLY_RUN_UNSTARTED: &str = "xshard.reply_run_unstarted";

/// [`send_reply_home`] for a caller that cannot wait for room (it holds its shard's state), as [`carry_home`] carries
/// it: the refusal is returned for the caller to count. Until 2026-11-09 `body` was handed to `target`'s channel once
/// and a full channel's refusal returned, so a reply met a full channel and was lost (CLAUDE.md §2 items 8 and 9).
pub(crate) fn hand_reply_home(
  target: u16,
  body: impl FnOnce() + Send + 'static,
) -> Result<(), RtError> {
  carry_home(target, body)
}

/// Carries a reply home to `origin` from a caller that cannot await, waiting for room as [`send_reply_home`] waits,
/// with the wait's first try made at once: `body` goes to `origin`'s control channel now, which needs nothing of this
/// shard, and only a full channel puts the wait on a task of this shard. So a reply is not lost to this shard's arena
/// when the channel has room (an [`AnswerUnstarted`] drops because its shard's arena is full; a task there for every
/// reply failed `a_forward_refused_at_admission_answers_its_client` 3 of 3), nor to a full channel while this shard can
/// run the wait. Returned: the refusal when neither could take it, `origin` gone or this shard's arena full with the
/// channel, for the caller to count; the reply is then dropped here, its guard standing down.
fn carry_home(origin: u16, body: impl FnOnce() + Send + 'static) -> Result<(), RtError> {
  let mut guard = RunUnstarted {
    at: origin,
    body: Some(Box::new(body)),
  };
  let back = SpawnRequest::new(
    Box::pin(async move {
      guard.run();
    }),
    None,
  );
  match registry::send_control_or_return(origin, Control::Spawn(Box::new(back))) {
    Ok(()) => Ok(()),
    Err(registry::RefusedControl {
      error: RtError::ControlFull { .. },
      message: Some(unsent),
    }) => discarding_unsent(|| {
      let waiting = slates_rt::futures::spawn(async move {
        if !given_room(origin, unsent).await {
          state::with_state_counted(|s| s.count(REPLY_DROPPED, 1));
        }
      })?;
      let _ = slates_rt::futures::detach(waiting);
      Ok(())
    }),
    Err(refused) => discarding_unsent(|| {
      drop(refused.message);
      Err(refused.error)
    }),
  }
}

/// Runs `drop_or_hand`, during which a reply guard dropped stands down ([`DISCARDING_UNSENT`]): the caller reports
/// the refusal.
fn discarding_unsent<T>(drop_or_hand: impl FnOnce() -> T) -> T {
  DISCARDING_UNSENT.with(|discarding| discarding.set(true));
  let result = drop_or_hand();
  DISCARDING_UNSENT.with(|discarding| discarding.set(false));
  result
}
