//! Subversion's site on the HTTP edge (A-681 part 1; §4.17.4 item 5 "HTTP", item 10 and piece 13): each request's head
//! routed as `mod_dav_svn` routes a repository under `SVNParentPath` (the URL's first segment names the repository),
//! its credentials proved to a principal of §4.13 (git's [`Authenticator`]), its repository found ([`Repositories`]),
//! the principal's reach of it decided by the repository's policy (`crate::vcs_policy::reaches`, A-597, A-622), and
//! the request answered by `slates_svn::dav::site` from the repository's index, green and lock table, which its owner
//! shard lends a step at a time ([`SvnStores`]) with the principal's compiled read rules (A-606), each path the answer
//! names judged by them (`mod_authz_svn`'s questions).
//!
//! **The order**, each refusal with its status:
//! 1. The route. A path whose escapes are malformed or name a slash or a NUL is `400`; a path past the request's room is
//!    `414`; a path naming no repository (the server's root) is `404`.
//! 2. The credentials, judged as git's and Mercurial's sites judge them: none, or none proving a principal, is `401`
//!    with the challenges the connection accepts (Basic over TLS alone, RFC 7617 §4: stock svn sends Basic after one
//!    challenge and on every request after, as recorded); a repeated or malformed `Authorization` is `400`; a credential
//!    record that did not answer is `503`.
//! 3. The repository's shard. A repository another shard holds moves the connection there whole (`Answer::Move`).
//! 4. The repository. None of that name, or one of another tool, is `404`.
//! 5. The policy. A principal the policy lets read none of the repository is `403`, for every request.
//! 6. The store. A repository whose store this shard does not hold is `503` (the daemon lends none yet: [`NoSvnStores`]).
//!
//! A served request's body is read whole within the request's room (`413` past it), then answered at its end from the
//! store as `slates_svn::dav::site` decides: its status, which the reply names as it begins (`207` for a PROPFIND), its
//! headers and its body; a file's text a piece a reply run, lent from the green each run, never held whole; a REPORT's
//! answer a slice of the room a reply run, the store lent again for each, its first slices stepped at the body's end
//! (the task yielding between them) until it has written its first bytes or ended, since they decide its status: a
//! walk refused before its first byte is answered with its error, as `mod_dav` answers one, and one refused after it
//! ends the connection (a refusal within those first slices after the first byte is `500`, nothing sent).
//!
//! **Names.** A repository is the one named by the URL's first path segment, decoded; the rest of the path is below
//! its root (`/repo/!svn/rvr/5/trunk` is `repo`'s `/!svn/rvr/5/trunk`). A repository whose name holds a slash cannot be
//! reached over HTTP: `SVNParentPath`'s rule, which a name routed to its partition by its hash cannot widen to a
//! longest-prefix search without asking every partition (GAPS). A URL's query is not read: `mod_dav_svn`'s `?p=` and
//! `?r=` for a browser's GET are not served yet.
//!
//! **Bounds.** The exchange holds the request's path, its `Depth` and its body, each charged to the request's room
//! before it is held. The XML reader's bounds ([`xml_bounds`]) are derived from the same room: a body of the room's
//! bytes, and as many elements, nesting levels and attributes as parsed elements fit in the room, so the parsed body
//! holds at most the room again. An answer is written within the room; one past it is `500`.

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VcsTool};
use slates_http::Admission;
use slates_http::head::{Head, Target};
use slates_http::percent::{self, Plus};
use slates_http::reply::Status;
use slates_http_edge::connection::Fields;
use slates_http_edge::credential;
use slates_rt::futures::yield_now;
use slates_svn::authz::PrincipalRules;
use slates_svn::dav::Bounds as XmlBounds;
use slates_svn::dav::site::{
  self, Answer as SiteAnswer, Headers, Report, ReportProgress, Request, Served,
};
use slates_svn::dav::xml::Element;
use slates_svn::index::{Index, Text};
use slates_svn::lock::{AprTime, LockTable};
use slates_svn::reads::Repository;

