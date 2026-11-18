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
//!
//! What goes back is never refused for want of room (§4.3, goal 238, A-296): a reply, a scatter part's result or a
//! call's answer that meets the origin's full control channel is kept by the sending shard's carrier ([`carrier`]),
//! behind anything kept for the same shard, for as long as that shard lives. The carrier asks the full shard to tell it
//! when its channel next has room (A-279's room wake, `slates_rt::room`) and sends what it keeps when the drain that
//! made the room says so, with no timer. It needs no task per message, so neither a full task arena nor the runtime's
//! admission, where a task refused there answers its client, can refuse one; what it keeps answers requests in flight,
//! which bound it (`DaemonConfig::kept_bound`), and every wait is counted ([`room_waits`]). Measured on main 80be552a
//! before: 4,074 to 4,518 results a run of the 3,000-client load rung dropped after one coordinator period's wait. A
//! scatter's part on its way out waits for room the same way, within a budget ([`given_room_within`]).

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, VecDeque};
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

/// Sends `back`, the task that carries a result home to `origin` (a reply to its client, an awaited call's answer): at
/// once when `origin`'s control channel has room, else kept by this shard's carrier until it has ([`carry`]), for as
/// long as `origin` lives (§4.3: backpressure, never a drop). Only an origin gone (its client with it) leaves the result
/// undelivered, counted on this shard (`xshard.reply_dropped`). The caller holds nothing while the result waits.
///
/// History: before 2026-10-07 every refusal dropped the result at once and unseen. Until 2026-11-18 one still refused
/// after a coordinator period was dropped: about 4,300 a run of the 3,000-client rung on main, every one an
/// acknowledgement scatter's part. The merge of agents/perf-replies that day made each wait in its own task while its
/// origin lived, which a full arena could refuse; the carrier needs no task of its own.
pub fn send_back(origin: u16, back: SpawnRequest) {
  if carry(origin, Control::Spawn(Box::new(back))).is_err() {
    state::with_state_counted(|s| s.count(REPLY_DROPPED, 1));
  }
}

