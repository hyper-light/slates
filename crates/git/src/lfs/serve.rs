//! Serving Git LFS (A-629, part 1; §4.17.2 "Large files" and "Access"): a request slates-http
//! has read (A-595) is routed here and answered here, through slates-http's reply framing. The
//! edge that reads sockets and authenticates the caller (A-625) hands in the head, the body and
//! a [`Context`]; the repository's own state comes in as three seams the server implements: who
//! may do what ([`Access`], vcs_policy's check, A-597), the objects ([`Objects`], `slates-vcs`
//! through [`super::objects`]), and the locks ([`Locks`], the repository's lock log, guarded then
//! applied: A-642 keeps a repository's own state in logs of its own, never on a position every
//! push passes).
//!
//! **Routes** ([`route`]), under `<repo>/info/lfs`, the LFS server URL git-lfs derives from a
//! remote (server-discovery.md \[B\]): the batch API (`POST objects/batch`), the locks API (`POST
//! locks`, `GET locks`, `POST locks/verify`, `POST locks/{id}/unlock`; A-610's [`super::route`]),
//! and the basic adapter's transfers this server hands out as actions: `PUT objects/{oid}` to
//! upload, `GET objects/{oid}` to download and `POST objects/{oid}/verify` to confirm an upload.
//! The path is read decoded as git's smart HTTP routes read one: an escape that names a slash or a
//! NUL, or a repository path that is not UTF-8, is `400`; an empty, `.` or `..` segment is `404`.
//! A JSON endpoint's body must be `application/vnd.git-lfs+json` (`415` otherwise; a `charset`
//! parameter is taken, as batch.md asks). An answer's type must be acceptable: an `Accept` that
//! lists ranges is `406` unless the most specific of them that cover LFS's type is not weighted
//! zero (RFC 9110 §12.5.1, §12.4.2's "not acceptable"); its empty list elements are no ranges.
//!
//! **Answers.** The batch API answers each object with the basic adapter's actions, each href under
//! the request's own LFS server URL ([`Context::base`]): a download where the repository holds the
//! object at the size asked, an upload and its verification where it holds none, nothing where it
//! holds it already, and a per-object `404` or `422` otherwise. An action works exactly as long as
//! the credential that asked for it, since a transfer authenticates at the edge as the batch did:
//! its `expires_at` is that credential's expiry when it has one (a bearer token's), and an action
//! asked with a credential that never expires (Basic) carries none. No expiry is chosen here.
//!
//! **Who may do what** is asked of [`Access`] as a [`Need`] on the ref the request names (the
//! repository's whole namespace for a transfer, which names none). A refusal of a request that sent
//! no credentials is `401` with `LFS-Authenticate` when the edge gave a challenge, so git-lfs asks
//! for credentials and retries; of one that did, `403`.
//!
//! **Bounds.** JSON is read by [`super::json`], charged to the request's admission. Every answer
//! is charged before it is made: its body as measured, and the hrefs and slots it holds while it
//! is written; one past the room is `413`, and a body is allocated at its measured length. A page
//! of locks borrows the log's records, never copies them, and charges each lock at its place in a
//! list with the comma before it, so at most a byte a list over what it writes. It is cut where
//! the client's limit ends, or where the room would no longer hold the next lock and a cursor
//! naming the one after it, so the cursor that ends a page always fits. A page that cannot hold
//! one lock is `413`, never an empty page whose cursor names the lock it could not hold: that
//! would bring the client back to the same page for ever. A cursor that names no place in the
//! lock log is `422`.

use slates_http::Admission as HttpAdmission;
use slates_http::head::{Head, HeadRefused, Target, Version as HttpVersion, media_type};
use slates_http::percent::{self, Plus};
use slates_http::reply::{self, BadField, Body, Status};
use slates_vcs::{Admitted, ContentRefused, ContentStore};

use super::batch::{
  self, Action, Answer as ObjectAnswer, ErrorCode, ObjectError, Operation, Outcome,
};
use super::json::{JsonStr, Refusal::Nesting};
use super::locks::{self, Lock};
use super::objects::{LfsObjects, ObjectRefused};
use super::{ErrorBody, MEDIA_TYPE, Oid, Route, Timestamp, write_error};
use crate::Admission;
use crate::out::Measure;
use crate::to_u64;

/// Format: `201 Created` (RFC 9110 §15.3.2): a lock created (locking.md).
pub const CREATED: Status = Status {
  code: 201,
  reason: "Created",
};

/// Format: `406 Not Acceptable` (RFC 9110 §15.5.7): batch.md's "The Accept header needs to be
/// `application/vnd.git-lfs+json`".
pub const NOT_ACCEPTABLE: Status = Status {
  code: 406,
  reason: "Not Acceptable",
};

/// Format: `409 Conflict` (RFC 9110 §15.5.10): locking.md's "Lock Exists".
pub const CONFLICT: Status = Status {
  code: 409,
  reason: "Conflict",
};

/// Format: `422 Unprocessable Content` (RFC 9110 §15.5.21): batch.md's "Validation error".
pub const UNPROCESSABLE: Status = Status {
  code: 422,
  reason: "Unprocessable Content",
};

/// Format: `507 Insufficient Storage` (RFC 4918 §11.5): batch.md's "The server has insufficient
/// storage capacity to complete the request".
pub const INSUFFICIENT_STORAGE: Status = Status {
  code: 507,
  reason: "Insufficient Storage",
};

/// Format: `500 Internal Server Error` (RFC 9110 §15.6.1).
pub const INTERNAL: Status = Status {
  code: 500,
  reason: "Internal Server Error",
};

/// Format: the bytes an escape in a path may not name: a slash, which would split a segment, and a
/// NUL (as git's smart HTTP routes refuse them).
const PATH_REFUSED: &[u8] = b"/\0";

/// Format: where the LFS server URL ends within a request's path (server-discovery.md: "Git LFS
/// will add `.git/info/lfs` to the end of a Git remote url").
const LFS_SUFFIX: &[u8] = b"/info/lfs";

/// Format: the media type of a downloaded object's bytes (basic-transfers.md:
/// `Content-Type: application/octet-stream`).
const OCTET_STREAM: &[u8] = b"application/octet-stream";