use super::git::{Authenticator, Located, Repositories, Unproven};
use super::{Answer, Decided, Exchange, Failed, Site};
use crate::vcs_policy::{self, Right};

/// Format: the bytes an escape may not name in a repository's path: a slash, which would move a segment's boundary,
/// and NUL.
const PATH_REFUSED: &[u8] = b"/\0";

/// Format: `HEAD`, whose reply is the head alone.
const HEAD: &[u8] = b"HEAD";

/// Format: the `Depth` header's name (RFC 4918 §10.2).
const DEPTH: &[u8] = b"depth";

/// A count as `u64`, saturating where it could not fit (no target slates ships has a wider `usize`).
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// A count as `usize`, saturating where it could not fit.
fn to_usize(count: u64) -> usize {
  usize::try_from(count).unwrap_or(usize::MAX)
}

/// Derived: the XML reader's bounds for a request of `room` bytes (the module doc): the body the room; a name the room;
/// elements, nesting levels and attributes as many as parsed elements (`size_of::<Element>()` each) fit in the room.
pub(crate) fn xml_bounds(room: u64) -> XmlBounds {
  let bytes = to_usize(room);
  let elements = bytes / size_of::<Element>().max(1);
  XmlBounds {
    body_bytes: bytes,
    depth: elements,
    name_bytes: bytes,
    attributes: elements,
    elements,
  }
}

/// What a repository's store lends for one step: its index, its green, its UUID, its locks, the principal's compiled
/// read rules (none where the repository names no authorization file) and the instant locks are judged at.
pub(crate) struct Lent<'a, R: ?Sized> {
  /// The index.
  pub(crate) index: &'a Index,
  /// The green.
  pub(crate) repository: &'a R,
  /// The repository's UUID.
  pub(crate) uuid: &'a str,
  /// Its locks.
  pub(crate) locks: &'a LockTable,
  /// The principal's compiled read rules.
  pub(crate) rules: Option<&'a PrincipalRules>,
  /// The instant a lock's expiry is judged at, on the owner's clock.
  pub(crate) now: AprTime,
}

/// Where Subversion repositories' stores are lent on this shard (A-662: each repository's tool store, kept on its owner
/// shard with its record), afresh for each step, as git's and Mercurial's sites lend theirs, so a shard's state is never
/// held across a wait.
pub(crate) trait SvnStores {
  /// The green a repository's requests are answered from.
  type Repository: Repository + ?Sized;
  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;
  /// Runs `step` over repository `id`'s store as `principal` reads it; none when it is not held here.
  fn lend<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Self::Repository>) -> T,
  ) -> Option<T>;
}

/// The stores as the daemon has them until the site is handed Subversion's (each repository's index and green on its
/// owner shard, its lock table, its authorization file and a principal's Subversion user): none, so every Subversion
/// request a principal may make is `503`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoSvnStores;

/// The green no repository has yet: no value has this type.
pub(crate) enum NoGreen {}

impl slates_svn::mergeinfo::Contents for NoGreen {
  fn entries(&self, _directory: &str, _revision: u64) -> Option<Vec<String>> {
    match *self {}
  }

  fn mergeinfo_value(&self, _properties: &[u8; 32]) -> Option<&[u8]> {
    match *self {}
  }
}

impl Repository for NoGreen {
  fn revision_properties(&self, _revision: u64) -> Option<Vec<(&[u8], &[u8])>> {
    match *self {}
  }

  fn properties(&self, _identity: &[u8; 32]) -> Option<Vec<(&[u8], &[u8])>> {
    match *self {}
  }

  fn text(&self, _identity: &[u8; 32], _offset: u64) -> Option<&[u8]> {
    match *self {}
  }
}

impl SvnStores for NoSvnStores {
  type Repository = NoGreen;

  fn holds(&self, _id: RepositoryId) -> bool {
    false
  }

