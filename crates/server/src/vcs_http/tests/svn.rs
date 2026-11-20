use std::collections::BTreeMap;
use std::future::Future;
use std::process::Command;
use std::task::{Context, Poll, Waker};

use aws_lc_rs::digest::{MD5_FOR_LEGACY_USE_ONLY, SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use slates_db::HostId;
use slates_db::catalog::{
  ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_http::Admission;
use slates_http::head::{Head, HeadReader, Progress};
use slates_http_edge::credential::Credential;
use slates_http_edge::tls::ServingIdentity;
use slates_rt::control::Control;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_svn::authz::{self, Authz, PrincipalRules};
use slates_svn::index::{
  Action, ChangedPath, ContentIdentity, Index, NodeKind, PropertyList, Revision, RevisionRecord,
  Text,
};
use slates_svn::lock::{AprTime, LockTable};
use slates_svn::mergeinfo::Contents;
use slates_svn::reads::Repository;

use super::super::git::{Authenticator, Located, Proven, Repositories, Unproven};
use slates_svn::dav::site::{self, Answer as SiteAnswer, ReportProgress};

use super::{Lent, SvnExchange, SvnSite, SvnStores, served_of, xml_bounds};
use crate::vcs_http::{Answer, Bounds, ClientGone, Decided, EdgeState, Exchange, Fields, serve};
use crate::vcs_store::svn_view::ServedRecords;

/// Shape: room enough for every request these tests send.
const ROOM: u64 = 1 << 20;
/// Shape: the realm the tests' site names.
const REALM: &str = "slates";
/// Shape: the uid of the tests' repositories' owner, Subversion's `alice`.
const OWNER: u32 = 1;
/// Shape: the uid of a principal the repository's policy gives nothing, Subversion's `bob`.
const STRANGER: u32 = 2;
/// Shape: the token that proves the owner, a Basic credential's password.
const OWNER_TOKEN: &str = "alice-token";
/// Shape: the token that proves the stranger.
const STRANGER_TOKEN: &str = "bob-token";
/// Shape: the id of the Subversion repository `repo`, whose store the tests' shard holds.
const REPO: RepositoryId = RepositoryId { bytes: [1; 16] };
/// Shape: the id of the Subversion repository `bare`, whose store it does not.
const BARE: RepositoryId = RepositoryId { bytes: [2; 16] };
/// Shape: the id of the git repository `r.git`.
const GIT: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: a repository another runtime shard holds, and that shard.
const AWAY: (&str, u16) = ("away", 3);
/// Shape: the repository's UUID.
const UUID: &str = "6b4a5e2c-1d3f-4a7b-9c8e-0f1a2b3c4d5e";
/// Shape: the repository's authorization file: alice and bob read everything but `/trunk/secret`, hidden from alice.
const AUTHZ: &[u8] = b"[/]\nalice = r\nbob = r\n[/trunk/secret]\nalice =\n";
/// Shape: the text of `/trunk/a.txt`.
const A_TEXT: &[u8] = b"hello, slates\n";
/// Shape: the bytes the tests' green hands over at once: fewer than a text, so a GET's reply takes several runs.
const PIECE: usize = 4;
/// Shape: revision 1's date, as stored.
const DATE: &str = "2026-11-19T00:00:01.000000Z";
/// Shape: the instant locks are judged at.
const NOW: AprTime = AprTime(1_800_000_000_000_000);
/// Shape: the PROPFIND `svn ls` sends at depth 1 where no `list` capability is listed (A-609's fixture).
const PROPFIND_KIND: &[u8] =
  b"<?xml version=\"1.0\" encoding=\"utf-8\"?><propfind xmlns=\"DAV:\"><prop>\
<resourcetype xmlns=\"DAV:\"/></prop></propfind>";

/// The principal with uid `uid` on host 0.
fn principal(uid: u32) -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid,
  }
}

/// Proves [`OWNER_TOKEN`] the owner and [`STRANGER_TOKEN`] the stranger, as a Basic password or a bearer token.
struct Tokens;

impl Authenticator for Tokens {
  async fn prove(&self, credential: &Credential) -> Result<Proven, Unproven> {
    let secret = match credential {
      Credential::Basic { password, .. } => password.as_slice(),
      Credential::Bearer(token) => token.as_slice(),
    };
    [(OWNER_TOKEN, OWNER), (STRANGER_TOKEN, STRANGER)]
      .into_iter()
      .find(|(token, _)| token.as_bytes() == secret)
      .map(|(_, uid)| Proven {
        principal: principal(uid),
        bearer_expires_ns: None,
      })
      .ok_or(Unproven::Unknown)
  }
}

/// Holds the Subversion repositories `repo` and `bare` (no store here) and the git repository `r.git`, each the owner's
/// with no rule; [`AWAY`] lives on another runtime shard.
struct Held;

