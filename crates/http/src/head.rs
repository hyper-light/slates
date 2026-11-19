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

use crate::grammar::{
  is_authority, is_field_byte, is_ows, is_path, is_query, is_token, list_elements, trim_ows,
};
use crate::reply::{self, Status};
use crate::{Admission, CRLF, MAX_LINE, to_u64};

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
    let mut values = self.fields_named(name);
    let first = values.next();
    match values.next() {
      Some(_) => Err(HeadRefused::BadRequest(Malformed::RepeatedField)),
      None => Ok(first),
    }
  }

  /// Every value of the field `name`, its case ignored, in the order received.
  pub fn fields_named<'s>(&'s self, name: &'s [u8]) -> impl Iterator<Item = &'a [u8]> + 's {
    self
      .fields
      .iter()
      .filter(move |(known, _)| known.eq_ignore_ascii_case(name))
      .map(|(_, value)| *value)
  }
}

/// A media type's essence: `value` without its parameters and surrounding whitespace (RFC 9110 §8.3.1: `media-type =
/// type "/" subtype parameters`), to be compared without regard to case ("The type and subtype tokens are
/// case-insensitive").
pub fn media_type(value: &[u8]) -> &[u8] {
  trim_ows(value.split(|byte| *byte == b';').next().unwrap_or_default())
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
    match self {
      HeadRefused::BadRequest(_) => Status::BAD_REQUEST,
      HeadRefused::ContentTooLarge => Status::CONTENT_TOO_LARGE,
      HeadRefused::UriTooLong => Status::URI_TOO_LONG,
      HeadRefused::UnsupportedCoding => Status::UNSUPPORTED_MEDIA_TYPE,
      HeadRefused::ExpectationFailed => Status::EXPECTATION_FAILED,
      HeadRefused::FieldsTooLarge => Status::FIELDS_TOO_LARGE,
      HeadRefused::NotImplemented => Status::NOT_IMPLEMENTED,
      HeadRefused::VersionNotSupported => Status::VERSION_NOT_SUPPORTED,
    }
  }

  /// Writes the reply to the refusal: its status, no body, the connection closed; a refused content coding names the one
  /// this server decodes.
  pub fn reply(self, out: &mut Vec<u8>) -> Result<(), reply::BadField> {
    let fields: &[(&[u8], &[u8])] = if self == HeadRefused::UnsupportedCoding {
      &[ACCEPT_GZIP]
    } else {
      &[]
    };
    reply::refusal(self.status(), fields, out)
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
    let room = usize::try_from(admission.left()).unwrap_or(usize::MAX);
    let window = input.get(..room).unwrap_or(input);
    let Some((start, empty_line)) = self.scan(window)? else {
      return unended(window.len(), room, self.request_line.is_some());
    };
    *self = Self::default();
    let consumed = empty_line.saturating_add(CRLF.len());
    let text = window
      .get(start..empty_line.saturating_sub(CRLF.len()))
      .unwrap_or_default();
    let head = parse(text, consumed, admission)?;
    Ok(Progress::Complete {
      value: head,
      consumed,
    })
  }

  /// Scans `window` line by line from where the last read stopped: where the request line starts and where the empty
  /// line that ends the head does, once it has come; `None` while it has not.
  fn scan(&mut self, window: &[u8]) -> Result<Option<(usize, usize)>, HeadRefused> {
    while let Some(line_end) = next_line_end(window, self.scanned) {
      self.scanned = line_end.saturating_add(CRLF.len());
      if let Some(found) = self.end_line(line_end)? {
        return Ok(Some(found));
      }
    }
    // The last byte may be a CR whose LF has not come: the next read looks at it again.
    self.scanned = self.scanned.max(window.len().saturating_sub(1));
    self.check_open_line(window.len())?;
    Ok(None)
  }

  /// A line has ended at `line_end`: an empty line ends the head once the request line has come, and is skipped before
  /// it; the first other line is the request line; a line past [`MAX_LINE`] is refused.
  fn end_line(&mut self, line_end: usize) -> Result<Option<(usize, usize)>, HeadRefused> {
    let start = self.line_start;
    let length = line_end.saturating_sub(start);
    self.line_start = line_end.saturating_add(CRLF.len());
    if length > MAX_LINE {
      return Err(too_long(self.request_line.is_none()));
    }
    match (length, self.request_line) {
      (0, Some(request_line)) => Ok(Some((request_line, line_end))),
      (0, None) | (_, Some(_)) => Ok(None),
      (_, None) => {
        self.request_line = Some(start);
        Ok(None)
      }
    }
  }

  /// Refuses the line still open at the window's end once it is past [`MAX_LINE`] (one byte more allowed: its CR may
  /// have come without its LF), without waiting for it to end.
  fn check_open_line(&self, window_len: usize) -> Result<(), HeadRefused> {
    let open = window_len.saturating_sub(self.line_start);
    if open > MAX_LINE.saturating_add(1) {
      Err(too_long(self.request_line.is_none()))
    } else {
      Ok(())
    }
  }
}

