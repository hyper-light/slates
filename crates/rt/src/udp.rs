//! A UDP socket on the runtime (§4.10a): the fleet claims plane rides UDP, and this is the async
//! datagram socket the transport uses over the executor's own driver — no foreign runtime (R6, D-9).
//! `recv_from` registers one-shot read-readiness with the shard's driver and yields; the driver wakes
//! the task when the socket is readable, then a non-blocking `recvfrom` takes the datagram. `send_to`
//! is a direct non-blocking `sendto` (a datagram send does not block on loopback or a healthy link).
//!
//! On a real runtime the socket is a `rustix` UDP socket (not `std::net`, which the lint wall reserves
//! — the address types come via `rustix::net`, the standard types re-exported); the readiness-native
//! drivers (kqueue, epoll) back it. On the simulation runtime it is a port on the deterministic
//! in-memory fabric (`crate::sim`), so the whole plane is testable at N=1 with no OS network — one
//! socket type, one code path, the driver deciding (R8). `recv_from` shares the same `Readable`
//! future either way; only the receive and the raw handle differ.

use std::os::fd::{AsRawFd, OwnedFd};

// The address types come through `rustix::net` (the standard `core::net` types re-exported), so the
// host-path wall's `std::net` guard is honoured while the socket calls stay in `rustix`.
use rustix::net::{
  AddressFamily, Ipv4Addr, RecvFlags, SendFlags, SocketAddr, SocketAddrV4, SocketFlags, SocketType,
  bind, getsockname, recvfrom, sendto, socket_with,
};

use crate::error::RtError;
use crate::readiness::readable;
use crate::{driver::refused, registry};

/// An async UDP socket: a real `rustix` socket, or a port on the simulation's in-memory fabric.
#[derive(Debug)]
pub enum UdpSocket {
  /// A real OS UDP socket.
  Real {
    /// The socket descriptor.
    fd: OwnedFd,
  },
  /// A simulated socket: a port on this thread's deterministic UDP fabric.
  Sim {
    /// The sim port (its address).
    port: u16,
  },
}

/// Whether the current shard runs the simulation driver (so a socket uses the in-memory fabric).
fn on_sim() -> bool {
  registry::with_current(|ctx| ctx.driver_is_sim()).unwrap_or(false)
}

impl UdpSocket {
  /// Binds a non-blocking UDP socket to `addr` (use port 0 for an OS-assigned port, then
  /// [`UdpSocket::local_addr`]). On the simulation runtime the requested address is ignored and a
  /// fabric port is assigned.
  pub fn bind(addr: SocketAddrV4) -> Result<UdpSocket, RtError> {
    if on_sim() {
      return Ok(UdpSocket::Sim {
        port: crate::sim::sim_udp_bind(),
      });
    }
    // `SocketFlags::NONBLOCK`/`CLOEXEC` on `socket()` are Linux-only; set both after creation so the
    // socket is non-blocking (the driver provides the waiting) and not inherited across exec.
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
    bind(&fd, &addr).map_err(|e| refused("bind", e))?;
    Ok(UdpSocket::Real { fd })
  }

  /// The local address the socket is bound to (the OS-assigned port on a real socket; the fabric
  /// port, on loopback, in simulation).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    match self {
      UdpSocket::Real { fd } => {
        let any = getsockname(fd).map_err(|e| refused("getsockname", e))?;
        match SocketAddr::try_from(any) {
          Ok(SocketAddr::V4(v4)) => Ok(v4),
          _ => Err(refused("getsockname", rustix::io::Errno::AFNOSUPPORT)),
        }
      }
      UdpSocket::Sim { port } => Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, *port)),
    }
  }

  /// Sends a datagram to `addr` (a non-blocking send; the bytes accepted are returned). In simulation
  /// the datagram is delivered to `addr`'s port on the fabric and any waiting receiver is woken.
  pub fn send_to(&self, buf: &[u8], addr: SocketAddrV4) -> Result<usize, RtError> {
    match self {
      UdpSocket::Real { fd } => {
        sendto(fd, buf, SendFlags::empty(), &addr).map_err(|e| refused("sendto", e))
      }
      UdpSocket::Sim { port } => {
        crate::sim::sim_udp_send(addr.port(), buf, *port);
        Ok(buf.len())
      }
    }
  }

  /// Receives one datagram, awaiting readability through the driver when none is ready. Returns the
  /// byte count and the sender's address.
  pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
    match self {
      UdpSocket::Real { fd } => self.recv_real(fd, buf).await,
      UdpSocket::Sim { port } => self.recv_sim(*port, buf).await,
    }
  }

  /// The real receive loop: non-blocking `recvfrom`, awaiting driver readiness on `AGAIN`.
  async fn recv_real(
    &self,
    fd: &OwnedFd,
    buf: &mut [u8],
  ) -> Result<(usize, SocketAddrV4), RtError> {
    loop {
      match recvfrom(fd, &mut *buf, RecvFlags::empty()) {
        Ok((n, _flags, Some(from))) => {
          return match SocketAddr::try_from(from) {
            Ok(SocketAddr::V4(v4)) => Ok((n, v4)),
            _ => Err(refused("recvfrom", rustix::io::Errno::AFNOSUPPORT)),
          };
        }
        Ok((n, _flags, None)) => return Ok((n, SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))),
        Err(rustix::io::Errno::AGAIN) => readable(fd.as_raw_fd()).await?,
        Err(e) => return Err(refused("recvfrom", e)),
      }
    }
  }

  /// The simulated receive loop: take from the fabric mailbox, awaiting the driver (fabric interest)
  /// when it is empty.
  async fn recv_sim(&self, port: u16, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
    loop {
      if let Some((bytes, from)) = crate::sim::sim_udp_recv(port) {
        let n = bytes.len().min(buf.len());
        buf[..n].copy_from_slice(&bytes[..n]);
        return Ok((n, SocketAddrV4::new(Ipv4Addr::LOCALHOST, from)));
      }
      readable(i32::from(port)).await?;
    }
  }
}
