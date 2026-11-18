//! A Shards stream's resync channel (§4.6, the Shards form; A-316): how a stream outlives its daemon's death, by the
//! conditions Shards' coordinator set (2026-11-18). A stream is a byte conversation whose frames are read whole by
//! length (Shards' device reads with `read_exact`), so a stream a daemon died in the middle of is never served on: a
//! reply cut short cannot be told from payload. A successor instead hands the device a fresh stream over a second,
//! message-oriented channel made at the attach beside the stream, and the device resends its outstanding request on it.
//!
//! **The channel.** A connected pair of Unix sockets that keep message boundaries and carry descriptors: sequenced
//! packets on Linux, datagrams on macOS (which has no sequenced-packet Unix sockets; a connected Unix datagram pair is
//! reliable and ordered there). One way, slates to the device. The daemon's end is held by the anchor across daemon
//! restarts, so it never closes with a daemon: the device reads its end-of-file only when no resync will ever come
//! (the attachment ended, the anchor itself went, or the stream gave up its resync).
//!
//! **The messages** ([`ResyncMessage`]), each one packet of [`RESYNC_MESSAGE_BYTES`], little-endian: a kind byte, the
//! version byte ([`RESYNC_VERSION`]), two reserved zero bytes, the attachment id (`u64`) and the attachment's resync
//! token (16 bytes).
//! - [`ResyncKind::Attached`], queued at the attach before the device has its end, carries no descriptor: it tells
//!   the device the token.
//! - [`ResyncKind::Resync`], sent by a successor daemon once its recovery kept the attachment, carries exactly one
//!   descriptor by `SCM_RIGHTS`: the device's end of a fresh stream for the same attachment.
//!
//! **Default deny (the coordinator's condition 4a).** A descriptor is not trusted for arriving: the device takes a
//! resync only if it parses, is the version it knows, carries one descriptor, and names the attachment and the token
//! its `Attached` named ([`ResyncMessage::resyncs`], the token compared in time independent of where it differs);
//! anything else ends the share as before, counted. The token is 128 bits from the daemon's secure random, minted per
//! attachment, never its NFS mount capability (a VMM given that could mount the volume), kept beside the daemon's held
//! end and never on disk, so holding the channel's other end alone proves nothing.
//!
//! **After a resync (condition 4b).** The device abandons the old stream, resends its outstanding request whole (its
//! unique unchanged) as the new stream's first frame, and asks again for each DAX mapping's descriptor. The successor
//! serves the new stream as the same attachment, with no `INIT`, and answers a resent request the dead daemon applied
//! and published but never answered from the reply it published with the effect (A-61's rule), never by applying it
//! twice.
//!
//! These are functions over bytes, holding no socket, so they run on every platform; the device's half lives in
//! Shards, and this module's parse is its reference, which slates' own tests run as the device.

/// Format: the bytes of every resync message: kind (1), version (1), reserved (2), attachment id (8), token (16).
pub const RESYNC_MESSAGE_BYTES: usize = 28;
/// Format: the version of the messages this module reads and writes.
pub const RESYNC_VERSION: u8 = 1;
/// Format: the bytes of an attachment's resync token: 128 bits, as the mount capability's are.
pub const TOKEN_BYTES: usize = 16;
/// Format: where the reserved bytes sit, and how many.
const AT_RESERVED: usize = 2;
/// Format: see [`AT_RESERVED`].
const RESERVED_BYTES: usize = 2;
/// Format: where the attachment id sits.
const AT_ATTACHMENT: usize = AT_RESERVED + RESERVED_BYTES;
/// Format: the kind byte of an [`ResyncKind::Attached`] message.
const KIND_ATTACHED: u8 = 1;
/// Format: the kind byte of a [`ResyncKind::Resync`] message.
const KIND_RESYNC: u8 = 2;

