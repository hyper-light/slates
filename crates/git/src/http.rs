//! git's smart HTTP routes and replies (A-595; §4.17.2 "Transports" 1, piece 8's first part), over `slates-http`'s
//! request codec, which the Mercurial and Subversion servers share (that crate's documentation says why it is a crate):
//! a request's head is read there, and routed here as `git http-backend` routes it [C: `http-backend.c`'s `services[]`,
//! `select_service`, `check_content_type`, `bad_request`].
//!
//! **Routes** ([`route`]). Each refusal is answered with `http-backend`'s status:
//! - `GET <repo>/info/refs?service=git-upload-pack` (or `git-receive-pack`) asks for the service's advertisement, and
//!   `HEAD` for its head alone (RFC 9110 §9.1: "All general-purpose servers MUST support the methods GET and HEAD";
//!   `http-backend` reads `HEAD` as `GET`). With no service, `info/refs` is the dumb protocol's ref list, which slates
//!   does not serve: `403`, as `http-backend` answers with `http.getanyfile` off, and so is every other dumb-protocol
//!   path (`HEAD`, `objects/info/alternates`, `objects/info/http-alternates`, `objects/info/packs`, a loose object, a
//!   pack or its index). An unknown service is `403` ("Unsupported service"), and so is `git-upload-archive`, which
//!   slates does not serve (git's own default: "Service not enabled").
//! - `POST <repo>/git-upload-pack` and `POST <repo>/git-receive-pack` carry a service's request, of the content type
//!   `application/x-<service>-request` (`415` otherwise, as `check_content_type` answers; the type is compared as RFC
//!   9110 §8.3.1 compares media types, without regard to case or parameters) and with a body (`411` without).
//! - A route asked by another method is `405` naming its methods (`GET, HEAD` or `POST`), or `400` to an HTTP/1.0
//!   request, which has no `405` (`bad_request`). A path no route names is `404`, and so is a repository path that is
//!   empty or has an empty, `.` or `..` segment (`daemon_avoid_alias`). A path is routed decoded, as a CGI program reads
//!   `PATH_INFO`; an escape that is malformed or names a slash or a NUL, or a repository path that is not UTF-8, is
//!   `400`. The checks run in `http-backend`'s order: the method, the repository path, the service, the content type.
//! - The protocol version ([`version`]): `Git-Protocol`'s greatest `version=` among its colon-separated keys, as
//!   `protocol.c` reads it (`determine_protocol_version_server`: "select the greatest version that the client has
//!   requested"); v0 without one. A `Git-Protocol` sent twice is `400`.
//!
//! **Replies.** A smart reply is `200` with its content type (`application/x-<service>-advertisement` or `-result`),
//! the three cache fields `http-backend` sends on every answer (`hdr_nocache`: an `Expires` in the past, `Pragma:
//! no-cache` and `Cache-Control: no-cache, max-age=0, must-revalidate`; gitprotocol-http: "Cache-Control headers
//! SHOULD be used to disable caching of the returned entity"), and a chunked body to an HTTP/1.1 request, one framed by
//! the close to HTTP/1.0 ([`reply_head`]). For protocol v0 and v1 an advertisement starts with the pkt-line
//! `# service=<service>` and a flush (gitprotocol-http: the first line "MUST be `# service=$servicename`"); for v2
//! nothing precedes the capability advertisement ([`advertisement_prelude`]). A refusal carries its status, the same
//! cache fields, `Allow` for a `405`, no body, and closes the connection ([`refusal_reply`]).
//!
//! **Golden vectors.** `fixtures/http/` holds stock git's own HTTP traffic with `git http-backend`, recorded by
//! `make-http-fixtures.sh` through `cgi-host.py`, a loopback CGI host. Every request's head is read and routed as git
//! meant it, every body's framing removed and every gzip body inflated to the bytes git compressed, and each
//! advertisement's start and each reply's fields compared with `http-backend`'s; the fixtures are SHA-256 checked. As
//! main ruled with A-607, they hold inflated bytes alone, never git's compressed bytes: a gzip body is held inflated and
//! gzipped again by the test, and no pack is held.

