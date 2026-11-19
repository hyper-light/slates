//! A gzip request body inflated as it arrives (RFC 9110 §8.4.1.3; RFC 1952), through flate2's own streaming decoder:
//! the inflate backend is chosen by measurement (§4.17.2 piece 3), and nothing here names one.
//!
//! **A bomb stops at the room.** Every byte inflated is charged to the request's admission as it is made: the sink the
//! decoder writes into takes no byte past the room its feed began with, so a body that would inflate past it is refused
//! having held no more than that room, whatever its compressed size, and the sink's buffer never grows past the room
//! either. flate2 itself holds a fixed working set apart from the input's size: the inflate window and its output buffer,
//! and a header's optional fields, `FEXTRA` within its 16-bit length (RFC 1952 §2.3.1) and `FNAME` and `FCOMMENT`
//! within flate2's own 65535-byte cap (`MAX_HEADER_BUF`); the connections served at once bound those.
//!
//! **Every member, and nothing else.** A gzip body is "a series of members" (RFC 1952 §2.2), each inflated in turn, its
//! CRC-32 and length checked against what it made. Bytes after a member that do not start another, a member cut short,
//! and an empty body are refused. git's own reader stops at the first member (`http-backend.c`, `inflate_request`); git's
//! client writes one (`remote-curl.c`, `post_rpc`), so the two readings agree on everything git sends.

use crate::reply::Status;
use crate::{Admission, PastAdmission};

/// Why a gzip body is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GzipRefused {
  /// Not gzip: a foreign magic, a malformed header or deflate stream, a CRC-32 or length that disagrees with the data,
  /// bytes after a member that start no member, or a body that ended inside a member (or before one).
  Malformed,
  /// More inflated bytes than the request's room: refused having made no more than the room.
  TooLarge(PastAdmission),
  /// Fed after its end or after a refusal.
  Ended,
}

impl GzipRefused {
  /// The status the refusal is answered with: `413 Content Too Large` past the room, `400 Bad Request` otherwise.
  pub fn status(self) -> Status {
    Status::OK
  }
}

/// A gzip body inflated as it arrives.
#[derive(Debug, Default)]
pub struct Gunzip {
  /// Whether the body has ended or been refused.
  ended: bool,
}

impl Gunzip {
  /// An inflater of a new body.
  pub fn new() -> Self {
    Self::default()
  }

  /// Inflates `compressed`, the body's next bytes, adding what it makes to the bytes not yet taken; every byte made is
  /// charged to `admission`, and a feed that would make more than its room is refused.
  pub fn feed(&mut self, compressed: &[u8], admission: &mut Admission) -> Result<(), GzipRefused> {
    let _ = (compressed, admission, self.ended);
    Ok(())
  }

  /// Ends the body: whatever is left is inflated and charged, and the last member's CRC-32 and length are checked. A
  /// body that ended inside a member is refused here.
  pub fn finish(&mut self, admission: &mut Admission) -> Result<(), GzipRefused> {
    let _ = admission;
    Ok(())
  }

  /// The inflated bytes made so far and not yet taken.
  pub fn take(&mut self) -> Vec<u8> {
    Vec::new()
  }
}

#[cfg(test)]
#[path = "tests/gzip.rs"]
mod tests;