/// What a resync message says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncKind {
  /// The attachment's token, told once at the attach.
  Attached,
  /// A fresh stream for the attachment, its end riding the message.
  Resync,
}

/// One message of the resync channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResyncMessage {
  /// What it says.
  pub kind: ResyncKind,
  /// The attachment it is for.
  pub attachment: u64,
  /// The attachment's resync token.
  pub token: [u8; TOKEN_BYTES],
}

/// Why bytes are no resync message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResyncRefusal {
  /// Not [`RESYNC_MESSAGE_BYTES`] long.
  Length {
    /// The bytes present.
    have: usize,
  },
  /// A kind this version does not define.
  Kind {
    /// The kind byte present.
    kind: u8,
  },
  /// Another version.
  Version {
    /// The version byte present.
    version: u8,
  },
  /// A reserved byte that is not zero.
  Reserved,
}

impl ResyncMessage {
  /// The message's bytes.
  pub fn to_bytes(&self) -> [u8; RESYNC_MESSAGE_BYTES] {
    let mut bytes = [0u8; RESYNC_MESSAGE_BYTES];
    let kind = match self.kind {
      ResyncKind::Attached => KIND_ATTACHED,
      ResyncKind::Resync => KIND_RESYNC,
    };
    let fields: [&[u8]; 5] = [
      &[kind],
      &[RESYNC_VERSION],
      &[0; RESERVED_BYTES],
      &self.attachment.to_le_bytes(),
      &self.token,
    ];
    for (slot, byte) in bytes.iter_mut().zip(fields.into_iter().flatten()) {
      *slot = *byte;
    }
    bytes
  }

  /// The message `bytes` hold, or why they hold none: checked by length first, then version, kind and the reserved
  /// bytes, before any field is taken.
  pub fn parse(bytes: &[u8]) -> Result<ResyncMessage, ResyncRefusal> {
    let short = ResyncRefusal::Length { have: bytes.len() };
    if bytes.len() != RESYNC_MESSAGE_BYTES {
      return Err(short);
    }
    let (&[kind, version, low, high], rest) =
      bytes.split_first_chunk::<AT_ATTACHMENT>().ok_or(short)?;
    if version != RESYNC_VERSION {
      return Err(ResyncRefusal::Version { version });
    }
    let kind = match kind {
      KIND_ATTACHED => ResyncKind::Attached,
      KIND_RESYNC => ResyncKind::Resync,
      kind => return Err(ResyncRefusal::Kind { kind }),
    };
    if low != 0 || high != 0 {
      return Err(ResyncRefusal::Reserved);
    }
    let (attachment, token) = rest
      .split_first_chunk::<{ size_of::<u64>() }>()
      .ok_or(short)?;
    Ok(ResyncMessage {
      kind,
      attachment: u64::from_le_bytes(*attachment),
      token: <[u8; TOKEN_BYTES]>::try_from(token).map_err(|_| short)?,
    })
  }

