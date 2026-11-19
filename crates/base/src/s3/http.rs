//! HTTP/1.1 for an S3 base's requests (A-471; §4.15 "An S3 base"): a request's head written, and a response read a
//! piece at a time as its bytes arrive, every length checked against its cap before anything is held for it. Pure: the
//! client (`super::client`) moves the bytes; this module only frames them, so every rule is tested without a socket.
//!
//! The framing is RFC 9112's (HTTP/1.1, 2022):
//! - a response's head is its status line and its fields up to the empty line (§2.1, §4, §5); a field's name meets its
//!   colon with no whitespace between, and a line folded onto the next (`obs-fold`) is refused (§5.1, §5.2);
//! - its body's length (§6.3): none for a `HEAD` request or a `204`, `304` or interim status; `chunked` when
//!   `Transfer-Encoding` names it last, and nothing else, since a base never asks for another coding; else
//!   `Content-Length`, every value of it equal. A message with both is refused, as §6.3 permits ("ought to be handled
//!   as an error"): it is the shape request smuggling takes. One with neither is refused too: S3 always frames its
//!   responses, and a body that ends at the connection's close cannot be told from one cut short;
//! - a chunked body is chunks of a hex size, its extensions passed over, then its data and a CRLF, until the
//!   zero-sized chunk and the trailer section's empty line (§7.1).
//!
//! The caps, each derived from what S3 documents (R3):
//! - a request's head ([`request_cap`]): its line, a key's longest encoding twice over (its path, and a listing's
//!   prefix), the longest continuation token assumed as long, and S3's documented 8 KB request header;
//! - a response's head ([`RESPONSE_HEAD_CAP`]): the stored headers a PUT may set (held to that same 8 KB) and S3's own;
//! - a body: what the request asked (a range's length, a listing page's cap), or an error document's
//!   ([`error_body_cap`]), whichever the status says the body is; a `Content-Length` past it, or chunks that would
//!   pass it, are refused before a byte of the excess is held.

use super::list::KEY_MAX_BYTES;

