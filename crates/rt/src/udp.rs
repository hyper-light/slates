//! A UDP socket on the runtime (§4.10a): the fleet claims plane rides UDP, and this is the async
//! datagram socket the transport uses over the executor's own driver — no foreign runtime (R6, D-9).
//! `recv_from` registers one-shot read-readiness with the shard's driver and yields; the driver wakes
//! the task when the socket is readable, then a non-blocking `recvfrom` takes the datagram. `send_to`
//! is a direct non-blocking `sendto` (a datagram send does not block on loopback or a healthy link).
//!
//! The socket is non-blocking from creation and uses `rustix` for the syscalls (not `std::net`, which
//! the lint wall reserves — this is `rustix::net`, bounds-checked byte buffers, no host path). It is
//! the readiness-native drivers (kqueue, epoll) that back it today; the completion-native drivers
//! (io_uring, IOCP) refuse `register_readable` until their slices land (owed, §4.10a phasing).

use std::future::Future;
use std::os::fd::{AsRawFd, OwnedFd};
use std::pin::Pin;
use std::task::{Context, Poll};

// The address types come through `rustix::net` (they are the standard `core::net` types re-exported),
// so the host-path wall's `std::net` guard is honoured while the socket calls stay in `rustix`.
use rustix::net::{
  AddressFamily, Ipv4Addr, RecvFlags, SendFlags, SocketAddr, SocketAddrV4, SocketFlags, SocketType,
  bind, getsockname, recvfrom, sendto, socket_with,
};

use crate::error::RtError;
use crate::waker::word_of;
use crate::{driver::refused, registry};

/// An async UDP socket bound to a local address.
#[derive(Debug)]
pub struct UdpSocket {
  fd: OwnedFd,
}

impl UdpSocket {
  /// Binds a non-blocking UDP socket to `addr` (use port 0 for an OS-assigned port, then
  /// [`UdpSocket::local_addr`]).
  pub fn bind(addr: SocketAddrV4) -> Result<UdpSocket, RtError> {
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
    Ok(UdpSocket { fd })
  }

  /// The local address the socket is bound to (the OS-assigned port, when bound to port 0).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    let any = getsockname(&self.fd).map_err(|e| refused("getsockname", e))?;
    match SocketAddr::try_from(any) {
      Ok(SocketAddr::V4(v4)) => Ok(v4),
      _ => Err(refused("getsockname", rustix::io::Errno::AFNOSUPPORT)),
    }
  }

  /// Sends a datagram to `addr` (a non-blocking send; the bytes accepted are returned).
  pub fn send_to(&self, buf: &[u8], addr: SocketAddrV4) -> Result<usize, RtError> {
    sendto(&self.fd, buf, SendFlags::empty(), &addr).map_err(|e| refused("sendto", e))
  }

  /// Receives one datagram, awaiting readability through the driver when none is ready. Returns the
  /// byte count and the sender's address.
  pub async fn recv_from(&self, buf: &mut [u8]) -> Result<(usize, SocketAddrV4), RtError> {
    loop {
      match recvfrom(&self.fd, &mut *buf, RecvFlags::empty()) {
        Ok((n, _flags, Some(from))) => {
          return match SocketAddr::try_from(from) {
            Ok(SocketAddr::V4(v4)) => Ok((n, v4)),
            _ => Err(refused("recvfrom", rustix::io::Errno::AFNOSUPPORT)),
          };
        }
        // A datagram with no reported source (rare); report the count against the unspecified addr.
        Ok((n, _flags, None)) => {
          return Ok((n, SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)));
        }
        Err(rustix::io::Errno::AGAIN) => {
          Readable {
            raw: self.fd.as_raw_fd(),
            armed: false,
          }
          .await?;
        }
        Err(e) => return Err(refused("recvfrom", e)),
      }
    }
  }
}

/// Awaits the socket's readability once: it registers one-shot read interest with the shard's driver
/// on the first poll and yields; the driver's completion re-queues this task, and the next poll
/// returns ready so the caller retries the non-blocking receive.
struct Readable {
  raw: i32,
  armed: bool,
}

impl Future for Readable {
  type Output = Result<(), RtError>;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), RtError>> {
    if self.armed {
      // The driver woke us; let the caller retry the receive (a spurious wake just retries).
      return Poll::Ready(Ok(()));
    }
    let Some(word) = word_of(cx.waker()) else {
      // A foreign waker cannot be armed on the driver; degrade to a retry (the caller's loop copes).
      return Poll::Ready(Ok(()));
    };
    match registry::with_current(|ctx| ctx.register_readable(self.raw, word.word())) {
      Some(Ok(())) => {
        self.armed = true;
        Poll::Pending
      }
      Some(Err(e)) => Poll::Ready(Err(e)),
      None => Poll::Ready(Err(RtError::NotOnShardThread)),
    }
  }
}
