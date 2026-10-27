//! The platform socket seam (§4.10a, §4.6; the vorpal `mem/src/policy.rs` shape: paired `#[cfg]`
//! functions with identical signatures, so `udp` and `tcp` hold the async logic cfg-free and only the
//! syscalls differ). A [`Socket`] owns one non-blocking INET socket, a datagram or a stream one; the free
//! functions are the blocking-free primitives the two types drive, each returning [`Io`] so "not ready
//! yet" is a value the caller awaits on, never an error to classify.
//!
//! Both cross platforms. The fleet claims plane rides slates's owned QUIC dialect over the datagram socket
//! (`crates/transport`, `rustls::quic`), and the NFS servers, the MCP HTTP edge and the network export ride
//! the stream socket, so each runs wherever a daemon does (`docs/wip/platforms.md`: until 2026-10-26 the
//! stream half was Unix's alone, and every TCP service was missing on Windows).
//!
//! The Unix arm is `rustix` (the lint wall reserves `std::net`; the address types are `core::net`'s,
//! which `rustix::net` re-exports); the Windows arm is Winsock 2 (`windows-sys`). Both present one
//! surface, so the readiness-native drivers back either — kqueue/epoll on Unix, the AFD reactor
//! (`crate::afd`) on the IOCP driver.

// The public address types are `core::net`'s on every platform (rustix re-exports the same ones), so a
// caller names a bind address without depending on the socket backend.
pub(crate) use core::net::{Ipv4Addr, SocketAddrV4};

/// The outcome of a non-blocking receive: bytes and a sender, or a signal to await readiness and retry.
pub(crate) enum Io<T> {
  /// The operation completed with this value.
  Ready(T),
  /// The socket is not ready (`EAGAIN`/`WSAEWOULDBLOCK`); await the read edge through the driver.
  WouldBlock,
  /// The call was interrupted before doing anything (`EINTR`); retry at once.
  Interrupted,
}

// ============================================================================== Unix (rustix)

#[cfg(unix)]
mod imp {
  use std::os::fd::{AsRawFd, OwnedFd};

