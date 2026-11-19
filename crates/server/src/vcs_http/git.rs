//! git's site on the HTTP edge (A-625 part 2; §4.17.2 "Transports" item 1 and "Access"): each request's head routed as
//! `git http-backend` routes it (`slates_git::http`, A-595), its credentials proved to a principal of §4.13 by an
//! authenticator ([`Authenticator`]), its repository found ([`Repositories`]), the principal's reach of it decided by the
//! repository's policy (`crate::vcs_policy::reaches`, A-597, A-622), and its service started ([`GitServices`]).
//!
//! **The order**, each refusal with its status and the three cache fields `http-backend` sends on every answer:
//! 1. The route. A path, method, service or content type `http-backend` refuses is refused as it refuses it, whoever
//!    asks: its syntax names no repository, so a client learns its mistake without credentials.
//! 2. The credentials. None, another scheme, Basic in cleartext, or a credential that proves no principal (none held
//!    under its hash, past its expiry, a revoked consumer's, an SSH key's) is `401` with the challenges the connection
//!    accepts (RFC 9110 §11.6.1: "A server generating a 401 (Unauthorized) response MUST send a WWW-Authenticate header
//!    field containing at least one challenge"; Basic over TLS alone, RFC 7617 §4); a repeated or malformed
//!    `Authorization` is `400`; a credential record that did not answer is `503`. Nothing about a repository is answered
//!    to a client that has proved no one. The proof is the credential record's (`crate::credentials`, A-625 part 3).
//! 3. The repository. A name another shard's partition holds moves the connection there whole, its head undecided, and
//!    the head is decided again where the repository lives ([`Answer::Move`]; main's ruling, 2026-11-19, as the NFS
//!    export moves a mount's connection, §4.6), so its every step lends the store on its own shard. There, none of that
//!    name, or one of another tool, is `404` (§4.17.2: "No repository is `404`").
//! 4. The policy. A principal the repository's policy lets reach it for none of the service's rights is `403`
//!    (gitprotocol-http: "If there is a repository at `$GIT_URL`, but access is not currently permitted, the server MUST
//!    respond with the `403 Forbidden` HTTP status code"). upload-pack asks to read; receive-pack, its advertisement too,
//!    asks to submit, rewrite or delete, any of which a push may do. The gate admits a principal some rule lets reach the
//!    repository; which refs it then sees or changes is each ref's verdict, the service's (`vcs_policy::admit` for a
//!    push).
//! 5. The service. One whose server half has not landed, or whose repository's store is not held here, is `503` (RFC
//!    9110 §15.6.4: "the server is currently unable to handle the request"). upload-pack is served from a repository's
//!    store by [`upload`] (A-640 part 2, A-625 part 3), once the server lends stores (owed); receive-pack answers `503`
//!    until its session's entry point lands (A-654). `Unserved` answers `503` to both.
//!
//! Every refusal of steps 2 to 5 is typed ([`Refused`]) and counted under its kind in the shard's status counters.
//!
//! A served request is `200` with the service's content type and the cache fields, in `http-backend`'s order
//! (`slates_git::http::reply_fields`), its body the advertisement's `# service=` prelude for protocols v0 and v1
//! (gitprotocol-http: the first line "MUST be `# service=$servicename`") and then what the service writes.
//!
//! **Names.** A repository is the one whose name is the URL's repository path exactly, decoded and without its leading
//! slash (`slates_git::http::route`): a client reaches `name` at `/name/info/refs`, and one named `name.git` at
//! `/name.git/info/refs`. No second spelling is tried.
//!
//! **Git LFS** (A-629; §4.17.2 "Large files"): a request under `<repo>/info/lfs`, the LFS server URL git-lfs derives from
//! a remote, is routed by `slates_git::lfs::serve::route` (the batch API, the locks API and the basic adapter's
//! transfers this server hands out) and taken in the same order: its route refused whoever asks (`405` with `Allow`,
//! `400`, `414`, `415`, `406`), a request without a `Host` `400`; its credentials, a `401` naming each challenge in
//! `LFS-Authenticate` too, as `slates_git::lfs::serve` answers one (A-629 part 1); its repository, moved to its shard or
//! `404`; the policy's gate for LFS's rights (a download or a listing reads, an upload or a lock submits, another's
//! lock rewrites; [`LFS_RIGHTS`]), else `403`; and its service ([`LfsService`]), `503` for an endpoint the site's
//! service does not serve (every one, on a site without a service). Each endpoint's own right on the ref it names is the
//! answer's (`crate::lfs::LfsAccess`). A served LFS request decides its status after its body
//! (`StatusMode::AfterBody`, main's ruling, 2026-11-19): its body is kept whole within the request's room, which the
//! engine's admission already bounds, and answered once it ends, with the status, fields and body the service decided,
//! its hrefs under the authority the request named, and its actions expiring with the bearer token that asked
//! ([`Proven`]).