/// Format: S3's documented limit on a request's header ("The PUT request header is limited to 8 KB in size", the user
/// guide's "Working with object metadata").
pub const S3_REQUEST_HEADER_BYTES: usize = 8 * 1024;
/// Format: the bytes URL encoding writes for one byte at most (`%XX`).
const ENCODED_BYTE_MAX: usize = 3;
/// Format: the request line's fixed parts at their longest: the method (`HEAD`), the spaces, `HTTP/1.1` and its CRLF,
/// the bucket of a path-style request (63 bytes, S3's bucket name limit) with its slashes, and a listing's fixed query
/// (`list-type=2&delimiter=%2F&encoding-type=url&max-keys=1000` and the separators of its two variable parameters).
const REQUEST_LINE_FIXED: usize = 4 + 2 + 8 + 2 + 63 + 2 + 96;
/// Format: the response head's own lines, at most: S3's response headers (about twenty-five lines in GetObject's
/// response syntax, `Date`, `ETag`, `Content-Range`, `x-amz-request-id`, `x-amz-id-2`, the checksums, the version id,
/// the expiration and restore notes, each well under 128 bytes) and the status line: about 3,300 bytes, rounded up to a
/// power of two.
const S3_RESPONSE_OWN_BYTES: usize = 4 * 1024;
/// Derived: the most a response's head may hold: the stored headers an object carries (its content type, disposition,
/// encoding, language, cache control and user metadata, all set by a PUT whose header S3 holds to
/// [`S3_REQUEST_HEADER_BYTES`]) and S3's own lines ([`S3_RESPONSE_OWN_BYTES`]): 12 KiB.
pub const RESPONSE_HEAD_CAP: usize = S3_REQUEST_HEADER_BYTES + S3_RESPONSE_OWN_BYTES;
/// Format: the CRLF that ends a line.
const CRLF: &[u8] = b"\r\n";
/// Format: the empty line that ends a head.
const HEAD_END: &[u8] = b"\r\n\r\n";
/// Format: a chunk size's hex digits at most: a `u64`'s sixteen.
const CHUNK_SIZE_DIGITS: usize = 16;
/// Format: the radix of a chunk's size (RFC 9112 §7.1: `chunk-size = 1*HEXDIG`).
const HEX_RADIX: u32 = 16;
/// Format: the request line's parts as long as an encoded key: the key's path, a listing's prefix, and a continuation
/// token, assumed no longer (S3 states no length; part 3's live run measures it).
const KEY_LONG_PARTS: usize = 3;
/// Format: the characters a refused signature's error document writes each echoed byte as (`CanonicalRequestBytes`,
/// `StringToSignBytes` in S3's `SignatureDoesNotMatch`: two hex digits and a space).
const ECHO_CHARS_PER_BYTE: usize = 3;
/// Format: a status line's parts, `HTTP-version SP status-code SP [reason-phrase]` (RFC 9112 §4); the reason phrase,
/// which may hold spaces, is the last.
const STATUS_LINE_PARTS: usize = 3;
/// Format: a status code's digits (RFC 9110 §15: "a three-digit integer code").
const STATUS_DIGITS: usize = 3;
/// Format: a status's class is its first digit (RFC 9110 §15), the status divided by a hundred.
const STATUS_CLASS_SPAN: u16 = 100;
/// Format: the class of an interim status, `1xx` (RFC 9110 §15.2), which a base never asks for.
const INTERIM_CLASS: u16 = 1;
/// Format: the class of a successful status, `2xx` (RFC 9110 §15.3), whose body is what the request asked.
const SUCCESS_CLASS: u16 = 2;
/// Format: `204 No Content`, whose response has no body (RFC 9110 §15.3.5; RFC 9112 §6.3).
pub const NO_CONTENT: u16 = 204;
/// Format: `206 Partial Content`, a range's answer (RFC 9110 §15.3.7).
pub const PARTIAL_CONTENT: u16 = 206;
/// Format: `200 OK` (RFC 9110 §15.3.1): to a ranged `GetObject`, the whole object (a range S3 ignored or covering it).
pub const OK: u16 = 200;
/// Format: `304 Not Modified`, whose response has no body (RFC 9110 §15.4.5; RFC 9112 §6.3).
pub const NOT_MODIFIED: u16 = 304;
/// Format: `400 Bad Request`, S3's status for `ExpiredToken` among other codes (S3's error responses).
pub const BAD_REQUEST: u16 = 400;
/// Format: `403 Forbidden`: `AccessDenied`, `SignatureDoesNotMatch`, `InvalidAccessKeyId`, `RequestTimeTooSkewed`.
pub const FORBIDDEN: u16 = 403;
/// Format: `404 Not Found`: `NoSuchKey`, `NoSuchBucket`, `NoSuchVersion`.
pub const NOT_FOUND: u16 = 404;
/// Format: `412 Precondition Failed`: an `If-Match` that no longer holds (RFC 9110 §13.1.1, §15.5.13).
pub const PRECONDITION_FAILED: u16 = 412;
/// Format: `416 Range Not Satisfiable`, S3's `InvalidRange` (RFC 9110 §15.5.17).
pub const RANGE_NOT_SATISFIABLE: u16 = 416;
/// Format: `500 Internal Server Error`, S3's `InternalError` ("Try again").
pub const INTERNAL_SERVER_ERROR: u16 = 500;
/// Format: `503 Service Unavailable`: S3's `SlowDown` and `ServiceUnavailable` ("Reduce your request rate").
pub const SERVICE_UNAVAILABLE: u16 = 503;

/// Derived: the most a request's head may hold: its line (the fixed parts, a key's path at its longest encoding, a
/// listing's prefix as long, and a continuation token assumed no longer: S3 calls tokens obfuscated and states no
/// length, which part 3's live run measures) and S3's documented request header limit. A request past it is refused
/// before a byte is sent.
pub fn request_cap() -> usize {
  let encoded_key = usize::try_from(KEY_MAX_BYTES)
    .unwrap_or(usize::MAX)
    .saturating_mul(ENCODED_BYTE_MAX);
  REQUEST_LINE_FIXED
    .saturating_add(encoded_key.saturating_mul(KEY_LONG_PARTS))
    .saturating_add(S3_REQUEST_HEADER_BYTES)
}

