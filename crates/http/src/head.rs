//! A request head (RFC 9112 §2 to §6; RFC 9110 §5, §8.4, §8.6, §10.1.1): the request line, the field lines, and what
//! they decide, read from the bytes a connection has delivered, resuming where the last read stopped.
//!
//! **Where it ends.** A head ends at its first empty line. A read scans only the bytes no earlier read scanned, line by
//! line, so a head that arrives a byte at a time costs one walk in all, and the reader holds nothing between reads but
//! three offsets. Empty lines before the request line are skipped (RFC 9112 §2.2: "a server that is expecting to receive
//! and parse a request-line SHOULD ignore at least one empty line (CRLF)").
//!
//! **What it holds.** The head's bytes stay in the caller's buffer: [`Head`] borrows them, and its one allocation is
//! the field index. A head may take no more of the input than its admission's room: one that has not ended there is
//! refused, `414` while its request line has not ended and `431` after, and a whole head is charged its bytes and its
//! field index before the index is built. Every line is held to [`MAX_LINE`], and a line still open past it is refused
//! without waiting for its end.
//!
//! **What it decides.** The framing (RFC 9112 §6.3): chunked when `Transfer-Encoding` ends in chunked, otherwise
//! `Content-Length`'s value, otherwise no body. The content coding: gzip, or none (RFC 9110 §8.4). Whether the
//! connection is kept (HTTP/1.1's default, HTTP/1.0's only on `keep-alive`, neither on `close`; RFC 9112 §9.3), and
//! whether the client waits for `100 Continue` (RFC 9110 §10.1.1). The authority: an absolute-form target's, which wins
//! over `Host` (RFC 9112 §3.2.2: "the origin server MUST ignore the received Host header field (if any)"), else
//! `Host`'s. A head's refusals are judged in one order whatever its fields' order: the grammar, `Host`, the length, the
//! framing, the content coding, the expectation.

use crate::reply::{self, Status};
use crate::{Admission, MAX_LINE};

/// A field line's name and value as received: the name in its own case, the value without its surrounding whitespace.
pub type Field<'a> = (&'a [u8], &'a [u8]);

/// The protocol version of a request (RFC 9112 §2.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
  /// HTTP/1.0: its connection closes after the request unless it asks to be kept, and it carries no transfer coding.
  Http10,
  /// HTTP/1.1, or a later minor version of 1, read as 1.1 (RFC 9110 §2.5: a recipient "SHOULD process the message as if
  /// it were in the highest minor version within that major version to which the recipient is conformant").
  Http11,
}

/// A request's target (RFC 9112 §3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target<'a> {
  /// The origin form, or the path and query of the absolute form: the path as sent, its escapes not decoded (`/` for an
  /// absolute URI with no path, RFC 9110 §4.2.3), and the query after the first `?` when there is one.
  Path {
    /// The path.
    path: &'a [u8],
    /// The query.
    query: Option<&'a [u8]>,
  },
  /// The asterisk form, `*`: an `OPTIONS` request about the server itself (RFC 9112 §3.2.4).
  Asterisk,
}

/// How a request's body is framed (RFC 9112 §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
  /// No body: neither `Content-Length` nor `Transfer-Encoding` (§6.3 item 6).
  None,
  /// `Content-Length` bytes.
  Length(u64),
  /// `Transfer-Encoding` ending in chunked (§7.1).
  Chunked,
}

/// The content coding of a request's body (RFC 9110 §8.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coding {
  /// As sent: no `Content-Encoding`, or `identity`.
  Identity,
  /// gzip (RFC 1952), named `gzip` or `x-gzip` (RFC 9110 §8.4.1.3: "A recipient SHOULD consider "x-gzip" to be
  /// equivalent to "gzip"").
  Gzip,
}

