//! Mercurial's site on the HTTP edge (A-650 part 1; §4.17.3 "The wire surface" and "Access"; 7.2's
//! `wireprotoserver.handlewsgirequest` and hgweb's `checkauthz` \[C\]): each request's head routed as hgweb routes a
//! repository's URL, its credentials proved to a principal of §4.13 (git's [`Authenticator`]), its repository found
//! ([`Repositories`]), the principal's reach of it decided by the repository's policy (`crate::vcs_policy::reaches`,
//! A-597, A-622), and its command answered from the repository's store by `slates_hg::serve::http` in the media type
//! the answer decides, named as the reply begins (A-650's edge change).
//!
//! **The order**, each refusal with its status:
//! 1. The route. A path whose escapes are malformed or name a slash or a NUL is `400`; a path, or a query's fields,
//!    past the request's room is `414`, as git's routes answer a target past theirs. A wire request (a `cmd` 7.2's
//!    table holds) by a method other than `GET` or `POST` is `405` with `Allow`.
//! 2. The credentials, judged as git's site judges them (its `prove`, over A-625 part 3's credential record): none, or
//!    none proving a principal (unknown, expired, a revoked consumer's, an SSH key's), is `401` with the edge's
//!    challenges, Basic's first, which stock 7.2 reads (A-686: its urllib reads the first challenge alone, and Basic
//!    with the token as the password is taken on the loopback cleartext edge as over TLS); a repeated or malformed
//!    `Authorization` is `400`; a record that did not answer is `503`.
//! 3. The repository's shard. A repository another shard holds moves the proven request's connection there whole
//!    (`Answer::Move`, A-625 part 3), where it is decided again.
//! 4. The repository. None of that name, or one of another tool, is `404`.
//! 5. The policy. A principal the policy lets read none of the repository is `403`, for every request, as hgweb's
//!    `checkauthz` reads `deny_read` and `allow_read` first. A push (`unbundle`, `pushkey`) also needs `POST` (`405`
//!    with `Allow: POST`, hgweb's "push requires POST request") and a right to submit, rewrite or delete (`403`).
//! 6. The service. hgweb's pages (a request naming no command of 7.2's table: the repository's browsing URLs,
//!    §4.17.3 piece 12) are `503`, and so is a repository whose store this shard does not hold (A-662 keeps each
//!    repository's on its owner shard; until the site is handed them no repository has one here, [`NoStores`]).
//!
//! A served request is `200`: its body is the command's answer and its media type the one the answer names
//! (`application/mercurial-0.1`, `-0.2` or `application/hg-error`), given as the reply begins.
//!
//! **Names.** A repository is the one whose name is the URL's path, decoded, its slashes at either end dropped as
//! hgwebdir drops them (`req.dispatchpath.strip(b'/')`): a client reaches `name` at `/name?cmd=...` and at
//! `/name/?cmd=...`. No other spelling is tried.
//!
//! **Bounds.** A request's exchange holds its arguments (its query and `X-Hg` fields, copied from the head), its body
//! and its answer, each charged to the request's room before it is held; a body past the room is `413`.
//!
//! **Compression** ([`Fastest`]). §4.17.3's estimator (the connection's delivery rate, and this repository's
//! compression speed and ratio per engine and level) is owed; until it lands a stream takes the design's first value,
//! the fastest form the client reads: `none` where it reads it, which costs no compression time, else its first
//! engine at that engine's fastest level; zlib at level 0 to a client that reads `application/mercurial-0.1` alone.
//!
//! **Differs from hgweb, on purpose.** A principal whose credentials hold and whom the policy refuses is `403` (RFC
//! 9110 §15.5.4: the credentials are "insufficient to grant access"), where hgweb answers `401` without a challenge.

use std::borrow::Cow;