/// Format: the field a refused content coding is answered with: gzip, the one coding read here (RFC 9110 §15.5.16).
const ACCEPT_GZIP: (&[u8], &[u8]) = (b"Accept-Encoding", b"gzip");

/// Format: the protocol's name before its version (RFC 9112 §2.3: `HTTP-version = HTTP-name "/" DIGIT "." DIGIT`).
const HTTP_NAME: &[u8] = b"HTTP/";

/// Format: the method whose target is an authority, which no server here tunnels (RFC 9110 §9.3.6).
const CONNECT: &[u8] = b"CONNECT";

/// Format: the method the asterisk form may name (RFC 9112 §3.2.4).
const OPTIONS: &[u8] = b"OPTIONS";

/// Format: the asterisk form.
const ASTERISK: &[u8] = b"*";

/// Format: the path an absolute URI with no path names (RFC 9110 §4.2.3).
const ROOT: &[u8] = b"/";

/// Format: the schemes whose absolute-form targets a server here reads (RFC 9110 §4.2), compared without regard to
/// case (RFC 3986 §3.1).
const SCHEMES: [&[u8]; 2] = [b"http://", b"https://"];

/// Format: decimal's radix, a `Content-Length`'s (RFC 9110 §8.6: `1*DIGIT`).
const DECIMAL: u64 = 10;

/// Format: the fields a head's decisions read (RFC 9110 §8.6, §8.4, §7.6.1, §10.1.1, §7.2; RFC 9112 §6.1), compared
/// without regard to case.
const CONTENT_LENGTH: &[u8] = b"content-length";
/// Format: as [`CONTENT_LENGTH`].
const TRANSFER_ENCODING: &[u8] = b"transfer-encoding";
/// Format: as [`CONTENT_LENGTH`].
const CONTENT_ENCODING: &[u8] = b"content-encoding";
/// Format: as [`CONTENT_LENGTH`].
const CONNECTION: &[u8] = b"connection";
/// Format: as [`CONTENT_LENGTH`].
const EXPECT: &[u8] = b"expect";
/// Format: as [`CONTENT_LENGTH`].
const HOST: &[u8] = b"host";

/// Format: the values those fields are read for: chunked (RFC 9112 §7.1), gzip's two names (RFC 9110 §8.4.1.3),
/// `identity`, the connection options (RFC 9112 §9.3, §9.6) and the one expectation (RFC 9110 §10.1.1).
const CHUNKED: &[u8] = b"chunked";
/// Format: as [`CHUNKED`].
const GZIP_NAMES: [&[u8]; 2] = [b"gzip", b"x-gzip"];
/// Format: as [`CHUNKED`].
const IDENTITY: &[u8] = b"identity";
/// Format: as [`CHUNKED`].
const CLOSE: &[u8] = b"close";
/// Format: as [`CHUNKED`].
const KEEP_ALIVE: &[u8] = b"keep-alive";
/// Format: as [`CHUNKED`].
const CONTINUE: &[u8] = b"100-continue";