/// Format: the field a `401` names the scheme the client authenticates with in (batch.md:
/// "`LFS-Authenticate` ... mirrors the standard `WWW-Authenticate` header").
const LFS_AUTHENTICATE: &[u8] = b"LFS-Authenticate";

/// Format: the bytes a path segment carries as themselves, and the slash between segments (RFC
/// 3986 §3.3: `pchar = unreserved / pct-encoded / sub-delims / ":" / "@"`, with §2.3's unreserved
/// letters and digits apart).
const PATH_SAFE: &[u8] = b"-._~!$&'()*+,;=:@/";

/// Format: where a batch answer's hrefs lead under the LFS server URL: an object, and its
/// verification after it.
const OBJECTS_PATH: &str = "/objects/";

/// Format: the verification's path after an object's.
const VERIFY_PATH: &str = "/verify";

/// Format: the reply field naming a body's media type (RFC 9110 §8.3).
const CONTENT_TYPE: &[u8] = b"Content-Type";

/// Format: the field naming a `405`'s methods (RFC 9110 §10.2.1).
const ALLOW: &[u8] = b"Allow";

/// Format: the field listing the media ranges a client accepts (RFC 9110 §12.5.1).
const ACCEPT: &[u8] = b"Accept";

/// Format: the range covering every subtype of `application`, LFS's type's top-level type (RFC
/// 9110 §12.5.1).
const APPLICATION_RANGE: &[u8] = b"application/*";

/// Format: the range covering every media type (RFC 9110 §12.5.1).
const ANY_RANGE: &[u8] = b"*/*";

/// Format: the name of a range's weight parameter (RFC 9110 §12.4.2: `weight = OWS ";" OWS "q="
/// qvalue`).
const WEIGHT: &[u8] = b"q";

/// What a request asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
  /// `POST objects/batch`.
  Batch,
  /// `PUT objects/{oid}`: an upload.
  Upload(Oid),
  /// `GET objects/{oid}`: a download.
  Download(Oid),
  /// `POST objects/{oid}/verify`: an upload confirmed.
  Verify(Oid),
  /// `POST locks`.
  CreateLock,
  /// `GET locks`.
  ListLocks,
  /// `POST locks/verify`.
  VerifyLocks,
  /// `POST locks/{id}/unlock`.
  Unlock {
    /// The lock's id, as the path names it.
    id: String,
  },
}

/// A routed request: the repository it is for and what it asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routed {
  /// The repository's path, decoded, without its leading `/`.
  pub repo: String,
  /// What the request asks.
  pub endpoint: Endpoint,
}

/// The methods a `405` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allow {
  /// `POST`: the batch API, a verification, `locks/verify`, an unlock.
  Post,
  /// `GET, POST`: `locks`.
  GetPost,
  /// `GET, PUT`: an object.
  GetPut,
}

impl Allow {
  /// The `Allow` field's value.
  pub fn methods(self) -> &'static [u8] {
    match self {
      Allow::Post => b"POST",
      Allow::GetPost => b"GET, POST",
      Allow::GetPut => b"GET, PUT",
    }
  }
}

/// Why a request is not routed; each kind names its status ([`RouteRefused::status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteRefused {
  /// `404`: no LFS endpoint names the path, or its repository path has an empty, `.` or `..`
  /// segment.
  NotFound,
  /// `405` naming the endpoint's methods; `400` to an HTTP/1.0 request, which has no `405`.
  MethodNotAllowed(Allow),
  /// `400`: a path escape that is malformed or names a slash or a NUL, or a repository path that
  /// is not UTF-8.
  BadPath,
  /// `414`: the decoded path past the request's room.
  PastRoom,
  /// The head refused where a field this routes by was read twice.
  Head(HeadRefused),
  /// `415`: a JSON endpoint's body of another media type.
  UnsupportedMediaType,
  /// `406`: an `Accept` that covers no LFS body.
  NotAcceptable,
}

impl RouteRefused {
  /// The status the refusal is answered with to a request of version `http`.
  pub fn status(self, http: HttpVersion) -> Status {
    match self {
      RouteRefused::NotFound => Status::NOT_FOUND,
      RouteRefused::MethodNotAllowed(_) if http == HttpVersion::Http10 => Status::BAD_REQUEST,
      RouteRefused::MethodNotAllowed(_) => Status::METHOD_NOT_ALLOWED,
      RouteRefused::BadPath => Status::BAD_REQUEST,
      RouteRefused::PastRoom => Status::URI_TOO_LONG,
      RouteRefused::Head(refused) => refused.status(),
      RouteRefused::UnsupportedMediaType => Status::UNSUPPORTED_MEDIA_TYPE,
      RouteRefused::NotAcceptable => NOT_ACCEPTABLE,
    }
  }

  /// Writes the refusal's reply to a request of version `http`: its status, `Allow` for a `405`, no
  /// body, the connection closed.
  pub fn reply(self, http: HttpVersion, out: &mut Vec<u8>) -> Result<(), BadField> {
    match self {
      RouteRefused::MethodNotAllowed(allow) if http == HttpVersion::Http11 => {
        reply::refusal(self.status(http), &[(ALLOW, allow.methods())], out)
      }
      _ => reply::refusal(self.status(http), &[], out),
    }
  }
}

/// Routes a request slates-http read (the module's list). The path's decoding is charged to
/// `admission`.
pub fn route(head: &Head<'_>, admission: &mut HttpAdmission) -> Result<Routed, RouteRefused> {
  let Target::Path { path, .. } = head.target else {
    return Err(RouteRefused::NotFound);
  };
  let relative = path.strip_prefix(b"/").ok_or(RouteRefused::NotFound)?;
  let decoded = decode_path(relative, admission)?;
  let (repo_len, rest) = split_lfs(&decoded).ok_or(RouteRefused::NotFound)?;
  let endpoint = endpoint(head.method, rest)?;
  let repo = repository(decoded.get(..repo_len).unwrap_or_default())?;
  check_media(head, &endpoint)?;
  Ok(Routed { repo, endpoint })
}