use slates_db::catalog::{Principal, RepositoryRecord, VcsTool};
use slates_git::http::{self, Field, Reply, Route, Service, Version};
use slates_git::lfs::Timestamp;
use slates_git::lfs::serve::{self as lfs_serve, Endpoint, RouteRefused, Routed};
use slates_http::Admission;
use slates_http::head::{Head, Target, Version as HttpVersion};
use slates_http::reply::Status;
use slates_http_edge::connection::{Fields, StatusMode};
use slates_http_edge::credential::{self, Credential, CredentialRefused};

use super::{Answer, Decided, Exchange, Failed, Site};
use crate::lfs::LfsCall;
use crate::vcs_policy::{self, Right};

/// The rights a principal must hold on some name of a repository for Git LFS to answer it at all (the module doc): a
/// download or a listing reads, an upload, a lock or a verification submits, releasing another's lock rewrites
/// (`crate::lfs::LfsAccess`).
const LFS_RIGHTS: &[Right] = &[Right::Read, Right::Submit, Right::Rewrite];

/// Format: the field a `405` names its methods in (RFC 9110 §10.2.1).
const ALLOW: &[u8] = b"Allow";

/// Format: the field `slates_git::lfs::serve` names a `401`'s challenge in, beside `WWW-Authenticate` (A-629 part 1).
const LFS_AUTHENTICATE: &[u8] = b"LFS-Authenticate";

/// Format: nanoseconds in a second, the credential record's unit against an LFS action's.
const NS_PER_SECOND: u64 = 1_000_000_000;

mod upload;

pub(crate) use upload::{StoreLender, UploadServices};

/// What a request's credential proved (§4.13; A-625 part 3): its principal, and the expiry an LFS action carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Proven {
  /// The principal.
  pub(crate) principal: Principal,
  /// The credential's expiry on the wall clock (nanoseconds since the Unix epoch), for a bearer token that has one:
  /// what an LFS action's `expires_at` carries (A-629). None for Basic, whose transfers authenticate as their batch did,
  /// and for a token that never expires.
  pub(crate) bearer_expires_ns: Option<u64>,
}

/// Why a credential proved no one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unproven {
  /// No credential is held under its hash.
  Unknown,
  /// It is past its expiry.
  Expired,
  /// It is an enrolled consumer's, and a human revoked the consumer (§4.13: "every later effect refuses").
  Revoked,
  /// Its hash names an SSH key, which proves no one over HTTP.
  NotAToken,
  /// The partition its hash routes to did not answer.
  Unanswered,
}

/// What proves a request's credentials to a principal (§4.13): the token a Basic credential carries as its password, or a
/// bearer token, looked up in the credential record (`crate::credentials`).
pub(crate) trait Authenticator {
  /// What `credential` proves.
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven>;
}

/// Where a repository's name lives (§4.17.1: a name is unique on the partition a stable hash of it chooses).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Located {
  /// On this shard.
  Here,
  /// On runtime shard `shard`, where the connection moves.
  Elsewhere(u16),
}

/// Where a repository is found by its name.
pub(crate) trait Repositories {
  /// Where the repository named `name` lives.
  fn locate(&self, name: &str) -> Located;
  /// The repository named `name` on this shard, if one is.
  fn find(&self, name: &str) -> Option<RepositoryRecord>;
}