use slates_http::Admission;
use slates_http::head::{Framing, Head, HeadRefused, Target, Version as HttpVersion, media_type};
use slates_http::percent::{self, Plus};
use slates_http::reply::{self, BadField, Body, Status};

use crate::pktline::{self, Packet};
use crate::to_u64;

/// The two services smart HTTP carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
  /// `git-upload-pack`: fetch and clone.
  UploadPack,
  /// `git-receive-pack`: push.
  ReceivePack,
}

impl Service {
  /// The service's name, as its path, its query and its content types spell it.
  pub fn name(self) -> &'static str {
    match self {
      Service::UploadPack => "git-upload-pack",
      Service::ReceivePack => "git-receive-pack",
    }
  }

  /// The content type of the service's requests.
  fn request_type(self) -> &'static [u8] {
    match self {
      Service::UploadPack => UPLOAD_PACK_REQUEST,
      Service::ReceivePack => RECEIVE_PACK_REQUEST,
    }
  }

  /// The content type of the service's advertisement.
  fn advertisement_type(self) -> &'static [u8] {
    match self {
      Service::UploadPack => UPLOAD_PACK_ADVERTISEMENT,
      Service::ReceivePack => RECEIVE_PACK_ADVERTISEMENT,
    }
  }

  /// The content type of the service's result.
  fn result_type(self) -> &'static [u8] {
    match self {
      Service::UploadPack => UPLOAD_PACK_RESULT,
      Service::ReceivePack => RECEIVE_PACK_RESULT,
    }
  }
}

/// The protocol version a request asks for, ordered oldest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Version {
  /// The pack protocol's first version.
  V0,
  /// v0 with the `version 1` line.
  V1,
  /// Protocol v2.
  V2,
}

/// Where a request goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
  /// `GET` or `HEAD` `<repo>/info/refs?service=<service>`: the service's advertisement.
  Advertise {
    /// The repository's path, decoded, without its leading `/`.
    repo: String,
    /// The service.
    service: Service,
    /// The version asked for.
    version: Version,
    /// Whether the request is `HEAD`: its reply is the head alone.
    head_only: bool,
  },
  /// `POST <repo>/<service>`: the service's request, in the body.
  Serve {
    /// The repository's path, decoded, without its leading `/`.
    repo: String,
    /// The service.
    service: Service,
    /// The version asked for.
    version: Version,
  },
}

/// The methods a `405` names, as `http-backend` names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allowed {
  /// `GET, HEAD`: `info/refs` and the dumb protocol's paths.
  GetHead,
  /// `POST`: a service's request.
  Post,
}

/// Why a request is not routed; each kind is answered with `http-backend`'s status ([`RouteRefused::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteRefused {
  /// `404`: no route names the path ("Request not supported"), or its repository path is empty or has an empty, `.` or
  /// `..` segment.
  NotFound,
  /// `405` naming the route's methods, or `400` to an HTTP/1.0 request.
  MethodNotAllowed(Allowed),
  /// `403`: the dumb protocol, which slates does not serve.
  DumbProtocol,
  /// `403`: a service that is not git's ("Unsupported service").
  UnsupportedService,
  /// `403`: `git-upload-archive`, a service of git's that slates does not serve ("Service not enabled").
  ServiceNotServed,
  /// `415`: a service's request of another content type.
  UnsupportedMediaType,
  /// `411`: a service's request with no body.
  LengthRequired,
  /// `400`: a path escape that is malformed or names a slash or a NUL, or a repository path that is not UTF-8.
  BadPath,
  /// The head refused when a field it routes by was read: `Git-Protocol` or `Content-Type` sent twice.
  Head(HeadRefused),
  /// `414`: the decoded path past the request's room.
  PastRoom,
}

impl RouteRefused {
  /// The status the refusal is answered with to a request of version `http`.
  pub fn status(self, http: HttpVersion) -> Status {
    match self {
      RouteRefused::NotFound => Status::NOT_FOUND,
      RouteRefused::MethodNotAllowed(_) if http == HttpVersion::Http10 => Status::BAD_REQUEST,
      RouteRefused::MethodNotAllowed(_) => Status::METHOD_NOT_ALLOWED,
      RouteRefused::DumbProtocol
      | RouteRefused::UnsupportedService
      | RouteRefused::ServiceNotServed => Status::FORBIDDEN,
      RouteRefused::UnsupportedMediaType => Status::UNSUPPORTED_MEDIA_TYPE,
      RouteRefused::LengthRequired => Status::LENGTH_REQUIRED,
      RouteRefused::BadPath => Status::BAD_REQUEST,
      RouteRefused::Head(refused) => refused.status(),
      RouteRefused::PastRoom => Status::URI_TOO_LONG,
    }
  }
}

