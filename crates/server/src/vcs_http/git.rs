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
//!    store by [`upload`] (A-640 part 2, A-625 part 3), the store lent from the owner shard's map a step at a time and
//!    the refs lent those the principal may read (A-682); receive-pack answers `503` until its session's entry point
//!    lands (A-654).
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

use slates_db::catalog::{Principal, RepositoryRecord, VcsTool};
use slates_git::http::{self, Field, Reply, Route, Service, Version};
use slates_http::Admission;
use slates_http::head::Head;
use slates_http::reply::Status;
use slates_http_edge::connection::Fields;
use slates_http_edge::credential::{self, Credential, CredentialRefused};

use super::{Answer, Decided, Exchange, Failed, Site};
use crate::vcs_policy::{self, Right};

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
/// repositories and services it decides with (the module doc).
pub(crate) struct GitSite<A, R, S> {
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
}

impl<A: Authenticator, R: Repositories, S: GitServices> GitSite<A, R, S> {
  /// A site naming `realm` in its challenges, charging each route's path to `room`.
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
    }
  }

  /// The answer to `head`, on a connection that is TLS when `secure`, in the module doc's order: refused by kind, moved to
  /// the shard its repository lives on, or served.
  pub(crate) async fn answer(
    &mut self,
    head: &Head<'_>,
    secure: bool,
  ) -> Decided<GitExchange<S::Exchange>> {
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
  ) -> Decided<GitExchange<S::Exchange>> {
    crate::fleet::count_refusal(refused.counter());
    let status = refused.status();
    if status == Status::UNAUTHORIZED {
      return self.unauthorized(secure);
    }
    refused_with(status, cache_fields(), proven)
  }

  /// `401` with the challenges a connection that is TLS when `secure` accepts, then the cache fields.
  fn unauthorized(&self, secure: bool) -> Decided<GitExchange<S::Exchange>> {
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
  ) -> Decided<GitExchange<S::Exchange>> {
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
        exchange: GitExchange { prelude, service },
      },
      proven: true,
    }
  }
}

impl<A: Authenticator, R: Repositories, S: GitServices> Site for GitSite<A, R, S> {
  type Exchange = GitExchange<S::Exchange>;

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

#[cfg(test)]
#[path = "tests/git.rs"]
mod tests;