impl Repositories for Held {
  fn locate(&self, name: &str) -> Located {
    if name == AWAY.0 {
      Located::Elsewhere(AWAY.1)
    } else {
      Located::Here
    }
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    let (id, tool, format) = match name {
      "repo" => (REPO, VcsTool::Subversion, ObjectFormat::Revisions),
      "bare" => (BARE, VcsTool::Subversion, ObjectFormat::Revisions),
      "r.git" => (GIT, VcsTool::Git, ObjectFormat::Sha1),
      _ => return None,
    };
    Some(RepositoryRecord {
      id,
      name: name.to_owned(),
      tool,
      format,
      default_branch: None,
      owner: principal(OWNER),
      policy: RepositoryPolicy {
        version: 1,
        rules: Vec::new(),
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    })
  }
}

/// A digest of `bytes` as a fixed array.
fn digest_of<const N: usize>(
  algorithm: &'static aws_lc_rs::digest::Algorithm,
  bytes: &[u8],
) -> [u8; N] {
  digest(algorithm, bytes).as_ref().try_into().unwrap()
}

/// The text `bytes` as the index names it.
fn text_of(bytes: &[u8]) -> Text {
  Text {
    identity: digest_of(&SHA256, bytes),
    md5: digest_of(&MD5_FOR_LEGACY_USE_ONLY, bytes),
    sha1: digest_of(&SHA1_FOR_LEGACY_USE_ONLY, bytes),
    size: u64::try_from(bytes.len()).unwrap(),
  }
}

/// An addition's changed-path row.
fn added(
  path: &str,
  kind: NodeKind,
  text: Option<Text>,
  properties: Option<ContentIdentity>,
) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action: Action::Add,
    kind,
    copy_from: None,
    text,
    properties: properties.map(|identity| PropertyList::Held {
      identity,
      has_mergeinfo: false,
    }),
    mergeinfo_modified: false,
  }
}

/// The tests' repository: revision 1 by alice adds `/trunk/a.txt` (with `svn:eol-style`) and `/trunk/secret/s.txt`.
struct Repo {
  /// The index.
  index: Index,
  /// Each revision's tree: each path and whether it is a directory.
  trees: Vec<BTreeMap<String, bool>>,
  /// The property lists, by identity.
  lists: BTreeMap<ContentIdentity, Vec<(Vec<u8>, Vec<u8>)>>,
  /// The texts, by identity.
  texts: BTreeMap<ContentIdentity, Vec<u8>>,
  /// Each revision's properties.
  revisions: Vec<Vec<(Vec<u8>, Vec<u8>)>>,
  /// The locks: none.
  locks: LockTable,
  /// The authorization file.
  authz: Authz,
}

impl Repo {
  /// The repository, built.
  fn build() -> Self {
    let secret: &[u8] = b"hidden\n";
    let list = vec![(b"svn:eol-style".to_vec(), b"native".to_vec())];
    let list_identity: ContentIdentity = digest_of(&SHA256, b"the list of /trunk/a.txt");
    let mut index = Index::new();
    let record = RevisionRecord {
      transaction: "0-1".to_owned(),
      changes: vec![
        added("/trunk", NodeKind::Directory, None, None),
        added(
          "/trunk/a.txt",
          NodeKind::File,
          Some(text_of(A_TEXT)),
          Some(list_identity),
        ),
        added("/trunk/secret", NodeKind::Directory, None, None),
        added(
          "/trunk/secret/s.txt",
          NodeKind::File,
          Some(text_of(secret)),
          None,
        ),
      ],
    };
    index
      .append(record, usize::try_from(ROOM).unwrap())
      .unwrap();
    let tree = |paths: &[(&str, bool)]| {
      paths
        .iter()
        .map(|(path, directory)| ((*path).to_owned(), *directory))
        .collect::<BTreeMap<_, _>>()
    };
    let date = |value: &str| (b"svn:date".to_vec(), value.as_bytes().to_vec());
    Self {
      index,
      trees: vec![
        tree(&[("/", true)]),
        tree(&[
          ("/", true),
          ("/trunk", true),
          ("/trunk/a.txt", false),
          ("/trunk/secret", true),
          ("/trunk/secret/s.txt", false),
        ]),
      ],
      lists: BTreeMap::from([(list_identity, list)]),
      texts: BTreeMap::from([
        (text_of(A_TEXT).identity, A_TEXT.to_vec()),
        (text_of(secret).identity, secret.to_vec()),
      ]),
      revisions: vec![
        vec![date("2026-11-18T00:00:00.000000Z")],
        vec![
          (b"svn:author".to_vec(), b"alice".to_vec()),
          date(DATE),
          (b"svn:log".to_vec(), b"the first".to_vec()),
        ],
      ],
      locks: LockTable::new(),
      authz: Authz::parse(AUTHZ, None, authz::Bounds::of(AUTHZ, None)).unwrap(),
    }
  }
}