/// Sends `message` to `target` at once, or keeps it for this shard's carrier ([`carrier`]) when `target`'s control
/// channel is full, or when messages for `target` are kept already, so they reach it in the order they were sent. A kept
/// message waits for room for as long as `target` lives. Nothing is spawned for it, so neither this shard's task arena
/// nor a borrow of its runtime can refuse it: a task refused at admission is dropped inside the runtime's admission,
/// where no task can be spawned, and its guard answers its client through here ([`AnswerUnstarted`]).
///
/// Refused, with the message dropped and every reply guard in it standing down, so the caller counts the loss: `target`
/// gone; or a full channel while no carrier runs (before the shard's start spawns it, after its shutdown), or with the
/// most messages kept that [`crate::config::DaemonConfig::kept_bound`] allows, which the requests in flight never reach
/// (counted `xshard.keep_refused`).
///
/// **What bounds the waiting (CLAUDE.md §2 item 8).** A kept message is a reply or a result another shard waits for: a
/// client's reply (each client holds at most its region's `slots` of credit), an acknowledgement part's result (at most
/// `ack_scatter_bound` scatters a shard), or what a task awaits or sends (a call's result, a connection's end). So
/// what a shard keeps is at most what the daemon has in flight, and the derived bound is that sum. Every kept message is
/// counted as a wait for room ([`room_waits`]).
fn carry(target: u16, message: Control) -> Result<(), RtError> {
  let behind = KEPT.with(|kept| {
    kept.try_borrow().is_ok_and(|kept| {
      kept
        .queues
        .get(&target)
        .is_some_and(|queue| !queue.is_empty())
    })
  });
  let unsent = if behind {
    message
  } else {
    match registry::send_control_or_return(target, message) {
      Ok(()) => return Ok(()),
      Err(registry::RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => unsent,
      Err(refused) => {
        discarding_unsent(|| drop(refused.message));
        return Err(refused.error);
      }
    }
  };
  match keep(target, unsent) {
    Ok(carrier) => {
      if let Some(carrier) = carrier {
        carrier.wake();
      }
      Ok(())
    }
    Err(unkept) => {
      discarding_unsent(|| drop(unkept));
      Err(RtError::ControlFull { shard: target })
    }
  }
}

/// Puts `message` behind what this shard keeps for `target`, counted as a wait for room: the carrier's waker, for the
/// caller to wake once the keeper is let go, when this is the first message kept for `target` and the carrier waits
/// (it must ask `target` for room, [`asked_for_room`]). A message kept behind others needs no wake: every poll of the
/// carrier that leaves messages kept for `target` asks it, and the first one's wake brought such a poll. The message
/// back, unkept, when no carrier runs, the bound is reached (both counted `xshard.keep_refused`), or the keeper is
/// borrowed, which nothing allows: the keeper is borrowed only inside this module's functions, and none of them keeps
/// while it holds the borrow.
fn keep(target: u16, message: Control) -> Result<Option<Waker>, Control> {
  KEPT.with(|kept| {
    let Ok(mut kept) = kept.try_borrow_mut() else {
      return Err(message);
    };
    let Some(bound) = kept.bound else {
      kept.refused = kept.refused.saturating_add(1);
      return Err(message);
    };
    if kept.held >= bound {
      kept.refused = kept.refused.saturating_add(1);
      return Err(message);
    }
    kept.held = kept.held.saturating_add(1);
    let queue = kept.queues.entry(target).or_default();
    let first = queue.is_empty();
    queue.push_back((message, RoomWait::begin()));
    Ok(if first { kept.carrier.take() } else { None })
  })
}

/// Sends what this shard keeps, each target's messages in order until its channel is full: the targets whose channels
/// are still full, which the carrier asks for room. What was kept for a target that is gone is let go of
/// ([`let_go_of`]): its clients went with it.
fn flush_kept() -> Vec<u16> {
  let mut gone = Vec::new();
  let full = KEPT.with(|kept| {
    // Never borrowed here: the keeper is borrowed only inside this module's functions, and none of them keeps while it
    // holds the borrow (a message let go of is dropped after the borrow ends).
    let Ok(mut kept) = kept.try_borrow_mut() else {
      return Vec::new();
    };
    for (target, queue) in &mut kept.queues {
      flush_queue(*target, queue, &mut gone);
    }
    kept.queues.retain(|_, queue| !queue.is_empty());
    kept.held = kept.queues.values().map(VecDeque::len).sum();
    kept.queues.keys().copied().collect()
  });
  let_go_of(gone);
  full
}

/// Sends what this shard keeps for `target`, in order until its channel is full: the carrier's second try, after it
/// asked `target` for room. A target gone lets go of it ([`let_go_of`]).
fn flush_kept_for(target: u16) {
  let mut gone = Vec::new();
  KEPT.with(|kept| {
    let Ok(mut kept) = kept.try_borrow_mut() else {
      return;
    };
    let Some(queue) = kept.queues.get_mut(&target) else {
      return;
    };
    flush_queue(target, queue, &mut gone);
    if queue.is_empty() {
      kept.queues.remove(&target);
    }
    kept.held = kept.queues.values().map(VecDeque::len).sum();
  });
  let_go_of(gone);
}

/// Takes what this shard keeps for `target` out of the keeper, for [`let_go_of`]: `target` has left its loop and will
/// never drain its channel again.
fn take_kept_for(target: u16) -> Vec<(Option<Control>, RoomWait)> {
  KEPT.with(|kept| {
    kept.try_borrow_mut().map_or_else(
      |_| Vec::new(),
      |mut kept| {
        let queue = kept.queues.remove(&target).unwrap_or_default();
        kept.held = kept.held.saturating_sub(queue.len());
        queue
          .into_iter()
          .map(|(message, wait)| (Some(message), wait))
          .collect()
      },
    )
  })
}

/// Asks `target`, whose control channel refused a send, to tell this shard when that channel next has room, for the task
/// polled now (`slates_rt::futures::want_room`; A-279's room wake, `slates_rt::room`). The caller then tries its send
/// once more and waits only if that try is refused too: either the try finds the room a drain made meanwhile, or that
/// drain finds the ask and wakes the task. `false` when `target` has left its loop, so it will never drain again. An ask
/// refused otherwise is counted (`xshard.room_wait_refused`) and answers `true`, so the try is made all the same: only a
/// task's poll on a shard can ask, which every caller is, so it is a tripwire.
fn asked_for_room(target: u16) -> bool {
  match slates_rt::futures::want_room(target) {
    Ok(()) => true,
    Err(RtError::ShardGone { .. }) => false,
    Err(_) => {
      state::with_state_counted(|s| s.count(ROOM_WAIT_REFUSED, 1));
      true
    }
  }
}

/// Sends `queue`'s messages to `target` in order until its channel is full (the rest stay, the first refused at the
/// front); a target gone moves them all to `gone`. A sent message's wait ends as its [`RoomWait`] drops.
fn flush_queue(
  target: u16,
  queue: &mut VecDeque<(Control, RoomWait)>,
  gone: &mut Vec<(Option<Control>, RoomWait)>,
) {
  while let Some((message, wait)) = queue.pop_front() {
    match registry::send_control_or_return(target, message) {
      Ok(()) => {}
      Err(registry::RefusedControl {
        error: RtError::ControlFull { .. },
        message: Some(unsent),
      }) => {
        queue.push_front((unsent, wait));
        return;
      }
      Err(refused) => {
        gone.push((refused.message, wait));
        gone.extend(queue.drain(..).map(|(message, wait)| (Some(message), wait)));
        return;
      }
    }
  }
}

/// Lets go of messages that can no longer be carried (their shard gone, or this shard's carrier ended): each reply
/// guard in them stands down, and they are counted (`xshard.reply_dropped`).
fn let_go_of(gone: Vec<(Option<Control>, RoomWait)>) {
  if gone.is_empty() {
    return;
  }
  let dropped = u64::try_from(gone.len()).unwrap_or(u64::MAX);
  discarding_unsent(|| drop(gone));
  state::with_state_counted(|s| s.count(REPLY_DROPPED, dropped));
}

/// What this shard keeps for other shards whose control channels were full ([`carry`]), and its carrier's hold on it.
struct Kept {
  /// By target shard, in the order they were kept, each with its wait for room.
  queues: BTreeMap<u16, VecDeque<(Control, RoomWait)>>,
  /// How many are kept, in all the queues.
  held: usize,
  /// The most that may be kept at once (`DaemonConfig::kept_bound`), set while a carrier runs; `None` before its start
  /// and after its end, when nothing is kept and a full channel is refused to the sender.
  bound: Option<usize>,
  /// The carrier's waker while it waits, taken by the next keep.
  carrier: Option<Waker>,
  /// Messages refused for want of a carrier or past the bound (`xshard.keep_refused`).
  refused: u64,
}

impl Kept {
  const fn new() -> Kept {
    Kept {
      queues: BTreeMap::new(),
      held: 0,
      bound: None,
      carrier: None,
      refused: 0,
    }
  }
}

impl Drop for Kept {
  fn drop(&mut self) {
    // At the thread's end, a message still kept (none once the carrier has ended, which lets go of them first) must
    // not reach for the shard's state, which may be gone by then: every reply guard in it stands down. A flag already
    // gone reads as standing down to the guards.
    let _ = DISCARDING_UNSENT.try_with(|discarding| discarding.set(true));
  }
}

thread_local! {
  /// What this shard keeps for other shards ([`Kept`]); only this shard's thread touches it, so it needs no lock (D-7).
  static KEPT: RefCell<Kept> = const { RefCell::new(Kept::new()) };
}

/// The carrier: sends what this shard keeps for other shards whose control channels were full ([`carry`]), each
/// target's messages in order. A target whose channel is still full is asked to tell this shard when it next has room
/// ([`asked_for_room`], A-279's room wake) and tried once more; then the carrier waits, with no timer, until a target it
/// asked drains its channel, which wakes it here, or the first message is kept for a target it has not asked. A target
/// that has left its loop is let go of, with what is kept for it. Each shard's start spawns the carrier before anything
/// else (`crate::daemon`), so nothing the shard sends is without it; `bound` is the most it keeps at once
/// ([`crate::config::DaemonConfig::kept_bound`]). Dropped at the shard's shutdown, it lets go of what it keeps (counted
/// `xshard.reply_dropped`: the clients go with the daemon), and the shard keeps nothing more.
pub(crate) fn carrier(bound: usize) -> Carrier {
  KEPT.with(|kept| {
    if let Ok(mut kept) = kept.try_borrow_mut() {
      kept.bound = Some(bound);
    }
  });
  Carrier {
    _running: CarrierRunning,
  }
}

/// The carrier's future ([`carrier`]).
pub(crate) struct Carrier {
  /// Lets go of what is kept when the carrier is dropped.
  _running: CarrierRunning,
}

impl Future for Carrier {
  type Output = ();

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
    for target in flush_kept() {
      if asked_for_room(target) {
        flush_kept_for(target);
      } else {
        let_go_of(take_kept_for(target));
      }
    }
    // What is still kept waits for its target's drain, which wakes this task; the first message kept for another
    // target wakes it through this waker.
    wait_for_keep(cx.waker());
    Poll::Pending
  }
}