/// Derived: the most an error document may hold. A refused signature's (`SignatureDoesNotMatch`) is the longest: it
/// echoes the canonical request, at most a request's head ([`request_cap`]: its fields are the head's, less the
/// `Authorization` the head adds), and its bytes again at [`ECHO_CHARS_PER_BYTE`] characters a byte, so four request
/// heads; and the string to sign with its bytes (under 600 bytes), the code, message, key id, signature and request
/// ids, all well under S3's own response lines ([`S3_RESPONSE_OWN_BYTES`]), which are added.
pub fn error_body_cap() -> usize {
  request_cap()
    .saturating_mul(ECHO_CHARS_PER_BYTE.saturating_add(1))
    .saturating_add(S3_RESPONSE_OWN_BYTES)
}

/// Whether `status` is a successful one, `2xx` (RFC 9110 §15.3).
pub fn is_success(status: u16) -> bool {
  status.checked_div(STATUS_CLASS_SPAN) == Some(SUCCESS_CLASS)
}

/// Why a request or a response was refused, by its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
  /// A request's head past [`request_cap`].
  RequestTooLarge,
  /// A request's field holding a CR, an LF or a NUL, which would end it early.
  BadRequestField,
  /// A response's head past [`RESPONSE_HEAD_CAP`].
  HeadTooLong,
  /// A status line that is not `HTTP/1.x` and a three-digit status.
  BadStatusLine,
  /// An interim (`1xx`) status, which a base never asks for.
  InterimStatus,
  /// A field line with no colon, whitespace before its colon, a name that is no token, or a folded line.
  BadFieldLine,
  /// Both `Transfer-Encoding` and `Content-Length`.
  AmbiguousLength,
  /// A `Content-Length` that is not one decimal length, or two that differ.
  BadContentLength,
  /// A transfer coding other than `chunked`, or `chunked` not last.
  UnsupportedTransferEncoding,
  /// A body framed neither by length nor by chunks.
  NoLength,
  /// A body past its cap.
  BodyTooLong {
    /// The cap.
    cap: u64,
  },
  /// A chunk's size line that does not parse, or is longer than a field line may be.
  BadChunkSize,
  /// A chunk's data not followed by its CRLF.
  BadChunkEnd,
  /// A trailer section past [`RESPONSE_HEAD_CAP`].
  TrailerTooLong,
}

/// Writes a request's head: `method target HTTP/1.1`, each field as `name: value`, and the empty line. Refused when a
/// field or the target holds a CR, an LF or a NUL (it would end the head early: a value from S3, an ETag decoded from a
/// listing, is no exception), and when the head passes [`request_cap`].
pub fn request_head(
  method: &str,
  target: &str,
  fields: &[(&str, &str)],
) -> Result<Vec<u8>, HttpError> {
  let breaks = |text: &str| text.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0'));
  if breaks(method) || breaks(target) || fields.iter().any(|(n, v)| breaks(n) || breaks(v)) {
    return Err(HttpError::BadRequestField);
  }
  let mut head = Vec::new();
  for part in [method, " ", target, " HTTP/1.1\r\n"] {
    head.extend_from_slice(part.as_bytes());
  }
  for (name, value) in fields {
    for part in [*name, ": ", *value, "\r\n"] {
      head.extend_from_slice(part.as_bytes());
    }
  }
  head.extend_from_slice(CRLF);
  if head.len() > request_cap() {
    return Err(HttpError::RequestTooLarge);
  }
  Ok(head)
}

/// A response read: its status, its fields (names in lower case, in the order sent) and its body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
  /// The status code.
  pub status: u16,
  /// The fields, each name lower-cased.
  pub fields: Vec<(String, String)>,
  /// The body, decoded.
  pub body: Vec<u8>,
  /// Whether the server closes the connection after this response (`Connection: close`, or HTTP/1.0).
  pub closes: bool,
}

