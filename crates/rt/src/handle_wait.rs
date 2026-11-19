//! A kernel object's signal awaited through the shard (Windows; §4.3 "Drivers", §4.7 "Failure matrix", A-475).
//!
//! A shard parks in its completion port, which hears of finished I/O and of the packets posted to it, not of a process
//! ending or an event being set. So a handle is waited on by the system thread pool: `RegisterWaitForSingleObject`
//! "directs a wait thread in the thread pool to wait on the object", and the callback that thread runs once the object
//! is signalled posts the waiting task's word to the shard's port (`PostQueuedCompletionStatus`, which never blocks),
//! where the shard's wait delivers it as it delivers any completion's word [B: Microsoft Learn,
//! "RegisterWaitForSingleObject function (winbase.h)", "UnregisterWaitEx function"]. The daemon's watch on a client's
//! process is the user: a process handle is signalled at the process's exit, however the process ended.
//!
//! - **One wait, one post.** `WT_EXECUTEONLYONCE` ("the thread will no longer wait on the handle after the callback
//!   function has been called once"), and the callback runs on the wait thread itself (`WT_EXECUTEINWAITTHREAD`, which
//!   "should be used only for short tasks"): it posts one packet. A process that has ended stays signalled, which the
//!   documentation allows only with one of those two flags.
//! - **Register, then test once.** The first poll registers the wait and then asks the object with a zero timeout, so an
//!   object signalled before the wait was made (a client dead before its watch) answers at that poll, not a packet
//!   later.
//! - **Which wake was this one.** Every later poll asks the object again with a zero timeout, so a wake from another of
//!   the task's sources leaves the future pending: A-149's rule (a readiness wait answers only for its own source), kept
//!   without an interest word.
//! - **Cancel before close.** The future borrows the handle, and its drop unregisters the wait with
//!   `UnregisterWaitEx(wait, INVALID_HANDLE_VALUE)`, which "waits for all callback functions to complete before
//!   returning". So the handle cannot close under a pending wait, which the documentation leaves undefined ("If this
//!   handle is closed while the wait is still pending, the function's behavior is undefined"), and the context the
//!   callback reads is freed only once no callback can read it. "Even wait operations that use WT_EXECUTEONLYONCE must
//!   be canceled", so a wait that fired is unregistered the same way. The callback only posts, so the drop waits for one
//!   post at most, and the documentation's deadlock (a callback taking a lock its unregistering thread holds) cannot
//!   arise.
//!
//! A packet posted before the wait was unregistered can still reach the shard after the future is gone; it carries the
//! task's word, so at worst it wakes that task's slot once more, a wake every task already tolerates.

use std::ffi::c_void;
use std::future::Future;
use std::os::windows::io::{AsRawHandle, BorrowedHandle};
use std::pin::Pin;
use std::task::{Context, Poll};

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Threading::{
  INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEINWAITTHREAD,
  WT_EXECUTEONLYONCE, WaitForSingleObject,
};

use crate::driver::{Kick, KickPort};
use crate::error::RtError;
use crate::registry;
use crate::waker::polling_task;

/// Awaits `handle`'s signal through the shard (the module doc): `Ok` once the object is signalled, refused when it is
/// polled off a shard, on a shard without a completion port, or when the system refuses the wait or the question.
pub fn signalled(handle: BorrowedHandle<'_>) -> Signalled<'_> {
  Signalled {
    handle,
    registered: None,
  }
}

/// The future [`signalled`] returns. It holds its wait from its first poll until it is dropped.
#[derive(Debug)]
pub struct Signalled<'handle> {
  handle: BorrowedHandle<'handle>,
  registered: Option<Registered>,
}

/// One registered wait: the wait object, and what its callback posts, boxed so its address holds while the wait may
/// fire.
#[derive(Debug)]
struct Registered {
  /// The wait object `RegisterWaitForSingleObject` returned, as its exposed address.
  wait: usize,
  /// What the callback reads; freed only after the wait is unregistered with no callback left running.
  post: Box<Post>,
}

/// What the callback posts, and to which shard's port.
#[derive(Debug)]
struct Post {
  /// The shard's port, pinned by the registry for each post.
  port: KickPort,
  /// The waiting task's word.
  word: u64,
}

impl Future for Signalled<'_> {
  type Output = Result<(), RtError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    if self.registered.is_none() {
      match register(self.handle, cx) {
        Ok(registered) => self.registered = Some(registered),
        Err(refusal) => return Poll::Ready(Err(refusal)),
      }
    }
    // The first poll's test once, and on every later poll which of the task's wakes this was.
    match signalled_now(self.handle) {
      Ok(true) => Poll::Ready(Ok(())),
      Ok(false) => Poll::Pending,
      Err(refusal) => Poll::Ready(Err(refusal)),
    }
  }
}

impl Drop for Signalled<'_> {
  fn drop(&mut self) {
    let Some(registered) = self.registered.take() else {
      return;
    };
    let wait: HANDLE = std::ptr::with_exposed_provenance_mut(registered.wait);
    // SAFETY: `wait` is the wait object `RegisterWaitForSingleObject` returned for this future, unregistered once, here,
    // on the shard's thread, never from its callback; with `INVALID_HANDLE_VALUE` the call returns only once no callback
    // of the wait runs or can run.
    let unregistered = unsafe { UnregisterWaitEx(wait, INVALID_HANDLE_VALUE) };
    if unregistered == 0 {
      // Refused, so a callback may still read the post: it is kept for the process's life rather than freed under the
      // callback. Only a wait object this future did not make is refused, which no path here passes.
      let _kept_for_the_process: &'static mut Post = Box::leak(registered.post);
    }
  }
}