/// A path without its leading `/`, decoded and charged: refused when an escape is malformed or
/// names a slash or a NUL.
fn decode_path(raw: &[u8], admission: &mut HttpAdmission) -> Result<Vec<u8>, RouteRefused> {
  if percent::names_any(raw, PATH_REFUSED) {
    return Err(RouteRefused::BadPath);
  }
  let length = percent::decoded_len(raw).map_err(|_| RouteRefused::BadPath)?;
  admission
    .charge(to_u64(length))
    .map_err(|_| RouteRefused::PastRoom)?;
  let mut decoded = Vec::new();
  decoded
    .try_reserve_exact(length)
    .map_err(|_| RouteRefused::PastRoom)?;
  percent::decode_into(raw, Plus::Plus, &mut decoded).map_err(|_| RouteRefused::BadPath)?;
  Ok(decoded)
}

/// Where the repository path ends in a decoded path and what follows `/info/lfs`: the last
/// `/info/lfs` that is a whole segment pair, so a repository path never ends inside the suffix.
fn split_lfs(path: &[u8]) -> Option<(usize, &[u8])> {
  let at = path
    .windows(LFS_SUFFIX.len())
    .rposition(|window| window == LFS_SUFFIX)?;
  let rest = path.get(at.checked_add(LFS_SUFFIX.len())?..)?;
  (rest.is_empty() || rest.starts_with(b"/")).then_some((at, rest))
}

/// The endpoint `rest`, the path after `/info/lfs`, names for `method`.
fn endpoint(method: &[u8], rest: &[u8]) -> Result<Endpoint, RouteRefused> {
  let (endpoint, allow) = match super::route(rest) {
    Some(Route::Batch) => (post(method, Endpoint::Batch), Allow::Post),
    Some(Route::Locks) => (locks_endpoint(method), Allow::GetPost),
    Some(Route::LocksVerify) => (post(method, Endpoint::VerifyLocks), Allow::Post),
    Some(Route::Unlock { id }) => {
      let id = String::from_utf8(id.to_vec()).map_err(|_| RouteRefused::BadPath)?;
      (post(method, Endpoint::Unlock { id }), Allow::Post)
    }
    None => transfer_endpoint(method, rest)?,
  };
  endpoint.ok_or(RouteRefused::MethodNotAllowed(allow))
}

/// `endpoint` when `method` is `POST`.
fn post(method: &[u8], endpoint: Endpoint) -> Option<Endpoint> {
  (method == b"POST").then_some(endpoint)
}

/// `locks` by its method: `POST` creates, `GET` lists.
fn locks_endpoint(method: &[u8]) -> Option<Endpoint> {
  match method {
    b"POST" => Some(Endpoint::CreateLock),
    b"GET" => Some(Endpoint::ListLocks),
    _ => None,
  }
}

/// A transfer's endpoint, `objects/{oid}` or `objects/{oid}/verify`, for `method`.
fn transfer_endpoint(
  method: &[u8],
  rest: &[u8],
) -> Result<(Option<Endpoint>, Allow), RouteRefused> {
  let mut segments = rest
    .strip_prefix(b"/")
    .ok_or(RouteRefused::NotFound)?
    .split(|byte| *byte == b'/');
  let (Some(b"objects"), Some(oid), next, None) = (
    segments.next(),
    segments.next(),
    segments.next(),
    segments.next(),
  ) else {
    return Err(RouteRefused::NotFound);
  };
  let oid = Oid::from_hex(oid).ok_or(RouteRefused::NotFound)?;
  match (next, method) {
    (None, b"PUT") => Ok((Some(Endpoint::Upload(oid)), Allow::GetPut)),
    (None, b"GET") => Ok((Some(Endpoint::Download(oid)), Allow::GetPut)),
    (None, _) => Ok((None, Allow::GetPut)),
    (Some(b"verify"), _) => Ok((post(method, Endpoint::Verify(oid)), Allow::Post)),
    (Some(_), _) => Err(RouteRefused::NotFound),
  }
}

/// The repository's path: not empty, no empty, `.` or `..` segment, UTF-8.
fn repository(path: &[u8]) -> Result<String, RouteRefused> {
  if path.is_empty()
    || path
      .split(|byte| *byte == b'/')
      .any(|segment| matches!(segment, b"" | b"." | b".."))
  {
    return Err(RouteRefused::NotFound);
  }
  String::from_utf8(path.to_vec()).map_err(|_| RouteRefused::BadPath)
}

/// Whether `endpoint` carries a JSON body: everything but an upload and a download.
fn takes_json(endpoint: &Endpoint) -> bool {
  !matches!(
    endpoint,
    Endpoint::Upload(_) | Endpoint::Download(_) | Endpoint::ListLocks
  )
}

/// Whether `value` names LFS's media type, its case ignored and its parameters set aside.
fn is_lfs_type(value: &[u8]) -> bool {
  media_type(value).eq_ignore_ascii_case(MEDIA_TYPE.as_bytes())
}

/// A JSON endpoint's body must be LFS's media type, and an API answer's media type acceptable.
fn check_media(head: &Head<'_>, endpoint: &Endpoint) -> Result<(), RouteRefused> {
  if takes_json(endpoint) {
    let content_type = head.field(CONTENT_TYPE).map_err(RouteRefused::Head)?;
    if !content_type.is_some_and(is_lfs_type) {
      return Err(RouteRefused::UnsupportedMediaType);
    }
  }
  if matches!(endpoint, Endpoint::Upload(_) | Endpoint::Download(_)) || accepts_lfs(head) {
    return Ok(());
  }
  Err(RouteRefused::NotAcceptable)
}

/// How specifically a media range covers LFS's type, least first (RFC 9110 §12.5.1: "the most
/// specific reference has precedence").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Covers {
  /// `*/*`.
  Any,
  /// `application/*`.
  Application,
  /// The type itself.
  Exactly,
}

/// How specifically `range` covers LFS's type, or `None` when it does not.
fn covers(range: &[u8]) -> Option<Covers> {
  let named = media_type(range);
  if is_lfs_type(named) {
    Some(Covers::Exactly)
  } else if named.eq_ignore_ascii_case(APPLICATION_RANGE) {
    Some(Covers::Application)
  } else {
    (named == ANY_RANGE).then_some(Covers::Any)
  }
}