/// Format: the content types of smart HTTP's requests, advertisements and results (gitprotocol-http "Smart Clients"
/// and "Smart Service git-upload-pack"; `http-backend.c`'s `application/x-git-%s-...`).
const UPLOAD_PACK_REQUEST: &[u8] = b"application/x-git-upload-pack-request";
/// Format: as [`UPLOAD_PACK_REQUEST`].
const RECEIVE_PACK_REQUEST: &[u8] = b"application/x-git-receive-pack-request";
/// Format: as [`UPLOAD_PACK_REQUEST`].
const UPLOAD_PACK_ADVERTISEMENT: &[u8] = b"application/x-git-upload-pack-advertisement";
/// Format: as [`UPLOAD_PACK_REQUEST`].
const RECEIVE_PACK_ADVERTISEMENT: &[u8] = b"application/x-git-receive-pack-advertisement";
/// Format: as [`UPLOAD_PACK_REQUEST`].
const UPLOAD_PACK_RESULT: &[u8] = b"application/x-git-upload-pack-result";
/// Format: as [`UPLOAD_PACK_REQUEST`].
const RECEIVE_PACK_RESULT: &[u8] = b"application/x-git-receive-pack-result";

/// Format: the three cache fields `http-backend` sends on every answer, in its order (`hdr_nocache`).
const NO_CACHE: [(&[u8], &[u8]); 3] = [
  (b"Expires", b"Fri, 01 Jan 1980 00:00:00 GMT"),
  (b"Pragma", b"no-cache"),
  (b"Cache-Control", b"no-cache, max-age=0, must-revalidate"),
];

/// Format: the field naming a reply's content type, and the one naming a `405`'s methods (RFC 9110 §8.3, §10.2.1).
const CONTENT_TYPE: &[u8] = b"Content-Type";
/// Format: as [`CONTENT_TYPE`].
const ALLOW: &[u8] = b"Allow";
/// Format: the methods a `405` names for `info/refs` and the dumb paths, and for a service's request (`bad_request`).
const ALLOW_GET_HEAD: &[u8] = b"GET, HEAD";
/// Format: as [`ALLOW_GET_HEAD`].
const ALLOW_POST: &[u8] = b"POST";

/// Format: the request fields a route reads (gitprotocol-v2 "HTTP Transport"; RFC 9110 §8.3).
const GIT_PROTOCOL: &[u8] = b"git-protocol";
/// Format: as [`GIT_PROTOCOL`].
const REQUEST_CONTENT_TYPE: &[u8] = b"content-type";

/// Format: the query field naming the service (gitprotocol-http "Smart Clients": `$GIT_URL/info/refs?service=...`),
/// and the one service of git's that slates does not serve.
const SERVICE_FIELD: &[u8] = b"service";
/// Format: as [`SERVICE_FIELD`].
const UPLOAD_ARCHIVE: &[u8] = b"git-upload-archive";

/// Format: what separates `Git-Protocol`'s keys, and the key naming a version (gitprotocol-v2 "HTTP Transport";
/// `protocol.c`).
const KEY_SEPARATOR: u8 = b':';
/// Format: as [`KEY_SEPARATOR`].
const VERSION_KEY: &[u8] = b"version=";

/// Format: the bytes an escape in a path may not name: a slash, which would split a segment, and a NUL.
const PATH_REFUSED: &[u8] = b"/\0";

/// Format: the start of an advertisement's first line (gitprotocol-http: "MUST be `# service=$servicename`").
const SERVICE_LINE: &[u8] = b"# service=";

/// Format: the hexadecimal digits of an object name: SHA-1's and SHA-256's (gitformat-loose; `http-backend.c`'s
/// `services[]` matches both).
const NAME_DIGITS: [usize; 2] = [40, 64];
/// Format: the digits a loose object's directory takes from the front of its name.
const LOOSE_DIRECTORY_DIGITS: usize = 2;

