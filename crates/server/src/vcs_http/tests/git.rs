use std::future::Future;
use std::task::{Context, Poll, Waker};

use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_git::http::{Reply, Service, Version, no_cache_fields, reply_fields};
use slates_git::lfs::Timestamp;
use slates_git::lfs::serve::{Answer as LfsAnswer, CREATED, Endpoint, Kind, NOT_ACCEPTABLE};
use slates_http::Admission;
use slates_http::head::{Head, HeadReader, Progress};
use slates_http::reply::Status;
use slates_http_edge::connection::{Fields, StatusMode};
use slates_http_edge::credential::Credential;

use slates_git::object_name::ObjectFormat as GitFormat;
use slates_git::store::Repository as GitStore;
use slates_git::upload::{PackPlan, PackRefused, PackSource, Served as UploadServed};
use slates_vcs::ContentStore;

use super::{
  Authenticator, GitServices, GitSite, LfsService, Located, Proven, Refused, Repositories,
  ServiceRefused, Start, StoreLender, Unproven, UploadServices,
};
use crate::lfs::LfsCall;
use crate::vcs_http::daemon_edge::ShardLfs;
use crate::vcs_http::{Answer, Decided, Exchange, Failed};

/// Shape: room enough for every head these tests send.
const ROOM: u64 = 1 << 20;
/// Shape: the realm the tests' site names.
const REALM: &str = "slates";
/// Shape: the uid of the tests' repositories' owner.
const OWNER: u32 = 1;
/// Shape: the uid of a principal the owner's rules may name.
const STRANGER: u32 = 2;
/// Shape: the token that proves the owner, as Basic's password or a bearer token.
const OWNER_TOKEN: &[u8] = b"owner-token";
/// Shape: the token that proves the stranger.
const STRANGER_TOKEN: &[u8] = b"stranger-token";
/// Shape: upload-pack's advertisement for the git repository `r.git`.
const UPLOAD_ADVERTISEMENT: &str = "/r.git/info/refs?service=git-upload-pack";
/// Shape: receive-pack's.
const RECEIVE_ADVERTISEMENT: &str = "/r.git/info/refs?service=git-receive-pack";
/// Shape: the owner's credentials as a bearer token, and as Basic's (`git:owner-token`).
const OWNER_BEARER: &str = "Authorization: Bearer owner-token";
/// Shape: as [`OWNER_BEARER`].
const OWNER_BASIC: &str = "Authorization: Basic Z2l0Om93bmVyLXRva2Vu";
/// Shape: the stranger's, as a bearer token.
const STRANGER_BEARER: &str = "Authorization: Bearer stranger-token";
/// Shape: the challenges a `401` carries over TLS, then over cleartext.
const BASIC_CHALLENGE: &str = "Basic realm=\"slates\", charset=\"UTF-8\"";
/// Shape: as [`BASIC_CHALLENGE`].
const BEARER_CHALLENGE: &str = "Bearer realm=\"slates\"";

/// Services that start nothing, each answering [`ServiceRefused::Unavailable`]: the stand-in for the tests of the
/// site's own order (route, credentials, repository, policy), which no service's answer reaches.
#[derive(Clone, Copy, Debug, Default)]
struct Unserved;

/// The exchange [`Unserved`] never starts: no value has this type.
enum NoExchange {}

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

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Shape: a token whose credential expired.
const EXPIRED_TOKEN: &[u8] = b"expired-token";
/// Shape: a token of a consumer a human revoked.
const REVOKED_TOKEN: &[u8] = b"revoked-token";
/// Shape: a token whose record's partition did not answer.
const SILENT_TOKEN: &[u8] = b"silent-token";
/// Shape: a token that proves the owner and expires at [`EXPIRES_NS`].
const EXPIRING_TOKEN: &[u8] = b"expiring-token";
/// Shape: [`EXPIRING_TOKEN`]'s expiry on the wall clock, half a second into the second 1,800,000,000 of the Unix epoch.
const EXPIRES_NS: u64 = 1_800_000_000_500_000_000;
/// Shape: the second [`EXPIRES_NS`] falls in, which an LFS action's expiry names.
const EXPIRES_SECOND: i64 = 1_800_000_000;

/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, from Basic's password or a bearer token, and
/// [`EXPIRING_TOKEN`] the owner until [`EXPIRES_NS`]; answers [`EXPIRED_TOKEN`], [`REVOKED_TOKEN`] and [`SILENT_TOKEN`]
/// with why each proves no one.
struct Tokens;

impl Authenticator for Tokens {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    let unproven = [
      (EXPIRED_TOKEN, Unproven::Expired),
      (REVOKED_TOKEN, Unproven::Revoked),
      (SILENT_TOKEN, Unproven::Unanswered),
    ];
    if let Some((_, why)) = unproven.into_iter().find(|(token, _)| *token == secret) {
      return Err(why);
    }
    [
      (OWNER_TOKEN, OWNER, None),
      (STRANGER_TOKEN, STRANGER, None),
      (EXPIRING_TOKEN, OWNER, Some(EXPIRES_NS)),
    ]
    .into_iter()
    .find(|(token, _, _)| *token == secret)
    .map(|(_, uid, expires)| Proven {
      principal: principal(uid),
      bearer_expires_ns: expires,
    })
    .ok_or(Unproven::Unknown)
  }
}

