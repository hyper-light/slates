//! A reply's head and its chunked body (RFC 9112 §4, §5, §6, §7.1; RFC 9110 §15). A head is the status line, the
//! caller's fields, the body's framing and, when the connection ends after the reply, `Connection: close`. A field
//! whose name is no token, or whose value holds a control byte, is refused before a byte is written: a value taken from
//! a request could otherwise end its field or the head early (response splitting, RFC 9112 §11.1). A chunked body is
//! its chunks, each its size in hexadecimal, CRLF, its data and CRLF, then [`LAST_CHUNK`]; a reply to an HTTP/1.0
//! request is never chunked (RFC 9112 §6.1: "A server MUST NOT send a response containing Transfer-Encoding unless the
//! corresponding request indicates HTTP/1.1"), and is framed by the connection's close instead ([`Body::UntilClose`]).

use crate::CRLF;
use crate::grammar::{is_field_byte, is_token};

/// A status code and its reason phrase (RFC 9110 §15; RFC 9112 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
  /// The three-digit code.
  pub code: u16,
  /// The reason phrase, as RFC 9110 names it.
  pub reason: &'static str,
}

impl Status {
  /// Format: `100 Continue` (RFC 9110 §15.2.1), sent before a body a client waits to send (§10.1.1).
  pub const CONTINUE: Status = Status {
    code: 100,
    reason: "Continue",
  };
  /// Format: `200 OK` (RFC 9110 §15.3.1).
  pub const OK: Status = Status {
    code: 200,
    reason: "OK",
  };
  /// Format: `207 Multi-Status` (RFC 4918 §11.1), a WebDAV answer holding a status for each resource it names.
  pub const MULTI_STATUS: Status = Status {
    code: 207,
    reason: "Multi-Status",
  };
  /// Format: `400 Bad Request` (RFC 9110 §15.5.1).
  pub const BAD_REQUEST: Status = Status {
    code: 400,
    reason: "Bad Request",
  };
  /// Format: `401 Unauthorized` (RFC 9110 §15.5.2).
  pub const UNAUTHORIZED: Status = Status {
    code: 401,
    reason: "Unauthorized",
  };
  /// Format: `403 Forbidden` (RFC 9110 §15.5.4).
  pub const FORBIDDEN: Status = Status {
    code: 403,
    reason: "Forbidden",
  };
  /// Format: `404 Not Found` (RFC 9110 §15.5.5).
  pub const NOT_FOUND: Status = Status {
    code: 404,
    reason: "Not Found",
  };
  /// Format: `405 Method Not Allowed` (RFC 9110 §15.5.6), which carries `Allow`.
  pub const METHOD_NOT_ALLOWED: Status = Status {
    code: 405,
    reason: "Method Not Allowed",
  };
  /// Format: `411 Length Required` (RFC 9110 §15.5.12).
  pub const LENGTH_REQUIRED: Status = Status {
    code: 411,
    reason: "Length Required",
  };
  /// Format: `413 Content Too Large` (RFC 9110 §15.5.14).
  pub const CONTENT_TOO_LARGE: Status = Status {
    code: 413,
    reason: "Content Too Large",
  };
  /// Format: `414 URI Too Long` (RFC 9110 §15.5.15).
  pub const URI_TOO_LONG: Status = Status {
    code: 414,
    reason: "URI Too Long",
  };
  /// Format: `415 Unsupported Media Type` (RFC 9110 §15.5.16).
  pub const UNSUPPORTED_MEDIA_TYPE: Status = Status {
    code: 415,
    reason: "Unsupported Media Type",
  };
  /// Format: `417 Expectation Failed` (RFC 9110 §15.5.18).
  pub const EXPECTATION_FAILED: Status = Status {
    code: 417,
    reason: "Expectation Failed",
  };
  /// Format: `431 Request Header Fields Too Large` (RFC 6585 §5).
  pub const FIELDS_TOO_LARGE: Status = Status {
    code: 431,
    reason: "Request Header Fields Too Large",
  };
  /// Format: `500 Internal Server Error` (RFC 9110 §15.6.1), a service that failed before its reply began.
  pub const INTERNAL_SERVER_ERROR: Status = Status {
    code: 500,
    reason: "Internal Server Error",
  };
  /// Format: `501 Not Implemented` (RFC 9110 §15.6.2).
  pub const NOT_IMPLEMENTED: Status = Status {
    code: 501,
    reason: "Not Implemented",
  };
  /// Format: `503 Service Unavailable` (RFC 9110 §15.6.4), a connection past the edge's derived bound.
  pub const SERVICE_UNAVAILABLE: Status = Status {
    code: 503,
    reason: "Service Unavailable",
  };
  /// Format: `505 HTTP Version Not Supported` (RFC 9110 §15.6.6).
  pub const VERSION_NOT_SUPPORTED: Status = Status {
    code: 505,
    reason: "HTTP Version Not Supported",
  };
}

