//! A version-control credential as fleet state (§4.13 "Principals"; A-625 part 3; main's ruling of 2026-11-20 on
//! w-balance's takeover test): every member of the region holds every credential a human enrolled anywhere in it, so
//! the HTTP and SSH edges authenticate a credential with one lookup on the node a client reached, and a client's token
//! still proves its principal on a successor after the node that enrolled it is lost.
//!
//! **The exchange.** The node that enrolls or revokes a credential sends it to every other member of its region on
//! [`CREDENTIAL_STREAM`]: [`CredentialMessage::Enroll`] with the record in its own wire encoding
//! (`slates_db::catalog::CredentialRecord`), or [`CredentialMessage::Revoke`] with its id. A member applies it to the
//! partition the id routes to on its own daemon, as its own enrollment would, and answers [`CredentialMessage::Answer`]:
//! held (the record is there, or for a revocation is gone) or not. An enrollment is acknowledged to the human once `f +
//! 1` members hold it, the enrolling node among them, so it survives `f` losses; one a member missed reaches it at the
//! next configuration change, when every member offers what it holds (the healer, `slates-server`).
//!
//! **Idempotent.** An enrollment of a record a member holds already, the same record, is held; a revocation of an id a
//! member does not hold is held too (it is gone either way). Neither ever fails a member for having seen it before.
//!
//! **The wire.** One kind byte, the id, then for an enrollment the record's length (`u32`, little-endian) and its
//! encoding, for an answer one byte, 1 held and 0 not. Nothing trails a message.

use slates_db::catalog::CredentialRecord;
use slates_wire::Wire;

/// Format: the record-session stream a credential exchange travels on (after the homes', 30).
pub const CREDENTIAL_STREAM: u64 = 31;

/// Format: the kind bytes.
const KIND_ENROLL: u8 = 1;
/// Format: see [`KIND_ENROLL`].
const KIND_REVOKE: u8 = 2;
/// Format: see [`KIND_ENROLL`].
const KIND_ANSWER: u8 = 3;
/// Shape: the longest record an enrollment carries: an SSH key's blob is the largest part, an RSA key of 8192 bits about
/// 1 KiB, and a token's record its CRAM-MD5 contexts; anything past this is not a credential's.
pub const MAX_RECORD_BYTES: usize = 16 * 1024;

/// A credential exchange's message (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialMessage {
  /// Hold this credential.
  Enroll {
    /// The record, whole.
    record: CredentialRecord,
  },
  /// Let the credential named `id` go.
  Revoke {
    /// Its id.
    id: [u8; 32],
  },
  /// A member's answer about the credential named `id`.
  Answer {
    /// Its id.
    id: [u8; 32],
    /// Whether the member holds it as asked (enrolled, or for a revocation gone).
    held: bool,
  },
}

/// A credential message's bytes refused (module docs, "The wire").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialWireError {
  /// The bytes end before a field they declare.
  Truncated,
  /// A kind or answer byte names nothing.
  Unknown {
    /// The byte found.
    byte: u8,
  },
  /// A length is past the bytes or [`MAX_RECORD_BYTES`], bytes trail the message, or the record's encoding is refused.
  BadLength,
}

impl CredentialMessage {
  /// The id the message is about.
  pub fn id(&self) -> [u8; 32] {
    match self {
      CredentialMessage::Enroll { record } => record.id,
      CredentialMessage::Revoke { id } | CredentialMessage::Answer { id, .. } => *id,
    }
  }

  /// The message's bytes (module docs, "The wire").
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      CredentialMessage::Enroll { record } => {
        out.push(KIND_ENROLL);
        out.extend_from_slice(&record.id);
        let mut encoded = Vec::new();
        record.encode(&mut encoded);
        out.extend_from_slice(
          &u32::try_from(encoded.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
        );
        out.extend_from_slice(&encoded);
      }
      CredentialMessage::Revoke { id } => {
        out.push(KIND_REVOKE);
        out.extend_from_slice(id);
      }
      CredentialMessage::Answer { id, held } => {
        out.push(KIND_ANSWER);
        out.extend_from_slice(id);
        out.push(u8::from(*held));
      }
    }
    out
  }

  /// A message from its bytes, or why they are refused. An enrollment whose record names another id than the message's
  /// is refused: a member never holds a record under a name it does not have.
  pub fn decode(bytes: &[u8]) -> Result<CredentialMessage, CredentialWireError> {
    let (&kind, rest) = bytes.split_first().ok_or(CredentialWireError::Truncated)?;
    // Format: the message's credential id, 32 bytes (`CredentialMessage::id`'s `[u8; 32]`).
    let (id, rest) = rest
      .split_first_chunk::<32>()
      .ok_or(CredentialWireError::Truncated)?;
    let id = *id;
    let (message, rest) = match kind {
      KIND_ENROLL => {
        // Format: the record's length, a little-endian `u32` (4 bytes).
        let (length, rest) = rest
          .split_first_chunk::<4>()
          .ok_or(CredentialWireError::Truncated)?;
        let length = usize::try_from(u32::from_le_bytes(*length)).unwrap_or(usize::MAX);
        if length > MAX_RECORD_BYTES || length > rest.len() {
          return Err(CredentialWireError::BadLength);
        }
        let (mut encoded, rest) = rest.split_at(length);
        let record =
          CredentialRecord::decode(&mut encoded).map_err(|_| CredentialWireError::BadLength)?;
        if !encoded.is_empty() || record.id != id {
          return Err(CredentialWireError::BadLength);
        }
        (CredentialMessage::Enroll { record }, rest)
      }
      KIND_REVOKE => (CredentialMessage::Revoke { id }, rest),
      KIND_ANSWER => {
        let (&held, rest) = rest.split_first().ok_or(CredentialWireError::Truncated)?;
        let held = match held {
          0 => false,
          1 => true,
          byte => return Err(CredentialWireError::Unknown { byte }),
        };
        (CredentialMessage::Answer { id, held }, rest)
      }
      byte => return Err(CredentialWireError::Unknown { byte }),
    };
    if rest.is_empty() {
      Ok(message)
    } else {
      Err(CredentialWireError::BadLength)
    }
  }
}

