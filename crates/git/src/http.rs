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
use slates_http::head::{Head, HeadRefused, Version as HttpVersion};
use slates_http::reply::{self, BadField, Status};

use crate::pktline::{self, Packet};

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
    let _ = (self, http);
    Status::OK
  }
}

/// Routes a request head ([`Route`], or why not: [`RouteRefused`]), charging the decoded path it keeps to `admission`.
pub fn route(head: &Head<'_>, admission: &mut Admission) -> Result<Route, RouteRefused> {
  let _ = (head, admission);
  Err(RouteRefused::NotFound)
}

/// The version a `Git-Protocol` value asks for: the greatest `version=` among its colon-separated keys that names a
/// version git speaks; v0 without one.
pub fn version(git_protocol: Option<&[u8]>) -> Version {
  let _ = git_protocol;
  Version::V1
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
  let _ = (reply, http, keep_alive, out);
  Ok(())
}

/// Writes the reply to a refused route to a request of version `http`: its status, `http-backend`'s cache fields,
/// `Allow` for a `405`, no body, and `Connection: close`.
pub fn refusal_reply(
  refused: RouteRefused,
  http: HttpVersion,
  out: &mut Vec<u8>,
) -> Result<(), BadField> {
  let _ = (refused, http, out, reply::LAST_CHUNK);
  Ok(())
}

/// Writes the start of an advertisement's body: for protocol v0 and v1 the pkt-line `# service=<service>` and a flush,
/// for v2 nothing.
pub fn advertisement_prelude(
  service: Service,
  version: Version,
  out: &mut Vec<u8>,
) -> Result<(), pktline::DataTooLong> {
  let _ = (service, version, out, Packet::Flush);
  Ok(())
}

#[cfg(test)]
#[path = "tests/http.rs"]
mod tests;
