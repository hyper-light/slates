//! Where the purge in progress lives on Windows (A-243, A-190, A-198; §4.6): the slot of `crate::purge_slot` in an
//! unnamed section both threads map, each through its own view (`slates_mem::SharedObject`, the slot's word declared
//! and its purge bytes a racy span). The mount thread holds the writer's view and the receive worker the reader's, so
//! neither borrows the other's memory and the section lives while either view does. The worker also keeps here the one
//! completion event of its control calls. The protocol and its reasons are in `crate::purge_slot`.

use crate::host::WinFspError;
use crate::purge_slot::{self, Memory, PURGE_AT, Refusal, SLOT_BYTES, WORD_AT, Writer};
use slates_mem::{SharedObject, WordRun, Words, words::Width};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::Ordering;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::CreateEventW;

pub(crate) use crate::purge_slot::Intent;

/// The mount thread's side: the only writer.
pub(crate) struct Sender {
  slot: SharedObject,
  writer: Writer,
}

/// The receive worker's side: the only reader, and the completion event of its control calls.
pub(crate) struct Intents {
  slot: SharedObject,
  /// The one completion event of this receiver's control transactions, which run one at a time. Every transaction
  /// names it in its OVERLAPPED, and the system clears it as each begins, so none makes its own (A-212).
  completed: OwnedHandle,
}

/// The slot's declared words: its word, and the purge's bytes as one racy span.
fn words() -> Words {
  Words::new()
    .with(WordRun::one(WORD_AT, Width::U64))
    .with(WordRun::racy(PURGE_AT, SLOT_BYTES - PURGE_AT))
}

impl Memory for SharedObject {
  fn load(&self, order: Ordering) -> Result<u64, Refusal> {
    Ok(
      self
        .atomic_u64(WORD_AT)
        .map_err(Refusal::Memory)?
        .load(order),
    )
  }
  fn store(&self, value: u64, order: Ordering) -> Result<(), Refusal> {
    self
      .atomic_u64(WORD_AT)
      .map_err(Refusal::Memory)?
      .store(value, order);
    Ok(())
  }
  fn copy_out(&self, at: usize, into: &mut [u8]) -> Result<(), Refusal> {
    self
      .read_racy(PURGE_AT.saturating_add(at), into)
      .map_err(Refusal::Memory)
  }
  fn copy_in(&mut self, at: usize, from: &[u8]) -> Result<(), Refusal> {
    self
      .write_racy(PURGE_AT.saturating_add(at), from)
      .map_err(Refusal::Memory)
  }
}

