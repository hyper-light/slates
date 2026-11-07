//! Progress for private coherence opens (A-198, §4.6 Windows). A drive open may be refused before WinFsp calls
//! the host. Such an open cannot be relied upon to drain its Begin/End messages. The bounded channel therefore
//! signals an owned Windows event, and the receiver drains messages whenever its control call returns.
//!
//! WinFsp's control call never goes pending: the driver waits for a request inside the call (v2.1
//! `src/sys/fsctl.c:95-99`, `src/sys/volume.c`'s `FspVolumeFastTransact`), so an idle receiver returns only at the
//! volume's transaction timeout. A publisher that finds the channel full therefore signals the event and cancels the
//! receiver's call (`CancelIoEx`): WinFsp waits for a request with `FsRtlCancellableWaitForSingleObject` on that call's
//! IRP, and a cancelled wait returns `STATUS_CANCELLED` with nothing delivered. Every response the call carried was
//! consumed before the wait (the driver completes them first and never looks at the cancel), and a call with no output
//! returns before any wait. So the receiver reads a cancellation it finds the event set for as an empty receive, drains,
//! and receives again; any other cancellation (a stopped queue) stays its refusal.

use crate::host::WinFspError;
use crate::purge::Purge;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::IO::CancelIoEx;
use windows_sys::Win32::System::Threading::{
  CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

/// Shape: the previous purge's End and the next Begin can precede that Begin's synchronous drive open.
const BOUND: usize = 2;
/// Format: the intent event follows the control completion event in WaitForMultipleObjects' array.
const INTENT_READY: u32 = WAIT_OBJECT_0 + 1;
/// Derived: how long a full publisher keeps cancelling before it refuses: two transaction timeouts. A live receiver
/// returns from any call within one, cancelled or not, and drains as it returns; a second covers a callback it was
/// serving when the first cancel found no call.
const ROOM_WITHIN: Duration =
  Duration::from_millis(crate::dispatcher::TRANSACT_TIMEOUT_MS as u64 * 2);

/// One purge in progress: the purge, and the process id of the client making its opens (`crate::purge_client`).
#[derive(Clone, Debug)]
pub(crate) struct Intent {
  pub(crate) purge: Purge,
  pub(crate) client: u32,
}

pub(crate) struct Sender {
  messages: SyncSender<Option<Intent>>,
  event: OwnedHandle,
  /// The volume's control handle's value, once the mount has it: the handle whose receive a full publisher cancels.
  /// Held as its address, as the dispatcher holds the file system's, so the sender stays `Send`.
  control: Option<usize>,
}

pub(crate) struct Intents {
  messages: Receiver<Option<Intent>>,
  event: OwnedHandle,
  /// The one completion event of this receiver's control transactions, which run one at a time. Every transaction
  /// names it in its OVERLAPPED, and the system clears it as each begins, so none makes its own (A-212).
  completed: OwnedHandle,
  current: Option<Intent>,
}

/// A new unnamed event, initially unsignalled, its reset automatic or by hand as `manual_reset` says.
fn new_event(manual_reset: i32, call: &'static str) -> Result<OwnedHandle, WinFspError> {
  // SAFETY: no borrowed descriptor or name; the flags are plain values.
  let raw = unsafe { CreateEventW(std::ptr::null(), manual_reset, 0, std::ptr::null()) };
  if raw.is_null() {
    return Err(os_error(call));
  }
  // SAFETY: the successful call returned this one newly owned handle.
  Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

pub(crate) fn channel() -> Result<(Sender, Intents), WinFspError> {
  let event = new_event(0, "create private-intent event")?;
  let completed = new_event(1, "create transaction completion event")?;
  let producer = event
    .as_handle()
    .try_clone_to_owned()
    .map_err(|error| WinFspError::Os {
      call: "duplicate private-intent event",
      code: error.raw_os_error(),
    })?;
  let (sender, messages) = sync_channel(BOUND);
  Ok((
    Sender {
      messages: sender,
      event: producer,
      control: None,
    },
    Intents {
      messages,
      event,
      completed,
      current: None,
    },
  ))
}

impl Sender {
  /// Names the volume's control handle, whose receive a full publisher cancels. The mount calls this once its DLL
  /// object holds the final handle, before any receive runs; the handle outlives every publish.
  pub(crate) fn cancel_on(&mut self, control: HANDLE) {
    self.control = Some(control.addr());
  }

  pub(crate) fn publish(&self, intent: Option<Intent>) -> Result<(), WinFspError> {
    let mut intent = intent;
    let started = Instant::now();
    loop {
      match self.messages.try_send(intent) {
        Ok(()) => break,
        Err(TrySendError::Full(back)) => {
          // A refused drive open may have produced no request, so the receiver may be waiting inside its call until the
          // volume's transaction timeout. Mark the cancel as ours, then end that wait; retry until it has drained.
          if started.elapsed() >= ROOM_WITHIN {
            return Err(WinFspError::Os {
              call: "publish a private intent: the receiver never made room",
              code: None,
            });
          }
          intent = back;
          self.signal()?;
          self.cancel_receive()?;
          std::thread::yield_now();
        }
        Err(TrySendError::Disconnected(_)) => return Err(WinFspError::MountThread),
      }
    }
    // Signal after publication as well: the reader drains when it next returns.
    self.signal()
  }

  /// Cancels this process's control call on the volume, so a receiver waiting inside it returns with nothing delivered.
  /// No call in progress (the receiver serving a callback, or between calls) is no refusal: the next attempt finds it.
  fn cancel_receive(&self) -> Result<(), WinFspError> {
    let Some(control) = self.control else {
      return Ok(());
    };
    let control = control as HANDLE;
    // SAFETY: `control` is the live DLL object's volume handle, which outlives every publish; a null OVERLAPPED asks
    // only to cancel this process's outstanding I/O on it, borrowing nothing.
    if unsafe { CancelIoEx(control, std::ptr::null()) } == 0 {
      let error = std::io::Error::last_os_error();
      if error.raw_os_error() != i32::try_from(ERROR_NOT_FOUND).ok() {
        return Err(WinFspError::Os {
          call: "cancel the receiver's call",
          code: error.raw_os_error(),
        });
      }
    }
    Ok(())
  }

  fn signal(&self) -> Result<(), WinFspError> {
    // SAFETY: this producer owns the event with modification rights for the whole synchronous call.
    if unsafe { SetEvent(self.event.as_raw_handle()) } == 0 {
      return Err(os_error("signal private intents"));
    }
    Ok(())
  }
}

impl Intents {
  pub(crate) fn current(&self) -> Option<&Intent> {
    self.current.as_ref()
  }

  /// Whether a publisher signalled before this call was cancelled: the cancel was a full publisher's, so the call
  /// delivered nothing and the receiver drains and receives again. Taking the signal resets the event; the drain that
  /// follows takes every message it announced.
  pub(crate) fn cancelled_by_publisher(&self) -> bool {
    // SAFETY: this receiver owns the event for the whole call; a zero timeout only tests and, if set, resets it.
    unsafe { WaitForSingleObject(self.event.as_raw_handle(), 0) == WAIT_OBJECT_0 }
  }

  /// One bounded drain. Further messages keep the event signalled and are drained at the next wait or callback.
  pub(crate) fn progress(&mut self) -> Result<usize, WinFspError> {
    let mut received = 0usize;
    for _ in 0..BOUND {
      match self.messages.try_recv() {
        Ok(intent) => {
          self.current = intent;
          received = received.saturating_add(1);
        }
        Err(TryRecvError::Empty) => break,
        Err(TryRecvError::Disconnected) => return Err(WinFspError::MountThread),
      }
    }
    Ok(received)
  }

  /// The event a control transaction names in its OVERLAPPED. It stays valid as long as this value, which outlives
  /// every transaction: the worker owns both and settles each before the next.
  pub(crate) fn completion_event(&self) -> HANDLE {
    self.completed.as_raw_handle()
  }

  /// Waits for the control transaction's completion event while servicing private intents. The caller must still settle
  /// its I/O before returning on an error here: this method owns neither its OVERLAPPED nor its borrowed kernel buffers.
  pub(crate) fn wait(&mut self) -> Result<(), WinFspError> {
    let events = [self.completed.as_raw_handle(), self.event.as_raw_handle()];
    let count = u32::try_from(events.len()).map_err(|_| WinFspError::MountThread)?;
    loop {
      // SAFETY: both handles remain owned throughout this wait; these are events, not abandoned mutex objects.
      match unsafe { WaitForMultipleObjects(count, events.as_ptr(), 0, INFINITE) } {
        WAIT_OBJECT_0 => return Ok(()),
        INTENT_READY => {
          self.progress()?;
        }
        _ => return Err(os_error("wait for control or private intents")),
      }
    }
  }
}

fn os_error(call: &'static str) -> WinFspError {
  WinFspError::Os {
    call,
    code: std::io::Error::last_os_error().raw_os_error(),
  }
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
          sender.publish(Some(Intent {
            purge: Purge {
              object: slates_bridge_core::ObjectId::new(1, 0),
              path: "\\file".to_owned(),
              offset: 0,
              len: 4096,
            },
            client: std::process::id(),
          }))?;
          sender.publish(None)?;
        }
        Ok::<_, WinFspError>(sender)
      });
      let mut received = 0usize;
      let mut missed = None;
      while received < PURGES * BOUND {
        // SAFETY: this reader owns the event until the wait returns; it has no outstanding kernel I/O.
        let ready = unsafe { WaitForSingleObject(intents.event.as_raw_handle(), WAIT_MS) };
        if ready != WAIT_OBJECT_0 {
          missed = Some(ready);
          break;
        }
        received += intents.progress().unwrap();
      }
      // Dropping the receiver also unblocks a broken publisher before the test joins it and reports the failure.
      drop(intents);
      let published = publisher.join().unwrap();
      assert_eq!(
        missed, None,
        "the private publisher must wake an idle receiver"
      );
      assert!(published.is_ok(), "all private messages must be published");
      assert_eq!(received, PURGES * BOUND);
    });
  }
}