/// Whether an enrollment held at `held` members, the enrolling node among them, of a region at `f`, survives `f` losses:
/// `f + 1` holders (module docs).
pub fn enrolled_durably(held: usize, f: usize) -> bool {
  held >= f.saturating_add(1)
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_db::catalog::{CredentialKind, Principal};

  fn record(id: u8) -> CredentialRecord {
    CredentialRecord {
      id: [id; 32],
      principal: Principal::Uid {
        host: slates_db::HostId(7),
        uid: 501,
      },
      kind: CredentialKind::HttpToken {
        cram_md5_contexts: [9; 32],
      },
      expires_ns: Some(42),
    }
  }

  /// A-764. Do: encode each message and decode it back. Expect the same message, and its id.
  #[test]
  fn a764_each_credential_message_comes_back_as_it_went() {
    for message in [
      CredentialMessage::Enroll { record: record(3) },
      CredentialMessage::Revoke { id: [4; 32] },
      CredentialMessage::Answer {
        id: [5; 32],
        held: true,
      },
      CredentialMessage::Answer {
        id: [6; 32],
        held: false,
      },
    ] {
      let bytes = message.encode();
      assert_eq!(CredentialMessage::decode(&bytes), Ok(message.clone()));
      assert_eq!(
        CredentialMessage::decode(&bytes).map(|m| m.id()),
        Ok(message.id())
      );
    }
  }

  /// A-764. Do: decode bytes cut short, trailing, of an unknown kind or answer, or an enrollment whose record names
  /// another id. Expect each refused, never a message.
  #[test]
  fn a764_a_credential_message_refuses_what_is_not_one() {
    let enroll = CredentialMessage::Enroll { record: record(3) }.encode();
    for cut in 0..enroll.len() {
      assert!(
        CredentialMessage::decode(enroll.get(..cut).unwrap_or_default()).is_err(),
        "cut at {cut}"
      );
    }
    let mut trailing = enroll.clone();
    trailing.push(0);
    assert_eq!(
      CredentialMessage::decode(&trailing),
      Err(CredentialWireError::BadLength)
    );
    let mut unknown = CredentialMessage::Revoke { id: [1; 32] }.encode();
    if let Some(kind) = unknown.first_mut() {
      *kind = 9;
    }
    assert_eq!(
      CredentialMessage::decode(&unknown),
      Err(CredentialWireError::Unknown { byte: 9 })
    );
    let mut answer = CredentialMessage::Answer {
      id: [1; 32],
      held: true,
    }
    .encode();
    if let Some(held) = answer.last_mut() {
      *held = 2;
    }
    assert_eq!(
      CredentialMessage::decode(&answer),
      Err(CredentialWireError::Unknown { byte: 2 })
    );
    let mut renamed = enroll;
    if let Some(byte) = renamed.get_mut(1) {
      *byte = 0xee;
    }
    assert_eq!(
      CredentialMessage::decode(&renamed),
      Err(CredentialWireError::BadLength),
      "a record under another id"
    );
  }

  /// A-764. Do: judge holders against `f`. Expect `f + 1` to survive `f` losses, fewer not.
  #[test]
  fn a764_an_enrollment_is_durable_at_f_plus_one_holders() {
    assert!(enrolled_durably(1, 0), "a fleet of one");
    assert!(!enrolled_durably(1, 1));
    assert!(enrolled_durably(2, 1));
    assert!(enrolled_durably(3, 1));
    assert!(!enrolled_durably(2, 2));
  }
}
