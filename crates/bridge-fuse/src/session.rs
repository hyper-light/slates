//! What a FUSE connection negotiated, kept with its device so a restarted daemon can serve it (§4.6 "Linux":
//! "restore from the anchor's held fd"; AC-3.4). The kernel sends `FUSE_INIT` once per connection and never
//! again, so a daemon that takes over a held device cannot learn the negotiation from the kernel. It learns it
//! from this value, which the daemon that negotiated it handed to the anchor beside the device.
//!
//! What it holds is exactly what serving depends on after `INIT`: whether the kernel honours expire-only entry
//! invalidations (the coherence round's choice between expiring and dropping an entry), and whether the kernel
//! can resend the requests the previous daemon read but never answered (`FUSE_HAS_RESEND`), without which a
//! takeover would leave those callers waiting forever and is refused. The reply encoders take no negotiated
//! parameter, so nothing else is needed.
//!
//! The bytes cross a process boundary (daemon to anchor to the next daemon), so they are external input to the
//! daemon that reads them: a fixed length, a format byte, and only the defined bits, or a typed refusal.

use crate::init::InitNegotiation;

/// Format: the encoding's version, the first byte.
const FORMAT: u8 = 1;
/// Format: the bytes of an encoded session: the format byte and one byte of capability bits.
pub const SESSION_BYTES: usize = 2;
/// Format: the capability bit for an expire-only kernel.
const EXPIRE_ONLY_BIT: u8 = 1 << 0;
/// Format: the capability bit for a kernel that can resend.
const RESENDS_BIT: u8 = 1 << 1;
/// Format: every defined capability bit; any other is refused.
const DEFINED_BITS: u8 = EXPIRE_ONLY_BIT | RESENDS_BIT;

/// What a connection negotiated at `FUSE_INIT` that serving it later depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
  /// The kernel honours `FUSE_EXPIRE_ONLY` on an entry invalidation.
  pub expire_only: bool,
  /// The kernel can resend the requests a daemon read and never answered (`FUSE_NOTIFY_RESEND`).
  pub kernel_resends: bool,
}

/// Why session bytes were refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRefusal {
  /// Not [`SESSION_BYTES`] long.
  Length {
    /// The bytes present.
    have: usize,
  },
  /// A format this daemon does not read.
  Format {
    /// The format byte present.
    format: u8,
  },
  /// A capability bit this format does not define.
  UndefinedBits {
    /// The capability byte present.
    bits: u8,
  },
}

impl Session {
  /// The session a negotiation produced.
  pub fn of(negotiated: &InitNegotiation) -> Session {
    Session {
      expire_only: negotiated.flags & crate::abi::flags::HAS_EXPIRE_ONLY != 0,
      kernel_resends: negotiated.kernel_resends,
    }
  }

  /// The session's bytes.
  pub fn to_bytes(self) -> [u8; SESSION_BYTES] {
    let mut bits = 0;
    if self.expire_only {
      bits |= EXPIRE_ONLY_BIT;
    }
    if self.kernel_resends {
      bits |= RESENDS_BIT;
    }
    [FORMAT, bits]
  }

  /// The session `bytes` encode, or why they do not.
  pub fn from_bytes(bytes: &[u8]) -> Result<Session, SessionRefusal> {
    let [format, bits] = <[u8; SESSION_BYTES]>::try_from(bytes)
      .map_err(|_| SessionRefusal::Length { have: bytes.len() })?;
    if format != FORMAT {
      return Err(SessionRefusal::Format { format });
    }
    if bits & !DEFINED_BITS != 0 {
      return Err(SessionRefusal::UndefinedBits { bits });
    }
    Ok(Session {
      expire_only: bits & EXPIRE_ONLY_BIT != 0,
      kernel_resends: bits & RESENDS_BIT != 0,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Golden vectors: every session encodes to its bytes and decodes back.
  #[test]
  fn every_session_round_trips_through_its_golden_bytes() {
    let vectors = [
      (false, false, [1, 0]),
      (true, false, [1, 1]),
      (false, true, [1, 2]),
      (true, true, [1, 3]),
    ];
    for (expire_only, kernel_resends, bytes) in vectors {
      let session = Session {
        expire_only,
        kernel_resends,
      };
      assert_eq!(session.to_bytes(), bytes);
      assert_eq!(Session::from_bytes(&bytes), Ok(session));
    }
  }

  /// Hostile input: wrong lengths, a foreign format and undefined bits are each refused by kind.
  #[test]
  fn hostile_session_bytes_are_refused_by_kind() {
    let cases: [(&[u8], SessionRefusal); 6] = [
      (&[], SessionRefusal::Length { have: 0 }),
      (&[1], SessionRefusal::Length { have: 1 }),
      (&[1, 0, 0], SessionRefusal::Length { have: 3 }),
      (&[0, 0], SessionRefusal::Format { format: 0 }),
      (&[0xFF, 0], SessionRefusal::Format { format: 0xFF }),
      (&[1, 4], SessionRefusal::UndefinedBits { bits: 4 }),
    ];
    for (bytes, refusal) in cases {
      assert_eq!(Session::from_bytes(bytes), Err(refusal), "{bytes:?}");
    }
  }

  /// Hostile input: every single-bit flip of a valid encoding is refused by format or bits, or decodes to a
  /// defined session that re-encodes to itself; nothing panics and nothing is read past the bytes.
  #[test]
  fn every_bit_flip_of_a_session_is_refused_or_a_defined_session() {
    let valid = [1u8, 3];
    for byte in 0..SESSION_BYTES {
      for bit in 0..u8::BITS {
        let mut flipped = valid;
        if let Some(target) = flipped.get_mut(byte) {
          *target ^= 1 << bit;
        }
        let decoded = Session::from_bytes(&flipped);
        let defined =
          decoded.is_ok_and(|session| Session::from_bytes(&session.to_bytes()) == Ok(session));
        let refused = matches!(
          decoded,
          Err(SessionRefusal::Format { .. } | SessionRefusal::UndefinedBits { .. })
        );
        assert!(defined || refused, "{decoded:?} for {flipped:?}");
      }
    }
  }
}