/// Where the next CRLF at or after `from` starts in `window`.
fn next_line_end(window: &[u8], from: usize) -> Option<usize> {
  window
    .get(from..)?
    .windows(CRLF.len())
    .position(|pair| pair == CRLF)
    .map(|found| from.saturating_add(found))
}

/// The refusal of a line past its bound: `414` for the request line, `431` for a field line.
fn too_long(request_line: bool) -> HeadRefused {
  if request_line {
    HeadRefused::UriTooLong
  } else {
    HeadRefused::FieldsTooLarge
  }
}

/// A head that has not ended in its window: partial while the input was shorter than the room, refused once it filled
/// the room.
fn unended<T>(
  window_len: usize,
  room: usize,
  request_line_ended: bool,
) -> Result<Progress<T>, HeadRefused> {
  if window_len < room {
    Ok(Progress::Partial)
  } else {
    Err(too_long(!request_line_ended))
  }
}

/// Builds the head from its text, the request line and the field lines without the empty line, after checking every
/// line and charging `consumed` (the input bytes the head took) and the field index to `admission`.
fn parse<'a>(
  text: &'a [u8],
  consumed: usize,
  admission: &mut Admission,
) -> Result<Head<'a>, HeadRefused> {
  let mut all = lines(text);
  let first = all
    .next()
    .unwrap_or(Err(HeadRefused::BadRequest(Malformed::RequestLine)))?;
  let line = request_line(first)?;
  let count = lines(text).skip(1).try_fold(0usize, |count, line| {
    field_line(line?).map(|_| count.saturating_add(1))
  })?;
  charge(admission, consumed, count)?;
  let mut fields = Vec::with_capacity(count);
  for line in all {
    fields.push(field_line(line?)?);
  }
  Decisions::read(&fields).head(line, fields)
}

/// The lines of a head's text, each without its CRLF; a CR or an LF that ends no line is refused.
fn lines(text: &[u8]) -> impl Iterator<Item = Result<&[u8], HeadRefused>> {
  let mut pieces = text.split(|byte| *byte == b'\n').peekable();
  core::iter::from_fn(move || {
    let piece = pieces.next()?;
    let line = if pieces.peek().is_some() {
      piece.strip_suffix(b"\r")
    } else {
      Some(piece)
    };
    Some(
      line
        .filter(|line| !line.contains(&b'\r'))
        .ok_or(HeadRefused::BadRequest(Malformed::LineBreak)),
    )
  })
}

/// Charges a head's bytes and its field index.
fn charge(admission: &mut Admission, consumed: usize, fields: usize) -> Result<(), HeadRefused> {
  let index = to_u64(core::mem::size_of::<Field<'_>>()).saturating_mul(to_u64(fields));
  admission
    .charge(to_u64(consumed).saturating_add(index))
    .map_err(|_| HeadRefused::FieldsTooLarge)
}

/// A request line's parts.
struct RequestLine<'a> {
  /// The method.
  method: &'a [u8],
  /// The target.
  target: Target<'a>,
  /// An absolute-form target's authority.
  authority: Option<&'a [u8]>,
  /// The version.
  version: Version,
}

/// Reads a request line (RFC 9112 §3): three parts, one space between each.
fn request_line(line: &[u8]) -> Result<RequestLine<'_>, HeadRefused> {
  let mut parts = line.split(|byte| *byte == b' ');
  let (Some(method), Some(target), Some(version), None) =
    (parts.next(), parts.next(), parts.next(), parts.next())
  else {
    return Err(HeadRefused::BadRequest(Malformed::RequestLine));
  };
  if target.is_empty() {
    return Err(HeadRefused::BadRequest(Malformed::RequestLine));
  }
  if !is_token(method) {
    return Err(HeadRefused::BadRequest(Malformed::Method));
  }
  let version = http_version(version)?;
  if method == CONNECT {
    return Err(HeadRefused::NotImplemented);
  }
  let (target, authority) = request_target(method, target)?;
  Ok(RequestLine {
    method,
    target,
    authority,
    version,
  })
}