/// Why git's site refused a request, by kind: each answered with its status and counted under its name (A-625 part 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
  /// No credentials, or a scheme other than Basic and Bearer: `401`.
  NoCredentials,
  /// Basic on a cleartext connection (RFC 7617 §4): `401`, its challenges offering Bearer alone.
  CleartextBasic,
  /// An `Authorization` field repeated or broken: `400`.
  MalformedCredentials,
  /// A credential that proved no one: `401`, or `503` when its record did not answer.
  Unproven(Unproven),
  /// No git repository of that name: `404`.
  NoRepository,
  /// The repository's policy lets the principal reach it for none of the service's rights: `403`.
  Forbidden,
  /// The repository's policy cannot be read: `500`.
  Unreadable,
  /// The service's server half, or the repository's store, is not here: `503`.
  Unavailable,
}

impl Refused {
  /// The status it is answered with.
  pub(crate) fn status(self) -> Status {
    match self {
      Refused::NoCredentials
      | Refused::CleartextBasic
      | Refused::Unproven(
        Unproven::Unknown | Unproven::Expired | Unproven::Revoked | Unproven::NotAToken,
      ) => Status::UNAUTHORIZED,
      Refused::MalformedCredentials => Status::BAD_REQUEST,
      Refused::Unproven(Unproven::Unanswered) | Refused::Unavailable => Status::SERVICE_UNAVAILABLE,
      Refused::NoRepository => Status::NOT_FOUND,
      Refused::Forbidden => Status::FORBIDDEN,
      Refused::Unreadable => Status::INTERNAL_SERVER_ERROR,
    }
  }

  /// Format: the counter it is counted under in the shard's status report.
  pub(crate) fn counter(self) -> &'static str {
    match self {
      Refused::NoCredentials => "vcs_http.git.no_credentials",
      Refused::CleartextBasic => "vcs_http.git.cleartext_basic",
      Refused::MalformedCredentials => "vcs_http.git.malformed_credentials",
      Refused::Unproven(Unproven::Unknown) => "vcs_http.git.unknown_credential",
      Refused::Unproven(Unproven::Expired) => "vcs_http.git.expired_credential",
      Refused::Unproven(Unproven::Revoked) => "vcs_http.git.revoked_consumer",
      Refused::Unproven(Unproven::NotAToken) => "vcs_http.git.not_a_token",
      Refused::Unproven(Unproven::Unanswered) => "vcs_http.git.credentials_unanswered",
      Refused::NoRepository => "vcs_http.git.no_repository",
      Refused::Forbidden => "vcs_http.git.forbidden",
      Refused::Unreadable => "vcs_http.git.policy_unreadable",
      Refused::Unavailable => "vcs_http.git.unavailable",
    }
  }
}

/// What a decided request starts its service with.
pub(crate) struct Start<'a> {
  /// The repository.
  pub(crate) repository: &'a RepositoryRecord,
  /// Who asks.
  pub(crate) principal: &'a Principal,
  /// The service.
  pub(crate) service: Service,
  /// The protocol version asked for.
  pub(crate) version: Version,
  /// Whether the request asks for the service's advertisement (`GET info/refs`) rather than serves its request
  /// (`POST`).
  pub(crate) advertise: bool,
}

/// Why a service did not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceRefused {
  /// The service's server half has not landed: `503`.
  Unavailable,
}

/// git's two services behind the site: upload-pack (fetch and clone) and receive-pack (push).
pub(crate) trait GitServices {
  /// The exchange a started service runs.
  type Exchange: Exchange;
  /// Starts the service `start` names, or refuses it.
  fn start(&mut self, start: &Start<'_>) -> Result<Self::Exchange, ServiceRefused>;
}

/// The services as the server has them today: neither has its server half (upload-pack's negotiation, A-615; receive-
/// pack's commit through the register, A-620 part 2), so each answers [`ServiceRefused::Unavailable`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Unserved;

/// The exchange an unserved service never starts: no value has this type.
pub(crate) enum NoExchange {}

impl Exchange for NoExchange {
  async fn body(&mut self, _run: &[u8]) -> Result<(), Failed> {
    match *self {}
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    match *self {}
  }

