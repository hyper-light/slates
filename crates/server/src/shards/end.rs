//! The daemon's end of a Shards stream as each platform serves it (§4.6, the Shards route; A-161, A-179): the one seam
//! between the serve loop in `crate::shards`, which is the same code everywhere, and the stream's I/O. Paired `#[cfg]`
//! functions with identical signatures, as the runtime's drivers are (`slates_rt::driver`).
//!
//! - **Unix**: a non-blocking socket of an anonymous pair (`slates_ipc::stream`), read and written by `recv` and
//!   `send`, a descriptor riding a reply's first byte by `SCM_RIGHTS`, and awaited by its descriptor's readiness.
//! - **Windows**: the server end of a duplex named pipe connected to itself (`slates_ipc::stream`'s Windows arm), taken
//!   into the shard's IOCP driver (`slates_rt::pipe::PipeEnd`), which keeps one read and one write of it in the kernel
//!   and turns their completions into the same try, would-block and await model. Its reads ask for the largest frame
//!   and its writes take as much, so a frame never needs more than one completion past its length word.
//!
//! What a try found is told in the same words on both ([`Received`], [`Sent`]), so the loop's framing, bounds and
//! teardown are one path (R8's rule for platforms: one code path, no mode switch).

#[cfg(windows)]
use slates_bridge_fuse::shards_fs::{FRAME_BYTES, LENGTH_BYTES};

/// The daemon's end of a stream: a non-blocking socket (Unix).
#[cfg(unix)]
pub(crate) type DaemonEnd = std::os::fd::OwnedFd;
/// The daemon's end of a stream: a pipe end served by the shard's completion driver (Windows, A-179).
#[cfg(windows)]
pub(crate) type DaemonEnd = slates_rt::pipe::PipeEnd;

/// The end handed to the client whose attach made the stream: a socket descriptor (Unix).
#[cfg(unix)]
pub type HandedEnd = std::os::fd::OwnedFd;
/// The end handed to the client whose attach made the stream: the client end of the pipe, which the handle route
/// duplicates into the client's process (Windows, A-179).
#[cfg(windows)]
pub type HandedEnd = std::os::windows::io::OwnedHandle;

/// What a try to read the daemon's end found.
pub(super) enum Received {
  /// This many bytes, at least one.
  Bytes(usize),
  /// The peer has gone (or the end was ended), and every byte it wrote has been read.
  Closed,
  /// Nothing waits now: await readability.
  Waiting,
  /// The stream failed.
  Failed,
}

/// What a try to write the daemon's end did.
pub(super) enum Sent {
  /// The end took this many bytes, at least one; a descriptor asked to ride them went with them.
  Taken(usize),
  /// The end takes nothing now: await writability.
  Blocked,
  /// The stream failed (its peer gone, or an error).
  Failed,
}

/// Derived: the bytes one read of a pipe end asks for and one write takes, and what the pipe buffers each way: one
/// largest frame and its length word, so any frame the protocol allows arrives in one read and a reply's write completes
/// without waiting for the guest to read it.
#[cfg(windows)]
fn frame_bytes() -> usize {
  LENGTH_BYTES.saturating_add(FRAME_BYTES)
}

/// Makes a stream: the daemon's end, ready to serve on this shard, and the end to hand out. Refused when the OS
/// refuses the pair.
#[cfg(unix)]
pub(super) fn pair() -> Result<(DaemonEnd, HandedEnd), slates_ipc::IpcError> {
  slates_ipc::stream::pair()
}

/// Makes a stream: the pipe's server end, taken into this shard's driver, and its client end. Refused when the OS
/// refuses the pipe or the driver refuses the end. Run on the shard that serves it (an attach runs on the volume's
/// owner shard, where its serve task is spawned).
#[cfg(windows)]
pub(super) fn pair() -> Result<(DaemonEnd, HandedEnd), slates_ipc::IpcError> {
  let (server, client) = slates_ipc::stream::pair(frame_bytes())?;
  let served =
    slates_rt::pipe::PipeEnd::adopt(server, frame_bytes(), frame_bytes()).map_err(|_| {
      slates_ipc::IpcError::OsRefused {
        call: "a pipe end taken into the shard's driver",
        code: None,
      }
    })?;
  Ok((served, client))
}

