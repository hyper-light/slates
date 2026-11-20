//! A-629: serving Git LFS, held to the stock git-lfs 3.8.1 client's recorded session (A-610's
//! fixtures) replayed through the routes and the server, and to the server's rules: routes and
//! media types, who may do what, an action's expiry, uploads checked, locks released by their
//! owner or by force, and pages cut by the client's limit and by the room (against a model).

use proptest::prelude::*;
use serde_json::Value;
use slates_http::Admission as HttpAdmission;
use slates_http::head::{Head, HeadReader, Progress, Target, Version};
use slates_vcs::ContentStore;

use super::{
  Access, Allow, Answer, Context, Endpoint, Kind, LockRecord, Locks, LocksRefused, Need, PAGE_SLOT,
  Request, RouteRefused, Routed, Server, Stored, Taken, base_url, route,
};
use crate::Admission;
use crate::lfs::batch::read_request;
use crate::lfs::objects::{LfsNaming, LfsObjects};
use crate::lfs::tests::{Exchange, LFS_PREFIX, exchanges};
use crate::lfs::{Oid, Timestamp};

/// Shape: room enough for every request and answer these tests make.
const ROOM: u64 = 1 << 20;

/// Shape: the LFS server URL the tests' client named.
const BASE: &str = "https://lfs.example/repo.git/info/lfs";

/// Shape: the time every request is served at, 2026-11-19T00:00:00Z (Python's
/// `calendar.timegm`), the recorded responder's lock time.
const NOW: i64 = 1_795_046_400;

/// Shape: a content store's budget and window, room for every object these tests keep.
const STORE: (u64, usize) = (1 << 22, 4_096);

/// LFS's media type, as a JSON body's `Content-Type`.
const JSON: (&str, &str) = ("Content-Type", "application/vnd.git-lfs+json");

/// The fields a request carries, by name and value.
type Fields<'a> = &'a [(&'a str, &'a str)];

/// A request as a test sends it: its method, target, fields and body.
type Sent<'a> = (&'a str, &'a str, Fields<'a>, &'a [u8]);

/// One object of a batch answer as the client reads it: its oid, its size, the names of the
/// actions it is given, and its error.
type ObjectSeen = (String, u64, Vec<String>, Value);

/// The lock log as a model: the locks held in the order taken, each named `lock-N` by its place in
/// the log, as the recorded responder named them.
#[derive(Debug, Default)]
struct ModelLocks {
  /// The locks held, each with its place in the log.
  held: Vec<(u64, LockRecord)>,
  /// The last place taken.
  last: u64,
  /// The most locks held at once, when the test sets one.
  capacity: Option<usize>,
}

impl Locks for ModelLocks {
  fn take(&mut self, path: &str, owner: &str, at: Timestamp) -> Result<Taken, LocksRefused> {
    if let Some((_, holder)) = self.held.iter().find(|(_, holder)| holder.path == path) {
      return Ok(Taken::Held(holder.clone()));
    }
    if self
      .capacity
      .is_some_and(|capacity| self.held.len() >= capacity)
    {
      return Err(LocksRefused::Full);
    }
    self.last += 1;
    let record = LockRecord {
      id: format!("lock-{}", self.last),
      path: path.to_string(),
      owner: owner.to_string(),
      locked_at: at,
    };
    self.held.push((self.last, record.clone()));
    Ok(Taken::New(record))
  }

  fn find(&self, id: &str) -> Option<LockRecord> {
    let mut held = self.held.iter().map(|(_, record)| record);
    held.find(|record| record.id == id).cloned()
  }

  fn listed_from(&self, from: Option<&str>) -> Option<impl Iterator<Item = LockRecord> + '_> {
    let start = match from {
      Some(id) => id.strip_prefix("lock-")?.parse::<u64>().ok()?,
      None => 0,
    };
    let listed = self.held.iter().filter(move |(place, _)| *place >= start);
    Some(listed.map(|(_, record)| record.clone()))
  }

  fn release(&mut self, id: &str) -> Result<Option<LockRecord>, LocksRefused> {
    let Some(at) = self.held.iter().position(|(_, record)| record.id == id) else {
      return Ok(None);
    };
    Ok(Some(self.held.remove(at).1))
  }
}

/// Everyone may do everything.
fn everyone(_: Need, _: Option<&str>) -> bool {
  true
}

/// A reader: downloads and lists locks, nothing else.
fn reader(need: Need, _: Option<&str>) -> bool {
  matches!(need, Need::Download | Need::ListLocks)
}

/// A writer who may not release another user's lock.
fn writer(need: Need, _: Option<&str>) -> bool {
  need != Need::UnlockOthers
}

/// The request `method target` over HTTP/1.1 with `fields`.
fn request_bytes(method: &str, target: &str, fields: Fields<'_>) -> Vec<u8> {
  let lines: String = fields
    .iter()
    .map(|(name, value)| format!("{name}: {value}\r\n"))
    .collect();
  format!("{method} {target} HTTP/1.1\r\nHost: lfs.example\r\n{lines}\r\n").into_bytes()
}