impl Contents for Repo {
  fn entries(&self, directory: &str, revision: Revision) -> Option<Vec<String>> {
    let tree = self.trees.get(usize::try_from(revision).ok()?)?;
    if !*tree.get(directory)? {
      return None;
    }
    let prefix = if directory == "/" {
      String::from("/")
    } else {
      format!("{directory}/")
    };
    Some(
      tree
        .keys()
        .filter_map(|path| path.strip_prefix(&prefix))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .map(str::to_owned)
        .collect(),
    )
  }

  fn mergeinfo_value(&self, _properties: &ContentIdentity) -> Option<&[u8]> {
    None
  }
}

impl Repository for Repo {
  fn revision_properties(&self, revision: Revision) -> Option<Vec<(&[u8], &[u8])>> {
    let properties = self.revisions.get(usize::try_from(revision).ok()?)?;
    Some(
      properties
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn properties(&self, identity: &ContentIdentity) -> Option<Vec<(&[u8], &[u8])>> {
    let list = self.lists.get(identity)?;
    Some(
      list
        .iter()
        .map(|(name, value)| (name.as_slice(), value.as_slice()))
        .collect(),
    )
  }

  fn text(&self, identity: &ContentIdentity, offset: u64) -> Option<&[u8]> {
    let text = self.texts.get(identity)?;
    let start = usize::try_from(offset).ok()?;
    text.get(start..start.saturating_add(PIECE).min(text.len()))
  }
}

/// The Subversion user each principal is in the authorization file: the owner alice, the stranger bob.
fn user_of(principal: &Principal) -> Option<&'static [u8]> {
  match principal {
    Principal::Uid { uid: OWNER, .. } => Some(b"alice"),
    Principal::Uid { uid: STRANGER, .. } => Some(b"bob"),
    _ => None,
  }
}

/// Lends `repo`'s store, a principal's rules compiled from its authorization file.
#[derive(Clone, Copy)]
struct Stores {
  /// The repository, kept for the test's life.
  repo: &'static Repo,
}

impl SvnStores for Stores {
  type Repository = Repo;

  fn holds(&self, id: RepositoryId) -> bool {
    id == REPO
  }

  fn lend<T>(
    &mut self,
    id: RepositoryId,
    principal: &Principal,
    step: impl FnOnce(&Lent<'_, Repo>) -> T,
  ) -> Option<T> {
    if id != REPO {
      return None;
    }
    let rules: PrincipalRules = self
      .repo
      .authz
      .rules(b"repo", authz::Principal::named(user_of(principal)));
    Some(step(&Lent {
      index: &self.repo.index,
      repository: self.repo,
      uuid: UUID,
      locks: &self.repo.locks,
      rules: Some(&rules),
      now: NOW,
    }))
  }
}

/// The tests' site, its repository built and kept for the test's life.
fn site() -> SvnSite<Tokens, Held, Stores> {
  let repo: &'static Repo = Box::leak(Box::new(Repo::build()));
  SvnSite::new(REALM.to_owned(), ROOM, Tokens, Held, Stores { repo })
}

/// Reads `input` as a head.
fn head(input: &[u8]) -> Head<'_> {
  match HeadReader::new().read(input, &mut Admission::new(ROOM)) {
    Ok(Progress::Complete { value, .. }) => value,
    other => panic!("{:?} read {other:?}", String::from_utf8_lossy(input)),
  }
}

/// `method target` over HTTP/1.1 with `Host` and `fields`.
fn request(method: &str, target: &str, fields: &[&str]) -> Vec<u8> {
  let mut text = format!("{method} {target} HTTP/1.1\r\nHost: svn.example\r\n");
  for field in fields {
    text.push_str(field);
    text.push_str("\r\n");
  }
  text.push_str("\r\n");
  text.into_bytes()
}

/// The `Authorization` field of a Basic credential for `user` and `token`, as stock svn sends it.
fn basic(user: &str, token: &str) -> String {
  let mut encoded = String::new();
  let bytes = format!("{user}:{token}").into_bytes();
  let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  for chunk in bytes.chunks(3) {
    let word = chunk.iter().enumerate().fold(0u32, |word, (at, byte)| {
      word | (u32::from(*byte) << (16 - 8 * at))
    });
    for at in 0..4 {
      if at <= chunk.len() {
        let sextet = (word >> (18 - 6 * at)) & 0x3f;
        encoded.push(char::from(alphabet[usize::try_from(sextet).unwrap()]));
      } else {
        encoded.push('=');
      }
    }
  }
  format!("Authorization: Basic {encoded}")
}

/// Shape: the polls the tests give a future that yields between its slices, far more than any of theirs takes.
const POLLS: usize = 10_000;

/// Runs `future`, which the tests' sites and exchanges finish without waiting on anything but their own yields.
fn ready<F: Future>(future: F) -> F::Output {
  let mut context = Context::from_waker(Waker::noop());
  let mut future = std::pin::pin!(future);
  for _ in 0..POLLS {
    if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
      return value;
    }
  }
  panic!("the tests' exchanges answer within {POLLS} polls");
}

