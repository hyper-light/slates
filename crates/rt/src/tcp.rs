//! Async TCP on the runtime (§4.6): the NFS loopback bridge's production server accepts connections
//! and serves each over the executor's own driver — no foreign runtime (R6, D-9). `accept` and `read`
//! await read-readiness through the shard's driver; `write_all` awaits write-readiness so a write to a
//! stalled peer (a soft-mounted NFS client that stopped reading, filling the send buffer) yields the
//! shard instead of blocking it; `connect` awaits write-readiness for the handshake to complete. The
//! socket is a `rustix` TCP socket (not `std::net`, which the lint wall reserves; the address types
//! come through `rustix::net`, the standard types re-exported). Only the readiness-native drivers
//! (kqueue, epoll) back it; TCP is host-local, so unlike the UDP fleet plane (§4.10a) it has no
//! simulated fabric — it runs on a real runtime.

use std::os::fd::{AsRawFd, OwnedFd};

use rustix::net::{
  AddressFamily, SocketAddr, SocketFlags, SocketType, accept, bind, connect, getsockname, listen,
  socket_with,
};

// The socket address types are `rustix::net`'s (the standard `core::net` types, re-exported); re-export
// them here so a consumer of this API can name a bind address without depending on `rustix` directly.
pub use rustix::net::{Ipv4Addr, SocketAddrV4};

use crate::driver::refused;
use crate::error::RtError;
use crate::readiness::{readable, writable};

/// A listening TCP socket on the runtime: a non-blocking `rustix` socket whose `accept` awaits the
/// shard's driver for the next connection.
#[derive(Debug)]
pub struct TcpListener {
  fd: OwnedFd,
}

/// A connected TCP stream on the runtime: a non-blocking `rustix` socket whose `read` and `write_all`
/// await the shard's driver.
#[derive(Debug)]
pub struct TcpStream {
  fd: OwnedFd,
}

/// Creates a non-blocking, close-on-exec INET stream socket. `SocketFlags` on `socket()` is
/// Linux-only, so CLOEXEC and non-blocking are set after creation (as the UDP socket does): the
/// driver provides the waiting, and the descriptor is not inherited across an exec.
fn stream_socket() -> Result<OwnedFd, RtError> {
  let fd = socket_with(
    AddressFamily::INET,
    SocketType::STREAM,
    SocketFlags::empty(),
    None,
  )
  .map_err(|e| refused("socket(STREAM)", e))?;
  set_nonblocking_cloexec(&fd)?;
  Ok(fd)
}

/// Marks `fd` close-on-exec and non-blocking (applied to a fresh socket and to each accepted one,
/// which does not inherit non-blocking on macOS/BSD).
fn set_nonblocking_cloexec(fd: &OwnedFd) -> Result<(), RtError> {
  rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
    .map_err(|e| refused("fcntl(CLOEXEC)", e))?;
  rustix::io::ioctl_fionbio(fd, true).map_err(|e| refused("ioctl(FIONBIO)", e))?;
  Ok(())
}

/// The bound address of `fd` as an IPv4 socket address.
fn local_v4(fd: &OwnedFd) -> Result<SocketAddrV4, RtError> {
  match SocketAddr::try_from(getsockname(fd).map_err(|e| refused("getsockname", e))?) {
    Ok(SocketAddr::V4(v4)) => Ok(v4),
    _ => Err(refused("getsockname", rustix::io::Errno::AFNOSUPPORT)),
  }
}

impl TcpListener {
  /// Binds a non-blocking listening socket to `addr` (use port 0 for an OS-assigned port, then
  /// [`TcpListener::local_addr`]) with a `backlog` of pending connections the kernel queues before
  /// the accept loop takes them — the caller derives it from its connection fan-in, and the OS clamps
  /// it to the system maximum.
  pub fn bind(addr: SocketAddrV4, backlog: i32) -> Result<TcpListener, RtError> {
    let fd = stream_socket()?;
    bind(&fd, &addr).map_err(|e| refused("bind", e))?;
    listen(&fd, backlog).map_err(|e| refused("listen", e))?;
    Ok(TcpListener { fd })
  }

  /// The local address the listener is bound to (the OS-assigned port on an ephemeral bind).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    local_v4(&self.fd)
  }

  /// Accepts the next connection, awaiting readability through the driver when none is pending. The
  /// accepted socket is made non-blocking and close-on-exec, like the listener.
  pub async fn accept(&self) -> Result<TcpStream, RtError> {
    loop {
      match accept(&self.fd) {
        Ok(fd) => {
          set_nonblocking_cloexec(&fd)?;
          return Ok(TcpStream { fd });
        }
        Err(rustix::io::Errno::AGAIN) => readable(self.fd.as_raw_fd()).await?,
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("accept", e)),
      }
    }
  }
}

impl TcpStream {
  /// Connects to `addr`, awaiting the driver until the handshake completes. A non-blocking `connect`
  /// returns immediately with `EINPROGRESS`; the socket becomes writable when the handshake finishes,
  /// and a pending socket error (a refused connection) surfaces then, as a typed refusal.
  pub async fn connect(addr: SocketAddrV4) -> Result<TcpStream, RtError> {
    let fd = stream_socket()?;
    match connect(&fd, &addr) {
      Ok(()) => {}
      Err(rustix::io::Errno::INPROGRESS) => {
        writable(fd.as_raw_fd()).await?;
        if let Err(err) =
          rustix::net::sockopt::socket_error(&fd).map_err(|e| refused("getsockopt(SO_ERROR)", e))?
        {
          return Err(refused("connect", err));
        }
      }
      Err(e) => return Err(refused("connect", e)),
    }
    Ok(TcpStream { fd })
  }

  /// The local address this stream is bound to.
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    local_v4(&self.fd)
  }

  /// Reads into `buf`, awaiting readability through the driver when nothing is ready; returns the
  /// byte count, or zero at end of stream (the peer closed its write half).
  pub async fn read(&self, buf: &mut [u8]) -> Result<usize, RtError> {
    loop {
      match rustix::io::read(&self.fd, &mut *buf) {
        Ok(n) => return Ok(n),
        Err(rustix::io::Errno::AGAIN) => readable(self.fd.as_raw_fd()).await?,
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("read", e)),
      }
    }
  }

  /// Writes all of `buf`, awaiting writability through the driver whenever the send buffer is full,
  /// so a stalled peer yields the shard rather than blocking it. Returns when every byte is accepted.
  pub async fn write_all(&self, buf: &[u8]) -> Result<(), RtError> {
    let mut sent = 0;
    while sent < buf.len() {
      match rustix::io::write(&self.fd, &buf[sent..]) {
        // A non-blocking write of a non-empty slice returns bytes written, `EAGAIN`, or an error; a
        // zero here would mean the kernel accepted nothing without blocking, which is a broken pipe.
        Ok(0) => return Err(refused("write", rustix::io::Errno::PIPE)),
        Ok(n) => sent += n,
        Err(rustix::io::Errno::AGAIN) => writable(self.fd.as_raw_fd()).await?,
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("write", e)),
      }
    }
    Ok(())
  }
}