/// Whether `range` is weighted zero, which is "not acceptable" (RFC 9110 §12.4.2: `qvalue = ( "0"
/// [ "." 0*3DIGIT ] ) / ...`): its first `q` parameter is `0`, with only zeros after a decimal
/// point. A range with no `q` weighs one.
fn zero_weight(range: &[u8]) -> bool {
  let weight = range
    .split(|byte| *byte == b';')
    .skip(1)
    .find_map(|parameter| {
      let at = parameter.iter().position(|byte| *byte == b'=')?;
      let name = parameter.get(..at)?.trim_ascii();
      let value = parameter.get(at.checked_add(1)?..)?.trim_ascii();
      name.eq_ignore_ascii_case(WEIGHT).then_some(value)
    });
  weight
    .and_then(|value| value.strip_prefix(b"0"))
    .is_some_and(|rest| {
      rest.is_empty()
        || rest
          .strip_prefix(b".")
          .is_some_and(|digits| digits.iter().all(|digit| *digit == b'0'))
    })
}

/// Whether the request's `Accept` lets an answer carry LFS's type: it does when it lists no range,
/// and otherwise when one of the most specific ranges that cover the type is not weighted zero
/// (RFC 9110 §12.5.1). Empty list elements are not ranges (§5.6.1: a recipient "MUST accept empty
/// list elements" and they do not count).
fn accepts_lfs(head: &Head<'_>) -> bool {
  let ranges = head
    .fields_named(ACCEPT)
    .flat_map(|value| value.split(|byte| *byte == b','))
    .map(<[u8]>::trim_ascii)
    .filter(|range| !range.is_empty());
  let mut listed = false;
  let mut best: Option<(Covers, bool)> = None;
  for range in ranges {
    listed = true;
    let Some(covering) = covers(range) else {
      continue;
    };
    let weighted = !zero_weight(range);
    best = match best {
      Some((held, any)) if held == covering => Some((held, any || weighted)),
      Some((held, _)) if held > covering => best,
      _ => Some((covering, weighted)),
    };
  }
  !listed || best.is_some_and(|(_, weighted)| weighted)
}

/// The LFS server URL a repository's hrefs are made under:
/// `{scheme}://{authority}/{repo}/info/lfs`, the repository's path written back with RFC 3986's
/// escapes for every byte a path segment may not carry as itself.
pub fn base_url(scheme: &str, authority: &str, repo: &str) -> String {
  let mut url = format!("{scheme}://{authority}/");
  for byte in repo.bytes() {
    if byte.is_ascii_alphanumeric() || PATH_SAFE.contains(&byte) {
      url.push(char::from(byte));
    } else {
      url.push_str(&format!("%{byte:02X}"));
    }
  }
  url.push_str("/info/lfs");
  url
}

/// What the edge knows of a request beyond its head and body (A-625 supplies it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context<'a> {
  /// The LFS server URL the client named, the base of every href handed out ([`base_url`]).
  pub base: &'a str,
  /// The principal's name, as a lock's owner shows it.
  pub owner: &'a str,
  /// Whether the request carried credentials: a refusal of one that did not asks for them.
  pub authenticated: bool,
  /// The challenge a `401` names in `LFS-Authenticate`, as the edge authenticates.
  pub challenge: Option<&'a str>,
  /// When the credential that asked expires: a bearer token's expiry; `None` for one that never
  /// does (Basic).
  pub credential_expiry: Option<Timestamp>,
  /// The time, to the second: when a lock is taken.
  pub now: Timestamp,
  /// The request's id, for whoever reads the server's records, named in an error's body.
  pub request_id: Option<&'a str>,
}

/// What a request needs the principal to be allowed: the server maps each to vcs_policy's rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
  /// Download objects: the batch API's `download`, a `GET` of an object.
  Download,
  /// Upload objects: the batch API's `upload`, a `PUT`, a verification.
  Upload,
  /// List locks (locking.md: "at least pull access").
  ListLocks,
  /// Take a lock (locking.md: "push access").
  Lock,
  /// List locks for a push's verification (locking.md: "push access").
  VerifyLocks,
  /// Release one's own lock (locking.md: "push access").
  Unlock,
  /// Release another user's lock with `force` (locking.md: "unless the `force` property is
  /// given").
  UnlockOthers,
}

/// Who may do what: vcs_policy's check (A-597), taken as a predicate.
pub trait Access {
  /// Whether the request's principal may have `need` on `ref_name`, or on the repository's whole
  /// namespace when it is `None`.
  fn allows(&self, need: Need, ref_name: Option<&str>) -> bool;
}

impl<F: Fn(Need, Option<&str>) -> bool> Access for F {
  fn allows(&self, need: Need, ref_name: Option<&str>) -> bool {
    self(need, ref_name)
  }
}

/// A repository's LFS objects, as the server serves them.
pub trait Objects {
  /// The size of the object `oid`, when held.
  fn held(&self, oid: &Oid) -> Option<u64>;
  /// The bytes of the object `oid`, when held.
  fn bytes(&self, oid: &Oid) -> Option<Vec<u8>>;
  /// Keeps `bytes` as the object `oid`, checked against it.
  fn admit(&mut self, oid: &Oid, bytes: &[u8]) -> Result<Admitted, ObjectRefused>;
}

/// A repository's LFS objects with the shared store they live in.
#[derive(Debug)]
pub struct Stored<'s> {
  /// The shared content store.
  pub store: &'s mut ContentStore,
  /// The repository's LFS index.
  pub objects: &'s mut LfsObjects,
}

impl Objects for Stored<'_> {
  fn held(&self, oid: &Oid) -> Option<u64> {
    self.objects.held(self.store, oid)
  }

  fn bytes(&self, oid: &Oid) -> Option<Vec<u8>> {
    self.objects.bytes(self.store, oid)
  }

  fn admit(&mut self, oid: &Oid, bytes: &[u8]) -> Result<Admitted, ObjectRefused> {
    self.objects.admit(self.store, oid, bytes)
  }
}