impl Response {
  /// The first value of the field `name` (lower case), if sent.
  pub fn field(&self, name: &str) -> Option<&str> {
    self
      .fields
      .iter()
      .find(|(field, _)| field == name)
      .map(|(_, value)| value.as_str())
  }
}

/// What a response's body is capped at: what a successful answer may hold (a range's length, a listing page's cap), and
/// what an error document may ([`error_body_cap`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyCaps {
  /// The cap of a `2xx` response's body.
  pub success: u64,
  /// The cap of any other response's body.
  pub error: u64,
}

/// Where a [`ResponseReader`] stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
  /// The head is being read.
  Head,
  /// A length-framed body, `left` bytes of it to come.
  Length { left: u64 },
  /// A chunk's size line is next.
  ChunkSize,
  /// A chunk's data, `left` bytes of it to come.
  ChunkData { left: u64 },
  /// A chunk's closing CRLF is next.
  ChunkEnd,
  /// The trailer section, `held` bytes of it read so far.
  Trailers { held: usize },
  /// The response is whole.
  Done,
}

/// What [`ResponseReader::advance`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Advance {
  /// More bytes are needed.
  More,
  /// The response is whole.
  Done(Response),
}

/// A response read as its bytes arrive. Each [`ResponseReader::advance`] takes what it can frame from the front of the
/// bytes received so far and leaves the rest, so the received bytes never hold more than a head's cap or a partial
/// chunk line beyond the body read so far.
#[derive(Debug)]
pub struct ResponseReader {
  caps: BodyCaps,
  head_request: bool,
  stage: Stage,
  response: Response,
  cap: u64,
}

impl ResponseReader {
  /// A reader for the response to a request: `head_request` for a `HEAD` (no body, whatever its length says), the
  /// body's `caps` otherwise.
  pub fn new(head_request: bool, caps: BodyCaps) -> ResponseReader {
    ResponseReader {
      caps,
      head_request,
      stage: Stage::Head,
      response: Response::default(),
      cap: 0,
    }
  }

  /// Frames what it can of `received`, removing what it framed from its front: [`Advance::Done`] with the response
  /// once whole (the bytes after it left in `received`), [`Advance::More`] while it needs more, or the framing's
  /// refusal.
  pub fn advance(&mut self, received: &mut Vec<u8>) -> Result<Advance, HttpError> {
    loop {
      let progressed = match self.stage {
        Stage::Head => self.head(received)?,
        Stage::Length { left } => {
          let taken = self.take_data(received, left);
          self.stage = match left.saturating_sub(taken) {
            0 => Stage::Done,
            rest => Stage::Length { left: rest },
          };
          taken > 0 || self.stage == Stage::Done
        }
        Stage::ChunkSize => self.chunk_size(received)?,
        Stage::ChunkData { left } => {
          let taken = self.take_data(received, left);
          self.stage = match left.saturating_sub(taken) {
            0 => Stage::ChunkEnd,
            rest => Stage::ChunkData { left: rest },
          };
          taken > 0
        }
        Stage::ChunkEnd => {
          if received.len() < CRLF.len() {
            false
          } else if received.starts_with(CRLF) {
            received.drain(..CRLF.len());
            self.stage = Stage::ChunkSize;
            true
          } else {
            return Err(HttpError::BadChunkEnd);
          }
        }
        Stage::Trailers { held } => self.trailers(received, held)?,
        Stage::Done => {
          return Ok(Advance::Done(std::mem::take(&mut self.response)));
        }
      };
      if !progressed {
        return Ok(Advance::More);
      }
    }
  }

  /// Whether the response's head has been read (the moment a request's round trip is measured to).
  pub fn has_head(&self) -> bool {
    self.stage != Stage::Head
  }

  /// Moves up to `left` bytes of data from the front of `received` into the body; how many.
  fn take_data(&mut self, received: &mut Vec<u8>, left: u64) -> u64 {
    let count = usize::try_from(left)
      .unwrap_or(usize::MAX)
      .min(received.len());
    self.response.body.extend(received.drain(..count));
    u64::try_from(count).unwrap_or(u64::MAX)
  }