/// The head `bytes` hold, whole.
fn head_of(bytes: &[u8]) -> Head<'_> {
  match HeadReader::new().read(bytes, &mut HttpAdmission::new(ROOM)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(bytes)),
  }
}

/// `method target` with `fields`, routed.
fn routed(method: &str, target: &str, fields: Fields<'_>) -> Result<Routed, RouteRefused> {
  let bytes = request_bytes(method, target, fields);
  route(&head_of(&bytes), &mut HttpAdmission::new(ROOM))
}

/// The context of a request by `owner` with credentials that never expire.
fn context(owner: &str) -> Context<'_> {
  Context {
    base: BASE,
    owner,
    authenticated: true,
    challenge: None,
    credential_expiry: None,
    now: Timestamp::from_unix(NOW).unwrap(),
    request_id: Some("slates-test"),
  }
}

/// A repository served: its store, its objects and its lock log.
struct World {
  /// The shared content store.
  store: ContentStore,
  /// The repository's LFS objects.
  objects: LfsObjects,
  /// The repository's lock log.
  locks: ModelLocks,
}

impl World {
  /// A repository holding nothing.
  fn new() -> World {
    World {
      store: ContentStore::new(STORE.0, STORE.1).unwrap(),
      objects: LfsObjects::new(),
      locks: ModelLocks::default(),
    }
  }

  /// Keeps `bytes` as an object, as an upload would.
  fn hold(&mut self, bytes: &[u8]) -> Oid {
    let oid = LfsNaming::oid(bytes);
    self.objects.admit(&mut self.store, &oid, bytes).unwrap();
    oid
  }

  /// Serves `sent` as `context`, allowed what `access` allows, within `room`.
  fn serve_within<A: Access>(
    &mut self,
    access: &A,
    sent: Sent<'_>,
    context: &Context<'_>,
    room: u64,
  ) -> Answer {
    let (method, target, fields, body) = sent;
    let bytes = request_bytes(method, target, fields);
    let head = head_of(&bytes);
    let routed = route(&head, &mut HttpAdmission::new(ROOM)).unwrap();
    let query: &[u8] = match head.target {
      Target::Path { query, .. } => query.unwrap_or_default(),
      Target::Asterisk => &[],
    };
    let mut objects = Stored {
      store: &mut self.store,
      objects: &mut self.objects,
    };
    let mut server = Server {
      access,
      objects: &mut objects,
      locks: &mut self.locks,
    };
    let request = Request {
      endpoint: &routed.endpoint,
      query,
      body,
    };
    server.answer(&request, context, &mut Admission::new(room))
  }

  /// [`World::serve_within`] with room for everything.
  fn serve<A: Access>(&mut self, access: &A, sent: Sent<'_>, context: &Context<'_>) -> Answer {
    self.serve_within(access, sent, context, ROOM)
  }
}

/// An answer's JSON body.
fn json(answer: &Answer) -> Value {
  serde_json::from_slice(&answer.body).unwrap()
}

/// A path under the LFS server URL.
fn lfs(path: &str) -> String {
  format!("{LFS_PREFIX}{path}")
}

/// The length of `text`.
fn len_of(text: &str) -> u64 {
  u64::try_from(text.len()).unwrap()
}

/// The unlock endpoint of `id`.
fn unlock(id: &str) -> Endpoint {
  Endpoint::Unlock { id: id.to_string() }
}

