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

/// What [`TcpStream::send_records`] did with a run of records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
  /// One send took the whole run.
  Whole,
  /// The kernel took part of the run, and this process sent the rest: the stream ended mid-record for a moment.
  Completed,
}

/// Whether a stream's sends of up to its record bound are whole ([`TcpStream::make_sends_whole`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WholeSends {
  /// Every such send takes all of its bytes or none.
  Guaranteed,
  /// The kernel may take part of a send.
  NotOffered,
}

/// The flags of a [`TcpStream::discard`]: Linux drops the bytes in the kernel; elsewhere they are copied out.
#[cfg(target_os = "linux")]
const DISCARD_FLAGS: rustix::net::RecvFlags = rustix::net::RecvFlags::TRUNC;
/// See the Linux arm.
#[cfg(not(target_os = "linux"))]
const DISCARD_FLAGS: rustix::net::RecvFlags = rustix::net::RecvFlags::empty();

/// Sets an integer socket option `rustix` does not wrap (`SO_RCVLOWAT`, `SO_SNDLOWAT`).
fn set_int_option(
  fd: &OwnedFd,
  level: libc::c_int,
  name: libc::c_int,
  value: i32,
) -> Result<(), rustix::io::Errno> {
  let length = libc::socklen_t::try_from(size_of::<i32>()).unwrap_or(0);
  // SAFETY: `fd` is a live socket this stream owns for the call; the value pointer names an `i32` on this frame that
  // outlives the call, and the length passed is that `i32`'s size, so the kernel reads exactly the bytes it is given.
  let result = unsafe {
    libc::setsockopt(
      fd.as_raw_fd(),
      level,
      name,
      std::ptr::from_ref(&value).cast::<libc::c_void>(),
      length,
    )
  };
  if result == 0 {
    Ok(())
  } else {
    Err(
      rustix::io::Errno::from_io_error(&std::io::Error::last_os_error())
        .unwrap_or(rustix::io::Errno::INVAL),
    )
  }
}

/// Format: the option naming the kernel's connection record and the established state's number in it: Linux
/// `TCP_INFO` with `TCP_ESTABLISHED` (`include/net/tcp_states.h`, 1).
#[cfg(target_os = "linux")]
const TCP_STATE_OPTION: libc::c_int = libc::TCP_INFO;
/// Format: Linux `TCP_ESTABLISHED` (`include/net/tcp_states.h`), the state byte of an established connection.
#[cfg(target_os = "linux")]
const TCP_ESTABLISHED: u8 = 1;
/// Format: macOS `TCP_CONNECTION_INFO` with `TCPS_ESTABLISHED` (`<netinet/tcp_fsm.h>`, 4).
#[cfg(not(target_os = "linux"))]
const TCP_STATE_OPTION: libc::c_int = libc::TCP_CONNECTION_INFO;
/// Format: macOS `TCPS_ESTABLISHED` (`<netinet/tcp_fsm.h>`), the state byte of an established connection.
#[cfg(not(target_os = "linux"))]
const TCP_ESTABLISHED: u8 = 4;

/// The TCP state byte at the head of the kernel's connection record: both kernels copy out as much of the record as the
/// caller's length asks, so one byte reads the state alone.
fn tcp_state(fd: &OwnedFd) -> Result<u8, rustix::io::Errno> {
  let mut state = 0u8;
  let mut length = libc::socklen_t::try_from(size_of::<u8>()).unwrap_or(0);
  // SAFETY: `fd` is a live socket this stream owns for the call; the buffer is one byte on this frame that outlives the
  // call, `length` says so and is a `socklen_t` on this frame the kernel may lower, so it writes at most that byte.
  let result = unsafe {
    libc::getsockopt(
      fd.as_raw_fd(),
      libc::IPPROTO_TCP,
      TCP_STATE_OPTION,
      std::ptr::from_mut(&mut state).cast::<libc::c_void>(),
      &raw mut length,
    )
  };
  if result == 0 && length > 0 {
    Ok(state)
  } else {
    Err(
      rustix::io::Errno::from_io_error(&std::io::Error::last_os_error())
        .unwrap_or(rustix::io::Errno::INVAL),
    )
  }
}