  async fn reply(&mut self, _out: &mut Vec<u8>) -> Result<bool, Failed> {
    match *self {}
  }
}

impl GitServices for Unserved {
  type Exchange = NoExchange;

  fn start(&mut self, _start: &Start<'_>) -> Result<NoExchange, ServiceRefused> {
    Err(ServiceRefused::Unavailable)
  }
}

/// A served request's exchange: the advertisement's prelude first, when it has one, then the service's.
pub(crate) struct GitExchange<E> {
  /// The prelude not yet given: `# service=<service>` and a flush for v0 and v1, nothing else.
  prelude: Vec<u8>,
  /// The service's exchange.
  service: E,
}

impl<E: Exchange> Exchange for GitExchange<E> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self.service.body(run).await
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    self.service.body_end().await
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    if !self.prelude.is_empty() {
      out.append(&mut self.prelude);
      return Ok(true);
    }
    self.service.reply(out).await
  }
}

/// Git LFS behind git's site (the module doc): what answers an LFS request the site has routed, proved and found the
/// repository of here, once its body is read whole. The daemon's answers on the repository's owner shard
/// (`crate::lfs::answer`, A-629 part 2b-ii).
pub(crate) trait LfsService {
  /// Whether it serves `endpoint`: the site answers one it does not `503` at its head, its body never read.
  fn serves(&self, endpoint: &Endpoint) -> bool;
  /// The answer to `call`.
  fn answer(&mut self, call: &LfsCall<'_>) -> lfs_serve::Answer;
}

/// A site's LFS service where it has none: it serves no endpoint, so it is never asked for an answer.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoLfs;

impl LfsService for NoLfs {
  fn serves(&self, _endpoint: &Endpoint) -> bool {
    false
  }

  fn answer(&mut self, _call: &LfsCall<'_>) -> lfs_serve::Answer {
    lfs_serve::Answer {
      status: Status::SERVICE_UNAVAILABLE,
      kind: lfs_serve::Kind::Empty,
      challenge: None,
      body: Vec::new(),
    }
  }
}

/// An LFS request's exchange (the module doc): its body kept whole, then its service's answer, whose status and fields
/// the reply's head carries and whose body is the reply.
pub(crate) struct LfsExchange<L> {
  /// The service that answers it.
  service: L,
  /// What its route found.
  routed: Routed,
  /// Its query, without the `?`.
  query: Vec<u8>,
  /// The principal its credentials proved.
  principal: Principal,
  /// When that credential expires: its actions' expiry.
  credential_expiry: Option<Timestamp>,
  /// The scheme the client reached the server by.
  scheme: &'static str,
  /// The authority it named (`Host`).
  authority: String,
  /// The bytes its answer may build: the edge's request room.
  room: u64,
  /// Its body as it comes, bounded by the engine's admission to the request's room.
  body: Vec<u8>,
  /// The service's answer, once the body has ended; its body taken as the reply.
  answer: Option<lfs_serve::Answer>,
}

impl<L: LfsService> Exchange for LfsExchange<L> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    // The engine's admission bounds the body by the request's room; memory that cannot be had is the server's failure.
    self
      .body
      .try_reserve(run.len())
      .map_err(|_| Failed(Status::INTERNAL_SERVER_ERROR))?;
    self.body.extend_from_slice(run);
    Ok(())
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    let body = std::mem::take(&mut self.body);
    let call = LfsCall {
      routed: &self.routed,
      query: &self.query,
      body: &body,
      principal: Some(&self.principal),
      credential_expiry: self.credential_expiry,
      challenge: None,
      origin: (self.scheme, &self.authority),
      request_id: None,
      room: self.room,
    };
    self.answer = Some(self.service.answer(&call));
    Ok(())
  }

  async fn reply_status(&mut self) -> Result<Option<Status>, Failed> {
    Ok(self.answer.as_ref().map(|answer| answer.status))
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    let fields = self.answer.as_ref().map(|answer| {
      let fields = answer.fields();
      fields
        .iter()
        .map(|(name, value)| (name.to_vec(), value.to_vec()))
        .collect()
    });
    Ok(fields.unwrap_or_default())
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    if let Some(mut answer) = self.answer.take() {
      out.append(&mut answer.body);
    }
    Ok(false)
  }
}

