//! A bounded channel between any two of a task on a shard, a task on another shard and a plain thread (A-767 piece
//! C2): the seam the vendored crates from mantle and hyper-raft (`hyper-block`'s issuer, `mantle-engine`'s worker
//! pool) used `hyper-rt::sync` for, ported onto slates' one runtime (CLAUDE.md §2 item 2, D-9; main's ruling of
//! 2026-11-20: no `hyper-rt`, no `unsafe impl Send`/`Sync`). Its surface is upstream's (`channel`, `channel_with`,
//! `Sender`, `ChannelReceiver`, `SyncError`), so the vendored code changes only its paths.
//!
//! **How a value crosses.** Upstream moves values through a lock-free ring of its own whose slots are written
//! through `UnsafeCell` and shared by `unsafe impl Send`/`Sync`. Here a value travels through the standard library's
//! bounded channel (`std::sync::mpsc::sync_channel`), whose `try_send` and `try_recv` take no lock (its array
//! flavour, a bounded MPMC queue). What upstream's cells add on top is kept: a task that waits registers its word in
//! a cell of the process-wide table ([`cell`]) and is woken through the registry, and senders waiting for room hold
//! places granted in turn ([`room`]), so a task never blocks its shard.
//!
//! **Who waits where.** A task waits in a cell (receiver) or a room place (sender) and is polled again by its wake.
//! A plain thread blocks inside the standard library's channel (`send`, `recv`), which wakes it itself: no thread
//! handle is kept anywhere, so the module holds no `unsafe` code. Every operation that makes room or delivers a
//! value wakes what waits on the other side, whichever kind it is: a take grants room to a waiting sender task, and
//! a send wakes the receiver task.
//!
//! **Ends going.** The last sender to go drops its end of the standard channel first, then wakes the receiver, so
//! the receiver's next look finds it disconnected; the receiver marks itself gone and drops its end, then wakes
//! every waiting sender, which finds the channel closed.

pub mod cell;
mod room;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::task::{Context, Poll};

use cell::{CellRef, claim};
use room::Room;

/// Why a send or a take did not happen; the value, where there was one, is handed back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError<T> {
  /// The other end is gone: nothing more will come (or be taken).
  Closed(T),
  /// The channel is full now, or every place for a waiting sender is held; the value is handed back.
  Full(T),
  /// A task's wait was polled by something that is not a slates task, or a plain thread's call was made on a shard
  /// (which must never block).
  NotOnShardThread(T),
}

impl<T> fmt::Display for SyncError<T> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Closed(_) => f.write_str("the channel's other end is gone"),
      Self::Full(_) => f.write_str("the channel is full"),
      Self::NotOnShardThread(_) => {
        f.write_str("a task's wait off a shard, or a blocking call on one")
      }
    }
  }
}

impl<T: fmt::Debug> std::error::Error for SyncError<T> {}

/// Why a channel could not be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncRefusal {
  /// The process-wide cell table is at its bound.
  Capacity {
    /// What was bounded.
    what: &'static str,
    /// The bound.
    bound: usize,
  },
  /// A capacity or waiter count the channel cannot honour.
  BadConfig {
    /// Which value.
    what: &'static str,
  },
}

impl fmt::Display for SyncRefusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Capacity { what, bound } => write!(f, "{what}: at the bound of {bound}"),
      Self::BadConfig { what } => write!(f, "refused: {what}"),
    }
  }
}

impl std::error::Error for SyncRefusal {}

/// Format: the receiver's cell's state bit once the receiver is gone (a send is refused `Closed`).
const RECEIVER_GONE: u64 = 1;

/// Format: the handles a new channel's cells start with: the first sender and the receiver.
const ENDS: u32 = 2;

/// The sending half of a bounded channel; clone it for more senders.
#[derive(Debug)]
pub struct Sender<T> {
  /// The standard channel's sender; `None` only inside `drop`, which lets it go before waking the receiver.
  values: Option<SyncSender<T>>,
  /// The receiver's cell: its waiter the receiver task, its state [`RECEIVER_GONE`], its `aux` the live senders.
  cell: CellRef,
  room: Room,
}

/// The receiving half of a bounded channel.
#[derive(Debug)]
pub struct ChannelReceiver<T> {
  /// The standard channel's receiver; `None` only inside `drop`, which lets it go before waking the senders.
  values: Option<Receiver<T>>,
  cell: CellRef,
  room: Room,
}

/// A bounded channel of `capacity` values from any number of senders to one receiver, with as many places for
/// sender tasks waiting at once. Refused `BadConfig` for a zero capacity, `Capacity` at the cell table's bound.
pub fn channel<T>(capacity: usize) -> Result<(Sender<T>, ChannelReceiver<T>), SyncRefusal> {
  channel_with(capacity, capacity)
}

