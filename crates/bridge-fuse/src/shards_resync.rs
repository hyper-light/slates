//! A Shards stream's resync channel (§4.6, the Shards form; A-316): how a stream outlives its daemon's death, by the
//! conditions Shards' coordinator set (2026-11-18). A stream is a byte conversation whose frames are read whole by
//! length (Shards' device reads with `read_exact`), so a stream a daemon died in the middle of is never served on: a
//! reply cut short cannot be told from payload. A successor instead hands the device a fresh stream over a second,
//! message-oriented channel made at the attach beside the stream, and the device resends its outstanding request on it.
//!
//! **The channel.** A connected pair of Unix sockets that carry descriptors: sequenced packets on Linux, each message
//! one packet; a stream on macOS (A-339), each message read whole by its fixed length, [`RESYNC_MESSAGE_BYTES`], a
//! `Resync`'s descriptor arriving with its bytes. macOS has no sequenced-packet Unix sockets, and its datagram pairs,
//! which keep messages whole, never tell a survivor that its peer has gone (measured 2026-11-18: a blocked read, a
//! `select` and a kqueue waited out their 5 s), which a device needs. On Windows (A-418), which carries no descriptor
//! with a message, a message-mode named pipe, each message one read whole, whose `Resync` names the fresh end's handle
//! instead of carrying it (below). One way, slates to the device. The daemon's end
//! is held by the anchor across daemon restarts, so it never closes with a daemon: the device reads its end-of-file
//! only when no resync will ever come (the attachment ended, the anchor itself went, or the stream gave up its
//! resync). A device that ends its share closes its end of the channel with its stream's.
//!
//! **The messages** ([`ResyncMessage`]), each [`RESYNC_MESSAGE_BYTES`] long, little-endian: a kind byte, the version
//! byte ([`RESYNC_VERSION`]), two reserved zero bytes, the attachment id (`u64`) and the attachment's resync token (16
//! bytes).
//! - [`ResyncKind::Attached`], queued at the attach before the device has its end, carries no descriptor: it tells
//!   the device the token. The same 28 bytes on every platform.
//! - [`ResyncKind::Resync`], sent by a successor daemon once its recovery kept the attachment, carries exactly one
//!   descriptor by `SCM_RIGHTS`: the device's end of a fresh stream for the same attachment.
//! - On Windows the `Resync` is a [`HandedResync`] (A-418), [`RESYNC_HANDLE_MESSAGE_BYTES`] long: the same 28 bytes,
//!   then the id of the process the successor duplicated the fresh end into (`u32`) and the end's handle value there
//!   (`u64`, never zero). A handle reaches another process only by its holder duplicating it into that process's table
//!   (`DuplicateHandle`, A-175's route), and only its value travels. The process is the stream's holder, not the
//!   confined VM: a VM in an AppContainer takes handles only through its spawner's grants, so the end goes where
//!   `--slates-fs-handle` sends the first one (Shards' coordinator, condition 4c), and how the device takes it from
//!   there is its handle path to specify. Neither form parses as the other: each is refused at the other's length.
//!
//! **Default deny (the coordinator's condition 4a).** A descriptor is not trusted for arriving: the device takes a
//! resync only if it parses, is the version it knows, carries one descriptor (on Windows: names a handle), and names
//! the attachment and the token its `Attached` named ([`ResyncMessage::resyncs`], [`HandedResync::resyncs`], the token
//! compared in time independent of where it differs); anything else ends the share as before, counted. The token is 128 bits from the daemon's secure random, minted per
//! attachment, never its NFS mount capability (a VMM given that could mount the volume), kept beside the daemon's held
//! end and never on disk, so holding the channel's other end alone proves nothing.
//!
//! **After a resync (condition 4b).** The device abandons the old stream, resends its outstanding request whole (its
//! unique unchanged) as the new stream's first frame, and asks again for each DAX mapping's descriptor. The successor
//! serves the new stream as the same attachment, with no `INIT`, and answers a resent request the dead daemon applied
//! and published but never answered from the reply it published with the effect (A-61's rule), never by applying it
//! twice. The successor keeps its own copy of the fresh end until the device's first bytes on it, or until the device's
//! end of the channel closes (A-339: macOS flushes a socket whose every copy rides a message), so a device that takes a
//! resync and then ends its share closes both ends, the channel's too.
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
/// Format: the bytes of a Windows `Resync` (A-418): the [`RESYNC_MESSAGE_BYTES`] of the message, then the holder's
/// process id (`u32`) and the fresh end's handle value there (`u64`), little-endian, so the value sits 8-aligned.
pub const RESYNC_HANDLE_MESSAGE_BYTES: usize =
  RESYNC_MESSAGE_BYTES + size_of::<u32>() + size_of::<u64>();

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
  /// A Windows `Resync` whose handle value is zero, which names no handle (A-175's convention, A-418).
  NoHandle,
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

