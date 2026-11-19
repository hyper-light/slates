//! A gzip request body inflated as it arrives (RFC 9110 §8.4.1.3; RFC 1952), through flate2's own streaming decoder
//! over the workspace's one deflate engine, zlib-rs, which main chose by measurement (A-607); nothing here names a
//! backend.
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

use std::io::Write as _;

use flate2::write::MultiGzDecoder;

use crate::reply::Status;
use crate::{Admission, PastAdmission, to_u64};

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
    match self {
      GzipRefused::TooLarge(_) => Status::CONTENT_TOO_LARGE,
      GzipRefused::Malformed | GzipRefused::Ended => Status::BAD_REQUEST,
    }
  }
}

/// A gzip body inflated as it arrives.
#[derive(Debug)]
pub struct Gunzip {
  /// flate2's decoder, writing into the bounded sink.
  decoder: MultiGzDecoder<Inflated>,
  /// Whether the body has ended or been refused: flate2's decoder must not be written after it finished.
  ended: bool,
}

/// The inflated bytes not yet taken, and the bounds of the feed under way.
#[derive(Debug, Default)]
struct Inflated {
  /// The bytes made and not yet taken.
  bytes: Vec<u8>,
  /// The room the feed began with: the admission's.
  room: u64,
  /// The most bytes `bytes` may hold by the feed's end: those held when it began and its room.
  limit: usize,
  /// The bytes the feed has made.
  made: u64,
  /// The refusal of a write past the room.
  refused: Option<PastAdmission>,
}

/// The message of the error the sink hands flate2 when a write would pass the room; the refusal itself is kept in the
/// sink, typed.
const PAST_ROOM: &str = "past the request's room";

impl std::io::Write for Inflated {
  fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
    let asked = self.made.saturating_add(to_u64(data.len()));
    if asked > self.room {
      self.refused = Some(PastAdmission {
        asked,
        left: self.room,
      });
      return Err(std::io::Error::other(PAST_ROOM));
    }
    reserve_within(&mut self.bytes, data.len(), self.limit);
    self.bytes.extend_from_slice(data);
    self.made = asked;
    Ok(data.len())
  }

  fn flush(&mut self) -> std::io::Result<()> {
    Ok(())
  }
}

/// Grows `bytes` for `more` bytes by doubling, as a vector grows, but never past `limit`, so the sink's buffer holds no
/// more than its room.
fn reserve_within(bytes: &mut Vec<u8>, more: usize, limit: usize) {
  let needed = bytes.len().saturating_add(more);
  if needed <= bytes.capacity() {
    return;
  }
  let doubled = bytes.capacity().saturating_mul(2).max(needed);
  let target = doubled.min(limit.max(needed));
  bytes.reserve_exact(target.saturating_sub(bytes.len()));
}

impl Default for Gunzip {
  fn default() -> Self {
    Self {
      decoder: MultiGzDecoder::new(Inflated::default()),
      ended: false,
    }
  }
}

impl Gunzip {
  /// An inflater of a new body.
  pub fn new() -> Self {
    Self::default()
  }

  /// Inflates `compressed`, the body's next bytes, adding what it makes to the bytes not yet taken; every byte made is
  /// charged to `admission`, and a feed that would make more than its room is refused.
  pub fn feed(&mut self, compressed: &[u8], admission: &mut Admission) -> Result<(), GzipRefused> {
    self.begin(admission)?;
    let written = self
      .decoder
      .write_all(compressed)
      .and_then(|()| self.decoder.flush());
    self.settle(written, admission)
  }

  /// Ends the body: whatever is left is inflated and charged, and the last member's CRC-32 and length are checked. A
  /// body that ended inside a member is refused here.
  pub fn finish(&mut self, admission: &mut Admission) -> Result<(), GzipRefused> {
    self.begin(admission)?;
    let finished = self.decoder.try_finish();
    let settled = self.settle(finished, admission);
    self.ended = true;
    settled
  }

  /// The inflated bytes made so far and not yet taken.
  pub fn take(&mut self) -> Vec<u8> {
    core::mem::take(&mut self.decoder.get_mut().bytes)
  }

  /// Opens a feed: refused after the end, and bounded by the admission's room.
  fn begin(&mut self, admission: &Admission) -> Result<(), GzipRefused> {
    if self.ended {
      return Err(GzipRefused::Ended);
    }
    let sink = self.decoder.get_mut();
    sink.room = admission.left();
    sink.limit = sink
      .bytes
      .len()
      .saturating_add(usize::try_from(sink.room).unwrap_or(usize::MAX));
    sink.made = 0;
    sink.refused = None;
    Ok(())
  }

  /// Closes a feed: charges what it made, held whatever the outcome, and refuses past the room or for a malformed
  /// stream, after which the inflater takes nothing more.
  fn settle(
    &mut self,
    result: std::io::Result<()>,
    admission: &mut Admission,
  ) -> Result<(), GzipRefused> {
    let sink = self.decoder.get_mut();
    let charged = admission.charge(sink.made);
    let outcome = match (result, sink.refused, charged) {
      (_, Some(past), _) | (Ok(()), None, Err(past)) => Err(GzipRefused::TooLarge(past)),
      (Err(_), None, _) => Err(GzipRefused::Malformed),
      (Ok(()), None, Ok(())) => Ok(()),
    };
    self.ended |= outcome.is_err();
    outcome
  }
}

#[cfg(test)]
#[path = "tests/gzip.rs"]
mod tests;