/// How the site decided `method target` with `fields`, on any connection (the site reads none since A-686): moved to a
/// shard, refused with a status, or served; and whether the credentials proved a principal.
fn decided(
  site: &mut SvnSite<Tokens, Held, Stores>,
  method: &str,
  target: &str,
  fields: &[&str],
) -> (String, bool) {
  let bytes = request(method, target, fields);
  let decided: Decided<SvnExchange<Stores>> = ready(site.answer(&head(&bytes)));
  let outcome = match decided.answer {
    Answer::Move { shard } => format!("moved {shard}"),
    Answer::Refuse { status, .. } => format!("refused {}", status.code),
    Answer::Serve { head_only, .. } => format!("served, head alone {head_only}"),
  };
  (outcome, decided.proven)
}

/// A-681 (the module doc's order): do ask with no credentials, with a token proving no one, for the server's root, for
/// a repository another shard holds, for a missing repository, for git's, for one whose store is not here, as a
/// principal the policy gives nothing, and as the owner by OPTIONS and HEAD; expect `401` (unproven) before anything
/// about a repository, `404` for no name, the move once proven, `404`, `404`, `503`, `403`, and the owner's served.
#[test]
fn each_request_is_decided_in_the_module_docs_order() {
  let mut site = site();
  let owner = basic("alice", OWNER_TOKEN);
  let stranger = basic("bob", STRANGER_TOKEN);
  let nobody = basic("eve", "no-token");
  let asked = [
    decided(&mut site, "OPTIONS", "/repo/trunk", &[]),
    decided(&mut site, "OPTIONS", "/repo/trunk", &[nobody.as_str()]),
    decided(&mut site, "OPTIONS", "/", &[owner.as_str()]),
    decided(&mut site, "OPTIONS", "/away/trunk", &[owner.as_str()]),
    decided(&mut site, "OPTIONS", "/missing/trunk", &[owner.as_str()]),
    decided(&mut site, "OPTIONS", "/r.git/trunk", &[owner.as_str()]),
    decided(&mut site, "OPTIONS", "/bare/trunk", &[owner.as_str()]),
    decided(&mut site, "OPTIONS", "/repo/trunk", &[stranger.as_str()]),
    decided(&mut site, "OPTIONS", "/repo/trunk", &[owner.as_str()]),
    decided(&mut site, "HEAD", "/repo/trunk/a.txt", &[owner.as_str()]),
  ];
  let expected = [
    ("refused 401", false),
    ("refused 401", false),
    ("refused 404", false),
    ("moved 3", true),
    ("refused 404", true),
    ("refused 404", true),
    ("refused 503", true),
    ("refused 403", true),
    ("served, head alone false", true),
    ("served, head alone true", true),
  ];
  let expected: Vec<(String, bool)> = expected
    .iter()
    .map(|(outcome, proven)| ((*outcome).to_owned(), *proven))
    .collect();
  assert_eq!(asked.to_vec(), expected);
}

/// Shape: how long the lender's test waits for its task on the daemon's shard.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Shape: the repository the lender's test makes on the daemon's shard.
const COMMITTED: &str = "committed";

/// Shape: a `log-report` of every revision with every property and the changed paths.
const LOG_EVERY: &[u8] = b"<S:log-report xmlns:S=\"svn:\"><S:start-revision>1</S:start-revision>\
<S:end-revision>0</S:end-revision><S:discover-changed-paths/><S:all-revprops/><S:path></S:path></S:log-report>";

/// The principal the daemon's test repository is made and read by.
fn daemon_owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 1234,
  }
}

/// `bytes`' content identity as the records name content: BLAKE3.
fn blake(bytes: &[u8]) -> ContentIdentity {
  *blake3::hash(bytes).as_bytes()
}

/// An addition of the node of `kind` at `path` bringing `text`, as the owner's step takes a row (its digests are the
/// drive's to check).
fn added_row(path: &str, kind: NodeKind, text: Option<&[u8]>) -> ChangedPath {
  ChangedPath {
    path: path.to_owned(),
    action: Action::Add,
    kind,
    copy_from: None,
    text: text.map(|bytes| Text {
      identity: blake(bytes),
      md5: [0; 16],
      sha1: [0; 20],
      size: u64::try_from(bytes.len()).unwrap(),
    }),
    properties: None,
    mergeinfo_modified: false,
  }
}