/// What git's site serves a request with: one of git's services, or Git LFS.
pub(crate) enum SiteExchange<E, L> {
  /// git's service.
  Git(GitExchange<E>),
  /// Git LFS.
  Lfs(LfsExchange<L>),
}

impl<E: Exchange, L: LfsService> Exchange for SiteExchange<E, L> {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    match self {
      SiteExchange::Git(git) => git.body(run).await,
      SiteExchange::Lfs(lfs) => lfs.body(run).await,
    }
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    match self {
      SiteExchange::Git(git) => git.body_end().await,
      SiteExchange::Lfs(lfs) => lfs.body_end().await,
    }
  }

  async fn reply_status(&mut self) -> Result<Option<Status>, Failed> {
    match self {
      SiteExchange::Git(git) => git.reply_status().await,
      SiteExchange::Lfs(lfs) => lfs.reply_status().await,
    }
  }

  async fn reply_fields(&mut self) -> Result<Fields, Failed> {
    match self {
      SiteExchange::Git(git) => git.reply_fields().await,
      SiteExchange::Lfs(lfs) => lfs.reply_fields().await,
    }
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    match self {
      SiteExchange::Git(git) => git.reply(out).await,
      SiteExchange::Lfs(lfs) => lfs.reply(out).await,
    }
  }
}

/// How an LFS request's repository stands with its principal (the module doc, steps 3 and 4).
enum Gate {
  /// Here, and the policy lets the principal reach it for some LFS right.
  Open,
  /// On runtime shard `shard`, where the connection moves.
  Elsewhere(u16),
  /// Refused, by kind.
  Refused(Refused),
}

/// What a routed request asks for.
struct Asked {
  /// The repository's name.
  repository: String,
  /// The service.
  service: Service,
  /// The protocol version.
  version: Version,
  /// Whether it asks for the advertisement.
  advertise: bool,
  /// Whether its reply is the head alone (`HEAD`).
  head_only: bool,
}

impl Asked {
  /// What `route` asks for.
  fn of(route: Route) -> Asked {
    match route {
      Route::Advertise {
        repo,
        service,
        version,
        head_only,
      } => Asked {
        repository: repo,
        service,
        version,
        advertise: true,
        head_only,
      },
      Route::Serve {
        repo,
        service,
        version,
      } => Asked {
        repository: repo,
        service,
        version,
        advertise: false,
        head_only: false,
      },
    }
  }
}

/// git's site: the realm its challenges name, the room a route's decoded path is charged to, and the authenticator,
/// repositories, services and Git LFS service it decides with (the module doc).
pub(crate) struct GitSite<A, R, S, L = NoLfs> {
  /// The realm a `401`'s challenges name.
  realm: String,
  /// The room a request's decoded path is charged to: the edge's request room.
  room: u64,
  /// What proves credentials.
  authenticator: A,
  /// Where repositories are found.
  repositories: R,
  /// The services.
  services: S,
  /// Git LFS's service.
  lfs: L,
}

impl<A, R, S> GitSite<A, R, S, NoLfs> {
  /// A site naming `realm` in its challenges, charging each route's path to `room`; it serves no Git LFS endpoint until
  /// it is given a service ([`GitSite::with_lfs`]).
  pub(crate) fn new(
    realm: String,
    room: u64,
    authenticator: A,
    repositories: R,
    services: S,
  ) -> Self {
    Self {
      realm,
      room,
      authenticator,
      repositories,
      services,
      lfs: NoLfs,
    }
  }
}

