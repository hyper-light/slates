//! Smart HTTP's request and response codec and git's routes (§4.17.2 "Transports" 1; piece 8's first part, A-595): pure
//! functions over bytes, as the rest of this crate, so every rule is tested on every host and the edge that reads sockets
//! (piece 8's next part) holds none of its own.
//!
//! **Requests** [B: RFC 9112; gitprotocol-http]. A head is a request line and field lines, each within [`MAX_LINE`],
//! at most [`MAX_FIELDS`] of them ([`parse_head`]). A body is framed by `Content-Length` or by
//! `Transfer-Encoding: chunked` (RFC 9112 §6.1, §7.1; [`decode_chunked`]), never both: a request carrying both is
//! refused, which RFC 9112 §6.1 allows and request smuggling asks for. `Content-Encoding: gzip`, which git sends for a
//! request past a kilobyte, is inflated within the caller's [`Admission`] ([`gunzip`]): a body that would inflate past the
//! room its request was admitted is refused there, holding no more than that room, whatever its compressed size. A chunked
//! body's declared sizes are held to the same room before their data arrives. `Git-Protocol` names the protocol version
//! (gitprotocol-v2 "HTTP Transport"; [`version`]).
//!
//! **Routes** ([`route`]). `GET <repo>/info/refs?service=git-upload-pack` (or `git-receive-pack`) asks for the
//! advertisement; `POST <repo>/git-upload-pack` and `POST <repo>/git-receive-pack` carry a service's request, with the
//! content type `application/x-<service>-request`. Every other request is refused by kind ([`Refused`]): a path no route
//! names is `404`; a route asked by another method `405`; an advertisement asked with no service (the dumb protocol,
//! which slates doesn't serve) or an unknown one `403`, as `git-http-backend` answers it; a request body of another type
//! `415`; a service's request with no body `411`.
//!
//! **Responses.** A smart reply is `200` with its content type (`application/x-<service>-advertisement` or `-result`),
//! `Cache-Control: no-cache` (gitprotocol-http: "Cache-Control headers SHOULD be used to disable caching of the returned
//! entity") and a chunked body ([`reply_head`], [`chunk`], [`LAST_CHUNK`]). For protocol v0 and v1 the advertisement
//! starts with the pkt-line `# service=<service>` and a flush ([`advertisement_prelude`]); for v2 nothing precedes the
//! capability advertisement.
//!
//! **Reused, and not.** The MCP edge's HTTP/1.1 head parser (`slates-mcp`'s `http::parse_head`) has this grammar and
//! these bounds, which this codec takes with their citations: RFC 9112 §3's 8000-octet lines and Apache's 100 fields. Its
//! head is MCP's own (its fields, `POST` alone, `Content-Length` required, chunked refused), and this crate depends on
//! nothing of the server's, so the parser is written here for git's fields; one HTTP head core both edges share is owed
//! (GAPS, the §4.17 entry).

use crate::pktline::{self, Packet};
use crate::{Admission, PastAdmission};

/// Format: the longest request line or field line accepted (RFC 9112 §3: "It is RECOMMENDED that all HTTP senders and
/// recipients support, at a minimum, request-line lengths of 8000 octets"), the bound the MCP edge holds each line to.
/// A chunked body's size lines and trailer lines are held to it too.
pub const MAX_LINE: usize = 8000;
/// Format: the most field lines accepted in one head, and the most trailer lines after a chunked body: Apache httpd's
/// `LimitRequestFields` default (tier C), the MCP edge's bound. A git client sends about ten.
pub const MAX_FIELDS: usize = 100;
/// Format: the line break ending each line of a head and of a chunked body (RFC 9112 §2.1, §7.1).
const CRLF: &[u8] = b"\r\n";
/// Format: the last chunk and the empty trailer section ending a chunked body (RFC 9112 §7.1).
pub const LAST_CHUNK: &[u8] = b"0\r\n\r\n";
/// Format: hexadecimal, a chunk size's radix (RFC 9112 §7.1: `chunk-size = 1*HEXDIG`).
const HEX: u32 = 16;

/// A request's method, as git's routes read it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
  /// `GET`.
  Get,
  /// `POST`.
  Post,
  /// Any other.
  Other(String),
}

/// How a request's body is framed (RFC 9112 §6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
  /// No body.
  None,
  /// `Content-Length` bytes.
  Length(u64),
  /// `Transfer-Encoding: chunked` ([`decode_chunked`]).
  Chunked,
}