/// Makes Subversion repository [`COMMITTED`] on a daemon's shard and commits its revision 1 through the owner's step
/// (A-655 part 2b): `/trunk`, `/trunk/a.txt`, `/trunk/sub` and `/trunk/sub/b.txt`, by alice. Its id.
fn committed_repository(state: &mut crate::state::ShardState) -> RepositoryId {
  let create = slates_ipc::protocol::RequestBody::CreateRepository {
    name: COMMITTED.to_owned(),
    tool: slates_ipc::protocol::VcsTool::Subversion,
    format: slates_ipc::protocol::ObjectFormat::Revisions,
    default_branch: "main".to_owned(),
    green: None,
  };
  let reply = crate::verbs::dispatch(state, 1, &daemon_owner(), create);
  assert!(
    matches!(reply, slates_ipc::protocol::ReplyBody::GreenCreated { .. }),
    "{reply:?}"
  );
  let repositories = state.db.partition().repositories();
  let id = repositories
    .repository_named(COMMITTED)
    .expect("the record")
    .id;
  let (a, b): (&[u8], &[u8]) = (b"alpha\n", b"beta\n");
  let revision = slates_svn::commit::revision::Revision {
    head: 0,
    record: RevisionRecord {
      transaction: "0-1".to_owned(),
      changes: vec![
        added_row("/trunk", NodeKind::Directory, None),
        added_row("/trunk/a.txt", NodeKind::File, Some(a)),
        added_row("/trunk/sub", NodeKind::Directory, None),
        added_row("/trunk/sub/b.txt", NodeKind::File, Some(b)),
      ],
    },
    properties: BTreeMap::from([
      (b"svn:author".to_vec(), b"alice".to_vec()),
      (b"svn:date".to_vec(), DATE.as_bytes().to_vec()),
      (b"svn:log".to_vec(), b"the first".to_vec()),
    ]),
    contents: [a, b]
      .iter()
      .map(|bytes| (blake(bytes), bytes.to_vec()))
      .collect(),
    lock_entries: Vec::new(),
  };
  let committed =
    crate::repository::svn_commit::commit(state, (&daemon_owner(), id), revision, None);
  assert!(committed.is_ok(), "{committed:?}");
  id
}

/// What the site reads of [`COMMITTED`] through the daemon's lender, from a task on its shard: whether the lender holds
/// it and another repository; the PROPFIND of `/trunk` at depth 1; the text the GET of `/trunk/sub/b.txt` streams; the
/// `log-report` of every revision.
fn read_through_the_lender(id: RepositoryId) -> (bool, bool, Vec<u8>, Vec<u8>, Vec<u8>) {
  let mut stores = ServedRecords;
  let held = (
    stores.holds(id),
    stores.holds(RepositoryId { bytes: [9; 16] }),
  );
  let (owner, root) = (daemon_owner(), format!("/{COMMITTED}"));
  let (bounds, room) = (xml_bounds(ROOM), usize::try_from(ROOM).unwrap());
  let mut answer = |method: &str, path: &str, depth: Option<&str>, body: &[u8]| {
    let request = site::Request {
      method,
      path,
      depth,
      body,
    };
    stores
      .lend(id, &owner, |lent| {
        site::answer(&served_of(lent, &root), &request, &bounds, room)
      })
      .expect("the records lent")
  };
  let SiteAnswer::Whole(listing) = answer("PROPFIND", "/trunk", Some("1"), PROPFIND_KIND) else {
    panic!("no listing");
  };
  let SiteAnswer::Text {
    text: Some(text), ..
  } = answer("GET", "/trunk/sub/b.txt", None, b"")
  else {
    panic!("no text");
  };
  let SiteAnswer::Report(mut report) = answer("REPORT", "/", None, LOG_EVERY) else {
    panic!("no report");
  };
  let fetched = stores
    .lend(id, &owner, |lent| {
      lent.repository.text(&text.identity, 0).map(<[u8]>::to_vec)
    })
    .flatten()
    .expect("the text");
  let mut log = Vec::new();
  while stores
    .lend(id, &owner, |lent| {
      report.resume(&served_of(lent, &root), &mut log, room)
    })
    .expect("the records lent")
    == ReportProgress::More
  {}
  (held.0, held.1, listing.body, fetched, log)
}

/// A-681 (`crate::vcs_store::svn_view`, the daemon's lender; `Index::children`): do make a Subversion repository on a
/// daemon's shard and commit its revision 1 through the owner's step, then read it from a task on that shard through
/// the daemon's lender: whether it holds the repository and another, a PROPFIND of `/trunk` at depth 1, the GET of
/// `/trunk/sub/b.txt` and a `log-report` of every revision; expect the repository held and the other not, the trunk
/// listed with its two children (the index's listing, which no green lent before), the file's text, and revision 1 by
/// alice with its message and its four added paths, then revision 0.
#[test]
fn the_daemons_lender_serves_a_repositorys_committed_records() {
  let daemon = crate::daemon::audit_daemon();
  let id = crate::daemon::observe_first(&daemon, committed_repository);
  let (sent, outcome) = std::sync::mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(read_through_the_lender(id));
    })
    .expect("the reading task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let read = outcome.recv_timeout(PATIENCE);
  daemon.stop();
  let (held, other, listing, text, log) = read.expect("the lender's task answered");
  let (listing, log) = (
    String::from_utf8(listing).unwrap(),
    String::from_utf8(log).unwrap(),
  );
  let listed = [
    "/committed/trunk/",
    "/committed/trunk/a.txt",
    "/committed/trunk/sub/",
  ]
  .map(|href| listing.contains(&format!("<D:href>{href}</D:href>")));
  let logged = [
    "<D:version-name>1</D:version-name>",
    "<D:creator-displayname>alice</D:creator-displayname>",
    "<D:comment>the first</D:comment>",
    ">/trunk/sub/b.txt</S:added-path>",
    "<D:version-name>0</D:version-name>",
  ]
  .map(|line| log.contains(line));
  assert_eq!(
    (held, other, listed, text, logged),
    (true, false, [true; 3], b"beta\n".to_vec(), [true; 5]),
    "{listing}\n{log}"
  );
}

