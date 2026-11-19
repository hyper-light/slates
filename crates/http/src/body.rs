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

use crate::grammar::{HEX_RADIX, is_field_byte, is_tchar};
use crate::head::Framing;
use crate::reply::Status;
use crate::{Admission, MAX_LINE, PastAdmission, to_u64};

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
    match self {
      BodyRefused::TrailersTooLarge(_) => Status::FIELDS_TOO_LARGE,
      _ => Status::BAD_REQUEST,
    }
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
  /// `left` bytes of a `Content-Length` body remain.
  Length {
    /// The bytes left.
    left: u64,
  },
  /// In a chunk-size line.
  Size(SizeLine),
  /// `left` bytes of a chunk's data remain.
  Data {
    /// The bytes left.
    left: u64,
  },
  /// After a chunk's data, its CRLF; `cr` once its CR has come.
  DataEnd {
    /// Whether the CR has come.
    cr: bool,
  },
  /// In the trailer section.
  Trailer(TrailerLine),
  /// The body has ended.
  Done,
}

/// How far a chunk-size line has been read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SizeLine {
  /// The size its digits have given so far.
  size: u64,
  /// Whether a digit has come.
  digits: bool,
  /// Which part of the line the next byte is in.
  part: SizePart,
  /// The line's bytes so far, its CRLF not counted.
  length: usize,
}

/// The parts of a chunk-size line (RFC 9112 §7.1: `chunk-size [ chunk-ext ] CRLF`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SizePart {
  /// The size's hexadecimal digits.
  #[default]
  Digits,
  /// Whitespace after the digits, which only an extension may follow.
  Space,
  /// The extensions, from the first `;`.
  Extension,
  /// The CR has come; the LF ends the line.
  Cr,
}

/// How far a trailer line has been read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TrailerLine {
  /// Which part of the line the next byte is in.
  part: TrailerPart,
  /// The line's bytes so far, its CRLF not counted.
  length: usize,
}

/// The parts of a trailer line (RFC 9112 §7.1.2: field lines, then the empty line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum TrailerPart {
  /// The line's start: a name's first byte, or the empty line's CR.
  #[default]
  Start,
  /// The name, up to its colon.
  Name,
  /// The value.
  Value,
  /// A field line's CR has come.
  Cr,
  /// The empty line's CR has come; its LF ends the body.
  EndCr,
}

impl BodyReader {
  /// A reader of a body framed as `framing` says.
  pub fn new(framing: Framing) -> Self {
    let state = match framing {
      Framing::None | Framing::Length(0) => State::Done,
      Framing::Length(left) => State::Length { left },
      Framing::Chunked => State::Size(SizeLine::default()),
    };
    Self { state }
  }

  /// Takes the next part of the body from `input`, the bytes after the last step's: a run of data, the need for more, or
  /// the end. A trailer section's bytes are charged to `admission`.
  pub fn step<'a>(
    &mut self,
    input: &'a [u8],
    admission: &mut Admission,
  ) -> Result<Step<'a>, BodyRefused> {
    let mut at = 0usize;
    loop {
      match self.state {
        State::Done => return Ok(Step::End { consumed: at }),
        State::Length { left } => return Ok(self.length_run(input, at, left)),
        State::Data { left } => return Ok(self.chunk_run(input, at, left)),
        State::Size(_) | State::DataEnd { .. } | State::Trailer(_) => {}
      }
      let Some(&byte) = input.get(at) else {
        return Ok(Step::More { consumed: at });
      };
      self.state = framing_byte(self.state, byte, admission)?;
      at = at.saturating_add(1);
    }
  }

  /// A run of a `Content-Length` body's data.
  fn length_run<'a>(&mut self, input: &'a [u8], at: usize, left: u64) -> Step<'a> {
    let (step, left) = run(input, at, left);
    self.state = if left == 0 {
      State::Done
    } else {
      State::Length { left }
    };
    step
  }

  /// A run of a chunk's data.
  fn chunk_run<'a>(&mut self, input: &'a [u8], at: usize, left: u64) -> Step<'a> {
    let (step, left) = run(input, at, left);
    self.state = if left == 0 {
      State::DataEnd { cr: false }
    } else {
      State::Data { left }
    };
    step
  }
}

/// The next run of data in `input` from `at`, at most `left` bytes, and what is left after it; the need for more when
/// the input has ended.
fn run(input: &[u8], at: usize, left: u64) -> (Step<'_>, u64) {
  let rest = input.get(at..).unwrap_or_default();
  if rest.is_empty() {
    return (Step::More { consumed: at }, left);
  }
  let take = usize::try_from(left).map_or(rest.len(), |left| left.min(rest.len()));
  let step = Step::Data {
    data: rest.get(..take).unwrap_or_default(),
    consumed: at.saturating_add(take),
  };
  (step, left.saturating_sub(to_u64(take)))
}

/// The state after one byte of framing: a chunk-size line's, a chunk's CRLF, or the trailer section's, whose every byte
/// is charged.
fn framing_byte(state: State, byte: u8, admission: &mut Admission) -> Result<State, BodyRefused> {
  match state {
    State::Size(line) => size_byte(line, byte),
    State::DataEnd { cr } => data_end_byte(cr, byte),
    State::Trailer(line) => {
      admission.charge(1).map_err(BodyRefused::TrailersTooLarge)?;
      trailer_byte(line, byte)
    }
    other => Ok(other),
  }
}