  /// Reads the head once its empty line has arrived, refusing one past its cap whether whole or not; then decides the
  /// body's framing. Whether it progressed.
  fn head(&mut self, received: &mut Vec<u8>) -> Result<bool, HttpError> {
    let Some(at) = find(received, HEAD_END) else {
      if received.len() > RESPONSE_HEAD_CAP {
        return Err(HttpError::HeadTooLong);
      }
      return Ok(false);
    };
    let end = at.saturating_add(HEAD_END.len());
    if end > RESPONSE_HEAD_CAP {
      return Err(HttpError::HeadTooLong);
    }
    let head = std::str::from_utf8(received.get(..at).unwrap_or_default())
      .map_err(|_| HttpError::BadFieldLine)?;
    let mut lines = head.split("\r\n");
    let (status, closes) = status_line(lines.next().unwrap_or_default())?;
    self.response.status = status;
    self.response.closes = closes;
    for line in lines {
      self.response.fields.push(field_line(line)?);
    }
    if self
      .response
      .field("connection")
      .is_some_and(|value| value.eq_ignore_ascii_case("close"))
    {
      self.response.closes = true;
    }
    received.drain(..end);
    self.cap = if is_success(status) {
      self.caps.success
    } else {
      self.caps.error
    };
    self.stage = self.framing()?;
    Ok(true)
  }

  /// The body's framing, by RFC 9112 §6.3, from the head just read.
  fn framing(&self) -> Result<Stage, HttpError> {
    let status = self.response.status;
    if self.head_request || status == NO_CONTENT || status == NOT_MODIFIED {
      return Ok(Stage::Done);
    }
    let encodings: Vec<&str> = self
      .response
      .fields
      .iter()
      .filter(|(name, _)| name == "transfer-encoding")
      .flat_map(|(_, value)| value.split(','))
      .map(str::trim)
      .filter(|coding| !coding.is_empty())
      .collect();
    let lengths: Vec<&str> = self
      .response
      .fields
      .iter()
      .filter(|(name, _)| name == "content-length")
      .flat_map(|(_, value)| value.split(','))
      .map(str::trim)
      .collect();
    match (encodings.as_slice(), lengths.as_slice()) {
      ([], []) => Err(HttpError::NoLength),
      ([_, ..], [_, ..]) => Err(HttpError::AmbiguousLength),
      ([coding], []) if coding.eq_ignore_ascii_case("chunked") => Ok(Stage::ChunkSize),
      ([_, ..], []) => Err(HttpError::UnsupportedTransferEncoding),
      ([], [first, rest @ ..]) => {
        let length = decimal(first).ok_or(HttpError::BadContentLength)?;
        if rest.iter().any(|other| decimal(other) != Some(length)) {
          return Err(HttpError::BadContentLength);
        }
        if length > self.cap {
          return Err(HttpError::BodyTooLong { cap: self.cap });
        }
        Ok(match length {
          0 => Stage::Done,
          left => Stage::Length { left },
        })
      }
    }
  }

  /// Reads a chunk's size line once whole, refusing one longer than a head's cap; a size that would take the body past
  /// its cap is refused before a byte of its data is held. Whether it progressed.
  fn chunk_size(&mut self, received: &mut Vec<u8>) -> Result<bool, HttpError> {
    let Some(at) = find(received, CRLF) else {
      if received.len() > RESPONSE_HEAD_CAP {
        return Err(HttpError::BadChunkSize);
      }
      return Ok(false);
    };
    if at > RESPONSE_HEAD_CAP {
      return Err(HttpError::BadChunkSize);
    }
    let line = std::str::from_utf8(received.get(..at).unwrap_or_default())
      .map_err(|_| HttpError::BadChunkSize)?;
    // The size, then any extensions after a `;`, which a base never asks for and passes over.
    let digits = line.split(';').next().unwrap_or_default().trim_end();
    if digits.is_empty()
      || digits.len() > CHUNK_SIZE_DIGITS
      || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
      return Err(HttpError::BadChunkSize);
    }
    let size = u64::from_str_radix(digits, HEX_RADIX).map_err(|_| HttpError::BadChunkSize)?;
    let held = u64::try_from(self.response.body.len()).unwrap_or(u64::MAX);
    if held.checked_add(size).is_none_or(|total| total > self.cap) {
      return Err(HttpError::BodyTooLong { cap: self.cap });
    }
    received.drain(..at.saturating_add(CRLF.len()));
    self.stage = match size {
      0 => Stage::Trailers { held: 0 },
      left => Stage::ChunkData { left },
    };
    Ok(true)
  }

  /// Passes the trailer section over a line at a time, to its empty line, refusing one past a head's cap. Whether it
  /// progressed.
  fn trailers(&mut self, received: &mut Vec<u8>, held: usize) -> Result<bool, HttpError> {
    let Some(at) = find(received, CRLF) else {
      if held.saturating_add(received.len()) > RESPONSE_HEAD_CAP {
        return Err(HttpError::TrailerTooLong);
      }
      return Ok(false);
    };
    let line = at.saturating_add(CRLF.len());
    let held = held.saturating_add(line);
    if held > RESPONSE_HEAD_CAP {
      return Err(HttpError::TrailerTooLong);
    }
    received.drain(..line);
    self.stage = if at == 0 {
      Stage::Done
    } else {
      Stage::Trailers { held }
    };
    Ok(true)
  }
}