/// Holds the git repository `r.git` (the owner's, its policy `rules`) and the Mercurial repository `hg`.
struct Held {
  /// The git repository's rules.
  rules: Vec<PolicyRule>,
}

/// Shape: the runtime shard [`ELSEWHERE`] lives on.
const ELSEWHERE_SHARD: u16 = 7;
/// Shape: a repository another shard's partition holds.
const ELSEWHERE: &str = "/elsewhere.git/info/refs?service=git-upload-pack";

impl Repositories for Held {
  fn locate(&self, name: &str) -> Located {
    if name == "elsewhere.git" {
      Located::Elsewhere(ELSEWHERE_SHARD)
    } else {
      Located::Here
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (tool, rules) = match name {
      "r.git" => (VcsTool::Git, self.rules.clone()),
      "hg" => (VcsTool::Mercurial, Vec::new()),
      _ => return None,
    };
    Some(RepositoryRecord {
      id: RepositoryId { bytes: [0; 16] },
      name: name.to_owned(),
      tool,
      format: ObjectFormat::Sha1,
      default_branch: None,
      owner: principal(OWNER),
      policy: RepositoryPolicy {
        version: 1,
        rules,
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    })
  }
}

/// A service's exchange that advertises what started it ([`started_as`]), or answers a request with how many body bytes it
/// took.
struct Scripted {
  /// The reply not yet given.
  reply: Option<Vec<u8>>,
  /// The body's bytes taken.
  took: usize,
}

impl Exchange for Scripted {
  async fn body(&mut self, run: &[u8]) -> Result<(), Failed> {
    self.took += run.len();
    Ok(())
  }

  async fn body_end(&mut self) -> Result<(), Failed> {
    self.reply = Some(format!("took {}", self.took).into_bytes());
    Ok(())
  }

  async fn reply(&mut self, out: &mut Vec<u8>) -> Result<bool, Failed> {
    match self.reply.take() {
      Some(reply) => {
        out.extend(reply);
        Ok(true)
      }
      None => Ok(false),
    }
  }
}

/// What the tests' services advertise: the repository, principal, service and version they were started with.
fn started_as(
  repository: &str,
  principal: &Principal,
  service: Service,
  version: Version,
) -> Vec<u8> {
  format!("{repository} {principal:?} {} {version:?}", service.name()).into_bytes()
}

/// Services that serve, each with a [`Scripted`] exchange whose advertisement names what started it.
struct Served;

impl GitServices for Served {
  type Exchange = Scripted;

  fn start(&mut self, start: &Start<'_>) -> Result<Scripted, ServiceRefused> {
    let advertised = started_as(
      &start.repository.name,
      start.principal,
      start.service,
      start.version,
    );
    Ok(Scripted {
      reply: start.advertise.then_some(advertised),
      took: 0,
    })
  }
}

/// A site over [`Tokens`] and [`Held`] with `rules` on `r.git`, and `services`.
fn site<S: GitServices>(rules: Vec<PolicyRule>, services: S) -> GitSite<Tokens, Held, S> {
  GitSite::new(REALM.to_owned(), ROOM, Tokens, Held { rules }, services)
}

/// The head `input` holds; it must be whole.
fn head(input: &[u8]) -> Head<'_> {
  match HeadReader::new().read(input, &mut Admission::new(ROOM)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// `method target` over HTTP/1.1 with `Host` and `fields`.
fn request(method: &str, target: &str, fields: &[&str]) -> Vec<u8> {
  let mut text = format!("{method} {target} HTTP/1.1\r\nHost: git.example\r\n");
  for field in fields {
    text.push_str(field);
    text.push_str("\r\n");
  }
  text.push_str("\r\n");
  text.into_bytes()
}

/// `pairs` as fields, then `http-backend`'s cache fields.
fn with_cache(pairs: &[(&str, &str)]) -> Fields {
  let mut fields: Fields = pairs
    .iter()
    .map(|(name, value)| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
    .collect();
  fields.extend(
    no_cache_fields()
      .iter()
      .map(|(name, value)| (name.to_vec(), value.to_vec())),
  );
  fields
}

/// A refusal's status, its fields and whether its credentials proved a principal; panics on a served answer.
fn refusal<E>(decided: &Decided<E>) -> (Status, Fields, bool) {
  match &decided.answer {
    Answer::Refuse { status, fields } => (*status, fields.clone(), decided.proven),
    Answer::Serve { .. } => panic!("served where a refusal was expected"),
    Answer::Move { shard } => panic!("moved to shard {shard} where a refusal was expected"),
  }
}

/// A served answer's fields, whether it is the head alone, its exchange, and whether its credentials proved a principal;
/// panics on a refusal.
fn served<E>(decided: Decided<E>) -> (Fields, bool, E, bool) {
  match decided.answer {
    Answer::Serve {
      fields,
      head_only,
      exchange,
      ..
    } => (fields, head_only, exchange, decided.proven),
    Answer::Refuse { status, .. } => panic!("refused {status:?} where a service was expected"),
    Answer::Move { shard } => panic!("moved to shard {shard} where a service was expected"),
  }
}

/// What `future` comes to at its first poll: every exchange in these tests answers without waiting.
fn ready<F: Future>(future: F) -> F::Output {
  let mut context = Context::from_waker(Waker::noop());
  match std::pin::pin!(future).poll(&mut context) {
    Poll::Ready(value) => value,
    Poll::Pending => panic!("the tests' exchanges answer at once"),
  }
}

/// Every run an exchange replies, in order, until its reply ends: a step's bytes are kept whether or not more follow.
fn replies<E: Exchange>(exchange: &mut E) -> Vec<Vec<u8>> {
  let mut runs = Vec::new();
  loop {
    let mut run = Vec::new();
    let more = ready(exchange.reply(&mut run)).unwrap();
    if !run.is_empty() {
      runs.push(run);
    }
    if !more {
      return runs;
    }
  }
}

/// A-625 part 2 (RFC 9110 §11.6.1; RFC 7617 §4). Do: ask for upload-pack's advertisement with no credentials, over TLS
/// and over a cleartext loopback connection. Expect: `401`, unproven, with Basic's and Bearer's challenges over TLS and
/// Bearer's alone in cleartext, each naming the realm, then `http-backend`'s cache fields.
#[test]
fn a_request_without_credentials_is_401_with_the_challenges_its_connection_accepts() {
  let mut site = site(Vec::new(), Unserved);
  let asked = request("GET", UPLOAD_ADVERTISEMENT, &[]);
  let www = "WWW-Authenticate";
  assert_eq!(
    refusal(&ready(site.answer(&head(&asked), true))),
    (
      Status::UNAUTHORIZED,
      with_cache(&[(www, BASIC_CHALLENGE), (www, BEARER_CHALLENGE)]),
      false
    )
  );
  assert_eq!(
    refusal(&ready(site.answer(&head(&asked), false))),
    (
      Status::UNAUTHORIZED,
      with_cache(&[(www, BEARER_CHALLENGE)]),
      false
    )
  );
}

/// A-625 part 2 (RFC 7617 §4, RFC 9110 §11.4 and §5.3). Do: send Basic credentials over cleartext, a bearer token that
/// proves no one, another scheme, and two `Authorization` lines. Expect: the first three `401` with the connection's
/// challenges, unproven; the repeated line `400` with the cache fields alone.
#[test]
fn credentials_that_prove_no_principal_are_401_and_a_repeated_line_400() {
  let mut site = site(Vec::new(), Unserved);
  let www = "WWW-Authenticate";
  let unproven = [
    (OWNER_BASIC, false),
    ("Authorization: Bearer someone-else", true),
    ("Authorization: Digest username=\"git\"", true),
  ];
  for (credential, secure) in unproven {
    let asked = request("GET", UPLOAD_ADVERTISEMENT, &[credential]);
    let (status, fields, proven) = refusal(&ready(site.answer(&head(&asked), secure)));
    let bearer = (
      www.as_bytes().to_vec(),
      BEARER_CHALLENGE.as_bytes().to_vec(),
    );
    assert_eq!(
      (status, fields.contains(&bearer), proven),
      (Status::UNAUTHORIZED, true, false),
      "{credential}"
    );
  }
  let repeated = request("GET", UPLOAD_ADVERTISEMENT, &[OWNER_BEARER, OWNER_BEARER]);
  assert_eq!(
    refusal(&ready(site.answer(&head(&repeated), true))),
    (Status::BAD_REQUEST, with_cache(&[]), false)
  );
}

/// A-625 part 2 (§4.17.2 "Transports" item 1; gitprotocol-http's `403`). Do: as the owner, ask for a repository that does
/// not exist and for a Mercurial repository; as a principal no rule names, ask for the owner's git repository; then as the
/// owner ask for it while its services are unserved. Expect: `404`, `404`, `403` and `503` in that order of checks, each
/// with the cache fields alone and each proven, since the credentials proved a principal.
#[test]
fn a_proven_principal_meets_404_then_403_then_503() {
  let mut site = site(Vec::new(), Unserved);
  let cases = [
    (
      "/missing.git/info/refs?service=git-upload-pack",
      OWNER_BEARER,
      Status::NOT_FOUND,
    ),
    (
      "/hg/info/refs?service=git-upload-pack",
      OWNER_BEARER,
      Status::NOT_FOUND,
    ),
    (UPLOAD_ADVERTISEMENT, STRANGER_BEARER, Status::FORBIDDEN),
    (
      UPLOAD_ADVERTISEMENT,
      OWNER_BASIC,
      Status::SERVICE_UNAVAILABLE,
    ),
  ];
  for (target, credential, expected) in cases {
    let asked = request("GET", target, &[credential]);
    assert_eq!(
      refusal(&ready(site.answer(&head(&asked), true))),
      (expected, with_cache(&[]), true),
      "{target} with {credential}"
    );
  }
}

/// A-625 part 2 (the policy's gate, `vcs_policy::reaches`). Do: with a rule letting the stranger read `refs/heads/*`, ask
/// as the stranger for upload-pack's advertisement and for receive-pack's. Expect: upload-pack past the gate (`503`
/// while it is unserved), receive-pack `403`, since no rule lets the stranger push.
#[test]
fn a_rule_that_lets_a_principal_read_admits_it_to_upload_pack_alone() {
  let rules = vec![PolicyRule {
    names: PolicyNames::Refs,
    pattern: "refs/heads/".to_owned(),
    right: PolicyRight::Read,
    effect: PolicyEffect::Allow,
    who: PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: principal(STRANGER),
      }],
    },
  }];
  let mut site = site(rules, Unserved);
  let upload = request("GET", UPLOAD_ADVERTISEMENT, &[STRANGER_BEARER]);
  let receive = request("GET", RECEIVE_ADVERTISEMENT, &[STRANGER_BEARER]);
  assert_eq!(
    (
      refusal(&ready(site.answer(&head(&upload), true))).0,
      refusal(&ready(site.answer(&head(&receive), true))).0
    ),
    (Status::SERVICE_UNAVAILABLE, Status::FORBIDDEN)
  );
}

/// A-625 part 2 (gitprotocol-http's advertisement; RFC 9110 §9.3.2's `HEAD`). Do: as the owner, with services that serve,
/// ask for upload-pack's advertisement at v0, at v2, and by `HEAD`. Expect: each served with `http-backend`'s fields for
/// an advertisement, proven, the service started for the owner on `r.git` at the version asked; at v0 the `# service=`
/// pkt-line and its flush before the service's bytes, at v2 the service's bytes alone; the `HEAD` the head alone.
#[test]
fn a_served_advertisement_is_its_prelude_then_its_service() {
  let mut site = site(Vec::new(), Served);
  let fields: Fields = reply_fields(Reply::Advertisement(Service::UploadPack))
    .iter()
    .map(|(name, value)| (name.to_vec(), value.to_vec()))
    .collect();
  let owner = principal(OWNER);
  let v0 = request("GET", UPLOAD_ADVERTISEMENT, &[OWNER_BEARER]);
  let (served_fields, head_only, mut exchange, proven) =
    served(ready(site.answer(&head(&v0), true)));
  assert_eq!((&served_fields, head_only, proven), (&fields, false, true));
  assert_eq!(
    replies(&mut exchange),
    [
      b"001e# service=git-upload-pack\n0000".to_vec(),
      started_as("r.git", &owner, Service::UploadPack, Version::V0)
    ]
  );
  let v2 = request(
    "GET",
    UPLOAD_ADVERTISEMENT,
    &[OWNER_BEARER, "Git-Protocol: version=2"],
  );
  let (_, _, mut exchange, _) = served(ready(site.answer(&head(&v2), true)));
  assert_eq!(
    replies(&mut exchange),
    [started_as(
      "r.git",
      &owner,
      Service::UploadPack,
      Version::V2
    )]
  );
  let only_head = request("HEAD", UPLOAD_ADVERTISEMENT, &[OWNER_BEARER]);
  assert!(served(ready(site.answer(&head(&only_head), true))).1);
}

/// A-625 part 2 (gitprotocol-http's service requests). Do: as the owner, POST a receive-pack request of its content type
/// with a three-byte body, and hand the body to the started service. Expect: served with `http-backend`'s fields for a
/// result, the body taken whole, and the service's reply after the body's end.
#[test]
fn a_service_request_streams_its_body_to_the_service_and_its_reply_back() {
  let mut site = site(Vec::new(), Served);
  let asked = request(
    "POST",
    "/r.git/git-receive-pack",
    &[
      OWNER_BEARER,
      "Content-Type: application/x-git-receive-pack-request",
      "Content-Length: 3",
    ],
  );
  let (fields, head_only, mut exchange, proven) = served(ready(site.answer(&head(&asked), true)));
  let result: Fields = reply_fields(Reply::Result(Service::ReceivePack))
    .iter()
    .map(|(name, value)| (name.to_vec(), value.to_vec()))
    .collect();
  assert_eq!((fields, head_only, proven), (result, false, true));
  ready(exchange.body(b"abc")).unwrap();
  ready(exchange.body_end()).unwrap();
  assert_eq!(replies(&mut exchange), [b"took 3".to_vec()]);
}

/// A-625 part 2 (`http-backend`'s routes come first). Do: with no credentials, ask for a dumb-protocol path and send a PUT
/// to a service. Expect: `403` and `405` with `Allow: POST` as `http-backend` answers them, unproven, never a `401`: the
/// route's syntax names no repository.
#[test]
fn a_route_refused_by_its_syntax_is_http_backends_whoever_asks() {
  let mut site = site(Vec::new(), Unserved);
  let dumb = request("GET", "/r.git/objects/info/packs", &[]);
  assert_eq!(
    refusal(&ready(site.answer(&head(&dumb), true))),
    (Status::FORBIDDEN, with_cache(&[]), false)
  );
  let put = request("PUT", "/r.git/git-upload-pack", &[]);
  assert_eq!(
    refusal(&ready(site.answer(&head(&put), true))),
    (
      Status::METHOD_NOT_ALLOWED,
      with_cache(&[("Allow", "POST")]),
      false
    )
  );
}

/// Shape: the content store's budget and window an upload test's empty repository is kept in.
const STORE_BUDGET: u64 = 1 << 20;
/// Shape: as [`STORE_BUDGET`]: a window of many identities.
const STORE_WINDOW: usize = 1 << 16;
/// Shape: the branch the test repository's `HEAD` names, unborn.
const UNBORN_HEAD: &[u8] = b"refs/heads/main";

/// An empty SHA-1 repository's store, kept for the test's life.
struct EmptyRepository {
  /// Its index, versions and greens: none yet.
  repository: GitStore,
  /// Its content: none yet.
  store: ContentStore,
}

/// What [`NoPacks`] answers if asked: no exchange of these tests begins a pack, so the refusal is never read. Past
/// admission is the plainest one slates-git has (A-640 part 3 removed the stall these tests gave before).
fn no_pack() -> PackRefused {
  PackRefused::PastAdmission(slates_git::PastAdmission { asked: 1, left: 0 })
}

/// A pack source no exchange of these tests begins: an empty repository has nothing to pack.
#[derive(Clone, Copy, Debug)]
struct NoPacks;

impl PackSource for NoPacks {
  type Pack = ();