/// A bounded channel of `capacity` values, with places for `waiters` sender tasks waiting at once (past it a task's
/// send is told `Full`; a plain thread's send waits in the standard channel and holds no place). Refused `BadConfig`
/// for a zero capacity or no waiters.
pub fn channel_with<T>(
  capacity: usize,
  waiters: usize,
) -> Result<(Sender<T>, ChannelReceiver<T>), SyncRefusal> {
  if capacity == 0 {
    return Err(SyncRefusal::BadConfig {
      what: "a channel of no capacity",
    });
  }
  let cell = claim(ENDS)?;
  let room = match Room::new(waiters, ENDS) {
    Ok(room) => room,
    Err(refused) => {
      cell.release();
      cell.release();
      return Err(refused);
    }
  };
  if let Some(words) = cell.cell() {
    words.aux.store(1, Ordering::Release);
  }
  let (sender, receiver) = sync_channel(capacity);
  // The cells were claimed with both ends' handles (`ENDS`), so the receiver's list is a copy that raises no count.
  let receiver_room = room.list_copy();
  Ok((
    Sender {
      values: Some(sender),
      cell,
      room,
    },
    ChannelReceiver {
      values: Some(receiver),
      cell,
      room: receiver_room,
    },
  ))
}

impl<T> Clone for Sender<T> {
  fn clone(&self) -> Self {
    self.cell.retain();
    if let Some(words) = self.cell.cell() {
      words.aux.fetch_add(1, Ordering::AcqRel);
    }
    Self {
      values: self.values.clone(),
      cell: self.cell,
      room: self.room.share(),
    }
  }
}

impl<T> Drop for Sender<T> {
  fn drop(&mut self) {
    // The standard channel's end goes first, so a receiver woken by the last sender finds it disconnected.
    drop(self.values.take());
    if let Some(words) = self.cell.cell()
      && words.aux.fetch_sub(1, Ordering::AcqRel) == 1
    {
      words.wake();
    }
    self.cell.release();
  }
}

impl<T> Sender<T> {
  /// Sends without waiting: `Full` when the channel is at its capacity, `Closed` when the receiver is gone. A value
  /// sent wakes a waiting receiver task.
  pub fn try_send(&self, value: T) -> Result<(), SyncError<T>> {
    if self.receiver_gone() {
      return Err(SyncError::Closed(value));
    }
    let Some(values) = &self.values else {
      return Err(SyncError::Closed(value));
    };
    match values.try_send(value) {
      Ok(()) => {
        self.cell.wake();
        Ok(())
      }
      Err(TrySendError::Full(value)) => Err(SyncError::Full(value)),
      Err(TrySendError::Disconnected(value)) => Err(SyncError::Closed(value)),
    }
  }

  /// Sends on a plain thread, waiting for room inside the standard channel. Refused `NotOnShardThread` on a shard
  /// (a shard never blocks), `Closed` when the receiver goes.
  pub fn blocking_send(&self, value: T) -> Result<(), SyncError<T>> {
    if crate::registry::current_shard().is_some() {
      return Err(SyncError::NotOnShardThread(value));
    }
    let Some(values) = &self.values else {
      return Err(SyncError::Closed(value));
    };
    match values.send(value) {
      Ok(()) => {
        self.cell.wake();
        Ok(())
      }
      Err(std::sync::mpsc::SendError(value)) => Err(SyncError::Closed(value)),
    }
  }

  /// Sends, waiting for room as a task.
  pub fn send(&self, value: T) -> Send<'_, T> {
    Send {
      sender: self,
      value: Some(value),
      place: None,
    }
  }

  fn receiver_gone(&self) -> bool {
    self
      .cell
      .cell()
      .is_none_or(|words| words.state.load(Ordering::Acquire) & RECEIVER_GONE != 0)
  }
}

/// The wait of [`Sender::send`]: a sender task that finds the channel full holds a place among the waiting senders,
/// its word registered there, and checks once more; each value the receiver takes grants one waiting place its room
/// and wakes it. A woken sender whose room a racing sender took waits again in its place. One dropped while waiting
/// frees its place; one dropped after a grant it did not use hands the room on.
#[derive(Debug)]
pub struct Send<'a, T> {
  sender: &'a Sender<T>,
  value: Option<T>,
  /// The place held among the waiting senders, while one is.
  place: Option<usize>,
}

impl<T> Unpin for Send<'_, T> {}

impl<T> Send<'_, T> {
  /// Lets the place go, if one is held: a grant this send did not use goes on.
  fn leave(&mut self, used: bool) {
    if let Some(place) = self.place.take() {
      self.sender.room.leave(place, used);
    }
  }
}