/// A Windows `Resync` (A-418): the resync, and where the fresh stream's end was put, since no descriptor rides a
/// message there. A successor duplicates the end into the stream's holder, `holder`, where its value is `handle`, and
/// sends this over the held channel; the device takes the end from that process by that value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandedResync {
  /// The resync: a [`ResyncKind::Resync`] naming the attachment and its token.
  pub message: ResyncMessage,
  /// The id of the process the fresh end was duplicated into.
  pub holder: u32,
  /// The end's handle value in that process; never zero.
  pub handle: u64,
}

impl HandedResync {
  /// The message's bytes: the resync's own, then the holder's process id and the handle's value.
  pub fn to_bytes(&self) -> [u8; RESYNC_HANDLE_MESSAGE_BYTES] {
    let mut bytes = [0u8; RESYNC_HANDLE_MESSAGE_BYTES];
    let head = self.message.to_bytes();
    let fields: [&[u8]; 3] = [
      &head,
      &self.holder.to_le_bytes(),
      &self.handle.to_le_bytes(),
    ];
    for (slot, byte) in bytes.iter_mut().zip(fields.into_iter().flatten()) {
      *slot = *byte;
    }
    bytes
  }

  /// The Windows `Resync` `bytes` hold, or why they hold none: checked by length first, then the resync's own bytes as
  /// [`ResyncMessage::parse`] checks them, which must be a [`ResyncKind::Resync`], then the handle, which must name one.
  pub fn parse(bytes: &[u8]) -> Result<HandedResync, ResyncRefusal> {
    let short = ResyncRefusal::Length { have: bytes.len() };
    if bytes.len() != RESYNC_HANDLE_MESSAGE_BYTES {
      return Err(short);
    }
    let (head, rest) = bytes
      .split_first_chunk::<RESYNC_MESSAGE_BYTES>()
      .ok_or(short)?;
    let message = ResyncMessage::parse(head)?;
    if message.kind != ResyncKind::Resync {
      return Err(ResyncRefusal::Kind {
        kind: KIND_ATTACHED,
      });
    }
    let (holder, handle) = rest
      .split_first_chunk::<{ size_of::<u32>() }>()
      .ok_or(short)?;
    let handle = u64::from_le_bytes(<[u8; size_of::<u64>()]>::try_from(handle).map_err(|_| short)?);
    if handle == 0 {
      return Err(ResyncRefusal::NoHandle);
    }
    Ok(HandedResync {
      message,
      holder: u32::from_le_bytes(*holder),
      handle,
    })
  }

