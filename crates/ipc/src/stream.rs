//! The anonymous stream a guest's transport rides (§4.6, the Shards fs route): a connected pair of Unix stream sockets
//! with no name and no filesystem entry, made by the daemon (R1: nothing on disk). The daemon keeps one end and serves
//! one attachment on it; the other end is the attachment's capability, handed to whoever the attach was for. Whoever
//! holds it reaches exactly that attachment, because the daemon serves nothing else on the pair. That is why a stream
//! needs none of the confinement a mapping's descriptor does (`rendezvous::Control::send_mapping`): a memory object's
//! descriptor carries its whole object, while a stream end carries only the conversation its peer will answer.
//!
//! Both ends are close-on-exec, so no child the daemon or a client spawns inherits one by accident; a client that hands
//! its end to a program (a VMM spawner) does so on purpose, by moving it to the number the program expects
//! ([`exec_with`]).
//! The daemon's
//! end is non-blocking, since it is served from a shard that never waits in a read, and a write to it after its peer
//! has gone answers `EPIPE` rather than raising the signal a write to a closed socket raises by default, which would end
//! a host process that had not set it aside: Linux asks per send (`MSG_NOSIGNAL`, [`send`]), macOS once, when the pair
//! is made (`SO_NOSIGPIPE`).

use std::os::fd::{BorrowedFd, OwnedFd};

use rustix::io::Errno;

use crate::error::IpcError;

/// A connected pair of anonymous stream sockets: the daemon's end (non-blocking) and the end handed out. Both are
/// close-on-exec. macOS creates a pair with no flags at all, so the flags are set on each end after it is made.
pub fn pair() -> Result<(OwnedFd, OwnedFd), IpcError> {
  use rustix::net::{AddressFamily, SocketFlags, SocketType};
  #[cfg(target_os = "linux")]
  let flags = SocketFlags::CLOEXEC;
  #[cfg(not(target_os = "linux"))]
  let flags = SocketFlags::empty();
  let (daemon, handed) =
    rustix::net::socketpair(AddressFamily::UNIX, SocketType::STREAM, flags, None)
      .map_err(|e| refused("socketpair", e))?;
  #[cfg(not(target_os = "linux"))]
  for end in [&daemon, &handed] {
    rustix::io::fcntl_setfd(end, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl_setfd", e))?;
  }
  #[cfg(target_vendor = "apple")]
  rustix::net::sockopt::set_socket_nosigpipe(&daemon, true)
    .map_err(|e| refused("setsockopt(SO_NOSIGPIPE)", e))?;
  rustix::io::ioctl_fionbio(&daemon, true).map_err(|e| refused("ioctl_fionbio", e))?;
  Ok((daemon, handed))
}

/// Sends what of `bytes` the daemon's end takes now: the bytes taken, `EAGAIN` when its buffer is full, `EPIPE` once
/// the peer has gone (never the signal; see the module doc).
pub fn send(end: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
  #[cfg(target_os = "linux")]
  let flags = rustix::net::SendFlags::NOSIGNAL;
  #[cfg(not(target_os = "linux"))]
  let flags = rustix::net::SendFlags::empty();
  rustix::net::send(end, bytes, flags)
}

/// Format: the descriptors one send carries by `SCM_RIGHTS`: a DAX window's descriptor (Shards part 3).
const SEND_DESCRIPTORS: usize = 1;

/// Sends what of `bytes` the daemon's end takes now, with `descriptor` riding the first byte by `SCM_RIGHTS`, as [`send`]
/// does otherwise: the bytes taken, at least one when it answers at all, so the descriptor went with them; `EAGAIN` when
/// the buffer is full, and nothing went. The peer holds a duplicate once it reads the byte; the caller keeps its own
/// descriptor and closes it when it will. A DAX object's descriptor is a memory object, not a socket, so the kernel's
/// collector of sockets in flight never touches it (the macOS flush of A-161 took a passed socket).
pub fn send_with_descriptor(
  end: BorrowedFd<'_>,
  bytes: &[u8],
  descriptor: BorrowedFd<'_>,
) -> Result<usize, Errno> {
  use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
  #[cfg(target_os = "linux")]
  let flags = rustix::net::SendFlags::NOSIGNAL;
  #[cfg(not(target_os = "linux"))]
  let flags = rustix::net::SendFlags::empty();
  let fds = [descriptor];
  let mut space =
    [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(SEND_DESCRIPTORS))];
  let mut control = SendAncillaryBuffer::new(&mut space);
  if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
    return Err(Errno::NOBUFS);
  }
  rustix::net::sendmsg(end, &[std::io::IoSlice::new(bytes)], &mut control, flags)
}

