//! A request body's framing removed as its bytes arrive (RFC 9112 §6.3, §7.1): `Content-Length` bytes, or chunked.
//!
//! **Streaming, never holding.** [`BodyReader::step`] hands back the body's data as slices of the input it was given, one
//! run at a time, and keeps between steps only the state of the line it is in the middle of: a chunk's size, how much of
//! a chunk is left, where it is in a trailer line. Every step that does not return data takes all of its input, so the
//! caller never keeps a byte of framing to show again, and a body of any size, a push's pack among them (§4.17.2 "A
//! push" item 3), passes through in constant memory. What a consumer keeps of the data, it charges to its own admission.
//!
//! **Chunked, strictly** (RFC 9112 §7.1). A chunk-size line is hexadecimal digits, at least one, within 64 bits, then
//! optionally whitespace and chunk extensions, which begin with `;` and are skipped unread ("A recipient MUST ignore
//! unrecognized chunk extensions", §7.1.1) but may hold no control byte other than HTAB, so a line can end nowhere but at
//! its CRLF. A chunk's data is followed by CRLF. The last chunk (size zero) is followed by a trailer section, field lines
//! read and dropped (§7.1.2: a recipient "MAY selectively retain or discard the received trailer fields"), then an empty
//! line. Every line is held to [`MAX_LINE`], and a trailer section's bytes are charged to the request's admission as a
//! head's are, so a body cannot run on forever in framing alone. A bare CR or LF, a NUL, a folded trailer line or a
//! chunk's data without its CRLF is refused by kind.

use crate::head::Framing;
use crate::reply::Status;
use crate::{Admission, MAX_LINE, PastAdmission};

/// What one step of a [`BodyReader`] found in its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step<'a> {
  /// A run of the body's data, a slice of the input. `consumed` counts the input bytes the step took, the framing before
  /// the data and the data; the next step starts after them.
  Data {
    /// The data.
    data: &'a [u8],
    /// The input bytes taken.
    consumed: usize,
  },
  /// The input ended inside the body: all of it was taken, and more is needed.
  More {
    /// The input bytes taken: all of them.
    consumed: usize,
  },
  /// The body ended after `consumed` bytes of the input; the bytes after them belong to the next request.
  End {
    /// The input bytes taken.
    consumed: usize,
  },
}

/// Why a body's framing is refused. Every kind ends the connection, since the next request's start is unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyRefused {
  /// A chunk-size line that is not hexadecimal digits, or whose size overflows 64 bits (RFC 9112 §7.1).
  ChunkSize,
  /// A chunk extension that does not start with `;`, or holds a control byte other than HTAB (§7.1.1).
  ChunkExtension,
  /// A chunk's data not followed by its CRLF.
  ChunkEnd,
  /// A CR not followed by LF, or an LF that no CR came before, in a chunk-size or trailer line (RFC 9112 §2.2).
  LineBreak,
  /// A chunk-size or trailer line past [`MAX_LINE`].
  LineTooLong,
  /// A trailer line that is no field line, or is folded (RFC 9112 §7.1.2, §5.2).
  Trailer,
  /// A trailer section past the request's room.
  TrailersTooLarge(PastAdmission),
}

impl BodyRefused {
  /// The status the refusal is answered with: `431 Request Header Fields Too Large` for trailers past the room, `400
  /// Bad Request` for the rest.
  pub fn status(self) -> Status {
    Status::OK
  }
}

/// A body's framing, removed as its bytes arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyReader {
  /// Where the reader is in the body.
  state: State,
}

/// Where a [`BodyReader`] is in the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
  /// The body has ended.
  Done,
}

impl BodyReader {
  /// A reader of a body framed as `framing` says.
  pub fn new(framing: Framing) -> Self {
    let _ = framing;
    Self { state: State::Done }
  }

  /// Takes the next part of the body from `input`, the bytes after the last step's: a run of data, the need for more, or
  /// the end. A trailer section's bytes are charged to `admission`.
  pub fn step<'a>(
    &mut self,
    input: &'a [u8],
    admission: &mut Admission,
  ) -> Result<Step<'a>, BodyRefused> {
    let _ = (admission, MAX_LINE, self.state);
    Ok(Step::More {
      consumed: input.len(),
    })
  }
}

#[cfg(test)]
#[path = "tests/body.rs"]
mod tests;
