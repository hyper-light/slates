//! The shard's receive buffer (§4.10a; RFC 8899 via RFC 9000 §14.3): one buffer of the largest UDP payload
//! per shard thread, lent to each synchronous read of a datagram socket and returned after it. An end must
//! read whatever size path MTU discovery lets its peer send — up to the limit it declares in its transport
//! parameters ([`DEFAULT_MAX_UDP_PAYLOAD`], 65,527 bytes) — so a smaller buffer would truncate a probe the
//! path carried. A buffer of that size per session would cost 64 KiB × every pooled session of a large
//! fleet; one per shard costs 64 KiB per thread, however many sessions it serves.
//!
//! The buffer is lent through a `Cell<Option<Vec<u8>>>`: taken for the read, put back after. A read that
//! finds it already taken — a nested read on the same thread — reads into a fresh buffer instead, so the
//! lending can never fail or panic (no `RefCell` borrow held across anything), and the thread keeps at
//! most the one spare. No borrow outlives the synchronous read: the caller awaits readiness first
//! ([`slates_rt::udp::UdpSocket::readable`]) and never holds the buffer across an await.

use std::cell::Cell;

use slates_rt::error::RtError;
use slates_rt::udp::{SocketAddrV4, UdpSocket};

use crate::params::DEFAULT_MAX_UDP_PAYLOAD;

/// Derived: the receive buffer's size — the largest UDP payload an end declares it reads
/// ([`DEFAULT_MAX_UDP_PAYLOAD`], RFC 9000 §18.2), so no datagram the declaration admits is truncated.
pub const RECEIVE_BUFFER_BYTES: usize = DEFAULT_MAX_UDP_PAYLOAD as usize;

thread_local! {
  /// This thread's spare receive buffer, when no read holds it.
  static SPARE: Cell<Option<Vec<u8>>> = const { Cell::new(None) };
}

/// Takes one waiting datagram off `socket` without blocking, through the thread's receive buffer: the
/// datagram's bytes (exactly its length) and its sender, or `None` when nothing is waiting.
pub fn try_receive(socket: &UdpSocket) -> Result<Option<(Vec<u8>, SocketAddrV4)>, RtError> {
  with_datagram(socket, |datagram, from| (datagram.to_vec(), from))
}

/// Takes one waiting datagram off `socket` without blocking and hands it to `use_datagram` in the thread's
/// receive buffer, without copying it out: what `use_datagram` returns, or `None` when nothing is waiting.
pub fn with_datagram<R>(
  socket: &UdpSocket,
  use_datagram: impl FnOnce(&[u8], SocketAddrV4) -> R,
) -> Result<Option<R>, RtError> {
  let mut buffer = SPARE
    .take()
    .unwrap_or_else(|| vec![0u8; RECEIVE_BUFFER_BYTES]);
  let received = socket.try_recv_from(&mut buffer);
  let outcome = received.map(|read| {
    read.map(|(length, from)| use_datagram(buffer.get(..length).unwrap_or_default(), from))
  });
  SPARE.set(Some(buffer));
  outcome
}

#[cfg(test)]
mod tests {
  use slates_rt::udp::Ipv4Addr;

  use super::*;

  /// Shape: a datagram above the 2,048-byte buffer the endpoints read into before 2026-09-28 and below
  /// macOS's default UDP datagram cap (`net.inet.udp.maxdgram`, 9,216), so it crosses loopback on every
  /// host the suite runs on.
  const LARGE: usize = 9_000;
  /// Shape: how many non-blocking reads a test makes before it calls a loopback datagram lost — loopback
  /// delivers within microseconds; this bounds the loop, it does not time anything.
  const READ_ATTEMPTS: usize = 1_000_000;

  fn pair() -> (UdpSocket, UdpSocket, SocketAddrV4) {
    let receiver = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let sender = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = receiver.local_addr().unwrap();
    (receiver, sender, target)
  }

  fn receive_one(socket: &UdpSocket) -> (Vec<u8>, SocketAddrV4) {
    for _ in 0..READ_ATTEMPTS {
      if let Some(datagram) = try_receive(socket).unwrap() {
        return datagram;
      }
      std::hint::spin_loop();
    }
    panic!("a loopback datagram arrived");
  }

  /// §4.10a (path MTU discovery's receive side): a datagram larger than the old fixed buffer arrives whole
  /// through the shard's receive buffer — exactly its bytes, from its sender — so a probe the path carries
  /// is never truncated by the reader.
  #[test]
  fn a_datagram_above_the_old_buffer_arrives_whole() {
    let (receiver, sender, target) = pair();
    let payload: Vec<u8> = (0..LARGE)
      .map(|index| u8::try_from(index % 251).unwrap_or(0))
      .collect();
    sender.send_to(&payload, target).unwrap();
    let (datagram, from) = receive_one(&receiver);
    assert_eq!(datagram, payload, "every byte, and no more");
    assert_eq!(from, sender.local_addr().unwrap());
  }

  /// The lending cannot fail: a read made while the thread's buffer is lent (a nested read) reads into a
  /// buffer of its own, and both datagrams arrive intact.
  #[test]
  fn a_nested_read_gets_its_own_buffer() {
    let (outer_socket, sender, outer_target) = pair();
    let (inner_socket, _, inner_target) = pair();
    sender.send_to(b"outer", outer_target).unwrap();
    sender.send_to(b"inner", inner_target).unwrap();
    let mut seen = None;
    for _ in 0..READ_ATTEMPTS {
      seen = with_datagram(&outer_socket, |outer, _| {
        (outer.to_vec(), receive_one(&inner_socket).0)
      })
      .unwrap();
      if seen.is_some() {
        break;
      }
      std::hint::spin_loop();
    }
    assert_eq!(seen, Some((b"outer".to_vec(), b"inner".to_vec())));
  }
}