  fn begin(
    &mut self,
    _served: &UploadServed<'_>,
    _plan: &PackPlan<'_>,
    _admission: &mut slates_git::Admission,
  ) -> Result<(), PackRefused> {
    Err(no_pack())
  }

  fn write(
    &mut self,
    _served: &UploadServed<'_>,
    _pack: &mut (),
    _admission: &mut slates_git::Admission,
    _out: &mut Vec<u8>,
    _room: usize,
  ) -> Result<bool, PackRefused> {
    Err(no_pack())
  }
}

/// Lends the git repository `r.git`'s store, an empty one, and no other.
#[derive(Clone, Copy)]
struct Lends {
  /// The store, kept for the test's life.
  held: &'static EmptyRepository,
  /// The pack source.
  packs: NoPacks,
}

impl Lends {
  /// A lender of a fresh empty repository's store.
  fn empty() -> Lends {
    let held = Box::leak(Box::new(EmptyRepository {
      repository: GitStore::new(GitFormat::Sha1),
      store: ContentStore::new(STORE_BUDGET, STORE_WINDOW).unwrap(),
    }));
    Lends {
      held,
      packs: NoPacks,
    }
  }
}

impl StoreLender for Lends {
  type Packs = NoPacks;

  fn holds(&self, id: RepositoryId) -> bool {
    id == (RepositoryId { bytes: [0; 16] })
  }

