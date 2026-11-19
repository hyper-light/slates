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
use slates_hg::serve::getbundle::{Logged, Store};
use slates_hg::serve::http::{Compression, Compressor, Media};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{Node, http as hg_wire};
use slates_http::Admission;
use slates_http::head::{Head, HeadReader, Progress};
use slates_http::reply::Status;
use slates_http_edge::connection::Fields;
use slates_http_edge::credential::Credential;

use super::super::git::{Authenticator, Repositories};
use super::{Fastest, HgSite, HgStores, NoStores};
use crate::vcs_http::{Answer, Decided, Exchange, Failed};

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
/// Shape: the stranger's.
const STRANGER_BEARER: &str = "Authorization: Bearer stranger-token";
/// Shape: the challenge a `401` carries over cleartext.
const BEARER_CHALLENGE: &str = "Bearer realm=\"slates\"";
/// Shape: the id of the Mercurial repository `hg`, whose store the tests' shard holds.
const HG: RepositoryId = RepositoryId { bytes: [1; 16] };
/// Shape: the id of the Mercurial repository `bare`, whose store it does not.
const BARE: RepositoryId = RepositoryId { bytes: [2; 16] };
/// Shape: the id of the git repository `r.git`.
const GIT: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: an empty repository's `heads`: the null node, then a line end (7.2's `heads` of no changeset).
const EMPTY_HEADS: &[u8] = b"0000000000000000000000000000000000000000\n";

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, from a bearer token.
struct Tokens;

impl Authenticator for Tokens {
  fn principal(&self, credential: &Credential) -> Option<Principal> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    [(OWNER_TOKEN, OWNER), (STRANGER_TOKEN, STRANGER)]
      .into_iter()
      .find(|(token, _)| *token == secret)
      .map(|(_, uid)| principal(uid))
  }
}

/// Holds the Mercurial repositories `hg` (the owner's, its policy `rules`) and `bare` (no store on this shard), and the
/// git repository `r.git`.
struct Held {
  /// The rules of `hg`'s policy.
  rules: Vec<PolicyRule>,
}

impl Repositories for Held {
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
}

impl Store for Empty {
  fn changeset_text(&self, _rev: Rev) -> Option<&[u8]> {
    None
  }

  fn manifest(&self, _node: &Node) -> Option<Logged<'_>> {
    None
  }

  fn file(&self, _path: &[u8], _node: &Node) -> Option<Logged<'_>> {
    None
  }

  fn file_node(&self, _manifest: &Node, _path: &[u8]) -> Option<Node> {
    None
  }

  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    std::iter::empty()
  }
}

/// The stores of this shard as the tests hold them: `hg`'s, empty; none for any other repository.
#[derive(Clone, Copy)]
struct OneStore;

