//! The issuer surface on Linux (A-754; §4.13 "Grants", A-299): how a process the anchor did not start reaches the
//! anchor's segment, and with it the issuer secret, as an operator's shell does on macOS and Windows.
//!
//! On macOS and Windows the segment is a named object whose mode or DACL admits the anchor's user alone, and the anchor
//! prints its name for the operator to export: whoever holds that name can attach the segment and prove the human's
//! authority. On Linux the segment is a memfd that only the anchor's children inherit, and `slates anchor` starts no
//! child an operator runs, so no user process could mint a credential (`slates credential token|key`), and stock git,
//! hg and svn could not authenticate to any edge there (w-interop, 2026-11-20).
//!
//! Main's ruling, matching the other platforms rather than widening anything: the anchor prints an issuer secret
//! beside the name of an abstract-namespace socket of its own, [`route_name`]; a client connects, sends the secret, and
//! the anchor answers with the segment's descriptor (`SCM_RIGHTS`) and its length, only where the peer's user is the
//! anchor's (`SO_PEERCRED`) and the secret is the one it printed, compared in constant time. Same-uid alone is not
//! enough: whoever holds the printed secret can issue, exactly as whoever holds the printed name can elsewhere. The
//! socket is abstract, so it has no file anyone could open or remove; it lives as long as the anchor holds it.
//!
//! The anchor answers waiting claims once a turn of its loop ([`IssuerRoute::answer_waiting`]), never waiting on a
//! client longer than the patience it passes; a refused claim is closed unanswered, which the client reads as a refusal.

use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
  SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};

use crate::error::IpcError;

/// Format: the variable naming the issuer route's abstract socket ([`route_name`]), which the anchor prints for the
/// operator to export (Linux).
pub const ENV_ISSUER_ROUTE: &str = "SLATES_ANCHOR_ISSUER_ROUTE";

/// Format: the variable holding the issuer secret the anchor printed, in lowercase hexadecimal (Linux).
pub const ENV_ISSUER_SECRET: &str = "SLATES_ANCHOR_ISSUER_SECRET";

/// Format: the issuer secret's width: 256 bits, the width of the daemon's own issuer secret (`ISSUER_SECRET_BYTES`).
pub const SECRET_BYTES: usize = 32;

/// Format: the answer's body: the segment's length, eight little-endian bytes, beside its descriptor.
const LENGTH_BYTES: usize = 8;

/// Shape: the claims the listener queues while the anchor is between turns: an operator's few commands at once.
const BACKLOG: i32 = 16;

/// The abstract name of `instance`'s issuer route for user `uid`, beside its client rendezvous's (`slates-rv-<instance>`).
pub fn route_name(instance: &str, uid: u32) -> String {
  let clean: String = instance
    .chars()
    .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
    .collect();
  format!("slates-issuer-{clean}/{uid}")
}

/// `secret` as the anchor prints it: lowercase hexadecimal.
pub fn secret_text(secret: &[u8; SECRET_BYTES]) -> String {
  secret.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The secret `text` writes in [`secret_text`]'s form, none for anything else.
pub fn parse_secret(text: &str) -> Option<[u8; SECRET_BYTES]> {
  let bytes = text.as_bytes();
  if bytes.len() != SECRET_BYTES.saturating_mul(2) {
    return None;
  }
  let mut secret = [0u8; SECRET_BYTES];
  let (pairs, _) = bytes.as_chunks::<2>();
  for (slot, pair) in secret.iter_mut().zip(pairs) {
    let text = std::str::from_utf8(pair).ok()?;
    *slot = u8::from_str_radix(text, 16).ok()?;
  }
  Some(secret)
}

/// Whether `first` and `second` are equal, in time that does not depend on where they differ.
fn same_secret(first: &[u8], second: &[u8; SECRET_BYTES]) -> bool {
  first.len() == SECRET_BYTES
    && first
      .iter()
      .zip(second)
      .fold(0u8, |differ, (a, b)| differ | (a ^ b))
      == 0
}

fn refused(call: &'static str, e: rustix::io::Errno) -> IpcError {
  IpcError::OsRefused {
    call,
    code: Some(e.raw_os_error()),
  }
}

fn address(name: &str) -> Result<SocketAddrUnix, IpcError> {
  SocketAddrUnix::new_abstract_name(name.as_bytes()).map_err(|e| refused("abstract name", e))
}

/// The socket a client waits on, read and written within `patience`.
fn with_patience(socket: &OwnedFd, patience: std::time::Duration) -> Result<(), IpcError> {
  rustix::net::sockopt::set_socket_timeout(
    socket,
    rustix::net::sockopt::Timeout::Recv,
    Some(patience),
  )
  .map_err(|e| refused("SO_RCVTIMEO", e))?;
  rustix::net::sockopt::set_socket_timeout(
    socket,
    rustix::net::sockopt::Timeout::Send,
    Some(patience),
  )
  .map_err(|e| refused("SO_SNDTIMEO", e))
}

/// What one turn of [`IssuerRoute::answer_waiting`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Answered {
  /// Claims answered with the segment.
  pub granted: u32,
  /// Claims refused: another user, a wrong or short secret, or a peer that did not send it in time.
  pub refused: u32,
}