  fn lend<R>(
    &mut self,
    (id, _reader): (RepositoryId, &Principal),
    step: impl FnOnce(&UploadServed<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    self.lent(id, UNBORN_HEAD, step)
  }

  fn lend_objects<R>(
    &mut self,
    id: RepositoryId,
    step: impl FnOnce(&UploadServed<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    self.lent(id, &[], step)
  }
}

impl Lends {
  /// Runs `step` over the empty store with `head` as `HEAD`'s branch, when `id` is the one held.
  fn lent<R>(
    &mut self,
    id: RepositoryId,
    head: &[u8],
    step: impl FnOnce(&UploadServed<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    let held = self.held;
    let served = UploadServed {
      repository: &held.repository,
      store: &held.store,
      refs: &[],
      head,
    };
    self.holds(id).then(|| step(&served, &mut self.packs))
  }
}

/// A-625 part 3 (A-640 part 2's upload-pack behind the edge). Do: as the owner, with upload-pack served from an empty
/// repository's store, ask for the advertisement at protocol v2, then POST an `ls-refs` request. Expect: the
/// advertisement is v2's capabilities, its `version 2` line first and `ls-refs=unborn` among them, with no `# service=`
/// line; the `ls-refs` answer is the flush alone, since the repository holds no ref.
#[test]
fn upload_pack_answers_the_advertisement_and_ls_refs_from_the_store() {
  let mut site = site(Vec::new(), UploadServices::new(Lends::empty(), ROOM));
  let advertise = request(
    "GET",
    UPLOAD_ADVERTISEMENT,
    &[OWNER_BEARER, "Git-Protocol: version=2"],
  );
  let (_, _, mut exchange, _) = served(ready(site.answer(&head(&advertise), true)));
  ready(exchange.body_end()).unwrap();
  let advertisement = replies(&mut exchange).concat();
  assert!(
    advertisement.starts_with(b"000eversion 2\n"),
    "{advertisement:?}"
  );
  assert!(
    advertisement
      .windows(b"ls-refs=unborn".len())
      .any(|window| window == b"ls-refs=unborn")
  );
  let ls_refs: &[u8] = b"0014command=ls-refs\n0000";
  let length = format!("Content-Length: {}", ls_refs.len());
  let post = request(
    "POST",
    "/r.git/git-upload-pack",
    &[
      OWNER_BEARER,
      "Git-Protocol: version=2",
      "Content-Type: application/x-git-upload-pack-request",
      &length,
    ],
  );
  let (_, _, mut exchange, _) = served(ready(site.answer(&head(&post), true)));
  ready(exchange.body(ls_refs)).unwrap();
  ready(exchange.body_end()).unwrap();
  assert_eq!(replies(&mut exchange).concat(), b"0000");
}

/// A-625 part 3 (A-640 part 2: a refused request is still answered). Do: POST upload-pack a body that is no pkt-line.
/// Expect: served `200`, the answer git's `ERR` line, as `git upload-pack --stateless-rpc` answers it.
#[test]
fn a_request_upload_pack_refuses_is_answered_in_gits_words() {
  let mut site = site(Vec::new(), UploadServices::new(Lends::empty(), ROOM));
  let garbage: &[u8] = b"zzzz";
  let post = request(
    "POST",
    "/r.git/git-upload-pack",
    &[
      OWNER_BEARER,
      "Git-Protocol: version=2",
      "Content-Type: application/x-git-upload-pack-request",
      "Content-Length: 4",
    ],
  );
  let (_, _, mut exchange, proven) = served(ready(site.answer(&head(&post), true)));
  ready(exchange.body(garbage)).unwrap();
  ready(exchange.body_end()).unwrap();
  let answer = replies(&mut exchange).concat();
  assert!(proven);
  assert!(
    answer
      .windows(b"ERR ".len())
      .any(|window| window == b"ERR "),
    "{:?}",
    String::from_utf8_lossy(&answer)
  );
}

/// A-625 part 3 (A-654 part 3: each service from its own half). Do: as the owner, with upload-pack's half alone, ask for
/// receive-pack's advertisement, and upload-pack's of a repository whose store is not held. Expect: `503` for each:
/// upload-pack's half serves upload-pack alone, and no store is lent for that repository.
#[test]
fn receive_pack_and_an_unheld_store_are_503() {
  let lender = Lends::empty();
  let mut site = site(Vec::new(), UploadServices::new(lender, ROOM));
  let receive = request("GET", RECEIVE_ADVERTISEMENT, &[OWNER_BEARER]);
  assert_eq!(
    refusal(&ready(site.answer(&head(&receive), true))).0,
    Status::SERVICE_UNAVAILABLE
  );
  assert!(!lender.holds(RepositoryId { bytes: [1; 16] }));
}

/// A-625 part 3 (main's ruling, 2026-11-19: the connection moves to its repository's owner shard). Do: ask for a
/// repository another shard's partition holds with the owner's bearer token, then with no credentials, then with a token
/// that proves no one. Expect: the first answered `Move` to that shard with its credentials proven, its head left
/// undecided for the shard it moves to; the other two refused `401` where they are, so a client that proved no one
/// never moves a connection.
#[test]
fn a_proven_request_for_another_shards_repository_moves_and_an_unproven_one_is_refused_here() {
  let mut site = site(Vec::new(), Unserved);
  let asked = request("GET", ELSEWHERE, &[OWNER_BEARER]);
  let decided = ready(site.answer(&head(&asked), true));
  assert!(
    matches!(decided.answer, Answer::Move { shard } if shard == ELSEWHERE_SHARD),
    "a proven request moves"
  );
  assert!(decided.proven);
  for fields in [&[][..], &["Authorization: Bearer nobody"][..]] {
    let asked = request("GET", ELSEWHERE, fields);
    let (status, _, proven) = refusal(&ready(site.answer(&head(&asked), true)));
    assert_eq!(
      (status, proven),
      (Status::UNAUTHORIZED, false),
      "{fields:?}"
    );
  }
}

/// A-625 part 3 (each refusal typed). Do: decide what each of these proves: no `Authorization`, an unknown scheme, Basic
/// in cleartext, a repeated line, an unknown token, an expired one, a revoked consumer's, one whose record did not
/// answer, the owner's. Expect each typed by why (`NoCredentials`, `NoCredentials`, `CleartextBasic`,
/// `MalformedCredentials`, then the record's `Unknown`, `Expired`, `Revoked`, `Unanswered`) and answered with its
/// status: `401` with the challenges, `400`, and `503` for the record that did not answer; the owner proven.
#[test]
fn a_credential_that_proves_no_one_is_typed_by_why() {
  let mut site = site(Vec::new(), Unserved);
  let cases: [(&[&str], bool, Refused); 8] = [
    (&[], true, Refused::NoCredentials),
    (
      &["Authorization: Negotiate abc"],
      true,
      Refused::NoCredentials,
    ),
    (&[OWNER_BASIC], false, Refused::CleartextBasic),
    (
      &[OWNER_BEARER, OWNER_BEARER],
      true,
      Refused::MalformedCredentials,
    ),
    (
      &["Authorization: Bearer nobody"],
      true,
      Refused::Unproven(Unproven::Unknown),
    ),
    (
      &["Authorization: Bearer expired-token"],
      true,
      Refused::Unproven(Unproven::Expired),
    ),
    (
      &["Authorization: Bearer revoked-token"],
      true,
      Refused::Unproven(Unproven::Revoked),
    ),
    (
      &["Authorization: Bearer silent-token"],
      true,
      Refused::Unproven(Unproven::Unanswered),
    ),
  ];
  for (fields, secure, why) in cases {
    let asked = request("GET", UPLOAD_ADVERTISEMENT, fields);
    assert_eq!(
      ready(site.proof(&head(&asked), secure)),
      Err(why),
      "{fields:?}"
    );
    let (status, _, proven) = refusal(&ready(site.answer(&head(&asked), secure)));
    assert_eq!((status, proven), (why.status(), false), "{fields:?}");
  }
  let asked = request("GET", UPLOAD_ADVERTISEMENT, &[OWNER_BEARER]);
  assert_eq!(
    ready(site.proof(&head(&asked), true)),
    Ok(Proven {
      principal: principal(OWNER),
      bearer_expires_ns: None,
    })
  );
}

/// Shape: Git LFS's media type, as its JSON endpoints take and answer it (A-610).
const LFS_JSON: &str = "application/vnd.git-lfs+json";
/// Shape: a request body's media type as LFS's JSON endpoints take it.
const LFS_BODY: &str = "Content-Type: application/vnd.git-lfs+json";
/// Shape: the locks API's endpoint of the git repository `r.git`.
const LFS_LOCKS: &str = "/r.git/info/lfs/locks";
/// Shape: an object's transfer endpoint of `r.git`, its oid the SHA-256 of nothing.
const LFS_OBJECT: &str =
  "/r.git/info/lfs/objects/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Git LFS's service in these tests: it serves every endpoint but an object's download, and answers each call `201`
/// with LFS's media type and a body naming what the call carried.
#[derive(Clone, Copy, Debug, Default)]
struct Carried;

impl LfsService for Carried {
  fn serves(&self, endpoint: &Endpoint) -> bool {
    !matches!(endpoint, Endpoint::Download(_))
  }

  fn answer(&mut self, call: &LfsCall<'_>) -> LfsAnswer {
    LfsAnswer {
      status: CREATED,
      kind: Kind::Json,
      challenge: None,
      body: carried(call).into_bytes(),
    }
  }
}

/// What `call` carried, as [`Carried`] answers it.
fn carried(call: &LfsCall<'_>) -> String {
  format!(
    "{:?} {} {} {:?} {:?} {:?} {:?} {:?} {}",
    call.routed,
    String::from_utf8_lossy(call.query),
    String::from_utf8_lossy(call.body),
    call.principal,
    call.credential_expiry,
    call.challenge,
    call.origin,
    call.request_id,
    call.room
  )
}

/// A site over [`Tokens`] and [`Held`] with no rules on `r.git`, its git services unserved, and Git LFS through `lfs`.
fn lfs_site<L: LfsService + Clone>(lfs: L) -> GitSite<Tokens, Held, Unserved, L> {
  site(Vec::new(), Unserved).with_lfs(lfs)
}

/// A served answer's fields, whether it is the head alone, when its status is decided, its exchange, and whether its
/// credentials proved a principal; panics on any other answer.
fn served_lfs<E>(decided: Decided<E>) -> (Fields, bool, StatusMode, E, bool) {
  match decided.answer {
    Answer::Serve {
      fields,
      head_only,
      status,
      exchange,
    } => (fields, head_only, status, exchange, decided.proven),
    Answer::Refuse { status, .. } => panic!("refused {status:?} where a service was expected"),
    Answer::Move { shard } => panic!("moved to shard {shard} where a service was expected"),
  }
}

/// A-629 part 2c (locking.md's methods; batch.md's media types; RFC 9110 §15.5.6). Do: with no credentials, `DELETE`
/// the locks API, `POST` its batch API a body of another media type, and ask it for an answer only HTML would satisfy.
/// Expect: `405` naming `GET, POST`, `415` and `406`, each with the cache fields and unproven: a route's refusal is
/// answered whoever asks, before any credential is read, as git's own routes' are.
#[test]
fn an_lfs_route_refused_by_its_syntax_is_answered_whoever_asks() {
  let mut site = lfs_site(Carried);
  let cases = [
    (
      request("DELETE", LFS_LOCKS, &[]),
      with_cache(&[("Allow", "GET, POST")]),
      Status::METHOD_NOT_ALLOWED,
    ),
    (
      request(
        "POST",
        "/r.git/info/lfs/objects/batch",
        &["Content-Type: text/plain"],
      ),
      with_cache(&[]),
      Status::UNSUPPORTED_MEDIA_TYPE,
    ),
    (
      request("POST", LFS_LOCKS, &[LFS_BODY, "Accept: text/html"]),
      with_cache(&[]),
      NOT_ACCEPTABLE,
    ),
  ];
  for (asked, fields, status) in cases {
    assert_eq!(
      refusal(&ready(site.answer(&head(&asked), true))),
      (status, fields, false),
      "{}",
      String::from_utf8_lossy(&asked)
    );
  }
}

/// A-629 part 2c (batch.md: "`LFS-Authenticate` ... mirrors the standard `WWW-Authenticate` header"). Do: ask the locks
/// API with no credentials over TLS. Expect: `401`, unproven, with Basic's and Bearer's challenges in
/// `WWW-Authenticate`, the cache fields, and each challenge again in `LFS-Authenticate`, so git-lfs asks for credentials.
#[test]
fn an_lfs_request_without_credentials_is_401_naming_its_challenges_for_git_lfs_too() {
  let mut site = lfs_site(Carried);
  let asked = request("POST", LFS_LOCKS, &[LFS_BODY]);
  let (www, lfs) = ("WWW-Authenticate", "LFS-Authenticate");
  let mut expected = with_cache(&[(www, BASIC_CHALLENGE), (www, BEARER_CHALLENGE)]);
  expected.extend([
    (lfs.as_bytes().to_vec(), BASIC_CHALLENGE.as_bytes().to_vec()),
    (
      lfs.as_bytes().to_vec(),
      BEARER_CHALLENGE.as_bytes().to_vec(),
    ),
  ]);
  assert_eq!(
    refusal(&ready(site.answer(&head(&asked), true))),
    (Status::UNAUTHORIZED, expected, false)
  );
}

/// A-629 part 2c (§4.17.2 "Access"; A-625 part 3's order). Do: as the owner, ask the locks API of a repository another
/// shard holds; of one that does not exist; of a Mercurial repository; as a principal no rule names, of `r.git`; then as
/// the owner, an object's download, which the service does not serve, and the locks API of a site with no LFS service.
/// Expect: the connection moved to the other shard; then `404`, `404`, `403`, `503` and `503`, each with the cache fields
/// and proven.
#[test]
fn an_lfs_request_meets_its_shard_then_404_403_and_503_for_an_endpoint_not_served() {
  let mut lfs = lfs_site(Carried);
  let elsewhere = request(
    "POST",
    "/elsewhere.git/info/lfs/locks",
    &[LFS_BODY, OWNER_BEARER],
  );
  let moved = ready(lfs.answer(&head(&elsewhere), true));
  assert!(
    matches!(moved.answer, Answer::Move { shard } if shard == ELSEWHERE_SHARD) && moved.proven
  );
  let cases = [
    (
      "POST",
      "/missing.git/info/lfs/locks",
      OWNER_BEARER,
      Status::NOT_FOUND,
    ),
    (
      "POST",
      "/hg/info/lfs/locks",
      OWNER_BEARER,
      Status::NOT_FOUND,
    ),
    ("POST", LFS_LOCKS, STRANGER_BEARER, Status::FORBIDDEN),
    ("GET", LFS_OBJECT, OWNER_BEARER, Status::SERVICE_UNAVAILABLE),
  ];
  for (method, target, credential, status) in cases {
    let asked = request(method, target, &[LFS_BODY, credential]);
    assert_eq!(
      refusal(&ready(lfs.answer(&head(&asked), true))),
      (status, with_cache(&[]), true),
      "{method} {target} with {credential}"
    );
  }
  let mut unserved = site(Vec::new(), Unserved);
  let asked = request("POST", LFS_LOCKS, &[LFS_BODY, OWNER_BEARER]);
  assert_eq!(
    refusal(&ready(unserved.answer(&head(&asked), true))),
    (Status::SERVICE_UNAVAILABLE, with_cache(&[]), true)
  );
}

/// A-629 part 2c (main's ruling, 2026-11-19: an LFS route decides its status after its body; the action expires with
/// the credential that asked). Do: as the owner, with a bearer token expiring half a second into a second, `POST` the
/// locks API over TLS with a query, its body in two runs. Expect: served, not the head alone, its status decided after
/// the body, proven; then the service's `201`, LFS's media type, and one run naming the call: the route, the query and
/// the whole body, the owner, the second the token expires in, no challenge, `https` and the request's authority, no
/// request id, and the edge's room.
#[test]
fn a_served_lfs_request_is_answered_after_its_body_with_its_services_status_and_call() {
  let mut site = lfs_site(Carried);
  let asked = request(
    "POST",
    "/r.git/info/lfs/locks?refspec=refs%2Fheads%2Fmain",
    &[LFS_BODY, "Authorization: Bearer expiring-token"],
  );
  let (fields, head_only, status, mut exchange, proven) =
    served_lfs(ready(site.answer(&head(&asked), true)));
  assert_eq!(
    (fields, head_only, status, proven),
    (Fields::new(), false, StatusMode::AfterBody, true)
  );
  for run in [&b"{\"path\":"[..], &b"\"a.bin\"}"[..]] {
    ready(exchange.body(run)).unwrap();
  }
  ready(exchange.body_end()).unwrap();
  let said = (
    exchange.reply_status(),
    ready(exchange.reply_fields()).unwrap(),
    replies(&mut exchange),
  );
  let call = format!(
    "Routed {{ repo: \"r.git\", endpoint: CreateLock }} refspec=refs%2Fheads%2Fmain {{\"path\":\"a.bin\"}} {:?} {:?} \
     None (\"https\", \"git.example\") None {ROOM}",
    Some(principal(OWNER)),
    Timestamp::from_unix(EXPIRES_SECOND),
  );
  assert_eq!(
    said,
    (
      CREATED,
      vec![(b"Content-Type".to_vec(), LFS_JSON.as_bytes().to_vec())],
      vec![call.into_bytes()]
    )
  );
}

/// A-629 part 3 (the daemon's LFS over the repository's store). Do: as the owner, ask the daemon's LFS service, through
/// the site, for the batch API, an object's upload, download and verification, and the locks API's create, list, verify
/// and unlock. Expect every one served, proven, each deciding its status after its body: the objects are the
/// repository's store's now, an upload durable before it is answered, so none is refused at its head.
#[test]
fn the_daemons_lfs_serves_every_endpoint_deciding_its_status_after_its_body() {
  let mut site = lfs_site(ShardLfs);
  let verify = format!("{LFS_OBJECT}/verify");
  let endpoints = [
    ("POST", "/r.git/info/lfs/objects/batch".to_owned()),
    ("PUT", LFS_OBJECT.to_owned()),
    ("GET", LFS_OBJECT.to_owned()),
    ("POST", verify),
    ("POST", LFS_LOCKS.to_owned()),
    ("GET", LFS_LOCKS.to_owned()),
    ("POST", format!("{LFS_LOCKS}/verify")),
    ("POST", format!("{LFS_LOCKS}/1.61/unlock")),
  ];
  for (method, target) in endpoints {
    let asked = request(method, &target, &[LFS_BODY, OWNER_BEARER]);
    let (_, _, status, _, proven) = served_lfs(ready(site.answer(&head(&asked), true)));
    assert_eq!(
      (status, proven),
      (StatusMode::AfterBody, true),
      "{method} {target}"
    );
  }
}