/// Reads `HTTP/` digit `.` digit: 1.0, 1.1 or a later minor version of 1, and refuses another major version.
fn http_version(text: &[u8]) -> Result<Version, HeadRefused> {
  match text.strip_prefix(HTTP_NAME) {
    Some([b'1', b'.', b'0']) => Ok(Version::Http10),
    Some([b'1', b'.', minor]) if minor.is_ascii_digit() => Ok(Version::Http11),
    Some([major, b'.', minor]) if major.is_ascii_digit() && minor.is_ascii_digit() => {
      Err(HeadRefused::VersionNotSupported)
    }
    _ => Err(HeadRefused::BadRequest(Malformed::Version)),
  }
}

/// Reads a target in the origin, absolute or asterisk form (RFC 9112 §3.2), and the authority the absolute form names.
fn request_target<'a>(
  method: &[u8],
  raw: &'a [u8],
) -> Result<(Target<'a>, Option<&'a [u8]>), HeadRefused> {
  if raw == ASTERISK {
    return if method == OPTIONS {
      Ok((Target::Asterisk, None))
    } else {
      Err(HeadRefused::BadRequest(Malformed::Target))
    };
  }
  if raw.first() == Some(&b'/') {
    return origin_form(raw).map(|target| (target, None));
  }
  absolute_form(raw)
}

/// Reads the origin form: a path of URI characters from its `/`, and a query after the first `?`.
fn origin_form(raw: &[u8]) -> Result<Target<'_>, HeadRefused> {
  let (path, query) = match raw.iter().position(|byte| *byte == b'?') {
    Some(at) => (
      raw.get(..at).unwrap_or_default(),
      raw.get(at.saturating_add(1)..),
    ),
    None => (raw, None),
  };
  if is_path(path) && query.is_none_or(is_query) {
    Ok(Target::Path { path, query })
  } else {
    Err(HeadRefused::BadRequest(Malformed::Target))
  }
}

/// Reads the absolute form: `http` or `https`, an authority with no userinfo, then a path or a query or neither.
fn absolute_form(raw: &[u8]) -> Result<(Target<'_>, Option<&[u8]>), HeadRefused> {
  let rest = SCHEMES
    .iter()
    .find_map(|scheme| {
      raw
        .get(..scheme.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(scheme))
        .and_then(|_| raw.get(scheme.len()..))
    })
    .ok_or(HeadRefused::BadRequest(Malformed::Target))?;
  let end = rest
    .iter()
    .position(|byte| *byte == b'/' || *byte == b'?')
    .unwrap_or(rest.len());
  let authority = rest.get(..end).unwrap_or_default();
  let after = rest.get(end..).unwrap_or_default();
  if authority.is_empty() || !is_authority(authority) {
    return Err(HeadRefused::BadRequest(Malformed::Target));
  }
  let target = if after.first() == Some(&b'/') {
    origin_form(after)?
  } else {
    root_with_query(after)?
  };
  Ok((target, Some(authority)))
}

/// The target of an absolute URI with no path: `/`, and the query `after` holds (nothing, or `?` and the query).
fn root_with_query(after: &[u8]) -> Result<Target<'_>, HeadRefused> {
  let query = after.strip_prefix(b"?");
  if after.is_empty() || query.is_some_and(is_query) {
    Ok(Target::Path { path: ROOT, query })
  } else {
    Err(HeadRefused::BadRequest(Malformed::Target))
  }
}

/// A field line's name and value (RFC 9112 §5): not folded, a token before its colon, a value of field bytes.
fn field_line(line: &[u8]) -> Result<Field<'_>, HeadRefused> {
  if line.first().is_some_and(|byte| is_ows(*byte)) {
    return Err(HeadRefused::BadRequest(Malformed::Folded));
  }
  let colon = line
    .iter()
    .position(|byte| *byte == b':')
    .ok_or(HeadRefused::BadRequest(Malformed::FieldName))?;
  let name = line.get(..colon).unwrap_or_default();
  let value = trim_ows(line.get(colon.saturating_add(1)..).unwrap_or_default());
  if !is_token(name) {
    return Err(HeadRefused::BadRequest(Malformed::FieldName));
  }
  if !value.iter().all(|byte| is_field_byte(*byte)) {
    return Err(HeadRefused::BadRequest(Malformed::FieldValue));
  }
  Ok((name, value))
}