/// A request head, borrowing the bytes it was read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head<'a> {
  /// The method, a token, compared as sent: methods are case-sensitive (RFC 9110 §9.1).
  pub method: &'a [u8],
  /// The target.
  pub target: Target<'a>,
  /// The version.
  pub version: Version,
  /// The authority the request names: an absolute-form target's, else `Host`'s (absent from an HTTP/1.0 request that
  /// sent none).
  pub authority: Option<&'a [u8]>,
  /// Every field line, in the order received.
  pub fields: Vec<Field<'a>>,
  /// How the body is framed.
  pub framing: Framing,
  /// The body's content coding.
  pub coding: Coding,
  /// Whether the connection is kept after this request.
  pub keep_alive: bool,
  /// Whether the client waits for `100 Continue` before it sends the body (an HTTP/1.1 request's
  /// `Expect: 100-continue`; RFC 9110 §10.1.1: "A server that receives a 100-continue expectation in an HTTP/1.0
  /// request MUST ignore that expectation").
  pub expect_continue: bool,
}

impl<'a> Head<'a> {
  /// The value of the field `name`, its case ignored, when at most one line carries it. Refused when several do: a
  /// field defined to hold one value is ambiguous given twice (RFC 9110 §5.3).
  pub fn field(&self, name: &[u8]) -> Result<Option<&'a [u8]>, HeadRefused> {
    let _ = name;
    Ok(None)
  }

  /// Every value of the field `name`, its case ignored, in the order received.
  pub fn fields_named<'s>(&'s self, name: &'s [u8]) -> impl Iterator<Item = &'a [u8]> + 's {
    self
      .fields
      .iter()
      .filter(move |(known, _)| known.is_empty() && name.is_empty())
      .map(|(_, value)| *value)
  }
}

/// A media type's essence: `value` without its parameters and surrounding whitespace (RFC 9110 §8.3.1: `media-type =
/// type "/" subtype parameters`), to be compared without regard to case ("The type and subtype tokens are
/// case-insensitive").
pub fn media_type(value: &[u8]) -> &[u8] {
  value
}

/// What a read of the bytes so far found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Progress<T> {
  /// Not whole yet: more bytes are needed, and nothing was charged.
  Partial,
  /// Whole.
  Complete {
    /// What was read.
    value: T,
    /// The input bytes it took; what follows them is the body, or the next request.
    consumed: usize,
  },
}

/// Why a head is refused. Each kind is answered with its own status ([`HeadRefused::status`]), and every one ends the
/// connection, since a head that was not read cannot say where the next request starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeadRefused {
  /// `400 Bad Request`: the head breaks HTTP/1.1's grammar or one of its framing rules; the fault names which.
  BadRequest(Malformed),
  /// `413 Content Too Large`: a `Content-Length` past what 64 bits hold.
  ContentTooLarge,
  /// `414 URI Too Long`: a request line past [`MAX_LINE`], or one the request's room could not hold.
  UriTooLong,
  /// `415 Unsupported Media Type`: a content coding other than gzip, or gzip twice; answered with
  /// `Accept-Encoding: gzip` (RFC 9110 §15.5.16: "If the problem was caused by an unsupported content coding, the
  /// Accept-Encoding response header field (Section 12.5.3) ought to be used to indicate which (if any) content codings
  /// would have been accepted in the request").
  UnsupportedCoding,
  /// `417 Expectation Failed`: an expectation other than `100-continue` (RFC 9110 §10.1.1: "A server that receives an
  /// Expect field value containing a member other than 100-continue MAY respond with a 417 (Expectation Failed)").
  ExpectationFailed,
  /// `431 Request Header Fields Too Large`: a field line past [`MAX_LINE`], or fields the request's room could not hold
  /// with their index.
  FieldsTooLarge,
  /// `501 Not Implemented`: a transfer coding other than chunked (RFC 9112 §6.1: "A server that receives a request
  /// message with a transfer coding it does not understand SHOULD respond with 501 (Not Implemented)"), or `CONNECT`,
  /// which no server here tunnels.
  NotImplemented,
  /// `505 HTTP Version Not Supported`: a major version other than 1.
  VersionNotSupported,
}