/// A new unnamed event, initially unsignalled, reset by hand.
fn new_event(call: &'static str) -> Result<OwnedHandle, WinFspError> {
  // SAFETY: no borrowed descriptor or name; the flags are plain values.
  let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
  if raw.is_null() {
    return Err(WinFspError::Os {
      call,
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  // SAFETY: the successful call returned this one newly owned handle.
  Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}

/// The slot, holding no purge, and its reader.
pub(crate) fn channel() -> Result<(Sender, Intents), WinFspError> {
  let slot = SharedObject::unnamed(SLOT_BYTES, words())
    .map_err(|error| WinFspError::Intent(Refusal::Memory(error)))?;
  let value = slot
    .handle_value()
    .map_err(|error| WinFspError::Intent(Refusal::Memory(error)))?;
  let raw = usize::try_from(value).map_err(|_| WinFspError::MountThread)? as *mut core::ffi::c_void;
  // SAFETY: the writer's object owns this live section, and the borrowed handle lasts only until duplication returns.
  let duplicate = unsafe { BorrowedHandle::borrow_raw(raw) }
    .as_handle()
    .try_clone_to_owned()
    .map_err(|error| WinFspError::Os {
      call: "duplicate the purge slot's section",
      code: error.raw_os_error(),
    })?;
  let view = SharedObject::from_held_handle(duplicate, SLOT_BYTES, words())
    .map_err(|error| WinFspError::Intent(Refusal::Memory(error)))?;
  Ok((
    Sender {
      slot,
      writer: Writer::new(),
    },
    Intents {
      slot: view,
      completed: new_event("create transaction completion event")?,
    },
  ))
}

impl Sender {
  /// Publishes `intent` as the purge in progress, or no purge: never waits for the reader.
  pub(crate) fn publish(&mut self, intent: Option<&Intent>) -> Result<(), WinFspError> {
    self
      .writer
      .publish(&mut self.slot, intent)
      .map_err(WinFspError::Intent)
  }
}

impl Intents {
  /// The purge in progress whose opens `process` makes, or `None` (`crate::purge_slot::current_for`).
  pub(crate) fn current_for(&self, process: u32) -> Result<Option<Intent>, WinFspError> {
    purge_slot::current_for(&self.slot, process).map_err(WinFspError::Intent)
  }

  /// The event a control transaction names in its OVERLAPPED. It stays valid as long as this value, which outlives
  /// every transaction: the worker owns both and settles each before the next.
  pub(crate) fn completion_event(&self) -> HANDLE {
    self.completed.as_raw_handle()
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]
  use super::*;
  use crate::purge::Purge;

  /// Shape: enough publications that a writer that waited for its reader would be caught many times over.
  const PUBLICATIONS: u32 = 10_000;
  /// Shape: the client the racing test's purges name, and a process that is not it.
  const CLIENT: u32 = 7;
  /// Shape: see [`CLIENT`].
  const PROGRAM: u32 = 8;

  fn intent(seed: u32) -> Intent {
    Intent {
      purge: Purge {
        object: slates_bridge_core::ObjectId::new(u64::from(seed), u64::from(seed) + 1),
        path: format!("\\{seed}"),
        offset: u64::from(seed) * 4096,
        len: 4096,
      },
      client: CLIENT,
    }
  }

  /// GAPS, "A private purge's intent can wait up to the 1 s transaction timeout". Do: through the section's two views,
  /// publish 10,000 purges and their ends with nothing reading. Expect: every publication returns, and the reader's view
  /// then sees no purge, then the next purge whole for its client only. The channel this replaced blocked on the third.
  #[test]
  fn a_publisher_never_waits_for_its_reader_across_the_sections_views() {
    let (mut sender, intents) = channel().unwrap();
    for seed in 1..=PUBLICATIONS {
      sender.publish(Some(&intent(seed))).unwrap();
      sender.publish(None).unwrap();
    }
    assert_eq!(intents.current_for(CLIENT).unwrap(), None);
    sender.publish(Some(&intent(PUBLICATIONS + 1))).unwrap();
    assert_eq!(
      intents.current_for(CLIENT).unwrap(),
      Some(intent(PUBLICATIONS + 1))
    );
    assert_eq!(intents.current_for(PROGRAM).unwrap(), None);
  }

  /// A-243. Do: the mount thread's view publishes 10,000 distinct purges while the worker's view reads as the client's
  /// Creates and a program's would, until the writer is done. Expect: the client's reads are no purge or one publication
  /// whole; a program's none; and once the writer is done, the last purge reads whole.
  #[test]
  fn a_publish_racing_a_create_reads_no_purge_or_a_whole_one_across_the_sections_views() {
    let (mut sender, intents) = channel().unwrap();
    let done = std::sync::atomic::AtomicBool::new(false);
    let mut racing = 0u32;
    std::thread::scope(|scope| {
      scope.spawn(|| {
        for seed in 1..=PUBLICATIONS {
          sender.publish(Some(&intent(seed))).unwrap();
        }
        done.store(true, Ordering::Release);
      });
      while !done.load(Ordering::Acquire) {
        assert_eq!(intents.current_for(PROGRAM).unwrap(), None);
        if let Some(read) = intents.current_for(CLIENT).unwrap() {
          let seed = u32::try_from(read.purge.object.inode).unwrap();
          assert_eq!(read, intent(seed), "a torn read of purge {seed}");
        }
        racing += 1;
      }
    });
    assert!(racing > 0, "no read raced the writer");
    assert_eq!(
      intents.current_for(CLIENT).unwrap(),
      Some(intent(PUBLICATIONS))
    );
  }
}
