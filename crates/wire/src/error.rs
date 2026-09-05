//! The closed refusal taxonomy of the wire (§4.9, "Refusals"), plus the decoder's own refusals
//! for non-canonical bytes. Every variant names what was expected, so a peer's bug is diagnosable
//! from the refusal alone.

use std::fmt;

/// A typed refusal from the wire; never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
  /// The header's magic is not ours.
  BadMagic {
    /// The word found.
    got: u32,
  },
  /// The header names a major this build does not speak; there is no decode across majors.
  UnsupportedMajor {
    /// The major found.
    got: u16,
    /// The major this build speaks.
    supported: u16,
  },
  /// The body length exceeds the class's cap; nothing was allocated.
  FrameTooLarge {
    /// The length claimed.
    length: u32,
    /// The class's cap.
    cap: u32,
  },
  /// The checksum did not match the body.
  ChecksumMismatch {
    /// The checksum carried.
    expected: u32,
    /// The checksum computed.
    got: u32,
  },
  /// The class word is not one of the four classes.
  UnknownClass {
    /// The word found.
    got: u16,
  },
  /// The kind is not registered for its class.
  UnknownKind {
    /// The class.
    class: u16,
    /// The kind.
    kind: u16,
  },
  /// The body's schema hash is not the one this build has for that kind.
  SchemaMismatch {
    /// The hash this build expects.
    expected: u64,
    /// The hash carried.
    got: u64,
  },
  /// A send would exceed the stream's credit.
  CreditExceeded {
    /// Bytes the sender wanted.
    wanted: u64,
    /// Bytes of credit left.
    available: u64,
  },
  /// The input ended before the value did (a streaming reader treats this as "need more").
  Truncated {
    /// Bytes needed beyond the input.
    needed: usize,
  },
  /// A tag byte that must be 0 or 1 was something else.
  BadTag {
    /// The byte found.
    got: u8,
  },
  /// An enum discriminant with no variant.
  BadDiscriminant {
    /// The discriminant found.
    got: u32,
  },
  /// A string whose bytes are not UTF-8.
  BadUtf8,
  /// A float that is not canonical (a non-canonical NaN or a negative zero).
  NonCanonicalFloat,
  /// Bytes were left after the body decoded; a canonical body has none.
  TrailingBytes {
    /// How many.
    count: usize,
  },
}

impl fmt::Display for WireError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::BadMagic { got } => write!(f, "bad magic {got:#010x}"),
      Self::UnsupportedMajor { got, supported } => {
        write!(f, "unsupported major {got}; this build speaks {supported}")
      }
      Self::FrameTooLarge { length, cap } => {
        write!(f, "frame of {length} bytes exceeds the class cap of {cap}")
      }
      Self::ChecksumMismatch { expected, got } => {
        write!(f, "checksum {got:#010x} does not match {expected:#010x}")
      }
      Self::UnknownClass { got } => write!(f, "unknown class {got}"),
      Self::UnknownKind { class, kind } => write!(f, "unknown kind {kind} in class {class}"),
      Self::SchemaMismatch { expected, got } => {
        write!(f, "schema {got:#018x} is not {expected:#018x}")
      }
      Self::CreditExceeded { wanted, available } => {
        write!(f, "credit exceeded: wanted {wanted}, {available} available")
      }
      Self::Truncated { needed } => write!(f, "truncated: {needed} more bytes needed"),
      Self::BadTag { got } => write!(f, "tag byte {got} is not 0 or 1"),
      Self::BadDiscriminant { got } => write!(f, "discriminant {got} names no variant"),
      Self::BadUtf8 => f.write_str("string is not UTF-8"),
      Self::NonCanonicalFloat => f.write_str("float is not canonical"),
      Self::TrailingBytes { count } => write!(f, "{count} trailing bytes after the body"),
    }
  }
}

impl std::error::Error for WireError {}