impl HgStores for OneStore {
  type Store = Empty;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HG
  }

  fn lend<R>(&mut self, id: RepositoryId, step: impl FnOnce(&Empty) -> R) -> Option<R> {
    (id == HG).then(|| step(&Empty))
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

/// A refusal's status and fields, or `None` for a served request.
fn refusal<E>(decided: &Decided<E>) -> Option<(Status, Fields)> {
  match &decided.answer {
    Answer::Refuse { status, fields } => Some((*status, fields.clone())),
    Answer::Serve { .. } => None,
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
  refusal(&site.answer(&head(&bytes), false)).map(|(status, _)| status)
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

/// AC-HG.9, A-650 part 1: do send `hg` requests that break each step of hgweb's order in turn: a wire command by `PUT`,
/// a path escaping a slash, no credentials, a repository of no name and one of git's, hgweb's own page, `unbundle` by
/// `GET`, `unbundle` by `POST` from a stranger who may only read, and a repository whose store this shard does not
/// hold; then, with no rule, the stranger's `heads`. Expect `405`, `400`, `401`, `404` twice, `503`, `405`, `403` and
/// `503`, and the owner's `heads` at `/hg/` served; then the stranger `403` and the owner's `heads` at `/hg` served.
#[test]
fn each_step_of_hgwebs_order_refuses_with_its_status() {
  let mut site = site(vec![stranger_may(PolicyRight::Read)]);
  let mut strict = self::site(Vec::new());
  let cases: [(&str, &str, &[&str], Option<Status>); 10] = [
    (
      "PUT",
      "/hg?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::METHOD_NOT_ALLOWED),
    ),
    (
      "GET",
      "/hg%2Fx?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::BAD_REQUEST),
    ),
    ("GET", "/hg?cmd=heads", &[], Some(Status::UNAUTHORIZED)),
    (
      "GET",
      "/nothing?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::NOT_FOUND),
    ),
    (
      "GET",
      "/r.git?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::NOT_FOUND),
    ),
    (
      "GET",
      "/hg",
      &[OWNER_BEARER],
      Some(Status::SERVICE_UNAVAILABLE),
    ),
    (
      "GET",
      "/hg?cmd=unbundle",
      &[OWNER_BEARER],
      Some(Status::METHOD_NOT_ALLOWED),
    ),
    (
      "POST",
      "/hg?cmd=unbundle",
      &[STRANGER_BEARER],
      Some(Status::FORBIDDEN),
    ),
    (
      "GET",
      "/bare?cmd=heads",
      &[OWNER_BEARER],
      Some(Status::SERVICE_UNAVAILABLE),
    ),
    ("GET", "/hg/?cmd=heads", &[OWNER_BEARER], None),
  ];
  for (method, target, fields, expected) in cases {
    assert_eq!(
      refused_status(&mut site, method, target, fields),
      expected,
      "{method} {target}"
    );
  }
  assert_eq!(
    (
      refused_status(&mut strict, "GET", "/hg?cmd=heads", &[STRANGER_BEARER]),
      refused_status(&mut strict, "GET", "/hg?cmd=heads", &[OWNER_BEARER]),
    ),
    (Some(Status::FORBIDDEN), None)
  );
}

/// AC-HG.9, A-650 part 1: do send a wire command by `PUT`, the owner's `unbundle` by `GET`, and `heads` with no
/// credentials. Expect the first two's `Allow` naming the methods each may use, and the `401` carrying the bearer
/// challenge a cleartext connection accepts; the first and the last decided before any principal is proved, the second
/// after the owner's proof.
#[test]
fn refusals_name_what_the_client_may_do_instead() {
  let mut site = site(Vec::new());
  let mut named = |method: &str, target: &str, fields: &[&str]| {
    let bytes = request(method, target, fields);
    let decided = site.answer(&head(&bytes), false);
    (refusal(&decided).unwrap().1, decided.proven)
  };
  let allow = |methods: &str| vec![(b"Allow".to_vec(), methods.as_bytes().to_vec())];
  let challenge = vec![(
    b"WWW-Authenticate".to_vec(),
    BEARER_CHALLENGE.as_bytes().to_vec(),
  )];
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

/// Runs `future`, which the tests' exchanges finish without waiting.
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
  ready(exchange.body_end()).unwrap();
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
  match site.answer(&head(&bytes), false).answer {
    Answer::Serve {
      fields,
      head_only,
      exchange,
    } => {
      assert!(fields.is_empty() && !head_only);
      exchange
    }
    Answer::Refuse { status, .. } => panic!("{method} {target} refused {status:?}"),
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
    (past, ready(orphan.body_end())),
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
  type Store = Empty;

  fn holds(&self, id: RepositoryId) -> bool {
    id == HG
  }

  fn lend<R>(&mut self, _id: RepositoryId, _step: impl FnOnce(&Empty) -> R) -> Option<R> {
    None
  }
}

/// AC-HG.9, A-650 part 1: do ask the server's stores today for a repository, and ask the first value's compressor for a
/// `0.2` client listing `none`, one listing zstd and zlib, and a `0.1` client. Expect no store (every Mercurial request
/// `503` until A-662), and `none`, zstd at libzstd's fastest level, and zlib at level 0 (stored), the fastest form each
/// client reads.
#[test]
fn until_the_stores_land_nothing_is_served_and_streams_take_the_fastest_form() {
  let record = Held { rules: Vec::new() }.find("hg").unwrap();
  let mut stores = NoStores;
  let held = (stores.holds(record.id), stores.lend(record.id, |_| ()));
  let mut fastest = Fastest;
  let chosen = [
    fastest.choose(Media::V2, &[Engine::Zstd, Engine::Uncompressed], 1),
    fastest.choose(Media::V2, &[Engine::Zstd, Engine::Zlib], 1),
    fastest.choose(Media::V1, &[Engine::Zlib], 1),
  ];
  let fastest_zstd = *Engine::Zstd.levels().start();
  assert_eq!(
    (held, chosen),
    (
      (false, None),
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
    )
  );
}