/// Leaves the carrier's waker for the next keep ([`keep`]).
fn wait_for_keep(waker: &Waker) {
  KEPT.with(|kept| {
    if let Ok(mut kept) = kept.try_borrow_mut() {
      kept.carrier = Some(waker.clone());
    }
  });
}

/// The carrier's hold on this shard's keeper ([`carrier`]): dropped with the carrier, it lets go of what is kept, each
/// reply guard in it standing down, and the shard keeps nothing more.
struct CarrierRunning;

impl Drop for CarrierRunning {
  fn drop(&mut self) {
    let kept_last: Vec<(Option<Control>, RoomWait)> = KEPT
      .try_with(|kept| {
        kept.try_borrow_mut().map_or_else(
          |_| Vec::new(),
          |mut kept| {
            kept.bound = None;
            kept.carrier = None;
            kept.held = 0;
            std::mem::take(&mut kept.queues)
              .into_values()
              .flatten()
              .map(|(message, wait)| (Some(message), wait))
              .collect()
          },
        )
      })
      .unwrap_or_default();
    let_go_of(kept_last);
  }
}

/// [`run_on`] for work whose loss would leave state behind on `shard` (a closed connection's byte states, A-192): sent
/// as a result carried home is ([`carry`]), kept while `shard`'s channel is full for as long as `shard` lives. Counted
/// under `refused` on this shard when the shard is gone, or the carrier could not keep it.
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
  if carry(shard, Control::Spawn(Box::new(task))).is_err() {
    state::with_state_counted(|s| s.count(refused, 1));
  }
}

/// Sends `message` to `target`, waiting out a full channel for at most `budget_ns` of time: whether it was sent. A full
/// channel's shard is asked to tell this shard when it next has room ([`asked_for_room`], A-279's room wake) and the
/// send tried once more; then the task waits for its first wake, the drain's tell or the deadline, and tries again.
/// `false` past the deadline, at once for a shard gone, or when the deadline cannot be armed (not a task's poll on a
/// shard, where no wait can succeed; counted `xshard.room_wait_refused`). A scatter's part waits this way
/// (`crate::verbs::send_part`), within a budget its client can still hear the answer in. The budget is a span of the
/// shard's clock, not a count of tries: until 2026-11-06 it was `budget / pace + 1` tries of one pause each, and on a
/// starved shard every pause ran late, so the wait stretched with the starvation (51 tries took 1.54 s against a
/// 500 ms budget with the shard kept off its CPU 20 ms at a time; `a_status_on_a_starved_shard_is_refused_overloaded_on_time`
/// in `crates/server/tests/ack_release.rs`). Until 2026-11-18 (A-296) it tried again every tenth of a coordinator
/// period, a timer each time, whether the channel had room or not.
pub(crate) async fn given_room_within(target: u16, mut message: Control, budget_ns: u64) -> bool {
  let deadline = slates_rt::futures::now_ns().saturating_add(budget_ns);
  let mut waiting: Option<RoomWait> = None;
  loop {
    message = match tried(target, message) {
      Tried::Full(unsent) => unsent,
      done => return matches!(done, Tried::Sent),
    };
    waiting.get_or_insert_with(RoomWait::begin);
    let left = deadline.saturating_sub(slates_rt::futures::now_ns());
    if left == 0 || !asked_for_room(target) {
      return false;
    }
    message = match tried(target, message) {
      Tried::Full(unsent) => unsent,
      done => return matches!(done, Tried::Sent),
    };
    let woken = slates_rt::futures::within(left, slates_rt::futures::idle()).await;
    if woken.is_err() {
      state::with_state_counted(|s| s.count(ROOM_WAIT_REFUSED, 1));
      return false;
    }
  }
}

