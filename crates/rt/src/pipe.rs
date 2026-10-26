//! A Windows named pipe's end on the runtime (§4.3, §4.6; A-179): the stream the daemon serves a Shards microVM's
//! virtio-fs device on (the Shards route on Windows), awaited through the shard's IOCP driver like any descriptor.
//!
//! **Why a pipe, and why completions.** Windows has no `socketpair`, and an AF_UNIX socket at an abstract address cannot
//! be connected to, so an AF_UNIX pair needs a socket file on disk, which R1 forbids. A duplex named pipe under a fresh
//! name needs none (Shards' PM M340, measured on jupiter and on GitHub's windows-2022 and windows-2025 runners,
//! 2026-10-26). Its pair costs 9.9 to 14.3 µs at p50, against an AF_UNIX pair's 0.6 ms. Its round trip is the shorter
//! on every host measured, 0.6 to 0.9 times loopback TCP's on the runners. A reader gets every byte at its writer's
//! exit. Pipes report their I/O on completion, and the AFD reactor that gives sockets readiness (`crate::afd`) does not
//! take them. So the driver keeps at most one read and one write of each end in the kernel, each into a [`Block`] the
//! kernel owns until its completion, and the decisions turning that into the runtime's try, would-block and await model
//! are pure and tested on every host ([`crate::pipe_state`]).
//!
//! **Ownership.** A block is leaked to the kernel when an operation is handed over and reclaimed exactly once, either
//! when the port delivers its completion or, for an operation the kernel refused at once, right here (a refusal posts
//! nothing). Every completion goes through the port, even one that finished at once, because the driver does not set
//! `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`. So no block is reclaimed twice. A block names its end by handle value and
//! generation, so a completion that arrives after its end was released (and the value reused) finds no entry, frees its
//! buffer and wakes no one. Releasing an end cancels its operations (`CancelIoEx`); their completions come back as
//! cancelled. A driver dropped with operations still in the kernel leaks their blocks rather than freeing memory the
//! kernel may still write, as the AFD reactor's poll blocks do.