/// One byte of a chunk-size line.
fn size_byte(line: SizeLine, byte: u8) -> Result<State, BodyRefused> {
  match (line.part, byte) {
    (SizePart::Cr, b'\n') => Ok(chunk_start(line.size)),
    (SizePart::Cr, _) | (_, b'\n') => Err(BodyRefused::LineBreak),
    (_, b'\r') if !line.digits => Err(BodyRefused::ChunkSize),
    (SizePart::Space, b'\r') => Err(BodyRefused::ChunkExtension),
    (_, b'\r') => Ok(State::Size(SizeLine {
      part: SizePart::Cr,
      ..line
    })),
    (part, _) => {
      let line = grow_size(line)?;
      let line = match part {
        SizePart::Digits => size_digit(line, byte)?,
        SizePart::Space => size_space(line, byte)?,
        SizePart::Extension | SizePart::Cr => extension_byte(line, byte)?,
      };
      Ok(State::Size(line))
    }
  }
}

/// What follows a chunk-size line: the chunk's data, or the trailer section after the last chunk.
fn chunk_start(size: u64) -> State {
  if size == 0 {
    State::Trailer(TrailerLine::default())
  } else {
    State::Data { left: size }
  }
}

/// A chunk-size line one byte longer, refused past [`MAX_LINE`].
fn grow_size(line: SizeLine) -> Result<SizeLine, BodyRefused> {
  let length = line.length.saturating_add(1);
  if length > MAX_LINE {
    Err(BodyRefused::LineTooLong)
  } else {
    Ok(SizeLine { length, ..line })
  }
}

/// One byte in the size's digits: another digit, within 64 bits; whitespace or `;` after one at least.
fn size_digit(line: SizeLine, byte: u8) -> Result<SizeLine, BodyRefused> {
  if let Some(digit) = char::from(byte).to_digit(HEX_RADIX) {
    let size = line
      .size
      .checked_mul(u64::from(HEX_RADIX))
      .and_then(|size| size.checked_add(u64::from(digit)))
      .ok_or(BodyRefused::ChunkSize)?;
    return Ok(SizeLine {
      size,
      digits: true,
      ..line
    });
  }
  match byte {
    b';' if line.digits => Ok(SizeLine {
      part: SizePart::Extension,
      ..line
    }),
    b' ' | b'\t' if line.digits => Ok(SizeLine {
      part: SizePart::Space,
      ..line
    }),
    _ => Err(BodyRefused::ChunkSize),
  }
}

/// One byte of whitespace after the size: more whitespace, or the `;` that starts an extension.
fn size_space(line: SizeLine, byte: u8) -> Result<SizeLine, BodyRefused> {
  match byte {
    b' ' | b'\t' => Ok(line),
    b';' => Ok(SizeLine {
      part: SizePart::Extension,
      ..line
    }),
    _ => Err(BodyRefused::ChunkExtension),
  }
}

/// One byte of an extension: skipped unread, but no control byte other than HTAB.
fn extension_byte(line: SizeLine, byte: u8) -> Result<SizeLine, BodyRefused> {
  if is_field_byte(byte) {
    Ok(line)
  } else {
    Err(BodyRefused::ChunkExtension)
  }
}

/// One byte of the CRLF after a chunk's data.
fn data_end_byte(cr: bool, byte: u8) -> Result<State, BodyRefused> {
  match (cr, byte) {
    (false, b'\r') => Ok(State::DataEnd { cr: true }),
    (true, b'\n') => Ok(State::Size(SizeLine::default())),
    _ => Err(BodyRefused::ChunkEnd),
  }
}

/// One byte of the trailer section.
fn trailer_byte(line: TrailerLine, byte: u8) -> Result<State, BodyRefused> {
  match (line.part, byte) {
    (TrailerPart::EndCr, b'\n') => Ok(State::Done),
    (TrailerPart::Cr, b'\n') => Ok(State::Trailer(TrailerLine::default())),
    (TrailerPart::EndCr | TrailerPart::Cr, _) | (_, b'\n') => Err(BodyRefused::LineBreak),
    (TrailerPart::Start, b'\r') => Ok(State::Trailer(TrailerLine {
      part: TrailerPart::EndCr,
      ..line
    })),
    (TrailerPart::Value, b'\r') => Ok(State::Trailer(TrailerLine {
      part: TrailerPart::Cr,
      ..line
    })),
    (part, _) => trailer_content(line, part, byte),
  }
}

/// One byte of a trailer line's name or value: a token's to the colon, a field value's after it.
fn trailer_content(line: TrailerLine, part: TrailerPart, byte: u8) -> Result<State, BodyRefused> {
  let length = line.length.saturating_add(1);
  if length > MAX_LINE {
    return Err(BodyRefused::LineTooLong);
  }
  let part = match part {
    TrailerPart::Start | TrailerPart::Name if is_tchar(byte) => TrailerPart::Name,
    TrailerPart::Name if byte == b':' => TrailerPart::Value,
    TrailerPart::Value if is_field_byte(byte) => TrailerPart::Value,
    _ => return Err(BodyRefused::Trailer),
  };
  Ok(State::Trailer(TrailerLine { part, length }))
}

#[cfg(test)]
#[path = "tests/body.rs"]
mod tests;