use slates_db::catalog::{Principal, RepositoryId, RepositoryRecord, VcsTool};
use slates_hg::compression::Engine;
use slates_hg::obsmarkers::Marker;
use slates_hg::phases::Phase;
use slates_hg::serve::getbundle::{Logged, Store};
use slates_hg::serve::http::{self as hg_http, Compression, Compressor, Media, Permission};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{Admission, Node, PastAdmission, http as hg_wire};
use slates_http::head::{Head, Target};
use slates_http::percent::{self, Plus};
use slates_http::reply::Status;
use slates_http_edge::connection::Fields;
use slates_http_edge::credential;

use super::git::{Authenticator, Located, Repositories, Unproven};
use super::{Answer, Decided, Exchange, Failed, Site};
use crate::vcs_policy::{self, Right};

/// Format: the methods a wire request may use, as `Allow` lists them: 7.2's client sends `GET`, and `POST` with a body.
const WIRE_METHODS: &[u8] = b"GET, POST";

/// Format: the method a push needs (hgweb's `checkauthz`: "push requires POST request"), as `Allow` lists it.
const PUSH_METHOD: &[u8] = b"POST";

/// Format: `GET`, the method 7.2's client sends a wire request by when it carries no body.
const GET: &[u8] = b"GET";

/// Format: `Allow`'s name (RFC 9110 §10.2.1).
const ALLOW: &[u8] = b"Allow";

/// Format: `Content-Type`'s name (RFC 9110 §8.3).
const CONTENT_TYPE: &[u8] = b"Content-Type";

/// Format: the bytes an escape may not name in a repository's path: a slash, which would move a segment's boundary,
/// and NUL.
const PATH_REFUSED: &[u8] = b"/\0";

/// Format: the slash hgwebdir drops from a path's ends.
const SLASH: u8 = b'/';

/// Format: the prefix of the fields a Mercurial request carries its arguments and what it reads in (`X-HgArg-<N>`,
/// `X-HgArgs-Post`, `X-HgProto-<N>`), compared without regard to case.
const HG_FIELDS: &[u8] = b"x-hg";

/// Derived: `httpheader`, the longest header line a client may split its arguments into: the edge's line bound, RFC
/// 9112 §3's 8000 octets (`slates_http::MAX_LINE`). A client's count holds the line's name, `: `, value and line end, and
/// the edge's leaves the line end out, so every line a client splits to is read.
fn header_line() -> u64 {
  u64::try_from(slates_http::MAX_LINE).unwrap_or(u64::MAX)
}

/// A count as `u64`, saturating where it could not fit (no target slates ships has a wider `usize`).
fn to_u64(count: usize) -> u64 {
  u64::try_from(count).unwrap_or(u64::MAX)
}

/// Where Mercurial repositories' stores are lent on this shard (A-662: each repository's tool store, kept on its owner
/// shard with its record), afresh for each step, as git's site's `StoreLender` lends its (A-640 part 2: "so a shard's
/// state is never held across a wait").
pub(crate) trait HgStores {
  /// The store a repository's commands are answered from.
  type Store: Store;
  /// Whether the store of repository `id` is held here.
  fn holds(&self, id: RepositoryId) -> bool;
  /// Runs `step` over repository `id`'s store; none when it is not held here.
  fn lend<R>(&mut self, id: RepositoryId, step: impl FnOnce(&Self::Store) -> R) -> Option<R>;
}

/// The stores as the site has them until it is handed A-662's (each repository's kept beside its record on its owner
/// shard): none, so every Mercurial request a principal may make is `503`.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoStores;

/// The store no repository has yet: no value has this type.
pub(crate) enum NoStore {}

impl Repository for NoStore {
  fn count(&self) -> Rev {
    match *self {}
  }

  fn node(&self, _rev: Rev) -> Option<Node> {
    match *self {}
  }

  fn rev(&self, _node: &Node) -> Option<Rev> {
    match *self {}
  }

  fn parents(&self, _rev: Rev) -> [Option<Rev>; 2] {
    match *self {}
  }

  fn branch(&self, _rev: Rev) -> &[u8] {
    match *self {}
  }

