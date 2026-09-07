//! The fleet claims-plane transport (§4.10a; design: `docs/wip/fleet-transport.md`, a draft awaiting
//! ratification). slates nodes speak two planes over one UDP substrate — a stateless header-encrypted
//! **control plane** and an owned **QUIC session plane** — never MCP (which is the agent-facing tool
//! edge). This is the daemon↔daemon mesh, where Meta-scale traffic lives; the laptop-degenerate is a
//! local/loopback delivery, one code path (R8).
//!
//! **What this crate holds so far (Phase 8 slice 1): the control-datagram wire codec.** A control
//! datagram is a fixed-layout little-endian cleartext prologue (the key-finding minimum — version,
//! sender, key epoch, sealed length) wrapping a sealed region that carries the envelope (a fixed
//! header — kind, class, flags, epoch, hybrid-logical clock, request id) and a canonical body. The
//! AEAD seal (AES-256-GCM under host-identity keys), the `rt` UDP driver, the owned QUIC dialect over
//! `rustls::quic` (TLS 1.3, slates D-15, not hecate's Noise), and the register/placement wiring are
//! the later slices in the design's phasing — **owed**. Here the sealed region is the plaintext
//! envelope + body; the crypto slice wraps exactly these bytes.
//!
//! This is a parser of external bytes, so every length is checked against the bytes that remain before
//! it is read (a wild `sealed_len` never allocates), `flags` must be zero, and every malformation is a
//! typed [`FrameError`], never a panic — the same discipline as `slates-merge`'s ops-document decode
//! and `slates-bridge-fuse`'s ABI codec. The codec is pure and tested on every host.

/// The protocol version this build speaks (the floor; negotiation to higher versions is owed with the
/// session plane).
pub const PROTOCOL_VERSION: u8 = 1;

/// A control-plane datagram (§4.10a): who sent it and under which key epoch (the cleartext prologue),
/// and the envelope and body it carries (the sealed region — plaintext until the crypto slice).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlDatagram {
  /// The sending node's id (the prologue's key hint).
  pub sender: u64,
  /// Which of the sender's key epochs sealed this datagram.
  pub key_epoch: u32,
  /// The envelope header (authenticated once the seal lands).
  pub envelope: Envelope,
  /// The canonical body (a `slates-wire` value; opaque to the transport).
  pub body: Vec<u8>,
}

/// The envelope header (§4.10a): the fixed fields every message carries, authenticated by the seal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Envelope {
  /// The message kind (a closed enum is owed; carried as its discriminant here).
  pub kind: u8,
  /// The delivery class (§4.8 durability/ordering).
  pub class: u8,
  /// Reserved flags; every bit must be zero (a set bit is a typed refusal).
  pub flags: u8,
  /// The sender's region-membership epoch (fencing, §4.8/D-16).
  pub epoch: u64,
  /// The hybrid-logical clock stamp (the replay window and liveness only; never safety).
  pub hlc: u64,
  /// Pairs a reply with its request (RIFL, D-15).
  pub request_id: u64,
}

/// A refusal decoding a control datagram (§4.10a): the closed set of ways the bytes can be malformed.
/// Every one is a corrupt or truncated datagram, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
  /// The bytes ended before a field could be read.
  Truncated,
  /// The version was not one this build decodes.
  BadVersion,
  /// A reserved flag bit was set.
  FlagsSet,
  /// Bytes remained after the datagram.
  TrailingBytes,
}

impl std::fmt::Display for FrameError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let reason = match self {
      FrameError::Truncated => "the control datagram ended early",
      FrameError::BadVersion => "the control datagram version is unknown",
      FrameError::FlagsSet => "a reserved flag bit was set",
      FrameError::TrailingBytes => "the control datagram has trailing bytes",
    };
    f.write_str(reason)
  }
}

impl std::error::Error for FrameError {}