use std::collections::BTreeMap;
use std::os::windows::io::{AsRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
  ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
  ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::IO::{CancelIoEx, CreateIoCompletionPort, OVERLAPPED};
use windows_sys::Win32::System::Pipes::DisconnectNamedPipe;

use crate::pipe_state::{Outcome, ReadSide, ReadStep, Started, Want, WriteSide, WriteStep};
pub use crate::pipe_state::{PipeRead, PipeWrite};

use crate::error::RtError;
use crate::readiness::{readable, writable};
use crate::registry;

/// Format: the completion key a pipe end is associated with its driver's port under; its completions carry a
/// [`Block`] in the overlapped pointer.
pub(crate) const PIPE_KEY: usize = usize::MAX - 3;

/// A pipe end the runtime serves: its handle, owned, and taken into its shard's driver for as long as it lives.
/// Made on the shard that serves it; every call must be made there.
///
/// The handle is never lent out. Every completion under [`PIPE_KEY`] is taken as a [`Block`] this module leaked, so
/// overlapped I/O issued on the handle by anyone else would hand the driver a pointer it does not own. What an owner
/// needs of the handle beyond reading and writing is a method here ([`Self::disconnect`]).
#[derive(Debug)]
pub struct PipeEnd {
  handle: OwnedHandle,
  raw: i32,
}

impl PipeEnd {
  /// Takes `handle`, an end of a pipe opened for overlapped I/O, into the current shard's driver. Each read asks for up
  /// to `read_bytes` and each write takes up to `write_bytes`, as the end's owner derived them (its largest frame).
  pub fn adopt(
    handle: OwnedHandle,
    read_bytes: usize,
    write_bytes: usize,
  ) -> Result<PipeEnd, RtError> {
    let raw = narrow(&handle)?;
    registry::with_current(|ctx| {
      ctx.adopt_pipe(
        raw,
        handle.as_raw_handle().expose_provenance(),
        (read_bytes, write_bytes),
      )
    })
    .unwrap_or(Err(RtError::NotOnShardThread))?;
    Ok(PipeEnd { handle, raw })
  }

  /// Tries to read into `out`: bytes, would-block (await [`Self::readable`]), or the end of the stream.
  pub fn try_read(&self, out: &mut [u8]) -> Result<PipeRead, RtError> {
    registry::with_current(|ctx| ctx.pipe_read(self.raw, out))
      .unwrap_or(Err(RtError::NotOnShardThread))
  }

  /// Tries to write `bytes`: the count the kernel took, or would-block (await [`Self::writable`]). A peer gone is an
  /// error, as `EPIPE` is on Unix.
  pub fn try_write(&self, bytes: &[u8]) -> Result<PipeWrite, RtError> {
    registry::with_current(|ctx| ctx.pipe_write(self.raw, bytes))
      .unwrap_or(Err(RtError::NotOnShardThread))
  }

  /// Awaits readability: a finished read's bytes, the end of the stream, or an error to report.
  pub async fn readable(&self) -> Result<(), RtError> {
    readable(self.raw).await
  }

  /// Awaits writability: the write in the kernel has finished.
  pub async fn writable(&self) -> Result<(), RtError> {
    writable(self.raw).await
  }

  /// The value the readiness seam names this end by.
  pub fn raw(&self) -> i32 {
    self.raw
  }

  /// Cuts the peer off without closing this end, as `shutdown(Both)` does a socket: the server end of the pipe is
  /// disconnected (`DisconnectNamedPipe`), so the peer's next operation fails, and anything this end has in the kernel
  /// is cancelled. The owner's task then wakes on its own readiness and finds the end of the stream: a read's completion
  /// says the pipe is no longer connected, or, cancelled, its next try is refused so at once. For a stream ended by
  /// anything but itself (a detach, a revocation, a destroy). The handle must be a pipe's server end.
  pub fn disconnect(&self) -> Result<(), RtError> {
    let handle: HANDLE = self.handle.as_raw_handle();
    // SAFETY: the handle is this end's, open for its life; disconnecting the server end of a pipe instance touches no
    // memory of this process.
    if unsafe { DisconnectNamedPipe(handle) } == 0 {
      return Err(RtError::os("DisconnectNamedPipe"));
    }
    // SAFETY: the same handle; cancelling every operation on it (a null overlapped) completes each as aborted, and the
    // driver reclaims their blocks when those completions arrive. Nothing in flight is not an error.
    unsafe { CancelIoEx(handle, std::ptr::null()) };
    Ok(())
  }
}

impl Drop for PipeEnd {
  /// Releases the end from its driver (cancelling what it has in the kernel) before the handle closes. Off its shard,
  /// as at a runtime's shutdown, the driver keeps a stale entry that a later end of the same value replaces.
  fn drop(&mut self) {
    let _released = registry::with_current(|ctx| ctx.release_pipe(self.raw));
    // `self.handle` closes after this, cancelling anything left in the kernel too.
    let _ = &self.handle;
  }
}

/// The value the readiness seam carries for `handle`: its low 32 bits, as a Winsock socket is carried
/// (`netsys::Socket::raw_id`). Kernel handle values fit; one that does not is refused rather than truncated.
fn narrow(handle: &OwnedHandle) -> Result<i32, RtError> {
  let low = u32::try_from(handle.as_raw_handle().addr()).map_err(|_| RtError::DriverRefused {
    call: "a pipe handle wider than the readiness seam",
    code: None,
  })?;
  Ok(i32::from_ne_bytes(low.to_ne_bytes()))
}

/// One operation the kernel owns: its `OVERLAPPED` first, so the completion's overlapped pointer is the block.
#[repr(C)]
pub(crate) struct Block {
  overlapped: OVERLAPPED,
  /// The end it belongs to, by handle value and the generation it was taken in under.
  raw: i32,
  generation: u64,
  /// Whether it writes (else it reads).
  writing: bool,
  /// The bytes it reads into or writes from; the kernel's until the completion.
  buffer: Vec<u8>,
}

/// One end in the driver: its handle (borrowed; the [`PipeEnd`] owns it), the generation it was taken in under, and
/// its two sides.
#[derive(Debug)]
struct Entry {
  handle: usize,
  generation: u64,
  read: ReadSide,
  write: WriteSide,
}

/// The pipe ends a driver serves, by handle value.
#[derive(Debug, Default)]
pub(crate) struct Pipes {
  entries: BTreeMap<i32, Entry>,
  /// The generation the next end is taken in under.
  next_generation: u64,
}

impl Pipes {
  /// Whether `raw` is an end this driver serves (else the readiness seam's `raw` is a socket).
  pub(crate) fn holds(&self, raw: i32) -> bool {
    self.entries.contains_key(&raw)
  }

  /// Takes an end into the driver: its handle associated with `port` under [`PIPE_KEY`], and its sides made. An entry
  /// left at the same value by an end released off its shard is replaced; its blocks still in the kernel find a newer
  /// generation and are freed.
  pub(crate) fn adopt(
    &mut self,
    port: HANDLE,
    (raw, handle): (i32, usize),
    (read_bytes, write_bytes): (usize, usize),
  ) -> Result<(), RtError> {
    let pipe: HANDLE = std::ptr::with_exposed_provenance_mut(handle);
    // SAFETY: `pipe` is the live handle the adopting `PipeEnd` owns and `port` the driver's live port; associating the
    // pipe with the port makes its completions arrive there under `PIPE_KEY`. The call returns the port or null.
    let associated = unsafe { CreateIoCompletionPort(pipe, port, PIPE_KEY, 0) };
    if associated.is_null() {
      return Err(RtError::os("CreateIoCompletionPort(pipe)"));
    }
    let generation = self.next_generation;
    self.next_generation = self.next_generation.wrapping_add(1);
    self.entries.insert(
      raw,
      Entry {
        handle,
        generation,
        read: ReadSide::new(read_bytes),
        write: WriteSide::new(write_bytes),
      },
    );
    Ok(())
  }

  /// Releases an end: what it has in the kernel is cancelled, and its entry goes, so those completions free their
  /// blocks and wake no one.
  pub(crate) fn release(&mut self, raw: i32) {
    if let Some(entry) = self.entries.remove(&raw)
      && (entry.read.in_kernel() || entry.write.in_kernel())
    {
      let pipe: HANDLE = std::ptr::with_exposed_provenance_mut(entry.handle);
      // SAFETY: the handle is still open (the `PipeEnd` releases before it closes it). Cancelling every operation on
      // it (a null overlapped) makes each complete as aborted; the blocks are reclaimed when those completions arrive.
      unsafe { CancelIoEx(pipe, std::ptr::null()) };
    }
  }

  /// A try to read `raw` into `out`.
  pub(crate) fn read(&mut self, raw: i32, out: &mut [u8]) -> Result<PipeRead, RtError> {
    let entry = self.entries.get_mut(&raw).ok_or(NOT_A_PIPE)?;
    match entry.read.try_read(out) {
      ReadStep::Answer(answer) => Ok(answer),
      ReadStep::Failed(code) => Err(refused("ReadFile", code)),
      ReadStep::Start(buffer) => {
        let started = start(entry, raw, false, buffer, None);
        entry.read.started(started);
        match entry.read.try_read(out) {
          ReadStep::Answer(answer) => Ok(answer),
          ReadStep::Failed(code) => Err(refused("ReadFile", code)),
          // A start refused as cancelled leaves the side idle: nothing held yet, so the caller awaits and retries.
          ReadStep::Start(buffer) => {
            entry
              .read
              .started(Started::Refused(Outcome::Cancelled, buffer));
            Ok(PipeRead::WouldBlock)
          }
        }
      }
    }
  }

  /// A try to write `bytes` to `raw`.
  pub(crate) fn write(&mut self, raw: i32, bytes: &[u8]) -> Result<PipeWrite, RtError> {
    let entry = self.entries.get_mut(&raw).ok_or(NOT_A_PIPE)?;
    match entry.write.try_write(bytes) {
      WriteStep::Answer(answer) => Ok(answer),
      WriteStep::PeerGone => Err(refused("WriteFile", peer_gone_code())),
      WriteStep::Failed(code) => Err(refused("WriteFile", code)),
      WriteStep::Start(buffer, count) => {
        let started = start(entry, raw, true, buffer, Some(count));
        let taken = started == Started::InKernel;
        entry.write.started(started);
        if taken {
          return Ok(PipeWrite::Taken(count));
        }
        match entry.write.try_write(bytes) {
          WriteStep::PeerGone => Err(refused("WriteFile", peer_gone_code())),
          WriteStep::Failed(code) => Err(refused("WriteFile", code)),
          // Refused as cancelled: nothing was taken; the caller awaits and retries.
          WriteStep::Answer(_) | WriteStep::Start(..) => Ok(PipeWrite::WouldBlock),
        }
      }
    }
  }

  /// A registration of readability (`writing` false) or writability for `raw` by `interest`: whether to deliver the
  /// word at once (the side is ready) or leave it to the operation's completion.
  pub(crate) fn want(&mut self, raw: i32, writing: bool, interest: u64) -> Result<bool, RtError> {
    let entry = self.entries.get_mut(&raw).ok_or(NOT_A_PIPE)?;
    if writing {
      return Ok(matches!(entry.write.want(interest), Want::Now));
    }
    match entry.read.want(interest) {
      Want::Now => Ok(true),
      Want::Later => Ok(false),
      Want::Start(buffer) => {
        let started = start(entry, raw, false, buffer, None);
        entry.read.started(started);
        // Once a read is in the kernel the waiter is set and its completion delivers the word; a refused start left the
        // side ready (the end of the stream, a failure) or idle, and either is told at once.
        match entry.read.want(interest) {
          Want::Later => Ok(false),
          Want::Now => Ok(true),
          Want::Start(buffer) => {
            entry
              .read
              .started(Started::Refused(Outcome::Cancelled, buffer));
            Ok(true)
          }
        }
      }
    }
  }

  /// A completion the port delivered under [`PIPE_KEY`]: reclaims its block and returns the waiter to wake, if its end
  /// is still served and one registered.
  ///
  /// # Safety
  /// `overlapped` is the pointer of a completion delivered under [`PIPE_KEY`]: a [`Block`] [`start`] leaked, delivered
  /// once.
  pub(crate) unsafe fn complete(&mut self, overlapped: *mut OVERLAPPED) -> Option<u64> {
    if overlapped.is_null() {
      return None;
    }
    // SAFETY: the contract above: a leaked block, whose `OVERLAPPED` head is what was handed over, delivered once;
    // `from_raw` takes ownership back.
    let block = unsafe { Box::from_raw(overlapped.cast::<Block>()) };
    let Block {
      overlapped,
      raw,
      generation,
      writing,
      buffer,
    } = *block;
    let entry = self
      .entries
      .get_mut(&raw)
      .filter(|entry| entry.generation == generation)?;
    let outcome = finished(&overlapped);
    if writing {
      entry.write.finished(outcome, buffer)
    } else {
      entry.read.finished(outcome, buffer)
    }
  }
}

/// The refusal a call naming no end of this driver's gets.
const NOT_A_PIPE: RtError = RtError::DriverRefused {
  call: "a pipe operation on an end this shard's driver does not serve",
  code: None,
};

/// Hands the kernel a read into `buffer` (`writing` false) or a write of its first `count` bytes, for `entry` at `raw`:
/// in the kernel, or refused at once with the buffer back.
fn start(entry: &Entry, raw: i32, writing: bool, buffer: Vec<u8>, count: Option<usize>) -> Started {
  let length = u32::try_from(count.unwrap_or(buffer.len()).min(buffer.len())).unwrap_or(u32::MAX);
  let block = Box::into_raw(Box::new(Block {
    // SAFETY: an all-zero OVERLAPPED is a valid, unstarted one (no offset, no event).
    overlapped: unsafe { std::mem::zeroed() },
    raw,
    generation: entry.generation,
    writing,
    buffer,
  }));
  let pipe: HANDLE = std::ptr::with_exposed_provenance_mut(entry.handle);
  // SAFETY: `block` was just leaked, so its buffer and `OVERLAPPED` stay put until the block is reclaimed: by the port's
  // completion (the operation was taken) or below (refused at once, when nothing is posted). The pipe is associated
  // with the driver's port under `PIPE_KEY`, so the completion is delivered there, and the driver does not skip the
  // port on a success at once, so every taken operation posts exactly one completion.
  let handed = unsafe {
    let buffer_start = (*block).buffer.as_mut_ptr();
    let overlapped = std::ptr::addr_of_mut!((*block).overlapped);
    if writing {
      WriteFile(
        pipe,
        buffer_start.cast_const(),
        length,
        std::ptr::null_mut(),
        overlapped,
      )
    } else {
      ReadFile(pipe, buffer_start, length, std::ptr::null_mut(), overlapped)
    }
  };
  let error = if handed == 0 {
    // SAFETY: no arguments.
    unsafe { GetLastError() }
  } else {
    0
  };
  if handed != 0 || error == ERROR_IO_PENDING {
    return Started::InKernel;
  }
  // SAFETY: the operation was refused at once, so the kernel posts no completion for it; the block is reclaimed here,
  // once.
  let block = unsafe { Box::from_raw(block) };
  Started::Refused(outcome_of(error, 0), block.buffer)
}

/// The outcome a finished operation's `OVERLAPPED` records: its status (the NTSTATUS in `Internal`) and the bytes it
/// moved (`InternalHigh`), as Microsoft documents the two fields for a completed request. The status is the field's
/// low 32 bits (the I/O status block's `Status` shares that word with a pointer), so it is taken as its first four
/// bytes in little-endian order, which both Windows targets are.
fn finished(overlapped: &OVERLAPPED) -> Outcome {
  let moved = overlapped.InternalHigh;
  let [first, second, third, fourth, ..] = overlapped.Internal.to_le_bytes();
  let status = i32::from_le_bytes([first, second, third, fourth]);
  if status == 0 {
    return Outcome::Moved(moved);
  }
  outcome_of(crate::afd::dos_error(status), moved)
}

/// The outcome a Win32 error means for a pipe operation that moved `moved` bytes.
fn outcome_of(error: u32, moved: usize) -> Outcome {
  match error {
    0 => Outcome::Moved(moved),
    ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED => Outcome::PeerGone,
    ERROR_OPERATION_ABORTED => Outcome::Cancelled,
    other => Outcome::Failed(i32::try_from(other).unwrap_or(i32::MAX)),
  }
}

/// The code a write to a gone peer reports.
fn peer_gone_code() -> i32 {
  i32::try_from(ERROR_BROKEN_PIPE).unwrap_or(i32::MAX)
}

/// A refusal of `call` with a Win32 `code`.
fn refused(call: &'static str, code: i32) -> RtError {
  RtError::DriverRefused {
    call,
    code: Some(code),
  }
}
