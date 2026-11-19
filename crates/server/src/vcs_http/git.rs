//! git's site on the HTTP edge (A-625 part 2; §4.17.2 "Transports" item 1 and "Access"): each request's head routed as
//! `git http-backend` routes it (`slates_git::http`, A-595), its credentials proved to a principal of §4.13 by an
//! authenticator ([`Authenticator`]), its repository found ([`Repositories`]), the principal's reach of it decided by the
//! repository's policy (`crate::vcs_policy::reaches`, A-597, A-622), and its service started ([`GitServices`]).
//!
//! **The order**, each refusal with its status and the three cache fields `http-backend` sends on every answer:
//! 1. The route. A path, method, service or content type `http-backend` refuses is refused as it refuses it, whoever
//!    asks: its syntax names no repository, so a client learns its mistake without credentials.
//! 2. The credentials. None, another scheme, Basic in cleartext, or a credential that proves no principal is `401` with
//!    the challenges the connection accepts (RFC 9110 §11.6.1: "A server generating a 401 (Unauthorized) response MUST
//!    send a WWW-Authenticate header field containing at least one challenge"; Basic over TLS alone, RFC 7617 §4); a
//!    repeated or malformed `Authorization` is `400`. Nothing about a repository is answered to a client that has proved
//!    no one.
//! 3. The repository. None of that name, or one of another tool, is `404` (§4.17.2: "No repository is `404`").
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
use slates_http_edge::credential::{self, Credential};

use super::{Answer, Decided, Exchange, Failed, Site};
use crate::vcs_policy::{self, Right};

mod upload;

pub(crate) use upload::{StoreLender, UploadServices};

/// What proves a request's credentials to a principal (§4.13): the token a Basic credential carries as its password, or a
/// bearer token, looked up among the principals' keys. A credential that proves no one is `401`.
pub(crate) trait Authenticator {
  /// The principal `credential` proves, if any.
  fn principal(&self, credential: &Credential) -> Option<Principal>;
}

/// Where a repository is found by its name (§4.17.1: a name unique on the partition a stable hash of it chooses).
pub(crate) trait Repositories {
  /// The repository named `name`, if one is.
  fn find(&self, name: &str) -> Option<RepositoryRecord>;
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

/// What a request's credentials came to.
enum Proof {
  /// The principal they prove.
  Principal(Principal),
  /// None proved: `401` with the challenges.
  Unauthorized,
  /// The `Authorization` line itself refused, with this status (`400`).
  Refused(Status),
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

  /// The answer to `head`, on a connection that is TLS when `secure`, in the module doc's order.
  pub(crate) fn answer(
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
    let principal = match self.proof(head, secure) {
      Proof::Principal(principal) => principal,
      Proof::Unauthorized => return self.unauthorized(secure),
      Proof::Refused(status) => return refused_with(status, cache_fields(), false),
    };
    let asked = Asked::of(route);
    let Some(repository) = self
      .repositories
      .find(&asked.repository)
      .filter(|repository| repository.tool == VcsTool::Git)
    else {
      return refused_with(Status::NOT_FOUND, cache_fields(), true);
    };
    match vcs_policy::reaches(&repository, &principal, rights(asked.service)) {
      Ok(true) => self.start(&asked, &repository, &principal),
      Ok(false) => refused_with(Status::FORBIDDEN, cache_fields(), true),
      // A policy its tool cannot read is a damaged record (A-622's guard keeps such rules out): never judged without it.
      Err(_) => refused_with(Status::INTERNAL_SERVER_ERROR, cache_fields(), true),
    }
  }

  /// What `head`'s credentials prove.
  fn proof(&self, head: &Head<'_>, secure: bool) -> Proof {
    match credential::credential(head, secure) {
      Ok(Some(credential)) => self
        .authenticator
        .principal(&credential)
        .map_or(Proof::Unauthorized, Proof::Principal),
      Ok(None) => Proof::Unauthorized,
      Err(refused) if refused.status() == Status::UNAUTHORIZED => Proof::Unauthorized,
      Err(refused) => Proof::Refused(refused.status()),
    }
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
  /// or `503` while it has not landed.
  fn start(
    &mut self,
    asked: &Asked,
    repository: &RepositoryRecord,
    principal: &Principal,
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
        return refused_with(Status::SERVICE_UNAVAILABLE, cache_fields(), true);
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
    self.answer(head, secure)
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