/// A lock as the repository keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockRecord {
  /// Its id, of RFC 3986's unreserved characters alone, since git-lfs writes it into a path
  /// unescaped.
  pub id: String,
  /// The path it locks.
  pub path: String,
  /// Who took it, by name.
  pub owner: String,
  /// When.
  pub locked_at: Timestamp,
}

impl LockRecord {
  /// The record as an answer writes it.
  pub fn view(&self) -> Lock<'_> {
    Lock {
      id: &self.id,
      path: &self.path,
      locked_at: self.locked_at,
      owner: Some(&self.owner),
    }
  }
}

/// What taking a lock did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Taken {
  /// The lock was taken.
  New(LockRecord),
  /// The path was locked already, by this lock.
  Held(LockRecord),
}

/// The lock records could not change: their store is at its capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocksFull;

/// A repository's locks, at most one a path: the repository's lock log (A-642), each change guarded
/// then applied, its order the order the locks were taken in. A lock's id names its place in the
/// log, so a cursor naming a lock released since still names a place.
pub trait Locks {
  /// Takes a lock on `path` for `owner` at `at`, or answers the lock that holds the path.
  fn take(&mut self, path: &str, owner: &str, at: Timestamp) -> Result<Taken, LocksFull>;
  /// The lock `id`.
  fn find(&self, id: &str) -> Option<&LockRecord>;
  /// The locks held in the log's order, from the place `from` names (inclusive) or from the first;
  /// `None` when `from` names no place in the log.
  fn listed_from(&self, from: Option<&str>) -> Option<impl Iterator<Item = &LockRecord>>;
  /// Releases the lock `id`, answering it, or `None` when there is none.
  fn release(&mut self, id: &str) -> Result<Option<LockRecord>, LocksFull>;
}

/// A request routed and read: its endpoint, its query and its body.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
  /// What it asks.
  pub endpoint: &'a Endpoint,
  /// Its query, without the `?` (a lock listing's filters).
  pub query: &'a [u8],
  /// Its body, whole.
  pub body: &'a [u8],
}

/// What an answer's body is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
  /// No body.
  Empty,
  /// LFS's JSON.
  Json,
  /// An object's bytes.
  Object,
}

/// An answer, made whole: its status, what its body is, the challenge of a `401`, and the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
  /// The status.
  pub status: Status,
  /// What the body is.
  pub kind: Kind,
  /// `LFS-Authenticate`'s value, for a `401`.
  pub challenge: Option<String>,
  /// The body.
  pub body: Vec<u8>,
}

impl Answer {
  /// Writes the answer: its head (its media type, `LFS-Authenticate` for a `401`, its length) and
  /// its body; the connection closed after it when `close`.
  pub fn reply(&self, close: bool, out: &mut Vec<u8>) -> Result<(), BadField> {
    let mut fields: Vec<(&[u8], &[u8])> = Vec::new();
    match self.kind {
      Kind::Json => fields.push((CONTENT_TYPE, MEDIA_TYPE.as_bytes())),
      Kind::Object => fields.push((CONTENT_TYPE, OCTET_STREAM)),
      Kind::Empty => {}
    }
    if let Some(challenge) = &self.challenge {
      fields.push((LFS_AUTHENTICATE, challenge.as_bytes()));
    }
    reply::head(
      self.status,
      &fields,
      Body::Length(to_u64(self.body.len())),
      close,
      out,
    )?;
    out.extend_from_slice(&self.body);
    Ok(())
  }
}

/// An answer with no body.
fn empty(status: Status) -> Answer {
  Answer {
    status,
    kind: Kind::Empty,
    challenge: None,
    body: Vec::new(),
  }
}

/// A JSON answer of `body`.
fn json_body(status: Status, body: Vec<u8>) -> Answer {
  Answer {
    status,
    kind: Kind::Json,
    challenge: None,
    body,
  }
}

/// An error's answer: `{"message": ...}` with the request's id.
fn message(status: Status, text: &str, context: &Context<'_>) -> Answer {
  let mut body = Vec::new();
  write_error(
    &ErrorBody {
      message: text,
      request_id: context.request_id,
      documentation_url: None,
    },
    &mut body,
  );
  json_body(status, body)
}

/// The answer to a request the principal may not make: `401` asking for credentials when it sent
/// none, `403` when it did.
fn denied(context: &Context<'_>) -> Answer {
  if context.authenticated {
    return message(Status::FORBIDDEN, "Forbidden", context);
  }
  let mut answer = message(Status::UNAUTHORIZED, "Credentials needed", context);
  answer.challenge = context.challenge.map(String::from);
  answer
}

/// The answer to a body of `need` bytes past the room: `413` (batch.md: "the request was otherwise
/// too large").
fn too_large(context: &Context<'_>) -> Answer {
  message(Status::CONTENT_TOO_LARGE, "Request too large", context)
}

/// The answer to a body that could not be allocated at its measured length.
fn unwritten(context: &Context<'_>) -> Answer {
  message(INTERNAL, "No memory for the answer", context)
}

/// An empty body with room for exactly `len` bytes, or `None` when the memory cannot be had.
fn body_of(len: u64) -> Option<Vec<u8>> {
  let len = usize::try_from(len).ok()?;
  let mut body = Vec::new();
  body.try_reserve_exact(len).ok()?;
  Some(body)
}

/// A JSON string's text, owned within `admission`, or `None` past it.
fn text_of(string: JsonStr<'_>, admission: &mut Admission) -> Option<String> {
  string.owned(admission).ok()
}

/// The server's three seams over one repository, and the request answered against them.
pub struct Server<'s, A: Access, O: Objects, L: Locks> {
  /// Who may do what.
  pub access: &'s A,
  /// The repository's objects.
  pub objects: &'s mut O,
  /// The repository's locks.
  pub locks: &'s mut L,
}