/// The BSD arm of [`TcpStream::make_sends_whole`]: the send buffer must hold the record, then the low-water mark is it.
#[cfg(not(target_os = "linux"))]
fn make_sends_whole(fd: &OwnedFd, record_bytes: usize) -> Result<WholeSends, RtError> {
  let buffer = rustix::net::sockopt::socket_send_buffer_size(fd)
    .map_err(|e| refused("getsockopt(SO_SNDBUF)", e))?;
  if buffer < record_bytes {
    return Err(refused(
      "setsockopt(SO_SNDLOWAT)",
      rustix::io::Errno::NOBUFS,
    ));
  }
  let value = i32::try_from(record_bytes)
    .map_err(|_| refused("setsockopt(SO_SNDLOWAT)", rustix::io::Errno::RANGE))?;
  set_int_option(fd, libc::SOL_SOCKET, libc::SO_SNDLOWAT, value)
    .map_err(|e| refused("setsockopt(SO_SNDLOWAT)", e))?;
  Ok(WholeSends::Guaranteed)
}

/// The Linux arm of [`TcpStream::make_sends_whole`]: `SO_SNDLOWAT` is fixed (socket(7)), and a send copies what fits.
#[cfg(target_os = "linux")]
fn make_sends_whole(_fd: &OwnedFd, _record_bytes: usize) -> Result<WholeSends, RtError> {
  Ok(WholeSends::NotOffered)
}

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