/// The anchor's issuer route: a listening abstract socket of its own, not inherited by its daemons.
#[derive(Debug)]
pub struct IssuerRoute {
  listener: OwnedFd,
  name: String,
  uid: u32,
}

impl IssuerRoute {
  /// Binds `instance`'s issuer route for this process's user. Refused where another anchor of this user and instance
  /// holds it (`EADDRINUSE`).
  pub fn bind(instance: &str) -> Result<IssuerRoute, IpcError> {
    let uid = rustix::process::getuid().as_raw();
    let name = route_name(instance, uid);
    let listener = rustix::net::socket_with(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      None,
    )
    .map_err(|e| refused("socket", e))?;
    rustix::net::bind(&listener, &address(&name)?).map_err(|e| refused("bind", e))?;
    rustix::net::listen(&listener, BACKLOG).map_err(|e| refused("listen", e))?;
    Ok(IssuerRoute {
      listener,
      name,
      uid,
    })
  }

  /// The route's abstract name, which the anchor prints ([`ENV_ISSUER_ROUTE`]).
  pub fn name(&self) -> &str {
    &self.name
  }

  /// Answers every claim waiting now, each read and written within `patience`: a peer of this user that sends
  /// `secret` is answered with `segment` and its `len`; any other is closed unanswered. Returns at once when none
  /// waits; a refused accept ends the turn.
  pub fn answer_waiting(
    &self,
    secret: &[u8; SECRET_BYTES],
    segment: BorrowedFd<'_>,
    len: u64,
    patience: std::time::Duration,
  ) -> Answered {
    let mut answered = Answered::default();
    loop {
      let peer = match rustix::net::accept_with(&self.listener, SocketFlags::CLOEXEC) {
        Ok(peer) => peer,
        Err(_) => return answered,
      };
      if self.answer(&peer, secret, segment, len, patience).is_ok() {
        answered.granted = answered.granted.saturating_add(1);
      } else {
        answered.refused = answered.refused.saturating_add(1);
      }
    }
  }

  /// Answers one claim: refused unless its peer is this user and it sends the secret within `patience`.
  fn answer(
    &self,
    peer: &OwnedFd,
    secret: &[u8; SECRET_BYTES],
    segment: BorrowedFd<'_>,
    len: u64,
    patience: std::time::Duration,
  ) -> Result<(), IpcError> {
    let credentials = crate::peer::peer_credentials(peer.as_fd())?;
    if credentials.uid != self.uid {
      return Err(IpcError::PeerRefused {
        uid: credentials.uid,
      });
    }
    with_patience(peer, patience)?;
    let mut offered = [0u8; SECRET_BYTES];
    let mut read = 0usize;
    while read < SECRET_BYTES {
      let rest = offered.get_mut(read..).unwrap_or_default();
      match rustix::net::recv(peer, rest, RecvFlags::empty()) {
        Ok((0, _)) | Err(_) => break,
        Ok((got, _)) => read = read.saturating_add(got),
      }
    }
    if !same_secret(offered.get(..read).unwrap_or_default(), secret) {
      return Err(IpcError::PeerRefused {
        uid: credentials.uid,
      });
    }
    let body = len.to_le_bytes();
    let fds = [segment];
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
      return Err(IpcError::Layout {
        reason: "the issuer route's answer had no room for the segment",
      });
    }
    rustix::net::sendmsg(
      peer,
      &[IoSlice::new(&body)],
      &mut control,
      SendFlags::NOSIGNAL,
    )
    .map_err(|e| refused("sendmsg issuer", e))?;
    Ok(())
  }
}