  fn closes(&self, _rev: Rev) -> bool {
    match *self {}
  }

  fn phase(&self, _rev: Rev) -> Phase {
    match *self {}
  }

  fn publishing(&self) -> bool {
    match *self {}
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    std::iter::empty()
  }
}

impl Store for NoStore {
  fn changeset_text(
    &self,
    _rev: Rev,
    _room: &mut Admission,
  ) -> Result<Option<Cow<'_, [u8]>>, PastAdmission> {
    match *self {}
  }

  fn manifest(
    &self,
    _node: &Node,
    _room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission> {
    match *self {}
  }

  fn file(
    &self,
    _path: &[u8],
    _node: &Node,
    _room: &mut Admission,
  ) -> Result<Option<Logged<'_>>, PastAdmission> {
    match *self {}
  }

  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    std::iter::empty()
  }
}

impl HgStores for NoStores {
  type Store = NoStore;

  fn holds(&self, _id: RepositoryId) -> bool {
    false
  }

  fn lend<R>(&mut self, _id: RepositoryId, _step: impl FnOnce(&NoStore) -> R) -> Option<R> {
    None
  }
}

/// The design's first value for a stream's compression (§4.17.3 "Learned tunables": "each named engine at its fastest
/// level"), until the estimator lands: the fastest form the client reads (the module doc).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Fastest;

impl Compressor for Fastest {
  fn choose(&mut self, _media: Media, engines: &[Engine], _length: u64) -> Compression {
    let engine = engines
      .iter()
      .copied()
      .find(|engine| *engine == Engine::Uncompressed)
      .or_else(|| engines.first().copied())
      .unwrap_or(Engine::Zlib);
    Compression {
      engine,
      level: *engine.levels().start(),
    }
  }
}

/// What a routed head names: its repository, and the wire command its query names, if any.
struct Routed {
  /// The repository's name.
  name: String,
  /// The permission the query's command needs, `None` where it names no command of 7.2's table (hgweb's pages).
  permission: Option<Permission>,
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

/// Mercurial's site: the realm its challenges name, the room a request is charged to, and the authenticator,
/// repositories and stores it decides and answers with (the module doc).
pub(crate) struct HgSite<A, R, S> {
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

impl<A: Authenticator, R: Repositories, S: HgStores + Clone> HgSite<A, R, S> {
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