/// The sealed region's fixed header size: the [`Envelope`] fields laid out little-endian.
const ENVELOPE_BYTES: usize = size_of::<u8>() // kind
  + size_of::<u8>() // class
  + size_of::<u8>() // flags
  + size_of::<u64>() // epoch
  + size_of::<u64>() // hlc
  + size_of::<u64>(); // request_id

impl ControlDatagram {
  /// The canonical byte encoding: the cleartext prologue (version, sender, key epoch, sealed length)
  /// then the sealed region (the envelope header then the body). Little-endian throughout; the crypto
  /// slice will encrypt the sealed region in place, its length unchanged in the prologue.
  pub fn encode(&self) -> Vec<u8> {
    let sealed_len = ENVELOPE_BYTES + self.body.len();
    let mut out = Vec::with_capacity(
      size_of::<u8>() + size_of::<u64>() + size_of::<u32>() + size_of::<u32>() + sealed_len,
    );
    out.push(PROTOCOL_VERSION);
    out.extend_from_slice(&self.sender.to_le_bytes());
    out.extend_from_slice(&self.key_epoch.to_le_bytes());
    // A control datagram is MTU-bounded, so the sealed length fits a u32; oversize is refused by the
    // per-path-MTU send builder (owed), so a saturating cast here can only be reached by a bug.
    out.extend_from_slice(&u32::try_from(sealed_len).unwrap_or(u32::MAX).to_le_bytes());
    // The sealed region (plaintext until the crypto slice): the envelope header, then the body.
    out.push(self.envelope.kind);
    out.push(self.envelope.class);
    out.push(self.envelope.flags);
    out.extend_from_slice(&self.envelope.epoch.to_le_bytes());
    out.extend_from_slice(&self.envelope.hlc.to_le_bytes());
    out.extend_from_slice(&self.envelope.request_id.to_le_bytes());
    out.extend_from_slice(&self.body);
    out
  }

  /// Decodes a control datagram from [`ControlDatagram::encode`]'s bytes. Bounds-checked against the
  /// bytes that remain before each read (a wild `sealed_len` never allocates), `flags` must be zero,
  /// and any malformation is a typed [`FrameError`].
  pub fn decode(bytes: &[u8]) -> Result<ControlDatagram, FrameError> {
    let mut reader = Reader::new(bytes);
    if reader.u8()? != PROTOCOL_VERSION {
      return Err(FrameError::BadVersion);
    }
    let sender = reader.u64()?;
    let key_epoch = reader.u32()?;
    let sealed_len = reader.u32()? as usize;
    // The sealed region must at least hold the envelope header, and must fit the bytes that remain.
    if sealed_len < ENVELOPE_BYTES || sealed_len > reader.remaining() {
      return Err(FrameError::Truncated);
    }
    let kind = reader.u8()?;
    let class = reader.u8()?;
    let flags = reader.u8()?;
    if flags != 0 {
      return Err(FrameError::FlagsSet);
    }
    let epoch = reader.u64()?;
    let hlc = reader.u64()?;
    let request_id = reader.u64()?;
    let body = reader.bytes(sealed_len - ENVELOPE_BYTES)?.to_vec();
    if !reader.is_empty() {
      return Err(FrameError::TrailingBytes);
    }
    Ok(ControlDatagram {
      sender,
      key_epoch,
      envelope: Envelope {
        kind,
        class,
        flags,
        epoch,
        hlc,
        request_id,
      },
      body,
    })
  }
}

/// A cursor over the datagram bytes: every read is bounds-checked against what remains, so a torn
/// datagram yields [`FrameError::Truncated`] rather than an out-of-bounds panic.
struct Reader<'a> {
  bytes: &'a [u8],
  at: usize,
}