/// A request's method, as the routes read it (methods are case-sensitive, RFC 9110 §9.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
  /// `GET`.
  Get,
  /// `HEAD`.
  Head,
  /// `POST`.
  Post,
  /// Any other.
  Other,
}

impl Method {
  /// The method `name` names.
  fn of(name: &[u8]) -> Self {
    match name {
      b"GET" => Method::Get,
      b"HEAD" => Method::Head,
      b"POST" => Method::Post,
      _ => Method::Other,
    }
  }
}

/// What a path's last segments name, as `http-backend`'s `services[]` table matches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
  /// `info/refs`.
  InfoRefs,
  /// A service's request.
  Service(Service),
  /// `git-upload-archive`'s request, which slates does not serve.
  Archive,
  /// A path of the dumb protocol.
  Dumb,
}

/// Routes a request head ([`Route`], or why not: [`RouteRefused`]), charging the decoded path it keeps to `admission`.
pub fn route(head: &Head<'_>, admission: &mut Admission) -> Result<Route, RouteRefused> {
  let Target::Path { path, query } = head.target else {
    return Err(RouteRefused::NotFound);
  };
  let relative = path.strip_prefix(b"/").ok_or(RouteRefused::NotFound)?;
  let decoded = decode_path(relative, admission)?;
  let (endpoint, span) = endpoint(&decoded).ok_or(RouteRefused::NotFound)?;
  let method = Method::of(head.method);
  check_method(endpoint, method)?;
  let repo = repository(decoded, span)?;
  match endpoint {
    Endpoint::InfoRefs => advertise(head, query.unwrap_or_default(), repo, method == Method::Head),
    Endpoint::Service(service) => serve(head, service, repo),
    Endpoint::Archive => Err(RouteRefused::ServiceNotServed),
    Endpoint::Dumb => Err(RouteRefused::DumbProtocol),
  }
}

/// A path without its leading `/`, decoded and charged: refused when an escape is malformed or names a slash or a NUL.
fn decode_path(raw: &[u8], admission: &mut Admission) -> Result<Vec<u8>, RouteRefused> {
  if percent::names_any(raw, PATH_REFUSED) {
    return Err(RouteRefused::BadPath);
  }
  let length = percent::decoded_len(raw).map_err(|_| RouteRefused::BadPath)?;
  admission
    .charge(to_u64(length))
    .map_err(|_| RouteRefused::PastRoom)?;
  let mut decoded = Vec::with_capacity(length);
  percent::decode_into(raw, Plus::Plus, &mut decoded).map_err(|_| RouteRefused::BadPath)?;
  Ok(decoded)
}

/// The endpoint a decoded path ends with, and the bytes it spans with the slash before each of its segments.
fn endpoint(path: &[u8]) -> Option<(Endpoint, usize)> {
  let mut segments = path.rsplit(|byte| *byte == b'/');
  let last = segments.next()?;
  let second = segments.next();
  let third = segments.next();
  let span = |names: &[&[u8]]| {
    names
      .iter()
      .map(|name| name.len().saturating_add(1))
      .fold(0, usize::saturating_add)
  };
  match (third, second, last) {
    (_, Some(info @ b"info"), refs @ b"refs") => Some((Endpoint::InfoRefs, span(&[info, refs]))),
    (_, _, b"git-upload-pack") => Some((Endpoint::Service(Service::UploadPack), span(&[last]))),
    (_, _, b"git-receive-pack") => Some((Endpoint::Service(Service::ReceivePack), span(&[last]))),
    (_, _, name) if name == UPLOAD_ARCHIVE => Some((Endpoint::Archive, span(&[last]))),
    (_, _, b"HEAD") => Some((Endpoint::Dumb, span(&[last]))),
    (Some(objects), Some(middle), file) if is_dumb_object_path(objects, middle, file) => {
      Some((Endpoint::Dumb, span(&[objects, middle, file])))
    }
    _ => None,
  }
}

