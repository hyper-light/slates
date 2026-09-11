//! A UDP socket on the runtime (§4.10a): the fleet claims plane rides slates's owned QUIC dialect
//! (`crates/transport`, `rustls::quic`) over this datagram socket, on the executor's own driver — no
//! foreign runtime (R6, D-9). `recv_from` registers one-shot read-readiness with the shard's driver
//! and yields; the driver wakes the task when the socket is readable, then a non-blocking `recvfrom`
//! takes the datagram. `send_to` is a direct non-blocking `sendto` (a datagram send does not block on
//! loopback or a healthy link).
//!
//! On a real runtime the socket is a platform datagram socket through the [`crate::netsys`] seam
//! (`rustix` on Unix, Winsock 2 on Windows — the lint wall reserves `std::net`), so it backs every
//! readiness-native driver: kqueue/epoll on Unix and the IOCP driver's AFD reactor (`crate::afd`) on
//! Windows, one code path. On the simulation runtime it is a port on the deterministic in-memory
//! fabric (`crate::sim`), so the whole plane is testable at N=1 with no OS network (R8). `recv_from`
//! shares the same `Readable` future either way; only the receive and the raw handle differ.

use crate::error::RtError;
use crate::netsys::{self, Io, Socket};
use crate::readiness::readable;
use crate::registry;

// The address types are `core::net`'s (the same ones the seam and `rustix::net` use), re-exported so
// a caller names an address without depending on the socket backend.
pub use core::net::{Ipv4Addr, SocketAddrV4};

/// An async UDP socket: a real OS datagram socket, or a port on the simulation's in-memory fabric.
/// The representation is hidden (the real/sim choice is the driver's, R8); a caller drives it through
/// the methods below, never by matching a backend.
#[derive(Debug)]
pub struct UdpSocket {
  inner: Inner,
}

/// The socket's backend: a real OS datagram socket (through the platform seam), or a simulation
/// fabric port. Private — the distinction is internal (`on_sim`), so a consumer sees one type.
#[derive(Debug)]
enum Inner {
  Real { socket: Socket },
  Sim { port: u16 },
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
      return Ok(UdpSocket {
        inner: Inner::Sim {
          port: crate::sim::sim_udp_bind(),
        },
      });
    }
    let socket = netsys::dgram_socket()?;
    netsys::bind(&socket, addr)?;
    Ok(UdpSocket {
      inner: Inner::Real { socket },
    })
  }

  /// The local address the socket is bound to (the OS-assigned port on a real socket; the fabric
  /// port, on loopback, in simulation).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    match &self.inner {
      Inner::Real { socket } => netsys::local_addr(socket),
      Inner::Sim { port } => Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, *port)),
    }
  }

  /// The socket's receive buffer in bytes — what the kernel queues for it before dropping datagrams
  /// (`SO_RCVBUF`); a consumer that redistributes the socket among several sessions sizes each session's
  /// queue from it. On the simulation fabric, a stated stand-in (the fabric's mailbox is unbounded).
  pub fn recv_buffer_bytes(&self) -> Result<usize, RtError> {
    match &self.inner {
      Inner::Real { socket } => netsys::recv_buffer_bytes(socket),
      Inner::Sim { .. } => Ok(crate::sim::SIM_RECV_BUFFER_BYTES),
    }
  }

  /// Sends a datagram to `addr` (a non-blocking send; the bytes accepted are returned). In simulation
  /// the datagram is delivered to `addr`'s port on the fabric and any waiting receiver is woken.
  pub fn send_to(&self, buf: &[u8], addr: SocketAddrV4) -> Result<usize, RtError> {
    match &self.inner {
      Inner::Real { socket } => netsys::send_to(socket, buf, addr),
      Inner::Sim { port } => {
        crate::sim::sim_udp_send(addr.port(), buf, *port);
        Ok(buf.len())
      }
    }
  }

  /// Receives one datagram, awaiting readability through the driver when none is ready. Returns the
  /// byte count and the sender's address.
  pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
    match &self.inner {
      Inner::Real { socket } => Self::recv_real(socket, buf).await,
      Inner::Sim { port } => Self::recv_sim(*port, buf).await,
    }
  }

  /// The real receive loop: non-blocking `recvfrom` through the seam, awaiting driver readiness on
  /// would-block.
  async fn recv_real(socket: &Socket, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
    loop {
      match netsys::recv_from(socket, buf)? {
        Io::Ready(out) => return Ok(out),
        Io::WouldBlock => readable(socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
  }

  /// The simulated receive loop: take from the fabric mailbox, awaiting the driver (fabric interest)
  /// when it is empty.
  async fn recv_sim(port: u16, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
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