impl<'a> Reader<'a> {
  fn new(bytes: &'a [u8]) -> Reader<'a> {
    Reader { bytes, at: 0 }
  }

  fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.at)
  }

  fn is_empty(&self) -> bool {
    self.remaining() == 0
  }

  fn bytes(&mut self, len: usize) -> Result<&'a [u8], FrameError> {
    let end = self.at.checked_add(len).ok_or(FrameError::Truncated)?;
    let slice = self.bytes.get(self.at..end).ok_or(FrameError::Truncated)?;
    self.at = end;
    Ok(slice)
  }

  fn u8(&mut self) -> Result<u8, FrameError> {
    Ok(self.bytes(size_of::<u8>())?[0])
  }

  fn u32(&mut self) -> Result<u32, FrameError> {
    let b = self.bytes(size_of::<u32>())?;
    let mut word = [0u8; size_of::<u32>()];
    word.copy_from_slice(b);
    Ok(u32::from_le_bytes(word))
  }

  fn u64(&mut self) -> Result<u64, FrameError> {
    let b = self.bytes(size_of::<u64>())?;
    let mut word = [0u8; size_of::<u64>()];
    word.copy_from_slice(b);
    Ok(u64::from_le_bytes(word))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> ControlDatagram {
    ControlDatagram {
      sender: 0x0102_0304_0506_0708,
      key_epoch: 7,
      envelope: Envelope {
        kind: 3,
        class: 1,
        flags: 0,
        epoch: 42,
        hlc: 0xDEAD_BEEF,
        request_id: 99,
      },
      body: b"a canonical body".to_vec(),
    }
  }

  /// A datagram round-trips through encode/decode exactly, body included.
  #[test]
  fn a_datagram_round_trips() {
    let datagram = sample();
    assert_eq!(ControlDatagram::decode(&datagram.encode()), Ok(datagram));
  }

  /// An empty body round-trips (the sealed region is just the envelope header).
  #[test]
  fn an_empty_body_round_trips() {
    let mut datagram = sample();
    datagram.body.clear();
    assert_eq!(ControlDatagram::decode(&datagram.encode()), Ok(datagram));
  }

  /// Hostile inputs decode to a typed refusal, never a panic (§4.10a — a torn or corrupt datagram).
  #[test]
  fn hostile_datagrams_refuse_by_type() {
    let good = sample().encode();

    // Empty and short inputs cannot hold the prologue.
    assert_eq!(ControlDatagram::decode(&[]), Err(FrameError::Truncated));
    assert_eq!(
      ControlDatagram::decode(&good[..4]),
      Err(FrameError::Truncated)
    );

    // A wrong version is named.
    let mut bad_version = good.clone();
    bad_version[0] = PROTOCOL_VERSION.wrapping_add(1);
    assert_eq!(
      ControlDatagram::decode(&bad_version),
      Err(FrameError::BadVersion)
    );

    // A truncated tail (drop the last body bytes) is truncated, not a panic.
    assert_eq!(
      ControlDatagram::decode(&good[..good.len() - 4]),
      Err(FrameError::Truncated)
    );

    // Trailing bytes past the datagram are rejected.
    let mut trailing = good.clone();
    trailing.push(0);
    assert_eq!(
      ControlDatagram::decode(&trailing),
      Err(FrameError::TrailingBytes)
    );

    // A wild sealed_len (u32::MAX at the prologue offset) cannot fit the remaining bytes.
    let mut wild = good.clone();
    let sealed_len_at = size_of::<u8>() + size_of::<u64>() + size_of::<u32>();
    wild[sealed_len_at..sealed_len_at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(ControlDatagram::decode(&wild), Err(FrameError::Truncated));

    // A set flag bit is refused. The flags byte is the third byte of the sealed region, which starts
    // right after the prologue (version + sender + key_epoch + sealed_len).
    let mut flagged = good.clone();
    let flags_at = sealed_len_at + size_of::<u32>() + size_of::<u8>() + size_of::<u8>();
    flagged[flags_at] = 1;
    assert_eq!(ControlDatagram::decode(&flagged), Err(FrameError::FlagsSet));
  }
}