/// What a head's fields decide, gathered from every field before any is judged, so the judgement's order is fixed.
#[derive(Default)]
struct Decisions<'a> {
  /// The `Content-Length` every value agreed on.
  length: Option<u64>,
  /// The first fault of a `Content-Length` (a malformed value before one past 64 bits).
  length_fault: Option<HeadRefused>,
  /// The transfer codings.
  transfer: Transfer,
  /// How many times gzip was named as a content coding.
  gzips: u32,
  /// Whether a content coding other than gzip and identity was named.
  unknown_coding: bool,
  /// Whether `Connection` named `close`.
  close: bool,
  /// Whether `Connection` named `keep-alive`.
  keep: bool,
  /// Whether `Expect` named `100-continue`.
  expect_continue: bool,
  /// Whether `Expect` named anything else.
  unmet_expectation: bool,
  /// How many `Host` lines came.
  hosts: u32,
  /// The last `Host` value.
  host: Option<&'a [u8]>,
}

/// The transfer codings of a head's `Transfer-Encoding` lines, read as one list.
#[derive(Default)]
struct Transfer {
  /// Whether any `Transfer-Encoding` line came.
  present: bool,
  /// How many times chunked was named.
  chunked: u32,
  /// Whether the last coding named is chunked.
  last_chunked: bool,
  /// Whether a coding other than chunked was named.
  unknown: bool,
}

impl<'a> Decisions<'a> {
  /// Gathers what `fields` decide.
  fn read(fields: &[Field<'a>]) -> Self {
    let mut decisions = Self::default();
    for &(name, value) in fields {
      decisions.field(name, value);
    }
    decisions
  }

  /// Gathers what one field decides; fields that decide nothing here are left to the router.
  fn field(&mut self, name: &[u8], value: &'a [u8]) {
    if name.eq_ignore_ascii_case(CONTENT_LENGTH) {
      self.length(value);
    } else if name.eq_ignore_ascii_case(TRANSFER_ENCODING) {
      self.transfer.read(value);
    } else if name.eq_ignore_ascii_case(CONTENT_ENCODING) {
      self.coding(value);
    } else if name.eq_ignore_ascii_case(CONNECTION) {
      self.connection(value);
    } else if name.eq_ignore_ascii_case(EXPECT) {
      self.expect(value);
    } else if name.eq_ignore_ascii_case(HOST) {
      self.hosts = self.hosts.saturating_add(1);
      self.host = Some(value);
    }
  }

  /// Reads a `Content-Length` line: a list of decimal values that agree with each other and with every earlier line.
  fn length(&mut self, value: &[u8]) {
    let mut any = false;
    for element in list_elements(value) {
      any = true;
      match decimal(element) {
        Ok(length) if self.length.is_none_or(|known| known == length) => self.length = Some(length),
        Ok(_) => self.fault(HeadRefused::BadRequest(Malformed::ContentLength)),
        Err(refusal) => self.fault(refusal),
      }
    }
    if !any {
      self.fault(HeadRefused::BadRequest(Malformed::ContentLength));
    }
  }

  /// Keeps a `Content-Length` fault: a malformed value's before one past 64 bits, whatever their order.
  fn fault(&mut self, refusal: HeadRefused) {
    if refusal != HeadRefused::ContentTooLarge || self.length_fault.is_none() {
      self.length_fault = Some(refusal);
    }
  }

  /// Reads a `Content-Encoding` line's codings.
  fn coding(&mut self, value: &[u8]) {
    for coding in list_elements(value) {
      if GZIP_NAMES
        .iter()
        .any(|name| coding.eq_ignore_ascii_case(name))
      {
        self.gzips = self.gzips.saturating_add(1);
      } else if !coding.eq_ignore_ascii_case(IDENTITY) {
        self.unknown_coding = true;
      }
    }
  }

  /// Reads a `Connection` line's options.
  fn connection(&mut self, value: &[u8]) {
    for option in list_elements(value) {
      self.close |= option.eq_ignore_ascii_case(CLOSE);
      self.keep |= option.eq_ignore_ascii_case(KEEP_ALIVE);
    }
  }

  /// Reads an `Expect` line's expectations.
  fn expect(&mut self, value: &[u8]) {
    for member in list_elements(value) {
      if member.eq_ignore_ascii_case(CONTINUE) {
        self.expect_continue = true;
      } else {
        self.unmet_expectation = true;
      }
    }
  }

  /// Judges what was gathered, in one order: `Host`, the length, the framing, the content coding, the expectation.
  fn head(self, line: RequestLine<'a>, fields: Vec<Field<'a>>) -> Result<Head<'a>, HeadRefused> {
    self.check_host(line.version)?;
    if let Some(refusal) = self.length_fault {
      return Err(refusal);
    }
    let framing = self.transfer.framing(self.length, line.version)?;
    let coding = match (self.unknown_coding, self.gzips) {
      (false, 0) => Coding::Identity,
      (false, 1) => Coding::Gzip,
      _ => return Err(HeadRefused::UnsupportedCoding),
    };
    if self.unmet_expectation {
      return Err(HeadRefused::ExpectationFailed);
    }
    Ok(Head {
      method: line.method,
      target: line.target,
      version: line.version,
      authority: line.authority.or(self.host),
      fields,
      framing,
      coding,
      keep_alive: !self.close && (line.version == Version::Http11 || self.keep),
      expect_continue: self.expect_continue && line.version == Version::Http11,
    })
  }