  fn lend<T>(
    &mut self,
    _id: RepositoryId,
    _principal: &Principal,
    _step: impl FnOnce(&Lent<'_, NoGreen>) -> T,
  ) -> Option<T> {
    None
  }
}

/// What a routed head names: its repository, and the path below its root.
struct Routed {
  /// The repository's name.
  name: String,
  /// The decoded path below the repository's root.
  path: String,
}

/// What a request's credentials came to.
enum Proof {
  /// The principal they prove.
  Principal(Principal),
  /// None proved: `401` with the challenges.
  Unauthorized,
  /// None decided, with this status: the `Authorization` line itself refused (`400`), or a credential record that did
  /// not answer (`503`).
  Refused(Status),
}

/// Subversion's site: the realm its challenges name, the room a request is charged to, and the authenticator,
/// repositories and stores it decides and answers with (the module doc).
pub(crate) struct SvnSite<A, R, S> {
  /// The realm a `401`'s challenges name.
  realm: String,
  /// The room a request's route and exchange are each charged to: the edge's request room.
  room: u64,
  /// What proves credentials.
  authenticator: A,
  /// Where repositories are found.
  repositories: R,
  /// Where their stores are read.
  stores: S,
}

impl<A: Authenticator, R: Repositories, S: SvnStores + Clone> SvnSite<A, R, S> {
  /// A site naming `realm` in its challenges, charging each request to `room`.
  pub(crate) fn new(
    realm: String,
    room: u64,
    authenticator: A,
    repositories: R,
    stores: S,
  ) -> Self {
    Self {
      realm,
      room,
      authenticator,
      repositories,
      stores,
    }
  }

  /// The answer to `head`, on a connection that is TLS when `secure`, in the module doc's order.
  pub(crate) async fn answer(&mut self, head: &Head<'_>, secure: bool) -> Decided<SvnExchange<S>> {
    let routed = match route(head, self.room) {
      Ok(routed) => routed,
      Err(status) => return refused_with(status, Fields::new(), false),
    };
    let principal = match self.proof(head, secure).await {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(secure),
      Proof::Refused(status) => return refused_with(status, Fields::new(), false),
    };
    if let Located::Elsewhere(shard) = self.repositories.locate(&routed.name) {
      return Decided {
        answer: Answer::Move { shard },
        proven: true,
      };
    }
    let Some(repository) = self
      .repositories
      .find(&routed.name)
      .filter(|repository| repository.tool == VcsTool::Subversion)
    else {
      return refused_with(Status::NOT_FOUND, Fields::new(), true);
    };
    match vcs_policy::reaches(&repository, &principal, &[Right::Read]) {
      Ok(true) => {}
      Ok(false) => return refused_with(Status::FORBIDDEN, Fields::new(), true),
      // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
      Err(_) => return refused_with(Status::INTERNAL_SERVER_ERROR, Fields::new(), true),
    }
    if !self.stores.holds(repository.id) {
      return refused_with(Status::SERVICE_UNAVAILABLE, Fields::new(), true);
    }
    let asked = Asked {
      head,
      routed,
      principal,
    };
    match SvnExchange::new(self.stores.clone(), &repository, asked, self.room) {
      Some(exchange) => Decided {
        answer: Answer::Serve {
          fields: Fields::new(),
          head_only: head.method == HEAD,
          exchange,
        },
        proven: true,
      },
      None => refused_with(Status::FIELDS_TOO_LARGE, Fields::new(), true),
    }
  }