/// What the owner's served `method target` with `depth` and `body` answers: its status, its fields and its reply's
/// bytes, and how many reply runs it took.
fn exchanged(
  method: &str,
  target: &str,
  depth: Option<&str>,
  body: &[u8],
) -> (u16, Fields, Vec<u8>, usize) {
  let mut site = site();
  let owner = basic("alice", OWNER_TOKEN);
  let depth = depth.map(|depth| format!("Depth: {depth}"));
  let mut fields = vec![owner.as_str()];
  fields.extend(depth.as_deref());
  let bytes = request(method, target, &fields);
  let Answer::Serve { mut exchange, .. } = ready(site.answer(&head(&bytes))).answer else {
    panic!("{method} {target} not served");
  };
  if !body.is_empty() {
    ready(exchange.body(body)).unwrap();
  }
  ready(exchange.body_end(ClientGone::never())).unwrap();
  let status = exchange.reply_status().code;
  let fields = ready(exchange.reply_fields()).unwrap();
  let (mut reply, mut runs) = (Vec::new(), 1);
  while ready(exchange.reply(&mut reply)).unwrap() {
    runs += 1;
  }
  (status, fields, reply, runs)
}

/// A-681 (`slates_svn::dav::site` through the edge's exchange; `mod_authz_svn`): do PROPFIND the trunk at depth 1 as the
/// owner, whose authorization file hides `/trunk/secret`, then GET `/trunk/secret/s.txt`; expect `207` named as the
/// reply begins, with its content type, listing the trunk and `a.txt` and leaving `secret` out; and the GET `403`.
#[test]
fn the_exchange_answers_from_the_lent_store_by_the_principals_rules() {
  let (status, fields, reply, _) = exchanged(
    "PROPFIND",
    "/repo/!svn/rvr/1/trunk",
    Some("1"),
    PROPFIND_KIND,
  );
  let body = String::from_utf8(reply).unwrap();
  let hrefs: Vec<&str> = body
    .split("<D:href>")
    .skip(1)
    .filter_map(|rest| rest.split("</D:href>").next())
    .collect();
  let (hidden, _, _, _) = exchanged("GET", "/repo/trunk/secret/s.txt", None, b"");
  assert_eq!(
    (status, fields, hrefs, hidden),
    (
      207,
      vec![(
        b"Content-Type".to_vec(),
        b"text/xml; charset=\"utf-8\"".to_vec()
      )],
      vec!["/repo/!svn/rvr/1/trunk/", "/repo/!svn/rvr/1/trunk/a.txt"],
      403
    )
  );
}

/// Shape: the `log-report` `svn log -v` sends for revision 1 down (A-609's fixture's form).
const LOG_VERBOSE: &[u8] = b"<S:log-report xmlns:S=\"svn:\"><S:start-revision>1</S:start-revision>\
<S:end-revision>0</S:end-revision><S:discover-changed-paths/><S:revprop>svn:author</S:revprop>\
<S:revprop>svn:date</S:revprop><S:revprop>svn:log</S:revprop><S:path></S:path><S:encode-binary-props/>\
</S:log-report>";