  use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddr, SocketFlags, SocketType, bind as rx_bind,
    getsockname, recvfrom, sendto as rx_sendto, socket_with,
  };

  use super::{Io, Ipv4Addr, SocketAddrV4};
  use crate::driver::refused;
  use crate::error::RtError;

  /// A non-blocking OS socket owned here, a datagram or a stream one (closed on drop, by `OwnedFd`).
  #[derive(Debug)]
  pub(crate) struct Socket {
    fd: OwnedFd,
  }

  impl Socket {
    /// The readiness handle a `crate::readiness` future registers (the raw fd).
    pub(crate) fn raw_id(&self) -> i32 {
      self.fd.as_raw_fd()
    }
  }

  /// The socket's kernel receive buffer (`SO_RCVBUF`) in bytes.
  pub(crate) fn recv_buffer_bytes(socket: &Socket) -> Result<usize, RtError> {
    rustix::net::sockopt::socket_recv_buffer_size(&socket.fd).map_err(|e| refused("getsockopt", e))
  }

  pub(crate) fn dgram_socket() -> Result<Socket, RtError> {
    let fd = new_socket(SocketType::DGRAM, "socket(DGRAM)")?;
    set_dont_fragment(&fd)?;
    Ok(Socket { fd })
  }

  /// A non-blocking, close-on-exec INET socket of `kind`, made so in the one call on Linux (`SOCK_CLOEXEC |
  /// SOCK_NONBLOCK`, socket(2)): a process another thread starts between a `socket()` and a later `fcntl` would keep a
  /// copy for its whole life, its copy never marked. `call` names the creation in a refusal.
  #[cfg(target_os = "linux")]
  fn new_socket(kind: SocketType, call: &'static str) -> Result<OwnedFd, RtError> {
    socket_with(
      AddressFamily::INET,
      kind,
      SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused(call, e))
  }

  /// A non-blocking, close-on-exec INET socket of `kind`: macOS's `socket()` takes no such flags, so both are set right
  /// after it, and a process another thread starts in that window can keep a copy (Apple offers no atomic form).
  #[cfg(not(target_os = "linux"))]
  fn new_socket(kind: SocketType, call: &'static str) -> Result<OwnedFd, RtError> {
    let fd = socket_with(AddressFamily::INET, kind, SocketFlags::empty(), None)
      .map_err(|e| refused(call, e))?;
    set_nonblocking_cloexec(&fd)?;
    Ok(fd)
  }

  /// Sets the don't-fragment bit on every datagram the socket sends, and has the kernel leave the
  /// sizing to the transport (RFC 8899 §3: a packetization layer that probes the path needs its probes
  /// dropped, not fragmented, when too large; §4.4: a datagram larger than the local interface then
  /// fails its send with `EMSGSIZE`). Linux: `IP_PMTUDISC_PROBE` sets DF and ignores the kernel's own
  /// path-MTU cache, so the transport's probes decide.
  #[cfg(target_os = "linux")]
  fn set_dont_fragment(fd: &OwnedFd) -> Result<(), RtError> {
    rustix::net::sockopt::set_ip_mtu_discover(fd, rustix::net::sockopt::Ipv4PathMtuDiscovery::PROBE)
      .map_err(|e| refused("setsockopt(IP_MTU_DISCOVER)", e))
  }

  /// macOS: `IP_DONTFRAG` sets DF on each datagram (see the Linux arm).
  #[cfg(target_os = "macos")]
  fn set_dont_fragment(fd: &OwnedFd) -> Result<(), RtError> {
    let on: libc::c_int = 1;
    let len = libc::socklen_t::try_from(size_of::<libc::c_int>()).unwrap_or(libc::socklen_t::MAX);
    // SAFETY: `IP_DONTFRAG` takes an `int`; the pointer and length name one live local `c_int`, and the
    // descriptor is this socket's, open for the call.
    let outcome = unsafe {
      libc::setsockopt(
        fd.as_raw_fd(),
        libc::IPPROTO_IP,
        libc::IP_DONTFRAG,
        (&raw const on).cast::<libc::c_void>(),
        len,
      )
    };
    if outcome == 0 {
      Ok(())
    } else {
      Err(RtError::os("setsockopt(IP_DONTFRAG)"))
    }
  }

  pub(crate) fn bind(socket: &Socket, addr: SocketAddrV4) -> Result<(), RtError> {
    rx_bind(&socket.fd, &addr).map_err(|e| refused("bind", e))
  }

  /// An already-bound OS socket the caller hands over (the socket-activation shape): the port was never
  /// released between the caller's bind and this adoption, so nothing can take it in between. Refused
  /// unless it is a datagram socket; made close-on-exec and non-blocking like one this seam created.
  pub(crate) fn adopt(fd: OwnedDatagram) -> Result<Socket, RtError> {
    let kind =
      rustix::net::sockopt::socket_type(&fd).map_err(|e| refused("getsockopt(SO_TYPE)", e))?;
    if kind != SocketType::DGRAM {
      return Err(refused("adopt(SO_TYPE)", rustix::io::Errno::PROTOTYPE));
    }
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl(CLOEXEC)", e))?;
    rustix::io::ioctl_fionbio(&fd, true).map_err(|e| refused("ioctl(FIONBIO)", e))?;
    set_dont_fragment(&fd)?;
    Ok(Socket { fd })
  }

  /// The owned OS handle [`adopt`] takes: a file descriptor.
  pub(crate) type OwnedDatagram = OwnedFd;

  /// Gives up the socket's descriptor — the counterpart to [`adopt`], for a supervisor that binds a
  /// socket and hands it to the process it spawns.
  pub(crate) fn into_owned(socket: Socket) -> OwnedDatagram {
    socket.fd
  }

  pub(crate) fn local_addr(socket: &Socket) -> Result<SocketAddrV4, RtError> {
    match SocketAddr::try_from(getsockname(&socket.fd).map_err(|e| refused("getsockname", e))?) {
      Ok(SocketAddr::V4(v4)) => Ok(v4),
      _ => Err(refused("getsockname", rustix::io::Errno::AFNOSUPPORT)),
    }
  }

  pub(crate) fn recv_from(
    socket: &Socket,
    buf: &mut [u8],
  ) -> Result<Io<(usize, SocketAddrV4)>, RtError> {
    match recvfrom(&socket.fd, buf, RecvFlags::empty()) {
      Ok((n, _flags, Some(from))) => match SocketAddr::try_from(from) {
        Ok(SocketAddr::V4(v4)) => Ok(Io::Ready((n, v4))),
        _ => Err(refused("recvfrom", rustix::io::Errno::AFNOSUPPORT)),
      },
      Ok((n, _flags, None)) => Ok(Io::Ready((n, SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("recvfrom", e)),
    }
  }

  pub(crate) fn send_to(
    socket: &Socket,
    buf: &[u8],
    addr: SocketAddrV4,
  ) -> Result<Io<usize>, RtError> {
    match rx_sendto(&socket.fd, buf, SendFlags::empty(), &addr) {
      Ok(n) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("sendto", e)),
    }
  }

  // ---------------------------------------------------------------------------- streams (TCP)

  /// The owned OS handle a stream or a listener is handed over as: a file descriptor.
  pub(crate) type OwnedStream = OwnedFd;

  /// The flags of a [`discard`]: Linux drops the bytes in the kernel; elsewhere they are copied out.
  #[cfg(target_os = "linux")]
  const DISCARD_FLAGS: RecvFlags = RecvFlags::TRUNC;
  /// See the Linux arm.
  #[cfg(not(target_os = "linux"))]
  const DISCARD_FLAGS: RecvFlags = RecvFlags::empty();

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

  /// Marks `fd` close-on-exec and non-blocking (applied to a fresh socket and to each accepted one, which does not
  /// inherit non-blocking on macOS/BSD).
  fn set_nonblocking_cloexec(fd: &OwnedFd) -> Result<(), RtError> {
    rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl(CLOEXEC)", e))?;
    rustix::io::ioctl_fionbio(fd, true).map_err(|e| refused("ioctl(FIONBIO)", e))?;
    Ok(())
  }

  /// A connected stream made ready to serve: non-blocking, close-on-exec and `TCP_NODELAY` ([`set_nodelay`]).
  fn prepared(fd: OwnedFd) -> Result<Socket, RtError> {
    set_nonblocking_cloexec(&fd)?;
    let socket = Socket { fd };
    set_nodelay(&socket)?;
    Ok(socket)
  }

  /// Creates a non-blocking, close-on-exec INET stream socket ([`new_socket`]): the driver provides the waiting, and
  /// the descriptor is not inherited across an exec.
  pub(crate) fn stream_socket() -> Result<Socket, RtError> {
    Ok(Socket {
      fd: new_socket(SocketType::STREAM, "socket(STREAM)")?,
    })
  }

  pub(crate) fn listen(socket: &Socket, backlog: i32) -> Result<(), RtError> {
    rustix::net::listen(&socket.fd, backlog).map_err(|e| refused("listen", e))
  }

  /// The next pending connection on `listener`, made ready to serve: on Linux close-on-exec and non-blocking as it is
  /// accepted (`accept4`, the reason [`new_socket`] gives), then `TCP_NODELAY`.
  #[cfg(target_os = "linux")]
  pub(crate) fn accept(listener: &Socket) -> Result<Io<Socket>, RtError> {
    match rustix::net::accept_with(&listener.fd, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
      Ok(fd) => {
        let socket = Socket { fd };
        set_nodelay(&socket)?;
        Ok(Io::Ready(socket))
      }
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("accept4", e)),
    }
  }

  /// The next pending connection on `listener`, made ready to serve ([`prepared`]): macOS has no `accept4`, so the
  /// flags are set right after the accept.
  #[cfg(not(target_os = "linux"))]
  pub(crate) fn accept(listener: &Socket) -> Result<Io<Socket>, RtError> {
    match rustix::net::accept(&listener.fd) {
      Ok(fd) => prepared(fd).map(Io::Ready),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("accept", e)),
    }
  }

  /// Begins `socket`'s connection to `addr`: `Ready` when it connected at once, `WouldBlock` while the handshake
  /// runs (`EINPROGRESS`), when the caller awaits writability and then asks [`connected`].
  pub(crate) fn connect(socket: &Socket, addr: SocketAddrV4) -> Result<Io<()>, RtError> {
    match rustix::net::connect(&socket.fd, &addr) {
      Ok(()) => Ok(Io::Ready(())),
      Err(rustix::io::Errno::INPROGRESS) => Ok(Io::WouldBlock),
      Err(e) => Err(refused("connect", e)),
    }
  }

  /// How a handshake that ran ended: the socket's pending error (`SO_ERROR`), a refused connection among them.
  pub(crate) fn connected(socket: &Socket) -> Result<(), RtError> {
    match rustix::net::sockopt::socket_error(&socket.fd)
      .map_err(|e| refused("getsockopt(SO_ERROR)", e))?
    {
      Ok(()) => Ok(()),
      Err(err) => Err(refused("connect", err)),
    }
  }

  /// `TCP_NODELAY`, so a small write leaves at once instead of waiting for the peer to acknowledge the previous one
  /// (Nagle, RFC 896; `crate::tcp` says why every stream here wants it).
  pub(crate) fn set_nodelay(socket: &Socket) -> Result<(), RtError> {
    rustix::net::sockopt::set_tcp_nodelay(&socket.fd, true)
      .map_err(|e| refused("setsockopt(TCP_NODELAY)", e))
  }

  pub(crate) fn read(socket: &Socket, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
    match rustix::io::read(&socket.fd, buf) {
      Ok(n) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("read", e)),
    }
  }

  pub(crate) fn write(socket: &Socket, buf: &[u8]) -> Result<Io<usize>, RtError> {
    match rustix::io::write(&socket.fd, buf) {
      Ok(n) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("write", e)),
    }
  }

  pub(crate) fn write_vectored(
    socket: &Socket,
    slices: &[std::io::IoSlice<'_>],
  ) -> Result<Io<usize>, RtError> {
    match rustix::io::writev(&socket.fd, slices) {
      Ok(n) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("writev", e)),
    }
  }

  /// The bytes at the head of the receive queue copied into `buf`, left queued (`MSG_PEEK`).
  pub(crate) fn peek(socket: &Socket, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
    match rustix::net::recv(&socket.fd, buf, RecvFlags::PEEK) {
      Ok((n, _)) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("recv(MSG_PEEK)", e)),
    }
  }

  /// Up to `into.len()` bytes consumed from the head of the receive queue and dropped: in the kernel on Linux
  /// (`MSG_TRUNC` on a TCP socket, tcp(7)), copied into `into` elsewhere.
  pub(crate) fn discard(socket: &Socket, into: &mut [u8]) -> Result<Io<usize>, RtError> {
    match rustix::net::recv(&socket.fd, into, DISCARD_FLAGS) {
      Ok((n, _)) => Ok(Io::Ready(n)),
      Err(rustix::io::Errno::AGAIN) => Ok(Io::WouldBlock),
      Err(rustix::io::Errno::INTR) => Ok(Io::Interrupted),
      Err(e) => Err(refused("recv(discard)", e)),
    }
  }

  /// Sets an integer socket option `rustix` does not wrap (`SO_RCVLOWAT`, `SO_SNDLOWAT`).
  fn set_int_option(
    fd: &OwnedFd,
    level: libc::c_int,
    name: libc::c_int,
    value: i32,
  ) -> Result<(), rustix::io::Errno> {
    let length = libc::socklen_t::try_from(size_of::<i32>()).unwrap_or(0);
    // SAFETY: `fd` is a live socket this stream owns for the call; the value pointer names an `i32` on this frame
    // that outlives the call, and the length passed is that `i32`'s size, so the kernel reads exactly the bytes it is
    // given.
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

  /// Reads a socket option `rustix` does not wrap into `into` (the kernel copies at most its length; an option whose
  /// value it shortens below that length is refused, since the caller reads every byte it asked for).
  fn get_option(
    fd: &OwnedFd,
    level: libc::c_int,
    name: libc::c_int,
    into: &mut [u8],
  ) -> Result<(), rustix::io::Errno> {
    let asked = libc::socklen_t::try_from(into.len()).map_err(|_| rustix::io::Errno::INVAL)?;
    let mut length = asked;
    // SAFETY: `fd` is a live socket this stream owns for the call; `into` is a buffer the caller owns for the call,
    // and `length` (on this frame, which the kernel may lower) says its size, so the kernel writes at most those bytes.
    let result = unsafe {
      libc::getsockopt(
        fd.as_raw_fd(),
        level,
        name,
        into.as_mut_ptr().cast::<libc::c_void>(),
        &raw mut length,
      )
    };
    if result != 0 {
      return Err(
        rustix::io::Errno::from_io_error(&std::io::Error::last_os_error())
          .unwrap_or(rustix::io::Errno::INVAL),
      );
    }
    if length < asked {
      return Err(rustix::io::Errno::INVAL);
    }
    Ok(())
  }

  /// The BSD arm of [`reserve_buffers`]: both buffers set and read back.
  #[cfg(not(target_os = "linux"))]
  pub(crate) fn reserve_buffers(socket: &Socket, buffer_bytes: usize) -> Result<(), RtError> {
    use rustix::net::sockopt;
    let fd = &socket.fd;
    sockopt::set_socket_send_buffer_size(fd, buffer_bytes)
      .map_err(|e| refused("setsockopt(SO_SNDBUF)", e))?;
    sockopt::set_socket_recv_buffer_size(fd, buffer_bytes)
      .map_err(|e| refused("setsockopt(SO_RCVBUF)", e))?;
    let send =
      sockopt::socket_send_buffer_size(fd).map_err(|e| refused("getsockopt(SO_SNDBUF)", e))?;
    let receive =
      sockopt::socket_recv_buffer_size(fd).map_err(|e| refused("getsockopt(SO_RCVBUF)", e))?;
    if send < buffer_bytes || receive < buffer_bytes {
      return Err(refused(
        "setsockopt(SO_SNDBUF/SO_RCVBUF)",
        rustix::io::Errno::NOBUFS,
      ));
    }
    Ok(())
  }

  /// The Linux arm of [`reserve_buffers`]: the receive buffer grown once, at admission, by raising the low-water mark
  /// to `buffer_bytes` and setting it back (`tcp_set_rcvlowat` raises an unlocked socket's buffer to fit the mark, up
  /// to `tcp_rmem[2]`, with no privilege; `SO_RCVBUF` is capped by `rmem_max` and would lock autotuning). Without it a
  /// reader that peeks and never consumes (A-113) can deadlock: autotuning grows the buffer from what the application
  /// consumes, so it stays at `tcp_rmem`'s default, and the kernel's per-segment overhead filled that with one third of
  /// a record (44,416 bytes of payload charged as 129,984 against a 131,072-byte buffer, Linux 6.12). The window
  /// closes, the record never completes, and the NFS client waited about 10 s and dialed again
  /// (`docs/bugs/2026-10-08-the-first-large-nfs-write-on-linux-stalls-ten-seconds.md`). The send buffer autotunes.
  #[cfg(target_os = "linux")]
  pub(crate) fn reserve_buffers(socket: &Socket, buffer_bytes: usize) -> Result<(), RtError> {
    let value = i32::try_from(buffer_bytes).unwrap_or(i32::MAX);
    set_int_option(&socket.fd, libc::SOL_SOCKET, libc::SO_RCVLOWAT, value)
      .map_err(|e| refused("setsockopt(SO_RCVLOWAT)", e))?;
    set_int_option(&socket.fd, libc::SOL_SOCKET, libc::SO_RCVLOWAT, 1)
      .map_err(|e| refused("setsockopt(SO_RCVLOWAT)", e))
  }

  /// The BSD arm of [`make_sends_whole`]: the send buffer must hold the record, then the low-water mark is it.
  /// Whether every send of at most `record_bytes` is now whole.
  #[cfg(not(target_os = "linux"))]
  pub(crate) fn make_sends_whole(socket: &Socket, record_bytes: usize) -> Result<bool, RtError> {
    let buffer = rustix::net::sockopt::socket_send_buffer_size(&socket.fd)
      .map_err(|e| refused("getsockopt(SO_SNDBUF)", e))?;
    if buffer < record_bytes {
      return Err(refused(
        "setsockopt(SO_SNDLOWAT)",
        rustix::io::Errno::NOBUFS,
      ));
    }
    let value = i32::try_from(record_bytes)
      .map_err(|_| refused("setsockopt(SO_SNDLOWAT)", rustix::io::Errno::RANGE))?;
    set_int_option(&socket.fd, libc::SOL_SOCKET, libc::SO_SNDLOWAT, value)
      .map_err(|e| refused("setsockopt(SO_SNDLOWAT)", e))?;
    Ok(true)
  }

  /// The Linux arm of [`make_sends_whole`]: `SO_SNDLOWAT` is fixed (socket(7)), and a send copies what fits.
  #[cfg(target_os = "linux")]
  pub(crate) fn make_sends_whole(_socket: &Socket, _record_bytes: usize) -> Result<bool, RtError> {
    Ok(false)
  }

  /// Sets the receive low-water mark (`SO_RCVLOWAT`) to `bytes` and reads it back, refused where the kernel kept less.
  pub(crate) fn want_bytes(socket: &Socket, bytes: usize) -> Result<(), RtError> {
    let value = i32::try_from(bytes.max(1)).unwrap_or(i32::MAX);
    set_int_option(&socket.fd, libc::SOL_SOCKET, libc::SO_RCVLOWAT, value)
      .map_err(|e| refused("setsockopt(SO_RCVLOWAT)", e))?;
    let mut kept = [0u8; size_of::<i32>()];
    get_option(&socket.fd, libc::SOL_SOCKET, libc::SO_RCVLOWAT, &mut kept)
      .map_err(|e| refused("getsockopt(SO_RCVLOWAT)", e))?;
    if i32::from_ne_bytes(kept) < value {
      return Err(refused(
        "setsockopt(SO_RCVLOWAT)",
        rustix::io::Errno::NOBUFS,
      ));
    }
    Ok(())
  }

  /// Whether the connection is still established, from the kernel's own connection record (Linux `TCP_INFO`, macOS
  /// `TCP_CONNECTION_INFO`): both copy out as much of it as the caller's length asks, and its first byte is the state.
  pub(crate) fn established(socket: &Socket) -> Result<bool, RtError> {
    let mut state = [0u8; 1];
    get_option(&socket.fd, libc::IPPROTO_TCP, TCP_STATE_OPTION, &mut state)
      .map_err(|e| refused("getsockopt(TCP state)", e))?;
    Ok(state == [TCP_ESTABLISHED])
  }

  /// Ends both directions of the connection, whoever else holds a copy of its descriptor.
  pub(crate) fn shutdown(socket: &Socket) -> Result<(), RtError> {
    rustix::net::shutdown(&socket.fd, rustix::net::Shutdown::Both)
      .map_err(|e| refused("shutdown", e))
  }

  /// Ends this side's sending (`SHUT_WR`) and keeps its receiving: the peer reads end of stream after every byte sent.
  pub(crate) fn shutdown_write(socket: &Socket) -> Result<(), RtError> {
    rustix::net::shutdown(&socket.fd, rustix::net::Shutdown::Write)
      .map_err(|e| refused("shutdown(SHUT_WR)", e))
  }

  /// A listening socket handed over (a supervisor's, across an exec), made non-blocking and close-on-exec.
  pub(crate) fn adopt_listener(fd: OwnedStream) -> Result<Socket, RtError> {
    set_nonblocking_cloexec(&fd)?;
    Ok(Socket { fd })
  }

  /// A connected stream handed over (from another shard, or across an exec), made ready to serve ([`prepared`]).
  pub(crate) fn adopt_stream(fd: OwnedStream) -> Result<Socket, RtError> {
    prepared(fd)
  }

  /// The socket's descriptor, borrowed: to hand a duplicate to another process by `SCM_RIGHTS`.
  pub(crate) fn as_fd(socket: &Socket) -> std::os::fd::BorrowedFd<'_> {
    std::os::fd::AsFd::as_fd(&socket.fd)
  }

  /// A send the kernel took nothing of without blocking: the stream is broken (`EPIPE`).
  pub(crate) fn broken_pipe(call: &'static str) -> RtError {
    refused(call, rustix::io::Errno::PIPE)
  }

  /// A request the caller's own arguments made impossible (`EINVAL`).
  pub(crate) fn invalid(call: &'static str) -> RtError {
    refused(call, rustix::io::Errno::INVAL)
  }

  /// A call that would have had to wait where the caller knew it would not (`EAGAIN`).
  pub(crate) fn would_block(call: &'static str) -> RtError {
    refused(call, rustix::io::Errno::AGAIN)
  }
}