/// A-629: do route every endpoint by its path and method, and a repository's path that is nested
/// and escaped. Expect each endpoint, and the repository's path decoded.
#[test]
fn every_endpoint_is_routed_by_its_path_and_method() {
  let oid = LfsNaming::oid(b"x");
  let object = format!("/r.git/info/lfs/objects/{oid}");
  let cases: [(&str, String, Fields<'_>, Endpoint); 8] = [
    (
      "POST",
      "/r.git/info/lfs/objects/batch".into(),
      &[JSON],
      Endpoint::Batch,
    ),
    ("PUT", object.clone(), &[], Endpoint::Upload(oid)),
    ("GET", object.clone(), &[], Endpoint::Download(oid)),
    (
      "POST",
      format!("{object}/verify"),
      &[JSON],
      Endpoint::Verify(oid),
    ),
    (
      "POST",
      "/r.git/info/lfs/locks".into(),
      &[JSON],
      Endpoint::CreateLock,
    ),
    (
      "GET",
      "/r.git/info/lfs/locks".into(),
      &[],
      Endpoint::ListLocks,
    ),
    (
      "POST",
      "/r.git/info/lfs/locks/verify".into(),
      &[JSON],
      Endpoint::VerifyLocks,
    ),
    (
      "POST",
      "/r.git/info/lfs/locks/lock-1/unlock".into(),
      &[JSON],
      unlock("lock-1"),
    ),
  ];
  for (method, target, fields, endpoint) in cases {
    let expected = Routed {
      repo: "r.git".to_string(),
      endpoint,
    };
    assert_eq!(
      routed(method, &target, fields),
      Ok(expected),
      "{method} {target}"
    );
  }
  let nested = routed("GET", "/org/a%20b.git/info/lfs/locks", &[]).unwrap();
  assert_eq!(nested.repo, "org/a b.git");
}

/// A-629: do route requests each wrong one way: an endpoint asked by another method; no
/// repository, a `.` segment, an oid that is none, an unknown endpoint; an escape naming a slash,
/// a repository path that is not UTF-8; a JSON body of another type or none; an `Accept` that
/// covers no LFS body. Expect each refused by its kind, with its status; and a malformed escape
/// refused by slates-http's reader before any route reads it (RFC 3986 §2.1).
#[test]
fn every_misrouted_request_is_refused_by_kind() {
  const PLAIN_TEXT: (&str, &str) = ("Content-Type", "text/plain");
  const HTML: (&str, &str) = ("Accept", "text/html");
  let object = lfs(&format!("/objects/{}", LfsNaming::oid(b"x")));
  let allow = RouteRefused::MethodNotAllowed;
  let (not_found, bad_path) = (RouteRefused::NotFound, RouteRefused::BadPath);
  let (unsupported, not_acceptable) = (
    RouteRefused::UnsupportedMediaType,
    RouteRefused::NotAcceptable,
  );
  let cases: [(&str, String, Fields<'_>, RouteRefused, u16); 12] = [
    ("GET", lfs("/objects/batch"), &[], allow(Allow::Post), 405),
    ("DELETE", lfs("/locks"), &[], allow(Allow::GetPost), 405),
    ("POST", object, &[JSON], allow(Allow::GetPut), 405),
    ("GET", "/info/lfs/locks".into(), &[], not_found, 404),
    ("GET", "/a/./info/lfs/locks".into(), &[], not_found, 404),
    ("GET", lfs("/objects/zz"), &[], not_found, 404),
    ("GET", lfs("/elsewhere"), &[], not_found, 404),
    ("GET", "/a%2Fb/info/lfs/locks".into(), &[], bad_path, 400),
    ("GET", "/a%ff/info/lfs/locks".into(), &[], bad_path, 400),
    ("POST", lfs("/objects/batch"), &[], unsupported, 415),
    ("POST", lfs("/locks"), &[PLAIN_TEXT], unsupported, 415),
    ("POST", lfs("/locks"), &[JSON, HTML], not_acceptable, 406),
  ];
  for (method, target, fields, refused, status) in cases {
    let which = format!("{method} {target}");
    assert_eq!(routed(method, &target, fields), Err(refused), "{which}");
    assert_eq!(refused.status(Version::Http11).code, status, "{which}");
  }
  let malformed = request_bytes("GET", "/a%zz/info/lfs/locks", &[]);
  let read = HeadReader::new().read(&malformed, &mut HttpAdmission::new(ROOM));
  assert!(read.is_err(), "{read:?}");
}

/// A JSON request to `locks`, its body's type in another case with a charset, whose `Accept` is
/// `accept`, routed.
fn with_accept(accept: &str) -> Result<Routed, RouteRefused> {
  let typed = (
    "Content-Type",
    "Application/VND.git-lfs+JSON; charset=utf-8",
  );
  routed("POST", &lfs("/locks"), &[typed, ("Accept", accept)])
}

/// A-629: do send `Accept`s that cover LFS's type exactly, by `application/*` or by `*/*`, beside
/// other ranges; one whose zero weight on `*/*` a more specific range overrides; one of empty list
/// elements alone; and two `Accept` lines, the second naming LFS's type. Expect each routed (RFC
/// 9110 §12.5.1, §5.6.1, §5.3).
#[test]
fn an_accept_that_covers_lfs_s_type_is_routed() {
  let accepted = [
    "application/vnd.git-lfs+json",
    "*/*",
    "Application/*",
    "text/html, */*;q=0.1",
    "*/*;q=0, application/vnd.git-lfs+json",
    ", ,application/*;q=0.5",
    " , ",
  ];
  for accept in accepted {
    assert!(with_accept(accept).is_ok(), "{accept:?}");
  }
  let lines = [
    JSON,
    ("Accept", "text/html"),
    ("Accept", "application/vnd.git-lfs+json"),
  ];
  assert!(routed("POST", &lfs("/locks"), &lines).is_ok());
}

/// A-629: do send `Accept`s that name only another type, that weigh LFS's type zero in several
/// spellings of zero, or whose most specific range that covers it weighs zero beside a broader one
/// that does not. Expect each refused `406` (RFC 9110 §12.4.2, §12.5.1).
#[test]
fn an_accept_that_refuses_lfs_s_type_is_406() {
  let refused = [
    "text/html",
    "application/vnd.git-lfs+json;q=0",
    "*/*; Q=0.000",
    "application/vnd.git-lfs+json;q=0., */*",
    "application/*;q=0, */*;q=1",
  ];
  for accept in refused {
    assert_eq!(
      with_accept(accept),
      Err(RouteRefused::NotAcceptable),
      "{accept:?}"
    );
  }
}

/// A-629: do write the refusal of a method an endpoint does not take, to HTTP/1.1, and find its
/// status for HTTP/1.0. Expect `405` naming the endpoint's methods in `Allow`, and `400` for
/// HTTP/1.0, which has no `405`.
#[test]
fn a_405_names_the_methods_the_endpoint_takes() {
  let mut out = Vec::new();
  let refused = RouteRefused::MethodNotAllowed(Allow::GetPost);
  refused.reply(Version::Http11, &mut out).unwrap();
  let text = String::from_utf8(out).unwrap();
  let head = "HTTP/1.1 405 Method Not Allowed\r\nAllow: GET, POST\r\n";
  assert!(text.starts_with(head), "{text}");
  assert_eq!(refused.status(Version::Http10).code, 400);
}

/// A-629: do make the LFS server URL of a repository whose path holds a space and a non-ASCII
/// letter. Expect them escaped, the slashes kept.
#[test]
fn the_base_url_escapes_what_a_path_may_not_carry() {
  let url = base_url("https", "lfs.example:8443", "org/a b/\u{e9}.git");
  assert_eq!(
    url,
    "https://lfs.example:8443/org/a%20b/%C3%A9.git/info/lfs"
  );
}

/// The target this server serves a recorded request at: the API's paths as recorded, and the
/// responder's transfer paths moved under the LFS server URL, as this server hands them out.
fn target_of(exchange: &Exchange) -> String {
  let path = if exchange.path.starts_with(LFS_PREFIX) {
    exchange.path.clone()
  } else if let Some(oid) = exchange.path.strip_prefix("/objects/") {
    lfs(&format!("/objects/{oid}"))
  } else {
    let sent: Value = serde_json::from_str(&exchange.request).unwrap();
    lfs(&format!(
      "/objects/{}/verify",
      sent["oid"].as_str().unwrap()
    ))
  };
  if exchange.query.is_empty() {
    path
  } else {
    format!("{path}?{}", exchange.query)
  }
}

/// The fields of a recorded request that this server routes by: its `Content-Type` and `Accept`.
fn recorded_fields(exchange: &Exchange) -> Vec<(&str, &str)> {
  let named = [
    ("Content-Type", &exchange.content_type),
    ("Accept", &exchange.accept),
  ];
  named
    .into_iter()
    .filter_map(|(name, value)| Some((name, value.as_deref()?)))
    .collect()
}

/// A batch answer's objects as the client reads them.
fn batch_projection(answer: &Value) -> Vec<ObjectSeen> {
  answer["objects"]
    .as_array()
    .unwrap()
    .iter()
    .map(|object| {
      let actions = object["actions"]
        .as_object()
        .map(|actions| actions.keys().cloned().collect())
        .unwrap_or_default();
      let oid = object["oid"].as_str().unwrap().to_string();
      let size = object["size"].as_u64().unwrap();
      (oid, size, actions, object["error"].clone())
    })
    .collect()
}

/// Whether each list `recorded` holds under `keys` begins the one `ours` holds there.
fn pages_agree(ours: &Value, recorded: &Value, keys: &[&str]) -> bool {
  keys.iter().all(
    |key| match (ours[*key].as_array(), recorded[*key].as_array()) {
      (Some(ours), Some(recorded)) => ours.starts_with(recorded),
      _ => false,
    },
  )
}

/// Whether this server's JSON answer `ours` to a request for `path` agrees with the `recorded`
/// one: a batch answer object by object as the client reads them; a page as a list that begins as
/// the recorded one did (the responder paged one lock at a time, this server by the client's limit
/// and the room); anything else whole.
fn json_agrees(path: &str, ours: &Value, recorded: &Value) -> bool {
  if path.ends_with("/objects/batch") {
    batch_projection(ours) == batch_projection(recorded)
  } else if recorded.get("ours").is_some() {
    pages_agree(ours, recorded, &["ours", "theirs"])
  } else if recorded.get("locks").is_some() {
    pages_agree(ours, recorded, &["locks"])
  } else {
    ours == recorded
  }
}

/// Checks the answer to a recorded request against the answer the stock client read: the same
/// status, and a JSON body that agrees ([`json_agrees`]) or any other body byte for byte.
fn check_replayed(exchange: &Exchange, answer: &Answer) {
  let which = format!("{} {}", exchange.method, target_of(exchange));
  assert_eq!(u64::from(answer.status.code), exchange.status, "{which}");
  if answer.kind == Kind::Json {
    let ours = json(answer);
    let recorded: Value = serde_json::from_str(&exchange.response).unwrap();
    let agrees = json_agrees(&exchange.path, &ours, &recorded);
    assert!(agrees, "{which}: {ours} where the client read {recorded}");
  } else {
    assert_eq!(answer.body, exchange.response.as_bytes(), "{which}");
  }
}

/// A-629, golden: do replay the stock git-lfs 3.8.1 client's whole recorded session (a push with
/// lock verification, uploads and their verifications, locks taken, a conflict, listings, unlocks
/// with and without force, a clone's downloads and a missing object) through the routes and the
/// server, the transfers at this server's own hrefs. Expect every status the client read, every
/// object's answer, every lock and the conflict as recorded, every page beginning as recorded, and
/// every downloaded byte.
#[test]
fn the_stock_client_s_recorded_session_is_served_as_it_was_answered() {
  let mut world = World::new();
  let recorded = exchanges();
  for exchange in &recorded {
    let (target, fields) = (target_of(exchange), recorded_fields(exchange));
    let body = exchange.request.as_bytes();
    let sent = (
      exchange.method.as_str(),
      target.as_str(),
      fields.as_slice(),
      body,
    );
    check_replayed(exchange, &world.serve(&everyone, sent, &context("test")));
  }
  assert_eq!(recorded.len(), 24);
}

/// A batch request's body for `operation` of the object `bytes` would be.
fn batch_body(operation: &str, bytes: &[u8]) -> Vec<u8> {
  let object = serde_json::json!({"oid": LfsNaming::oid(bytes).to_string(), "size": bytes.len()});
  let body = serde_json::json!({
    "operation": operation,
    "objects": [object],
    "ref": {"name": "refs/heads/main"},
  });
  body.to_string().into_bytes()
}

/// A-629: do ask, as a reader, for an upload by the batch API, to put an object, to verify one, to
/// take a lock and to verify locks; then for a download. Expect each of the first refused `403`,
/// and the download answered.
#[test]
fn a_reader_may_download_and_nothing_more() {
  let mut world = World::new();
  let held = world.hold(b"held\n");
  let upload = batch_body("upload", b"new\n");
  let verify = lfs(&format!("/objects/{held}/verify"));
  let put = lfs(&format!("/objects/{}", LfsNaming::oid(b"new\n")));
  let (batch, locks, locks_verify) = (lfs("/objects/batch"), lfs("/locks"), lfs("/locks/verify"));
  let refused: [Sent<'_>; 5] = [
    ("POST", &batch, &[JSON], &upload),
    ("PUT", &put, &[], b"new\n"),
    ("POST", &verify, &[JSON], br#"{"oid":"x","size":1}"#),
    ("POST", &locks, &[JSON], br#"{"path":"a.bin"}"#),
    ("POST", &locks_verify, &[JSON], b"{}"),
  ];
  for sent in refused {
    let answer = world.serve(&reader, sent, &context("reader"));
    assert_eq!(answer.status.code, 403, "{} {}", sent.0, sent.1);
  }
  let download = batch_body("download", b"held\n");
  let answer = world.serve(
    &reader,
    ("POST", &batch, &[JSON], &download),
    &context("reader"),
  );
  assert_eq!(answer.status.code, 200);
}

/// A-629: do ask without credentials for what the policy refuses, without and then with a
/// challenge from the edge. Expect `401` both times, naming the challenge in `LFS-Authenticate`
/// when there is one, so git-lfs asks for credentials.
#[test]
fn a_refusal_without_credentials_asks_for_them() {
  let mut world = World::new();
  let (upload, batch) = (batch_body("upload", b"new\n"), lfs("/objects/batch"));
  let mut anonymous = context("");
  anonymous.authenticated = false;
  let bare = world.serve(&reader, ("POST", &batch, &[JSON], &upload), &anonymous);
  assert_eq!((bare.status.code, bare.challenge), (401, None));
  anonymous.challenge = Some("Basic realm=\"slates\"");
  let asked = world.serve(&reader, ("POST", &batch, &[JSON], &upload), &anonymous);
  let mut reply = Vec::new();
  asked.reply(false, &mut reply).unwrap();
  let text = String::from_utf8(reply).unwrap();
  assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\n"), "{text}");
  assert!(
    text.contains("\r\nLFS-Authenticate: Basic realm=\"slates\"\r\n"),
    "{text}"
  );
}

/// Every action's `expires_at` in a batch answer, and whether any carries `expires_in`.
fn expiries(answer: &Value) -> (Vec<Value>, bool) {
  let actions: Vec<&Value> = answer["objects"]
    .as_array()
    .unwrap()
    .iter()
    .filter_map(|object| object["actions"].as_object())
    .flat_map(|actions| actions.values())
    .collect();
  let at = actions
    .iter()
    .map(|action| action["expires_at"].clone())
    .collect();
  (
    at,
    actions
      .iter()
      .any(|action| action.get("expires_in").is_some()),
  )
}

/// The RFC 3339 text of `at`, as an answer writes it.
fn rfc3339(at: Timestamp) -> String {
  let mut text = Vec::new();
  at.put_rfc3339(&mut text);
  String::from_utf8(text).unwrap()
}

/// A-629: do ask for an upload and a download with a credential that expires in an hour, then with
/// one that never does. Expect every action to expire with the first credential, at its expiry,
/// and none to carry an expiry with the second.
#[test]
fn an_action_expires_exactly_when_its_credential_does() {
  let mut world = World::new();
  world.hold(b"held\n");
  let expiry = Timestamp::from_unix(NOW + 3_600).unwrap();
  let mut bearer = context("test");
  bearer.credential_expiry = Some(expiry);
  let (batch, at) = (lfs("/objects/batch"), Value::from(rfc3339(expiry)));
  let cases: [(&str, &[u8], usize); 2] = [("upload", b"new\n", 2), ("download", b"held\n", 1)];
  for (operation, bytes, actions) in cases {
    let body = batch_body(operation, bytes);
    let timed = world.serve(&everyone, ("POST", &batch, &[JSON], &body), &bearer);
    assert_eq!(
      expiries(&json(&timed)),
      (vec![at.clone(); actions], false),
      "{operation}"
    );
    let basic = world.serve(
      &everyone,
      ("POST", &batch, &[JSON], &body),
      &context("test"),
    );
    assert_eq!(
      expiries(&json(&basic)),
      (vec![Value::Null; actions], false),
      "{operation}"
    );
  }
}

/// A-629, hostile input: do put bytes that are not the object the path names, then download that
/// object and ask the batch API for it. Expect the upload refused `422`, and the object neither
/// downloaded (`404`) nor offered (a per-object `404`).
#[test]
fn an_upload_is_checked_against_its_oid() {
  let mut world = World::new();
  let object = lfs(&format!("/objects/{}", LfsNaming::oid(b"the real bytes\n")));
  let put = world.serve(
    &everyone,
    ("PUT", &object, &[], b"other bytes\n"),
    &context("test"),
  );
  let got = world.serve(&everyone, ("GET", &object, &[], b""), &context("test"));
  let (body, batch) = (
    batch_body("download", b"the real bytes\n"),
    lfs("/objects/batch"),
  );
  let asked = world.serve(
    &everyone,
    ("POST", &batch, &[JSON], &body),
    &context("test"),
  );
  let offered = json(&asked)["objects"][0]["error"]["code"].clone();
  let codes = (put.status.code, got.status.code, offered);
  assert_eq!(codes, (422, 404, Value::from(404)));
}

/// A-629: do take a lock as alice, then release it as bob without force, with force where bob may
/// not release another user's lock, and with force where he may. Expect `403`, `403`, then the
/// lock released and gone.
#[test]
fn a_lock_is_released_by_its_owner_or_by_force() {
  let mut world = World::new();
  let (locks, lock) = (lfs("/locks"), br#"{"path":"a.bin"}"#);
  let taken = world.serve(
    &everyone,
    ("POST", &locks, &[JSON], lock),
    &context("alice"),
  );
  let (unlock, bob) = (lfs("/locks/lock-1/unlock"), context("bob"));
  let (plain, forced): (&[u8], &[u8]) = (b"{}", br#"{"force":true}"#);
  let codes = [
    world
      .serve(&writer, ("POST", &unlock, &[JSON], plain), &bob)
      .status
      .code,
    world
      .serve(&writer, ("POST", &unlock, &[JSON], forced), &bob)
      .status
      .code,
    world
      .serve(&everyone, ("POST", &unlock, &[JSON], forced), &bob)
      .status
      .code,
  ];
  assert_eq!((taken.status.code, codes), (201, [403, 403, 200]));
  assert_eq!(world.locks.find("lock-1"), None);
}

/// Takes locks on `file-0`, `file-1`, ..., `count` of them, as `test`: each answer's length.
fn take_locks(world: &mut World, count: u64) -> Vec<u64> {
  let locks = lfs("/locks");
  (0..count)
    .map(|path| {
      let body = format!("{{\"path\":\"file-{path}\"}}");
      let sent: Sent<'_> = ("POST", &locks, &[JSON], body.as_bytes());
      let answer = world.serve(&everyone, sent, &context("test"));
      u64::try_from(answer.body.len()).unwrap()
    })
    .collect()
}

/// The ids a page of locks lists.
fn listed_ids(page: &Value) -> Vec<String> {
  page["locks"]
    .as_array()
    .unwrap()
    .iter()
    .map(|lock| lock["id"].as_str().unwrap().to_string())
    .collect()
}

/// `lock-1` to `lock-{last}`.
fn ids_up_to(last: u64) -> Vec<String> {
  (1..=last).map(|place| format!("lock-{place}")).collect()
}

/// What following a listing's cursors found: the ids of each page answered, and whether a request
/// was refused `413`, which ends it.
#[derive(Debug, Default, PartialEq, Eq)]
struct Listing {
  /// Each page's ids.
  pages: Vec<Vec<String>>,
  /// Whether the last request was refused `413`.
  refused: bool,
}

/// The listing's target: `limit` when it is not zero, and `cursor` when there is one.
fn page_target(limit: u64, cursor: Option<&str>) -> String {
  let limit = (limit > 0).then(|| format!("limit={limit}"));
  let cursor = cursor.map(|cursor| format!("cursor={cursor}"));
  let pairs: Vec<String> = limit.into_iter().chain(cursor).collect();
  lfs(&format!("/locks?{}", pairs.join("&")))
}

/// Follows the listing's cursors from the first page, `limit` to a page when it is not zero,
/// within `room`, until a page names no cursor or a request is refused `413`, at most `bound`
/// requests so a server naming cursors for ever cannot hold the test: what it found, and the
/// longest page answered.
fn follow(world: &mut World, limit: u64, room: u64, bound: usize) -> (Listing, usize) {
  let (mut listing, mut longest, mut cursor) = (Listing::default(), 0, None::<String>);
  for _ in 0..bound {
    let target = page_target(limit, cursor.as_deref());
    let answer = world.serve_within(
      &everyone,
      ("GET", &target, &[], b""),
      &context("test"),
      room,
    );
    if answer.status.code == 413 {
      listing.refused = true;
      return (listing, longest);
    }
    assert_eq!(answer.status.code, 200, "{target}");
    longest = longest.max(answer.body.len());
    let page = json(&answer);
    listing.pages.push(listed_ids(&page));
    cursor = page["next_cursor"].as_str().map(String::from);
    if cursor.is_none() {
      return (listing, longest);
    }
  }
  panic!("the listing still named a cursor after {bound} requests: {listing:?}");
}

/// A-629: do take ten locks, then list them three to a page, following each page's cursor. Expect
/// pages of three, three, three and one, the last naming no cursor, and every lock once, in order.
#[test]
fn a_listing_is_paged_by_the_client_s_limit() {
  let mut world = World::new();
  take_locks(&mut world, 10);
  let (listing, _) = follow(&mut world, 3, ROOM, 10);
  let sizes: Vec<usize> = listing.pages.iter().map(Vec::len).collect();
  assert_eq!(
    (sizes, listing.pages.concat()),
    (vec![3, 3, 3, 1], ids_up_to(10))
  );
}

/// A-629: do take ten locks, then list them with room for three and a half lock answers, following
/// each page's cursor. Expect more than one page, none empty and none refused, each answer within
/// the room, and every lock once, in order.
#[test]
fn a_listing_is_cut_where_the_room_ends() {
  let mut world = World::new();
  let one = take_locks(&mut world, 10)[0];
  let room = one * 7 / 2;
  let (listing, longest) = follow(&mut world, 0, room, 10);
  let cut = listing.pages.len() > 1 && listing.pages.iter().all(|page| !page.is_empty());
  let within = u64::try_from(longest).unwrap() <= room;
  let found = (cut, within, listing.refused, listing.pages.concat());
  assert_eq!(found, (true, true, false, ids_up_to(10)));
}

/// A listing refused `413` at its first page.
fn refused_at_once() -> Listing {
  Listing {
    pages: Vec::new(),
    refused: true,
  }
}

/// A-629: do list two locks with room for the listing's frame but not for one lock, and ask the
/// batch API for an object held with room for reading the request and not a byte more, then with
/// room enough. Expect `413`, never a page that lists nothing and names a cursor; then `413` and
/// `200`.
#[test]
fn an_answer_past_the_room_is_413() {
  let mut world = World::new();
  let one = take_locks(&mut world, 2)[0];
  let (listing, _) = follow(&mut world, 0, one / 2, 2);
  world.hold(b"x\n");
  let object = serde_json::json!({"oid": LfsNaming::oid(b"x\n").to_string(), "size": 2});
  let body = serde_json::json!({"operation": "download", "objects": [object]}).to_string();
  let mut reading = Admission::new(ROOM);
  read_request(body.as_bytes(), &mut reading).unwrap();
  let batch = lfs("/objects/batch");
  let sent: Sent<'_> = ("POST", &batch, &[JSON], body.as_bytes());
  let codes = [ROOM - reading.left(), ROOM].map(|room| {
    world
      .serve_within(&everyone, sent, &context("test"), room)
      .status
      .code
  });
  assert_eq!((listing, codes), (refused_at_once(), [413, 200]));
}

/// A-629: do take a lock where the lock log is full, list from a cursor this server never handed
/// out, and list with a limit that is no number. Expect `507`, `422` and `422`.
#[test]
fn a_full_log_and_a_foreign_cursor_are_refused_by_kind() {
  let mut world = World::new();
  world.locks.capacity = Some(0);
  let (locks, foreign, limit) = (
    lfs("/locks"),
    lfs("/locks?cursor=else"),
    lfs("/locks?limit=x"),
  );
  let full = world.serve(
    &everyone,
    ("POST", &locks, &[JSON], br#"{"path":"a"}"#),
    &context("t"),
  );
  let foreign = world.serve(&everyone, ("GET", &foreign, &[], b""), &context("t"));
  let limit = world.serve(&everyone, ("GET", &limit, &[], b""), &context("t"));
  let codes = [full.status.code, foreign.status.code, limit.status.code];
  assert_eq!(codes, [507, 422, 422]);
}

/// The bytes a lock takes on a page by the paging rule: its JSON in a list with the separator
/// before it, the page's slot for it, and its record's text.
fn place_bytes(lock: &LockRecord) -> u64 {
  let written = serde_json::json!({
    "id": lock.id,
    "path": lock.path,
    "locked_at": rfc3339(lock.locked_at),
    "owner": {"name": lock.owner},
  });
  let text = lock.id.len() + lock.path.len() + lock.owner.len();
  let slot = u64::try_from(PAGE_SLOT + text).unwrap();
  len_of(",") + len_of(&written.to_string()) + slot
}

/// The bytes a page's cursor naming `id` takes in its answer.
fn cursor_bytes(id: &str) -> u64 {
  len_of(",\"next_cursor\":") + len_of(&Value::from(id).to_string())
}

/// One page by the paging rule, from `held[start..]` within `left`: the ids it lists and where the
/// next page starts, or `None` when the page is refused.
fn rule_page(
  held: &[LockRecord],
  start: usize,
  limit: u64,
  mut left: u64,
) -> Option<(Vec<String>, usize)> {
  let mut page = Vec::new();
  let mut at = start;
  while let Some(lock) = held.get(at) {
    let after = held.get(at + 1).map_or(0, |next| cursor_bytes(&next.id));
    let full = limit > 0 && u64::try_from(page.len()).ok()? == limit;
    if full || place_bytes(lock) + after > left {
      break;
    }
    left -= place_bytes(lock);
    page.push(lock.id.clone());
    at += 1;
  }
  let next = held.get(at);
  let refused = next.is_some_and(|next| page.is_empty() || cursor_bytes(&next.id) > left);
  (!refused).then_some((page, at))
}

/// The paging rule (the serving module's bounds), stated over the held locks' JSON apart from the
/// server: a page's room, less its cursor's text (charged as the query is read) and its frame
/// (`{"locks":[]}`), holds each lock while the limit allows it and the room holds its place and the
/// cursor naming the next lock; a page that holds no lock while locks remain is refused `413`.
fn paging_rule(held: &[LockRecord], limit: u64, room: u64) -> Listing {
  let mut listing = Listing::default();
  let mut start = 0;
  loop {
    let reading = held
      .get(start)
      .filter(|_| start > 0)
      .map_or(0, |next| len_of(&next.id));
    let framed = room
      .checked_sub(reading)
      .and_then(|left| left.checked_sub(len_of("{\"locks\":[]}")));
    let Some((page, next)) = framed.and_then(|left| rule_page(held, start, limit, left)) else {
      listing.refused = true;
      return listing;
    };
    listing.pages.push(page);
    if next == held.len() {
      return listing;
    }
    start = next;
  }
}

/// Releases, through the lock log, each of the first `count` locks whose place `kept` marks
/// `false`.
fn release_unkept(world: &mut World, kept: &[bool], count: u64) {
  let count = usize::try_from(count).unwrap();
  for (place, keep) in kept.iter().enumerate().take(count) {
    if !keep {
      world.locks.release(&format!("lock-{}", place + 1)).unwrap();
    }
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-629, model: do take up to nineteen locks, release some, and follow the listing's cursors
  /// with a limit or none, in a room from too small for anything to room for every lock. Expect
  /// exactly the pages, or the `413` that ends them, that the paging rule states over the locks'
  /// JSON, every held lock listed once in the log's order, and each answer within its room.
  #[test]
  fn a_listing_follows_the_paging_rule(
    count in 0u64..20,
    kept in proptest::collection::vec(any::<bool>(), 20),
    limit in 0u64..5,
    room in 0u64..4_000,
  ) {
    let mut world = World::new();
    take_locks(&mut world, count);
    release_unkept(&mut world, &kept, count);
    let held: Vec<LockRecord> =
      world.locks.listed_from(None).map(|held| held.collect()).unwrap_or_default();
    let (listing, longest) = follow(&mut world, limit, room, 24);
    prop_assert_eq!(listing, paging_rule(&held, limit, room));
    prop_assert!(u64::try_from(longest).is_ok_and(|longest| longest <= room));
  }
}

/// A principal who may upload to `refs/heads/topic` alone.
fn topic_only(need: Need, ref_name: Option<&str>) -> bool {
  need == Need::Upload && ref_name == Some("refs/heads/topic")
}

/// A-629 part 2b: do ask the batch API for an upload naming `refs/heads/topic`, as a principal who
/// may upload there alone, then put the object at the href the batch handed out and at that href
/// without its query. Expect the href to name the ref (`refspec`), the put there answered, and the
/// put without it refused `403`: a transfer is judged on the ref its batch was.
#[test]
fn a_transfer_is_judged_on_the_ref_its_batch_named() {
  let mut world = World::new();
  let object = serde_json::json!({"oid": LfsNaming::oid(b"new\n").to_string(), "size": 4});
  let ask = serde_json::json!({
    "operation": "upload",
    "objects": [object],
    "ref": {"name": "refs/heads/topic"},
  });
  let (batch, body) = (lfs("/objects/batch"), ask.to_string());
  let sent: Sent<'_> = ("POST", &batch, &[JSON], body.as_bytes());
  let answer = json(&world.serve(&topic_only, sent, &context("test")));
  let href = answer["objects"][0]["actions"]["upload"]["href"]
    .as_str()
    .unwrap()
    .to_owned();
  let target = href.strip_prefix("https://lfs.example").unwrap().to_owned();
  let bare = target.split('?').next().unwrap().to_owned();
  let codes = [&target, &bare].map(|target| {
    let sent: Sent<'_> = ("PUT", target, &[], b"new\n");
    world.serve(&topic_only, sent, &context("test")).status.code
  });
  let named = target.ends_with("?refspec=refs/heads/topic");
  assert_eq!((named, codes), (true, [200, 403]));
}