  /// Whether this message is a resync the device may take, given the `attached` message its channel brought first:
  /// a [`ResyncKind::Resync`] for the same attachment with the same token, the token compared over all its bytes
  /// whatever they hold, so how long a comparison takes tells nothing of where a forgery differs. The descriptor count
  /// is the caller's to check (exactly one).
  pub fn resyncs(&self, attached: &ResyncMessage) -> bool {
    let differing = self
      .token
      .iter()
      .zip(attached.token.iter())
      .fold(0u8, |differing, (mine, theirs)| differing | (mine ^ theirs));
    self.kind == ResyncKind::Resync
      && attached.kind == ResyncKind::Attached
      && self.attachment == attached.attachment
      && differing == 0
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The attachment and token the tests' messages name.
  const ATTACHMENT: u64 = 0x0102_0304_0506_0708;
  const TOKEN: [u8; TOKEN_BYTES] = [
    0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
  ];

  fn message(kind: ResyncKind) -> ResyncMessage {
    ResyncMessage {
      kind,
      attachment: ATTACHMENT,
      token: TOKEN,
    }
  }

  /// A-316, golden vector. Do: encode an `Attached` and a `Resync` for one attachment and token, and parse them back.
  /// Expect the bytes Shards' device reads, field by field (kind, version 1, two zero bytes, the attachment
  /// little-endian, the token), and each message back unchanged.
  #[test]
  fn each_message_encodes_to_its_golden_bytes_and_back() {
    let mut golden = vec![KIND_ATTACHED, 1, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1];
    golden.extend_from_slice(&TOKEN);
    assert_eq!(message(ResyncKind::Attached).to_bytes().to_vec(), golden);
    golden[0] = KIND_RESYNC;
    assert_eq!(message(ResyncKind::Resync).to_bytes().to_vec(), golden);
    for kind in [ResyncKind::Attached, ResyncKind::Resync] {
      assert_eq!(
        ResyncMessage::parse(&message(kind).to_bytes()),
        Ok(message(kind))
      );
    }
  }

  /// A-316, condition 4a. Do: judge a resync against its attach, and forgeries: another token (each byte flipped in
  /// turn), another attachment, an `Attached` passed off as a resync, and a resync judged against a resync. Expect only
  /// the true resync taken.
  #[test]
  fn only_a_resync_naming_its_attachments_token_is_taken() {
    let attached = message(ResyncKind::Attached);
    assert!(message(ResyncKind::Resync).resyncs(&attached));
    for at in 0..TOKEN_BYTES {
      let mut forged = message(ResyncKind::Resync);
      forged.token[at] ^= 1;
      assert!(!forged.resyncs(&attached), "token byte {at} differs");
    }
    let mut elsewhere = message(ResyncKind::Resync);
    elsewhere.attachment ^= 1;
    assert!(!elsewhere.resyncs(&attached), "another attachment");
    assert!(
      !message(ResyncKind::Attached).resyncs(&attached),
      "an attach is no resync"
    );
    assert!(
      !message(ResyncKind::Resync).resyncs(&message(ResyncKind::Resync)),
      "a resync is judged only against the attach"
    );
  }

  /// A-316, hostile input. Do: parse no bytes, one byte short and one too many, every length up to twice the message,
  /// an unknown kind, another version, a nonzero reserved byte, and every single-bit flip of a valid message. Expect
  /// each wrong length refused `Length` with the length it had, the kind, version and reserved byte refused by name,
  /// and every flip either parsed or refused, never a panic.
  #[test]
  fn a_message_of_the_wrong_shape_is_refused_by_its_reason() {
    let valid = message(ResyncKind::Resync).to_bytes();
    for len in (0..=2 * RESYNC_MESSAGE_BYTES).filter(|len| *len != RESYNC_MESSAGE_BYTES) {
      let bytes: Vec<u8> = valid.iter().copied().cycle().take(len).collect();
      assert_eq!(
        ResyncMessage::parse(&bytes),
        Err(ResyncRefusal::Length { have: len })
      );
    }
    let mut unknown = valid;
    unknown[0] = 3;
    assert_eq!(
      ResyncMessage::parse(&unknown),
      Err(ResyncRefusal::Kind { kind: 3 })
    );
    let mut later = valid;
    // The version byte, after the kind.
    later[1] = 2;
    assert_eq!(
      ResyncMessage::parse(&later),
      Err(ResyncRefusal::Version { version: 2 })
    );
    for at in AT_RESERVED..AT_ATTACHMENT {
      let mut reserved = valid;
      reserved[at] = 1;
      assert_eq!(
        ResyncMessage::parse(&reserved),
        Err(ResyncRefusal::Reserved)
      );
    }
    for bit in 0..RESYNC_MESSAGE_BYTES * 8 {
      let mut flipped = valid;
      flipped[bit / 8] ^= 1 << (bit % 8);
      let _ = ResyncMessage::parse(&flipped);
    }
  }
}