impl<A, R, S, L> GitSite<A, R, S, L> {
  /// The site serving Git LFS through `lfs`.
  pub(crate) fn with_lfs<M>(self, lfs: M) -> GitSite<A, R, S, M> {
    GitSite {
      realm: self.realm,
      room: self.room,
      authenticator: self.authenticator,
      repositories: self.repositories,
      services: self.services,
      lfs,
    }
  }
}

impl<A: Authenticator, R: Repositories, S: GitServices, L: LfsService + Clone> GitSite<A, R, S, L> {
  /// The answer to `head`, on a connection that is TLS when `secure`, in the module doc's order: refused by kind, moved to
  /// the shard its repository lives on, or served.
  pub(crate) async fn answer(
    &mut self,
    head: &Head<'_>,
    secure: bool,
  ) -> Decided<SiteExchange<S::Exchange, L>> {
    match lfs_serve::route(head, &mut Admission::new(self.room)) {
      Ok(routed) => return self.answer_lfs(head, secure, routed).await,
      // Not an LFS path: git's own routes decide it.
      Err(RouteRefused::NotFound) => {}
      Err(refused) => return lfs_route_refused(refused, head.version),
    }
    let route = match http::route(head, &mut Admission::new(self.room)) {
      Ok(route) => route,
      Err(refused) => {
        let (status, fields) = http::refusal_parts(refused, head.version);
        return refused_with(status, owned(&fields), false);
      }
    };
    let proven = match self.proof(head, secure).await {
      Ok(proven) => proven,
      Err(refused) => return self.refuse(refused, secure, false),
    };
    let asked = Asked::of(route);
    if let Located::Elsewhere(shard) = self.repositories.locate(&asked.repository) {
      return Decided {
        answer: Answer::Move { shard },
        proven: true,
      };
    }
    let Some(repository) = self
      .repositories
      .find(&asked.repository)
      .filter(|repository| repository.tool == VcsTool::Git)
    else {
      return self.refuse(Refused::NoRepository, secure, true);
    };
    match vcs_policy::reaches(&repository, &proven.principal, rights(asked.service)) {
      Ok(true) => self.start(&asked, &repository, &proven.principal, secure),
      Ok(false) => self.refuse(Refused::Forbidden, secure, true),
      // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
      Err(_) => self.refuse(Refused::Unreadable, secure, true),
    }
  }

  /// The answer to an LFS request routed to `routed` (the module doc): a request naming no authority `400`; its
  /// credentials, a `401` naming its challenges for git-lfs too; its repository and the policy's gate; an endpoint the
  /// service does not serve `503`; then served, its status, fields and body its service's, decided once its body has
  /// ended.
  async fn answer_lfs(
    &mut self,
    head: &Head<'_>,
    secure: bool,
    routed: Routed,
  ) -> Decided<SiteExchange<S::Exchange, L>> {
    let Some(authority) = authority(head) else {
      return refused_with(Status::BAD_REQUEST, cache_fields(), false);
    };
    let proven = match self.proof(head, secure).await {
      Ok(proven) => proven,
      Err(refused) => return lfs_challenged(self.refuse(refused, secure, false)),
    };
    match self.lfs_gate(&routed.repo, &proven.principal) {
      Gate::Open => {}
      Gate::Elsewhere(shard) => {
        return Decided {
          answer: Answer::Move { shard },
          proven: true,
        };
      }
      Gate::Refused(refused) => return self.refuse(refused, secure, true),
    }
    if !self.lfs.serves(&routed.endpoint) {
      return self.refuse(Refused::Unavailable, secure, true);
    }
    let exchange = LfsExchange {
      service: self.lfs.clone(),
      routed,
      query: query_of(head),
      principal: proven.principal,
      credential_expiry: expiry_of(proven.bearer_expires_ns),
      scheme: if secure { "https" } else { "http" },
      authority,
      room: self.room,
      body: Vec::new(),
      answer: None,
    };
    Decided {
      answer: Answer::Serve {
        fields: Fields::new(),
        head_only: false,
        status: StatusMode::AfterBody,
        exchange: SiteExchange::Lfs(exchange),
      },
      proven: true,
    }
  }