/// Where `needle` first occurs in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack
    .windows(needle.len())
    .position(|window| window == needle)
}

/// A decimal length: digits only, at most a `u64`.
fn decimal(text: &str) -> Option<u64> {
  if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  text.parse::<u64>().ok()
}

/// A status line's status, and whether the version closes the connection after it (HTTP/1.0); interim statuses
/// refused, and a status below them, which RFC 9110 §15 calls invalid.
fn status_line(line: &str) -> Result<(u16, bool), HttpError> {
  let mut parts = line.splitn(STATUS_LINE_PARTS, ' ');
  let version = parts.next().unwrap_or_default();
  let closes = match version {
    "HTTP/1.1" => false,
    "HTTP/1.0" => true,
    _ => return Err(HttpError::BadStatusLine),
  };
  let code = parts.next().unwrap_or_default();
  if code.len() != STATUS_DIGITS || !code.bytes().all(|b| b.is_ascii_digit()) {
    return Err(HttpError::BadStatusLine);
  }
  let status = code.parse::<u16>().map_err(|_| HttpError::BadStatusLine)?;
  match status.checked_div(STATUS_CLASS_SPAN) {
    Some(INTERIM_CLASS) => Err(HttpError::InterimStatus),
    Some(0) | None => Err(HttpError::BadStatusLine),
    Some(_) => Ok((status, closes)),
  }
}

/// A field line's name, lower-cased, and its value without the whitespace around it; a line with no colon, with
/// whitespace before its colon, with a name that is no token, or folded (starting with whitespace) is refused.
fn field_line(line: &str) -> Result<(String, String), HttpError> {
  let (name, value) = line.split_once(':').ok_or(HttpError::BadFieldLine)?;
  let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
  if name.is_empty() || !name.bytes().all(token) {
    return Err(HttpError::BadFieldLine);
  }
  Ok((
    name.to_ascii_lowercase(),
    value.trim_matches(|c| c == ' ' || c == '\t').to_owned(),
  ))
}

#[cfg(test)]
mod tests {
  use proptest::prelude::*;

  use super::*;

  /// Shape: the caps a test reads under: a success body of 64 bytes, an error document of 32.
  const CAPS: BodyCaps = BodyCaps {
    success: 64,
    error: 32,
  };

  /// `bytes` fed to a fresh reader `split` bytes at a time (all at once when zero): the response, or the refusal.
  fn read(bytes: &[u8], head_request: bool, split: usize) -> Result<Response, HttpError> {
    let mut reader = ResponseReader::new(head_request, CAPS);
    let mut received = Vec::new();
    let step = if split == 0 {
      bytes.len().max(1)
    } else {
      split
    };
    for piece in bytes.chunks(step) {
      received.extend_from_slice(piece);
      if let Advance::Done(response) = reader.advance(&mut received)? {
        return Ok(response);
      }
    }
    match reader.advance(&mut received)? {
      Advance::Done(response) => Ok(response),
      Advance::More => Err(HttpError::NoLength),
    }
  }

