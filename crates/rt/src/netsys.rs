//! The platform datagram-socket seam (§4.10a, the vorpal `mem/src/policy.rs` shape: paired `#[cfg]`
//! functions with identical signatures, so `udp` holds the async logic cfg-free and only the syscalls
//! differ). A [`Socket`] owns one non-blocking INET **UDP** socket; the free functions are the
//! blocking-free primitives the datagram type drives, each returning [`Io`] so "not ready yet" is a
//! value the caller awaits on, never an error to classify.
//!
//! UDP is the transport that must cross platforms: the fleet claims plane rides slates's owned QUIC
//! dialect over this socket (`crates/transport`, `rustls::quic`), so it runs wherever a daemon does.
//! TCP is *not* here — the runtime's async TCP is the NFS loopback mount server's alone (macOS/Linux;
//! Windows mounts through WinFsp), so it stays `rustix`-only in `crate::tcp`, gated off Windows.
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

  /// A non-blocking OS UDP socket owned here (closed on drop, by `OwnedFd`).
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

  pub(crate) fn dgram_socket() -> Result<Socket, RtError> {
    // `SocketFlags` on `socket()` is Linux-only, so non-blocking/cloexec are set after creation.
    let fd = socket_with(
      AddressFamily::INET,
      SocketType::DGRAM,
      SocketFlags::empty(),
      None,
    )
    .map_err(|e| refused("socket(DGRAM)", e))?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)
      .map_err(|e| refused("fcntl(CLOEXEC)", e))?;
    rustix::io::ioctl_fionbio(&fd, true).map_err(|e| refused("ioctl(FIONBIO)", e))?;
    Ok(Socket { fd })
  }

  pub(crate) fn bind(socket: &Socket, addr: SocketAddrV4) -> Result<(), RtError> {
    rx_bind(&socket.fd, &addr).map_err(|e| refused("bind", e))
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

  pub(crate) fn send_to(socket: &Socket, buf: &[u8], addr: SocketAddrV4) -> Result<usize, RtError> {
    rx_sendto(&socket.fd, buf, SendFlags::empty(), &addr).map_err(|e| refused("sendto", e))
  }
}

// ============================================================================== Windows (Winsock 2)

#[cfg(windows)]
mod imp {
  use std::sync::OnceLock;

  use windows_sys::Win32::Networking::WinSock::{
    AF_INET, FIONBIO, IN_ADDR, IN_ADDR_0, INVALID_SOCKET, IPPROTO_UDP, SOCK_DGRAM, SOCKADDR,
    SOCKADDR_IN, SOCKET, SOCKET_ERROR, WSADATA, WSAEINTR, WSAEWOULDBLOCK, WSAGetLastError,
    WSAStartup, bind as ws_bind, closesocket, getsockname, ioctlsocket, recvfrom as ws_recvfrom,
    sendto as ws_sendto, socket as ws_socket,
  };

  use super::{Io, Ipv4Addr, SocketAddrV4};
  use crate::error::RtError;

  /// A non-blocking Winsock UDP socket owned here (closed on drop). Held as the raw `SOCKET`; the drop
  /// closes it once. Not `Copy`, so ownership is single.
  #[derive(Debug)]
  pub(crate) struct Socket {
    socket: SOCKET,
  }

  impl Drop for Socket {
    fn drop(&mut self) {
      // SAFETY: our socket, created by `socket()`; closed exactly once here.
      unsafe { closesocket(self.socket) };
    }
  }

  /// Winsock must be initialized once per process before any socket call; `WSAStartup(2.2)` and no
  /// matching cleanup (process-lifetime) means one success suffices. A `OnceLock` makes it
  /// exactly-once with no lock on the data path.
  fn ensure_started() -> Result<(), RtError> {
    static STARTED: OnceLock<bool> = OnceLock::new();
    let ok = *STARTED.get_or_init(|| {
      // SAFETY: an all-zero WSADATA is a valid, uninitialized out-param.
      let mut data: WSADATA = unsafe { std::mem::zeroed() };
      // SAFETY: `data` is a live, writable WSADATA; `WSAStartup` fills it and returns 0 on success.
      // 0x0202 requests Winsock 2.2.
      unsafe { WSAStartup(0x0202, &mut data) == 0 }
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

  pub(crate) fn dgram_socket() -> Result<Socket, RtError> {
    ensure_started()?;
    // SAFETY: a plain socket creation; the result is checked against INVALID_SOCKET.
    let raw = unsafe { ws_socket(AF_INET as i32, SOCK_DGRAM, IPPROTO_UDP) };
    if raw == INVALID_SOCKET {
      return Err(last("socket(DGRAM)"));
    }
    let socket = Socket { socket: raw };
    let mut nonblocking: u32 = 1;
    // SAFETY: FIONBIO takes one u32 by pointer; a live local suffices.
    if unsafe { ioctlsocket(raw, FIONBIO, &mut nonblocking) } == SOCKET_ERROR {
      return Err(last("ioctlsocket(FIONBIO)"));
    }
    Ok(socket)
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

  pub(crate) fn send_to(socket: &Socket, buf: &[u8], addr: SocketAddrV4) -> Result<usize, RtError> {
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
      Err(last("sendto"))
    } else {
      Ok(usize::try_from(rc).unwrap_or(0))
    }
  }
}

pub(crate) use imp::Socket;
pub(crate) use imp::{bind, dgram_socket, local_addr, recv_from, send_to};
