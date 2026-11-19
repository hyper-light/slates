//! The transport's refusals (A-594): one closed taxonomy. Each refusal ends the connection, and each names the reason
//! the server's `SSH_MSG_DISCONNECT` carries (RFC 4253 §11.1; RFC 4250 §4.2.2's numbers), so a client is told in the
//! protocol's own terms why it was refused.

use crate::ident::IdentificationRefusal;
use crate::kex::KexRefusal;
use crate::kexinit::KexInitRefusal;
use crate::packet::PacketRefusal;
use crate::wire::WireRefusal;

/// The disconnect reasons the server sends (RFC 4250 §4.2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisconnectReason {
  /// `SSH_DISCONNECT_PROTOCOL_ERROR`, 2.
  ProtocolError,
  /// `SSH_DISCONNECT_KEY_EXCHANGE_FAILED`, 3.
  KeyExchangeFailed,
  /// `SSH_DISCONNECT_MAC_ERROR`, 5.
  MacError,
  /// `SSH_DISCONNECT_SERVICE_NOT_AVAILABLE`, 7.
  ServiceNotAvailable,
  /// `SSH_DISCONNECT_PROTOCOL_VERSION_NOT_SUPPORTED`, 8.
  ProtocolVersionNotSupported,
  /// `SSH_DISCONNECT_BY_APPLICATION`, 11.
  ByApplication,
}

impl DisconnectReason {
  /// The reason's number on the wire (RFC 4250 §4.2.2).
  pub fn code(self) -> u32 {
    match self {
      DisconnectReason::ProtocolError => PROTOCOL_ERROR,
      DisconnectReason::KeyExchangeFailed => KEY_EXCHANGE_FAILED,
      DisconnectReason::MacError => MAC_ERROR,
      DisconnectReason::ServiceNotAvailable => SERVICE_NOT_AVAILABLE,
      DisconnectReason::ProtocolVersionNotSupported => PROTOCOL_VERSION_NOT_SUPPORTED,
      DisconnectReason::ByApplication => BY_APPLICATION,
    }
  }
}

/// Format: `SSH_DISCONNECT_PROTOCOL_ERROR` (RFC 4250 §4.2.2).
const PROTOCOL_ERROR: u32 = 2;
/// Format: `SSH_DISCONNECT_KEY_EXCHANGE_FAILED`.
const KEY_EXCHANGE_FAILED: u32 = 3;
/// Format: `SSH_DISCONNECT_MAC_ERROR`.
const MAC_ERROR: u32 = 5;
/// Format: `SSH_DISCONNECT_SERVICE_NOT_AVAILABLE`.
const SERVICE_NOT_AVAILABLE: u32 = 7;
/// Format: `SSH_DISCONNECT_PROTOCOL_VERSION_NOT_SUPPORTED`.
const PROTOCOL_VERSION_NOT_SUPPORTED: u32 = 8;
/// Format: `SSH_DISCONNECT_BY_APPLICATION`.
const BY_APPLICATION: u32 = 11;

/// A direction of the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
  /// What the client sends.
  ClientToServer,
  /// What the server sends.
  ServerToClient,
}

/// Why the transport ended a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The client's identification line (RFC 4253 §4.2).
  Identification(IdentificationRefusal),
  /// A binary packet's framing, tag or padding (RFC 4253 §6).
  Packet(PacketRefusal),
  /// A transport message whose fields did not read (RFC 4251 §5).
  Message {
    /// The message's number.
    number: u8,
    /// Where it stopped.
    refusal: WireRefusal,
  },
  /// The client's KEXINIT, or a list with no algorithm both sides have (RFC 4253 §7.1).
  KexInit(KexInitRefusal),
  /// The key exchange's own refusals (RFC 8731 §3).
  Kex(KexRefusal),
  /// A message the exchange's state does not allow now (RFC 4253 §7.1; `draft-ietf-sshm-strict-kex-03` §3.2).
  Unexpected {
    /// The message's number.
    number: u8,
  },
  /// Under strict key exchange, the client's KEXINIT was not its first message (`draft-ietf-sshm-strict-kex-03` §3.2).
  KexInitNotFirst,
  /// A sequence number would wrap before the first exchange ended (`draft-ietf-sshm-strict-kex-03` §3.2).
  SequenceWrapBeforeKeys,
  /// The client asked for a service the server does not run (RFC 4253 §10).
  ServiceNotAvailable,
  /// A key reached the most its cipher may process (RFC 4344 §3) without a new exchange to replace it.
  KeyExhausted {
    /// The direction whose key ran out.
    direction: Direction,
  },
  /// The random bytes the transport needs could not be drawn.
  Entropy,
}

impl Refusal {
  /// The reason the server's disconnect carries.
  pub fn reason(&self) -> DisconnectReason {
    match self {
      Refusal::Identification(IdentificationRefusal::UnsupportedVersion) => {
        DisconnectReason::ProtocolVersionNotSupported
      }
      Refusal::Packet(PacketRefusal::TagMismatch) => DisconnectReason::MacError,
      Refusal::KexInit(_) | Refusal::Kex(_) => DisconnectReason::KeyExchangeFailed,
      Refusal::ServiceNotAvailable => DisconnectReason::ServiceNotAvailable,
      Refusal::Entropy => DisconnectReason::ByApplication,
      Refusal::Identification(_)
      | Refusal::Packet(_)
      | Refusal::Message { .. }
      | Refusal::Unexpected { .. }
      | Refusal::KexInitNotFirst
      | Refusal::SequenceWrapBeforeKeys
      | Refusal::KeyExhausted { .. } => DisconnectReason::ProtocolError,
    }
  }

  /// The description the server's disconnect carries, in plain English.
  pub fn description(&self) -> &'static str {
    match self {
      Refusal::Identification(_) => "invalid SSH identification string",
      Refusal::Packet(PacketRefusal::TagMismatch) => "packet authentication failed",
      Refusal::Packet(_) => "malformed packet",
      Refusal::Message { .. } => "malformed message",
      Refusal::KexInit(KexInitRefusal::NoCommonAlgorithm(_)) => "no matching algorithm",
      Refusal::KexInit(_) => "malformed key exchange init",
      Refusal::Kex(_) => "key exchange failed",
      Refusal::Unexpected { .. } => "unexpected message",
      Refusal::KexInitNotFirst => "strict key exchange: key exchange init was not first",
      Refusal::SequenceWrapBeforeKeys => {
        "sequence number wrapped before the first key exchange ended"
      }
      Refusal::ServiceNotAvailable => "service not available",
      Refusal::KeyExhausted { .. } => "key exhausted before a new key exchange",
      Refusal::Entropy => "random bytes unavailable",
    }
  }
}