  /// How the repository named `name` stands with `principal` for Git LFS (the module doc, steps 3 and 4).
  fn lfs_gate(&self, name: &str, principal: &Principal) -> Gate {
    if let Located::Elsewhere(shard) = self.repositories.locate(name) {
      return Gate::Elsewhere(shard);
    }
    let found = self.repositories.find(name);
    let Some(repository) = found.filter(|repository| repository.tool == VcsTool::Git) else {
      return Gate::Refused(Refused::NoRepository);
    };
    match vcs_policy::reaches(&repository, principal, LFS_RIGHTS) {
      Ok(true) => Gate::Open,
      Ok(false) => Gate::Refused(Refused::Forbidden),
      // As for git's services: a policy its tool cannot read is never judged without.
      Err(_) => Gate::Refused(Refused::Unreadable),
    }
  }

  /// What `head`'s credentials prove, or why they prove no one.
  async fn proof(&self, head: &Head<'_>, secure: bool) -> Result<Proven, Refused> {
    let credential = match credential::credential(head, secure) {
      Ok(Some(credential)) => credential,
      Ok(None) | Err(CredentialRefused::UnknownScheme) => return Err(Refused::NoCredentials),
      Err(CredentialRefused::BasicInCleartext) => return Err(Refused::CleartextBasic),
      Err(CredentialRefused::Repeated | CredentialRefused::Malformed) => {
        return Err(Refused::MalformedCredentials);
      }
    };
    self
      .authenticator
      .prove(&credential)
      .await
      .map_err(Refused::Unproven)
  }

  /// `refused`'s answer, counted under its kind: `401` with the challenges a connection that is TLS when `secure`
  /// accepts, any other status with the cache fields. `proven` when the credentials proved a principal.
  fn refuse(
    &self,
    refused: Refused,
    secure: bool,
    proven: bool,
  ) -> Decided<SiteExchange<S::Exchange, L>> {
    crate::fleet::count_refusal(refused.counter());
    let status = refused.status();
    if status == Status::UNAUTHORIZED {
      return self.unauthorized(secure);
    }
    refused_with(status, cache_fields(), proven)
  }

  /// `401` with the challenges a connection that is TLS when `secure` accepts, then the cache fields.
  fn unauthorized(&self, secure: bool) -> Decided<SiteExchange<S::Exchange, L>> {
    let Ok(challenges) = credential::challenges(&self.realm, secure) else {
      // A realm a challenge cannot carry is the operator's misconfiguration, never the client's.
      return refused_with(Status::INTERNAL_SERVER_ERROR, cache_fields(), false);
    };
    let mut fields: Fields = challenges
      .into_iter()
      .map(|challenge| (credential::WWW_AUTHENTICATE.to_vec(), challenge))
      .collect();
    fields.extend(cache_fields());
    refused_with(Status::UNAUTHORIZED, fields, false)
  }

  /// Starts the service `asked` names for `principal` on `repository`: served with its reply's fields and its exchange,
  /// or `503` while it has not landed or the repository's store is not here.
  fn start(
    &mut self,
    asked: &Asked,
    repository: &RepositoryRecord,
    principal: &Principal,
    secure: bool,
  ) -> Decided<SiteExchange<S::Exchange, L>> {
    let start = Start {
      repository,
      principal,
      service: asked.service,
      version: asked.version,
      advertise: asked.advertise,
    };
    let service = match self.services.start(&start) {
      Ok(service) => service,
      Err(ServiceRefused::Unavailable) => {
        return self.refuse(Refused::Unavailable, secure, true);
      }
    };
    let mut prelude = Vec::new();
    let reply = if asked.advertise {
      if http::advertisement_prelude(asked.service, asked.version, &mut prelude).is_err() {
        return refused_with(Status::INTERNAL_SERVER_ERROR, cache_fields(), true);
      }
      Reply::Advertisement(asked.service)
    } else {
      Reply::Result(asked.service)
    };
    Decided {
      answer: Answer::Serve {
        fields: owned(&http::reply_fields(reply)),
        head_only: asked.head_only,
        status: StatusMode::Streamed,
        exchange: SiteExchange::Git(GitExchange { prelude, service }),
      },
      proven: true,
    }
  }
}