  /// A-471 (RFC 9112 §6, §7.1): do read a ranged answer framed by its length, a chunked answer with an extension and a
  /// trailer, and a `HEAD` answer whose length describes the object, each fed whole and a byte at a time. Expect the
  /// same status, fields (lower-cased names, values trimmed) and decoded body either way, and no body for the `HEAD`.
  #[test]
  fn length_chunked_and_head_answers_read_alike_however_their_bytes_arrive() {
    let length =
      b"HTTP/1.1 206 Partial Content\r\nETag: \"abc\"\r\nContent-Range: bytes 0-9/100\r\n\
                   Content-Length: 10\r\n\r\n0123456789";
    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4;ext=1\r\nabcd\r\n\
                    A\r\n0123456789\r\n0\r\nx-trailer: t\r\n\r\n";
    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nx-amz-version-id:  v1 \r\n\r\n";
    for split in [0, 1] {
      let ranged = read(length, false, split).unwrap();
      assert_eq!(ranged.status, 206);
      assert_eq!(ranged.field("etag"), Some("\"abc\""));
      assert_eq!(ranged.body, b"0123456789");
      let decoded = read(chunked, false, split).unwrap();
      assert_eq!(decoded.body, b"abcd0123456789");
      let headed = read(head, true, split).unwrap();
      assert_eq!(headed.field("content-length"), Some("4096"));
      assert_eq!(headed.field("x-amz-version-id"), Some("v1"));
      assert!(headed.body.is_empty());
    }
  }

  /// A-471: do read two responses back to back from one buffer, as a kept-alive connection delivers them. Expect the
  /// first whole and the second's bytes left for its own reader.
  #[test]
  fn a_kept_alive_connections_next_response_is_left_for_its_reader() {
    let mut received =
      b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 404 Not Found\r\n".to_vec();
    let mut reader = ResponseReader::new(false, CAPS);
    let Advance::Done(first) = reader.advance(&mut received).unwrap() else {
      panic!("the first response is whole");
    };
    assert_eq!(first.body, b"ok");
    assert_eq!(received, b"HTTP/1.1 404 Not Found\r\n");
  }

