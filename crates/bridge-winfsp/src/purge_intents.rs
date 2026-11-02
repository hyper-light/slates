//! Progress for private coherence opens (A-198, §4.6 Windows). A drive open may be refused before WinFsp calls
//! the host. Such an open cannot be relied upon to drain its Begin/End messages. The bounded channel therefore
//! signals an owned Windows event: the receiver drains messages while its overlapped control I/O is pending, as
//! well as before a callback. A full publisher wakes the reader before waiting for room and again after publishing.
//! Every wait still owns its kernel buffers until completion; a wake never cancels the held volume's receive.

use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects};
use crate::host::WinFspError;
use crate::purge::Purge;

/// Shape: the previous purge's End and the next Begin can precede that Begin's synchronous drive open.
const BOUND: usize = 2;
/// Format: the intent event follows the control completion event in WaitForMultipleObjects' array.
const INTENT_READY: u32 = WAIT_OBJECT_0 + 1;

pub(crate) struct Sender {
  messages: SyncSender<Option<Purge>>,
  event: OwnedHandle,
}

pub(crate) struct Intents {
  messages: Receiver<Option<Purge>>,
  event: OwnedHandle,
  current: Option<Purge>,
}

pub(crate) fn channel() -> Result<(Sender, Intents), WinFspError> {
  // SAFETY: a new unnamed auto-reset event, initially unsignalled; no borrowed descriptor or name.
  let raw = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
  if raw.is_null() { return Err(os_error("create private-intent event")); }
  // SAFETY: the successful call returned this one newly owned handle.
  let event = unsafe { OwnedHandle::from_raw_handle(raw) };
  let producer = event.as_handle().try_clone_to_owned().map_err(|error| WinFspError::Os { call: "duplicate private-intent event", code: error.raw_os_error() })?;
  let (sender, messages) = sync_channel(BOUND);
  Ok((Sender { messages: sender, event: producer }, Intents { messages, event, current: None }))
}

impl Sender {
  pub(crate) fn publish(&self, intent: Option<Purge>) -> Result<(), WinFspError> {
    match self.messages.try_send(intent) {
      Ok(()) => {}
      Err(TrySendError::Full(intent)) => {
        // Wake before a blocking send: a rejected drive open may have produced no callback at all.
        self.signal()?;
        self.messages.send(intent).map_err(|_| WinFspError::MountThread)?;
      }
      Err(TrySendError::Disconnected(_)) => return Err(WinFspError::MountThread),
    }
    // Signal after publication as well: the reader may have drained the prior full queue before this send finished.
    self.signal()
  }

  fn signal(&self) -> Result<(), WinFspError> {
    // SAFETY: this producer owns the event with modification rights for the whole synchronous call.
    if unsafe { SetEvent(self.event.as_raw_handle()) } == 0 { return Err(os_error("signal private intents")); }
    Ok(())
  }
}

impl Intents {
  pub(crate) fn current(&self) -> Option<&Purge> { self.current.as_ref() }

  /// One bounded drain. Further messages keep the event signalled and are drained at the next wait or callback.
  pub(crate) fn progress(&mut self) -> Result<usize, WinFspError> {
    let mut received = 0usize;
    for _ in 0..BOUND {
      match self.messages.try_recv() {
        Ok(intent) => { self.current = intent; received = received.saturating_add(1); }
        Err(TryRecvError::Empty) => break,
        Err(TryRecvError::Disconnected) => return Err(WinFspError::MountThread),
      }
    }
    Ok(received)
  }

  /// Waits for the caller's control event while servicing private intents. The caller must still settle its I/O
  /// before returning on an error here: this method owns neither its OVERLAPPED nor its borrowed kernel buffers.
  pub(crate) fn wait(&mut self, completed: BorrowedHandle<'_>) -> Result<(), WinFspError> {
    let events = [completed.as_raw_handle(), self.event.as_raw_handle()];
    let count = u32::try_from(events.len()).map_err(|_| WinFspError::MountThread)?;
    loop {
      // SAFETY: both handles remain owned throughout this wait; these are events, not abandoned mutex objects.
      match unsafe { WaitForMultipleObjects(count, events.as_ptr(), 0, INFINITE) } {
        WAIT_OBJECT_0 => return Ok(()),
        INTENT_READY => { self.progress()?; }
        _ => return Err(os_error("wait for control or private intents")),
      }
    }
  }
}

fn os_error(call: &'static str) -> WinFspError {
  WinFspError::Os { call, code: std::io::Error::last_os_error().raw_os_error() }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used)]
  use super::*;
  use windows_sys::Win32::System::Threading::WaitForSingleObject;
  /// Shape: the existing Windows process fixture's ten-second wait; a missed wake fails within it.
  const WAIT_MS: u32 = 10_000;
  /// Shape: two complete purges, exceeding the channel's two-message capacity without any kernel callback.
  const PURGES: usize = 2;

  /// A-198. Do: publish two Begin/End pairs with no kernel request or callback. Expect: event wakes let the reader
  /// consume all four messages and let the owned publisher finish; a full channel cannot depend on a refused open.
  #[test]
  fn private_purges_make_progress_without_a_kernel_callback() {
    std::thread::scope(|scope| {
    let (sender, mut intents) = channel().unwrap();
    let publisher = scope.spawn(move || {
      for _ in 0..PURGES {
        sender.publish(Some(Purge { object: slates_bridge_core::ObjectId::new(1, 0), path: "\\file".to_owned(), offset: 0, len: 4096 }))?;
        sender.publish(None)?;
      }
      Ok::<_, WinFspError>(sender)
    });
    let mut received = 0usize;
    let mut missed = None;
    while received < PURGES * BOUND {
      // SAFETY: this reader owns the event until the wait returns; it has no outstanding kernel I/O.
      let ready = unsafe { WaitForSingleObject(intents.event.as_raw_handle(), WAIT_MS) };
      if ready != WAIT_OBJECT_0 { missed = Some(ready); break; }
      received += intents.progress().unwrap();
    }
    // Dropping the receiver also unblocks a broken publisher before the test joins it and reports the failure.
    drop(intents);
    let published = publisher.join().unwrap();
    assert_eq!(missed, None, "the private publisher must wake an idle receiver");
    assert!(published.is_ok(), "all private messages must be published");
    assert_eq!(received, PURGES * BOUND);
    });
  }
}