// ============================================================================== Windows (Winsock 2)

#[cfg(windows)]
mod imp {
  use std::os::windows::io::{FromRawSocket, IntoRawSocket};
  use std::sync::OnceLock;

  use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
  use windows_sys::Win32::Networking::WinSock::{
    AF_INET, FIONBIO, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IP_DONTFRAGMENT, IPPROTO_IP, IPPROTO_TCP,
    IPPROTO_UDP, MSG_PEEK, SD_BOTH, SD_SEND, SIO_TCP_INFO, SO_ERROR, SO_EXCLUSIVEADDRUSE,
    SO_RCVBUF, SO_RCVLOWAT, SO_SNDBUF, SO_TYPE, SOCK_DGRAM, SOCK_STREAM, SOCKADDR, SOCKADDR_IN,
    SOCKET, SOCKET_ERROR, SOL_SOCKET, TCP_INFO_v0, TCP_NODELAY, TCPSTATE_ESTABLISHED,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSABUF, WSADATA, WSAEINTR, WSAEINVAL,
    WSAENOBUFS, WSAESHUTDOWN, WSAEWOULDBLOCK, WSAGetLastError, WSAIoctl, WSASend, WSASocketW,
    WSAStartup, accept as ws_accept, bind as ws_bind, closesocket, connect as ws_connect,
    getsockname, getsockopt, ioctlsocket, listen as ws_listen, recv as ws_recv,
    recvfrom as ws_recvfrom, send as ws_send, sendto as ws_sendto, setsockopt,
    shutdown as ws_shutdown,
  };