/// Whether `objects/<middle>/<file>` is one of the dumb protocol's object paths: `info/alternates`,
/// `info/http-alternates`, `info/packs`, a loose object, or a pack or its index.
fn is_dumb_object_path(objects: &[u8], middle: &[u8], file: &[u8]) -> bool {
  objects == b"objects"
    && match middle {
      b"info" => matches!(file, b"alternates" | b"http-alternates" | b"packs"),
      b"pack" => is_pack_file(file),
      directory => is_loose_object(directory, file),
    }
}

/// Whether `directory/file` names a loose object: two lowercase hexadecimal digits, then the rest of a name's.
fn is_loose_object(directory: &[u8], file: &[u8]) -> bool {
  directory.len() == LOOSE_DIRECTORY_DIGITS
    && is_lower_hex(directory)
    && is_lower_hex(file)
    && NAME_DIGITS
      .iter()
      .any(|digits| file.len().saturating_add(LOOSE_DIRECTORY_DIGITS) == *digits)
}

/// Whether `file` is `pack-<name>.pack` or `pack-<name>.idx`.
fn is_pack_file(file: &[u8]) -> bool {
  let name = file
    .strip_prefix(b"pack-")
    .and_then(|rest| rest.strip_suffix(b".pack").or_else(|| rest.strip_suffix(b".idx")));
  name.is_some_and(|name| is_lower_hex(name) && NAME_DIGITS.contains(&name.len()))
}

/// Whether `bytes` are lowercase hexadecimal digits, as `http-backend`'s patterns (`[0-9a-f]`) take them.
fn is_lower_hex(bytes: &[u8]) -> bool {
  bytes
    .iter()
    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

/// Whether `method` may ask for `endpoint`: `GET` or `HEAD` for `info/refs` and the dumb paths, `POST` for a service.
fn check_method(endpoint: Endpoint, method: Method) -> Result<(), RouteRefused> {
  match (endpoint, method) {
    (Endpoint::Service(_) | Endpoint::Archive, Method::Post)
    | (Endpoint::InfoRefs | Endpoint::Dumb, Method::Get | Method::Head) => Ok(()),
    (Endpoint::Service(_) | Endpoint::Archive, _) => {
      Err(RouteRefused::MethodNotAllowed(Allowed::Post))
    }
    (Endpoint::InfoRefs | Endpoint::Dumb, _) => Err(RouteRefused::MethodNotAllowed(Allowed::GetHead)),
  }
}

/// The repository path: the decoded path before the endpoint's span, refused when it is empty or has an empty, `.` or
/// `..` segment (`daemon_avoid_alias`), or is not UTF-8.
fn repository(mut decoded: Vec<u8>, span: usize) -> Result<String, RouteRefused> {
  let length = decoded
    .len()
    .checked_sub(span)
    .filter(|length| *length > 0)
    .ok_or(RouteRefused::NotFound)?;
  decoded.truncate(length);
  if decoded
    .split(|byte| *byte == b'/')
    .any(|segment| matches!(segment, b"" | b"." | b".."))
  {
    return Err(RouteRefused::NotFound);
  }
  String::from_utf8(decoded).map_err(|_| RouteRefused::BadPath)
}

/// The advertisement route: the service the query names, the version asked.
fn advertise(
  head: &Head<'_>,
  query: &[u8],
  repo: String,
  head_only: bool,
) -> Result<Route, RouteRefused> {
  let service = query_service(query)?;
  Ok(Route::Advertise {
    repo,
    service,
    version: asked_version(head)?,
    head_only,
  })
}

/// The service a query names: its last `service` field, decoded as a form's, as `http-backend`'s `get_parameter` reads
/// it; the dumb protocol without one.
fn query_service(query: &[u8]) -> Result<Service, RouteRefused> {
  let mut named = None;
  for (name, value) in percent::form_fields(query) {
    if percent::decodes_to(name, Plus::Space, SERVICE_FIELD).map_err(|_| RouteRefused::BadPath)? {
      named = Some(value);
    }
  }
  let value = named.ok_or(RouteRefused::DumbProtocol)?;
  let is = |name: &[u8]| percent::decodes_to(value, Plus::Space, name).unwrap_or(false);
  if is(Service::UploadPack.name().as_bytes()) {
    Ok(Service::UploadPack)
  } else if is(Service::ReceivePack.name().as_bytes()) {
    Ok(Service::ReceivePack)
  } else if is(UPLOAD_ARCHIVE) {
    Err(RouteRefused::ServiceNotServed)
  } else {
    Err(RouteRefused::UnsupportedService)
  }
}

/// The service route: a request of the service's content type, with a body, at the version asked.
fn serve(head: &Head<'_>, service: Service, repo: String) -> Result<Route, RouteRefused> {
  let content_type = head
    .field(REQUEST_CONTENT_TYPE)
    .map_err(RouteRefused::Head)?;
  if !content_type.is_some_and(|value| media_type(value).eq_ignore_ascii_case(service.request_type())) {
    return Err(RouteRefused::UnsupportedMediaType);
  }
  if head.framing == Framing::None {
    return Err(RouteRefused::LengthRequired);
  }
  Ok(Route::Serve {
    repo,
    service,
    version: asked_version(head)?,
  })
}

/// The version the request's `Git-Protocol` asks for; refused when it came twice.
fn asked_version(head: &Head<'_>) -> Result<Version, RouteRefused> {
  head
    .field(GIT_PROTOCOL)
    .map(version)
    .map_err(RouteRefused::Head)
}

/// The version a `Git-Protocol` value asks for: the greatest `version=` among its colon-separated keys that names a
/// version git speaks; v0 without one.
pub fn version(git_protocol: Option<&[u8]>) -> Version {
  git_protocol
    .into_iter()
    .flat_map(|value| value.split(|byte| *byte == KEY_SEPARATOR))
    .filter_map(|key| key.strip_prefix(VERSION_KEY))
    .filter_map(|asked| match asked {
      b"0" => Some(Version::V0),
      b"1" => Some(Version::V1),
      b"2" => Some(Version::V2),
      _ => None,
    })
    .max()
    .unwrap_or(Version::V0)
}

/// What a smart reply carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply {
  /// The service's advertisement.
  Advertisement(Service),
  /// The service's result.
  Result(Service),
}