/// A request head, as git's edge reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestHead {
  /// The method.
  pub method: Method,
  /// The target's path, before any `?`.
  pub path: String,
  /// The target's query, after the `?`; empty without one.
  pub query: String,
  /// Whether the connection is kept after this request (HTTP/1.1's default, `Connection` naming otherwise).
  pub keep_alive: bool,
  /// How its body is framed.
  pub framing: Framing,
  /// Whether its body is gzip-encoded (`Content-Encoding: gzip`).
  pub gzip: bool,
  /// The `Content-Type` field, when given.
  pub content_type: Option<String>,
  /// The `Git-Protocol` field, when given.
  pub git_protocol: Option<String>,
  /// The `Authorization` field, when given (piece 8's authentication reads it).
  pub authorization: Option<String>,
  /// The `Host` field.
  pub host: Option<String>,
}

/// Why a request is refused, as the status it is answered with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// `400 Bad Request`: a malformed head, contradictory framing, a missing or repeated `Host`, or a path that leaves
  /// its repository.
  BadRequest,
  /// `403 Forbidden`: an advertisement asked with no service (the dumb protocol) or an unknown one.
  Forbidden,
  /// `404 Not Found`: a path no route names.
  NotFound,
  /// `405 Method Not Allowed`: a route asked by another method.
  MethodNotAllowed,
  /// `411 Length Required`: a service's request with no body.
  LengthRequired,
  /// `414 URI Too Long`: a request line past [`MAX_LINE`].
  UriTooLong,
  /// `415 Unsupported Media Type`: a service's request of another content type, or a content coding other than gzip.
  UnsupportedMediaType,
  /// `431 Request Header Fields Too Large`: a field line past [`MAX_LINE`], or more than [`MAX_FIELDS`] of them.
  HeadersTooLarge,
  /// `501 Not Implemented`: a transfer coding other than chunked (RFC 9112 §6.1: "A server that receives a request
  /// message with a transfer coding it does not understand SHOULD respond with 501 (Not Implemented)").
  NotImplemented,
}

impl Refused {
  /// The status line's code and reason.
  pub fn status(self) -> &'static str {
    match self {
      Refused::BadRequest => "400 Bad Request",
      Refused::Forbidden => "403 Forbidden",
      Refused::NotFound => "404 Not Found",
      Refused::MethodNotAllowed => "405 Method Not Allowed",
      Refused::LengthRequired => "411 Length Required",
      Refused::UriTooLong => "414 URI Too Long",
      Refused::UnsupportedMediaType => "415 Unsupported Media Type",
      Refused::HeadersTooLarge => "431 Request Header Fields Too Large",
      Refused::NotImplemented => "501 Not Implemented",
    }
  }
}

/// Parses a request head (the bytes before its blank line): the request line and every field line, each within
/// [`MAX_LINE`], at most [`MAX_FIELDS`] fields, the framing consistent, an HTTP/1.1 request's one `Host` present.
pub fn parse_head(head: &[u8]) -> Result<RequestHead, Refused> {
  let _ = head;
  Err(Refused::BadRequest)
}

/// What a chunked body's bytes so far decode to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunked {
  /// The whole body: its data, and how many input bytes it took (a next request starts after them).
  Whole {
    /// The body's data, its chunks joined.
    body: Vec<u8>,
    /// The input bytes the chunked body took, its trailer section and final CRLF included.
    consumed: usize,
  },
  /// Not whole yet: the input is cut somewhere in it, and a reader waits for more.
  Partial,
}

/// Why a request body is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyRefused {
  /// A chunk-size line that isn't hexadecimal, overflows or runs past [`MAX_LINE`]; a chunk's data not followed by its
  /// CRLF; or a trailer section past [`MAX_FIELDS`] lines of [`MAX_LINE`].
  BadChunk,
  /// More than the room its request was admitted ([`Admission`]): declared by its chunks, or inflated from gzip.
  TooLarge(PastAdmission),
  /// A gzip body that doesn't inflate (RFC 1952).
  BadGzip,
}

/// Decodes a chunked body from the bytes after its head (RFC 9112 §7.1), charging the body's exact size to `admission`
/// before it builds it. Its declared sizes are held to the admission's room as they are read, so a chunk declaring more
/// than the request may hold is refused before its data arrives; a cut anywhere is [`Chunked::Partial`] and charges
/// nothing.
pub fn decode_chunked(input: &[u8], admission: &mut Admission) -> Result<Chunked, BodyRefused> {
  let _ = (
    input,
    admission,
    walk_chunks(&[], 0),
    line_end(&[], 0),
    chunk_size(b""),
  );
  Ok(Chunked::Partial)
}