  /// Whether this is a resync the device may take, given the `attached` message its channel brought first: the
  /// resync's own check ([`ResyncMessage::resyncs`]) and a handle that names one. Which process the device takes the
  /// end from, and how, is the device's handle path (Shards' coordinator, condition 4c).
  pub fn resyncs(&self, attached: &ResyncMessage) -> bool {
    self.handle != 0 && self.message.resyncs(attached)
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

  /// The process and the handle value the tests' Windows resyncs name.
  const HOLDER: u32 = 0x1122_3344;
  const HANDLE: u64 = 0x01A4;

  fn handed() -> HandedResync {
    HandedResync {
      message: message(ResyncKind::Resync),
      holder: HOLDER,
      handle: HANDLE,
    }
  }

  /// A-418, golden vector. Do: encode a Windows `Resync` naming a holder and a handle, and parse it back. Expect the
  /// resync's 28 bytes as the Unix one has them, then the holder's id and the handle's value little-endian, 40 bytes in
  /// all, and the message back unchanged.
  #[test]
  fn a_windows_resync_encodes_to_its_golden_bytes_and_back() {
    let mut golden = vec![KIND_RESYNC, 1, 0, 0, 8, 7, 6, 5, 4, 3, 2, 1];
    golden.extend_from_slice(&TOKEN);
    golden.extend_from_slice(&[0x44, 0x33, 0x22, 0x11]);
    golden.extend_from_slice(&[0xA4, 0x01, 0, 0, 0, 0, 0, 0]);
    assert_eq!(golden.len(), RESYNC_HANDLE_MESSAGE_BYTES);
    assert_eq!(handed().to_bytes().to_vec(), golden);
    assert_eq!(HandedResync::parse(&golden), Ok(handed()));
  }

  /// A-418, condition 4a. Do: judge a Windows resync against its attach, and forgeries: another token (each byte
  /// flipped in turn), another attachment, an `Attached` in the Windows form, and a handle of zero, parsed and built by
  /// hand. Expect only the true resync taken: the attach in the Windows form refused by its kind, and the zero handle
  /// refused `NoHandle` and never taken.
  #[test]
  fn only_a_windows_resync_naming_a_handle_and_its_attachments_token_is_taken() {
    let attached = message(ResyncKind::Attached);
    assert!(handed().resyncs(&attached));
    for at in 0..TOKEN_BYTES {
      let mut forged = handed();
      forged.message.token[at] ^= 1;
      assert!(!forged.resyncs(&attached), "token byte {at} differs");
    }
    let mut elsewhere = handed();
    elsewhere.message.attachment ^= 1;
    assert!(!elsewhere.resyncs(&attached), "another attachment");
    let attach_form = HandedResync {
      message: attached,
      ..handed()
    };
    assert_eq!(
      HandedResync::parse(&attach_form.to_bytes()),
      Err(ResyncRefusal::Kind {
        kind: KIND_ATTACHED
      })
    );
    assert!(!attach_form.resyncs(&attached), "an attach is no resync");
    let nothing = HandedResync {
      handle: 0,
      ..handed()
    };
    assert_eq!(
      HandedResync::parse(&nothing.to_bytes()),
      Err(ResyncRefusal::NoHandle)
    );
    assert!(!nothing.resyncs(&attached), "a handle of zero names none");
  }

  /// A-418, hostile input. Do: parse as a Windows resync every length up to twice its own but its own, a Unix resync's
  /// 28 bytes, another version, a nonzero reserved byte and every single-bit flip of a valid one; and a Windows resync's
  /// 40 bytes as a Unix message. Expect each wrong length refused `Length` with the length it had, so neither form passes
  /// for the other; the version and the reserved byte refused by name; and every flip parsed or refused, never a panic.
  #[test]
  fn a_windows_resync_of_the_wrong_shape_is_refused_by_its_reason() {
    let valid = handed().to_bytes();
    for len in
      (0..=2 * RESYNC_HANDLE_MESSAGE_BYTES).filter(|len| *len != RESYNC_HANDLE_MESSAGE_BYTES)
    {
      let bytes: Vec<u8> = valid.iter().copied().cycle().take(len).collect();
      assert_eq!(
        HandedResync::parse(&bytes),
        Err(ResyncRefusal::Length { have: len })
      );
    }
    assert_eq!(
      HandedResync::parse(&message(ResyncKind::Resync).to_bytes()),
      Err(ResyncRefusal::Length {
        have: RESYNC_MESSAGE_BYTES
      })
    );
    assert_eq!(
      ResyncMessage::parse(&valid),
      Err(ResyncRefusal::Length {
        have: RESYNC_HANDLE_MESSAGE_BYTES
      })
    );
    let mut later = valid;
    later[1] = 2;
    assert_eq!(
      HandedResync::parse(&later),
      Err(ResyncRefusal::Version { version: 2 })
    );
    for at in AT_RESERVED..AT_ATTACHMENT {
      let mut reserved = valid;
      reserved[at] = 1;
      assert_eq!(HandedResync::parse(&reserved), Err(ResyncRefusal::Reserved));
    }
    for bit in 0..RESYNC_HANDLE_MESSAGE_BYTES * 8 {
      let mut flipped = valid;
      flipped[bit / 8] ^= 1 << (bit % 8);
      let _ = HandedResync::parse(&flipped);
    }
  }
}