  /// What `head`'s credentials prove, as git's and Mercurial's sites read them.
  async fn proof(&self, head: &Head<'_>, secure: bool) -> Proof {
    let credential = match credential::credential(head, secure) {
      Ok(Some(credential)) => credential,
      Ok(None) => return Proof::Unauthorized,
      Err(refused) if refused.status() == Status::UNAUTHORIZED => return Proof::Unauthorized,
      Err(refused) => return Proof::Refused(refused.status()),
    };
    match self.authenticator.prove(&credential).await {
      Ok(proven) => Proof::Principal(proven.principal),
      Err(Unproven::Unanswered) => Proof::Refused(Status::SERVICE_UNAVAILABLE),
      Err(Unproven::Unknown | Unproven::Expired | Unproven::Revoked | Unproven::NotAToken) => {
        Proof::Unauthorized
      }
    }
  }

  /// `401` with the challenges a connection that is TLS when `secure` accepts.
  fn unauthorized(&self, secure: bool) -> Decided<SvnExchange<S>> {
    let Ok(challenges) = credential::challenges(&self.realm, secure) else {
      // A realm a challenge cannot carry is the operator's misconfiguration, never the client's.
      return refused_with(Status::INTERNAL_SERVER_ERROR, Fields::new(), false);
    };
    let fields: Fields = challenges
      .into_iter()
      .map(|challenge| (credential::WWW_AUTHENTICATE.to_vec(), challenge))
      .collect();
    refused_with(Status::UNAUTHORIZED, fields, false)
  }
}

impl<A: Authenticator, R: Repositories, S: SvnStores + Clone> Site for SvnSite<A, R, S> {
  type Exchange = SvnExchange<S>;

  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange> {
    self.answer(head, secure).await
  }
}

/// Routes `head` (the module doc's names): its repository's name and the path below its root, read within `room`;
/// refused `400` where the path's escapes cannot be read, `414` where it is past `room`, and `404` where no repository
/// is named.
fn route(head: &Head<'_>, room: u64) -> Result<Routed, Status> {
  let Target::Path { path, .. } = head.target else {
    return Err(Status::NOT_FOUND);
  };
  if percent::names_any(path, PATH_REFUSED) {
    return Err(Status::BAD_REQUEST);
  }
  let length = percent::decoded_len(path).map_err(|_| Status::BAD_REQUEST)?;
  Admission::new(room)
    .charge(to_u64(length))
    .map_err(|_| Status::URI_TOO_LONG)?;
  let mut decoded = Vec::with_capacity(length);
  percent::decode_into(path, Plus::Plus, &mut decoded).map_err(|_| Status::BAD_REQUEST)?;
  let decoded = String::from_utf8(decoded).map_err(|_| Status::NOT_FOUND)?;
  let trimmed = decoded.trim_start_matches('/');
  let (name, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
  if name.is_empty() {
    return Err(Status::NOT_FOUND);
  }
  Ok(Routed {
    name: name.to_owned(),
    path: format!("/{rest}"),
  })
}

/// A refusal with `status` and `fields`; `proven` when the request's credentials proved a principal.
fn refused_with<E>(status: Status, fields: Fields, proven: bool) -> Decided<E> {
  Decided {
    answer: Answer::Refuse { status, fields },
    proven,
  }
}

/// What a decided head asks: the head itself, its route and the principal its credentials proved.
struct Asked<'h, 'b> {
  /// The head.
  head: &'h Head<'b>,
  /// Its route.
  routed: Routed,
  /// The principal.
  principal: Principal,
}

/// What a served request's answer is, once its body ended.
enum Answered {
  /// A whole answer; its body taken by the first reply run.
  Whole {
    /// The status.
    status: Status,
    /// The headers.
    headers: Headers,
    /// The body, until the reply takes it.
    body: Vec<u8>,
  },
  /// A file's text, read from the green a piece a reply run, from `offset`.
  Text {
    /// The headers.
    headers: Headers,
    /// The text, none for an empty file.
    text: Option<Text>,
    /// How much of it the reply has given.
    offset: u64,
  },
  /// A REPORT's answer, a slice a reply run.
  Report {
    /// The headers.
    headers: Headers,
    /// The report.
    report: Report,
    /// What its first slices wrote, until the first reply run takes it.
    pending: Vec<u8>,
    /// Whether the answer is whole.
    done: bool,
  },
}

/// The repository as `site` reads it for one step: what `lent` lends, under the repository root's URL path `root`.
fn served_of<'a, R: ?Sized>(lent: &'a Lent<'a, R>, root: &'a str) -> Served<'a, R> {
  Served {
    index: lent.index,
    repository: lent.repository,
    root,
    uuid: lent.uuid,
    locks: lent.locks,
    rules: lent.rules,
    now: lent.now,
  }
}