/// One try of a send that waits for room ([`tried`]).
enum Tried {
  /// The channel took the message.
  Sent,
  /// The channel was full: the message back, for the next try.
  Full(Control),
  /// Refused otherwise (the target gone): the message dropped.
  Refused,
}

/// Tries to send `message` to `target` once ([`Tried`]).
fn tried(target: u16, message: Control) -> Tried {
  match registry::send_control_or_return(target, message) {
    Ok(()) => Tried::Sent,
    Err(registry::RefusedControl {
      error: RtError::ControlFull { .. },
      message: Some(unsent),
    }) => Tried::Full(unsent),
    Err(_) => Tried::Refused,
  }
}

/// Counter: a wait for room whose ask ([`asked_for_room`]) or deadline ([`given_room_within`]) was refused for want of
/// a task's poll on a shard, which every caller is: a tripwire. Until 2026-11-18 (A-296) its pause was counted here as
/// `xshard.room_pause_refused`.
pub(crate) const ROOM_WAIT_REFUSED: &str = "xshard.room_wait_refused";

/// Format: the status counter of waits for room this shard began: messages its carrier kept ([`carry`]) and scatter
/// parts that met a full channel ([`given_room_within`]).
pub(crate) const ROOM_WAITS_STARTED: &str = "xshard.room_waits_started";
/// Format: the status counter of waits for room this shard ended, sent or not: with [`ROOM_WAITS_STARTED`], the waits
/// it holds now.
pub(crate) const ROOM_WAITS_ENDED: &str = "xshard.room_waits_ended";
/// Format: the status counter of the most waits for room this shard has held at once.
pub(crate) const ROOM_WAITS_PEAK: &str = "xshard.room_waits_peak";
/// Format: the status counter of messages this shard's carrier refused to keep: none ran, or its bound was reached
/// ([`carry`]). Each is counted again as its sender's loss.
pub(crate) const KEEP_REFUSED: &str = "xshard.keep_refused";

/// This shard's waits for room ([`RoomWait`]): held now, the most held at once, and begun and ended in all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RoomWaits {
  /// Held now.
  pub now: u64,
  /// The most held at once.
  pub peak: u64,
  /// Begun in all.
  pub started: u64,
  /// Ended in all, sent or not.
  pub ended: u64,
}

thread_local! {
  /// This shard thread's waits for room ([`RoomWait`]). Per thread, so each shard reports its own, and never through
  /// the shard's state, which a caller that keeps a reply may be borrowing (`hand_reply_home`'s callers).
  static ROOM_WAITS: Cell<RoomWaits> = const {
    Cell::new(RoomWaits {
      now: 0,
      peak: 0,
      started: 0,
      ended: 0,
    })
  };
}

/// This shard thread's waits for room so far (in the shard's status as `xshard.room_waits_started`, `_ended` and
/// `_peak`).
pub fn room_waits() -> RoomWaits {
  ROOM_WAITS.with(Cell::get)
}

/// The messages this shard thread's carrier refused to keep so far ([`KEEP_REFUSED`]).
pub fn keep_refused() -> u64 {
  KEPT.with(|kept| kept.try_borrow().map_or(0, |kept| kept.refused))
}

/// One wait for room in progress on this shard: a message its carrier keeps ([`carry`]) or a scatter part's
/// ([`given_room_within`]). Counted when it begins and, dropped, when it ends, however it ends (sent, the target gone,
/// the carrier or the part's task ended).
struct RoomWait;

impl RoomWait {
  fn begin() -> RoomWait {
    ROOM_WAITS.with(|waits| {
      let mut counted = waits.get();
      counted.now = counted.now.saturating_add(1);
      counted.peak = counted.peak.max(counted.now);
      counted.started = counted.started.saturating_add(1);
      waits.set(counted);
    });
    RoomWait
  }
}

impl Drop for RoomWait {
  fn drop(&mut self) {
    // Through `try_with`: a wait still kept at the thread's end drops with the keeper, after which nothing reads this.
    let _ = ROOM_WAITS.try_with(|waits| {
      let mut counted = waits.get();
      counted.now = counted.now.saturating_sub(1);
      counted.ended = counted.ended.saturating_add(1);
      waits.set(counted);
    });
  }
}

/// Format: the status counter of results that could not be carried home: the shard they went to is gone ([`send_back`],
/// [`flush_kept`]), or this shard stopped while its carrier kept them.
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
      send_back(origin, back);
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