/// Claims the segment from the anchor's issuer route `name` with `secret`, waiting at most `patience` for each step:
/// the segment's descriptor and its length. Refused where nothing listens, or the anchor closed the claim unanswered
/// (another user, or a secret that is not the one it printed).
pub fn claim(
  name: &str,
  secret: &[u8; SECRET_BYTES],
  patience: std::time::Duration,
) -> Result<(OwnedFd, u64), IpcError> {
  let socket = rustix::net::socket_with(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC,
    None,
  )
  .map_err(|e| refused("socket", e))?;
  with_patience(&socket, patience)?;
  rustix::net::connect(&socket, &address(name)?).map_err(|e| refused("connect issuer", e))?;
  let sent = rustix::net::send(&socket, secret, SendFlags::NOSIGNAL)
    .map_err(|e| refused("send issuer", e))?;
  if sent != SECRET_BYTES {
    return Err(IpcError::Layout {
      reason: "the issuer secret was sent short",
    });
  }
  let mut body = [0u8; LENGTH_BYTES];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
  let mut control = RecvAncillaryBuffer::new(&mut space);
  let received = rustix::net::recvmsg(
    &socket,
    &mut [IoSliceMut::new(&mut body)],
    &mut control,
    RecvFlags::CMSG_CLOEXEC,
  )
  .map_err(|e| refused("recvmsg issuer", e))?;
  let mut segment = None;
  for message in control.drain() {
    if let RecvAncillaryMessage::ScmRights(mut fds) = message {
      segment = segment.or_else(|| fds.next());
    }
  }
  let segment = segment.ok_or(IpcError::Layout {
    reason: "the anchor answered the issuer claim with no segment (refused, or another user's anchor)",
  })?;
  if received.bytes != LENGTH_BYTES {
    return Err(IpcError::Layout {
      reason: "the issuer route's answer was short",
    });
  }
  Ok((segment, u64::from_le_bytes(body)))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-754. Do: print a secret and read it back; read texts a byte short, a byte long and not hexadecimal; compare a
  /// secret with itself, a one-bit change and a short prefix. Expect: the secret back; each bad text refused; only the
  /// same secret equal.
  #[test]
  fn a_printed_secret_reads_back_and_compares_whole() {
    let mut secret = [0u8; SECRET_BYTES];
    for (index, byte) in secret.iter_mut().enumerate() {
      *byte = u8::try_from(index * 7).unwrap();
    }
    let text = secret_text(&secret);
    assert_eq!(parse_secret(&text), Some(secret));
    assert_eq!(parse_secret(text.get(1..).unwrap()), None);
    assert_eq!(parse_secret(&format!("{text}0")), None);
    assert_eq!(parse_secret(&"zz".repeat(SECRET_BYTES)), None);
    assert!(same_secret(&secret, &secret));
    let mut flipped = secret;
    flipped[SECRET_BYTES - 1] ^= 1;
    assert!(!same_secret(&flipped, &secret));
    assert!(!same_secret(&secret[..SECRET_BYTES - 1], &secret));
  }

  /// A-754 across a socket. Do: bind a route; from another thread, claim it with the right secret, then with a wrong
  /// one; answer the waiting claims between. Expect: the right claim gets the segment's descriptor (it names the same
  /// object: its size is the one sent) and its length; the wrong one is refused with no descriptor; one granted, one
  /// refused.
  #[test]
  fn a_claim_with_the_printed_secret_gets_the_segment_and_another_does_not() {
    let instance = format!("issuer-route-test-{}", std::process::id());
    let route = IssuerRoute::bind(&instance).unwrap();
    let name = route.name().to_owned();
    let segment =
      rustix::fs::memfd_create("issuer-route-test", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    #[allow(clippy::disallowed_methods)] // sizing a memory object: RAM, not a host path (R1).
    rustix::fs::ftruncate(&segment, 4096).unwrap();
    let secret = [9u8; SECRET_BYTES];
    let patience = std::time::Duration::from_secs(10);
    let claimer = {
      let name = name.clone();
      std::thread::spawn(move || {
        let right = claim(&name, &[9u8; SECRET_BYTES], patience);
        let wrong = claim(&name, &[8u8; SECRET_BYTES], patience);
        (right, wrong)
      })
    };
    let mut answered = Answered::default();
    let started = std::time::Instant::now();
    while answered.granted + answered.refused < 2 && started.elapsed() < patience {
      let turn = route.answer_waiting(&secret, segment.as_fd(), 4096, patience);
      answered.granted += turn.granted;
      answered.refused += turn.refused;
      std::thread::yield_now();
    }
    let (right, wrong) = claimer.join().unwrap();
    let (fd, len) = right.unwrap();
    assert_eq!(len, 4096);
    assert_eq!(rustix::fs::fstat(&fd).unwrap().st_size, 4096);
    assert!(wrong.is_err(), "{wrong:?}");
    assert_eq!(
      answered,
      Answered {
        granted: 1,
        refused: 1
      }
    );
  }
}