/// Reads what has arrived on the daemon's end into `into`: the bytes read, zero once the peer has closed its end or
/// this end was [`end`]ed, `EAGAIN` when nothing waits.
pub fn recv(end: BorrowedFd<'_>, into: &mut [u8]) -> Result<usize, Errno> {
  rustix::net::recv(end, into, rustix::net::RecvFlags::empty()).map(|(read, _)| read)
}

/// Ends the conversation on `end` both ways without closing the descriptor: a wait on it wakes and its reads answer
/// zero, so the task serving it learns of the end through the same readiness it serves by, and the peer's reads answer
/// zero. The peer's writes answer `EPIPE` on Linux at once; macOS takes and discards them until the descriptor closes,
/// when its owner drops it, so a peer learns of the end at its next read on both.
pub fn end(end: BorrowedFd<'_>) -> Result<(), Errno> {
  rustix::net::shutdown(end, rustix::net::Shutdown::Both)
}

/// Format: the lowest descriptor number a program may be handed a stream end at: past standard input, output and error,
/// which a program reads and writes as itself.
pub const FIRST_HANDED_NUMBER: i32 = 3;

/// Replaces this process with `program` and `args`, handing it `stream` at descriptor number `at` (§4.6, A-161): what
/// `slates shards` does, so a Shards spawner, or a test, gets its stream end without linking slates. The process a
/// launcher started becomes the program, its pid, its signals and its exit the program's, and when it ends its end
/// closes and the stream ends its attachment. No process is left waiting on it: one would outlive a supervisor's kill
/// of the program, and, killed itself, would leave the program and its attachment running (2026-10-24, the killed
/// launch in `crates/cli/tests/cli.rs`, which `spawn_with`, this function's predecessor, failed).
///
/// Exactly one copy of the end reaches the program, at `at`: `dup2` places it there with close-on-exec cleared, and the
/// copy this function was given is made close-on-exec first. Returns only on failure: a number below
/// [`FIRST_HANDED_NUMBER`] is refused before anything changes; a failed `exec` (the program missing or not executable)
/// is refused after `dup2` has placed the end at `at`, so the caller should then exit, as the CLI does, rather than go
/// on with that number taken.
pub fn exec_with(
  program: &std::ffi::OsStr,
  args: &[std::ffi::OsString],
  stream: OwnedFd,
  at: i32,
) -> IpcError {
  use std::os::fd::AsRawFd;
  use std::os::unix::process::CommandExt;
  if at < FIRST_HANDED_NUMBER {
    return IpcError::Layout {
      reason: "a stream end is handed past standard input, output and error",
    };
  }
  // `dup2` must copy from another number than `at`: a copy onto itself would leave close-on-exec set.
  let source = if stream.as_raw_fd() == at {
    match rustix::io::fcntl_dupfd_cloexec(&stream, FIRST_HANDED_NUMBER) {
      Ok(copy) => copy,
      Err(e) => return refused("fcntl_dupfd_cloexec", e),
    }
  } else {
    stream
  };
  if let Err(e) = rustix::io::fcntl_setfd(&source, rustix::io::FdFlags::CLOEXEC) {
    return refused("fcntl_setfd", e);
  }
  let from = source.as_raw_fd();
  let mut command = std::process::Command::new(program);
  command.args(args);
  // SAFETY: the closure runs in this process just before `execvp`, after the standard library has reset the signal state
  // it changes, and makes one system call, `dup2(from, at)`, on numbers it copied, reading errno on failure: no
  // allocation, no lock, no other state, which is what `pre_exec` asks. `from` is open until the exec returns, since
  // `source` is dropped only after it; `dup2` places a copy at `at`, replacing whatever was open there, with
  // close-on-exec cleared, so the exec keeps that copy while `from`, close-on-exec, closes.
  unsafe {
    command.pre_exec(move || {
      if libc::dup2(from, at) < 0 {
        return Err(std::io::Error::last_os_error());
      }
      Ok(())
    });
  }
  let error = command.exec();
  drop(source);
  IpcError::OsRefused {
    call: "exec",
    code: error.raw_os_error(),
  }
}