impl<A: Access, O: Objects, L: Locks> Server<'_, A, O, L> {
  /// Answers `request` (the module's list), every byte it builds charged to `admission`.
  pub fn answer(
    &mut self,
    request: &Request<'_>,
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    match request.endpoint {
      Endpoint::Batch => self.batch(request.body, context, admission),
      Endpoint::Upload(oid) => self.upload(oid, request.body, context),
      Endpoint::Download(oid) => self.download(oid, context, admission),
      Endpoint::Verify(oid) => self.verify(oid, request.body, context, admission),
      Endpoint::CreateLock => self.create_lock(request.body, context, admission),
      Endpoint::ListLocks => self.list_locks(request.query, context, admission),
      Endpoint::VerifyLocks => self.verify_locks(request.body, context, admission),
      Endpoint::Unlock { id } => self.unlock(id, request.body, context, admission),
    }
  }

  /// The batch API.
  fn batch(&mut self, body: &[u8], context: &Context<'_>, admission: &mut Admission) -> Answer {
    let request = match batch::read_request(body, admission) {
      Ok(request) => request,
      Err(refused) => return batch_refused(refused, context),
    };
    let ref_name = match request.ref_name {
      Some(name) => match text_of(name, admission) {
        Some(text) => Some(text),
        None => return too_large(context),
      },
      None => None,
    };
    let need = match request.operation {
      Operation::Download => Need::Download,
      Operation::Upload => Need::Upload,
    };
    if !self.access.allows(need, ref_name.as_deref()) {
      return denied(context);
    }
    let basic =
      request.transfers.is_empty() || request.transfers.iter().any(|name| name.is(batch::BASIC));
    if !basic {
      let text = "No transfer adapter offered is basic";
      return message(UNPROCESSABLE, text, context);
    }
    self.batch_answer(request.operation, &request.objects, context, admission)
  }

  /// The batch API's answer for `objects`: its hrefs, the answers' slots and its body charged
  /// before any is made.
  fn batch_answer(
    &mut self,
    operation: Operation,
    objects: &[batch::Object],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    let href_len = context
      .base
      .len()
      .saturating_add(OBJECTS_PATH.len())
      .saturating_add(super::OID_HEX_LEN);
    let per_object = href_len.saturating_mul(2).saturating_add(VERIFY_PATH.len());
    let made = admission
      .charge(to_u64(per_object.saturating_mul(objects.len())))
      .and_then(|()| admission.charge_items::<(String, String)>(objects.len()))
      .and_then(|()| admission.charge_items::<ObjectAnswer<'_>>(objects.len()));
    if made.is_err() {
      return too_large(context);
    }
    let hrefs: Vec<(String, String)> = objects
      .iter()
      .map(|object| {
        let href = format!("{}{OBJECTS_PATH}{}", context.base, object.oid);
        let verify = format!("{href}{VERIFY_PATH}");
        (href, verify)
      })
      .collect();
    let answers: Vec<ObjectAnswer<'_>> = objects
      .iter()
      .zip(&hrefs)
      .map(|(object, (href, verify))| self.object_answer(operation, object, href, verify, context))
      .collect();
    let mut measure = Measure::default();
    if batch::write_response(&answers, &mut measure).is_err()
      || admission.charge(measure.0).is_err()
    {
      return too_large(context);
    }
    let Some(mut body) = body_of(measure.0) else {
      return unwritten(context);
    };
    match batch::write_response(&answers, &mut body) {
      Ok(()) => json_body(Status::OK, body),
      Err(_) => message(INTERNAL, "Answer not written", context),
    }
  }

  /// One object's answer.
  fn object_answer<'h>(
    &self,
    operation: Operation,
    object: &batch::Object,
    href: &'h str,
    verify: &'h str,
    context: &Context<'_>,
  ) -> ObjectAnswer<'h> {
    let action = |href| Action {
      href,
      header: &[],
      expires_in: None,
      expires_at: context.credential_expiry,
    };
    let mismatch = Outcome::Error(ObjectError {
      code: ErrorCode::Validation,
      message: "Object size does not match",
    });
    let outcome = match (operation, self.objects.held(&object.oid)) {
      (Operation::Download, Some(size)) if size == object.size => Outcome::Download(action(href)),
      (Operation::Download, None) => Outcome::Error(ObjectError {
        code: ErrorCode::NotFound,
        message: "Object does not exist",
      }),
      (Operation::Upload, Some(size)) if size == object.size => Outcome::Held,
      (Operation::Upload, None) => Outcome::Upload {
        upload: action(href),
        verify: Some(action(verify)),
      },
      (_, Some(_)) => mismatch,
    };
    ObjectAnswer {
      oid: object.oid,
      size: object.size,
      authenticated: false,
      outcome,
    }
  }

  /// An upload: the bytes kept as the object, checked against its oid.
  fn upload(&mut self, oid: &Oid, body: &[u8], context: &Context<'_>) -> Answer {
    if !self.access.allows(Need::Upload, None) {
      return denied(context);
    }
    match self.objects.admit(oid, body) {
      Ok(_) => empty(Status::OK),
      Err(ObjectRefused::Mismatch) => {
        message(UNPROCESSABLE, "Content does not match its oid", context)
      }
      Err(ObjectRefused::Store(ContentRefused::OverBudget { .. })) => {
        message(INSUFFICIENT_STORAGE, "No room for the object", context)
      }
      Err(_) => message(INTERNAL, "Object not kept", context),
    }
  }

  /// A download: the object's bytes, charged before they are read.
  fn download(&mut self, oid: &Oid, context: &Context<'_>, admission: &mut Admission) -> Answer {
    if !self.access.allows(Need::Download, None) {
      return denied(context);
    }
    let Some(size) = self.objects.held(oid) else {
      return message(Status::NOT_FOUND, "Object does not exist", context);
    };
    if admission.charge(size).is_err() {
      return too_large(context);
    }
    match self.objects.bytes(oid) {
      Some(bytes) => Answer {
        status: Status::OK,
        kind: Kind::Object,
        challenge: None,
        body: bytes,
      },
      None => message(INTERNAL, "Object not read", context),
    }
  }

  /// A verification: the object the client just uploaded is held, at its size.
  fn verify(
    &mut self,
    oid: &Oid,
    body: &[u8],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    if !self.access.allows(Need::Upload, None) {
      return denied(context);
    }
    let object = match batch::read_verify(body, admission) {
      Ok(object) => object,
      Err(refused) => return batch_refused(refused, context),
    };
    if object.oid != *oid {
      return message(UNPROCESSABLE, "The body names another object", context);
    }
    match self.objects.held(oid) {
      Some(size) if size == object.size => empty(Status::OK),
      Some(_) => message(UNPROCESSABLE, "Object size does not match", context),
      None => message(Status::NOT_FOUND, "Object does not exist", context),
    }
  }

  /// A lock's creation.
  fn create_lock(
    &mut self,
    body: &[u8],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    let request = match locks::read_create(body, admission) {
      Ok(request) => request,
      Err(refused) => return locks_refused(refused, context),
    };
    let path = text_of(request.path, admission);
    let ref_name = request.ref_name.map(|name| text_of(name, admission));
    let (Some(path), Some(ref_name)) = (path, ref_name.transpose_room()) else {
      return too_large(context);
    };
    if !self.access.allows(Need::Lock, ref_name.as_deref()) {
      return denied(context);
    }
    if path.is_empty() {
      return message(UNPROCESSABLE, "A lock names a path", context);
    }
    match self.locks.take(&path, context.owner, context.now) {
      Ok(Taken::New(record)) => lock_answer(CREATED, &record, context, admission),
      Ok(Taken::Held(record)) => conflict(&record, context, admission),
      Err(LocksFull) => message(INSUFFICIENT_STORAGE, "No room for the lock", context),
    }
  }

  /// A lock listing.
  fn list_locks(
    &mut self,
    query: &[u8],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    let list = match locks::read_list_query(query) {
      Ok(list) => list,
      Err(locks::QueryRefusal::Limit { .. }) => {
        return message(UNPROCESSABLE, "Validation error", context);
      }
      Err(_) => return message(Status::BAD_REQUEST, "Malformed query", context),
    };
    let owned = |value: Option<locks::QueryStr<'_>>, admission: &mut Admission| {
      value.map(|value| value.owned(admission).ok())
    };
    let (path, id, cursor, refspec) = (
      owned(list.path, admission),
      owned(list.id, admission),
      owned(list.cursor, admission),
      owned(list.refspec, admission),
    );
    let (Some(path), Some(id), Some(cursor), Some(refspec)) = (
      path.transpose_room(),
      id.transpose_room(),
      cursor.transpose_room(),
      refspec.transpose_room(),
    ) else {
      return too_large(context);
    };
    if !self.access.allows(Need::ListLocks, refspec.as_deref()) {
      return denied(context);
    }
    let filter = |record: &LockRecord| {
      path.as_ref().is_none_or(|path| record.path == *path)
        && id.as_ref().is_none_or(|id| record.id == *id)
    };
    let mut frame = Measure::default();
    locks::write_list(&[], None, &mut frame);
    let cut = page(
      &*self.locks,
      cursor.as_deref(),
      list.limit,
      frame.0,
      admission,
      filter,
    );
    let page = match cut {
      Ok(page) => page,
      Err(refused) => return page_refused(refused, context),
    };
    let views: Vec<Lock<'_>> = page.locks.iter().map(|record| record.view()).collect();
    let mut measure = Measure::default();
    locks::write_list(&views, page.next, &mut measure);
    let Some(mut body) = body_of(measure.0) else {
      return unwritten(context);
    };
    locks::write_list(&views, page.next, &mut body);
    json_body(Status::OK, body)
  }

  /// A listing for a push's verification: the locks split into the caller's and the others'.
  fn verify_locks(
    &mut self,
    body: &[u8],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    let request = match locks::read_verify(body, admission) {
      Ok(request) => request,
      Err(refused) => return locks_refused(refused, context),
    };
    let (ref_name, cursor) = (
      request.ref_name.map(|name| text_of(name, admission)),
      request.cursor.map(|cursor| text_of(cursor, admission)),
    );
    let (Some(ref_name), Some(cursor)) = (ref_name.transpose_room(), cursor.transpose_room())
    else {
      return too_large(context);
    };
    if !self.access.allows(Need::VerifyLocks, ref_name.as_deref()) {
      return denied(context);
    }
    let mut frame = Measure::default();
    locks::write_verify(&[], &[], None, &mut frame);
    let cut = page(
      &*self.locks,
      cursor.as_deref(),
      request.limit,
      frame.0,
      admission,
      |_| true,
    );
    let page = match cut {
      Ok(page) => page,
      Err(refused) => return page_refused(refused, context),
    };
    let (ours, theirs): (Vec<Lock<'_>>, Vec<Lock<'_>>) = page
      .locks
      .iter()
      .map(|record| record.view())
      .partition(|view| view.owner == Some(context.owner));
    let mut measure = Measure::default();
    locks::write_verify(&ours, &theirs, page.next, &mut measure);
    let Some(mut body) = body_of(measure.0) else {
      return unwritten(context);
    };
    locks::write_verify(&ours, &theirs, page.next, &mut body);
    json_body(Status::OK, body)
  }

  /// A lock's release: one's own, or another's with `force` where the caller may.
  fn unlock(
    &mut self,
    id: &str,
    body: &[u8],
    context: &Context<'_>,
    admission: &mut Admission,
  ) -> Answer {
    let request = match locks::read_unlock(body, admission) {
      Ok(request) => request,
      Err(refused) => return locks_refused(refused, context),
    };
    let Some(ref_name) = request
      .ref_name
      .map(|name| text_of(name, admission))
      .transpose_room()
    else {
      return too_large(context);
    };
    if !self.access.allows(Need::Unlock, ref_name.as_deref()) {
      return denied(context);
    }
    let found = self.locks.find(id);
    let Some(theirs) = found.map(|record| record.owner != context.owner) else {
      return message(Status::NOT_FOUND, "Lock not found", context);
    };
    if theirs {
      if !request.force {
        let text = "The lock is another user's: release it with force";
        return message(Status::FORBIDDEN, text, context);
      }
      if !self.access.allows(Need::UnlockOthers, ref_name.as_deref()) {
        return denied(context);
      }
    }
    match self.locks.release(id) {
      Ok(Some(record)) => lock_answer(Status::OK, &record, context, admission),
      Ok(None) => message(Status::NOT_FOUND, "Lock not found", context),
      Err(LocksFull) => message(INSUFFICIENT_STORAGE, "No room for the release", context),
    }
  }
}