  /// The answer to `head`, on any connection, in the module doc's order.
  pub(crate) async fn answer(&mut self, head: &Head<'_>) -> Decided<HgExchange<S>> {
    let routed = match route(head, self.room) {
      Ok(routed) => routed,
      Err(status) => return refused_with(status, Fields::new(), false),
    };
    if routed.permission.is_some() && head.method != GET && head.method != PUSH_METHOD {
      return refused_with(Status::METHOD_NOT_ALLOWED, allow(WIRE_METHODS), false);
    }
    let principal = match self.proof(head).await {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(),
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
      .filter(|repository| repository.tool == VcsTool::Mercurial)
    else {
      return refused_with(Status::NOT_FOUND, Fields::new(), true);
    };
    if let Some(refused) = judged(&repository, &principal, head, routed.permission) {
      return refused;
    }
    if !self.stores.holds(repository.id) {
      return refused_with(Status::SERVICE_UNAVAILABLE, Fields::new(), true);
    }
    match HgExchange::new(self.stores.clone(), repository, head, self.room) {
      Some(exchange) => Decided {
        answer: Answer::Serve {
          fields: Fields::new(),
          head_only: false,
          exchange,
        },
        proven: true,
      },
      None => refused_with(Status::FIELDS_TOO_LARGE, Fields::new(), true),
    }
  }

  /// What `head`'s credentials prove, as git's site reads them.
  async fn proof(&self, head: &Head<'_>) -> Proof {
    let credential = match credential::credential(head) {
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

  /// `401` with the challenges, Basic's first and then Bearer's.
  fn unauthorized(&self) -> Decided<HgExchange<S>> {
    let Ok(challenges) = credential::challenges(&self.realm) else {
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

impl<A: Authenticator, R: Repositories, S: HgStores + Clone> Site for HgSite<A, R, S> {
  type Exchange = HgExchange<S>;

  /// The site's answer, whatever the connection: no step of it reads whether the connection is TLS (A-686).
  async fn decide(&mut self, head: &Head<'_>, _secure: bool) -> Decided<Self::Exchange> {
    self.answer(head).await
  }
}

/// The policy's refusal of `principal` on `repository` for a request needing `permission`, if any (the module doc,
/// steps 4 and 5): read first, then hgweb's pages, then a push's method and rights.
fn judged<E>(
  repository: &RepositoryRecord,
  principal: &Principal,
  head: &Head<'_>,
  permission: Option<Permission>,
) -> Option<Decided<E>> {
  let refused = |status| Some(refused_with(status, Fields::new(), true));
  match vcs_policy::reaches(repository, principal, &[Right::Read]) {
    Ok(true) => {}
    Ok(false) => return refused(Status::FORBIDDEN),
    // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
    Err(_) => return refused(Status::INTERNAL_SERVER_ERROR),
  }
  match permission {
    None => refused(Status::SERVICE_UNAVAILABLE),
    Some(Permission::Pull) => None,
    Some(Permission::Push) if head.method != PUSH_METHOD => Some(refused_with(
      Status::METHOD_NOT_ALLOWED,
      allow(PUSH_METHOD),
      true,
    )),
    Some(Permission::Push) => {
      let push = [Right::Submit, Right::Rewrite, Right::Delete];
      match vcs_policy::reaches(repository, principal, &push) {
        Ok(true) => None,
        Ok(false) => refused(Status::FORBIDDEN),
        Err(_) => refused(Status::INTERNAL_SERVER_ERROR),
      }
    }
  }
}

/// Routes `head` (the module doc's names): its repository's name and its command's permission, each read within
/// `room`; refused `400` where the path's escapes cannot be read, `414` where the path or the query's fields are past
/// `room`, and `404` where no repository could be named.
fn route(head: &Head<'_>, room: u64) -> Result<Routed, Status> {
  let Target::Path { path, query } = head.target else {
    return Err(Status::NOT_FOUND);
  };
  let mut admission = Admission::new(room);
  if percent::names_any(path, PATH_REFUSED) {
    return Err(Status::BAD_REQUEST);
  }
  let length = percent::decoded_len(path).map_err(|_| Status::BAD_REQUEST)?;
  admission
    .charge(to_u64(length))
    .map_err(|_| Status::URI_TOO_LONG)?;
  let mut decoded = Vec::with_capacity(length);
  percent::decode_into(path, Plus::Plus, &mut decoded).map_err(|_| Status::BAD_REQUEST)?;
  let name = String::from_utf8(trimmed(&decoded).to_vec()).map_err(|_| Status::NOT_FOUND)?;
  let command = match query {
    Some(query) => hg_http::command(query, &mut admission).map_err(query_refused)?,
    None => None,
  };
  Ok(Routed {
    name,
    permission: command.as_deref().and_then(hg_http::permission),
  })
}

/// The status of a query whose fields cannot be read: `414` where they are past the request's room, as a path past it
/// is (RFC 9110 §15.5.15: a target "longer than the server is willing to interpret"); `400` for every other refusal,
/// each named, so a refusal the codec adds is judged here before it compiles.
fn query_refused(refusal: hg_wire::Refusal) -> Status {
  match refusal {
    hg_wire::Refusal::PastAdmission(_) => Status::URI_TOO_LONG,
    hg_wire::Refusal::PostArguments
    | hg_wire::Refusal::MissingArgument { .. }
    | hg_wire::Refusal::MediaHeader => Status::BAD_REQUEST,
  }
}

/// `path` without the slashes at either end, as hgwebdir strips them.
fn trimmed(path: &[u8]) -> &[u8] {
  let start = path
    .iter()
    .position(|byte| *byte != SLASH)
    .unwrap_or(path.len());
  let end = path
    .iter()
    .rposition(|byte| *byte != SLASH)
    .map_or(start, |last| last.saturating_add(1));
  path.get(start..end.max(start)).unwrap_or_default()
}

/// The `Allow` field naming `methods`.
fn allow(methods: &[u8]) -> Fields {
  vec![(ALLOW.to_vec(), methods.to_vec())]
}

/// A refusal with `status` and `fields`; `proven` when the request's credentials proved a principal.
fn refused_with<E>(status: Status, fields: Fields, proven: bool) -> Decided<E> {
  Decided {
    answer: Answer::Refuse { status, fields },
    proven,
  }
}

/// A served request's exchange (the module doc's bounds): its arguments copied from the head, its body, and its answer
/// once the body has ended.
pub(crate) struct HgExchange<S> {
  /// Where the repository's store is read.
  stores: S,
  /// The repository.
  repository: RepositoryRecord,
  /// The request's query.
  query: Vec<u8>,
  /// The request's `X-Hg` fields, name and value.
  fields: Vec<(Vec<u8>, Vec<u8>)>,
  /// The request's body.
  body: Vec<u8>,
  /// The room the exchange may still hold.
  room: Admission,
  /// The answer's media type and body, once made.
  answered: Option<(&'static [u8], Vec<u8>)>,
}

impl<S> HgExchange<S> {
  /// The exchange for `head` on `repository`, its arguments copied and charged to `room`; none past it.
  fn new(stores: S, repository: RepositoryRecord, head: &Head<'_>, room: u64) -> Option<Self> {
    let mut admission = Admission::new(room);
    let query = match head.target {
      Target::Path { query, .. } => query.unwrap_or_default(),
      Target::Asterisk => &[],
    };
    let fields: Vec<(Vec<u8>, Vec<u8>)> = head
      .fields
      .iter()
      .filter(|(name, _)| {
        name
          .get(..HG_FIELDS.len())
          .is_some_and(|prefix| prefix.eq_ignore_ascii_case(HG_FIELDS))
      })
      .map(|(name, value)| (name.to_vec(), value.to_vec()))
      .collect();
    let held = fields
      .iter()
      .fold(to_u64(query.len()), |total, (name, value)| {
        total
          .saturating_add(to_u64(name.len()))
          .saturating_add(to_u64(value.len()))
      });
    admission.charge(held).ok()?;
    Some(Self {
      stores,
      repository,
      query: query.to_vec(),
      fields,
      body: Vec::new(),
      room: admission,
      answered: None,
    })
  }
}

impl<S: HgStores> Exchange for HgExchange<S> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self
      .room
      .charge(to_u64(run.len()))
      .map_err(|_| Failed(Status::CONTENT_TOO_LARGE))?;
    self.body.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    let fields: Vec<(&[u8], &[u8])> = self
      .fields
      .iter()
      .map(|(name, value)| (name.as_slice(), value.as_slice()))
      .collect();
    let request = slates_hg::http::Request {
      query: &self.query,
      headers: &fields,
      body: &self.body,
    };
    let mut admission = self.room;
    let answered = self
      .stores
      .lend(self.repository.id, |store| {
        hg_http::answer(store, &request, header_line(), &mut Fastest, &mut admission)
      })
      .ok_or(Failed(Status::SERVICE_UNAVAILABLE))?;
    self.answered = Some((answered.media_type, answered.body));
    Ok(())
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    let (media_type, _) = self
      .answered
      .as_ref()
      .ok_or(Failed(Status::INTERNAL_SERVER_ERROR))?;
    Ok(vec![(CONTENT_TYPE.to_vec(), media_type.to_vec())])
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    if let Some((_, mut body)) = self.answered.take() {
      out.append(&mut body);
    }
    Ok(false)
  }
}

#[cfg(test)]
#[path = "tests/hg.rs"]
mod tests;
