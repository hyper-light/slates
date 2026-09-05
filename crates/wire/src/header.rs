//! The 32-byte frame header: magic (4), major (2), minor (2), flags (4), class (2), kind (2),
//! length (4), checksum (4), request id (8); little-endian, hand-encoded byte by byte so host
//! layout never matters (vorpal's discipline; §4.9 "Framing").

use crate::error::WireError;
use crate::request::RequestId;

/// Format: the header's size in bytes.
pub const HEADER_LEN: usize = 32;
/// Format: the magic, `SLTS` in little-endian ASCII.
pub const MAGIC: u32 = 0x5354_4C53;
/// Format: the protocol major this build speaks; a mismatch is refused outright.
pub const MAJOR: u16 = 1;
/// Format: the protocol minor this build speaks; minors add kinds and fields, append-only.
pub const MINOR: u16 = 0;

/// Format: field offsets within the header.
const AT_MAGIC: usize = 0;
/// Format: major.
const AT_MAJOR: usize = 4;
/// Format: minor.
const AT_MINOR: usize = 6;
/// Format: flags.
const AT_FLAGS: usize = 8;
/// Format: class.
const AT_CLASS: usize = 12;
/// Format: kind.
const AT_KIND: usize = 14;
/// Format: body length.
const AT_LENGTH: usize = 16;
/// Format: checksum.
const AT_CHECKSUM: usize = 20;
/// Format: request id.
const AT_REQUEST: usize = 24;

/// The four message classes, each with its own credit pool and queue (§4.9 "Classes").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Class {
  /// Never shed.
  Control = 0,
  /// Leases, catalog, registers.
  Metadata = 1,
  /// Chunks and archive streams.
  Bulk = 2,
  /// Shed first (Format: class word 3; the words are the priority order).
  Telemetry = 3,
}

impl Class {
  /// Format: the four classes, in priority order.
  pub const ALL: [Class; 4] = [
    Class::Control,
    Class::Metadata,
    Class::Bulk,
    Class::Telemetry,
  ];

  /// The class for a header word.
  pub fn from_word(word: u16) -> Result<Class, WireError> {
    Self::ALL
      .iter()
      .copied()
      .find(|c| *c as u16 == word)
      .ok_or(WireError::UnknownClass { got: word })
  }

  /// The index in `ALL`.
  pub fn index(self) -> usize {
    usize::from(self as u16)
  }

  /// Whether bodies of this class carry a CRC32C (bulk carries the BLAKE3 identity instead).
  pub const fn checksummed(self) -> bool {
    !matches!(self, Class::Bulk)
  }
}

/// Header flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u32);

impl Flags {
  /// Format: the body is the last frame of its stream.
  pub const END_OF_STREAM: Flags = Flags(1);
  /// Format: the body is a refusal, not the requested kind's success body.
  pub const REFUSAL: Flags = Flags(1 << 1);

  /// Whether every bit of `other` is set.
  pub const fn contains(self, other: Flags) -> bool {
    self.0 & other.0 == other.0
  }

  /// The union.
  pub const fn with(self, other: Flags) -> Flags {
    Flags(self.0 | other.0)
  }
}

/// The header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
  /// The protocol minor of the sender.
  pub minor: u16,
  /// Flags.
  pub flags: Flags,
  /// The class.
  pub class: Class,
  /// The kind within the class.
  pub kind: u16,
  /// The body length in bytes.
  pub length: u32,
  /// The body checksum (CRC32C for checksummed classes; 0 for bulk).
  pub checksum: u32,
  /// The request id.
  pub request: RequestId,
}