/// Derived: the bytes a page holds for each lock beside the lock's place in the answer: the borrow
/// of its record and the view the answer is written from.
const PAGE_SLOT: usize =
  core::mem::size_of::<&LockRecord>().saturating_add(core::mem::size_of::<Lock<'static>>());

/// A page of locks borrowed from the log, and the id of the lock that continues it.
#[derive(Debug, Default)]
struct Page<'l> {
  /// The locks.
  locks: Vec<&'l LockRecord>,
  /// The next lock's id, when more follow.
  next: Option<&'l str>,
}

/// Why a page of locks was not cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PageRefused {
  /// The cursor names no place in the log.
  Cursor,
  /// The room holds not even the page's frame and one lock with the cursor after it.
  Room,
}

/// The answer to a page that was not cut: `422` for a cursor this server never handed out, `413`
/// for a room too small for one lock.
fn page_refused(refused: PageRefused, context: &Context<'_>) -> Answer {
  match refused {
    PageRefused::Cursor => message(UNPROCESSABLE, "Unknown cursor", context),
    PageRefused::Room => too_large(context),
  }
}

/// A page of the locks `keep` keeps, from the place `cursor` names on, charged to `admission` (the
/// module's bounds): its frame of `frame` bytes first; then each lock while the client's `limit`
/// allows it and the room holds it and a cursor naming the lock after it; then the cursor that
/// ends it, which that keeps room for.
fn page<'l, L: Locks>(
  locks: &'l L,
  cursor: Option<&str>,
  limit: Option<u64>,
  frame: u64,
  admission: &mut Admission,
  keep: impl Fn(&LockRecord) -> bool,
) -> Result<Page<'l>, PageRefused> {
  let limit = limit.filter(|limit| *limit > 0).unwrap_or(u64::MAX);
  let listed = locks.listed_from(cursor).ok_or(PageRefused::Cursor)?;
  admission.charge(frame).map_err(|_| PageRefused::Room)?;
  let mut kept = listed.filter(|record| keep(record)).peekable();
  let mut page = Page::default();
  while let Some(record) = kept.next() {
    let after = kept.peek().map_or(0, |next| locks::cursor_len(&next.id));
    if to_u64(page.locks.len()) >= limit || !charge_place(record, after, admission) {
      page.next = Some(record.id.as_str());
      break;
    }
    page.locks.push(record);
  }
  if let Some(next) = page.next {
    if page.locks.is_empty() || admission.charge(locks::cursor_len(next)).is_err() {
      return Err(PageRefused::Room);
    }
  }
  Ok(page)
}

