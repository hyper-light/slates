//! The bridge codec's closed refusal taxonomy (§4.6, §4.9's rule that a parser of external
//! bytes refuses before it allocates): a request the kernel sent that this crate cannot make
//! sense of. Never a panic.

use std::fmt;

/// A typed refusal from the FUSE codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuseError {
  /// The message is shorter than the fixed header the ABI requires.
  ShortHeader {
    /// The bytes present.
    have: usize,
    /// The bytes the header needs.
    need: usize,
  },
  /// The header's length field is smaller than the header itself, or larger than the buffer.
  BadLength {
    /// The length the header claims.
    claimed: u32,
    /// The bytes actually present.
    have: usize,
  },
  /// The opcode is not one this bridge serves.
  UnknownOpcode {
    /// The opcode value.
    opcode: u32,
  },
  /// The request body is shorter than the opcode's fixed part requires.
  ShortBody {
    /// The opcode.
    opcode: u32,
    /// The bytes present in the body.
    have: usize,
    /// The bytes the body needs.
    need: usize,
  },
  /// A name field carries no terminating NUL within the body.
  UnterminatedName,
  /// A reply buffer is smaller than the reply to be written.
  ReplyTooSmall {
    /// The bytes available.
    have: usize,
    /// The bytes the reply needs.
    need: usize,
  },
}

impl fmt::Display for FuseError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::ShortHeader { have, need } => {
        write!(f, "short FUSE header: {have} bytes, need {need}")
      }
      Self::BadLength { claimed, have } => {
        write!(
          f,
          "bad FUSE length: header claims {claimed}, buffer has {have}"
        )
      }
      Self::UnknownOpcode { opcode } => write!(f, "unknown FUSE opcode {opcode}"),
      Self::ShortBody { opcode, have, need } => write!(
        f,
        "short body for opcode {opcode}: {have} bytes, need {need}"
      ),
      Self::UnterminatedName => f.write_str("a name field is not NUL-terminated"),
      Self::ReplyTooSmall { have, need } => {
        write!(f, "reply buffer too small: {have} bytes, need {need}")
      }
    }
  }
}

impl std::error::Error for FuseError {}
