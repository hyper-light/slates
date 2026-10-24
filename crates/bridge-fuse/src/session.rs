//! What a FUSE connection negotiated, kept with its device so a restarted daemon can serve it (§4.6 "Linux":
//! "restore from the anchor's held fd"; AC-3.4). The kernel sends `FUSE_INIT` once per connection and never
//! again, so a daemon that takes over a held device cannot learn the negotiation from the kernel. It learns it
//! from this value, which the daemon that negotiated it handed to the anchor beside the device.
//!
//! What it holds is exactly what serving depends on after `INIT`:
//! - whether the kernel honours expire-only entry invalidations (the coherence round's choice between expiring and
//!   dropping an entry);
//! - whether the kernel can resend the requests the previous daemon read but never answered (`FUSE_HAS_RESEND`),
//!   without which a takeover would leave those callers waiting forever and is refused;
//! - whether the kernel can drop every name it caches at once (`FUSE_NOTIFY_INC_EPOCH`), which a takeover sends so no
//!   name the dead daemon changed and never told the kernel of outlives it (A-158).
//!
//! The reply encoders take no negotiated parameter, so nothing else is needed.
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
/// Format: the capability bit for a kernel that can bump its dentry epoch (A-158). Added to format 1 rather than
/// starting a format 2: a daemon that predates it refuses the bit as undefined and ends the mount, which is what it
/// must do with a session it cannot fully read, and every session it wrote reads here unchanged.
const EPOCH_BIT: u8 = 1 << 2;
/// Format: every defined capability bit; any other is refused.
const DEFINED_BITS: u8 = EXPIRE_ONLY_BIT | RESENDS_BIT | EPOCH_BIT;
/// Format: the first FUSE minor version whose kernel takes `FUSE_NOTIFY_INC_EPOCH` (`include/uapi/linux/fuse.h`:
/// "7.44 - add FUSE_NOTIFY_INC_EPOCH"; Linux 6.16). An older kernel answers the unknown code `EINVAL`, which the
/// notify thread takes for a device refusal, so the bump is sent only to a kernel that offered at least this.
const EPOCH_MINOR: u32 = 44;

/// What a connection negotiated at `FUSE_INIT` that serving it later depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
  /// The kernel honours `FUSE_EXPIRE_ONLY` on an entry invalidation.
  pub expire_only: bool,
  /// The kernel can resend the requests a daemon read and never answered (`FUSE_NOTIFY_RESEND`).
  pub kernel_resends: bool,
  /// The kernel takes `FUSE_NOTIFY_INC_EPOCH`: one message after which every name it caches is looked up again at its
  /// next use (A-158).
  pub kernel_bumps_epoch: bool,
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
      kernel_bumps_epoch: negotiated.kernel_minor >= EPOCH_MINOR,
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
    if self.kernel_bumps_epoch {
      bits |= EPOCH_BIT;
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
      kernel_bumps_epoch: bits & EPOCH_BIT != 0,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Golden vectors: every session encodes to its bytes and decodes back. The first four are the bytes every daemon
  /// before A-158 wrote, which must read here unchanged.
  #[test]
  fn every_session_round_trips_through_its_golden_bytes() {
    let vectors = [
      (false, false, false, [1, 0]),
      (true, false, false, [1, 1]),
      (false, true, false, [1, 2]),
      (true, true, false, [1, 3]),
      (false, false, true, [1, 4]),
      (true, false, true, [1, 5]),
      (false, true, true, [1, 6]),
      (true, true, true, [1, 7]),
    ];
    for (expire_only, kernel_resends, kernel_bumps_epoch, bytes) in vectors {
      let session = Session {
        expire_only,
        kernel_resends,
        kernel_bumps_epoch,
      };
      assert_eq!(session.to_bytes(), bytes);
      assert_eq!(Session::from_bytes(&bytes), Ok(session));
    }
  }

  /// A-158: a kernel that offered FUSE 7.44 or later is recorded as able to bump its epoch, whatever minor the
  /// connection agreed (slates agrees at most 7.31), and an older one is not. Do: negotiate `INIT` bodies offering
  /// minors 43, 44 and 45. Expect the agreed minor 31 each time, and the bump only for 44 and 45.
  #[test]
  fn a_kernel_offering_fuse_7_44_is_recorded_as_able_to_bump_its_epoch() {
    for (offered, bumps) in [(43, false), (44, true), (45, true)] {
      let mut body = [0u8; 16];
      body[..4].copy_from_slice(&7u32.to_le_bytes());
      body[4..8].copy_from_slice(&u32::to_le_bytes(offered));
      let negotiated =
        crate::init::negotiate(&body, slates_bridge_core::CacheCoherence::Invalidated).unwrap();
      assert_eq!(negotiated.minor, crate::abi::FUSE_KERNEL_MINOR_VERSION);
      assert_eq!(negotiated.kernel_minor, offered);
      assert_eq!(
        Session::of(&negotiated).kernel_bumps_epoch,
        bumps,
        "minor {offered}"
      );
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
      (&[1, 8], SessionRefusal::UndefinedBits { bits: 8 }),
    ];
    for (bytes, refusal) in cases {
      assert_eq!(Session::from_bytes(bytes), Err(refusal), "{bytes:?}");
    }
  }

  /// Hostile input: every single-bit flip of a valid encoding is refused by format or bits, or decodes to a
  /// defined session that re-encodes to itself; nothing panics and nothing is read past the bytes.
  #[test]
  fn every_bit_flip_of_a_session_is_refused_or_a_defined_session() {
    let valid = [1u8, 7];
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