/// What in a head broke HTTP/1.1's grammar or its framing rules; each is answered `400 Bad Request`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
  /// The request line is not `method SP target SP version` (RFC 9112 §3).
  RequestLine,
  /// The method is not a token (RFC 9110 §9.1).
  Method,
  /// The target is in no form a server reads (origin; absolute, `http` or `https` with no userinfo; or, for `OPTIONS`,
  /// asterisk; RFC 9112 §3.2), or holds a byte a URI cannot (RFC 3986 §2).
  Target,
  /// The version is not `HTTP/` digit `.` digit (RFC 9112 §2.3).
  Version,
  /// A CR or an LF that does not end a line (RFC 9112 §2.2: "A recipient of such a bare CR MUST consider that element
  /// to be invalid or replace each bare CR with SP").
  LineBreak,
  /// A field line with no colon, or a name that is not a token or is followed by whitespace (RFC 9112 §5.1: "No
  /// whitespace is allowed between the field name and colon").
  FieldName,
  /// A field value holding a control byte other than HTAB (RFC 9110 §5.5).
  FieldValue,
  /// A field line folded onto the one before it (RFC 9112 §5.2: a server that receives an obs-fold "MUST either reject
  /// the message by sending a 400 (Bad Request)" or replace it).
  Folded,
  /// An HTTP/1.1 request with no `Host` (RFC 9112 §3.2: "A server MUST respond with a 400 (Bad Request) status code to
  /// any HTTP/1.1 request message that lacks a Host header field").
  NoHost,
  /// More than one `Host` line (§3.2, the same sentence's "and to any request message that contains more than one Host
  /// header field line").
  RepeatedHost,
  /// A `Host` value that is no authority, or one with a userinfo (§3.2; RFC 3986 §3.2).
  Host,
  /// A field asked for as one value that was given more than once (RFC 9110 §5.3).
  RepeatedField,
  /// A `Content-Length` that is not digits (RFC 9110 §8.6: `Content-Length = 1*DIGIT`), or whose values disagree
  /// (RFC 9112 §6.3 item 5: "the message framing is invalid and the recipient MUST treat it as an unrecoverable error").
  ContentLength,
  /// Both `Content-Length` and `Transfer-Encoding` (RFC 9112 §6.1: "A server MAY reject a request that contains both
  /// Content-Length and Transfer-Encoding").
  LengthAndTransferCoding,
  /// `Transfer-Encoding` in an HTTP/1.0 request, which a recipient "MUST treat ... as if the framing is faulty, even if
  /// a Content-Length is present" (RFC 9112 §6.1).
  TransferCodingInHttp10,
  /// A transfer-coding list whose last coding is not chunked (RFC 9112 §6.3 item 4: "the message body length cannot be
  /// determined reliably; the server MUST respond with the 400 (Bad Request) status code").
  ChunkedNotLast,
  /// chunked more than once (RFC 9112 §6.1: "A sender MUST NOT apply the chunked transfer coding more than once to a
  /// message body").
  ChunkedTwice,
}

impl HeadRefused {
  /// The status the refusal is answered with.
  pub fn status(self) -> Status {
    Status::OK
  }

  /// Writes the reply to the refusal: its status, no body, the connection closed; a refused content coding names the one
  /// this server decodes.
  pub fn reply(self, out: &mut Vec<u8>) -> Result<(), reply::BadField> {
    let _ = out;
    Ok(())
  }
}

/// Reads a request head from the bytes a connection delivers, resuming where its last read stopped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeadReader {
  /// How far the input has been scanned for line ends.
  scanned: usize,
  /// Where the line being scanned starts.
  line_start: usize,
  /// Where the request line starts, once a line that is not empty has ended.
  request_line: Option<usize>,
}

impl HeadReader {
  /// A reader of a new head.
  pub fn new() -> Self {
    Self::default()
  }

  /// Reads the head at the front of `input`: the bytes delivered for this request so far, the same front at every call
  /// and longer at each. [`Progress::Partial`] while the head has not ended, charging nothing; once it has, the head and
  /// the bytes it took, its bytes and its field index charged to `admission`. The reader is then ready for the next
  /// request, whose bytes start after those.
  pub fn read<'a>(
    &mut self,
    input: &'a [u8],
    admission: &mut Admission,
  ) -> Result<Progress<Head<'a>>, HeadRefused> {
    let _ = (input, admission, MAX_LINE, self.scanned, self.line_start, self.request_line);
    Ok(Progress::Partial)
  }
}

#[cfg(test)]
#[path = "tests/head.rs"]
mod tests;