/// Charges `record`'s place on a page, with its slots, when the room holds that and `after` bytes
/// more: the cursor that would name the lock after it, were the page cut there.
fn charge_place(record: &LockRecord, after: u64, admission: &mut Admission) -> bool {
  let place = locks::listed_len(&record.view()).saturating_add(to_u64(PAGE_SLOT));
  place.saturating_add(after) <= admission.left() && admission.charge(place).is_ok()
}

/// An owned text read within the room: `None` past it.
trait TransposeRoom {
  /// The text, absent, or `None` when it could not be held.
  fn transpose_room(self) -> Option<Option<String>>;
}

impl TransposeRoom for Option<Option<String>> {
  fn transpose_room(self) -> Option<Option<String>> {
    match self {
      None => Some(None),
      Some(Some(text)) => Some(Some(text)),
      Some(None) => None,
    }
  }
}

/// A lock's answer, `{"lock": ...}`, with `status`.
fn lock_answer(
  status: Status,
  record: &LockRecord,
  context: &Context<'_>,
  admission: &mut Admission,
) -> Answer {
  let mut measure = Measure::default();
  locks::write_lock(&record.view(), &mut measure);
  if admission.charge(measure.0).is_err() {
    return too_large(context);
  }
  let Some(mut body) = body_of(measure.0) else {
    return unwritten(context);
  };
  locks::write_lock(&record.view(), &mut body);
  json_body(status, body)
}

/// The answer to a path locked already: `409` with the lock that holds it.
fn conflict(record: &LockRecord, context: &Context<'_>, admission: &mut Admission) -> Answer {
  let error = ErrorBody {
    message: "already created lock",
    request_id: context.request_id,
    documentation_url: None,
  };
  let mut measure = Measure::default();
  locks::write_conflict(&record.view(), &error, &mut measure);
  if admission.charge(measure.0).is_err() {
    return too_large(context);
  }
  let Some(mut body) = body_of(measure.0) else {
    return unwritten(context);
  };
  locks::write_conflict(&record.view(), &error, &mut body);
  json_body(CONFLICT, body)
}

/// The answer to a batch request or a verification that was not read: `400` for a body that is no
/// request, `422` for one whose values are refused, `413` past the room (its lists, or its JSON's
/// nesting).
fn batch_refused(refused: batch::Refusal, context: &Context<'_>) -> Answer {
  match refused {
    batch::Refusal::Json(Nesting { .. }) | batch::Refusal::PastAdmission(_) => too_large(context),
    batch::Refusal::Json(_)
    | batch::Refusal::Type { .. }
    | batch::Refusal::Missing { .. }
    | batch::Refusal::Repeated { .. } => message(Status::BAD_REQUEST, "Malformed request", context),
    _ => message(UNPROCESSABLE, "Validation error", context),
  }
}

/// The answer to a locks request that was not read: `400`, `422` for a limit refused, `413` past
/// the room.
fn locks_refused(refused: locks::Refusal, context: &Context<'_>) -> Answer {
  match refused {
    locks::Refusal::Json(Nesting { .. }) => too_large(context),
    locks::Refusal::NegativeLimit { .. } | locks::Refusal::Limit { .. } => {
      message(UNPROCESSABLE, "Validation error", context)
    }
    _ => message(Status::BAD_REQUEST, "Malformed request", context),
  }
}

#[cfg(test)]
#[path = "../tests/lfs_serve.rs"]
mod tests;
