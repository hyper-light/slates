use std::borrow::Cow;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, PolicyEffect, PolicyEntry, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_hg::compression::Engine;
use slates_hg::obsmarkers::Marker;
use slates_hg::phases::Phase;
use slates_hg::push::Outcome;
use slates_hg::push::reply::Reply;
use slates_hg::serve::getbundle::{Logged, Store};
use slates_hg::serve::http::{Compression, Compressor, Media};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{Node, http as hg_wire};
use slates_http::Admission;
use slates_http::head::{Head, HeadReader, Progress};
use slates_http::reply::Status;
use slates_http_edge::connection::Fields;
use slates_http_edge::credential::Credential;

use super::super::git::{Authenticator, Located, Proven, Repositories, Unproven};
use super::{Fastest, HgSite, HgStores};
use crate::repository::hg_push::Pushing;
use crate::vcs_http::{Answer, ClientGone, Decided, Exchange, Failed, Site};

/// Shape: room enough for every request these tests send.
const ROOM: u64 = 1 << 20;
/// Shape: the realm the tests' site names.
const REALM: &str = "slates";
/// Shape: the uid of the tests' repositories' owner.
const OWNER: u32 = 1;
/// Shape: the uid of a principal the owner's rules may name.
const STRANGER: u32 = 2;
/// Shape: the token that proves the owner, as a bearer token.
const OWNER_TOKEN: &[u8] = b"owner-token";
/// Shape: the token that proves the stranger.
const STRANGER_TOKEN: &[u8] = b"stranger-token";
/// Shape: the owner's credentials as a bearer token.
const OWNER_BEARER: &str = "Authorization: Bearer owner-token";
/// Shape: the owner's token as Basic's password (`hg:owner-token`).
const OWNER_BASIC: &str = "Authorization: Basic aGc6b3duZXItdG9rZW4=";
/// Shape: the stranger's.
const STRANGER_BEARER: &str = "Authorization: Bearer stranger-token";
/// Shape: the challenges a `401` carries, in this order, on every connection.
const BASIC_CHALLENGE: &str = "Basic realm=\"slates\", charset=\"UTF-8\"";
/// Shape: as [`BASIC_CHALLENGE`].
const BEARER_CHALLENGE: &str = "Bearer realm=\"slates\"";
/// Shape: the id of the Mercurial repository `hg`, whose store the tests' shard holds.
const HG: RepositoryId = RepositoryId { bytes: [1; 16] };
/// Shape: the id of the Mercurial repository `bare`, whose store it does not.
const BARE: RepositoryId = RepositoryId { bytes: [2; 16] };
/// Shape: the id of the git repository `r.git`.
const GIT: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: credentials whose record did not answer (its partition quiet).
const UNANSWERED_BEARER: &str = "Authorization: Bearer unanswered-token";
/// Shape: the token they carry.
const UNANSWERED_TOKEN: &[u8] = b"unanswered-token";
/// Shape: a Mercurial repository another runtime shard holds.
const AWAY: &str = "away";
/// Shape: the runtime shard that holds it.
const AWAY_SHARD: u16 = 3;
/// Shape: an empty repository's `heads`: the null node, then a line end (7.2's `heads` of no changeset).
const EMPTY_HEADS: &[u8] = b"0000000000000000000000000000000000000000\n";

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, from a bearer token; its record does not
/// answer for [`UNANSWERED_TOKEN`].
struct Tokens;

impl Authenticator for Tokens {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    if secret == UNANSWERED_TOKEN {
      return Err(Unproven::Unanswered);
    }
    [(OWNER_TOKEN, OWNER), (STRANGER_TOKEN, STRANGER)]
      .into_iter()
      .find(|(token, _)| *token == secret)
      .map(|(_, uid)| Proven {
        principal: principal(uid),
        bearer_expires_ns: None,
      })
      .ok_or(Unproven::Unknown)
  }
}

