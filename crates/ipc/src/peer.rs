//! The credentials of a Unix socket's peer on Linux (`SO_PEERCRED`, unix(7)), read as the kernel writes them (§4.7
//! "Rendezvous"; `docs/bugs/2026-10-27-a-peer-whose-process-the-daemon-cannot-see-was-refused-by-undefined-behaviour.md`).
//!
//! The kernel reports the peer's process id as the reader's pid namespace sees it, and 0 when the peer's process is not
//! visible from there (pid_namespaces(7)). That is an ordinary placement, not a fault: a client in a container of a pod
//! beside the daemon's container, with no process namespace shared, which is a pod's default; or a VMM in a namespace of
//! its own connecting to a vhost-user device.
//!
//! rustix 1.1's `socket_peercred` returns the id as a `Pid`, a non-zero integer, so a 0 there is an invalid value and
//! its reading is undefined behaviour. It did not stay silent: `Result<UCred, Errno>` keeps its error discriminant in
//! the pid's niche, so the credentials of an unseen peer read back as an error, the uid's low 16 bits taken for an
//! errno (uid 65532, 0xFFFC, read as -4: "SO_PEERCRED refused (code Some(4))", a KIND pod's ephemeral container,
//! 2026-10-27). This module reads the kernel's `struct ucred` as plain integers instead, so 0 stays 0.

use std::os::fd::{AsRawFd, BorrowedFd};

use crate::error::IpcError;

/// A socket peer's credentials, as the reading process's namespaces see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
  /// The peer's process id in the reader's pid namespace; 0 when the peer's process is not visible from it.
  pub pid: u32,
  /// The peer's user id in the reader's user namespace.
  pub uid: u32,
  /// The peer's group id in the reader's user namespace.
  pub gid: u32,
}

/// The credentials of `socket`'s peer (`getsockopt(SOL_SOCKET, SO_PEERCRED)`). Refused `OsRefused` with the call's
/// errno, or `Layout` where the kernel wrote a structure of another size.
pub fn peer_credentials(socket: BorrowedFd<'_>) -> Result<PeerCredentials, IpcError> {
  let mut credentials = libc::ucred {
    pid: 0,
    uid: 0,
    gid: 0,
  };
  let size = std::mem::size_of::<libc::ucred>();
  let mut written = libc::socklen_t::try_from(size).map_err(|_| IpcError::Layout {
    reason: "struct ucred is wider than a socklen_t",
  })?;
  // SAFETY: `getsockopt` writes at most `written` bytes, the size of the `libc::ucred` it is pointed at, a local of
  // plain integers that every bit pattern is valid for; `socket` is a live descriptor for the call.
  let answered = unsafe {
    libc::getsockopt(
      socket.as_raw_fd(),
      libc::SOL_SOCKET,
      libc::SO_PEERCRED,
      (&raw mut credentials).cast::<libc::c_void>(),
      &raw mut written,
    )
  };
  if answered != 0 {
    return Err(IpcError::OsRefused {
      call: "SO_PEERCRED",
      code: std::io::Error::last_os_error().raw_os_error(),
    });
  }
  if usize::try_from(written).ok() != Some(size) {
    return Err(IpcError::Layout {
      reason: "SO_PEERCRED wrote a structure of another size than struct ucred",
    });
  }
  Ok(PeerCredentials {
    // A pid is never negative; were it, it would name no process here, as 0 does.
    pid: u32::try_from(credentials.pid).unwrap_or(0),
    uid: credentials.uid,
    gid: credentials.gid,
  })
}