impl<A: Authenticator, R: Repositories, S: GitServices, L: LfsService + Clone> Site
  for GitSite<A, R, S, L>
{
  type Exchange = SiteExchange<S::Exchange, L>;

  async fn decide(&mut self, head: &Head<'_>, secure: bool) -> Decided<Self::Exchange> {
    self.answer(head, secure).await
  }
}

/// The rights a principal must hold on some name of a repository for `service` to start (the module doc, step 4).
fn rights(service: Service) -> &'static [Right] {
  match service {
    Service::UploadPack => &[Right::Read],
    Service::ReceivePack => &[Right::Submit, Right::Rewrite, Right::Delete],
  }
}

/// A refusal with `status` and `fields`; `proven` when the request's credentials proved a principal.
fn refused_with<E>(status: Status, fields: Fields, proven: bool) -> Decided<E> {
  Decided {
    answer: Answer::Refuse { status, fields },
    proven,
  }
}

/// The cache fields `http-backend` sends on every answer, as the edge's fields.
fn cache_fields() -> Fields {
  owned(&http::no_cache_fields())
}

/// `fields` as the edge's fields.
fn owned(fields: &[Field]) -> Fields {
  fields
    .iter()
    .map(|(name, value)| (name.to_vec(), value.to_vec()))
    .collect()
}

/// An LFS route's refusal, whoever asks (the module doc): its status, `Allow` naming the endpoint's methods on a `405`,
/// then the cache fields every refusal of the site carries, as git's own routes' do.
fn lfs_route_refused<E>(refused: RouteRefused, version: HttpVersion) -> Decided<E> {
  let status = refused.status(version);
  let mut fields = match refused {
    RouteRefused::MethodNotAllowed(allow) if status == Status::METHOD_NOT_ALLOWED => {
      vec![(ALLOW.to_vec(), allow.methods().to_vec())]
    }
    _ => Fields::new(),
  };
  fields.extend(cache_fields());
  refused_with(status, fields, false)
}

/// `decided`, a `401`'s every `WWW-Authenticate` challenge named again in `LFS-Authenticate` (the module doc).
fn lfs_challenged<E>(mut decided: Decided<E>) -> Decided<E> {
  if let Answer::Refuse { status, fields } = &mut decided.answer
    && *status == Status::UNAUTHORIZED
  {
    let challenges: Fields = fields
      .iter()
      .filter(|(name, _)| name.eq_ignore_ascii_case(credential::WWW_AUTHENTICATE))
      .map(|(_, value)| (LFS_AUTHENTICATE.to_vec(), value.clone()))
      .collect();
    fields.extend(challenges);
  }
  decided
}

/// The authority `head` names, which an LFS answer's hrefs are made under: an absolute-form target's, else `Host`'s, as
/// slates-http read and checked it (RFC 9112 §3.2, RFC 3986 §3.2); none for an HTTP/1.0 request that named none, or an
/// empty one.
fn authority(head: &Head<'_>) -> Option<String> {
  let authority = std::str::from_utf8(head.authority?).ok()?;
  (!authority.is_empty()).then(|| authority.to_owned())
}

/// `head`'s query, without its `?`.
fn query_of(head: &Head<'_>) -> Vec<u8> {
  match head.target {
    Target::Path { query, .. } => query.unwrap_or_default().to_vec(),
    Target::Asterisk => Vec::new(),
  }
}

/// An LFS action's expiry for a credential expiring at `ns` on the wall clock (the module doc): the second it falls in,
/// so no action outlives its credential; none for a credential that never expires, or for one past RFC 3339's years,
/// which no action's `expires_at` can name.
fn expiry_of(ns: Option<u64>) -> Option<Timestamp> {
  let seconds = ns?.checked_div(NS_PER_SECOND)?;
  Timestamp::from_unix(i64::try_from(seconds).ok()?)
}

#[cfg(test)]
#[path = "tests/git.rs"]
mod tests;