/// [`call_within`], waiting for room on the target's control channel within the same deadline rather than failing at
/// once when it is full: for a shard that many others ask at once (a green's owner, A-247), where a refused call is
/// asked again and a burst of them fills the channel for good.
pub async fn call_within_room<T: Send + 'static>(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) -> T + Send + 'static,
  deadline_ns: u64,
) -> Option<T> {
  if shard == origin {
    return state::with_state(work);
  }
  let started = slates_rt::futures::now_ns();
  let id = register();
  let task = SpawnRequest::new(
    Box::pin(async move {
      let result = state::with_state(work);
      let back = SpawnRequest::new(
        Box::pin(async move {
          deliver(id, Box::new(result));
        }),
        None,
      );
      send_back(origin, back);
    }),
    None,
  );
  if !given_room_within(shard, Control::Spawn(Box::new(task)), deadline_ns).await {
    forget(id);
    return None;
  }
  // The target delivers what `state::with_state` answered, an `Option<T>`, which the call's future reads as such.
  let call: CrossShardCall<T> = CrossShardCall {
    target: shard,
    id: Some(id),
    ready: None,
    _thread: PhantomData,
  };
  let left = deadline_ns.saturating_sub(slates_rt::futures::now_ns().saturating_sub(started));
  within(call, left).await
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
        send_back(origin, back);
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
    send_back(origin, back);
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

  /// Shape: how often the test thread looks at a flag a shard's task sets: a tenth of a coordinator period, the
  /// collection loops' cadence (`crate::fleet::POLL_PER_PERIOD`).
  const LOOK_EVERY_NS: u64 = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;

  /// A result carried home waits for room (2026-10-07). Do: on a shard, register a stand-in slot whose control channel
  /// holds one message, fill it, and send a result home to it with `send_back`; from the test thread, take the filler
  /// off the channel and tell the shards waiting for room, as a drain does. Expect: the result's message arrives after
  /// the filler. Before, the full channel dropped it at once; until 2026-11-18 it waited a coordinator period and was
  /// dropped after.
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
      send_back(home, back);
      (registration, receiver)
    });
    // The filler occupies the one slot, so the reply behind it can only land once this takes it. Both waits take the
    // daemon's budget for a slow shard: two pacing intervals were not enough on a loaded shared runner, which failed
    // this with the daemon behaving correctly (CI run 39073360651, macOS lane), and a reply that is lost never comes.
    assert!(take(&receiver, &registration).is_ok(), "the filler");
    let carried = take(&receiver, &registration);
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
  /// the test thread, take the filler off the channel and tell the shards waiting for room, as a drain does. Expect: the
  /// hand-off reported no refusal, and the reply's message arrives after the filler. Handed to the channel once, as
  /// until 2026-11-07, or dropped on a full channel, nothing arrives.
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
    assert!(take(&receiver, &registration).is_ok(), "the filler");
    assert!(
      take(&receiver, &registration).is_ok(),
      "the reply's message arrived once there was room"
    );
    drop(registration);
    daemon.stop();
  }

  /// Test support: fills this shard's task arena with tasks that park by yielding until `released` is set (no timer
  /// slot held, as `Daemon::fill_task_arena` parks its own), stopping at the first refusal or at `cap` tasks. Returns
  /// how many it spawned and whether the arena refused one, which is the fill's point.
  fn fill_this_arena(
    released: &'static std::sync::atomic::AtomicBool,
    cap: usize,
  ) -> (usize, bool) {
    for spawned in 0..cap {
      let filler = slates_rt::futures::spawn(async move {
        while !released.load(std::sync::atomic::Ordering::Acquire) {
          slates_rt::futures::yield_now().await;
        }
      });
      if filler.and_then(slates_rt::futures::detach).is_err() {
        return (spawned, true);
      }
    }
    (cap, false)
  }

  /// Shape: how long the test thread waits for a message on a stand-in slot's channel: the daemon's own budget for a
  /// slow shard's answer (`OBSERVE_BUDGET_NS`). A shard whose arena is full of fillers that yield polls each of them a
  /// step, so its steps run long; a message that is lost never comes, so no shorter bound is needed to tell it apart.
  const MESSAGE_WAIT: std::time::Duration =
    std::time::Duration::from_nanos(crate::daemon::OBSERVE_BUDGET_NS);

  /// Test support: takes the next message off a stand-in slot's channel within [`MESSAGE_WAIT`] and then tells the
  /// shards waiting for room in it, as a shard's drain that took a message tells them (`Registration::tell_room`,
  /// A-279). The test thread stands for the slot's shard draining, and a carrier waiting on it is moved by that word
  /// alone: it has no timer.
  fn take(
    receiver: &std::sync::mpsc::Receiver<Control>,
    registration: &registry::Registration,
  ) -> Result<Control, std::sync::mpsc::RecvTimeoutError> {
    let taken = receiver.recv_timeout(MESSAGE_WAIT);
    if taken.is_ok() {
      registration.tell_room();
    }
    taken
  }

  /// A reply handed home waits for room even while this shard's task arena is full (goal 238; §4.3, banned items 8 and
  /// 9). Do: on a shard, register a stand-in slot whose control channel holds one message and fill it; fill this
  /// shard's task arena with tasks that park by yielding; and hand a reply home to the stand-in with `hand_reply_home`,
  /// from the shard's state as its callers do. Expect: the hand-off reported no refusal, and once the filler is taken
  /// off the channel (the shards waiting for room told, as a drain tells them) the reply's message arrives. Until
  /// 2026-11-18 the wait for room was a task spawned on this shard, which the full arena refused: the reply was lost,
  /// counted by its caller.
  #[test]
  // The test thread stands for the client's shard draining later: it must wait off-shard while the reply waits.
  #[allow(clippy::disallowed_methods)]
  fn a_reply_handed_home_while_this_shards_arena_is_full_waits_for_room() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RELEASED: AtomicBool = AtomicBool::new(false);
    RELEASED.store(false, Ordering::Release);
    let daemon = crate::daemon::audit_daemon();
    let (registration, receiver, filled, handed) = crate::daemon::observe_first(&daemon, |state| {
      let cap = state.config.runtime.tasks_per_shard.saturating_mul(2);
      let (registration, receiver) = registry::register(
        4,
        1,
        registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
      )
      .unwrap();
      let home = registration.shard();
      let filler = SpawnRequest::new(Box::pin(async {}), None);
      registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
      let filled = fill_this_arena(&RELEASED, cap);
      let handed = hand_reply_home(home, || {});
      (registration, receiver, filled, handed)
    });
    let (fillers, refused) = filled;
    assert!(
      refused,
      "the arena refused a filler (the precondition): {fillers} spawned"
    );
    assert!(handed.is_ok(), "the hand-off was refused: {handed:?}");
    assert!(take(&receiver, &registration).is_ok(), "the filler");
    let carried = take(&receiver, &registration);
    RELEASED.store(true, Ordering::Release);
    assert!(
      carried.is_ok(),
      "the reply's message arrived once there was room, {fillers} fillers holding the arena"
    );
    drop(registration);
    daemon.stop();
  }

  /// A task refused at admission answers its client through a full channel (goal 238; §4.3, §4.9). Do: on a shard,
  /// register a stand-in slot for the client's shard whose control channel holds one message and fill it; then, from a
  /// task of this shard, fill its task arena with tasks that park by yielding, top it up after a turn, and send this
  /// shard's own control channel a task owing the client its answer ([`AnswerUnstarted`]). The shard refuses that task
  /// at admission and drops it inside the admission, as it drops every refused task, where nothing can be spawned. Once
  /// the dropped task's guard has met the full channel, take the filler off it and tell the shards waiting for room, as a
  /// drain does. Expect: the owing task never ran, and the answer's message arrives. Until 2026-11-18 the answer waited on a task spawned there, which the full arena and the
  /// admission itself refused: lost, counted `xshard.unstarted_unanswered`, and its client waited while the daemon lived
  /// (A-232).
  #[test]
  // The test thread stands for the client's shard draining later: it must wait off-shard while the answer waits.
  #[allow(clippy::disallowed_methods)]
  fn a_task_refused_at_admission_answers_its_client_through_a_full_channel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static RELEASED: AtomicBool = AtomicBool::new(false);
    static ADMITTED: AtomicBool = AtomicBool::new(false);
    static FILLED: AtomicBool = AtomicBool::new(false);
    static DROPPED: AtomicBool = AtomicBool::new(false);
    /// The owing task's captures. Fields drop in their order: the guard answers (or stands down, if the task ran),
    /// then the flag says it has, so the test takes the filler only after the answer met the full channel.
    struct Owing {
      guard: AnswerUnstarted,
      _then: Dropped,
    }
    struct Dropped;
    impl Drop for Dropped {
      fn drop(&mut self) {
        DROPPED.store(true, Ordering::Release);
      }
    }
    RELEASED.store(false, Ordering::Release);
    ADMITTED.store(false, Ordering::Release);
    FILLED.store(false, Ordering::Release);
    DROPPED.store(false, Ordering::Release);
    let daemon = crate::daemon::audit_daemon();
    let (registration, receiver) = crate::daemon::observe_first(&daemon, |state| {
      let cap = state.config.runtime.tasks_per_shard.saturating_mul(2);
      let at = state.shard;
      let (registration, receiver) = registry::register(
        4,
        1,
        registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
      )
      .unwrap();
      let home = registration.shard();
      let filler = SpawnRequest::new(Box::pin(async {}), None);
      registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
      let maker = slates_rt::futures::spawn(async move {
        let (_, refused) = fill_this_arena(&RELEASED, cap);
        // A turn lets a task that ended before the fill (this observation's) give its slot back; the top-up takes it.
        slates_rt::futures::yield_now().await;
        let (_, topped) = fill_this_arena(&RELEASED, cap);
        FILLED.store(refused && topped, Ordering::Release);
        let owing = Owing {
          guard: AnswerUnstarted::new(
            home,
            at,
            0,
            0,
            slates_ipc::protocol::ReplyBody::Refused {
              refusal: slates_ipc::protocol::Refusal::Overloaded { shard: at },
            },
          ),
          _then: Dropped,
        };
        let task = SpawnRequest::new(
          Box::pin(async move {
            // The whole struct moves in, never its guard alone, so a drop unrun drops both fields in their order.
            let mut owing = owing;
            owing.guard.started();
            ADMITTED.store(true, Ordering::Release);
          }),
          None,
        );
        registry::send_control(at, Control::Spawn(Box::new(task))).unwrap();
        // This task keeps its own slot until released, so the arena is still full when the shard drains its channel.
        while !RELEASED.load(Ordering::Acquire) {
          slates_rt::futures::yield_now().await;
        }
      });
      maker.and_then(slates_rt::futures::detach).unwrap();
      (registration, receiver)
    });
    let began = std::time::Instant::now();
    while !DROPPED.load(Ordering::Acquire) {
      assert!(
        began.elapsed() < MESSAGE_WAIT,
        "the owing task was dropped within {MESSAGE_WAIT:?}"
      );
      std::thread::sleep(std::time::Duration::from_nanos(LOOK_EVERY_NS));
    }
    assert!(take(&receiver, &registration).is_ok(), "the filler");
    let answered = take(&receiver, &registration);
    RELEASED.store(true, Ordering::Release);
    assert!(
      FILLED.load(Ordering::Acquire),
      "the arena refused a filler, before and after the turn (the precondition)"
    );
    assert!(
      !ADMITTED.load(Ordering::Acquire),
      "the owing task was refused at admission (the precondition), not run"
    );
    assert!(
      answered.is_ok(),
      "the answer's message arrived once there was room"
    );
    drop(registration);
    daemon.stop();
  }

  /// The carrier keeps at most its bound, sends what it keeps in order, and refuses one past the bound to its sender,
  /// counted (§4.3; CLAUDE.md §2 item 8; goal 238). Do: start a one-shard daemon with one client a shard, so its
  /// derived bound (`DaemonConfig::kept_bound`) is about one task arena; on its shard, register a stand-in slot whose
  /// control channel holds half the bound and fill it; hand `bound` numbered replies home to the stand-in, from the
  /// shard's state as `hand_reply_home`'s callers do, then one more. Expect: each of the `bound` hand-offs was taken,
  /// and the one past them was refused to its sender and never ran; the shard's report counts one
  /// `xshard.keep_refused`, and its waits for room `bound` begun, all held at once. Then take every message off the
  /// channel, running each and telling the shards waiting for room after each, as a drain does: the fillers arrive,
  /// then the `bound` replies in the order they were handed home, and nothing else; every wait has ended. The channel
  /// holds half of what is kept, so every send of what is kept meets it full part-way. Kept past its bound, the last
  /// hand-off is taken; a send refused part-way and put back behind the rest rather than in front, the numbers arrive
  /// out of order; a carrier that does not ask its target for room is never woken again, and no reply arrives.
  #[test]
  // The test thread stands for the client's shard draining later: it must wait off-shard while the replies wait.
  #[allow(clippy::disallowed_methods)]
  fn the_carrier_keeps_its_bound_in_order_and_refuses_one_past_it() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static OUT_OF_ORDER: AtomicUsize = AtomicUsize::new(0);
    static PAST_RAN: AtomicBool = AtomicBool::new(false);
    NEXT.store(0, Ordering::Release);
    OUT_OF_ORDER.store(0, Ordering::Release);
    PAST_RAN.store(false, Ordering::Release);
    let daemon = crate::daemon::audit_daemon_configured(|config| config.clients_per_shard = 1);
    let (registration, receiver, bound, room, taken, past) =
      crate::daemon::observe_first(&daemon, |state| {
        let bound = state.config.kept_bound().get();
        let room = (bound / 2).max(1);
        let (registration, receiver) = registry::register(
          4,
          room,
          registry::RegisterKick::Kick(slates_rt::driver::Kick::none()),
        )
        .unwrap();
        let home = registration.shard();
        for _ in 0..room {
          let filler = SpawnRequest::new(Box::pin(async {}), None);
          registry::send_control(home, Control::Spawn(Box::new(filler))).unwrap();
        }
        let taken = (0..bound)
          .filter(|&number| {
            hand_reply_home(home, move || {
              if NEXT.fetch_add(1, Ordering::AcqRel) != number {
                OUT_OF_ORDER.fetch_add(1, Ordering::AcqRel);
              }
            })
            .is_ok()
          })
          .count();
        let past = hand_reply_home(home, || PAST_RAN.store(true, Ordering::Release));
        (registration, receiver, bound, room, taken, past)
      });
    println!("the carrier's bound: {bound} replies, the stand-in channel {room}");
    assert_eq!(taken, bound, "every hand-off up to the bound was taken");
    assert!(
      past.is_err(),
      "the hand-off past the bound was refused to its sender: {past:?}"
    );
    let (refused, held) = crate::daemon::observe_first(&daemon, |state| {
      let report = crate::verbs::shard_report(state);
      let refused: u64 = report
        .refusals
        .iter()
        .filter(|counted| counted.kind == KEEP_REFUSED)
        .map(|counted| counted.count)
        .sum();
      (refused, room_waits())
    });
    assert_eq!(refused, 1, "the shard's report counts the one refused keep");
    let bound_waits = u64::try_from(bound).unwrap();
    assert_eq!(
      (held.started, held.peak),
      (bound_waits, bound_waits),
      "every kept reply is a wait for room, all held at once"
    );
    let ran = run_until_every_reply_arrived(&receiver, &registration, (&NEXT, bound));
    let after = crate::daemon::observe_first(&daemon, |_| room_waits());
    assert_eq!(
      ran,
      room + bound,
      "the fillers, then the replies, and nothing else"
    );
    assert!(receiver.try_recv().is_err(), "no message was sent twice");
    assert_eq!(
      OUT_OF_ORDER.load(Ordering::Acquire),
      0,
      "the replies arrived in the order they were handed home"
    );
    assert!(
      !PAST_RAN.load(Ordering::Acquire),
      "the refused reply never ran"
    );
    assert_eq!(
      (after.ended, after.now),
      (after.started, 0),
      "every wait for room ended once its reply was sent"
    );
    drop(registration);
    daemon.stop();
  }

  /// The drain of `the_carrier_keeps_its_bound_in_order_and_refuses_one_past_it`: take every message off the stand-in's
  /// channel and run it, as the client's shard would, until `next` counts all `bound` replies. Expect each a task that
  /// runs to its end at once, and a message within the wait each time. Returns the messages run.
  fn run_until_every_reply_arrived(
    receiver: &std::sync::mpsc::Receiver<Control>,
    registration: &registry::Registration,
    (next, bound): (&std::sync::atomic::AtomicUsize, usize),
  ) -> usize {
    use std::sync::atomic::Ordering;
    let mut ran = 0_usize;
    while next.load(Ordering::Acquire) < bound {
      match take(receiver, registration) {
        Ok(Control::Spawn(mut request)) => {
          let mut context = Context::from_waker(Waker::noop());
          assert!(
            request.future.as_mut().poll(&mut context).is_ready(),
            "a message's task runs to its end at once"
          );
        }
        Ok(_) => panic!("only tasks were sent"),
        Err(error) => panic!(
          "{} of {bound} replies arrived, then none within {MESSAGE_WAIT:?}: {error:?}",
          next.load(Ordering::Acquire)
        ),
      }
      ran += 1;
    }
    ran
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
  /// back to back with a task that spins and yields; and wait for room there within one coordinator period. Expect: the
  /// wait gives up about one coordinator period after it began, not much more. Before 2026-11-06 it was a count of tries
  /// of one pause each, and each try lasted a pause plus the shard's busy poll; since 2026-11-18 (A-296) it asks for
  /// room and waits for the drain's word, which never comes here, so only its deadline ends it.
  #[test]
  // The test thread waits for the result off the shard.
  #[allow(clippy::disallowed_methods)]
  fn a_bounded_wait_for_room_waits_its_period_in_time_on_a_starved_shard() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    /// Shape: how long the starver keeps the shard busy at a time: a fifth of the room wait's budget (20 ms), so the
    /// deadline falls inside a busy poll.
    const STARVED_FOR_NS: u64 = REPLY_ROOM_NS / 5;
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
      std::thread::sleep(std::time::Duration::from_nanos(LOOK_EVERY_NS));
    }
    let waited = WAITED_NS.load(Ordering::Acquire);
    let longest = LONGEST_POLL_NS.load(Ordering::Acquire);
    // The budget, then at most the busy poll under way at the deadline and the waiter's own next turn behind another,
    // and a tenth of a period for the deadline's timer tick and wake: a deadline overshoots by that, where a count of
    // tries overshoots by a poll for every try.
    let bound = REPLY_ROOM_NS + 2 * longest + LOOK_EVERY_NS;
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
    // A send its sender saw refused: the sender answers the client on its own refusal path. A flag already gone (the
    // thread ending) reads as standing down.
    if DISCARDING_UNSENT.try_with(Cell::get).unwrap_or(true) {
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
    // This shard's arena refused the task the guard was in, and the guard drops inside the runtime's admission, where
    // no task can be spawned: the answer is carried home as any reply is ([`hand_reply_home`]), kept by the carrier
    // while the client's shard's channel is full, which needs neither.
    if hand_reply_home(home, move || {
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
    Err(refused) => discarding_unsent(|| {
      drop(refused.message);
      Err(refused.error)
    }),
  }
}

/// Counter: a task owing a client its reply that was dropped before it ran (its spawn refused at admission, its shard
/// shutting down), its client answered instead ([`AnswerUnstarted`]).
pub(crate) const UNSTARTED_ANSWERED: &str = "xshard.unstarted_answered";
/// Counter: such a task whose answer could not be carried home either: the client's shard gone, or this shard's
/// carrier refusing to keep it ([`hand_reply_home`]).
pub(crate) const UNSTARTED_UNANSWERED: &str = "xshard.unstarted_unanswered";

/// The synchronous end of a reply carried home ([`send_reply_home`]): run on the client's shard by its task, or, when
/// that shard drops the task unrun (its arena full at admission), run there by the drop. Dropped anywhere else (the
/// client's shard gone, or the sending shard stopping while its carrier kept it; counted `xshard.reply_dropped`), it
/// does nothing.
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
    if DISCARDING_UNSENT.try_with(Cell::get).unwrap_or(true) || self.body.is_none() {
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
/// arena there. The sibling of [`AnswerUnstarted`] the supervisor ordered fixed with it (2026-11-07). A reply that
/// could not be carried (its client's shard gone) is counted `xshard.reply_dropped`.
pub fn send_reply_home(origin: u16, body: impl FnOnce() + Send + 'static) {
  send_back(origin, reply_task(origin, body));
}

/// The task that runs `body`, a reply's synchronous end, on `origin`, or runs it there as it is dropped unrun
/// ([`RunUnstarted`]).
fn reply_task(origin: u16, body: impl FnOnce() + Send + 'static) -> SpawnRequest {
  let mut guard = RunUnstarted {
    at: origin,
    body: Some(Box::new(body)),
  };
  SpawnRequest::new(
    Box::pin(async move {
      guard.run();
    }),
    None,
  )
}

/// Counter: a reply carried home whose task the client's shard refused at admission, run there by its drop instead
/// ([`send_reply_home`]).
pub(crate) const REPLY_RUN_UNSTARTED: &str = "xshard.reply_run_unstarted";

/// [`send_reply_home`] for a caller that holds its shard's state, or a guard dropped inside the runtime's admission
/// ([`AnswerUnstarted`]): carried the same way ([`carry`]), with the refusal (the client's shard gone, or the carrier
/// refusing to keep it) returned for the caller to count, the reply dropped and its guard standing down. Until
/// 2026-11-09 `body` was handed to `target`'s channel once, so a reply that met a full channel was lost (CLAUDE.md §2
/// items 8 and 9); until 2026-11-18 one that met a full channel waited in a task spawned here, which a full arena, or
/// the admission a guard drops in, refused.
pub(crate) fn hand_reply_home(
  target: u16,
  body: impl FnOnce() + Send + 'static,
) -> Result<(), RtError> {
  carry(target, Control::Spawn(Box::new(reply_task(target, body))))
}

/// Runs `drop_or_hand`, during which a reply guard dropped stands down ([`DISCARDING_UNSENT`]): the caller reports
/// the refusal.
fn discarding_unsent<T>(drop_or_hand: impl FnOnce() -> T) -> T {
  // The flag as it was, restored after: a stand-down inside another keeps the outer one standing down.
  let before = DISCARDING_UNSENT.with(|discarding| discarding.replace(true));
  let result = drop_or_hand();
  DISCARDING_UNSENT.with(|discarding| discarding.set(before));
  result
}