/// Prepares a connected stream (accepted, connected or adopted): non-blocking and close-on-exec, and
/// `TCP_NODELAY`, so a small write leaves at once instead of waiting for the peer to acknowledge the
/// previous one (Nagle, RFC 896). Every stream here carries request/reply traffic that the server already
/// coalesces into one write per batch, so Nagle has nothing left to merge; left on, a reply written while
/// an earlier one is unacknowledged waits for the client's delayed ACK — 40 ms on Linux (`TCP_DELACK_MIN`),
/// measured as the median round of `a_reply_in_two_writes_does_not_wait_for_the_peers_delayed_acknowledgement`
/// on Linux (42 ms before; docs/bugs/2026-10-05-nagle-delayed-ack.md).
fn prepare_stream(fd: &OwnedFd) -> Result<(), RtError> {
  set_nonblocking_cloexec(fd)?;
  rustix::net::sockopt::set_tcp_nodelay(fd, true).map_err(|e| refused("setsockopt(TCP_NODELAY)", e))
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

  /// Adopts an already-bound, listening socket from an existing descriptor — a listener a supervisor
  /// bound and handed over across an exec so its port survives a daemon restart (§4.6, "One TCP
  /// loopback listener held by the anchor"). The descriptor must name a bound, listening INET stream
  /// socket; it is made non-blocking (the driver waits on it) and close-on-exec (the adopting process
  /// does not re-inherit it), then owned here. `fd` is an `OwnedFd`, so the caller has already taken
  /// ownership of the raw descriptor (its own `unsafe` at the inheritance boundary).
  pub fn from_fd(fd: OwnedFd) -> Result<TcpListener, RtError> {
    set_nonblocking_cloexec(&fd)?;
    Ok(TcpListener { fd })
  }

  /// Gives up ownership of the underlying descriptor — the counterpart to [`TcpListener::from_fd`],
  /// for a supervisor that binds the listener, then hands its descriptor to the daemon it spawns so
  /// the port survives a restart (§4.6). The caller owns the returned descriptor and its lifetime.
  pub fn into_fd(self) -> OwnedFd {
    self.fd
  }

  /// The local address the listener is bound to (the OS-assigned port on an ephemeral bind).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    local_v4(&self.fd)
  }

  /// Accepts the next connection, awaiting readability through the driver when none is pending. The
  /// accepted socket is made non-blocking and close-on-exec, like the listener, and `TCP_NODELAY`.
  pub async fn accept(&self) -> Result<TcpStream, RtError> {
    loop {
      match accept(&self.fd) {
        Ok(fd) => {
          prepare_stream(&fd)?;
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
    rustix::net::sockopt::set_tcp_nodelay(&fd, true)
      .map_err(|e| refused("setsockopt(TCP_NODELAY)", e))?;
    Ok(TcpStream { fd })
  }

  /// Gives up the stream's descriptor, to move the connection to another shard (§4.6: a mount's connection is
  /// served on the shard that owns its volume). No readiness is armed between awaits — every registration is
  /// one-shot, re-armed by the next await — so nothing on this shard's driver waits on it once the serving
  /// task stops awaiting it; on epoll a fired one-shot entry stays in this shard's interest list disabled,
  /// never reported, until the descriptor closes.
  pub fn into_fd(self) -> OwnedFd {
    self.fd
  }

  /// Adopts a connected stream's descriptor on the current shard: the counterpart to
  /// [`TcpStream::into_fd`]. It is made non-blocking and close-on-exec, as an accepted stream is.
  pub fn from_fd(fd: OwnedFd) -> Result<TcpStream, RtError> {
    prepare_stream(&fd)?;
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

  /// Awaits readability through the driver: bytes queued, at least as many as the receive low-water mark
  /// ([`TcpStream::want_bytes`]; kqueue's read filter and Linux's `tcp_poll` both honour it), or end of stream. It
  /// borrows no buffer, so a caller can await it and then [`TcpStream::peek_now`] into a buffer it holds only across
  /// that synchronous call.
  pub async fn wait_readable(&self) -> Result<(), RtError> {
    readable(self.fd.as_raw_fd()).await
  }

  /// Copies the bytes at the head of the receive queue into `buf` without consuming them (`MSG_PEEK`), without
  /// waiting: `Some(0)` at end of stream, `None` when nothing is queued. The bytes stay in the kernel until
  /// [`TcpStream::discard`] consumes them, so a process that dies holding a peeked request leaves it for the next owner
  /// of the descriptor (A-113).
  pub fn peek_now(&self, buf: &mut [u8]) -> Result<Option<usize>, RtError> {
    loop {
      match rustix::net::recv(&self.fd, &mut *buf, rustix::net::RecvFlags::PEEK) {
        Ok((n, _)) => return Ok(Some(n)),
        Err(rustix::io::Errno::AGAIN) => return Ok(None),
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("recv(MSG_PEEK)", e)),
      }
    }
  }

  /// Consumes `count` bytes from the head of the receive queue that a [`TcpStream::peek_now`] already saw, so they are
  /// queued and the call never waits. Linux discards them in the kernel (`MSG_TRUNC` on a TCP socket, tcp(7)); elsewhere
  /// they are copied into `scratch` and dropped. A queue holding fewer than `count` bytes is a typed refusal, never a
  /// wait: the caller's view of the queue was wrong.
  pub fn discard(&self, count: usize, scratch: &mut [u8]) -> Result<(), RtError> {
    let mut left = count;
    while left > 0 {
      let take = left.min(scratch.len());
      let into = scratch
        .get_mut(..take)
        .filter(|into| !into.is_empty())
        .ok_or_else(|| refused("recv(discard)", rustix::io::Errno::INVAL))?;
      match rustix::net::recv(&self.fd, into, DISCARD_FLAGS) {
        Ok((0, _)) => return Err(refused("recv(discard)", rustix::io::Errno::PIPE)),
        Ok((n, _)) => left = left.saturating_sub(n.min(take)),
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("recv(discard)", e)),
      }
    }
    Ok(())
  }

  /// Sends `records`, a run of whole RPC records, awaiting writability through the driver until the kernel takes them.
  /// On a stream whose sends are whole ([`TcpStream::make_sends_whole`] answered [`WholeSends::Guaranteed`]) and a run
  /// no longer than its record bound, one send takes all of it or nothing, so a process that dies here leaves the stream
  /// at a record boundary (A-113). A short send is completed by this call and answered [`Sent::Completed`] so the caller
  /// can count it: the stream crossed a moment when it ended mid-record.
  pub async fn send_records(&self, records: &[u8]) -> Result<Sent, RtError> {
    loop {
      match rustix::io::write(&self.fd, records) {
        Ok(0) if !records.is_empty() => return Err(refused("write", rustix::io::Errno::PIPE)),
        Ok(n) if n >= records.len() => return Ok(Sent::Whole),
        Ok(n) => {
          self.write_all(records.get(n..).unwrap_or_default()).await?;
          return Ok(Sent::Completed);
        }
        Err(rustix::io::Errno::AGAIN) => writable(self.fd.as_raw_fd()).await?,
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("write", e)),
      }
    }
  }

  /// Sizes the stream's kernel buffers to hold `buffer_bytes` each way (`SO_SNDBUF`, `SO_RCVBUF`), refused unless the
  /// kernel grants at least that much (it reports what it kept; Linux keeps double, for its bookkeeping). A buffer set
  /// here is no longer auto-tuned.
  pub fn reserve_buffers(&self, buffer_bytes: usize) -> Result<(), RtError> {
    use rustix::net::sockopt;
    sockopt::set_socket_send_buffer_size(&self.fd, buffer_bytes)
      .map_err(|e| refused("setsockopt(SO_SNDBUF)", e))?;
    sockopt::set_socket_recv_buffer_size(&self.fd, buffer_bytes)
      .map_err(|e| refused("setsockopt(SO_RCVBUF)", e))?;
    let send = sockopt::socket_send_buffer_size(&self.fd)
      .map_err(|e| refused("getsockopt(SO_SNDBUF)", e))?;
    let receive = sockopt::socket_recv_buffer_size(&self.fd)
      .map_err(|e| refused("getsockopt(SO_RCVBUF)", e))?;
    if send < buffer_bytes || receive < buffer_bytes {
      return Err(refused(
        "setsockopt(SO_SNDBUF/SO_RCVBUF)",
        rustix::io::Errno::NOBUFS,
      ));
    }
    Ok(())
  }

  /// Makes every send of at most `record_bytes` whole — all of it or none — where the kernel offers that, and says
  /// whether it does. On macOS and the BSDs a non-blocking `sosend` refuses `EWOULDBLOCK`, copying nothing, while the
  /// free space is below both the request and the send low-water mark, and allocates its buffers waiting rather than
  /// failing midway (XNU `bsd/kern/uipc_socket.c` `sosend`, read 2026-10-06); with `SO_SNDLOWAT` at `record_bytes` a
  /// send that size or smaller is whole. The kernel clamps the mark to the send buffer without saying so, so a buffer
  /// smaller than `record_bytes` is refused here rather than trusted. Linux keeps `SO_SNDLOWAT` fixed (socket(7)) and
  /// copies what fits, so it answers [`WholeSends::NotOffered`].
  pub fn make_sends_whole(&self, record_bytes: usize) -> Result<WholeSends, RtError> {
    make_sends_whole(&self.fd, record_bytes)
  }

  /// Sets the receive low-water mark (`SO_RCVLOWAT`): readability is reported once `bytes` are queued (or at end of
  /// stream), so a reader waiting for a whole record is woken once, not for every segment of it.
  pub fn want_bytes(&self, bytes: usize) -> Result<(), RtError> {
    let value = i32::try_from(bytes.max(1)).unwrap_or(i32::MAX);
    set_int_option(&self.fd, libc::SOL_SOCKET, libc::SO_RCVLOWAT, value)
      .map_err(|e| refused("setsockopt(SO_RCVLOWAT)", e))
  }

  /// Whether the connection has left the established state — the peer closed its half (its FIN arrived: close-wait) or
  /// the connection is gone. A reader that peeks and never consumes an unfinished record cannot learn of the peer's close
  /// from a zero-length read, since the unfinished bytes stay queued; it asks this once readability says something
  /// changed (A-113). This process never half-closes a stream it still reads, so every other state means the peer is
  /// done. Read from the kernel's own connection record (Linux `TCP_INFO`, macOS `TCP_CONNECTION_INFO`), whose first byte
  /// is the TCP state.
  pub fn peer_closed(&self) -> Result<bool, RtError> {
    let state = tcp_state(&self.fd).map_err(|e| refused("getsockopt(TCP state)", e))?;
    Ok(state != TCP_ESTABLISHED)
  }

  /// Ends both directions of the connection (`shutdown(SHUT_RDWR)`), whoever else holds a copy of its descriptor: a
  /// close drops only this process's copy, and the connection lives while another process (the anchor) holds one.
  pub fn shutdown(&self) -> Result<(), RtError> {
    rustix::net::shutdown(&self.fd, rustix::net::Shutdown::Both).map_err(|e| refused("shutdown", e))
  }

  /// The stream's descriptor, borrowed: to hand a duplicate to another process by `SCM_RIGHTS`.
  pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
    std::os::fd::AsFd::as_fd(&self.fd)
  }

  /// Writes all of `buf`, awaiting writability through the driver whenever the send buffer is full,
  /// so a stalled peer yields the shard rather than blocking it. Returns when every byte is accepted.
  pub async fn write_all(&self, buf: &[u8]) -> Result<(), RtError> {
    let mut sent = 0;
    while let Some(unsent) = buf.get(sent..).filter(|unsent| !unsent.is_empty()) {
      match rustix::io::write(&self.fd, unsent) {
        // A non-blocking write of a non-empty slice returns bytes written, `EAGAIN`, or an error; a
        // zero here would mean the kernel accepted nothing without blocking, which is a broken pipe.
        Ok(0) => return Err(refused("write", rustix::io::Errno::PIPE)),
        Ok(n) => sent = sent.saturating_add(n),
        Err(rustix::io::Errno::AGAIN) => writable(self.fd.as_raw_fd()).await?,
        Err(rustix::io::Errno::INTR) => continue,
        Err(e) => return Err(refused("write", e)),
      }
    }
    Ok(())
  }
}