fn refused(call: &'static str, e: rustix::io::Errno) -> IpcError {
  IpcError::OsRefused {
    call,
    code: Some(e.raw_os_error()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::os::fd::AsFd;

  /// Do: make a pair, write on the handed end, read on the daemon's; then read the daemon's end empty. Expect the bytes
  /// across, the daemon's end refusing to wait (non-blocking), and both ends close-on-exec.
  #[test]
  fn a_pair_carries_bytes_and_the_daemons_end_never_waits() {
    let (daemon, handed) = pair().unwrap();
    rustix::io::write(&handed, b"frame").unwrap();
    let mut buf = [0u8; 8];
    let read = recv(daemon.as_fd(), &mut buf).unwrap();
    assert_eq!(buf.get(..read), Some(&b"frame"[..]));
    assert_eq!(
      recv(daemon.as_fd(), &mut buf),
      Err(Errno::AGAIN),
      "the daemon's end returns at once when nothing waits"
    );
    for end in [&daemon, &handed] {
      let flags = rustix::io::fcntl_getfd(end).unwrap();
      assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
    }
  }

  /// Do: fill the daemon's end until it refuses, then close the handed end and send again. Expect `EAGAIN` once the
  /// buffer is full (never a wait), and `EPIPE` after the peer has gone, with the test process still alive (no signal).
  #[test]
  fn a_full_end_refuses_at_once_and_a_gone_peer_answers_epipe() {
    let (daemon, handed) = pair().unwrap();
    let chunk = [0u8; 4096];
    let mut sent = 0usize;
    let refused = loop {
      match send(daemon.as_fd(), &chunk) {
        Ok(taken) => sent += taken,
        Err(errno) => break errno,
      }
      assert!(
        sent < 64 << 20,
        "a socket buffer took 64 MiB without refusing"
      );
    };
    assert_eq!(refused, Errno::AGAIN);
    drop(handed);
    assert_eq!(send(daemon.as_fd(), &chunk), Err(Errno::PIPE));
  }

  /// Do: end the daemon's end while the peer still holds its own, then drop it. Expect the daemon's reads to answer
  /// zero (the serving task's own wake) and the peer's reads zero; the peer's writes `EPIPE` at once on Linux, taken and
  /// discarded on macOS until the daemon's descriptor closes, and `EPIPE` on both after.
  #[test]
  fn ending_an_end_ends_both_directions_for_both_sides() {
    let (daemon, handed) = pair().unwrap();
    end(daemon.as_fd()).unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(recv(daemon.as_fd(), &mut buf), Ok(0));
    assert_eq!(rustix::io::read(&handed, &mut buf), Ok(0));
    #[cfg(target_os = "linux")]
    let flags = rustix::net::SendFlags::NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = rustix::net::SendFlags::empty();
    #[cfg(target_vendor = "apple")]
    rustix::net::sockopt::set_socket_nosigpipe(&handed, true).unwrap();
    let before_close = rustix::net::send(&handed, b"x", flags);
    #[cfg(target_os = "linux")]
    assert_eq!(before_close, Err(Errno::PIPE));
    #[cfg(not(target_os = "linux"))]
    assert_eq!(before_close, Ok(1));
    drop(daemon);
    assert_eq!(rustix::net::send(&handed, b"x", flags), Err(Errno::PIPE));
  }
}