  /// `Host`: once in an HTTP/1.1 request, at most once in an HTTP/1.0 one, and an authority.
  fn check_host(&self, version: Version) -> Result<(), HeadRefused> {
    match (self.hosts, version) {
      (0, Version::Http11) => Err(HeadRefused::BadRequest(Malformed::NoHost)),
      (0 | 1, _) if self.host.is_none_or(is_authority) => Ok(()),
      (0 | 1, _) => Err(HeadRefused::BadRequest(Malformed::Host)),
      _ => Err(HeadRefused::BadRequest(Malformed::RepeatedHost)),
    }
  }
}

impl Transfer {
  /// Reads a `Transfer-Encoding` line's codings, after any earlier line's.
  fn read(&mut self, value: &[u8]) {
    self.present = true;
    for coding in list_elements(value) {
      let chunked = coding.eq_ignore_ascii_case(CHUNKED);
      self.chunked = self.chunked.saturating_add(u32::from(chunked));
      self.unknown |= !chunked;
      self.last_chunked = chunked;
    }
  }

  /// The framing (RFC 9112 §6.3), every reading two recipients could disagree on refused.
  fn framing(&self, length: Option<u64>, version: Version) -> Result<Framing, HeadRefused> {
    if !self.present {
      return Ok(length.map_or(Framing::None, Framing::Length));
    }
    if length.is_some() {
      return Err(HeadRefused::BadRequest(Malformed::LengthAndTransferCoding));
    }
    if version == Version::Http10 {
      return Err(HeadRefused::BadRequest(Malformed::TransferCodingInHttp10));
    }
    if !self.last_chunked {
      return Err(HeadRefused::BadRequest(Malformed::ChunkedNotLast));
    }
    if self.unknown {
      return Err(HeadRefused::NotImplemented);
    }
    if self.chunked > 1 {
      return Err(HeadRefused::BadRequest(Malformed::ChunkedTwice));
    }
    Ok(Framing::Chunked)
  }
}

/// A `Content-Length` value: decimal digits within 64 bits.
fn decimal(digits: &[u8]) -> Result<u64, HeadRefused> {
  if !digits.iter().all(u8::is_ascii_digit) {
    return Err(HeadRefused::BadRequest(Malformed::ContentLength));
  }
  digits
    .iter()
    .try_fold(0u64, |value, digit| {
      value
        .checked_mul(DECIMAL)
        .and_then(|value| value.checked_add(u64::from(digit.wrapping_sub(b'0'))))
    })
    .ok_or(HeadRefused::ContentTooLarge)
}

#[cfg(test)]
#[path = "tests/head.rs"]
mod tests;