/// Holds the Mercurial repositories `hg` (the owner's, its policy `rules`) and `bare` (no store on this shard), and the
/// git repository `r.git`; [`AWAY`] lives on runtime shard [`AWAY_SHARD`].
struct Held {
  /// The rules of `hg`'s policy.
  rules: Vec<PolicyRule>,
}

impl Repositories for Held {
  fn locate(&self, name: &str) -> Located {
    if name == AWAY {
      Located::Elsewhere(AWAY_SHARD)
    } else {
      Located::Here
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (id, tool, rules) = match name {
      "hg" => (HG, VcsTool::Mercurial, self.rules.clone()),
      "bare" => (BARE, VcsTool::Mercurial, Vec::new()),
      "r.git" => (GIT, VcsTool::Git, Vec::new()),
      _ => return None,
    };
    Some(RepositoryRecord {
      id,
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

/// An empty repository's store: no changeset, no bookmark, no marker; publishing.
struct Empty;

impl Repository for Empty {
  fn count(&self) -> Rev {
    0
  }

  fn node(&self, _rev: Rev) -> Option<Node> {
    None
  }

  fn rev(&self, _node: &Node) -> Option<Rev> {
    None
  }

  fn parents(&self, _rev: Rev) -> [Option<Rev>; 2] {
    [None, None]
  }

  fn branch(&self, _rev: Rev) -> &[u8] {
    b"default"
  }

  fn closes(&self, _rev: Rev) -> bool {
    false
  }

  fn phase(&self, _rev: Rev) -> Phase {
    Phase::Public
  }

  fn publishing(&self) -> bool {
    true
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    std::iter::empty()
  }

  fn draft_roots(&self) -> impl Iterator<Item = Node> {
    std::iter::empty()
  }
}

impl Store for Empty {
  fn changeset_text(
    &self,
    _rev: Rev,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Cow<'_, [u8]>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn manifest(
    &self,
    _node: &Node,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Logged<'_>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn file(
    &self,
    _path: &[u8],
    _node: &Node,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Logged<'_>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    std::iter::empty()
  }
}

/// The stores of this shard as the tests hold them: `hg`'s, empty; none for any other repository.
#[derive(Clone, Copy)]
struct OneStore;

impl HgStores for OneStore {
  type Store<'s> = Empty;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HG
  }

  fn lend<R>(
    &mut self,
    id: RepositoryId,
    room: &mut slates_hg::Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut slates_hg::Admission) -> R,
  ) -> Option<R> {
    (id == HG).then(|| step(&Empty, room))
  }

  /// `hg`'s pushes are accepted with no reply part, as a push without `replycaps` is answered.
  async fn push(
    &mut self,
    id: RepositoryId,
    _pushing: Pushing<'_>,
    _room: &mut slates_hg::Admission,
  ) -> Option<Outcome> {
    (id == HG).then(|| Outcome::Accepted(Reply::accepted(&[], false)))
  }
}

/// The tests' site, `hg`'s policy `rules`.
fn site(rules: Vec<PolicyRule>) -> HgSite<Tokens, Held, OneStore> {
  HgSite::new(REALM.to_owned(), ROOM, Tokens, Held { rules }, OneStore)
}

/// The head `input` holds.
fn head(input: &[u8]) -> Head<'_> {
  match HeadReader::new().read(input, &mut Admission::new(ROOM)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// `method target` over HTTP/1.1 with `Host` and `fields`.
fn request(method: &str, target: &str, fields: &[&str]) -> Vec<u8> {
  let mut text = format!("{method} {target} HTTP/1.1\r\nHost: hg.example\r\n");
  for field in fields {
    text.push_str(field);
    text.push_str("\r\n");
  }
  text.push_str("\r\n");
  text.into_bytes()
}

/// A refusal's status and fields, or `None` for a served or moved request.
fn refusal<E>(decided: &Decided<E>) -> Option<(Status, Fields)> {
  match &decided.answer {
    Answer::Refuse { status, fields } => Some((*status, fields.clone())),
    Answer::Serve { .. } | Answer::Move { .. } => None,
  }
}

/// The status `site` refuses `method target` with, the request carrying `fields`, over cleartext; `None` if served.
fn refused_status(
  site: &mut HgSite<Tokens, Held, OneStore>,
  method: &str,
  target: &str,
  fields: &[&str],
) -> Option<Status> {
  let bytes = request(method, target, fields);
  refusal(&ready(site.decide(&head(&bytes), false))).map(|(status, _)| status)
}

/// A rule letting the stranger reach `hg` with `right` on every named branch.
fn stranger_may(right: PolicyRight) -> PolicyRule {
  PolicyRule {
    names: PolicyNames::Branches,
    pattern: "*".to_owned(),
    right,
    effect: PolicyEffect::Allow,
    who: PolicyWho::AnyOf {
      entries: vec![PolicyEntry::Is {
        principal: principal(STRANGER),
      }],
    },
  }
}

/// One case of [`each_step_of_hgwebs_order_refuses_with_its_status`]: the site (`open` or `strict`), the method, the
/// path, the credentials, and the refusal expected (`None`: served).
type StepCase = (
  &'static str,
  &'static str,
  &'static str,
  &'static [&'static str],
  Option<Status>,
);

/// AC-HG.9, A-650 part 1: do send `hg` requests that break each step of hgweb's order in turn: a wire command by `PUT`,
/// a path escaping a slash, no credentials, a repository of no name and one of git's, hgweb's own page, `unbundle` by
/// `GET`, `unbundle` by `POST` from a stranger who may only read, and a repository whose store this shard does not
/// hold; then, with no rule, the stranger's `heads`. Expect `405`, `400`, `401`, `404` twice, `503`, `405`, `403` and
/// `503`, and the owner's `heads` at `/hg/` served, by a bearer token and by Basic with the token as its password alike
/// (A-686: the loopback cleartext connection these tests decide on takes Basic); then the stranger `403` and the
/// owner's `heads` at `/hg` served. Every case is judged before any is compared, so a failing run names each that
/// differs.
#[test]
fn each_step_of_hgwebs_order_refuses_with_its_status() {
  let mut open = site(vec![stranger_may(PolicyRight::Read)]);
  let mut strict = site(Vec::new());
  let cases: [StepCase; 13] = [
    (
      "open",
      "PUT",
      "/hg?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::METHOD_NOT_ALLOWED),
    ),
    (
      "open",
      "GET",
      "/hg%2Fx?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::BAD_REQUEST),
    ),
    (
      "open",
      "GET",
      "/hg?cmd=heads",
      &[],
      Some(Status::UNAUTHORIZED),
    ),
    (
      "open",
      "GET",
      "/nothing?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::NOT_FOUND),
    ),
    (
      "open",
      "GET",
      "/r.git?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::NOT_FOUND),
    ),
    (
      "open",
      "GET",
      "/hg",
      &[OWNER_BEARER],
      Some(Status::SERVICE_UNAVAILABLE),
    ),
    (
      "open",
      "GET",
      "/hg?cmd=unbundle",
      &[OWNER_BEARER],
      Some(Status::METHOD_NOT_ALLOWED),
    ),
    (
      "open",
      "POST",
      "/hg?cmd=unbundle",
      &[STRANGER_BEARER],
      Some(Status::FORBIDDEN),
    ),
    (
      "open",
      "GET",
      "/bare?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::SERVICE_UNAVAILABLE),
    ),
    ("open", "GET", "/hg/?cmd=heads", &[OWNER_BEARER], None),
    ("open", "GET", "/hg/?cmd=heads", &[OWNER_BASIC], None),
    (
      "strict",
      "GET",
      "/hg?cmd=heads",
      &[STRANGER_BEARER],
      Some(Status::FORBIDDEN),
    ),
    ("strict", "GET", "/hg?cmd=heads", &[OWNER_BEARER], None),
  ];
  let (judged, expected): (Vec<_>, Vec<_>) = cases
    .into_iter()
    .map(|(which, method, target, fields, expected)| {
      let site = if which == "strict" {
        &mut strict
      } else {
        &mut open
      };
      let status = refused_status(site, method, target, fields);
      (
        (which, method, target, fields, status),
        (which, method, target, fields, expected),
      )
    })
    .unzip();
  assert_eq!(judged, expected);
}

/// AC-HG.9, A-650 part 1 and A-686: do send a wire command by `PUT`, the owner's `unbundle` by `GET`, and `heads` with
/// no credentials, on a loopback cleartext connection. Expect the first two's `Allow` naming the methods each may use,
/// and the `401` carrying Basic's challenge first and Bearer's second, as on every connection (stock Mercurial 7.2's
/// urllib reads the first challenge alone and crashed on a Bearer first); the first and the last decided before any
/// principal is proved, the second after the owner's proof.
#[test]
fn refusals_name_what_the_client_may_do_instead() {
  let mut site = site(Vec::new());
  let mut named = |method: &str, target: &str, fields: &[&str]| {
    let bytes = request(method, target, fields);
    let decided = ready(site.decide(&head(&bytes), false));
    (refusal(&decided).unwrap().1, decided.proven)
  };
  let allow = |methods: &str| vec![(b"Allow".to_vec(), methods.as_bytes().to_vec())];
  let challenge = [BASIC_CHALLENGE, BEARER_CHALLENGE]
    .map(|value| (b"WWW-Authenticate".to_vec(), value.as_bytes().to_vec()))
    .to_vec();
  assert_eq!(
    (
      named("PUT", "/hg?cmd=heads", &[OWNER_BEARER]),
      named("GET", "/hg?cmd=unbundle", &[OWNER_BEARER]),
      named("GET", "/hg?cmd=heads", &[]),
    ),
    (
      (allow("GET, POST"), false),
      (allow("POST"), true),
      (challenge, false)
    )
  );
}

/// Shape: a request's room in [`requests_past_their_room_and_a_damaged_policy_are_refused`].
const SMALL_ROOM: u64 = 256;
/// Shape: a run of bytes past [`SMALL_ROOM`].
const PAST_SMALL_ROOM: usize = 300;

/// AC-HG.9, A-650 part 1 and A-674: do send the owner's `heads`, to a site admitting each request 256 bytes, by a path
/// past them, by a query whose fields are past them, and with an argument header past them; then the owner's `heads` of
/// a Mercurial repository whose policy holds a rule only git reads (a damaged record). Expect `414` for the path and
/// the query alike, `431` for the header, and `500` for the damaged policy, which is never judged without its rule.
#[test]
fn requests_past_their_room_and_a_damaged_policy_are_refused() {
  let mut small = HgSite::new(
    REALM.to_owned(),
    SMALL_ROOM,
    Tokens,
    Held { rules: Vec::new() },
    OneStore,
  );
  let long = "x".repeat(PAST_SMALL_ROOM);
  let argument = format!("X-HgArg-1: pad={long}");
  let git_rule = PolicyRule {
    names: PolicyNames::Refs,
    pattern: "refs/heads/*".to_owned(),
    ..stranger_may(PolicyRight::Read)
  };
  let mut damaged = site(vec![git_rule]);
  let judged = [
    refused_status(
      &mut small,
      "GET",
      &format!("/{long}?cmd=heads"),
      &[OWNER_BEARER],
    ),
    refused_status(
      &mut small,
      "GET",
      &format!("/hg?cmd=heads&pad={long}"),
      &[OWNER_BEARER],
    ),
    refused_status(
      &mut small,
      "GET",
      "/hg?cmd=heads",
      &[OWNER_BEARER, argument.as_str()],
    ),
    refused_status(&mut damaged, "GET", "/hg?cmd=heads", &[OWNER_BEARER]),
  ];
  assert_eq!(
    judged,
    [
      Some(Status::URI_TOO_LONG),
      Some(Status::URI_TOO_LONG),
      Some(Status::FIELDS_TOO_LARGE),
      Some(Status::INTERNAL_SERVER_ERROR),
    ]
  );
}

/// A-650 part 1 on A-625 part 3's edge. Do: ask for [`AWAY`], a repository another runtime shard holds, as the owner,
/// with no credentials, and with a token the credential record did not answer for. Expect the owner's request moved to
/// that shard whole, proven (`Move`), and the others decided here before any move: `401`, and `503` for the record that
/// did not answer, as git's site answers them.
#[test]
fn a_request_for_a_repository_another_shard_holds_moves_there_once_proven() {
  let mut site = site(Vec::new());
  let mut decide = |fields: &[&str]| {
    let bytes = request("GET", "/away?cmd=capabilities", fields);
    let decided = ready(site.answer(&head(&bytes)));
    let refused = refusal(&decided).map(|(status, _)| status);
    let moved = match &decided.answer {
      Answer::Move { shard } => Some(*shard),
      Answer::Serve { .. } | Answer::Refuse { .. } => None,
    };
    (moved, refused, decided.proven)
  };
  assert_eq!(
    [
      decide(&[OWNER_BEARER]),
      decide(&[]),
      decide(&[UNANSWERED_BEARER]),
    ],
    [
      (Some(AWAY_SHARD), None, true),
      (None, Some(Status::UNAUTHORIZED), false),
      (None, Some(Status::SERVICE_UNAVAILABLE), false),
    ]
  );
}

/// Runs `future`, which the tests' sites and exchanges finish without waiting.
fn ready<F: Future>(future: F) -> F::Output {
  let mut context = Context::from_waker(Waker::noop());
  match std::pin::pin!(future).poll(&mut context) {
    Poll::Ready(value) => value,
    Poll::Pending => panic!("the tests' exchanges answer at once"),
  }
}

/// What an exchange gives the engine for a request with body `body`: its reply's fields and its reply's bytes.
fn exchanged<E: Exchange>(mut exchange: E, body: &[u8]) -> (Result<Fields, Failed>, Vec<u8>) {
  if !body.is_empty() {
    ready(exchange.body(body)).unwrap();
  }
  ready(exchange.body_end(ClientGone::never())).unwrap();
  let fields = ready(exchange.reply_fields());
  let mut reply = Vec::new();
  while ready(exchange.reply(&mut reply)).unwrap() {}
  (fields, reply)
}

/// The exchange `site` starts for `method target` with `fields`, by the owner.
fn served<S: HgStores + Clone>(
  site: &mut HgSite<Tokens, Held, S>,
  method: &str,
  target: &str,
  fields: &[&str],
) -> super::HgExchange<S> {
  let mut all = vec![OWNER_BEARER];
  all.extend_from_slice(fields);
  let bytes = request(method, target, &all);
  match ready(site.answer(&head(&bytes))).answer {
    Answer::Serve {
      fields,
      head_only,
      exchange,
      ..
    } => {
      assert!(fields.is_empty() && !head_only);
      exchange
    }
    Answer::Refuse { status, .. } => panic!("{method} {target} refused {status:?}"),
    Answer::Move { shard } => panic!("{method} {target} moved to shard {shard}"),
  }
}

/// The `Content-Type` field naming `media_type`.
fn media(media_type: &[u8]) -> Result<Fields, Failed> {
  Ok(vec![(b"Content-Type".to_vec(), media_type.to_vec())])
}

/// AC-HG.1, A-650 part 1: do serve the owner's `heads`, `capabilities`, and `known` without its nodes (by `POST`, the
/// arguments in the body behind `X-HgArgs-Post`) of the empty repository `hg`. Expect each answer's media type named as
/// its reply begins, after its body: `heads` the null node in `application/mercurial-0.1`, the capabilities naming
/// `httpheader=8000` (the edge's line bound), and `known` refused out of band in `application/hg-error`.
#[test]
fn a_served_command_names_its_media_type_as_its_reply_begins() {
  let mut site = site(Vec::new());
  let heads = exchanged(served(&mut site, "GET", "/hg?cmd=heads", &[]), b"");
  let caps = exchanged(served(&mut site, "GET", "/hg?cmd=capabilities", &[]), b"");
  let known = exchanged(
    served(
      &mut site,
      "POST",
      "/hg?cmd=known",
      &["X-HgArgs-Post: 4", "Content-Length: 4"],
    ),
    b"a=b&",
  );
  assert_eq!(
    (heads, &caps.0, &known.0),
    (
      (media(hg_wire::MEDIA_TYPE_V1), EMPTY_HEADS.to_vec()),
      &media(hg_wire::MEDIA_TYPE_V1),
      &media(hg_wire::MEDIA_TYPE_ERROR)
    )
  );
  let line = String::from_utf8_lossy(&caps.1);
  assert!(
    line.split(' ').any(|cap| cap == "httpheader=8000"),
    "{line}"
  );
}

/// AC-HG.9, A-650 part 1: do hand an exchange admitted a small room a body past it, and serve a repository whose store
/// is gone when its body ends. Expect the body refused `413`, and the answer `503`, neither held.
#[test]
fn an_exchange_holds_nothing_past_its_room_or_its_store() {
  let mut small = HgSite::new(
    REALM.to_owned(),
    256,
    Tokens,
    Held { rules: Vec::new() },
    OneStore,
  );
  let mut exchange = served(
    &mut small,
    "POST",
    "/hg?cmd=heads",
    &["Content-Length: 512"],
  );
  let past = ready(exchange.body(&[b'x'; 512]));
  let mut gone = HgSite::new(
    REALM.to_owned(),
    ROOM,
    Tokens,
    Held { rules: Vec::new() },
    Vanishing,
  );
  let mut orphan = served(&mut gone, "GET", "/hg?cmd=heads", &[]);
  assert_eq!(
    (past, ready(orphan.body_end(ClientGone::never()))),
    (
      Err(Failed(Status::CONTENT_TOO_LARGE)),
      Err(Failed(Status::SERVICE_UNAVAILABLE))
    )
  );
}

/// Stores that say they hold `hg`'s when the site decides and lend nothing when the exchange answers: a repository
/// released between its decision and its answer.
#[derive(Clone)]
struct Vanishing;

impl HgStores for Vanishing {
  type Store<'s> = Empty;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HG
  }

  fn lend<R>(
    &mut self,
    _id: RepositoryId,
    _room: &mut slates_hg::Admission,
    _step: impl for<'s> FnOnce(&Self::Store<'s>, &mut slates_hg::Admission) -> R,
  ) -> Option<R> {
    None
  }

  async fn push(
    &mut self,
    _id: RepositoryId,
    _pushing: Pushing<'_>,
    _room: &mut slates_hg::Admission,
  ) -> Option<Outcome> {
    None
  }
}

/// A-650 part 1: do ask the first value's compressor for a `0.2` client listing `none`, one listing zstd and zlib, and a
/// `0.1` client. Expect `none`, zstd at libzstd's fastest level, and zlib at level 0 (stored), the fastest form each
/// client reads.
#[test]
fn streams_take_the_fastest_form_the_client_reads() {
  let mut fastest = Fastest;
  let chosen = [
    fastest.choose(Media::V2, &[Engine::Zstd, Engine::Uncompressed], 1),
    fastest.choose(Media::V2, &[Engine::Zstd, Engine::Zlib], 1),
    fastest.choose(Media::V1, &[Engine::Zlib], 1),
  ];
  let fastest_zstd = *Engine::Zstd.levels().start();
  assert_eq!(
    chosen,
    [
      Compression {
        engine: Engine::Uncompressed,
        level: 0
      },
      Compression {
        engine: Engine::Zstd,
        level: fastest_zstd
      },
      Compression {
        engine: Engine::Zlib,
        level: 0
      },
    ]
  );
}