  use super::{Io, Ipv4Addr, SocketAddrV4};
  use crate::error::RtError;

  /// A non-blocking Winsock socket owned here, a datagram or a stream one (closed on drop). Held as the raw
  /// `SOCKET`; the drop closes it once. Not `Copy`, so ownership is single.
  #[derive(Debug)]
  pub(crate) struct Socket {
    socket: SOCKET,
  }

  impl Drop for Socket {
    fn drop(&mut self) {
      // SAFETY: our socket, made by `WSASocketW`, accepted or adopted; closed exactly once here.
      unsafe { closesocket(self.socket) };
    }
  }

  /// Winsock must be initialized once per process before any socket call; `WSAStartup(2.2)` and no
  /// matching cleanup (process-lifetime) means one success suffices. A `OnceLock` makes it
  /// exactly-once with no lock on the data path.
  fn ensure_started() -> Result<(), RtError> {
    /// Format: the Winsock version to request in `WSAStartup` — 2.2, low byte major, high byte minor
    /// (`MAKEWORD(2, 2)` = `0x0202`), the version every current Windows provides.
    const WINSOCK_VERSION_2_2: u16 = 0x0202;
    static STARTED: OnceLock<bool> = OnceLock::new();
    let ok = *STARTED.get_or_init(|| {
      // SAFETY: an all-zero WSADATA is a valid, uninitialized out-param.
      let mut data: WSADATA = unsafe { std::mem::zeroed() };
      // SAFETY: `data` is a live, writable WSADATA; `WSAStartup` fills it and returns 0 on success.
      unsafe { WSAStartup(WINSOCK_VERSION_2_2, &mut data) == 0 }
    });
    if ok {
      Ok(())
    } else {
      Err(RtError::os("WSAStartup"))
    }
  }