/// The value the runtime's readiness seam names `end` by.
#[cfg(unix)]
pub(super) fn raw(end: &DaemonEnd) -> i32 {
  std::os::fd::AsRawFd::as_raw_fd(end)
}

/// The value the runtime's readiness seam names `end` by.
#[cfg(windows)]
pub(super) fn raw(end: &DaemonEnd) -> i32 {
  end.raw()
}

/// Reads what has arrived on `end` into `room` (never empty).
#[cfg(unix)]
pub(super) fn receive(end: &DaemonEnd, room: &mut [u8]) -> Received {
  use std::os::fd::AsFd;
  match slates_ipc::stream::recv(end.as_fd(), room) {
    Ok(0) => Received::Closed,
    Ok(read) => Received::Bytes(read),
    Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Received::Waiting,
    Err(_) => Received::Failed,
  }
}

/// Reads what has arrived on `end` into `room` (never empty).
#[cfg(windows)]
pub(super) fn receive(end: &DaemonEnd, room: &mut [u8]) -> Received {
  use slates_rt::pipe_state::PipeRead;
  match end.try_read(room) {
    // Zero only for an empty buffer, which `room` never is; taken as nothing more now rather than as the end.
    Ok(PipeRead::Bytes(0) | PipeRead::WouldBlock) => Received::Waiting,
    Ok(PipeRead::Bytes(read)) => Received::Bytes(read),
    Ok(PipeRead::Ended) => Received::Closed,
    Err(_) => Received::Failed,
  }
}

/// Writes what of `bytes` the end takes now, with `descriptor`, if one, riding the first byte by `SCM_RIGHTS`.
#[cfg(unix)]
pub(super) fn send(
  end: &DaemonEnd,
  bytes: &[u8],
  descriptor: Option<&super::dax::Descriptor>,
) -> Sent {
  use std::os::fd::AsFd;
  let sent = match descriptor {
    Some(descriptor) => {
      slates_ipc::stream::send_with_descriptor(end.as_fd(), bytes, descriptor.as_fd())
    }
    None => slates_ipc::stream::send(end.as_fd(), bytes),
  };
  match sent {
    Ok(0) | Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => Sent::Blocked,
    Ok(taken) => Sent::Taken(taken),
    Err(_) => Sent::Failed,
  }
}

/// Writes what of `bytes` the end takes now. No descriptor rides a pipe: Windows has none to send, and its descriptor
/// asks are refused before one is owed (`super::dax`, Windows' arm), so `descriptor` is always `None` here.
#[cfg(windows)]
pub(super) fn send(
  end: &DaemonEnd,
  bytes: &[u8],
  descriptor: Option<&super::dax::Descriptor>,
) -> Sent {
  use slates_rt::pipe_state::PipeWrite;
  if let Some(never) = descriptor {
    match *never {}
  }
  match end.try_write(bytes) {
    Ok(PipeWrite::Taken(0) | PipeWrite::WouldBlock) => Sent::Blocked,
    Ok(PipeWrite::Taken(taken)) => Sent::Taken(taken),
    Err(_) => Sent::Failed,
  }
}

/// Ends the conversation on `end` both ways without closing it, so the task serving it wakes on its own readiness and
/// the peer learns of the end at its next read. Whether the OS did it.
#[cfg(unix)]
pub(super) fn end(end: &DaemonEnd) -> bool {
  use std::os::fd::AsFd;
  slates_ipc::stream::end(end.as_fd()).is_ok()
}

/// Ends the conversation on `end` both ways without closing it: the pipe disconnected, and what the end has in the
/// kernel cancelled, so its serve task wakes and finds the stream ended at its next try (`PipeEnd::disconnect`).
/// Whether the OS did it.
#[cfg(windows)]
pub(super) fn end(end: &DaemonEnd) -> bool {
  end.disconnect().is_ok()
}