impl<T> Drop for Send<'_, T> {
  fn drop(&mut self) {
    self.leave(false);
  }
}

impl<T> Future for Send<'_, T> {
  type Output = Result<(), SyncError<T>>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = &mut *self;
    let Some(value) = this.value.take() else {
      return Poll::Ready(Ok(()));
    };
    let Some(word) = crate::waker::polling_task(cx.waker()) else {
      this.leave(false);
      return Poll::Ready(Err(SyncError::NotOnShardThread(value)));
    };
    let sender = this.sender;
    let room = &sender.room;
    // A grant seen before the try is used by a send that follows it.
    let granted = this.place.is_some_and(|place| room.granted(place));
    let value = match sender.try_send(value) {
      Err(SyncError::Full(value)) => value,
      sent => {
        this.leave(granted);
        return Poll::Ready(sent);
      }
    };
    let place = match this.place {
      Some(place) => {
        if room.granted(place) {
          // Granted, and the room went to a racing sender: wait again in the same place.
          room.wait_again(place);
        }
        place
      }
      None => match room.hold() {
        Some(place) => {
          this.place = Some(place);
          place
        }
        None => return Poll::Ready(Err(SyncError::Full(value))),
      },
    };
    room.register_task(place, word);
    // Room made before the place showed waiting is seen here; room made after it is granted to the place.
    match sender.try_send(value) {
      Err(SyncError::Full(value)) => {
        this.value = Some(value);
        if room.granted(place) {
          // A grant landed before this task's word did, so its wake went nowhere: be polled again.
          cx.waker().wake_by_ref();
        }
        Poll::Pending
      }
      sent => {
        this.leave(false);
        Poll::Ready(sent)
      }
    }
  }
}

impl<T> ChannelReceiver<T> {
  /// Takes a value without waiting: `None` when the channel is empty, `Closed` when every sender is gone and every
  /// value taken. A value taken grants its room to one waiting sender task.
  pub fn try_recv(&mut self) -> Result<Option<T>, SyncError<()>> {
    let Some(values) = &self.values else {
      return Err(SyncError::Closed(()));
    };
    match values.try_recv() {
      Ok(value) => {
        self.room.grant_one();
        Ok(Some(value))
      }
      Err(TryRecvError::Empty) => Ok(None),
      Err(TryRecvError::Disconnected) => Err(SyncError::Closed(())),
    }
  }

  /// Takes a value on a plain thread, waiting inside the standard channel. Refused `NotOnShardThread` on a shard.
  pub fn blocking_recv(&mut self) -> Result<T, SyncError<()>> {
    if crate::registry::current_shard().is_some() {
      return Err(SyncError::NotOnShardThread(()));
    }
    let Some(values) = &self.values else {
      return Err(SyncError::Closed(()));
    };
    match values.recv() {
      Ok(value) => {
        self.room.grant_one();
        Ok(value)
      }
      Err(std::sync::mpsc::RecvError) => Err(SyncError::Closed(())),
    }
  }

  /// Takes a value, waiting as a task: `Closed` once every sender is gone and the channel is empty.
  pub fn recv(&mut self) -> Recv<'_, T> {
    Recv { receiver: self }
  }
}

impl<T> Drop for ChannelReceiver<T> {
  fn drop(&mut self) {
    // Senders refuse from now on; the standard channel's end goes before the senders are woken, so a woken sender
    // finds the channel closed, and the values sent drop with it.
    if let Some(words) = self.cell.cell() {
      words.state.fetch_or(RECEIVER_GONE, Ordering::AcqRel);
    }
    drop(self.values.take());
    self.room.wake_all();
    self.cell.release();
  }
}

/// The wait of [`ChannelReceiver::recv`].
#[derive(Debug)]
pub struct Recv<'a, T> {
  receiver: &'a mut ChannelReceiver<T>,
}

impl<T> Future for Recv<'_, T> {
  type Output = Result<T, SyncError<()>>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let Some(word) = crate::waker::polling_task(cx.waker()) else {
      return Poll::Ready(Err(SyncError::NotOnShardThread(())));
    };
    if let Some(ready) = ready_value(self.receiver.try_recv()) {
      return Poll::Ready(ready);
    }
    self.receiver.cell.register(word);
    match ready_value(self.receiver.try_recv()) {
      Some(ready) => Poll::Ready(ready),
      None => Poll::Pending,
    }
  }
}

/// A take's answer for a waiting receiver: a value or the channel's end, `None` while it is only empty.
fn ready_value<T>(taken: Result<Option<T>, SyncError<()>>) -> Option<Result<T, SyncError<()>>> {
  match taken {
    Ok(Some(value)) => Some(Ok(value)),
    Ok(None) => None,
    Err(error) => Some(Err(error)),
  }
}

#[cfg(test)]
mod tests;