/// Registers the wait on `handle` for the task `cx` polls, posting to its shard's port.
fn register(handle: BorrowedHandle<'_>, cx: &Context<'_>) -> Result<Registered, RtError> {
  let word = polling_task(cx.waker()).ok_or(RtError::NotOnShardThread)?;
  let port = match registry::with_entry(word.shard(), |entry| entry.kick) {
    Some(Kick::Iocp(port)) => port,
    Some(_) => {
      return Err(RtError::DriverRefused {
        call: "a handle wait on a shard without a completion port",
        code: None,
      });
    }
    None => return Err(RtError::NotOnShardThread),
  };
  let post = Box::new(Post {
    port,
    word: word.word(),
  });
  let mut wait: HANDLE = std::ptr::null_mut();
  // SAFETY: `wait` is a live local the call writes; `handle` is borrowed for this future's life and the wait is
  // unregistered in its drop before the borrow can end; the context is the boxed post, whose address holds until the
  // wait is unregistered with no callback left; `fired` matches the callback's signature.
  let registered = unsafe {
    RegisterWaitForSingleObject(
      &raw mut wait,
      handle.as_raw_handle(),
      Some(fired),
      std::ptr::from_ref::<Post>(post.as_ref()).cast::<c_void>(),
      INFINITE,
      WT_EXECUTEONLYONCE | WT_EXECUTEINWAITTHREAD,
    )
  };
  if registered == 0 {
    return Err(RtError::os("RegisterWaitForSingleObject"));
  }
  Ok(Registered {
    wait: wait.expose_provenance(),
    post,
  })
}

/// Whether `handle`'s object is signalled now: asked with a zero timeout, which never blocks.
fn signalled_now(handle: BorrowedHandle<'_>) -> Result<bool, RtError> {
  // SAFETY: the handle is borrowed live for the call; a zero timeout never blocks.
  match unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) } {
    WAIT_OBJECT_0 => Ok(true),
    WAIT_TIMEOUT => Ok(false),
    _ => Err(RtError::os("WaitForSingleObject")),
  }
}

/// The wait thread's callback: posts the waiting task's word to its shard's port. A shard that left its loop has no
/// port to post to, and then no task of it waits: its tasks were dropped first, and each dropped wait was unregistered.
extern "system" fn fired(context: *mut c_void, _timed_out: bool) {
  // SAFETY: the context is the `Post` its registration boxed, freed only after `UnregisterWaitEx` returned, which waits
  // for this callback; it is only read.
  let post = unsafe { &*context.cast::<Post>() };
  let _posted = post
    .port
    .with(|port| crate::iocp::post_word(port, post.word));
}

#[cfg(test)]
mod tests {
  use std::os::windows::io::AsHandle;

  use super::signalled;
  use crate::runtime::{LocalRuntime, RuntimeConfig};

  /// Shape: a runtime for one wait: one shard, a few task slots.
  fn runtime() -> LocalRuntime {
    LocalRuntime::new(&RuntimeConfig {
      shards: 1,
      tasks_per_shard: 8,
      timers_per_shard: 8,
      ring_entries: 8,
      step_budget_ns: 1_000_000,
      timer_tick_ns: 100_000,
      batch: 8,
      pin: false,
      cores: Vec::new(),
      page_bytes: 4096,
      spin_ns: 0,
      wake_tracking: None,
    })
    .unwrap()
  }

  /// Shape: how long a test lets a wait run before it gives up: a second, far past a kill's signal.
  const BOUND_NS: u64 = 1_000_000_000;

  /// A-475. Do: on a shard, wait on a child process's handle, kill the child once the wait is registered, and run the
  /// shard. Expect: the wait answers `Ok` at the post of its signal, well inside the bound.
  #[test]
  fn a_wait_on_a_process_answers_at_its_exit() {
    let runtime = runtime();
    let mut child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .unwrap();
    let process = child.as_handle().try_clone_to_owned().unwrap();
    let (sent, answer) = std::sync::mpsc::channel();
    runtime
      .spawn(async move {
        let waited = crate::futures::within(BOUND_NS, signalled(process.as_handle())).await;
        let _ = sent.send(waited);
      })
      .unwrap();
    // The first poll registers the wait and tests the live process once.
    let _ = runtime.context().step();
    child.kill().unwrap();
    runtime.run_until_idle();
    child.wait().unwrap();
    let waited = answer.try_recv().unwrap();
    assert!(
      matches!(waited, Ok(Some(Ok(())))),
      "the wait answered {waited:?}"
    );
  }

  /// A-475. Do: kill a child and reap it, then wait on its handle. Expect: the wait answers at its first poll, from the
  /// test after its registration.
  #[test]
  fn a_wait_on_a_process_already_gone_answers_at_once() {
    let runtime = runtime();
    let mut child = std::process::Command::new("cmd")
      .args(["/C", "ping", "-n", "30", "127.0.0.1"])
      .spawn()
      .unwrap();
    let process = child.as_handle().try_clone_to_owned().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let (sent, answer) = std::sync::mpsc::channel();
    runtime
      .spawn(async move {
        let waited = crate::futures::within(BOUND_NS, signalled(process.as_handle())).await;
        let _ = sent.send(waited);
      })
      .unwrap();
    let _ = runtime.context().step();
    let waited = answer.try_recv().unwrap();
    assert!(
      matches!(waited, Ok(Some(Ok(())))),
      "the wait answered {waited:?}"
    );
  }
}