  /// The last Winsock error as an `RtError`, carrying the `WSAGetLastError` code (the shape
  /// `RtError::os` gives on Unix).
  fn last(call: &'static str) -> RtError {
    RtError::DriverRefused {
      call,
      // SAFETY: a pure query of thread-local last-error state.
      code: Some(unsafe { WSAGetLastError() }),
    }
  }

  /// `WSAGetLastError` for the current thread (to classify would-block/interrupted before mapping).
  fn last_code() -> i32 {
    // SAFETY: a pure query of thread-local last-error state.
    unsafe { WSAGetLastError() }
  }

  impl Socket {
    /// The readiness handle a `crate::readiness` future registers. A Winsock `SOCKET` is pointer-width
    /// but a kernel handle-table value that fits in a positive `i32` in practice; the readiness seam
    /// (and the AFD reactor that reconstructs it) carry it as that `i32`, the width a Unix fd uses.
    pub(crate) fn raw_id(&self) -> i32 {
      // The low 32 bits of the socket, reinterpreted as `i32` bit-for-bit — a checked narrowing (the
      // socket fits) then a bit-preserving reinterpret, so the AFD reactor's `raw as u32 as SOCKET`
      // reconstructs the same handle. No lossy `as` cast.
      let low = u32::try_from(self.socket).unwrap_or(u32::MAX);
      i32::from_ne_bytes(low.to_ne_bytes())
    }
  }

  /// Creates a non-blocking INET datagram socket that no child process inherits and whose address no other socket may
  /// share ([`exclusive_socket`]): without `SO_EXCLUSIVEADDRUSE` another process could bind a fleet node's port with
  /// `SO_REUSEADDR` and take its datagrams, and an inheritable socket outlives the daemon in any child it starts.
  /// Until 2026-10-26 this was `socket()`, which gives neither.
  pub(crate) fn dgram_socket() -> Result<Socket, RtError> {
    let socket = exclusive_socket(SOCK_DGRAM, IPPROTO_UDP, "WSASocketW(DGRAM)")?;
    set_dont_fragment(&socket)?;
    Ok(socket)
  }

  /// Creates a non-blocking INET socket of `kind` over `protocol` that no child process inherits
  /// (`WSA_FLAG_NO_HANDLE_INHERIT`, Unix's close-on-exec, set as the socket is made so no concurrent process start can
  /// take a copy) and whose address no other socket may share (`SO_EXCLUSIVEADDRUSE`, set before any bind, which is
  /// when Windows takes it: Microsoft Learn, "Using SO_REUSEADDR and SO_EXCLUSIVEADDRUSE"). `WSA_FLAG_OVERLAPPED` is
  /// what `socket()` gives every socket; nothing here issues overlapped I/O on it, the AFD reactor polls its base
  /// handle. `call` names the creation in a refusal.
  fn exclusive_socket(kind: i32, protocol: i32, call: &'static str) -> Result<Socket, RtError> {
    ensure_started()?;
    // SAFETY: a plain socket creation with no protocol record or group; checked against INVALID_SOCKET.
    let raw = unsafe {
      WSASocketW(
        i32::from(AF_INET),
        kind,
        protocol,
        std::ptr::null(),
        0,
        WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
      )
    };
    if raw == INVALID_SOCKET {
      return Err(last(call));
    }
    let socket = Socket { socket: raw };
    set_nonblocking(&socket)?;
    set_int(
      &socket,
      SOL_SOCKET,
      SO_EXCLUSIVEADDRUSE,
      1,
      "setsockopt(SO_EXCLUSIVEADDRUSE)",
    )?;
    Ok(socket)
  }

