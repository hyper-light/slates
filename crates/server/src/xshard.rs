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
use slates_rt::futures::sleep;
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

/// Runs `work` on `shard`'s state — directly when `shard` is this shard (`origin`), else as a task there —
/// delivering nothing back. The spawn is refused typed at the target's admission bound.
pub fn run_on(
  origin: u16,
  shard: u16,
  work: impl FnOnce(&mut ShardState) + Send + 'static,
) -> Result<(), RtError> {
  if shard == origin {
    state::with_state(work);
    return Ok(());
  }
  let task = SpawnRequest::new(
    Box::pin(async move {
      state::with_state(work);
    }),
    None,
  );
  registry::send_control(shard, Control::Spawn(Box::new(task)))
}

/// The future a call's result arrives on: ready when the target's task delivered it, or at once when the
/// call ran on this shard; `None` if the target's state was unavailable, the call was delivered back
/// as something else, or the entry vanished.
pub struct CrossShardCall<T> {
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
      let _ = registry::send_control(origin, Control::Spawn(Box::new(back)));
    }),
    None,
  );
  match registry::send_control(shard, Control::Spawn(Box::new(task))) {
    Ok(()) => Ok(CrossShardCall {
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

/// Runs `work` on `shard` and awaits its result for at most `deadline_ns`; `None` if the call could not
/// be sent, the target's state was unavailable, or the deadline passed (the entry is forgotten, so a late
/// result is dropped rather than delivered to a caller that moved on).
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

/// Awaits a registered call within the caller's remaining time budget (§4.3). Dropping this future
/// cancels its registration even when neither the call nor its deadline has completed.
pub async fn within<T: 'static>(call: CrossShardCall<T>, deadline_ns: u64) -> Option<T> {
  let mut call = std::pin::pin!(call);
  let mut timer = std::pin::pin!(sleep(deadline_ns));
  std::future::poll_fn(|cx| {
    if let Poll::Ready(result) = call.as_mut().poll(cx) {
      return Poll::Ready(result);
    }
    if timer.as_mut().poll(cx).is_ready() {
      return Poll::Ready(None);
    }
    Poll::Pending
  })
  .await
}

#[cfg(test)]
mod tests {
  use super::*;

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