/// A-681 part 1's second slice (`reports/log.c` through the edge's exchange; `mod_authz_svn`'s questions): do REPORT
/// `svn log -v`'s body on the trunk's revision root as the owner, whose rules hide `/trunk/secret`, then a log naming a
/// missing path; expect `200`, `text/xml`, the one entry `log.c` writes with the secret's two changed paths left out and
/// the author and date alone (revision 1 is the owner's in part), and the second `404` with FSFS's "File not found".
#[test]
fn a_log_report_is_streamed_by_the_principals_rules() {
  let (status, fields, reply, _) = exchanged("REPORT", "/repo/!svn/rvr/1/trunk", None, LOG_VERBOSE);
  let expected = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<S:log-report xmlns:S=\"svn:\" xmlns:D=\"DAV:\">\n\
<S:log-item>\n<S:added-path node-kind=\"dir\" text-mods=\"false\" prop-mods=\"false\">/trunk</S:added-path>\n\
<S:added-path node-kind=\"file\" text-mods=\"true\" prop-mods=\"true\">/trunk/a.txt</S:added-path>\n\
<D:version-name>1</D:version-name>\n<D:creator-displayname>alice</D:creator-displayname>\n<S:date>{DATE}</S:date>\n\
</S:log-item>\n</S:log-report>\n"
  );
  let content_type = (
    b"Content-Type".to_vec(),
    b"text/xml; charset=\"utf-8\"".to_vec(),
  );
  assert_eq!(
    (status, fields, String::from_utf8(reply).unwrap()),
    (200, vec![content_type], expected)
  );
  let missing = b"<S:log-report xmlns:S=\"svn:\"><S:path>missing</S:path></S:log-report>";
  let (status, _, reply, _) = exchanged("REPORT", "/repo/!svn/rvr/1/trunk", None, missing);
  let words = "File not found: revision 1, path '/trunk/missing'";
  assert_eq!(
    (status, String::from_utf8(reply).unwrap().contains(words)),
    (404, true)
  );
}

/// A-681 (`repos.c` `deliver`; §4.17.4 item 11: a read streams from chunks): do GET a file whose green hands its text
/// over [`PIECE`] bytes at a time; expect `200`, the text whole, given over as many reply runs as pieces, and the
/// file's headers.
#[test]
fn a_get_streams_the_text_a_piece_a_reply_run() {
  let (status, fields, reply, runs) = exchanged("GET", "/repo/!svn/rvr/1/trunk/a.txt", None, b"");
  let names: Vec<&[u8]> = fields.iter().map(|(name, _)| name.as_slice()).collect();
  assert_eq!(
    (status, reply, runs, names),
    (
      200,
      A_TEXT.to_vec(),
      A_TEXT.len().div_ceil(PIECE),
      vec![b"Cache-Control".as_slice(), b"ETag", b"Content-Type"]
    )
  );
}

/// Shape: a runtime for one edge: one shard, room for its listener, its connections and their timers.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 32,
    timers_per_shard: 32,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// Stops shard `0` when dropped, however the client's thread ends, so a test never hangs on a shard nobody stops.
struct StopsShard(u16);

impl Drop for StopsShard {
  fn drop(&mut self) {
    let _ = slates_rt::registry::send_control(self.0, Control::Shutdown);
  }
}

/// A self-signed identity for `localhost` and `127.0.0.1`.
fn identity() -> ServingIdentity {
  let key = rcgen::KeyPair::generate().expect("a key");
  let names = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
  let certificate = rcgen::CertificateParams::new(names)
    .expect("parameters")
    .self_signed(&key)
    .expect("a self-signed certificate");
  let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
  ServingIdentity::new(vec![certificate.der().clone()], private).expect("an identity")
}

/// The test's scratch directory for svn's configuration, in the build output beside `CARGO_TARGET_TMPDIR`'s
/// (`<target>/tmp`), named with the process id, made empty; removed when dropped. Test code is outside the R1 wall: it
/// writes only into this directory.
struct Scratch(std::path::PathBuf);

impl Scratch {
  /// Makes the directory.
  #[allow(clippy::disallowed_methods)] // The test's own scratch directory (the type's doc).
  fn new() -> Self {
    let executable = std::env::current_exe().expect("the test binary's path");
    let target = executable
      .ancestors()
      .nth(3)
      .expect("the build output above the test binary");
    let path = target
      .join("tmp")
      .join(format!("slates-svn-dav-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("the scratch directory");
    Self(path)
  }
}

impl Drop for Scratch {
  #[allow(clippy::disallowed_methods)] // The test's own scratch directory (the type's doc).
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.0);
  }
}