  /// Puts `socket` in non-blocking mode (`FIONBIO`), as every socket this seam drives must be.
  fn set_nonblocking(socket: &Socket) -> Result<(), RtError> {
    let mut nonblocking: u32 = 1;
    // SAFETY: FIONBIO takes one u32 by pointer; a live local suffices.
    if unsafe { ioctlsocket(socket.socket, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
      return Err(last("ioctlsocket(FIONBIO)"));
    }
    Ok(())
  }

  /// Sets the don't-fragment bit on every datagram the socket sends (`IP_DONTFRAGMENT`; RFC 8899 §3 — see
  /// the Unix arms), so a path-MTU probe that is too large is dropped rather than fragmented.
  fn set_dont_fragment(socket: &Socket) -> Result<(), RtError> {
    let on: i32 = 1;
    let len = i32::try_from(std::mem::size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: `IP_DONTFRAGMENT` takes a DWORD-sized `int`; the pointer and length name one live local.
    let outcome = unsafe {
      setsockopt(
        socket.socket,
        IPPROTO_IP,
        IP_DONTFRAGMENT,
        (&raw const on).cast::<u8>(),
        len,
      )
    };
    if outcome == SOCKET_ERROR {
      return Err(last("setsockopt(IP_DONTFRAGMENT)"));
    }
    Ok(())
  }

  /// One `int`-valued `SOL_SOCKET` option of `socket` (`SO_RCVBUF`, `SO_TYPE`); `call` names the query in a
  /// refusal.
  fn int_option(socket: &Socket, option: i32, call: &'static str) -> Result<i32, RtError> {
    let mut value: i32 = 0;
    // The option is an `int`: its length is that type's size, which always fits an `i32`.
    let mut len: i32 = i32::try_from(std::mem::size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: the option is an `int`; the out pointer and its length name one live local `i32`.
    let outcome = unsafe {
      getsockopt(
        socket.socket,
        SOL_SOCKET,
        option,
        (&raw mut value).cast::<u8>(),
        &raw mut len,
      )
    };
    if outcome == SOCKET_ERROR {
      return Err(last(call));
    }
    Ok(value)
  }

  /// The socket's kernel receive buffer (`SO_RCVBUF`) in bytes.
  pub(crate) fn recv_buffer_bytes(socket: &Socket) -> Result<usize, RtError> {
    let bytes = int_option(socket, SO_RCVBUF, "getsockopt(SO_RCVBUF)")?;
    usize::try_from(bytes).map_err(|_| last("getsockopt(SO_RCVBUF)"))
  }

  /// A `SOCKADDR_IN` for `addr` (network byte order for the port and address, as the wire wants).
  fn sockaddr(addr: SocketAddrV4) -> SOCKADDR_IN {
    SOCKADDR_IN {
      sin_family: AF_INET,
      sin_port: addr.port().to_be(),
      sin_addr: IN_ADDR {
        S_un: IN_ADDR_0 {
          // The octets in memory order [a, b, c, d] are already network order.
          S_addr: u32::from_ne_bytes(addr.ip().octets()),
        },
      },
      sin_zero: [0; 8],
    }
  }

  /// The `SocketAddrV4` a filled `SOCKADDR_IN` names (the inverse of [`sockaddr`]).
  fn from_sockaddr(raw: &SOCKADDR_IN) -> SocketAddrV4 {
    // SAFETY: reading the `S_addr` arm of the address union — a plain `u32`, always initialized.
    let addr_bytes = unsafe { raw.sin_addr.S_un.S_addr }.to_ne_bytes();
    SocketAddrV4::new(
      Ipv4Addr::new(addr_bytes[0], addr_bytes[1], addr_bytes[2], addr_bytes[3]),
      u16::from_be(raw.sin_port),
    )
  }

  /// An already-bound OS socket the caller hands over (the socket-activation shape): the port was never
  /// released between the caller's bind and this adoption, so nothing can take it in between. Refused
  /// unless it is a datagram socket; made non-blocking, and taken out of every child's inheritance, like one
  /// this seam created. Its sharing is its creator's: `SO_EXCLUSIVEADDRUSE` takes effect only before a bind, so a
  /// supervisor handing a port over sets it on the socket it binds.
  pub(crate) fn adopt(owned: OwnedDatagram) -> Result<Socket, RtError> {
    ensure_started()?;
    let Ok(raw) = SOCKET::try_from(owned.into_raw_socket()) else {
      return Err(RtError::DriverRefused {
        call: "adopt(SOCKET)",
        code: None,
      });
    };
    // Owned from here: the drop closes it once, on every refusal below too.
    let socket = Socket { socket: raw };
    let kind = int_option(&socket, SO_TYPE, "getsockopt(SO_TYPE)")?;
    if kind != SOCK_DGRAM {
      return Err(RtError::DriverRefused {
        call: "adopt(SO_TYPE)",
        code: Some(kind),
      });
    }
    not_inherited(&socket)?;
    set_nonblocking(&socket)?;
    set_dont_fragment(&socket)?;
    Ok(socket)
  }

  /// The owned OS handle [`adopt`] takes: a Winsock socket.
  pub(crate) type OwnedDatagram = std::os::windows::io::OwnedSocket;

  pub(crate) fn bind(socket: &Socket, addr: SocketAddrV4) -> Result<(), RtError> {
    let sa = sockaddr(addr);
    // SAFETY: `sa` is a live SOCKADDR_IN of the given length, passed as the generic SOCKADDR.
    let rc = unsafe {
      ws_bind(
        socket.socket,
        std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
        i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(0),
      )
    };
    if rc == SOCKET_ERROR {
      Err(last("bind"))
    } else {
      Ok(())
    }
  }

  pub(crate) fn local_addr(socket: &Socket) -> Result<SocketAddrV4, RtError> {
    // SAFETY: an all-zero SOCKADDR_IN is a valid empty address getsockname fills.
    let mut sa: SOCKADDR_IN = unsafe { std::mem::zeroed() };
    let mut len = i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(0);
    // SAFETY: getsockname writes up to `len` bytes into `sa` and the actual length back into `len`.
    let rc = unsafe {
      getsockname(
        socket.socket,
        std::ptr::addr_of_mut!(sa).cast::<SOCKADDR>(),
        &mut len,
      )
    };
    if rc == SOCKET_ERROR {
      return Err(last("getsockname"));
    }
    Ok(from_sockaddr(&sa))
  }

  pub(crate) fn recv_from(
    socket: &Socket,
    buf: &mut [u8],
  ) -> Result<Io<(usize, SocketAddrV4)>, RtError> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: an all-zero SOCKADDR_IN is a valid empty address recvfrom fills.
    let mut from: SOCKADDR_IN = unsafe { std::mem::zeroed() };
    let mut from_len = i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(0);
    // SAFETY: recvfrom writes up to `len` bytes into `buf` and the sender into `from`/`from_len`.
    let rc = unsafe {
      ws_recvfrom(
        socket.socket,
        buf.as_mut_ptr(),
        len,
        0,
        std::ptr::addr_of_mut!(from).cast::<SOCKADDR>(),
        &mut from_len,
      )
    };
    if rc == SOCKET_ERROR {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last("recvfrom")),
      });
    }
    Ok(Io::Ready((
      usize::try_from(rc).unwrap_or(0),
      from_sockaddr(&from),
    )))
  }

  pub(crate) fn send_to(
    socket: &Socket,
    buf: &[u8],
    addr: SocketAddrV4,
  ) -> Result<Io<usize>, RtError> {
    let sa = sockaddr(addr);
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: sendto reads `len` bytes from `buf` and the destination from `sa`.
    let rc = unsafe {
      ws_sendto(
        socket.socket,
        buf.as_ptr(),
        len,
        0,
        std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
        i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(0),
      )
    };
    if rc == SOCKET_ERROR {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last("sendto")),
      });
    }
    Ok(Io::Ready(usize::try_from(rc).unwrap_or(0)))
  }

  // ---------------------------------------------------------------------------- streams (TCP)

  /// The owned OS handle a stream or a listener is handed over as: a Winsock socket.
  pub(crate) type OwnedStream = std::os::windows::io::OwnedSocket;

  /// Sets one `int`-valued option of `socket` at `level`; `call` names it in a refusal.
  fn set_int(
    socket: &Socket,
    level: i32,
    option: i32,
    value: i32,
    call: &'static str,
  ) -> Result<(), RtError> {
    let len = i32::try_from(std::mem::size_of::<i32>()).unwrap_or(i32::MAX);
    // SAFETY: the option is an `int`; the pointer and length name one live local `i32`, and the socket is this
    // value's, open for the call.
    let outcome = unsafe {
      setsockopt(
        socket.socket,
        level,
        option,
        (&raw const value).cast::<u8>(),
        len,
      )
    };
    if outcome == SOCKET_ERROR {
      return Err(last(call));
    }
    Ok(())
  }

  /// Takes `socket` out of every child process's inheritance (`HANDLE_FLAG_INHERIT` off): Windows' close-on-exec. A
  /// socket made with `WSA_FLAG_NO_HANDLE_INHERIT` starts so; an accepted or adopted one is cleared here, as Rust's
  /// standard library clears each accepted socket.
  fn not_inherited(socket: &Socket) -> Result<(), RtError> {
    // SAFETY: changes one flag of a socket this value owns; a base provider's socket is a kernel handle, the shape
    // `crate::afd` registers it in.
    if unsafe { SetHandleInformation(socket.socket as HANDLE, HANDLE_FLAG_INHERIT, 0) } == 0 {
      return Err(RtError::os("SetHandleInformation(socket)"));
    }
    Ok(())
  }

  /// A connected stream made ready to serve: no child inherits it, non-blocking, and `TCP_NODELAY` ([`set_nodelay`]).
  fn prepared(socket: Socket) -> Result<Socket, RtError> {
    not_inherited(&socket)?;
    set_nonblocking(&socket)?;
    set_nodelay(&socket)?;
    Ok(socket)
  }

  /// Creates a non-blocking INET stream socket that no child process inherits and whose address no other socket may
  /// share ([`exclusive_socket`]): without `SO_EXCLUSIVEADDRUSE` another process could bind a listener's port with
  /// `SO_REUSEADDR` and take its connections.
  pub(crate) fn stream_socket() -> Result<Socket, RtError> {
    exclusive_socket(SOCK_STREAM, IPPROTO_TCP, "WSASocketW(STREAM)")
  }

  pub(crate) fn listen(socket: &Socket, backlog: i32) -> Result<(), RtError> {
    // SAFETY: a listen on a bound socket this value owns.
    if unsafe { ws_listen(socket.socket, backlog) } == SOCKET_ERROR {
      return Err(last("listen"));
    }
    Ok(())
  }

  /// The next pending connection on `listener`, made ready to serve ([`prepared`]).
  pub(crate) fn accept(listener: &Socket) -> Result<Io<Socket>, RtError> {
    // SAFETY: an accept on a listening socket this value owns, asking for no peer address.
    let raw = unsafe { ws_accept(listener.socket, std::ptr::null_mut(), std::ptr::null_mut()) };
    if raw == INVALID_SOCKET {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last("accept")),
      });
    }
    prepared(Socket { socket: raw }).map(Io::Ready)
  }

  /// Begins `socket`'s connection to `addr`: `Ready` when it connected at once, `WouldBlock` while the handshake runs
  /// (Winsock answers a non-blocking connect under way `WSAEWOULDBLOCK`, its `EINPROGRESS`), when the caller awaits
  /// writability (the AFD reactor's writable edge includes `AFD_POLL_CONNECT_FAIL`) and then asks [`connected`].
  pub(crate) fn connect(socket: &Socket, addr: SocketAddrV4) -> Result<Io<()>, RtError> {
    let sa = sockaddr(addr);
    // SAFETY: `sa` is a live SOCKADDR_IN of the given length, passed as the generic SOCKADDR.
    let rc = unsafe {
      ws_connect(
        socket.socket,
        std::ptr::addr_of!(sa).cast::<SOCKADDR>(),
        i32::try_from(size_of::<SOCKADDR_IN>()).unwrap_or(0),
      )
    };
    if rc == SOCKET_ERROR {
      return match last_code() {
        WSAEWOULDBLOCK => Ok(Io::WouldBlock),
        _ => Err(last("connect")),
      };
    }
    Ok(Io::Ready(()))
  }

  /// How a handshake that ran ended: the socket's pending error (`SO_ERROR`), a refused connection among them.
  pub(crate) fn connected(socket: &Socket) -> Result<(), RtError> {
    let error = int_option(socket, SO_ERROR, "getsockopt(SO_ERROR)")?;
    if error != 0 {
      return Err(RtError::DriverRefused {
        call: "connect",
        code: Some(error),
      });
    }
    Ok(())
  }

  /// `TCP_NODELAY` (`crate::tcp` says why every stream here wants it).
  pub(crate) fn set_nodelay(socket: &Socket) -> Result<(), RtError> {
    set_int(
      socket,
      IPPROTO_TCP,
      TCP_NODELAY,
      1,
      "setsockopt(TCP_NODELAY)",
    )
  }

  /// One non-blocking `recv` of up to `buf.len()` bytes with `flags`, classified; `call` names it in a refusal.
  fn receive(
    socket: &Socket,
    buf: &mut [u8],
    flags: i32,
    call: &'static str,
  ) -> Result<Io<usize>, RtError> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: recv writes at most `len` bytes into `buf`, which this call borrows mutably.
    let rc = unsafe { ws_recv(socket.socket, buf.as_mut_ptr(), len, flags) };
    if rc == SOCKET_ERROR {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last(call)),
      });
    }
    Ok(Io::Ready(usize::try_from(rc).unwrap_or(0)))
  }

  pub(crate) fn read(socket: &Socket, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
    receive(socket, buf, 0, "recv")
  }

  /// The bytes at the head of the receive queue copied into `buf`, left queued (`MSG_PEEK`).
  pub(crate) fn peek(socket: &Socket, buf: &mut [u8]) -> Result<Io<usize>, RtError> {
    receive(socket, buf, MSG_PEEK, "recv(MSG_PEEK)")
  }

  /// Up to `into.len()` bytes consumed from the head of the receive queue, copied into `into` and dropped (Winsock
  /// has no `MSG_TRUNC` for TCP).
  pub(crate) fn discard(socket: &Socket, into: &mut [u8]) -> Result<Io<usize>, RtError> {
    receive(socket, into, 0, "recv(discard)")
  }

  pub(crate) fn write(socket: &Socket, buf: &[u8]) -> Result<Io<usize>, RtError> {
    let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
    // SAFETY: send reads at most `len` bytes from `buf`, which this call borrows.
    let rc = unsafe { ws_send(socket.socket, buf.as_ptr(), len, 0) };
    if rc == SOCKET_ERROR {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last("send")),
      });
    }
    Ok(Io::Ready(usize::try_from(rc).unwrap_or(0)))
  }

  /// One gathering send of `slices` (`WSASend` with no `OVERLAPPED`, so it completes or refuses before it returns). A
  /// slice longer than a `WSABUF` holds offers its first `u32::MAX` bytes: the count sent says how far it went, which
  /// is all a caller reads.
  pub(crate) fn write_vectored(
    socket: &Socket,
    slices: &[std::io::IoSlice<'_>],
  ) -> Result<Io<usize>, RtError> {
    let buffers: Vec<WSABUF> = slices
      .iter()
      .map(|slice| WSABUF {
        len: u32::try_from(slice.len()).unwrap_or(u32::MAX),
        buf: slice.as_ptr().cast_mut(),
      })
      .collect();
    let count = u32::try_from(buffers.len()).unwrap_or(u32::MAX);
    let mut sent = 0u32;
    // SAFETY: `buffers` describes `count` live slices the caller borrows for the call, which WSASend reads alone; with
    // no OVERLAPPED the call is done with them before it returns.
    let rc = unsafe {
      WSASend(
        socket.socket,
        buffers.as_ptr(),
        count,
        &raw mut sent,
        0,
        std::ptr::null_mut(),
        None,
      )
    };
    if rc == SOCKET_ERROR {
      return Ok(match last_code() {
        WSAEWOULDBLOCK => Io::WouldBlock,
        WSAEINTR => Io::Interrupted,
        _ => return Err(last("WSASend")),
      });
    }
    Ok(Io::Ready(usize::try_from(sent).unwrap_or(0)))
  }

  /// Both buffers set to `buffer_bytes` and read back, refused where Windows kept less, as on the BSDs.
  pub(crate) fn reserve_buffers(socket: &Socket, buffer_bytes: usize) -> Result<(), RtError> {
    let value = i32::try_from(buffer_bytes).unwrap_or(i32::MAX);
    set_int(
      socket,
      SOL_SOCKET,
      SO_SNDBUF,
      value,
      "setsockopt(SO_SNDBUF)",
    )?;
    set_int(
      socket,
      SOL_SOCKET,
      SO_RCVBUF,
      value,
      "setsockopt(SO_RCVBUF)",
    )?;
    let send = int_option(socket, SO_SNDBUF, "getsockopt(SO_SNDBUF)")?;
    let receive = int_option(socket, SO_RCVBUF, "getsockopt(SO_RCVBUF)")?;
    if send < value || receive < value {
      return Err(RtError::DriverRefused {
        call: "setsockopt(SO_SNDBUF/SO_RCVBUF)",
        code: Some(WSAENOBUFS),
      });
    }
    Ok(())
  }

  /// Never on Windows: a non-blocking send on a stream socket may take "between 1 and the requested length" bytes
  /// (Microsoft Learn, `send`), and Winsock keeps no send low-water mark that would make it all or none.
  pub(crate) fn make_sends_whole(_socket: &Socket, _record_bytes: usize) -> Result<bool, RtError> {
    Ok(false)
  }

  /// Sets the receive low-water mark (`SO_RCVLOWAT`) to `bytes` and reads it back. Microsoft Learn says Windows' TCP
  /// provider refuses the option (`WSAEINVAL` since Vista); Windows' own answer is what the caller gets, typed.
  pub(crate) fn want_bytes(socket: &Socket, bytes: usize) -> Result<(), RtError> {
    let value = i32::try_from(bytes.max(1)).unwrap_or(i32::MAX);
    set_int(
      socket,
      SOL_SOCKET,
      SO_RCVLOWAT,
      value,
      "setsockopt(SO_RCVLOWAT)",
    )?;
    let kept = int_option(socket, SO_RCVLOWAT, "getsockopt(SO_RCVLOWAT)")?;
    if kept < value {
      return Err(RtError::DriverRefused {
        call: "setsockopt(SO_RCVLOWAT)",
        code: Some(WSAENOBUFS),
      });
    }
    Ok(())
  }

  /// Whether the connection is still established, from Windows' own record of it (`SIO_TCP_INFO`, version 0, Windows
  /// 10 1703 and later): its state is `TCPSTATE_ESTABLISHED`.
  pub(crate) fn established(socket: &Socket) -> Result<bool, RtError> {
    let version: u32 = 0;
    let mut info = TCP_INFO_v0::default();
    let mut returned = 0u32;
    // SAFETY: the version is a live DWORD in and the record a live TCP_INFO_v0 out, each passed with its own length;
    // with no OVERLAPPED the call is done with both before it returns.
    let rc = unsafe {
      WSAIoctl(
        socket.socket,
        SIO_TCP_INFO,
        (&raw const version).cast(),
        u32::try_from(size_of::<u32>()).unwrap_or(0),
        (&raw mut info).cast(),
        u32::try_from(size_of::<TCP_INFO_v0>()).unwrap_or(0),
        &raw mut returned,
        std::ptr::null_mut(),
        None,
      )
    };
    if rc == SOCKET_ERROR {
      return Err(last("WSAIoctl(SIO_TCP_INFO)"));
    }
    Ok(info.State == TCPSTATE_ESTABLISHED)
  }

  /// Ends both directions of the connection, whoever else holds a copy of the socket.
  pub(crate) fn shutdown(socket: &Socket) -> Result<(), RtError> {
    shutdown_directions(socket, SD_BOTH, "shutdown")
  }

  /// Ends this side's sending (`SD_SEND`) and keeps its receiving: the peer reads end of stream after every byte sent.
  pub(crate) fn shutdown_write(socket: &Socket) -> Result<(), RtError> {
    shutdown_directions(socket, SD_SEND, "shutdown(SD_SEND)")
  }

  /// Shuts down the directions `how` names (`SD_BOTH`, `SD_SEND`) of `socket`; `call` names it in a refusal.
  fn shutdown_directions(socket: &Socket, how: i32, call: &'static str) -> Result<(), RtError> {
    // SAFETY: a shutdown of a socket this value owns, open for the call.
    if unsafe { ws_shutdown(socket.socket, how) } == SOCKET_ERROR {
      return Err(last(call));
    }
    Ok(())
  }

  /// A socket handed over, refused unless it is a stream socket; owned from here, no child inherits it, non-blocking.
  fn adopt_stream_socket(owned: OwnedStream) -> Result<Socket, RtError> {
    ensure_started()?;
    let Ok(raw) = SOCKET::try_from(owned.into_raw_socket()) else {
      return Err(RtError::DriverRefused {
        call: "adopt(SOCKET)",
        code: None,
      });
    };
    // Owned from here: the drop closes it once, on every refusal below too.
    let socket = Socket { socket: raw };
    let kind = int_option(&socket, SO_TYPE, "getsockopt(SO_TYPE)")?;
    if kind != SOCK_STREAM {
      return Err(RtError::DriverRefused {
        call: "adopt(SO_TYPE)",
        code: Some(kind),
      });
    }
    not_inherited(&socket)?;
    set_nonblocking(&socket)?;
    Ok(socket)
  }

  /// A listening socket handed over (a supervisor's), made ready as one this seam made.
  pub(crate) fn adopt_listener(owned: OwnedStream) -> Result<Socket, RtError> {
    adopt_stream_socket(owned)
  }

  /// A connected stream handed over (from another shard, or another process), made ready to serve.
  pub(crate) fn adopt_stream(owned: OwnedStream) -> Result<Socket, RtError> {
    let socket = adopt_stream_socket(owned)?;
    set_nodelay(&socket)?;
    Ok(socket)
  }

  /// Gives up the socket: the counterpart to the adoptions.
  pub(crate) fn into_owned(socket: Socket) -> OwnedStream {
    let socket = std::mem::ManuallyDrop::new(socket);
    // A `SOCKET` is pointer-width and a raw socket 64 bits, so the widening keeps every bit on every target Rust
    // builds for Windows.
    let raw = socket.socket as std::os::windows::io::RawSocket;
    // SAFETY: the socket this value owned, whose drop never runs (`ManuallyDrop`), so the returned owner closes it
    // once.
    unsafe { std::os::windows::io::OwnedSocket::from_raw_socket(raw) }
  }

  /// A send Windows took nothing of without blocking: the stream can take no more (`WSAESHUTDOWN`, Winsock's nearest
  /// to `EPIPE`).
  pub(crate) fn broken_pipe(call: &'static str) -> RtError {
    RtError::DriverRefused {
      call,
      code: Some(WSAESHUTDOWN),
    }
  }

  /// A request the caller's own arguments made impossible (`WSAEINVAL`).
  pub(crate) fn invalid(call: &'static str) -> RtError {
    RtError::DriverRefused {
      call,
      code: Some(WSAEINVAL),
    }
  }

  /// A call that would have had to wait where the caller knew it would not (`WSAEWOULDBLOCK`).
  pub(crate) fn would_block(call: &'static str) -> RtError {
    RtError::DriverRefused {
      call,
      code: Some(WSAEWOULDBLOCK),
    }
  }
}

#[cfg(unix)]
pub(crate) use imp::as_fd;
pub(crate) use imp::{OwnedDatagram, OwnedStream, Socket};
pub(crate) use imp::{
  accept, adopt_listener, adopt_stream, broken_pipe, connect, connected, discard, established,
  into_owned, invalid, listen, make_sends_whole, peek, read, reserve_buffers, set_nodelay,
  shutdown, shutdown_write, stream_socket, want_bytes, would_block, write, write_vectored,
};
pub(crate) use imp::{
  adopt, bind, dgram_socket, local_addr, recv_buffer_bytes, recv_from, send_to,
};