/// How a reply's body is framed (RFC 9112 §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Body {
  /// `Content-Length` bytes; `Length(0)` for a reply with no body.
  Length(u64),
  /// `Transfer-Encoding: chunked`: the body is [`chunk`]s and then [`LAST_CHUNK`].
  Chunked,
  /// No framing field: the body ends when the connection does (RFC 9112 §6.3 item 8), the framing an HTTP/1.0 client
  /// reads. `Connection: close` is written whatever the caller asked.
  UntilClose,
}

/// A field a reply could not carry: its name is no token, or its value holds a control byte other than HTAB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadField;

/// Format: the status line's version: HTTP/1.1, the highest this server conforms to (RFC 9110 §2.5: "A server SHOULD
/// send a response version equal to the highest version to which the server is conformant that has a major version less
/// than or equal to the one received in the request").
const STATUS_VERSION: &[u8] = b"HTTP/1.1 ";

/// Format: what separates a field's name from its value (RFC 9112 §5: `field-line = field-name ":" OWS field-value
/// OWS`, one space written).
const FIELD_SEPARATOR: &[u8] = b": ";

/// Format: the field naming a body's length (RFC 9110 §8.6).
const CONTENT_LENGTH: &[u8] = b"Content-Length";

/// Format: the field naming a body's transfer coding, and chunked's name (RFC 9112 §6.1, §7.1).
const TRANSFER_ENCODING: &[u8] = b"Transfer-Encoding";

/// Format: chunked's name.
const CHUNKED: &[u8] = b"chunked";

/// Format: the field and option that end the connection after a reply (RFC 9112 §9.6).
const CONNECTION: &[u8] = b"Connection";

/// Format: `close`.
const CLOSE: &[u8] = b"close";

/// Format: a chunked body's end: the last chunk, size zero, and an empty trailer section (RFC 9112 §7.1).
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";

/// Format: the interim reply that tells a client waiting with `Expect: 100-continue` to send its body (RFC 9110
/// §10.1.1, §15.2.1).
pub const CONTINUE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

/// Writes a reply's head to `out`: the status line, `fields` in order, the body's framing, `Connection: close` when
/// `close` (or when the body runs until the close), and the empty line. Refused, writing nothing, when a field could not
/// be carried.
pub fn head(
  status: Status,
  fields: &[(&[u8], &[u8])],
  body: Body,
  close: bool,
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  if !fields.iter().all(|(name, value)| carries(name, value)) {
    return Err(BadField);
  }
  status_line(status, out);
  for (name, value) in fields {
    field(name, value, out);
  }
  framing(body, close, out);
  Ok(())
}

/// Writes a reply's head with no fields of the caller's: the status line, the body's framing and `Connection: close`
/// when asked. It holds no field a reply cannot carry, so it is never refused: the head a caller falls back to when its
/// own fields were.
pub fn bare(status: Status, body: Body, close: bool, out: &mut Vec<u8>) {
  status_line(status, out);
  framing(body, close, out);
}

/// Writes the status line.
fn status_line(status: Status, out: &mut Vec<u8>) {
  out.extend_from_slice(STATUS_VERSION);
  out.extend_from_slice(status.code.to_string().as_bytes());
  out.push(b' ');
  out.extend_from_slice(status.reason.as_bytes());
  out.extend_from_slice(CRLF);
}

/// Writes the body's framing field, `Connection: close` when `close` or when the body runs until the close, and the
/// empty line that ends the head.
fn framing(body: Body, close: bool, out: &mut Vec<u8>) {
  match body {
    Body::Length(length) => field(CONTENT_LENGTH, length.to_string().as_bytes(), out),
    Body::Chunked => field(TRANSFER_ENCODING, CHUNKED, out),
    Body::UntilClose => {}
  }
  if close || body == Body::UntilClose {
    field(CONNECTION, CLOSE, out);
  }
  out.extend_from_slice(CRLF);
}

/// Whether a reply can carry the field `name: value`: a token for its name, and no control byte but HTAB in its value.
fn carries(name: &[u8], value: &[u8]) -> bool {
  is_token(name) && value.iter().all(|byte| is_field_byte(*byte))
}

/// Writes the field line `name: value`.
fn field(name: &[u8], value: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(name);
  out.extend_from_slice(FIELD_SEPARATOR);
  out.extend_from_slice(value);
  out.extend_from_slice(CRLF);
}

/// Writes the reply to a refused request: `status`, `fields`, no body, and `Connection: close`, since a refused
/// request's body is not read and the connection cannot carry another.
pub fn refusal(
  status: Status,
  fields: &[(&[u8], &[u8])],
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  head(status, fields, Body::Length(0), true, out)
}

/// Writes one chunk of a chunked body: its size in hexadecimal, CRLF, `data` and CRLF. Writes nothing for no data,
/// since a chunk of size zero would end the body: [`LAST_CHUNK`] does that.
pub fn chunk(data: &[u8], out: &mut Vec<u8>) {
  if data.is_empty() {
    return;
  }
  out.extend_from_slice(format!("{:x}", data.len()).as_bytes());
  out.extend_from_slice(CRLF);
  out.extend_from_slice(data);
  out.extend_from_slice(CRLF);
}

#[cfg(test)]
#[path = "tests/reply.rs"]
mod tests;