/// Steps `report` until its first bytes or its end, the store lent for each slice of `room` and the task yielding
/// between them (§4.3): what they come to decides the status the reply begins with, as `dav_method_report` answers a
/// report's error only while it has sent nothing. A refusal after its first bytes, within these slices, fails the
/// request `500` before any of it is sent, where `mod_dav` would have sent the bytes and ended the connection.
async fn first_slices<S: SvnStores>(
  stores: &mut S,
  asker: (RepositoryId, &Principal, &str),
  mut report: Report,
  room: usize,
) -> Result<Answered, Failed> {
  let (repository, principal, root) = asker;
  let mut pending = Vec::new();
  loop {
    let progress = stores
      .lend(repository, principal, |lent| {
        report.resume(&served_of(lent, root), &mut pending, room)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    let done = match progress {
      ReportProgress::More if pending.is_empty() => {
        yield_now().await;
        continue;
      }
      ReportProgress::More => false,
      ReportProgress::Done => true,
      ReportProgress::Refused(answer) => {
        return Ok(Answered::Whole {
          status: status_of(answer.status),
          headers: answer.headers,
          body: answer.body,
        });
      }
      ReportProgress::Broken => return Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    };
    return Ok(Answered::Report {
      headers: report.headers(),
      report,
      pending,
      done,
    });
  }
}

/// A served request's exchange (the module doc's bounds): its route, method and depth copied from the head, its body,
/// and its answer once the body has ended.
pub(crate) struct SvnExchange<S> {
  /// Where the repository's store is read.
  stores: S,
  /// The repository.
  repository: RepositoryId,
  /// The repository root's URL path, decoded (`/repo`).
  root: String,
  /// Who asks.
  principal: Principal,
  /// The method.
  method: String,
  /// The decoded path below the repository's root.
  path: String,
  /// The `Depth` header's value, if one was sent.
  depth: Option<String>,
  /// The request's body.
  body: Vec<u8>,
  /// The room the exchange may still hold.
  room: Admission,
  /// The room the request was given, from which the XML reader's bounds and an answer's room are derived.
  request_room: u64,
  /// The answer, once made.
  answered: Option<Answered>,
}

impl<S> SvnExchange<S> {
  /// The exchange for `asked` on `repository`, its route, method and depth copied and charged to `room`; none past it
  /// or for a `Depth` that is no text.
  fn new(
    stores: S,
    repository: &RepositoryRecord,
    asked: Asked<'_, '_>,
    room: u64,
  ) -> Option<Self> {
    let mut admission = Admission::new(room);
    let method = String::from_utf8(asked.head.method.to_vec()).ok()?;
    let depth = match asked.head.field(DEPTH) {
      Ok(Some(value)) => Some(String::from_utf8(value.to_vec()).ok()?),
      Ok(None) => None,
      Err(_) => return None,
    };
    let held = to_u64(asked.routed.path.len())
      .saturating_add(to_u64(asked.routed.name.len()))
      .saturating_add(to_u64(method.len()))
      .saturating_add(to_u64(depth.as_ref().map_or(0, String::len)));
    admission.charge(held).ok()?;
    Some(Self {
      stores,
      repository: repository.id,
      root: format!("/{}", asked.routed.name),
      principal: asked.principal,
      method,
      path: asked.routed.path,
      depth,
      body: Vec::new(),
      room: admission,
      request_room: room,
      answered: None,
    })
  }
}

/// The status a site's answer names, by its code (RFC 9110 §15, RFC 4918 §11.1); a code this site never answers is
/// `500`.
fn status_of(code: u16) -> Status {
  [
    Status::OK,
    Status::MULTI_STATUS,
    Status::BAD_REQUEST,
    Status::FORBIDDEN,
    Status::NOT_FOUND,
    Status::CONTENT_TOO_LARGE,
    Status::NOT_IMPLEMENTED,
  ]
  .into_iter()
  .find(|status| status.code == code)
  .unwrap_or(Status::INTERNAL_SERVER_ERROR)
}

impl<S: SvnStores> Exchange for SvnExchange<S> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self
      .room
      .charge(to_u64(run.len()))
      .map_err(|_| Failed(Status::CONTENT_TOO_LARGE))?;
    self.body.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    let request = Request {
      method: &self.method,
      path: &self.path,
      depth: self.depth.as_deref(),
      body: &self.body,
    };
    let bounds = xml_bounds(self.request_room);
    let room = to_usize(self.request_room);
    let root = self.root.as_str();
    let answer = self
      .stores
      .lend(self.repository, &self.principal, |lent| {
        site::answer(&served_of(lent, root), &request, &bounds, room)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    let answered = match answer {
      SiteAnswer::Whole(whole) => Answered::Whole {
        status: status_of(whole.status),
        headers: whole.headers,
        body: whole.body,
      },
      SiteAnswer::Text { headers, text } => Answered::Text {
        headers,
        text,
        offset: 0,
      },
      SiteAnswer::Report(report) => {
        let asker = (self.repository, &self.principal, root);
        first_slices(&mut self.stores, asker, report, room).await?
      }
    };
    self.answered = Some(answered);
    Ok(())
  }

  fn reply_status(&self) -> Status {
    match &self.answered {
      Some(Answered::Whole { status, .. }) => *status,
      Some(Answered::Text { .. } | Answered::Report { .. }) => Status::OK,
      None => Status::INTERNAL_SERVER_ERROR,
    }
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    let headers = match &self.answered {
      Some(
        Answered::Whole { headers, .. }
        | Answered::Text { headers, .. }
        | Answered::Report { headers, .. },
      ) => headers,
      None => return Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    };
    Ok(
      headers
        .iter()
        .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
        .collect(),
    )
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    match &mut self.answered {
      Some(Answered::Whole { body, .. }) => {
        out.append(body);
        Ok(false)
      }
      Some(Answered::Text { text, offset, .. }) => {
        let Some(text) = *text else {
          return Ok(false);
        };
        let at = *offset;
        let piece = self
          .stores
          .lend(self.repository, &self.principal, |lent| {
            lent.repository.text(&text.identity, at).map(<[u8]>::to_vec)
          })
          .flatten()
          .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
        *offset = at.saturating_add(to_u64(piece.len()));
        out.extend_from_slice(&piece);
        Ok(!piece.is_empty() && *offset < text.size)
      }
      Some(Answered::Report {
        report,
        pending,
        done,
        ..
      }) => {
        if !pending.is_empty() || *done {
          out.append(pending);
          return Ok(!*done);
        }
        let (root, room) = (self.root.as_str(), to_usize(self.request_room));
        let progress = self
          .stores
          .lend(self.repository, &self.principal, |lent| {
            report.resume(&served_of(lent, root), out, room)
          })
          .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
        match progress {
          ReportProgress::More => Ok(true),
          ReportProgress::Done => {
            *done = true;
            Ok(false)
          }
          // Its first slices wrote its header, so a refusal now can only end the connection.
          ReportProgress::Refused(_) | ReportProgress::Broken => {
            Err(Failed(Status::INTERNAL_SERVER_ERROR))
          }
        }
      }
      None => Err(Failed(Status::INTERNAL_SERVER_ERROR)),
    }
  }
}

#[cfg(test)]
#[path = "tests/svn.rs"]
mod tests;