/// Writes a smart reply's head: `200`, the reply's content type, `http-backend`'s cache fields, and a chunked body to
/// an HTTP/1.1 request (`Connection: close` too unless `keep_alive`), or one framed by the close to HTTP/1.0.
pub fn reply_head(
  reply: Reply,
  http: HttpVersion,
  keep_alive: bool,
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  let content_type = match reply {
    Reply::Advertisement(service) => service.advertisement_type(),
    Reply::Result(service) => service.result_type(),
  };
  let [expires, pragma, cache_control] = NO_CACHE;
  let fields = [expires, pragma, cache_control, (CONTENT_TYPE, content_type)];
  let body = match http {
    HttpVersion::Http11 => Body::Chunked,
    HttpVersion::Http10 => Body::UntilClose,
  };
  reply::head(Status::OK, &fields, body, !keep_alive, out)
}

/// Writes the reply to a refused route to a request of version `http`: its status, `http-backend`'s cache fields,
/// `Allow` for a `405`, no body, and `Connection: close`.
pub fn refusal_reply(
  refused: RouteRefused,
  http: HttpVersion,
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  let status = refused.status(http);
  let [expires, pragma, cache_control] = NO_CACHE;
  match refused {
    RouteRefused::MethodNotAllowed(allowed) if status == Status::METHOD_NOT_ALLOWED => {
      let methods = match allowed {
        Allowed::GetHead => ALLOW_GET_HEAD,
        Allowed::Post => ALLOW_POST,
      };
      reply::refusal(status, &[(ALLOW, methods), expires, pragma, cache_control], out)
    }
    _ => reply::refusal(status, &NO_CACHE, out),
  }
}

/// Writes the start of an advertisement's body: for protocol v0 and v1 the pkt-line `# service=<service>` and a flush,
/// for v2 nothing.
pub fn advertisement_prelude(
  service: Service,
  version: Version,
  out: &mut Vec<u8>,
) -> Result<(), pktline::DataTooLong> {
  if version == Version::V2 {
    return Ok(());
  }
  let mut line = SERVICE_LINE.to_vec();
  line.extend_from_slice(service.name().as_bytes());
  line.push(b'\n');
  pktline::write(Packet::Data(&line), out)?;
  pktline::write(Packet::Flush, out)
}

#[cfg(test)]
#[path = "tests/http.rs"]
mod tests;