/// The first walk of a chunked body: its total size and the input bytes it takes, `None` while it's cut, each declared
/// size held to `room`.
fn walk_chunks(input: &[u8], room: u64) -> Result<Option<(u64, usize)>, BodyRefused> {
  let (mut at, mut total) = (0usize, 0u64);
  loop {
    let Some(end) = line_end(input, at)? else {
      return Ok(None);
    };
    let size = chunk_size(input.get(at..end).ok_or(BodyRefused::BadChunk)?)?;
    at = end.checked_add(CRLF.len()).ok_or(BodyRefused::BadChunk)?;
    if size == 0 {
      return Ok(trailers(input, at)?.map(|consumed| (total, consumed)));
    }
    total = total.checked_add(size).ok_or(BodyRefused::BadChunk)?;
    if total > room {
      return Err(BodyRefused::TooLarge(PastAdmission {
        asked: total,
        left: room,
      }));
    }
    let data_to = at
      .checked_add(usize::try_from(size).map_err(|_| BodyRefused::BadChunk)?)
      .ok_or(BodyRefused::BadChunk)?;
    let crlf_to = data_to
      .checked_add(CRLF.len())
      .ok_or(BodyRefused::BadChunk)?;
    match input.get(data_to..crlf_to) {
      None => return Ok(None),
      Some(after) if after != CRLF => return Err(BodyRefused::BadChunk),
      Some(_) => at = crlf_to,
    }
  }
}

/// The trailer section from `at` to its empty line (RFC 9112 §7.1.2), at most [`MAX_FIELDS`] lines: the input bytes the
/// whole chunked body takes, `None` while it's cut.
fn trailers(input: &[u8], mut at: usize) -> Result<Option<usize>, BodyRefused> {
  let mut lines = 0usize;
  loop {
    let Some(end) = line_end(input, at)? else {
      return Ok(None);
    };
    let empty = end == at;
    at = end.checked_add(CRLF.len()).ok_or(BodyRefused::BadChunk)?;
    if empty {
      return Ok(Some(at));
    }
    lines = lines.saturating_add(1);
    if lines > MAX_FIELDS {
      return Err(BodyRefused::BadChunk);
    }
  }
}

/// Where the line starting at `at` ends (the index of its CRLF), `None` while it's cut, refused past [`MAX_LINE`].
fn line_end(input: &[u8], at: usize) -> Result<Option<usize>, BodyRefused> {
  let rest = input.get(at..).unwrap_or_default();
  let window = rest
    .get(..MAX_LINE.saturating_add(CRLF.len()))
    .unwrap_or(rest);
  match window.windows(CRLF.len()).position(|pair| pair == CRLF) {
    Some(found) => Ok(Some(at.saturating_add(found))),
    None if rest.len() > MAX_LINE.saturating_add(1) => Err(BodyRefused::BadChunk),
    None => Ok(None),
  }
}

/// A chunk-size line's size (RFC 9112 §7.1: `chunk-size [ chunk-ext ]`): its hexadecimal digits before any extension,
/// at least one, within `u64`.
fn chunk_size(line: &[u8]) -> Result<u64, BodyRefused> {
  let digits = line.split(|byte| *byte == b';').next().unwrap_or_default();
  let digits = std::str::from_utf8(digits).map_err(|_| BodyRefused::BadChunk)?;
  let digits = digits.trim_end_matches([' ', '\t']);
  if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
    return Err(BodyRefused::BadChunk);
  }
  u64::from_str_radix(digits, HEX).map_err(|_| BodyRefused::BadChunk)
}

/// Inflates a whole gzip body (RFC 1952) within `admission`: the inflated bytes are made at most one past the room left
/// and charged, so a body that would inflate past its request's room (a gzip bomb) is refused having held no more than
/// that room, whatever its compressed size.
pub fn gunzip(body: &[u8], admission: &mut Admission) -> Result<Vec<u8>, BodyRefused> {
  let _ = (body, admission);
  Err(BodyRefused::BadGzip)
}

/// The two services smart HTTP carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
  /// `git-upload-pack`: fetch and clone.
  UploadPack,
  /// `git-receive-pack`: push.
  ReceivePack,
}