/// Runs stock `svn` with `arguments` against the site, as alice: its exit status and its output, standard error after
/// standard output.
fn svn(config: &Scratch, arguments: &[&str]) -> (bool, String) {
  let config_dir = config.0.to_string_lossy().into_owned();
  let output = Command::new("svn")
    .args([
      "--non-interactive",
      "--no-auth-cache",
      "--config-dir",
      &config_dir,
      "--config-option",
      "servers:global:http-timeout=60",
      "--trust-server-cert-failures=unknown-ca,cn-mismatch",
      "--username",
      "alice",
      "--password",
      OWNER_TOKEN,
    ])
    .args(arguments)
    .output()
    .expect("svn runs");
  let text = format!(
    "{}{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  (output.status.success(), text)
}

/// Whether stock `svn` is on this host.
fn svn_present() -> bool {
  Command::new("svn")
    .args(["--version", "--quiet"])
    .output()
    .is_ok_and(|output| output.status.success())
}

/// A-681, the stock client against the site (the brief's oracle): do serve the tests' repository over HTTPS on loopback
/// and run stock svn's `info`, `cat`, `ls`, `proplist -v` against it as alice, then `cat` a file her authorization file
/// hides, then `log` and `log -v` of the trunk; expect svn to read the file's node kind, revision, UUID and author, its
/// text, the trunk's listing without the hidden directory and the file's `svn:eol-style`, the hidden file refused as
/// forbidden, and revision 1's entry by alice with neither its message nor the hidden paths (she reads the revision in
/// part), its readable changed paths listed. Skips loudly where svn is not installed.
#[test]
fn stock_svn_reads_a_repository_through_the_site_over_https() {
  if !svn_present() {
    eprintln!(
      "SKIPPED stock_svn_reads_a_repository_through_the_site_over_https: no `svn` on this host"
    );
    return;
  }
  let runtime = runtime();
  let bounds = Bounds::of_shard(4, usize::try_from(ROOM).unwrap(), 100_000);
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), bounds.backlog())
    .expect("a loopback listener");
  let address = listener.local_addr().expect("the listener's address");
  let state = runtime
    .context()
    .keep(EdgeState::new(bounds, Some(identity())))
    .expect("the edge's state kept on its shard");
  runtime
    .spawn(async move {
      let _ = serve(listener, state, site).await;
    })
    .expect("the listener's task");
  let shard = runtime.shard_id().0;
  let client = std::thread::spawn(move || {
    let _stops = StopsShard(shard);
    let config = Scratch::new();
    let url = |path: &str| format!("https://127.0.0.1:{}/repo{path}", address.port());
    [
      svn(&config, &["info", &url("/trunk/a.txt")]),
      svn(&config, &["cat", &url("/trunk/a.txt")]),
      svn(&config, &["ls", &url("/trunk")]),
      svn(&config, &["proplist", "-v", &url("/trunk/a.txt")]),
      svn(&config, &["cat", &url("/trunk/secret/s.txt")]),
      svn(&config, &["log", &url("/trunk")]),
      svn(&config, &["log", "-v", &url("/trunk")]),
    ]
  });
  runtime.context().run();
  let [info, cat, ls, proplist, hidden, log, verbose] = client.join().expect("the client's thread");
  for (ran, output) in [&info, &cat, &ls, &proplist, &hidden, &log, &verbose] {
    eprintln!("svn (success {ran}):\n{output}");
  }
  let read = |(ran, output): &(bool, String), wanted: &[&str]| {
    *ran && wanted.iter().all(|line| output.contains(line))
  };
  assert_eq!(
    (
      read(
        &info,
        &[
          "Node Kind: file",
          "Revision: 1",
          UUID,
          "Last Changed Author: alice"
        ]
      ),
      cat.clone(),
      ls.clone(),
      read(&proplist, &["svn:eol-style", "native"]),
      (
        hidden.0,
        hidden.1.contains("403") || hidden.1.contains("orbidden")
      ),
    ),
    (
      true,
      (true, String::from_utf8_lossy(A_TEXT).into_owned()),
      (true, String::from("a.txt\n")),
      true,
      (false, true),
    )
  );
  let unseen = |(_, output): &(bool, String), hidden: &[&str]| {
    hidden.iter().all(|word| !output.contains(word))
  };
  assert_eq!(
    (
      read(&log, &["r1 | alice |"]) && unseen(&log, &["the first"]),
      read(&verbose, &["r1 | alice |", "A /trunk", "A /trunk/a.txt"])
        && unseen(&verbose, &["secret", "the first"]),
    ),
    (true, true)
  );
}

/// A-681 (§4.17.4 item 5 "HTTP"; the daemon's [`crate::vcs_http::ToolSites`]): a request is Subversion's by its shape,
/// since stock svn sends what git's and Mercurial's clients never send. Do: ask [`super::dav_request`] of the heads
/// stock svn 1.15 sends (recorded, g297: OPTIONS, PROPFIND, REPORT on a public URL; a GET and a POST under `!svn`) and
/// of git's (`info/refs`, `git-upload-pack`, `git-receive-pack`) and Git LFS's. Expect svn's every one, and none of
/// git's.
#[test]
fn a_request_is_subversions_by_its_method_or_its_special_segment() {
  let asked = |method: &str, target: &str| {
    let input = request(method, target, &[]);
    super::dav_request(&head(&input))
  };
  let svn = [
    asked("OPTIONS", "/project"),
    asked("PROPFIND", "/project/trunk"),
    asked("REPORT", "/project/!svn/me"),
    asked("GET", "/project/!svn/rvr/3/trunk/a.txt"),
    asked("POST", "/project/!svn/me"),
    asked("MERGE", "/project/trunk"),
  ];
  let others = [
    asked("GET", "/project/info/refs?service=git-upload-pack"),
    asked("POST", "/project/git-upload-pack"),
    asked("POST", "/project/git-receive-pack"),
    asked("POST", "/project.git/info/lfs/objects/batch"),
    asked("GET", "/project/svn/trunk"),
  ];
  assert_eq!((svn, others), ([true; 6], [false; 5]));
}
