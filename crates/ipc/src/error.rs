//! The closed refusal taxonomy of the IPC crate (§4.7's failure matrix, §4.9's refusals).

use std::fmt;

use slates_mem::MemError;

use crate::delivery::DeliveryFault;

/// A typed refusal from the IPC crate; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
  /// The daemon is not reachable at the endpoint: `why` says what the client found (no rendezvous
  /// there, a claim it never answered, no rendezvous on this platform), so an operator reading
  /// "no daemon" learns which of those it was.
  DaemonUnavailable {
    /// The endpoint tried.
    endpoint: String,
    /// What the client found.
    why: &'static str,
  },
  /// The peer's credentials are not the daemon's user.
  PeerRefused {
    /// The peer's uid (or the SID hash on Windows).
    uid: u32,
  },
  /// The ring is full: the client blocks on credit, it never drops; a caller that cannot
  /// block sees this.
  RingFull,
  /// A slot's kind or length is not one of ours.
  BadSlot {
    /// What was wrong.
    reason: &'static str,
  },
  /// A payload larger than a slot carries and no bulk region to spill into.
  PayloadTooLarge {
    /// The bytes offered.
    offered: usize,
    /// The bytes a slot carries.
    capacity: usize,
  },
  /// The region's header is not one of ours.
  Layout {
    /// What was wrong.
    reason: &'static str,
  },
  /// The OS refused; the call is named.
  OsRefused {
    /// The call.
    call: &'static str,
    /// The OS error code, when one exists.
    code: Option<i32>,
  },
  /// The shared memory object refused.
  Memory(MemError),
  /// The rendezvous form is not available on this platform yet.
  Unsupported {
    /// The feature.
    feature: &'static str,
  },
  /// A descriptor offered to a peer would authorize more than the hand-off does (§4.6, A-135).
  /// A descriptor carries its **whole object**, read and write, to whoever holds it: the offsets and
  /// lengths in the message scope the request, never the descriptor. So one whose object is larger
  /// than the authorized window, whose window does not start at the object's start, or that the peer
  /// could still write or resize, is refused here rather than sent, and the caller serves the bytes by
  /// copy. Record: `docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md`.
  UnconfinedDescriptor {
    /// The object's whole length — what the descriptor actually authorizes.
    object_bytes: u64,
    /// The bytes the hand-off was for.
    authorized_bytes: u64,
    /// Whether the peer could still write or resize the object (no `F_SEAL_FUTURE_WRITE`/`F_SEAL_WRITE`
    /// with `F_SEAL_SHRINK` and `F_SEAL_GROW`); measured, since a read-only duplicate alone is no
    /// defence — a same-uid peer re-opens it `O_RDWR` through `/proc/self/fd`.
    writable: bool,
  },
  /// A wait ended at its deadline.
  DeadlineExceeded,
  /// The daemon's derived client bound is reached (§4.7 admission, AC-2.6).
  TooManyClients {
    /// The bound.
    limit: usize,
  },
  /// A client was assigned an id and a region but its handoff failed after that (the peer's socket
  /// refusing, a descriptor that could not be duplicated): the daemon releases the id and the client
  /// sees its connect fail. Carries the id so the release is exact, and the cause.
  HandoffLost {
    /// The id the daemon had assigned.
    client_id: u32,
    /// What failed.
    cause: Box<IpcError>,
  },
  /// The consumer capability the harness delivers on an inherited descriptor (§4.13; `delivery`)
  /// could not be taken: which fault. A process spawned as a consumer whose delivery is unusable is
  /// refused here rather than bound to the account's ambient authority; only `Absent` means the
  /// process was not spawned as a consumer at all.
  CapabilityNotDelivered {
    /// The fault.
    fault: DeliveryFault,
  },
  /// The Shards stream's socket route refused (§4.6, A-161): the daemon's answer to a claim, or the route's own
  /// place on this host.
  RouteRefused {
    /// Why.
    refusal: RouteRefusal,
  },
}

/// Why the Shards stream's socket route refused (A-161; `crate::route`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteRefusal {
  /// The daemon holds no offer for the attachment: none was made, it was claimed already, or its time ran out.
  NoOffer,
  /// The claim's proof did not open under the offer's secret.
  Proof,
  /// A message of the exchange was not the protocol's: cut short, too long, or of another version.
  Malformed,
  /// The daemon was serving its bound of exchanges at once.
  Busy,
  /// The daemon's confirmation did not open under the offer's secret: whoever answered did not make the offer.
  Confirmation,
  /// The end arrived flushed: the kernel emptied its receive buffer while it was in flight (XNU's collector of
  /// descriptors in flight), so its reads would answer end of file. The daemon keeps its copy until the client has its
  /// own, which prevents this; a flushed end is refused rather than handed to a VMM whose share would fail at once.
  Flushed,
  /// The directory the socket lives in is not the user's alone.
  SharedDirectory,
  /// The socket's path is longer than a Unix socket's address holds.
  PathTooLong,
  /// A daemon of this instance already answers at the socket.
  InUse,
}

impl fmt::Display for RouteRefusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(match self {
      Self::NoOffer => {
        "the daemon holds no offer for the attachment (none made, claimed, or expired)"
      }
      Self::Proof => "the claim's proof did not open under the offer's secret",
      Self::Malformed => "a message of the exchange was not the protocol's",
      Self::Busy => "the daemon was serving its bound of exchanges",
      Self::Confirmation => {
        "the confirmation did not open: whoever answered did not make the offer"
      }
      Self::Flushed => "the end arrived flushed by the kernel in flight; attach again",
      Self::SharedDirectory => "the socket's directory is not the user's alone",
      Self::PathTooLong => "the socket's path is longer than an address holds",
      Self::InUse => "a daemon of this instance already answers at the socket",
    })
  }
}

impl fmt::Display for IpcError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::DaemonUnavailable { endpoint, why } => {
        write!(f, "daemon unavailable at {endpoint}: {why}")
      }
      Self::PeerRefused { uid } => write!(f, "peer refused (uid {uid})"),
      Self::RingFull => f.write_str("ring full"),
      Self::BadSlot { reason } => write!(f, "bad slot: {reason}"),
      Self::PayloadTooLarge { offered, capacity } => {
        write!(
          f,
          "payload of {offered} bytes exceeds the slot's {capacity}"
        )
      }
      Self::Layout { reason } => write!(f, "region layout: {reason}"),
      Self::OsRefused { call, code } => write!(f, "{call} refused (code {code:?})"),
      Self::Memory(e) => write!(f, "shared memory: {e}"),
      Self::Unsupported { feature } => write!(f, "unsupported here: {feature}"),
      Self::UnconfinedDescriptor {
        object_bytes,
        authorized_bytes,
        writable,
      } => write!(
        f,
        "the descriptor authorizes {object_bytes} bytes where {authorized_bytes} were offered{}",
        if *writable {
          " and the peer could still write or resize it"
        } else {
          ""
        }
      ),
      Self::DeadlineExceeded => f.write_str("deadline exceeded"),
      Self::TooManyClients { limit } => write!(f, "too many clients (the bound is {limit})"),
      Self::HandoffLost { client_id, cause } => {
        write!(
          f,
          "the handoff to client {client_id} failed after admission: {cause}"
        )
      }
      Self::CapabilityNotDelivered { fault } => {
        write!(f, "consumer capability not delivered: {fault}")
      }
      Self::RouteRefused { refusal } => write!(f, "the stream route refused: {refusal}"),
    }
  }
}

impl std::error::Error for IpcError {}

impl From<MemError> for IpcError {
  fn from(e: MemError) -> Self {
    Self::Memory(e)
  }
}