impl Header {
  /// Writes the header.
  pub fn encode(&self) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    put(&mut out, AT_MAGIC, &MAGIC.to_le_bytes());
    put(&mut out, AT_MAJOR, &MAJOR.to_le_bytes());
    put(&mut out, AT_MINOR, &self.minor.to_le_bytes());
    put(&mut out, AT_FLAGS, &self.flags.0.to_le_bytes());
    put(&mut out, AT_CLASS, &(self.class as u16).to_le_bytes());
    put(&mut out, AT_KIND, &self.kind.to_le_bytes());
    put(&mut out, AT_LENGTH, &self.length.to_le_bytes());
    put(&mut out, AT_CHECKSUM, &self.checksum.to_le_bytes());
    put(&mut out, AT_REQUEST, &self.request.word().to_le_bytes());
    out
  }

  /// Reads a header, refusing a bad magic or another major before anything else is looked at.
  pub fn decode(bytes: &[u8]) -> Result<Header, WireError> {
    if bytes.len() < HEADER_LEN {
      return Err(WireError::Truncated {
        needed: HEADER_LEN - bytes.len(),
      });
    }
    let magic = u32::from_le_bytes(take(bytes, AT_MAGIC));
    if magic != MAGIC {
      return Err(WireError::BadMagic { got: magic });
    }
    let major = u16::from_le_bytes(take(bytes, AT_MAJOR));
    if major != MAJOR {
      return Err(WireError::UnsupportedMajor {
        got: major,
        supported: MAJOR,
      });
    }
    Ok(Header {
      minor: u16::from_le_bytes(take(bytes, AT_MINOR)),
      flags: Flags(u32::from_le_bytes(take(bytes, AT_FLAGS))),
      class: Class::from_word(u16::from_le_bytes(take(bytes, AT_CLASS)))?,
      kind: u16::from_le_bytes(take(bytes, AT_KIND)),
      length: u32::from_le_bytes(take(bytes, AT_LENGTH)),
      checksum: u32::from_le_bytes(take(bytes, AT_CHECKSUM)),
      request: RequestId::from_word(u64::from_le_bytes(take(bytes, AT_REQUEST))),
    })
  }
}

fn put(out: &mut [u8], at: usize, bytes: &[u8]) {
  out[at..at + bytes.len()].copy_from_slice(bytes);
}

fn take<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
  let mut word = [0u8; N];
  word.copy_from_slice(&bytes[at..at + N]);
  word
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> Header {
    Header {
      minor: 3,
      flags: Flags::END_OF_STREAM.with(Flags::REFUSAL),
      class: Class::Metadata,
      kind: 7,
      length: 4096,
      checksum: 0xDEAD_BEEF,
      request: RequestId {
        client: 42,
        sequence: 9,
      },
    }
  }

  #[test]
  fn the_header_round_trips_and_is_exactly_thirty_two_bytes_little_endian() {
    let h = sample();
    let bytes = h.encode();
    assert_eq!(bytes.len(), HEADER_LEN);
    assert_eq!(&bytes[0..4], b"SLTS");
    assert_eq!(&bytes[4..6], &[1, 0]);
    assert_eq!(&bytes[16..20], &4096u32.to_le_bytes());
    assert_eq!(Header::decode(&bytes).unwrap(), h);
  }

  #[test]
  fn bad_magic_and_another_major_are_refused_before_anything_else() {
    let mut bytes = sample().encode();
    bytes[0] ^= 1;
    assert!(matches!(
      Header::decode(&bytes),
      Err(WireError::BadMagic { .. })
    ));
    let mut bytes = sample().encode();
    bytes[4] = 2;
    bytes[12] = 200; // an unknown class too, which must not be reached
    assert!(matches!(
      Header::decode(&bytes),
      Err(WireError::UnsupportedMajor {
        got: 2,
        supported: 1
      })
    ));
    let mut bytes = sample().encode();
    bytes[12] = 200;
    assert!(matches!(
      Header::decode(&bytes),
      Err(WireError::UnknownClass { got: 200 })
    ));
    assert!(matches!(
      Header::decode(&bytes[..10]),
      Err(WireError::Truncated { needed: 22 })
    ));
  }

  #[test]
  fn flags_compose() {
    let f = Flags::END_OF_STREAM.with(Flags::REFUSAL);
    assert!(f.contains(Flags::REFUSAL));
    assert!(!Flags::END_OF_STREAM.contains(Flags::REFUSAL));
    assert!(Class::Control.checksummed() && !Class::Bulk.checksummed());
    assert_eq!(Class::Telemetry.index(), 3);
  }
}