  /// A-471 (hostile input): do read answers each broken one way: a head past its cap, whole or still arriving; a status
  /// line of another protocol, of a two-digit or non-digit status, an interim one, and one below the interim class; a
  /// field with no colon, with a space before its colon, and folded; both length framings; two lengths that differ, and
  /// a length that is no number; a coding other than chunked, and chunked not last; no framing at all; a length past
  /// the success cap and one past the error cap; a chunk size of no hex, of seventeen digits, and one that would pass
  /// the cap; a chunk not closed by its CRLF; and a trailer section past the cap. Expect each refused by its kind,
  /// never a panic.
  #[test]
  fn every_hostile_answer_is_refused_by_its_kind() {
    let long = format!(
      "HTTP/1.1 200 OK\r\nx-pad: {}\r\nContent-Length: 0\r\n\r\n",
      "a".repeat(RESPONSE_HEAD_CAP)
    );
    let unfinished = format!(
      "HTTP/1.1 200 OK\r\nx-pad: {}",
      "a".repeat(RESPONSE_HEAD_CAP)
    );
    let long_trailer = format!(
      "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nx: {}\r\n\r\n",
      "t".repeat(RESPONSE_HEAD_CAP)
    );
    let cases: Vec<(String, HttpError)> = vec![
      (long, HttpError::HeadTooLong),
      (unfinished, HttpError::HeadTooLong),
      ("SPDY/3 200 OK\r\n\r\n".to_owned(), HttpError::BadStatusLine),
      (
        "HTTP/1.1 20 OK\r\n\r\n".to_owned(),
        HttpError::BadStatusLine,
      ),
      (
        "HTTP/1.1 2x0 OK\r\n\r\n".to_owned(),
        HttpError::BadStatusLine,
      ),
      (
        "HTTP/1.1 100 Continue\r\n\r\n".to_owned(),
        HttpError::InterimStatus,
      ),
      (
        "HTTP/1.1 099 Below\r\n\r\n".to_owned(),
        HttpError::BadStatusLine,
      ),
      (
        "HTTP/1.1 200 OK\r\nno-colon\r\n\r\n".to_owned(),
        HttpError::BadFieldLine,
      ),
      (
        "HTTP/1.1 200 OK\r\nName : v\r\n\r\n".to_owned(),
        HttpError::BadFieldLine,
      ),
      (
        "HTTP/1.1 200 OK\r\nA: v\r\n folded\r\n\r\n".to_owned(),
        HttpError::BadFieldLine,
      ),
      (
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n".to_owned(),
        HttpError::AmbiguousLength,
      ),
      (
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n".to_owned(),
        HttpError::BadContentLength,
      ),
      (
        "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n".to_owned(),
        HttpError::BadContentLength,
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\n\r\n".to_owned(),
        HttpError::UnsupportedTransferEncoding,
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked, gzip\r\n\r\n".to_owned(),
        HttpError::UnsupportedTransferEncoding,
      ),
      ("HTTP/1.1 200 OK\r\n\r\n".to_owned(), HttpError::NoLength),
      (
        "HTTP/1.1 200 OK\r\nContent-Length: 65\r\n\r\n".to_owned(),
        HttpError::BodyTooLong { cap: 64 },
      ),
      (
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 33\r\n\r\n".to_owned(),
        HttpError::BodyTooLong { cap: 32 },
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n".to_owned(),
        HttpError::BadChunkSize,
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10000000000000000\r\n".to_owned(),
        HttpError::BadChunkSize,
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n20\r\n".to_owned()
          + &"x".repeat(32)
          + "\r\n21\r\n",
        HttpError::BodyTooLong { cap: 64 },
      ),
      (
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabXX".to_owned(),
        HttpError::BadChunkEnd,
      ),
      (long_trailer, HttpError::TrailerTooLong),
    ];
    for (text, refusal) in cases {
      assert_eq!(
        read(text.as_bytes(), false, 0),
        Err(refusal),
        "{}",
        text.get(..text.len().min(80)).unwrap_or_default()
      );
    }
  }

  /// A-471 (header injection): do write a request whose field value or target holds a CR, an LF or a NUL (an ETag
  /// decoded from a hostile listing could), and one whose head passes the cap. Expect each refused before a byte is
  /// written; and a well-formed head written in order.
  #[test]
  fn a_request_head_refuses_a_line_break_and_its_cap() {
    assert_eq!(
      request_head("GET", "/k", &[("If-Match", "\"e\"\r\nx-evil: 1")]),
      Err(HttpError::BadRequestField)
    );
    assert_eq!(
      request_head("GET", "/k\n", &[]),
      Err(HttpError::BadRequestField)
    );
    assert_eq!(
      request_head("GET", "/k", &[("x", "a\0b")]),
      Err(HttpError::BadRequestField)
    );
    let huge = "t".repeat(request_cap());
    assert_eq!(
      request_head("GET", "/k", &[("x-amz-security-token", &huge)]),
      Err(HttpError::RequestTooLarge)
    );
    assert_eq!(
      request_head("HEAD", "/a%20b", &[("Host", "h"), ("x-amz-date", "d")]).unwrap(),
      b"HEAD /a%20b HTTP/1.1\r\nHost: h\r\nx-amz-date: d\r\n\r\n"
    );
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// A-471 (hostile input): do feed arbitrary bytes, and a chunked answer cut short anywhere, in arbitrary pieces.
    /// Expect a response, a refusal or a wait for more, never a panic; and a cut answer never read as whole.
    #[test]
    fn arbitrary_bytes_and_cut_answers_never_panic_or_pass(
      bytes in proptest::collection::vec(any::<u8>(), 0..512),
      split in 1usize..32,
      cut in 0usize..60,
    ) {
      let _ = read(&bytes, false, split);
      let whole = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nabcd\r\n0\r\n\r\n";
      let cut = cut.min(whole.len() - 1);
      prop_assert!(read(&whole[..cut], false, split).is_err());
    }
  }
}