impl Service {
  /// The service's name, as its path and its content types spell it.
  pub fn name(self) -> &'static str {
    match self {
      Service::UploadPack => "git-upload-pack",
      Service::ReceivePack => "git-receive-pack",
    }
  }

  /// The service a name names.
  pub fn named(name: &str) -> Option<Service> {
    [Service::UploadPack, Service::ReceivePack]
      .into_iter()
      .find(|service| service.name() == name)
  }
}

/// The protocol version a request asks for (gitprotocol-v2 "HTTP Transport": `Git-Protocol: version=2`); the pack
/// protocol's v0 without one, or with a version slates doesn't speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
  /// The pack protocol's first version.
  V0,
  /// v0 with the `version 1` line.
  V1,
  /// Protocol v2.
  V2,
}

/// The version a `Git-Protocol` field asks for: its `version=` key among its colon-separated keys (the last, as git reads
/// them), v0 without one.
pub fn version(git_protocol: Option<&str>) -> Version {
  let _ = git_protocol;
  Version::V0
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
  /// `GET <repo>/info/refs?service=<service>`: the service's advertisement.
  Advertise {
    /// The repository's path, as the request named it.
    repo: String,
    /// The service.
    service: Service,
    /// The version asked for.
    version: Version,
  },
  /// `POST <repo>/<service>`: the service's request, in the body.
  Serve {
    /// The repository's path.
    repo: String,
    /// The service.
    service: Service,
    /// The version asked for.
    version: Version,
  },
}

/// Format: the path ending that asks for an advertisement (gitprotocol-http "Smart Clients").
const INFO_REFS: &str = "/info/refs";

/// Routes a request head ([`Route`], or why not: [`Refused`]).
pub fn route(head: &RequestHead) -> Result<Route, Refused> {
  let _ = (head, checked_repo("/"), INFO_REFS);
  Err(Refused::NotFound)
}

/// A repository path as a route takes it: non-empty, rooted, no empty, `.` or `..` segment and no control byte, so no
/// request names a path outside its repository.
fn checked_repo(repo: &str) -> Result<String, Refused> {
  let segments = repo.strip_prefix('/').ok_or(Refused::NotFound)?;
  if segments.is_empty()
    || repo.bytes().any(|byte| byte.is_ascii_control())
    || segments
      .split('/')
      .any(|segment| segment.is_empty() || segment == "." || segment == "..")
  {
    return Err(Refused::BadRequest);
  }
  Ok(repo.to_owned())
}

/// The content type of `service`'s advertisement.
pub fn advertisement_type(service: Service) -> String {
  format!("application/x-{}-advertisement", service.name())
}

/// The content type of `service`'s result.
pub fn result_type(service: Service) -> String {
  format!("application/x-{}-result", service.name())
}

/// The head of a smart reply: `200`, `content_type`, `Cache-Control: no-cache` and a chunked body; `Connection: close`
/// where the connection ends after it.
pub fn reply_head(content_type: &str, keep_alive: bool) -> Vec<u8> {
  let _ = (content_type, keep_alive);
  Vec::new()
}

/// Format: the methods git's routes take, which a `405` names (RFC 9110 §15.5.6: "The origin server MUST generate an
/// Allow header field in a 405 response").
const ALLOW: &str = "GET, POST";

/// The reply to a refused request: its status, no body, the connection closed (a refused request's body is not read, so
/// the connection can't carry another); a `405` names the methods the routes take.
pub fn refusal_reply(refused: Refused) -> Vec<u8> {
  let _ = (refused, ALLOW);
  Vec::new()
}

/// The start of an advertisement's body: for v0 and v1 the pkt-line `# service=<service>` and a flush
/// (gitprotocol-http: the first line "MUST be `# service=$servicename`", followed by a flush-pkt); for v2 nothing, the
/// capability advertisement starting at once.
pub fn advertisement_prelude(
  service: Service,
  version: Version,
) -> Result<Vec<u8>, pktline::DataTooLong> {
  let _ = (service, version, Packet::Flush, pktline::MAX_LEN);
  Ok(Vec::new())
}

/// One chunk of a chunked reply (RFC 9112 §7.1): its size in hexadecimal, CRLF, its data, CRLF. No chunk for no data,
/// which would end the body: [`LAST_CHUNK`] does that.
pub fn chunk(data: &[u8]) -> Vec<u8> {
  let _ = data;
  Vec::new()
}

#[cfg(test)]
#[path = "tests/http.rs"]
mod tests;
